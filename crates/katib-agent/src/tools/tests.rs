use std::path::PathBuf;

use futures_util::FutureExt;
use minima_core::tools::Scratch;
use serde_json::{Value, json};

use super::*;

/// A vault in a scratch directory, holding `files` by path relative to the root.
fn vault(name: &str, files: &[(&str, &str)]) -> (Scratch, VaultTools) {
    let dir = Scratch::new(&format!("katib-agent-{name}"));
    for (rel, body) in files {
        let path = PathBuf::from(dir.file(rel));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let tools = VaultTools::open(Vault::open(dir.file("")).unwrap()).unwrap();
    (dir, tools)
}

/// Every call is synchronous, so its future is ready at once.
fn call(tools: &VaultTools, name: &str, args: Value) -> Result<String> {
    let (cancel, args) = (Cancel::new(), args.to_string());
    let future = tools.call(name, &args, &cancel);
    future.now_or_never().expect("ready").map(|o| o.body)
}

fn disk(dir: &Scratch, rel: &str) -> String {
    fs::read_to_string(dir.file(rel)).unwrap()
}

#[test]
fn a_delete_needs_a_read_of_the_current_text_and_names_links_left() {
    let (dir, tools) = vault(
        "delete",
        &[
            ("a.md", "# A\n"),
            ("b.md", "[[a]] and [[a#x]]\n"),
            ("c.md", "[[a]]\n"),
        ],
    );
    let err = call(&tools, "delete", json!({ "note": "a" })).unwrap_err();
    assert!(err.to_string().contains("read a before"), "{err}");

    call(&tools, "read", json!({ "note": "a" })).unwrap();
    fs::write(dir.file("a.md"), "# A\nedited elsewhere\n").unwrap();
    let err = call(&tools, "delete", json!({ "note": "a" })).unwrap_err();
    assert!(err.to_string().contains("changed since"), "{err}");
    assert!(PathBuf::from(dir.file("a.md")).exists());

    call(&tools, "read", json!({ "note": "a" })).unwrap();
    let out = call(&tools, "delete", json!({ "note": "a" })).unwrap();
    assert_eq!(out, "moved a to the trash; links to it remain in b, c");
    assert!(!PathBuf::from(dir.file("a.md")).exists());
    assert_eq!(disk(&dir, ".katib/trash/a.md"), "# A\nedited elsewhere\n");
    let listed = call(&tools, "list", json!({})).unwrap();
    assert!(!listed.contains("a - "), "{listed}");
    let err = call(&tools, "delete", json!({ "note": "a" })).unwrap_err();
    assert!(err.to_string().contains("no note a"), "{err}");
}

#[test]
fn ids_that_leave_the_vault_are_refused() {
    let (dir, tools) = vault("escape", &[("a.md", "# A\n")]);
    fs::create_dir_all(dir.file(".git")).unwrap();
    fs::write(dir.file(".git/config.md"), "secret").unwrap();
    for id in [
        "../a",
        "/etc/passwd",
        ".katib/cache/index",
        ".git/config",
        "a/../../x",
        "a\\b",
    ] {
        let err = call(&tools, "read", json!({ "note": id })).unwrap_err();
        assert!(
            err.to_string().contains("not a valid note name"),
            "{id}: {err}"
        );
        let err = call(&tools, "write", json!({ "note": id, "content": "x" })).unwrap_err();
        assert!(
            err.to_string().contains("not a valid note name"),
            "{id}: {err}"
        );
    }
    assert_eq!(disk(&dir, ".git/config.md"), "secret");
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_out_of_the_vault_is_neither_read_nor_written() {
    let outside = Scratch::new("katib-agent-outside");
    fs::write(outside.file("secret.md"), "secret").unwrap();
    let (dir, tools) = vault("symlink", &[]);
    std::os::unix::fs::symlink(outside.file("secret.md"), dir.file("leak.md")).unwrap();
    std::os::unix::fs::symlink(outside.file(""), dir.file("linked")).unwrap();

    for id in ["leak", "linked/secret", "linked/new"] {
        let err = call(&tools, "read", json!({ "note": id })).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{id}: {err}");
        let err = call(&tools, "write", json!({ "note": id, "content": "x" })).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{id}: {err}");
    }
    assert_eq!(disk(&outside, "secret.md"), "secret");
    assert!(!PathBuf::from(outside.file("new.md")).exists());
}

#[test]
fn a_note_is_created_but_replaced_only_after_it_was_read() {
    let (dir, tools) = vault("write", &[("a.md", "old\n")]);

    let out = call(
        &tools,
        "write",
        json!({ "note": "dir/new", "content": "hi\n" }),
    )
    .unwrap();
    assert_eq!(out, "created dir/new");
    assert_eq!(disk(&dir, "dir/new.md"), "hi\n");

    let err = call(&tools, "write", json!({ "note": "a", "content": "x" })).unwrap_err();
    assert!(err.to_string().contains("read it before"), "{err}");
    assert_eq!(disk(&dir, "a.md"), "old\n");

    assert_eq!(
        call(&tools, "read", json!({ "note": "a" })).unwrap(),
        "old\n"
    );
    let out = call(
        &tools,
        "write",
        json!({ "note": "a.md", "content": "new\n" }),
    )
    .unwrap();
    assert_eq!(out, "replaced a");
    assert_eq!(disk(&dir, "a.md"), "new\n");
}

/// The window or sync wrote the note after the agent read it.
#[test]
fn a_replacement_is_refused_when_the_note_changed_since_it_was_read() {
    let (dir, tools) = vault("conflict", &[("a.md", "old\n")]);
    call(&tools, "read", json!({ "note": "a" })).unwrap();
    fs::write(dir.file("a.md"), "typed in the window\n").unwrap();

    let err = call(&tools, "write", json!({ "note": "a", "content": "x" })).unwrap_err();
    assert!(
        err.to_string().contains("changed since you read it"),
        "{err}"
    );
    assert_eq!(disk(&dir, "a.md"), "typed in the window\n");
}

#[test]
fn an_edit_replaces_exactly_one_occurrence() {
    let (dir, tools) = vault("edit", &[("a.md", "one two two\n")]);

    let out = call(
        &tools,
        "edit",
        json!({ "note": "a", "old": "one", "new": "1" }),
    )
    .unwrap();
    assert_eq!(out, "edited a");
    assert_eq!(disk(&dir, "a.md"), "1 two two\n");

    let err = call(
        &tools,
        "edit",
        json!({ "note": "a", "old": "two", "new": "2" }),
    )
    .unwrap_err();
    assert!(err.to_string().contains("occurs 2 times"), "{err}");
    let err = call(
        &tools,
        "edit",
        json!({ "note": "a", "old": "six", "new": "6" }),
    )
    .unwrap_err();
    assert!(err.to_string().contains("not in a"), "{err}");
    let err = call(
        &tools,
        "edit",
        json!({ "note": "a", "old": "", "new": "6" }),
    )
    .unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
    assert_eq!(disk(&dir, "a.md"), "1 two two\n");

    // The edit's result counts as read, so a replacement may follow it.
    call(&tools, "write", json!({ "note": "a", "content": "z" })).unwrap();
    assert_eq!(disk(&dir, "a.md"), "z");
}

#[test]
fn search_list_and_backlinks_see_the_agents_own_writes() {
    let (_dir, tools) = vault("query", &[("journal/day.md", "# Day\nNothing yet.\n")]);
    call(
        &tools,
        "write",
        json!({ "note": "ideas", "content": "# Ideas\nA zeppelin. See [[Day]].\n" }),
    )
    .unwrap();

    let hits = call(&tools, "search", json!({ "query": "zeppelin" })).unwrap();
    assert!(hits.starts_with("ideas - Ideas"), "{hits}");
    assert_eq!(
        call(&tools, "search", json!({ "query": "absent" })).unwrap(),
        "no matches"
    );

    let all = call(&tools, "list", json!({})).unwrap();
    assert_eq!(all, "ideas - Ideas\njournal/day - Day");
    let journal = call(&tools, "list", json!({ "folder": "journal/" })).unwrap();
    assert_eq!(journal, "journal/day - Day");

    let links = call(&tools, "backlinks", json!({ "note": "journal/day" })).unwrap();
    assert!(links.starts_with("ideas - Ideas"), "{links}");
}

#[test]
fn a_move_rewrites_links_and_forgets_the_texts_it_changed() {
    let (dir, tools) = vault("move", &[("a.md", "# A\n"), ("b.md", "see [a](a.md)\n")]);
    call(&tools, "read", json!({ "note": "b" })).unwrap();

    let out = call(&tools, "move", json!({ "from": "a", "to": "archive/a" })).unwrap();
    assert_eq!(out, "moved a to archive/a; rewrote links in b");
    assert_eq!(disk(&dir, "b.md"), "see [a](archive/a.md)\n");

    let err = call(&tools, "write", json!({ "note": "b", "content": "x" })).unwrap_err();
    assert!(err.to_string().contains("read it before"), "{err}");
}

#[test]
fn malformed_arguments_and_unknown_tools_are_errors() {
    let (_dir, tools) = vault("args", &[]);
    let err = call(&tools, "read", json!({})).unwrap_err();
    assert!(
        err.to_string().contains("tool arguments were not valid"),
        "{err}"
    );
    let err = call(&tools, "bash", json!({})).unwrap_err();
    assert_eq!(err.to_string(), "no such tool: bash");
}

#[test]
fn every_tool_advertises_an_object_schema() {
    let (_dir, tools) = vault("specs", &[]);
    let names: Vec<_> = tools.specs().into_iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "list",
            "read",
            "search",
            "backlinks",
            "write",
            "edit",
            "move",
            "delete"
        ]
    );
    for spec in tools.specs() {
        assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
        assert!(spec.parameters.get("$schema").is_none(), "{}", spec.name);
    }
}

/// The loop runs a scripted model's `write`, then takes its answer.
#[tokio::test]
async fn the_agent_loop_writes_through_the_vault_tools() {
    use minima_core::provider::{Dialect, Provider, mock::Mock};

    let (dir, tools) = vault("loop", &[]);
    let args = json!({ "note": "hello", "content": "# Hello\n" }).to_string();
    let script = json!([
        [{ "tool_call": { "index": 0, "id": "c1", "name": "write", "arguments": args } }],
        [{ "text": "done" }]
    ]);
    let config = minima_core::Config {
        provider: "mock".into(),
        base_url: String::new(),
        api_key: String::new(),
        model: "mock".into(),
        dialect: Dialect::Chat,
        context: 128_000,
        context_guessed: false,
        max_output: None,
        adaptive_thinking: false,
        max_turns: 4,
        pricing: None,
        rates: None,
        cap_output: false,
        cache_dir: None,
    };
    let provider = Provider::Mock(Mock::from_script(&script.to_string()).unwrap());
    let mut agent = minima_core::Agent::new(provider, config, "system".into(), Box::new(tools));

    struct Quiet;
    impl minima_core::Frontend for Quiet {
        fn text(&mut self, _: &str) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: Option<&str>, _: bool) {}
        fn retry(&mut self, _: u32, _: std::time::Duration) {}
        fn turn_end(&mut self, _: minima_core::provider::Usage) {}
        fn compacted(&mut self, _: u32, _: u32) {}
        fn cancelled(&mut self) {}
    }
    agent
        .run("say hello", &mut Quiet, &Cancel::new())
        .await
        .unwrap();

    assert_eq!(disk(&dir, "hello.md"), "# Hello\n");
    let last = agent.messages().last().unwrap();
    assert_eq!(last.content.as_deref(), Some("done"));
}

#[test]
fn a_call_is_described_by_what_it_names() {
    let target = describe("move", r#"{"from": "a", "to": "b/a"}"#);
    assert_eq!(target.note.as_deref(), Some("b/a"));
    assert_eq!(target.detail.as_deref(), Some("from a"));
    assert_eq!(
        describe("read", r#"{"note": "x"}"#).note.as_deref(),
        Some("x")
    );
    assert_eq!(
        describe("search", r#"{"query": "q"}"#).detail.as_deref(),
        Some("q")
    );
    assert_eq!(describe("write", "{not json"), Target::default());
}
