//! Changing tasks and tags in note files. Each change is a guarded write of
//! one note, refused if the part it changes is no longer as listed.

use std::fs;

use crate::{
    Index, ManageError, Target, Vault, front,
    note::{due_word, parse},
    save_guarded,
};

type Result<T> = std::result::Result<T, ManageError>;

fn read(vault: &Vault, id: &str) -> Result<(std::path::PathBuf, String)> {
    let path = vault.entry(id)?.path;
    let text = fs::read_to_string(&path).map_err(|e| ManageError::Io(path.clone(), e))?;
    Ok((path, text))
}

fn write(path: &std::path::Path, id: &str, base: &str, new: &str) -> Result<()> {
    save_guarded(path, Some(base), new).map_err(|error| ManageError::Save {
        id: id.to_owned(),
        error,
        done: Vec::new(),
    })
}

/// `text` with the checklist marker at `marker` set to `done`, if a marker
/// that is `!done` is there.
pub fn with_item(text: &str, marker: usize, done: bool) -> Option<String> {
    let current = parse(text).items.into_iter().find(|i| i.marker == marker)?;
    if current.done == done {
        return None;
    }
    let mut out = text.to_owned();
    out.replace_range(marker..marker + 3, if done { "[x]" } else { "[ ]" });
    Some(out)
}

/// Tick (or untick) the checklist item at `marker` in note `src`.
pub fn set_item(vault: &Vault, src: &str, marker: usize, done: bool) -> Result<()> {
    let (path, text) = read(vault, src)?;
    let new = with_item(&text, marker, done).ok_or_else(|| ManageError::Changed(src.to_owned()))?;
    write(&path, src, &text, &new)
}

/// Set task note `id`'s front matter `status`.
pub fn set_status(vault: &Vault, id: &str, status: &str) -> Result<()> {
    let (path, text) = read(vault, id)?;
    if parse(&text).front.kind.as_deref() != Some("task") {
        return Err(ManageError::Changed(id.to_owned()));
    }
    let new = front::set(&text, "status", Some(&front::scalar(status)));
    write(&path, id, &text, &new)
}

/// Set note `id`'s tags; none removes the field.
pub fn set_tags(vault: &Vault, id: &str, tags: &[String]) -> Result<()> {
    let (path, text) = read(vault, id)?;
    let value = (!tags.is_empty()).then(|| front::list(tags));
    let new = front::set(&text, "tags", value.as_deref());
    if new != text {
        write(&path, id, &text, &new)?;
    }
    Ok(())
}

/// Turn the checklist item at `marker` in note `src` into a task note in
/// folder `dir` ("" for the top), titled by the item's text. The item becomes
/// a link to it. Returns the new note's id. The index must be current.
pub fn promote(
    vault: &Vault,
    index: &Index,
    src: &str,
    marker: usize,
    dir: &str,
) -> Result<String> {
    let (path, text) = read(vault, src)?;
    let item = parse(&text)
        .items
        .into_iter()
        .find(|i| i.marker == marker)
        .ok_or_else(|| ManageError::Changed(src.to_owned()))?;
    let title: String = item
        .text
        .split_whitespace()
        .filter(|w| due_word(w).is_none())
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        return Err(ManageError::Changed(src.to_owned()));
    }
    let name = file_name(&title);
    let id = free_id(vault, dir, &name);
    let mut fields = vec!["type: task".to_owned()];
    fields.push(format!(
        "status: {}",
        if item.done { "done" } else { "open" }
    ));
    if let Some(due) = &item.due {
        fields.push(format!("due: {due}"));
    }
    let page = format!("---\n{}\n---\n# {title}\n\n", fields.join("\n"));
    vault.create(&id, &page)?;
    // The title is the shorter link when it will name only the new note.
    let free = matches!(index.resolve_wiki(&title)?, Target::Missing(_));
    let link = if free && !title.contains(['#', '|', '[', ']']) {
        format!("[[{title}]]")
    } else {
        format!("[[{id}|{}]]", title.replace(['|', ']'], " "))
    };
    let end = text[marker..].find('\n').map_or(text.len(), |i| marker + i);
    let mut out = text.clone();
    out.replace_range(marker..end, &link);
    write(&path, src, &text, &out).inspect_err(|_| {
        // The item stays; do not leave a task note it does not link to.
        let _ = vault.entry(&id).map(|e| fs::remove_file(e.path));
    })?;
    Ok(id)
}

/// A file name from a title: path separators and characters that ids refuse
/// become spaces, runs of spaces one, cut to 80 bytes.
fn file_name(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '#' | '|' | '[' | ']') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let name = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = name.trim_start_matches('.').to_owned();
    let mut cut = name.len().min(80);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    let name = name[..cut].trim().to_owned();
    if name.is_empty() { "task".into() } else { name }
}

/// `dir/name`, or `dir/name 2` and so on when taken.
fn free_id(vault: &Vault, dir: &str, name: &str) -> String {
    let join = |n: &str| {
        if dir.is_empty() {
            n.to_owned()
        } else {
            format!("{dir}/{n}")
        }
    };
    let taken = |id: &str| vault.entry(id).is_ok_and(|e| e.path.exists());
    let mut id = join(name);
    let mut n = 2;
    while taken(&id) {
        id = join(&format!("{name} {n}"));
        n += 1;
    }
    id
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
    fn ticking_an_item_checks_it_first() {
        let (dir, vault, _) = setup("tick", &[("a.md", "- [ ] one\n- [x] two\n")]);
        set_item(&vault, "a", 2, true).unwrap();
        assert_eq!(read(&dir, "a.md"), "- [x] one\n- [x] two\n");
        // Already done, or no marker there: refused.
        assert!(matches!(
            set_item(&vault, "a", 2, true),
            Err(ManageError::Changed(_))
        ));
        assert!(matches!(
            set_item(&vault, "a", 0, true),
            Err(ManageError::Changed(_))
        ));
        set_item(&vault, "a", 12, false).unwrap();
        assert_eq!(read(&dir, "a.md"), "- [x] one\n- [ ] two\n");
    }

    #[test]
    fn status_and_tags_edit_front_matter() {
        let (dir, vault, _) = setup(
            "status",
            &[
                ("t.md", "---\ntype: task\nstatus: open\n---\n# T\n"),
                ("n.md", "# N\n"),
            ],
        );
        set_status(&vault, "t", "done").unwrap();
        assert_eq!(
            read(&dir, "t.md"),
            "---\ntype: task\nstatus: done\n---\n# T\n"
        );
        assert!(set_status(&vault, "n", "done").is_err());
        set_tags(&vault, "n", &["a b".into(), "c".into()]).unwrap();
        assert_eq!(read(&dir, "n.md"), "---\ntags: [a b, c]\n---\n# N\n");
        set_tags(&vault, "n", &[]).unwrap();
        assert_eq!(read(&dir, "n.md"), "# N\n");
    }

    #[test]
    fn promote_makes_a_task_note_and_links_it() {
        let (dir, vault, index) = setup(
            "promote",
            &[
                ("a.md", "# A\n- [ ] ship it due:2026-12-01\n- [x] Taken\n"),
                ("Taken.md", "# Taken\n"),
            ],
        );
        let id = promote(&vault, &index, "a", 6, "tasks").unwrap();
        assert_eq!(id, "tasks/ship it");
        assert_eq!(
            read(&dir, "tasks/ship it.md"),
            "---\ntype: task\nstatus: open\ndue: 2026-12-01\n---\n# ship it\n\n"
        );
        assert_eq!(read(&dir, "a.md"), "# A\n- [[ship it]]\n- [x] Taken\n");
        // A title another note has: the link names the id, with the title as label.
        let text = read(&dir, "a.md");
        let marker = text.find("[x]").unwrap();
        let id = promote(&vault, &index, "a", marker, "").unwrap();
        assert_eq!(id, "Taken 2");
        assert!(read(&dir, "a.md").ends_with("- [[Taken 2|Taken]]\n"));
        assert!(read(&dir, "Taken 2.md").contains("status: done"));
    }
}
