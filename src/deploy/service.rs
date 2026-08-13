//! The deployment flow, as a state machine with no I/O in it.
//!
//! This is where "select a provider, check the CLI, check auth, link, deploy"
//! actually lives. It holds no processes, opens no sockets and touches no
//! files: it is handed events and returns [`DeployAction`]s describing what
//! should happen next, and `main.rs`'s event loop is the only thing that
//! carries them out.
//!
//! That split is the same one `App` already makes for token usage and quota
//! writes, and for the same reason: it makes the entire flow -- every branch,
//! every error path, every retry -- testable by feeding it canned
//! [`CommandOutput`]s, with no CLI installed, nobody signed in and no network.
//! A deployment integration whose failure paths can only be exercised by
//! actually failing a deployment is one whose failure paths are never
//! exercised.
//!
//! ```text
//!   Menu(Provider) → Menu(Confirm) → Menu(Settings) ⇄ Menu(EditField) → Prompt(…)
//!                                          ↓
//!                                      Menu(Env) ⇄ Prompt(EnvKey) → Prompt(EnvValue)
//!                                          ↓
//!                                      Menu(Target)
//!                                          ↓
//!   Working(CheckingCli) → Menu(InstallCli) → Working(InstallingCli)
//!                                          ↓
//!   Working(CheckingAuth) → Menu(Login) → Working(LoggingIn) / Prompt(Token)
//!                                          ↓
//!   Working(ListingProjects) → Menu(Link) → Working(Creating/LinkingProject)
//!                                          ↓
//!   Working(Deploying) → Finished  |  Menu(Failure) → retry / logs / cancel
//! ```

use super::cli::{self, CliState};
use super::detect::ProjectProfile;
use super::history::{self, Deployment};
use super::runner::CommandOutput;
use super::{
    provider_by_id, providers, AuthState, DeployPlan, DeployStatus, DeploymentProvider, EnvVar,
    LinkChoice, ProviderCommand, RemoteProject, Secret, Target,
};
use std::collections::VecDeque;
use std::path::PathBuf;

/// How much of the log goes back to the model when a deployment fails. Enough
/// to diagnose a build error, bounded so a webpack log cannot eat the context
/// window -- the same trade `max_output_bytes` makes for a shell command.
const REPORTED_LOG_LINES: usize = 60;

/// How many streamed log lines are kept. A cap, not a limit on the deployment:
/// the runner already bounds what it captures, and the panel only ever shows
/// the tail anyway.
const MAX_LOG_LINES: usize = 400;

/// A screen offering a choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    Provider,
    Confirm,
    Settings,
    EditField,
    Env,
    Target,
    Link,
    InstallCli,
    Login,
    Failure,
}

/// A screen asking for a line of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    Name,
    BuildCommand,
    OutputDir,
    EnvKey,
    EnvValue,
    Token,
}

impl Prompt {
    /// Whether what is typed should be shown as dots.
    pub fn masked(self) -> bool {
        matches!(self, Prompt::EnvValue | Prompt::Token)
    }

    pub fn title(self) -> &'static str {
        match self {
            Prompt::Name => " Project name ",
            Prompt::BuildCommand => " Build command ",
            Prompt::OutputDir => " Output directory ",
            Prompt::EnvKey => " Variable name ",
            Prompt::EnvValue => " Variable value ",
            Prompt::Token => " API token ",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Prompt::Name => "lowercase letters, digits and hyphens (Enter to confirm, Esc to go back)",
            Prompt::BuildCommand => "leave blank for no build step (Enter to confirm, Esc to go back)",
            Prompt::OutputDir => "relative to the project root, e.g. dist (Enter to confirm, Esc to go back)",
            Prompt::EnvKey => "e.g. API_URL — the name only (Enter to confirm, Esc to go back)",
            Prompt::EnvValue => "hidden as you type, never logged or stored (Enter to confirm, Esc to go back)",
            Prompt::Token => "hidden as you type, kept in memory for this deployment only (Enter to confirm, Esc to go back)",
        }
    }
}

/// A unit of work handed to the event loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    CheckingCli,
    InstallingCli,
    CheckingAuth,
    SigningOut,
    LoggingIn,
    ListingProjects,
    CreatingProject,
    LinkingProject,
    Deploying,
    /// Asking the provider what it thinks of the deployment that just
    /// finished. Never able to fail the deployment -- the deploy command's own
    /// exit code already settled that -- so this only ever adds detail.
    CheckingStatus,
}

impl Step {
    /// Present tense, for the spinner.
    pub fn verb(self) -> &'static str {
        match self {
            Step::CheckingCli => "Checking for the CLI",
            Step::InstallingCli => "Installing the CLI",
            Step::CheckingAuth => "Checking authentication",
            Step::SigningOut => "Clearing the stored session",
            Step::LoggingIn => "Waiting for the browser login",
            Step::ListingProjects => "Fetching your projects",
            Step::CreatingProject => "Creating the project",
            Step::LinkingProject => "Linking the project",
            Step::Deploying => "Building and uploading",
            Step::CheckingStatus => "Confirming the deployment",
        }
    }

    /// Past tense, for the checklist a finished step leaves behind.
    pub fn done_label(self) -> &'static str {
        match self {
            Step::CheckingCli => "CLI available",
            Step::InstallingCli => "CLI installed",
            Step::CheckingAuth => "Authenticated",
            Step::SigningOut => "Stored session cleared",
            Step::LoggingIn => "Signed in",
            Step::ListingProjects => "Projects fetched",
            Step::CreatingProject => "Project created",
            Step::LinkingProject => "Project linked",
            Step::Deploying => "Built and uploaded",
            Step::CheckingStatus => "Confirmed live",
        }
    }

    /// Whether this step's output is worth showing live. A `--version` check
    /// producing one line does not need a log panel; a build does.
    pub fn is_verbose(self) -> bool {
        matches!(self, Step::InstallingCli | Step::Deploying)
    }
}

/// Where a step got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Running,
    Done,
    Failed,
    Skipped,
}

impl StepState {
    pub fn glyph(self) -> &'static str {
        match self {
            StepState::Running => "·",
            StepState::Done => "✔",
            StepState::Failed => "✖",
            StepState::Skipped => "—",
        }
    }
}

/// One line of the progress checklist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepLine {
    pub label: String,
    pub state: StepState,
}

/// Which screen the flow is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Menu(Menu),
    Prompt(Prompt),
    Working(Step),
    /// Deployed. `session.url` carries where to.
    Finished,
}

/// One entry on a menu screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuOption {
    pub label: String,
    /// A dimmer second line, where the choice needs explaining.
    pub detail: Option<String>,
}

impl MenuOption {
    fn plain(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
        }
    }

    fn with_detail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: Some(detail.into()),
        }
    }
}

/// What the event loop must do next. Returned by the session, never performed
/// by it.
#[derive(Clone, Debug, PartialEq)]
pub enum DeployAction {
    /// Run it with pipes and stream the output back.
    Run {
        step: Step,
        command: ProviderCommand,
        cwd: PathBuf,
    },
    /// Run it attached to the real terminal, which the caller must free first.
    RunInteractive {
        step: Step,
        command: ProviderCommand,
        cwd: PathBuf,
    },
    /// Append to the local history file.
    Record(Box<Deployment>),
}

/// What comes back from the event loop.
#[derive(Clone, Debug, PartialEq)]
pub enum DeployEvent {
    /// One line of output, already redacted by the runner.
    Log(String),
    Finished { step: Step, output: CommandOutput },
}

/// The parts of `[deploy]` the flow itself has to obey, copied in at the start
/// so the session never reaches for global config mid-flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeployPolicy {
    pub allow_cli_install: bool,
}

impl Default for DeployPolicy {
    fn default() -> Self {
        Self {
            allow_cli_install: true,
        }
    }
}

/// The whole flow's state.
pub struct DeploySession {
    pub profile: ProjectProfile,
    pub provider_id: Option<&'static str>,
    pub policy: DeployPolicy,

    // ---- the plan being assembled ----
    pub project_name: String,
    pub build_command: Option<String>,
    pub output_dir: Option<String>,
    pub env: Vec<EnvVar>,
    pub target: Target,
    pub link: LinkChoice,
    pub remote_projects: Vec<RemoteProject>,
    /// Held in memory for this session only, never written anywhere.
    pub token: Option<Secret>,
    /// Who the provider says we are, for the progress panel.
    pub identity: Option<String>,

    // ---- screen state ----
    pub stage: Stage,
    pub selected: usize,
    /// Text-entry buffer for `Stage::Prompt`.
    pub input: String,
    pub input_cursor: usize,
    /// Half-entered variable, between the name and the value screens.
    pending_env_key: String,

    // ---- progress ----
    pub steps: Vec<StepLine>,
    pub log: VecDeque<String>,
    pub show_full_log: bool,
    pub url: Option<String>,
    /// What the provider said about the finished deployment, when it was
    /// asked. Purely informational.
    pub status_detail: Option<String>,
    pub failure: Option<String>,
    /// Why the guardrails flag the CLI's install command, shown on the install
    /// prompt. `None` when they have nothing to say about it.
    pub install_reason: Option<String>,
    pub started: Option<std::time::Instant>,
    pub finished_status: Option<DeployStatus>,
    /// True when the model asked for this, rather than the user typing
    /// `/deploy`. Changes only how it ends: the result goes back to the model
    /// as a tool result instead of sitting on screen waiting to be dismissed.
    pub driven_by_model: bool,
}

impl DeploySession {
    pub fn new(profile: ProjectProfile, policy: DeployPolicy) -> Self {
        Self {
            project_name: profile.name.clone(),
            build_command: profile.build_command.clone(),
            output_dir: profile.output_dir.clone(),
            profile,
            provider_id: None,
            policy,
            env: Vec::new(),
            target: Target::Production,
            link: LinkChoice::New,
            remote_projects: Vec::new(),
            token: None,
            identity: None,
            stage: Stage::Menu(Menu::Provider),
            selected: 0,
            input: String::new(),
            input_cursor: 0,
            pending_env_key: String::new(),
            steps: Vec::new(),
            log: VecDeque::new(),
            show_full_log: false,
            url: None,
            status_detail: None,
            failure: None,
            install_reason: None,
            started: None,
            finished_status: None,
            driven_by_model: false,
        }
    }

    /// A session the model asked for, rather than one the user is clicking
    /// through: the provider and target come from the tool call, so every
    /// screen before the work itself is already answered and skipped.
    ///
    /// From there it is the *same* session as `/deploy` -- which is the whole
    /// point. A missing CLI still raises the install prompt with the
    /// guardrails' verdict on it, a browser login still hands over the real
    /// terminal, a failure still offers retry. None of that can happen inside
    /// a tool executor, which has no UI and cannot take the terminal.
    pub fn for_agent(
        profile: ProjectProfile,
        policy: DeployPolicy,
        provider_id: &'static str,
        target: Target,
    ) -> (Self, Option<DeployAction>) {
        let mut session = Self::new(profile, policy);
        session.provider_id = Some(provider_id);
        session.target = target;
        session.driven_by_model = true;
        let action = session.begin_work();
        (session, action)
    }

    pub fn provider(&self) -> Option<Box<dyn DeploymentProvider>> {
        self.provider_id.and_then(provider_by_id)
    }

    /// One line for the model when this session ends, whatever way it ended.
    ///
    /// Carries the build log on failure: the reason to let a model deploy at
    /// all is that it can read what broke and fix it.
    pub fn report(&self) -> String {
        // Checked before the failure branch below: cancelling sets a failure
        // reason too, and "the user stopped it" and "it broke" call for
        // opposite responses -- one should be retried after a fix, the other
        // should not be retried at all.
        if self.finished_status == Some(DeployStatus::Cancelled) {
            return "The user cancelled the deployment before it finished. Do not retry it; \
                    ask what they would like to do instead."
                .to_string();
        }
        let provider = self.provider_label();
        match (&self.url, &self.failure) {
            (Some(url), _) => format!(
                "Deployed successfully to {provider}.\n\n{} URL: {url}\n\nTell the user the URL.",
                self.target.label()
            ),
            (None, Some(reason)) => {
                let tail = tail_of(&self.log, REPORTED_LOG_LINES);
                format!(
                    "The deployment did not finish.\n\nReason: {reason}\n\n\
                     --- last output ---\n{tail}\n\n\
                     If this was a build failure, read the log, fix the real problem, and only \
                     then deploy again. If the user declined or cancelled, do not retry."
                )
            }
            (None, None) => {
                "The user cancelled the deployment. Do not retry it; ask what they want instead."
                    .to_string()
            }
        }
    }

    pub fn provider_label(&self) -> String {
        self.provider()
            .map(|p| p.label().to_string())
            .unwrap_or_else(|| "a provider".to_string())
    }

    /// The plan as it currently stands.
    pub fn plan(&self) -> DeployPlan {
        DeployPlan {
            root: self.profile.root.clone(),
            project_name: self.project_name.clone(),
            framework: self.profile.framework.clone(),
            build_command: self.build_command.clone(),
            output_dir: self.output_dir.clone(),
            env: self.env.clone(),
            target: self.target,
            link: self.link.clone(),
            token: self.token.clone(),
        }
    }

    // ---- rendering inputs -------------------------------------------------

    /// The title of the current screen.
    pub fn title(&self) -> String {
        match &self.stage {
            Stage::Menu(Menu::Provider) => " Select deployment provider ".to_string(),
            Stage::Menu(Menu::Confirm) => " Continue with deployment? ".to_string(),
            Stage::Menu(Menu::Settings) => " Use these settings? ".to_string(),
            Stage::Menu(Menu::EditField) => " Edit configuration ".to_string(),
            Stage::Menu(Menu::Env) => " Environment variables ".to_string(),
            Stage::Menu(Menu::Target) => " Deployment type ".to_string(),
            Stage::Menu(Menu::Link) => format!(" Which {} project? ", self.provider_label()),
            Stage::Menu(Menu::InstallCli) => " CLI required ".to_string(),
            Stage::Menu(Menu::Login) => format!(" Sign in to {} ", self.provider_label()),
            Stage::Menu(Menu::Failure) => " Deployment failed ".to_string(),
            Stage::Prompt(prompt) => prompt.title().to_string(),
            Stage::Working(step) => format!(" {} ", step.verb()),
            Stage::Finished => " Deployment successful ".to_string(),
        }
    }

    /// The choices on the current screen. Empty for non-menu stages.
    pub fn options(&self) -> Vec<MenuOption> {
        match &self.stage {
            Stage::Menu(Menu::Provider) => providers()
                .iter()
                .map(|p| MenuOption::with_detail(p.label(), format!("via the {} CLI", p.cli_binary())))
                .collect(),
            Stage::Menu(Menu::Confirm) => vec![
                MenuOption::plain("Yes"),
                MenuOption::plain("No"),
            ],
            Stage::Menu(Menu::Settings) => vec![
                MenuOption::plain("Yes"),
                MenuOption::plain("Edit configuration"),
            ],
            Stage::Menu(Menu::EditField) => vec![
                MenuOption::with_detail("Project name", self.project_name.clone()),
                MenuOption::with_detail(
                    "Build command",
                    self.build_command.clone().unwrap_or_else(|| "none".to_string()),
                ),
                MenuOption::with_detail(
                    "Output directory",
                    self.output_dir.clone().unwrap_or_else(|| {
                        if self.profile.framework.output_is_provider_managed() {
                            "handled by the provider".to_string()
                        } else {
                            "none".to_string()
                        }
                    }),
                ),
                MenuOption::plain("Done"),
            ],
            Stage::Menu(Menu::Env) => {
                let mut options = vec![MenuOption::plain("Add environment variable")];
                // Names only, never values -- the list is a reminder of what is
                // set, not a place to read a secret back out of. `masked()` is
                // a fixed width, so it does not leak the length either.
                for var in &self.env {
                    options.push(MenuOption::with_detail(
                        format!("{} = {}", var.key, var.value.masked()),
                        "select to remove",
                    ));
                }
                options.push(MenuOption::plain(if self.env.is_empty() {
                    "Continue without variables".to_string()
                } else {
                    format!("Continue with {} variable(s)", self.env.len())
                }));
                options
            }
            Stage::Menu(Menu::Target) => vec![
                MenuOption::with_detail("Production", "the live site"),
                MenuOption::with_detail("Preview", "a throwaway URL, the live site untouched"),
            ],
            Stage::Menu(Menu::Link) => {
                let mut options = vec![MenuOption::with_detail(
                    format!("Create '{}'", self.project_name),
                    "a new project on this account",
                )];
                for project in &self.remote_projects {
                    options.push(MenuOption {
                        label: project.name.clone(),
                        detail: project.url.clone(),
                    });
                }
                options
            }
            Stage::Menu(Menu::InstallCli) => {
                let command = self
                    .provider()
                    .map(|p| p.install_command().display())
                    .unwrap_or_default();
                vec![
                    MenuOption::with_detail("Yes", command.trim_start_matches("$ ").to_string()),
                    MenuOption::plain("No"),
                ]
            }
            Stage::Menu(Menu::Login) => vec![
                MenuOption::with_detail(
                    "Log in with a browser",
                    "hands the terminal to the provider's own login, then comes back",
                ),
                MenuOption::with_detail("Paste an API token", "used for this session only"),
                // The recovery path for a session that exists and is stale --
                // a rotated token, or an account switched elsewhere. Logging in
                // over the top of one of those often fails in a way that reads
                // as a bug in the login itself.
                MenuOption::with_detail(
                    "Sign out, then sign in",
                    "clears the CLI's stored session first",
                ),
                MenuOption::plain("Cancel"),
            ],
            Stage::Menu(Menu::Failure) => vec![
                MenuOption::plain(if self.show_full_log {
                    "Hide detailed logs"
                } else {
                    "View detailed logs"
                }),
                MenuOption::plain("Retry deployment"),
                MenuOption::plain("Cancel"),
            ],
            _ => Vec::new(),
        }
    }

    /// The detected/configured summary shown above the choices, as label/value
    /// pairs. Values are all non-secret by construction.
    pub fn summary(&self) -> Vec<(String, String)> {
        let mut rows = Vec::new();
        match self.stage {
            Stage::Menu(Menu::Confirm) => {
                rows.push(("Project".to_string(), self.profile.name.clone()));
                rows.push(("Framework".to_string(), self.profile.framework.label()));
                rows.push((
                    "Directory".to_string(),
                    self.profile.root.display().to_string(),
                ));
                rows.push(("Provider".to_string(), self.provider_label()));
                // Worth knowing before choosing: an existing config file means
                // the provider's own settings win over anything set here.
                if let Some(id) = self.provider_id {
                    if self.profile.configured_for(id) {
                        rows.push((
                            "Existing config".to_string(),
                            "found — its settings take precedence".to_string(),
                        ));
                    }
                }
            }
            Stage::Menu(Menu::Settings) | Stage::Menu(Menu::EditField) => {
                rows.push(("Framework".to_string(), self.profile.framework.label()));
                rows.push((
                    "Package manager".to_string(),
                    self.profile.package_manager.label().to_string(),
                ));
                rows.push(("Name".to_string(), self.project_name.clone()));
                rows.push((
                    "Build command".to_string(),
                    self.build_command.clone().unwrap_or_else(|| "none".to_string()),
                ));
                rows.push((
                    "Output directory".to_string(),
                    self.output_dir.clone().unwrap_or_else(|| {
                        if self.profile.framework.output_is_provider_managed() {
                            "handled by the provider".to_string()
                        } else {
                            "none".to_string()
                        }
                    }),
                ));
            }
            Stage::Finished => {
                rows.push(("Provider".to_string(), self.provider_label()));
                rows.push(("Type".to_string(), self.target.label().to_string()));
            }
            _ => {}
        }
        rows
    }

    // ---- driving the flow -------------------------------------------------

    /// Move the highlight. Wraps, matching the slash-command menu.
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.options().len();
        if count == 0 {
            return;
        }
        let current = self.selected.min(count - 1) as isize;
        let next = (current + delta).rem_euclid(count as isize);
        self.selected = next as usize;
    }

    /// Commit the highlighted choice.
    pub fn select(&mut self) -> Option<DeployAction> {
        let Stage::Menu(menu) = self.stage else {
            return None;
        };
        let index = self.selected.min(self.options().len().saturating_sub(1));
        self.selected = 0;

        match menu {
            Menu::Provider => {
                let all = providers();
                let provider = all.get(index)?;
                self.provider_id = provider_by_id(provider.id()).map(|_| leak_id(provider.id()));
                self.stage = Stage::Menu(Menu::Confirm);
                None
            }
            Menu::Confirm => {
                if index == 0 {
                    self.stage = Stage::Menu(Menu::Settings);
                } else {
                    self.abandon(DeployStatus::Cancelled, "Cancelled before anything ran.");
                }
                None
            }
            Menu::Settings => {
                self.stage = if index == 0 {
                    Stage::Menu(Menu::Env)
                } else {
                    Stage::Menu(Menu::EditField)
                };
                None
            }
            Menu::EditField => {
                match index {
                    0 => self.open_prompt(Prompt::Name, self.project_name.clone()),
                    1 => self.open_prompt(
                        Prompt::BuildCommand,
                        self.build_command.clone().unwrap_or_default(),
                    ),
                    2 => self.open_prompt(
                        Prompt::OutputDir,
                        self.output_dir.clone().unwrap_or_default(),
                    ),
                    _ => self.stage = Stage::Menu(Menu::Settings),
                }
                None
            }
            Menu::Env => {
                let last = self.options().len().saturating_sub(1);
                if index == 0 {
                    self.open_prompt(Prompt::EnvKey, String::new());
                } else if index == last {
                    self.stage = Stage::Menu(Menu::Target);
                } else {
                    // Selecting a set variable removes it. There is deliberately
                    // no "edit": the old value cannot be shown to edit against,
                    // so re-entering it is the only honest option.
                    let removing = index - 1;
                    if removing < self.env.len() {
                        self.env.remove(removing);
                    }
                }
                None
            }
            Menu::Target => {
                self.target = if index == 0 {
                    Target::Production
                } else {
                    Target::Preview
                };
                self.begin_work()
            }
            Menu::Link => {
                self.link = if index == 0 {
                    LinkChoice::New
                } else {
                    match self.remote_projects.get(index - 1) {
                        Some(project) => LinkChoice::Existing(project.clone()),
                        None => LinkChoice::New,
                    }
                };
                self.after_link_choice()
            }
            Menu::InstallCli => {
                if index == 0 {
                    let provider = self.provider()?;
                    Some(self.start(Step::InstallingCli, provider.install_command()))
                } else {
                    let binary = self
                        .provider()
                        .map(|p| p.cli_binary().to_string())
                        .unwrap_or_else(|| "the provider".to_string());
                    self.fail(format!(
                        "The {binary} CLI is required to deploy and was not installed. Install it \
                         yourself, then ask again."
                    ));
                    None
                }
            }
            Menu::Login => match index {
                0 => {
                    let provider = self.provider()?;
                    let command = provider.authenticate();
                    self.mark_running(Step::LoggingIn);
                    self.stage = Stage::Working(Step::LoggingIn);
                    Some(DeployAction::RunInteractive {
                        step: Step::LoggingIn,
                        command,
                        cwd: self.profile.root.clone(),
                    })
                }
                1 => {
                    self.open_prompt(Prompt::Token, String::new());
                    None
                }
                2 => {
                    let provider = self.provider()?;
                    Some(self.start(Step::SigningOut, provider.logout()))
                }
                _ => {
                    self.fail("Cancelled at the sign-in step.".to_string());
                    None
                }
            },
            Menu::Failure => match index {
                0 => {
                    self.show_full_log = !self.show_full_log;
                    None
                }
                1 => {
                    // A retry starts the machinery again from the top, keeping
                    // the configuration and any token already supplied -- the
                    // usual cause is a build error just fixed in another window.
                    self.failure = None;
                    self.finished_status = None;
                    self.steps.clear();
                    self.log.clear();
                    self.show_full_log = false;
                    self.begin_work()
                }
                _ => {
                    self.stage = Stage::Finished;
                    self.finished_status = Some(DeployStatus::Failed);
                    None
                }
            },
        }
    }

    /// Commit the text on a `Stage::Prompt` screen.
    pub fn submit_prompt(&mut self) -> Option<DeployAction> {
        let Stage::Prompt(prompt) = self.stage else {
            return None;
        };
        let value = std::mem::take(&mut self.input).trim().to_string();
        self.input_cursor = 0;
        self.selected = 0;

        match prompt {
            Prompt::Name => {
                let cleaned = super::detect::sanitize_name(&value);
                if !cleaned.is_empty() {
                    self.project_name = cleaned;
                }
                self.stage = Stage::Menu(Menu::EditField);
                None
            }
            Prompt::BuildCommand => {
                self.build_command = (!value.is_empty()).then_some(value);
                self.stage = Stage::Menu(Menu::EditField);
                None
            }
            Prompt::OutputDir => {
                self.output_dir = (!value.is_empty()).then_some(value);
                self.stage = Stage::Menu(Menu::EditField);
                None
            }
            Prompt::EnvKey => {
                if value.is_empty() {
                    self.stage = Stage::Menu(Menu::Env);
                    return None;
                }
                self.pending_env_key = value;
                self.open_prompt(Prompt::EnvValue, String::new());
                None
            }
            Prompt::EnvValue => {
                let key = std::mem::take(&mut self.pending_env_key);
                if !key.is_empty() {
                    // Replace rather than duplicate: two entries for one name
                    // would silently make the second one win at the far end.
                    self.env.retain(|existing| existing.key != key);
                    self.env.push(EnvVar {
                        key,
                        value: Secret::new(value),
                    });
                }
                self.stage = Stage::Menu(Menu::Env);
                None
            }
            Prompt::Token => {
                if value.is_empty() {
                    self.stage = Stage::Menu(Menu::Login);
                    return None;
                }
                self.token = Some(Secret::new(value));
                let provider = self.provider()?;
                let command = provider.is_authenticated(self.token.as_ref());
                Some(self.start(Step::CheckingAuth, command))
            }
        }
    }

    /// Esc. On a menu it steps back; while something is running it stops it.
    ///
    /// Returns true when the whole session should close.
    pub fn back(&mut self) -> bool {
        match self.stage {
            // Once there is a URL the deployment is already live, so stopping
            // the confirmation query that follows it must not relabel a
            // successful deployment as cancelled.
            Stage::Working(_) if self.url.is_some() => {
                self.mark_last(StepState::Skipped);
                self.stage = Stage::Finished;
                self.finished_status = Some(DeployStatus::Success);
                false
            }
            Stage::Working(_) => {
                self.mark_last(StepState::Failed);
                self.abandon(DeployStatus::Cancelled, "Cancelled while it was running.");
                false
            }
            Stage::Menu(Menu::Provider) | Stage::Finished => true,
            Stage::Menu(Menu::Confirm) => {
                self.stage = Stage::Menu(Menu::Provider);
                self.selected = 0;
                false
            }
            Stage::Menu(Menu::Settings) => {
                self.stage = Stage::Menu(Menu::Confirm);
                self.selected = 0;
                false
            }
            Stage::Menu(Menu::EditField) => {
                self.stage = Stage::Menu(Menu::Settings);
                self.selected = 0;
                false
            }
            Stage::Menu(Menu::Env) => {
                self.stage = Stage::Menu(Menu::Settings);
                self.selected = 0;
                false
            }
            Stage::Menu(Menu::Target) => {
                self.stage = Stage::Menu(Menu::Env);
                self.selected = 0;
                false
            }
            // Past the point where "back" has an obvious meaning: these are
            // decisions about work already under way, so Esc ends the attempt
            // rather than rewinding to a screen whose answers no longer apply.
            Stage::Menu(_) => {
                self.abandon(DeployStatus::Cancelled, "Cancelled.");
                false
            }
            Stage::Prompt(Prompt::EnvValue) => {
                self.pending_env_key.clear();
                self.input.clear();
                self.input_cursor = 0;
                self.stage = Stage::Menu(Menu::Env);
                false
            }
            Stage::Prompt(Prompt::EnvKey) => {
                self.input.clear();
                self.input_cursor = 0;
                self.stage = Stage::Menu(Menu::Env);
                false
            }
            Stage::Prompt(Prompt::Token) => {
                self.input.clear();
                self.input_cursor = 0;
                self.stage = Stage::Menu(Menu::Login);
                false
            }
            Stage::Prompt(_) => {
                self.input.clear();
                self.input_cursor = 0;
                self.stage = Stage::Menu(Menu::EditField);
                false
            }
        }
    }

    /// A result, or a line of output, from the event loop.
    pub fn on_event(&mut self, event: DeployEvent) -> Option<DeployAction> {
        match event {
            DeployEvent::Log(line) => {
                self.push_log(line);
                None
            }
            DeployEvent::Finished { step, output } => self.on_finished(step, output),
        }
    }

    fn on_finished(&mut self, step: Step, output: CommandOutput) -> Option<DeployAction> {
        // A result for a step that is no longer the one in flight belongs to a
        // cancelled attempt: same reasoning as `App`'s stale-request-id guard.
        if self.stage != Stage::Working(step) {
            return None;
        }
        let provider = self.provider()?;

        match step {
            Step::CheckingCli => match cli::parse_version(&output) {
                CliState::Present(version) => {
                    self.mark_last_labelled(
                        StepState::Done,
                        format!("{} CLI {version}", provider.label()),
                    );
                    Some(self.start(
                        Step::CheckingAuth,
                        provider.is_authenticated(self.token.as_ref()),
                    ))
                }
                CliState::Missing => {
                    self.mark_last(StepState::Skipped);
                    if self.policy.allow_cli_install {
                        let install = provider.install_command();
                        let line =
                            format!("{} {}", install.program, install.args.join(" "));
                        // Judged by the same guardrails every shell command
                        // faces, so the prompt says the same thing about
                        // `npm install -g` that the tool-approval prompt would
                        // -- and a future provider cannot smuggle in an install
                        // command the guardrails would refuse outright.
                        match cli::may_offer_install(&line, &self.profile.root) {
                            Err(reason) => {
                                self.fail(format!(
                                    "The {} CLI is missing, and its install command is one the \
                                     safety guardrails refuse to run: {reason}. Install it \
                                     yourself, then ask again.",
                                    provider.label()
                                ));
                                return None;
                            }
                            Ok(reason) => self.install_reason = reason,
                        }
                        self.stage = Stage::Menu(Menu::InstallCli);
                        self.selected = 0;
                    } else {
                        // Turned off by config. Say what to run rather than
                        // failing with "not installed" and leaving it there.
                        self.fail(format!(
                            "The {} CLI is not installed, and installing from inside this app is \
                             turned off (`allow_cli_install = false` under [deploy]). Install it \
                             yourself, then ask again:\n{}",
                            provider.label(),
                            provider.install_command().display()
                        ));
                    }
                    None
                }
                CliState::Broken(detail) => {
                    self.mark_last(StepState::Failed);
                    self.fail(format!(
                        "The {} CLI is installed but is not working: {detail}\nSee {}",
                        provider.label(),
                        provider.docs_url()
                    ));
                    None
                }
            },

            Step::InstallingCli => {
                if output.success() {
                    self.mark_last(StepState::Done);
                    // Verified rather than assumed: a package manager can exit
                    // zero and still leave nothing on PATH, and finding that out
                    // at the deploy step would blame the wrong thing.
                    Some(self.start(Step::CheckingCli, provider.version_command()))
                } else {
                    self.mark_last(StepState::Failed);
                    self.fail(format!(
                        "Installing the {} CLI failed. Install it yourself, then ask again:\n{}",
                        provider.label(),
                        provider.install_command().display()
                    ));
                    None
                }
            }

            Step::CheckingAuth => match provider.parse_auth(&output) {
                AuthState::In(identity) => {
                    self.identity = Some(identity.clone());
                    self.mark_last_labelled(StepState::Done, format!("Signed in as {identity}"));
                    Some(self.start(
                        Step::ListingProjects,
                        provider.get_projects(self.token.as_ref()),
                    ))
                }
                AuthState::Out => {
                    self.mark_last(StepState::Skipped);
                    self.stage = Stage::Menu(Menu::Login);
                    self.selected = 0;
                    None
                }
                AuthState::Unknown(detail) => {
                    // Not treated as signed out: sending someone through a login
                    // they may not need is a worse answer than saying what
                    // happened and offering the choice.
                    self.mark_last(StepState::Skipped);
                    self.push_log(format!("Could not tell whether you are signed in: {detail}"));
                    self.stage = Stage::Menu(Menu::Login);
                    self.selected = 0;
                    None
                }
            },

            // Whether it worked or not, a browser login is the next step: a
            // logout that fails usually means there was no session to clear,
            // which is exactly the state we were trying to reach.
            Step::SigningOut => {
                self.mark_last(if output.success() {
                    StepState::Done
                } else {
                    StepState::Skipped
                });
                self.token = None;
                self.identity = None;
                let command = provider.authenticate();
                self.mark_running(Step::LoggingIn);
                self.stage = Stage::Working(Step::LoggingIn);
                Some(DeployAction::RunInteractive {
                    step: Step::LoggingIn,
                    command,
                    cwd: self.profile.root.clone(),
                })
            }

            Step::LoggingIn => {
                if output.success() {
                    self.mark_last(StepState::Done);
                } else {
                    self.mark_last(StepState::Failed);
                }
                // Either way, ask the provider rather than trusting the exit
                // code: a login that reports success and leaves no session is
                // exactly the case that produces a baffling failure later.
                Some(self.start(
                    Step::CheckingAuth,
                    provider.is_authenticated(self.token.as_ref()),
                ))
            }

            Step::ListingProjects => {
                // A listing that fails is not fatal -- creating a new project
                // does not depend on it, so the flow continues with an empty
                // list rather than refusing over a nice-to-have.
                self.remote_projects = if output.success() {
                    provider.parse_projects(&output)
                } else {
                    Vec::new()
                };
                self.mark_last_labelled(
                    StepState::Done,
                    format!("{} project(s) found", self.remote_projects.len()),
                );
                // Skip a choice with only one answer.
                if self.remote_projects.is_empty() {
                    self.link = LinkChoice::New;
                    self.after_link_choice()
                } else {
                    self.stage = Stage::Menu(Menu::Link);
                    self.selected = 0;
                    None
                }
            }

            Step::CreatingProject | Step::LinkingProject => {
                if output.success() {
                    self.mark_last(StepState::Done);
                    Some(self.start(Step::Deploying, provider.deploy(&self.plan())))
                } else {
                    self.mark_last(StepState::Failed);
                    self.fail(provider.explain_failure(&output));
                    None
                }
            }

            Step::Deploying => {
                if output.success() {
                    self.mark_last(StepState::Done);
                    self.url = provider.get_deployment_url(&output);
                    // Ask the provider to confirm, where it has an answer worth
                    // having. The deployment is already a success either way --
                    // this only adds detail, and cannot take it back.
                    match self
                        .url
                        .clone()
                        .and_then(|url| provider.get_deployment_status(&url, self.token.as_ref()))
                    {
                        Some(command) => Some(self.start(Step::CheckingStatus, command)),
                        None => self.succeed(),
                    }
                } else {
                    self.mark_last(StepState::Failed);
                    let reason = provider.explain_failure(&output);
                    self.fail(reason.clone());
                    Some(self.record(DeployStatus::Failed, Some(reason)))
                }
            }

            Step::CheckingStatus => {
                if output.success() {
                    self.mark_last(StepState::Done);
                    // One line, so the success screen can say what the provider
                    // says rather than only what we inferred from an exit code.
                    self.status_detail = output.last_line();
                } else {
                    // Not a failure of the deployment: it is already live. A
                    // status query that cannot answer just has nothing to add.
                    self.mark_last(StepState::Skipped);
                }
                self.succeed()
            }
        }
    }

    /// Settle a finished deployment as a success and hand back its history
    /// entry. One place, so the two paths into it cannot disagree.
    fn succeed(&mut self) -> Option<DeployAction> {
        self.stage = Stage::Finished;
        self.finished_status = Some(DeployStatus::Success);
        Some(self.record(DeployStatus::Success, None))
    }

    // ---- internals --------------------------------------------------------

    /// Everything from the CLI check onward. Entered from the target screen and
    /// again from a retry.
    fn begin_work(&mut self) -> Option<DeployAction> {
        let provider = self.provider()?;
        self.started = Some(std::time::Instant::now());
        self.steps.push(StepLine {
            label: format!("Project validated ({})", self.profile.framework.label()),
            state: StepState::Done,
        });

        // A token already exported is the cheapest sign-in there is: nothing
        // to type, nothing to copy, no browser. Checked before the CLI, so
        // someone running in CI with `VERCEL_TOKEN` set is never asked
        // anything at all.
        if self.token.is_none() {
            let name = provider.token_env_var();
            if let Some(value) = std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            {
                self.token = Some(Secret::new(value));
                self.steps.push(StepLine {
                    // The name, never the value.
                    label: format!("Using {name} from the environment"),
                    state: StepState::Done,
                });
            }
        }

        Some(self.start(Step::CheckingCli, provider.version_command()))
    }

    /// What to run once the user has said which project to deploy into.
    fn after_link_choice(&mut self) -> Option<DeployAction> {
        let provider = self.provider()?;
        let plan = self.plan();
        let (step, command) = match &self.link {
            LinkChoice::New => match provider.create_project(&plan) {
                Some(command) => (Step::CreatingProject, command),
                None => (Step::Deploying, provider.deploy(&plan)),
            },
            LinkChoice::Existing(_) => match provider.link_project(&plan) {
                Some(command) => (Step::LinkingProject, command),
                None => (Step::Deploying, provider.deploy(&plan)),
            },
        };
        Some(self.start(step, command))
    }

    /// Enter a working stage and hand the command back to the event loop.
    fn start(&mut self, step: Step, command: ProviderCommand) -> DeployAction {
        self.mark_running(step);
        self.stage = Stage::Working(step);
        self.selected = 0;
        if step.is_verbose() {
            self.push_log(command.display());
        }
        DeployAction::Run {
            step,
            command,
            cwd: self.profile.root.clone(),
        }
    }

    fn mark_running(&mut self, step: Step) {
        self.steps.push(StepLine {
            label: step.verb().to_string(),
            state: StepState::Running,
        });
    }

    fn mark_last(&mut self, state: StepState) {
        if let Some(last) = self.steps.last_mut() {
            last.state = state;
            if state == StepState::Done {
                // The checklist reads as a record of what happened, so a
                // finished line switches from "Checking…" to "Checked".
                if let Some(step) = current_step_of(&last.label) {
                    last.label = step.done_label().to_string();
                }
            }
        }
    }

    fn mark_last_labelled(&mut self, state: StepState, label: String) {
        if let Some(last) = self.steps.last_mut() {
            last.state = state;
            last.label = label;
        }
    }

    fn open_prompt(&mut self, prompt: Prompt, initial: String) {
        self.input_cursor = initial.len();
        self.input = initial;
        self.stage = Stage::Prompt(prompt);
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() >= MAX_LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn fail(&mut self, reason: String) {
        self.failure = Some(reason);
        self.finished_status = Some(DeployStatus::Failed);
        self.stage = Stage::Menu(Menu::Failure);
        self.selected = 0;
    }

    /// End the attempt without a provider verdict -- a cancellation, or a
    /// refusal before anything ran.
    fn abandon(&mut self, status: DeployStatus, reason: &str) {
        self.finished_status = Some(status);
        self.failure = Some(reason.to_string());
        self.stage = Stage::Finished;
    }

    /// The history entry for a finished attempt. Carries variable *names*, and
    /// no values -- see `history`'s module doc.
    fn record(&self, status: DeployStatus, detail: Option<String>) -> DeployAction {
        DeployAction::Record(Box::new(Deployment {
            date: history::today(),
            at: history::now_secs(),
            project: self.project_name.clone(),
            path: self.profile.root.display().to_string(),
            provider: self
                .provider_id
                .map(str::to_string)
                .unwrap_or_else(|| "unknown".to_string()),
            target: self.target.label().to_string(),
            status: status.label().to_string(),
            url: self.url.clone(),
            env_keys: self.env.iter().map(|v| v.key.clone()).collect(),
            // Already redacted: everything in `detail` came from
            // `explain_failure`, which reads output the runner scrubbed.
            detail: detail.map(|d| super::redact(&d)),
        }))
    }
}

/// The *last* `lines` lines of the log. A build log's cause is at the end,
/// where the failure is -- the opposite of `tools::clip`, which keeps the head.
fn tail_of(log: &VecDeque<String>, lines: usize) -> String {
    let start = log.len().saturating_sub(lines);
    let tail: Vec<&str> = log.iter().skip(start).map(String::as_str).collect();
    let tail = tail.join("\n");
    if start > 0 {
        format!("[… {start} earlier lines omitted]\n{tail}")
    } else {
        tail
    }
}

/// Recover which step a running checklist line belongs to, so it can be
/// relabelled in the past tense when it finishes.
fn current_step_of(label: &str) -> Option<Step> {
    [
        Step::CheckingCli,
        Step::InstallingCli,
        Step::CheckingAuth,
        Step::SigningOut,
        Step::LoggingIn,
        Step::ListingProjects,
        Step::CreatingProject,
        Step::LinkingProject,
        Step::Deploying,
        Step::CheckingStatus,
    ]
    .into_iter()
    .find(|step| step.verb() == label)
}

/// `providers()` hands back boxed trait objects whose `id()` is already
/// `&'static str`; this just makes that explicit at the one call site that
/// needs to keep the id after the box is dropped.
fn leak_id(id: &str) -> &'static str {
    providers()
        .iter()
        .map(|p| p.id())
        .find(|candidate| *candidate == id)
        .expect("id came from the registry")
}

#[cfg(test)]
pub mod tests_support {
    use super::*;
    use crate::deploy::detect::{Framework, PackageManager};

    /// A profile that needs no filesystem, for tests about the flow rather
    /// than about detection.
    pub fn profile() -> ProjectProfile {
        ProjectProfile {
            root: PathBuf::from("/Users/dev/my-app"),
            name: "my-app".to_string(),
            framework: Framework::Vite,
            package_manager: PackageManager::Npm,
            build_command: Some("npm run build".to_string()),
            output_dir: Some("dist".to_string()),
            markers: vec!["vite.config.ts".to_string()],
            warnings: Vec::new(),
            has_vercel_config: false,
            has_netlify_config: false,
        }
    }

    pub fn plan() -> DeployPlan {
        DeploySession::new(profile(), DeployPolicy::default()).plan()
    }

    pub fn session(provider_id: &'static str) -> DeploySession {
        let mut session = DeploySession::new(profile(), DeployPolicy::default());
        session.provider_id = Some(provider_id);
        session
    }

    pub fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            code: Some(0),
            stdout: stdout.to_string(),
            ..Default::default()
        }
    }

    pub fn err(stderr: &str) -> CommandOutput {
        CommandOutput {
            code: Some(1),
            stderr: stderr.to_string(),
            ..Default::default()
        }
    }

    pub fn missing() -> CommandOutput {
        CommandOutput {
            not_found: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;

    /// Walk the session to the point where work begins, choosing the first
    /// option on every screen: provider, continue, use these settings, no
    /// variables, production.
    fn walk_to_work(provider_id: &'static str) -> (DeploySession, DeployAction) {
        let mut session = DeploySession::new(profile(), DeployPolicy::default());
        // Provider
        session.selected = providers()
            .iter()
            .position(|p| p.id() == provider_id)
            .expect("known provider");
        assert!(session.select().is_none());
        // Continue? → Yes
        session.selected = 0;
        assert!(session.select().is_none());
        // Use these settings? → Yes
        session.selected = 0;
        assert!(session.select().is_none());
        // Environment variables → Continue without
        session.selected = session.options().len() - 1;
        assert!(session.select().is_none());
        // Production
        session.selected = 0;
        let action = session.select().expect("work must start");
        (session, action)
    }

    fn step_of(action: &DeployAction) -> Step {
        match action {
            DeployAction::Run { step, .. } | DeployAction::RunInteractive { step, .. } => *step,
            other => panic!("expected a command, got {other:?}"),
        }
    }

    // ---- the happy path ---------------------------------------------------

    #[test]
    fn a_whole_successful_deployment_runs_end_to_end() {
        let (mut session, first) = walk_to_work("vercel");
        assert_eq!(step_of(&first), Step::CheckingCli);

        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::CheckingCli,
                output: ok("Vercel CLI 33.5.1\n"),
            })
            .expect("auth check follows");
        assert_eq!(step_of(&next), Step::CheckingAuth);

        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::CheckingAuth,
                output: ok("ada\n"),
            })
            .expect("project listing follows");
        assert_eq!(step_of(&next), Step::ListingProjects);
        assert_eq!(session.identity.as_deref(), Some("ada"));

        // No projects yet, so the "which project" question has one answer and
        // is not asked.
        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::ListingProjects,
                output: ok(""),
            })
            .expect("linking follows");
        // Vercel creates the project as part of linking, so one command does
        // both -- the step is named for what the user asked for.
        assert_eq!(step_of(&next), Step::CreatingProject);

        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::CreatingProject,
                output: ok("Linked to ada/my-app\n"),
            })
            .expect("deploy follows");
        assert_eq!(step_of(&next), Step::Deploying);

        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::Deploying,
                output: ok("Production: https://my-app.vercel.app\n"),
            })
            .expect("the provider is asked to confirm");
        assert_eq!(step_of(&next), Step::CheckingStatus);
        // The URL is known before the confirmation runs: the deployment is
        // already live, and the query only adds detail.
        assert_eq!(session.url.as_deref(), Some("https://my-app.vercel.app"));

        let recorded = session
            .on_event(DeployEvent::Finished {
                step: Step::CheckingStatus,
                output: ok("status\tREADY\n"),
            })
            .expect("a finished deployment is recorded");

        assert_eq!(session.stage, Stage::Finished);
        assert_eq!(session.status_detail.as_deref(), Some("status\tREADY"));
        assert_eq!(session.url.as_deref(), Some("https://my-app.vercel.app"));
        assert_eq!(session.finished_status, Some(DeployStatus::Success));
        match recorded {
            DeployAction::Record(entry) => {
                assert_eq!(entry.status, "Success");
                assert_eq!(entry.provider, "vercel");
                assert_eq!(entry.url.as_deref(), Some("https://my-app.vercel.app"));
            }
            other => panic!("expected a history record, got {other:?}"),
        }
    }

    /// Netlify creates the site explicitly where Vercel folds it into linking.
    /// The flow must not care which.
    #[test]
    fn netlify_creates_a_site_where_vercel_links_one() {
        let (mut session, _) = walk_to_work("netlify");
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: ok("netlify-cli/17.10.1\n"),
        });
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingAuth,
            output: ok("Current Netlify User\n  Email: ada@example.com\n"),
        });
        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::ListingProjects,
                output: ok("[]"),
            })
            .expect("creation follows");
        assert_eq!(step_of(&next), Step::CreatingProject);
    }

    // ---- CLI installation -------------------------------------------------

    #[test]
    fn a_missing_cli_asks_before_installing_anything() {
        let (mut session, _) = walk_to_work("vercel");
        let nothing = session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: missing(),
        });
        // Crucially: no action. Nothing is installed without a decision.
        assert!(nothing.is_none(), "install must not start on its own");
        assert_eq!(session.stage, Stage::Menu(Menu::InstallCli));

        let options = session.options();
        assert_eq!(options[0].label, "Yes");
        assert!(
            options[0].detail.as_deref().unwrap_or_default().contains("npm install -g vercel"),
            "the exact command must be shown: {options:?}"
        );

        session.selected = 0;
        let action = session.select().expect("install starts once allowed");
        assert_eq!(step_of(&action), Step::InstallingCli);
    }

    #[test]
    fn declining_the_install_ends_the_attempt_with_an_explanation() {
        let (mut session, _) = walk_to_work("vercel");
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: missing(),
        });
        session.selected = 1; // No
        assert!(session.select().is_none());
        assert_eq!(session.stage, Stage::Menu(Menu::Failure));
        let failure = session.failure.clone().unwrap_or_default();
        assert!(failure.contains("vercel CLI is required"), "{failure}");
    }

    /// A package manager can exit zero and still leave nothing on PATH.
    /// Finding that out at the deploy step would blame the wrong thing.
    #[test]
    fn a_finished_install_is_verified_rather_than_assumed() {
        let (mut session, _) = walk_to_work("vercel");
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: missing(),
        });
        session.selected = 0;
        session.select();
        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::InstallingCli,
                output: ok("added 1 package\n"),
            })
            .expect("re-check follows");
        assert_eq!(step_of(&next), Step::CheckingCli);
    }

    #[test]
    fn a_broken_cli_is_not_offered_a_reinstall() {
        let (mut session, _) = walk_to_work("vercel");
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: err("Cannot find module 'chalk'"),
        });
        assert_eq!(session.stage, Stage::Menu(Menu::Failure));
        let failure = session.failure.clone().unwrap_or_default();
        assert!(failure.contains("not working"), "{failure}");
    }

    // ---- authentication ---------------------------------------------------

    #[test]
    fn being_signed_out_offers_a_browser_login_and_a_token() {
        let (mut session, _) = walk_to_work("vercel");
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: ok("Vercel CLI 33.5.1"),
        });
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingAuth,
            output: err("Error: No existing credentials found. Please run `vercel login`"),
        });
        assert_eq!(session.stage, Stage::Menu(Menu::Login));

        let labels: Vec<String> = session.options().iter().map(|o| o.label.clone()).collect();
        assert_eq!(labels[0], "Log in with a browser");
        assert_eq!(labels[1], "Paste an API token");

        session.selected = 0;
        let action = session.select().expect("login starts");
        // A browser login needs the real terminal, not a pipe.
        assert!(
            matches!(action, DeployAction::RunInteractive { .. }),
            "{action:?}"
        );
    }

    /// Sending someone through a login they may not need is a worse answer
    /// than saying what happened and offering the choice.
    #[test]
    fn an_unrecognised_auth_answer_explains_itself_rather_than_forcing_a_login() {
        let (mut session, _) = walk_to_work("vercel");
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: ok("Vercel CLI 33.5.1"),
        });
        session.on_event(DeployEvent::Finished {
            step: Step::CheckingAuth,
            output: err("socket hang up"),
        });
        assert_eq!(session.stage, Stage::Menu(Menu::Login));
        assert!(
            session.log.iter().any(|l| l.contains("Could not tell whether you are signed in")),
            "{:?}",
            session.log
        );
    }

    /// A login that reports success and leaves no session is exactly the case
    /// that produces a baffling failure three steps later.
    #[test]
    fn a_finished_login_is_re_verified_with_the_provider() {
        let mut session = session("vercel");
        session.stage = Stage::Working(Step::LoggingIn);
        session.mark_running(Step::LoggingIn);
        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::LoggingIn,
                output: ok(""),
            })
            .expect("re-check follows");
        assert_eq!(step_of(&next), Step::CheckingAuth);
    }

    #[test]
    fn a_pasted_token_is_kept_in_memory_and_re_checked() {
        let mut session = session("vercel");
        session.stage = Stage::Menu(Menu::Login);
        session.selected = 1; // Paste an API token
        assert!(session.select().is_none());
        assert_eq!(session.stage, Stage::Prompt(Prompt::Token));
        assert!(Prompt::Token.masked(), "a token must never be echoed");

        session.input = "vercel_pasted_token".to_string();
        let action = session.submit_prompt().expect("re-checks auth");
        assert_eq!(step_of(&action), Step::CheckingAuth);
        assert!(session.token.is_some());

        // ...and it goes to the child's environment, not its argv.
        match action {
            DeployAction::Run { command, .. } => {
                assert!(!command.display().contains("vercel_pasted_token"));
                assert!(command.env.iter().any(|(k, _)| k == "VERCEL_TOKEN"));
            }
            other => panic!("{other:?}"),
        }
    }

    // ---- configuration ----------------------------------------------------

    #[test]
    fn detected_settings_are_offered_and_can_be_edited() {
        let mut session = session("vercel");
        session.stage = Stage::Menu(Menu::Settings);

        let summary = session.summary();
        assert!(summary.iter().any(|(k, v)| k == "Framework" && v == "Vite"));
        assert!(summary.iter().any(|(k, v)| k == "Build command" && v == "npm run build"));
        assert!(summary.iter().any(|(k, v)| k == "Output directory" && v == "dist"));

        session.selected = 1; // Edit configuration
        session.select();
        assert_eq!(session.stage, Stage::Menu(Menu::EditField));

        session.selected = 2; // Output directory
        session.select();
        assert_eq!(session.stage, Stage::Prompt(Prompt::OutputDir));
        // The field opens pre-filled with what it is now, so a small change is
        // a small edit rather than a retype.
        assert_eq!(session.input, "dist");

        session.input = "build".to_string();
        session.submit_prompt();
        assert_eq!(session.output_dir.as_deref(), Some("build"));
        assert_eq!(session.stage, Stage::Menu(Menu::EditField));
    }

    #[test]
    fn a_project_name_is_folded_into_something_the_provider_accepts() {
        let mut session = session("vercel");
        session.stage = Stage::Prompt(Prompt::Name);
        session.input = "My Cool App!".to_string();
        session.submit_prompt();
        assert_eq!(session.project_name, "my-cool-app");
    }

    #[test]
    fn a_blank_build_command_means_no_build_step() {
        let mut session = session("vercel");
        session.stage = Stage::Prompt(Prompt::BuildCommand);
        session.input = "   ".to_string();
        session.submit_prompt();
        assert_eq!(session.build_command, None);
    }

    // ---- environment variables --------------------------------------------

    #[test]
    fn a_variable_is_added_by_name_then_value_and_never_shown_again() {
        let mut session = session("vercel");
        session.stage = Stage::Menu(Menu::Env);
        session.selected = 0; // Add environment variable
        session.select();
        assert_eq!(session.stage, Stage::Prompt(Prompt::EnvKey));

        session.input = "API_URL".to_string();
        session.submit_prompt();
        assert_eq!(session.stage, Stage::Prompt(Prompt::EnvValue));
        assert!(Prompt::EnvValue.masked(), "a value must never be echoed");

        session.input = "https://api.example.com/secret-path".to_string();
        session.submit_prompt();

        assert_eq!(session.env.len(), 1);
        assert_eq!(session.env[0].key, "API_URL");

        // The menu lists the name and offers removal -- it is never a place to
        // read a value back out of.
        let rendered = format!("{:?}", session.options());
        assert!(rendered.contains("API_URL"), "{rendered}");
        assert!(!rendered.contains("secret-path"), "a value leaked: {rendered}");
        assert!(!format!("{:?}", session.summary()).contains("secret-path"));
    }

    /// Two entries for one name would silently make the second win at the far
    /// end, which is a very confusing way to debug a wrong value.
    #[test]
    fn setting_the_same_variable_twice_replaces_it() {
        let mut session = session("vercel");
        for value in ["first", "second"] {
            session.stage = Stage::Prompt(Prompt::EnvKey);
            session.input = "TOKEN_ISH".to_string();
            session.submit_prompt();
            session.input = value.to_string();
            session.submit_prompt();
        }
        assert_eq!(session.env.len(), 1);
        assert_eq!(session.env[0].value.expose(), "second");
    }

    #[test]
    fn a_set_variable_can_be_removed_again() {
        let mut session = session("vercel");
        session.env.push(EnvVar {
            key: "GOING_AWAY".to_string(),
            value: Secret::new("x"),
        });
        session.stage = Stage::Menu(Menu::Env);
        session.selected = 1; // the variable itself
        session.select();
        assert!(session.env.is_empty());
    }

    #[test]
    fn abandoning_a_half_entered_variable_leaves_nothing_behind() {
        let mut session = session("vercel");
        session.stage = Stage::Menu(Menu::Env);
        session.selected = 0;
        session.select();
        session.input = "HALF".to_string();
        session.submit_prompt(); // now on the value screen
        session.back();
        assert!(session.env.is_empty());
        assert_eq!(session.stage, Stage::Menu(Menu::Env));
    }

    // ---- target and linking ------------------------------------------------

    #[test]
    fn production_and_preview_both_reach_the_deploy_command() {
        for (index, expected) in [(0, Target::Production), (1, Target::Preview)] {
            let mut session = session("vercel");
            session.stage = Stage::Menu(Menu::Target);
            session.selected = index;
            session.select();
            assert_eq!(session.target, expected);
        }
    }

    #[test]
    fn an_existing_project_can_be_picked_from_the_list() {
        let mut session = session("vercel");
        session.stage = Stage::Working(Step::ListingProjects);
        session.mark_running(Step::ListingProjects);
        let none = session.on_event(DeployEvent::Finished {
            step: Step::ListingProjects,
            output: ok("  my-site   2d ago\n  other      5h ago\n"),
        });
        assert!(none.is_none(), "the choice must be offered, not skipped");
        assert_eq!(session.stage, Stage::Menu(Menu::Link));
        assert_eq!(session.remote_projects.len(), 2);

        session.selected = 1; // the first existing project
        let action = session.select().expect("linking starts");
        assert_eq!(step_of(&action), Step::LinkingProject);
        assert!(matches!(session.link, LinkChoice::Existing(_)));
    }

    /// Listing projects is a convenience; failing to list them must not stop a
    /// deployment that was going to create a new one anyway.
    #[test]
    fn a_failed_project_listing_does_not_stop_the_deployment() {
        let mut session = session("vercel");
        session.stage = Stage::Working(Step::ListingProjects);
        session.mark_running(Step::ListingProjects);
        let next = session
            .on_event(DeployEvent::Finished {
                step: Step::ListingProjects,
                output: err("network unreachable"),
            })
            .expect("the flow continues");
        assert_eq!(step_of(&next), Step::CreatingProject);
    }

    // ---- failure handling --------------------------------------------------

    #[test]
    fn a_failed_build_offers_logs_a_retry_and_a_way_out() {
        let (mut session, _) = walk_to_work("vercel");
        session.stage = Stage::Working(Step::Deploying);
        session.mark_running(Step::Deploying);

        let recorded = session
            .on_event(DeployEvent::Finished {
                step: Step::Deploying,
                output: err("Error: Command \"npm run build\" exited with 1"),
            })
            .expect("a failure is recorded too");

        assert_eq!(session.stage, Stage::Menu(Menu::Failure));
        let failure = session.failure.clone().unwrap_or_default();
        assert!(failure.to_lowercase().contains("build command failed"), "{failure}");

        let labels: Vec<String> = session.options().iter().map(|o| o.label.clone()).collect();
        assert_eq!(labels, vec!["View detailed logs", "Retry deployment", "Cancel"]);

        match recorded {
            DeployAction::Record(entry) => assert_eq!(entry.status, "Failed"),
            other => panic!("expected a history record, got {other:?}"),
        }
    }

    #[test]
    fn retrying_starts_the_work_again_and_keeps_the_configuration() {
        let (mut session, _) = walk_to_work("vercel");
        session.project_name = "renamed".to_string();
        session.token = Some(Secret::new("kept"));
        session.fail("something went wrong".to_string());

        session.selected = 1; // Retry deployment
        let action = session.select().expect("work restarts");
        assert_eq!(step_of(&action), Step::CheckingCli);
        assert_eq!(session.project_name, "renamed", "configuration survives a retry");
        assert!(session.token.is_some(), "a supplied token survives a retry");
        assert!(session.failure.is_none());
    }

    #[test]
    fn viewing_detailed_logs_toggles_rather_than_leaving_the_screen() {
        let mut session = session("vercel");
        session.fail("nope".to_string());
        session.selected = 0;
        assert!(session.select().is_none());
        assert!(session.show_full_log);
        assert_eq!(session.stage, Stage::Menu(Menu::Failure), "still on the failure screen");

        session.selected = 0;
        session.select();
        assert!(!session.show_full_log);
    }

    // ---- cancellation and navigation ---------------------------------------

    #[test]
    fn escape_while_working_stops_the_deployment() {
        let (mut session, _) = walk_to_work("vercel");
        assert!(matches!(session.stage, Stage::Working(_)));
        let close = session.back();
        assert!(!close, "the outcome screen stays up rather than vanishing");
        assert_eq!(session.finished_status, Some(DeployStatus::Cancelled));
        assert_eq!(session.stage, Stage::Finished);
    }

    /// A result arriving for a step that is no longer in flight belongs to a
    /// cancelled attempt -- the same reasoning as `App`'s stale-request guard.
    #[test]
    fn a_result_from_a_cancelled_step_is_ignored() {
        let (mut session, _) = walk_to_work("vercel");
        session.back(); // cancel
        let late = session.on_event(DeployEvent::Finished {
            step: Step::CheckingCli,
            output: ok("Vercel CLI 33.5.1"),
        });
        assert!(late.is_none(), "a stale result must not restart the flow");
        assert_eq!(session.finished_status, Some(DeployStatus::Cancelled));
    }

    #[test]
    fn escape_steps_back_through_the_configuration_screens() {
        let mut session = DeploySession::new(profile(), DeployPolicy::default());
        session.select(); // provider → confirm
        assert_eq!(session.stage, Stage::Menu(Menu::Confirm));

        assert!(!session.back());
        assert_eq!(session.stage, Stage::Menu(Menu::Provider));
        // ...and once more closes the whole thing.
        assert!(session.back());
    }

    #[test]
    fn saying_no_at_the_confirmation_ends_without_running_anything() {
        let mut session = DeploySession::new(profile(), DeployPolicy::default());
        session.select(); // provider
        session.selected = 1; // No
        assert!(session.select().is_none());
        assert_eq!(session.finished_status, Some(DeployStatus::Cancelled));
    }

    #[test]
    fn the_highlight_wraps_in_both_directions() {
        let mut session = DeploySession::new(profile(), DeployPolicy::default());
        let count = session.options().len();
        session.move_selection(-1);
        assert_eq!(session.selected, count - 1);
        session.move_selection(1);
        assert_eq!(session.selected, 0);
    }

    // ---- surfaces ----------------------------------------------------------

    /// Every screen has to render, or the flow can strand someone on a blank.
    #[test]
    fn every_stage_produces_a_title_and_renderable_state() {
        let mut session = session("vercel");
        session.remote_projects = vec![RemoteProject {
            id: "1".to_string(),
            name: "existing".to_string(),
            url: None,
        }];
        let stages = [
            Stage::Menu(Menu::Provider),
            Stage::Menu(Menu::Confirm),
            Stage::Menu(Menu::Settings),
            Stage::Menu(Menu::EditField),
            Stage::Menu(Menu::Env),
            Stage::Menu(Menu::Target),
            Stage::Menu(Menu::Link),
            Stage::Menu(Menu::InstallCli),
            Stage::Menu(Menu::Login),
            Stage::Menu(Menu::Failure),
            Stage::Prompt(Prompt::Name),
            Stage::Prompt(Prompt::EnvValue),
            Stage::Working(Step::Deploying),
            Stage::Finished,
        ];
        for stage in stages {
            session.stage = stage.clone();
            assert!(!session.title().trim().is_empty(), "{stage:?} has no title");
            if let Stage::Menu(_) = stage {
                assert!(!session.options().is_empty(), "{stage:?} offers no choices");
            }
            let _ = session.summary();
        }
    }

    #[test]
    fn every_step_has_both_a_present_and_a_past_tense() {
        for step in [
            Step::CheckingCli,
            Step::InstallingCli,
            Step::CheckingAuth,
            Step::LoggingIn,
            Step::ListingProjects,
            Step::CreatingProject,
            Step::LinkingProject,
            Step::Deploying,
            Step::CheckingStatus,
            Step::SigningOut,
        ] {
            assert!(!step.verb().is_empty());
            assert!(!step.done_label().is_empty());
            // The relabel-on-completion path depends on this round trip.
            assert_eq!(current_step_of(step.verb()), Some(step));
        }
    }

    /// Everything above feeds the state machine canned output. This one wires
    /// the real pieces together -- `detect` reading a real directory, the real
    /// runner spawning a real process, `cli::parse_version` reading what it
    /// actually said -- and checks they agree at the seams.
    ///
    /// It deploys nothing: the binary it looks for genuinely does not exist, so
    /// what it exercises is the missing-CLI path, which is the one a first-time
    /// user hits. That makes it safe to run anywhere, including CI.
    #[tokio::test]
    async fn detection_the_runner_and_the_flow_agree_end_to_end() {
        use crate::deploy::runner;

        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"Smoke Test App","scripts":{"build":"vite build"},"devDependencies":{"vite":"5"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("vite.config.ts"), "export default {}").unwrap();

        // 1. Detection reads the real directory.
        let profile = super::super::detect::detect(dir.path()).expect("a deployable project");
        assert_eq!(profile.framework.label(), "Vite");
        assert_eq!(profile.name, "smoke-test-app", "the npm name is folded, not used raw");

        // 2. The flow gets to the point of needing a CLI.
        let mut session = DeploySession::new(profile, DeployPolicy::default());
        session.selected = 0; // Vercel
        session.select();
        session.selected = 0; // continue
        session.select();
        session.selected = 0; // use these settings
        session.select();
        session.selected = session.options().len() - 1; // no env vars
        session.select();
        session.selected = 1; // Preview, so nothing here even resembles a live deploy
        let action = session.select().expect("work starts");

        let DeployAction::Run { step, command, cwd } = action else {
            panic!("expected a command to run, got {action:?}");
        };
        assert_eq!(step, Step::CheckingCli);

        // 3. The real runner spawns it. `vercel` is not installed here, which
        //    is exactly the path being tested.
        let output = runner::run(&command, &cwd, None).await;

        // 4. And the flow reads that real result correctly.
        let next = session.on_event(DeployEvent::Finished { step, output });
        match session.stage {
            // The ordinary case on a machine without the CLI.
            Stage::Menu(Menu::InstallCli) => {
                assert!(next.is_none(), "nothing may be installed without a decision");
                let options = session.options();
                assert!(options[0]
                    .detail
                    .as_deref()
                    .unwrap_or_default()
                    .contains("npm install -g vercel"));
            }
            // A developer machine that happens to have it: the flow moves on to
            // authentication rather than offering to install what is present.
            Stage::Working(Step::CheckingAuth) => {
                assert!(next.is_some(), "the auth check should have been started");
            }
            other => panic!("unexpected stage after a real --version check: {other:?}"),
        }
    }

    #[test]
    fn the_log_is_capped_so_a_long_build_cannot_grow_without_bound() {
        let mut session = session("vercel");
        for i in 0..(MAX_LOG_LINES + 50) {
            session.on_event(DeployEvent::Log(format!("line {i}")));
        }
        assert_eq!(session.log.len(), MAX_LOG_LINES);
        // The tail is what is kept: the newest lines are the ones being read.
        assert!(session.log.back().unwrap().contains(&(MAX_LOG_LINES + 49).to_string()));
    }
}
