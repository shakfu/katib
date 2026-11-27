//! Vault tools. Notes are named by id, never by path, so every read and write stays in the vault:
//! `Vault::entry` refuses absolute, hidden and climbing ids, and `entry` here refuses a symbolic
//! link on the way to the file.
//!
//! Writes go straight to disk through `save_guarded`. Replacing an existing note requires having
//! read it, and fails if the file changed since, so an edit made in the window or by sync is never
//! overwritten unseen. Calls are synchronous and short, so a cancel takes effect between them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use futures_util::future::LocalBoxFuture;
use katib_core::{Entry, Index, SaveError, Vault, move_note, save_guarded, trash_note};
use minima_core::Cancel;
use minima_core::tools::{Outcome, Spec, Tools, parse, schema_of};
use schemars::JsonSchema;
use serde::Deserialize;

const SEARCH_LIMIT: usize = 20;
const SEARCH_MAX: usize = 100;

pub struct VaultTools {
    vault: Vault,
    index: RefCell<Index>,
    /// Each note's text as this agent last read or wrote it: the base a replacement is guarded by.
    seen: RefCell<HashMap<String, String>>,
}

#[derive(Deserialize, JsonSchema)]
struct NoteArgs {
    /// Note id: its path in the vault without `.md`, such as `projects/katib`.
    note: String,
}

#[derive(Deserialize, JsonSchema)]
struct ListArgs {
    /// Only notes under this folder, such as `journal`. Omit for every note.
    #[serde(default)]
    folder: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Words that must all occur; the last may be a prefix.
    query: String,
    /// At most this many results. Default 20, at most 100.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// Note id, without `.md`.
    note: String,
    /// The whole new text of the note, in markdown.
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    /// Note id, without `.md`.
    note: String,
    /// Exact text to replace. It must occur exactly once in the note.
    old: String,
    /// Replacement text.
    new: String,
}

#[derive(Deserialize, JsonSchema)]
struct MoveArgs {
    /// Note id to move or rename.
    from: String,
    /// New note id.
    to: String,
}

impl VaultTools {
    /// Opens the vault's index and brings it up to date.
    pub fn open(vault: Vault) -> Result<Self> {
        let mut index = Index::open(&vault).context("opening the index")?;
        index.sync(&vault).context("indexing the vault")?;
        Ok(Self {
            vault,
            index: RefCell::new(index),
            seen: RefCell::new(HashMap::new()),
        })
    }

    /// The note `id` names, refused if the id leaves the vault or the path crosses a symbolic
    /// link, which could lead outside it. The file need not exist.
    fn entry(&self, id: &str) -> Result<Entry> {
        let entry = self.vault.entry(id)?;
        let rel = entry
            .path
            .strip_prefix(self.vault.root())
            .expect("an entry is under the root");
        let mut path = self.vault.root().to_path_buf();
        for part in rel.components() {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => bail!(
                    "{} passes through a symbolic link, which may lead outside the vault",
                    entry.id
                ),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) => return Err(e).with_context(|| format!("reading {}", entry.id)),
            }
        }
        Ok(entry)
    }

    /// The index after picking up changes made outside this agent.
    fn synced(&self) -> Result<std::cell::RefMut<'_, Index>> {
        let mut index = self.index.borrow_mut();
        index.sync(&self.vault).context("indexing the vault")?;
        Ok(index)
    }

    fn list(&self, args: ListArgs) -> Result<String> {
        let prefix = args
            .folder
            .map(|f| format!("{}/", f.trim_matches('/')))
            .filter(|p| p != "/");
        let notes = self.synced()?.notes()?;
        let lines: Vec<_> = notes
            .iter()
            .filter(|n| prefix.as_ref().is_none_or(|p| n.id.starts_with(p.as_str())))
            .map(|n| format!("{} - {}", n.id, n.title))
            .collect();
        Ok(match lines.is_empty() {
            true => "no notes".into(),
            false => lines.join("\n"),
        })
    }

    fn read(&self, args: NoteArgs) -> Result<String> {
        let entry = self.entry(&args.note)?;
        let text = match fs::read_to_string(&entry.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                bail!("no note {}; `list` or `search` finds notes", entry.id)
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", entry.id)),
        };
        self.seen.borrow_mut().insert(entry.id, text.clone());
        Ok(text)
    }

    fn search(&self, args: SearchArgs) -> Result<String> {
        let limit = args.limit.unwrap_or(SEARCH_LIMIT).clamp(1, SEARCH_MAX);
        let hits = self.synced()?.search(&args.query, limit)?;
        let lines: Vec<_> = hits
            .iter()
            .map(|h| format!("{} - {}\n  {}", h.id, h.title, h.snippet))
            .collect();
        Ok(match lines.is_empty() {
            true => "no matches".into(),
            false => lines.join("\n"),
        })
    }

    fn backlinks(&self, args: NoteArgs) -> Result<String> {
        let entry = self.entry(&args.note)?;
        let links = self.synced()?.backlinks(&entry.id)?;
        let lines: Vec<_> = links
            .iter()
            .map(|b| format!("{} - {}\n  {}", b.src, b.title, b.context))
            .collect();
        Ok(match lines.is_empty() {
            true => format!("no notes link to {}", entry.id),
            false => lines.join("\n"),
        })
    }

    fn write(&self, args: WriteArgs) -> Result<String> {
        let entry = self.entry(&args.note)?;
        let exists = exists(&entry.path)?;
        let base = match (exists, self.seen.borrow().get(&entry.id)) {
            (false, _) => None,
            (true, Some(seen)) => Some(seen.clone()),
            (true, None) => bail!("{} exists; read it before replacing it", entry.id),
        };
        self.save(&entry, base.as_deref(), args.content)?;
        Ok(match exists {
            true => format!("replaced {}", entry.id),
            false => format!("created {}", entry.id),
        })
    }

    fn edit(&self, args: EditArgs) -> Result<String> {
        if args.old.is_empty() {
            bail!("`old` is empty; give the exact text to replace");
        }
        let entry = self.entry(&args.note)?;
        let text =
            fs::read_to_string(&entry.path).with_context(|| format!("reading {}", entry.id))?;
        match text.matches(&args.old).count() {
            0 => bail!("the text to replace is not in {}; read it again", entry.id),
            1 => {}
            n => bail!(
                "the text to replace occurs {n} times in {}; include more of the text around it",
                entry.id
            ),
        }
        let new = text.replacen(&args.old, &args.new, 1);
        self.save(&entry, Some(&text), new)?;
        Ok(format!("edited {}", entry.id))
    }

    /// Writes `new` if the file holds `base`, then records it as seen and indexes it.
    fn save(&self, entry: &Entry, base: Option<&str>, new: String) -> Result<()> {
        match save_guarded(&entry.path, base, &new) {
            Ok(()) => {}
            Err(SaveError::Conflict { disk: Some(_) }) => {
                bail!("{} changed since you read it; read it again", entry.id)
            }
            Err(SaveError::Conflict { disk: None }) => {
                bail!("{} was deleted since you read it", entry.id)
            }
            Err(e) => return Err(e.into()),
        }
        self.index.borrow_mut().update(entry, &new)?;
        self.seen.borrow_mut().insert(entry.id.clone(), new);
        Ok(())
    }

    /// Moves a note to the trash, if it still holds what this agent read: a delete is never of
    /// an edit the agent has not seen. Links to it are left, and named, for the agent to fix.
    fn delete(&self, args: NoteArgs) -> Result<String> {
        let entry = self.entry(&args.note)?;
        let disk = match fs::read_to_string(&entry.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => bail!("no note {}", entry.id),
            Err(e) => return Err(e).with_context(|| format!("reading {}", entry.id)),
        };
        match self.seen.borrow().get(&entry.id) {
            None => bail!("read {} before deleting it", entry.id),
            Some(seen) if *seen != disk => {
                bail!("{} changed since you read it; read it again", entry.id)
            }
            Some(_) => {}
        }
        let mut linking: Vec<String> = self
            .synced()?
            .backlinks(&entry.id)?
            .into_iter()
            .map(|b| b.src)
            .filter(|src| *src != entry.id)
            .collect();
        linking.dedup();
        trash_note(&self.vault, &entry.id)?;
        self.seen.borrow_mut().remove(&entry.id);
        self.synced()?;
        Ok(match linking.is_empty() {
            true => format!("moved {} to the trash", entry.id),
            false => format!(
                "moved {} to the trash; links to it remain in {}",
                entry.id,
                linking.join(", ")
            ),
        })
    }

    fn move_note(&self, args: MoveArgs) -> Result<String> {
        let (from, to) = (self.entry(&args.from)?, self.entry(&args.to)?);
        let moved = {
            let index = self.synced()?;
            move_note(&self.vault, &index, &from.id, &to.id)?
        };
        self.synced()?;
        // Rewritten notes no longer hold the text this agent saw; a stale base would only fail.
        let mut seen = self.seen.borrow_mut();
        for (old, _) in &moved.notes {
            seen.remove(old);
        }
        for id in &moved.rewritten {
            seen.remove(id);
        }
        let Some((old, new)) = moved.notes.first() else {
            return Ok(format!("{} is already at {}", from.id, to.id));
        };
        Ok(match moved.rewritten.is_empty() {
            true => format!("moved {old} to {new}"),
            false => format!(
                "moved {old} to {new}; rewrote links in {}",
                moved.rewritten.join(", ")
            ),
        })
    }
}

/// What a call acts on, for a transcript: the note it names, if any, and its other argument.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub note: Option<String>,
    pub detail: Option<String>,
}

/// Reads the arguments loosely: a call the model got wrong still shows what it named.
pub fn describe(name: &str, arguments: &str) -> Target {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let field = |key: &str| args.get(key).and_then(|v| v.as_str()).map(String::from);
    match name {
        "move" => Target {
            note: field("to"),
            detail: field("from").map(|f| format!("from {f}")),
        },
        "search" => Target {
            note: None,
            detail: field("query"),
        },
        "list" => Target {
            note: None,
            detail: field("folder"),
        },
        _ => Target {
            note: field("note"),
            detail: None,
        },
    }
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn spec<T: JsonSchema>(name: &str, description: &str) -> Spec {
    Spec {
        name: name.into(),
        description: description.into(),
        parameters: schema_of::<T>(),
    }
}

impl Tools for VaultTools {
    fn specs(&self) -> Vec<Spec> {
        vec![
            spec::<ListArgs>("list", "List notes by id and title."),
            spec::<NoteArgs>("read", "Read a note's markdown text."),
            spec::<SearchArgs>(
                "search",
                "Full-text search over note titles, headings, tags and text, best match first.",
            ),
            spec::<NoteArgs>("backlinks", "List the notes that link to a note."),
            spec::<WriteArgs>(
                "write",
                "Create a note, or replace one you have read. Fails if it changed since you read \
                 it.",
            ),
            spec::<EditArgs>(
                "edit",
                "Replace an exact string in a note. Fails if it is absent or occurs more than once.",
            ),
            spec::<MoveArgs>(
                "move",
                "Move or rename a note, rewriting every link to it across the vault.",
            ),
            spec::<NoteArgs>(
                "delete",
                "Move a note you have read to the vault's trash. Fails if it changed since you \
                 read it. Names the notes still linking to it.",
            ),
        ]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        _cancel: &'a Cancel,
    ) -> LocalBoxFuture<'a, Result<Outcome>> {
        let body = match name {
            "list" => parse(arguments).and_then(|a| self.list(a)),
            "read" => parse(arguments).and_then(|a| self.read(a)),
            "search" => parse(arguments).and_then(|a| self.search(a)),
            "backlinks" => parse(arguments).and_then(|a| self.backlinks(a)),
            "write" => parse(arguments).and_then(|a| self.write(a)),
            "edit" => parse(arguments).and_then(|a| self.edit(a)),
            "move" => parse(arguments).and_then(|a| self.move_note(a)),
            "delete" => parse(arguments).and_then(|a| self.delete(a)),
            _ => Err(anyhow::anyhow!("no such tool: {name}")),
        };
        Box::pin(std::future::ready(body.map(Outcome::from)))
    }
}

#[cfg(test)]
mod tests;
