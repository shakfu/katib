//! An outside agent over the Agent Client Protocol, version 1
//! (https://agentclientprotocol.com/protocol/): katib starts the agent's command in the vault and
//! talks JSON-RPC to it, one message per line on its stdin and stdout. The agent's events reach
//! the panel as a built-in `Session`'s do.
//!
//! The agent's own tools are its own: ACP lets it ask the client to read and write files, and
//! katib answers those only inside the vault, but nothing makes the agent ask. Writes it asks for
//! are guarded against the text it last read through katib, as the built-in agent's are.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

use katib_core::{SaveError, Vault, save_guarded, write_atomic};
use minima_core::Cancel;
use serde_json::{Value, json};

use crate::session::{Event, Finish, PermissionOption, Session};

const VERSION: u64 = 1;
/// Lines of the agent's stderr kept, to say why it stopped.
const STDERR_LINES: usize = 20;

/// An outside agent: its command line, and presets for known ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub command: &'static str,
}

/// Commands as their projects document them. `npx` fetches the adapter on first use.
pub const PRESETS: [Preset; 3] = [
    Preset {
        id: "claude",
        name: "Claude Code",
        command: "npx -y @agentclientprotocol/claude-agent-acp",
    },
    Preset {
        id: "gemini",
        name: "Gemini CLI",
        command: "npx -y @google/gemini-cli --acp",
    },
    Preset {
        id: "minima",
        name: "minima",
        command: "minima --acp",
    },
];

/// A command line split into words. Double or single quotes keep spaces in a word; there is
/// no other escaping, and no shell runs it.
pub fn split_command(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("a quote is not closed".into());
    }
    if started {
        words.push(word);
    }
    if words.is_empty() {
        return Err("no command".into());
    }
    Ok(words)
}

/// An MCP server to offer the agent in each session, such as `katib mcp <vault>`.
#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
}

impl Session {
    /// Starts `command` in the vault and opens one ACP session there. A failure, such as a
    /// command that is not found or an agent that needs a sign-in, arrives as `Event::Failed`.
    pub fn acp(vault: Vault, command: Vec<String>, mcp: Option<McpServer>) -> Self {
        Self::spawn_with(move |inbox, answers, events, stop| {
            let root = vault.root().to_path_buf();
            let spawned = std::process::Command::new(&command[0])
                .args(&command[1..])
                .current_dir(&root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match spawned {
                Ok(child) => child,
                Err(e) => {
                    let _ = events.try_send(Event::Failed(format!("starting {}: {e}", command[0])));
                    return;
                }
            };
            let stderr = keep_stderr(&mut child);
            let incoming = read_lines(child.stdout.take().expect("piped"));
            let stdin = child.stdin.take().expect("piped");
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = events.try_send(Event::Failed(format!("starting the agent: {e}")));
                    return;
                }
            };
            let client = Client::new(&root, Box::new(stdin), events.clone(), mcp);
            let ended = runtime.block_on(client.run(incoming, inbox, answers, stop));
            stop_child(&mut child);
            if let Err(why) = ended {
                let tail = stderr
                    .lock()
                    .map(|l| Vec::from(l.clone()).join("\n"))
                    .unwrap_or_default();
                let why = match tail.trim() {
                    "" => why,
                    tail => format!("{why}\n{tail}"),
                };
                let _ = events.try_send(Event::Failed(why));
            }
        })
    }
}

/// Closing stdin asks the agent to stop; it is then killed, as it may not.
fn stop_child(child: &mut Child) {
    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();
}

/// The agent's stderr, echoed to katib's and its last lines kept.
fn keep_stderr(child: &mut Child) -> Arc<Mutex<VecDeque<String>>> {
    let lines = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(stderr) = child.stderr.take() {
        let lines = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("agent: {line}");
                if let Ok(mut lines) = lines.lock() {
                    if lines.len() == STDERR_LINES {
                        lines.pop_front();
                    }
                    lines.push_back(line);
                }
            }
        });
    }
    lines
}

/// Each JSON line of `reader`, on a thread; the channel closes when the agent's stdout does.
pub(crate) fn read_lines(
    reader: impl std::io::Read + Send + 'static,
) -> async_channel::Receiver<Value> {
    let (sender, receiver) = async_channel::unbounded();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            match serde_json::from_str(&line) {
                Ok(message) => {
                    if sender.send_blocking(message).is_err() {
                        break;
                    }
                }
                Err(e) => eprintln!("agent: not JSON ({e}): {line}"),
            }
        }
    });
    receiver
}

/// What a request of ours is waiting for.
enum Waiting {
    Initialize,
    NewSession,
    Prompt,
}

pub(crate) struct Client {
    out: Box<dyn Write>,
    events: async_channel::Sender<Event>,
    fs: Fs,
    mcp: Option<McpServer>,
    next_id: u64,
    waiting: HashMap<u64, Waiting>,
    session: Option<String>,
    /// The prompt sent and not yet answered, and whether its cancel was sent.
    running: Option<(Cancel, bool)>,
    queued: VecDeque<(String, Cancel)>,
    /// Permission requests the panel has not answered: the agent's request id, by ours.
    asked: HashMap<u64, Value>,
    next_ask: u64,
}

impl Client {
    pub(crate) fn new(
        root: &Path,
        out: Box<dyn Write>,
        events: async_channel::Sender<Event>,
        mcp: Option<McpServer>,
    ) -> Self {
        Self {
            out,
            events,
            fs: Fs::new(root),
            mcp,
            next_id: 0,
            waiting: HashMap::new(),
            session: None,
            running: None,
            queued: VecDeque::new(),
            asked: HashMap::new(),
            next_ask: 0,
        }
    }

    /// Serves until the session stops, `Ok`, or the agent fails or exits, `Err` with why.
    pub(crate) async fn run(
        mut self,
        incoming: async_channel::Receiver<Value>,
        prompts: async_channel::Receiver<(String, Cancel)>,
        answers: async_channel::Receiver<(u64, Option<String>)>,
        stop: Cancel,
    ) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": false,
                },
                "clientInfo": { "name": "katib", "version": env!("CARGO_PKG_VERSION") },
            }),
            Waiting::Initialize,
        )?;
        loop {
            let running = self
                .running
                .as_ref()
                .filter(|(_, sent)| !sent)
                .map(|(c, _)| c.clone());
            tokio::select! {
                biased;
                () = stop.cancelled() => return Ok(()),
                message = incoming.recv() => match message {
                    Ok(message) => self.handle(message)?,
                    Err(_) => return Err(self.exited()),
                },
                () = async { running.as_ref().expect("guarded").cancelled().await }, if running.is_some() => {
                    self.cancel()?;
                }
                answer = answers.recv() => if let Ok((ask, option)) = answer {
                    self.answer(ask, option)?;
                },
                prompt = prompts.recv() => match prompt {
                    Ok(prompt) => {
                        self.queued.push_back(prompt);
                        self.next_prompt()?;
                    }
                    Err(_) => return Ok(()),
                },
            }
        }
    }

    fn exited(&mut self) -> String {
        if self.running.take().is_some() {
            self.emit(Event::Done(Finish::Failed("the agent stopped".into())));
        }
        "the agent stopped".into()
    }

    fn emit(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.out
            .write_all(line.as_bytes())
            .and_then(|()| self.out.flush())
            .map_err(|e| format!("writing to the agent: {e}"))
    }

    fn request(&mut self, method: &str, params: Value, waiting: Waiting) -> Result<(), String> {
        self.next_id += 1;
        let id = self.next_id;
        self.waiting.insert(id, waiting);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
    }

    fn reply(&mut self, id: Value, result: Result<Value, (i64, String)>) -> Result<(), String> {
        let message = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => json!({
                "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message },
            }),
        };
        self.send(&message)
    }

    /// Sends the next queued prompt once the session is open and none runs. One cancelled while
    /// it waited ends without reaching the agent.
    fn next_prompt(&mut self) -> Result<(), String> {
        let Some(session) = self.session.clone() else {
            return Ok(());
        };
        while self.running.is_none() {
            let Some((text, cancel)) = self.queued.pop_front() else {
                return Ok(());
            };
            if cancel.is_cancelled() {
                self.emit(Event::Done(Finish::Cancelled));
                continue;
            }
            self.running = Some((cancel, false));
            self.request(
                "session/prompt",
                json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
                Waiting::Prompt,
            )?;
        }
        Ok(())
    }

    /// The running prompt was cancelled: tell the agent, which then answers it `cancelled`, and
    /// answer its open permission requests `cancelled`, as the protocol asks.
    fn cancel(&mut self) -> Result<(), String> {
        if let Some((_, sent)) = &mut self.running {
            *sent = true;
        }
        let session = self.session.clone().unwrap_or_default();
        self.send(&json!({
            "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session },
        }))?;
        let asked: Vec<u64> = self.asked.keys().copied().collect();
        for ask in asked {
            self.answer(ask, None)?;
        }
        Ok(())
    }

    /// The panel's answer to permission request `ask`: an option id, or `None` to cancel.
    fn answer(&mut self, ask: u64, option: Option<String>) -> Result<(), String> {
        let Some(id) = self.asked.remove(&ask) else {
            return Ok(());
        };
        let outcome = match option {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.emit(Event::Answered(ask));
        self.reply(id, Ok(json!({ "outcome": outcome })))
    }

    fn handle(&mut self, message: Value) -> Result<(), String> {
        let method = message["method"].as_str().map(String::from);
        let id = message.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => self.agent_request(&method, id, &message["params"]),
            (Some(method), None) => {
                if method == "session/update" {
                    self.update(&message["params"]["update"]);
                }
                Ok(())
            }
            (None, Some(id)) => self.response(&id, &message),
            (None, None) => Ok(()),
        }
    }

    fn agent_request(&mut self, method: &str, id: Value, params: &Value) -> Result<(), String> {
        match method {
            "session/request_permission" => {
                self.next_ask += 1;
                let ask = self.next_ask;
                self.asked.insert(ask, id);
                let call = &params["toolCall"];
                let options = params["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| {
                        Some(PermissionOption {
                            id: o["optionId"].as_str()?.to_owned(),
                            name: o["name"].as_str().unwrap_or_default().to_owned(),
                            allow: o["kind"].as_str().is_some_and(|k| k.starts_with("allow")),
                        })
                    })
                    .collect();
                self.emit(Event::Permission {
                    ask,
                    title: call["title"].as_str().unwrap_or("a tool call").to_owned(),
                    note: self.fs.note(&call["locations"]),
                    options,
                });
                // Asked after the prompt was cancelled: there is no one to ask.
                if self.running.as_ref().is_some_and(|(_, sent)| *sent) {
                    self.answer(ask, None)?;
                }
                Ok(())
            }
            "fs/read_text_file" => {
                let result = self
                    .fs
                    .read(params)
                    .map(|content| json!({ "content": content }));
                self.reply(id, result.map_err(|e| (-32602, e)))
            }
            "fs/write_text_file" => {
                let result = self.fs.write(params).map(|()| Value::Null);
                self.reply(id, result.map_err(|e| (-32602, e)))
            }
            _ => self.reply(id, Err((-32601, format!("katib does not offer {method}")))),
        }
    }

    fn response(&mut self, id: &Value, message: &Value) -> Result<(), String> {
        let Some(waiting) = id.as_u64().and_then(|id| self.waiting.remove(&id)) else {
            return Ok(());
        };
        let error = message.get("error").map(|e| {
            let text = e["message"].as_str().unwrap_or("an error");
            // `auth_required`: the agent's own sign-in has not been done.
            match e["code"].as_i64() {
                Some(-32000) => {
                    format!("{text}. Sign in by running the agent in a terminal first.")
                }
                _ => text.to_owned(),
            }
        });
        let result = &message["result"];
        match waiting {
            Waiting::Initialize => {
                if let Some(e) = error {
                    return Err(format!("the agent refused to start: {e}"));
                }
                if result["protocolVersion"].as_u64() != Some(VERSION) {
                    return Err(format!(
                        "the agent speaks ACP version {}; katib speaks {VERSION}",
                        result["protocolVersion"]
                    ));
                }
                let info = &result["agentInfo"];
                let name = info["title"]
                    .as_str()
                    .or(info["name"].as_str())
                    .unwrap_or("agent");
                self.fs.agent = name.to_owned();
                let servers: Vec<Value> = self
                    .mcp
                    .iter()
                    .map(|m| json!({ "name": m.name, "command": m.command, "args": m.args, "env": [] }))
                    .collect();
                self.request(
                    "session/new",
                    json!({ "cwd": self.fs.root, "mcpServers": servers }),
                    Waiting::NewSession,
                )
            }
            Waiting::NewSession => {
                if let Some(e) = error {
                    return Err(format!("the agent could not open a session: {e}"));
                }
                let Some(session) = result["sessionId"].as_str() else {
                    return Err("the agent opened a session without an id".into());
                };
                self.session = Some(session.to_owned());
                self.emit(Event::Ready {
                    provider: self.fs.agent.clone(),
                    model: String::new(),
                    context: 0,
                    cost_estimated: false,
                });
                self.next_prompt()
            }
            Waiting::Prompt => {
                let cancelled = self.running.take().is_some_and(|(c, _)| c.is_cancelled());
                let finish = match (error, result["stopReason"].as_str()) {
                    (_, _) if cancelled => Finish::Cancelled,
                    (Some(e), _) => Finish::Failed(e),
                    (None, Some("end_turn")) => Finish::Answered,
                    (None, Some("cancelled")) => Finish::Cancelled,
                    (None, Some("refusal")) => Finish::Refused,
                    (None, Some("max_tokens")) => {
                        Finish::Failed("the answer reached the model's output limit".into())
                    }
                    (None, Some("max_turn_requests")) => {
                        Finish::Failed("the agent reached its limit of turns".into())
                    }
                    (None, other) => Finish::Failed(format!("the agent stopped: {other:?}")),
                };
                self.emit(Event::Done(finish));
                self.next_prompt()
            }
        }
    }

    fn update(&mut self, update: &Value) {
        let id = || update["toolCallId"].as_str().unwrap_or_default().to_owned();
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                if let Some(text) = update["content"]["text"].as_str() {
                    self.emit(Event::Text(text.to_owned()));
                }
            }
            Some("tool_call") => {
                self.emit(Event::ToolStart {
                    id: id(),
                    name: update["title"].as_str().unwrap_or("tool").to_owned(),
                    note: self.fs.note(&update["locations"]),
                    detail: None,
                });
                self.tool_end(update);
            }
            Some("tool_call_update") => self.tool_end(update),
            Some("usage_update") => {
                let cost = &update["cost"];
                self.emit(Event::Totals {
                    used: update["used"].as_u64().unwrap_or(0) as u32,
                    size: update["size"].as_u64().unwrap_or(0) as u32,
                    cost: (cost["currency"] == "USD")
                        .then(|| cost["amount"].as_f64())
                        .flatten(),
                });
            }
            _ => {}
        }
    }

    /// A call that has finished, with the first text it returned.
    fn tool_end(&self, update: &Value) {
        let ok = match update["status"].as_str() {
            Some("completed") => true,
            Some("failed") => false,
            _ => return,
        };
        let body = update["content"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|c| c["content"]["text"].as_str())
            .unwrap_or_default()
            .to_owned();
        self.emit(Event::ToolEnd {
            id: update["toolCallId"].as_str().unwrap_or_default().to_owned(),
            body,
            note: None,
            ok,
        });
    }
}

/// The agent's file requests, answered inside the vault only.
struct Fs {
    root: PathBuf,
    /// The agent's name, for messages.
    agent: String,
    /// Each file's text as the agent last read or wrote it through katib.
    seen: HashMap<PathBuf, String>,
}

impl Fs {
    fn new(root: &Path) -> Self {
        Self {
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            agent: "agent".into(),
            seen: HashMap::new(),
        }
    }

    /// `path` resolved, if it is in the vault and not under a hidden folder such as `.git`.
    /// Symbolic links are followed before the check, so one cannot lead out.
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return Err(format!("{} is not an absolute path", path.display()));
        }
        // Past the part that exists, `..` is not resolved, so it could climb out unseen.
        if path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(format!("{} climbs with ..", path.display()));
        }
        let full = canonical_ahead(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let rel = full
            .strip_prefix(&self.root)
            .map_err(|_| format!("{} is outside the vault", path.display()))?;
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            return Err(format!("{} is in a hidden folder", path.display()));
        }
        Ok(full)
    }

    fn read(&mut self, params: &Value) -> Result<String, String> {
        let path = self.resolve(params["path"].as_str().unwrap_or_default())?;
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let line = params["line"].as_u64().map_or(1, |l| l.max(1)) as usize;
        let limit = params["limit"].as_u64().map(|l| l as usize);
        let part = match (line, limit) {
            (1, None) => text.clone(),
            _ => text
                .split_inclusive('\n')
                .skip(line - 1)
                .take(limit.unwrap_or(usize::MAX))
                .collect(),
        };
        self.seen.insert(path, text);
        Ok(part)
    }

    /// Writes the file. One the agent read through katib must still hold what it read; one it
    /// never read is written over, since it may have read it with its own tools.
    fn write(&mut self, params: &Value) -> Result<(), String> {
        let path = self.resolve(params["path"].as_str().unwrap_or_default())?;
        let content = params["content"].as_str().unwrap_or_default();
        let shown = path.display().to_string();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{shown}: {e}"))?;
        }
        match self.seen.get(&path) {
            Some(base) => save_guarded(&path, Some(base), content).map_err(|e| match e {
                SaveError::Conflict { .. } => {
                    format!("{shown} changed since you read it; read it again")
                }
                e => format!("{shown}: {e}"),
            })?,
            None => write_atomic(&path, content).map_err(|e| format!("{shown}: {e}"))?,
        }
        self.seen.insert(path, content.to_owned());
        Ok(())
    }

    /// The note a tool call's first location names, if it is one in the vault.
    fn note(&self, locations: &Value) -> Option<String> {
        let path = self.resolve(locations.get(0)?["path"].as_str()?).ok()?;
        let rel = path.strip_prefix(&self.root).ok()?;
        let parts: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        parts.join("/").strip_suffix(".md").map(String::from)
    }
}

/// `p` with its longest existing prefix canonicalized, so a file not yet written resolves.
fn canonical_ahead(p: &Path) -> std::io::Result<PathBuf> {
    let mut tail = Vec::new();
    let mut cur = p;
    loop {
        match cur.canonicalize() {
            Ok(c) => return Ok(tail.iter().rev().fold(c, |acc, part| acc.join(part))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (cur.file_name(), cur.parent()) else {
                    return Err(e);
                };
                tail.push(name.to_owned());
                cur = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests;
