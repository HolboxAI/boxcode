//! The model's tools: a shell escape hatch, two typed file operations, and a
//! web search.
//!
//! `run_command` is the general-purpose one -- inspecting a command string
//! cannot tell you what it will do (`cat $(echo ... | base64 -d)`), so the
//! only real control is the user approving each one before it runs. That
//! approval lives in `app.rs`; this module assumes a decision has already
//! been made.
//!
//! `read_file`/`write_file` exist alongside it because routing every file
//! operation through the shell has real costs: writing more than a line or
//! two of code means the model hand-encoding it into a heredoc or a quoted
//! `printf`, which is exactly the kind of string-escaping work models are
//! worst at, and it hides "create this file" and "run this file" behind one
//! opaque approval instead of two reviewable ones. A typed `write_file` also
//! gets a real (if narrow) safety property a shell command cannot: its path
//! is resolved and checked against the workspace root before anything
//! happens, see `resolve_in_workspace`.
//!
//! `web_search` shells out to Python's `ddgs` package rather than talking to
//! a search engine directly: DuckDuckGo's own scraping-resistant endpoints
//! only yield real results behind TLS-fingerprint tricks that `ddgs`
//! actively maintains and this project deliberately does not reimplement.
//! The query and result count reach the driver script as `argv`, never
//! spliced into the embedded Python source, so there is nothing for a query
//! containing quotes or shell metacharacters to break out of.
//!
//! Failures come back as results the model can read rather than Rust errors: a
//! non-zero exit, a missing file, a failed search, or bad arguments are
//! information, not a reason to abandon the turn.

use crate::config::ToolsConfig;
use crate::danger;
use crate::llm::ToolCall;
use crate::workspace::Workspace;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub const RUN_COMMAND: &str = "run_command";
pub const READ_FILE: &str = "read_file";
pub const WRITE_FILE: &str = "write_file";
pub const WEB_SEARCH: &str = "web_search";

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
    vec![
        json!({
            "type": "function",
            "function": {
                "name": RUN_COMMAND,
                "description": format!(
                    "Run a shell command in the user's project directory via `{shell_name} {shell_flag}` \
                     and get back its exit code, stdout and stderr. Use this for things that are not \
                     reading or writing a single file: searching (grep/findstr), running builds and \
                     tests, installing packages. Prefer {READ_FILE}/{WRITE_FILE} over this for reading \
                     or writing files. The user must approve every command before it runs."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The command line to run, e.g. `grep -rn TODO src` or `python3 -m pytest`."
                        },
                        "purpose": {
                            "type": "string",
                            "description": "One short sentence on why you need it. Shown to the user in the approval prompt, so be specific and honest."
                        }
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": READ_FILE,
                "description": "Read a file's contents from the user's project directory. Prefer this \
                                 over running `cat`/`type` through run_command -- it is not subject to \
                                 shell quoting.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the project directory."
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": WRITE_FILE,
                "description": "Create a file, or overwrite an existing one, with new content. Creates \
                                 parent directories as needed. Prefer this over shell redirection or \
                                 `sed` through run_command -- the full new content is one argument, not \
                                 something to hand-encode into a shell string. The user approves every \
                                 write before it happens.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to write, relative to the project directory."
                        },
                        "content": {
                            "type": "string",
                            "description": "The file's full new contents. This replaces the entire file."
                        }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": WEB_SEARCH,
                "description": "Search the web and get back a short list of results (title, URL, \
                                 snippet). Use this when you need current information, something \
                                 outside your training data, or the user asks you to look something \
                                 up online. Requires Python 3 with the `ddgs` package installed on \
                                 the user's machine (`pip install ddgs`); if that's missing the \
                                 result will say so plainly rather than silently returning nothing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query, e.g. `rust async runtime comparison 2026`."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "How many results to return, 1-10. Defaults to 5."
                        }
                    },
                    "required": ["query"]
                }
            }
        }),
    ]
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
    // The command that hands a file to whatever app the OS has registered for
    // it -- `open`/`xdg-open`/`start` all return immediately after launching
    // that app, they do not block waiting for it to close, so this is safe
    // under the same non-interactive/timeout rules as any other command.
    let opener = match os {
        "macos" => "open",
        "windows" => "start",
        _ => "xdg-open",
    };

    format!(
        "You are tuisample-code, a terminal coding assistant.\n\n\
         Working directory: {}\n\
         Operating system: {os} — shell commands run through `{shell_name} {shell_flag}`\n\n\
         Tools:\n\
         - {READ_FILE}(path): read a file's contents.\n\
         - {WRITE_FILE}(path, content): create a file, or overwrite one, with new content.\n\
         - {RUN_COMMAND}(command, purpose): run a shell command and get back its exit code, \
           stdout and stderr.\n\
         - {WEB_SEARCH}(query, max_results): search the web, get back titles/URLs/snippets. \
           Needs Python 3 + the `ddgs` package on the user's machine -- if that's missing you'll \
           get a clear error instead of results; tell the user plainly rather than retrying.\n\n\
         Rules:\n\
         - {os_hint}\n\
         - Narrate in plain sentences, not just tool calls. Before acting, say in one short \
           sentence what you're about to do and why (e.g. \"I'll create hello.py and run it.\"). \
           After tool results come back, close with one short sentence saying what happened \
           (e.g. \"Created hello.py and ran it — it printed Hello, World!\"). Never end a turn \
           with only tool calls and nothing said about them; the tool log is not a substitute \
           for telling the user what you did. When you already know you'll need more than one \
           call to finish the thought -- write a file, then run it -- request all of them in the \
           same turn instead of one at a time: one before-sentence and one after-sentence should \
           cover the whole batch, not a fresh pair around each individual call.\n\
         - Verify before declaring success: run what you wrote, or run a real check -- the test \
           suite, a linter, importing the module, curling the endpoint -- and read the actual \
           output. Do not assume something works because the code looks right. If a command \
           fails, read the error and fix the real problem before retrying; do not repeat the \
           same failing command unchanged.\n\
         - If what you just created or changed is meant to be looked at rather than run for \
           output -- a webpage, an image, a document -- offer to open it with `{opener}` via \
           {RUN_COMMAND} instead of only telling the user how. This is still just another \
           command: it waits for the same approval as everything else, it does not skip the \
           prompt.\n\
         - Use {READ_FILE} to read a file and {WRITE_FILE} to create or change one -- not \
           `cat`/`type`/`sed`/shell redirection through {RUN_COMMAND}. Reserve {RUN_COMMAND} for \
           things that are not reading or writing a single file: search, builds, tests, running \
           a program. Look at the real project instead of guessing what it contains.\n\
         - Commands run through {RUN_COMMAND} are NON-INTERACTIVE: stdin is closed. Never run \
           anything that waits for input, opens an editor (vim, nano), or runs a server in the \
           foreground. Such a command will simply time out after {} seconds.\n\
         - The user approves every write, every command, and every web search before it runs \
           (reads of a short, conservative allowlist may go through without asking). If something \
           is declined, do not retry it — take a different approach or answer without it.\n\
         - Anything that changes or deletes files is real and immediate. Be conservative and \
           prefer the narrowest action that does the job.\n\
         - Answers appear in a terminal: keep narration to a sentence or two, not a report.",
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

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
}

#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default)]
    max_results: Option<u32>,
}

/// Bounds on how many results a single search may ask for. Below `MIN` a
/// search would be pointless; above `MAX` it is a good way to fill the
/// context window with snippets nobody reads.
const MIN_SEARCH_RESULTS: u32 = 1;
const MAX_SEARCH_RESULTS: u32 = 10;
const DEFAULT_SEARCH_RESULTS: u32 = 5;

/// What a call is asking to do, in a form the approval popup and the
/// transcript can render without knowing which tool produced it.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Command { command: String, purpose: Option<String> },
    Read { path: String },
    Write { path: String, content: String },
    Search { query: String, max_results: u32 },
}

impl Action {
    /// One line for `$ ... —` / transcript-style summaries, with a leading
    /// icon so the four kinds stay visually distinct in a transcript full
    /// of them.
    pub fn label(&self) -> String {
        match self {
            Action::Command { command, .. } => format!("$ {command}"),
            Action::Read { path } => format!("📄 read {path}"),
            Action::Write { path, .. } => format!("📝 write {path}"),
            Action::Search { query, .. } => format!("🔎 search \"{query}\""),
        }
    }
}

/// What `call` is asking to do, for the approval prompt. `None` if the model
/// sent something unusable (unknown tool name, unparseable or empty
/// arguments), in which case there is nothing to approve -- the runner
/// reports the malformed arguments back to the model instead.
pub fn describe_action(call: &ToolCall) -> Option<Action> {
    match call.function.name.as_str() {
        RUN_COMMAND => {
            let args: RunArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let command = args.command.trim().to_string();
            if command.is_empty() {
                return None;
            }
            Some(Action::Command {
                command,
                purpose: args.purpose.filter(|p| !p.trim().is_empty()),
            })
        }
        READ_FILE => {
            let args: ReadFileArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let path = args.path.trim().to_string();
            (!path.is_empty()).then_some(Action::Read { path })
        }
        WRITE_FILE => {
            let args: WriteFileArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let path = args.path.trim().to_string();
            (!path.is_empty()).then_some(Action::Write { path, content: args.content })
        }
        WEB_SEARCH => {
            let args: WebSearchArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let query = args.query.trim().to_string();
            if query.is_empty() {
                return None;
            }
            let max_results = args
                .max_results
                .unwrap_or(DEFAULT_SEARCH_RESULTS)
                .clamp(MIN_SEARCH_RESULTS, MAX_SEARCH_RESULTS);
            Some(Action::Search { query, max_results })
        }
        _ => None,
    }
}

/// Joins `path` onto the workspace root and rejects anything that resolves
/// outside it, purely by collapsing `..`/`.` components -- no filesystem
/// access, so this works for a `write_file` target that does not exist yet.
///
/// This is a guardrail against typos and prompt-injected paths, not a
/// sandbox: it does not follow symlinks, so a symlink inside the workspace
/// pointing outside it is not caught. It is nonetheless a real safety
/// property `run_command`'s shell cannot offer at all -- see the module doc.
fn resolve_in_workspace(workspace: &Workspace, path: &str) -> Result<PathBuf, String> {
    let mut resolved = workspace.root().to_path_buf();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => resolved.push(part),
            // An absolute `path` (a leading `/`, or `C:\` on Windows) is
            // exactly the escape this function exists to catch: replace
            // rather than join, and let the `starts_with` check below reject it.
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                resolved = PathBuf::from(component.as_os_str());
            }
        }
    }
    if !resolved.starts_with(workspace.root()) {
        return Err(format!(
            "'{path}' resolves outside the workspace ({})",
            workspace.root().display()
        ));
    }
    Ok(resolved)
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
    // Belt and braces. `app::advance_approvals` already refuses blocked calls
    // before they can be queued, so reaching this is a bug -- but the cost of
    // the check is a string scan and the cost of missing it is an erased disk,
    // so the runner refuses independently rather than trusting its caller.
    if let Some(Action::Command { command, .. }) = describe_action(call) {
        if let danger::Risk::Blocked(reason) = danger::classify(&command, workspace.root()) {
            return refused_as_dangerous(call, &reason);
        }
    }

    match call.function.name.as_str() {
        RUN_COMMAND => execute_run_command(call, workspace, config).await,
        READ_FILE => execute_read_file(call, workspace, config).await,
        WRITE_FILE => execute_write_file(call, workspace).await,
        WEB_SEARCH => execute_web_search(call, config).await,
        other => outcome(
            &call.id,
            format!("⚙ {other} — unknown tool"),
            format!(
                "Error: there is no tool named '{other}'. The tools are {RUN_COMMAND}, \
                 {READ_FILE}, {WRITE_FILE}, {WEB_SEARCH}."
            ),
        ),
    }
}

async fn execute_run_command(call: &ToolCall, workspace: &Workspace, config: &ToolsConfig) -> ToolOutcome {
    let Ok(args) = serde_json::from_str::<RunArgs>(&call.function.arguments) else {
        return outcome(
            &call.id,
            "⚙ run_command — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"command": "ls -la"}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };
    let command = args.command.trim().to_string();
    if command.is_empty() {
        return outcome(
            &call.id,
            "⚙ run_command — empty command".to_string(),
            "Error: the command was empty. Nothing was run.".to_string(),
        );
    }

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

async fn execute_read_file(call: &ToolCall, workspace: &Workspace, config: &ToolsConfig) -> ToolOutcome {
    let Ok(args) = serde_json::from_str::<ReadFileArgs>(&call.function.arguments) else {
        return outcome(
            &call.id,
            "📄 read_file — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"path": "src/main.rs"}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };
    let path = args.path.trim();
    if path.is_empty() {
        return outcome(
            &call.id,
            "📄 read_file — empty path".to_string(),
            "Error: the path was empty. Nothing was read.".to_string(),
        );
    }

    let resolved = match resolve_in_workspace(workspace, path) {
        Ok(p) => p,
        Err(e) => {
            return outcome(
                &call.id,
                format!("📄 read {} — refused", clip(path, 50)),
                format!("Error: {e}"),
            )
        }
    };

    match tokio::fs::read(&resolved).await {
        Ok(bytes) => {
            // Lossy on purpose, matching run_command's stdout/stderr handling:
            // a source file is not guaranteed valid UTF-8, and replacement
            // characters beat refusing to report anything.
            let text = String::from_utf8_lossy(&bytes);
            let lines = text.lines().count();
            outcome(
                &call.id,
                format!("📄 read {} — {lines} line{}", clip(path, 50), if lines == 1 { "" } else { "s" }),
                clip(&text, config.max_output_bytes),
            )
        }
        Err(e) => outcome(
            &call.id,
            format!("📄 read {} — failed", clip(path, 50)),
            format!("Error: could not read {path}: {e}"),
        ),
    }
}

async fn execute_write_file(call: &ToolCall, workspace: &Workspace) -> ToolOutcome {
    let Ok(args) = serde_json::from_str::<WriteFileArgs>(&call.function.arguments) else {
        return outcome(
            &call.id,
            "📝 write_file — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"path": "hello.py", "content": "..."}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };
    let path = args.path.trim();
    if path.is_empty() {
        return outcome(
            &call.id,
            "📝 write_file — empty path".to_string(),
            "Error: the path was empty. Nothing was written.".to_string(),
        );
    }

    let resolved = match resolve_in_workspace(workspace, path) {
        Ok(p) => p,
        Err(e) => {
            return outcome(
                &call.id,
                format!("📝 write {} — refused", clip(path, 50)),
                format!("Error: {e}"),
            )
        }
    };

    if let Some(parent) = resolved.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return outcome(
                    &call.id,
                    format!("📝 write {} — failed", clip(path, 50)),
                    format!("Error: could not create the directory for {path}: {e}"),
                );
            }
        }
    }

    match tokio::fs::write(&resolved, &args.content).await {
        Ok(()) => outcome(
            &call.id,
            format!("📝 write {} — {} bytes", clip(path, 50), args.content.len()),
            format!("Wrote {} bytes to {path}", args.content.len()),
        ),
        Err(e) => outcome(
            &call.id,
            format!("📝 write {} — failed", clip(path, 50)),
            format!("Error: could not write {path}: {e}"),
        ),
    }
}

/// The embedded driver for `web_search`, run via `python3 -c <this>`.
///
/// The query and result count arrive as `argv`, never interpolated into this
/// source string -- so a query containing quotes, backslashes, or something
/// that looks like Python (`"; import os; os.system(...)`) has nothing to
/// break out of, the same way `execute_run_command` never builds its shell
/// string by splicing untrusted text into other untrusted text.
///
/// Prints exactly one line of JSON and always exits 0 (even when `ddgs` is
/// missing or the search itself fails) so the two are told apart by content,
/// not by trying to parse a nonzero exit code out of a foreign interpreter's
/// traceback.
const DDGS_SCRIPT: &str = r#"
import json
import sys

try:
    from ddgs import DDGS
except ImportError:
    print(json.dumps({"error": "ddgs_not_installed"}))
    sys.exit(0)

query = sys.argv[1]
max_results = int(sys.argv[2])

try:
    with DDGS() as ddgs:
        results = list(ddgs.text(query, max_results=max_results))
    print(json.dumps({"results": results}))
except Exception as exc:
    print(json.dumps({"error": str(exc)}))
"#;

async fn execute_web_search(call: &ToolCall, config: &ToolsConfig) -> ToolOutcome {
    let Ok(args) = serde_json::from_str::<WebSearchArgs>(&call.function.arguments) else {
        return outcome(
            &call.id,
            "🔎 web_search — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"query": "..."}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };
    let query = args.query.trim().to_string();
    if query.is_empty() {
        return outcome(
            &call.id,
            "🔎 web_search — empty query".to_string(),
            "Error: the query was empty. Nothing was searched.".to_string(),
        );
    }
    let max_results = args
        .max_results
        .unwrap_or(DEFAULT_SEARCH_RESULTS)
        .clamp(MIN_SEARCH_RESULTS, MAX_SEARCH_RESULTS);

    let mut cmd = tokio::process::Command::new(&config.python_bin);
    cmd.arg("-c")
        .arg(DDGS_SCRIPT)
        .arg(&query)
        .arg(max_results.to_string())
        // Closed stdin and captured stdout/stderr for the same reason as
        // run_command: nothing here should ever wait on a terminal that
        // does not exist.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let limit = Duration::from_secs(config.search_timeout_secs);
    let output = match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return outcome(
                &call.id,
                format!("🔎 search \"{}\" — could not start", clip(&query, 50)),
                format!(
                    "Error: could not run '{}': {e}. web_search needs Python 3 with the `ddgs` \
                     package installed (pip install ddgs). If Python is installed under a \
                     different name on this machine, set tools.python_bin in config.toml.",
                    config.python_bin
                ),
            )
        }
        Err(_) => {
            return outcome(
                &call.id,
                format!("🔎 search \"{}\" — timed out", clip(&query, 50)),
                format!(
                    "Error: killed after {}s waiting for search results. Try again, or a \
                     narrower query.",
                    config.search_timeout_secs
                ),
            )
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return outcome(
            &call.id,
            format!("🔎 search \"{}\" — failed", clip(&query, 50)),
            format!(
                "Error: the search process exited with {:?}: {}",
                output.status.code(),
                clip(stderr.trim(), 500)
            ),
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    format_search_result(&call.id, &query, &stdout, config.max_output_bytes)
}

/// Turns the driver script's one line of JSON into what the model sees.
///
/// Split out from `execute_web_search` on purpose: this parsing and
/// formatting is the part worth testing exhaustively with fixture JSON --
/// success, empty results, a reported error, garbage output -- without any
/// of those tests needing Python or `ddgs` actually installed. Only the thin
/// subprocess plumbing around it requires a real interpreter to exercise.
fn format_search_result(call_id: &str, query: &str, stdout: &str, budget: usize) -> ToolOutcome {
    let trimmed = stdout.trim();

    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
        return outcome(
            call_id,
            format!("🔎 search \"{}\" — unreadable response", clip(query, 50)),
            format!(
                "Error: could not parse the search driver's output: {}",
                clip(trimmed, 300)
            ),
        );
    };

    if let Some(reason) = map.get("error").and_then(Value::as_str) {
        return if reason == "ddgs_not_installed" {
            outcome(
                call_id,
                format!("🔎 search \"{}\" — ddgs not installed", clip(query, 50)),
                "Error: the `ddgs` Python package is not installed. Install it with: \
                 pip install ddgs"
                    .to_string(),
            )
        } else {
            outcome(
                call_id,
                format!("🔎 search \"{}\" — failed", clip(query, 50)),
                format!("Error: web search failed: {reason}"),
            )
        };
    }

    let results = map
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        return outcome(
            call_id,
            format!("🔎 search \"{}\" — 0 results", clip(query, 50)),
            format!("No results found for '{query}'."),
        );
    }

    let mut content = String::new();
    for (i, r) in results.iter().enumerate() {
        let title = r.get("title").and_then(Value::as_str).unwrap_or("(untitled)");
        let href = r.get("href").and_then(Value::as_str).unwrap_or("");
        let body = r.get("body").and_then(Value::as_str).unwrap_or("");
        content.push_str(&format!("{}. {title}\n   {href}\n   {body}\n\n", i + 1));
    }

    let count = results.len();
    outcome(
        call_id,
        format!(
            "🔎 search \"{}\" — {count} result{}",
            clip(query, 50),
            if count == 1 { "" } else { "s" }
        ),
        clip(content.trim_end(), budget),
    )
}

/// The result to hand back when the user says no.
pub fn declined(call: &ToolCall) -> ToolOutcome {
    let label = describe_action(call)
        .map(|a| a.label())
        .unwrap_or_else(|| call.function.name.clone());
    outcome(
        &call.id,
        format!("{} — declined", clip(&label, 60)),
        "The user declined to let this happen. Do not try it again; take a different \
         approach or answer without it."
            .to_string(),
    )
}

/// The result for a call the guardrails refused outright.
///
/// Worded so the model treats it as a settled boundary rather than an obstacle
/// to route around: without that it tends to retry the same thing spelled
/// differently, which is exactly what a blocklist is worst at catching.
pub fn refused_as_dangerous(call: &ToolCall, reason: &str) -> ToolOutcome {
    let label = describe_action(call)
        .map(|a| a.label())
        .unwrap_or_else(|| call.function.name.clone());
    outcome(
        &call.id,
        format!("⛔ {} — blocked", clip(&label, 60)),
        format!(
            "Blocked by the safety guardrails and never run: {reason}.\n\
             This was refused by the tool itself, not by the user, and no setting can permit \
             it. Do not attempt this again in any form, and do not try to work around it. \
             Tell the user plainly what you wanted to do and why it was blocked, and let them \
             run it themselves if they judge it safe."
        ),
    )
}

/// The result for a call abandoned before any decision was made.
pub fn unanswered(call: &ToolCall, reason: &str) -> ToolOutcome {
    let label = describe_action(call)
        .map(|a| a.label())
        .unwrap_or_else(|| call.function.name.clone());
    outcome(
        &call.id,
        format!("{} — not run", clip(&label, 60)),
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

    fn read_call(path: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: READ_FILE.to_string(),
                arguments: json!({ "path": path }).to_string(),
            },
        }
    }

    fn write_call(path: &str, content: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: WRITE_FILE.to_string(),
                arguments: json!({ "path": path, "content": content }).to_string(),
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

        match describe_action(&c).expect("should describe") {
            Action::Command { command, purpose } => {
                assert_eq!(command, "rm -rf build");
                assert_eq!(purpose.as_deref(), Some("clear stale build output"));
            }
            other => panic!("expected Action::Command, got {other:?}"),
        }
    }

    #[test]
    fn read_and_write_actions_are_described_for_the_approval_prompt() {
        match describe_action(&read_call("src/main.rs")).expect("should describe") {
            Action::Read { path } => assert_eq!(path, "src/main.rs"),
            other => panic!("expected Action::Read, got {other:?}"),
        }

        match describe_action(&write_call("hello.py", "print('hi')\n")).expect("should describe") {
            Action::Write { path, content } => {
                assert_eq!(path, "hello.py");
                assert_eq!(content, "print('hi')\n");
            }
            other => panic!("expected Action::Write, got {other:?}"),
        }
    }

    #[test]
    fn a_call_with_an_unknown_tool_name_has_no_action_to_describe() {
        let mut c = call("ls");
        c.function.name = "delete_everything".to_string();
        assert_eq!(describe_action(&c), None);
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

    /// Regression: without this, a model that only emits tool calls leaves the
    /// transcript as a bare log of "$ ..."/"📝 ..." lines with nothing said
    /// about them -- what a user pointed at directly when comparing this to
    /// Claude Code's narrated "I'll just run it." / "Ran it — output: ...".
    #[test]
    fn the_system_prompt_requires_narration_before_and_after_tool_use() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, true);

        assert!(prompt.contains("Before acting"), "{prompt}");
        assert!(prompt.contains("After tool results come back"), "{prompt}");
        assert!(
            prompt.contains("Never end a turn with only tool calls"),
            "{prompt}"
        );
    }

    /// Regression: without this, "narrate before and after" was read as
    /// per-call rather than per-turn, so a write followed by a run produced
    /// three separate narrated turns (before the write, before the run, and a
    /// final summary) instead of one plan sentence and one result sentence --
    /// the gap a user pointed at directly when comparing this to Claude Code's
    /// single-summary output for the same two-step task.
    #[test]
    fn the_system_prompt_asks_for_multiple_calls_in_one_turn_rather_than_one_narrated_turn_each() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, true);

        assert!(prompt.contains("request all of them in the same turn"), "{prompt}");
        assert!(
            prompt.contains("not a fresh pair around each individual call"),
            "{prompt}"
        );
    }

    /// Without this a model can write code, never run it, and declare success
    /// on the strength of "it looks right" -- the same class of gap narration
    /// closed for communication, but for correctness.
    #[test]
    fn the_system_prompt_requires_verifying_work_before_declaring_success() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, true);

        assert!(prompt.contains("Verify before declaring success"), "{prompt}");
        assert!(
            prompt.contains("Do not assume something works because the code looks right"),
            "{prompt}"
        );
        assert!(
            prompt.contains("do not repeat the same failing command unchanged"),
            "{prompt}"
        );
    }

    /// Regression: without this, the model would write something like
    /// hello.html and only describe how to open it, instead of offering to
    /// open it itself the way Claude Code does. The nudge must not come at
    /// the cost of the approval rule -- opening still goes through the same
    /// prompt as every other command, there is no exception carved out here.
    #[test]
    fn the_system_prompt_offers_to_open_viewable_output_without_skipping_approval() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, true);

        assert!(
            prompt.contains("offer to open it with"),
            "{prompt}"
        );
        assert!(
            prompt.contains("it waits for the same approval as everything else, it does not skip the prompt"),
            "{prompt}"
        );
    }

    #[test]
    fn the_schemas_name_exactly_the_tools_that_execute() {
        let schemas = schemas();
        let names: Vec<_> = schemas.iter().map(|s| s["function"]["name"].clone()).collect();
        assert_eq!(names, vec![RUN_COMMAND, READ_FILE, WRITE_FILE, WEB_SEARCH]);
    }

    #[test]
    fn clipping_never_splits_a_multibyte_character() {
        let clipped = clip("héllo wörld→", 4);
        assert!(clipped.starts_with("héll"), "{clipped}");
        assert!(clipped.contains("truncated"), "{clipped}");
    }

    // ---- read_file / write_file --------------------------------------------

    #[tokio::test]
    async fn write_file_creates_parent_directories_and_writes_content() {
        let (dir, ws, cfg) = fixture();
        let out = execute(&write_call("nested/hello.py", "print('hi')\n"), &ws, &cfg).await;

        assert!(out.content.contains("Wrote"), "{}", out.content);
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("nested/hello.py")).await.unwrap(),
            "print('hi')\n"
        );
    }

    #[tokio::test]
    async fn write_file_overwrites_an_existing_file() {
        let (_dir, ws, cfg) = fixture();
        execute(&write_call("hello.txt", "new content\n"), &ws, &cfg).await;
        let out = execute(&read_call("hello.txt"), &ws, &cfg).await;
        assert_eq!(out.content, "new content\n");
    }

    #[tokio::test]
    async fn read_file_reports_the_content_and_line_count() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&read_call("hello.txt"), &ws, &cfg).await;

        assert_eq!(out.content, "one\ntwo\nthree\n");
        assert!(out.display.contains("3 lines"), "{}", out.display);
    }

    #[tokio::test]
    async fn reading_a_missing_file_is_reported_not_a_panic() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&read_call("does-not-exist.txt"), &ws, &cfg).await;
        assert!(out.content.starts_with("Error:"), "{}", out.content);
    }

    /// The one safety property a typed file tool has that the shell tool
    /// cannot: the path is checked before anything happens, not merely hoped
    /// to be well-behaved.
    #[tokio::test]
    async fn a_path_that_escapes_the_workspace_is_refused() {
        let (_dir, ws, cfg) = fixture();
        for escaping in ["../outside.txt", "../../etc/passwd", "/etc/passwd"] {
            let out = execute(&write_call(escaping, "pwned"), &ws, &cfg).await;
            assert!(
                out.content.contains("outside the workspace"),
                "{escaping}: {}",
                out.content
            );

            let out = execute(&read_call(escaping), &ws, &cfg).await;
            assert!(
                out.content.contains("outside the workspace"),
                "{escaping}: {}",
                out.content
            );
        }
    }

    /// `..` that nets out *inside* the workspace must still work -- the guard
    /// is about where a path ends up, not whether it merely contains `..`.
    #[tokio::test]
    async fn a_path_using_dotdot_that_stays_inside_the_workspace_is_allowed() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&read_call("subdir/../hello.txt"), &ws, &cfg).await;
        assert_eq!(out.content, "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn read_output_is_capped_like_command_output() {
        let (dir, ws, mut cfg) = fixture();
        cfg.max_output_bytes = 10;
        std::fs::write(dir.path().join("big.txt"), "a".repeat(1000)).unwrap();
        let out = execute(&read_call("big.txt"), &ws, &cfg).await;
        assert!(out.content.contains("truncated at 10"), "{}", out.content);
    }

    // --- web_search ---------------------------------------------------------
    //
    // Two layers, tested separately on purpose (see `format_search_result`'s
    // doc comment): the pure JSON-to-transcript formatting below needs no
    // subprocess and runs everywhere, while the subprocess-level tests swap
    // in a fake "interpreter" -- a tiny shell script standing in for
    // `python3` -- so timeouts, a missing `ddgs`, and malformed output can
    // all be exercised deterministically without depending on what's
    // actually installed on the machine running the test suite.

    fn search_call(query: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: WEB_SEARCH.to_string(),
                arguments: json!({ "query": query }).to_string(),
            },
        }
    }

    fn search_call_with_max(query: &str, max_results: u32) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: WEB_SEARCH.to_string(),
                arguments: json!({ "query": query, "max_results": max_results }).to_string(),
            },
        }
    }

    #[test]
    fn web_search_action_defaults_and_clamps_max_results() {
        assert_eq!(
            describe_action(&search_call("rust")),
            Some(Action::Search { query: "rust".to_string(), max_results: 5 })
        );
        assert_eq!(
            describe_action(&search_call_with_max("rust", 0)),
            Some(Action::Search { query: "rust".to_string(), max_results: 1 })
        );
        assert_eq!(
            describe_action(&search_call_with_max("rust", 999)),
            Some(Action::Search { query: "rust".to_string(), max_results: 10 })
        );
        assert_eq!(
            describe_action(&search_call_with_max("rust", 3)),
            Some(Action::Search { query: "rust".to_string(), max_results: 3 })
        );
    }

    #[test]
    fn an_empty_or_whitespace_search_query_has_no_action() {
        assert_eq!(describe_action(&search_call("")), None);
        assert_eq!(describe_action(&search_call("   ")), None);
    }

    #[test]
    fn format_search_result_renders_multiple_results_in_order() {
        let json = r#"{"results": [
            {"title": "Rust", "href": "https://rust-lang.org", "body": "A systems language"},
            {"title": "Rust (Wikipedia)", "href": "https://en.wikipedia.org/wiki/Rust", "body": "Encyclopedia entry"}
        ]}"#;
        let out = format_search_result("call_1", "rust", json, 8192);
        assert!(out.display.contains("2 results"), "{}", out.display);
        assert!(out.content.contains("1. Rust\n"), "{}", out.content);
        assert!(out.content.contains("https://rust-lang.org"), "{}", out.content);
        assert!(out.content.contains("A systems language"), "{}", out.content);
        assert!(out.content.contains("2. Rust (Wikipedia)"), "{}", out.content);
        // Order matters: result 1 must appear before result 2.
        assert!(out.content.find("1. Rust\n").unwrap() < out.content.find("2. Rust").unwrap());
    }

    #[test]
    fn format_search_result_uses_singular_wording_for_one_result() {
        let json = r#"{"results": [{"title": "T", "href": "https://x", "body": "B"}]}"#;
        let out = format_search_result("call_1", "q", json, 8192);
        assert!(out.display.contains("1 result"), "{}", out.display);
        assert!(!out.display.contains("1 results"), "{}", out.display);
    }

    #[test]
    fn format_search_result_reports_no_results_plainly_rather_than_as_an_error() {
        let out = format_search_result("call_1", "an obscure query", r#"{"results": []}"#, 8192);
        assert!(!out.content.starts_with("Error:"), "{}", out.content);
        assert!(out.content.contains("No results found for 'an obscure query'"), "{}", out.content);
    }

    #[test]
    fn format_search_result_explains_a_missing_ddgs_package() {
        let out = format_search_result("call_1", "q", r#"{"error": "ddgs_not_installed"}"#, 8192);
        assert!(out.content.contains("pip install ddgs"), "{}", out.content);
    }

    #[test]
    fn format_search_result_surfaces_a_generic_search_failure() {
        let out = format_search_result("call_1", "q", r#"{"error": "RatelimitException: 202"}"#, 8192);
        assert!(out.content.contains("RatelimitException: 202"), "{}", out.content);
    }

    #[test]
    fn format_search_result_handles_garbage_output_without_panicking() {
        for garbage in ["not json at all", "", "   ", "[1,2,3]", "\"just a string\"", "null", "{}"] {
            let out = format_search_result("call_1", "q", garbage, 8192);
            assert!(!out.content.is_empty(), "garbage={garbage:?}");
        }
    }

    #[test]
    fn format_search_result_tolerates_missing_fields_in_a_result() {
        let out = format_search_result("call_1", "q", r#"{"results": [{}]}"#, 8192);
        assert!(out.content.contains("(untitled)"), "{}", out.content);
    }

    #[test]
    fn format_search_result_output_is_capped_by_the_configured_budget() {
        let many: Vec<Value> = (0..50)
            .map(|i| json!({"title": format!("Result {i}"), "href": "https://x", "body": "x".repeat(200)}))
            .collect();
        let stdout = json!({ "results": many }).to_string();
        let out = format_search_result("call_1", "q", &stdout, 500);
        assert!(out.content.contains("truncated at 500"), "{}", out.content);
    }

    #[tokio::test]
    async fn unusable_web_search_arguments_are_explained_rather_than_run() {
        let (_dir, ws, cfg) = fixture();
        let mut bad = search_call("");
        bad.function.arguments = "{not json".to_string();
        let out = execute(&bad, &ws, &cfg).await;
        assert!(out.content.contains("could not read the arguments"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_empty_web_search_query_is_not_run() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&search_call("   "), &ws, &cfg).await;
        assert!(out.content.starts_with("Error:"), "{}", out.content);
    }

    /// A nonexistent interpreter must fail the same way a nonexistent shell
    /// would: cleanly, with a message that tells the user what to install,
    /// not a panic or a silent hang. No fake script needed -- `Command::new`
    /// fails to spawn identically on every platform when the binary is not
    /// found.
    #[tokio::test]
    async fn a_missing_python_interpreter_is_explained_rather_than_panicking() {
        let (_dir, ws, mut cfg) = fixture();
        cfg.python_bin = "no-such-interpreter-xyz-123".to_string();
        let out = execute(&search_call("rust"), &ws, &cfg).await;
        assert!(out.content.contains("could not run"), "{}", out.content);
        assert!(out.content.contains("pip install ddgs"), "{}", out.content);
    }

    /// A fake "interpreter" -- a tiny shell script standing in for `python3`
    /// -- so the subprocess-handling paths in `execute_web_search` can be
    /// tested deterministically regardless of whether the real `ddgs` is
    /// installed on the machine running the tests. Unix-only: a faithful
    /// Windows equivalent needs a `.bat`/`.cmd` or a real exe, which is more
    /// machinery than this is worth duplicating for.
    #[cfg(unix)]
    fn fake_interpreter(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-python.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake interpreter");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn web_search_reports_ddgs_not_installed_end_to_end() {
        let (dir, ws, mut cfg) = fixture();
        cfg.python_bin = fake_interpreter(dir.path(), r#"echo '{"error": "ddgs_not_installed"}'"#);
        let out = execute(&search_call("rust"), &ws, &cfg).await;
        assert!(out.content.contains("pip install ddgs"), "{}", out.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn web_search_returns_real_looking_results_end_to_end() {
        let (dir, ws, mut cfg) = fixture();
        cfg.python_bin = fake_interpreter(
            dir.path(),
            r#"echo '{"results": [{"title": "Rust", "href": "https://rust-lang.org", "body": "A language"}]}'"#,
        );
        let out = execute(&search_call("rust programming language"), &ws, &cfg).await;
        assert!(out.content.contains("Rust"), "{}", out.content);
        assert!(out.content.contains("https://rust-lang.org"), "{}", out.content);
        assert!(out.display.contains("1 result"), "{}", out.display);
    }

    /// Without a timeout, a search that never returns (a hung `ddgs` call, a
    /// stalled network) would freeze the turn forever -- the same failure
    /// mode `a_command_that_never_finishes_is_killed` guards against for
    /// `run_command`.
    #[cfg(unix)]
    #[tokio::test]
    async fn web_search_that_never_finishes_is_killed() {
        let (dir, ws, mut cfg) = fixture();
        cfg.python_bin = fake_interpreter(dir.path(), "sleep 30");
        cfg.search_timeout_secs = 1;

        let started = std::time::Instant::now();
        let out = execute(&search_call("rust"), &ws, &cfg).await;

        assert!(out.content.contains("killed after 1s"), "{}", out.content);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "should have been killed promptly, took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn web_search_surfaces_a_reported_backend_failure() {
        let (dir, ws, mut cfg) = fixture();
        cfg.python_bin =
            fake_interpreter(dir.path(), r#"echo '{"error": "RatelimitException"}'"#);
        let out = execute(&search_call("rust"), &ws, &cfg).await;
        assert!(out.content.contains("RatelimitException"), "{}", out.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn web_search_output_is_capped_like_command_output() {
        let (dir, ws, mut cfg) = fixture();
        cfg.max_output_bytes = 20;
        cfg.python_bin = fake_interpreter(
            dir.path(),
            r#"echo '{"results": [{"title": "T", "href": "https://x", "body": "a very long snippet that goes on and on"}]}'"#,
        );
        let out = execute(&search_call("rust"), &ws, &cfg).await;
        assert!(out.content.contains("truncated at 20"), "{}", out.content);
    }

    /// A query that looks like it might break out of the embedded Python
    /// source or a shell -- quotes, backticks, `$()`, an `os.system` payload
    /// -- must be treated as inert search text, never executed. The query
    /// reaches the driver script as `argv`, not spliced into source, so
    /// there is nothing here for it to break out of; confirmed manually
    /// against the real `ddgs` backend with a batch of these during review
    /// (recorded in the session, not kept as a flaky network-dependent test).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_query_that_looks_like_an_injection_attempt_is_inert() {
        let (dir, ws, mut cfg) = fixture();
        cfg.python_bin = fake_interpreter(
            dir.path(),
            r#"echo '{"results": [{"title": "T", "href": "https://x", "body": "B"}]}'"#,
        );
        for q in [
            r#""; import os; os.system('touch /tmp/pwned-test-marker'); x = ""#,
            "query with `backticks` and $(command) substitution",
            "query\nwith\nnewlines\tand\ttabs",
        ] {
            let out = execute(&search_call(q), &ws, &cfg).await;
            assert!(out.content.contains("https://x"), "{}", out.content);
        }
        assert!(
            !std::path::Path::new("/tmp/pwned-test-marker").exists(),
            "an injection attempt in the query must never execute"
        );
    }

    /// The real thing, run against the actual `ddgs` package -- skipped
    /// rather than failed when Python or `ddgs` are not available, since a
    /// third-party network dependency has no business making the unit test
    /// suite red on a machine that has neither installed.
    #[tokio::test]
    async fn web_search_works_end_to_end_against_the_real_ddgs_if_available() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(&search_call("rust programming language"), &ws, &cfg).await;
        if out.content.contains("could not run") || out.content.contains("pip install ddgs") {
            eprintln!("skipping: python3/ddgs not available in this environment ({})", out.content);
            return;
        }
        assert!(
            out.display.contains("result"),
            "expected real search results, got: {}",
            out.content
        );
    }
}
