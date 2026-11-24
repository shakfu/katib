//! Attachments (design.md section 6): files named by the SHA-256 of their
//! bytes, under `attachments/`, linked from notes by relative path. Only the
//! `git` backend exists: the file itself is committed.

use std::{collections::HashSet, fs, io, path::Path};

use sha2::{Digest as _, Sha256};

use crate::{
    Index, IndexError, Vault, VaultError,
    config::{Backend, Config},
    paths::{percent_encode, relative_to},
};

/// Larger files are refused: the `git` backend commits every byte, and hosts
/// such as GitHub reject files over 100 MB.
pub const MAX_ATTACHMENT: u64 = 50 * 1024 * 1024;

const DIR: &str = "attachments";

#[derive(Debug)]
pub enum AttachError {
    TooLarge(u64),
    /// The vault's objects backend cannot store files yet.
    Unsupported(String),
    Io(io::Error),
    Vault(VaultError),
    Index(IndexError),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(
                f,
                "{} MB is over the {} MB limit for attachments",
                n / 1024 / 1024,
                MAX_ATTACHMENT / 1024 / 1024
            ),
            Self::Unsupported(why) => write!(f, "{why}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Vault(e) => write!(f, "{e}"),
            Self::Index(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// Store `bytes` as an attachment named after `name`; returns its vault path,
/// `attachments/3f/3f9a...c2-name.png`. The same bytes always get the same
/// directory and hash, so storing twice, or on two machines, adds one file.
pub fn store(vault: &Vault, bytes: &[u8], name: &str) -> Result<String, AttachError> {
    if bytes.len() as u64 > MAX_ATTACHMENT {
        return Err(AttachError::TooLarge(bytes.len() as u64));
    }
    let config = Config::read(vault);
    if let Some(problem) = config.problem {
        return Err(AttachError::Unsupported(format!(
            ".katib/config.toml: {problem}"
        )));
    }
    match config.backend {
        Backend::Git => {}
        Backend::Lfs => ensure_lfs_attributes(vault).map_err(AttachError::Io)?,
        Backend::Store => {
            return Err(AttachError::Unsupported(
                "the \"store\" objects backend is not built yet; use \"git\" or \"lfs\"".into(),
            ));
        }
    }
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let rel = format!("{DIR}/{}/{hash}-{}", &hash[..2], clean_name(name));
    let path = vault.root().join(&rel);
    // Same name and same hash: the same bytes are already there, unless the
    // file is an LFS pointer whose object was never downloaded.
    if !path.exists() || is_lfs_pointer(&path) {
        // Write through a temporary file: a torn attachment would keep its hash name.
        write_bytes(&path, bytes).map_err(AttachError::Io)?;
    }
    Ok(rel)
}

/// Whether the file at `path` is a git-lfs pointer rather than its content.
pub fn is_lfs_pointer(path: &Path) -> bool {
    use std::io::Read as _;
    const MAGIC: &[u8] = b"version https://git-lfs.github.com/spec/v1";
    let mut head = [0; MAGIC.len()];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == MAGIC)
}

/// The `.gitattributes` line that sends attachments through git-lfs.
pub const LFS_ATTRIBUTES: &str = "attachments/** filter=lfs diff=lfs merge=lfs -text";

/// Add [`LFS_ATTRIBUTES`] to the vault's `.gitattributes` if it is missing.
/// `.gitattributes` is committed, so every clone stores attachments in LFS.
pub fn ensure_lfs_attributes(vault: &Vault) -> io::Result<()> {
    let path = vault.root().join(".gitattributes");
    let current = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if current.lines().any(|l| l.trim() == LFS_ATTRIBUTES) {
        return Ok(());
    }
    let sep = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    crate::write_atomic(&path, &format!("{current}{sep}{LFS_ATTRIBUTES}\n"))
}

/// Copy the file at `source` in as an attachment.
pub fn store_file(vault: &Vault, source: &Path) -> Result<String, AttachError> {
    let size = fs::metadata(source).map_err(AttachError::Io)?.len();
    if size > MAX_ATTACHMENT {
        return Err(AttachError::TooLarge(size));
    }
    let bytes = fs::read(source).map_err(AttachError::Io)?;
    let name = source
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    store(vault, &bytes, &name)
}

/// `write_atomic` for bytes.
fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.katib-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A file name safe in a path and a link: letters, digits, `.`, `-`, `_`;
/// anything else becomes `-`. At most 80 bytes, keeping the extension.
fn clean_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let keep = c.is_alphanumeric() || matches!(c, '.' | '-' | '_');
        let c = if keep { c } else { '-' };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out = out.replace("-.", ".").trim_matches(['-', '.']).to_owned();
    if out.is_empty() {
        return "file".into();
    }
    if out.len() <= 80 {
        return out;
    }
    let ext = out.rsplit_once('.').map_or("", |(_, e)| e);
    let ext = if ext.len() <= 10 { ext } else { "" };
    let mut cut = 80 - ext.len() - 1;
    while !out.is_char_boundary(cut) {
        cut -= 1;
    }
    match ext {
        "" => out[..cut].to_owned(),
        ext => format!("{}.{ext}", &out[..cut]),
    }
}

/// The markdown that links note `from` to attachment `path`: an image for
/// image types, a plain link otherwise. The label is the original name.
pub fn link_markdown(from: &str, path: &str, name: &str) -> String {
    let target = percent_encode(&relative_to(from, path));
    let label: String = name.chars().filter(|c| !matches!(c, '[' | ']')).collect();
    if is_image(path) {
        format!("![{label}]({target})")
    } else {
        format!("[{label}]({target})")
    }
}

pub fn is_image(path: &str) -> bool {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp")
    )
}

/// Attachments no note links to, by vault path. Deleting them is a separate,
/// explicit step (design.md section 6.2).
pub fn unreferenced(vault: &Vault, index: &Index) -> Result<Vec<String>, AttachError> {
    let linked: HashSet<String> = index.linked_files().map_err(AttachError::Index)?;
    let mut out = Vec::new();
    let root = vault.root().join(DIR);
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(items) = fs::read_dir(&dir) else {
            continue;
        };
        for item in items.flatten() {
            let path = item.path();
            let name = item.file_name().to_string_lossy().into_owned();
            let Ok(kind) = item.file_type() else { continue };
            if name.starts_with('.') {
                continue;
            } else if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && let Ok(rel) = path.strip_prefix(vault.root())
            {
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if !linked.contains(&rel) {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Move attachment `rel` (a vault path) to `.katib/trash/`.
pub fn trash_attachment(vault: &Vault, rel: &str) -> Result<(), AttachError> {
    if !rel.starts_with("attachments/") || rel.split('/').any(|p| p == ".." || p.is_empty()) {
        return Err(AttachError::Io(io::ErrorKind::InvalidInput.into()));
    }
    let dest = vault
        .katib_dir()
        .map_err(AttachError::Vault)?
        .join("trash")
        .join(rel);
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(AttachError::Io)?;
    }
    fs::rename(vault.root().join(rel), dest).map_err(AttachError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TempDir;

    #[test]
    fn same_bytes_same_name() {
        let dir = TempDir::new("attach-store");
        let vault = Vault::open(&dir.0).unwrap();
        let a = store(&vault, b"hello", "My Photo (1).PNG").unwrap();
        // SHA-256 of "hello".
        assert_eq!(
            a,
            "attachments/2c/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824-My-Photo-1.PNG"
        );
        assert_eq!(store(&vault, b"hello", "My Photo (1).PNG").unwrap(), a);
        assert_eq!(fs::read(dir.0.join(&a)).unwrap(), b"hello");
        let b = store(&vault, b"other", "My Photo (1).PNG").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn storing_again_replaces_an_lfs_pointer() {
        let dir = TempDir::new("attach-pointer");
        let vault = Vault::open(&dir.0).unwrap();
        let a = store(&vault, b"hello", "x.png").unwrap();
        let pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:2c\nsize 5\n";
        fs::write(dir.0.join(&a), pointer).unwrap();
        assert!(is_lfs_pointer(&dir.0.join(&a)));
        store(&vault, b"hello", "x.png").unwrap();
        assert_eq!(fs::read(dir.0.join(&a)).unwrap(), b"hello");
        assert!(!is_lfs_pointer(&dir.0.join(&a)));
    }

    #[test]
    fn names_are_cleaned_and_bounded() {
        assert_eq!(clean_name("a b/../c?.pdf"), "a-b..-c.pdf");
        assert_eq!(clean_name("..."), "file");
        let long = format!("{}.jpeg", "x".repeat(200));
        let cleaned = clean_name(&long);
        assert!(
            cleaned.len() <= 80 && cleaned.ends_with(".jpeg"),
            "{cleaned}"
        );
    }

    #[test]
    fn links_are_relative_and_typed() {
        let p = "attachments/2c/2c-x y.png";
        assert_eq!(
            link_markdown("dir/note", p, "x y.png"),
            "![x y.png](../attachments/2c/2c-x%20y.png)"
        );
        assert_eq!(
            link_markdown("note", "attachments/aa/aa-r.pdf", "r.pdf"),
            "[r.pdf](attachments/aa/aa-r.pdf)"
        );
    }

    #[test]
    fn too_large_is_refused() {
        let dir = TempDir::new("attach-big");
        let vault = Vault::open(&dir.0).unwrap();
        let big = vec![0u8; (MAX_ATTACHMENT + 1) as usize];
        assert!(matches!(
            store(&vault, &big, "x.bin"),
            Err(AttachError::TooLarge(_))
        ));
    }

    #[test]
    fn unreferenced_lists_only_unlinked_files() {
        let dir = TempDir::new("attach-gc");
        let vault = Vault::open(&dir.0).unwrap();
        let used = store(&vault, b"used", "u.png").unwrap();
        let unused = store(&vault, b"unused", "n.png").unwrap();
        dir.write(
            "d/note.md",
            &format!("{}\n", link_markdown("d/note", &used, "u.png")),
        );
        let mut index = Index::open(&vault).unwrap();
        index.sync(&vault).unwrap();
        assert_eq!(
            unreferenced(&vault, &index).unwrap(),
            std::slice::from_ref(&unused)
        );
        trash_attachment(&vault, &unused).unwrap();
        assert!(!dir.0.join(&unused).exists());
        assert!(trash_attachment(&vault, "attachments/../d/note.md").is_err());
    }

    #[test]
    fn the_backend_decides_how_files_are_kept() {
        let dir = TempDir::new("attach-backend");
        let vault = Vault::open(&dir.0).unwrap();
        store(&vault, b"plain", "a.png").unwrap();
        assert!(!dir.0.join(".gitattributes").exists());

        dir.write(".katib/config.toml", "[objects]\nbackend = \"lfs\"\n");
        dir.write(".gitattributes", "*.md text");
        store(&vault, b"one", "b.png").unwrap();
        store(&vault, b"two", "c.png").unwrap();
        assert_eq!(
            read(&dir, ".gitattributes"),
            format!("*.md text\n{LFS_ATTRIBUTES}\n")
        );

        dir.write(".katib/config.toml", "[objects]\nbackend = \"store\"\n");
        assert!(matches!(
            store(&vault, b"x", "x.png"),
            Err(AttachError::Unsupported(_))
        ));
    }

    fn read(dir: &TempDir, p: &str) -> String {
        fs::read_to_string(dir.0.join(p)).unwrap()
    }
}
