//! The note index: titles, links and full-text search in one SQLite file under
//! `.katib/cache/`. It is derived from the files and rebuilt from them, so it
//! is never synced (design.md section 7).

use std::{collections::HashMap, fmt, fs, path::Path, time::UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::{
    Link, LinkKind, Vault, VaultError,
    note::{self, Note},
    paths::{page_folder, relative_path},
    vault::Entry,
};

/// Bump to rebuild every index on next open.
const SCHEMA: i64 = 4;

const SCHEMA_SQL: &str = "
CREATE TABLE notes (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    -- Lower-cased id, title and file name, for case-insensitive link resolution.
    id_key TEXT NOT NULL,
    title_key TEXT NOT NULL,
    name_key TEXT NOT NULL,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL,
    -- Front matter `type: task` and its fields.
    task INTEGER NOT NULL DEFAULT 0,
    status TEXT,
    priority TEXT,
    due TEXT
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
    -- Wiki: the lower-cased target. Markdown: the lower-cased id or path it points to.
    key TEXT,
    -- The source line holding the link, for the backlinks panel.
    context TEXT NOT NULL
);
CREATE INDEX links_src ON links(src);
CREATE INDEX links_key ON links(key);
CREATE TABLE tags (
    src TEXT NOT NULL,
    tag TEXT NOT NULL,
    -- Lower-cased, for grouping and lookup.
    key TEXT NOT NULL
);
CREATE INDEX tags_src ON tags(src);
CREATE INDEX tags_key ON tags(key);
CREATE TABLE items (
    src TEXT NOT NULL,
    marker INTEGER NOT NULL,
    text TEXT NOT NULL,
    done INTEGER NOT NULL,
    due TEXT
);
CREATE INDEX items_src ON items(src);
CREATE VIRTUAL TABLE fts USING fts5(title, headings, tags, body, tokenize = 'unicode61 remove_diacritics 2');
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

/// A note offered while typing a wiki link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub id: String,
    pub title: String,
    /// What to write between `[[` and `]]`: the title when it names this note
    /// alone, else the id.
    pub reference: String,
}

/// A tag and how many notes carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

/// A checklist item or a task note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRow {
    /// The note holding the item, or the task note.
    pub src: String,
    pub title: String,
    /// A checklist item's marker offset; `None` for a task note.
    pub marker: Option<usize>,
    /// The item's text, or the task note's title.
    pub text: String,
    /// `open`, `doing` or `done`; an item is `open` or `done`.
    pub status: String,
    pub priority: Option<String>,
    pub due: Option<String>,
}

impl TaskRow {
    pub fn is_done(&self) -> bool {
        self.status == "done"
    }
}

/// A note and when its file last changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recent {
    pub id: String,
    pub title: String,
    /// Seconds since the Unix epoch.
    pub modified: i64,
}

/// The shape of the vault's links.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub notes: usize,
    /// Notes no other note links to.
    pub orphans: Vec<NoteRow>,
    /// Notes that link to no other note.
    pub dead_ends: Vec<NoteRow>,
    /// Notes by how many other notes link to them, most first.
    pub most_linked: Vec<(NoteRow, usize)>,
    /// Links that reach nothing or several notes; see [`Index::broken`].
    pub broken: usize,
}

/// A link that reaches nothing, or more than one note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Broken {
    pub src: String,
    /// The source note's title.
    pub title: String,
    pub link: Link,
    /// The line holding the link.
    pub context: String,
    pub problem: Problem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No note matches; the id a new note for it would take.
    Missing(String),
    /// Several notes match.
    Ambiguous(Vec<String>),
    /// A link to a file that does not exist, by vault path.
    NoFile(String),
    /// The link climbs out of the vault.
    Outside,
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
                "DROP TABLE IF EXISTS notes; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS fts;
                 DROP TABLE IF EXISTS tags; DROP TABLE IF EXISTS items;",
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
            "SELECT n.id, n.title, snippet(fts, 3, '', '', '...', 12)
             FROM fts JOIN notes n ON n.rowid = fts.rowid
             WHERE fts MATCH ?1
             ORDER BY bm25(fts, 10.0, 5.0, 5.0, 1.0)
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
                    // `dir` or `dir/` reaches the folder's page, if it has one.
                    let page = format!("{}/readme", path.to_lowercase());
                    let ids = self.ids_where("id_key", &page)?;
                    return Ok(match ids.as_slice() {
                        [] => Target::File(path),
                        [one] => Target::Note(one.clone()),
                        _ => Target::Ambiguous(ids),
                    });
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

    /// By path, then folder page (`dir/README` for `dir`), then title, then
    /// file name; case is ignored.
    pub fn resolve_wiki(&self, target: &str) -> Result<Target> {
        let written = target.trim();
        let folder = written.ends_with('/');
        let name = written.trim_end_matches('/');
        if name.is_empty() {
            return Ok(Target::Invalid);
        }
        let key = name.to_lowercase();
        let page = format!("{key}/readme");
        let rules = [
            ("id_key", &key),
            ("id_key", &page),
            ("title_key", &key),
            ("name_key", &key),
        ];
        // `dir/` names only the folder's page.
        let rules = if folder { &rules[1..2] } else { &rules[..] };
        for &(column, key) in rules {
            let mut ids = self.ids_where(column, key)?;
            match ids.len() {
                0 => continue,
                1 => return Ok(Target::Note(ids.remove(0))),
                _ => return Ok(Target::Ambiguous(ids)),
            }
        }
        Ok(Target::Missing(if folder {
            format!("{name}/README")
        } else {
            name.to_owned()
        }))
    }

    pub(crate) fn ids_where(&self, column: &str, key: &str) -> Result<Vec<String>> {
        // `column` is always a literal, never input.
        let mut stmt = self.db.prepare_cached(&format!(
            "SELECT id FROM notes WHERE {column} = ?1 ORDER BY id"
        ))?;
        let ids = stmt
            .query_map([key], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Notes whose title or id contains `query`, ignoring case: title prefixes
    /// first, then by title. Each comes with the reference that names it.
    pub fn complete(&self, query: &str, limit: usize) -> Result<Vec<Suggestion>> {
        let key = query.trim().to_lowercase();
        let escaped = key
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut stmt = self.db.prepare_cached(
            "SELECT id, title FROM notes
             WHERE title_key LIKE '%' || ?1 || '%' ESCAPE '\\'
                OR id_key LIKE '%' || ?1 || '%' ESCAPE '\\'
             ORDER BY title_key LIKE ?1 || '%' ESCAPE '\\' DESC, title_key, id
             LIMIT ?2",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![escaped, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(id, title)| {
                let unique = self.resolve_wiki(&title)? == Target::Note(id.clone());
                let reference = if unique { title.clone() } else { id.clone() };
                Ok(Suggestion {
                    id,
                    title,
                    reference,
                })
            })
            .collect()
    }

    /// Notes whose title, file name or id is close to `target`: within a
    /// third of its length in edits, or containing it. Closest first.
    pub fn similar(&self, target: &str, limit: usize) -> Result<Vec<NoteRow>> {
        let key = target.trim().trim_end_matches(".md").to_lowercase();
        let name = key.rsplit('/').next().unwrap_or(&key).to_owned();
        if name.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .db
            .prepare("SELECT id, title, id_key, title_key, name_key FROM notes")?;
        let rows: Vec<(String, String, String, String, String)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let most = (name.chars().count() / 3).max(1);
        let mut scored: Vec<(usize, NoteRow)> = rows
            .into_iter()
            .filter_map(|(id, title, id_key, title_key, name_key)| {
                let d = [&id_key, &title_key, &name_key]
                    .iter()
                    .map(|k| {
                        let shorter = k.len().min(name.len());
                        if shorter >= 3 && (k.contains(&name) || name.contains(k.as_str())) {
                            1
                        } else {
                            edit_distance(k, &name).min(edit_distance(k, &key))
                        }
                    })
                    .min()?;
                (d <= most).then_some((d, NoteRow { id, title }))
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.title.cmp(&b.1.title)));
        Ok(scored.into_iter().take(limit).map(|(_, n)| n).collect())
    }

    /// Every wiki and markdown link that reaches no note or file, or several
    /// notes, by source title. `root` is the vault, to check that files exist.
    pub fn broken(&self, root: &Path) -> Result<Vec<Broken>> {
        let mut stmt = self.db.prepare(
            "SELECT l.src, n.title, l.kind, l.target, l.fragment, l.label, l.start, l.end, l.context
             FROM links l JOIN notes n ON n.id = l.src
             WHERE l.kind IN (0, 1)
             ORDER BY n.title, l.src, l.start",
        )?;
        type Row = (
            String,
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            i64,
            i64,
            String,
        );
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (src, title, kind, target, fragment, label, start, end, context) in rows {
            let link = Link {
                kind: kind_from(kind),
                target,
                fragment,
                label,
                range: start as usize..end as usize,
            };
            let problem = match self.resolve(&src, &link)? {
                Target::Missing(id) => Problem::Missing(id),
                Target::Ambiguous(ids) => Problem::Ambiguous(ids),
                Target::File(rel) if !root.join(&rel).exists() => Problem::NoFile(rel),
                Target::Invalid => Problem::Outside,
                _ => continue,
            };
            out.push(Broken {
                src,
                title,
                link,
                context,
                problem,
            });
        }
        Ok(out)
    }

    /// Notes by when their file changed, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Recent>> {
        let mut stmt = self
            .db
            .prepare("SELECT id, title, mtime FROM notes ORDER BY mtime DESC, id LIMIT ?1")?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(Recent {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    modified: r.get::<_, i64>(2)? / 1_000_000_000,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Orphans, dead ends, the most-linked notes (up to `top`) and the broken
    /// link count. Every link is resolved, so this reads the whole index.
    pub fn stats(&self, root: &Path, top: usize) -> Result<Stats> {
        let notes = self.notes()?;
        let mut incoming: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        let mut outgoing: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut stmt = self
            .db
            .prepare("SELECT src, kind, target FROM links WHERE kind IN (0, 1)")?;
        let rows: Vec<(String, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut broken = 0;
        for (src, kind, target) in rows {
            let link = Link {
                kind: kind_from(kind),
                target,
                fragment: None,
                label: None,
                range: 0..0,
            };
            match self.resolve(&src, &link)? {
                Target::Note(to) if to != src => {
                    outgoing.insert(src.clone());
                    incoming.entry(to).or_default().insert(src);
                }
                Target::Note(_) | Target::External(_) => {}
                Target::File(rel) if root.join(&rel).exists() => {}
                _ => broken += 1,
            }
        }
        let count = |id: &str| incoming.get(id).map_or(0, |s| s.len());
        let mut most_linked: Vec<(NoteRow, usize)> = notes
            .iter()
            .filter(|n| count(&n.id) > 0)
            .map(|n| (n.clone(), count(&n.id)))
            .collect();
        most_linked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.title.cmp(&b.0.title)));
        most_linked.truncate(top);
        let by_title = |mut v: Vec<NoteRow>| {
            v.sort_by_key(|n| n.title.to_lowercase());
            v
        };
        Ok(Stats {
            notes: notes.len(),
            orphans: by_title(
                notes
                    .iter()
                    .filter(|n| count(&n.id) == 0)
                    .cloned()
                    .collect(),
            ),
            dead_ends: by_title(
                notes
                    .iter()
                    .filter(|n| !outgoing.contains(&n.id))
                    .cloned()
                    .collect(),
            ),
            most_linked,
            broken,
        })
    }

    /// Every tag, by name ignoring case, with its note count.
    pub fn tags(&self) -> Result<Vec<TagCount>> {
        let mut stmt = self
            .db
            .prepare("SELECT MIN(tag), COUNT(DISTINCT src) FROM tags GROUP BY key ORDER BY key")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TagCount {
                    tag: r.get(0)?,
                    count: r.get::<_, i64>(1)? as usize,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Notes tagged `tag`, ignoring case, by title.
    pub fn tagged(&self, tag: &str) -> Result<Vec<NoteRow>> {
        let key = tag.trim().trim_start_matches('#').to_lowercase();
        let mut stmt = self.db.prepare(
            "SELECT DISTINCT n.id, n.title FROM tags t JOIN notes n ON n.id = t.src
             WHERE t.key = ?1 ORDER BY n.title_key, n.id",
        )?;
        let rows = stmt
            .query_map([key], |r| {
                Ok(NoteRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Note `id`'s tags, as written.
    pub fn note_tags(&self, id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT tag FROM tags WHERE src = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Checklist items and task notes: unfinished first, then by due date
    /// (none last), then by note and position.
    pub fn tasks(&self) -> Result<Vec<TaskRow>> {
        let mut stmt = self.db.prepare(
            "SELECT i.src, n.title, i.marker, i.text,
                    CASE WHEN i.done THEN 'done' ELSE 'open' END, NULL, i.due
             FROM items i JOIN notes n ON n.id = i.src
             UNION ALL
             SELECT id, title, NULL, title,
                    CASE WHEN lower(status) IN ('done', 'doing') THEN lower(status) ELSE 'open' END,
                    priority, due
             FROM notes WHERE task",
        )?;
        let mut rows: Vec<TaskRow> = stmt
            .query_map([], |r| {
                Ok(TaskRow {
                    src: r.get(0)?,
                    title: r.get(1)?,
                    marker: r.get::<_, Option<i64>>(2)?.map(|m| m as usize),
                    text: r.get(3)?,
                    status: r.get(4)?,
                    priority: r.get(5)?,
                    due: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.sort_by(|a, b| {
            (a.is_done(), a.due.is_none(), &a.due, &a.title, a.marker).cmp(&(
                b.is_done(),
                b.due.is_none(),
                &b.due,
                &b.title,
                b.marker,
            ))
        });
        Ok(rows)
    }

    /// Every vault file a markdown link or image points at, by vault path.
    pub fn linked_files(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT src, target FROM links WHERE kind = 1")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            let (src, target) = row?;
            if let Some(path) = relative_path(&src, &target) {
                out.insert(path);
            }
        }
        Ok(out)
    }

    /// Notes with a markdown link or image to folder `dir` or into it, by source id.
    pub fn sources_linking_under(&self, dir: &str) -> Result<Vec<String>> {
        let prefix = format!("{dir}/");
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT src, target FROM links WHERE kind = 1")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (src, target) = row?;
            if relative_path(&src, &target).is_some_and(|p| p == dir || p.starts_with(&prefix))
                && !out.contains(&src)
            {
                out.push(src);
            }
        }
        Ok(out)
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
             WHERE l.key IN (?1, ?2, ?3, ?5) AND l.src != ?4
             ORDER BY n.title, l.start",
        )?;
        let folder = page_folder(id).unwrap_or(id).to_lowercase();
        let rows: Vec<(String, String, i64, String, String)> = stmt
            .query_map(
                params![id.to_lowercase(), title_key, name_key, id, folder],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?
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

/// Levenshtein distance by characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
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
    let front = &note.front;
    let task = front.kind.as_deref() == Some("task");
    tx.execute(
        "INSERT INTO notes (id, title, id_key, title_key, name_key, mtime, size, task, status, priority, due)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            entry.id,
            title,
            entry.id.to_lowercase(),
            title.to_lowercase(),
            name.to_lowercase(),
            mtime,
            size,
            task,
            front.status,
            front.priority,
            front.due,
        ],
    )?;
    tx.execute(
        "INSERT INTO fts (rowid, title, headings, tags, body)
         VALUES (last_insert_rowid(), ?1, ?2, ?3, ?4)",
        params![
            title,
            note.headings.join("\n"),
            note.front.tags.join("\n"),
            text
        ],
    )?;
    // After the fts row, which takes the notes row's rowid.
    let mut seen = std::collections::HashSet::new();
    for tag in &front.tags {
        let tag = tag.trim().trim_start_matches('#');
        if !tag.is_empty() && seen.insert(tag.to_lowercase()) {
            tx.execute(
                "INSERT INTO tags (src, tag, key) VALUES (?1, ?2, ?3)",
                params![entry.id, tag, tag.to_lowercase()],
            )?;
        }
    }
    for item in &note.items {
        tx.execute(
            "INSERT INTO items (src, marker, text, done, due) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![entry.id, item.marker as i64, item.text, item.done, item.due],
        )?;
    }
    let mut insert = tx.prepare_cached(
        "INSERT INTO links (src, kind, target, fragment, label, start, end, key, context)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for link in &note.links {
        let key = match link.kind {
            LinkKind::Wiki => Some(link.target.trim().trim_end_matches('/').to_lowercase()),
            // A path without `.md` may name a folder, and so its page.
            LinkKind::Markdown => relative_path(&entry.id, &link.target)
                .map(|p| p.strip_suffix(".md").unwrap_or(&p).to_lowercase()),
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
    tx.execute("DELETE FROM tags WHERE src = ?1", [id])?;
    tx.execute("DELETE FROM items WHERE src = ?1", [id])?;
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
    fn a_folder_link_reaches_its_readme() {
        let (d, _v, index) = vault(
            "idx-folder",
            &[
                ("p/README.md", "# Projects\n"),
                ("p/x.md", ""),
                ("q.md", ""),
                ("q/readme.md", ""),
                (
                    "r/n.md",
                    "[up](../p/) [down](../p) [[p]] [[p/]] [[P/README]]",
                ),
            ],
        );
        let r = |t: &str| index.resolve("r/n", &wiki(t)).unwrap();
        assert_eq!(r("p"), Target::Note("p/README".into()));
        assert_eq!(r("p/"), Target::Note("p/README".into()));
        // A note at the folder's path wins over the folder's page.
        assert_eq!(r("q"), Target::Note("q".into()));
        assert_eq!(r("q/"), Target::Note("q/readme".into()));
        // A folder link names no title or file name, and a missing one names a page.
        assert_eq!(r("Projects/"), Target::Missing("Projects/README".into()));
        assert_eq!(r("x/"), Target::Missing("x/README".into()));
        assert_eq!(r("/"), Target::Invalid);
        let md = |t: &str| Link {
            kind: LinkKind::Markdown,
            ..wiki(t)
        };
        let m = |t: &str| index.resolve("r/n", &md(t)).unwrap();
        assert_eq!(m("../p/"), Target::Note("p/README".into()));
        assert_eq!(m("../p"), Target::Note("p/README".into()));
        assert_eq!(m("../r/"), Target::File("r".into()));
        let back: Vec<_> = index
            .backlinks("p/README")
            .unwrap()
            .into_iter()
            .map(|b| b.src)
            .collect();
        assert_eq!(back, ["r/n"]);
        assert!(index.broken(&d.0).unwrap().is_empty());
    }

    #[test]
    fn a_folder_with_two_pages_is_ambiguous_by_either_link() {
        let (_d, _v, index) = vault(
            "idx-two-pages",
            &[("r/README.md", ""), ("r/readme.md", ""), ("n.md", "")],
        );
        // A case-insensitive file system holds only one of the two.
        if index.ids_where("id_key", "r/readme").unwrap().len() < 2 {
            return;
        }
        let both = Target::Ambiguous(vec!["r/README".into(), "r/readme".into()]);
        assert_eq!(index.resolve("n", &wiki("r/")).unwrap(), both);
        let md = Link {
            kind: LinkKind::Markdown,
            ..wiki("r/")
        };
        assert_eq!(index.resolve("n", &md).unwrap(), both);
    }

    #[test]
    fn a_new_folder_page_wins_over_a_title_or_file_name_match() {
        let (d, vault, mut index) = vault(
            "idx-page-wins",
            &[
                ("notes/p.md", ""),
                ("t.md", "# Q\n"),
                ("x.md", "[[p]] [[q]]\n"),
            ],
        );
        let r = |index: &Index, t: &str| index.resolve("x", &wiki(t)).unwrap();
        assert_eq!(r(&index, "p"), Target::Note("notes/p".into()));
        assert_eq!(r(&index, "q"), Target::Note("t".into()));
        d.write("p/README.md", "");
        d.write("q/README.md", "");
        index.sync(&vault).unwrap();
        // The page rule comes before title and file name (design.md section 8).
        assert_eq!(r(&index, "p"), Target::Note("p/README".into()));
        assert_eq!(r(&index, "q"), Target::Note("q/README".into()));
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
    fn headings_and_tags_outrank_body_text() {
        // One mention in a heading or a tag beats two in the body.
        let words = "words ".repeat(20);
        let (_d, _v, index) = vault(
            "idx-rank",
            &[
                ("a.md", &format!("# A\n{words}compost {words}compost\n")),
                ("b.md", &format!("# B\n## Compost\n{words}{words}\n")),
                (
                    "c.md",
                    &format!("---\ntags: [compost]\n---\n# C\n{words}{words}\n"),
                ),
            ],
        );
        let ids: Vec<_> = index
            .search("compost", 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids.last().map(String::as_str), Some("a"), "{ids:?}");
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

    #[test]
    fn completion_matches_titles_and_ids() {
        let (_d, _v, index) = vault(
            "complete",
            &[
                ("projects/katib.md", "# Katib design\n"),
                ("katib-notes.md", "# Notes on katib\n"),
                ("a.md", "# Same\n"),
                ("b.md", "# Same\n"),
                ("100%.md", "# Percent\n"),
            ],
        );
        let got: Vec<_> = index
            .complete("kat", 10)
            .unwrap()
            .into_iter()
            .map(|s| (s.id, s.reference))
            .collect();
        // Title prefixes first; a unique title is the reference.
        assert_eq!(
            got,
            [
                ("projects/katib".to_owned(), "Katib design".to_owned()),
                ("katib-notes".to_owned(), "Notes on katib".to_owned()),
            ]
        );
        // A shared title would resolve to neither note, so the id is used.
        let same: Vec<_> = index.complete("same", 10).unwrap();
        assert_eq!(
            same.iter()
                .map(|s| s.reference.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        // LIKE wildcards in the query are literal.
        assert_eq!(index.complete("%", 10).unwrap().len(), 1);
        assert_eq!(index.complete("", 2).unwrap().len(), 2);
    }

    #[test]
    fn broken_links_and_close_matches() {
        let (d, _v, index) = vault(
            "broken",
            &[
                (
                    "a.md",
                    "# A\n[[Projcts]] [[b]] [[Dup]] [x](gone.png) [y](../../out.md) [z](b.md)\n",
                ),
                ("b.md", "# Projects\n"),
                ("c.md", "# Dup\n"),
                ("d.md", "# Dup\n"),
            ],
        );
        let broken = index.broken(&d.0).unwrap();
        let problems: Vec<_> = broken.iter().map(|b| b.problem.clone()).collect();
        assert_eq!(
            problems,
            [
                Problem::Missing("Projcts".into()),
                Problem::Ambiguous(vec!["c".into(), "d".into()]),
                Problem::NoFile("gone.png".into()),
                Problem::Outside,
            ]
        );
        assert_eq!(broken[0].src, "a");
        let text = fs::read_to_string(d.0.join("a.md")).unwrap();
        assert_eq!(&text[broken[0].link.range.clone()], "[[Projcts]]");
        let near: Vec<_> = index
            .similar("Projcts", 3)
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(near, ["b"]);
        assert!(index.similar("zzzzzz", 3).unwrap().is_empty());
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn tags_and_tasks() {
        let (_d, _v, index) = vault(
            "tasks",
            &[
                (
                    "a.md",
                    "---\ntags: [Rust, ideas]\n---\n# A\n- [ ] later\n- [ ] soon due:2026-01-02\n- [x] old\n",
                ),
                (
                    "b.md",
                    "---\ntags:\n  - rust\ntype: task\nstatus: Doing\ndue: 2026-01-01\npriority: high\n---\n# Ship\n",
                ),
            ],
        );
        assert_eq!(
            index.tags().unwrap(),
            [
                TagCount {
                    tag: "Rust".into(),
                    count: 2
                },
                TagCount {
                    tag: "ideas".into(),
                    count: 1
                },
            ]
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
        );
        let tagged: Vec<_> = index
            .tagged("#RUST")
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(tagged, ["a", "b"]);
        assert_eq!(index.note_tags("a").unwrap(), ["Rust", "ideas"]);
        let tasks: Vec<_> = index
            .tasks()
            .unwrap()
            .into_iter()
            .map(|t| (t.text, t.status, t.due))
            .collect();
        assert_eq!(
            tasks,
            [
                (
                    "Ship".to_owned(),
                    "doing".to_owned(),
                    Some("2026-01-01".to_owned())
                ),
                (
                    "soon due:2026-01-02".to_owned(),
                    "open".to_owned(),
                    Some("2026-01-02".to_owned())
                ),
                ("later".to_owned(), "open".to_owned(), None),
                ("old".to_owned(), "done".to_owned(), None),
            ]
        );
    }

    #[test]
    fn stats_count_links_between_notes() {
        let (d, _v, index) = vault(
            "stats",
            &[
                ("hub.md", "# Hub\n[[a]] [[b]] [[hub]]\n"),
                ("a.md", "# A\n[[b]] [[gone]] [x](pic.png)\n"),
                ("b.md", "# B\n"),
                ("lone.md", "# Lone\n"),
            ],
        );
        let s = index.stats(&d.0, 10).unwrap();
        let ids = |v: &[NoteRow]| v.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        assert_eq!(s.notes, 4);
        // A link to itself does not count.
        assert_eq!(ids(&s.orphans), ["hub", "lone"]);
        assert_eq!(ids(&s.dead_ends), ["b", "lone"]);
        assert_eq!(
            s.most_linked
                .iter()
                .map(|(n, c)| (n.id.as_str(), *c))
                .collect::<Vec<_>>(),
            [("b", 2), ("a", 1)]
        );
        assert_eq!(s.broken, 2);
        assert_eq!(index.recent(2).unwrap().len(), 2);
    }
}
