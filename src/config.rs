use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub llm: LlmConfig,
    /// Absent from configs written before agent mode existed, hence `default`.
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    /// How many rounds of "model asks for tools, we run them" one prompt may
    /// take before giving up. The backstop against a model that never concludes.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_secs: u64,
    /// Coding turns carry whole files; the old 4096 truncated them routinely.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_iterations() -> usize {
    25
}

fn default_shell_timeout() -> u64 {
    120
}

fn default_max_tokens() -> u32 {
    8192
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// Registry id from `providers::PROVIDERS` (e.g. "deepseek"), set by the
    /// `/provider` overlay. Empty means a custom/manually-entered endpoint, in
    /// which case a standalone `/model` has nothing to scope to.
    #[serde(default)]
    pub provider: String,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path();

        let mut config = if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            toml::from_str::<Config>(&contents).map_err(|e| {
                format!("{} is not valid TOML: {e}", config_path.display())
            })?
        } else {
            Self::default()
        };

        // Environment variables win over the file, so exporting a key works even
        // when a stale config.toml is on disk.
        if let Some(v) = env_var("TUISAMPLE_ENDPOINT") {
            config.llm.endpoint = v;
        }
        if let Some(v) = env_var("TUISAMPLE_MODEL") {
            config.llm.model = v;
        }
        if let Some(v) = env_var("TUISAMPLE_API_KEY") {
            config.llm.api_key = v;
        }

        config.normalize();
        Ok(config)
    }

    /// Trim stray whitespace. A trailing newline in an API key (very easy to get
    /// from `export KEY=$(cat file)`) produces an invalid Authorization header.
    fn normalize(&mut self) {
        self.llm.endpoint = self.llm.endpoint.trim().trim_end_matches('/').to_string();
        self.llm.model = self.llm.model.trim().to_string();
        self.llm.api_key = self.llm.api_key.trim().to_string();
        self.llm.provider = self.llm.provider.trim().to_string();

        let d = LlmConfig::default();
        if self.llm.endpoint.is_empty() {
            self.llm.endpoint = d.endpoint;
        }
        if self.llm.model.is_empty() {
            self.llm.model = d.model;
        }

        // A hand-edited 0 would make every prompt fail before it started.
        self.agent.max_iterations = self.agent.max_iterations.clamp(1, 200);
        self.agent.shell_timeout_secs = self.agent.shell_timeout_secs.clamp(1, 600);
        self.agent.max_tokens = self.agent.max_tokens.max(256);
    }

    /// Human-readable reasons the app cannot talk to an endpoint yet, shown on
    /// the welcome screen rather than failing silently on the first prompt.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.llm.api_key.is_empty() {
            warnings.push(
                "No API key set. Export TUISAMPLE_API_KEY or add api_key to ~/.tuisample-code/config.toml."
                    .to_string(),
            );
        }
        if !self.llm.endpoint.starts_with("http://") && !self.llm.endpoint.starts_with("https://") {
            warnings.push(format!(
                "Endpoint '{}' has no http:// or https:// scheme.",
                self.llm.endpoint
            ));
        }
        warnings
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = Self::config_path();

        // Create directory if it doesn't exist
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let toml_str = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, toml_str)?;
        Ok(())
    }

    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".tuisample-code").join("config.toml")
    }
}

fn env_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            shell_timeout_secs: default_shell_timeout(),
            max_tokens: default_max_tokens(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:8000".to_string(),
            model: "gpt-3.5-turbo".to_string(),
            api_key: String::new(),
            provider: String::new(),
        }
    }
}

/// Test-only helper for isolating `$HOME` so `Config::save()`/`load()` never
/// touch the real developer/CI home directory. Shared across every test module
/// in the crate (this is a single binary crate with one test binary, so `$HOME`
/// is genuinely global process state -- two independent per-module mutexes
/// would not actually serialize against each other).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn with_isolated_home<R>(f: impl FnOnce() -> R) -> R {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("failed to create temp HOME");
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());

        let result = f();

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::with_isolated_home;
    use super::*;

    #[test]
    fn save_then_load_round_trips_all_llm_fields_including_provider() {
        with_isolated_home(|| {
            // TUISAMPLE_* env vars win over the file (by design), so a developer
            // machine that happens to have one exported must not leak into this
            // assertion.
            let saved_env: Vec<(&str, Option<String>)> =
                ["TUISAMPLE_ENDPOINT", "TUISAMPLE_MODEL", "TUISAMPLE_API_KEY"]
                    .iter()
                    .map(|&k| (k, std::env::var(k).ok()))
                    .collect();
            for (k, _) in &saved_env {
                std::env::remove_var(k);
            }

            let config = Config {
                llm: LlmConfig {
                    endpoint: "https://api.deepseek.com".to_string(),
                    model: "deepseek-v4-pro".to_string(),
                    api_key: "sk-test-key".to_string(),
                    provider: "deepseek".to_string(),
                },
                ..Config::default()
            };
            config.save().expect("save should succeed");

            let loaded = Config::load().expect("load should succeed");
            assert_eq!(loaded.llm.endpoint, "https://api.deepseek.com");
            assert_eq!(loaded.llm.model, "deepseek-v4-pro");
            assert_eq!(loaded.llm.api_key, "sk-test-key");
            assert_eq!(loaded.llm.provider, "deepseek");

            for (k, v) in saved_env {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        });
    }

    /// Configs written by 0.3.0 and earlier have no [agent] table at all.
    #[test]
    fn a_config_without_an_agent_section_still_loads_with_defaults() {
        let parsed: Config = toml::from_str(
            r#"
            [llm]
            endpoint = "https://api.deepseek.com"
            model = "deepseek-v4-pro"
            "#,
        )
        .expect("an [agent]-less config must still parse");

        assert_eq!(parsed.agent.max_iterations, 25);
        assert_eq!(parsed.agent.shell_timeout_secs, 120);
        assert_eq!(parsed.agent.max_tokens, 8192);
    }

    #[test]
    fn agent_settings_are_clamped_to_usable_values() {
        let mut config = Config {
            agent: AgentConfig {
                max_iterations: 0,
                shell_timeout_secs: 100_000,
                max_tokens: 1,
            },
            ..Config::default()
        };
        config.normalize();

        // 0 iterations would fail every prompt before it started.
        assert_eq!(config.agent.max_iterations, 1);
        assert_eq!(config.agent.shell_timeout_secs, 600);
        assert_eq!(config.agent.max_tokens, 256);
    }

    #[test]
    fn save_creates_the_parent_directory_if_missing() {
        with_isolated_home(|| {
            assert!(!Config::config_path().parent().unwrap().exists());
            Config::default().save().expect("save should succeed");
            assert!(Config::config_path().exists());
        });
    }
}
