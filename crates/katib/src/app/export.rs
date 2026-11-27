//! Exporting the open note with Quarto (`katib_core::export`). "Export"
//! never runs the note's code; "Run and export" does, and says so first.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
    v_flex,
};
use gpui_kit::*;
use katib_core::{
    Index,
    export::{self, ExportError, Format},
    github,
};

use super::{Katib, notify_error};

/// The export in progress.
pub(super) struct Exporting {
    label: String,
    cancel: Arc<AtomicBool>,
}

impl Katib {
    /// Ask for a format, then export the open note; `run` executes its code.
    pub(super) fn prompt_export(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_id(cx).is_none() {
            return;
        }
        let this = cx.weak_entity();
        let (title, hint) = if run {
            (
                "Run and export note",
                "Runs the note's code cells as you, in its folder, with your files and network.",
            )
        } else {
            (
                "Export note",
                "Code cells are shown, not run. The file goes to .katib/out/ and opens.",
            )
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let buttons = Format::ALL.into_iter().enumerate().map(|(ix, format)| {
                let this = this.clone();
                Button::new(("export-format", ix))
                    .label(format.label())
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let _ = this.update(cx, |this, cx| this.export(format, run, window, cx));
                    })
            });
            dialog.title(title).w(px(480.)).child(
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(hint),
                    )
                    .child(h_flex().gap_2().flex_wrap().children(buttons)),
            )
        });
    }

    fn export(&mut self, format: Format, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.exporting.is_some() {
            return notify_error(window, cx, "An export is already running".into());
        }
        let Some(id) = self.open_id(cx) else {
            return;
        };
        // Export what the editor shows, with links resolved as they are now.
        self.save_open(cx);
        if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
            return notify_error(window, cx, e.to_string());
        }
        let staged = match export::stage(&self.vault, &self.index.borrow(), &id) {
            Ok(staged) => staged,
            Err(e) => return notify_error(window, cx, e.to_string()),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let verb = if run { "Running" } else { "Exporting" };
        self.exporting = Some(Exporting {
            label: format!("{verb} {} as {}", id, format.label()),
            cancel: cancel.clone(),
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { export::render(&staged, format, run, &cancel) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.exporting = None;
                cx.notify();
                match result {
                    Ok(path) => {
                        cx.open_with_system(&path);
                        let shown = path.strip_prefix(this.vault.root()).unwrap_or(&path);
                        window.push_notification(
                            Notification::info(format!("Exported to {}", shown.display())),
                            cx,
                        );
                    }
                    Err(ExportError::Cancelled) => {}
                    Err(e) => notify_error(window, cx, e.to_string()),
                }
            });
        })
        .detach();
    }

    /// Ask for a folder, then write the vault there as GitHub renders it
    /// (`katib_core::github`). The folder rules are checked before writing.
    pub(super) fn export_github(
        &mut self,
        style: github::Style,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_open(cx);
        if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
            return notify_error(window, cx, e.to_string());
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Export".into()),
        });
        let vault = self.vault.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = picked.await else {
                return;
            };
            let Some(dest) = paths.pop() else {
                return;
            };
            let shown = dest.display().to_string();
            // The window's index is not Send; this one reads the same file.
            let result = cx
                .background_spawn(async move {
                    let index = Index::open(&vault)?;
                    github::export(&vault, &index, &dest, style)
                })
                .await;
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(report) => {
                    let mut message = format!(
                        "Exported {} notes and {} files to {shown}",
                        report.notes, report.files
                    );
                    if report.unlinked.is_empty() {
                        return window.push_notification(Notification::info(message), cx);
                    }
                    message += &format!(
                        ". {} links match no single note and are left as text:",
                        report.unlinked.len()
                    );
                    for u in report.unlinked.iter().take(5) {
                        message += &format!("\n{}: {}", u.id, u.link);
                    }
                    window.push_notification(Notification::warning(message), cx);
                }
                Err(e) => notify_error(window, cx, e.to_string()),
            });
        })
        .detach();
    }

    /// The status bar's export state and its "Cancel" button.
    pub(super) fn export_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let exporting = self.exporting.as_ref()?;
        let cancel = exporting.cancel.clone();
        Some(
            h_flex()
                .gap_1()
                .child(
                    Button::new("export-state")
                        .ghost()
                        .xsmall()
                        .loading(true)
                        .label(exporting.label.clone()),
                )
                .child(
                    Button::new("export-cancel")
                        .xsmall()
                        .label("Cancel")
                        .on_click(
                            cx.listener(move |_, _, _, _| cancel.store(true, Ordering::Relaxed)),
                        ),
                )
                .into_any_element(),
        )
    }
}
