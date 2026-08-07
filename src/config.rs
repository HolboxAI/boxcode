use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub llm: LlmConfig,
    /// `default` is load-bearing: every config.toml written before file tools
    /// existed has no `[tools]` table, and without it those files stop parsing
    /// the moment a user upgrades.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// `default` for the same reason as `[tools]`: every config.toml written
    /// before this existed has no `[quota]` table, and without it those files
    /// stop parsing the moment a user upgrades.
    #[serde(default)]
    pub quota: QuotaConfig,
    /// Same again for `[free_tier]`, newer still.
    #[serde(default)]
    pub free_tier: FreeTierConfig,
    /// Same again for `[ui]`.
    #[serde(default)]
    pub ui: UiConfig,
}

/// Anonymous free-tier enrolment. See `freetier.rs`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreeTierConfig {
    /// False stops this install ever contacting the gateway.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Base URL of the gateway.
    #[serde(default = "default_gateway")]
    pub gateway: String,
    /// Device token from `/register`. Not a provider key: it only spends this
    /// device's daily allowance and is useless for anything else.
    #[serde(default)]
    pub device_token: String,
    /// Stable identifier for machines with no readable hardware id, persisted so
    /// such a machine does not draw a fresh budget on every launch.
    #[serde(default)]
    pub fallback_id: String,
}

fn default_gateway() -> String {
    crate::freetier::DEFAULT_GATEWAY.to_string()
}

impl Default for FreeTierConfig {
    fn default() -> Self {
        Self {
            enabled: yes(),
            gateway: default_gateway(),
            device_token: String::new(),
            fallback_id: String::new(),
        }
    }
}

/// Optional daily ceilings. See `quota.rs`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaConfig {
    /// Off means no counting, no persistence, and no enforcement.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Requests per UTC day. One prompt can spend several: a tool round trip is
    /// another request to the endpoint and is counted as one.
    #[serde(default)]
    pub max_requests_per_day: u64,
    /// Prompt + completion tokens per UTC day.
    #[serde(default)]
    pub max_tokens_per_day: u64,
    /// Dollars per UTC day. Only meaningful for models priced below; usage on an
    /// unpriced model cannot contribute to it.
    #[serde(default)]
    pub max_usd_per_day: f64,
    /// Percentage of a limit at which the UI starts warning.
    #[serde(default = "default_warn_at")]
    pub warn_at_percent: u8,
    /// Ask the endpoint for exact token counts via `stream_options`. Turn off
    /// for endpoints that reject the field; counts then fall back to the same
    /// character estimate `usage.rs` uses, marked as such wherever shown.
    #[serde(default = "yes")]
    pub include_usage: bool,
    /// Per-model prices in USD per million tokens, keyed by the exact model
    /// name sent on the wire:
    ///
    /// ```toml
    /// [quota.pricing."deepseek-v4-flash"]
    /// input_per_mtok = 0.14
    /// output_per_mtok = 0.28
    /// ```
    ///
    /// Empty by default and deliberately so: shipping a built-in table would
    /// mean guessing at prices that change without notice and do not exist for
    /// local models. A model with no entry has its tokens counted and its cost
    /// reported as unknown.
    #[serde(default)]
    pub pricing: std::collections::HashMap<String, crate::quota::ModelPrice>,
}

fn default_warn_at() -> u8 {
    80
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            enabled: yes(),
            max_requests_per_day: 0,
            max_tokens_per_day: 0,
            max_usd_per_day: 0.0,
            warn_at_percent: default_warn_at(),
            include_usage: yes(),
            pricing: std::collections::HashMap::new(),
        }
    }
}

impl QuotaConfig {
    pub fn price_for(&self, model: &str) -> Option<crate::quota::ModelPrice> {
        self.pricing.get(model).copied()
    }

    /// True when at least one ceiling is actually set.
    pub fn has_limits(&self) -> bool {
        self.enabled
            && (self.max_requests_per_day > 0
                || self.max_tokens_per_day > 0
                || self.max_usd_per_day > 0.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// Ceiling on one reply's length. The old hard-coded 4096 was fine for chat
    /// and far too small the moment the model started producing whole files: a
    /// long write is simply cut off mid-token, and the endpoint reports that as
    /// a finished reply.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Registry id from `providers::PROVIDERS` (e.g. "deepseek"), set by the
    /// `/provider` overlay. Empty means a custom/manually-entered endpoint, in
    /// which case a standalone `/model` has nothing to scope to.
    #[serde(default)]
    pub provider: String,
}

/// Appearance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiConfig {
    /// `auto`, `dark` or `light`.
    ///
    /// The colours have to suit the terminal's background, and the app never
    /// paints one -- it draws on whatever is already there. `auto` asks the
    /// terminal and falls back to a palette that is legible either way, which
    /// is safe but less vivid than saying outright which one you use.
    #[serde(default = "default_theme")]
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { theme: default_theme() }
    }
}

fn default_theme() -> String {
    "auto".to_string()
}

/// Settings for the shell command tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolsConfig {
    /// Off means the tool schema is never sent, which is also the escape hatch
    /// for endpoints that reject requests carrying a `tools` field at all.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Working directory commands run in. "." means the directory the app was
    /// launched from. Not a sandbox -- a command can `cd` out of it.
    #[serde(default = "dot")]
    pub workspace: String,
    /// Ask before every command. This is the only real control over what the
    /// model does to the machine, so turning it off is a deliberate act: the
    /// model can then delete files with no prompt at all.
    #[serde(default = "yes")]
    pub require_approval: bool,
    /// Skip the approval prompt for a short allowlist of read-only commands
    /// (`ls`, `cat`, `grep`, `git status`/`diff`/`log`/`show`, ...) so the
    /// prompt stays meaningful for the commands that can actually change
    /// something. See `tools::is_read_only` for exactly what qualifies --
    /// anything not obviously read-only still asks, `require_approval` still
    /// governs everything else, and `false` here just means "ask about those
    /// too."
    #[serde(default = "yes")]
    pub auto_approve_read_only: bool,
    /// How long a single command may run before it is killed.
    #[serde(default = "default_command_timeout")]
    pub command_timeout_secs: u64,
    /// Ceiling on the output of one command, so `find /` cannot eat the whole
    /// context window.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    /// How many command rounds one prompt may take before the schema is withheld
    /// and the model is made to answer. Without this a model that keeps running
    /// commands burns tokens until the user notices.
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    /// The Python interpreter `web_search` shells out to. Not on every machine's
    /// `PATH` under this name (some only have `python`, some want a specific
    /// venv), so it is a setting rather than hardcoded -- and pointing it at a
    /// deliberately broken path is also how tests simulate "Python is missing"
    /// without needing to uninstall anything.
    #[serde(default = "default_python_bin")]
    pub python_bin: String,
    /// How long `web_search` may wait for a search before it is killed.
    /// Separate from `command_timeout_secs`: starting a Python interpreter and
    /// making a network request is routinely slower than the shell commands
    /// that budget was sized for.
    #[serde(default = "default_search_timeout")]
    pub search_timeout_secs: u64,
}

fn yes() -> bool {
    true
}

fn dot() -> String {
    ".".to_string()
}

fn default_max_tokens() -> u32 {
    16384
}

fn default_command_timeout() -> u64 {
    60
}

fn default_max_output_bytes() -> usize {
    64 * 1024
}

fn default_max_steps() -> usize {
    // 10 was set when the model had three tools and made one-shot edits. With
    // six tools and an agentic loop, real work blows through ten rounds while
    // it is still gathering context -- and the failure mode is bad: the schemas
    // are withheld mid-task, so the model writes its next call out as text and
    // the turn dies. See `App::finish_stream`.
    40
}

fn default_python_bin() -> String {
    "python3".to_string()
}

fn default_search_timeout() -> u64 {
    20
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: yes(),
            workspace: dot(),
            require_approval: yes(),
            auto_approve_read_only: yes(),
            command_timeout_secs: default_command_timeout(),
            max_output_bytes: default_max_output_bytes(),
            max_steps: default_max_steps(),
            python_bin: default_python_bin(),
            search_timeout_secs: default_search_timeout(),
        }
    }
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
        if let Some(v) = env_var("TUISAMPLE_WORKSPACE") {
            config.tools.workspace = v;
        }
        if let Some(v) = env_var("TUISAMPLE_TOOLS_ENABLED") {
            config.tools.enabled = truthy(&v);
        }
        // Exists so an automated test can drive the loop without a human at the
        // keyboard. Setting it in normal use hands the model an unattended shell.
        if let Some(v) = env_var("TUISAMPLE_GATEWAY") {
            config.free_tier.gateway = v;
        }
        if let Some(v) = env_var("TUISAMPLE_FREE_TIER") {
            config.free_tier.enabled = truthy(&v);
        }
        if let Some(v) = env_var("TUISAMPLE_QUOTA_ENABLED") {
            config.quota.enabled = truthy(&v);
        }
        for (name, slot) in [
            ("TUISAMPLE_MAX_REQUESTS_PER_DAY", 0),
            ("TUISAMPLE_MAX_TOKENS_PER_DAY", 1),
        ] {
            if let Some(v) = env_var(name) {
                if let Ok(n) = v.trim().parse::<u64>() {
                    match slot {
                        0 => config.quota.max_requests_per_day = n,
                        _ => config.quota.max_tokens_per_day = n,
                    }
                }
            }
        }
        if let Some(v) = env_var("TUISAMPLE_MAX_USD_PER_DAY") {
            if let Ok(n) = v.trim().parse::<f64>() {
                config.quota.max_usd_per_day = n;
            }
        }
        if let Some(v) = env_var("TUISAMPLE_TOOLS_APPROVAL") {
            config.tools.require_approval = truthy(&v);
        }

        config.normalize();
        config.normalize_quota();
        Ok(config)
    }

    /// Trim stray whitespace. A trailing newline in an API key (very easy to get
    /// from `export KEY=$(cat file)`) produces an invalid Authorization header.
    fn normalize(&mut self) {
        self.llm.endpoint = self.llm.endpoint.trim().trim_end_matches('/').to_string();
        self.llm.model = self.llm.model.trim().to_string();
        self.llm.api_key = self.llm.api_key.trim().to_string();
        self.llm.provider = self.llm.provider.trim().to_string();

        // A typo here must not silently mean "dark": fall back to `auto`,
        // which is the variant that cannot be unreadable.
        self.ui.theme = self.ui.theme.trim().to_ascii_lowercase();
        if !matches!(self.ui.theme.as_str(), "auto" | "dark" | "light") {
            self.ui.theme = default_theme();
        }
        // 0 would make every reply empty; the ceiling is a sanity bound, not a
        // claim about what any particular endpoint accepts.
        self.llm.max_tokens = self.llm.max_tokens.clamp(256, 200_000);

        let d = LlmConfig::default();
        if self.llm.endpoint.is_empty() {
            self.llm.endpoint = d.endpoint;
        }
        if self.llm.model.is_empty() {
            self.llm.model = d.model;
        }

        self.tools.workspace = self.tools.workspace.trim().to_string();
        if self.tools.workspace.is_empty() {
            self.tools.workspace = dot();
        }
        // A hand-edited `max_output_bytes = 0` would make every command look
        // like it printed nothing, which reads as a broken tool rather than a
        // bad setting.
        self.tools.max_output_bytes = self.tools.max_output_bytes.clamp(1024, 8 * 1024 * 1024);
        self.tools.command_timeout_secs = self.tools.command_timeout_secs.clamp(1, 3600);
        self.tools.max_steps = self.tools.max_steps.clamp(1, 50);

        self.tools.python_bin = self.tools.python_bin.trim().to_string();
        if self.tools.python_bin.is_empty() {
            self.tools.python_bin = default_python_bin();
        }
        self.tools.search_timeout_secs = self.tools.search_timeout_secs.clamp(1, 300);
    }

    /// Human-readable reasons the app cannot talk to an endpoint yet, shown on
    /// the welcome screen rather than failing silently on the first prompt.
    /// Nonsense quota settings are clamped rather than obeyed. A warn threshold
    /// of 0 would fire before the first request; above 100 it could never fire.
    fn normalize_quota(&mut self) {
        self.free_tier.gateway = self.free_tier.gateway.trim().trim_end_matches('/').to_string();
        self.free_tier.device_token = self.free_tier.device_token.trim().to_string();
        if self.free_tier.gateway.is_empty() {
            self.free_tier.gateway = default_gateway();
        }
        self.quota.warn_at_percent = self.quota.warn_at_percent.clamp(1, 100);
        if self.quota.max_usd_per_day < 0.0 || !self.quota.max_usd_per_day.is_finite() {
            self.quota.max_usd_per_day = 0.0;
        }
        // A negative price would credit the user for using the model.
        self.quota.pricing.retain(|_, p| {
            p.input_per_mtok >= 0.0
                && p.output_per_mtok >= 0.0
                && p.input_per_mtok.is_finite()
                && p.output_per_mtok.is_finite()
        });
    }

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

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:8000".to_string(),
            model: "gpt-3.5-turbo".to_string(),
            api_key: String::new(),
            max_tokens: default_max_tokens(),
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

    // pub(crate), not private: tools.rs's own embedded_python_path tests
    // need to hold this too (see there) -- an async test can't route
    // through with_isolated_home below without nesting a second tokio
    // runtime inside the first, which panics, so it locks this directly
    // instead.
    pub(crate) static HOME_LOCK: Mutex<()> = Mutex::new(());

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
                quota: QuotaConfig::default(),
                free_tier: FreeTierConfig::default(),
                ui: UiConfig::default(),
                llm: LlmConfig {
                    endpoint: "https://api.deepseek.com".to_string(),
                    model: "deepseek-v4-pro".to_string(),
                    api_key: "sk-test-key".to_string(),
                    max_tokens: 8192,
                    provider: "deepseek".to_string(),
                },
                tools: ToolsConfig::default(),
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

    /// Regression guard: every config.toml written before file tools existed has
    /// no `[tools]` table. If that stops parsing, upgrading bricks the app for
    /// every existing user.
    #[test]
    fn a_config_written_before_file_tools_existed_still_loads() {
        with_isolated_home(|| {
            let path = Config::config_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                "[llm]\nendpoint = \"https://api.deepseek.com\"\nmodel = \"deepseek-chat\"\napi_key = \"sk-old\"\n",
            )
            .unwrap();

            let loaded = Config::load().expect("a pre-tools config must still load");
            assert_eq!(loaded.llm.model, "deepseek-chat");
            assert!(loaded.tools.enabled);
            assert_eq!(loaded.tools.workspace, ".");
            assert_eq!(loaded.tools.max_steps, default_max_steps());
            // The safe default has to survive an absent table, or upgrading
            // silently hands existing users an unattended shell.
            assert!(loaded.tools.require_approval);
            assert_eq!(loaded.tools.python_bin, "python3");
            assert_eq!(loaded.tools.search_timeout_secs, 20);
        });
    }

    /// Same again for a config that has a `[tools]` table but no `python_bin`
    /// or `search_timeout_secs` key -- anyone who edited the table by hand
    /// before `web_search` existed.
    #[test]
    fn a_tools_table_without_web_search_settings_still_defaults() {
        let parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[tools]\nenabled = true\nmax_steps = 3\n",
        )
        .expect("should parse");
        assert_eq!(parsed.tools.python_bin, "python3");
        assert_eq!(parsed.tools.search_timeout_secs, 20);
    }

    /// Same again for a config that has a `[tools]` table but no
    /// `require_approval` key -- anyone who edited the table by hand.
    #[test]
    fn a_tools_table_without_require_approval_still_defaults_to_asking() {
        let parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[tools]\nenabled = true\nmax_steps = 3\n",
        )
        .expect("should parse");
        assert!(parsed.tools.require_approval);
    }

    /// Same again for `auto_approve_read_only` -- an old table or a hand-edited
    /// one without the key must still get the safe (and useful) default.
    #[test]
    fn a_tools_table_without_auto_approve_read_only_still_defaults_to_true() {
        let parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[tools]\nenabled = true\nmax_steps = 3\n",
        )
        .expect("should parse");
        assert!(parsed.tools.auto_approve_read_only);
    }

    #[test]
    fn nonsense_tool_limits_are_clamped_to_something_usable() {
        let mut config = Config::default();
        config.tools.max_output_bytes = 0;
        config.tools.command_timeout_secs = 0;
        config.tools.max_steps = 0;
        config.tools.workspace = "  ".to_string();
        config.tools.python_bin = "  ".to_string();
        config.tools.search_timeout_secs = 0;
        config.normalize();

        assert_eq!(config.tools.max_output_bytes, 1024);
        assert_eq!(config.tools.command_timeout_secs, 1);
        assert_eq!(config.tools.max_steps, 1);
        assert_eq!(config.tools.workspace, ".");
        assert_eq!(config.tools.python_bin, "python3");
        assert_eq!(config.tools.search_timeout_secs, 1);
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
