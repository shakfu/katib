//! `.katib/config.toml`: the vault's committed settings (design.md section 2).
//!
//! Three keys exist, so this reads and writes them directly rather than
//! pulling in a TOML parser: a top-level `sync = true`,
//! `[objects] backend = "..."` and `[journal] folder = "..."`.

use std::{fs, io};

use crate::Vault;

/// The journal folder when the config names none.
pub const JOURNAL: &str = "journal";

/// Where attachment bytes live (design.md section 6.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// The files are committed as they are.
    #[default]
    Git,
    /// The files are committed as git-lfs pointers.
    Lfs,
    /// An object store; not built yet.
    Store,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Sync commits and pushes everything in the vault, so it is opt-in.
    pub sync: bool,
    pub backend: Backend,
    /// Where daily notes live, `journal/2026-11-24.md`.
    pub journal: String,
    /// A value katib did not understand, reported rather than guessed at.
    pub problem: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sync: false,
            backend: Backend::Git,
            journal: JOURNAL.to_owned(),
            problem: None,
        }
    }
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Lfs => "lfs",
            Self::Store => "store",
        }
    }
}

impl Config {
    fn path(vault: &Vault) -> std::path::PathBuf {
        vault.root().join(".katib/config.toml")
    }

    /// Write the settings back, keeping the file's comments and other lines.
    pub fn write(&self, vault: &Vault) -> io::Result<()> {
        let path = Self::path(vault);
        let mut text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        text = set_value(&text, "", "sync", if self.sync { "true" } else { "false" });
        text = set_value(&text, "objects", "backend", &quote(self.backend.name()));
        text = set_value(&text, "journal", "folder", &quote(&self.journal));
        vault.katib_dir().map_err(io::Error::other)?;
        crate::write_atomic(&path, &text)
    }

    pub fn read(vault: &Vault) -> Self {
        fs::read_to_string(Self::path(vault))
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        let mut table = String::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                table = name.trim().to_owned();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unquote(value.trim());
            let value = value.as_str();
            match (table.as_str(), key.trim()) {
                ("", "sync") => config.sync = value == "true",
                ("journal", "folder") => {
                    let folder = value.trim_matches('/');
                    // A daily note's id is `<folder>/<date>`; the vault checks it.
                    if !folder.is_empty() {
                        config.journal = folder.to_owned();
                    }
                }
                ("objects", "backend") => {
                    config.backend = match value {
                        "git" => Backend::Git,
                        "lfs" => Backend::Lfs,
                        "store" => Backend::Store,
                        other => {
                            config.problem = Some(format!("unknown objects backend {other:?}"));
                            Backend::Git
                        }
                    }
                }
                _ => {}
            }
        }
        config
    }
}

/// The text of a TOML basic string, or a bare value as it is.
fn unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_owned();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' {
            chars.next().unwrap_or(c)
        } else {
            c
        });
    }
    out
}

/// A TOML basic string. Only `"` and `\\` need escaping in the values written here.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `text` with `key = value` set in `[table]` ("" for the top level): an
/// existing line is replaced, else the key is added at the table's end.
fn set_value(text: &str, table: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut current = String::new();
    // Where the table's lines end, if the table exists.
    let mut end = table.is_empty().then_some(0);
    for (ix, line) in lines.iter_mut().enumerate() {
        let bare = line.split('#').next().unwrap_or("").trim();
        if let Some(name) = bare.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name.trim().to_owned();
            if current == table {
                end = Some(ix + 1);
            }
            continue;
        }
        if current != table {
            continue;
        }
        if bare.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
            *line = format!("{key} = {value}");
            return lines.join("\n") + "\n";
        }
        if !bare.is_empty() {
            end = Some(ix + 1);
        }
    }
    match end {
        Some(at) => lines.insert(at, format!("{key} = {value}")),
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{table}]"));
            lines.push(format!("{key} = {value}"));
        }
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_two_keys() {
        let c = Config::parse("# vault\nsync = true # on\n\n[objects]\nbackend = \"lfs\"\n");
        assert_eq!((c.sync, c.backend, c.problem), (true, Backend::Lfs, None));
        assert!(!Config::parse("sync = false\n").sync);
        assert!(!Config::parse("# sync = true\n").sync);
        // A key belongs to the table above it.
        assert!(!Config::parse("[objects]\nsync = true\n").sync);
        let bad = Config::parse("[objects]\nbackend = \"s3\"\n");
        assert_eq!(bad.backend, Backend::Git);
        assert!(bad.problem.is_some());
        assert_eq!(Config::parse("").journal, "journal");
        assert_eq!(
            Config::parse("[journal]\nfolder = \"/days/\"\n").journal,
            "days"
        );
    }

    #[test]
    fn writing_keeps_other_lines() {
        let text = "# mine\nsync = false # off\n\n[objects]\nbackend = \"git\"\n[other]\nx = 1\n";
        let text = set_value(text, "", "sync", "true");
        let text = set_value(&text, "objects", "backend", "\"lfs\"");
        let text = set_value(&text, "journal", "folder", "\"days\"");
        assert_eq!(
            text,
            "# mine\nsync = true\n\n[objects]\nbackend = \"lfs\"\n[other]\nx = 1\n\n[journal]\nfolder = \"days\"\n"
        );
        // A new top-level key goes before the first table.
        assert_eq!(
            set_value("[objects]\nbackend = \"git\"\n", "", "sync", "true"),
            "sync = true\n[objects]\nbackend = \"git\"\n"
        );
        // A key added to an existing table goes after its last setting.
        assert_eq!(
            set_value(
                "[objects]\nbackend = \"git\"\n\n[x]\n",
                "objects",
                "store",
                "1"
            ),
            "[objects]\nbackend = \"git\"\nstore = 1\n\n[x]\n"
        );
    }

    #[test]
    fn round_trip() {
        let dir = crate::test_dir::TempDir::new("config-write");
        let vault = Vault::open(&dir.0).unwrap();
        let config = Config {
            sync: true,
            backend: Backend::Lfs,
            journal: "days \"q\"".into(),
            problem: None,
        };
        config.write(&vault).unwrap();
        assert_eq!(Config::read(&vault), config);
    }
}
