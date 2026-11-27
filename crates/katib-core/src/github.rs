//! Exporting the vault as a copy that GitHub renders: a repository, where
//! `[[wiki]]` links show as text, or a GitHub wiki, whose page URLs ignore
//! folders. Rules checked on a live wiki: docs/dev/github-export.md.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    Index, IndexError, LinkKind, Target, Vault,
    export::ExportError,
    manage::{definition_replacement, destination_replacement},
    md,
    note::{parse, slug},
    paths::{page_folder, percent_encode, relative_to},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Same layout; wiki links become relative markdown links.
    Repository,
    /// Notes flat at the top, one unique page name each; files keep their paths.
    Wiki,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub notes: usize,
    pub files: usize,
    /// Links left as text because no single note matches them.
    pub unlinked: Vec<Unlinked>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Unlinked {
    pub id: String,
    /// The link as written.
    pub link: String,
}

/// Write the vault to `dest` in `style`. `dest` must be empty apart from
/// dot-entries, or a git work tree with nothing uncommitted, whose files are
/// then replaced. `index` should be synced first.
pub fn export(
    vault: &Vault,
    index: &Index,
    dest: &Path,
    style: Style,
) -> Result<Report, ExportError> {
    let dest = destination(vault.root(), dest)?;
    let files = walk(vault.root())?;
    let ids: Vec<&str> = files.iter().filter_map(|f| f.strip_suffix(".md")).collect();
    let names = (style == Style::Wiki).then(|| page_names(&ids));
    let mut report = Report::default();
    for rel in &files {
        let from = vault.root().join(rel);
        let to = match (rel.strip_suffix(".md"), &names) {
            (Some(id), Some(names)) => dest.join(format!("{}.md", names[id])),
            _ => dest.join(rel),
        };
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| ExportError::Io(parent.to_owned(), e))?;
        }
        match rel.strip_suffix(".md") {
            Some(id) => {
                let text =
                    fs::read_to_string(&from).map_err(|e| ExportError::Io(from.clone(), e))?;
                let (out, missed) = convert(&text, id, index, names.as_ref())?;
                fs::write(&to, out).map_err(|e| ExportError::Io(to.clone(), e))?;
                report.notes += 1;
                report
                    .unlinked
                    .extend(missed.into_iter().map(|link| Unlinked {
                        id: id.to_owned(),
                        link,
                    }));
            }
            None => {
                fs::copy(&from, &to).map_err(|e| ExportError::Io(from.clone(), e))?;
                report.files += 1;
            }
        }
    }
    if let Some(names) = &names
        && !names.values().any(|n| n.eq_ignore_ascii_case("Home"))
    {
        let home = dest.join("Home.md");
        fs::write(&home, contents(index, &ids, names)?).map_err(|e| ExportError::Io(home, e))?;
    }
    Ok(report)
}

/// A front page for a wiki with no root README: every note by title, in id order.
fn contents(
    index: &Index,
    ids: &[&str],
    names: &HashMap<String, String>,
) -> Result<String, IndexError> {
    let mut out = String::from("# Contents\n\n");
    for id in ids {
        let title = index.title(id)?.unwrap_or_else(|| (*id).to_owned());
        let page = percent_encode(&names[*id]);
        out += &format!("- [{}]({page})\n", title.replace(']', "\\]"));
    }
    Ok(out)
}

/// `text` (note `id`) with its wiki links as markdown links. With `names`
/// (a wiki export), relative links are rewritten to page names and to paths
/// from the wiki root, since every page's URL sits at the root, and front
/// matter, which a wiki draws as a rule and a heading, becomes its `title` as
/// a heading. Returns the links left unresolved.
fn convert(
    text: &str,
    id: &str,
    index: &Index,
    names: Option<&HashMap<String, String>>,
) -> Result<(String, Vec<String>), IndexError> {
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut missed = Vec::new();
    let note = parse(text);
    if names.is_some()
        && let Some(n) = md::parse(text).first()
        && n.kind == md::Kind::FrontMatter
    {
        // The block and the blank lines after it.
        let end = text.len() - text[n.range.end..].trim_start().len();
        let title = note
            .front
            .title
            .as_ref()
            .map_or(String::new(), |t| format!("# {t}\n\n"));
        edits.push((0..end, title));
    }
    for link in note.links {
        let s = &text[link.range.clone()];
        match link.kind {
            LinkKind::External => {}
            LinkKind::Wiki => {
                let inner = &s[2..s.len() - 2];
                let shown = match inner.split_once('|') {
                    Some((_, l)) if !l.trim().is_empty() => l.trim(),
                    Some((r, _)) => r.strip_suffix('\\').unwrap_or(r).trim(),
                    None => inner.trim(),
                };
                let fragment = link.fragment.as_deref().map(|f| format!("#{}", slug(f)));
                let href = match index.resolve(id, &link)? {
                    _ if link.target.is_empty() => fragment,
                    Target::Note(t) => {
                        let path = match names {
                            Some(names) => names[&t].clone(),
                            None => relative_to(id, &format!("{t}.md")),
                        };
                        // `|` would split a table cell.
                        let path = percent_encode(&path).replace('|', "%7C");
                        Some(path + fragment.as_deref().unwrap_or(""))
                    }
                    _ => None,
                };
                match href {
                    Some(href) => edits.push((
                        link.range.clone(),
                        format!("[{}]({href})", shown.replace(']', "\\]")),
                    )),
                    None => {
                        missed.push(s.to_owned());
                        edits.push((link.range.clone(), shown.to_owned()));
                    }
                }
            }
            LinkKind::Markdown if link.target.is_empty() => {}
            LinkKind::Markdown => {
                let Some(names) = names else { continue };
                let path = match index.resolve(id, &link)? {
                    Target::Note(t) => names[&t].clone(),
                    Target::File(p) => p,
                    _ => {
                        missed.push(s.to_owned());
                        continue;
                    }
                };
                if let Some(edit) = destination_replacement(text, &link, &path)
                    .or_else(|| definition_replacement(text, &link, &path))
                {
                    edits.push(edit);
                }
            }
        }
    }
    // Two reference links can share one definition; replace it once.
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    edits.dedup_by_key(|(r, _)| r.start);
    let mut out = text.to_owned();
    for (range, new) in edits {
        out.replace_range(range, &new);
    }
    Ok((out, missed))
}

/// A unique wiki page name per note id: its file name, a folder page its
/// folder's name, the root README `Home`. Names that clash, ignoring case,
/// fall back to the id with `/` as `-`, then a number. Spaces become `-`, as
/// GitHub's page URLs have them.
fn page_names(ids: &[&str]) -> HashMap<String, String> {
    let preferred = |id: &str| {
        let name = match page_folder(id) {
            Some(dir) => dir.rsplit('/').next().unwrap_or(dir),
            None if id.eq_ignore_ascii_case("readme") => "Home",
            None => id.rsplit('/').next().unwrap_or(id),
        };
        name.replace(' ', "-")
    };
    let mut count: HashMap<String, usize> = HashMap::new();
    for id in ids {
        *count.entry(preferred(id).to_lowercase()).or_default() += 1;
    }
    let mut taken = HashSet::new();
    let mut out = HashMap::new();
    // The root README is the wiki's front page, whatever else is named Home.
    let first = ids.iter().filter(|id| id.eq_ignore_ascii_case("readme"));
    for id in first.chain(
        ids.iter()
            .filter(|id| count[&preferred(id).to_lowercase()] == 1),
    ) {
        let name = preferred(id);
        if taken.insert(name.to_lowercase()) {
            out.insert((*id).to_owned(), name);
        }
    }
    for id in ids {
        if out.contains_key(*id) {
            continue;
        }
        let base = id.replace(['/', ' '], "-");
        let mut name = base.clone();
        let mut n = 2;
        while !taken.insert(name.to_lowercase()) {
            name = format!("{base}-{n}");
            n += 1;
        }
        out.insert((*id).to_owned(), name);
    }
    out
}

/// Every file under `root` by `/`-separated relative path, sorted. Dot-entries
/// (`.git`, `.katib`) and symlinks are skipped, as `Vault::entries` does.
fn walk(root: &Path) -> Result<Vec<String>, ExportError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let io = |e| ExportError::Io(dir.clone(), e);
        for item in fs::read_dir(&dir).map_err(io)? {
            let item = item.map_err(io)?;
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let kind = item.file_type().map_err(io)?;
            let path = item.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let rel = path.strip_prefix(root).expect("walked from the root");
                let parts: Vec<_> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `dest`, created if missing, after checking that writing there loses nothing.
/// Inside the vault it must be under a dot-folder, which the walk skips. A git
/// work tree with no changes, ignored files included, has its non-dot entries
/// removed, so notes deleted since the last export go too.
fn destination(root: &Path, dest: &Path) -> Result<PathBuf, ExportError> {
    let refuse = |why| Err(ExportError::Destination(dest.to_owned(), why));
    let io = |p: &Path| {
        let p = p.to_owned();
        move |e| ExportError::Io(p, e)
    };
    let root = root.canonicalize().map_err(io(root))?;
    let full = canonical_ahead(dest).map_err(io(dest))?;
    if let Ok(rel) = full.strip_prefix(&root)
        && !rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    {
        return refuse("is inside the vault");
    }
    fs::create_dir_all(&full).map_err(io(&full))?;
    let mut visible = Vec::new();
    for item in fs::read_dir(&full).map_err(io(&full))? {
        let item = item.map_err(io(&full))?;
        if !item.file_name().to_string_lossy().starts_with('.') {
            visible.push(item.path());
        }
    }
    if visible.is_empty() {
        return Ok(full);
    }
    if !full.join(".git").exists() {
        return refuse("is not empty, and is not a git work tree");
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&full)
        .args(["status", "--porcelain", "--ignored"])
        .output();
    match status {
        Ok(o) if o.status.success() && o.stdout.is_empty() => {}
        Ok(o) if o.status.success() => return refuse("has uncommitted or ignored files"),
        _ => return refuse("is not empty, and git could not check it"),
    }
    for path in visible {
        let removed = if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(io(&path))?;
    }
    Ok(full)
}

/// `p` made absolute, with its longest existing prefix canonicalized.
fn canonical_ahead(p: &Path) -> io::Result<PathBuf> {
    let p = std::path::absolute(p)?;
    let mut tail = Vec::new();
    let mut cur = p.as_path();
    loop {
        match cur.canonicalize() {
            Ok(c) => return Ok(tail.iter().rev().fold(c, |acc, part| acc.join(part))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
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
