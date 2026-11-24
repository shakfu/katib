//! One open note: its two editors, autosave, draft recovery and conflicts.
//! The write rules are design.md section 4.4.

use std::{
    cell::RefCell,
    fs, io,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, InputEvent, Position},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_core::{Entry, Index, Link, SaveError, Vault, attach, save_guarded};
use katib_editor::{EditorEvent, LiveEditor};

/// Save after this long without typing.
const IDLE: Duration = Duration::from_secs(1);
/// Save at least this often while typing continues.
const MAX_UNSAVED: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Live,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Saved,
    Unsaved,
    /// The file changed on disk (or was deleted) while this buffer had edits.
    /// Autosave stops until the user picks a side.
    Conflict {
        disk: Option<String>,
    },
    Failed(String),
}

pub enum PaneEvent {
    Saved,
    OpenLink(Link),
    Close,
    /// Something failed that the user should hear about.
    Error(String),
}

pub struct NotePane {
    vault: Vault,
    index: Rc<RefCell<Index>>,
    pub entry: Entry,
    /// The file's text as katib last read or wrote it; `None` once it is gone.
    base: Option<String>,
    live: Entity<LiveEditor>,
    raw: Entity<EditorState>,
    mode: Mode,
    state: State,
    /// Unsaved text from an earlier session that differs from the file.
    draft: Option<String>,
    dirty_since: Option<Instant>,
    last_change: Instant,
    save_scheduled: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaneEvent> for NotePane {}

impl NotePane {
    /// A pane over `entry`, whose file holds `text`.
    pub fn new(
        vault: Vault,
        index: Rc<RefCell<Index>>,
        entry: Entry,
        text: String,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = vault.read_draft(&entry).filter(|d| *d != text);
        let live = cx.new(|cx| {
            let mut editor = LiveEditor::new(text.clone(), window, cx);
            editor.set_note(vault.root().to_owned(), entry.id.clone(), cx);
            editor.set_completer(crate::complete::completer(index.clone()));
            editor
        });
        let raw = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("markdown")
                .line_number(false)
                .soft_wrap(true)
                .default_value(text.clone());
            state.lsp_mut().completion_provider =
                Some(Rc::new(crate::complete::WikiCompletions(index.clone())));
            state
        });
        let subscriptions =
            vec![
                cx.subscribe_in(&live, window, |this, _, event: &EditorEvent, window, cx| {
                    match event {
                        EditorEvent::Changed => this.changed(cx),
                        EditorEvent::OpenLink(link) => cx.emit(PaneEvent::OpenLink(link.clone())),
                        EditorEvent::PasteImage(bytes, ext) => {
                            this.attach_bytes(bytes.clone(), format!("pasted.{ext}"), window, cx)
                        }
                        EditorEvent::PasteFiles(paths) => this.attach_files(paths, window, cx),
                    }
                }),
                cx.subscribe(&raw, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.changed(cx);
                    }
                }),
            ];
        Self {
            vault,
            index,
            entry,
            base: Some(text),
            live,
            raw,
            mode,
            state: State::Saved,
            draft,
            dirty_since: None,
            last_change: Instant::now(),
            save_scheduled: false,
            _subscriptions: subscriptions,
        }
    }

    /// Whether the file holds the buffer, so a commit would not catch a half-saved edit.
    pub fn is_saved(&self) -> bool {
        self.dirty_since.is_none() && self.state == State::Saved
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        match self.mode {
            Mode::Live => window.focus(&self.live.focus_handle(cx), cx),
            Mode::Raw => window.focus(&self.raw.focus_handle(cx), cx),
        }
    }

    pub fn text(&self, cx: &App) -> String {
        match self.mode {
            Mode::Live => self.live.read(cx).text().to_owned(),
            Mode::Raw => self.raw.read(cx).value().to_string(),
        }
    }

    /// Words in the buffer, for the status bar.
    pub fn words(&self, cx: &App) -> usize {
        match self.mode {
            Mode::Live => self.live.read(cx).text().split_whitespace().count(),
            Mode::Raw => self.raw.read(cx).value().split_whitespace().count(),
        }
    }

    /// The caret's byte offset.
    pub fn head(&self, cx: &App) -> usize {
        match self.mode {
            Mode::Live => self.live.read(cx).head(),
            Mode::Raw => self.raw.read(cx).cursor(),
        }
    }

    /// One line for the status bar.
    pub fn status(&self) -> String {
        match &self.state {
            State::Saved => "saved".into(),
            State::Unsaved => "unsaved".into(),
            State::Conflict { disk: Some(_) } => "changed on disk; not saving".into(),
            State::Conflict { disk: None } => "deleted on disk; not saving".into(),
            State::Failed(e) => format!("save failed: {e}"),
        }
    }

    /// Replace the buffer without marking it edited, keeping the caret near where it was.
    fn set_buffer(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let head = self.head(cx).min(text.len());
        match self.mode {
            Mode::Live => self.live.update(cx, |e, cx| e.set_text(text, head, cx)),
            Mode::Raw => self.raw.update(cx, |s, cx| {
                s.set_value(text.to_owned(), window, cx);
                s.set_selected_range(head..head, cx);
            }),
        }
    }

    pub fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let head = self.head(cx);
        // Each mode keeps its undo history; text changed in the other mode
        // arrives as one undoable step.
        self.mode = match self.mode {
            Mode::Live => {
                self.raw.update(cx, |s, cx| {
                    if s.value() != text {
                        s.replace_all(text, window, cx);
                    }
                    s.set_selected_range(head..head, cx);
                });
                Mode::Raw
            }
            Mode::Raw => {
                self.live.update(cx, |e, cx| e.replace_all(&text, head, cx));
                Mode::Live
            }
        };
        self.focus(window, cx);
        cx.notify();
    }

    /// Insert `text` at the caret as one undo step.
    fn insert(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Live => self.live.update(cx, |e, cx| e.insert(text, window, cx)),
            Mode::Raw => self
                .raw
                .update(cx, |s, cx| s.insert(text.to_owned(), window, cx)),
        }
    }

    /// Store files as attachments and link them at the caret, one per line.
    /// Reading and hashing run in the background; a file can be 50 MB.
    pub fn attach_files(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let (vault, id) = (self.vault.clone(), self.entry.id.clone());
        let paths: Vec<_> = paths.iter().filter(|p| p.is_file()).cloned().collect();
        self.attach_in_background(window, cx, move || {
            paths
                .iter()
                .map(|path| {
                    let name = path
                        .file_name()
                        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
                    attach::store_file(&vault, path)
                        .map(|rel| attach::link_markdown(&id, &rel, &name))
                        .map_err(|e| format!("{name}: {e}"))
                })
                .collect()
        });
    }

    fn attach_bytes(
        &mut self,
        bytes: Vec<u8>,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (vault, id) = (self.vault.clone(), self.entry.id.clone());
        self.attach_in_background(window, cx, move || {
            vec![
                attach::store(&vault, &bytes, &name)
                    .map(|rel| attach::link_markdown(&id, &rel, &name))
                    .map_err(|e| e.to_string()),
            ]
        });
    }

    /// Run `store` off the main thread, then insert the links it made at the caret.
    fn attach_in_background(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        store: impl FnOnce() -> Vec<Result<String, String>> + Send + 'static,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let results = cx.background_spawn(async move { store() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let mut links = Vec::new();
                for result in results {
                    match result {
                        Ok(link) => links.push(link),
                        Err(e) => cx.emit(PaneEvent::Error(e)),
                    }
                }
                if !links.is_empty() {
                    this.insert(&links.join("\n"), window, cx);
                }
            });
        })
        .detach();
    }

    /// Move the caret to byte `offset`, scroll to it, and focus the editor.
    pub fn go_to(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.place_caret(offset, window, cx);
        self.focus(window, cx);
    }

    /// Move the caret to byte `offset` and scroll to it, without focusing.
    pub fn place_caret(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let mut offset = offset.min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        match self.mode {
            Mode::Live => self.live.update(cx, |e, cx| e.move_caret(offset, cx)),
            Mode::Raw => {
                // `set_cursor_position` scrolls. Its column counts characters.
                let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
                let line = text[..offset].matches('\n').count() as u32;
                let column = text[line_start..offset].chars().count() as u32;
                self.raw.update(cx, |s, cx| {
                    s.set_cursor_position(Position::new(line, column), window, cx)
                });
            }
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.last_change = now;
        self.dirty_since.get_or_insert(now);
        if matches!(self.state, State::Saved | State::Failed(_)) {
            self.state = State::Unsaved;
        }
        if !self.save_scheduled {
            self.save_scheduled = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    let done = this.update(cx, |this, cx| {
                        let due = this.last_change.elapsed() >= IDLE
                            || this.dirty_since.is_some_and(|t| t.elapsed() >= MAX_UNSAVED);
                        if due {
                            this.save_scheduled = false;
                            this.save(cx);
                        }
                        due
                    });
                    if done.unwrap_or(true) {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }

    /// Save if there are edits and no unresolved conflict. Returns whether the
    /// file now holds the buffer.
    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dirty_since.is_none() {
            return true;
        }
        if matches!(self.state, State::Conflict { .. }) {
            return false;
        }
        let base = self.base.clone();
        self.write(base.as_deref(), cx)
    }

    /// Write the buffer over a file expected to hold `expected`.
    fn write(&mut self, expected: Option<&str>, cx: &mut Context<Self>) -> bool {
        let text = self.text(cx);
        let ok = match save_guarded(&self.entry.path, expected, &text) {
            Ok(()) => {
                self.vault.remove_draft(&self.entry);
                if let Err(e) = self.index.borrow_mut().update(&self.entry, &text) {
                    eprintln!("katib: {e}");
                }
                self.base = Some(text);
                self.dirty_since = None;
                self.state = State::Saved;
                cx.emit(PaneEvent::Saved);
                true
            }
            Err(e) => {
                // Keep the edits on disk somewhere before reporting.
                let kept = self.vault.write_draft(&self.entry, &text);
                self.state = match (e, kept) {
                    (SaveError::Conflict { disk }, Ok(())) => State::Conflict { disk },
                    (SaveError::Conflict { .. }, Err(d)) => State::Failed(format!(
                        "the file changed on disk, and the draft could not be kept: {d}"
                    )),
                    (e @ SaveError::Io(..), _) => State::Failed(e.to_string()),
                };
                false
            }
        };
        cx.notify();
        ok
    }

    /// React to a change on disk: reload a clean buffer, mark an edited one.
    pub fn disk_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let disk = match fs::read_to_string(&self.entry.path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return,
        };
        if disk == self.base || matches!(&self.state, State::Conflict { disk: d } if *d == disk) {
            return;
        }
        match (&disk, self.dirty_since) {
            (Some(text), None) => {
                self.set_buffer(text, window, cx);
                self.base = disk;
                self.state = State::Saved;
            }
            _ => {
                if self.dirty_since.is_none() {
                    // Deleted while clean: nothing to lose, but say so.
                    self.dirty_since = Some(Instant::now());
                }
                let _ = self.vault.write_draft(&self.entry, &self.text(cx));
                self.state = State::Conflict { disk };
            }
        }
        cx.notify();
    }

    fn keep_mine(&mut self, cx: &mut Context<Self>) {
        if let State::Conflict { disk } = &self.state {
            let disk = disk.clone();
            self.write(disk.as_deref(), cx);
        }
    }

    fn take_theirs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let State::Conflict { disk } = self.state.clone() else {
            return;
        };
        match disk {
            Some(text) => {
                self.set_buffer(&text, window, cx);
                self.base = Some(text);
                self.dirty_since = None;
                self.state = State::Saved;
                self.vault.remove_draft(&self.entry);
                cx.notify();
            }
            // Deleted on disk; the draft keeps this buffer.
            None => cx.emit(PaneEvent::Close),
        }
    }

    fn recover_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft.take() {
            self.set_buffer(&draft, window, cx);
            self.changed(cx);
        }
    }

    fn discard_draft(&mut self, cx: &mut Context<Self>) {
        self.draft = None;
        self.vault.remove_draft(&self.entry);
        cx.notify();
    }

    fn banner(&self, message: String, buttons: Vec<Button>, cx: &App) -> Div {
        let theme = cx.theme();
        h_flex()
            .gap_2()
            .px_4()
            .py_2()
            .bg(theme.warning.opacity(0.15))
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex_1().text_sm().child(message))
            .children(buttons)
    }
}

impl Render for NotePane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let draft_banner = self.draft.is_some().then(|| {
            self.banner(
                "This note has unsaved changes from an earlier session.".into(),
                vec![
                    Button::new("recover")
                        .primary()
                        .small()
                        .label("Recover them")
                        .on_click(cx.listener(|this, _, w, cx| this.recover_draft(w, cx))),
                    Button::new("discard")
                        .small()
                        .label("Discard them")
                        .on_click(cx.listener(|this, _, _, cx| this.discard_draft(cx))),
                ],
                cx,
            )
        });
        let conflict_banner = match &self.state {
            State::Conflict { disk } => Some(self.banner(
                match disk {
                    Some(_) => "The file changed on disk while you were editing. Your text is kept as a draft.",
                    None => "The file was deleted on disk while you were editing. Your text is kept as a draft.",
                }
                .into(),
                vec![
                    Button::new("mine")
                        .primary()
                        .small()
                        .label(if disk.is_some() { "Keep mine" } else { "Recreate it" })
                        .on_click(cx.listener(|this, _, _, cx| this.keep_mine(cx))),
                    Button::new("theirs")
                        .small()
                        .label(if disk.is_some() { "Take the file's" } else { "Close it" })
                        .on_click(cx.listener(|this, _, w, cx| this.take_theirs(w, cx))),
                ],
                cx,
            )),
            _ => None,
        };
        v_flex()
            .size_full()
            .children(draft_banner)
            .children(conflict_banner)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    // Files dropped on the note become attachments.
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        this.attach_files(paths.paths(), window, cx);
                    }))
                    .when(self.mode == Mode::Live, |d| d.child(self.live.clone()))
                    .when(self.mode == Mode::Raw, |d| {
                        d.p_4().child(Editor::new(&self.raw).h_full())
                    }),
            )
    }
}
