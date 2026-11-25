//! The window's layout: toolbar, sidebar, tabs, the note with its backlinks,
//! and the status bar.
//!
//! The title bar's title sits outside both toolbars. A `Toolbar` prevents the
//! default of every press on it, which stops the title bar moving the window.

use super::*;
use gpui_kit::component::{
    Disableable as _, IconName, Selectable as _, Sizable as _, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    list::ListItem,
    menu::DropdownMenu as _,
    resizable::{h_resizable, resizable_panel, v_resizable},
    status_bar::StatusBar,
    tab::{Tab, TabBar},
    toolbar::{Toolbar, ToolbarGroup},
    tree::tree,
    v_flex,
};

impl Katib {
    /// Two toolbars with `title` between them, for the title bar.
    fn toolbar(&self, title: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let note_open = !self.tabs.is_empty();
        let mode = self.pane().map_or(self.mode, |p| p.read(cx).mode());
        let focus = self.focus.clone();
        let (can_back, can_forward) = self.can_go();
        let button = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).ghost().icon(icon).tooltip(tip)
        };
        let left = Toolbar::new("toolbar")
            .small()
            .child(
                ToolbarGroup::new("view")
                    .label("View")
                    .child(
                        button("sidebar", IconName::PanelLeft, "Toggle sidebar")
                            .selected(self.sidebar_open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_open = !this.sidebar_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        button("search", IconName::Search, "Open, search or run a command")
                            .on_click(
                                cx.listener(|this, _, w, cx| this.open_palette(false, w, cx)),
                            ),
                    ),
            )
            .child(
                ToolbarGroup::new("history")
                    .label("History")
                    .child(
                        button("back", IconName::ArrowLeft, "Back")
                            .disabled(!can_back)
                            .on_click(cx.listener(|this, _, w, cx| this.go(true, w, cx))),
                    )
                    .child(
                        button("forward", IconName::ArrowRight, "Forward")
                            .disabled(!can_forward)
                            .on_click(cx.listener(|this, _, w, cx| this.go(false, w, cx))),
                    ),
            )
            .child(
                ToolbarGroup::new("notes")
                    .label("Notes")
                    .child(
                        button("new-note", IconName::Plus, "New note").on_click(cx.listener(
                            |this, _, w, cx| this.prompt_name(manage::Naming::Create, w, cx),
                        )),
                    )
                    .child(
                        button("tasks", IconName::CircleCheck, "Tasks")
                            .on_click(cx.listener(|this, _, w, cx| this.open_tasks(w, cx))),
                    ),
            )
            .content(self.calendar_button(cx));
        let title = div()
            .flex_1()
            .min_w_0()
            .text_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .truncate()
            .child(title.to_owned());
        let right = Toolbar::new("toolbar-note")
            .small()
            .child(
                ToolbarGroup::new("note")
                    .label("Note")
                    .child(
                        Button::new("mode")
                            .ghost()
                            .icon(match mode {
                                Mode::Live => IconName::Eye,
                                Mode::Raw => IconName::FileText,
                            })
                            .label(match mode {
                                Mode::Live => "Live",
                                Mode::Raw => "Raw",
                            })
                            .tooltip("Toggle raw markdown")
                            .on_click(cx.listener(|this, _, w, cx| this.toggle_mode(w, cx))),
                    )
                    .child(
                        button("settings", IconName::Settings, "Settings")
                            .on_click(cx.listener(|this, _, w, cx| this.open_settings(w, cx))),
                    ),
            )
            .content(
                Button::new("more")
                    .ghost()
                    .small()
                    .icon(IconName::Ellipsis)
                    .tooltip("More")
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        menu.action_context(focus.clone())
                            .menu_with_icon_and_disabled(
                                "Attach file...",
                                IconName::File,
                                Box::new(AttachFile),
                                !note_open,
                            )
                            .menu_with_icon_and_disabled(
                                "Rename or move note...",
                                IconName::FileText,
                                Box::new(RenameNote),
                                !note_open,
                            )
                            .menu_with_icon_and_disabled(
                                "Delete note...",
                                IconName::Delete,
                                Box::new(DeleteNote),
                                !note_open,
                            )
                            .separator()
                            .menu_with_icon(
                                "Remove unused attachments...",
                                IconName::Delete,
                                Box::new(PruneAttachments),
                            )
                            .menu_with_icon("Sync now", IconName::RefreshCw, Box::new(SyncNow))
                    }),
            );
        h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(left)
            .child(title)
            .child(right)
    }

    /// The overview's tab, then one tab per open note.
    fn tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.tabs.iter().enumerate().map(|(ix, tab)| {
            let saved = tab.pane.read(cx).is_saved();
            let title = self.tab_title(tab, cx);
            Tab::new()
                .label(if saved { title } else { format!("{title} *") })
                .suffix(
                    Button::new(("close-tab", ix))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Close tab")
                        .on_click(cx.listener(move |this, _, w, cx| {
                            // The tab under the button must not take the click too.
                            cx.stop_propagation();
                            this.close_tab(ix, w, cx);
                        })),
                )
        });
        let this = cx.weak_entity();
        let selected = if self.overview_shown() {
            0
        } else {
            self.active + 1
        };
        TabBar::new("tabs")
            .small()
            .selected_index(selected)
            .on_click(move |ix, window, cx| {
                let _ = this.update(cx, |this, cx| match ix.checked_sub(1) {
                    None => this.show_overview(window, cx),
                    Some(tab) => this.activate(tab, window, cx),
                });
            })
            .child(Tab::new().icon(IconName::LayoutDashboard).label("Overview"))
            .children(tabs)
            .suffix(
                Button::new("new-tab")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip("New tab")
                    .on_click(cx.listener(|this, _, w, cx| this.open_palette(true, w, cx))),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let menu_view = view.clone();
        // Folder pages by tree key: a folder's row opens its page.
        let pages: Rc<HashMap<String, String>> = Rc::new(
            self.pages
                .iter()
                .map(|(dir, page)| (format!("dir:{dir}"), page.clone()))
                .collect(),
        );
        let menu_pages = pages.clone();
        tree(&self.tree, move |ix, entry, selected, _, _| {
            let item = entry.item();
            // A folder holding only its page has no children, but is still a folder.
            let folder = item.id.starts_with("dir:");
            let icon = match (folder, entry.is_expanded()) {
                (false, _) => IconName::File,
                (true, true) => IconName::FolderOpen,
                (true, false) => IconName::Folder,
            };
            let note = item
                .id
                .strip_prefix("note:")
                .map(str::to_owned)
                .or_else(|| pages.get(item.id.as_ref()).cloned());
            ListItem::new(ix)
                .w_full()
                .py_0p5()
                .px_2()
                .pl(px(14.) * entry.depth() as f32 + px(8.))
                .selected(selected)
                .child(h_flex().gap_2().child(icon).child(item.label.clone()))
                .when_some(note, |row, id| {
                    let view = view.clone();
                    row.on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| this.open(&id, window, cx));
                    })
                })
        })
        .context_menu(move |_, entry, menu, _, _| {
            let id = &entry.item().id;
            Katib::tree_menu(&menu_view, id, menu_pages.get(id.as_ref()).cloned(), menu)
        })
        .text_sm()
        .p_1()
        .h_full()
    }

    /// The active note, its backlinks below it.
    fn editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pane) = self.pane().cloned() else {
            return self.overview(cx);
        };
        let theme = cx.theme();
        let backlinks = self.backlinks.iter().enumerate().map(|(ix, b)| {
            let id = b.src.clone();
            v_flex()
                .id(("backlink", ix))
                .px_4()
                .py_1()
                .cursor_pointer()
                .hover(|d| d.bg(theme.muted))
                .child(div().text_sm().child(b.title.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(b.context.clone()),
                )
                .on_click(cx.listener(move |this, _, w, cx| this.open(&id, w, cx)))
        });
        let open = self.backlinks_open;
        let header = h_flex()
            .px_2()
            .py_0p5()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Button::new("backlinks-toggle")
                    .ghost()
                    .xsmall()
                    .icon(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .label(format!("{} backlinks", self.backlinks.len()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.backlinks_open = !this.backlinks_open;
                        cx.notify();
                    })),
            );
        if open {
            v_resizable("note-and-backlinks")
                .child(resizable_panel().child(pane.clone()))
                .child(
                    resizable_panel().size(px(160.)).child(
                        v_flex()
                            .id("backlinks")
                            .size_full()
                            .overflow_y_scroll()
                            .child(header)
                            .children(backlinks),
                    ),
                )
                .into_any_element()
        } else {
            v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(pane.clone()))
                .child(header)
                .into_any_element()
        }
    }

    fn status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sync = self.sync_status(cx);
        let theme = cx.theme();
        let mut bar = StatusBar::new();
        match self.pane() {
            Some(pane) => {
                let p = pane.read(cx);
                let state = div()
                    .when(!p.is_saved(), |d| d.text_color(theme.warning))
                    .child(p.status());
                let tags = (!self.note_tags.is_empty()).then(|| {
                    self.note_tags
                        .iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                bar = bar
                    .left(p.entry.id.clone())
                    .left(state)
                    .when_some(tags, |b, tags| b.left(tags))
                    .right(format!("{} words", p.words(cx)));
            }
            None => bar = bar.left("Overview. cmd-p: open, search or create a note"),
        }
        bar.when(self.syncing, |b| b.right("indexing")).right(sync)
    }
}

impl Render for Katib {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = v_flex()
            .size_full()
            .child(self.tab_bar(cx))
            .child(div().flex_1().min_h_0().child(self.editor(cx)));
        let body = if self.sidebar_open {
            h_resizable("main")
                .child(resizable_panel().size(px(240.)).child(self.sidebar(cx)))
                .child(resizable_panel().child(main))
                .into_any_element()
        } else {
            main.into_any_element()
        };
        let root = v_flex()
            .size_full()
            .bg(cx.theme().background)
            .key_context(CONTEXT)
            .track_focus(&self.focus);
        let title = self.pane().map_or_else(
            || "katib".to_owned(),
            |p| format!("{} - katib", self.tab_title_of(p, cx)),
        );
        // The title the OS shows in the Dock, taskbar and window lists.
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title.clone();
        }
        let this = cx.weak_entity();
        let title_bar = TitleBar::new()
            // Closing from the title bar saves first, as the OS close does.
            .on_close_window(move |_, window, cx| {
                let _ = this.update(cx, |this, cx| this.close(cx));
                window.remove_window();
            })
            .child(
                h_flex()
                    .size_full()
                    .min_w_0()
                    .gap_1()
                    .children(
                        self.menu_bar
                            .clone()
                            .map(|menus| div().h_full().child(menus)),
                    )
                    .child(self.toolbar(&title, cx)),
            );
        self.on_actions(root, cx)
            .child(title_bar)
            .child(div().flex_1().min_h_0().child(body))
            .child(self.status_bar(cx))
    }
}
