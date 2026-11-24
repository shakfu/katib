//! The settings dialog. Settings are the vault's `.katib/config.toml`, so they
//! are committed and every clone of the vault shares them.

use gpui_kit::component::{
    ActiveTheme as _, IconName, WindowExt as _,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    v_flex,
};
use gpui_kit::*;
use katib_core::config::{Backend, Config, JOURNAL};

use super::{Katib, sync::SyncState};

impl Katib {
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_error = None;
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog
                .title("Settings")
                .w(px(820.))
                .content(move |content, _, _| {
                    content.child(
                        div()
                            .h(px(480.))
                            .child(Settings::new("settings").pages(pages(&this))),
                    )
                })
        });
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

fn pages(this: &Entity<Katib>) -> Vec<SettingPage> {
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
    vec![sync, attachments, daily, about]
}
