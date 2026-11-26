//! `katib mcp`: the vault tools over the Model Context Protocol, for agents outside katib. The
//! stdio transport: one JSON-RPC message per line in, one per line out. Only tools are offered.
//! Spec: https://modelcontextprotocol.io/specification/2025-06-18

use std::io::{BufRead, Write};

use anyhow::Result;
use minima_core::Cancel;
use minima_core::tools::Tools;
use serde_json::{Value, json};

/// Newest first; the first is offered to a client asking for one not listed.
const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Tools for a katib vault: a folder of markdown notes. A note is named \
by its id, its path in the vault without `.md`, such as `projects/katib`. Notes link with \
`[[Title]]` or `[[id#Heading|label]]` wiki links and with relative markdown links; `move` rewrites \
them, so use it rather than writing a copy. Replacing a note requires reading it first, and fails \
if it changed since.";

/// Answers requests from `input` on `output` until `input` ends.
pub fn serve(tools: &dyn Tools, input: impl BufRead, mut output: impl Write) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = answer(tools, &runtime, &line) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// The reply to one message; none for a notification or a response.
fn answer(tools: &dyn Tools, runtime: &tokio::runtime::Runtime, line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => return Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").cloned();
    let (Some(method), Some(id)) = (method, id) else {
        return None;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => initialize(&params),
        "ping" => json!({}),
        "tools/list" => list(tools),
        "tools/call" => match call(tools, runtime, &params) {
            Ok(result) => result,
            Err(e) => return Some(error(id, -32602, &e)),
        },
        _ => return Some(error(id, -32601, &format!("no method {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| VERSIONS.contains(v))
        .unwrap_or(VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "katib", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn list(tools: &dyn Tools) -> Value {
    let tools: Vec<Value> = tools
        .specs()
        .into_iter()
        .map(|s| json!({ "name": s.name, "description": s.description, "inputSchema": s.parameters }))
        .collect();
    json!({ "tools": tools })
}

/// A tool's failure is a result the model reads (`isError`); only a malformed request is an error.
fn call(
    tools: &dyn Tools,
    runtime: &tokio::runtime::Runtime,
    params: &Value,
) -> Result<Value, String> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err("tools/call needs a `name`".into());
    };
    if !tools.specs().iter().any(|s| s.name == name) {
        return Err(format!("no tool {name}"));
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let outcome = runtime.block_on(tools.call(name, &arguments.to_string(), &Cancel::new()));
    let (text, failed) = match outcome {
        Ok(outcome) => (outcome.body, false),
        Err(e) => (format!("{e:#}"), true),
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": failed }))
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use katib_core::Vault;
    use minima_core::tools::Scratch;

    use super::*;
    use crate::VaultTools;

    /// The replies to `lines`, one request per line.
    fn exchange(tools: &dyn Tools, lines: &[Value]) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut output = Vec::new();
        serve(tools, input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn request(id: u32, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn call(id: u32, name: &str, arguments: Value) -> Value {
        request(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    fn text(reply: &Value) -> (&str, bool) {
        let result = &reply["result"];
        (
            result["content"][0]["text"].as_str().unwrap(),
            result["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn a_client_initializes_lists_and_calls_the_tools() {
        let dir = Scratch::new("katib-mcp");
        std::fs::write(dir.file("a.md"), "# Alpha\n").unwrap();
        let tools = VaultTools::open(Vault::open(dir.file("")).unwrap()).unwrap();
        let replies = exchange(
            &tools,
            &[
                request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                request(2, "tools/list", json!({})),
                call(3, "read", json!({ "note": "a" })),
                call(4, "write", json!({ "note": "a", "content": "# Beta\n" })),
                call(5, "read", json!({ "note": "missing" })),
                call(6, "nope", json!({})),
                request(7, "resources/list", json!({})),
            ],
        );
        // The notification has no reply.
        assert_eq!(replies.len(), 7, "{replies:?}");
        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert!(replies[0]["result"]["capabilities"]["tools"].is_object());

        let listed = replies[1]["result"]["tools"].as_array().unwrap();
        let names: Vec<_> = listed.iter().map(|t| t["name"].as_str().unwrap()).collect();
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
        assert_eq!(listed[1]["inputSchema"]["type"], "object");

        assert_eq!(text(&replies[2]), ("# Alpha\n", false));
        assert_eq!(text(&replies[3]), ("replaced a", false));
        assert_eq!(
            std::fs::read_to_string(dir.file("a.md")).unwrap(),
            "# Beta\n"
        );
        let (missing, failed) = text(&replies[4]);
        assert!(
            failed && missing.starts_with("no note missing"),
            "{missing}"
        );
        assert_eq!(replies[5]["error"]["code"], -32602);
        assert_eq!(replies[6]["error"]["code"], -32601);
    }

    #[test]
    fn a_replacement_without_a_read_is_refused() {
        let dir = Scratch::new("katib-mcp-guard");
        std::fs::write(dir.file("a.md"), "mine\n").unwrap();
        let tools = VaultTools::open(Vault::open(dir.file("")).unwrap()).unwrap();
        let replies = exchange(
            &tools,
            &[call(
                1,
                "write",
                json!({ "note": "a", "content": "theirs\n" }),
            )],
        );
        let (why, failed) = text(&replies[0]);
        assert!(failed && why.contains("read it before"), "{why}");
        assert_eq!(std::fs::read_to_string(dir.file("a.md")).unwrap(), "mine\n");
    }

    #[test]
    fn an_unknown_version_gets_the_newest_and_bad_json_an_error() {
        let dir = Scratch::new("katib-mcp-version");
        let tools = VaultTools::open(Vault::open(dir.file("")).unwrap()).unwrap();
        let input = format!(
            "{}\nnot json\n",
            request(1, "initialize", json!({ "protocolVersion": "1999-01-01" }))
        );
        let mut output = Vec::new();
        serve(&tools, input.as_bytes(), &mut output).unwrap();
        let replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies[0]["result"]["protocolVersion"], VERSIONS[0]);
        assert_eq!(replies[1]["error"]["code"], -32700);
        assert_eq!(replies[1]["id"], Value::Null);
    }
}
