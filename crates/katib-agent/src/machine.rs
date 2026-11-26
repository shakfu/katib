//! Machine-local agent settings, in `dirs::config_dir()/katib/` (design.md section 2), never in
//! the vault: keys must not be committed, and each machine keeps its own provider and model.
//!
//! - `<provider>.key`: the provider's API key, read when its environment variable is unset.
//! - `agent.json`: the provider last used, and each provider's model and base URL.
//! - `models-*.json`: `minima-core`'s model list cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use minima_core::cache::Models;
use minima_core::provider::{Dialect, registry};
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

/// A provider's model ids, sorted, as `minima-core` last cached them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ModelList {
    pub ids: Vec<String>,
    /// The list is missing or a day old, and no fetch failed in the last hour.
    pub due: bool,
}

/// `provider`'s cached model list. Reads one file; never the network.
pub fn models(dir: Option<&Path>, provider: &str) -> ModelList {
    endpoint(dir, provider).map_or_else(ModelList::default, |base| listed(dir, &base))
}

/// Fetch `provider`'s model list into the cache. Blocks on the network, on a runtime of its
/// own, so call it off the UI thread. A failure holds off the next fetch for an hour.
pub fn fetch_models(dir: Option<&Path>, provider: &str) -> Result<Vec<String>, String> {
    let entry = registry::find(provider).ok_or_else(|| format!("unknown provider {provider}"))?;
    let key = match entry.key_env {
        Some(var) => key(dir, var).ok_or_else(|| format!("no key: set ${var}"))?,
        None => String::new(),
    };
    let base = endpoint(dir, provider).expect("found above");
    fetch(dir, &base, entry.dialect, &key)
}

/// `provider`'s registry base URL.
pub fn default_base_url(provider: &str) -> Option<&'static str> {
    registry::find(provider).map(|e| e.base_url)
}

/// The base URL `minima-core` resolves for `provider`: the one set here, else the registry's.
/// It keys the model cache, so each URL has its own list.
fn endpoint(dir: Option<&Path>, provider: &str) -> Option<String> {
    let set = Memory::load(dir).base_url(provider).map(String::from);
    let base = set.or_else(|| default_base_url(provider).map(String::from))?;
    Some(base.trim_end_matches('/').to_owned())
}

/// `url` as a base URL: trimmed, without a trailing `/`. `None` for blank, which means the
/// registry's. Only `http` and `https`, with a host, are accepted.
pub fn check_base_url(url: &str) -> Result<Option<String>, String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Ok(None);
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("{url}: a base URL starts with http:// or https://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || url.contains(char::is_whitespace) {
        return Err(format!("{url}: not a URL"));
    }
    Ok(Some(url.to_owned()))
}

fn listed(dir: Option<&Path>, base: &str) -> ModelList {
    let cache = Models::load(dir, base);
    let mut ids: Vec<String> = cache.ids().map(String::from).collect();
    ids.sort();
    ModelList {
        ids,
        due: cache.is_stale() && cache.retry_due(),
    }
}

fn fetch(
    dir: Option<&Path>,
    base: &str,
    dialect: Dialect,
    key: &str,
) -> Result<Vec<String>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut cache = Models::load(dir, base);
    match runtime.block_on(cache.refresh(base, key, dialect)) {
        Ok(()) => {
            let mut ids: Vec<String> = cache.ids().map(String::from).collect();
            ids.sort();
            Ok(ids)
        }
        Err(e) => {
            cache.record_failure();
            Err(format!("{e:#}"))
        }
    }
}

/// The provider and models last used. A file that cannot be read is an empty memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, String>,
    /// Set only where it differs from the registry's: a server on another port, a gateway.
    #[serde(default)]
    base_urls: BTreeMap<String, String>,
    /// The outside agent in use, by preset id or `custom`; none is the built-in agent.
    #[serde(default)]
    outside: Option<String>,
    /// Command lines edited from a preset's, and the custom one.
    #[serde(default)]
    commands: BTreeMap<String, String>,
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

    /// The outside agent chosen, by preset id or `custom`; `None` for the built-in agent.
    pub fn outside(&self) -> Option<&str> {
        self.outside.as_deref()
    }

    pub fn set_outside(&mut self, dir: Option<&Path>, outside: Option<&str>) {
        self.outside = outside.map(String::from);
        self.store(dir);
    }

    /// The command line for outside agent `id`: as edited, else its preset's.
    pub fn command(&self, id: &str) -> String {
        match self.commands.get(id) {
            Some(command) => command.clone(),
            None => crate::acp::PRESETS
                .iter()
                .find(|p| p.id == id)
                .map_or(String::new(), |p| p.command.to_owned()),
        }
    }

    /// The command line for outside agent `id`; blank goes back to its preset's. Refuses one
    /// that does not split into words, writing nothing.
    pub fn set_command(&mut self, dir: Option<&Path>, id: &str, line: &str) -> Result<(), String> {
        let line = line.trim();
        let preset = crate::acp::PRESETS.iter().find(|p| p.id == id);
        if line.is_empty() || preset.is_some_and(|p| p.command == line) {
            self.commands.remove(id);
        } else {
            crate::acp::split_command(line)?;
            self.commands.insert(id.to_owned(), line.to_owned());
        }
        self.store(dir);
        Ok(())
    }

    pub fn model(&self, provider: &str) -> Option<String> {
        self.models.get(provider).cloned()
    }

    pub fn base_url(&self, provider: &str) -> Option<&str> {
        self.base_urls.get(provider).map(String::as_str)
    }

    /// The base URL to use with `provider`; blank forgets it. Refuses one `check_base_url`
    /// does not accept, writing nothing.
    pub fn set_base_url(
        &mut self,
        dir: Option<&Path>,
        provider: &str,
        url: &str,
    ) -> Result<(), String> {
        match check_base_url(url)? {
            Some(url) => self.base_urls.insert(provider.to_owned(), url),
            None => self.base_urls.remove(provider),
        };
        self.store(dir);
        Ok(())
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
    fn the_outside_agent_and_its_command_round_trip() {
        let dir = Scratch::new("katib-agent-outside");
        let root = PathBuf::from(dir.file(""));
        let mut memory = Memory::load(Some(&root));
        assert_eq!(memory.outside(), None);
        assert_eq!(memory.command("minima"), "minima --acp");
        memory.set_outside(Some(&root), Some("minima"));
        memory
            .set_command(Some(&root), "minima", "/opt/minima --acp --model x")
            .unwrap();
        memory
            .set_command(Some(&root), "custom", "my-agent")
            .unwrap();
        assert!(memory.set_command(Some(&root), "custom", "a 'b").is_err());
        let mut memory = Memory::load(Some(&root));
        assert_eq!(memory.outside(), Some("minima"));
        assert_eq!(memory.command("minima"), "/opt/minima --acp --model x");
        assert_eq!(memory.command("custom"), "my-agent");
        // Blank, or the preset's own line, goes back to the preset.
        memory.set_command(Some(&root), "minima", " ").unwrap();
        assert_eq!(memory.command("minima"), "minima --acp");
        assert_eq!(memory.command("unknown"), "");
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

    /// Serve `body` as the answer to one HTTP request; the URL to reach it.
    fn serve_once(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        });
        url
    }

    #[test]
    fn a_fetched_model_list_is_cached_sorted() {
        let dir = Scratch::new("katib-agent-models");
        let root = PathBuf::from(dir.file(""));
        let base = serve_once(r#"{"data": [{"id": "zeta"}, {"id": "alpha"}]}"#);
        assert_eq!(
            listed(Some(&root), &base),
            ModelList {
                ids: vec![],
                due: true
            }
        );

        let ids = fetch(Some(&root), &base, Dialect::Chat, "").unwrap();
        assert_eq!(ids, ["alpha", "zeta"]);
        assert_eq!(
            listed(Some(&root), &base),
            ModelList {
                ids: vec!["alpha".into(), "zeta".into()],
                due: false
            }
        );
    }

    #[test]
    fn a_failed_fetch_holds_off_the_next() {
        let dir = Scratch::new("katib-agent-models-down");
        let root = PathBuf::from(dir.file(""));
        // A port nothing listens on: bound, then closed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let base = format!("http://127.0.0.1:{port}/v1");
        assert!(fetch(Some(&root), &base, Dialect::Chat, "").is_err());
        assert_eq!(
            listed(Some(&root), &base),
            ModelList {
                ids: vec![],
                due: false
            }
        );
    }

    #[test]
    fn base_urls_are_checked_and_key_the_model_cache() {
        assert_eq!(check_base_url("  "), Ok(None));
        assert_eq!(
            check_base_url(" http://box:9000/v1/ ").unwrap().as_deref(),
            Some("http://box:9000/v1")
        );
        for bad in [
            "box:9000",
            "ftp://box",
            "http://",
            "http:///v1",
            "http://a b",
        ] {
            assert!(check_base_url(bad).is_err(), "{bad}");
        }

        let dir = Scratch::new("katib-agent-base-url");
        let root = PathBuf::from(dir.file(""));
        let base = serve_once(r#"{"data": [{"id": "here"}]}"#);
        let mut memory = Memory::load(Some(&root));
        assert!(
            memory
                .set_base_url(Some(&root), "llamacpp", "nope")
                .is_err()
        );
        memory.set_base_url(Some(&root), "llamacpp", &base).unwrap();
        assert_eq!(fetch_models(Some(&root), "llamacpp").unwrap(), ["here"]);
        assert_eq!(models(Some(&root), "llamacpp").ids, ["here"]);

        // Back to the registry's URL, whose list is another file.
        Memory::load(Some(&root))
            .set_base_url(Some(&root), "llamacpp", "")
            .unwrap();
        assert_eq!(Memory::load(Some(&root)).base_url("llamacpp"), None);
        assert_eq!(models(Some(&root), "llamacpp").ids, Vec::<String>::new());
    }

    #[test]
    fn an_unknown_provider_has_no_models() {
        assert_eq!(models(None, "nope"), ModelList::default());
        assert!(fetch_models(None, "nope").is_err());
    }

    #[test]
    fn an_unreadable_memory_is_empty() {
        let dir = Scratch::new("katib-agent-memory-bad");
        let root = PathBuf::from(dir.file(""));
        fs::write(root.join("agent.json"), "{not json").unwrap();
        assert_eq!(Memory::load(Some(&root)).provider(), None);
    }
}
