//! `katib lsp`: a language server for a katib vault, so other editors get wiki-link completion,
//! go to definition, broken links as diagnostics, and rename as a move that rewrites links.
//! Base protocol: JSON-RPC with `Content-Length` headers over stdio.
//! Spec: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use katib_core::complete::{completion_edit, wiki_query};
use katib_core::{Index, LinkKind, Target, Vault, heading_offset, move_note_edits, parse};
use serde_json::{Value, json};

const COMPLETIONS: usize = 50;

/// Serves one editor until it sends `exit` or closes `input`. `root` is the vault; without it,
/// the workspace the editor opens.
pub fn serve(root: Option<PathBuf>, mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut server = Server {
        root,
        ..Server::default()
    };
    while let Some(body) = read_message(&mut input)? {
        let message: Value = match serde_json::from_slice(&body) {
            Ok(m) => m,
            Err(e) => {
                let reply = error(Value::Null, -32700, &format!("parse error: {e}"));
                write_message(&mut output, &reply)?;
                continue;
            }
        };
        for reply in server.handle(&message) {
            write_message(&mut output, &reply)?;
        }
        if server.exit {
            break;
        }
    }
    Ok(())
}

#[derive(Default)]
struct Server {
    root: Option<PathBuf>,
    vault: Option<(Vault, Index)>,
    /// Open documents by URI: their version and text.
    docs: HashMap<String, (i64, String)>,
    /// Positions count UTF-8 bytes, when the editor offers it; else UTF-16 units, the default.
    utf8: bool,
    /// The editor applies file renames in a workspace edit.
    renames_files: bool,
    exit: bool,
}

impl Server {
    /// The replies and notifications for one message.
    fn handle(&mut self, message: &Value) -> Vec<Value> {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = message.get("id").cloned() else {
            return self.notify(method, &params);
        };
        if method.is_empty() {
            // A response to a request of ours; none are sent.
            return Vec::new();
        }
        if self.vault.is_none() && method != "initialize" && method != "shutdown" {
            return vec![error(id, -32002, "not initialized")];
        }
        let result = match method {
            "initialize" => self.initialize(&params),
            "shutdown" => Ok(Value::Null),
            "textDocument/completion" => self.completion(&params),
            "textDocument/definition" => self.definition(&params),
            "textDocument/prepareRename" => self.prepare_rename(&params),
            "textDocument/rename" => self.rename(&params),
            _ => return vec![error(id, -32601, &format!("no method {method}"))],
        };
        match result {
            Ok(result) => vec![json!({ "jsonrpc": "2.0", "id": id, "result": result })],
            // RequestFailed: the request was understood and could not be done.
            Err(e) => vec![error(id, -32803, &format!("{e:#}"))],
        }
    }

    fn notify(&mut self, method: &str, params: &Value) -> Vec<Value> {
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        match method {
            "exit" => {
                self.exit = true;
                Vec::new()
            }
            "textDocument/didOpen" => {
                let doc = &params["textDocument"];
                let text = doc["text"].as_str().unwrap_or("").to_owned();
                self.docs
                    .insert(uri.clone(), (doc["version"].as_i64().unwrap_or(0), text));
                self.sync();
                self.diagnose(&uri).into_iter().collect()
            }
            "textDocument/didChange" => {
                // Full sync: the last change holds the whole text.
                let changes = params["contentChanges"].as_array();
                let Some(text) = changes.and_then(|c| c.last()?["text"].as_str()) else {
                    return Vec::new();
                };
                let version = params["textDocument"]["version"].as_i64().unwrap_or(0);
                self.docs.insert(uri.clone(), (version, text.to_owned()));
                self.diagnose(&uri).into_iter().collect()
            }
            "textDocument/didSave" => {
                // A save can create or rename what other open notes link to.
                self.sync();
                let uris: Vec<String> = self.docs.keys().cloned().collect();
                uris.iter().filter_map(|u| self.diagnose(u)).collect()
            }
            "textDocument/didClose" => {
                self.docs.remove(&uri);
                vec![diagnostics(&uri, Vec::new())]
            }
            _ => Vec::new(),
        }
    }

    fn initialize(&mut self, params: &Value) -> Result<Value> {
        let root = match &self.root {
            Some(root) => root.clone(),
            None => {
                workspace(params).context("no vault: open a folder, or run `katib lsp <vault>`")?
            }
        };
        // Absolute, as the editor's URIs are.
        let root = std::path::absolute(&root)?;
        let vault = Vault::open(&root)?;
        let mut index = Index::open(&vault)?;
        index.sync(&vault)?;
        self.root = Some(root);
        self.vault = Some((vault, index));
        let caps = &params["capabilities"];
        let encodings = caps["general"]["positionEncodings"].as_array();
        self.utf8 = encodings.is_some_and(|e| e.iter().any(|v| v == "utf-8"));
        let edit = &caps["workspace"]["workspaceEdit"];
        self.renames_files = edit["documentChanges"] == true
            && edit["resourceOperations"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(|o| o == "rename"));
        Ok(json!({
            "capabilities": {
                "positionEncoding": if self.utf8 { "utf-8" } else { "utf-16" },
                "textDocumentSync": { "openClose": true, "change": 1, "save": {} },
                "completionProvider": { "triggerCharacters": ["["] },
                "definitionProvider": true,
                "renameProvider": { "prepareProvider": true },
            },
            "serverInfo": { "name": "katib", "version": env!("CARGO_PKG_VERSION") },
        }))
    }

    /// Picks up edits made outside the editor. A failure leaves the index as it was; requests
    /// still answer from it.
    fn sync(&mut self) {
        if let Some((vault, index)) = &mut self.vault {
            let _ = index.sync(vault);
        }
    }

    fn vault(&self) -> &(Vault, Index) {
        self.vault.as_ref().expect("checked before each request")
    }

    /// The note id a URI names, if it is a note in the vault.
    fn note_id(&self, uri: &str) -> Option<String> {
        let root = self.root.as_ref()?;
        let path = uri_path(uri)?;
        let rel = path
            .strip_prefix(root)
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| {
                let (root, path) = (root.canonicalize().ok()?, path.canonicalize().ok()?);
                path.strip_prefix(root).ok().map(Path::to_path_buf)
            })?;
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let id = parts.join("/").strip_suffix(".md")?.to_owned();
        // `entry` refuses hidden and climbing ids, which are no notes.
        let entry = self.vault().0.entry(&id).ok()?;
        Some(entry.id)
    }

    fn note_uri(&self, id: &str) -> String {
        path_uri(&self.vault().0.root().join(format!("{id}.md")))
    }

    /// A note's text: the editor's, while it is open, else the file's.
    fn text_of(&self, id: &str) -> Option<String> {
        let uri = self.note_uri(id);
        match self.docs.get(&uri) {
            Some((_, text)) => Some(text.clone()),
            None => std::fs::read_to_string(self.vault().0.entry(id).ok()?.path).ok(),
        }
    }

    /// The open document a request names, its note id, and the byte offset of its position.
    fn at(&self, params: &Value) -> Option<(String, &str, usize)> {
        let uri = params["textDocument"]["uri"].as_str()?;
        let id = self.note_id(uri)?;
        let (_, text) = self.docs.get(uri)?;
        let offset = offset(text, &params["position"], self.utf8);
        Some((id, text, offset))
    }

    fn diagnose(&self, uri: &str) -> Option<Value> {
        let (id, (_, text)) = (self.note_id(uri)?, self.docs.get(uri)?);
        let (vault, index) = self.vault();
        let mut out = Vec::new();
        for link in parse(text).links {
            if link.kind == LinkKind::External {
                continue;
            }
            let message = match index.resolve(&id, &link).ok()? {
                Target::Missing(id) => format!("no note {id}"),
                Target::Ambiguous(ids) => format!("{} notes match: {}", ids.len(), ids.join(", ")),
                Target::File(rel) if !vault.root().join(&rel).exists() => format!("no file {rel}"),
                Target::Invalid => "leads outside the vault".to_owned(),
                _ => continue,
            };
            out.push(json!({
                "range": range(text, link.range.start, link.range.end, self.utf8),
                "severity": 2,
                "source": "katib",
                "message": message,
            }));
        }
        Some(diagnostics(uri, out))
    }

    fn completion(&mut self, params: &Value) -> Result<Value> {
        self.sync();
        let Some((_, text, head)) = self.at(params) else {
            return Ok(Value::Null);
        };
        let Some((start, query)) = wiki_query(text, head) else {
            return Ok(Value::Null);
        };
        let items: Vec<Value> = self
            .vault()
            .1
            .complete(query, COMPLETIONS)?
            .into_iter()
            .map(|s| {
                let (edit, new) = completion_edit(text, start, head, &s.reference);
                json!({
                    "label": s.title,
                    "detail": s.id,
                    "filterText": format!("{} {}", s.title, s.id),
                    "textEdit": {
                        "range": range(text, edit.start, edit.end, self.utf8),
                        "newText": new,
                    },
                })
            })
            .collect();
        // At most `COMPLETIONS` per query: ask again as the query grows.
        Ok(json!({ "isIncomplete": true, "items": items }))
    }

    fn definition(&mut self, params: &Value) -> Result<Value> {
        self.sync();
        let Some((id, text, offset)) = self.at(params) else {
            return Ok(Value::Null);
        };
        let Some(link) = parse(text)
            .links
            .into_iter()
            .find(|l| l.range.contains(&offset))
        else {
            return Ok(Value::Null);
        };
        let (vault, index) = self.vault();
        Ok(match index.resolve(&id, &link)? {
            Target::Note(target) => {
                let body = self.text_of(&target).unwrap_or_default();
                let at = link
                    .fragment
                    .as_deref()
                    .and_then(|f| heading_offset(&body, f))
                    .unwrap_or(0);
                json!({ "uri": self.note_uri(&target), "range": range(&body, at, at, self.utf8) })
            }
            Target::File(rel) if vault.root().join(&rel).exists() => json!({
                "uri": path_uri(&vault.root().join(&rel)),
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
            }),
            _ => Value::Null,
        })
    }

    /// The link under the position and the note it reaches.
    fn link_target(&self, params: &Value) -> Result<Option<(usize, usize, String)>> {
        let Some((id, text, offset)) = self.at(params) else {
            return Ok(None);
        };
        let Some(link) = parse(text)
            .links
            .into_iter()
            .find(|l| l.range.contains(&offset))
        else {
            return Ok(None);
        };
        Ok(match self.vault().1.resolve(&id, &link)? {
            Target::Note(target) => Some((link.range.start, link.range.end, target)),
            _ => None,
        })
    }

    fn prepare_rename(&mut self, params: &Value) -> Result<Value> {
        self.sync();
        let Some((start, end, target)) = self.link_target(params)? else {
            bail!("rename works on a link to a note");
        };
        let (_, text, _) = self.at(params).expect("found the link");
        Ok(json!({ "range": range(text, start, end, self.utf8), "placeholder": target }))
    }

    /// Renaming a link's note moves it: the edits rewrite every link to it, and the file is
    /// renamed, all applied by the editor so its open buffers stay its own.
    fn rename(&mut self, params: &Value) -> Result<Value> {
        if !self.renames_files {
            bail!("this editor cannot rename files through LSP");
        }
        self.sync();
        let Some((_, _, from)) = self.link_target(params)? else {
            bail!("rename works on a link to a note");
        };
        let to = params["newName"].as_str().unwrap_or("").trim();
        let to = to.strip_suffix(".md").unwrap_or(to);
        let (vault, index) = self.vault();
        let edits: BTreeMap<String, (String, String)> = move_note_edits(vault, index, &from, to)?;
        let to = vault.entry(to)?.id;
        let mut changes = Vec::new();
        for (id, (old, new)) in &edits {
            let uri = self.note_uri(id);
            let version = match self.docs.get(&uri) {
                // The edits were made from the file; unsaved text would be lost.
                Some((_, text)) if text != old => bail!("save {id} first"),
                Some((version, _)) => json!(version),
                None => Value::Null,
            };
            let (start, end, text) = difference(old, new);
            changes.push(json!({
                "textDocument": { "uri": uri, "version": version },
                "edits": [{ "range": range(old, start, end, self.utf8), "newText": text }],
            }));
        }
        if from != to {
            changes.push(json!({
                "kind": "rename",
                "oldUri": self.note_uri(&from),
                "newUri": self.note_uri(&to),
            }));
        }
        Ok(json!({ "documentChanges": changes }))
    }
}

/// The vault the editor opened: the first workspace folder, else the root URI.
fn workspace(params: &Value) -> Option<PathBuf> {
    let folder = params["workspaceFolders"]
        .get(0)
        .and_then(|f| f["uri"].as_str());
    uri_path(folder.or(params["rootUri"].as_str())?)
}

fn diagnostics(uri: &str, items: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": items },
    })
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The one range that turns `old` into `new`: what lies between their common prefix and suffix,
/// so an editor keeps its cursor and marks outside it.
fn difference<'a>(old: &str, new: &'a str) -> (usize, usize, &'a str) {
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let most = old.len().min(new.len()) - start;
    let mut tail = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(most)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - tail) || !new.is_char_boundary(new.len() - tail) {
        tail -= 1;
    }
    (start, old.len() - tail, &new[start..new.len() - tail])
}

/// The byte offset of an LSP position. A line past the end is the end; a character past the
/// line's end is the line's end, as the spec asks.
fn offset(text: &str, position: &Value, utf8: bool) -> usize {
    let line = position["line"].as_u64().unwrap_or(0) as usize;
    let character = position["character"].as_u64().unwrap_or(0) as usize;
    let Some(start) = line_start(text, line) else {
        return text.len();
    };
    let rest = &text[start..];
    let line_text = &rest[..rest.find('\n').unwrap_or(rest.len())];
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
    let mut units = 0;
    for (i, c) in line_text.char_indices() {
        if units >= character {
            return start + i;
        }
        units += if utf8 { c.len_utf8() } else { c.len_utf16() };
    }
    start + line_text.len()
}

fn line_start(text: &str, line: usize) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    text.match_indices('\n').nth(line - 1).map(|(i, _)| i + 1)
}

fn position(text: &str, offset: usize, utf8: bool) -> Value {
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let part = &before[start..];
    let character = if utf8 {
        part.len()
    } else {
        part.encode_utf16().count()
    };
    json!({ "line": line, "character": character })
}

fn range(text: &str, start: usize, end: usize, utf8: bool) -> Value {
    json!({ "start": position(text, start, utf8), "end": position(text, end, utf8) })
}

/// The path of a `file:` URI.
fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path` names another machine; only the empty host is this one.
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let path = katib_core::paths::percent_decode(path);
    // `/C:/notes` on Windows.
    if cfg!(windows) && path.as_bytes().get(2) == Some(&b':') {
        return Some(PathBuf::from(&path[1..]));
    }
    Some(PathBuf::from(path))
}

/// The `file:` URI of `path`, escaping all but unreserved characters and `/`.
fn path_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !path.starts_with('/') {
        out.push('/');
    }
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            // A drive letter's colon stays, as editors write it.
            b':' if cfg!(windows) => out.push(':'),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One message's body; `None` once `input` ends.
fn read_message(input: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let Some(length) = length else {
        bail!("a message without Content-Length");
    };
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<()> {
    let body = message.to_string();
    write!(output, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests;
