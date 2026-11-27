//! Exporting a note to HTML, PDF, Word, EPUB or GitHub markdown with Quarto
//! (design.md section 8).
//!
//! The note is staged as `.qmd` in a temporary folder, beside copies of the
//! files it links to, so its relative links resolve as in the vault. Links to
//! other notes become their label: a single output file has nowhere for them
//! to go. Quarto runs from the staged note's folder, since an embedded HTML
//! render fails from any other.
//!
//! A plain export runs no code: the staged front matter names the markdown
//! engine and drops `jupyter` and `knitr`, each of which starts its engine
//! even with no cells, and which Quarto's `-M` does not override. Code cells
//! print as code, and neither R nor Python is needed. A run
//! executes the cells in the note's own vault folder, so relative data paths
//! work; synced notes can carry anyone's code, so running is always asked for.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use crate::{
    Index, IndexError, LinkKind, Target, Vault, VaultError, front,
    manage::label_span,
    md,
    note::{parse, slug},
    paths::relative_path,
};

/// Vault-wide Quarto options under `.katib/`; a note's front matter wins.
pub const DEFAULTS: &str = "quarto.yml";

/// How long a plain export may take.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// How long a run may take: its code can be slow.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Html,
    Pdf,
    Docx,
    Epub,
    Gfm,
}

impl Format {
    pub const ALL: [Self; 5] = [Self::Html, Self::Pdf, Self::Docx, Self::Epub, Self::Gfm];

    pub fn label(self) -> &'static str {
        match self {
            Self::Html => "HTML",
            Self::Pdf => "PDF",
            Self::Docx => "Word",
            Self::Epub => "EPUB",
            Self::Gfm => "GitHub markdown",
        }
    }

    /// Quarto's `--to`. `typst` writes a PDF, with Typst bundled in Quarto;
    /// `typst-pdf` would too, but ignores the note's `format: typst` options.
    fn to(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Pdf => "typst",
            Self::Docx => "docx",
            Self::Epub => "epub",
            Self::Gfm => "gfm",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Epub => "epub",
            Self::Gfm => "md",
        }
    }
}

#[derive(Debug)]
pub enum ExportError {
    Vault(VaultError),
    Index(IndexError),
    Io(PathBuf, io::Error),
    NoQuarto,
    /// Quarto exited with an error; the text is the end of its log.
    Failed(String),
    TimedOut(Duration),
    Cancelled,
    /// An unknown `${name}` in the front matter.
    Variable(String),
    /// A GitHub export refused to write to this folder, for the reason given.
    Destination(PathBuf, &'static str),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vault(e) => write!(f, "{e}"),
            Self::Index(e) => write!(f, "{e}"),
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::NoQuarto => write!(f, "quarto was not found; see https://quarto.org"),
            Self::Failed(log) => write!(f, "quarto failed: {log}"),
            Self::TimedOut(d) => write!(f, "quarto took longer than {} s", d.as_secs()),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Variable(e) => write!(f, "{e}"),
            Self::Destination(p, why) => write!(f, "{} {why}", p.display()),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<VaultError> for ExportError {
    fn from(e: VaultError) -> Self {
        Self::Vault(e)
    }
}

impl From<IndexError> for ExportError {
    fn from(e: IndexError) -> Self {
        Self::Index(e)
    }
}

/// A note staged for Quarto. The staging folder is removed on drop.
#[derive(Debug)]
pub struct Staged {
    dir: PathBuf,
    /// The staged `.qmd`, at the note's id under `dir`.
    input: PathBuf,
    /// Its text, before a plain export sets the engine.
    text: String,
    /// `.katib/quarto.yml` with its variables expanded, if the vault has one.
    defaults: Option<String>,
    /// The note's folder in the vault, where a run executes.
    folder: PathBuf,
    /// Where the output goes: `.katib/out/<id>` without an extension.
    out: PathBuf,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Copy note `id` and the files it links to into a new staging folder.
pub fn stage(vault: &Vault, index: &Index, id: &str) -> Result<Staged, ExportError> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let entry = vault.entry(id)?;
    let io = |p: &Path| {
        let p = p.to_owned();
        move |e| ExportError::Io(p, e)
    };
    let text = fs::read_to_string(&entry.path).map_err(io(&entry.path))?;
    let home = std::env::home_dir().map(|h| h.to_string_lossy().replace('\\', "/"));
    let (text, mut files) = prepare(&text, id, vault, index, home.as_deref())?;
    let katib = vault.katib_dir()?;
    let defaults_path = katib.join(DEFAULTS);
    let defaults = match fs::read_to_string(&defaults_path) {
        Ok(yaml) => {
            let (yaml, named) = expand_yaml(&yaml, id, vault, home.as_deref())
                .map_err(|e| ExportError::Variable(format!(".katib/{DEFAULTS}: {e}")))?;
            files.extend(named);
            Some(yaml)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(ExportError::Io(defaults_path, e)),
    };
    let dir = loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // Not under `.katib/`: Quarto ignores a project file in a hidden folder.
        let dir = std::env::temp_dir().join(format!("katib-export-{}-{n}", std::process::id()));
        // A new folder only, readable by us only: in a shared `/tmp` another
        // user could have put a link at that name.
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(ExportError::Io(dir, e)),
        }
    };
    let staged = Staged {
        input: dir.join(format!("{id}.qmd")),
        text,
        defaults,
        folder: entry.path.parent().unwrap_or(vault.root()).to_owned(),
        out: katib.join("out").join(id),
        dir,
    };
    let mkdir = |to: &Path| {
        let parent = to.parent().unwrap_or(&staged.dir);
        fs::create_dir_all(parent).map_err(io(parent))
    };
    for file in &files {
        let (from, to) = (vault.root().join(file), staged.dir.join(file));
        mkdir(&to)?;
        fs::copy(&from, &to).map_err(io(&from))?;
    }
    mkdir(&staged.input)?;
    // Typst refuses files outside its root, which is the note's folder unless
    // a project file puts it here; `../img/a.png` would fail in a PDF.
    let project = staged.dir.join("_quarto.yml");
    fs::write(&project, "project:\n  type: default\n").map_err(io(&project))?;
    Ok(staged)
}

/// `text` (note `id`) as Quarto should read it, and the vault files that its
/// links or front matter name. Links to notes, folders and missing targets
/// become their label; a wiki link to a heading here becomes a `#slug` link.
/// In front matter, `${VAULT}` and `${HOME}` are expanded (see `expand`).
fn prepare(
    text: &str,
    id: &str,
    vault: &Vault,
    index: &Index,
    home: Option<&str>,
) -> Result<(String, Vec<String>), ExportError> {
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut files = Vec::new();
    for link in parse(text).links {
        let s = &text[link.range.clone()];
        match link.kind {
            LinkKind::External => {}
            LinkKind::Wiki => {
                let inner = &s[2..s.len() - 2];
                let (reference, label) = match inner.split_once('|') {
                    Some((r, l)) => (r.strip_suffix('\\').unwrap_or(r), Some(l)),
                    None => (inner, None),
                };
                let shown = label.unwrap_or(reference).trim().to_owned();
                let new = match &link.fragment {
                    Some(f) if link.target.is_empty() => format!("[{shown}](#{})", slug(f)),
                    _ => shown,
                };
                edits.push((link.range.clone(), new));
            }
            // A `#heading` link stays, as Quarto gives headings the same ids.
            LinkKind::Markdown if link.target.is_empty() => {}
            LinkKind::Markdown => match index.resolve(id, &link)? {
                Target::File(path) if vault.root().join(&path).is_file() => files.push(path),
                _ => {
                    let label = label_span(s).map_or("", |(a, b)| &s[a..b]);
                    edits.push((link.range.clone(), label.to_owned()));
                }
            },
        }
    }
    let mut out = text.to_owned();
    for (range, new) in edits.into_iter().rev() {
        out.replace_range(range, &new);
    }
    // Files the front matter names, such as `bibliography: refs.bib`.
    if let Some(n) = md::parse(text).first()
        && n.kind == md::Kind::FrontMatter
    {
        let (block, named) = expand_yaml(&text[n.range.clone()], id, vault, home)
            .map_err(|e| ExportError::Variable(format!("front matter: {e}")))?;
        files.extend(named);
        // Links are never in front matter, so the edits left it in place.
        out.replace_range(n.range.clone(), &block);
    }
    files.sort();
    files.dedup();
    Ok((out, files))
}

/// YAML for note `id` with its variables expanded, and the vault files its
/// values name. Quarto reads a path relative to the staged note, which sits
/// at its id, so `${VAULT}` is the way from there to the root.
fn expand_yaml(
    yaml: &str,
    id: &str,
    vault: &Vault,
    home: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    let depth = id.matches('/').count();
    let root = if depth == 0 {
        ".".to_owned()
    } else {
        vec![".."; depth].join("/")
    };
    let yaml = expand(yaml, &root, home)?;
    let files = front::scalars(&yaml)
        .iter()
        .filter(|v| !v.starts_with('/'))
        .filter_map(|v| relative_path(id, v))
        .filter(|p| vault.root().join(p).is_file())
        .collect();
    Ok((yaml, files))
}

/// Front matter `block` with `${VAULT}` as `root`, the vault root relative
/// to the staged note, and `${HOME}` as `home`. Any other `${name}` is an
/// error naming it, so a misspelling fails rather than reaching Quarto; other
/// environment variables are not read, since a synced note could put one into
/// a shared document. A `${` not followed by a name, as in `${}^{14}C$`, stays.
fn expand(block: &str, root: &str, home: Option<&str>) -> Result<String, String> {
    let mut out = String::with_capacity(block.len());
    let mut rest = block;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let name = after.find('}').map(|end| &after[..end]).filter(|n| {
            n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        let Some(name) = name else {
            out.push_str("${");
            rest = after;
            continue;
        };
        out.push_str(match (name, home) {
            ("VAULT", _) => root,
            ("HOME", Some(home)) => home,
            ("HOME", None) => return Err("${HOME}: no home folder is set".into()),
            _ => {
                return Err(format!(
                    "${{{name}}}: only ${{VAULT}} and ${{HOME}} are expanded"
                ));
            }
        });
        rest = &after[name.len() + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Render `staged` to `format`, running its code if `execute`, and move the
/// output to `.katib/out/<id>.<ext>`. Setting `cancel` stops Quarto.
pub fn render(
    staged: &Staged,
    format: Format,
    execute: bool,
    cancel: &AtomicBool,
) -> Result<PathBuf, ExportError> {
    let quarto = find_quarto().ok_or(ExportError::NoQuarto)?;
    let cwd = staged.input.parent().unwrap_or(&staged.dir);
    let stem = staged
        .input
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let output = format!("{stem}.{}", format.ext());
    let text = if execute {
        staged.text.clone()
    } else {
        plain(&staged.text)
    };
    fs::write(&staged.input, text).map_err(|e| ExportError::Io(staged.input.clone(), e))?;
    let defaults = staged.dir.join(DEFAULTS);
    if let Some(yaml) = &staged.defaults {
        // Here too an engine key starts its engine, whatever the note says.
        let yaml = if execute {
            yaml.clone()
        } else {
            without_engines(yaml)
        };
        fs::write(&defaults, yaml).map_err(|e| ExportError::Io(defaults.clone(), e))?;
    }
    let mut cmd = Command::new(quarto);
    cmd.current_dir(cwd)
        .arg("render")
        .arg(staged.input.file_name().unwrap_or_default())
        .args(["--to", format.to(), "--output", &output]);
    if format == Format::Html {
        cmd.arg("--embed-resources");
    }
    if execute {
        cmd.arg("--execute")
            .arg("--execute-dir")
            .arg(&staged.folder);
    }
    if staged.defaults.is_some() {
        // A note's own front matter wins over these.
        cmd.arg("--metadata-file").arg(&defaults);
    }
    if let Some(path) = search_path() {
        cmd.env("PATH", path);
    }
    let timeout = if execute { RUN_TIMEOUT } else { TIMEOUT };
    run(cmd, &staged.dir.join("quarto.log"), timeout, cancel)?;
    let from = cwd.join(&output);
    let to = staged.out.with_added_extension(format.ext());
    let parent = to.parent().unwrap_or(&staged.dir);
    fs::create_dir_all(parent).map_err(|e| ExportError::Io(parent.to_owned(), e))?;
    // The temporary folder may be on another file system.
    if fs::rename(&from, &to).is_err() {
        fs::copy(&from, &to).map_err(|e| ExportError::Io(from, e))?;
    }
    Ok(to)
}

/// `text` with front matter that makes Quarto run none of its code.
fn plain(text: &str) -> String {
    let text = front::set(text, "jupyter", None);
    let text = front::set(&text, "knitr", None);
    front::set(&text, "engine", Some("markdown"))
}

/// YAML without the keys that start an engine: `engine`, `jupyter`, `knitr`.
fn without_engines(yaml: &str) -> String {
    let mut text = format!("---\n{yaml}\n---\n");
    for key in ["engine", "jupyter", "knitr"] {
        text = front::set(&text, key, None);
    }
    let inner = text.strip_prefix("---\n").unwrap_or(&text);
    inner
        .rsplit_once("---\n")
        .map_or("", |(yaml, _)| yaml)
        .trim_end()
        .to_owned()
        + "\n"
}

/// Run `cmd` with its output in `log`, killing it on `cancel` or after `timeout`.
fn run(
    mut cmd: Command,
    log: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<(), ExportError> {
    let io = |e| ExportError::Io(log.to_owned(), e);
    let file = fs::File::create(log).map_err(io)?;
    cmd.stdin(Stdio::null())
        .stdout(file.try_clone().map_err(io)?)
        .stderr(file);
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => ExportError::NoQuarto,
        _ => io(e),
    })?;
    let start = Instant::now();
    let status: ExitStatus = loop {
        if let Some(status) = child.try_wait().map_err(io)? {
            break status;
        }
        let stop = if cancel.load(Ordering::Relaxed) {
            Some(ExportError::Cancelled)
        } else if start.elapsed() > timeout {
            Some(ExportError::TimedOut(timeout))
        } else {
            None
        };
        if let Some(e) = stop {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if status.success() {
        Ok(())
    } else {
        Err(ExportError::Failed(log_error(
            &fs::read_to_string(log).unwrap_or_default(),
        )))
    }
}

/// The lines of Quarto's log worth showing: from its first `ERROR`, else the
/// last few, without colour codes or the stack trace.
fn log_error(log: &str) -> String {
    let lines: Vec<String> = log
        .lines()
        .map(strip_ansi)
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("at "))
        .collect();
    let from = lines
        .iter()
        .position(|l| l.contains("ERROR"))
        .unwrap_or(lines.len().saturating_sub(5));
    let end = lines.len().min(from + 5);
    lines[from..end]
        .iter()
        .take_while(|l| !l.starts_with("Stack trace"))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: `ESC [`, parameters, then a final letter.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Where an app started from the macOS Finder looks for programs: its PATH
/// lacks the Homebrew and `/usr/local` folders that Quarto, R and Python use.
const EXTRA_PATH: [&str; 3] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/Applications/quarto/bin",
];

/// PATH with `EXTRA_PATH` appended, or `None` to leave it as it is.
fn search_path() -> Option<std::ffi::OsString> {
    if cfg!(windows) {
        return None;
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in EXTRA_PATH {
        if !dirs.iter().any(|d| d == Path::new(extra)) {
            dirs.push(extra.into());
        }
    }
    std::env::join_paths(dirs).ok()
}

/// The `quarto` program on PATH or in `EXTRA_PATH`.
pub fn find_quarto() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "quarto.exe"
    } else {
        "quarto"
    };
    let path = search_path().or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
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

    #[test]
    fn links_to_notes_become_labels_and_files_are_kept() {
        let (_d, vault, index) = setup(
            "export-prepare",
            &[
                ("other.md", "# Other\n"),
                ("p/README.md", ""),
                ("img/a.png", "png"),
                ("n/refs.bib", ""),
                ("style/a.css", ""),
                (
                    "n/note.md",
                    "---\ntitle: N\nbibliography: refs.bib\ncss: [\"../style/a.css\", gone.css]\n---\n\
                     [[other]] [[other|the other]] [[#My Heading]] [[gone]]\n\
                     [md](../other.md) [**dir**](../p/) [none](../none.md) [up](#top)\n\
                     ![pic](../img/a.png) [again](../img/a.png) <https://x.org>\n\
                     | a |\n|-|\n| [[other\\|cell]] |\n\
                     `[[code]]`\n",
                ),
            ],
        );
        let text = fs::read_to_string(vault.root().join("n/note.md")).unwrap();
        let (out, files) = prepare(&text, "n/note", &vault, &index, None).unwrap();
        assert_eq!(
            out,
            "---\ntitle: N\nbibliography: refs.bib\ncss: [\"../style/a.css\", gone.css]\n---\n\
             other the other [#My Heading](#my-heading) gone\n\
             md **dir** none [up](#top)\n\
             ![pic](../img/a.png) [again](../img/a.png) <https://x.org>\n\
             | a |\n|-|\n| cell |\n\
             `[[code]]`\n"
        );
        assert_eq!(files, ["img/a.png", "n/refs.bib", "style/a.css"]);
    }

    #[test]
    fn a_staged_note_sits_beside_its_files_and_is_removed_on_drop() {
        let (_d, vault, index) = setup(
            "export-stage",
            &[("img/a.png", "png"), ("n/note.md", "![x](../img/a.png)\n")],
        );
        let staged = stage(&vault, &index, "n/note").unwrap();
        assert_eq!(staged.input, staged.dir.join("n/note.qmd"));
        assert_eq!(staged.text, "![x](../img/a.png)\n");
        assert_eq!(fs::read(staged.dir.join("img/a.png")).unwrap(), b"png");
        assert_eq!(staged.folder, vault.root().join("n"));
        let dir = staged.dir.clone();
        drop(staged);
        assert!(!dir.exists());
    }

    #[test]
    fn a_plain_export_names_the_markdown_engine_and_no_other() {
        assert_eq!(plain("# A\n"), "---\nengine: markdown\n---\n# A\n");
        assert_eq!(
            plain(
                "---\ntitle: T\nengine: knitr\njupyter:\n  kernelspec:\n    name: p\nknitr:\n  opts_chunk: {}\n---\nx\n"
            ),
            "---\ntitle: T\nengine: markdown\n---\nx\n"
        );
    }

    #[test]
    fn front_matter_variables_name_the_vault_and_home_and_nothing_else() {
        let e = |b: &str| expand(b, "..", Some("/home/u"));
        assert_eq!(
            e("doc: ${VAULT}/t/house.docx\ncss: [\"${HOME}/a.css\"]\n").unwrap(),
            "doc: ../t/house.docx\ncss: [\"/home/u/a.css\"]\n"
        );
        assert_eq!(
            e("title: \"${}^{14}C$ and $\\{x\\}$\"\n").unwrap(),
            "title: \"${}^{14}C$ and $\\{x\\}$\"\n"
        );
        assert!(e("doc: ${vualt}/x\n").unwrap_err().contains("${vualt}"));
        assert!(e("doc: ${PATH}\n").is_err());
        // Names are case-sensitive, like the environment's.
        assert!(e("doc: ${vault}\n").is_err());
        assert!(expand("x: ${HOME}\n", ".", None).is_err());
    }

    #[test]
    fn a_vault_file_in_front_matter_is_staged_relative_to_the_note() {
        let fm = "---\nreference-doc: ${VAULT}/templates/house.docx\n\
                  css: [\"${VAULT}/style/a.css\", ${VAULT}/gone.css]\nbin: /usr/bin/env\n---\n${VAULT}\n";
        let (_d, vault, index) = setup(
            "export-vault",
            &[
                ("templates/house.docx", ""),
                ("style/a.css", ""),
                ("n/note.md", fm),
                ("top.md", "---\nx: ${VAULT}/style/a.css\n---\n"),
            ],
        );
        let (out, files) = prepare(fm, "n/note", &vault, &index, None).unwrap();
        assert_eq!(
            out,
            "---\nreference-doc: ../templates/house.docx\n\
             css: [\"../style/a.css\", ../gone.css]\nbin: /usr/bin/env\n---\n${VAULT}\n"
        );
        assert_eq!(files, ["style/a.css", "templates/house.docx"]);
        let top = "---\nx: ${VAULT}/style/a.css\n---\n";
        let (out, files) = prepare(top, "top", &vault, &index, None).unwrap();
        assert_eq!(out, "---\nx: ./style/a.css\n---\n");
        assert_eq!(files, ["style/a.css"]);
    }

    #[test]
    fn vault_defaults_lose_their_engines_in_a_plain_export() {
        assert_eq!(
            without_engines("author: A\njupyter:\n  kernel: p\nengine: knitr\nlang: de\n"),
            "author: A\nlang: de\n"
        );
        assert_eq!(without_engines("engine: knitr\n"), "\n");
    }

    #[test]
    fn vault_defaults_are_staged_with_their_files() {
        let (_d, vault, index) = setup(
            "export-defaults",
            &[
                ("templates/house.docx", ""),
                ("n/note.md", "# N\n"),
                (
                    ".katib/quarto.yml",
                    "format:\n  docx:\n    reference-doc: ${VAULT}/templates/house.docx\n",
                ),
            ],
        );
        let staged = stage(&vault, &index, "n/note").unwrap();
        assert_eq!(
            staged.defaults.as_deref(),
            Some("format:\n  docx:\n    reference-doc: ../templates/house.docx\n")
        );
        assert!(staged.dir.join("templates/house.docx").is_file());
        fs::write(vault.root().join(".katib/quarto.yml"), "x: ${NOPE}\n").unwrap();
        let e = stage(&vault, &index, "n/note").unwrap_err().to_string();
        assert!(e.starts_with(".katib/quarto.yml: ${NOPE}"), "{e}");
    }

    #[test]
    fn the_log_shows_the_error_without_colours_or_stack() {
        let log = "\x1b[31mprocessing\x1b[39m\n\x1b[91mERROR: bad cell\x1b[39m\n\n\
                   Stack trace:\n    at a (x.js:1)\n";
        assert_eq!(log_error(log), "ERROR: bad cell");
        assert_eq!(log_error("a\nb\n"), "a\nb");
    }

    #[cfg(unix)]
    #[test]
    fn a_run_stops_on_cancel_or_timeout_and_reports_failure() {
        let dir = TempDir::new("export-run");
        let log = dir.0.join("log");
        let sh = |script: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            c
        };
        let no = AtomicBool::new(false);
        assert!(run(sh("exit 0"), &log, TIMEOUT, &no).is_ok());
        match run(sh("echo 'ERROR: no' >&2; exit 1"), &log, TIMEOUT, &no) {
            Err(ExportError::Failed(text)) => assert_eq!(text, "ERROR: no"),
            other => panic!("{other:?}"),
        }
        let short = Duration::from_millis(100);
        assert!(matches!(
            run(sh("sleep 5"), &log, short, &no),
            Err(ExportError::TimedOut(_))
        ));
        let yes = AtomicBool::new(true);
        assert!(matches!(
            run(sh("sleep 5"), &log, TIMEOUT, &yes),
            Err(ExportError::Cancelled)
        ));
    }

    /// Runs only where Quarto is installed.
    #[test]
    fn quarto_renders_a_staged_note_and_runs_code_only_when_asked() {
        if find_quarto().is_none() {
            return;
        }
        let (_d, vault, index) = setup(
            "export-quarto",
            &[
                ("n/data.txt", "from the vault\n"),
                (
                    "n/note.md",
                    "# Note\n\nSee [[other]] and @knuth.\n\n![dot](../img/a.svg)\n\n```{r}\ncat(readLines(\"data.txt\"))\n```\n",
                ),
                (
                    "refs.bib",
                    "@book{knuth, author={Knuth, Donald}, title={T}, year={1968}}\n",
                ),
                (".katib/quarto.yml", "bibliography: ${VAULT}/refs.bib\n"),
                (
                    "img/a.svg",
                    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"/>",
                ),
            ],
        );
        let no = AtomicBool::new(false);
        let staged = stage(&vault, &index, "n/note").unwrap();
        let out = render(&staged, Format::Gfm, false, &no).unwrap();
        assert_eq!(out, vault.root().join(".katib/out/n/note.md"));
        let text = fs::read_to_string(&out).unwrap();
        assert!(text.contains("See other and Knuth (1968)."), "{text}");
        assert!(!text.contains("from the vault"), "{text}");
        let pdf = render(&staged, Format::Pdf, false, &no).unwrap();
        assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF"));
        // Running needs R with knitr.
        if Command::new("Rscript")
            .args(["-e", "library(knitr)"])
            .output()
            .is_ok_and(|o| o.status.success())
        {
            let out = render(&staged, Format::Gfm, true, &no).unwrap();
            let text = fs::read_to_string(&out).unwrap();
            assert!(text.contains("from the vault"), "{text}");
        }
    }
}
