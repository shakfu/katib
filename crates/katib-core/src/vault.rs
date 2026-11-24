use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::note::{self, Note};
use crate::write::write_atomic;

/// A directory of notes. Every `.md` file outside hidden directories is a note.
#[derive(Clone, Debug)]
pub struct Vault {
    root: PathBuf,
}

#[derive(Debug)]
pub enum VaultError {
    NotADirectory(PathBuf),
    Io(PathBuf, io::Error),
    /// A note id that is empty, absolute, hidden, or climbs out of the vault.
    BadId(String),
    Exists(String),
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotADirectory(p) => write!(f, "not a directory: {}", p.display()),
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::BadId(id) => write!(f, "not a valid note name: {id:?}"),
            Self::Exists(id) => write!(f, "a note named {id:?} exists"),
        }
    }
}

impl std::error::Error for VaultError {}

/// A note file: its id (path without `.md`, `/`-separated) and its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub path: PathBuf,
}

impl Vault {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let root = root.into();
        let meta = fs::metadata(&root).map_err(|e| VaultError::Io(root.clone(), e))?;
        if !meta.is_dir() {
            return Err(VaultError::NotADirectory(root));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every note file, sorted by id. Symlinks are not followed; a link could
    /// point outside the vault, and sync would then write there.
    pub fn entries(&self) -> Result<Vec<Entry>, VaultError> {
        let mut out = Vec::new();
        self.collect(&self.root, &mut out)?;
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn collect(&self, dir: &Path, out: &mut Vec<Entry>) -> Result<(), VaultError> {
        let io = |e| VaultError::Io(dir.to_owned(), e);
        for item in fs::read_dir(dir).map_err(io)? {
            let item = item.map_err(io)?;
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let path = item.path();
            let kind = item.file_type().map_err(io)?;
            if kind.is_dir() {
                self.collect(&path, out)?;
            } else if kind.is_file() && name.ends_with(".md") {
                out.push(Entry {
                    id: self.id_for(&path),
                    path,
                });
            }
        }
        Ok(())
    }

    fn id_for(&self, path: &Path) -> String {
        let rel = path
            .strip_prefix(&self.root)
            .expect("entry is under the root");
        let parts: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        parts.join("/").trim_end_matches(".md").to_owned()
    }

    /// The entry for a note id, if the id is valid. The file may not exist.
    pub fn entry(&self, id: &str) -> Result<Entry, VaultError> {
        let id = id.trim().trim_end_matches(".md");
        let valid = !id.is_empty()
            && !id.contains('\\')
            && id
                .split('/')
                .all(|part| !part.is_empty() && !part.starts_with('.') && part.trim() == part);
        if !valid {
            return Err(VaultError::BadId(id.to_owned()));
        }
        Ok(Entry {
            id: id.to_owned(),
            path: self.root.join(format!("{id}.md")),
        })
    }

    /// Create a note; refuses to replace an existing file.
    pub fn create(&self, id: &str, text: &str) -> Result<Entry, VaultError> {
        let entry = self.entry(id)?;
        if let Some(dir) = entry.path.parent() {
            fs::create_dir_all(dir).map_err(|e| VaultError::Io(dir.to_owned(), e))?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&entry.path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::AlreadyExists => VaultError::Exists(entry.id.clone()),
                _ => VaultError::Io(entry.path.clone(), e),
            })?;
        io::Write::write_all(&mut file, text.as_bytes())
            .map_err(|e| VaultError::Io(entry.path.clone(), e))?;
        Ok(entry)
    }

    /// `.katib/`, with a `.gitignore` listing its machine-local parts.
    pub fn katib_dir(&self) -> Result<PathBuf, VaultError> {
        let dir = self.root.join(".katib");
        let ignore = dir.join(".gitignore");
        let current = fs::read_to_string(&ignore).unwrap_or_default();
        let missing: String = ["cache/", "drafts/", "trash/"]
            .iter()
            .filter(|line| !current.lines().any(|l| l.trim() == **line))
            .map(|line| format!("{line}\n"))
            .collect();
        if !missing.is_empty() {
            let sep = if current.is_empty() || current.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            write_atomic(&ignore, &format!("{current}{sep}{missing}"))
                .map_err(|e| VaultError::Io(ignore.clone(), e))?;
        }
        Ok(dir)
    }

    fn draft_path(&self, entry: &Entry) -> Result<PathBuf, VaultError> {
        Ok(self
            .katib_dir()?
            .join("drafts")
            .join(format!("{}.md", entry.id)))
    }

    /// Keep unsaved text for crash recovery.
    pub fn write_draft(&self, entry: &Entry, text: &str) -> Result<(), VaultError> {
        let path = self.draft_path(entry)?;
        write_atomic(&path, text).map_err(|e| VaultError::Io(path, e))
    }

    pub fn read_draft(&self, entry: &Entry) -> Option<String> {
        fs::read_to_string(self.draft_path(entry).ok()?).ok()
    }

    pub fn remove_draft(&self, entry: &Entry) {
        if let Ok(path) = self.draft_path(entry) {
            let _ = fs::remove_file(path);
        }
    }

    /// Read and parse one note.
    pub fn read(&self, entry: &Entry) -> Result<(String, Note), VaultError> {
        let source =
            fs::read_to_string(&entry.path).map_err(|e| VaultError::Io(entry.path.clone(), e))?;
        let note = note::parse(&source);
        Ok((source, note))
    }
}

impl Entry {
    /// The note's title: front matter `title`, else its first level-one heading,
    /// else its file name.
    pub fn title(&self, note: &Note) -> String {
        note.heading_title
            .clone()
            .unwrap_or_else(|| self.id.rsplit('/').next().unwrap_or(&self.id).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TempDir;

    #[test]
    fn lists_notes_skipping_hidden_and_non_markdown() {
        let dir = TempDir::new("list");
        dir.write("b.md", "# B");
        dir.write("a/deep/c.md", "");
        dir.write("a/image.png", "");
        dir.write(".katib/drafts/x.md", "");
        dir.write(".git/y.md", "");
        let vault = Vault::open(&dir.0).unwrap();
        let ids: Vec<_> = vault.entries().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["a/deep/c", "b"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let outside = TempDir::new("outside");
        outside.write("secret.md", "");
        let dir = TempDir::new("links");
        dir.write("a.md", "");
        std::os::unix::fs::symlink(&outside.0, dir.0.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("secret.md"), dir.0.join("s.md")).unwrap();
        let ids: Vec<_> = Vault::open(&dir.0)
            .unwrap()
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn title_falls_back_to_file_name() {
        let dir = TempDir::new("title");
        dir.write("dir/plain note.md", "no heading");
        dir.write("h.md", "# Heading");
        let vault = Vault::open(&dir.0).unwrap();
        let titles: Vec<_> = vault
            .entries()
            .unwrap()
            .iter()
            .map(|e| e.title(&vault.read(e).unwrap().1))
            .collect();
        assert_eq!(titles, ["plain note", "Heading"]);
    }

    #[test]
    fn open_rejects_a_file() {
        let dir = TempDir::new("file");
        dir.write("f.md", "");
        assert!(matches!(
            Vault::open(dir.0.join("f.md")),
            Err(VaultError::NotADirectory(_))
        ));
    }

    #[test]
    fn entry_rejects_ids_that_leave_the_vault() {
        let dir = TempDir::new("ids");
        let vault = Vault::open(&dir.0).unwrap();
        for bad in [
            "",
            "../x",
            "a/../../x",
            "/abs",
            ".katib/x",
            "a//b",
            "a\\b",
            "a /b",
        ] {
            assert!(
                matches!(vault.entry(bad), Err(VaultError::BadId(_))),
                "{bad:?}"
            );
        }
        assert_eq!(vault.entry("dir/new note.md").unwrap().id, "dir/new note");
    }

    #[test]
    fn create_does_not_replace() {
        let dir = TempDir::new("create");
        let vault = Vault::open(&dir.0).unwrap();
        let e = vault.create("d/n", "# N\n").unwrap();
        assert_eq!(fs::read_to_string(&e.path).unwrap(), "# N\n");
        assert!(matches!(
            vault.create("d/n", "x"),
            Err(VaultError::Exists(_))
        ));
        assert_eq!(fs::read_to_string(&e.path).unwrap(), "# N\n");
    }

    #[test]
    fn drafts_round_trip_and_are_ignored_by_git() {
        let dir = TempDir::new("drafts");
        let vault = Vault::open(&dir.0).unwrap();
        let e = vault.entry("a/b").unwrap();
        assert_eq!(vault.read_draft(&e), None);
        vault.write_draft(&e, "unsaved").unwrap();
        assert_eq!(vault.read_draft(&e).as_deref(), Some("unsaved"));
        vault.remove_draft(&e);
        assert_eq!(vault.read_draft(&e), None);
        let ignore = fs::read_to_string(dir.0.join(".katib/.gitignore")).unwrap();
        assert!(ignore.contains("drafts/"));
        assert_eq!(vault.entries().unwrap(), []);
    }
}
