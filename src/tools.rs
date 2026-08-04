//! The one tool the model gets: run a shell command.
//!
//! A single `run_command` replaces per-operation tools. It is far more capable --
//! reading a PDF is `pdftotext`, listing an archive is `unzip -l`, searching is
//! `grep` -- at the cost of any enforceable sandbox. Inspecting a command string
//! cannot tell you what it will do (`cat $(echo ... | base64 -d)`), so the only
//! real control is the user approving each command before it runs. That approval
//! lives in `app.rs`; this module assumes a decision has already been made.
//!
//! Failures come back as results the model can read rather than Rust errors: a
//! non-zero exit is information, not a reason to abandon the turn.

use crate::config::ToolsConfig;
use crate::llm::ToolCall;
use crate::workspace::Workspace;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

pub const RUN_COMMAND: &str = "run_command";

/// One executed (or declined) tool call, on its way back to the model.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub call_id: String,
    /// One line for the transcript.
    pub display: String,
    /// What the model receives.
    pub content: String,
}

/// The shell used to run a command, per platform.
///
/// `cmd /C` on Windows and `sh -c` everywhere else. `sh` rather than the user's
/// login shell: it exists on every Unix, and a model that has been told "sh"
/// will not reach for zsh-only or fish-only syntax.
pub fn shell() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    }
}

pub fn schemas() -> Vec<Value> {
    let (shell_name, shell_flag) = shell();
    vec![json!({
        "type": "function",
        "function": {
            "name": RUN_COMMAND,
            "description": format!(
                "Run a shell command in the user's project directory via `{shell_name} {shell_flag}` \
                 and get back its exit code, stdout and stderr. Use this to inspect and change the \
                 project: read files by printing them, search with grep/findstr, run builds and tests. \
                 The user must approve every command before it runs."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command line to run, e.g. `cat src/main.rs` or `grep -rn TODO src`."
                    },
                    "purpose": {
                        "type": "string",
                        "description": "One short sentence on why you need it. Shown to the user in the approval prompt, so be specific and honest."
                    }
                },
                "required": ["command"]
            }
        }
    })]
}

/// What the model is told about its situation.
///
/// The operating system is stated outright because the single most common way
/// this tool fails is a model reaching for `ls` on Windows. `tools_available`
/// goes false once the step budget is spent.
pub fn system_prompt(workspace: &Workspace, config: &ToolsConfig, tools_available: bool) -> String {
    if !tools_available {
        return format!(
            "You are tuisample-code, a terminal coding assistant working in {}.\n\
             You have used up this turn's command budget. Answer the user now, in text, \
             from what you have already seen. Do not ask to run anything else.",
            workspace.root().display()
        );
    }

    let (shell_name, shell_flag) = shell();
    let os = std::env::consts::OS;
    let os_hint = if cfg!(windows) {
        "This is Windows: use `dir`, `type`, `findstr`, `copy`. Do NOT use ls/cat/grep."
    } else {
        "This is a Unix-like system: use `ls`, `cat`, `grep`, `find`, `sed`."
    };

    format!(
        "You are tuisample-code, a terminal coding assistant.\n\n\
         Working directory: {}\n\
         Operating system: {os} — commands run through `{shell_name} {shell_flag}`\n\n\
         Tool:\n\
         - {RUN_COMMAND}(command, purpose): run a shell command and get back its exit code, \
           stdout and stderr.\n\n\
         Rules:\n\
         - {os_hint}\n\
         - To read a file, print it. To find something, search for it. Look at the real \
           project instead of guessing what it contains.\n\
         - Commands are NON-INTERACTIVE: stdin is closed. Never run anything that waits for \
           input, opens an editor (vim, nano), or runs a server in the foreground. Such a \
           command will simply time out after {} seconds.\n\
         - The user approves every command before it runs. If one is declined, do not retry \
           it — take a different approach or answer without it.\n\
         - Anything that changes or deletes files is real and immediate. Be conservative, \
           prefer the narrowest command that does the job, and say what you are about to do.\n\
         - Answers appear in a terminal: keep them short and concrete.",
        workspace.root().display(),
        config.command_timeout_secs,
    )
}

#[derive(Deserialize)]
struct RunArgs {
    command: String,
    #[serde(default)]
    purpose: Option<String>,
}

/// The command a call wants to run, for the approval prompt. `None` if the model
/// sent something unusable, in which case there is nothing to approve.
pub fn described_command(call: &ToolCall) -> Option<(String, Option<String>)> {
    if call.function.name != RUN_COMMAND {
        return None;
    }
    let args: RunArgs = serde_json::from_str(&call.function.arguments).ok()?;
    let command = args.command.trim().to_string();
    if command.is_empty() {
        return None;
    }
    Some((command, args.purpose.filter(|p| !p.trim().is_empty())))
}

/// Any of these and a command is not judged read-only, no matter what it
/// starts with: `cat file; rm -rf /` starts with `cat` but chains into
/// something else, and a prefix check alone cannot see past that.
const CHAINING_CHARS: &[char] = &[';', '|', '&', '>', '<', '`', '\n'];

/// Program names whose ordinary, no-flags-needed use is inherently read-only
/// -- nothing here deletes, writes, or changes state, so there is nothing for
/// an approval prompt to protect against.
///
/// Deliberately short and conservative. Getting this list wrong by leaving an
/// obviously-safe command off it just costs an extra keypress; getting it
/// wrong the other way runs something destructive with no prompt at all. When
/// in doubt, leave a command off.
const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "grep", "egrep", "fgrep", "which", "whoami", "date",
    "env", "printenv", "uname", "id", "file", "stat", "du", "df", "ps", "echo",
];

/// Whether `command` is read-only and reversible enough to skip the approval
/// prompt for -- see `[tools] auto_approve_read_only` in `config.rs`.
///
/// `find` and general `git` are deliberately excluded: both have common,
/// easy-to-type destructive forms (`find . -delete`, `git reset --hard`) that
/// a prefix check cannot rule out. `git status`/`diff`/`log`/`show` are
/// allowed as literal two-word prefixes instead, since none of their flags
/// change anything on disk.
pub fn is_read_only(command: &str) -> bool {
    // `$(...)` command substitution can run anything; caught separately from
    // `CHAINING_CHARS` because a bare `$` alone (`echo $HOME`) is ordinary and
    // safe, so `$` cannot be in that blanket set.
    if command.contains(CHAINING_CHARS) || command.contains("$(") {
        return false;
    }

    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };

    if READ_ONLY_PROGRAMS.contains(&program) {
        return true;
    }
    if program == "git" {
        if let Some(sub) = words.next() {
            return matches!(sub, "status" | "diff" | "log" | "show");
        }
    }
    false
}

pub async fn execute(call: &ToolCall, workspace: &Workspace, config: &ToolsConfig) -> ToolOutcome {
    if call.function.name != RUN_COMMAND {
        return outcome(
            &call.id,
            format!("⚙ {} — unknown tool", call.function.name),
            format!(
                "Error: there is no tool named '{}'. The only tool is {RUN_COMMAND}.",
                call.function.name
            ),
        );
    }

    let Some((command, _)) = described_command(call) else {
        return outcome(
            &call.id,
            "⚙ run_command — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"command": "ls -la"}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };

    let (shell_name, shell_flag) = shell();
    let mut cmd = tokio::process::Command::new(shell_name);
    cmd.arg(shell_flag)
        .arg(&command)
        .current_dir(workspace.root())
        // Closed stdin, not inherited: the TUI owns the terminal, and a command
        // that waits for input would otherwise hang forever with no way to type
        // into it. Timing out is the better failure.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Kills the child when the timeout below drops this future.
        .kill_on_drop(true);

    let limit = Duration::from_secs(config.command_timeout_secs);
    let output = match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return outcome(
                &call.id,
                format!("$ {} — could not start", clip(&command, 50)),
                format!("Error: could not run the command: {e}"),
            )
        }
        Err(_) => {
            return outcome(
                &call.id,
                format!("$ {} — timed out", clip(&command, 50)),
                format!(
                    "Error: killed after {}s. It was probably waiting for input or would not \
                     terminate. Try a non-interactive form of the command.",
                    config.command_timeout_secs
                ),
            )
        }
    };

    // Lossy on purpose: a command may legitimately print bytes that are not
    // UTF-8, and replacement characters beat refusing to report anything.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code();

    let mut content = match code {
        Some(code) => format!("exit code: {code}\n"),
        None => "exit code: killed by a signal\n".to_string(),
    };
    let budget = config.max_output_bytes;
    if !stdout.trim().is_empty() {
        content.push_str("--- stdout ---\n");
        content.push_str(&clip(&stdout, budget));
        content.push('\n');
    }
    if !stderr.trim().is_empty() {
        content.push_str("--- stderr ---\n");
        content.push_str(&clip(&stderr, budget / 4));
        content.push('\n');
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        content.push_str("(no output)\n");
    }

    let lines = stdout.lines().count() + stderr.lines().count();
    let status = match code {
        Some(0) => format!("{lines} line{}", if lines == 1 { "" } else { "s" }),
        Some(code) => format!("exit {code}"),
        None => "killed".to_string(),
    };

    outcome(
        &call.id,
        format!("$ {} — {status}", clip(&command, 60)),
        content,
    )
}

/// The result to hand back when the user says no.
pub fn declined(call: &ToolCall) -> ToolOutcome {
    let command = described_command(call)
        .map(|(command, _)| command)
        .unwrap_or_else(|| call.function.name.clone());
    outcome(
        &call.id,
        format!("$ {} — declined", clip(&command, 60)),
        "The user declined to run this command. Do not try it again; take a different \
         approach or answer without it."
            .to_string(),
    )
}

/// The result for a call abandoned before any decision was made.
pub fn unanswered(call: &ToolCall, reason: &str) -> ToolOutcome {
    let command = described_command(call)
        .map(|(command, _)| command)
        .unwrap_or_else(|| call.function.name.clone());
    outcome(
        &call.id,
        format!("$ {} — not run", clip(&command, 60)),
        reason.to_string(),
    )
}

fn outcome(call_id: &str, display: String, content: String) -> ToolOutcome {
    ToolOutcome {
        call_id: call_id.to_string(),
        display,
        content,
    }
}

/// Truncate to `max` characters (not bytes -- this must never split a char).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n[… truncated at {max} characters]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::FunctionCall;

    fn fixture() -> (tempfile::TempDir, Workspace, ToolsConfig) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("hello.txt"), "one\ntwo\nthree\n").unwrap();
        let ws = Workspace::new(dir.path()).expect("workspace");
        (dir, ws, ToolsConfig::default())
    }

    fn call(command: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: RUN_COMMAND.to_string(),
                arguments: json!({ "command": command }).to_string(),
            },
        }
    }

    /// `dir` on Windows, `ls` elsewhere -- the tests have to be as
    /// platform-honest as the tool claims to be.
    fn list_command() -> &'static str {
        if cfg!(windows) {
            "dir"
        } else {
            "ls"
        }
    }

    fn print_command() -> &'static str {
        if cfg!(windows) {
            "type hello.txt"
        } else {
            "cat hello.txt"
        }
    }

    #[tokio::test]
    async fn a_command_runs_and_reports_its_output() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&call(print_command()), &ws, &cfg).await;

        assert!(out.content.contains("exit code: 0"), "{}", out.content);
        assert!(out.content.contains("two"), "{}", out.content);
        assert_eq!(out.call_id, "call_1");
    }

    /// The working directory is the whole point: a bare `ls` has to see the
    /// project, not wherever the app happened to be launched from.
    #[tokio::test]
    async fn commands_run_in_the_workspace_directory() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&call(list_command()), &ws, &cfg).await;
        assert!(out.content.contains("hello.txt"), "{}", out.content);
    }

    /// A non-zero exit is information for the model, not a failure of the tool.
    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code_and_stderr() {
        let (_dir, ws, cfg) = fixture();
        let command = if cfg!(windows) {
            "type nope-does-not-exist.txt"
        } else {
            "cat nope-does-not-exist.txt"
        };
        let out = execute(&call(command), &ws, &cfg).await;

        assert!(!out.content.contains("exit code: 0"), "{}", out.content);
        assert!(out.content.contains("--- stderr ---"), "{}", out.content);
        assert!(out.display.contains("exit "), "{}", out.display);
    }

    /// Without a timeout one bad command freezes the turn forever. Without
    /// closed stdin, plenty of ordinary commands are that bad command.
    #[tokio::test]
    async fn a_command_that_never_finishes_is_killed() {
        let (_dir, ws, mut cfg) = fixture();
        cfg.command_timeout_secs = 1;
        let command = if cfg!(windows) {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };

        let started = std::time::Instant::now();
        let out = execute(&call(command), &ws, &cfg).await;

        assert!(out.content.contains("killed after 1s"), "{}", out.content);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "should have been killed promptly, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_command_reading_stdin_does_not_hang_forever() {
        let (_dir, ws, mut cfg) = fixture();
        cfg.command_timeout_secs = 5;
        // With inherited stdin this blocks; with /dev/null it returns at once.
        let command = if cfg!(windows) {
            "more"
        } else {
            "cat"
        };
        let out = execute(&call(command), &ws, &cfg).await;
        assert!(out.content.contains("exit code"), "{}", out.content);
    }

    #[tokio::test]
    async fn runaway_output_is_capped() {
        let (_dir, ws, mut cfg) = fixture();
        cfg.max_output_bytes = 200;
        let command = if cfg!(windows) {
            "for /L %i in (1,1,5000) do @echo aaaaaaaaaaaaaaaaaaaa"
        } else {
            "for i in $(seq 1 5000); do echo aaaaaaaaaaaaaaaaaaaa; done"
        };
        let out = execute(&call(command), &ws, &cfg).await;
        assert!(out.content.contains("truncated at 200"), "{}", out.content);
    }

    #[tokio::test]
    async fn unusable_arguments_are_explained_rather_than_run() {
        let (_dir, ws, cfg) = fixture();
        let mut bad = call("");
        bad.function.arguments = "{not json".to_string();
        let out = execute(&bad, &ws, &cfg).await;
        assert!(out.content.contains("could not read the arguments"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_empty_command_is_not_run() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&call("   "), &ws, &cfg).await;
        assert!(out.content.starts_with("Error:"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_tool_the_model_invented_is_reported_not_executed() {
        let (_dir, ws, cfg) = fixture();
        let mut made_up = call("ls");
        made_up.function.name = "delete_everything".to_string();
        let out = execute(&made_up, &ws, &cfg).await;
        assert!(out.content.contains("no tool named"), "{}", out.content);
    }

    #[test]
    fn the_command_and_purpose_are_extracted_for_the_approval_prompt() {
        let mut c = call("rm -rf build");
        c.function.arguments = json!({
            "command": "rm -rf build",
            "purpose": "clear stale build output"
        })
        .to_string();

        let (command, purpose) = described_command(&c).expect("should describe");
        assert_eq!(command, "rm -rf build");
        assert_eq!(purpose.as_deref(), Some("clear stale build output"));
    }

    #[test]
    fn plain_read_only_commands_are_recognised() {
        for cmd in ["ls -la", "cat src/main.rs", "grep -rn TODO src", "pwd", "wc -l file.txt"] {
            assert!(is_read_only(cmd), "expected read-only: {cmd}");
        }
    }

    #[test]
    fn narrow_git_subcommands_are_recognised() {
        for cmd in ["git status", "git diff HEAD~1", "git log --oneline -5", "git show HEAD"] {
            assert!(is_read_only(cmd), "expected read-only: {cmd}");
        }
    }

    #[test]
    fn destructive_or_unlisted_commands_are_not_read_only() {
        for cmd in [
            "rm -rf build",
            "git push --force",
            "git reset --hard",
            "git branch -D main",
            "find . -delete",
            "curl https://example.com/install.sh",
            "sed -i s/x/y/ file.txt",
        ] {
            assert!(!is_read_only(cmd), "expected NOT read-only: {cmd}");
        }
    }

    /// A read-only-looking prefix chained into something else must not slip
    /// through -- this is the whole reason `is_read_only` isn't just a prefix
    /// check against `READ_ONLY_PROGRAMS`.
    #[test]
    fn chaining_into_another_command_defeats_the_read_only_prefix() {
        for cmd in [
            "cat file; rm -rf /",
            "ls | xargs rm",
            "echo hi > important_file",
            "cat $(rm -rf /)",
            "cat file `rm -rf /`",
            "ls && rm -rf /",
        ] {
            assert!(!is_read_only(cmd), "expected NOT read-only: {cmd}");
        }
    }

    #[test]
    fn declining_tells_the_model_not_to_retry() {
        let out = declined(&call("rm -rf /"));
        assert!(out.content.contains("declined"), "{}", out.content);
        assert!(out.content.contains("Do not try it again"), "{}", out.content);
    }

    /// The most common way this tool fails in the wild is a model using `ls` on
    /// Windows, so the prompt has to name the platform outright.
    #[test]
    fn the_system_prompt_names_the_platform_and_the_shell() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, true);

        assert!(prompt.contains(std::env::consts::OS), "{prompt}");
        assert!(prompt.contains(shell().0), "{prompt}");
        assert!(prompt.contains("NON-INTERACTIVE"), "{prompt}");
        if cfg!(windows) {
            assert!(prompt.contains("Do NOT use ls/cat/grep"), "{prompt}");
        } else {
            assert!(prompt.contains("Unix-like"), "{prompt}");
        }

        let exhausted = system_prompt(&ws, &cfg, false);
        assert!(exhausted.contains("Answer the user now"), "{exhausted}");
    }

    #[test]
    fn the_schema_names_exactly_the_tool_that_executes() {
        let schemas = schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["function"]["name"], RUN_COMMAND);
    }

    #[test]
    fn clipping_never_splits_a_multibyte_character() {
        let clipped = clip("héllo wörld→", 4);
        assert!(clipped.starts_with("héll"), "{clipped}");
        assert!(clipped.contains("truncated"), "{clipped}");
    }
}
