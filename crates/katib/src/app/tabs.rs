//! Open notes as tabs. Opening a note shows its tab if it has one, else
//! replaces the active tab's note; "new tab" opens beside it. Each tab keeps
//! the notes it showed, for back and forward.

use gpui_kit::*;

use super::{Katib, notify_error};
use crate::pane::{NotePane, PaneEvent};

/// How many places back and forward a tab remembers.
const HISTORY: usize = 100;

/// A note and a caret offset in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Place {
    pub(super) id: String,
    pub(super) offset: usize,
}

pub(super) struct Tab {
    pub(super) pane: Entity<NotePane>,
    pub(super) back: Vec<Place>,
    pub(super) forward: Vec<Place>,
    _subscription: Subscription,
}

impl Tab {
    fn place(&self, cx: &App) -> Place {
        let pane = self.pane.read(cx);
        Place {
            id: pane.entry.id.clone(),
            offset: pane.head(cx),
        }
    }
}

/// Record `place` as the newest entry of `stack`, dropping the oldest past the limit.
fn push(stack: &mut Vec<Place>, place: Place) {
    if stack.last() != Some(&place) {
        stack.push(place);
    }
    if stack.len() > HISTORY {
        stack.remove(0);
    }
}

impl Katib {
    /// The active tab's pane.
    /// `None` while the overview shows.
    pub(super) fn pane(&self) -> Option<&Entity<NotePane>> {
        if self.overview.open {
            return None;
        }
        self.tabs.get(self.active).map(|t| &t.pane)
    }

    /// The active tab's note id.
    pub(super) fn open_id(&self, cx: &App) -> Option<String> {
        self.pane().map(|p| p.read(cx).entry.id.clone())
    }

    /// Whether every tab's file holds its buffer.
    pub(super) fn all_saved(&self, cx: &App) -> bool {
        self.tabs.iter().all(|t| t.pane.read(cx).is_saved())
    }

    pub(super) fn save_all(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            tab.pane.update(cx, |p, cx| p.save(cx));
        }
    }

    pub(super) fn save_open(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane() {
            pane.update(cx, |p, cx| p.save(cx));
        }
    }

    pub fn open(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_in(id, false, window, cx);
    }

    /// Show note `id`: its tab if it has one, else in a new tab (`new_tab`) or
    /// in place of the active tab's note.
    pub(super) fn open_in(
        &mut self,
        id: &str,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.tab_of(id, cx) {
            return self.activate(ix, window, cx);
        }
        let Some(tab) = self.new_tab(id, window, cx) else {
            return;
        };
        // From the overview, a note opens beside the tabs rather than over one.
        let new_tab = new_tab || std::mem::take(&mut self.overview.open);
        if new_tab || self.tabs.is_empty() {
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        } else {
            self.save_open(cx);
            let old = std::mem::replace(&mut self.tabs[self.active], tab);
            let here = old.place(cx);
            let new = &mut self.tabs[self.active];
            new.back = old.back;
            push(&mut new.back, here);
        }
        // Focus the window, not the editor: the note reads formatted, with no
        // caret, until clicked, and the window's shortcuts keep working.
        window.focus(&self.focus, cx);
        self.refresh(cx);
    }

    /// Load tab `ix` again from note `id`, after the note moved or was renamed.
    pub(super) fn reopen(
        &mut self,
        ix: usize,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(mut tab) = self.new_tab(id, window, cx) {
            let old = &mut self.tabs[ix];
            tab.back = std::mem::take(&mut old.back);
            tab.forward = std::mem::take(&mut old.forward);
            self.tabs[ix] = tab;
        }
    }

    /// Show the previous place in the active tab (`back`), or the next one.
    pub(super) fn go(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.overview.open {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let here = tab.place(cx);
        let tab = &mut self.tabs[self.active];
        let Some(to) = (if back {
            tab.back.pop()
        } else {
            tab.forward.pop()
        }) else {
            return;
        };
        self.save_open(cx);
        // A note deleted since is dropped from the history; new_tab says why.
        let Some(mut new) = self.new_tab(&to.id, window, cx) else {
            return;
        };
        let old = &mut self.tabs[self.active];
        new.back = std::mem::take(&mut old.back);
        new.forward = std::mem::take(&mut old.forward);
        push(
            if back {
                &mut new.forward
            } else {
                &mut new.back
            },
            here,
        );
        new.pane
            .update(cx, |p, cx| p.place_caret(to.offset, window, cx));
        self.tabs[self.active] = new;
        window.focus(&self.focus, cx);
        self.refresh(cx);
    }

    /// Whether the active tab can go back, and forward.
    pub(super) fn can_go(&self) -> (bool, bool) {
        if self.overview.open {
            return (false, false);
        }
        self.tabs.get(self.active).map_or((false, false), |t| {
            (!t.back.is_empty(), !t.forward.is_empty())
        })
    }

    /// Follow moved notes in every tab's history.
    pub(super) fn rename_in_history(&mut self, new_id: impl Fn(&str) -> Option<String>) {
        for tab in &mut self.tabs {
            for place in tab.back.iter_mut().chain(tab.forward.iter_mut()) {
                if let Some(id) = new_id(&place.id) {
                    place.id = id;
                }
            }
        }
    }

    pub(super) fn tab_of(&self, id: &str, cx: &App) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.pane.read(cx).entry.id == id)
    }

    fn new_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<Tab> {
        let entry = match self.vault.entry(id) {
            Ok(entry) => entry,
            Err(e) => {
                notify_error(window, cx, e.to_string());
                return None;
            }
        };
        let text = match std::fs::read_to_string(&entry.path) {
            Ok(text) => text,
            Err(e) => {
                notify_error(window, cx, format!("{id}: {e}"));
                return None;
            }
        };
        let (vault, index, mode) = (self.vault.clone(), self.index.clone(), self.mode);
        let pane = cx.new(|cx| NotePane::new(vault, index, entry, text, mode, window, cx));
        let subscription =
            cx.subscribe_in(&pane, window, |this, pane, event, window, cx| match event {
                PaneEvent::Saved => {
                    this.sync.note_change();
                    this.refresh(cx);
                }
                PaneEvent::OpenLink(link) => this.follow(link.clone(), window, cx),
                PaneEvent::Error(message) => notify_error(window, cx, message.clone()),
                PaneEvent::Close => {
                    if let Some(ix) = this.tabs.iter().position(|t| t.pane == *pane) {
                        this.remove_tab(ix, window, cx);
                    }
                }
            });
        Some(Tab {
            pane,
            back: Vec::new(),
            forward: Vec::new(),
            _subscription: subscription,
        })
    }

    pub(super) fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let changed = ix != self.active || self.overview.open;
        if ix < self.tabs.len() && changed {
            self.overview.open = false;
            self.active = ix;
            window.focus(&self.focus, cx);
            self.refresh(cx);
        }
    }

    /// Save tab `ix`, then close it.
    pub(super) fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        // A failed save keeps a draft (pane.rs), so closing loses no text.
        tab.pane.update(cx, |p, cx| p.save(cx));
        self.remove_tab(ix, window, cx);
    }

    /// Close tab `ix` without saving.
    pub(super) fn remove_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        self.active = active_after_close(self.active, ix, self.tabs.len());
        window.focus(&self.focus, cx);
        self.refresh(cx);
    }

    /// Show the tab `delta` places away, wrapping around.
    pub(super) fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len() as isize;
        if n > 1 {
            let ix = (self.active as isize + delta).rem_euclid(n) as usize;
            self.activate(ix, window, cx);
        }
    }

    /// A tab's label: the note's title.
    pub(super) fn tab_title(&self, tab: &Tab, cx: &App) -> String {
        self.tab_title_of(&tab.pane, cx)
    }

    pub(super) fn tab_title_of(&self, pane: &Entity<NotePane>, cx: &App) -> String {
        let id = &pane.read(cx).entry.id;
        self.notes.iter().find(|n| n.id == *id).map_or_else(
            || id.rsplit('/').next().unwrap_or(id).to_owned(),
            |n| n.title.clone(),
        )
    }
}

/// The active tab after closing tab `closed`, with `left` tabs remaining: the
/// same tab if another closed, else the one that took its place, else the last.
fn active_after_close(active: usize, closed: usize, left: usize) -> usize {
    let active = if closed < active { active - 1 } else { active };
    active.min(left.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::{HISTORY, Place, active_after_close, push};

    #[test]
    fn history_skips_repeats_and_is_bounded() {
        let at = |id: &str| Place {
            id: id.into(),
            offset: 0,
        };
        let mut stack = Vec::new();
        push(&mut stack, at("a"));
        push(&mut stack, at("a"));
        assert_eq!(stack.len(), 1);
        for i in 0..HISTORY + 5 {
            push(&mut stack, at(&i.to_string()));
        }
        assert_eq!(stack.len(), HISTORY);
        assert_eq!(stack[0], at("5"));
    }

    #[test]
    fn closing_a_tab_keeps_a_sensible_one_active() {
        // Tabs a b c, b active.
        assert_eq!(active_after_close(1, 0, 2), 0); // close a: b stays
        assert_eq!(active_after_close(1, 1, 2), 1); // close b: c takes its place
        assert_eq!(active_after_close(1, 2, 2), 1); // close c: b stays
        assert_eq!(active_after_close(2, 2, 2), 1); // close the last, active: b
        assert_eq!(active_after_close(0, 0, 0), 0); // close the only tab
    }
}
