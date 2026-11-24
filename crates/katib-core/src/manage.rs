//! Moving, renaming and deleting notes. A move rewrites every link that
//! pointed at the note, and the moved note's own relative links, so no link
//! breaks (design.md section 7).

use std::{
    collections::{BTreeMap, HashMap},
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    Index, IndexError, Link, LinkKind, SaveError, Target, Vault, VaultError,
    note::parse,
    paths::{percent_decode, percent_encode, relative_path, relative_to},
    save_guarded,
};

#[derive(Debug)]
pub enum ManageError {
    Vault(VaultError),
    Index(IndexError),
    /// A file changed on disk while links were being rewritten; nothing after
    /// it was written. `done` lists the notes already rewritten.
    Save {
        id: String,
        error: SaveError,
        done: Vec<String>,
    },
    Io(PathBuf, io::Error),
    /// The link to repair is no longer in the note as it was listed.
    Changed(String),
}

impl fmt::Display for ManageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vault(e) => write!(f, "{e}"),
            Self::Index(e) => write!(f, "{e}"),
            Self::Save { id, error, done } if done.is_empty() => write!(f, "{id}: {error}"),
            Self::Save { id, error, done } => write!(
                f,
                "{id}: {error}; links in {} were already updated",
                done.join(", ")
            ),
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Changed(id) => write!(f, "{id}: the link changed since it was listed"),
        }
    }
}

impl std::error::Error for ManageError {}

impl From<VaultError> for ManageError {
    fn from(e: VaultError) -> Self {
        Self::Vault(e)
    }
}

impl From<IndexError> for ManageError {
    fn from(e: IndexError) -> Self {
        Self::Index(e)
    }
}

type Result<T> = std::result::Result<T, ManageError>;

/// What a move changed.
#[derive(Debug, PartialEq, Eq)]
pub struct Moved {
    /// Each moved note, old id to new id.
    pub notes: Vec<(String, String)>,
    /// Other notes whose links were rewritten, by id.
    pub rewritten: Vec<String>,
}

impl Moved {
    /// Where note `id` went, if it moved.
    pub fn new_id(&self, id: &str) -> Option<&str> {
        self.notes
            .iter()
            .find(|(from, _)| from == id)
            .map(|(_, to)| to.as_str())
    }
}

/// The moves one operation makes: notes by id, and for a folder move, every
/// file under its path.
struct Plan {
    notes: HashMap<String, String>,
    folder: Option<(String, String)>,
    /// How wiki links name each moved note afterwards.
    wiki_targets: HashMap<String, String>,
}

impl Plan {
    fn new(
        index: &Index,
        notes: HashMap<String, String>,
        folder: Option<(String, String)>,
    ) -> Result<Self> {
        let mut wiki_targets = HashMap::new();
        for (from, to) in &notes {
            wiki_targets.insert(from.clone(), wiki_target_for(index, from, to)?);
        }
        Ok(Self {
            notes,
            folder,
            wiki_targets,
        })
    }

    /// Where vault path `path` (a note's `id.md` or any file) ends up.
    fn new_path(&self, path: &str) -> Option<String> {
        if let Some(to) = path.strip_suffix(".md").and_then(|id| self.notes.get(id)) {
            return Some(format!("{to}.md"));
        }
        let (from, to) = self.folder.as_ref()?;
        path.strip_prefix(from.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| format!("{to}/{rest}"))
    }
}

/// Move (or rename) note `from` to id `to`, rewriting links. The index must be
/// current; sync it afterwards.
pub fn move_note(vault: &Vault, index: &Index, from: &str, to: &str) -> Result<Moved> {
    let from = vault.entry(from)?;
    let to = vault.entry(to)?;
    if !from.path.is_file() {
        return Err(VaultError::Io(from.path.clone(), io::ErrorKind::NotFound.into()).into());
    }
    // A case-only rename names the same file on a case-insensitive disk.
    let same_file = from.path.canonicalize().ok() == to.path.canonicalize().ok();
    if to.path.exists() && !same_file {
        return Err(VaultError::Exists(to.id).into());
    }
    if from.id == to.id {
        return Ok(Moved {
            notes: Vec::new(),
            rewritten: Vec::new(),
        });
    }
    let plan = Plan::new(
        index,
        HashMap::from([(from.id.clone(), to.id.clone())]),
        None,
    )?;
    let rewritten = rewrite_all(vault, index, &plan)?;
    if let Some(dir) = to.path.parent() {
        fs::create_dir_all(dir).map_err(|e| ManageError::Io(dir.to_owned(), e))?;
    }
    fs::rename(&from.path, &to.path).map_err(|e| ManageError::Io(from.path.clone(), e))?;
    move_drafts(vault, &plan)?;
    remove_empty_dirs(vault.root(), from.path.parent());
    Ok(Moved {
        notes: vec![(from.id, to.id)],
        rewritten,
    })
}

/// Move folder `from` (a vault path) to `to`, with every file in it, rewriting
/// links to anything inside and the moved notes' own relative links.
pub fn move_folder(vault: &Vault, index: &Index, from: &str, to: &str) -> Result<Moved> {
    let (from, to) = (folder(vault, from)?, folder(vault, to)?);
    let (from_dir, to_dir) = (vault.root().join(&from), vault.root().join(&to));
    if !from_dir.is_dir() {
        return Err(VaultError::NotADirectory(from_dir).into());
    }
    if to == from || to.starts_with(&format!("{from}/")) {
        return Err(VaultError::BadId(to).into());
    }
    let same_dir = from_dir.canonicalize().ok() == to_dir.canonicalize().ok();
    if to_dir.exists() && !same_dir {
        return Err(VaultError::Exists(to).into());
    }
    let notes: HashMap<_, _> = vault
        .entries()?
        .into_iter()
        .filter_map(|e| {
            let rest = e.id.strip_prefix(&format!("{from}/"))?;
            Some((e.id.clone(), format!("{to}/{rest}")))
        })
        .collect();
    let plan = Plan::new(index, notes, Some((from.clone(), to.clone())))?;
    let rewritten = rewrite_all(vault, index, &plan)?;
    if let Some(parent) = to_dir.parent() {
        fs::create_dir_all(parent).map_err(|e| ManageError::Io(parent.to_owned(), e))?;
    }
    fs::rename(&from_dir, &to_dir).map_err(|e| ManageError::Io(from_dir.clone(), e))?;
    move_drafts(vault, &plan)?;
    remove_empty_dirs(vault.root(), from_dir.parent());
    let mut notes: Vec<_> = plan.notes.into_iter().collect();
    notes.sort();
    Ok(Moved { notes, rewritten })
}

/// A folder path checked with the rules for note ids.
fn folder(vault: &Vault, path: &str) -> Result<String> {
    let path = path.trim().trim_matches('/');
    let probe = vault.entry(&format!("{path}/x"))?;
    Ok(probe.id.strip_suffix("/x").unwrap_or(path).to_owned())
}

fn move_drafts(vault: &Vault, plan: &Plan) -> Result<()> {
    for (from, to) in &plan.notes {
        let (from, to) = (vault.entry(from)?, vault.entry(to)?);
        if let Some(draft) = vault.read_draft(&from) {
            vault.write_draft(&to, &draft)?;
            vault.remove_draft(&from);
        }
    }
    Ok(())
}

/// Rewrite every note whose links the plan changes, each written only if it
/// still holds what was read. Moving notes are written at their old paths;
/// the caller then moves the files. Returns the non-moving notes rewritten.
fn rewrite_all(vault: &Vault, index: &Index, plan: &Plan) -> Result<Vec<String>> {
    let mut sources: BTreeMap<String, ()> = BTreeMap::new();
    for id in plan.notes.keys() {
        sources.insert(id.clone(), ());
        for b in index.backlinks(id)? {
            sources.insert(b.src, ());
        }
    }
    if let Some((from, _)) = &plan.folder {
        for src in index.sources_linking_under(from)? {
            sources.insert(src, ());
        }
    }
    let mut edits: BTreeMap<String, (String, String)> = BTreeMap::new();
    for src in sources.into_keys() {
        let path = vault.entry(&src)?.path;
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let new = rewrite(&text, &src, index, plan)?;
        if new != text {
            edits.insert(src, (text, new));
        }
    }
    let mut done = Vec::new();
    // Other notes first, then the moving ones, so a failure leaves no note moved.
    let order = edits
        .keys()
        .filter(|id| !plan.notes.contains_key(*id))
        .chain(edits.keys().filter(|id| plan.notes.contains_key(*id)));
    for id in order {
        let (base, new) = &edits[id];
        let path = vault.entry(id)?.path;
        save_guarded(&path, Some(base), new).map_err(|error| ManageError::Save {
            id: id.clone(),
            error,
            done: done.clone(),
        })?;
        if !plan.notes.contains_key(id) {
            done.push(id.clone());
        }
    }
    Ok(done)
}

/// How wiki links should name the note at `to`: its file name when that is
/// unambiguous after the move, else its full id.
fn wiki_target_for(index: &Index, from: &str, to: &str) -> Result<String> {
    let name = to.rsplit('/').next().unwrap_or(to);
    let free = match index.resolve_wiki(name)? {
        Target::Missing(_) => true,
        Target::Note(id) => id == from,
        _ => false,
    };
    Ok(if free { name.to_owned() } else { to.to_owned() })
}

/// Point `link`, in note `src`, at note `to`, keeping its fragment and label.
/// Refused if the note no longer holds that link where it was. The index must
/// be current; update it afterwards.
pub fn retarget(vault: &Vault, index: &Index, src: &str, link: &Link, to: &str) -> Result<()> {
    let path = vault.entry(src)?.path;
    let text = fs::read_to_string(&path).map_err(|e| ManageError::Io(path.clone(), e))?;
    let same = |l: &Link| l.range == link.range && l.kind == link.kind && l.target == link.target;
    if !parse(&text).links.iter().any(same) {
        return Err(ManageError::Changed(src.to_owned()));
    }
    let edit = match link.kind {
        LinkKind::Wiki => {
            let name = to.rsplit('/').next().unwrap_or(to);
            let reference = match index.resolve_wiki(name)? {
                Target::Note(id) if id == to => name,
                _ => to,
            };
            Some(wiki_reference(&text, link, reference))
        }
        LinkKind::Markdown => {
            let written = if link.target.starts_with('/') {
                format!("/{to}.md")
            } else {
                relative_to(src, &format!("{to}.md"))
            };
            destination_replacement(&text, link, &written)
                .or_else(|| definition_replacement(&text, link, &written))
        }
        LinkKind::External => None,
    };
    let Some((range, new)) = edit else {
        return Err(ManageError::Changed(src.to_owned()));
    };
    let mut out = text.clone();
    out.replace_range(range, &new);
    save_guarded(&path, Some(&text), &out).map_err(|error| ManageError::Save {
        id: src.to_owned(),
        error,
        done: Vec::new(),
    })
}

/// The reference part of `[[reference#fragment|label]]`, to be `new`.
fn wiki_reference(text: &str, link: &Link, new: &str) -> (std::ops::Range<usize>, String) {
    let inner_start = link.range.start + 2;
    let inner = &text[inner_start..link.range.end - 2];
    let reference_len = inner.find(['#', '|']).unwrap_or(inner.len());
    (inner_start..inner_start + reference_len, new.to_owned())
}

/// `text` (note `src`) with every link the plan moves pointing at the new
/// place. In a moving note, relative links are rebased onto its new folder.
fn rewrite(text: &str, src: &str, index: &Index, plan: &Plan) -> Result<String> {
    let new_src = plan.notes.get(src).map_or(src, String::as_str);
    let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for link in parse(text).links {
        let target = index.resolve(src, &link)?;
        let moved_note = match &target {
            Target::Note(t) => plan.notes.get(t).map(|to| (t, to)),
            _ => None,
        };
        match link.kind {
            LinkKind::Wiki => {
                if let Some((from, to)) = moved_note
                    && let Some(r) =
                        wiki_replacement(text, &link, index, from, to, &plan.wiki_targets[from])?
                {
                    replacements.push(r);
                }
            }
            LinkKind::Markdown => {
                let Some(old) = relative_path(src, &link.target) else {
                    continue;
                };
                let new = match moved_note {
                    Some((_, to)) => format!("{to}.md"),
                    None => plan.new_path(&old).unwrap_or(old),
                };
                let written = if link.target.starts_with('/') {
                    format!("/{new}")
                } else {
                    relative_to(new_src, &new)
                };
                if written == percent_decode(&link.target) {
                    continue;
                }
                if let Some(r) = destination_replacement(text, &link, &written)
                    .or_else(|| definition_replacement(text, &link, &written))
                {
                    replacements.push(r);
                }
            }
            LinkKind::External => {}
        }
    }
    // Two reference links can share one definition; replace it once.
    replacements.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    replacements.dedup_by_key(|(r, _)| r.start);
    let mut out = text.to_owned();
    for (range, new) in replacements {
        out.replace_range(range, &new);
    }
    Ok(out)
}

/// Replace the reference part of `[[reference#fragment|label]]`, keeping the
/// rest. A link that names the note by an explicit title still resolves after a
/// move and is left alone.
fn wiki_replacement(
    text: &str,
    link: &Link,
    index: &Index,
    from: &str,
    to: &str,
    wiki_target: &str,
) -> Result<Option<(std::ops::Range<usize>, String)>> {
    let key = link.target.trim().to_lowercase();
    let from_name = from.rsplit('/').next().unwrap_or(from).to_lowercase();
    let title = index.title(from)?.unwrap_or_default().to_lowercase();
    if key != from.to_lowercase() && key == title && title != from_name {
        return Ok(None);
    }
    let new = if link.target.contains('/') {
        to
    } else {
        wiki_target
    };
    Ok(Some(wiki_reference(text, link, new)))
}

/// The byte range of a markdown link's destination path (before any `#`) and
/// its replacement. `None` for reference-style links, whose destination is on
/// another line.
fn destination_replacement(
    text: &str,
    link: &Link,
    path: &str,
) -> Option<(std::ops::Range<usize>, String)> {
    let s = &text[link.range.clone()];
    let bytes = s.as_bytes();
    // The label's closing bracket: skip `!`, then match brackets, honouring escapes.
    let mut i = s.find('[')? + 1;
    let mut depth = 1;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    i += 1;
    while bytes.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    let (start, end, angled) = if bytes.get(i) == Some(&b'<') {
        let end = s[i + 1..].find('>')? + i + 1;
        (i + 1, end, true)
    } else {
        let mut depth = 0;
        let mut j = i;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 1,
                b'(' => depth += 1,
                b')' if depth == 0 => break,
                b')' => depth -= 1,
                b if b.is_ascii_whitespace() => break,
                _ => {}
            }
            j += 1;
        }
        (i, j.min(bytes.len()), false)
    };
    // Keep a `#fragment`.
    let end = s[start..end].find('#').map_or(end, |k| start + k);
    let new = if angled {
        path.to_owned()
    } else {
        percent_encode(path)
    };
    Some((link.range.start + start..link.range.start + end, new))
}

/// The destination of the definition (`[ref]: path`) a reference-style link
/// uses, and its replacement. Labels match ignoring case and runs of spaces.
fn definition_replacement(
    text: &str,
    link: &Link,
    path: &str,
) -> Option<(std::ops::Range<usize>, String)> {
    let s = &text[link.range.clone()];
    let first = bracketed(s, s.find('[')?)?;
    let label = match bracketed(s, first.end + 1) {
        Some(second) if !s[second.clone()].trim().is_empty() => &s[second],
        _ => &s[first],
    };
    let norm = |l: &str| {
        l.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let want = norm(label);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let body = &line[indent..];
        if indent <= 3
            && let Some(def) = bracketed(body, 0)
            && body[def.end + 1..].starts_with(':')
            && norm(&body[def.clone()]) == want
        {
            let rest = &body[def.end + 2..];
            let lead = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            let start = offset + indent + def.end + 2 + lead;
            let dest = &text[start..];
            let (start, len, angled) = match dest.strip_prefix('<') {
                Some(inner) => (start + 1, inner.find('>')?, true),
                None => (
                    start,
                    dest.find(char::is_whitespace).unwrap_or(dest.len()),
                    false,
                ),
            };
            let len = text[start..start + len].find('#').unwrap_or(len);
            let new = if angled {
                path.to_owned()
            } else {
                percent_encode(path)
            };
            return Some((start..start + len, new));
        }
        offset += line.len();
    }
    None
}

/// The byte range inside the brackets that open at `open` in `s`.
fn bracketed(s: &str, open: usize) -> Option<std::ops::Range<usize>> {
    let bytes = s.as_bytes();
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + 1..i);
                }
            }
            b'\n' if depth > 0 && i + 1 < bytes.len() && bytes[i + 1] == b'\n' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Remove `dir` and its parents while empty, stopping at `root`.
fn remove_empty_dirs(root: &Path, mut dir: Option<&Path>) {
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || fs::remove_dir(d).is_err() {
            return;
        }
        dir = d.parent();
    }
}

/// Move note `id` to `.katib/trash/`, keeping its folder structure. Returns
/// where it went. Nothing is deleted, so a mistake can be undone by hand.
pub fn trash_note(vault: &Vault, id: &str) -> Result<PathBuf> {
    let entry = vault.entry(id)?;
    let trash = vault.katib_dir()?.join("trash");
    let mut dest = trash.join(format!("{}.md", entry.id));
    let mut n = 2;
    while dest.exists() {
        dest = trash.join(format!("{} ({n}).md", entry.id));
        n += 1;
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| ManageError::Io(dir.to_owned(), e))?;
    }
    fs::rename(&entry.path, &dest).map_err(|e| ManageError::Io(entry.path.clone(), e))?;
    vault.remove_draft(&entry);
    remove_empty_dirs(vault.root(), entry.path.parent());
    Ok(dest)
}

/// The first free `Untitled`, `Untitled 2`, ... id in folder `dir` (`""` for the root).
pub fn untitled_id(vault: &Vault, dir: &str) -> String {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    (1..)
        .map(|n| match n {
            1 => format!("{prefix}Untitled"),
            n => format!("{prefix}Untitled {n}"),
        })
        .find(|id| vault.entry(id).is_ok_and(|e| !e.path.exists()))
        .expect("some number is free")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TempDir;

    fn setup(name: &str, files: &[(&str, &str)]) -> (TempDir, Vault, Index) {
        let dir = TempDir::new(name);
        for (p, t) in files {
            dir.write(p, t);
        }
        let vault = Vault::open(&dir.0).unwrap();
        let mut index = Index::open(&vault).unwrap();
        index.sync(&vault).unwrap();
        (dir, vault, index)
    }

    fn read(dir: &TempDir, p: &str) -> String {
        fs::read_to_string(dir.0.join(p)).unwrap()
    }

    #[test]
    fn a_move_rewrites_links_to_the_note() {
        let (dir, vault, index) = setup(
            "mv-in",
            &[
                ("old.md", "no heading\n"),
                ("titled.md", "# Kept Title\n"),
                (
                    "d/src.md",
                    "[[old]] [[old#Part|label]] [[OLD]] [x](../old.md#part) [y](<../old.md>) [[Kept Title]] [[other]]\n",
                ),
            ],
        );
        let moved = move_note(&vault, &index, "old", "notes/new name").unwrap();
        assert_eq!(moved.rewritten, ["d/src"]);
        assert_eq!(
            read(&dir, "d/src.md"),
            "[[new name]] [[new name#Part|label]] [[new name]] [x](../notes/new%20name.md#part) [y](<../notes/new name.md>) [[Kept Title]] [[other]]\n"
        );
        assert!(!dir.0.join("old.md").exists());
        assert_eq!(read(&dir, "notes/new name.md"), "no heading\n");

        // An explicit title still resolves after a move, so it is not rewritten.
        let mut index = index;
        index.sync(&vault).unwrap();
        fs::write(dir.0.join("e.md"), "[[Kept Title]] [[titled]]\n").unwrap();
        index.sync(&vault).unwrap();
        move_note(&vault, &index, "titled", "t2").unwrap();
        assert_eq!(read(&dir, "e.md"), "[[Kept Title]] [[t2]]\n");
    }

    #[test]
    fn the_moved_note_rebases_its_own_relative_links() {
        let (dir, vault, index) = setup(
            "mv-own",
            &[
                (
                    "a/n.md",
                    "![p](img/p.png) [b](../b.md) [me](n.md) [[b]] [w](https://x.org/a.md)\n",
                ),
                ("b.md", ""),
            ],
        );
        move_note(&vault, &index, "a/n", "c/d/n").unwrap();
        assert_eq!(
            read(&dir, "c/d/n.md"),
            "![p](../../a/img/p.png) [b](../../b.md) [me](n.md) [[b]] [w](https://x.org/a.md)\n"
        );
        // The emptied folder is removed.
        assert!(!dir.0.join("a").exists());
    }

    #[test]
    fn a_name_taken_elsewhere_gets_the_full_path() {
        let (dir, vault, index) = setup(
            "mv-amb",
            &[("x.md", ""), ("other/y.md", ""), ("s.md", "[[x]]\n")],
        );
        move_note(&vault, &index, "x", "z/y").unwrap();
        assert_eq!(read(&dir, "s.md"), "[[z/y]]\n");
    }

    #[test]
    fn refuses_to_replace_and_keeps_concurrent_edits() {
        let (dir, vault, index) = setup("mv-refuse", &[("a.md", ""), ("b.md", "")]);
        assert!(matches!(
            move_note(&vault, &index, "a", "b"),
            Err(ManageError::Vault(VaultError::Exists(_)))
        ));
        assert!(matches!(
            move_note(&vault, &index, "a", "../out"),
            Err(ManageError::Vault(VaultError::BadId(_)))
        ));
        assert_eq!(read(&dir, "a.md"), "");
    }

    #[test]
    fn trash_keeps_the_file_and_numbers_repeats() {
        let (dir, vault, _index) = setup("trash", &[("d/a.md", "one")]);
        let first = trash_note(&vault, "d/a").unwrap();
        assert_eq!(fs::read_to_string(&first).unwrap(), "one");
        assert!(!dir.0.join("d").exists());
        dir.write("d/a.md", "two");
        let second = trash_note(&vault, "d/a").unwrap();
        assert!(second.ends_with("d/a (2).md"));
        assert!(read(&dir, ".katib/.gitignore").contains("trash/"));
    }

    #[test]
    fn untitled_names_count_up() {
        let (dir, vault, _index) = setup("untitled", &[("Untitled.md", ""), ("f/x.md", "")]);
        assert_eq!(untitled_id(&vault, ""), "Untitled 2");
        assert_eq!(untitled_id(&vault, "f"), "f/Untitled");
        drop(dir);
    }

    #[test]
    fn a_folder_move_carries_its_files_and_links() {
        let (dir, vault, index) = setup(
            "mv-folder",
            &[
                (
                    "p/a.md",
                    "[b](b.md) [img](img/x.png) [up](../top.md) [[b]]\n",
                ),
                ("p/b.md", "# B\n"),
                ("p/img/x.png", "png"),
                (
                    "top.md",
                    "[a](p/a.md) ![x](p/img/x.png) [root](/p/b.md) [[p/b]]\n",
                ),
            ],
        );
        let moved = move_folder(&vault, &index, "p", "q/r").unwrap();
        assert_eq!(moved.new_id("p/a"), Some("q/r/a"));
        assert_eq!(moved.rewritten, ["top"]);
        // Links inside the folder stay as they were; only the way out changes.
        assert_eq!(
            read(&dir, "q/r/a.md"),
            "[b](b.md) [img](img/x.png) [up](../../top.md) [[b]]\n"
        );
        assert_eq!(
            read(&dir, "top.md"),
            "[a](q/r/a.md) ![x](q/r/img/x.png) [root](/q/r/b.md) [[q/r/b]]\n"
        );
        assert_eq!(read(&dir, "q/r/img/x.png"), "png");
        assert!(!dir.0.join("p").exists());
        assert!(matches!(
            move_folder(&vault, &index, "q", "q/r/s"),
            Err(ManageError::Vault(VaultError::BadId(_)))
        ));
    }

    #[test]
    fn reference_definitions_are_rewritten_once() {
        let (dir, vault, index) = setup(
            "mv-ref",
            &[
                ("old.md", ""),
                (
                    "s.md",
                    "see [one][ref] and [Ref][] and ![pic][img]\n\n[ref]: old.md#top \"Title\"\n  [img]: <old.md>\n",
                ),
            ],
        );
        move_note(&vault, &index, "old", "d/new").unwrap();
        assert_eq!(
            read(&dir, "s.md"),
            "see [one][ref] and [Ref][] and ![pic][img]\n\n[ref]: d/new.md#top \"Title\"\n  [img]: <d/new.md>\n"
        );
    }

    #[test]
    fn retarget_repairs_one_link_and_refuses_a_stale_one() {
        let (dir, vault, mut index) = setup(
            "retarget",
            &[
                (
                    "a.md",
                    "[[Projcts#Plan|the plan]] and [x](sub/gone.md#top)\n",
                ),
                ("p/projects.md", "# Projects\n"),
                ("sub/b.md", "# B\n"),
            ],
        );
        let links = parse(&read(&dir, "a.md")).links;
        retarget(&vault, &index, "a", &links[0], "p/projects").unwrap();
        assert_eq!(
            read(&dir, "a.md"),
            "[[projects#Plan|the plan]] and [x](sub/gone.md#top)\n"
        );
        // The ranges moved, so the old listing is stale.
        assert!(matches!(
            retarget(&vault, &index, "a", &links[1], "sub/b"),
            Err(ManageError::Changed(_))
        ));
        index.sync(&vault).unwrap();
        let links = parse(&read(&dir, "a.md")).links;
        retarget(&vault, &index, "a", &links[1], "sub/b").unwrap();
        assert_eq!(
            read(&dir, "a.md"),
            "[[projects#Plan|the plan]] and [x](sub/b.md#top)\n"
        );
    }
}
