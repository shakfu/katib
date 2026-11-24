//! Git sync for a vault (design.md section 5), through the `git` binary.
//!
//! One [`Repo::sync`] call runs a whole cycle: commit, fetch, merge, push. A
//! merge is computed with `git merge-tree --write-tree`, which touches neither
//! the working tree nor the index; the working tree only moves by a
//! fast-forward, which git refuses if it would overwrite uncommitted changes.
//! So a conflict never leaves markers on disk.

use std::{
    ffi::OsStr,
    fmt, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub enum SyncError {
    /// `git` could not be run at all.
    Spawn(io::Error),
    /// A git command failed; its arguments and stderr.
    Git(String, String),
    /// A resolution did not cover every conflicted path.
    Unresolved(Vec<String>),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "cannot run git: {e}"),
            Self::Git(args, err) => write!(f, "git {args}: {}", err.trim()),
            Self::Unresolved(paths) => write!(f, "unresolved: {}", paths.join(", ")),
        }
    }
}

impl std::error::Error for SyncError {}

type Result<T> = std::result::Result<T, SyncError>;

/// How a sync cycle ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Up to date with the remote, or nothing to do.
    Synced,
    /// No upstream branch is configured; commits stay local.
    NoRemote,
    /// The remote could not be reached; commits stay local.
    Offline(String),
    /// The remote moved during the cycle; run again.
    Retry,
    /// The merge has conflicts. Nothing was changed or pushed.
    Conflicted(Conflicts),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub committed: bool,
    pub outcome: Outcome,
    /// Local commits the remote does not have.
    pub ahead: usize,
}

/// A merge that stopped on conflicts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflicts {
    /// The remote commit being merged.
    pub theirs: String,
    pub paths: Vec<String>,
}

/// Three versions of a conflicted file; `None` where it does not exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versions {
    pub base: Option<String>,
    pub mine: Option<String>,
    pub theirs: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Mine,
    Theirs,
    /// Mine at the path; theirs beside it as `<name> (conflict <host>).<ext>`.
    Both,
}

pub struct Repo {
    root: PathBuf,
    /// Extra environment for every git command (tests isolate the config).
    env: Vec<(String, String)>,
}

impl Repo {
    /// The repository whose work tree is `root`, or `None` if `root` is not
    /// one (sync is off). Requires git 2.38 for `merge-tree --write-tree`.
    pub fn open(root: &Path) -> Result<Option<Self>> {
        Self::open_with_env(root, Vec::new())
    }

    fn open_with_env(root: &Path, env: Vec<(String, String)>) -> Result<Option<Self>> {
        let repo = Self {
            root: root.to_owned(),
            env,
        };
        let top = match repo.git(["rev-parse", "--show-toplevel"]) {
            Ok(top) => PathBuf::from(top.trim()),
            Err(SyncError::Git(..)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let same = |a: &Path, b: &Path| a.canonicalize().ok() == b.canonicalize().ok();
        if !same(&top, root) {
            return Ok(None);
        }
        repo.exclude_temp_files()?;
        Ok(Some(repo))
    }

    /// Keep katib's in-flight temporary files out of commits, without touching
    /// a file the user commits: `.git/info/exclude` is local.
    fn exclude_temp_files(&self) -> Result<()> {
        let path = PathBuf::from(
            self.git(["rev-parse", "--git-path", "info/exclude"])?
                .trim(),
        );
        let path = if path.is_absolute() {
            path
        } else {
            self.root.join(path)
        };
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if !current.lines().any(|l| l.trim() == "*.katib-tmp") {
            let sep = if current.is_empty() || current.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let io = |e| SyncError::Git("info/exclude".into(), format!("{e}"));
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
            std::fs::write(&path, format!("{current}{sep}*.katib-tmp\n")).map_err(io)?;
        }
        Ok(())
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.root)
            .args(args)
            // Never wait on a prompt: there is no terminal to answer it.
            .env("GIT_TERMINAL_PROMPT", "0")
            // Stable English messages, which `push` parses.
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    fn run<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command(args).output().map_err(SyncError::Spawn)
    }

    /// Run git; its stdout on success.
    fn git<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.run(&args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(failed(&args, &out))
        }
    }

    /// Run git with `input` on stdin; its stdout on success.
    fn git_stdin(&self, args: &[&str], input: &[u8]) -> Result<String> {
        use std::io::Write as _;
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(SyncError::Spawn)?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(input)
            .map_err(SyncError::Spawn)?;
        let out = child.wait_with_output().map_err(SyncError::Spawn)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let args: Vec<_> = args
                .iter()
                .map(|a| (*a).into())
                .collect::<Vec<std::ffi::OsString>>();
            Err(failed(&args, &out))
        }
    }

    fn rev(&self, name: &str) -> Result<Option<String>> {
        let out = self.run(["rev-parse", "--verify", "--quiet", name])?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
    }

    /// Whether the work tree differs from HEAD, untracked files included.
    pub fn has_changes(&self) -> Result<bool> {
        Ok(!self
            .git(["status", "--porcelain", "--untracked-files=all"])?
            .trim()
            .is_empty())
    }

    /// Commit every change in the work tree. Returns whether there was one.
    pub fn commit_all(&self, host: &str) -> Result<bool> {
        if !self.has_changes()? {
            return Ok(false);
        }
        self.git(["add", "-A"])?;
        let message = format!("katib: {host} {}", iso8601(SystemTime::now()));
        self.git(["commit", "--quiet", "--no-verify", "-m", &message])?;
        Ok(true)
    }

    /// The upstream's remote and branch ref, from the current branch's config.
    fn upstream(&self) -> Result<Option<(String, String)>> {
        let Ok(branch) = self.git(["symbolic-ref", "--quiet", "--short", "HEAD"]) else {
            return Ok(None);
        };
        let branch = branch.trim();
        let get = |key: &str| {
            self.git(["config", "--get", &format!("branch.{branch}.{key}")])
                .ok()
                .map(|v| v.trim().to_owned())
        };
        Ok(get("remote").zip(get("merge")))
    }

    /// Commits on HEAD that `@{u}` lacks, and the reverse.
    fn ahead_behind(&self) -> Result<(usize, usize)> {
        let out = self.git(["rev-list", "--left-right", "--count", "HEAD...@{u}"])?;
        let mut n = out.split_whitespace().map(|n| n.parse().unwrap_or(0));
        Ok((n.next().unwrap_or(0), n.next().unwrap_or(0)))
    }

    /// One cycle: commit (if `commit`), fetch, merge, push.
    pub fn sync(&self, commit: bool, host: &str) -> Result<Report> {
        let committed = commit && self.commit_all(host)?;
        let report = |outcome, ahead| Report {
            committed,
            outcome,
            ahead,
        };
        let Some((remote, merge)) = self.upstream()? else {
            return Ok(report(Outcome::NoRemote, 0));
        };
        if self.rev("HEAD")?.is_none() {
            // An empty repository: nothing to merge or push yet.
            return Ok(report(Outcome::Synced, 0));
        }
        if let Err(e) = self.git(["fetch", "--quiet", &remote]) {
            let ahead = self.ahead_behind().map_or(0, |(a, _)| a);
            return Ok(report(Outcome::Offline(e.to_string()), ahead));
        }
        let (ahead, behind) = self.ahead_behind()?;
        if behind > 0 {
            if ahead > 0 {
                let theirs = self.rev("@{u}")?.expect("upstream exists after fetch");
                match self.merge_tree(&theirs)? {
                    (tree, paths) if paths.is_empty() => self.apply(&tree, &theirs)?,
                    (_, paths) => {
                        return Ok(report(
                            Outcome::Conflicted(Conflicts { theirs, paths }),
                            ahead,
                        ));
                    }
                }
            } else {
                self.git(["merge", "--ff-only", "--quiet", "@{u}"])?;
            }
        }
        let (ahead, _) = self.ahead_behind()?;
        if ahead == 0 {
            return Ok(report(Outcome::Synced, 0));
        }
        let out = self.run(["push", "--porcelain", &remote, &format!("HEAD:{merge}")])?;
        if out.status.success() {
            return Ok(report(Outcome::Synced, 0));
        }
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let outcome = if text.contains("[rejected]") || text.contains("non-fast-forward") {
            Outcome::Retry
        } else {
            Outcome::Offline(text.trim().to_owned())
        };
        Ok(report(outcome, ahead))
    }

    /// Merge HEAD with `theirs` in memory: the result tree and the conflicted paths.
    fn merge_tree(&self, theirs: &str) -> Result<(String, Vec<String>)> {
        let args = [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            "HEAD",
            theirs,
        ];
        let out = self.run(args)?;
        // Exit 0: clean; 1: conflicts; anything else: an error.
        if !matches!(out.status.code(), Some(0 | 1)) {
            return Err(failed(&args.map(Into::into), &out));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut fields = stdout.split('\0');
        let tree = fields.next().unwrap_or_default().trim().to_owned();
        let paths = fields
            .take_while(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        Ok((tree, paths))
    }

    /// Commit `tree` as the merge of HEAD and `theirs`, then move the work tree
    /// to it by fast-forward.
    fn apply(&self, tree: &str, theirs: &str) -> Result<()> {
        let message = "katib: merge";
        let commit = self
            .git([
                "commit-tree",
                tree,
                "-p",
                "HEAD",
                "-p",
                theirs,
                "-m",
                message,
            ])?
            .trim()
            .to_owned();
        self.git(["merge", "--ff-only", "--quiet", &commit])?;
        Ok(())
    }

    /// The three versions of a conflicted `path`.
    pub fn versions(&self, theirs: &str, path: &str) -> Result<Versions> {
        let base = self
            .git(["merge-base", "HEAD", theirs])
            .ok()
            .map(|b| b.trim().to_owned());
        let show = |rev: &str| {
            self.run(["show", &format!("{rev}:{path}")])
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        Ok(Versions {
            base: base.as_deref().and_then(show),
            mine: show("HEAD"),
            theirs: show(theirs),
        })
    }

    /// Finish a conflicted merge with one choice per conflicted path, then
    /// fast-forward to it. The merge is recomputed, so HEAD may have moved.
    pub fn resolve(&self, theirs: &str, choices: &[(String, Choice)], host: &str) -> Result<()> {
        let (tree, paths) = self.merge_tree(theirs)?;
        let missing: Vec<_> = paths
            .iter()
            .filter(|p| !choices.iter().any(|(c, _)| c == *p))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(SyncError::Unresolved(missing));
        }
        // Edit the merged tree in a scratch index, never the real one.
        let index = PathBuf::from(
            self.git(["rev-parse", "--git-path", "katib-merge-index"])?
                .trim(),
        );
        let index = if index.is_absolute() {
            index
        } else {
            self.root.join(index)
        };
        let scratch = Repo {
            root: self.root.clone(),
            env: self
                .env
                .iter()
                .cloned()
                .chain([(
                    "GIT_INDEX_FILE".into(),
                    index.to_string_lossy().into_owned(),
                )])
                .collect(),
        };
        scratch.git(["read-tree", &tree])?;
        for path in &paths {
            let choice = &choices.iter().find(|(p, _)| p == path).expect("checked").1;
            let v = self.versions(theirs, path)?;
            let (keep, beside) = match choice {
                Choice::Mine => (v.mine, None),
                Choice::Theirs => (v.theirs, None),
                // Deleted here: there is only one version to keep.
                Choice::Both if v.mine.is_none() => (v.theirs, None),
                Choice::Both => (v.mine, v.theirs.map(|t| (conflict_name(path, host), t))),
            };
            scratch.put(path, keep.as_deref())?;
            if let Some((name, text)) = beside {
                scratch.put(&name, Some(&text))?;
            }
        }
        let resolved = scratch.git(["write-tree"])?.trim().to_owned();
        let _ = std::fs::remove_file(&index);
        self.apply(&resolved, theirs)
    }

    /// Set `path` in the index to `text`, or remove it.
    fn put(&self, path: &str, text: Option<&str>) -> Result<()> {
        match text {
            Some(text) => {
                let blob = self.git_stdin(&["hash-object", "-w", "--stdin"], text.as_bytes())?;
                let info = format!("100644,{},{path}", blob.trim());
                self.git(["update-index", "--add", "--cacheinfo", &info])?;
            }
            None => {
                self.git(["update-index", "--force-remove", "--", path])?;
            }
        }
        Ok(())
    }
}

fn failed(args: &[std::ffi::OsString], out: &Output) -> SyncError {
    let args = args
        .iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    SyncError::Git(args, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// `dir/name (conflict host).md` for `dir/name.md`.
pub fn conflict_name(path: &str, host: &str) -> String {
    let (dir, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (file, String::new()),
    };
    let name = format!("{stem} (conflict {host}){ext}");
    if dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    }
}

/// UTC time as `2026-11-24T12:00:00Z`.
pub fn iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// This machine's name, for commit messages and conflict copies.
pub fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|h| !h.is_empty())
        .map(|h| h.split('.').next().unwrap_or(&h).to_owned())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests;
