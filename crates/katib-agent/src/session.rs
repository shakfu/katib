//! One agent on its own thread. The loop's futures are not `Send` (`Tools` and `Frontend` box
//! local futures), so the thread runs a single-threaded tokio runtime, and the window talks to it
//! through channels: prompts in, `Event`s out.

use std::time::Duration;

use katib_core::Vault;
use minima_core::provider::http::Http;
use minima_core::provider::{Provider, Usage};
use minima_core::{Agent, Cancel, Config, Frontend, Request, price};

use crate::VaultTools;
use crate::machine::{self, Memory};

const MAX_TURNS: u32 = 100;

const SYSTEM: &str = "You are the assistant built into katib, a notes app. The user's notes are \
markdown files in a folder, the vault. You reach them only through your tools, which name a note \
by its id: its path in the vault without `.md`, such as `projects/katib`. Notes link with \
`[[Title]]` or `[[id#Heading|label]]` wiki links and with relative markdown links; `move` \
rewrites them, so use it rather than writing a copy. Read a note before replacing it, and prefer \
`edit` for a small change. Keep the note's front matter and formatting. Answer in markdown.";

/// What the agent did, in order. A prompt's events end with `Done`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The session can take prompts.
    Ready {
        provider: String,
        model: String,
        context: u32,
        /// Costs are estimated from a price list rather than reported.
        cost_estimated: bool,
    },
    /// The session could not start; it takes no prompts.
    Failed(String),
    Text(String),
    /// A tool call began. `id` matches its `ToolEnd`; calls may overlap. `note` is the note it
    /// acts on, if any, and `detail` its other argument.
    ToolStart {
        id: String,
        name: String,
        note: Option<String>,
        detail: Option<String>,
    },
    /// `ok` is false when the call failed; `body` is what the model was told.
    ToolEnd {
        id: String,
        body: String,
        note: Option<String>,
        ok: bool,
    },
    /// An outside agent asks before a tool call. Answer with `Session::answer(ask, ..)`.
    Permission {
        ask: u64,
        title: String,
        note: Option<String>,
        options: Vec<PermissionOption>,
    },
    /// Permission request `ask` was answered, or cancelled with its prompt.
    Answered(u64),
    /// An outside agent's totals: tokens in its window, the window, and the session's cost.
    Totals {
        used: u32,
        size: u32,
        cost: Option<f64>,
    },
    Retry {
        attempt: u32,
        delay: Duration,
    },
    /// One request finished.
    Usage(Usage),
    Compacted {
        before: u32,
        after: u32,
    },
    Done(Finish),
}

/// One answer an outside agent offers to a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// It allows the call, once or from now on.
    pub allow: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Finish {
    Answered,
    Cancelled,
    Refused,
    Failed(String),
}

/// What the user chose; unset fields come from this machine's memory.
#[derive(Debug, Clone, Default)]
pub struct Choice {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// For a local server or a gateway.
    pub base_url: Option<String>,
    /// Appended to the system prompt: what the window knows, such as today's date.
    pub extra: Option<String>,
}

pub struct Session {
    prompts: async_channel::Sender<(String, Cancel)>,
    /// Answers to permission requests; only an outside agent asks.
    answers: async_channel::Sender<(u64, Option<String>)>,
    events: async_channel::Receiver<Event>,
    /// Set on drop: ends the running prompt and drops the queued ones.
    stop: Cancel,
}

impl Session {
    /// Resolves the choice against this machine's keys and memory, then serves prompts against
    /// `vault`. A failure arrives as `Event::Failed`.
    pub fn start(vault: Vault, choice: Choice) -> Self {
        Self::spawn(move || async move {
            let dir = machine::config_dir();
            let memory = Memory::load(dir.as_deref());
            let mut config = request(&choice, &memory, dir.clone())
                .resolve(|var| machine::key(dir.as_deref(), var), |p| memory.model(p))
                .await
                .map_err(|e| e.to_string())?;
            if let Some(listing) = price::lookup(&config, false).await {
                listing.apply(&mut config);
            }
            let http = Http::new().map_err(|e| format!("{e:#}"))?;
            let system = match &choice.extra {
                Some(extra) => format!("{SYSTEM}\n\n{extra}"),
                None => SYSTEM.to_owned(),
            };
            let mut agent = agent(vault, Provider::Http(http), config, system)?;
            // Once a turn streams: the provider has accepted the model.
            agent.on_first_turn(move |c| {
                Memory::load(dir.as_deref()).remember(dir.as_deref(), &c.provider, &c.model)
            });
            Ok(agent)
        })
    }

    /// A session over a given provider and config, with nothing read from or written to this
    /// machine. For tests and scripted runs.
    pub fn with_provider(vault: Vault, provider: Provider, config: Config) -> Self {
        Self::spawn(move || async move { agent(vault, provider, config, SYSTEM.into()) })
    }

    fn spawn<F, Fut>(make: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Agent, String>>,
    {
        Self::spawn_with(move |inbox, _answers, sender, stopped| {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = sender.try_send(Event::Failed(format!("starting the agent: {e}")));
                    return;
                }
            };
            runtime.block_on(serve(make(), inbox, sender, stopped));
        })
    }

    /// Runs `body` on the agent thread with the session's channels: prompts in, answers in,
    /// events out, and the token set on drop.
    pub(crate) fn spawn_with(
        body: impl FnOnce(
            async_channel::Receiver<(String, Cancel)>,
            async_channel::Receiver<(u64, Option<String>)>,
            async_channel::Sender<Event>,
            Cancel,
        ) + Send
        + 'static,
    ) -> Self {
        let (prompts, inbox) = async_channel::unbounded::<(String, Cancel)>();
        let (answers, answered) = async_channel::unbounded();
        let (sender, events) = async_channel::unbounded();
        let stop = Cancel::new();
        let stopped = stop.clone();
        std::thread::Builder::new()
            .name("katib-agent".into())
            .spawn(move || body(inbox, answered, sender, stopped))
            .expect("spawning the agent thread");
        Self {
            prompts,
            answers,
            events,
            stop,
        }
    }

    /// Queues a prompt. The handle cancels it, whether it is running or still queued.
    pub fn prompt(&self, text: impl Into<String>) -> Cancel {
        let cancel = Cancel::new();
        // Unbounded and open while `self` lives; a closed one means the session failed, which
        // `Event::Failed` already reported.
        let _ = self.prompts.try_send((text.into(), cancel.clone()));
        cancel
    }

    /// Answers permission request `ask` with an option's id, or `None` to refuse it.
    pub fn answer(&self, ask: u64, option: Option<String>) {
        let _ = self.answers.try_send((ask, option));
    }

    /// Every event, in order. The stream ends once the session stops.
    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.cancel();
        self.prompts.close();
    }
}

fn agent(
    vault: Vault,
    provider: Provider,
    config: Config,
    system: String,
) -> Result<Agent, String> {
    let tools = VaultTools::open(vault).map_err(|e| format!("{e:#}"))?;
    Ok(Agent::new(provider, config, system, Box::new(tools)))
}

async fn serve(
    make: impl Future<Output = Result<Agent, String>>,
    prompts: async_channel::Receiver<(String, Cancel)>,
    events: async_channel::Sender<Event>,
    stop: Cancel,
) {
    let mut agent = match make.await {
        Ok(agent) => agent,
        Err(e) => {
            let _ = events.try_send(Event::Failed(e));
            return;
        }
    };
    let _ = events.try_send(Event::Ready {
        provider: agent.provider().to_owned(),
        model: agent.model().to_owned(),
        context: agent.context_window(),
        cost_estimated: agent.cost_is_estimate(),
    });
    loop {
        let (prompt, cancel) = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            next = prompts.recv() => match next {
                Ok(next) => next,
                Err(_) => return,
            },
        };
        let mut forward = Forward {
            events: &events,
            refused: false,
            calls: 0,
        };
        // The session is ending, so a run dropped mid-way leaves nothing that matters.
        let result = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            result = agent.run(&prompt, &mut forward, &cancel) => result,
        };
        let finish = finish(&mut agent, result, &cancel, forward.refused);
        let _ = events.try_send(Event::Done(finish));
    }
}

/// How a run ended. A refused prompt is dropped from the conversation, so the next request does
/// not carry it to the model again.
fn finish(agent: &mut Agent, result: anyhow::Result<()>, cancel: &Cancel, refused: bool) -> Finish {
    match result {
        Err(e) => Finish::Failed(format!("{e:#}")),
        Ok(()) if cancel.is_cancelled() => Finish::Cancelled,
        Ok(()) if refused => {
            agent.forget_last_prompt();
            Finish::Refused
        }
        Ok(()) => Finish::Answered,
    }
}

/// The loop's events, onto the channel. A closed channel drops them: nobody is listening.
struct Forward<'a> {
    events: &'a async_channel::Sender<Event>,
    refused: bool,
    /// Calls started; the loop runs one at a time, so the last is the open one.
    calls: u32,
}

impl Forward<'_> {
    fn send(&self, event: Event) {
        let _ = self.events.try_send(event);
    }
}

impl Frontend for Forward<'_> {
    fn text(&mut self, delta: &str) {
        self.send(Event::Text(delta.to_owned()));
    }

    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.calls += 1;
        let target = crate::describe(name, arguments);
        self.send(Event::ToolStart {
            id: self.calls.to_string(),
            name: name.to_owned(),
            note: target.note,
            detail: target.detail,
        });
    }

    fn tool_end(&mut self, body: &str, note: Option<&str>, ok: bool) {
        self.send(Event::ToolEnd {
            id: self.calls.to_string(),
            body: body.to_owned(),
            note: note.map(String::from),
            ok,
        });
    }

    fn retry(&mut self, attempt: u32, delay: Duration) {
        self.send(Event::Retry { attempt, delay });
    }

    fn turn_end(&mut self, usage: Usage) {
        self.send(Event::Usage(usage));
    }

    fn compacted(&mut self, before: u32, after: u32) {
        self.send(Event::Compacted { before, after });
    }

    fn cancelled(&mut self) {}

    fn refused(&mut self) {
        self.refused = true;
    }
}

/// The request for `choice`, filled from `memory`. A provider chosen in Settings is named, not
/// only preferred: autoselect skips providers without a key, so it would never pick a local
/// server, and with a key set would pick another provider unseen.
fn request(choice: &Choice, memory: &Memory, dir: Option<std::path::PathBuf>) -> Request {
    let provider = choice
        .provider
        .clone()
        .or_else(|| memory.provider().map(String::from));
    let base_url = choice.base_url.clone().or_else(|| {
        let url = memory.base_url(provider.as_deref()?)?;
        Some(url.to_owned())
    });
    Request {
        provider,
        model: choice.model.clone(),
        base_url,
        max_turns: MAX_TURNS,
        cache_dir: dir,
        ..Request::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use minima_core::provider::Dialect;
    use minima_core::provider::mock::Mock;
    use minima_core::tools::Scratch;
    use serde_json::json;

    use super::*;

    fn config() -> Config {
        Config {
            provider: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: "mock".into(),
            dialect: Dialect::Chat,
            context: 128_000,
            context_guessed: false,
            max_output: None,
            adaptive_thinking: false,
            max_turns: 4,
            pricing: None,
            rates: None,
            cap_output: false,
            cache_dir: None,
        }
    }

    fn session(dir: &Scratch, script: serde_json::Value) -> Session {
        let vault = Vault::open(dir.file("")).unwrap();
        let mock = Mock::from_script(&script.to_string()).unwrap();
        Session::with_provider(vault, Provider::Mock(mock), config())
    }

    /// The next event, or `None` once the channel closes or 5 s pass.
    fn next(events: &async_channel::Receiver<Event>) -> Option<Event> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match events.try_recv() {
                Ok(event) => return Some(event),
                Err(async_channel::TryRecvError::Closed) => return None,
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
            }
        }
        panic!("no event within 5 s");
    }

    /// Every event up to and including `Done`.
    fn until_done(events: &async_channel::Receiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(event) = next(events) {
            let done = matches!(event, Event::Done(_));
            out.push(event);
            if done {
                break;
            }
        }
        out
    }

    #[test]
    fn a_prompt_streams_its_events_and_writes_the_note() {
        let dir = Scratch::new("katib-session-run");
        let args = json!({ "note": "hello", "content": "# Hello\n" }).to_string();
        let session = session(
            &dir,
            json!([
                [{ "tool_call": { "index": 0, "id": "c1", "name": "write", "arguments": args } }],
                [{ "text": "done" }, { "usage": { "prompt_tokens": 9, "completion_tokens": 1 } }]
            ]),
        );
        let events = session.events();
        assert!(matches!(next(&events), Some(Event::Ready { .. })));

        session.prompt("say hello");
        let got = until_done(&events);
        assert!(
            got.contains(&Event::ToolStart {
                id: "1".into(),
                name: "write".into(),
                note: Some("hello".into()),
                detail: None,
            }),
            "{got:?}"
        );
        assert!(
            got.iter().any(
                |e| matches!(e, Event::ToolEnd { body, ok: true, .. } if body == "created hello")
            ),
            "{got:?}"
        );
        assert!(got.contains(&Event::Text("done".into())), "{got:?}");
        assert_eq!(got.last(), Some(&Event::Done(Finish::Answered)));
        let written = std::fs::read_to_string(PathBuf::from(dir.file("hello.md"))).unwrap();
        assert_eq!(written, "# Hello\n");
    }

    #[test]
    fn a_cancelled_prompt_ends_cancelled_and_the_session_takes_the_next() {
        let dir = Scratch::new("katib-session-cancel");
        let session = session(
            &dir,
            json!({
                "slow": [[{ "pause": 60_000 }, { "text": "late" }]],
                "fast": [[{ "text": "quick" }]]
            }),
        );
        let events = session.events();
        assert!(matches!(next(&events), Some(Event::Ready { .. })));

        let cancel = session.prompt("slow");
        cancel.cancel();
        assert_eq!(
            until_done(&events).last(),
            Some(&Event::Done(Finish::Cancelled))
        );

        session.prompt("fast");
        let got = until_done(&events);
        assert!(got.contains(&Event::Text("quick".into())), "{got:?}");
        assert_eq!(got.last(), Some(&Event::Done(Finish::Answered)));
    }

    /// Dropping the session ends a running prompt and closes the events.
    #[test]
    fn dropping_the_session_stops_it() {
        let dir = Scratch::new("katib-session-drop");
        let session = session(&dir, json!([[{ "pause": 60_000 }, { "text": "late" }]]));
        let events = session.events();
        assert!(matches!(next(&events), Some(Event::Ready { .. })));
        session.prompt("go");
        drop(session);
        while let Some(event) = next(&events) {
            assert_ne!(event, Event::Text("late".into()));
        }
    }

    #[test]
    fn the_chosen_provider_and_its_base_url_are_named() {
        let dir = Scratch::new("katib-session-request");
        let root = PathBuf::from(dir.file(""));
        let mut memory = Memory::load(Some(&root));
        memory.set_provider(Some(&root), Some("ollama"));
        memory
            .set_base_url(Some(&root), "ollama", "http://box:11434/v1")
            .unwrap();
        let named = request(&Choice::default(), &memory, None);
        assert_eq!(named.provider.as_deref(), Some("ollama"));
        assert_eq!(named.base_url.as_deref(), Some("http://box:11434/v1"));

        // A local server resolves with no key set anywhere.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let named = Request {
            model: Some("m".into()),
            ..named
        };
        let config = rt.block_on(named.resolve(|_| None, |_| None)).unwrap();
        assert_eq!(
            (config.provider.as_str(), config.base_url.as_str()),
            ("ollama", "http://box:11434/v1")
        );

        // Automatic: nothing named.
        memory.set_provider(Some(&root), None);
        let auto = request(&Choice::default(), &memory, None);
        assert_eq!((auto.provider, auto.base_url), (None, None));
    }

    #[test]
    fn a_refused_prompt_is_dropped_from_the_conversation() {
        let dir = Scratch::new("katib-session-refused");
        let script = json!({ "ok": [[{ "text": "fine" }]], "bad": [["refused"]] });
        let mock = Mock::from_script(&script.to_string()).unwrap();
        let tools = VaultTools::open(Vault::open(dir.file("")).unwrap()).unwrap();
        let mut agent = Agent::new(Provider::Mock(mock), config(), "s".into(), Box::new(tools));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let run = |agent: &mut Agent, prompt: &str| {
            let mut forward = Forward {
                events: &async_channel::unbounded().0,
                refused: false,
                calls: 0,
            };
            let cancel = Cancel::new();
            let result = rt.block_on(agent.run(prompt, &mut forward, &cancel));
            finish(agent, result, &cancel, forward.refused)
        };
        assert_eq!(run(&mut agent, "ok"), Finish::Answered);
        let kept = agent.messages().len();
        assert_eq!(run(&mut agent, "bad"), Finish::Refused);
        assert_eq!(agent.messages().len(), kept);
    }

    #[test]
    fn a_choice_that_does_not_resolve_fails_the_session() {
        let dir = Scratch::new("katib-session-fail");
        let vault = Vault::open(dir.file("")).unwrap();
        let session = Session::start(
            vault,
            Choice {
                provider: Some("notaprovider".into()),
                ..Choice::default()
            },
        );
        match next(&session.events()) {
            Some(Event::Failed(e)) => assert!(e.contains("unknown provider"), "{e}"),
            other => panic!("{other:?}"),
        }
    }
}
