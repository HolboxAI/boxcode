//! Netlify, driven through the official `netlify` CLI.
//!
//! The same shape as `vercel.rs` -- described commands, parsed output, no
//! bespoke REST client -- with the differences that matter in practice:
//!
//! - **Netlify builds locally.** `netlify deploy --build` runs the build on
//!   this machine and uploads the result, where Vercel uploads the source and
//!   builds remotely. So configured environment variables genuinely reach the
//!   build here, and the output directory genuinely matters.
//! - **Sites have ids.** `--site` takes an opaque id, not a name, so linking to
//!   an existing site needs the id from `sites:list` rather than the name the
//!   user recognises. Both are kept on `RemoteProject` for that reason.
//! - **`--json` is available and used.** Netlify's JSON output is stable enough
//!   to parse properly, so URLs and site listings do not rely on scraping
//!   human-facing text. The text path is still there as a fallback, because
//!   `--json` is quietly ignored by some subcommands.

use super::vercel::{first_url, is_deployment_url};
use super::{
    AuthState, CommandOutput, DeployPlan, DeploymentProvider, ProviderCommand, RemoteProject, Secret,
};
use serde_json::Value;

/// `--build` means the build happens here, on this machine, before the upload.
/// That is slower than Vercel's remote build and just as capable of taking
/// several minutes.
const DEPLOY_TIMEOUT_SECS: u64 = 1_800;
const LOGIN_TIMEOUT_SECS: u64 = 300;

pub struct NetlifyProvider;

impl NetlifyProvider {
    fn token_env(token: Option<&Secret>) -> Vec<(String, Secret)> {
        match token {
            Some(token) if !token.is_empty() => {
                vec![("NETLIFY_AUTH_TOKEN".to_string(), token.clone())]
            }
            _ => Vec::new(),
        }
    }

    fn build_env(plan: &DeployPlan) -> Vec<(String, Secret)> {
        plan.env
            .iter()
            .map(|var| (var.key.clone(), var.value.clone()))
            .collect()
    }
}

impl DeploymentProvider for NetlifyProvider {
    fn id(&self) -> &'static str {
        "netlify"
    }

    fn label(&self) -> &'static str {
        "Netlify"
    }

    fn cli_binary(&self) -> &'static str {
        "netlify"
    }

    fn docs_url(&self) -> &'static str {
        "https://docs.netlify.com/cli/get-started/"
    }

    fn token_env_var(&self) -> &'static str {
        "NETLIFY_AUTH_TOKEN"
    }

    fn version_command(&self) -> ProviderCommand {
        ProviderCommand::new("netlify", &["--version"])
    }

    fn install_command(&self) -> ProviderCommand {
        ProviderCommand::new("npm", &["install", "-g", "netlify-cli"]).timeout(600)
    }

    fn is_authenticated(&self, token: Option<&Secret>) -> ProviderCommand {
        ProviderCommand::new("netlify", &["status"]).with_env(Self::token_env(token))
    }

    fn authenticate(&self) -> ProviderCommand {
        ProviderCommand::new("netlify", &["login"])
            .interactive()
            .timeout(LOGIN_TIMEOUT_SECS)
    }

    fn logout(&self) -> ProviderCommand {
        ProviderCommand::new("netlify", &["logout"])
    }

    fn get_projects(&self, token: Option<&Secret>) -> ProviderCommand {
        ProviderCommand::new("netlify", &["sites:list", "--json"])
            .with_env(Self::token_env(token))
            .timeout(60)
    }

    fn create_project(&self, plan: &DeployPlan) -> Option<ProviderCommand> {
        // Unlike Vercel, Netlify will not conjure a site as part of deploying:
        // without a linked site `netlify deploy` asks, and stdin is closed.
        Some(
            ProviderCommand::new("netlify", &["sites:create"])
                .arg("--name")
                .arg(plan.project_name.clone())
                .with_env(Self::token_env(plan.token.as_ref()))
                .timeout(120),
        )
    }

    fn link_project(&self, plan: &DeployPlan) -> Option<ProviderCommand> {
        // Only meaningful for a site that already exists; a new one is linked
        // by `sites:create` above as a side effect of creating it.
        let existing = plan.existing()?;
        Some(
            ProviderCommand::new("netlify", &["link"])
                .arg("--id")
                .arg(existing.id.clone())
                .with_env(Self::token_env(plan.token.as_ref()))
                .timeout(120),
        )
    }

    fn deploy(&self, plan: &DeployPlan) -> ProviderCommand {
        let mut env = Self::token_env(plan.token.as_ref());
        env.extend(Self::build_env(plan));

        // `--dir` is omitted for the frameworks Netlify's own build plugins
        // handle (Next.js and friends): passing a directory there overrides a
        // correct answer with a guess. See `Framework::output_is_provider_managed`.
        let output = plan
            .output_dir
            .as_deref()
            .filter(|_| !plan.framework.output_is_provider_managed());

        ProviderCommand::new("netlify", &["deploy", "--build"])
            .flag_if(plan.target.is_production(), "--prod")
            .opt("--dir", output)
            .opt("--site", plan.existing().map(|p| p.id.as_str()))
            .arg("--json")
            .with_env(env)
            .timeout(DEPLOY_TIMEOUT_SECS)
    }

    fn get_deployment_status(
        &self,
        _deployment: &str,
        token: Option<&Secret>,
    ) -> Option<ProviderCommand> {
        Some(
            ProviderCommand::new("netlify", &["status", "--json"])
                .with_env(Self::token_env(token)),
        )
    }

    fn parse_auth(&self, out: &CommandOutput) -> AuthState {
        if out.not_found {
            return AuthState::Unknown("the Netlify CLI is not installed".to_string());
        }
        let combined = out.combined();
        let lower = combined.to_lowercase();

        // Signed-in is tested first, and -- crucially -- **without requiring a
        // zero exit code**. `netlify status` exits 1 in a directory that is not
        // yet linked to a site:
        //
        //     Current Netlify User
        //     Email: someone@example.com          <- signed in, plainly
        //     Error: You don't appear to be in a folder that is linked to a
        //     project
        //     $? = 1
        //
        // That is a complaint about the *directory*, not the session, and the
        // whole point of this step is to find out whether we have to log in.
        // Gating on `success()` meant a signed-in user was sent to the login
        // screen, logged in, was asked again, and looped forever without ever
        // deploying. Linking is the next step's job, not this one's.
        for prefix in ["Email:", "Name:"] {
            if let Some(value) = combined
                .lines()
                .find_map(|line| line.trim().strip_prefix(prefix))
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                return AuthState::In(value.to_string());
            }
        }
        // ...and `--json` an object carrying the same thing.
        if let Some(json) = extract_json(&out.stdout) {
            let account = json.get("account").or(Some(&json));
            for key in ["email", "name", "slug"] {
                if let Some(value) = account
                    .and_then(|a| a.get(key))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    return AuthState::In(value.to_string());
                }
            }
        }
        if lower.contains("current netlify user") || lower.contains("logged in as") {
            return AuthState::In("this account".to_string());
        }

        // Only now, and only on phrases that cannot appear in a signed-in
        // status block.
        if lower.contains("not logged in")
            || lower.contains("you are not logged in")
            || lower.contains("no netlify user")
            || lower.contains("please log in")
        {
            return AuthState::Out;
        }

        AuthState::Unknown(
            out.last_line()
                .unwrap_or_else(|| "the CLI gave no answer".to_string()),
        )
    }

    fn parse_projects(&self, out: &CommandOutput) -> Vec<RemoteProject> {
        // `sites:list --json` is an array of site objects. Anything else --
        // an older CLI ignoring `--json`, a warning printed before the array --
        // falls through to the text reader below rather than failing.
        if let Some(sites) = extract_json(&out.stdout).and_then(|v| v.as_array().cloned()) {
            let parsed: Vec<RemoteProject> = sites
                .iter()
                .filter_map(|site| {
                    let name = site.get("name").and_then(Value::as_str)?;
                    Some(RemoteProject {
                        id: site
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or(name)
                            .to_string(),
                        name: name.to_string(),
                        url: site
                            .get("ssl_url")
                            .or_else(|| site.get("url"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect();
            if !parsed.is_empty() {
                return parsed;
            }
        }

        // Text fallback: one site per line, name first.
        out.stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('─'))
            .filter_map(|line| line.split_whitespace().next())
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
        // The JSON answer names which URL is which, so no guessing is needed.
        if let Some(json) = extract_json(&out.stdout) {
            for key in ["url", "ssl_url", "deploy_ssl_url", "deploy_url", "logs"] {
                if key == "logs" {
                    break; // a log URL is not where the site is served
                }
                if let Some(url) = json.get(key).and_then(Value::as_str) {
                    if url.starts_with("https://") {
                        return Some(url.to_string());
                    }
                }
            }
        }

        let text = out.combined();
        for label in ["website url", "live url", "unique deploy url", "website draft url"] {
            if let Some(url) = text
                .lines()
                .filter(|line| line.to_lowercase().contains(label))
                .find_map(|line| first_url(line).filter(|url| is_deployment_url(url)))
            {
                return Some(url);
            }
        }
        // Filtered for the same reason as Vercel's: the CLI prints admin and
        // documentation links alongside the site, and some of them come last.
        text.lines()
            .rev()
            .find_map(|line| first_url(line).filter(|url| is_deployment_url(url)))
    }

    fn explain_failure(&self, out: &CommandOutput) -> String {
        if out.not_found {
            return "The Netlify CLI is not installed or not on PATH.".to_string();
        }
        if out.timed_out {
            return "The deployment took longer than the timeout and was stopped.".to_string();
        }
        if let Some(error) = out.spawn_error.as_ref() {
            return format!("The Netlify CLI could not be started: {error}");
        }

        let lower = out.combined().to_lowercase();
        if lower.contains("not logged in") || lower.contains("unauthorized") {
            return "Netlify rejected the credentials. Run /deploy again and choose to log in."
                .to_string();
        }
        if lower.contains("site not found") || lower.contains("no site id") {
            return "Netlify could not find that site. It may have been deleted, or belong to \
                    another account — run /deploy again and pick from the current site list."
                .to_string();
        }
        if lower.contains("name already exists") || lower.contains("subdomain") {
            return "That site name is already taken on Netlify — every name shares one global \
                    namespace. Choose a different one in the configuration step."
                .to_string();
        }
        if lower.contains("build failed") || lower.contains("build.command failed") {
            return "The build failed before anything was uploaded. The build log is above; run \
                    the same build locally to reproduce it."
                .to_string();
        }
        if lower.contains("deploy directory") && lower.contains("does not exist") {
            return "The output directory does not exist after the build. Check the build command \
                    and the publish directory in the configuration step."
                .to_string();
        }

        out.last_line()
            .filter(|line| !line.is_empty())
            .map(|line| format!("Netlify reported: {line}"))
            .unwrap_or_else(|| {
                format!(
                    "The Netlify CLI exited with {}.",
                    out.code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "no status".to_string())
                )
            })
    }
}

/// The first JSON value in `text`, ignoring anything printed around it.
///
/// The CLI prepends warnings and update notices to `--json` output often
/// enough that parsing the whole capture fails on a perfectly good answer.
fn extract_json(text: &str) -> Option<Value> {
    let start = text.find(['{', '['])?;
    let candidate = &text[start..];
    // Longest-prefix parse: `serde_json`'s streaming deserializer stops at the
    // end of the first complete value and reports where, which is exactly what
    // is needed when trailing text follows.
    let mut stream = serde_json::Deserializer::from_str(candidate).into_iter::<Value>();
    stream.next()?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::detect::Framework;
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
    fn deploying_builds_locally_and_asks_for_production_only_when_wanted() {
        let mut plan = plan();
        plan.target = Target::Production;
        let args = NetlifyProvider.deploy(&plan).args;
        assert!(args.contains(&"--build".to_string()), "{args:?}");
        assert!(args.contains(&"--prod".to_string()), "{args:?}");

        plan.target = Target::Preview;
        assert!(!NetlifyProvider.deploy(&plan).args.contains(&"--prod".to_string()));
    }

    #[test]
    fn the_output_directory_is_passed_for_frameworks_netlify_does_not_infer() {
        let mut plan = plan();
        plan.framework = Framework::Vite;
        plan.output_dir = Some("dist".to_string());
        let args = NetlifyProvider.deploy(&plan).args;
        assert!(args.windows(2).any(|w| w[0] == "--dir" && w[1] == "dist"), "{args:?}");
    }

    /// Overriding a correct answer with a guess is worse than saying nothing:
    /// Netlify's own Next.js plugin knows where the output goes.
    #[test]
    fn the_output_directory_is_withheld_when_the_provider_infers_it() {
        let mut plan = plan();
        plan.framework = Framework::NextJs;
        plan.output_dir = Some("dist".to_string());
        assert!(!NetlifyProvider.deploy(&plan).args.contains(&"--dir".to_string()));
    }

    #[test]
    fn an_existing_site_is_deployed_by_id_not_by_name() {
        let mut plan = plan();
        plan.link = LinkChoice::Existing(RemoteProject {
            id: "1a2b3c-site-id".to_string(),
            name: "friendly-name".to_string(),
            url: None,
        });
        let args = NetlifyProvider.deploy(&plan).args;
        assert!(
            args.windows(2).any(|w| w[0] == "--site" && w[1] == "1a2b3c-site-id"),
            "{args:?}"
        );
    }

    /// Netlify will not conjure a site while deploying the way Vercel does,
    /// and stdin is closed, so a new site has to be created explicitly first.
    #[test]
    fn a_new_site_is_created_explicitly_before_deploying() {
        let mut plan = plan();
        plan.link = LinkChoice::New;
        plan.project_name = "fresh-site".to_string();
        let create = NetlifyProvider.create_project(&plan).expect("must create");
        assert!(create.args.contains(&"sites:create".to_string()), "{:?}", create.args);
        assert!(create.args.contains(&"fresh-site".to_string()), "{:?}", create.args);
        // ...and there is nothing to link, since creating it links it.
        assert!(NetlifyProvider.link_project(&plan).is_none());
    }

    #[test]
    fn a_token_travels_in_the_environment_and_nowhere_else() {
        let mut plan = plan();
        plan.token = Some(Secret::new("nfp_secret_value"));
        let deploy = NetlifyProvider.deploy(&plan);
        assert!(deploy.env.iter().any(|(k, _)| k == "NETLIFY_AUTH_TOKEN"));
        assert!(!deploy.display().contains("nfp_secret_value"));
    }

    #[test]
    fn configured_environment_variables_reach_the_local_build() {
        let mut plan = plan();
        plan.env = vec![EnvVar {
            key: "VITE_API".to_string(),
            value: Secret::new("https://api.example.com"),
        }];
        let deploy = NetlifyProvider.deploy(&plan);
        assert!(deploy.env.iter().any(|(k, _)| k == "VITE_API"));
        assert!(!deploy.display().contains("api.example.com"));
    }

    // ---- auth ------------------------------------------------------------

    #[test]
    fn a_status_block_naming_an_account_reads_as_signed_in() {
        let status = "Current Netlify User\n  Name: Ada Lovelace\n  Email: ada@example.com\n";
        assert_eq!(
            NetlifyProvider.parse_auth(&output(0, status, "")),
            AuthState::In("ada@example.com".to_string())
        );
    }

    /// Regression, captured verbatim from the machine this reproduced on:
    /// `netlify status` exits **1** in a directory that is not linked to a
    /// site, while plainly reporting who is signed in. Requiring a zero exit
    /// here sent a signed-in user to the login screen, logged them in, asked
    /// again, and looped forever without ever deploying.
    #[test]
    fn a_signed_in_user_in_an_unlinked_directory_is_signed_in() {
        let real = CommandOutput {
            code: Some(1),
            stdout: "──────────────────────┐\n \
                     Current Netlify User │\n\
                     ──────────────────────┘\n\
                     Email: shivam@holbox.ai\n\
                     Teams: \n  - shivam-19buluq's team\n"
                .to_string(),
            stderr: " ›   Warning: Did you run `netlify link` yet?\n \
                      ›   Error: You don't appear to be in a folder that is linked to a project\n"
                .to_string(),
            ..Default::default()
        };
        assert_eq!(
            NetlifyProvider.parse_auth(&real),
            AuthState::In("shivam@holbox.ai".to_string()),
            "an unlinked directory is a complaint about the directory, not the session"
        );
    }

    /// Regression, from the same session: `netlify status` mentions
    /// `netlify login` in its own hint text *while signed in*, and a
    /// signed-out rule matching that phrase fired on a perfectly good session.
    #[test]
    fn a_signed_in_status_that_mentions_the_login_command_is_still_signed_in() {
        let status = "──────────────────────┐\n\
                       Current Netlify User │\n\
                      ──────────────────────┘\n\
                      Name: Shivam Pandey\n\
                      Email: shivam@example.com\n\
                      Teams: holboxai\n\
                      \n\
                      To log out, run netlify logout. To switch accounts, run netlify login.\n";
        assert_eq!(
            NetlifyProvider.parse_auth(&output(0, status, "")),
            AuthState::In("shivam@example.com".to_string())
        );
    }

    /// The same property stated directly: no signed-in output may ever be read
    /// as signed out, whatever hint text the CLI decides to append.
    #[test]
    fn a_successful_status_naming_an_account_is_never_read_as_signed_out() {
        for extra in [
            "Run `netlify login` to switch accounts.",
            "netlify login",
            "please log in again soon",
        ] {
            let status = format!("Current Netlify User\n  Email: ada@example.com\n{extra}\n");
            assert!(
                matches!(NetlifyProvider.parse_auth(&output(0, &status, "")), AuthState::In(_)),
                "read as signed out because of: {extra}"
            );
        }
    }

    #[test]
    fn a_json_status_is_read_for_the_account_too() {
        let json = r#"{"account":{"email":"ada@example.com","name":"Ada"}}"#;
        assert_eq!(
            NetlifyProvider.parse_auth(&output(0, json, "")),
            AuthState::In("ada@example.com".to_string())
        );
    }

    #[test]
    fn the_shapes_netlify_uses_for_signed_out_are_recognised() {
        for text in [
            "You are not logged in. Run `netlify login` to log in.",
            "Not logged in. Please log in first.",
        ] {
            assert_eq!(NetlifyProvider.parse_auth(&output(1, "", text)), AuthState::Out, "{text}");
        }
    }

    #[test]
    fn an_unrecognised_answer_is_unknown_rather_than_signed_out() {
        assert!(matches!(
            NetlifyProvider.parse_auth(&output(1, "", "ETIMEDOUT api.netlify.com")),
            AuthState::Unknown(_)
        ));
    }

    // ---- projects --------------------------------------------------------

    #[test]
    fn the_json_site_listing_is_parsed_with_ids_and_urls() {
        let json = r#"[
            {"id":"site-1","name":"my-site","ssl_url":"https://my-site.netlify.app"},
            {"id":"site-2","name":"other","url":"http://other.netlify.app"}
        ]"#;
        let sites = NetlifyProvider.parse_projects(&output(0, json, ""));
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].id, "site-1");
        assert_eq!(sites[0].name, "my-site");
        assert_eq!(sites[0].url.as_deref(), Some("https://my-site.netlify.app"));
    }

    /// The CLI prints update notices before its JSON often enough that
    /// parsing the whole capture would fail on a perfectly good answer.
    #[test]
    fn json_is_found_even_behind_a_warning_banner() {
        let noisy = "⚠ A new version of netlify-cli is available\n[{\"id\":\"s1\",\"name\":\"only-site\"}]\n";
        let sites = NetlifyProvider.parse_projects(&output(0, noisy, ""));
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].name, "only-site");
    }

    #[test]
    fn a_text_listing_still_works_when_json_is_ignored() {
        let text = "my-first-site\nmy-second-site\n";
        let sites = NetlifyProvider.parse_projects(&output(0, text, ""));
        let names: Vec<&str> = sites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["my-first-site", "my-second-site"]);
    }

    #[test]
    fn an_empty_listing_is_an_empty_list() {
        assert!(NetlifyProvider.parse_projects(&output(0, "[]", "")).is_empty());
    }

    // ---- urls ------------------------------------------------------------

    #[test]
    fn the_live_url_is_read_from_the_json_answer() {
        let json = r#"{"site_id":"abc","url":"https://my-project.netlify.app","logs":"https://app.netlify.com/sites/x/deploys/y"}"#;
        assert_eq!(
            NetlifyProvider.get_deployment_url(&output(0, json, "")),
            Some("https://my-project.netlify.app".to_string())
        );
    }

    /// A log URL is not where the site is served, and handing someone the
    /// wrong one of the two is worse than handing them neither.
    #[test]
    fn a_log_url_is_never_mistaken_for_the_site() {
        let json = r#"{"logs":"https://app.netlify.com/sites/x/deploys/y"}"#;
        let url = NetlifyProvider.get_deployment_url(&output(0, json, ""));
        assert_ne!(url.as_deref(), Some("https://app.netlify.com/sites/x/deploys/y"));
    }

    #[test]
    fn a_labelled_text_url_is_found_when_there_is_no_json() {
        let text = "Website URL: https://my-project.netlify.app\n";
        assert_eq!(
            NetlifyProvider.get_deployment_url(&output(0, text, "")),
            Some("https://my-project.netlify.app".to_string())
        );
    }

    // ---- failures --------------------------------------------------------

    #[test]
    fn each_kind_of_failure_gets_its_own_explanation() {
        for (out, expected) in [
            (output(1, "", "Error: Build failed with exit code 1"), "build failed"),
            (output(1, "", "Error: Site not found"), "could not find that site"),
            (
                output(1, "", "Error: A site with the name already exists"),
                "already taken",
            ),
            (
                output(1, "", "Error: Deploy directory 'dist' does not exist"),
                "output directory does not exist",
            ),
            (output(1, "", "Error: Unauthorized"), "rejected the credentials"),
        ] {
            let explained = NetlifyProvider.explain_failure(&out).to_lowercase();
            assert!(explained.contains(expected), "{explained:?} should mention {expected:?}");
        }
    }

    #[test]
    fn an_unrecognised_failure_quotes_the_cli() {
        let explained = NetlifyProvider.explain_failure(&output(1, "", "Error: ENOSPC"));
        assert!(explained.contains("ENOSPC"), "{explained}");
    }

    #[test]
    fn json_extraction_ignores_trailing_output() {
        let value = extract_json("{\"a\":1}\nsome trailing log line\n").expect("parses");
        assert_eq!(value.get("a").and_then(Value::as_i64), Some(1));
        assert!(extract_json("no json here").is_none());
    }
}
