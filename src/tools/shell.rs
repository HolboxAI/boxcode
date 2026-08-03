//! Running commands. This is how builds, tests, `git` and `gh` happen -- there
//! are no dedicated tools for those, just prompts that know to reach for this one.

use super::{arg_str, cap, opt_usize, ToolCtx, ToolOutcome};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Ceiling on the model-supplied timeout. A command that genuinely needs longer
/// than this should be started in the background by the command itself.
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

pub fn run_shell_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Shell command to run from the workspace root, e.g. 'cargo test --all'."
            },
            "timeout_secs": {
                "type": "integer",
                "description": "Seconds to wait before killing the command. Capped at 600."
            }
        },
        "required": ["command"]
    })
}

pub async fn run_shell(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let command = match arg_str(args, "command") {
        Ok(c) => c.trim(),
        Err(e) => return ToolOutcome::Err(e),
    };
    if command.is_empty() {
        return ToolOutcome::Err("command must not be empty".to_string());
    }

    let timeout = opt_usize(args, "timeout_secs")
        .map(|s| Duration::from_secs(s as u64))
        .unwrap_or(ctx.shell_timeout)
        .min(MAX_TIMEOUT);

    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&ctx.workspace)
        // Null stdin: anything that would prompt gets EOF and exits, instead of
        // blocking until the timeout with no indication why.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Cancelling the run drops this future, which drops the child. Without
        // this a killed agent leaves `cargo build` running.
        .kill_on_drop(true)
        .spawn();

    let child = match child {
        Ok(c) => c,
        Err(e) => return ToolOutcome::Err(format!("could not start '{command}': {e}")),
    };

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return ToolOutcome::Err(format!("'{command}' failed to run: {e}")),
        Err(_) => {
            return ToolOutcome::Err(format!(
                "'{command}' timed out after {}s and was killed.",
                timeout.as_secs()
            ))
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code();

    let mut report = String::new();
    if !stdout.trim().is_empty() {
        report.push_str(stdout.trim_end());
        report.push('\n');
    }
    if !stderr.trim().is_empty() {
        if !report.is_empty() {
            report.push_str("--- stderr ---\n");
        }
        report.push_str(stderr.trim_end());
        report.push('\n');
    }
    if report.is_empty() {
        report.push_str("(no output)\n");
    }

    match code {
        Some(0) => ToolOutcome::Ok(cap(report.trim_end())),
        // A failing build or test is information the model needs, so the output
        // is identical either way -- but it comes back as Err so the model (and
        // the transcript) cannot mistake it for success.
        Some(code) => ToolOutcome::Err(cap(&format!("exit status {code}\n{}", report.trim_end()))),
        None => ToolOutcome::Err(cap(&format!(
            "killed by a signal\n{}",
            report.trim_end()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn captures_stdout_on_success() {
        let (_dir, ctx) = ctx();
        let out = run_shell(&json!({"command": "echo hello"}), &ctx).await;
        assert!(out.is_ok(), "{}", out.text());
        assert_eq!(out.text(), "hello");
    }

    #[tokio::test]
    async fn a_non_zero_exit_comes_back_as_an_error_with_the_output() {
        let (_dir, ctx) = ctx();
        let out = run_shell(&json!({"command": "echo boom >&2; exit 3"}), &ctx).await;
        assert!(!out.is_ok());
        assert!(out.text().contains("exit status 3"), "{}", out.text());
        assert!(out.text().contains("boom"), "{}", out.text());
    }

    #[tokio::test]
    async fn runs_in_the_workspace_root() {
        let (_dir, ctx) = ctx();
        write(&ctx, "marker.txt", "");
        let out = run_shell(&json!({"command": "ls"}), &ctx).await;
        assert!(out.text().contains("marker.txt"), "{}", out.text());
    }

    #[tokio::test]
    async fn a_hanging_command_is_killed_at_the_timeout() {
        let (_dir, ctx) = ctx();
        let out = run_shell(&json!({"command": "sleep 30", "timeout_secs": 1}), &ctx).await;
        assert!(!out.is_ok());
        assert!(out.text().contains("timed out"), "{}", out.text());
    }

    /// Anything expecting input must not sit there consuming the whole timeout.
    #[tokio::test]
    async fn stdin_is_closed_so_interactive_commands_do_not_hang() {
        let (_dir, ctx) = ctx();
        let out = run_shell(&json!({"command": "cat", "timeout_secs": 5}), &ctx).await;
        assert!(out.is_ok(), "{}", out.text());
        assert!(!out.text().contains("timed out"));
    }

    #[tokio::test]
    async fn silent_commands_report_no_output_rather_than_nothing() {
        let (_dir, ctx) = ctx();
        let out = run_shell(&json!({"command": "true"}), &ctx).await;
        assert!(out.is_ok());
        assert_eq!(out.text(), "(no output)");
    }

    #[tokio::test]
    async fn an_empty_command_is_rejected() {
        let (_dir, ctx) = ctx();
        let out = run_shell(&json!({"command": "   "}), &ctx).await;
        assert!(!out.is_ok());
        assert!(out.text().contains("must not be empty"));
    }

    #[tokio::test]
    async fn the_model_cannot_ask_for_an_unbounded_timeout() {
        let (_dir, ctx) = ctx();
        // Capped at MAX_TIMEOUT; the command itself is instant, so this asserts
        // the clamp does not reject or hang, only bounds.
        let out = run_shell(&json!({"command": "echo ok", "timeout_secs": 99999}), &ctx).await;
        assert!(out.is_ok(), "{}", out.text());
    }
}
