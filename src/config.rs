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
    /// Same again for `[ui]`.
    #[serde(default)]
    pub ui: UiConfig,
    /// Same again for `[deploy]`.
    #[serde(default)]
    pub deploy: DeployConfig,
    /// Same again for `[update]`.
    #[serde(default)]
    pub update: UpdateConfig,
    /// Same again for `[compact]`.
    #[serde(default)]
    pub compact: CompactConfig,
}

/// When the conversation compacts itself. `/compact` by hand is always
/// available regardless of these settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactConfig {
    /// Compact automatically when the context passes `auto_at_tokens`.
    ///
    /// On by default because the failure it prevents is a slow one nobody
    /// chose: every turn of a long session resends the whole transcript, so
    /// the session gets more expensive and eventually stops fitting the
    /// model's window at all -- and by the time that error appears, the cheap
    /// moment to summarise is long gone.
    #[serde(default = "yes")]
    pub auto: bool,
    /// The context size, in tokens, that triggers it. Exact when the endpoint
    /// reports prompt tokens, estimated at 4 chars/token otherwise.
    ///
    /// The default suits models with a 128k window or more: late enough that
    /// short sessions never see it, early enough to leave room for the turns
    /// after the summary.
    #[serde(default = "default_auto_at_tokens")]
    pub auto_at_tokens: u64,
}

fn default_auto_at_tokens() -> u64 {
    80_000
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            auto: yes(),
            auto_at_tokens: default_auto_at_tokens(),
        }
    }
}

/// Whether launching boxcode should notice that a newer release exists.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// Check for a newer release on startup, and offer to install it.
    ///
    /// Defaults to on, because the failure it prevents is silent: a user
    /// stays on a build with a fixed bug still in it, and nothing ever tells
    /// them. It is a single small request with a short timeout, and every
    /// failure is ignored -- see `upgrade::check_on_start`.
    ///
    /// Worth turning off on a machine with no route to github.com, where the
    /// check can only ever fail, and in CI, where nothing should be prompting.
    #[serde(default = "yes")]
    pub check_on_start: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_on_start: yes(),
        }
    }
}

/// Settings for `/deploy`. See `src/deploy/`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployConfig {
    /// Off removes `/deploy` and `/deployments` from the command list
    /// entirely, for anyone who would rather this feature did not exist.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Whether a missing provider CLI may be installed from inside the app.
    /// `false` does not make the flow fail silently -- it explains what to
    /// install and stops. Nothing is ever installed without confirmation
    /// regardless of this setting; this only controls whether the offer is
    /// made at all, for machines where global installs are somebody else's
    /// decision.
    #[serde(default = "yes")]
    pub allow_cli_install: bool,
    /// How many past deployments `/deployments` prints.
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
}

fn default_history_limit() -> usize {
    10
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            enabled: yes(),
            allow_cli_install: yes(),
            history_limit: default_history_limit(),
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
    /// Sampling temperature sent on the wire. `None` means "say nothing and
    /// let the endpoint use its own default" -- most OpenAI-compatible
    /// servers already default to something reasonable, and a blanket value
    /// here would override that silently for every provider, not just the
    /// one it was chosen for.
    ///
    /// Left unset in `config.toml`, the effective value still is not always
    /// `None`: `effective_temperature` falls back to a per-provider default
    /// (currently only DeepSeek's) for the same reason `max_tokens` has one
    /// -- so the good setting is what a new install gets without anyone
    /// having to discover the knob first. Set explicitly here to override
    /// that default in either direction, including back to "send nothing"
    /// by leaving it unset and picking a provider with no opinion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl LlmConfig {
    /// The temperature to actually send: the configured value if there is
    /// one, otherwise this provider's built-in default (see
    /// `providers::default_temperature`), which is `None` for every provider
    /// without a specific reason to disagree with the endpoint's own default.
    pub fn effective_temperature(&self) -> Option<f32> {
        self.temperature.or_else(|| crate::providers::default_temperature(&self.provider))
    }
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
    /// Wipe the terminal on the way out, so quitting leaves it as it was
    /// found rather than with the whole conversation still sitting there.
    ///
    /// On by default: a session is a working scratchpad, and the state most
    /// people want after closing one is the shell they started from. It is a
    /// setting rather than a fixed behaviour because it is genuinely a
    /// trade -- see the caveat below.
    ///
    /// **This clears the scrollback, not only the visible screen**, which is
    /// what makes the difference between "the conversation is gone" and "the
    /// conversation is one scroll away". That is the same thing `clear` does
    /// on most terminals, and it has the same consequence: anything that was
    /// in the scrollback *before* boxcode started goes with it. Set this to
    /// `false` on a terminal whose history you rely on.
    ///
    /// Never applied on the way out of a failure. An error message that
    /// erased itself would be worse than no error message, since at least a
    /// missing one leaves the exit code.
    #[serde(default = "yes")]
    pub clear_on_exit: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            clear_on_exit: yes(),
        }
    }
}

fn default_theme() -> String {
    "auto".to_string()
}

/// Which actions stop and wait for a yes/no.
///
/// This is the posture of the whole tool layer, and it is deliberately two
/// values rather than a dial. The question a person actually has is "does it
/// interrupt me for ordinary work, or only for the things I would want to
/// catch" -- and every finer gradation between those was, in practice, a way
/// to be asked about reading a file.
///
/// **Neither value reaches the blocklist.** `rm -rf /`, disk formatting, fork
/// bombs and `curl | sh` are refused outright in both, and there is no
/// setting, key or flag that changes that -- see `danger.rs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Ask only about actions that destroy something or put something on the
    /// public internet -- the `Risk::Dangerous` tier in `danger.rs`, plus a
    /// plan, which is what hands the writing tools back.
    ///
    /// The default, and the reason is what the alternative did to a real
    /// session. Building anything -- a web app, a service, a migration -- is
    /// dozens of ordinary steps: `mkdir`, `npm install`, `npm run build`,
    /// `cargo test`, and a file written for each one. Asking about every one
    /// of those does not make the destructive ones safer; it buries them.
    /// Twenty identical prompts in a row are answered `y` by reflex, and the
    /// twenty-first, the one that mattered, is answered the same way.
    ///
    /// Writes and edits are included in "ordinary" on purpose. Both are
    /// confined to the workspace by `tools::resolve_in_workspace`, neither can
    /// invoke a shell, and a file this tool wrote is a file `git diff` shows
    /// and `git checkout` undoes. Deleting is the irreversible half, and
    /// deleting is still in the tier that asks.
    #[default]
    Destructive,
    /// Ask about every write and every command, sparing only the reads.
    ///
    /// What the default used to be. Reading, listing, globbing, grepping and
    /// the read-only command allowlist stay silent -- being asked whether a
    /// file may be *read* was never protecting anything -- and everything else
    /// waits for a decision.
    Always,
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
    /// Which actions stop for a yes/no.
    ///
    /// See [`ApprovalMode`]. The default asks only about the things that
    /// cannot be undone; the blocklist in `danger.rs` is unaffected by this
    /// setting in either direction and refuses the catastrophic tier outright.
    #[serde(default)]
    pub approval: ApprovalMode,
    /// Retired in favour of `approval`. Read so an old config still loads,
    /// then discarded.
    ///
    /// Deliberately **not** carried over, and the reason is that it cannot be
    /// read as a choice. `ToolsConfig` used to serialize every field, so every
    /// `save` -- `/provider`, `/model`, the first-run setup -- wrote
    /// `require_approval = true` into the file. Its presence says the app
    /// saved a config once, not that anyone asked for anything: `true` was
    /// already the default, so writing it by hand and leaving it out were
    /// indistinguishable, and the only deliberate act the old setting could
    /// express was `false`.
    ///
    /// An earlier revision of this migration mapped `true` to
    /// [`ApprovalMode::Always`], on the theory that it should never loosen a
    /// posture someone chose on purpose. That was the right instinct applied
    /// to the wrong signal: since the app wrote the key itself, it made *every*
    /// existing install migrate to the old behaviour, so the new default
    /// reached nobody who had ever run the program.
    ///
    /// `false` needs no carrying over either -- it already meant "only the
    /// dangerous tier asks", which is exactly what `approval` now defaults to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval: Option<bool>,
    /// Superseded by `approval`, with no replacement.
    ///
    /// It only ever had one non-default use -- `false`, meaning "prompt me
    /// before reading a file too" -- and prompting for a read is what trains
    /// people to stop reading the prompts that matter. Read and discarded so
    /// an old config still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approve_read_only: Option<bool>,
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
    /// Where `publish_artifact` sends its manifest to be signed.
    ///
    /// A URL rather than a compiled-in constant so a fork, an internal
    /// mirror, or a self-hosted bucket can be pointed at without rebuilding --
    /// and so setting it to "" switches the feature off entirely for anyone
    /// who would rather boxcode could not publish anything.
    #[serde(default = "default_artifact_endpoint")]
    pub artifact_endpoint: String,
    /// Where `deploy_backend` sends a project's source to be hosted as a real
    /// server. Same reasoning as `artifact_endpoint`, and the same escape
    /// hatch: "" switches backend hosting off for anyone who would rather this
    /// build could not stand up a server on somebody else's box.
    #[serde(default = "default_backend_endpoint")]
    pub backend_endpoint: String,
    /// Where `enable_auth` sends a project id to be provisioned with
    /// sign-up/sign-in. Same reasoning as `artifact_endpoint`: a URL, not a
    /// constant, so a fork or a self-hosted control-plane can be pointed at
    /// without rebuilding, and "" switches the feature off.
    #[serde(default = "default_auth_endpoint")]
    pub auth_endpoint: String,
    /// Where `db_query` sends one SQL statement to run against a project's
    /// database. Same reasoning as `artifact_endpoint`/`auth_endpoint`.
    #[serde(default = "default_db_endpoint")]
    pub db_endpoint: String,
    /// Where `list_change_requests`/`resolve_change_request` read and
    /// consume the change-request mailbox for a project. Same reasoning as
    /// `artifact_endpoint`/`auth_endpoint`/`db_endpoint`.
    #[serde(default = "default_requests_endpoint")]
    pub requests_endpoint: String,
    /// How many request rounds one subagent may take before its schemas are
    /// withheld and it is made to answer. Separate from `max_steps`: a child
    /// exists to answer one focused question, so its budget is deliberately
    /// smaller than the loop that spawned it.
    #[serde(default = "default_subagent_max_steps")]
    pub subagent_max_steps: usize,
    /// Ceiling on the tokens one subagent may spend across all of its rounds
    /// (exact counts when the endpoint reports them, the usual character
    /// estimate when it does not). Steps bound how many *turns* a child takes;
    /// this bounds how much each of those turns is allowed to cost -- without
    /// it, fifteen rounds over a growing transcript is an unbounded bill.
    #[serde(default = "default_subagent_token_budget")]
    pub subagent_token_budget: usize,
}

fn yes() -> bool {
    true
}

fn dot() -> String {
    ".".to_string()
}

fn default_max_tokens() -> u32 {
    // The ceiling on one *reply*, not on the conversation. 16k was already
    // generous for chat and still too small for the thing this tool is
    // actually asked to do -- write a whole file, or a deck, in one go. A
    // reply cut off mid-token costs the entire turn, so the cap is set above
    // what any single answer plausibly needs rather than close to it.
    32768
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

fn default_artifact_endpoint() -> String {
    "https://boxcode.sh/api/artifact".to_string()
}

fn default_backend_endpoint() -> String {
    "https://boxcode.sh/api/deploy".to_string()
}

fn default_auth_endpoint() -> String {
    "https://auth.boxcode.sh/provision".to_string()
}

fn default_db_endpoint() -> String {
    // Same box, same vhost and cert as auth -- see infra/db/README.md for
    // why this never needed a second domain.
    "https://auth.boxcode.sh/db/query".to_string()
}

fn default_requests_endpoint() -> String {
    // Same box, same vhost and cert as auth/db -- see infra/requests/README.md.
    "https://auth.boxcode.sh/requests".to_string()
}

fn default_search_timeout() -> u64 {
    20
}

fn default_subagent_max_steps() -> usize {
    // Enough to explore a question properly (a child that greps, reads five
    // files and answers uses six), small enough that a child which has lost
    // the thread is cut off well before the parent's own 40-round budget.
    15
}

fn default_subagent_token_budget() -> usize {
    // Roomy for research over a real codebase, but a hard ceiling: at typical
    // per-round transcript growth this is the cost of one thorough child, not
    // a runaway one.
    200_000
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: yes(),
            workspace: dot(),
            approval: ApprovalMode::default(),
            require_approval: None,
            auto_approve_read_only: None,
            command_timeout_secs: default_command_timeout(),
            max_output_bytes: default_max_output_bytes(),
            max_steps: default_max_steps(),
            python_bin: default_python_bin(),
            search_timeout_secs: default_search_timeout(),
            artifact_endpoint: default_artifact_endpoint(),
            backend_endpoint: default_backend_endpoint(),
            auth_endpoint: default_auth_endpoint(),
            db_endpoint: default_db_endpoint(),
            requests_endpoint: default_requests_endpoint(),
            subagent_max_steps: default_subagent_max_steps(),
            subagent_token_budget: default_subagent_token_budget(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path();

        let mut config = if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            // Tightened on the way past, not only on save. This file holds an
            // API key in clear text, and the realistic user sets it once
            // through `/provider` and never writes it again -- so a repair
            // that only ran on save would leave almost every existing install
            // world-readable forever.
            harden(&config_path);
            toml::from_str::<Config>(&contents).map_err(|e| {
                format!("{} is not valid TOML: {e}", config_path.display())
            })?
        } else {
            Self::default()
        };

        // Environment variables win over the file, so exporting a key works even
        // when a stale config.toml is on disk.
        if let Some(v) = env_var("BOXCODE_ENDPOINT") {
            config.llm.endpoint = v;
        }
        if let Some(v) = env_var("BOXCODE_MODEL") {
            config.llm.model = v;
        }
        if let Some(v) = env_var("BOXCODE_API_KEY") {
            config.llm.api_key = v;
        }
        if let Some(v) = env_var("BOXCODE_WORKSPACE") {
            config.tools.workspace = v;
        }
        if let Some(v) = env_var("BOXCODE_TOOLS_ENABLED") {
            config.tools.enabled = truthy(&v);
        }
        if let Some(v) = env_var("BOXCODE_QUOTA_ENABLED") {
            config.quota.enabled = truthy(&v);
        }
        for (name, slot) in [
            ("BOXCODE_MAX_REQUESTS_PER_DAY", 0),
            ("BOXCODE_MAX_TOKENS_PER_DAY", 1),
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
        if let Some(v) = env_var("BOXCODE_MAX_USD_PER_DAY") {
            if let Ok(n) = v.trim().parse::<f64>() {
                config.quota.max_usd_per_day = n;
            }
        }
        if let Some(v) = env_var("BOXCODE_TOOLS_APPROVAL") {
            // The mode names, plus the truthy spellings this variable always
            // took. `=1` is unambiguous here in a way the config key is not:
            // nothing ever wrote this variable on a user's behalf, so setting
            // it is always a deliberate act and can be honoured as one.
            config.tools.approval = match v.trim().to_ascii_lowercase().as_str() {
                "always" => ApprovalMode::Always,
                "destructive" => ApprovalMode::Destructive,
                other if truthy(other) => ApprovalMode::Always,
                _ => ApprovalMode::Destructive,
            };
            config.tools.require_approval = None;
        }

        config.normalize();
        config.normalize_quota();
        Ok(config)
    }

    /// Trim stray whitespace. A trailing newline in an API key (very easy to get
    /// from `export KEY=$(cat file)`) produces an invalid Authorization header.
    /// `pub(crate)` so tests elsewhere can put a real `config.toml` through
    /// the same path `load` does. Loading a file and parsing one are not the
    /// same thing -- the retired-key handling lives here, not in serde -- and
    /// a test that skipped it would be checking a config no running program
    /// ever sees.
    pub(crate) fn normalize(&mut self) {
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
        // Both retired keys are dropped rather than translated -- see
        // `require_approval`'s own comment for why neither one can be read as
        // a choice its owner made. `approval` is the only thing that decides,
        // and an absent `approval` means the default.
        self.tools.require_approval = None;
        self.tools.auto_approve_read_only = None;
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

        // A history limit of 0 would make `/deployments` print a heading and
        // nothing else, which reads as a broken command rather than a setting.
        self.deploy.history_limit = self.deploy.history_limit.clamp(1, 200);

        // A tiny threshold would compact after every turn -- each one a real,
        // metered request -- and a threshold of 0 would try before the first.
        self.compact.auto_at_tokens = self.compact.auto_at_tokens.max(4_000);
    }

    /// Nonsense quota settings are clamped rather than obeyed. A warn threshold
    /// of 0 would fire before the first request; above 100 it could never fire.
    fn normalize_quota(&mut self) {
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
                "No API key set. Export BOXCODE_API_KEY or add api_key to ~/.boxcode/config.toml."
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
        write_private(&config_path, &toml_str)?;
        // Repairs a file that already existed, which the create-time mode
        // above cannot: `OpenOptions::mode` applies only when the file is
        // created, so every config.toml written before this change would
        // otherwise keep the 0644 it was born with.
        harden(&config_path);
        Ok(())
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    /// Where this install keeps its state.
    ///
    /// Calls `adopt_legacy_dir` on the way past, so the very first thing any
    /// caller does is inherit the old directory if there is one.
    pub fn config_dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = home.join(".boxcode");
        adopt_legacy_dir(&home, &dir);
        dir
    }
}

/// Write `contents` to `path`, created readable and writable by its owner and
/// nobody else.
///
/// `fs::write` creates at whatever the umask allows -- 0644 on a stock macOS
/// or Linux account, meaning every other user on the machine can read it.
/// That is the wrong default for the one file whose job is to hold an API key
/// in clear text.
///
/// The mode is set *at creation* rather than chmod'ed afterwards, so there is
/// no window, however brief, in which the key sits on disk world-readable.
/// That is also why `save` still calls `harden` after this: `mode` applies
/// only when the file is created, so it does nothing for a config.toml that
/// already exists, which is every existing install.
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())
}

/// Best-effort `chmod 600`.
///
/// Failure is deliberately ignored. A config that could not be tightened is
/// still a config that works, and refusing to start over a permission bit --
/// on a filesystem that may not implement them at all (an SMB share, some
/// FUSE mounts, a container bind) -- would be a worse bug than the exposure
/// it is guarding against.
///
/// A no-op off Unix, which has no mode bits to set. Windows keeps this file
/// under `%USERPROFILE%`, whose default ACL already excludes other
/// non-administrator users, so the specific hole this closes does not exist
/// there in the same form.
fn harden(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Take over `~/.tuisample-code` when this install has no directory of its own.
///
/// Renamed rather than copied, and only when there is nothing to overwrite, so
/// this can run on every launch and does exactly nothing after the first.
///
/// Without it the rename silently costs every existing user their API key and
/// their free-tier device token -- and the token is the painful one: the device
/// re-enrols as brand new, which reads to the gateway as someone farming fresh
/// daily budgets rather than as the same machine after an upgrade.
fn adopt_legacy_dir(home: &std::path::Path, dir: &std::path::Path) {
    if dir.exists() {
        return;
    }
    let legacy = home.join(".tuisample-code");
    if !legacy.is_dir() {
        return;
    }
    // A failure here is not worth interrupting startup for: the app carries on
    // with a fresh directory, which is exactly what would have happened anyway.
    let _ = std::fs::rename(&legacy, dir);
}

/// Whether this launch is the one that will inherit the pre-1.0 directory.
///
/// Has to be asked *before* anything calls `Config::config_dir`, since that is
/// what performs the adoption. Purely so the app can say it happened: a silent
/// migration is one the user cannot verify.
pub fn legacy_dir_pending() -> bool {
    let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    else {
        return false;
    };
    !home.join(".boxcode").exists() && home.join(".tuisample-code").is_dir()
}

/// Deprecated `TUISAMPLE_*` variables that are actually doing something.
///
/// One shadowed by its `BOXCODE_*` replacement is inert and not worth a
/// warning -- naming it would train people to ignore the line.
pub fn deprecated_env_vars_in_use() -> Vec<String> {
    let mut found: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("TUISAMPLE_"))
        .filter(|key| {
            read_env(&format!("BOXCODE_{}", key.trim_start_matches("TUISAMPLE_"))).is_none()
        })
        .collect();
    found.sort();
    found
}

/// Read `BOXCODE_*`, falling back to the `TUISAMPLE_*` name it replaced.
///
/// The old names stay readable so a shell profile, a CI job or an intern's
/// half-remembered setup keeps working across the rename. The new name wins
/// when both are set, so migrating is a matter of adding the new one rather
/// than having to find and remove the old.
fn env_var(name: &str) -> Option<String> {
    if let Some(value) = read_env(name) {
        return Some(value);
    }
    let legacy = name.strip_prefix("BOXCODE_")?;
    read_env(&format!("TUISAMPLE_{legacy}"))
}

fn read_env(name: &str) -> Option<String> {
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
            temperature: None,
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

    /// The file holds an API key in clear text, so nobody else on the machine
    /// should be able to read it. `fs::write` alone would leave it at the
    /// umask default, which is 0644 on a stock account.
    #[cfg(unix)]
    #[test]
    fn saving_leaves_the_config_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        with_isolated_home(|| {
            Config::default().save().unwrap();

            let mode = std::fs::metadata(Config::config_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "got {mode:o}, expected 600");
        });
    }

    /// Every install that predates this wrote its key at 0644 and will not
    /// necessarily ever save again -- the usual pattern is to set the key once
    /// through `/provider` and never touch it. Loading has to repair it, or
    /// those files stay exposed forever.
    #[cfg(unix)]
    #[test]
    fn loading_tightens_a_config_that_was_left_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        with_isolated_home(|| {
            let path = Config::config_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "[llm]\napi_key = \"sk-exposed\"\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

            Config::load().unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "got {mode:o}, expected 600");
        });
    }

    /// Tightening the file must not cost the contents. An overwrite that
    /// landed on an existing 0600 file has to still replace it in full.
    #[test]
    fn saving_over_an_existing_config_still_replaces_its_contents() {
        with_isolated_home(|| {
            for key in ["BOXCODE_ENDPOINT", "BOXCODE_MODEL", "BOXCODE_API_KEY"] {
                std::env::remove_var(key);
            }
            let mut config = Config::default();
            config.llm.api_key = "sk-first".to_string();
            config.save().unwrap();

            config.llm.api_key = "sk-second".to_string();
            config.save().unwrap();

            let on_disk = std::fs::read_to_string(Config::config_path()).unwrap();
            assert!(on_disk.contains("sk-second"), "{on_disk}");
            assert!(!on_disk.contains("sk-first"), "{on_disk}");
        });
    }

    #[test]
    fn save_then_load_round_trips_all_llm_fields_including_provider() {
        with_isolated_home(|| {
            // BOXCODE_* env vars win over the file (by design), so a developer
            // machine that happens to have one exported must not leak into this
            // assertion.
            let saved_env: Vec<(&str, Option<String>)> =
                ["BOXCODE_ENDPOINT", "BOXCODE_MODEL", "BOXCODE_API_KEY"]
                    .iter()
                    .map(|&k| (k, std::env::var(k).ok()))
                    .collect();
            for (k, _) in &saved_env {
                std::env::remove_var(k);
            }

            let config = Config {
                quota: QuotaConfig::default(),
                ui: UiConfig::default(),
                deploy: DeployConfig::default(),
                update: UpdateConfig::default(),
                compact: CompactConfig::default(),
                llm: LlmConfig {
                    endpoint: "https://api.deepseek.com".to_string(),
                    model: "deepseek-v4-pro".to_string(),
                    api_key: "sk-test-key".to_string(),
                    max_tokens: 8192,
                    provider: "deepseek".to_string(),
                    temperature: Some(0.5),
                },
                tools: ToolsConfig::default(),
            };
            config.save().expect("save should succeed");

            let loaded = Config::load().expect("load should succeed");
            assert_eq!(loaded.llm.endpoint, "https://api.deepseek.com");
            assert_eq!(loaded.llm.model, "deepseek-v4-pro");
            assert_eq!(loaded.llm.api_key, "sk-test-key");
            assert_eq!(loaded.llm.provider, "deepseek");
            assert_eq!(loaded.llm.temperature, Some(0.5));

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
            // The posture has to survive an absent table. Destructive is the
            // default, and the destructive tier still asks in it -- what must
            // not happen is an absent table deserializing into something with
            // no approval at all.
            assert_eq!(loaded.tools.approval, ApprovalMode::Destructive);
            assert_eq!(loaded.tools.python_bin, "python3");
            assert_eq!(loaded.tools.search_timeout_secs, 20);
            // ...and the same again for `[deploy]`, added later still.
            assert!(loaded.deploy.enabled);
            assert!(loaded.deploy.allow_cli_install);
            assert_eq!(loaded.deploy.history_limit, 10);
        });
    }

    /// A `[deploy]` table someone half-filled in by hand must still get the
    /// safe defaults for the keys they left out.
    #[test]
    fn a_partial_deploy_table_defaults_the_rest() {
        let parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[deploy]\nallow_cli_install = false\n",
        )
        .expect("should parse");
        assert!(!parsed.deploy.allow_cli_install);
        assert!(parsed.deploy.enabled);
        assert_eq!(parsed.deploy.history_limit, 10);
    }

    /// A limit of zero would make `/deployments` print a heading and nothing
    /// else, which reads as a broken command rather than a setting.
    #[test]
    fn a_zero_history_limit_is_clamped_to_something_usable() {
        let mut config = Config::default();
        config.deploy.history_limit = 0;
        config.normalize();
        assert_eq!(config.deploy.history_limit, 1);
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

    /// A config written before `clear_on_exit` existed must still load, and
    /// must get the new default rather than failing to parse.
    #[test]
    fn a_ui_table_without_clear_on_exit_still_loads() {
        let parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[ui]\ntheme = \"dark\"\n",
        )
        .expect("should parse");
        assert_eq!(parsed.ui.theme, "dark");
        assert!(parsed.ui.clear_on_exit);
    }

    /// And someone who turns it off keeps it off across a save.
    #[test]
    fn clear_on_exit_survives_a_round_trip() {
        let mut parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[ui]\nclear_on_exit = false\n",
        )
        .expect("should parse");
        parsed.normalize();
        assert!(!parsed.ui.clear_on_exit);
        let written = toml::to_string_pretty(&parsed).unwrap();
        assert!(written.contains("clear_on_exit = false"), "{written}");
    }

    /// A `[tools]` table with no `approval` key -- anyone who edited it by
    /// hand, and every config written before the key existed.
    #[test]
    fn a_tools_table_without_approval_defaults_to_destructive() {
        let parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[tools]\nenabled = true\nmax_steps = 3\n",
        )
        .expect("should parse");
        assert_eq!(parsed.tools.approval, ApprovalMode::Destructive);
    }

    /// The regression that made the whole feature a no-op in practice.
    ///
    /// This is a real `config.toml` off a machine that had simply run the
    /// program and picked a provider. `require_approval = true` is in it
    /// because `save` wrote every field, not because anyone asked -- `true`
    /// was the default, so it says nothing about intent. Reading it as a
    /// deliberate "ask me about everything" sent every existing install
    /// straight back to the old behaviour, which is every install there is.
    #[test]
    fn a_config_the_app_wrote_itself_still_gets_the_new_default() {
        let mut parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"https://api.deepseek.com\"\nmodel = \"deepseek-v4-pro\"\n\n             [tools]\nenabled = true\nworkspace = \".\"\nrequire_approval = true\n             auto_approve_read_only = true\ncommand_timeout_secs = 60\nmax_steps = 40\n",
        )
        .expect("should parse");
        parsed.normalize();
        assert_eq!(parsed.tools.approval, ApprovalMode::Destructive);
        // ...and neither retired key is written back out.
        assert!(parsed.tools.require_approval.is_none());
        assert!(parsed.tools.auto_approve_read_only.is_none());
        let written = toml::to_string_pretty(&parsed).unwrap();
        assert!(!written.contains("require_approval"), "{written}");
        assert!(!written.contains("auto_approve_read_only"), "{written}");
    }

    /// `require_approval = false` already meant "only the dangerous tier
    /// asks", which is exactly the new default -- so those installs see no
    /// change at all, and must not be pushed the other way.
    #[test]
    fn the_old_unattended_setting_maps_to_the_new_default() {
        let mut parsed: Config = toml::from_str(
            "[llm]\nendpoint = \"http://x\"\n\n[tools]\nrequire_approval = false\nauto_approve_read_only = false\n",
        )
        .expect("should parse");
        parsed.normalize();
        assert_eq!(parsed.tools.approval, ApprovalMode::Destructive);
        assert!(parsed.tools.auto_approve_read_only.is_none());
    }

    /// The new key, spelled the way the docs spell it.
    #[test]
    fn the_approval_mode_round_trips_through_toml() {
        for (text, expected) in [
            ("always", ApprovalMode::Always),
            ("destructive", ApprovalMode::Destructive),
        ] {
            let parsed: Config = toml::from_str(&format!(
                "[llm]\nendpoint = \"http://x\"\n\n[tools]\napproval = \"{text}\"\n"
            ))
            .expect("should parse");
            assert_eq!(parsed.tools.approval, expected);
            assert!(toml::to_string_pretty(&parsed).unwrap().contains(text));
        }
    }

    /// `approval` is the only key that decides, whatever else is left in the
    /// table beside it -- in both directions.
    #[test]
    fn the_new_key_is_the_only_one_that_decides() {
        for (extra, expected) in [
            ("approval = \"always\"\nrequire_approval = false\n", ApprovalMode::Always),
            ("approval = \"destructive\"\nrequire_approval = true\n", ApprovalMode::Destructive),
        ] {
            let mut parsed: Config =
                toml::from_str(&format!("[llm]\nendpoint = \"http://x\"\n\n[tools]\n{extra}"))
                    .expect("should parse");
            parsed.normalize();
            assert_eq!(parsed.tools.approval, expected);
        }
    }

    /// The env var is different: nothing ever set it on a user's behalf, so
    /// setting it is always deliberate and is honoured as one.
    #[test]
    fn the_env_var_can_still_ask_for_the_strict_mode() {
        with_isolated_home(|| {
            for (value, expected) in [
                ("always", ApprovalMode::Always),
                ("1", ApprovalMode::Always),
                ("destructive", ApprovalMode::Destructive),
                ("0", ApprovalMode::Destructive),
            ] {
                std::env::set_var("BOXCODE_TOOLS_APPROVAL", value);
                let loaded = Config::load().expect("loads");
                assert_eq!(loaded.tools.approval, expected, "for {value:?}");
            }
            std::env::remove_var("BOXCODE_TOOLS_APPROVAL");
        });
    }

    /// The reply cap is the difference between a whole generated file and one
    /// truncated mid-token, and a truncated reply costs the entire turn. It is
    /// deliberately well above what any single answer needs.
    #[test]
    fn the_default_reply_cap_is_generous_enough_for_a_whole_file() {
        assert_eq!(LlmConfig::default().max_tokens, 32768);
        // And a config written before this key existed still gets it.
        let parsed: Config = toml::from_str("[llm]\nendpoint = \"http://x\"\n").expect("parses");
        assert_eq!(parsed.llm.max_tokens, 32768);
    }

    /// DeepSeek gets a deterministic temperature without anyone having to
    /// discover the setting; every other provider (and a blank/custom one)
    /// sends nothing and defers to the endpoint's own default. An explicit
    /// `temperature` in `config.toml` always wins over both.
    #[test]
    fn effective_temperature_defaults_to_zero_for_deepseek_and_none_elsewhere() {
        let deepseek: Config = toml::from_str(
            "[llm]\nendpoint = \"https://api.deepseek.com\"\nprovider = \"deepseek\"\n",
        )
        .expect("parses");
        assert_eq!(deepseek.llm.temperature, None, "nothing written to disk yet");
        assert_eq!(deepseek.llm.effective_temperature(), Some(0.0));

        let openai: Config =
            toml::from_str("[llm]\nendpoint = \"https://api.openai.com\"\nprovider = \"openai\"\n")
                .expect("parses");
        assert_eq!(openai.llm.effective_temperature(), None);

        let custom: Config = toml::from_str("[llm]\nendpoint = \"http://localhost:8000\"\n")
            .expect("parses");
        assert_eq!(custom.llm.effective_temperature(), None);

        // An explicit setting overrides the built-in default in either
        // direction, including opting DeepSeek back out.
        let overridden: Config = toml::from_str(
            "[llm]\nendpoint = \"https://api.deepseek.com\"\nprovider = \"deepseek\"\ntemperature = 0.7\n",
        )
        .expect("parses");
        assert_eq!(overridden.llm.effective_temperature(), Some(0.7));
    }

    /// The rename must not cost anyone their settings. The painful one is the
    /// free-tier device token: losing it re-enrols the machine as brand new,
    /// which reads to the gateway as someone farming fresh daily budgets rather
    /// than as the same machine after an upgrade.
    #[test]
    fn the_pre_rename_directory_is_adopted_on_first_run() {
        with_isolated_home(|| {
            let home = PathBuf::from(std::env::var("HOME").unwrap());
            let legacy = home.join(".tuisample-code");
            std::fs::create_dir_all(&legacy).unwrap();
            std::fs::write(
                legacy.join("config.toml"),
                "[llm]\nendpoint = \"https://api.deepseek.com\"\nmodel = \"kept\"\napi_key = \"sk-kept\"\n",
            )
            .unwrap();
            std::fs::write(legacy.join("device_id"), "device-kept").unwrap();

            let dir = Config::config_dir();
            assert!(dir.ends_with(".boxcode"), "{dir:?}");
            assert!(!legacy.exists(), "the old directory should have been moved, not copied");
            assert_eq!(
                std::fs::read_to_string(dir.join("device_id")).unwrap(),
                "device-kept",
                "the free-tier identity must survive the rename"
            );

            for key in ["BOXCODE_ENDPOINT", "BOXCODE_MODEL", "BOXCODE_API_KEY"] {
                std::env::remove_var(key);
            }
            let loaded = Config::load().expect("the adopted config still loads");
            assert_eq!(loaded.llm.model, "kept");
            assert_eq!(loaded.llm.api_key, "sk-kept");
        });
    }

    /// Adopting must never clobber a directory this install already has.
    #[test]
    fn an_existing_directory_is_never_overwritten_by_the_old_one() {
        with_isolated_home(|| {
            let home = PathBuf::from(std::env::var("HOME").unwrap());
            std::fs::create_dir_all(home.join(".boxcode")).unwrap();
            std::fs::write(home.join(".boxcode/device_id"), "current").unwrap();
            std::fs::create_dir_all(home.join(".tuisample-code")).unwrap();
            std::fs::write(home.join(".tuisample-code/device_id"), "stale").unwrap();

            let dir = Config::config_dir();
            assert_eq!(std::fs::read_to_string(dir.join("device_id")).unwrap(), "current");
            assert!(
                home.join(".tuisample-code").exists(),
                "the old directory is left alone once there is a current one"
            );
        });
    }

    /// A shell profile, a CI job or an intern's half-remembered setup should
    /// keep working across the rename.
    #[test]
    fn the_old_environment_variable_names_still_work() {
        with_isolated_home(|| {
            for key in ["BOXCODE_MODEL", "TUISAMPLE_MODEL"] {
                std::env::remove_var(key);
            }
            std::env::set_var("TUISAMPLE_MODEL", "from-the-old-name");
            assert_eq!(
                Config::load().unwrap().llm.model,
                "from-the-old-name",
                "the pre-rename variable must still be honoured"
            );

            // ...and the new name wins when both are set, so migrating means
            // adding the new one rather than hunting down the old.
            std::env::set_var("BOXCODE_MODEL", "from-the-new-name");
            assert_eq!(Config::load().unwrap().llm.model, "from-the-new-name");

            for key in ["BOXCODE_MODEL", "TUISAMPLE_MODEL"] {
                std::env::remove_var(key);
            }
        });
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
