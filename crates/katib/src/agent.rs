//! The agent panel: a conversation with katib's built-in agent, beside the note. The agent runs
//! on its own thread (`katib_agent::Session`); this view shows its events and sends prompts.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Escape, InputEvent, Textarea, TextareaState},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_agent::acp::{self, McpServer};
use katib_agent::machine::{self, Memory};
use katib_agent::{Cancel, Choice, Event, Finish, PermissionOption, Session};
use katib_core::Vault;

/// One entry of the transcript.
enum Row {
    User(String),
    /// Streamed markdown, appended to as it arrives.
    Assistant(Entity<TextViewState>),
    Tool {
        id: String,
        name: String,
        note: Option<String>,
        detail: Option<String>,
        /// Set when the call ends: whether it worked, and the first line of what it returned.
        result: Option<(bool, String)>,
    },
    Notice {
        text: String,
        error: bool,
    },
    /// An outside agent asks before a tool call; `answered` is the choice once made.
    Permission {
        ask: u64,
        title: String,
        note: Option<String>,
        options: Vec<PermissionOption>,
        answered: Option<String>,
    },
}

enum Status {
    /// No session yet: the panel has not been opened.
    Idle,
    Starting,
    Ready {
        provider: String,
        model: String,
    },
    Failed,
}

/// Asks the window to open a note the agent named.
pub struct OpenNote(pub String);

pub struct AgentPanel {
    vault: Vault,
    /// Appended to the system prompt: today's date and the journal folder.
    extra: String,
    session: Option<Session>,
    status: Status,
    /// This machine's provider and model choice, as Settings shows it.
    memory: Memory,
    /// The prompt running, while one is.
    running: Option<Cancel>,
    /// Prompts typed while one runs, sent in order once it answers. Shown below the transcript
    /// rather than in it, so they do not interleave with the answer still streaming.
    queued: Vec<String>,
    /// For putting queued prompts back in the input from an event.
    window: AnyWindowHandle,
    rows: Vec<Row>,
    scroller: Entity<MessageScrollerState>,
    input: Entity<TextareaState>,
    /// The session's spend so far, in USD, when the provider or a price list gives it.
    cost: Option<f64>,
    cost_estimated: bool,
    /// Tokens the last request carried, against the model's window.
    used: u32,
    context: u32,
    _events: Option<Task<()>>,
    _input_events: Subscription,
}

impl EventEmitter<OpenNote> for AgentPanel {}

impl AgentPanel {
    pub fn new(vault: Vault, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Ask about your notes. Shift-Enter for a new line.")
        });
        let input_events = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit(window, cx);
            }
        });
        Self {
            vault,
            extra: String::new(),
            session: None,
            status: Status::Idle,
            memory: Memory::load(machine::config_dir().as_deref()),
            running: None,
            queued: Vec::new(),
            window: window.window_handle(),
            rows: Vec::new(),
            scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            input,
            cost: None,
            cost_estimated: false,
            used: 0,
            context: 0,
            _events: None,
            _input_events: input_events,
        }
    }

    /// Starts the session on first use, then focuses the input. `extra` is what the window
    /// knows that the agent should: today's date, the journal folder.
    pub fn open(&mut self, extra: String, window: &mut Window, cx: &mut Context<Self>) {
        self.extra = extra;
        if self.session.is_none() {
            self.start(cx);
        }
        self.input.update(cx, |s, cx| s.focus(window, cx));
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    /// Re-read the choice, which a session updates once a turn streams.
    pub fn reload_memory(&mut self) {
        self.memory = Memory::load(machine::config_dir().as_deref());
    }

    /// Start with `provider` from now on; `None` takes the first provider with a key.
    pub fn choose_provider(&mut self, provider: Option<&str>, cx: &mut Context<Self>) {
        let dir = machine::config_dir();
        self.memory.set_provider(dir.as_deref(), provider);
        self.restart(cx);
    }

    /// Use `model` with the chosen provider. Without a chosen provider there is nothing to
    /// attach it to.
    pub fn choose_model(&mut self, model: &str, cx: &mut Context<Self>) {
        let Some(provider) = self.memory.provider().map(String::from) else {
            return;
        };
        // A new conversation for the same model would only lose the current one.
        if self.memory.model(&provider).unwrap_or_default() == model.trim() {
            return;
        }
        let dir = machine::config_dir();
        self.memory
            .set_model(dir.as_deref(), &provider, Some(model));
        self.restart(cx);
    }

    /// Use `url` as the chosen provider's base URL; blank is the registry's. A URL that is not
    /// one is refused, with the reason.
    pub fn choose_base_url(&mut self, url: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(provider) = self.memory.provider().map(String::from) else {
            return Ok(());
        };
        if self.memory.base_url(&provider).unwrap_or_default() == url.trim().trim_end_matches('/') {
            return Ok(());
        }
        let dir = machine::config_dir();
        self.memory.set_base_url(dir.as_deref(), &provider, url)?;
        self.restart(cx);
        Ok(())
    }

    /// A new conversation, with the provider and model now chosen. Only if one was started.
    fn restart(&mut self, cx: &mut Context<Self>) {
        if self.session.is_some() {
            self.start(cx);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        // Dropping the old session cancels its prompt and stops its thread.
        self.session = None;
        self.running = None;
        self.queued.clear();
        self.rows.clear();
        self.cost = None;
        self.used = 0;
        self.scroller.update(cx, |s, cx| s.reset(0, cx));
        self.status = Status::Starting;
        let session = match self.memory.outside().map(String::from) {
            Some(id) => match acp::split_command(&self.memory.command(&id)) {
                Ok(command) => Session::acp(self.vault.clone(), command, self.mcp()),
                Err(e) => {
                    self.status = Status::Failed;
                    self.notice(format!("The agent's command: {e}"), true, cx);
                    self.notice(OUTSIDE_HINT, false, cx);
                    cx.notify();
                    return;
                }
            },
            None => {
                let choice = Choice {
                    extra: Some(self.extra.clone()),
                    ..Choice::default()
                };
                Session::start(self.vault.clone(), choice)
            }
        };
        let events = session.events();
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.on_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        self.session = Some(session);
        cx.notify();
    }

    /// `katib mcp <vault>`, offered to an outside agent so it gets the note tools. None if this
    /// program's path is unknown.
    fn mcp(&self) -> Option<McpServer> {
        let command = std::env::current_exe().ok()?;
        let root = std::path::absolute(self.vault.root()).ok()?;
        Some(McpServer {
            name: "katib".into(),
            command,
            args: vec!["mcp".into(), root.display().to_string()],
        })
    }

    /// Use outside agent `id` (a preset's, or `custom`) from now on; `None` is the built-in one.
    pub fn choose_outside(&mut self, id: Option<&str>, cx: &mut Context<Self>) {
        if self.memory.outside() == id {
            return;
        }
        let dir = machine::config_dir();
        self.memory.set_outside(dir.as_deref(), id);
        self.restart(cx);
    }

    /// The command line for outside agent `id`; blank is its preset's. Refused, with the reason,
    /// if it does not split into words.
    pub fn choose_command(
        &mut self,
        id: &str,
        line: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.memory.command(id) == line.trim() {
            return Ok(());
        }
        let dir = machine::config_dir();
        self.memory.set_command(dir.as_deref(), id, line)?;
        if self.memory.outside() == Some(id) {
            self.restart(cx);
        }
        Ok(())
    }

    /// Answers permission request `ask` with option `option`.
    fn permit(&mut self, ask: u64, option: &PermissionOption, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        session.answer(ask, Some(option.id.clone()));
        for row in &mut self.rows {
            if let Row::Permission {
                ask: a, answered, ..
            } = row
                && *a == ask
            {
                *answered = Some(option.name.clone());
            }
        }
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() || !matches!(self.status, Status::Ready { .. }) {
            return;
        }
        if self.running.is_some() {
            self.queued.push(text);
        } else {
            self.send(text, cx);
        }
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        self.running = Some(session.prompt(text.clone()));
        self.push(Row::User(text), cx);
    }

    /// Queued prompts back into the input, ahead of what it holds, for a prompt that stopped:
    /// they may have depended on its answer.
    fn unqueue(&mut self, cx: &mut Context<Self>) {
        if self.queued.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.queued);
        let input = self.input.clone();
        let current = input.read(cx).value().trim().to_owned();
        if !current.is_empty() {
            parts.push(current);
        }
        let text = parts.join("\n\n");
        let handle = self.window;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                input.update(cx, |s, cx| s.set_value(text, window, cx));
            });
        });
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.running {
            cancel.cancel();
        }
        cx.notify();
    }

    fn push(&mut self, row: Row, cx: &mut Context<Self>) {
        self.rows.push(row);
        self.scroller.update(cx, |s, cx| {
            s.append(1, cx);
        });
    }

    /// The last row's height changed.
    fn remeasure_last(&mut self, cx: &mut Context<Self>) {
        let last = self.rows.len().saturating_sub(1);
        self.scroller.update(cx, |s, cx| {
            s.remeasure_items(last..last + 1, cx);
        });
    }

    fn notice(&mut self, text: impl Into<String>, error: bool, cx: &mut Context<Self>) {
        let text = text.into();
        self.push(Row::Notice { text, error }, cx);
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Ready {
                provider,
                model,
                context,
                cost_estimated,
            } => {
                self.context = context;
                self.cost_estimated = cost_estimated;
                self.status = Status::Ready { provider, model };
            }
            Event::Failed(e) => {
                self.status = Status::Failed;
                self.running = None;
                self.notice(e, true, cx);
                match self.memory.outside() {
                    Some(_) => self.notice(OUTSIDE_HINT, false, cx),
                    None => self.notice(key_hint(), false, cx),
                }
            }
            Event::Text(delta) => match self.rows.last() {
                Some(Row::Assistant(state)) => {
                    state.update(cx, |s, cx| s.push_str(&delta, cx));
                    self.remeasure_last(cx);
                }
                _ => {
                    let state = cx.new(|cx| TextViewState::markdown(&delta, cx).selectable(true));
                    self.push(Row::Assistant(state), cx);
                }
            },
            Event::ToolStart {
                id,
                name,
                note,
                detail,
            } => self.push(
                Row::Tool {
                    id,
                    name,
                    note,
                    detail,
                    result: None,
                },
                cx,
            ),
            Event::ToolEnd { id, body, note, ok } => {
                let summary = note.unwrap_or(body);
                let line = summary.lines().next().unwrap_or_default().to_owned();
                // Calls can overlap, so the row is found by id.
                let at = self
                    .rows
                    .iter()
                    .rposition(|r| matches!(r, Row::Tool { id: i, .. } if *i == id));
                if let Some(ix) = at
                    && let Row::Tool { result, .. } = &mut self.rows[ix]
                {
                    *result = Some((ok, line));
                    self.scroller.update(cx, |s, cx| {
                        s.remeasure_items(ix..ix + 1, cx);
                    });
                }
            }
            Event::Permission {
                ask,
                title,
                note,
                options,
            } => self.push(
                Row::Permission {
                    ask,
                    title,
                    note,
                    options,
                    answered: None,
                },
                cx,
            ),
            Event::Answered(ask) => {
                for row in &mut self.rows {
                    if let Row::Permission {
                        ask: a, answered, ..
                    } = row
                        && *a == ask
                        && answered.is_none()
                    {
                        *answered = Some("Cancelled".into());
                    }
                }
            }
            Event::Totals { used, size, cost } => {
                self.used = used;
                self.context = size;
                self.cost = cost;
                self.cost_estimated = false;
            }
            Event::Retry { attempt, delay } => self.notice(
                format!("Retrying in {} s (attempt {attempt})", delay.as_secs()),
                false,
                cx,
            ),
            Event::Usage(usage) => {
                if let Some(cost) = usage.cost {
                    *self.cost.get_or_insert(0.0) += cost;
                }
                self.used = usage
                    .total_tokens
                    .max(usage.prompt_tokens + usage.completion_tokens);
            }
            Event::Compacted { before, after } => self.notice(
                format!("Compacted the conversation: about {before} to {after} tokens"),
                false,
                cx,
            ),
            Event::Done(finish) => {
                self.running = None;
                let go_on = matches!(finish, Finish::Answered | Finish::Refused);
                match finish {
                    Finish::Answered => {}
                    Finish::Cancelled => self.notice("Stopped", false, cx),
                    Finish::Refused => self.notice(
                        "The model declined to answer. The prompt is dropped from the \
                         conversation.",
                        true,
                        cx,
                    ),
                    Finish::Failed(e) => self.notice(e, true, cx),
                }
                if !go_on {
                    self.unqueue(cx);
                } else if !self.queued.is_empty() {
                    let next = self.queued.remove(0);
                    self.send(next, cx);
                }
            }
        }
        cx.notify();
    }

    fn row(&self, ix: usize, view: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let body = match row {
            Row::User(text) => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.85))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(theme.muted)
                        .text_sm()
                        .child(text.clone()),
                )
                .into_any_element(),
            Row::Assistant(state) => TextView::new(state).text_sm().into_any_element(),
            Row::Tool {
                name,
                note,
                detail,
                result,
                ..
            } => {
                let icon = match result {
                    None => IconName::LoaderCircle,
                    Some((true, _)) => IconName::Check,
                    Some((false, _)) => IconName::CircleAlert,
                };
                let failed = matches!(result, Some((false, _)));
                let target = note.clone().map(|id| {
                    let view = view.clone();
                    div()
                        .id(("tool-note", ix))
                        .text_color(theme.link)
                        .cursor_pointer()
                        .child(id.clone())
                        .on_click(move |_, _, cx| {
                            view.update(cx, |_, cx| cx.emit(OpenNote(id.clone())))
                        })
                });
                v_flex()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(icon)
                            .child(name.clone())
                            .children(target)
                            .children(detail.clone()),
                    )
                    .children(result.as_ref().map(|(_, line)| {
                        div()
                            .pl_4()
                            .truncate()
                            .when(failed, |d| d.text_color(theme.danger))
                            .child(line.clone())
                    }))
                    .into_any_element()
            }
            Row::Permission {
                ask,
                title,
                note,
                options,
                answered,
            } => {
                let view = view.clone();
                let target = note.clone().map(|id| {
                    let view = view.clone();
                    div()
                        .id(("permission-note", ix))
                        .text_color(theme.link)
                        .cursor_pointer()
                        .child(id.clone())
                        .on_click(move |_, _, cx| {
                            view.update(cx, |_, cx| cx.emit(OpenNote(id.clone())))
                        })
                });
                let ask = *ask;
                let choices = match answered {
                    Some(choice) => div()
                        .text_color(theme.muted_foreground)
                        .child(choice.clone())
                        .into_any_element(),
                    None => h_flex()
                        .gap_1()
                        .flex_wrap()
                        .children(options.iter().enumerate().map(|(n, option)| {
                            let (view, option) = (view.clone(), option.clone());
                            Button::new(("permit", ix * 16 + n))
                                .xsmall()
                                .label(option.name.clone())
                                .when(option.allow, |b| b.primary())
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |this, cx| this.permit(ask, &option, cx))
                                })
                        }))
                        .into_any_element(),
                };
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(IconName::CircleAlert)
                            .child(format!("Allow {title}?"))
                            .children(target),
                    )
                    .child(choices)
                    .into_any_element()
            }
            Row::Notice { text, error } => div()
                .text_xs()
                .text_color(if *error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text.clone())
                .into_any_element(),
        };
        div().px_3().py_1().child(body).into_any_element()
    }

    fn status_line(&self) -> String {
        let model = match &self.status {
            Status::Idle => return String::new(),
            Status::Starting => return "Starting...".into(),
            Status::Failed => return "Not started".into(),
            Status::Ready { provider, model } if model.is_empty() => provider.clone(),
            Status::Ready { provider, model } => format!("{provider} / {model}"),
        };
        let mut parts = vec![model];
        if self.context > 0 && self.used > 0 {
            let share = u64::from(self.used) * 100 / u64::from(self.context);
            parts.push(format!("{share}% of context"));
        }
        if let Some(cost) = self.cost {
            let mark = if self.cost_estimated { "~" } else { "" };
            parts.push(format!("{mark}${cost:.4}"));
        }
        parts.join("  ")
    }
}

/// For an outside agent that could not start.
const OUTSIDE_HINT: &str = "Check the agent's command in Settings, Agent. An agent that needs a \
    sign-in, such as Claude Code, needs it done once in a terminal.";

/// Where keys are read from, for a session that could not start.
fn key_hint() -> String {
    let dir = machine::config_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "katib's config folder".into());
    format!(
        "Set the provider's key variable, such as OPENAI_API_KEY, or save the key as \
         {dir}/<provider>.key. Choose the provider and model in Settings, Agent."
    )
}

impl Render for AgentPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity();
        let rows = MessageScroller::new("agent-rows", self.scroller.clone(), move |ix, _, cx| {
            view.read(cx).row(ix, &view, cx)
        })
        .size_full();
        let running = self.running.is_some();
        let ready = matches!(self.status, Status::Ready { .. });
        let send = if running {
            Button::new("agent-stop")
                .icon(IconName::Square)
                .tooltip("Stop (Escape)")
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("agent-send")
                .primary()
                .icon(IconName::ArrowUp)
                .tooltip("Send")
                .disabled(!ready)
                .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx)))
        };
        let header = h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(IconName::Bot)
            .child(div().flex_1().text_sm().child("Agent"))
            .child(
                Button::new("agent-new")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip("New conversation")
                    .disabled(running)
                    .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
            );
        let about = match self.memory.outside() {
            None => {
                "The agent reads, searches and edits the notes in this vault, and nothing \
                     outside it. Its edits are saved at once."
            }
            Some(_) => {
                "An outside agent runs with its own tools, which can reach beyond the \
                        vault. katib answers its file requests inside the vault only, and offers \
                        it katib's note tools. Its edits are saved at once."
            }
        };
        let empty = self.rows.is_empty().then(|| {
            div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(about)
        });
        let queued = (!self.queued.is_empty()).then(|| {
            v_flex()
                .px_3()
                .py_1()
                .gap_1()
                .border_t_1()
                .border_color(theme.border)
                .children(self.queued.iter().enumerate().map(|(ix, text)| {
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(format!("Queued: {text}")),
                        )
                        .child(
                            Button::new(("agent-unqueue", ix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip("Remove")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if ix < this.queued.len() {
                                        this.queued.remove(ix);
                                    }
                                    cx.notify();
                                })),
                        )
                }))
        });
        v_flex()
            .size_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(header)
            .child(div().flex_1().min_h_0().map(|d| match empty {
                Some(empty) => d.child(empty),
                None => d.child(rows),
            }))
            .children(queued)
            .child(
                v_flex()
                    // The input passes on an Escape it has no use for.
                    .on_action(cx.listener(|this, _: &Escape, _, cx| {
                        if this.running.is_some() {
                            this.stop(cx);
                        } else {
                            cx.propagate();
                        }
                    }))
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(div().flex_1().min_w_0().child(Textarea::new(&self.input)))
                            .child(send),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(self.status_line()),
                    ),
            )
    }
}
