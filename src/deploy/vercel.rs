//! Vercel, driven through the official `vercel` CLI.
//!
//! Every command here is one a person could type themselves, which is the
//! point: when something goes wrong, the fix is a command they already have,
//! not an API call only this app knows how to make. The progress panel shows
//! the exact command line for the same reason.
//!
//! # Authentication, in the order it is tried
//!
//! 1. `VERCEL_TOKEN` in the environment — used as `--token`'s value via the
//!    child's *environment*, never its argv, and never shown.
//! 2. An existing CLI session (`~/.local/share/com.vercel.cli`), found by
//!    `vercel whoami`. Someone who has ever run `vercel login` on this machine
//!    is already done, and is never asked for anything.
//! 3. `vercel login`, run against the real terminal with the TUI torn down —
//!    it prints a URL, opens a browser and polls. Nothing is typed into this
//!    app, so no secret ever passes through it.
//!
//! Only if all three are unavailable is a token pasted, into a masked field.
//!
//! # Linking
//!
//! Vercel creates the project as part of linking, so `create_project` and
//! `link_project` are the same command: `vercel link --yes`, with `--project`
//! naming an existing project or claiming a new name. `vercel deploy --yes`
//! would do this on its own, but doing it as its own step means "creating the
//! project" and "building it" fail separately and legibly.

use super::{
    AuthState, CommandOutput, DeployPlan, DeploymentProvider, ProviderCommand, RemoteProject, Secret,
};

/// A deploy is a full remote build. Vercel's own builds routinely run for
/// several minutes on a cold cache, and killing one halfway leaves a
/// half-finished deployment nobody asked for.
const DEPLOY_TIMEOUT_SECS: u64 = 1_800;
/// A browser login is a human at a keyboard, somewhere else.
const LOGIN_TIMEOUT_SECS: u64 = 300;

pub struct VercelProvider;

impl VercelProvider {
    /// `--token` reads from the environment rather than argv. Both are
    /// supported by the CLI; only one of them is invisible to `ps`.
    fn token_env(token: Option<&Secret>) -> Vec<(String, Secret)> {
        match token {
            Some(token) if !token.is_empty() => {
                vec![("VERCEL_TOKEN".to_string(), token.clone())]
            }
            _ => Vec::new(),
        }
    }

    /// Environment variables the user configured, for builds that run locally.
    /// See the note in `README.md`: Vercel builds remotely by default, so
    /// these reach the build only for a prebuilt/local build, and persistent
    /// runtime values belong in the project's own settings.
    fn build_env(plan: &DeployPlan) -> Vec<(String, Secret)> {
        plan.env
            .iter()
            .map(|var| (var.key.clone(), var.value.clone()))
            .collect()
    }
}

impl DeploymentProvider for VercelProvider {
    fn id(&self) -> &'static str {
        "vercel"
    }

    fn label(&self) -> &'static str {
        "Vercel"
    }

    fn cli_binary(&self) -> &'static str {
        "vercel"
    }

    fn docs_url(&self) -> &'static str {
        "https://vercel.com/docs/cli"
    }

    fn token_env_var(&self) -> &'static str {
        "VERCEL_TOKEN"
    }

    fn version_command(&self) -> ProviderCommand {
        ProviderCommand::new("vercel", &["--version"])
    }

    fn install_command(&self) -> ProviderCommand {
        // Global by design -- a deployment CLI is a tool, not a dependency of
        // the project being deployed. `danger::classify` rates `npm -g` as
        // destructive-but-legitimate, so this always stops for confirmation.
        ProviderCommand::new("npm", &["install", "-g", "vercel"]).timeout(600)
    }

    fn is_authenticated(&self, token: Option<&Secret>) -> ProviderCommand {
        ProviderCommand::new("vercel", &["whoami"]).with_env(Self::token_env(token))
    }

    fn authenticate(&self) -> ProviderCommand {
        ProviderCommand::new("vercel", &["login"])
            .interactive()
            .timeout(LOGIN_TIMEOUT_SECS)
    }

    fn logout(&self) -> ProviderCommand {
        ProviderCommand::new("vercel", &["logout"])
    }

    fn get_projects(&self, token: Option<&Secret>) -> ProviderCommand {
        ProviderCommand::new("vercel", &["projects", "ls"]).with_env(Self::token_env(token))
    }

    fn create_project(&self, plan: &DeployPlan) -> Option<ProviderCommand> {
        // Linking is what creates it; see the module doc.
        self.link_project(plan)
    }

    fn link_project(&self, plan: &DeployPlan) -> Option<ProviderCommand> {
        let name = plan
            .existing()
            .map(|project| project.name.clone())
            .unwrap_or_else(|| plan.project_name.clone());
        Some(
            ProviderCommand::new("vercel", &["link", "--yes"])
                .arg("--project")
                .arg(name)
                .with_env(Self::token_env(plan.token.as_ref()))
                .timeout(120),
        )
    }

    fn deploy(&self, plan: &DeployPlan) -> ProviderCommand {
        let mut env = Self::token_env(plan.token.as_ref());
        env.extend(Self::build_env(plan));

        ProviderCommand::new("vercel", &["deploy", "--yes"])
            // Production is an explicit flag; a preview is what you get
            // otherwise, which is the safer default to be wrong about.
            .flag_if(plan.target.is_production(), "--prod")
            .with_env(env)
            .timeout(DEPLOY_TIMEOUT_SECS)
    }

    fn get_deployment_status(
        &self,
        deployment: &str,
        token: Option<&Secret>,
    ) -> Option<ProviderCommand> {
        Some(
            ProviderCommand::new("vercel", &["inspect"])
                .arg(deployment)
                .with_env(Self::token_env(token)),
        )
    }

    fn parse_auth(&self, out: &CommandOutput) -> AuthState {
        if out.not_found {
            return AuthState::Unknown("the Vercel CLI is not installed".to_string());
        }
        let combined = out.combined();
        let lower = combined.to_lowercase();

        if out.success() {
            // `whoami` prints just the username or team slug. Some versions
            // prefix a `> ` progress line on stderr, hence the last real line
            // rather than the whole capture.
            if let Some(identity) = out
                .stdout
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty() && !line.starts_with('>'))
            {
                return AuthState::In(identity.to_string());
            }
        }
        if lower.contains("not authenticated")
            || lower.contains("no existing credentials")
            || lower.contains("credentials not found")
            || lower.contains("please log in")
            || lower.contains("run `vercel login`")
        {
            return AuthState::Out;
        }
        if lower.contains("token") && (lower.contains("invalid") || lower.contains("expired")) {
            return AuthState::Out;
        }
        AuthState::Unknown(
            out.last_line()
                .unwrap_or_else(|| "the CLI gave no answer".to_string()),
        )
    }

    fn parse_projects(&self, out: &CommandOutput) -> Vec<RemoteProject> {
        // `vercel projects ls` prints a header, a blank line, then one project
        // per row: name, then age/updated columns. Anything that does not look
        // like a row is skipped rather than guessed at -- an empty list is a
        // fine answer, since "create a new one" is always offered too.
        out.stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter(|line| !line.starts_with('>'))
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| {
                !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "project" | "name" | "projects" | "latest" | "no" | "updated" | "id"
                )
            })
            .filter(|name| {
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            })
            .map(|name| RemoteProject {
                id: name.to_string(),
                name: name.to_string(),
                url: None,
            })
            .collect()
    }

    fn get_deployment_url(&self, out: &CommandOutput) -> Option<String> {
        let text = out.combined();
        // Prefer an explicitly labelled URL. A deploy prints several, and only
        // some of them are the site:
        //
        //   Inspect: https://vercel.com/<team>/<project>/<id>   <- dashboard
        //   Preview: https://<project>-<hash>.vercel.app        <- the site
        //   To deploy to production ... Learn more: https://vercel.com/docs/…  <- docs
        //
        // Production first, then preview.
        for label in ["production:", "aliased to", "preview:"] {
            if let Some(url) = text
                .lines()
                .filter(|line| line.to_lowercase().contains(label))
                .find_map(|line| first_url(line).filter(|url| is_deployment_url(url)))
            {
                return Some(url);
            }
        }
        // Otherwise the last URL that could actually be a deployment. Taking
        // the last URL unfiltered is what once handed the user a link to
        // Vercel's own documentation and called it their site.
        text.lines()
            .rev()
            .find_map(|line| first_url(line).filter(|url| is_deployment_url(url)))
    }

    fn explain_failure(&self, out: &CommandOutput) -> String {
        if out.not_found {
            return "The Vercel CLI is not installed or not on PATH.".to_string();
        }
        if out.timed_out {
            return "The deployment took longer than the timeout and was stopped.".to_string();
        }
        if let Some(error) = out.spawn_error.as_ref() {
            return format!("The Vercel CLI could not be started: {error}");
        }

        let combined = out.combined();
        let lower = combined.to_lowercase();
        if lower.contains("not authorized") || lower.contains("forbidden") {
            return "Vercel refused the request: this account cannot deploy that project. Check \
                    the team or scope, or try again and pick a different project."
                .to_string();
        }
        if lower.contains("already exists") {
            return "A Vercel project with that name already exists on this account. Pick it from \
                    the existing-project list instead of creating a new one, or choose another name."
                .to_string();
        }
        if lower.contains("rate limit") || lower.contains("too many requests") {
            return "Vercel is rate limiting this account. Wait a few minutes and try again."
                .to_string();
        }
        if lower.contains("command \"") && lower.contains("exited with") {
            return "The build command failed on Vercel. The last lines of the build log are \
                    above; run the same build locally to reproduce it."
                .to_string();
        }

        // Nothing recognised: the CLI's own last line beats a summary invented
        // here, since it is what a person searching for the error will find.
        out.last_line()
            .filter(|line| !line.is_empty())
            .map(|line| format!("Vercel reported: {line}"))
            .unwrap_or_else(|| {
                format!(
                    "The Vercel CLI exited with {}.",
                    out.code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "no status".to_string())
                )
            })
    }
}

/// Hosts and paths that are never the deployed site, however they are printed.
///
/// Both CLIs sprinkle documentation, dashboard and support links through their
/// output, and several of them come *after* the real URL -- so "the last URL
/// printed" is not a safe fallback without this.
const NOT_A_SITE: &[&str] = &[
    "vercel.com/docs",
    "vercel.com/help",
    "vercel.com/support",
    "vercel.com/account",
    "vercel.com/dashboard",
    "app.netlify.com",
    "docs.netlify.com",
    "netlify.com/support",
    "github.com/",
    "npmjs.com/",
];

/// Whether a URL could plausibly be where the project is now served.
pub(super) fn is_deployment_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    if NOT_A_SITE.iter().any(|marker| lower.contains(marker)) {
        return false;
    }
    // A deployment lives on its own host. `vercel.com/...` with a path is the
    // dashboard -- the inspect link -- not the site.
    !lower.trim_start_matches("https://").starts_with("vercel.com/")
}

/// The first `https://` URL in a line, trimmed of the punctuation CLIs wrap
/// them in. Shared by both providers.
pub(super) fn first_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace())
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')', ']', '"', '\'', ';']);
    (url.len() > "https://".len()).then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::service::tests_support::plan;
    use crate::deploy::{EnvVar, LinkChoice, Target};

    fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_production_deploy_asks_for_production_and_a_preview_does_not() {
        let mut plan = plan();
        plan.target = Target::Production;
        assert!(VercelProvider.deploy(&plan).args.contains(&"--prod".to_string()));

        plan.target = Target::Preview;
        assert!(!VercelProvider.deploy(&plan).args.contains(&"--prod".to_string()));
    }

    /// `--yes` is what keeps the CLI from opening its own interactive prompt
    /// against a stdin we have deliberately closed.
    #[test]
    fn deploying_never_waits_for_a_prompt() {
        assert!(VercelProvider.deploy(&plan()).args.contains(&"--yes".to_string()));
        assert!(VercelProvider
            .link_project(&plan())
            .unwrap()
            .args
            .contains(&"--yes".to_string()));
    }

    #[test]
    fn linking_uses_the_existing_project_name_when_there_is_one() {
        let mut plan = plan();
        plan.link = LinkChoice::Existing(RemoteProject {
            id: "prj_123".to_string(),
            name: "already-there".to_string(),
            url: None,
        });
        let args = VercelProvider.link_project(&plan).unwrap().args;
        assert!(args.contains(&"already-there".to_string()), "{args:?}");

        let mut fresh = self::plan();
        fresh.link = LinkChoice::New;
        fresh.project_name = "brand-new".to_string();
        let args = VercelProvider.link_project(&fresh).unwrap().args;
        assert!(args.contains(&"brand-new".to_string()), "{args:?}");
    }

    #[test]
    fn a_token_travels_in_the_environment_and_nowhere_else() {
        let mut plan = plan();
        plan.token = Some(Secret::new("vercel_secret_value"));
        let deploy = VercelProvider.deploy(&plan);

        assert!(deploy.env.iter().any(|(k, _)| k == "VERCEL_TOKEN"));
        assert!(!deploy.display().contains("vercel_secret_value"));
        assert!(!deploy.args.iter().any(|a| a.contains("vercel_secret_value")));
    }

    #[test]
    fn configured_environment_variables_reach_the_build_process() {
        let mut plan = plan();
        plan.env = vec![EnvVar {
            key: "API_URL".to_string(),
            value: Secret::new("https://api.example.com"),
        }];
        let deploy = VercelProvider.deploy(&plan);
        assert!(deploy.env.iter().any(|(k, _)| k == "API_URL"));
        // ...and not into argv, where `ps` would show it.
        assert!(!deploy.display().contains("api.example.com"));
    }

    // ---- auth ------------------------------------------------------------

    #[test]
    fn whoami_naming_an_account_reads_as_signed_in() {
        assert_eq!(
            VercelProvider.parse_auth(&output(0, "ada-lovelace\n", "> checking\n")),
            AuthState::In("ada-lovelace".to_string())
        );
    }

    #[test]
    fn the_shapes_vercel_uses_for_signed_out_are_recognised() {
        for stderr in [
            "Error: No existing credentials found. Please run `vercel login`",
            "Error! You are not authenticated. Please log in.",
            "Error: The token is invalid",
        ] {
            assert_eq!(
                VercelProvider.parse_auth(&output(1, "", stderr)),
                AuthState::Out,
                "{stderr}"
            );
        }
    }

    /// Sending someone through a login they do not need is worse than saying
    /// plainly that we could not tell.
    #[test]
    fn an_unrecognised_answer_is_unknown_rather_than_signed_out() {
        match VercelProvider.parse_auth(&output(1, "", "socket hang up")) {
            AuthState::Unknown(detail) => assert!(detail.contains("socket"), "{detail}"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_cli_is_unknown_auth_not_signed_out() {
        let missing = CommandOutput {
            not_found: true,
            ..Default::default()
        };
        assert!(matches!(
            VercelProvider.parse_auth(&missing),
            AuthState::Unknown(_)
        ));
    }

    // ---- projects --------------------------------------------------------

    #[test]
    fn the_project_listing_is_read_and_its_header_ignored() {
        let listing = "> Projects found under ada\n\n  Project Name    Latest Deployment\n  my-site         2d ago\n  api-worker      5h ago\n";
        let projects = VercelProvider.parse_projects(&output(0, listing, ""));
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"my-site"), "{names:?}");
        assert!(names.contains(&"api-worker"), "{names:?}");
        assert!(!names.contains(&"Project"), "header leaked: {names:?}");
    }

    /// An account with no projects is an ordinary state, not a failure -- the
    /// flow always offers "create a new one" alongside the list.
    #[test]
    fn an_empty_listing_is_an_empty_list() {
        assert!(VercelProvider.parse_projects(&output(0, "", "")).is_empty());
    }

    // ---- urls ------------------------------------------------------------

    #[test]
    fn the_production_alias_wins_over_the_per_deployment_url() {
        let log = "Inspect: https://vercel.com/ada/my-site/abc123\n\
                   Preview: https://my-site-abc123-ada.vercel.app\n\
                   Production: https://my-site.vercel.app\n";
        assert_eq!(
            VercelProvider.get_deployment_url(&output(0, log, "")),
            Some("https://my-site.vercel.app".to_string())
        );
    }

    /// Regression, from a real deployment: the CLI prints a documentation
    /// link *after* the deployment URL, and "last URL wins" handed the user
    /// `vercel.com/docs/deployments/environments` as though it were their site.
    #[test]
    fn a_trailing_documentation_link_is_never_mistaken_for_the_site() {
        let log = "Vercel CLI 48.2.9\n\
                   🔍  Inspect: https://vercel.com/shivam/deploy-demo/A1b2C3 [1s]\n\
                   ✅  Preview: https://deploy-demo-ny3nejs8p-shivam.vercel.app [2s]\n\
                   📝  To deploy to production, run `vercel --prod`. Learn more: https://vercel.com/docs/deployments/environments\n";
        assert_eq!(
            VercelProvider.get_deployment_url(&output(0, log, "")),
            Some("https://deploy-demo-ny3nejs8p-shivam.vercel.app".to_string())
        );
    }

    /// The dashboard link is not the site either, and it is printed first.
    #[test]
    fn the_inspect_link_is_not_the_site() {
        let log = "🔍  Inspect: https://vercel.com/shivam/deploy-demo/A1b2C3\n";
        assert_eq!(VercelProvider.get_deployment_url(&output(0, log, "")), None);
    }

    #[test]
    fn documentation_and_dashboard_urls_are_told_apart_from_deployments() {
        for not_a_site in [
            "https://vercel.com/docs/deployments/environments",
            "https://vercel.com/help",
            "https://vercel.com/shivam/deploy-demo/A1b2C3",
            "https://app.netlify.com/sites/x/deploys/y",
            "https://github.com/vercel/vercel",
        ] {
            assert!(!is_deployment_url(not_a_site), "{not_a_site}");
        }
        for site in [
            "https://deploy-demo-abc.vercel.app",
            "https://my-project.netlify.app",
            "https://example.com",
        ] {
            assert!(is_deployment_url(site), "{site}");
        }
    }

    #[test]
    fn a_deployment_url_is_still_found_without_a_production_label() {
        let log = "Deployed to https://my-site-xyz.vercel.app\n";
        assert_eq!(
            VercelProvider.get_deployment_url(&output(0, log, "")),
            Some("https://my-site-xyz.vercel.app".to_string())
        );
    }

    #[test]
    fn urls_are_trimmed_of_the_punctuation_around_them() {
        assert_eq!(
            first_url("see (https://example.com/x)."),
            Some("https://example.com/x".to_string())
        );
        assert_eq!(first_url("no url at all"), None);
    }

    #[test]
    fn no_url_in_the_output_means_no_url_claimed() {
        assert_eq!(VercelProvider.get_deployment_url(&output(0, "done\n", "")), None);
    }

    // ---- failures --------------------------------------------------------

    #[test]
    fn each_kind_of_failure_gets_its_own_explanation() {
        let cases = [
            (
                output(1, "", "Error: Command \"npm run build\" exited with 1"),
                "build command failed",
            ),
            (
                output(1, "", "Error: A project with that name already exists"),
                "already exists",
            ),
            (output(1, "", "Error: Not authorized"), "refused the request"),
            (output(1, "", "Error: rate limit exceeded"), "rate limiting"),
        ];
        for (out, expected) in cases {
            let explained = VercelProvider.explain_failure(&out);
            assert!(
                explained.to_lowercase().contains(expected),
                "{explained:?} should mention {expected:?}"
            );
        }
    }

    #[test]
    fn a_missing_cli_and_a_timeout_are_explained_as_themselves() {
        let missing = CommandOutput { not_found: true, ..Default::default() };
        assert!(VercelProvider.explain_failure(&missing).contains("not installed"));

        let slow = CommandOutput { timed_out: true, ..Default::default() };
        assert!(VercelProvider.explain_failure(&slow).contains("longer than the timeout"));
    }

    /// An unrecognised failure keeps the CLI's own words, which is what a
    /// person searching for the error will actually find.
    #[test]
    fn an_unrecognised_failure_quotes_the_cli_rather_than_inventing_a_summary() {
        let explained = VercelProvider.explain_failure(&output(1, "", "Error: EPERM on /var/x"));
        assert!(explained.contains("EPERM on /var/x"), "{explained}");
    }
}
