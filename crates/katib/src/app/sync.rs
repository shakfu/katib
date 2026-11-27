//! Driving git sync from the window (design.md section 5): when to run a
//! cycle, what the status bar says, and the conflict dialog.

use std::{collections::HashMap, sync::Arc, time::Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{
    Vault,
    config::{Backend, Config},
};
use katib_sync::{Choice, Conflicts, Outcome, Repo, Report, Versions, schedule::Schedule};

use super::{Katib, notify_error};

/// A conflict preview shows at most this many bytes of each side.
const PREVIEW: usize = 4000;

pub(super) struct SyncState {
    repo: Option<Arc<Repo>>,
    /// Why sync is off, when `repo` is `None`.
    off: Option<String>,
    host: String,
    busy: bool,
    schedule: Schedule,
    report: Option<Report>,
    error: Option<String>,
    choices: HashMap<String, Choice>,
    /// Loaded when the conflict dialog opens.
    versions: Vec<(String, Versions)>,
}

impl SyncState {
    pub(super) fn new(vault: &Vault, config: &Config) -> Self {
        let (repo, off) = if let Some(problem) = &config.problem {
            (None, Some(format!(".katib/config.toml: {problem}")))
        } else if !config.sync {
            // Sync commits and pushes everything in the vault, so it is opt-in:
            // opening some other repository must not commit it.
            (
                None,
                Some("set sync = true in .katib/config.toml".to_owned()),
            )
        } else {
            match Repo::open(vault.root()) {
                Ok(Some(mut repo)) => match config.backend {
                    // Without git-lfs, attachments would be committed as plain files.
                    Backend::Lfs => match katib_core::attach::ensure_lfs_attributes(vault) {
                        Err(e) => (None, Some(format!(".gitattributes: {e}"))),
                        Ok(()) => match repo.enable_lfs() {
                            Ok(()) => (Some(Arc::new(repo)), None),
                            Err(e) => (None, Some(format!("git-lfs: {e}"))),
                        },
                    },
                    _ => (Some(Arc::new(repo)), None),
                },
                Ok(None) => (None, Some("not a git repository".to_owned())),
                Err(e) => (None, Some(e.to_string())),
            }
        };
        Self {
            repo,
            off,
            host: katib_sync::hostname(),
            busy: false,
            // The first cycle runs as soon as the window is up.
            schedule: Schedule::new(Instant::now()),
            report: None,
            error: None,
            choices: HashMap::new(),
            versions: Vec::new(),
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.busy
    }

    pub(super) fn note_change(&mut self) {
        self.schedule.note_change(Instant::now());
    }

    pub(super) fn fetch_soon(&mut self) {
        self.schedule.fetch_soon(Instant::now());
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

    /// Commit what is saved, without the network; for closing the window. Waits for an
    /// agent's move to finish.
    pub(super) fn commit_now(&self, vault: &Vault) {
        if let Some(repo) = &self.repo
            && self.schedule.changed()
        {
            let _exclusive = vault.exclusive();
            if let Err(e) = repo.commit_all(&self.host) {
                eprintln!("katib: {e}");
            }
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
            Some(Report {
                lfs_error: Some(_), ..
            }) => "synced; some attachments not downloaded".into(),
            Some(r) => match &r.outcome {
                Outcome::Synced => "synced".into(),
                Outcome::NoRemote => "no remote; commits stay here".into(),
                Outcome::Offline(_) => format!("offline, {} unpushed", r.ahead),
                Outcome::Retry if self.schedule.retries_exhausted() => {
                    "push keeps being rejected; retrying later".into()
                }
                Outcome::Retry => "remote moved; syncing again".into(),
                Outcome::Conflicted(c) => format!("{} conflicts", c.paths.len()),
            },
        }
    }
}

impl Katib {
    /// Called twice a second: run a cycle if one is due and every buffer is saved.
    pub(super) fn sync_tick(&mut self, cx: &mut Context<Self>) {
        let s = &self.sync;
        if s.repo.is_none() || s.busy || s.conflicts().is_some() {
            return;
        }
        // An unsaved buffer delays the sync; it is never committed half-way.
        if !self.all_saved(cx) {
            return;
        }
        if s.schedule.due(Instant::now()) {
            self.run_sync(cx);
        }
    }

    fn run_sync(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.sync.repo.clone() else {
            return;
        };
        let host = self.sync.host.clone();
        let vault = self.vault.clone();
        self.sync.busy = true;
        self.sync.schedule.started();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    // An agent's move lands whole in this cycle or the next.
                    let _exclusive = vault.exclusive();
                    repo.sync(true, &host)
                })
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
                s.schedule.finished(Some(&report.outcome), now);
                if matches!(report.outcome, Outcome::Conflicted(_)) {
                    s.choices.clear();
                }
                s.report = Some(report);
            }
            Err(e) => {
                s.error = Some(e.to_string());
                s.schedule.finished(None, now);
            }
        }
        cx.notify();
    }

    /// Run a cycle now, as the status bar button and the Sync menu ask.
    pub(super) fn sync_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(off) = &self.sync.off {
            return notify_error(window, cx, format!("Sync is off: {off}"));
        }
        if self.sync.busy {
            return;
        }
        if self.sync.conflicts().is_some() {
            return self.open_conflicts(window, cx);
        }
        self.save_all(cx);
        if !self.all_saved(cx) {
            return notify_error(window, cx, "An open note has unsaved changes".into());
        }
        self.run_sync(cx);
    }

    /// The status bar's sync part: the state, which runs a cycle when clicked,
    /// plus a button while conflicts wait.
    pub(super) fn sync_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let icon = match (&self.sync.off, &self.sync.error, self.sync.conflicts()) {
            (Some(_), ..) => IconName::CircleX,
            (_, Some(_), _) | (_, _, Some(_)) => IconName::TriangleAlert,
            _ => IconName::RefreshCw,
        };
        let state = Button::new("sync-state")
            .ghost()
            .xsmall()
            .icon(icon)
            .label(self.sync.status())
            .loading(self.sync.busy)
            .tooltip(match &self.sync.off {
                Some(_) => "Sync is off; see Settings",
                None => "Sync now",
            })
            .on_click(cx.listener(|this, _, w, cx| this.sync_now(w, cx)));
        h_flex()
            .gap_1()
            .child(state)
            .when(self.sync.conflicts().is_some(), |row| {
                row.child(
                    Button::new("resolve")
                        .xsmall()
                        .primary()
                        .label("Resolve")
                        .on_click(cx.listener(|this, _, w, cx| this.open_conflicts(w, cx))),
                )
            })
            .into_any_element()
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
        self.save_all(cx);
        if !self.all_saved(cx) {
            return notify_error(window, cx, "Save the open notes first".into());
        }
        let choices: Vec<_> = self
            .sync
            .choices
            .iter()
            .map(|(p, c)| (p.clone(), c.clone()))
            .collect();
        let host = self.sync.host.clone();
        let vault = self.vault.clone();
        window.close_dialog(cx);
        self.sync.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let _exclusive = vault.exclusive();
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
