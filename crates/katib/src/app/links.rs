//! The broken links dialog: links that reach no note or file, or several
//! notes, with a repair for each where one can be guessed.

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{Broken, NoteRow, Problem, retarget};

use super::{Katib, notify_error};

/// How many close matches a missing link offers.
const NEAR: usize = 3;

pub(super) struct BrokenRow {
    broken: Broken,
    /// Notes the link may have meant.
    near: Vec<NoteRow>,
}

impl Katib {
    pub(super) fn open_broken(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_broken();
        if self.broken.is_empty() {
            window.push_notification(Notification::info("No broken links"), cx);
            return;
        }
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog
                .title("Broken links")
                .w(px(760.))
                .content(move |content, _, cx| {
                    content.child(this.update(cx, |k, cx| k.broken_list(cx)))
                })
        });
    }

    fn load_broken(&mut self) {
        let index = self.index.borrow();
        let title = |id: &str| {
            self.notes
                .iter()
                .find(|n| n.id == id)
                .map_or_else(|| id.to_owned(), |n| n.title.clone())
        };
        self.broken = match index.broken(self.vault.root()) {
            Ok(list) => list
                .into_iter()
                .map(|broken| {
                    let near = match &broken.problem {
                        Problem::Missing(_) => {
                            index.similar(&broken.link.target, NEAR).unwrap_or_default()
                        }
                        Problem::Ambiguous(ids) => ids
                            .iter()
                            .map(|id| NoteRow {
                                id: id.clone(),
                                title: title(id),
                            })
                            .collect(),
                        Problem::NoFile(_) | Problem::Outside => Vec::new(),
                    };
                    BrokenRow { broken, near }
                })
                .collect(),
            Err(e) => {
                eprintln!("katib: {e}");
                Vec::new()
            }
        };
    }

    fn broken_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let rows = self.broken.iter().enumerate().map(|(ix, row)| {
            let b = &row.broken;
            let problem = match &b.problem {
                Problem::Missing(_) => format!("No note \"{}\"", b.link.target),
                Problem::Ambiguous(_) => format!("\"{}\" matches several notes", b.link.target),
                Problem::NoFile(path) => format!("No file {path}"),
                Problem::Outside => format!("{} points outside the vault", b.link.target),
            };
            let repairs = row.near.iter().enumerate().map(|(n, note)| {
                let to = note.id.clone();
                Button::new(SharedString::from(format!("repair-{ix}-{n}")))
                    .small()
                    .primary()
                    .label(format!("Link to {}", note.title))
                    .tooltip(note.id.clone())
                    .on_click(cx.listener(move |this, _, w, cx| this.repair(ix, &to, w, cx)))
            });
            let create = match &b.problem {
                Problem::Missing(id) => Some({
                    let id = id.clone();
                    Button::new(("create", ix))
                        .small()
                        .label(format!("Create {id}"))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            w.close_dialog(cx);
                            this.create(&id, false, w, cx);
                        }))
                }),
                _ => None,
            };
            let (src, at) = (b.src.clone(), b.link.range.start);
            v_flex()
                .gap_1()
                .pb_3()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().text_sm().child(b.title.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(problem),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(b.context.clone()),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(repairs)
                        .children(create)
                        .child(
                            Button::new(("show", ix))
                                .small()
                                .ghost()
                                .label("Show")
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    w.close_dialog(cx);
                                    this.open(&src, w, cx);
                                    if let Some(pane) = this.pane() {
                                        pane.update(cx, |p, cx| p.go_to(at, w, cx));
                                    }
                                })),
                        ),
                )
        });
        let count = self.broken.len();
        v_flex()
            .id("broken")
            .max_h(px(560.))
            .overflow_y_scroll()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(match count {
                        0 => "No broken links left.".to_owned(),
                        1 => "1 link reaches no note or file, or several notes.".to_owned(),
                        n => format!("{n} links reach no note or file, or several notes."),
                    }),
            )
            .children(rows)
            .when(count == 0, |d| {
                d.child(
                    h_flex().justify_end().child(
                        Button::new("done")
                            .primary()
                            .label("Done")
                            .on_click(|_, w, cx| w.close_dialog(cx)),
                    ),
                )
            })
            .into_any_element()
    }

    /// Point broken link `ix` at note `to`, then list again.
    fn repair(&mut self, ix: usize, to: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.broken.get(ix) else {
            return;
        };
        let (src, link) = (row.broken.src.clone(), row.broken.link.clone());
        if !self.ready_to_change_files(window, cx) {
            return;
        }
        let result = retarget(&self.vault, &self.index.borrow(), &src, &link, to);
        if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
            eprintln!("katib: {e}");
        }
        match result {
            Ok(()) => {
                self.sync.note_change();
                if let Some(tab) = self.tab_of(&src, cx) {
                    let pane = self.tabs[tab].pane.clone();
                    pane.update(cx, |p, cx| p.disk_changed(window, cx));
                }
            }
            Err(e) => notify_error(window, cx, e.to_string()),
        }
        self.load_broken();
        self.refresh(cx);
    }
}
