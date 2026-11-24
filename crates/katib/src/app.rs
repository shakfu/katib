//! The window: the note list, the open note, its backlinks, the palette, and
//! the watcher that keeps the index and the open note current.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui_kit::component::{
    ActiveTheme as _, IconName, IndexPath, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    list::ListItem,
    notification::Notification,
    resizable::{h_resizable, resizable_panel, v_resizable},
    tree::{TreeEvent, TreeItem, TreeState, tree},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{Backlink, Index, Link, NoteRow, Target, Vault};
use notify::Watcher as _;

use crate::pane::{Mode, NotePane, PaneEvent};

mod sync;

const CONTEXT: &str = "Katib";

actions!(katib, [OpenPalette, ToggleMode, Save, Quit]);

pub fn bind_keys(cx: &mut App) {
    #[cfg(target_os = "macos")]
    let [palette, toggle, save, quit] = ["cmd-p", "cmd-e", "cmd-s", "cmd-q"];
    #[cfg(not(target_os = "macos"))]
    let [palette, toggle, save, quit] = ["ctrl-p", "ctrl-e", "ctrl-s", "ctrl-q"];
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new(palette, OpenPalette, c),
        KeyBinding::new(toggle, ToggleMode, c),
        KeyBinding::new(save, Save, c),
        KeyBinding::new(quit, Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// What a palette row does.
#[derive(Clone)]
enum Pick {
    Open(String),
    Create(String),
    ToggleMode,
    Save,
}

pub struct Katib {
    vault: Vault,
    /// The window's connection; a background sync uses its own.
    index: Rc<RefCell<Index>>,
    notes: Vec<NoteRow>,
    tree: Entity<TreeState>,
    /// Open folders, by tree id; the tree forgets them when its items are replaced.
    expanded: HashSet<String>,
    pane: Option<Entity<NotePane>>,
    backlinks: Vec<Backlink>,
    mode: Mode,
    palette: Entity<CommandState>,
    /// The palette's rows, by group then row, matching what it shows.
    picks: Vec<Vec<Pick>>,
    groups: Vec<CommandGroup>,
    /// Set by the file watcher; read by the poll loop.
    disk_changed: Arc<AtomicBool>,
    syncing: bool,
    resync: bool,
    sync: sync::SyncState,
    /// Whether the backlinks panel is expanded; collapsed at start.
    backlinks_open: bool,
    /// Holds focus when no editor does, so the window's key bindings apply.
    focus: FocusHandle,
    pane_subscription: Option<Subscription>,
    _watcher: Option<notify::RecommendedWatcher>,
    _subscriptions: Vec<Subscription>,
}

impl Katib {
    pub fn new(vault: Vault, index: Index, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let disk_changed = Arc::new(AtomicBool::new(false));
        let watcher = match watch(vault.root(), disk_changed.clone()) {
            Ok(w) => Some(w),
            Err(e) => {
                window.push_notification(
                    Notification::warning(format!(
                        "Cannot watch the vault ({e}); changes made outside katib show after a restart."
                    )),
                    cx,
                );
                None
            }
        };
        let tree = cx.new(|cx| TreeState::new(cx));
        let subscriptions = vec![
            cx.on_app_quit(|this, cx| {
                this.close(cx);
                async {}
            }),
            cx.subscribe(&tree, |this, _, event: &TreeEvent, _| match event {
                TreeEvent::Expanded(id) => {
                    this.expanded.insert(id.to_string());
                }
                TreeEvent::Collapsed(id) => {
                    this.expanded.remove(id.as_ref());
                }
            }),
        ];
        window.on_window_should_close(cx, {
            let this = cx.weak_entity();
            move |_, cx| {
                let _ = this.update(cx, |this, cx| this.close(cx));
                true
            }
        });
        let activation = cx.observe_window_activation(window, |this, window, _| {
            // Edits from another machine arrive when the user comes back.
            if window.is_window_active() {
                this.sync.fetch_soon();
            }
        });
        let sync = sync::SyncState::new(vault.root());
        let mut this = Self {
            vault,
            index: Rc::new(RefCell::new(index)),
            notes: Vec::new(),
            tree,
            expanded: HashSet::new(),
            pane: None,
            backlinks: Vec::new(),
            mode: Mode::default(),
            palette: cx.new(|cx| CommandState::new(window, cx)),
            picks: Vec::new(),
            groups: Vec::new(),
            disk_changed,
            syncing: false,
            resync: false,
            sync,
            backlinks_open: false,
            focus: cx.focus_handle(),
            pane_subscription: None,
            _watcher: watcher,
            _subscriptions: subscriptions.into_iter().chain([activation]).collect(),
        };
        window.focus(&this.focus, cx);
        this.refresh(cx);
        this.sync_index(cx);
        this.poll(window, cx);
        this
    }

    /// Re-read the note list and the open note's backlinks from the index.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let index = self.index.borrow();
        match index.notes() {
            Ok(notes) if notes != self.notes => {
                let items = build_tree(&notes, &self.expanded);
                self.tree.update(cx, |t, cx| t.set_items(items, cx));
                self.notes = notes;
            }
            Ok(_) => {}
            Err(e) => eprintln!("katib: {e}"),
        }
        let open = self.pane.as_ref().map(|p| p.read(cx).entry.id.clone());
        self.backlinks = open
            .as_ref()
            .and_then(|id| index.backlinks(id).ok())
            .unwrap_or_default();
        drop(index);
        if let Some(id) = open {
            let item = TreeItem::new(note_key(&id), "");
            self.tree
                .update(cx, |t, cx| t.set_selected_item(Some(&item), cx));
        }
        cx.notify();
    }

    /// Bring the index up to date on a background thread, then refresh.
    fn sync_index(&mut self, cx: &mut Context<Self>) {
        if self.syncing {
            self.resync = true;
            return;
        }
        self.syncing = true;
        let vault = self.vault.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { Index::open(&vault)?.sync(&vault) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.syncing = false;
                if let Err(e) = result {
                    eprintln!("katib: {e}");
                }
                this.refresh(cx);
                if std::mem::take(&mut this.resync) {
                    this.sync_index(cx);
                }
            });
        })
        .detach();
    }

    /// Twice a second, act on what the watcher saw.
    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let alive = this.update_in(cx, |this, window, cx| {
                    if this.disk_changed.swap(false, Ordering::Relaxed) {
                        this.sync.note_change();
                        this.sync_index(cx);
                        if let Some(pane) = &this.pane {
                            pane.update(cx, |p, cx| p.disk_changed(window, cx));
                        }
                    }
                    this.sync_tick(cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Save the open note and commit what is saved; no network on the way out.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.save_open(cx);
        if self.pane.as_ref().is_none_or(|p| p.read(cx).is_saved()) {
            self.sync.commit_now();
        }
    }

    fn save_open(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = &self.pane {
            pane.update(cx, |p, cx| p.save(cx));
        }
    }

    pub fn open(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let entry = match self.vault.entry(id) {
            Ok(entry) => entry,
            Err(e) => return notify_error(window, cx, e.to_string()),
        };
        let text = match std::fs::read_to_string(&entry.path) {
            Ok(text) => text,
            Err(e) => return notify_error(window, cx, format!("{id}: {e}")),
        };
        self.save_open(cx);
        let (vault, index, mode) = (self.vault.clone(), self.index.clone(), self.mode);
        let pane = cx.new(|cx| NotePane::new(vault, index, entry, text, mode, window, cx));
        let subscription =
            cx.subscribe_in(&pane, window, |this, _, event, window, cx| match event {
                PaneEvent::Saved => {
                    this.sync.note_change();
                    this.refresh(cx);
                }
                PaneEvent::OpenLink(link) => this.follow(link.clone(), window, cx),
                PaneEvent::Close => {
                    this.pane = None;
                    this.pane_subscription = None;
                    this.refresh(cx);
                }
            });
        // Focus the window, not the editor: the note reads formatted, with no
        // caret, until clicked, and the window's shortcuts keep working.
        window.focus(&self.focus, cx);
        self.pane = Some(pane);
        self.pane_subscription = Some(subscription);
        self.refresh(cx);
    }

    fn follow(&mut self, link: Link, window: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.pane.as_ref().map(|p| p.read(cx).entry.id.clone()) else {
            return;
        };
        let target = self.index.borrow().resolve(&from, &link);
        match target {
            Ok(Target::Note(id)) => {
                if id != from {
                    self.open(&id, window, cx);
                }
                if let Some(fragment) = link.fragment.filter(|f| !f.trim().is_empty()) {
                    self.go_to_heading(&fragment, window, cx);
                }
            }
            Ok(Target::Ambiguous(ids)) => self.choose(ids, window, cx),
            Ok(Target::Missing(id)) => self.create(&id, window, cx),
            Ok(Target::File(rel)) => {
                let path = self.vault.root().join(&rel);
                if path.exists() {
                    cx.open_with_system(&path);
                } else {
                    notify_error(window, cx, format!("No file {rel}"));
                }
            }
            Ok(Target::External(url)) => {
                // Note text is untrusted input: open only web and mail links.
                let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
                if matches!(scheme.as_str(), "http" | "https" | "mailto") {
                    cx.open_url(&url);
                } else {
                    notify_error(window, cx, format!("Not opening a {scheme}: link"));
                }
            }
            Ok(Target::Invalid) => {
                notify_error(window, cx, "That link points outside the vault".into())
            }
            Err(e) => notify_error(window, cx, e.to_string()),
        }
    }

    /// Move to the heading `fragment` names in the open note.
    fn go_to_heading(&mut self, fragment: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.pane.clone() else {
            return;
        };
        let text = pane.read(cx).text(cx);
        match katib_core::heading_offset(&text, fragment) {
            Some(offset) => pane.update(cx, |p, cx| p.go_to(offset, window, cx)),
            None => notify_error(
                window,
                cx,
                format!("No heading \"{fragment}\" in this note"),
            ),
        }
    }

    /// Create a note titled after its id's last part, then open it.
    fn create(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let title = id.rsplit('/').next().unwrap_or(id).trim();
        let text = format!("# {title}\n\n");
        match self.vault.create(id, &text) {
            Ok(entry) => {
                if let Err(e) = self.index.borrow_mut().update(&entry, &text) {
                    eprintln!("katib: {e}");
                }
                self.refresh(cx);
                self.open(&entry.id, window, cx);
                // A new note is opened to be written: caret after the title.
                if let Some(pane) = &self.pane {
                    pane.update(cx, |p, cx| p.go_to(text.len(), window, cx));
                }
            }
            Err(e) => notify_error(window, cx, e.to_string()),
        }
    }

    fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = &self.pane {
            self.mode = pane.update(cx, |p, cx| {
                p.toggle_mode(window, cx);
                p.mode()
            });
        } else {
            self.mode = match self.mode {
                Mode::Live => Mode::Raw,
                Mode::Raw => Mode::Live,
            };
        }
        cx.notify();
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.fill_palette("", None, cx);
        self.show_palette("Open, search or run a command...", window, cx);
    }

    /// Ask which of several notes a link means.
    fn choose(&mut self, ids: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.fill_palette("", Some(ids), cx);
        self.show_palette("This link matches several notes", window, cx);
    }

    fn show_palette(
        &mut self,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.palette.clone();
        state.update(cx, |s, cx| s.set_query("", window, cx));
        let this = cx.weak_entity();
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (state, this, focused) = (state.clone(), this.clone(), focused.clone());
            dialog
                .close_button(false)
                .p_0()
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let state = state.clone();
                        window.defer(cx, move |window, cx| {
                            state.update(cx, |s, cx| s.focus(window, cx))
                        });
                    }
                    let groups = this
                        .read_with(cx, |this, _| this.groups.clone())
                        .unwrap_or_default();
                    let (on_query, on_confirm) = (this.clone(), this.clone());
                    let command = Command::new(&state)
                        .bordered(false)
                        .filterable(false)
                        .placeholder(placeholder)
                        .max_h(px(420.))
                        .on_query(move |query, _, cx| {
                            let _ =
                                on_query.update(cx, |this, cx| this.fill_palette(query, None, cx));
                        })
                        .on_confirm(move |ix, window, cx| {
                            window.close_dialog(cx);
                            let _ = on_confirm.update(cx, |this, cx| this.pick(ix, window, cx));
                        });
                    content.child(groups.into_iter().fold(command, Command::group))
                })
        });
    }

    /// Build the palette rows for `query`, or for a fixed list of note `choices`.
    fn fill_palette(&mut self, query: &str, choices: Option<Vec<String>>, cx: &mut Context<Self>) {
        let query = query.trim();
        let lower = query.to_lowercase();
        let title = |id: &str| {
            self.notes
                .iter()
                .find(|n| n.id == id)
                .map_or_else(|| id.to_owned(), |n| n.title.clone())
        };
        let mut groups: Vec<(&str, Vec<Row>)> = Vec::new();
        if let Some(ids) = choices {
            groups.push((
                "Which note?",
                ids.iter()
                    .map(|id| (title(id), Some(id.clone()), Pick::Open(id.clone())))
                    .collect(),
            ));
        } else {
            let named: Vec<_> = self
                .notes
                .iter()
                .filter(|n| {
                    lower.is_empty()
                        || n.title.to_lowercase().contains(&lower)
                        || n.id.to_lowercase().contains(&lower)
                })
                .take(if lower.is_empty() { 200 } else { 30 })
                .map(|n| {
                    (
                        n.title.clone(),
                        Some(n.id.clone()),
                        Pick::Open(n.id.clone()),
                    )
                })
                .collect();
            let exact = self
                .notes
                .iter()
                .any(|n| n.title.to_lowercase() == lower || n.id.to_lowercase() == lower);
            let listed: Vec<_> = named.iter().map(|(_, id, _)| id.clone()).collect();
            let hits = self.index.borrow().search(query, 30).unwrap_or_default();
            let found: Vec<_> = hits
                .into_iter()
                .filter(|h| !listed.contains(&Some(h.id.clone())))
                .map(|h| (h.title, Some(h.snippet), Pick::Open(h.id)))
                .collect();
            let commands: Vec<_> = [
                ("Toggle raw markdown", Pick::ToggleMode),
                ("Save", Pick::Save),
            ]
            .into_iter()
            .filter(|(label, _)| lower.is_empty() || label.to_lowercase().contains(&lower))
            .map(|(label, pick)| (label.to_owned(), None, pick))
            .collect();
            groups.push(("Notes", named));
            groups.push(("Text", found));
            if !query.is_empty() && !exact {
                groups.push((
                    "Create",
                    vec![(
                        format!("New note: {query}"),
                        None,
                        Pick::Create(query.to_owned()),
                    )],
                ));
            }
            groups.push(("Commands", commands));
        }
        groups.retain(|(_, rows)| !rows.is_empty());
        self.picks = groups
            .iter()
            .map(|(_, rows)| rows.iter().map(|(_, _, p)| p.clone()).collect())
            .collect();
        self.groups = groups
            .into_iter()
            .map(|(label, rows)| {
                CommandGroup::new()
                    .label(label)
                    .items(rows.into_iter().map(|(text, detail, _)| row(text, detail)))
            })
            .collect();
        cx.notify();
    }

    fn pick(&mut self, ix: IndexPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pick) = self
            .picks
            .get(ix.section)
            .and_then(|g| g.get(ix.row))
            .cloned()
        else {
            return;
        };
        match pick {
            Pick::Open(id) => self.open(&id, window, cx),
            Pick::Create(id) => self.create(&id, window, cx),
            Pick::ToggleMode => self.toggle_mode(window, cx),
            Pick::Save => self.save_open(cx),
        }
    }
}

/// A palette row's label, optional second line, and action.
type Row = (String, Option<String>, Pick);

/// A palette row: a label, and a second muted line when there is detail.
fn row(text: String, detail: Option<String>) -> CommandItem {
    let item = CommandItem::new().label(text.clone());
    match detail {
        None => item,
        Some(detail) => item.child(move |_, cx| {
            v_flex()
                .w_full()
                .py_1()
                .child(div().text_sm().child(text.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(detail.clone()),
                )
        }),
    }
}

fn notify_error(window: &mut Window, cx: &mut App, message: String) {
    window.push_notification(Notification::error(message), cx);
}

/// Watch `root` recursively; set `flag` on any change outside hidden paths
/// (`.git`, `.katib`, and katib's own temporary files).
fn watch(root: &Path, flag: Arc<AtomicBool>) -> notify::Result<notify::RecommendedWatcher> {
    let base = root.to_owned();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        let visible = |p: &Path| {
            p.strip_prefix(&base).is_ok_and(|rel| {
                rel.components()
                    .all(|c| !c.as_os_str().to_string_lossy().starts_with('.'))
            })
        };
        if event.paths.iter().any(|p| visible(p)) {
            flag.store(true, Ordering::Relaxed);
        }
    })?;
    watcher.watch(root, notify::RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn note_key(id: &str) -> String {
    format!("note:{id}")
}

/// The vault as folders and notes. Folders come first, then notes by title.
/// Keys carry a prefix, since `a.md` and the folder `a/` can both exist.
fn build_tree(notes: &[NoteRow], expanded: &HashSet<String>) -> Vec<TreeItem> {
    #[derive(Default)]
    struct Dir<'a> {
        dirs: BTreeMap<&'a str, Dir<'a>>,
        notes: Vec<&'a NoteRow>,
    }
    fn items(dir: Dir, path: &str, expanded: &HashSet<String>) -> Vec<TreeItem> {
        let mut out: Vec<TreeItem> = dir
            .dirs
            .into_iter()
            .map(|(name, sub)| {
                let path = if path.is_empty() {
                    name.to_owned()
                } else {
                    format!("{path}/{name}")
                };
                let key = format!("dir:{path}");
                let open = expanded.contains(&key);
                TreeItem::new(key, name.to_owned())
                    .children(items(sub, &path, expanded))
                    .expanded(open)
            })
            .collect();
        let mut notes = dir.notes;
        notes.sort_by_key(|n| n.title.to_lowercase());
        out.extend(
            notes
                .into_iter()
                .map(|n| TreeItem::new(note_key(&n.id), n.title.clone())),
        );
        out
    }
    let mut root = Dir::default();
    for note in notes {
        let mut dir = &mut root;
        let mut parts: Vec<&str> = note.id.split('/').collect();
        parts.pop();
        for part in parts {
            dir = dir.dirs.entry(part).or_default();
        }
        dir.notes.push(note);
    }
    items(root, "", expanded)
}

impl Render for Katib {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sync = self.sync_status(cx);
        let theme = cx.theme();
        let view = cx.entity();
        let sidebar = tree(&self.tree, move |ix, entry, selected, _, _| {
            let item = entry.item();
            let icon = match (entry.is_folder(), entry.is_expanded()) {
                (false, _) => IconName::File,
                (true, true) => IconName::FolderOpen,
                (true, false) => IconName::Folder,
            };
            let note = item.id.strip_prefix("note:").map(str::to_owned);
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
        .text_sm()
        .p_1()
        .h_full();
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
        let status = match &self.pane {
            Some(pane) => {
                let p = pane.read(cx);
                let mode = match p.mode() {
                    Mode::Live => "live",
                    Mode::Raw => "raw",
                };
                format!("{} | {} | {mode}", p.entry.id, p.status())
            }
            None => "cmd-p: open, search or create a note".into(),
        };
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
        let editor = match &self.pane {
            Some(pane) if open => v_resizable("note-and-backlinks")
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
                .into_any_element(),
            Some(pane) => v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(pane.clone()))
                .child(header)
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("No note open")
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(theme.background)
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &OpenPalette, w, cx| this.open_palette(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleMode, w, cx| this.toggle_mode(w, cx)))
            .on_action(cx.listener(|this, _: &Save, _, cx| this.save_open(cx)))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("main")
                        .child(resizable_panel().size(px(240.)).child(sidebar))
                        .child(resizable_panel().child(editor)),
                ),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .border_t_1()
                    .border_color(theme.border)
                    .gap_3()
                    .child(status)
                    .when(self.syncing, |d| d.child(div().child("indexing")))
                    .child(div().ml_auto().child(sync)),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not a glob: `gpui_kit::*` would bring GPUI's own `test` macro in.
    use super::{HashSet, NoteRow, TreeItem, build_tree};

    fn row(id: &str, title: &str) -> NoteRow {
        NoteRow {
            id: id.into(),
            title: title.into(),
        }
    }

    fn shape(items: &[TreeItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i.is_folder() {
                true => format!("{} [{}]", i.id, shape(&i.children).join(", ")),
                false => format!("{}={}", i.id, i.label),
            })
            .collect()
    }

    #[test]
    fn tree_puts_folders_first_and_keeps_expansion() {
        let notes = [
            row("z", "Alpha"),
            row("a/b", "Beta"),
            row("a", "Same name as the folder"),
            row("a/c/d", "Deep"),
        ];
        let expanded = HashSet::from(["dir:a".to_owned()]);
        let items = build_tree(&notes, &expanded);
        assert_eq!(
            shape(&items),
            [
                "dir:a [dir:a/c [note:a/c/d=Deep], note:a/b=Beta]",
                "note:z=Alpha",
                "note:a=Same name as the folder",
            ]
        );
        assert!(items[0].is_expanded());
        assert!(!items[0].children[0].is_expanded());
    }
}
