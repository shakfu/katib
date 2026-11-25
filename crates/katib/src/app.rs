//! The window: the note list, the open notes as tabs, their backlinks, the
//! palette, and the watcher that keeps the index and the open notes current.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui_kit::component::{
    ActiveTheme as _, GlobalState, Icon, IconName, IndexPath, WindowExt as _,
    calendar::CalendarState,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    kbd::Kbd,
    menu::AppMenuBar,
    notification::Notification,
    tree::{TreeEvent, TreeItem, TreeState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{
    Backlink, Index, Link, NoteRow, Target, Vault, config::Config, paths::page_folder,
};
use notify::Watcher as _;

use crate::pane::Mode;

mod chrome;
mod journal;
mod links;
mod manage;
mod overview;
mod settings;
mod sync;
mod tabs;
mod tasks;

const CONTEXT: &str = "Katib";

actions!(
    katib,
    [
        OpenPalette,
        ToggleMode,
        Save,
        Quit,
        NewNote,
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        OpenSettings,
        OpenToday,
        ToggleSidebar,
        ToggleBacklinks,
        SyncNow,
        AttachFile,
        RenameNote,
        DeleteNote,
        PruneAttachments,
        GoBack,
        GoForward,
        BrokenLinks,
        OpenTasks,
        EditTags,
        BrowseTags,
        ShowOverview,
    ]
);

pub fn bind_keys(cx: &mut App) {
    // Bindings take `&'static str`: cmd on macOS, ctrl elsewhere.
    macro_rules! key {
        ($k:literal) => {
            if cfg!(target_os = "macos") {
                concat!("cmd-", $k)
            } else {
                concat!("ctrl-", $k)
            }
        };
    }
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new(key!("p"), OpenPalette, c),
        KeyBinding::new(key!("e"), ToggleMode, c),
        KeyBinding::new(key!("s"), Save, c),
        KeyBinding::new(key!("n"), NewNote, c),
        KeyBinding::new(key!("t"), NewTab, c),
        KeyBinding::new(key!("w"), CloseTab, c),
        KeyBinding::new("ctrl-tab", NextTab, c),
        KeyBinding::new("ctrl-shift-tab", PrevTab, c),
        KeyBinding::new(key!("shift-]"), NextTab, c),
        KeyBinding::new(key!("shift-["), PrevTab, c),
        KeyBinding::new(key!(","), OpenSettings, c),
        KeyBinding::new(key!("shift-d"), OpenToday, c),
        KeyBinding::new(key!("\\"), ToggleSidebar, c),
        KeyBinding::new(key!("shift-b"), ToggleBacklinks, c),
        KeyBinding::new(key!("q"), Quit, None),
        // VS Code's keys: cmd-[ is outdent in the raw editor, alt-left a word move on macOS.
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl--", GoBack, c),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift--", GoForward, c),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-left", GoBack, c),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-right", GoForward, c),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// Whether the OS draws the menu bar. Elsewhere the menus go in the window's
/// title bar, which every platform draws itself.
pub const NATIVE_MENUS: bool = cfg!(target_os = "macos");

/// The application menu bar. Its items dispatch the same actions as the keys.
/// macOS draws it; on Windows and Linux the window draws it with `AppMenuBar`.
pub fn set_menus(cx: &mut App) {
    // macOS puts Settings and Quit in the app menu; elsewhere they end File.
    let mut file = vec![
        MenuItem::action("New Note...", NewNote),
        MenuItem::action("New Tab", NewTab),
        MenuItem::action("Today's Note", OpenToday),
        MenuItem::action("Open...", OpenPalette),
        MenuItem::separator(),
        MenuItem::action("Save", Save),
        MenuItem::action("Close Tab", CloseTab),
    ];
    if !NATIVE_MENUS {
        file.extend([
            MenuItem::separator(),
            MenuItem::action("Settings...", OpenSettings),
            MenuItem::action("Quit", Quit),
        ]);
    }
    let mut menus = vec![
        Menu::new("File").items(file),
        Menu::new("Note").items([
            MenuItem::action("Attach File...", AttachFile),
            MenuItem::action("Rename or Move...", RenameNote),
            MenuItem::action("Delete...", DeleteNote),
            MenuItem::separator(),
            MenuItem::action("Edit Tags...", EditTags),
            MenuItem::action("Tasks...", OpenTasks),
            MenuItem::action("Tags...", BrowseTags),
            MenuItem::action("Broken Links...", BrokenLinks),
            MenuItem::action("Remove Unused Attachments...", PruneAttachments),
        ]),
        Menu::new("View").items([
            MenuItem::action("Overview", ShowOverview),
            MenuItem::action("Toggle Raw Markdown", ToggleMode),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Backlinks", ToggleBacklinks),
            MenuItem::separator(),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PrevTab),
            MenuItem::separator(),
            MenuItem::action("Back", GoBack),
            MenuItem::action("Forward", GoForward),
        ]),
        Menu::new("Sync").items([MenuItem::action("Sync Now", SyncNow)]),
    ];
    if NATIVE_MENUS {
        menus.insert(
            0,
            Menu::new("katib").items([
                MenuItem::action("Settings...", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit katib", Quit),
            ]),
        );
        cx.set_menus(menus);
    } else {
        // `AppMenuBar` reads its menus from here, not from the platform.
        let owned = menus.into_iter().map(Menu::owned).collect();
        GlobalState::global_mut(cx).set_app_menus(owned);
    }
}

/// The palette's commands: label, icon, and the action it runs. The action's
/// key binding shows beside the label.
fn commands(note_open: bool) -> Vec<(&'static str, IconName, Box<dyn Action>)> {
    let mut out: Vec<(&'static str, IconName, Box<dyn Action>)> = vec![
        ("New note...", IconName::Plus, Box::new(NewNote)),
        ("Today's note", IconName::Calendar, Box::new(OpenToday)),
        ("Settings...", IconName::Settings, Box::new(OpenSettings)),
        ("Sync now", IconName::RefreshCw, Box::new(SyncNow)),
        (
            "Toggle sidebar",
            IconName::PanelLeft,
            Box::new(ToggleSidebar),
        ),
        (
            "Overview",
            IconName::LayoutDashboard,
            Box::new(ShowOverview),
        ),
        ("Tasks...", IconName::CircleCheck, Box::new(OpenTasks)),
        ("Tags...", IconName::Star, Box::new(BrowseTags)),
        (
            "Broken links...",
            IconName::TriangleAlert,
            Box::new(BrokenLinks),
        ),
        (
            "Remove unused attachments...",
            IconName::Delete,
            Box::new(PruneAttachments),
        ),
    ];
    if note_open {
        out.extend([
            (
                "Toggle raw markdown",
                IconName::Eye,
                Box::new(ToggleMode) as Box<dyn Action>,
            ),
            (
                "Toggle backlinks",
                IconName::PanelBottom,
                Box::new(ToggleBacklinks),
            ),
            ("Attach file...", IconName::File, Box::new(AttachFile)),
            (
                "Rename or move note...",
                IconName::FileText,
                Box::new(RenameNote),
            ),
            ("Delete note...", IconName::Delete, Box::new(DeleteNote)),
            ("Save", IconName::Check, Box::new(Save)),
            ("Close tab", IconName::Close, Box::new(CloseTab)),
            ("Edit tags...", IconName::Star, Box::new(EditTags)),
            ("Back", IconName::ArrowLeft, Box::new(GoBack)),
            ("Forward", IconName::ArrowRight, Box::new(GoForward)),
        ]);
    }
    out
}

/// What a palette row does.
enum Pick {
    Open(String),
    Create(String),
    Run(Box<dyn Action>),
    /// Search again with this query.
    Query(String),
}

impl Clone for Pick {
    fn clone(&self) -> Self {
        match self {
            Self::Open(id) => Self::Open(id.clone()),
            Self::Create(id) => Self::Create(id.clone()),
            Self::Run(action) => Self::Run(action.boxed_clone()),
            Self::Query(q) => Self::Query(q.clone()),
        }
    }
}

pub struct Katib {
    vault: Vault,
    config: Config,
    /// The window's connection; a background sync uses its own.
    index: Rc<RefCell<Index>>,
    notes: Vec<NoteRow>,
    /// Each folder's page by folder path, from `folder_pages`.
    pages: HashMap<String, String>,
    tree: Entity<TreeState>,
    /// Open folders, by tree id; the tree forgets them when its items are replaced.
    expanded: HashSet<String>,
    tabs: Vec<tabs::Tab>,
    /// The tab shown; meaningless while `tabs` is empty.
    active: usize,
    backlinks: Vec<Backlink>,
    /// The mode new tabs open in: the last one chosen.
    mode: Mode,
    palette: Entity<CommandState>,
    /// The palette's rows, by group then row, matching what it shows.
    picks: Vec<Vec<Pick>>,
    groups: Vec<CommandGroup>,
    /// The palette opens the picked note in a new tab.
    palette_new_tab: bool,
    /// Set by the file watcher; read by the poll loop.
    disk_changed: Arc<AtomicBool>,
    syncing: bool,
    resync: bool,
    sync: sync::SyncState,
    /// The open name dialog, if any.
    prompt: Option<manage::Prompt>,
    /// Whether the backlinks panel is expanded; collapsed at start.
    backlinks_open: bool,
    sidebar_open: bool,
    calendar: Entity<CalendarState>,
    calendar_open: bool,
    /// The window title last given to the OS.
    window_title: String,
    /// The menus in the window's title bar, where the OS draws no menu bar.
    menu_bar: Option<Entity<AppMenuBar>>,
    overview: overview::Overview,
    /// The tasks dialog's rows, while it is open.
    tasks: Vec<katib_core::TaskRow>,
    tasks_show_done: bool,
    /// The active note's tags.
    note_tags: Vec<String>,
    /// The broken links dialog's rows, while it is open.
    broken: Vec<links::BrokenRow>,
    /// Why the last settings change was refused, shown in the settings dialog.
    settings_error: Option<String>,
    /// Holds focus when no editor does, so the window's key bindings apply.
    focus: FocusHandle,
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
        let calendar = cx.new(|cx| CalendarState::new(window, cx));
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
            cx.subscribe_in(&calendar, window, Self::calendar_event),
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
        let config = Config::read(&vault);
        let sync = sync::SyncState::new(&vault, &config);
        let mut this = Self {
            vault,
            config,
            index: Rc::new(RefCell::new(index)),
            notes: Vec::new(),
            pages: HashMap::new(),
            tree,
            expanded: HashSet::new(),
            tabs: Vec::new(),
            active: 0,
            backlinks: Vec::new(),
            mode: Mode::default(),
            palette: cx.new(|cx| CommandState::new(window, cx)),
            picks: Vec::new(),
            groups: Vec::new(),
            palette_new_tab: false,
            disk_changed,
            syncing: false,
            resync: false,
            sync,
            prompt: None,
            backlinks_open: false,
            sidebar_open: true,
            calendar,
            calendar_open: false,
            window_title: String::new(),
            menu_bar: (!NATIVE_MENUS).then(|| AppMenuBar::new(cx)),
            broken: Vec::new(),
            overview: overview::Overview::default(),
            tasks: Vec::new(),
            tasks_show_done: false,
            note_tags: Vec::new(),
            settings_error: None,
            focus: cx.focus_handle(),
            _watcher: watcher,
            _subscriptions: subscriptions.into_iter().chain([activation]).collect(),
        };
        window.focus(&this.focus, cx);
        this.refresh(cx);
        this.load_overview(cx);
        this.sync_index(cx);
        this.poll(window, cx);
        this
    }

    /// Re-read the note list and the open note's backlinks from the index.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let index = self.index.borrow();
        match index.notes() {
            Ok(notes) if notes != self.notes => {
                // A folder that is gone must not come back expanded.
                let dirs = folder_keys(&notes);
                self.expanded.retain(|k| dirs.contains(k));
                self.pages = folder_pages(&notes);
                let items = build_tree(&notes, &self.pages, &self.expanded);
                self.tree.update(cx, |t, cx| t.set_items(items, cx));
                self.notes = notes;
            }
            Ok(_) => {}
            Err(e) => eprintln!("katib: {e}"),
        }
        let open = self.open_id(cx);
        self.backlinks = open
            .as_ref()
            .and_then(|id| index.backlinks(id).ok())
            .unwrap_or_default();
        self.note_tags = open
            .as_ref()
            .and_then(|id| index.note_tags(id).ok())
            .unwrap_or_default();
        drop(index);
        // A folder page has no row of its own; its folder's row stands for it.
        let item = open.map(|id| match page_folder(&id) {
            Some(dir) if self.pages.get(dir) == Some(&id) => {
                TreeItem::new(format!("dir:{dir}"), "")
            }
            _ => TreeItem::new(note_key(&id), ""),
        });
        self.tree
            .update(cx, |t, cx| t.set_selected_item(item.as_ref(), cx));
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
                if this.overview_shown() {
                    this.load_overview(cx);
                }
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
                        for tab in &this.tabs {
                            tab.pane.update(cx, |p, cx| p.disk_changed(window, cx));
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

    /// Save the open notes and commit what is saved; no network on the way out.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.save_all(cx);
        if self.all_saved(cx) {
            self.sync.commit_now();
        }
    }

    fn follow(&mut self, link: Link, window: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.open_id(cx) else {
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
            Ok(Target::Missing(id)) => self.create(&id, false, window, cx),
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
        let Some(pane) = self.pane().cloned() else {
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
    fn create(&mut self, id: &str, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("# {}\n\n", new_note_title(id));
        match self.vault.create(id, &text) {
            Ok(entry) => {
                if let Err(e) = self.index.borrow_mut().update(&entry, &text) {
                    eprintln!("katib: {e}");
                }
                self.refresh(cx);
                self.open_in(&entry.id, new_tab, window, cx);
                // A new note is opened to be written: caret after the title.
                if let Some(pane) = self.pane() {
                    pane.update(cx, |p, cx| p.go_to(text.len(), window, cx));
                }
            }
            Err(e) => notify_error(window, cx, e.to_string()),
        }
    }

    fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.pane() {
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

    fn open_palette(&mut self, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_new_tab = new_tab;
        self.fill_palette("", None, cx);
        let placeholder = if new_tab {
            "Open in a new tab..."
        } else {
            "Open, search or run a command... (#tag for tags)"
        };
        self.show_palette(placeholder, "", window, cx);
    }

    /// Open the palette on `query`, as "Tags..." does with `#`.
    fn open_palette_at(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_new_tab = false;
        self.fill_palette(query, None, cx);
        self.show_palette(
            "Open, search or run a command... (#tag for tags)",
            query,
            window,
            cx,
        );
    }

    /// Ask which of several notes a link means.
    fn choose(&mut self, ids: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_new_tab = false;
        self.fill_palette("", Some(ids), cx);
        self.show_palette("This link matches several notes", "", window, cx);
    }

    fn show_palette(
        &mut self,
        placeholder: &'static str,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.palette.clone();
        state.update(cx, |s, cx| s.set_query(query, window, cx));
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
        let mut groups: Vec<(&str, Vec<CommandItem>, Vec<Pick>)> = Vec::new();
        let note = |text: String, detail: Option<String>, icon: IconName, pick: Pick| {
            (row(text, detail, icon), pick)
        };
        let push = |groups: &mut Vec<_>, label, rows: Vec<(CommandItem, Pick)>| {
            let (items, picks) = rows.into_iter().unzip();
            groups.push((label, items, picks));
        };
        if let Some(tag) = query.strip_prefix('#') {
            let tag = tag.trim();
            let key = tag.to_lowercase();
            let index = self.index.borrow();
            let tags = index
                .tags()
                .unwrap_or_default()
                .into_iter()
                .filter(|t| t.tag.to_lowercase().contains(&key) && t.tag.to_lowercase() != key)
                .map(|t| {
                    let notes = if t.count == 1 {
                        "1 note".into()
                    } else {
                        format!("{} notes", t.count)
                    };
                    note(
                        format!("#{}", t.tag),
                        Some(notes),
                        IconName::Star,
                        Pick::Query(format!("#{}", t.tag)),
                    )
                })
                .collect();
            let tagged = if tag.is_empty() {
                Vec::new()
            } else {
                index
                    .tagged(tag)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| {
                        note(
                            n.title,
                            Some(n.id.clone()),
                            IconName::File,
                            Pick::Open(n.id),
                        )
                    })
                    .collect()
            };
            drop(index);
            push(&mut groups, "Tagged", tagged);
            push(&mut groups, "Tags", tags);
        } else if let Some(ids) = choices {
            let rows = ids
                .iter()
                .map(|id| {
                    note(
                        title(id),
                        Some(id.clone()),
                        IconName::File,
                        Pick::Open(id.clone()),
                    )
                })
                .collect();
            push(&mut groups, "Which note?", rows);
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
                .collect();
            let exact = self
                .notes
                .iter()
                .any(|n| n.title.to_lowercase() == lower || n.id.to_lowercase() == lower);
            let listed: HashSet<_> = named.iter().map(|n| n.id.clone()).collect();
            let named = named
                .into_iter()
                .map(|n| {
                    note(
                        n.title.clone(),
                        Some(n.id.clone()),
                        IconName::File,
                        Pick::Open(n.id.clone()),
                    )
                })
                .collect();
            let hits = self.index.borrow().search(query, 30).unwrap_or_default();
            let found = hits
                .into_iter()
                .filter(|h| !listed.contains(&h.id))
                .map(|h| note(h.title, Some(h.snippet), IconName::Search, Pick::Open(h.id)))
                .collect();
            let commands = commands(!self.tabs.is_empty())
                .into_iter()
                .filter(|(label, ..)| lower.is_empty() || label.to_lowercase().contains(&lower))
                .map(|(label, icon, action)| {
                    (
                        command_row(label, icon, action.boxed_clone()),
                        Pick::Run(action),
                    )
                })
                .collect();
            push(&mut groups, "Notes", named);
            push(&mut groups, "Text", found);
            if !query.is_empty() && !exact {
                let create = note(
                    format!("New note: {query}"),
                    None,
                    IconName::Plus,
                    Pick::Create(query.to_owned()),
                );
                push(&mut groups, "Create", vec![create]);
            }
            push(&mut groups, "Commands", commands);
        }
        groups.retain(|(_, items, _)| !items.is_empty());
        self.picks = groups.iter().map(|(_, _, picks)| picks.clone()).collect();
        self.groups = groups
            .into_iter()
            .map(|(label, items, _)| CommandGroup::new().label(label).items(items))
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
        let new_tab = std::mem::take(&mut self.palette_new_tab);
        match pick {
            Pick::Open(id) => self.open_in(&id, new_tab, window, cx),
            Pick::Create(id) => self.create(&id, new_tab, window, cx),
            Pick::Query(query) => self.open_palette_at(&query, window, cx),
            Pick::Run(action) => {
                // The palette had focus; its dialog is gone, so the window's
                // own handle takes the action to the handlers below.
                window.focus(&self.focus, cx);
                window.dispatch_action(action, cx);
            }
        }
    }

    /// Every action's handler, on the window's root element.
    fn on_actions(&self, root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(cx.listener(|this, _: &OpenPalette, w, cx| this.open_palette(false, w, cx)))
            .on_action(cx.listener(|this, _: &NewTab, w, cx| this.open_palette(true, w, cx)))
            .on_action(cx.listener(|this, _: &ToggleMode, w, cx| this.toggle_mode(w, cx)))
            .on_action(cx.listener(|this, _: &Save, _, cx| this.save_open(cx)))
            .on_action(cx.listener(|this, _: &NewNote, w, cx| {
                this.prompt_name(manage::Naming::Create, w, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseTab, w, cx| this.close_tab(this.active, w, cx)))
            .on_action(cx.listener(|this, _: &NextTab, w, cx| this.cycle_tab(1, w, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, w, cx| this.cycle_tab(-1, w, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, w, cx| this.open_settings(w, cx)))
            .on_action(cx.listener(|this, _: &OpenToday, w, cx| this.open_today(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleBacklinks, _, cx| {
                this.backlinks_open = !this.backlinks_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SyncNow, w, cx| this.sync_now(w, cx)))
            .on_action(cx.listener(|this, _: &AttachFile, w, cx| this.pick_attachments(w, cx)))
            .on_action(cx.listener(|this, _: &RenameNote, w, cx| {
                if let Some(id) = this.open_id(cx) {
                    this.prompt_name(manage::Naming::Move(id), w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DeleteNote, w, cx| {
                if let Some(id) = this.open_id(cx) {
                    this.confirm_delete(id, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PruneAttachments, w, cx| this.confirm_prune(w, cx)))
            .on_action(cx.listener(|this, _: &GoBack, w, cx| this.go(true, w, cx)))
            .on_action(cx.listener(|this, _: &GoForward, w, cx| this.go(false, w, cx)))
            .on_action(cx.listener(|this, _: &BrokenLinks, w, cx| this.open_broken(w, cx)))
            .on_action(cx.listener(|this, _: &OpenTasks, w, cx| this.open_tasks(w, cx)))
            .on_action(cx.listener(|this, _: &ShowOverview, w, cx| this.show_overview(w, cx)))
            .on_action(cx.listener(|this, _: &BrowseTags, w, cx| this.open_palette_at("#", w, cx)))
            .on_action(cx.listener(|this, _: &EditTags, w, cx| {
                if let Some(id) = this.open_id(cx) {
                    this.prompt_name(manage::Naming::Tags(id), w, cx);
                }
            }))
    }
}

/// A palette row: an icon, a label, and a second muted line when there is detail.
fn row(text: String, detail: Option<String>, icon: IconName) -> CommandItem {
    let item = CommandItem::new().label(text.clone()).icon(icon.clone());
    match detail {
        None => item,
        Some(detail) => item.child(move |_, cx| {
            h_flex()
                .w_full()
                .gap_2()
                .child(Icon::new(icon.clone()).text_color(cx.theme().muted_foreground))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .py_1()
                        .child(div().text_sm().child(text.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(detail.clone()),
                        ),
                )
        }),
    }
}

/// A palette command row with the key binding of its action, if any.
fn command_row(label: &'static str, icon: IconName, action: Box<dyn Action>) -> CommandItem {
    CommandItem::new().label(label).child(move |window, cx| {
        h_flex()
            .w_full()
            .gap_2()
            .child(Icon::new(icon.clone()).text_color(cx.theme().muted_foreground))
            .child(div().flex_1().text_sm().child(label))
            .children(Kbd::binding_for_action(
                action.as_ref(),
                Some(CONTEXT),
                window,
            ))
    })
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

/// The tree keys of every folder holding a note.
fn folder_keys(notes: &[NoteRow]) -> HashSet<String> {
    let mut keys = HashSet::new();
    for note in notes {
        let mut end = 0;
        while let Some(i) = note.id[end..].find('/') {
            end += i;
            keys.insert(format!("dir:{}", &note.id[..end]));
            end += 1;
        }
    }
    keys
}

/// Follow folder `from` to `to` in the set of expanded folder keys.
fn move_expanded(expanded: &mut HashSet<String>, from: &str, to: &str) {
    let (from, to) = (format!("dir:{from}"), format!("dir:{to}"));
    let moved: Vec<String> = expanded
        .iter()
        .filter(|k| **k == from || k.starts_with(&format!("{from}/")))
        .cloned()
        .collect();
    for key in moved {
        expanded.remove(&key);
        expanded.insert(format!("{to}{}", &key[from.len()..]));
    }
}

/// The heading a new note `id` starts with: its file name, or for a folder
/// page, its folder's name.
fn new_note_title(id: &str) -> &str {
    let name = page_folder(id).unwrap_or(id);
    name.rsplit('/').next().unwrap_or(name).trim()
}

/// Each folder's page (`dir/README`, any case) by folder path. A folder with
/// two pages differing in case has none, as `[[dir/]]` is ambiguous there.
fn folder_pages(notes: &[NoteRow]) -> HashMap<String, String> {
    let mut pages: HashMap<String, Option<String>> = HashMap::new();
    for note in notes {
        if let Some(dir) = page_folder(&note.id) {
            pages
                .entry(dir.to_owned())
                .and_modify(|p| *p = None)
                .or_insert(Some(note.id.clone()));
        }
    }
    pages
        .into_iter()
        .filter_map(|(dir, page)| Some((dir, page?)))
        .collect()
}

/// The vault as folders and notes. Folders come first, then notes by title.
/// Keys carry a prefix, since `a.md` and the folder `a/` can both exist. A
/// folder's page is left out: the folder's row opens it.
fn build_tree(
    notes: &[NoteRow],
    pages: &HashMap<String, String>,
    expanded: &HashSet<String>,
) -> Vec<TreeItem> {
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
        if page_folder(&note.id).is_some_and(|d| pages.get(d) == Some(&note.id)) {
            // The folder still needs its row.
            let mut dir = &mut root;
            for part in note.id.split('/').take(note.id.split('/').count() - 1) {
                dir = dir.dirs.entry(part).or_default();
            }
            continue;
        }
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

#[cfg(test)]
mod tests {
    // Not a glob: `gpui_kit::*` would bring GPUI's own `test` macro in.
    use super::{
        HashMap, HashSet, NoteRow, TreeItem, build_tree, folder_keys, folder_pages, move_expanded,
        new_note_title,
    };

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
        let items = build_tree(&notes, &HashMap::new(), &expanded);
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

    #[test]
    fn expansion_follows_a_moved_folder_and_forgets_a_gone_one() {
        let mut expanded =
            HashSet::from(["dir:p", "dir:p/q", "dir:pp", "dir:x"].map(str::to_owned));
        move_expanded(&mut expanded, "p", "w/p");
        let mut keys: Vec<_> = expanded.iter().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["dir:pp", "dir:w/p", "dir:w/p/q", "dir:x"]);
        let dirs = folder_keys(&[row("w/p/q/n", "N"), row("top", "T")]);
        let mut dirs: Vec<_> = dirs.into_iter().collect();
        dirs.sort();
        assert_eq!(dirs, ["dir:w", "dir:w/p", "dir:w/p/q"]);
    }

    #[test]
    fn a_folder_page_hides_behind_its_folder_row() {
        let notes = [
            row("p/README", "Projects"),
            row("p/a", "A"),
            row("q/readme", "Q"),
            row("r/README", "R1"),
            row("r/readme", "R2"),
            row("README", "Root"),
        ];
        let pages = folder_pages(&notes);
        let mut listed: Vec<_> = pages.iter().map(|(d, p)| format!("{d}={p}")).collect();
        listed.sort();
        // `r` has two pages differing in case, so neither is its page.
        assert_eq!(listed, ["p=p/README", "q=q/readme"]);
        let items = build_tree(&notes, &pages, &HashSet::new());
        assert_eq!(
            shape(&items),
            [
                "dir:p [note:p/a=A]",
                "dir:q=q",
                "dir:r [note:r/README=R1, note:r/readme=R2]",
                "note:README=Root",
            ]
        );
    }

    #[test]
    fn a_new_folder_page_is_titled_by_its_folder() {
        assert_eq!(new_note_title("journal/2026-11-25"), "2026-11-25");
        assert_eq!(new_note_title("a/projects/README"), "projects");
        assert_eq!(new_note_title("README"), "README");
    }
}
