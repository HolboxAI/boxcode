//! Deploying the project you are working in to a hosting provider.
//!
//! # Why the provider trait describes commands instead of running them
//!
//! Every provider here is an official CLI (`vercel`, `netlify`) that this app
//! shells out to, exactly the way `tools.rs` shells out for `run_command`.
//! Reimplementing their REST APIs would mean owning their auth token formats,
//! their upload protocols and their build pipelines -- a large surface that
//! changes without notice, to arrive at what the vendor's own CLI already does
//! correctly.
//!
//! So [`DeploymentProvider`] does not execute anything. Each operation returns
//! a [`ProviderCommand`] -- a described, not-yet-run command -- and one shared
//! runner (`runner.rs`) executes it, streams its output, enforces the timeout
//! and kills it on cancellation. Two things fall out of that:
//!
//! - Every provider is testable with no network, no CLI installed and no
//!   process spawned: assert on the command it describes and feed its parser a
//!   captured `CommandOutput`. The same reasoning split `format_search_result`
//!   out of `execute_web_search` in `tools.rs`.
//! - Timeouts, cancellation, redaction and line streaming exist once rather
//!   than once per provider, so a new provider cannot forget them.
//!
//! Adding AWS Amplify, Cloudflare Pages, GitHub Pages or Render means one file
//! implementing this trait plus one line in [`providers`]. Nothing in `app.rs`
//! or `ui.rs` names a provider.
//!
//! # Secrets
//!
//! Tokens and environment-variable values are [`Secret`], which has no
//! `Display` impl at all -- `format!("{}", secret)` does not compile, so a
//! value cannot reach the transcript, the log panel or the history file by
//! accident. The one way out is [`Secret::expose`], named so that every call
//! site reads as a deliberate act in review. Streamed output goes through
//! [`redact`] on the way in regardless, because a CLI may print a token this
//! app never held.

pub mod cli;
pub mod backend;
pub mod detect;
pub mod history;
pub mod netlify;
pub mod runner;
pub mod service;
pub mod vercel;

/// The vocabulary the rest of the app uses. Everything else stays behind its
/// own module path, so `deploy::detect::detect` and `deploy::history::recent`
/// read as what they are at the call site.
pub use runner::CommandOutput;
pub use service::{DeployAction, DeployEvent, DeploySession, Menu, Stage, StepState};

use detect::Framework;
use std::path::PathBuf;

/// A value that must never be rendered.
///
/// Deliberately implements neither `Display` nor a revealing `Debug`: the
/// compiler, not a code-review habit, is what keeps a token out of a format
/// string.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only way to read the value. Named to stand out in review -- if this
    /// appears anywhere near a `format!` that reaches the UI, that is a bug.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// A fixed-width stand-in for the UI. Fixed rather than proportional to the
    /// real length: the length of a secret is itself a small leak, and a row of
    /// eight dots reads as "set" just as well.
    pub fn masked(&self) -> &'static str {
        "••••••••"
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(••••)")
    }
}

/// One environment variable for the build. The key is ordinary text; the value
/// never is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: Secret,
}

/// Which kind of deployment to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Production,
    Preview,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::Production => "Production",
            Target::Preview => "Preview",
        }
    }

    pub fn is_production(self) -> bool {
        matches!(self, Target::Production)
    }
}

/// A project that already exists on the provider's side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteProject {
    /// Provider-side id, where there is one. Netlify's `--site` takes this;
    /// Vercel links by name, so it may be the name again.
    pub id: String,
    pub name: String,
    /// The site's current URL, when the listing includes it. Shown in the
    /// picker so "which of my six sites is this" is answerable.
    pub url: Option<String>,
}

/// Whether to attach to something that already exists or make a new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkChoice {
    New,
    Existing(RemoteProject),
}

/// Everything a provider needs to build its deploy command. Assembled by
/// `service.rs` from the detected profile plus whatever the user overrode.
#[derive(Clone, Debug, PartialEq)]
pub struct DeployPlan {
    pub root: PathBuf,
    pub project_name: String,
    pub framework: Framework,
    pub build_command: Option<String>,
    pub output_dir: Option<String>,
    /// Passed to the child process's environment, never to its argv -- argv is
    /// world-readable through `ps`, and an environment is not.
    pub env: Vec<EnvVar>,
    pub target: Target,
    pub link: LinkChoice,
    /// A token supplied by the environment or by the user, used instead of an
    /// interactive login when one is available.
    pub token: Option<Secret>,
}

impl DeployPlan {
    /// The provider-side project this plan attaches to, if it already exists.
    pub fn existing(&self) -> Option<&RemoteProject> {
        match &self.link {
            LinkChoice::Existing(project) => Some(project),
            LinkChoice::New => None,
        }
    }
}

/// A command a provider wants run, described but not yet executed.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment for the child. Secret by type, so it cannot be
    /// rendered by `display()` below even accidentally.
    pub env: Vec<(String, Secret)>,
    /// True when a human has to interact with this command -- a browser login.
    /// The runner refuses to pipe these: see `runner::run_interactive`.
    pub interactive: bool,
    pub timeout_secs: u64,
}

/// Default ceiling for the quick informational commands (`--version`,
/// `whoami`, `sites:list`). A build is not one of these; `deploy` overrides it.
const QUICK_TIMEOUT_SECS: u64 = 30;

impl ProviderCommand {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: Vec::new(),
            interactive: false,
            timeout_secs: QUICK_TIMEOUT_SECS,
        }
    }

    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }

    /// Append `flag value` only when `value` is `Some` -- the shape most of
    /// these commands need, where an absent option means "omit it entirely"
    /// rather than "pass an empty string".
    pub fn opt(self, flag: &str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.arg(flag).arg(value),
            None => self,
        }
    }

    pub fn flag_if(self, condition: bool, flag: &str) -> Self {
        if condition {
            self.arg(flag)
        } else {
            self
        }
    }

    pub fn with_env(mut self, env: Vec<(String, Secret)>) -> Self {
        self.env.extend(env);
        self
    }

    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }

    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// One line for the progress panel. Cannot leak: `env` is not rendered and
    /// `Secret` has no `Display`, so the only way a token could appear here is
    /// if a provider put one in `args`, which is why none of them do.
    pub fn display(&self) -> String {
        let mut out = String::from("$ ");
        out.push_str(&self.program);
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

/// What the provider's CLI says about who is signed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// Signed in, carrying whatever identity the CLI reported.
    In(String),
    Out,
    /// The CLI answered in a shape this build does not recognise. Distinct
    /// from `Out` on purpose: sending someone through a login they do not need
    /// is a worse answer than saying plainly that we could not tell.
    Unknown(String),
}

/// How a finished deployment ended. Persisted in the history file as a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeployStatus {
    Success,
    Failed,
    Cancelled,
}

impl DeployStatus {
    pub fn label(self) -> &'static str {
        match self {
            DeployStatus::Success => "Success",
            DeployStatus::Failed => "Failed",
            DeployStatus::Cancelled => "Cancelled",
        }
    }
}

/// The operations every provider supports.
///
/// The names mirror the shape a deployment integration is usually described
/// in -- `authenticate`, `is_authenticated`, `get_projects`, `create_project`,
/// `link_project`, `deploy`, `get_deployment_status`, `get_deployment_url`,
/// `logout` -- with the split noted in the module doc: the `*_command`-shaped
/// ones describe work, the `parse_*`/`get_*` ones read the result back.
///
/// Object-safe by construction (no generics, no `async fn`), so
/// `Box<dyn DeploymentProvider>` works and the UI can hold a provider it knows
/// nothing specific about.
pub trait DeploymentProvider: Send + Sync {
    /// Stable id, stored in config and history.
    fn id(&self) -> &'static str;
    /// Shown in the picker.
    fn label(&self) -> &'static str;
    /// The executable this provider drives.
    fn cli_binary(&self) -> &'static str;
    /// Where to send someone whose CLI is behaving unexpectedly.
    fn docs_url(&self) -> &'static str;
    /// The environment variable that carries a non-interactive token, if the
    /// provider has one. Checked before any login is offered.
    fn token_env_var(&self) -> &'static str;

    // ---- described operations -------------------------------------------

    fn version_command(&self) -> ProviderCommand;
    /// How to install the CLI. Never run without explicit confirmation -- see
    /// `cli::install_risk`.
    fn install_command(&self) -> ProviderCommand;
    fn is_authenticated(&self, token: Option<&Secret>) -> ProviderCommand;
    fn authenticate(&self) -> ProviderCommand;
    fn logout(&self) -> ProviderCommand;
    fn get_projects(&self, token: Option<&Secret>) -> ProviderCommand;
    /// `None` when the provider creates the project as part of deploying, which
    /// is the common case -- both CLIs here do.
    fn create_project(&self, plan: &DeployPlan) -> Option<ProviderCommand>;
    /// `None` when linking is folded into the deploy command's own flags.
    fn link_project(&self, plan: &DeployPlan) -> Option<ProviderCommand>;
    fn deploy(&self, plan: &DeployPlan) -> ProviderCommand;
    /// `None` when the provider has no separate status query worth making.
    fn get_deployment_status(&self, deployment: &str, token: Option<&Secret>)
        -> Option<ProviderCommand>;

    // ---- readers ---------------------------------------------------------

    fn parse_auth(&self, out: &CommandOutput) -> AuthState;
    fn parse_projects(&self, out: &CommandOutput) -> Vec<RemoteProject>;
    /// The URL a finished deployment is served from.
    fn get_deployment_url(&self, out: &CommandOutput) -> Option<String>;
    /// One human sentence for why a command failed, for the failure screen.
    fn explain_failure(&self, out: &CommandOutput) -> String;
}

/// Every provider this build knows about, in the order the picker shows them.
pub fn providers() -> Vec<Box<dyn DeploymentProvider>> {
    vec![
        Box::new(vercel::VercelProvider),
        Box::new(netlify::NetlifyProvider),
    ]
}

pub fn provider_by_id(id: &str) -> Option<Box<dyn DeploymentProvider>> {
    providers().into_iter().find(|p| p.id() == id)
}

/// Keys whose value is a secret whatever it looks like.
const SECRET_KEY_HINTS: &[&str] = &[
    "token", "secret", "password", "passwd", "pwd", "apikey", "api_key", "auth", "credential",
    "private_key", "access_key",
];

/// Token prefixes worth catching on sight, since a CLI may print one this app
/// never held and therefore cannot know to mask.
const TOKEN_PREFIXES: &[&str] = &["vercel_", "nfp_", "ghp_", "gho_", "github_pat_", "sk-", "nf_"];

const MASK: &str = "••••";

/// Scrub anything token-shaped out of a line before it reaches the UI, the
/// transcript or the history file.
///
/// Three rules, all conservative -- a rule that mangles ordinary build output
/// is its own kind of failure, so this deliberately does not try to guess at
/// "long opaque string" in general (commit SHAs, content hashes and Vercel's
/// own deployment URLs are all exactly that shape):
///
/// 1. `Bearer <anything>` -> `Bearer ••••`
/// 2. `KEY=value` / `KEY: value` where KEY names a secret -> value masked
/// 3. bare tokens carrying a known vendor prefix -> masked
pub fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for (i, word) in line.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&redact_word(word, i > 0 && follows_bearer(line, i)));
    }
    out
}

/// Whether word `index` is the one directly after a `Bearer`/`token` keyword.
fn follows_bearer(line: &str, index: usize) -> bool {
    line.split(' ')
        .nth(index.wrapping_sub(1))
        .map(|previous| {
            let previous = previous.trim_end_matches(':').to_ascii_lowercase();
            previous == "bearer" || previous == "token:" || previous == "token"
        })
        .unwrap_or(false)
}

fn redact_word(word: &str, after_keyword: bool) -> String {
    if after_keyword && !word.trim().is_empty() {
        return MASK.to_string();
    }

    // `KEY=value` and `KEY:value`, where the key names a secret.
    for separator in ['=', ':'] {
        if let Some((key, value)) = word.split_once(separator) {
            if value.is_empty() {
                continue;
            }
            let normalized = key.trim_start_matches('-').to_ascii_lowercase();
            let normalized = normalized.replace(['-', '.'], "_");
            if SECRET_KEY_HINTS.iter().any(|hint| normalized.contains(hint)) {
                return format!("{key}{separator}{MASK}");
            }
        }
    }

    let bare = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
    if !bare.is_empty() && TOKEN_PREFIXES.iter().any(|p| bare.starts_with(p)) && bare.len() > 8 {
        return word.replace(bare, MASK);
    }

    word.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_cannot_be_formatted_into_a_string() {
        // The compile-time property this type exists for is the absence of a
        // `Display` impl, which a test cannot assert directly -- what it can
        // assert is that the two escape hatches that do exist stay narrow.
        let secret = Secret::new("vercel_abc123def456");
        assert_eq!(format!("{secret:?}"), "Secret(••••)");
        assert_eq!(secret.masked(), "••••••••");
        assert_eq!(secret.expose(), "vercel_abc123def456");
    }

    /// The masked form must not encode the real length: how long a token is is
    /// itself a small leak.
    #[test]
    fn masking_is_a_fixed_width_regardless_of_the_secret() {
        assert_eq!(Secret::new("a").masked(), Secret::new("a".repeat(200)).masked());
    }

    #[test]
    fn a_described_command_never_renders_its_environment() {
        let command = ProviderCommand::new("vercel", &["deploy", "--yes"])
            .with_env(vec![("VERCEL_TOKEN".to_string(), Secret::new("tok_secret"))]);
        let shown = command.display();
        assert_eq!(shown, "$ vercel deploy --yes");
        assert!(!shown.contains("tok_secret"), "{shown}");
        assert!(!shown.contains("VERCEL_TOKEN"), "{shown}");
    }

    #[test]
    fn optional_flags_are_omitted_rather_than_passed_empty() {
        let with = ProviderCommand::new("netlify", &["deploy"]).opt("--site", Some("abc"));
        assert_eq!(with.args, vec!["deploy", "--site", "abc"]);

        let without = ProviderCommand::new("netlify", &["deploy"]).opt("--site", None);
        assert_eq!(without.args, vec!["deploy"]);
    }

    #[test]
    fn conditional_flags_only_appear_when_asked_for() {
        assert!(ProviderCommand::new("vercel", &[]).flag_if(true, "--prod").args == vec!["--prod"]);
        assert!(ProviderCommand::new("vercel", &[]).flag_if(false, "--prod").args.is_empty());
    }

    // ---- redaction ---------------------------------------------------------

    #[test]
    fn a_bearer_token_is_masked() {
        assert_eq!(
            redact("Authorization: Bearer abc123xyz789"),
            "Authorization: Bearer ••••"
        );
    }

    #[test]
    fn assignments_to_secret_looking_keys_are_masked() {
        for (input, expected) in [
            ("VERCEL_TOKEN=abc123", "VERCEL_TOKEN=••••"),
            ("--token=deadbeef", "--token=••••"),
            ("API_KEY=xyz", "API_KEY=••••"),
            ("DB_PASSWORD=hunter2", "DB_PASSWORD=••••"),
            ("NETLIFY_AUTH_TOKEN=nfp_x", "NETLIFY_AUTH_TOKEN=••••"),
        ] {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn vendor_prefixed_tokens_are_caught_even_bare() {
        assert_eq!(
            redact("using vercel_1a2b3c4d5e6f to authenticate"),
            "using •••• to authenticate"
        );
        assert_eq!(redact("(nfp_abcdefghijkl)"), "(••••)");
    }

    /// A redactor that mangles ordinary build output is its own failure: the
    /// log panel is where a failed build gets diagnosed.
    #[test]
    fn ordinary_build_output_passes_through_untouched() {
        for line in [
            "✓ Compiled successfully in 4.2s",
            "Route (app)                    Size     First Load JS",
            "https://my-project-a1b2c3d4.vercel.app",
            "warning: 3 vulnerabilities (1 moderate, 2 high)",
            "commit 9f8e7d6c5b4a3928374656473829101112131415",
            "NODE_ENV=production",
            "PORT=3000",
        ] {
            assert_eq!(redact(line), line, "redaction damaged: {line}");
        }
    }

    #[test]
    fn the_registry_exposes_both_providers_with_distinct_ids() {
        let all = providers();
        assert_eq!(all.len(), 2);
        let ids: Vec<&str> = all.iter().map(|p| p.id()).collect();
        assert!(ids.contains(&"vercel"), "{ids:?}");
        assert!(ids.contains(&"netlify"), "{ids:?}");

        assert_eq!(provider_by_id("vercel").map(|p| p.label()), Some("Vercel"));
        assert_eq!(provider_by_id("netlify").map(|p| p.label()), Some("Netlify"));
        assert!(provider_by_id("heroku").is_none());
    }

    /// Nothing in the UI may need to know which provider it is talking to, so
    /// every provider has to answer every question the flow asks.
    #[test]
    fn every_provider_answers_the_whole_interface() {
        for provider in providers() {
            assert!(!provider.id().is_empty());
            assert!(!provider.label().is_empty());
            assert!(!provider.cli_binary().is_empty());
            assert!(provider.docs_url().starts_with("https://"));
            assert!(provider.token_env_var().ends_with("TOKEN"));
            assert!(!provider.version_command().args.is_empty());
            assert!(!provider.install_command().args.is_empty());
            assert!(!provider.is_authenticated(None).args.is_empty());
            assert!(provider.authenticate().interactive, "login needs a human");
            assert!(!provider.logout().args.is_empty());
            assert!(!provider.get_projects(None).args.is_empty());
        }
    }

    /// A deploy is a build, not a `--version` check: its ceiling has to be far
    /// above the quick-command default or every real deployment is killed.
    #[test]
    fn deploying_gets_a_much_longer_timeout_than_a_version_check() {
        let plan = crate::deploy::service::tests_support::plan();
        for provider in providers() {
            let deploy = provider.deploy(&plan);
            assert!(
                deploy.timeout_secs >= 600,
                "{} deploy timeout is only {}s",
                provider.id(),
                deploy.timeout_secs
            );
            assert!(provider.version_command().timeout_secs <= 60);
        }
    }

    /// argv is world-readable through `ps`; a process environment is not.
    #[test]
    fn no_provider_ever_puts_a_token_in_argv() {
        let mut plan = crate::deploy::service::tests_support::plan();
        plan.token = Some(Secret::new("tok_do_not_leak"));
        for provider in providers() {
            let commands = [
                Some(provider.deploy(&plan)),
                Some(provider.is_authenticated(plan.token.as_ref())),
                Some(provider.get_projects(plan.token.as_ref())),
                provider.link_project(&plan),
                provider.create_project(&plan),
            ];
            for command in commands.into_iter().flatten() {
                assert!(
                    !command.args.iter().any(|a| a.contains("tok_do_not_leak")),
                    "{} leaked a token into argv: {:?}",
                    provider.id(),
                    command.args
                );
                assert!(!command.display().contains("tok_do_not_leak"));
            }
        }
    }
}
