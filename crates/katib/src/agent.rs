//! The agent panel: a conversation with katib's built-in agent, beside the note. The agent runs
//! on its own thread (`katib_agent::Session`); this view shows its events and sends prompts.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use katib_agent::machine::{self, Memory};
use katib_agent::{Cancel, Choice, Event, Finish, Session, describe};
use katib_core::Vault;

/// One entry of the transcript.
enum Row {
    User(String),
    /// Streamed markdown, appended to as it arrives.
    Assistant(Entity<TextViewState>),
    Tool {
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
        let dir = machine::config_dir();
        self.memory
            .set_model(dir.as_deref(), &provider, Some(model));
        self.restart(cx);
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
        self.rows.clear();
        self.cost = None;
        self.used = 0;
        self.scroller.update(cx, |s, cx| s.reset(0, cx));
        self.status = Status::Starting;
        let choice = Choice {
            extra: Some(self.extra.clone()),
            ..Choice::default()
        };
        let session = Session::start(self.vault.clone(), choice);
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

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() || self.running.is_some() || !matches!(self.status, Status::Ready { .. })
        {
            return;
        }
        let Some(session) = &self.session else { return };
        self.running = Some(session.prompt(text.clone()));
        self.push(Row::User(text), cx);
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
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
                self.notice(e, true, cx);
                self.notice(key_hint(), false, cx);
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
            Event::ToolStart { name, arguments } => {
                let target = describe(&name, &arguments);
                self.push(
                    Row::Tool {
                        name,
                        note: target.note,
                        detail: target.detail,
                        result: None,
                    },
                    cx,
                );
            }
            Event::ToolEnd { body, note, ok } => {
                let summary = note.unwrap_or(body);
                let line = summary.lines().next().unwrap_or_default().to_owned();
                if let Some(Row::Tool { result, .. }) = self.rows.last_mut() {
                    *result = Some((ok, line));
                    self.remeasure_last(cx);
                }
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
                match finish {
                    Finish::Answered => {}
                    Finish::Cancelled => self.notice("Stopped", false, cx),
                    Finish::Refused => self.notice("The model declined to answer", true, cx),
                    Finish::Failed(e) => self.notice(e, true, cx),
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
                .tooltip("Stop")
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
        let empty = self.rows.is_empty().then(|| {
            div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(
                    "The agent reads, searches and edits the notes in this vault, and nothing \
                     outside it. Its edits are saved at once.",
                )
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
            .child(
                v_flex()
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
