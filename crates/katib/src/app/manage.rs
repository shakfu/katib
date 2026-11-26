//! Creating, renaming, moving and deleting notes from the window.

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{
    ManageError, Moved, attach, move_folder, move_note, preview_move_folder, preview_move_note,
    trash_note,
};

use super::{Katib, notify_error};

/// The name in the dialog's input, as a move or create takes it.
fn typed_name(input: &Entity<InputState>, cx: &App) -> String {
    input
        .read(cx)
        .value()
        .trim()
        .trim_end_matches('/')
        .to_owned()
}

/// Notes listed by name in a move preview; the rest are counted.
const PREVIEW_NAMES: usize = 5;

/// A move preview for the dialog, and whether it is an error.
fn preview_text(result: Result<Moved, ManageError>) -> (String, bool) {
    let moved = match result {
        Ok(moved) => moved,
        Err(e) => return (e.to_string(), true),
    };
    if moved.notes.is_empty() && moved.rewritten.is_empty() {
        return (String::new(), false);
    }
    let notes = |n: usize| {
        if n == 1 {
            "1 note".to_owned()
        } else {
            format!("{n} notes")
        }
    };
    let mut text = if moved.notes.len() > 1 {
        format!("Moves {}. ", notes(moved.notes.len()))
    } else {
        String::new()
    };
    match moved.rewritten.as_slice() {
        [] => text.push_str("No links to update."),
        ids => {
            let mut names = ids[..ids.len().min(PREVIEW_NAMES)].join(", ");
            if ids.len() > PREVIEW_NAMES {
                names.push_str(&format!(" and {} more", ids.len() - PREVIEW_NAMES));
            }
            text.push_str(&format!("Updates links in {}: {names}.", notes(ids.len())));
        }
    }
    (text, false)
}

/// What a sidebar menu item does.
type MenuAction = dyn Fn(&mut Katib, &mut Window, &mut Context<Katib>);

/// What the name dialog does with what was typed.
#[derive(Clone)]
pub(super) enum Naming {
    Create,
    /// A folder, made with its page: git keeps no empty folder, and the tree lists notes.
    CreateFolder,
    Move(String),
    MoveFolder(String),
    /// Edit note `id`'s tags.
    Tags(String),
}

pub(super) struct Prompt {
    input: Entity<InputState>,
    naming: Naming,
    /// For a move: what it would change, and whether that is an error.
    preview: (String, bool),
    _subscription: Subscription,
}

impl Katib {
    /// Ask for a note name, then create or move.
    pub(super) fn prompt_name(
        &mut self,
        naming: Naming,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, initial, ok) = match &naming {
            Naming::Create => ("New note", String::new(), "Create"),
            Naming::CreateFolder => ("New folder", String::new(), "Create"),
            Naming::Move(id) => ("Rename or move note", id.clone(), "Rename"),
            Naming::MoveFolder(dir) => ("Rename or move folder", dir.clone(), "Rename"),
            Naming::Tags(id) => {
                let tags = self.index.borrow().note_tags(id).unwrap_or_default();
                ("Tags", tags.join(", "), "Save")
            }
        };
        self.prompt_with(naming, title, initial, ok, window, cx);
    }

    /// Ask for a note name in folder `dir`.
    pub(super) fn prompt_new_in(&mut self, dir: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_with(
            Naming::Create,
            "New note",
            format!("{dir}/"),
            "Create",
            window,
            cx,
        );
    }

    /// Ask for a folder name in folder `dir`.
    fn prompt_folder_in(&mut self, dir: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_with(
            Naming::CreateFolder,
            "New folder",
            format!("{dir}/"),
            "Create",
            window,
            cx,
        );
    }

    /// Create folder `path` with its page, `path/README.md`, and open the page.
    fn create_folder(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        match new_folder_page(&self.vault, path) {
            Ok(page) => self.create(&page, false, window, cx),
            Err(e) => notify_error(window, cx, e),
        }
    }

    fn prompt_with(
        &mut self,
        naming: Naming,
        title: &'static str,
        initial: String,
        ok: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hint = match naming {
            Naming::Tags(_) => "Separated by commas. Empty removes every tag.",
            Naming::CreateFolder => {
                "A path in the vault. The folder is created with its page, README.md, which \
                 keeps it in the tree and in sync."
            }
            _ => "A path in the vault, without .md. Folders are created as needed.",
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit_name(window, cx),
                InputEvent::Change => this.preview_move(cx),
                _ => {}
            },
        );
        self.prompt = Some(Prompt {
            input: input.clone(),
            naming,
            preview: (String::new(), false),
            _subscription: subscription,
        });
        self.preview_move(cx);
        let this = cx.weak_entity();
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, this, focused) = (input.clone(), this.clone(), focused.clone());
            let submit = this.clone();
            dialog
                .title(title)
                .w(px(480.))
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = input.clone();
                        window.defer(cx, move |window, cx| {
                            input.update(cx, |s, cx| s.focus(window, cx));
                        });
                    }
                    let (preview, error) = this
                        .upgrade()
                        .and_then(|this| this.read(cx).prompt.as_ref().map(|p| p.preview.clone()))
                        .unwrap_or_default();
                    let theme = cx.theme();
                    content.child(
                        v_flex()
                            .gap_2()
                            .child(Input::new(&input))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(hint),
                            )
                            .when(!preview.is_empty(), |col| {
                                col.child(
                                    div()
                                        .text_sm()
                                        .when(error, |d| d.text_color(theme.danger))
                                        .child(preview),
                                )
                            }),
                    )
                })
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(Button::new("ok").primary().label(ok).on_click(
                            move |_, window, cx| {
                                let _ = submit.update(cx, |this, cx| this.submit_name(window, cx));
                            },
                        )),
                )
        });
    }

    /// Show what the move typed so far would change.
    fn preview_move(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let name = typed_name(&prompt.input, cx);
        let (vault, index) = (&self.vault, &self.index.borrow());
        let preview = match &prompt.naming {
            _ if name.is_empty() => (String::new(), false),
            Naming::Move(from) | Naming::MoveFolder(from) if *from == name => {
                (String::new(), false)
            }
            Naming::Move(from) => preview_text(preview_move_note(vault, index, from, &name)),
            Naming::MoveFolder(from) => {
                preview_text(preview_move_folder(vault, index, from, &name))
            }
            Naming::Create | Naming::CreateFolder | Naming::Tags(_) => return,
        };
        if let Some(prompt) = &mut self.prompt {
            prompt.preview = preview;
        }
        cx.notify();
    }

    fn submit_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let name = typed_name(&prompt.input, cx);
        window.close_dialog(cx);
        if let Naming::Tags(id) = &prompt.naming {
            return self.save_tags(id, &name, window, cx);
        }
        if name.is_empty() {
            return;
        }
        match prompt.naming {
            Naming::Create => self.create(&name, false, window, cx),
            Naming::CreateFolder => self.create_folder(&name, window, cx),
            Naming::Move(from) => self.move_to(&from, &name, false, window, cx),
            Naming::MoveFolder(from) => self.move_to(&from, &name, true, window, cx),
            Naming::Tags(_) => {}
        }
    }

    /// Write `list` (comma separated) as note `id`'s tags.
    fn save_tags(&mut self, id: &str, list: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut tags: Vec<String> = Vec::new();
        for tag in list
            .split(',')
            .map(|t| t.trim().trim_start_matches('#').trim())
        {
            if !tag.is_empty() && !tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                tags.push(tag.to_owned());
            }
        }
        if !self.ready_to_change_files(window, cx) {
            return;
        }
        if let Err(e) = katib_core::tasks::set_tags(&self.vault, id, &tags) {
            return notify_error(window, cx, e.to_string());
        }
        if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
            eprintln!("katib: {e}");
        }
        self.sync.note_change();
        if let Some(tab) = self.tab_of(id, cx) {
            let pane = self.tabs[tab].pane.clone();
            pane.update(cx, |p, cx| p.disk_changed(window, cx));
        }
        self.refresh(cx);
    }

    /// Whether files may change now: the open note is saved and no sync is writing.
    pub(super) fn ready_to_change_files(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.save_all(cx);
        if !self.all_saved(cx) {
            notify_error(window, cx, "An open note has unsaved changes".into());
            return false;
        }
        if self.sync.is_busy() {
            notify_error(window, cx, "Sync is running; try again in a moment".into());
            return false;
        }
        true
    }

    /// Move note (or, with `folder`, folder) `from` to `to`.
    fn move_to(
        &mut self,
        from: &str,
        to: &str,
        folder: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ready_to_change_files(window, cx) {
            return;
        }
        let result = if folder {
            move_folder(&self.vault, &self.index.borrow(), from, to)
        } else {
            move_note(&self.vault, &self.index.borrow(), from, to)
        };
        match result {
            Ok(moved) => {
                if folder {
                    super::move_expanded(&mut self.expanded, from, to);
                }
                self.rename_in_history(|id| moved.new_id(id).map(str::to_owned));
                if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
                    eprintln!("katib: {e}");
                }
                self.sync.note_change();
                for ix in 0..self.tabs.len() {
                    let id = self.tabs[ix].pane.read(cx).entry.id.clone();
                    if let Some(new_id) = moved.new_id(&id) {
                        let new_id = new_id.to_owned();
                        self.reopen(ix, &new_id, window, cx);
                    } else if moved.rewritten.contains(&id) {
                        let pane = self.tabs[ix].pane.clone();
                        pane.update(cx, |p, cx| p.disk_changed(window, cx));
                    }
                }
                self.refresh(cx);
                let links = match moved.rewritten.len() {
                    0 => String::new(),
                    1 => "; updated links in 1 note".into(),
                    n => format!("; updated links in {n} notes"),
                };
                window
                    .push_notification(Notification::success(format!("Moved to {to}{links}")), cx);
            }
            Err(e) => {
                // Some files may have been rewritten before the failure; show them.
                let _ = self.index.borrow_mut().sync(&self.vault);
                self.refresh(cx);
                notify_error(window, cx, e.to_string());
            }
        }
    }

    /// Confirm, then move note `id` to the vault's trash.
    pub(super) fn confirm_delete(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let backlinks = self.index.borrow().backlinks(&id).map_or(0, |b| b.len());
        let warning = match backlinks {
            0 => String::new(),
            1 => " 1 note links to it; that link will break.".into(),
            n => format!(" {n} notes link to it; those links will break."),
        };
        let message = format!("Move \"{id}\" to .katib/trash?{warning}");
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (this, id) = (this.clone(), id.clone());
            alert
                .title("Delete note")
                .description(message.clone())
                .show_cancel(true)
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.delete(&id, window, cx));
                    true
                })
        });
    }

    fn delete(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready_to_change_files(window, cx) {
            return;
        }
        match trash_note(&self.vault, id) {
            Ok(dest) => {
                if let Some(ix) = self.tab_of(id, cx) {
                    self.remove_tab(ix, window, cx);
                }
                if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
                    eprintln!("katib: {e}");
                }
                self.sync.note_change();
                self.refresh(cx);
                let shown = dest
                    .strip_prefix(self.vault.root())
                    .unwrap_or(&dest)
                    .display()
                    .to_string();
                window.push_notification(Notification::success(format!("Moved to {shown}")), cx);
            }
            Err(e) => notify_error(window, cx, e.to_string()),
        }
    }

    /// Pick files to attach to the open note.
    pub(super) fn pick_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.pane().cloned() else {
            return;
        };
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = pane.update_in(cx, |p, window, cx| p.attach_files(&paths, window, cx));
            }
        })
        .detach();
    }

    /// List attachments no note links to, and offer to move them to the trash.
    pub(super) fn confirm_prune(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A just-saved link must count, so read the files as they are now.
        if let Err(e) = self.index.borrow_mut().sync(&self.vault) {
            return notify_error(window, cx, e.to_string());
        }
        let unused = match attach::unreferenced(&self.vault, &self.index.borrow()) {
            Ok(unused) => unused,
            Err(e) => return notify_error(window, cx, e.to_string()),
        };
        if unused.is_empty() {
            window.push_notification(
                Notification::info("Every attachment is linked from a note"),
                cx,
            );
            return;
        }
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (this, list) = (this.clone(), unused.clone());
            let files = v_flex()
                .gap_2()
                .child(format!(
                    "No note links to these {} files. They will move to .katib/trash.",
                    list.len()
                ))
                .child(
                    v_flex()
                        .id("unused")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .children(list.iter().map(|p| div().child(p.clone()))),
                );
            alert
                .width(px(560.))
                .title("Remove unused attachments")
                .description(files)
                .show_cancel(true)
                .ok_text("Remove")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.prune(&list, window, cx));
                    true
                })
        });
    }

    fn prune(&mut self, unused: &[String], window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready_to_change_files(window, cx) {
            return;
        }
        let failed: Vec<_> = unused
            .iter()
            .filter_map(|p| {
                attach::trash_attachment(&self.vault, p)
                    .err()
                    .map(|e| format!("{p}: {e}"))
            })
            .collect();
        self.sync.note_change();
        if failed.is_empty() {
            window.push_notification(
                Notification::success(format!("Moved {} files to .katib/trash", unused.len())),
                cx,
            );
        } else {
            notify_error(window, cx, failed.join("; "));
        }
    }

    /// The sidebar's right-click menu for tree item `key` (`note:<id>` or
    /// `dir:<path>`); `has_page` tells whether a folder has a page.
    pub(super) fn tree_menu(
        view: &Entity<Katib>,
        key: &str,
        page: Option<String>,
        menu: PopupMenu,
    ) -> PopupMenu {
        let item = |label: &'static str, f: Rc<MenuAction>| {
            let view = view.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let f = f.clone();
                view.update(cx, |this, cx| f(this, window, cx));
            })
        };
        if let Some(id) = key.strip_prefix("note:") {
            let (a, b, c) = (id.to_owned(), id.to_owned(), id.to_owned());
            menu.item(item(
                "Open in new tab",
                Rc::new(move |this, w, cx| this.open_in(&c, true, w, cx)),
            ))
            .separator()
            .item(item(
                "Rename or move...",
                Rc::new(move |this, w, cx| this.prompt_name(Naming::Move(a.clone()), w, cx)),
            ))
            .item(item(
                "Delete...",
                Rc::new(move |this, w, cx| this.confirm_delete(b.clone(), w, cx)),
            ))
        } else if let Some(dir) = key.strip_prefix("dir:") {
            let (a, b, c) = (dir.to_owned(), dir.to_owned(), dir.to_owned());
            // The page is not in the tree, so its own actions are here.
            let menu = match page {
                Some(page) => {
                    let p = page.clone();
                    menu.item(item(
                        "Rename or move page...",
                        Rc::new(move |this, w, cx| {
                            this.prompt_name(Naming::Move(p.clone()), w, cx)
                        }),
                    ))
                    .item(item(
                        "Delete page...",
                        Rc::new(move |this, w, cx| this.confirm_delete(page.clone(), w, cx)),
                    ))
                    .separator()
                }
                None => menu.item(item(
                    "Create folder page",
                    Rc::new(move |this, w, cx| this.create(&format!("{c}/README"), false, w, cx)),
                )),
            };
            let d = dir.to_owned();
            menu.item(item(
                "New note here...",
                Rc::new(move |this, w, cx| this.prompt_new_in(&a, w, cx)),
            ))
            .item(item(
                "New folder here...",
                Rc::new(move |this, w, cx| this.prompt_folder_in(&d, w, cx)),
            ))
            .item(item(
                "Rename or move folder...",
                Rc::new(move |this, w, cx| this.prompt_name(Naming::MoveFolder(b.clone()), w, cx)),
            ))
        } else {
            menu
        }
    }
}

/// The page id for a new folder at `path`, refused if the path is no valid folder or exists.
fn new_folder_page(vault: &katib_core::Vault, path: &str) -> Result<String, String> {
    let path = path.trim().trim_matches('/');
    let page = vault
        .entry(&format!("{path}/README"))
        .map_err(|e| e.to_string())?;
    if page.path.parent().is_some_and(|dir| dir.exists()) {
        return Err(format!("A folder named {path:?} exists"));
    }
    Ok(page.id)
}

#[cfg(test)]
mod tests {
    use super::{Moved, new_folder_page, preview_text};

    fn moved(notes: usize, rewritten: &[&str]) -> Moved {
        Moved {
            notes: (0..notes).map(|i| (i.to_string(), i.to_string())).collect(),
            rewritten: rewritten.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_new_folder_is_its_page_and_never_an_existing_folder() {
        let root = std::env::temp_dir().join(format!("katib-new-folder-{}", std::process::id()));
        std::fs::create_dir_all(root.join("old")).unwrap();
        let vault = katib_core::Vault::open(&root).unwrap();
        let page = |p: &str| new_folder_page(&vault, p);
        assert_eq!(page(" a/b/ "), Ok("a/b/README".into()));
        assert_eq!(page("old"), Err("A folder named \"old\" exists".into()));
        assert!(page("").is_err() && page(".hidden").is_err() && page("../out").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn previews_name_the_first_notes_and_count_the_rest() {
        let text = |n, r| preview_text(Ok(moved(n, r))).0;
        assert_eq!(text(0, &[]), "");
        // A folder of attachments moves no note but can rewrite links to it.
        assert_eq!(text(0, &["a"]), "Updates links in 1 note: a.");
        assert_eq!(text(1, &[]), "No links to update.");
        assert_eq!(text(1, &["a"]), "Updates links in 1 note: a.");
        assert_eq!(
            text(3, &["a", "b", "c", "d", "e", "f", "g"]),
            "Moves 3 notes. Updates links in 7 notes: a, b, c, d, e and 2 more."
        );
    }
}
