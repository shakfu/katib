//! Guarded, atomic writes. A save names the text it expects on disk, so an edit
//! made elsewhere (sync, another editor) is never overwritten unseen.

use std::{
    fmt,
    fs::{self, File},
    io::{self, Write as _},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub enum SaveError {
    /// The file no longer holds the expected text. `disk` is what it holds now;
    /// `None` when the file is gone.
    Conflict {
        disk: Option<String>,
    },
    Io(PathBuf, io::Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { disk: Some(_) } => write!(f, "the file changed on disk"),
            Self::Conflict { disk: None } => write!(f, "the file was deleted"),
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
        }
    }
}

impl std::error::Error for SaveError {}

/// Replace `path` with `new` if it holds `base` (`None`: if it does not exist).
///
/// The comparison is on the full text rather than a hash: the caller holds the
/// base text anyway, and equal text cannot collide.
pub fn save_guarded(path: &Path, base: Option<&str>, new: &str) -> Result<(), SaveError> {
    let io = |e| SaveError::Io(path.to_owned(), e);
    let disk = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(e)),
    };
    if disk.as_deref() != base {
        return Err(SaveError::Conflict { disk });
    }
    write_atomic(path, new).map_err(io)
}

/// Write through a temporary file in the same directory, then rename over
/// `path`, so a crash leaves the old file or the new one, never a torn one.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    // A leading dot keeps the vault listing and the watcher's note filter off it.
    let tmp = dir.join(format!(".{name}.katib-tmp"));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TempDir;

    #[test]
    fn saves_when_the_disk_matches() {
        let dir = TempDir::new("save-ok");
        let p = dir.0.join("a.md");
        fs::write(&p, "old").unwrap();
        save_guarded(&p, Some("old"), "new").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "new");
        assert!(!dir.0.join(".a.md.katib-tmp").exists());
    }

    #[test]
    fn refuses_when_the_disk_changed_and_keeps_it() {
        let dir = TempDir::new("save-conflict");
        let p = dir.0.join("a.md");
        fs::write(&p, "theirs").unwrap();
        match save_guarded(&p, Some("old"), "mine") {
            Err(SaveError::Conflict { disk }) => assert_eq!(disk.as_deref(), Some("theirs")),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&p).unwrap(), "theirs");
    }

    #[test]
    fn deleted_and_created_files_conflict() {
        let dir = TempDir::new("save-gone");
        let p = dir.0.join("a.md");
        assert!(matches!(
            save_guarded(&p, Some("old"), "mine"),
            Err(SaveError::Conflict { disk: None })
        ));
        save_guarded(&p, None, "first").unwrap();
        assert!(matches!(
            save_guarded(&p, None, "second"),
            Err(SaveError::Conflict { disk: Some(_) })
        ));
    }
}
