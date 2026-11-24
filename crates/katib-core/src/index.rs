//! The note index: titles, links and full-text search in one SQLite file under
//! `.katib/cache/`. It is derived from the files and rebuilt from them, so it
//! is never synced (design.md section 7).

use std::{collections::HashMap, fmt, fs, path::Path, time::UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::{
    Link, LinkKind, Vault, VaultError,
    note::{self, Note},
    vault::Entry,
};

/// Bump to rebuild every index on next open.
const SCHEMA: i64 = 1;

const SCHEMA_SQL: &str = "
CREATE TABLE notes (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    -- Lower-cased id, title and file name, for case-insensitive link resolution.
    id_key TEXT NOT NULL,
    title_key TEXT NOT NULL,
    name_key TEXT NOT NULL,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL
);
CREATE INDEX notes_id_key ON notes(id_key);
CREATE INDEX notes_title_key ON notes(title_key);
CREATE INDEX notes_name_key ON notes(name_key);
CREATE TABLE links (
    src TEXT NOT NULL,
    kind INTEGER NOT NULL,
    target TEXT NOT NULL,
    fragment TEXT,
    label TEXT,
    start INTEGER NOT NULL,
    end INTEGER NOT NULL,
    -- Wiki: the lower-cased target. Markdown: the lower-cased id it points to.
    key TEXT,
    -- The source line holding the link, for the backlinks panel.
    context TEXT NOT NULL
);
CREATE INDEX links_src ON links(src);
CREATE INDEX links_key ON links(key);
CREATE VIRTUAL TABLE fts USING fts5(title, body, tokenize = 'unicode61 remove_diacritics 2');
";

#[derive(Debug)]
pub enum IndexError {
    Sql(rusqlite::Error),
    Vault(VaultError),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "index: {e}"),
            Self::Vault(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

impl From<VaultError> for IndexError {
    fn from(e: VaultError) -> Self {
        Self::Vault(e)
    }
}

type Result<T> = std::result::Result<T, IndexError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteRow {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub id: String,
    pub title: String,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backlink {
    pub src: String,
    pub title: String,
    pub context: String,
}

/// Where a link goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Note(String),
    /// More than one note matches at the first rule that matches any.
    Ambiguous(Vec<String>),
    /// No note matches; the id a new note for it would take.
    Missing(String),
    /// A non-note file in the vault, by path relative to the root.
    File(String),
    External(String),
    /// A relative link that climbs out of the vault, or a target that names nothing.
    Invalid,
}

pub struct Index {
    db: Connection,
}

impl Index {
    /// Open the index for `vault`, creating or rebuilding it if needed.
    pub fn open(vault: &Vault) -> Result<Self> {
        let dir = vault.katib_dir()?.join("cache");
        fs::create_dir_all(&dir).map_err(|e| VaultError::Io(dir.clone(), e))?;
        Self::open_at(&dir.join("index.db"))
    }

    fn open_at(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        // WAL lets the window read while a background sync writes.
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA {
            db.execute_batch(
                "DROP TABLE IF EXISTS notes; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS fts;",
            )?;
            db.execute_batch(SCHEMA_SQL)?;
            db.execute_batch(&format!("PRAGMA user_version = {SCHEMA}"))?;
        }
        Ok(Self { db })
    }

    /// Bring the index up to date with the files: re-read those whose size or
    /// modification time changed, drop those that are gone. Returns whether
    /// anything changed.
    pub fn sync(&mut self, vault: &Vault) -> Result<bool> {
        let entries = vault.entries()?;
        let known: HashMap<String, (i64, i64)> = {
            let mut stmt = self.db.prepare("SELECT id, mtime, size FROM notes")?;
            stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
                .collect::<rusqlite::Result<_>>()?
        };
        let tx = self.db.transaction()?;
        let mut changed = false;
        for entry in &entries {
            let Some(stamp) = stamp(&entry.path) else {
                continue;
            };
            if known.get(&entry.id) == Some(&stamp) {
                continue;
            }
            // A file that cannot be read (removed meanwhile, not UTF-8) is left out.
            let Ok(text) = fs::read_to_string(&entry.path) else {
                continue;
            };
            put(&tx, entry, &text, &note::parse(&text), stamp)?;
            changed = true;
        }
        let present: std::collections::HashSet<_> = entries.iter().map(|e| &e.id).collect();
        for id in known.keys().filter(|id| !present.contains(id)) {
            remove(&tx, id)?;
            changed = true;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Re-index one note after katib wrote it.
    pub fn update(&mut self, entry: &Entry, text: &str) -> Result<()> {
        let stamp = stamp(&entry.path).unwrap_or((0, 0));
        let tx = self.db.transaction()?;
        put(&tx, entry, text, &note::parse(text), stamp)?;
        tx.commit()?;
        Ok(())
    }

    /// Every note, by id.
    pub fn notes(&self) -> Result<Vec<NoteRow>> {
        let mut stmt = self.db.prepare("SELECT id, title FROM notes ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(NoteRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn title(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT title FROM notes WHERE id = ?1", [id], |r| r.get(0))
            .optional()?)
    }

    /// Full-text search over titles and bodies, best first. Each word of
    /// `query` must occur; the last one may be a prefix.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(expr) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.db.prepare(
            "SELECT n.id, n.title, snippet(fts, 1, '', '', '...', 12)
             FROM fts JOIN notes n ON n.rowid = fts.rowid
             WHERE fts MATCH ?1
             ORDER BY bm25(fts, 10.0, 1.0)
             LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(params![expr, limit as i64], |r| {
                Ok(Hit {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    snippet: r.get::<_, String>(2)?.replace('\n', " "),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(hits)
    }

    /// Where `link`, written in note `from`, goes.
    pub fn resolve(&self, from: &str, link: &Link) -> Result<Target> {
        match link.kind {
            LinkKind::External => Ok(Target::External(link.target.clone())),
            LinkKind::Wiki if link.target.is_empty() => Ok(Target::Note(from.to_owned())),
            LinkKind::Wiki => self.resolve_wiki(&link.target),
            LinkKind::Markdown => {
                let Some(path) = relative_path(from, &link.target) else {
                    return Ok(Target::Invalid);
                };
                let Some(id) = path.strip_suffix(".md") else {
                    return Ok(Target::File(path));
                };
                let ids = self.ids_where("id_key", &id.to_lowercase())?;
                Ok(match ids.as_slice() {
                    [] => Target::Missing(id.to_owned()),
                    [one] => Target::Note(one.clone()),
                    _ => Target::Ambiguous(ids),
                })
            }
        }
    }

    /// By path, then title, then file name; case is ignored.
    fn resolve_wiki(&self, target: &str) -> Result<Target> {
        let key = target.trim().to_lowercase();
        for column in ["id_key", "title_key", "name_key"] {
            let mut ids = self.ids_where(column, &key)?;
            match ids.len() {
                0 => continue,
                1 => return Ok(Target::Note(ids.remove(0))),
                _ => return Ok(Target::Ambiguous(ids)),
            }
        }
        Ok(Target::Missing(target.trim().to_owned()))
    }

    fn ids_where(&self, column: &str, key: &str) -> Result<Vec<String>> {
        // `column` is one of three literals above, never input.
        let mut stmt = self.db.prepare_cached(&format!(
            "SELECT id FROM notes WHERE {column} = ?1 ORDER BY id"
        ))?;
        let ids = stmt
            .query_map([key], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Notes with a link that resolves to `id`, by source id.
    pub fn backlinks(&self, id: &str) -> Result<Vec<Backlink>> {
        let Some((title_key, name_key)) = self
            .db
            .query_row(
                "SELECT title_key, name_key FROM notes WHERE id = ?1",
                [id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            return Ok(Vec::new());
        };
        let mut stmt = self.db.prepare(
            "SELECT l.src, n.title, l.kind, l.target, l.context
             FROM links l JOIN notes n ON n.id = l.src
             WHERE l.key IN (?1, ?2, ?3) AND l.src != ?4
             ORDER BY n.title, l.start",
        )?;
        let rows: Vec<(String, String, i64, String, String)> = stmt
            .query_map(params![id.to_lowercase(), title_key, name_key, id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut out: Vec<Backlink> = Vec::new();
        for (src, title, kind, target, context) in rows {
            // The key narrows candidates; resolution decides, since a title can lose to a path.
            let link = Link {
                kind: kind_from(kind),
                target,
                fragment: None,
                label: None,
                range: 0..0,
            };
            if self.resolve(&src, &link)? == Target::Note(id.to_owned())
                && !out.iter().any(|b| b.src == src && b.context == context)
            {
                out.push(Backlink {
                    src,
                    title,
                    context,
                });
            }
        }
        Ok(out)
    }
}

/// The file's modification time (ns) and size, which decide whether to re-read it.
fn stamp(path: &Path) -> Option<(i64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((mtime, meta.len() as i64))
}

fn put(
    tx: &rusqlite::Transaction,
    entry: &Entry,
    text: &str,
    note: &Note,
    (mtime, size): (i64, i64),
) -> Result<()> {
    remove(tx, &entry.id)?;
    let title = entry.title(note);
    let name = entry.id.rsplit('/').next().unwrap_or(&entry.id);
    tx.execute(
        "INSERT INTO notes (id, title, id_key, title_key, name_key, mtime, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            entry.id,
            title,
            entry.id.to_lowercase(),
            title.to_lowercase(),
            name.to_lowercase(),
            mtime,
            size
        ],
    )?;
    tx.execute(
        "INSERT INTO fts (rowid, title, body) VALUES (last_insert_rowid(), ?1, ?2)",
        params![title, text],
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO links (src, kind, target, fragment, label, start, end, key, context)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for link in &note.links {
        let key = match link.kind {
            LinkKind::Wiki => Some(link.target.trim().to_lowercase()),
            LinkKind::Markdown => relative_path(&entry.id, &link.target)
                .and_then(|p| p.strip_suffix(".md").map(str::to_lowercase)),
            LinkKind::External => None,
        };
        insert.execute(params![
            entry.id,
            kind_to(link.kind),
            link.target,
            link.fragment,
            link.label,
            link.range.start as i64,
            link.range.end as i64,
            key,
            line_around(text, link.range.start),
        ])?;
    }
    Ok(())
}

fn remove(tx: &rusqlite::Transaction, id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM fts WHERE rowid = (SELECT rowid FROM notes WHERE id = ?1)",
        [id],
    )?;
    tx.execute("DELETE FROM notes WHERE id = ?1", [id])?;
    tx.execute("DELETE FROM links WHERE src = ?1", [id])?;
    Ok(())
}

fn kind_to(kind: LinkKind) -> i64 {
    match kind {
        LinkKind::Wiki => 0,
        LinkKind::Markdown => 1,
        LinkKind::External => 2,
    }
}

fn kind_from(kind: i64) -> LinkKind {
    match kind {
        0 => LinkKind::Wiki,
        1 => LinkKind::Markdown,
        _ => LinkKind::External,
    }
}

/// The line holding byte `at`, trimmed and cut to 200 bytes.
fn line_around(text: &str, at: usize) -> String {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    let line = text[start..end].trim();
    let mut cut = line.len().min(200);
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    line[..cut].to_owned()
}

/// A vault-relative path for a markdown link `target` written in note `from`.
/// `None` if it climbs above the root. Percent escapes are decoded.
fn relative_path(from: &str, target: &str) -> Option<String> {
    let target = percent_decode(target);
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        let mut dir: Vec<_> = from.split('/').collect();
        dir.pop();
        dir
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// An FTS5 expression from user input: each word quoted, so FTS syntax
/// characters are literal; the last word matches as a prefix.
fn fts_query(query: &str) -> Option<String> {
    let words: Vec<_> = query
        .split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    let (last, rest) = words.split_last()?;
    Some(
        rest.iter()
            .chain([&format!("{last}*")])
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TempDir;

    fn vault(name: &str, files: &[(&str, &str)]) -> (TempDir, Vault, Index) {
        let dir = TempDir::new(name);
        for (path, body) in files {
            dir.write(path, body);
        }
        let vault = Vault::open(&dir.0).unwrap();
        let mut index = Index::open(&vault).unwrap();
        index.sync(&vault).unwrap();
        (dir, vault, index)
    }

    fn wiki(target: &str) -> Link {
        Link {
            kind: LinkKind::Wiki,
            target: target.into(),
            fragment: None,
            label: None,
            range: 0..0,
        }
    }

    #[test]
    fn titles_and_resolution_order() {
        let (_d, _v, index) = vault(
            "idx-resolve",
            &[
                ("a.md", "# Alpha\n"),
                ("dir/b.md", "# Beta\n"),
                ("other/b.md", "# Bee\n"),
                ("c.md", "# b\n"),
            ],
        );
        let titles: Vec<_> = index
            .notes()
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles, ["Alpha", "b", "Beta", "Bee"]);
        let r = |t: &str| index.resolve("a", &wiki(t)).unwrap();
        assert_eq!(r("dir/B"), Target::Note("dir/b".into()));
        assert_eq!(r("alpha"), Target::Note("a".into()));
        // The title "b" wins over the two file names "b".
        assert_eq!(r("B"), Target::Note("c".into()));
        assert_eq!(r("Nope"), Target::Missing("Nope".into()));
    }

    #[test]
    fn ambiguous_file_names() {
        let (_d, _v, index) = vault("idx-amb", &[("x/n.md", ""), ("y/n.md", "")]);
        assert_eq!(
            index.resolve("x/n", &wiki("n")).unwrap(),
            Target::Ambiguous(vec!["x/n".into(), "y/n".into()])
        );
    }

    #[test]
    fn markdown_links_resolve_relative_to_the_note() {
        let (_d, _v, index) = vault("idx-md", &[("d/a.md", ""), ("b c.md", "")]);
        let md = |t: &str| Link {
            kind: LinkKind::Markdown,
            ..wiki(t)
        };
        assert_eq!(
            index.resolve("d/x", &md("a.md")).unwrap(),
            Target::Note("d/a".into())
        );
        assert_eq!(
            index.resolve("d/x", &md("../b%20c.md")).unwrap(),
            Target::Note("b c".into())
        );
        assert_eq!(
            index.resolve("d/x", &md("img/p.png")).unwrap(),
            Target::File("d/img/p.png".into())
        );
        assert_eq!(
            index.resolve("d/x", &md("../../etc/passwd")).unwrap(),
            Target::Invalid
        );
        assert_eq!(
            index.resolve("x", &md("new.md")).unwrap(),
            Target::Missing("new".into())
        );
    }

    #[test]
    fn backlinks_follow_resolution() {
        let (_d, _v, index) = vault(
            "idx-back",
            &[
                ("t.md", "# Target\n"),
                ("by-title.md", "see [[target]]\n"),
                ("by-path.md", "line one\nsee [it](t.md) here\n"),
                ("self.md", "[[self]]"),
                ("none.md", "[[Other]]"),
            ],
        );
        let back = index.backlinks("t").unwrap();
        let srcs: Vec<_> = back
            .iter()
            .map(|b| (b.src.as_str(), b.context.as_str()))
            .collect();
        assert_eq!(
            srcs,
            [
                ("by-path", "see [it](t.md) here"),
                ("by-title", "see [[target]]")
            ]
        );
        assert!(index.backlinks("self").unwrap().is_empty());
    }

    #[test]
    fn search_ranks_titles_and_takes_prefixes() {
        let (_d, _v, index) = vault(
            "idx-search",
            &[
                ("a.md", "# Gardening\nsoil and seeds"),
                ("b.md", "# Notes\nsome gardening tips"),
                ("c.md", "# Other\nnothing \"here\" (x) AND"),
            ],
        );
        let ids = |q: &str| -> Vec<String> {
            index
                .search(q, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect()
        };
        assert_eq!(ids("garden"), ["a", "b"]);
        assert_eq!(ids("soil seed"), ["a"]);
        // FTS syntax in the query is taken literally.
        assert_eq!(ids("\"here\" (x) AND"), ["c"]);
        assert!(ids("   ").is_empty());
    }

    #[test]
    fn sync_picks_up_edits_and_deletions() {
        let (dir, vault, mut index) = vault("idx-sync", &[("a.md", "# One\n"), ("b.md", "")]);
        assert!(!index.sync(&vault).unwrap());
        // Same size, so the modification time must decide; set it explicitly.
        dir.write("a.md", "# Two\n");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::options()
            .write(true)
            .open(dir.0.join("a.md"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        fs::remove_file(dir.0.join("b.md")).unwrap();
        assert!(index.sync(&vault).unwrap());
        assert_eq!(
            index.notes().unwrap(),
            [NoteRow {
                id: "a".into(),
                title: "Two".into()
            }]
        );
        assert_eq!(index.search("one", 5).unwrap(), []);
    }

    #[test]
    fn a_new_schema_rebuilds() {
        let dir = TempDir::new("idx-schema");
        let path = dir.0.join("index.db");
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE notes (x); PRAGMA user_version = 0;")
                .unwrap();
        }
        let index = Index::open_at(&path).unwrap();
        assert_eq!(index.notes().unwrap(), []);
    }
}
