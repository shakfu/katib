use std::fs;

use super::*;

/// A vault in a fresh temporary folder, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("katib-lsp-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (p, t) in files {
            let path = dir.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, t).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn uri(&self, rel: &str) -> String {
        path_uri(&self.0.join(rel))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A server initialized on `dir`, with file renames and the default UTF-16 positions.
fn server(dir: &Dir) -> Server {
    let mut server = Server::default();
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "rootUri": path_uri(&dir.0),
            "capabilities": { "workspace": { "workspaceEdit": {
                "documentChanges": true, "resourceOperations": ["create", "rename"],
            } } },
        },
    });
    let reply = server.handle(&init);
    assert_eq!(
        reply[0]["result"]["capabilities"]["positionEncoding"], "utf-16",
        "{reply:?}"
    );
    server
}

fn open(server: &mut Server, uri: &str, text: &str) -> Vec<Value> {
    server.handle(&json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": uri, "version": 1, "languageId": "markdown", "text": text } },
    }))
}

fn request(server: &mut Server, method: &str, params: Value) -> Value {
    let mut reply =
        server.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    assert_eq!(reply.len(), 1, "{reply:?}");
    reply.remove(0)
}

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } })
}

#[test]
fn broken_links_are_diagnostics_and_clear_once_fixed() {
    let dir = Dir::new(
        "diag",
        &[("a.md", ""), ("x/b.md", "# B\n"), ("y/b.md", "# B\n")],
    );
    let mut server = server(&dir);
    let uri = dir.uri("a.md");
    // `é` is one UTF-16 unit and two bytes: columns count units.
    let got = open(
        &mut server,
        &uri,
        "é [[gone]] [[b]] [[x/b]] [f](none.png) [w](https://x)\n",
    );
    let items = got[0]["params"]["diagnostics"].as_array().unwrap();
    let found: Vec<_> = items
        .iter()
        .map(|d| {
            (
                d["range"]["start"]["character"].as_u64().unwrap(),
                d["message"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (2, "no note gone"),
            (11, "2 notes match: x/b, y/b"),
            (25, "no file none.png")
        ]
    );

    fs::write(dir.0.join("gone.md"), "").unwrap();
    let got = server.handle(&json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave", "params": { "textDocument": { "uri": uri } },
    }));
    assert_eq!(got[0]["params"]["diagnostics"].as_array().unwrap().len(), 2);
}

#[test]
fn completion_offers_notes_and_closes_the_link() {
    let dir = Dir::new(
        "complete",
        &[("p/Project.md", "# Project Plan\n"), ("q.md", "# Other\n")],
    );
    let mut server = server(&dir);
    let uri = dir.uri("a.md");
    open(&mut server, &uri, "x\nsee [[proj");
    let reply = request(&mut server, "textDocument/completion", at(&uri, 1, 10));
    let items = reply["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["label"], "Project Plan");
    assert_eq!(items[0]["textEdit"]["newText"], "Project Plan]]");
    assert_eq!(
        items[0]["textEdit"]["range"],
        json!({ "start": { "line": 1, "character": 6 }, "end": { "line": 1, "character": 10 } })
    );
    // Outside an open `[[`, nothing.
    let reply = request(&mut server, "textDocument/completion", at(&uri, 0, 1));
    assert_eq!(reply["result"], Value::Null);
}

#[test]
fn definition_goes_to_the_note_and_its_heading() {
    let dir = Dir::new(
        "def",
        &[("b.md", "# B\n\n## Part Two\n"), ("img.png", "png")],
    );
    let mut server = server(&dir);
    let uri = dir.uri("a.md");
    open(&mut server, &uri, "[[b#part two]] ![i](img.png) [[none]]");
    let reply = request(&mut server, "textDocument/definition", at(&uri, 0, 3));
    assert_eq!(reply["result"]["uri"], dir.uri("b.md"));
    assert_eq!(
        reply["result"]["range"]["start"],
        json!({ "line": 2, "character": 0 })
    );
    let reply = request(&mut server, "textDocument/definition", at(&uri, 0, 20));
    assert_eq!(reply["result"]["uri"], dir.uri("img.png"));
    let reply = request(&mut server, "textDocument/definition", at(&uri, 0, 32));
    assert_eq!(reply["result"], Value::Null);
}

#[test]
fn rename_moves_the_note_and_rewrites_its_links() {
    let dir = Dir::new(
        "rename",
        &[
            ("old.md", "# Old\n"),
            ("a.md", "see [[old]] here\n"),
            ("c.md", "and [x](old.md)\n"),
        ],
    );
    let mut server = server(&dir);
    let uri = dir.uri("a.md");
    open(&mut server, &uri, "see [[old]] here\n");
    let reply = request(&mut server, "textDocument/prepareRename", at(&uri, 0, 6));
    assert_eq!(reply["result"]["placeholder"], "old");

    let mut params = at(&uri, 0, 6);
    params["newName"] = json!("notes/new");
    let reply = request(&mut server, "textDocument/rename", params.clone());
    let changes = reply["result"]["documentChanges"].as_array().unwrap();
    assert_eq!(changes.len(), 3, "{changes:?}");
    // The open document carries its version; the edit is only the changed text.
    assert_eq!(
        changes[0]["textDocument"],
        json!({ "uri": uri, "version": 1 })
    );
    assert_eq!(changes[0]["edits"][0]["newText"], "new");
    assert_eq!(
        changes[0]["edits"][0]["range"],
        json!({ "start": { "line": 0, "character": 6 }, "end": { "line": 0, "character": 9 } })
    );
    assert_eq!(changes[1]["textDocument"]["version"], Value::Null);
    assert_eq!(changes[1]["edits"][0]["newText"], "notes/new");
    assert_eq!(
        changes[2],
        json!({ "kind": "rename", "oldUri": dir.uri("old.md"), "newUri": dir.uri("notes/new.md") })
    );
    // Nothing is written: the editor applies the edit.
    assert!(dir.0.join("old.md").exists());

    // Unsaved text in a note the move rewrites would be lost.
    server.handle(&json!({
        "jsonrpc": "2.0", "method": "textDocument/didChange",
        "params": { "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": "see [[old]] here, edited\n" }] },
    }));
    let reply = request(&mut server, "textDocument/rename", params);
    assert_eq!(reply["error"]["message"], "save a first");
}

#[test]
fn rename_needs_an_editor_that_renames_files() {
    let dir = Dir::new("rename-caps", &[("old.md", ""), ("a.md", "[[old]]")]);
    let mut server = server(&dir);
    server.renames_files = false;
    let uri = dir.uri("a.md");
    open(&mut server, &uri, "[[old]]");
    let mut params = at(&uri, 0, 3);
    params["newName"] = json!("new");
    let reply = request(&mut server, "textDocument/rename", params);
    assert_eq!(reply["error"]["code"], -32803);
}

#[test]
fn the_server_speaks_framed_messages_until_exit() {
    let dir = Dir::new("framed", &[("a.md", "")]);
    let frame = |v: Value| {
        let body = v.to_string();
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    };
    let input = [
        frame(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "textDocument/definition", "params": {} }),
        ),
        frame(json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize",
                      "params": { "rootUri": path_uri(&dir.0), "capabilities": {
                          "general": { "positionEncodings": ["utf-8", "utf-16"] } } } })),
        frame(json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" })),
        frame(json!({ "jsonrpc": "2.0", "method": "exit" })),
        frame(json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" })),
    ]
    .concat();
    let mut output = Vec::new();
    serve(None, input.as_bytes(), &mut output).unwrap();
    let mut replies = Vec::new();
    let mut rest = output.as_slice();
    while let Some(body) = read_message(&mut rest).unwrap() {
        replies.push(serde_json::from_slice::<Value>(&body).unwrap());
    }
    // Nothing after `exit` is answered.
    assert_eq!(replies.len(), 3, "{replies:?}");
    assert_eq!(replies[0]["error"]["code"], -32002);
    assert_eq!(
        replies[1]["result"]["capabilities"]["positionEncoding"],
        "utf-8"
    );
    assert_eq!(replies[2]["result"], Value::Null);
}

#[test]
fn positions_convert_in_both_encodings() {
    // `😀` is two UTF-16 units and four bytes.
    let text = "a😀b\r\nc";
    let pos = |line: u32, character: u32| json!({ "line": line, "character": character });
    assert_eq!(offset(text, &pos(0, 3), false), 5);
    assert_eq!(offset(text, &pos(0, 5), true), 5);
    // Past the line's end stops before its `\r\n`; past the last line is the end.
    assert_eq!(offset(text, &pos(0, 99), false), 6);
    assert_eq!(offset(text, &pos(1, 0), false), 8);
    assert_eq!(offset(text, &pos(9, 0), false), text.len());
    assert_eq!(position(text, 5, false), pos(0, 3));
    assert_eq!(position(text, 5, true), pos(0, 5));
    assert_eq!(position(text, 8, false), pos(1, 0));
}

#[test]
fn uris_round_trip_with_escapes() {
    let path = Path::new("/home/me/my notes/a#1.md");
    let uri = path_uri(path);
    assert_eq!(uri, "file:///home/me/my%20notes/a%231.md");
    assert_eq!(uri_path(&uri).unwrap(), path);
    assert_eq!(uri_path("file://localhost/x").unwrap(), Path::new("/x"));
    assert_eq!(uri_path("file://host/x"), None);
    assert_eq!(uri_path("untitled:1"), None);
}

#[test]
fn the_difference_is_one_range_at_char_boundaries() {
    assert_eq!(difference("see [[old]] x", "see [[new]] x"), (6, 9, "new"));
    assert_eq!(difference("abc", "abc"), (3, 3, ""));
    assert_eq!(difference("aé", "aè"), (1, 3, "è"));
    // A shared tail is not counted twice when one text is a prefix of the other.
    assert_eq!(difference("aa", "aaa"), (2, 2, "a"));
}
