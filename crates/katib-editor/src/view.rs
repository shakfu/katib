//! The live preview view: renders a [`Projection`] through gpui-kit's
//! `editor_host`, and draws the caret and selection from the leaf geometry.

use std::{
    collections::HashMap,
    ops::Range,
    time::{Duration, Instant},
};

use gpui_kit::{
    base::text::editor_host::{FlowPosition, LeafGeometry, RenderContext, collect_leaf_geometry},
    component::{ActiveTheme as _, v_flex},
    *,
};

use katib_core::Link;

use crate::{Document, Projection, project};

const CONTEXT: &str = "KatibEditor";

actions!(
    katib_editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        Home,
        End,
        Enter,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
    ]
);

/// What the editor reports to its owner.
#[derive(Clone, Debug)]
pub enum EditorEvent {
    /// The text changed.
    Changed,
    /// A link was cmd-clicked (ctrl-clicked off macOS).
    OpenLink(Link),
}

/// Edits further apart than this are separate undo steps.
const UNDO_PAUSE: Duration = Duration::from_secs(1);

/// Bind the editor's keys. Call once, after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    let [line_start, line_end, all, copy, cut, paste, undo, redo] = [
        "cmd-left",
        "cmd-right",
        "cmd-a",
        "cmd-c",
        "cmd-x",
        "cmd-v",
        "cmd-z",
        "cmd-shift-z",
    ];
    #[cfg(not(target_os = "macos"))]
    let [line_start, line_end, all, copy, cut, paste, undo, redo] = [
        "home",
        "end",
        "ctrl-a",
        "ctrl-c",
        "ctrl-x",
        "ctrl-v",
        "ctrl-z",
        "ctrl-shift-z",
    ];
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new(line_start, Home, c),
        KeyBinding::new(line_end, End, c),
        KeyBinding::new("enter", Enter, c),
        KeyBinding::new(all, SelectAll, c),
        KeyBinding::new(copy, Copy, c),
        KeyBinding::new(cut, Cut, c),
        KeyBinding::new(paste, Paste, c),
        KeyBinding::new(undo, Undo, c),
        KeyBinding::new(redo, Redo, c),
    ]);
}

#[derive(Default)]
struct Timing {
    project: Duration,
    frame: Duration,
    worst_frame: Duration,
    started: Option<Instant>,
}

pub struct LiveEditor {
    focus: FocusHandle,
    doc: Document,
    projection: Projection,
    /// From the last prepaint; window coordinates.
    geometry: HashMap<u64, LeafGeometry>,
    /// The x a vertical move aims for, kept across consecutive vertical moves.
    goal_x: Option<Pixels>,
    selecting: bool,
    context: RenderContext,
    timing: Timing,
    scroll: ScrollHandle,
    /// Scroll the caret into view on the next frame.
    autoscroll: bool,
    last_edit: Option<Instant>,
    _subscriptions: Vec<Subscription>,
}

impl LiveEditor {
    pub fn new(text: impl Into<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        // Redraw on focus changes: the caret's block shows its markdown only while focused.
        let subscriptions = vec![
            cx.on_focus(&focus, window, |_, _, cx| cx.notify()),
            cx.on_blur(&focus, window, |_, _, cx| cx.notify()),
        ];
        Self {
            focus,
            _subscriptions: subscriptions,
            doc: Document::new(text),
            projection: Projection::default(),
            geometry: HashMap::new(),
            goal_x: None,
            selecting: false,
            // Clicking a link in the editor places the caret; it does not open the link.
            context: RenderContext::from_app(cx).on_link_click(|_, _, _, _| {}),
            timing: Timing::default(),
            scroll: ScrollHandle::new(),
            autoscroll: true,
            last_edit: None,
        }
    }

    pub fn text(&self) -> &str {
        self.doc.text()
    }

    /// The caret's byte offset.
    pub fn head(&self) -> usize {
        self.doc.head()
    }

    /// Replace the text (a reload or a mode switch); clears undo history.
    pub fn set_text(&mut self, text: impl Into<String>, head: usize, cx: &mut Context<Self>) {
        self.doc.set_text(text, head);
        self.autoscroll = true;
        cx.notify();
    }

    /// Replace the text as one undoable edit (a switch back from raw mode).
    pub fn replace_all(&mut self, text: &str, head: usize, cx: &mut Context<Self>) {
        self.doc.replace_all(text, head);
        self.autoscroll = true;
        cx.notify();
    }

    /// Put the caret at `offset` and scroll to it.
    pub fn move_caret(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.doc.move_to(offset, false);
        self.doc.seal();
        self.goal_x = None;
        self.autoscroll = true;
        cx.notify();
    }

    /// Frame timing, for the spike window's status line.
    pub fn stats(&self) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1000.;
        format!(
            "{} bytes, {} leaves | project {:.2} ms | frame {:.2} ms, worst {:.2} ms",
            self.doc.text().len(),
            self.projection.leaves.len(),
            ms(self.timing.project),
            ms(self.timing.frame),
            ms(self.timing.worst_frame),
        )
    }

    /// Run one edit. A pause since the last edit starts a new undo step.
    fn edit(&mut self, f: impl FnOnce(&mut Document), window: &mut Window, cx: &mut Context<Self>) {
        if self.last_edit.is_none_or(|t| t.elapsed() > UNDO_PAUSE) {
            self.doc.seal();
        }
        f(&mut self.doc);
        self.last_edit = Some(Instant::now());
        self.changed(window, cx);
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.autoscroll = true;
        window.notify_text_input_changed(&self.focus);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn moved(&mut self, offset: usize, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_to(offset, extend);
        self.doc.seal();
        self.autoscroll = true;
        window.notify_text_selection_changed(&self.focus);
        cx.notify();
    }

    fn horizontal(
        &mut self,
        forward: bool,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goal_x = None;
        let sel = self.doc.selection();
        let to = match (forward, extend || sel.is_empty()) {
            (true, true) => self.doc.next_boundary(self.doc.head()),
            (false, true) => self.doc.prev_boundary(self.doc.head()),
            (true, false) => sel.end,
            (false, false) => sel.start,
        };
        self.moved(to, extend, window, cx);
    }

    /// The leaf drawing the caret's byte, and the caret's index in it.
    fn caret_leaf(&self, offset: usize) -> Option<(usize, &LeafGeometry, usize)> {
        self.projection
            .leaves
            .iter()
            .enumerate()
            .find_map(|(ix, leaf)| {
                let i = leaf.to_leaf(offset)?;
                Some((ix, self.geometry.get(&leaf.key)?, i))
            })
    }

    /// Drawn leaves in document order.
    fn stops(&self) -> Vec<usize> {
        let mut stops: Vec<_> = (0..self.projection.leaves.len())
            .filter(|&ix| self.geometry.contains_key(&self.projection.leaves[ix].key))
            .collect();
        stops.sort_by_key(|&ix| self.projection.leaves[ix].source.start);
        stops
    }

    /// Move one visual line, using only the last frame's leaf geometry.
    fn vertical(&mut self, down: bool, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let head = self.doc.head();
        let Some((leaf_ix, geo, i)) = self.caret_leaf(head).map(|(ix, g, i)| (ix, g.clone(), i))
        else {
            return self.moved(
                if down { self.doc.text().len() } else { 0 },
                extend,
                window,
                cx,
            );
        };
        let pos = geo.position_for_index(i);
        let x = *self.goal_x.get_or_insert(pos.x);
        let lh = geo.line_height();
        let y = if down {
            pos.y + lh * 1.5
        } else {
            pos.y - lh * 0.5
        };
        let bounds = geo.bounds();
        let leaf = &self.projection.leaves[leaf_ix];
        let to = if y >= bounds.top() && y < bounds.bottom() {
            leaf.to_source(geo.index_for_position(point(x, y)))
        } else {
            let stops = self.stops();
            let at = stops.iter().position(|&ix| ix == leaf_ix);
            let next = at.and_then(|at| {
                if down {
                    stops.get(at + 1)
                } else {
                    at.checked_sub(1).and_then(|p| stops.get(p))
                }
            });
            match next {
                Some(ix) => {
                    let leaf = &self.projection.leaves[*ix];
                    let geo = &self.geometry[&leaf.key];
                    let b = geo.bounds();
                    let half = geo.line_height() * 0.5;
                    let y = if down {
                        b.top() + half
                    } else {
                        b.bottom() - half
                    };
                    leaf.to_source(geo.index_for_position(point(x, y)))
                }
                None if down => self.doc.text().len(),
                None => 0,
            }
        };
        self.moved(to, extend, window, cx);
    }

    /// The source offset under a window point: the nearest leaf, then the index in it.
    fn source_at(&self, p: Point<Pixels>) -> Option<usize> {
        let distance = |b: Bounds<Pixels>| {
            let dy = f32::from((b.top() - p.y).max(p.y - b.bottom()).max(px(0.)));
            let dx = f32::from((b.left() - p.x).max(p.x - b.right()).max(px(0.)));
            dy * 1000. + dx
        };
        let (key, geo) = self
            .geometry
            .iter()
            .min_by(|a, b| distance(a.1.bounds()).total_cmp(&distance(b.1.bounds())))?;
        Some(
            self.projection
                .leaf(*key)?
                .to_source(geo.index_for_position(p)),
        )
    }

    fn decorations(&self, cx: &App) -> HashMap<u64, Vec<(Range<usize>, Hsla)>> {
        let mut out: HashMap<u64, Vec<_>> = HashMap::new();
        let sel = self.doc.selection();
        let marked = self.doc.marked();
        let theme = cx.theme();
        for leaf in &self.projection.leaves {
            let ranges = out.entry(leaf.key).or_default();
            if !sel.is_empty() {
                ranges.extend(
                    leaf.leaf_ranges(&sel)
                        .into_iter()
                        .map(|r| (r, theme.selection)),
                );
            }
            if let Some(m) = &marked {
                ranges.extend(leaf.leaf_ranges(m).into_iter().map(|r| (r, theme.accent)));
            }
        }
        out.retain(|_, r| !r.is_empty());
        out
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if e.modifiers.secondary(cx)
            && let Some(offset) = self.source_at(e.position)
            && let Some(link) = katib_core::parse(self.doc.text())
                .links
                .into_iter()
                .find(|l| l.range.start <= offset && offset < l.range.end)
        {
            cx.emit(EditorEvent::OpenLink(link));
            return;
        }
        self.selecting = true;
        if let Some(offset) = self.source_at(e.position) {
            self.goal_x = None;
            self.moved(offset, e.modifiers.shift, window, cx);
        }
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.selecting
            && let Some(offset) = self.source_at(e.position)
        {
            self.moved(offset, true, window, cx);
        }
    }
}

impl EventEmitter<EditorEvent> for LiveEditor {}

impl Focusable for LiveEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LiveEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let started = Instant::now();
        // Unfocused, there is no caret to edit at, so no block shows its markdown.
        let head = self.focus.is_focused(window).then(|| self.doc.head());
        self.projection = project(self.doc.text(), head);
        self.timing.project = started.elapsed();
        self.timing.started = Some(started);

        let segments: Vec<AnyElement> = self
            .projection
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.blocks.is_empty())
            .map(|(ix, seg)| {
                v_flex()
                    .children(seg.blocks.iter().enumerate().map(|(bix, block)| {
                        let id = ("katib-block", ((ix as u64) << 20) | bix as u64);
                        block.render_with_id(
                            id,
                            FlowPosition::at(&[block], 0, true),
                            &self.context,
                            window,
                            cx,
                        )
                    }))
                    .into_any_element()
            })
            .collect();
        let decorations = self.decorations(cx);
        let theme = cx.theme();

        v_flex().size_full().bg(theme.background).child(
            div()
                .id("katib-editor")
                .key_context(CONTEXT)
                .track_focus(&self.focus)
                .cursor(CursorStyle::IBeam)
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .on_action(
                    cx.listener(|this, _: &Backspace, w, cx| this.edit(Document::backspace, w, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &Delete, w, cx| this.edit(Document::delete, w, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &Enter, w, cx| {
                        this.edit(|d| d.replace(None, "\n"), w, cx)
                    }),
                )
                .on_action(cx.listener(|this, _: &Undo, w, cx| {
                    if this.doc.undo() {
                        this.changed(w, cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &Redo, w, cx| {
                    if this.doc.redo() {
                        this.changed(w, cx);
                    }
                }))
                .on_action(
                    cx.listener(|this, _: &Left, w, cx| this.horizontal(false, false, w, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &Right, w, cx| this.horizontal(true, false, w, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &SelectLeft, w, cx| this.horizontal(false, true, w, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &SelectRight, w, cx| this.horizontal(true, true, w, cx)),
                )
                .on_action(cx.listener(|this, _: &Up, w, cx| this.vertical(false, false, w, cx)))
                .on_action(cx.listener(|this, _: &Down, w, cx| this.vertical(true, false, w, cx)))
                .on_action(
                    cx.listener(|this, _: &SelectUp, w, cx| this.vertical(false, true, w, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &SelectDown, w, cx| this.vertical(true, true, w, cx)),
                )
                .on_action(cx.listener(|this, _: &Home, w, cx| {
                    let to = this.doc.line_start(this.doc.head());
                    this.goal_x = None;
                    this.moved(to, false, w, cx);
                }))
                .on_action(cx.listener(|this, _: &End, w, cx| {
                    let to = this.doc.line_end(this.doc.head());
                    this.goal_x = None;
                    this.moved(to, false, w, cx);
                }))
                .on_action(cx.listener(|this, _: &SelectAll, w, cx| {
                    this.doc.select(0..this.doc.text().len(), false);
                    w.notify_text_selection_changed(&this.focus);
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &Copy, _, cx| {
                    let sel = this.doc.selection();
                    if !sel.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            this.doc.text()[sel].to_owned(),
                        ));
                    }
                }))
                .on_action(cx.listener(|this, _: &Cut, w, cx| {
                    let sel = this.doc.selection();
                    if !sel.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            this.doc.text()[sel.clone()].to_owned(),
                        ));
                        this.doc.seal();
                        this.edit(|d| d.replace(Some(sel), ""), w, cx);
                        this.doc.seal();
                    }
                }))
                .on_action(cx.listener(|this, _: &Paste, w, cx| {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        this.doc.seal();
                        this.edit(|d| d.replace(None, &text), w, cx);
                        this.doc.seal();
                    }
                }))
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                .on_mouse_move(cx.listener(Self::on_mouse_move))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.selecting = false),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.selecting = false),
                )
                .child(EditorElement {
                    editor: cx.entity(),
                    decorations,
                    child: v_flex()
                        .w_full()
                        .px_8()
                        .py_6()
                        .children(segments)
                        .into_any_element(),
                }),
        )
    }
}

/// Lays out the rendered blocks, collecting each leaf's geometry during prepaint.
/// The top of a caret at leaf byte `i`. `position_for_index` gives the line-box top,
/// but GPUI centres glyphs in the box, so half the leading sits above them.
fn caret_top(geo: &LeafGeometry, i: usize) -> Point<Pixels> {
    let at = geo.position_for_index(i);
    point(at.x, at.y + (geo.line_height() - geo.caret_height()) / 2.)
}

/// Scroll so `caret` sits at least one line inside the viewport. The new
/// offset applies from the next frame, which this requests.
fn scroll_into_view(
    scroll: &ScrollHandle,
    caret: Bounds<Pixels>,
    margin: Pixels,
    window: &mut Window,
) {
    let view = scroll.bounds();
    if view.size.height <= px(0.) {
        return;
    }
    let mut offset = scroll.offset();
    let before = offset.y;
    if caret.top() - margin < view.top() {
        offset.y += view.top() - (caret.top() - margin);
    } else if caret.bottom() + margin > view.bottom() {
        offset.y -= caret.bottom() + margin - view.bottom();
    }
    offset.y = offset.y.min(px(0.)).max(-scroll.max_offset().y);
    if offset.y != before {
        scroll.set_offset(offset);
        window.request_animation_frame();
    }
}

struct EditorElement {
    editor: Entity<LiveEditor>,
    decorations: HashMap<u64, Vec<(Range<usize>, Hsla)>>,
    child: AnyElement,
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<PaintQuad>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let child = self.child.request_layout(window, cx);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        (window.request_layout(style, [child], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<PaintQuad> {
        let decorations = std::mem::take(&mut self.decorations);
        let child = &mut self.child;
        let (_, geometry) = collect_leaf_geometry(decorations, || child.prepaint(window, cx));
        let caret_color = cx.theme().caret;
        self.editor.update(cx, |editor, _| {
            editor.geometry = geometry;
            let head = editor.doc.head();
            let (_, geo, i) = editor.caret_leaf(head)?;
            let at = caret_top(geo, i);
            let caret = Bounds::new(at, size(px(2.), geo.caret_height()));
            let margin = geo.line_height();
            if std::mem::take(&mut editor.autoscroll) {
                scroll_into_view(&editor.scroll, caret, margin, window);
            }
            Some(fill(caret, caret_color))
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        caret: &mut Option<PaintQuad>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        self.child.paint(window, cx);
        if focus.is_focused(window)
            && let Some(caret) = caret.take()
        {
            window.paint_quad(caret);
        }
        self.editor.update(cx, |editor, _| {
            if let Some(started) = editor.timing.started.take() {
                editor.timing.frame = started.elapsed();
                editor.timing.worst_frame = editor.timing.worst_frame.max(editor.timing.frame);
            }
        });
    }
}

impl EntityInputHandler for LiveEditor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let r = self.doc.range_from_utf16(&range);
        adjusted.replace(self.doc.range_to_utf16(&r));
        Some(self.doc.text()[r].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.doc.range_to_utf16(&self.doc.selection()),
            reversed: self.doc.is_reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.doc.marked().map(|r| self.doc.range_to_utf16(&r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.unmark();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.map(|r| self.doc.range_from_utf16(&r));
        self.edit(|d| d.replace(range, text), window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.map(|r| self.doc.range_from_utf16(&r));
        // `selected` counts UTF-16 units within `text`; convert through a scratch document.
        let selected = selected.map(|s| Document::new(text).range_from_utf16(&s));
        self.edit(|d| d.replace_and_mark(range, text, selected), window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let r = self.doc.range_from_utf16(&range);
        let (_, geo, start) = self.caret_leaf(r.start)?;
        let a = caret_top(geo, start);
        let end = caret_top(geo, start + (r.end - r.start));
        let right = if end.y == a.y { end.x } else { a.x };
        Some(Bounds::from_corners(
            a,
            point(right, a.y + geo.caret_height()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        p: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.source_at(p).map(|o| self.doc.to_utf16(o))
    }
}
