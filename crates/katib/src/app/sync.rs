//! Driving git sync from the window (design.md section 5): when to run a
//! cycle, what the status bar says, and the conflict dialog.

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_sync::{Choice, Conflicts, Outcome, Repo, Report, Versions};

use super::{Katib, notify_error};

/// Commit once the vault has been quiet this long.
const QUIET: Duration = Duration::from_secs(30);
/// Fetch at least this often.
const FETCH_EVERY: Duration = Duration::from_secs(60);
/// The longest wait between attempts while offline.
const MAX_BACKOFF: Duration = Duration::from_secs(600);
/// A conflict preview shows at most this many bytes of each side.
const PREVIEW: usize = 4000;

pub(super) struct SyncState {
    repo: Option<Arc<Repo>>,
    /// Why sync is off, when `repo` is `None`.
    off: Option<String>,
    host: String,
    busy: bool,
    /// The vault changed since the last cycle began.
    changed: bool,
    last_change: Instant,
    next_fetch: Instant,
    backoff: Duration,
    report: Option<Report>,
    error: Option<String>,
    choices: HashMap<String, Choice>,
    /// Loaded when the conflict dialog opens.
    versions: Vec<(String, Versions)>,
}

impl SyncState {
    pub(super) fn new(root: &Path) -> Self {
        let (repo, off) = if !opted_in(root) {
            (
                None,
                Some("set sync = true in .katib/config.toml".to_owned()),
            )
        } else {
            match Repo::open(root) {
                Ok(Some(repo)) => (Some(Arc::new(repo)), None),
                Ok(None) => (None, Some("not a git repository".to_owned())),
                Err(e) => (None, Some(e.to_string())),
            }
        };
        Self {
            repo,
            off,
            host: katib_sync::hostname(),
            busy: false,
            changed: false,
            last_change: Instant::now(),
            // The first cycle runs as soon as the window is up.
            next_fetch: Instant::now(),
            backoff: Duration::ZERO,
            report: None,
            error: None,
            choices: HashMap::new(),
            versions: Vec::new(),
        }
    }

    pub(super) fn note_change(&mut self) {
        self.changed = true;
        self.last_change = Instant::now();
    }

    pub(super) fn fetch_soon(&mut self) {
        self.next_fetch = Instant::now();
    }

    fn conflicts(&self) -> Option<&Conflicts> {
        match &self.report {
            Some(Report {
                outcome: Outcome::Conflicted(c),
                ..
            }) => Some(c),
            _ => None,
        }
    }

    /// Commit what is saved, without the network; for closing the window.
    pub(super) fn commit_now(&self) {
        if let Some(repo) = &self.repo
            && self.changed
            && let Err(e) = repo.commit_all(&self.host)
        {
            eprintln!("katib: {e}");
        }
    }

    /// The status bar's sync text.
    pub(super) fn status(&self) -> String {
        if let Some(off) = &self.off {
            return format!("sync off: {off}");
        }
        if self.busy {
            return "syncing".into();
        }
        if let Some(e) = &self.error {
            return format!("sync error: {e}");
        }
        match &self.report {
            None => "not synced yet".into(),
            Some(r) => match &r.outcome {
                Outcome::Synced => "synced".into(),
                Outcome::NoRemote => "no remote; commits stay here".into(),
                Outcome::Offline(_) => format!("offline, {} unpushed", r.ahead),
                Outcome::Retry => "remote moved; syncing again".into(),
                Outcome::Conflicted(c) => format!("{} conflicts", c.paths.len()),
            },
        }
    }
}

/// Sync commits and pushes everything in the vault, so it never starts on
/// its own: a vault opts in with a top-level `sync = true` line in
/// `.katib/config.toml`. Opening some other repository must not commit it.
fn opted_in(root: &Path) -> bool {
    let Ok(config) = std::fs::read_to_string(root.join(".katib/config.toml")) else {
        return false;
    };
    config
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        // Keys after the first `[table]` header belong to that table.
        .take_while(|l| !l.starts_with('['))
        .any(|l| {
            l.split_once('=')
                .is_some_and(|(k, v)| k.trim() == "sync" && v.trim() == "true")
        })
}

impl Katib {
    /// Called twice a second: run a cycle if one is due and every buffer is saved.
    pub(super) fn sync_tick(&mut self, cx: &mut Context<Self>) {
        let s = &self.sync;
        if s.repo.is_none() || s.busy || s.conflicts().is_some() {
            return;
        }
        // An unsaved buffer delays the sync; it is never committed half-way.
        if self.pane.as_ref().is_some_and(|p| !p.read(cx).is_saved()) {
            return;
        }
        let commit_due = s.changed && s.last_change.elapsed() >= QUIET;
        if commit_due || Instant::now() >= s.next_fetch {
            self.run_sync(cx);
        }
    }

    fn run_sync(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.sync.repo.clone() else {
            return;
        };
        let host = self.sync.host.clone();
        self.sync.busy = true;
        self.sync.changed = false;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { repo.sync(true, &host) })
                .await;
            let _ = this.update(cx, |this, cx| this.sync_done(result, cx));
        })
        .detach();
    }

    fn sync_done(&mut self, result: Result<Report, katib_sync::SyncError>, cx: &mut Context<Self>) {
        let s = &mut self.sync;
        s.busy = false;
        let now = Instant::now();
        match result {
            Ok(report) => {
                s.error = None;
                s.next_fetch = match &report.outcome {
                    Outcome::Offline(_) => {
                        s.backoff = (s.backoff * 2).clamp(FETCH_EVERY, MAX_BACKOFF);
                        now + s.backoff
                    }
                    Outcome::Retry => now,
                    _ => {
                        s.backoff = Duration::ZERO;
                        now + FETCH_EVERY
                    }
                };
                if matches!(report.outcome, Outcome::Conflicted(_)) {
                    s.choices.clear();
                }
                s.report = Some(report);
            }
            Err(e) => {
                s.error = Some(e.to_string());
                s.next_fetch = now + FETCH_EVERY;
            }
        }
        cx.notify();
    }

    /// The status bar's sync part: text, plus a button while conflicts wait.
    pub(super) fn sync_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let text = div().child(self.sync.status());
        match self.sync.conflicts() {
            Some(_) => h_flex()
                .gap_2()
                .child(text)
                .child(
                    Button::new("resolve")
                        .xsmall()
                        .primary()
                        .label("Resolve")
                        .on_click(cx.listener(|this, _, w, cx| this.open_conflicts(w, cx))),
                )
                .into_any_element(),
            None => text.into_any_element(),
        }
    }

    fn open_conflicts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(repo), Some(c)) = (self.sync.repo.clone(), self.sync.conflicts().cloned()) else {
            return;
        };
        let mut versions = Vec::new();
        for path in &c.paths {
            match repo.versions(&c.theirs, path) {
                Ok(v) => versions.push((path.clone(), v)),
                Err(e) => return notify_error(window, cx, e.to_string()),
            }
        }
        self.sync.versions = versions;
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog
                .w(px(900.))
                .title("Sync conflicts")
                .content(move |content, _, cx| {
                    let Some(view) = this.upgrade() else {
                        return content;
                    };
                    content.child(view.update(cx, |k, cx| k.conflict_list(cx)))
                })
        });
    }

    /// One row per conflicted file: both versions and the three choices, then Apply.
    fn conflict_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let side = |label: &str, text: &Option<String>| {
            let body = match text {
                Some(t) if t.len() > PREVIEW => {
                    let mut cut = PREVIEW;
                    while !t.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}\n...", &t[..cut])
                }
                Some(t) => t.clone(),
                None => "(deleted)".to_owned(),
            };
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("side-{label}")))
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .whitespace_normal()
                        .child(body),
                )
        };
        let rows = self
            .sync
            .versions
            .iter()
            .enumerate()
            .map(|(ix, (path, v))| {
                let chosen = self.sync.choices.get(path);
                let button = |label: &'static str, choice: Choice| {
                    let path = path.clone();
                    let selected = chosen == Some(&choice);
                    Button::new(SharedString::from(format!("{label}-{ix}")))
                        .small()
                        .label(label)
                        .when(selected, |b| b.primary())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.sync.choices.insert(path.clone(), choice.clone());
                            cx.notify();
                        }))
                };
                v_flex()
                    .gap_2()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().text_sm().child(path.clone()))
                    .child(
                        h_flex()
                            .gap_3()
                            .items_start()
                            .child(side("This machine", &v.mine))
                            .child(side("Other machine", &v.theirs)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(button("Keep mine", Choice::Mine))
                            .child(button("Keep theirs", Choice::Theirs))
                            .child(button("Keep both", Choice::Both)),
                    )
            });
        let ready = self.sync.versions.len() == self.sync.choices.len();
        v_flex()
            .id("conflicts")
            .max_h(px(560.))
            .overflow_y_scroll()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Sync is paused until each file is resolved. \"Keep both\" saves the other machine's version beside yours."),
            )
            .children(rows)
            .child(
                h_flex().justify_end().child(
                    Button::new("apply")
                        .primary()
                        .label("Apply")
                        .disabled(!ready)
                        .on_click(cx.listener(|this, _, w, cx| this.apply_resolution(w, cx))),
                ),
            )
            .into_any_element()
    }

    fn apply_resolution(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(repo), Some(c)) = (self.sync.repo.clone(), self.sync.conflicts().cloned()) else {
            return;
        };
        if self.pane.as_ref().is_some_and(|p| !p.read(cx).is_saved()) {
            return notify_error(window, cx, "Save the open note first".into());
        }
        let choices: Vec<_> = self
            .sync
            .choices
            .iter()
            .map(|(p, c)| (p.clone(), c.clone()))
            .collect();
        let host = self.sync.host.clone();
        window.close_dialog(cx);
        self.sync.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    // Commit saved edits first; the merge is computed against HEAD.
                    repo.commit_all(&host)?;
                    repo.resolve(&c.theirs, &choices, &host)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sync.busy = false;
                match result {
                    Ok(()) => {
                        this.sync.report = None;
                        this.sync.choices.clear();
                        this.sync.fetch_soon();
                    }
                    Err(e) => notify_error(window, cx, e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::opted_in;

    #[test]
    fn sync_needs_an_explicit_top_level_opt_in() {
        let dir = std::env::temp_dir().join(format!("katib-optin-{}", std::process::id()));
        let config = dir.join(".katib/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let check = |text: &str| {
            std::fs::write(&config, text).unwrap();
            opted_in(&dir)
        };
        assert!(check("sync = true\n"));
        assert!(check(
            "# vault\nsync=true # on\n[objects]\nbackend = \"git\"\n"
        ));
        assert!(!check("sync = false\n"));
        assert!(!check("# sync = true\n"));
        assert!(!check("[objects]\nsync = true\n"));
        std::fs::remove_file(&config).unwrap();
        assert!(!opted_in(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
