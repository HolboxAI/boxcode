//! Running a provider's command, and getting its output back a line at a time.
//!
//! The one place a deployment subprocess is actually spawned. Providers only
//! describe commands (see the module doc on `deploy::DeploymentProvider`), so
//! timeouts, cancellation, output caps and redaction live here once instead of
//! being re-implemented, and forgotten, per provider.
//!
//! Two modes, and the difference matters:
//!
//! - [`run`] pipes stdout and stderr, emits every line to the UI as it arrives
//!   and closes stdin. This is everything except logging in.
//! - [`run_interactive`] inherits the real terminal. A browser login prints a
//!   URL and waits; with a closed stdin the vendor CLIs cannot run their own
//!   prompt at all. The caller **must** have torn the TUI down first -- see
//!   `main.rs`, which leaves the alternate screen, runs this, and restores.
//!
//! Every line is passed through [`crate::deploy::redact`] before it leaves this
//! module, because a CLI can print a token this app never held and therefore
//! could not know to mask.

use super::{redact, DeployEvent, ProviderCommand};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc::Sender;

/// Ceiling on what is kept from one stream, so a build that prints a megabyte
/// of webpack output cannot grow the process without bound. The tail is what
/// gets dropped; the head is where a failure's cause usually is.
const MAX_CAPTURED_BYTES: usize = 256 * 1024;

/// What a finished command did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// The binary is not on `PATH`. Distinct from a non-zero exit, because the
    /// remedy is completely different: install it, rather than read the error.
    pub not_found: bool,
    /// Set when the runner could not even start the process, for reasons other
    /// than the binary being absent.
    pub spawn_error: Option<String>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.not_found && self.spawn_error.is_none()
    }

    /// Both streams together, which is how these CLIs are meant to be read --
    /// they put progress on stderr and results on stdout, and a failure's
    /// cause can land on either.
    pub fn combined(&self) -> String {
        let mut out = String::with_capacity(self.stdout.len() + self.stderr.len() + 1);
        out.push_str(&self.stdout);
        if !self.stdout.is_empty() && !self.stderr.is_empty() {
            out.push('\n');
        }
        out.push_str(&self.stderr);
        out
    }

    /// The last non-empty line of either stream, which is where both CLIs put
    /// the thing they most want read -- a URL on success, a reason on failure.
    pub fn last_line(&self) -> Option<String> {
        self.combined()
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    }
}

/// Whether commands go through a shell.
///
/// On Windows they must: `vercel`, `netlify` and `npm` are all installed as
/// `.cmd` shims, which `CreateProcess` will not execute directly, so
/// `Command::new("vercel")` fails with "not found" on a machine that plainly
/// has it. On Unix they are real executables and are run directly, which keeps
/// arguments out of a shell's hands entirely.
fn spawn_command(command: &ProviderCommand, cwd: &Path) -> tokio::process::Command {
    let mut cmd = if cfg!(windows) {
        let mut shell = tokio::process::Command::new("cmd");
        shell.arg("/C").arg(&command.program);
        for arg in &command.args {
            shell.arg(arg);
        }
        shell
    } else {
        let mut direct = tokio::process::Command::new(&command.program);
        direct.args(&command.args);
        direct
    };

    cmd.current_dir(cwd);
    // Environment, never argv: argv is world-readable through `ps`.
    for (key, value) in &command.env {
        cmd.env(key, value.expose());
    }
    // Both CLIs colour their output and draw spinners when they think they are
    // on a terminal. Piped into a Ratatui panel, those escape sequences render
    // as garbage, so say plainly that this is not a terminal.
    cmd.env("NO_COLOR", "1");
    cmd.env("FORCE_COLOR", "0");
    cmd.env("CI", "1");
    cmd
}

/// Run `command`, streaming each output line to `sink` as it arrives.
///
/// Never returns an error: a failure to spawn, a timeout and a non-zero exit
/// are all information the flow has to show the user and act on, not reasons to
/// abandon the deployment with a Rust error nobody can read. That mirrors how
/// `tools::execute` reports command failures back to the model.
pub async fn run(
    command: &ProviderCommand,
    cwd: &Path,
    sink: Option<&Sender<DeployEvent>>,
) -> CommandOutput {
    let mut child = match spawn_command(command, cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Kills the child if this future is dropped -- which is exactly what
        // cancelling a deployment does.
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CommandOutput {
                not_found: true,
                stderr: format!("{} is not installed or not on PATH", command.program),
                ..Default::default()
            }
        }
        Err(e) => {
            return CommandOutput {
                spawn_error: Some(e.to_string()),
                stderr: format!("could not start {}: {e}", command.program),
                ..Default::default()
            }
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let limit = Duration::from_secs(command.timeout_secs);
    let collected = tokio::time::timeout(limit, async {
        let (out, err, status) = tokio::join!(
            pump(stdout, sink),
            pump(stderr, sink),
            child.wait(),
        );
        (out, err, status)
    })
    .await;

    match collected {
        Ok((stdout, stderr, status)) => CommandOutput {
            code: status.ok().and_then(|s| s.code()),
            stdout,
            stderr,
            ..Default::default()
        },
        Err(_) => {
            // The timed-out future has been dropped by now, so the borrow on
            // `child` is over and it can be killed explicitly. `kill_on_drop`
            // would do it too, but only whenever the value happens to drop.
            let _ = child.start_kill();
            CommandOutput {
                timed_out: true,
                stderr: format!("killed after {}s", command.timeout_secs),
                ..Default::default()
            }
        }
    }
}

/// Run `command` attached to the real terminal, for a login that needs a human.
///
/// Produces no streamed lines and captures no output: the child owns the
/// screen while it runs. The caller has to have restored the terminal out of
/// raw mode and off the alternate screen first, and has to put it back
/// afterwards.
pub async fn run_interactive(command: &ProviderCommand, cwd: &Path) -> CommandOutput {
    let status = spawn_command(command, cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await;

    match status {
        Ok(status) => CommandOutput {
            code: status.code(),
            ..Default::default()
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CommandOutput {
            not_found: true,
            stderr: format!("{} is not installed or not on PATH", command.program),
            ..Default::default()
        },
        Err(e) => CommandOutput {
            spawn_error: Some(e.to_string()),
            stderr: format!("could not start {}: {e}", command.program),
            ..Default::default()
        },
    }
}

/// Remove ANSI escape sequences.
///
/// `spawn_command` asks for plain output via `NO_COLOR`/`FORCE_COLOR`/`CI`, and
/// both CLIs honour that today. This is the belt to that pair of braces: a CLI
/// that ignores them would otherwise put raw escape bytes through the log
/// panel, and -- worse -- break the parsers, since `Email:` does not match
/// `\x1b[32mEmail:`. That failure mode is invisible until it is a login loop.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI sequences: ESC [ ... <final byte in @-~>. Anything else after
        // ESC is a two-character sequence, so one more char is consumed.
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: ends at BEL or ESC \.
                for c in chars.by_ref() {
                    if c == '\u{7}' || c == '\x1b' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Read one stream to the end, emitting redacted lines as they arrive and
/// returning the (capped) whole.
async fn pump<R>(reader: Option<R>, sink: Option<&Sender<DeployEvent>>) -> String
where
    R: AsyncRead + Unpin,
{
    let Some(reader) = reader else {
        return String::new();
    };
    let mut lines = BufReader::new(reader).lines();
    let mut collected = String::new();
    let mut truncated = false;

    while let Ok(Some(line)) = lines.next_line().await {
        let line = redact(strip_ansi(&line).trim_end());
        if line.trim().is_empty() {
            continue;
        }
        if collected.len() + line.len() < MAX_CAPTURED_BYTES {
            collected.push_str(&line);
            collected.push('\n');
        } else if !truncated {
            truncated = true;
            collected.push_str("[… output truncated]\n");
        }
        if let Some(sink) = sink {
            // A closed receiver means the app is shutting down or the
            // deployment was cancelled; stop reading rather than filling a
            // buffer nobody will drain.
            if sink.send(DeployEvent::Log(line)).await.is_err() {
                break;
            }
        }
    }
    collected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::Secret;
    use tokio::sync::mpsc;

    /// A command that exercises the runner without needing either vendor CLI:
    /// `sh -c` on Unix is always there, and these tests are the only place in
    /// this module that cares which platform it is on.
    fn shell_command(script: &str) -> ProviderCommand {
        if cfg!(windows) {
            ProviderCommand::new("cmd", &["/C", script])
        } else {
            ProviderCommand::new("sh", &["-c", script])
        }
    }

    fn drain(rx: &mut mpsc::Receiver<DeployEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let DeployEvent::Log(line) = event {
                out.push(line);
            }
        }
        out
    }

    #[tokio::test]
    async fn a_successful_command_reports_its_output_and_a_zero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&shell_command("echo hello"), dir.path(), None).await;
        assert!(out.success(), "{out:?}");
        assert_eq!(out.code, Some(0));
        assert!(out.stdout.contains("hello"), "{out:?}");
    }

    #[tokio::test]
    async fn a_failing_command_is_reported_rather_than_raised() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&shell_command("echo broken 1>&2; exit 3"), dir.path(), None).await;
        assert!(!out.success());
        assert_eq!(out.code, Some(3));
        assert!(out.combined().contains("broken"), "{out:?}");
    }

    /// The difference between "not installed" and "exited non-zero" is the
    /// whole basis of the install prompt, so it has to survive the runner.
    #[tokio::test]
    async fn a_missing_binary_is_distinguished_from_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            &ProviderCommand::new("definitely-not-a-real-binary-xyz", &["--version"]),
            dir.path(),
            None,
        )
        .await;
        assert!(out.not_found, "{out:?}");
        assert!(!out.success());
    }

    #[tokio::test]
    async fn output_is_streamed_line_by_line_while_the_command_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let out = run(
            &shell_command("echo one; echo two; echo three"),
            dir.path(),
            Some(&tx),
        )
        .await;
        assert!(out.success());

        let lines = drain(&mut rx);
        assert!(lines.iter().any(|l| l == "one"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "two"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "three"), "{lines:?}");
    }

    /// stderr is where both CLIs put their progress, so losing it would mean
    /// showing an empty panel through the whole build.
    #[tokio::test]
    async fn both_streams_are_captured_and_streamed() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        run(
            &shell_command("echo to-stdout; echo to-stderr 1>&2"),
            dir.path(),
            Some(&tx),
        )
        .await;
        let lines = drain(&mut rx);
        assert!(lines.iter().any(|l| l == "to-stdout"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "to-stderr"), "{lines:?}");
    }

    /// The property the whole secret story rests on: a token printed by a CLI
    /// this app never handed one to must still not reach the UI.
    #[tokio::test]
    async fn a_token_printed_by_the_command_is_redacted_before_it_is_streamed() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let out = run(
            &shell_command("echo 'using vercel_supersecrettoken123 now'"),
            dir.path(),
            Some(&tx),
        )
        .await;

        let lines = drain(&mut rx);
        assert!(
            !lines.iter().any(|l| l.contains("supersecrettoken")),
            "a token reached the UI: {lines:?}"
        );
        assert!(
            !out.combined().contains("supersecrettoken"),
            "a token reached the captured output: {out:?}"
        );
        assert!(lines.iter().any(|l| l.contains("••••")), "{lines:?}");
    }

    #[tokio::test]
    async fn a_command_that_will_not_finish_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&shell_command("sleep 30").timeout(1), dir.path(), None).await;
        assert!(out.timed_out, "{out:?}");
        assert!(!out.success());
        assert!(out.stderr.contains("killed after 1s"), "{out:?}");
    }

    /// stdin is closed, so a command that waits for input times out instead of
    /// hanging the app forever with no way to type into it -- the same rule
    /// `tools::execute_run_command` follows.
    #[tokio::test]
    async fn a_command_waiting_on_stdin_gets_eof_rather_than_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&shell_command("cat; echo done").timeout(5), dir.path(), None).await;
        assert!(!out.timed_out, "closed stdin should end `cat` at once: {out:?}");
        assert!(out.stdout.contains("done"), "{out:?}");
    }

    /// Secret environment values must reach the child and appear nowhere else.
    #[tokio::test]
    async fn secret_environment_reaches_the_child_without_touching_argv() {
        let dir = tempfile::tempdir().unwrap();
        let command = shell_command("echo \"len=${#MY_TOKEN}\"")
            .with_env(vec![("MY_TOKEN".to_string(), Secret::new("abcd1234"))]);

        assert!(!command.display().contains("abcd1234"));
        if cfg!(windows) {
            return; // the probe script above is shell-specific
        }
        let out = run(&command, dir.path(), None).await;
        assert!(out.stdout.contains("len=8"), "the child did not see it: {out:?}");
    }

    /// A CLI that ignores `NO_COLOR` would otherwise break every parser --
    /// `Email:` does not match `\x1b[32mEmail:` -- and that failure is
    /// invisible until it presents as a login loop.
    #[test]
    fn colour_codes_are_stripped_so_parsers_see_plain_text() {
        assert_eq!(strip_ansi("\x1b[32mEmail: \x1b[39mada@example.com"), "Email: ada@example.com");
        assert_eq!(strip_ansi("\x1b[1m\x1b[31mError\x1b[0m: nope"), "Error: nope");
        // Text with no escapes is returned untouched.
        assert_eq!(strip_ansi("plain line"), "plain line");
        // Box drawing and other real UTF-8 must survive.
        assert_eq!(strip_ansi("──────┐ Current Netlify User │"), "──────┐ Current Netlify User │");
    }

    #[tokio::test]
    async fn commands_run_in_the_directory_they_are_given() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let listing = if cfg!(windows) { "dir /b" } else { "ls" };
        let out = run(&shell_command(listing), dir.path(), None).await;
        assert!(out.stdout.contains("marker.txt"), "{out:?}");
    }
}
