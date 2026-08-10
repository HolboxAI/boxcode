//! Is the provider's CLI installed, and may we install it?
//!
//! Reading a `--version` answer is separated from *running* it for the same
//! reason the providers describe commands rather than running them: this is
//! the part worth testing exhaustively, and none of those tests should need
//! `vercel` or `netlify` present on the machine running them.
//!
//! Installing is never automatic. `npm install -g` writes outside the project,
//! so it goes through the same `danger` classifier every shell command does and
//! surfaces that verdict in the confirmation prompt -- the user sees the exact
//! command and why it is flagged before anything is written.

use super::runner::CommandOutput;
use crate::danger;
use std::path::Path;

/// What a `--version` check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliState {
    /// Installed, carrying the reported version for the progress line.
    Present(String),
    /// Not on `PATH` at all -- the case the install prompt exists for.
    Missing,
    /// Present but not answering properly. Distinct from `Missing` because
    /// installing it again is unlikely to help, and saying "not installed"
    /// about a binary that plainly is would send someone the wrong way.
    Broken(String),
}

/// Read a `--version` result.
pub fn parse_version(out: &CommandOutput) -> CliState {
    if out.not_found {
        return CliState::Missing;
    }
    if let Some(error) = &out.spawn_error {
        return CliState::Broken(error.clone());
    }
    if out.timed_out {
        return CliState::Broken("the CLI did not answer `--version` in time".to_string());
    }

    let combined = out.combined();
    // A shell reports a missing command by exiting 127, which is how the
    // Windows path (`cmd /C vercel --version`) surfaces "not installed" -- the
    // spawn itself succeeded there, because `cmd` exists.
    let lower = combined.to_lowercase();
    if out.code == Some(127)
        || lower.contains("command not found")
        || lower.contains("is not recognized as an internal or external command")
    {
        return CliState::Missing;
    }

    if !out.success() {
        return CliState::Broken(
            out.last_line()
                .unwrap_or_else(|| "the CLI exited with an error".to_string()),
        );
    }

    match version_of(&combined) {
        Some(version) => CliState::Present(version),
        // Exited zero but said nothing recognisable. Treat it as present: the
        // thing that matters is that it runs, and refusing to proceed over an
        // unparsed version string would be pedantry with a cost.
        None => CliState::Present("installed".to_string()),
    }
}

/// The version out of a `--version` line.
///
/// Both CLIs print something like `Vercel CLI 33.5.1` or `netlify-cli/17.10.1
/// darwin-arm64 node-v20.11.0`, so this takes the first token that looks like a
/// dotted number rather than assuming either shape.
fn version_of(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '/')
        .map(|token| token.trim_start_matches('v'))
        .find(|token| {
            let mut parts = token.split('.');
            let major = parts.next().unwrap_or("");
            !major.is_empty()
                && major.chars().all(|c| c.is_ascii_digit())
                && parts.next().is_some_and(|minor| {
                    !minor.is_empty() && minor.chars().next().is_some_and(|c| c.is_ascii_digit())
                })
        })
        .map(str::to_string)
}

/// What the guardrails make of an install command, judged against the directory
/// it would run in.
///
/// Reuses `danger::classify` rather than having an opinion of its own, so the
/// install prompt says the same thing about `npm install -g` that the ordinary
/// command-approval prompt would. `npm -g` classifies as destructive-but-
/// legitimate, which is exactly right: it is a normal thing to do and it writes
/// outside the project.
pub fn install_risk(command: &str, workspace_root: &Path) -> danger::Risk {
    danger::classify(command, workspace_root)
}

/// Whether an install command is safe enough to even offer.
///
/// A `Blocked` verdict means it never gets a prompt -- the same rule the tool
/// approval flow follows. No install command in this crate is blocked, but a
/// future provider's might be, and finding that out at the prompt is too late.
pub fn may_offer_install(command: &str, workspace_root: &Path) -> Result<Option<String>, String> {
    match install_risk(command, workspace_root) {
        danger::Risk::Blocked(reason) => Err(reason),
        danger::Risk::Dangerous(reason) => Ok(Some(reason)),
        danger::Risk::Normal => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::providers;

    fn output(code: i32, stdout: &str) -> CommandOutput {
        CommandOutput {
            code: Some(code),
            stdout: stdout.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_missing_binary_reads_as_missing() {
        let out = CommandOutput {
            not_found: true,
            ..Default::default()
        };
        assert_eq!(parse_version(&out), CliState::Missing);
    }

    /// The Windows path runs through `cmd /C`, so the spawn succeeds even when
    /// the CLI is absent and the only signal is the shell's own exit code.
    #[test]
    fn a_shell_reporting_command_not_found_also_reads_as_missing() {
        assert_eq!(parse_version(&output(127, "")), CliState::Missing);
        assert_eq!(
            parse_version(&output(1, "'vercel' is not recognized as an internal or external command")),
            CliState::Missing
        );
        assert_eq!(
            parse_version(&output(127, "sh: vercel: command not found")),
            CliState::Missing
        );
    }

    #[test]
    fn the_version_is_read_from_the_shapes_both_clis_print() {
        assert_eq!(
            parse_version(&output(0, "Vercel CLI 33.5.1\n")),
            CliState::Present("33.5.1".to_string())
        );
        assert_eq!(
            parse_version(&output(0, "netlify-cli/17.10.1 darwin-arm64 node-v20.11.0\n")),
            CliState::Present("17.10.1".to_string())
        );
        assert_eq!(
            parse_version(&output(0, "v12.0.4\n")),
            CliState::Present("12.0.4".to_string())
        );
    }

    /// Exiting zero is the thing that matters. Refusing to proceed because a
    /// version string did not parse would be pedantry with a real cost.
    #[test]
    fn an_unparseable_version_is_still_present() {
        assert_eq!(
            parse_version(&output(0, "some future format\n")),
            CliState::Present("installed".to_string())
        );
    }

    /// Installing again will not fix a CLI that is present and broken, so
    /// telling someone it is "not installed" sends them the wrong way.
    #[test]
    fn a_present_but_failing_cli_is_broken_rather_than_missing() {
        let out = CommandOutput {
            code: Some(1),
            stderr: "Error: cannot find module 'chalk'".to_string(),
            ..Default::default()
        };
        match parse_version(&out) {
            CliState::Broken(detail) => assert!(detail.contains("chalk"), "{detail}"),
            other => panic!("expected Broken, got {other:?}"),
        }

        let slow = CommandOutput {
            timed_out: true,
            ..Default::default()
        };
        assert!(matches!(parse_version(&slow), CliState::Broken(_)));
    }

    /// The install prompt has to say the same thing about `npm install -g`
    /// that the ordinary command-approval prompt would.
    #[test]
    fn installing_a_cli_is_flagged_as_writing_outside_the_project() {
        let root = Path::new("/Users/dev/project");
        for provider in providers() {
            let command = provider.install_command();
            let line = format!("{} {}", command.program, command.args.join(" "));
            match may_offer_install(&line, root) {
                Ok(Some(reason)) => assert!(
                    reason.contains("globally") || reason.contains("outside"),
                    "{}: {reason}",
                    provider.id()
                ),
                other => panic!("{} install should warn, got {other:?}", provider.id()),
            }
        }
    }

    /// No install command may be one the guardrails refuse outright -- finding
    /// that out at the prompt would be too late.
    #[test]
    fn no_providers_install_command_is_blocked_outright() {
        let root = Path::new("/Users/dev/project");
        for provider in providers() {
            let command = provider.install_command();
            let line = format!("{} {}", command.program, command.args.join(" "));
            assert!(
                may_offer_install(&line, root).is_ok(),
                "{} has an install command the guardrails block",
                provider.id()
            );
        }
    }

    #[test]
    fn a_genuinely_catastrophic_install_command_would_be_refused() {
        // No provider does this; the check exists so a future one cannot.
        assert!(may_offer_install("rm -rf / && npm i -g thing", Path::new("/Users/dev/project")).is_err());
    }
}
