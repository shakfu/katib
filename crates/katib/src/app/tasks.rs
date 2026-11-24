//! The tasks dialog: checklist items from every note and task notes, open
//! ones first, by due date.

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex, v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{ManageError, TaskRow, tasks};

use super::{Katib, notify_error};

/// Due within this many days reads as soon.
const SOON_DAYS: i64 = 7;

impl Katib {
    pub(super) fn open_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_tasks();
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog
                .title("Tasks")
                .w(px(820.))
                .content(move |content, _, cx| {
                    content.child(this.update(cx, |k, cx| k.task_list(cx)))
                })
        });
    }

    pub(super) fn load_tasks(&mut self) {
        self.tasks = self.index.borrow().tasks().unwrap_or_else(|e| {
            eprintln!("katib: {e}");
            Vec::new()
        });
    }

    pub(super) fn task_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let today = jiff::ZonedDateTime::now().date();
        let soon = today
            .checked_add(jiff::Span::new().days(SOON_DAYS))
            .unwrap_or(today)
            .to_string();
        let today = today.to_string();
        let show_done = self.tasks_show_done;
        let shown: Vec<(usize, &TaskRow)> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| show_done || !t.is_done())
            .collect();
        let open = self.tasks.iter().filter(|t| !t.is_done()).count();
        let rows = shown.into_iter().map(|(ix, t)| {
            let done = t.is_done();
            let control = match t.marker {
                Some(_) => Checkbox::new(("task-check", ix))
                    .checked(done)
                    .on_click(cx.listener(move |this, checked: &bool, w, cx| {
                        this.change_task(ix, TaskChange::Tick(*checked), w, cx)
                    }))
                    .into_any_element(),
                None => {
                    let status = |label: &'static str, value: &'static str| {
                        Button::new(SharedString::from(format!("status-{ix}-{value}")))
                            .xsmall()
                            .label(label)
                            .when(t.status == value, |b| b.primary())
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.change_task(ix, TaskChange::Status(value), w, cx)
                            }))
                    };
                    h_flex()
                        .gap_1()
                        .child(status("Open", "open"))
                        .child(status("Doing", "doing"))
                        .child(status("Done", "done"))
                        .into_any_element()
                }
            };
            let due = t.due.clone().map(|due| {
                let color = if done {
                    theme.muted_foreground
                } else if due < today {
                    theme.danger
                } else if due <= soon {
                    theme.warning
                } else {
                    theme.muted_foreground
                };
                div()
                    .text_xs()
                    .text_color(color)
                    .child(format!("due {due}"))
            });
            let priority = t.priority.clone().map(|p| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("priority {p}"))
            });
            let (src, marker) = (t.src.clone(), t.marker);
            h_flex()
                .gap_3()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(control)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .truncate()
                                .when(done, |d| {
                                    d.line_through().text_color(theme.muted_foreground)
                                })
                                .child(t.text.clone()),
                        )
                        .child(
                            h_flex()
                                .gap_3()
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    if marker.is_some() {
                                        t.title.clone()
                                    } else {
                                        "task note".into()
                                    },
                                ))
                                .children(due)
                                .children(priority),
                        ),
                )
                .when(marker.is_some(), |row| {
                    row.child(
                        Button::new(("promote", ix))
                            .xsmall()
                            .ghost()
                            .label("Promote")
                            .tooltip("Make this item a task note, linked from here")
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.change_task(ix, TaskChange::Promote, w, cx)
                            })),
                    )
                })
                .child(
                    Button::new(("show-task", ix))
                        .xsmall()
                        .ghost()
                        .label("Show")
                        .on_click(cx.listener(move |this, _, w, cx| {
                            w.close_dialog(cx);
                            this.open(&src, w, cx);
                            if let (Some(at), Some(pane)) = (marker, this.pane()) {
                                pane.update(cx, |p, cx| p.go_to(at, w, cx));
                            }
                        })),
                )
        });
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(match open {
                                0 => "Nothing open.".to_owned(),
                                1 => "1 open task.".to_owned(),
                                n => format!("{n} open tasks."),
                            }),
                    )
                    .child(
                        Button::new("show-done")
                            .xsmall()
                            .label(if show_done { "Hide done" } else { "Show done" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tasks_show_done = !this.tasks_show_done;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("tasks")
                    .max_h(px(540.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    /// Apply `change` to listed task `ix`, then list again.
    fn change_task(
        &mut self,
        ix: usize,
        change: TaskChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.tasks.get(ix).cloned() else {
            return;
        };
        if !self.ready_to_change_files(window, cx) {
            return;
        }
        let result: Result<Option<String>, ManageError> = match (change, task.marker) {
            (TaskChange::Tick(done), Some(marker)) => {
                tasks::set_item(&self.vault, &task.src, marker, done).map(|()| None)
            }
            (TaskChange::Status(status), None) => {
                tasks::set_status(&self.vault, &task.src, status).map(|()| None)
            }
            (TaskChange::Promote, Some(marker)) => {
                // Beside the note it came from.
                let dir = task.src.rsplit_once('/').map_or("", |(d, _)| d);
                tasks::promote(&self.vault, &self.index.borrow(), &task.src, marker, dir).map(Some)
            }
            _ => return,
        };
        if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
            eprintln!("katib: {e}");
        }
        match result {
            Ok(created) => {
                self.sync.note_change();
                if let Some(tab) = self.tab_of(&task.src, cx) {
                    let pane = self.tabs[tab].pane.clone();
                    pane.update(cx, |p, cx| p.disk_changed(window, cx));
                }
                if let Some(id) = created {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::success(format!(
                            "Created task note {id}"
                        )),
                        cx,
                    );
                }
            }
            Err(e) => notify_error(window, cx, e.to_string()),
        }
        self.load_tasks();
        self.refresh(cx);
    }
}

#[derive(Clone, Copy)]
enum TaskChange {
    Tick(bool),
    Status(&'static str),
    Promote,
}
