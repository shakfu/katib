//! The overview: recent changes, tasks, and the shape of the vault's links.
//! It is the first tab, shown when no note tab is.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{NoteRow, Recent, Stats, TagCount};
use katib_sync::LastCommit;

use super::Katib;

/// Notes the latest list shows.
const RECENT: usize = 200;
/// Commits searched for each file's last author.
const COMMITS: usize = 500;
/// Rows a stats list shows before "and N more".
const LIST: usize = 50;

#[derive(Default)]
pub(super) struct Overview {
    pub(super) open: bool,
    section: usize,
    recent: Vec<Recent>,
    commits: HashMap<String, LastCommit>,
    stats: Option<Stats>,
    tags: Vec<TagCount>,
}

impl Katib {
    /// Whether the overview, not a note, fills the editor area.
    pub(super) fn overview_shown(&self) -> bool {
        self.overview.open || self.tabs.is_empty()
    }

    pub(super) fn show_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_open(cx);
        self.overview.open = true;
        window.focus(&self.focus, cx);
        self.load_overview(cx);
        self.refresh(cx);
    }

    /// Read the overview's lists from the index; git authors in the background.
    pub(super) fn load_overview(&mut self, cx: &mut Context<Self>) {
        {
            let index = self.index.borrow();
            self.overview.recent = index.recent(RECENT).unwrap_or_default();
            self.overview.stats = index.stats(self.vault.root(), LIST).ok();
            self.overview.tags = index.tags().unwrap_or_default();
        }
        self.load_tasks();
        let root = self.vault.root().to_owned();
        cx.spawn(async move |this, cx| {
            let commits = cx
                .background_spawn(async move { katib_sync::last_commits(&root, COMMITS) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.overview.commits = commits;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn overview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let body = match self.overview.section {
            0 => self.latest(cx),
            1 => self.task_list(cx),
            _ => self.stats(cx),
        };
        let this = cx.weak_entity();
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        TabBar::new("overview-sections")
                            .segmented()
                            .small()
                            .selected_index(self.overview.section)
                            .on_click(move |ix, _, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.overview.section = *ix;
                                    cx.notify();
                                });
                            })
                            .child(Tab::new().label("Latest"))
                            .child(Tab::new().label("Tasks"))
                            .child(Tab::new().label("Stats")),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("overview-refresh")
                            .ghost()
                            .small()
                            .icon(IconName::RefreshCw)
                            .tooltip("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.load_overview(cx))),
                    ),
            )
            .child(
                div()
                    .id("overview-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }

    fn latest(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let rows = self.overview.recent.iter().enumerate().map(|(ix, n)| {
            // A file newer than its last commit has changes git has not recorded.
            let who = match self.overview.commits.get(&format!("{}.md", n.id)) {
                Some(c) if c.time + 1 >= n.modified => format!("{} {}", c.author, ago(now, c.time)),
                Some(c) => format!("changed since {}'s commit", c.author),
                None if self.overview.commits.is_empty() => String::new(),
                None => "not committed".into(),
            };
            let id = n.id.clone();
            h_flex()
                .id(("recent", ix))
                .gap_3()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(|d| d.bg(theme.muted))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(n.title.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(n.id.clone()),
                        ),
                )
                .child(
                    v_flex()
                        .items_end()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(ago(now, n.modified))
                        .child(who),
                )
                .on_click(cx.listener(move |this, _, w, cx| this.open(&id, w, cx)))
        });
        v_flex().gap_0p5().children(rows).into_any_element()
    }

    fn stats(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(stats) = self.overview.stats.clone() else {
            return div()
                .child("The index could not be read.")
                .into_any_element();
        };
        let list =
            |name: &'static str, rows: Vec<(NoteRow, Option<String>)>, cx: &mut Context<Self>| {
                let theme = cx.theme();
                let more = rows.len().saturating_sub(LIST);
                let items: Vec<_> = rows
                    .into_iter()
                    .take(LIST)
                    .enumerate()
                    .map(|(ix, (n, extra))| {
                        let id = n.id.clone();
                        h_flex()
                            .id(SharedString::from(format!("{name}-{ix}")))
                            .gap_2()
                            .px_2()
                            .py_0p5()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.muted))
                            .child(div().flex_1().min_w_0().text_sm().truncate().child(n.title))
                            .children(extra.map(|e| {
                                div().text_xs().text_color(theme.muted_foreground).child(e)
                            }))
                            .on_click(cx.listener(move |this, _, w, cx| this.open(&id, w, cx)))
                    })
                    .collect();
                v_flex()
                    .flex_1()
                    .min_w(px(200.))
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(name),
                    )
                    .when(items.is_empty(), |d| {
                        d.child(
                            div()
                                .px_2()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("None"),
                        )
                    })
                    .children(items)
                    .when(more > 0, |d| {
                        d.child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("and {more} more")),
                        )
                    })
            };
        let most = stats
            .most_linked
            .iter()
            .map(|(n, c)| (n.clone(), Some(c.to_string())))
            .collect();
        let orphans = stats.orphans.iter().map(|n| (n.clone(), None)).collect();
        let dead = stats.dead_ends.iter().map(|n| (n.clone(), None)).collect();
        let tags = self
            .overview
            .tags
            .iter()
            .enumerate()
            .map(|(ix, t)| {
                let query = format!("#{}", t.tag);
                Button::new(("overview-tag", ix))
                    .xsmall()
                    .label(format!("#{} {}", t.tag, t.count))
                    .on_click(
                        cx.listener(move |this, _, w, cx| this.open_palette_at(&query, w, cx)),
                    )
            })
            .collect::<Vec<_>>();
        let summary = h_flex()
            .gap_4()
            .text_sm()
            .child(format!("{} notes", stats.notes))
            .child(format!("{} orphans", stats.orphans.len()))
            .child(format!("{} dead ends", stats.dead_ends.len()))
            .child(
                Button::new("overview-broken")
                    .small()
                    .when(stats.broken > 0, |b| b.danger())
                    .label(match stats.broken {
                        0 => "No broken links".to_owned(),
                        1 => "1 broken link".to_owned(),
                        n => format!("{n} broken links"),
                    })
                    .on_click(cx.listener(|this, _, w, cx| this.open_broken(w, cx))),
            );
        v_flex()
            .gap_4()
            .child(summary)
            .child(
                h_flex()
                    .items_start()
                    .gap_4()
                    .flex_wrap()
                    .child(list("Most linked", most, cx))
                    .child(list("Orphans: nothing links here", orphans, cx))
                    .child(list("Dead ends: they link nowhere", dead, cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(muted).child("Tags"))
                    .child(h_flex().gap_1().flex_wrap().children(tags)),
            )
            .into_any_element()
    }
}

/// `then` relative to `now`, both in seconds: "just now", "5 min ago", "3 h
/// ago", "2 d ago", else the date.
fn ago(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        86_400..2_592_000 => format!("{} d ago", d / 86_400),
        _ => katib_sync::iso8601(UNIX_EPOCH + std::time::Duration::from_secs(then.max(0) as u64))
            [..10]
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::ago;

    #[test]
    fn times_read_relative_then_as_dates() {
        let now = 1_795_000_000;
        assert_eq!(ago(now, now - 5), "just now");
        assert_eq!(ago(now, now - 300), "5 min ago");
        assert_eq!(ago(now, now - 7200), "2 h ago");
        assert_eq!(ago(now, now - 3 * 86_400), "3 d ago");
        assert_eq!(ago(now, now - 40 * 86_400), "2026-10-09");
        assert_eq!(ago(now, now + 100), "just now");
    }
}
