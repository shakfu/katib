use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use minima_core::tools::Scratch;

use super::*;

/// A fake agent on a thread: reads the client's messages from a pipe, answers through the
/// client's incoming channel, as a real agent's stdout would.
struct Fake {
    lines: std::io::Lines<BufReader<std::io::PipeReader>>,
    to_client: async_channel::Sender<Value>,
}

impl Fake {
    fn next(&mut self) -> Value {
        let line = self.lines.next().expect("the client is open").unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn send(&self, message: Value) {
        self.to_client.send_blocking(message).unwrap();
    }

    fn update(&self, update: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": "session/update",
                          "params": { "sessionId": "s1", "update": update } }));
    }

    /// Asks the client and returns its reply.
    fn ask(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let reply = self.next();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    fn answer(&self, id: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
}

/// The client over a fake agent running `script`, and the session's ends.
struct Harness {
    prompts: async_channel::Sender<(String, Cancel)>,
    answers: async_channel::Sender<(u64, Option<String>)>,
    events: async_channel::Receiver<Event>,
    stop: Cancel,
}

fn start(
    root: &Path,
    mcp: Option<McpServer>,
    script: impl FnOnce(Fake) + Send + 'static,
) -> Harness {
    let (reader, writer) = std::io::pipe().unwrap();
    let (to_client, incoming) = async_channel::unbounded();
    std::thread::spawn(move || {
        script(Fake {
            lines: BufReader::new(reader).lines(),
            to_client,
        })
    });
    let (prompts, inbox) = async_channel::unbounded();
    let (answers, answered) = async_channel::unbounded();
    let (sender, events) = async_channel::unbounded();
    let stop = Cancel::new();
    let (root, stopped) = (root.to_path_buf(), stop.clone());
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = Client::new(&root, Box::new(writer), sender.clone(), mcp);
        if let Err(e) = runtime.block_on(client.run(incoming, inbox, answered, stopped)) {
            let _ = sender.try_send(Event::Failed(e));
        }
    });
    Harness {
        prompts,
        answers,
        events,
        stop,
    }
}

impl Harness {
    fn prompt(&self, text: &str) -> Cancel {
        let cancel = Cancel::new();
        self.prompts
            .try_send((text.into(), cancel.clone()))
            .unwrap();
        cancel
    }

    fn next(&self) -> Event {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(event) = self.events.try_recv() {
                return event;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no event within 5 s");
    }

    fn until_done(&self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let event = self.next();
            let done = matches!(event, Event::Done(_) | Event::Failed(_));
            out.push(event);
            if done {
                return out;
            }
        }
    }
}

/// Answers `initialize` and `session/new`; returns the `session/new` params.
fn open(fake: &mut Fake) -> Value {
    let init = fake.next();
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["protocolVersion"], 1);
    assert_eq!(
        init["params"]["clientCapabilities"]["fs"]["writeTextFile"],
        true
    );
    fake.answer(
        &init["id"],
        json!({ "protocolVersion": 1, "agentInfo": { "name": "fake", "title": "Fake Agent" } }),
    );
    let new = fake.next();
    assert_eq!(new["method"], "session/new");
    fake.answer(&new["id"], json!({ "sessionId": "s1" }));
    new["params"].clone()
}

fn prompt_text(message: &Value) -> &str {
    message["params"]["prompt"][0]["text"].as_str().unwrap()
}

#[test]
fn a_prompt_streams_text_and_tools_and_files_stay_in_the_vault() {
    let dir = Scratch::new("katib-acp-run");
    std::fs::write(dir.file("a.md"), "old\n").unwrap();
    let root = PathBuf::from(dir.file("")).canonicalize().unwrap();
    let note = root.join("a.md").display().to_string();
    let mcp = McpServer {
        name: "katib".into(),
        command: "/bin/katib".into(),
        args: vec!["mcp".into(), "/v".into()],
    };
    let script_note = note.clone();
    let harness = start(&root, Some(mcp), move |mut fake| {
        let new = open(&mut fake);
        assert_eq!(
            new["mcpServers"],
            json!([{ "name": "katib", "command": "/bin/katib", "args": ["mcp", "/v"], "env": [] }])
        );
        let prompt = fake.next();
        assert_eq!(prompt_text(&prompt), "edit a");
        fake.update(json!({ "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "on it" } }));
        fake.update(
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.md",
                            "kind": "edit", "status": "pending",
                            "locations": [{ "path": script_note }] }),
        );
        let read = fake.ask(
            10,
            "fs/read_text_file",
            json!({ "sessionId": "s1", "path": script_note }),
        );
        assert_eq!(read["result"]["content"], "old\n");
        let wrote = fake.ask(
            11,
            "fs/write_text_file",
            json!({ "sessionId": "s1", "path": script_note, "content": "new\n" }),
        );
        assert_eq!(wrote["result"], Value::Null, "{wrote}");
        let outside = fake.ask(
            12,
            "fs/read_text_file",
            json!({ "sessionId": "s1", "path": "/etc/hostname" }),
        );
        assert!(
            outside["error"]["message"]
                .as_str()
                .unwrap()
                .contains("outside the vault")
        );
        let hidden = fake.ask(
            13,
            "fs/write_text_file",
            json!({ "sessionId": "s1", "path": format!("{}/.git/config", script_note.trim_end_matches("/a.md")), "content": "x" }),
        );
        assert!(
            hidden["error"]["message"]
                .as_str()
                .unwrap()
                .contains("hidden"),
            "{hidden}"
        );
        let terminal = fake.ask(14, "terminal/create", json!({}));
        assert_eq!(terminal["error"]["code"], -32601);
        fake.update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                            "content": [{ "type": "content", "content": { "type": "text", "text": "edited" } }] }));
        fake.update(
            json!({ "sessionUpdate": "usage_update", "used": 900, "size": 200000,
                            "cost": { "amount": 0.25, "currency": "USD" } }),
        );
        fake.answer(&prompt["id"], json!({ "stopReason": "end_turn" }));
        // Hold the pipe open until the client stops.
        let _ = fake.lines.next();
    });
    assert_eq!(
        harness.next(),
        Event::Ready {
            provider: "Fake Agent".into(),
            model: String::new(),
            context: 0,
            cost_estimated: false
        }
    );
    harness.prompt("edit a");
    let got = harness.until_done();
    assert_eq!(
        got,
        [
            Event::Text("on it".into()),
            Event::ToolStart {
                id: "t1".into(),
                name: "Edit a.md".into(),
                note: Some("a".into()),
                detail: None
            },
            Event::ToolEnd {
                id: "t1".into(),
                body: "edited".into(),
                note: None,
                ok: true
            },
            Event::Totals {
                used: 900,
                size: 200000,
                cost: Some(0.25)
            },
            Event::Done(Finish::Answered),
        ]
    );
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "new\n");
    harness.stop.cancel();
}

#[test]
fn a_write_after_a_read_is_refused_if_the_file_changed() {
    let dir = Scratch::new("katib-acp-guard");
    std::fs::write(dir.file("a.md"), "one\n").unwrap();
    let root = PathBuf::from(dir.file("")).canonicalize().unwrap();
    let note = root.join("a.md").display().to_string();
    let mut fs = Fs::new(&root);
    let read = |fs: &mut Fs| fs.read(&json!({ "path": note }));
    let write = |fs: &mut Fs, text: &str| fs.write(&json!({ "path": note, "content": text }));
    read(&mut fs).unwrap();
    std::fs::write(&note, "theirs\n").unwrap();
    assert!(
        write(&mut fs, "mine\n")
            .unwrap_err()
            .contains("changed since you read it")
    );
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "theirs\n");
    read(&mut fs).unwrap();
    write(&mut fs, "mine\n").unwrap();
    // A new file in a new folder; `..` is refused even where nothing exists yet.
    let new = root.join("sub/b.md").display().to_string();
    fs.write(&json!({ "path": new, "content": "b" })).unwrap();
    let climb = root.join("x/../../escape.md").display().to_string();
    assert!(fs.write(&json!({ "path": climb, "content": "b" })).is_err());
    // Lines are 1-based.
    std::fs::write(&note, "1\n2\n3\n").unwrap();
    assert_eq!(
        fs.read(&json!({ "path": note, "line": 2, "limit": 1 }))
            .unwrap(),
        "2\n"
    );
}

#[test]
fn permissions_reach_the_panel_and_a_cancel_answers_them() {
    let dir = Scratch::new("katib-acp-permission");
    let root = PathBuf::from(dir.file("")).canonicalize().unwrap();
    let harness = start(&root, None, |mut fake| {
        open(&mut fake);
        let prompt = fake.next();
        let options = json!([
            { "optionId": "yes", "name": "Allow", "kind": "allow_once" },
            { "optionId": "no", "name": "Reject", "kind": "reject_once" },
        ]);
        let ask = |fake: &mut Fake, id| {
            fake.ask(
                id,
                "session/request_permission",
                json!({
                    "sessionId": "s1", "toolCall": { "toolCallId": "t1", "title": "Run ls" },
                    "options": options,
                }),
            )
        };
        let reply = ask(&mut fake, 20);
        assert_eq!(
            reply["result"]["outcome"],
            json!({ "outcome": "selected", "optionId": "yes" })
        );
        fake.answer(&prompt["id"], json!({ "stopReason": "end_turn" }));

        // A second prompt is cancelled while its permission request waits.
        let prompt = fake.next();
        fake.send(json!({ "jsonrpc": "2.0", "id": 21, "method": "session/request_permission",
                          "params": { "sessionId": "s1", "toolCall": { "title": "Run rm" }, "options": options } }));
        let mut got = [fake.next(), fake.next()];
        got.sort_by_key(|m| m["method"].is_null());
        assert_eq!(got[0]["method"], "session/cancel");
        assert_eq!(
            got[1]["result"]["outcome"],
            json!({ "outcome": "cancelled" })
        );
        fake.answer(&prompt["id"], json!({ "stopReason": "cancelled" }));
        let _ = fake.lines.next();
    });
    assert!(matches!(harness.next(), Event::Ready { .. }));
    harness.prompt("first");
    let Event::Permission {
        ask,
        title,
        options,
        ..
    } = harness.next()
    else {
        panic!("no permission request");
    };
    assert_eq!(title, "Run ls");
    assert_eq!(
        options,
        [
            PermissionOption {
                id: "yes".into(),
                name: "Allow".into(),
                allow: true
            },
            PermissionOption {
                id: "no".into(),
                name: "Reject".into(),
                allow: false
            },
        ]
    );
    harness.answers.try_send((ask, Some("yes".into()))).unwrap();
    assert_eq!(
        harness.until_done(),
        [Event::Answered(ask), Event::Done(Finish::Answered)]
    );

    let cancel = harness.prompt("second");
    let Event::Permission { ask, .. } = harness.next() else {
        panic!("no permission request");
    };
    cancel.cancel();
    assert_eq!(
        harness.until_done(),
        [Event::Answered(ask), Event::Done(Finish::Cancelled)]
    );
    harness.stop.cancel();
}

#[test]
fn an_agent_that_exits_or_speaks_another_version_fails() {
    let dir = Scratch::new("katib-acp-exit");
    let root = PathBuf::from(dir.file("")).canonicalize().unwrap();
    let harness = start(&root, None, |mut fake| {
        let init = fake.next();
        fake.answer(&init["id"], json!({ "protocolVersion": 2 }));
        let _ = fake.lines.next();
    });
    match harness.next() {
        Event::Failed(e) => assert!(e.contains("ACP version 2"), "{e}"),
        other => panic!("{other:?}"),
    }

    let harness = start(&root, None, |mut fake| {
        open(&mut fake);
        let _prompt = fake.next();
        // Dropping the fake closes the agent's stdout.
    });
    assert!(matches!(harness.next(), Event::Ready { .. }));
    harness.prompt("go");
    let got = harness.until_done();
    assert_eq!(
        got[0],
        Event::Done(Finish::Failed("the agent stopped".into()))
    );
}

#[test]
fn commands_split_on_spaces_outside_quotes() {
    assert_eq!(
        split_command(" minima  --acp ").unwrap(),
        ["minima", "--acp"]
    );
    assert_eq!(
        split_command(r#"run "a b" 'c d' e"f"g"#).unwrap(),
        ["run", "a b", "c d", "efg"]
    );
    assert_eq!(split_command(r#"x """#).unwrap(), ["x", ""]);
    assert!(split_command("").is_err() && split_command("a 'b").is_err());
}

/// Against minima's ACP mock, from a minima checkout beside katib's:
/// `MINIMA=/path/to/minima cargo test -p katib-agent -- --ignored`.
#[test]
#[ignore]
fn minima_mock_answers_through_the_session() {
    let mock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../minima/mock/acp.json");
    let dir = Scratch::new("katib-acp-minima");
    let vault = Vault::open(dir.file("")).unwrap();
    let minima = std::env::var("MINIMA").unwrap_or_else(|_| "minima".into());
    let command = vec![
        minima,
        "--acp".into(),
        "--mock".into(),
        mock.display().to_string(),
    ];
    let session = Session::acp(vault, command, None);
    let events = session.events();
    let next = || events.recv_blocking().unwrap();
    assert!(
        matches!(next(), Event::Ready { .. }),
        "minima did not start"
    );
    session.prompt("say hello");
    let mut text = String::new();
    loop {
        match next() {
            Event::Text(t) => text += &t,
            Event::Done(finish) => {
                assert_eq!(finish, Finish::Answered);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(text.trim_end(), "hello from the mock");
}
