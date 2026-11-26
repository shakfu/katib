//! The settings dialog. Settings are the vault's `.katib/config.toml`, so they
//! are committed and every clone of the vault shares them.

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    input::{Input, InputEvent, InputState},
    searchable_list::SearchableVec,
    select::{Select, SelectEvent, SelectState},
    setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    v_flex,
};
use gpui_kit::*;
use katib_agent::machine;
use katib_core::config::{Backend, Config, JOURNAL};

use super::{Katib, sync::SyncState};

type ModelSelect = SelectState<SearchableVec<SharedString>>;

/// How Settings asks for the agent's model.
#[derive(Clone, Copy, PartialEq)]
enum ModelField {
    /// No provider is chosen, so there is nothing to attach a model to.
    Hidden,
    /// The provider's models, searchable.
    List,
    /// A text field: the provider's list is unknown.
    Text,
    /// An outside agent is chosen: it picks its own model.
    Outside,
}

/// The agent fields in Settings that keep state: the chosen provider's models, and two text
/// fields. Each change starts a new conversation, so the text fields apply on Enter or on
/// leaving the field, not on each key.
pub(super) struct AgentFields {
    models: Entity<ModelSelect>,
    /// The model, typed, for a provider with no list.
    model: Entity<InputState>,
    base_url: Entity<InputState>,
    /// The provider the fields are for.
    provider: Option<String>,
    listed: usize,
    /// A fetch in progress, why the last one failed, or why a base URL was refused.
    note: Option<String>,
    /// The outside agent's command line.
    command: Entity<InputState>,
    /// Why the typed command was refused.
    command_note: Option<String>,
    _subscriptions: [Subscription; 4],
}

impl Katib {
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_error = None;
        self.agent.update(cx, |a, _| a.reload_memory());
        self.load_agent_fields(window, cx);
        let this = cx.entity();
        let handle = window.window_handle();
        window.open_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog
                .title("Settings")
                .w(px(820.))
                .content(move |content, _, cx| {
                    let katib = this.read(cx);
                    let listed = katib.agent_fields.as_ref().is_some_and(|f| f.listed > 0);
                    let memory = katib.agent.read(cx).memory();
                    let model = match memory.provider() {
                        _ if memory.outside().is_some() => ModelField::Outside,
                        None => ModelField::Hidden,
                        Some(_) if listed => ModelField::List,
                        Some(_) => ModelField::Text,
                    };
                    content.child(
                        div()
                            .h(px(480.))
                            .child(Settings::new("settings").pages(pages(&this, handle, model))),
                    )
                })
        });
    }

    /// Fill the agent fields for the chosen provider: its model and base URL, and its models
    /// from the cache, fetched in the background if missing or a day old.
    fn load_agent_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self
            .agent_fields
            .get_or_insert_with(|| agent_fields(window, cx));
        let memory = self.agent.read(cx).memory();
        let provider = memory.provider().map(String::from);
        let chosen = provider.as_deref().and_then(|p| memory.model(p));
        let url = provider.as_deref().and_then(|p| memory.base_url(p));
        let url = url.unwrap_or_default().to_owned();
        let default = provider.as_deref().and_then(machine::default_base_url);
        let typed = chosen.clone().unwrap_or_default();
        let outside = memory.outside().map(String::from);
        let command = outside
            .as_deref()
            .map(|id| memory.command(id))
            .unwrap_or_default();
        let preset = katib_agent::acp::PRESETS
            .iter()
            .find(|p| Some(p.id) == outside.as_deref())
            .map_or("", |p| p.command);
        fields
            .model
            .update(cx, |s, cx| s.set_value(typed, window, cx));
        fields.base_url.update(cx, |s, cx| {
            s.set_value(url, window, cx);
            s.set_placeholder(default.unwrap_or_default(), window, cx);
        });
        fields.command.update(cx, |s, cx| {
            s.set_value(command, window, cx);
            s.set_placeholder(preset, window, cx);
        });
        fields.command_note = None;
        let dir = machine::config_dir();
        let list = provider
            .as_deref()
            .map(|p| machine::models(dir.as_deref(), p))
            .unwrap_or_default();
        fields.provider = provider.clone();
        fields.note = None;
        set_models(fields, list.ids, chosen.as_deref(), window, cx);
        let (Some(provider), true) = (provider, list.due) else {
            return;
        };
        fields.note = Some("Fetching the model list...".into());
        cx.spawn_in(window, async move |this, cx| {
            let fetching = provider.clone();
            let result = cx
                .background_spawn(async move { machine::fetch_models(dir.as_deref(), &fetching) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let chosen = this.agent.read(cx).memory().model(&provider);
                let Some(fields) = this.agent_fields.as_mut() else {
                    return;
                };
                // The provider changed while the list was fetched.
                if fields.provider.as_deref() != Some(provider.as_str()) {
                    return;
                }
                match result {
                    Ok(ids) => {
                        fields.note = None;
                        set_models(fields, ids, chosen.as_deref(), window, cx);
                    }
                    Err(e) => fields.note = Some(format!("Could not list models: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply the typed command line for the outside agent chosen.
    fn commit_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(fields) = &self.agent_fields else {
            return;
        };
        let Some(id) = self.agent.read(cx).memory().outside().map(String::from) else {
            return;
        };
        let line = fields.command.read(cx).value();
        let result = self
            .agent
            .update(cx, |a, cx| a.choose_command(&id, &line, cx));
        self.load_agent_fields(window, cx);
        if let (Err(e), Some(fields)) = (result, self.agent_fields.as_mut()) {
            fields.command_note = Some(format!("Not applied: {e}"));
        }
        cx.notify();
    }

    /// Apply the typed base URL. The model list is the new URL's, so it reloads.
    fn commit_base_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(fields) = &self.agent_fields else {
            return;
        };
        let url = fields.base_url.read(cx).value();
        let result = self.agent.update(cx, |a, cx| a.choose_base_url(&url, cx));
        self.load_agent_fields(window, cx);
        if let (Err(e), Some(fields)) = (result, self.agent_fields.as_mut()) {
            fields.note = Some(e);
        }
        cx.notify();
    }

    /// Write `config` to the vault and apply it; sync restarts if it changed.
    fn apply_config(&mut self, config: Config, cx: &mut Context<Self>) {
        let restart = config.sync != self.config.sync || config.backend != self.config.backend;
        if restart && self.sync.is_busy() {
            self.settings_error = Some("Sync is running; try again in a moment".into());
        } else if let Err(e) = config.write(&self.vault) {
            self.settings_error = Some(format!(".katib/config.toml: {e}"));
        } else {
            self.settings_error = None;
            self.config = config;
            // config.toml is in .katib, which the watcher ignores.
            self.sync.note_change();
            if restart {
                self.sync = SyncState::new(&self.vault, &self.config);
            }
        }
        cx.notify();
    }

    /// Change one setting, starting from the settings in effect.
    fn change(&mut self, f: impl FnOnce(&mut Config), cx: &mut Context<Self>) {
        let mut config = self.config.clone();
        f(&mut config);
        if config != self.config {
            self.apply_config(config, cx);
        }
    }

    /// Set the journal folder, if the name is one the vault accepts.
    fn set_journal(&mut self, folder: &str, cx: &mut Context<Self>) {
        let folder = folder.trim().trim_matches('/');
        // `#` starts a comment in config.toml, which the reader does not unquote.
        let valid = !folder.is_empty()
            && !folder.contains('#')
            && self.vault.entry(&format!("{folder}/x")).is_ok();
        if valid {
            self.change(|c| c.journal = folder.to_owned(), cx);
        } else {
            self.settings_error = Some(format!("\"{folder}\" is not a folder name katib accepts"));
            cx.notify();
        }
    }
}

/// The agent fields' state, with their subscriptions to `Katib`.
fn agent_fields(window: &mut Window, cx: &mut Context<Katib>) -> AgentFields {
    let models = cx.new(|cx| {
        SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
    });
    let model = cx.new(|cx| InputState::new(window, cx));
    let base_url = cx.new(|cx| InputState::new(window, cx));
    let picked = cx.subscribe_in(
        &models,
        window,
        |this, _, event: &SelectEvent<SearchableVec<SharedString>>, _, cx| {
            if let SelectEvent::Confirm(Some(model)) = event {
                this.agent.update(cx, |a, cx| a.choose_model(model, cx));
            }
        },
    );
    let typed = cx.subscribe_in(&model, window, |this, input, event: &InputEvent, _, cx| {
        if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
            let model = input.read(cx).value();
            this.agent.update(cx, |a, cx| a.choose_model(&model, cx));
        }
    });
    let url = cx.subscribe_in(
        &base_url,
        window,
        |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_base_url(window, cx);
            }
        },
    );
    let command = cx.new(|cx| InputState::new(window, cx));
    let typed_command = cx.subscribe_in(
        &command,
        window,
        |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_command(window, cx);
            }
        },
    );
    AgentFields {
        models,
        model,
        base_url,
        provider: None,
        listed: 0,
        note: None,
        command,
        command_note: None,
        _subscriptions: [picked, typed, url, typed_command],
    }
}

/// Show `ids` in the model list, with `chosen` selected if listed.
fn set_models(
    picker: &mut AgentFields,
    ids: Vec<String>,
    chosen: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    picker.listed = ids.len();
    let items: Vec<SharedString> = ids.into_iter().map(SharedString::from).collect();
    picker.models.update(cx, |s, cx| {
        s.set_items(SearchableVec::new(items), window, cx);
        match chosen {
            Some(model) => s.set_selected_value(&SharedString::from(model.to_owned()), window, cx),
            None => s.set_selected_index(None, window, cx),
        }
    });
}

fn pages(this: &Entity<Katib>, handle: AnyWindowHandle, model: ModelField) -> Vec<SettingPage> {
    let read = |f: fn(&Katib) -> SharedString| {
        let this = this.clone();
        move |cx: &App| f(this.read(cx))
    };
    let error = SettingItem::render({
        let this = this.clone();
        move |_, _, cx| {
            let error = this.read(cx).settings_error.clone();
            div()
                .text_sm()
                .text_color(cx.theme().danger)
                .children(error)
        }
    });

    let sync_switch = {
        let (get, set) = (this.clone(), this.clone());
        SettingField::switch(
            move |cx| get.read(cx).config.sync,
            move |on, cx| set.update(cx, |k, cx| k.change(|c| c.sync = on, cx)),
        )
        .default_value(false)
    };
    let sync = SettingPage::new("Sync")
        .icon(IconName::RefreshCw)
        .default_open(true)
        .group(
            SettingGroup::new()
                .title("Git sync")
                .item(
                    SettingItem::new("Sync this vault", sync_switch).description(
                        "Commit every change, then pull and push with the branch's upstream. \
                         The vault must be the top folder of a git repository.",
                    ),
                )
                .item(SettingItem::new(
                    "State",
                    SettingField::render({
                        let this = this.clone();
                        move |_, _, cx| div().text_sm().child(this.read(cx).sync.status())
                    }),
                ))
                .item(error.clone()),
        );

    let backend = {
        let set = this.clone();
        SettingField::dropdown(
            vec![
                ("git".into(), "Git: files as they are".into()),
                ("lfs".into(), "Git LFS: files as pointers".into()),
            ],
            read(|k| k.config.backend.name().into()),
            move |value, cx| {
                let backend = match value.as_ref() {
                    "lfs" => Backend::Lfs,
                    _ => Backend::Git,
                };
                set.update(cx, |k, cx| k.change(|c| c.backend = backend, cx));
            },
        )
        .default_value("git")
    };
    let attachments = SettingPage::new("Attachments").icon(IconName::File).group(
        SettingGroup::new()
            .title("Storage")
            .item(
                SettingItem::new("Store attachments with", backend).description(
                    "Git LFS keeps large files out of the repository's history. \
                     It needs git-lfs on every machine and a remote that serves LFS. \
                     Attachments already committed move to LFS on the next commit.",
                ),
            )
            .item(error.clone()),
    );

    let journal = {
        let set = this.clone();
        SettingField::input(
            read(|k| k.config.journal.clone().into()),
            move |value, cx| set.update(cx, |k, cx| k.set_journal(&value, cx)),
        )
        .default_value(JOURNAL)
    };
    let daily = SettingPage::new("Daily notes")
        .icon(IconName::Calendar)
        .group(
            SettingGroup::new()
                .title("Journal")
                .item(
                    SettingItem::new("Folder", journal)
                        .description("Daily notes are <folder>/<YYYY-MM-DD>.md."),
                )
                .item(error),
        );

    let agent = agent_page(this, handle, model);

    let about =
        SettingPage::new("About")
            .icon(IconName::Info)
            .resettable(false)
            .group(SettingGroup::new().item(SettingItem::render({
                let this = this.clone();
                move |_, _, cx| {
                    let k = this.read(cx);
                    v_flex()
                        .gap_1()
                        .text_sm()
                        .child(format!("katib {}", env!("CARGO_PKG_VERSION")))
                        .child(format!("Vault: {}", k.vault.root().display()))
                        .child(format!("{} notes", k.notes.len()))
                        .child(div().text_color(cx.theme().muted_foreground).child(
                            "Settings are kept in .katib/config.toml and sync with the vault.",
                        ))
                }
            })));
    vec![sync, attachments, daily, agent, about]
}

/// The built-in agent's provider and model. They are this machine's, in katib's config folder,
/// not the vault's: keys must not be committed, and each machine has its own.
fn agent_page(this: &Entity<Katib>, handle: AnyWindowHandle, field: ModelField) -> SettingPage {
    const AUTO: &str = "auto";
    const BUILT_IN: &str = "builtin";
    const CUSTOM: &str = "custom";
    let provider = {
        let (get, set) = (this.clone(), this.clone());
        let mut options: Vec<(SharedString, SharedString)> = vec![(
            AUTO.into(),
            "Automatic: the first provider with a key".into(),
        )];
        options.extend(machine::providers().map(|p| (p.into(), p.into())));
        SettingField::dropdown(
            options,
            move |cx| {
                let agent = get.read(cx).agent.read(cx);
                agent.memory().provider().unwrap_or(AUTO).to_owned().into()
            },
            move |value, cx| {
                let agent = set.read(cx).agent.clone();
                let provider = (value.as_ref() != AUTO).then_some(value.as_ref());
                agent.update(cx, |a, cx| a.choose_provider(provider, cx));
                // The list needs the window, which this callback is inside.
                let katib = set.clone();
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, cx| {
                        katib.update(cx, |k, cx| k.load_agent_fields(window, cx));
                    });
                });
            },
        )
        .default_value(AUTO)
    };
    let chosen = this.clone();
    // The fields' state exists once Settings has opened, which is when this page is built.
    let input = |pick: fn(&AgentFields) -> &Entity<InputState>| {
        let this = this.clone();
        SettingField::render(move |_, _, cx| {
            let Some(fields) = &this.read(cx).agent_fields else {
                return div().into_any_element();
            };
            Input::new(pick(fields))
                .small()
                .w(px(320.))
                .into_any_element()
        })
    };
    let model = if field == ModelField::List {
        let this = this.clone();
        SettingField::render(move |_, _, cx| {
            let katib = this.read(cx);
            let Some(fields) = &katib.agent_fields else {
                return div().into_any_element();
            };
            let memory = katib.agent.read(cx).memory();
            let current = memory.provider().and_then(|p| memory.model(p));
            Select::new(&fields.models)
                .w(px(320.))
                .small()
                .accessibility_label("Model")
                .search_placeholder("Search models")
                .placeholder(current.unwrap_or_else(|| "Choose a model".into()))
                .into_any_element()
        })
    } else {
        input(|f| &f.model)
    };
    let base_url = input(|f| &f.base_url);
    let base_url_item = SettingItem::new("Base URL", base_url).description(
        "Empty: the provider's own, shown greyed. Set it for a server on another port or \
         machine, or a gateway; the provider's key is sent to it.",
    );
    let model_item = SettingItem::new("Model", model).description(
        "A model of the chosen provider. Unset: the one last used with it, or a local \
         server's only model.",
    );
    let model_note = SettingItem::render(move |_, _, cx| {
        let katib = chosen.read(cx);
        let note = match katib.agent.read(cx).memory().provider() {
            None => Some("Choose a provider to choose its model.".to_owned()),
            Some(_) => katib.agent_fields.as_ref().and_then(|f| f.note.clone()),
        };
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .children(note)
    });
    let key = SettingItem::new(
        "Key",
        SettingField::render({
            let this = this.clone();
            move |_, _, cx| {
                let agent = this.read(cx).agent.read(cx);
                let provider = agent.memory().provider().map(String::from);
                div().text_sm().child(key_status(provider.as_deref()))
            }
        }),
    );
    let agent = {
        let (get, set) = (this.clone(), this.clone());
        let mut options: Vec<(SharedString, SharedString)> =
            vec![(BUILT_IN.into(), "Built-in".into())];
        options.extend(
            katib_agent::acp::PRESETS
                .iter()
                .map(|p| (p.id.into(), p.name.into())),
        );
        options.push((CUSTOM.into(), "Custom command".into()));
        SettingField::dropdown(
            options,
            move |cx| {
                let agent = get.read(cx).agent.read(cx);
                agent
                    .memory()
                    .outside()
                    .unwrap_or(BUILT_IN)
                    .to_owned()
                    .into()
            },
            move |value, cx| {
                let agent = set.read(cx).agent.clone();
                let outside = (value.as_ref() != BUILT_IN).then_some(value.as_ref());
                agent.update(cx, |a, cx| a.choose_outside(outside, cx));
                let katib = set.clone();
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, cx| {
                        katib.update(cx, |k, cx| k.load_agent_fields(window, cx));
                    });
                });
            },
        )
        .default_value(BUILT_IN)
    };
    let agent_group =
        SettingGroup::new()
            .title("Agent")
            .item(SettingItem::new("Agent", agent).description(
                "Built-in runs katib's own agent on the provider below. An outside agent runs its \
             own command over ACP, with its own sign-in, model and tools.",
            ));
    if field == ModelField::Outside {
        let note = {
            let this = this.clone();
            SettingItem::render(move |_, _, cx| {
                let note = this
                    .read(cx)
                    .agent_fields
                    .as_ref()
                    .and_then(|f| f.command_note.clone());
                div().text_xs().text_color(cx.theme().danger).children(note)
            })
        };
        let group = agent_group
            .item(
                SettingItem::new("Command", input(|f| &f.command)).description(
                    "Run in the vault folder, without a shell; quote a word with spaces. Empty: \
                     the preset's, shown greyed. Applies on Enter.",
                ),
            )
            .item(note)
            .item(SettingItem::render(|_, _, cx| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "The agent's tools can reach outside the vault. katib answers its file \
                     requests inside the vault only, and offers it the note tools through \
                     `katib mcp`. A change starts a new conversation.",
                    )
            }));
        return SettingPage::new("Agent").icon(IconName::Bot).group(group);
    }
    let group = SettingGroup::new().title("Provider").item(
        SettingItem::new("Provider", provider).description(
            "OpenRouter, Anthropic and OpenAI need a key. Ollama and llama.cpp run on this \
             machine and need none.",
        ),
    );
    let group = if field == ModelField::Hidden {
        group
    } else {
        group.item(model_item).item(base_url_item)
    };
    let group = group
        .item(model_note)
        .item(key)
        .item(SettingItem::render(|_, _, cx| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(
                    "Kept on this machine, not in the vault. A change starts a new \
                     conversation.",
                )
        }));
    SettingPage::new("Agent")
        .icon(IconName::Bot)
        .group(agent_group)
        .group(group)
}

/// Whether the chosen provider's key is found, and where katib looks. Never the key itself.
fn key_status(provider: Option<&str>) -> String {
    let dir = machine::config_dir();
    let folder = dir.as_ref().map_or_else(
        || "katib's config folder".into(),
        |d| d.display().to_string(),
    );
    let Some(provider) = provider else {
        return format!(
            "Each provider reads its key variable, such as OPENAI_API_KEY, else \
             {folder}/<provider>.key."
        );
    };
    match machine::key_var(provider) {
        None => format!("{provider} needs no key."),
        Some(var) => match machine::key(dir.as_deref(), var) {
            Some(_) => format!("Found, in ${var} or {folder}/{provider}.key."),
            None => format!("Not found. Set ${var}, or save the key as {folder}/{provider}.key."),
        },
    }
}
