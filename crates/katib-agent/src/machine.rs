//! Machine-local agent settings, in `dirs::config_dir()/katib/` (design.md section 2), never in
//! the vault: keys must not be committed, and each machine keeps its own provider and model.
//!
//! - `<provider>.key`: the provider's API key, read when its environment variable is unset.
//! - `agent.json`: the provider last used, and the model last used with each provider.
//! - `models-*.json`: `minima-core`'s model list cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use minima_core::provider::registry;
use serde::{Deserialize, Serialize};

/// `dirs::config_dir()/katib`: `~/Library/Application Support/katib` on macOS,
/// `$XDG_CONFIG_HOME/katib` on Linux, `%APPDATA%\katib` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("katib"))
}

/// Every provider id, in the order autoselect tries them.
pub fn providers() -> impl Iterator<Item = &'static str> {
    registry::ALL.iter().map(|e| e.id)
}

/// The variable a provider's key is read from; `None` for a local server.
pub fn key_var(provider: &str) -> Option<&'static str> {
    registry::find(provider)?.key_env
}

/// A key variable's value: the environment's, else the provider's key file in `dir`. Empty
/// values count as unset.
pub fn key(dir: Option<&Path>, var: &str) -> Option<String> {
    let set = |v: String| Some(v.trim().to_owned()).filter(|v| !v.is_empty());
    if let Some(value) = std::env::var(var).ok().and_then(set) {
        return Some(value);
    }
    let provider = registry::ALL.iter().find(|e| e.key_env == Some(var))?;
    fs::read_to_string(dir?.join(format!("{}.key", provider.id)))
        .ok()
        .and_then(set)
}

/// The provider and models last used. A file that cannot be read is an empty memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, String>,
}

impl Memory {
    pub fn load(dir: Option<&Path>) -> Self {
        dir.and_then(|d| fs::read_to_string(d.join("agent.json")).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn model(&self, provider: &str) -> Option<String> {
        self.models.get(provider).cloned()
    }

    /// Records the pairing and writes it.
    pub fn remember(&mut self, dir: Option<&Path>, provider: &str, model: &str) {
        self.provider = Some(provider.to_owned());
        self.models.insert(provider.to_owned(), model.to_owned());
        self.store(dir);
    }

    /// The provider to start with; `None` autoselects.
    pub fn set_provider(&mut self, dir: Option<&Path>, provider: Option<&str>) {
        self.provider = provider.map(String::from);
        self.store(dir);
    }

    /// The model to use with `provider`; `None` or blank forgets it.
    pub fn set_model(&mut self, dir: Option<&Path>, provider: &str, model: Option<&str>) {
        match model.map(str::trim).filter(|m| !m.is_empty()) {
            Some(model) => self.models.insert(provider.to_owned(), model.to_owned()),
            None => self.models.remove(provider),
        };
        self.store(dir);
    }

    /// Writes the memory, owner-only. Best effort: a memory that cannot be written only costs
    /// the next start a choice.
    fn store(&self, dir: Option<&Path>) {
        let Some(dir) = dir else { return };
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        let path = dir.join("agent.json");
        if katib_core::write_atomic(&path, &text).is_ok() {
            let _ = minima_core::fs::restrict_to_owner(dir);
            let _ = minima_core::fs::restrict_to_owner(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use minima_core::tools::Scratch;

    use super::*;

    #[test]
    fn a_key_file_answers_for_its_provider_only_when_the_variable_is_unset() {
        let dir = Scratch::new("katib-agent-keys");
        let root = PathBuf::from(dir.file(""));
        fs::write(root.join("openrouter.key"), " sk-file \n").unwrap();
        fs::write(root.join("openai.key"), "\n").unwrap();

        // A variable no test environment sets, so the file decides. The registry's own
        // variables may be set on a developer's machine.
        assert_eq!(key(Some(&root), "KATIB_TEST_UNSET_VAR"), None);
        let var = "OPENROUTER_API_KEY";
        if std::env::var(var).is_err() {
            assert_eq!(key(Some(&root), var).as_deref(), Some("sk-file"));
        }
        if std::env::var("OPENAI_API_KEY").is_err() {
            assert_eq!(key(Some(&root), "OPENAI_API_KEY"), None, "blank is unset");
        }
        assert_eq!(key(None, "KATIB_TEST_UNSET_VAR"), None);
    }

    #[test]
    fn the_memory_round_trips_per_provider() {
        let dir = Scratch::new("katib-agent-memory");
        let root = PathBuf::from(dir.file(""));
        let mut memory = Memory::load(Some(&root));
        assert_eq!(memory.provider(), None);

        memory.remember(Some(&root), "openai", "gpt-a");
        memory.remember(Some(&root), "anthropic", "claude-b");
        let mut memory = Memory::load(Some(&root));
        assert_eq!(memory.provider(), Some("anthropic"));
        assert_eq!(memory.model("openai").as_deref(), Some("gpt-a"));
        assert_eq!(memory.model("ollama"), None);

        memory.set_provider(Some(&root), None);
        memory.set_model(Some(&root), "openai", Some("  "));
        memory.set_model(Some(&root), "llamacpp", Some("qwen"));
        let memory = Memory::load(Some(&root));
        assert_eq!(memory.provider(), None);
        assert_eq!(memory.model("openai"), None, "blank forgets");
        assert_eq!(memory.model("llamacpp").as_deref(), Some("qwen"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.join("agent.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn an_unreadable_memory_is_empty() {
        let dir = Scratch::new("katib-agent-memory-bad");
        let root = PathBuf::from(dir.file(""));
        fs::write(root.join("agent.json"), "{not json").unwrap();
        assert_eq!(Memory::load(Some(&root)).provider(), None);
    }
}
