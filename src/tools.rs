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
//! This makes a working Python install a real runtime dependency, which is
//! why `install.sh`/`install.ps1` auto-install `ddgs` (and, when there is no
//! system Python at all, a self-contained one -- see `embedded_python_path`
//! below and those scripts' own `ensure_ddgs_available`/`Install-Ddgs`). All
//! of this is a TEMPORARY stop-gap, not a permanent architecture decision:
//! the plan is to move `web_search` off `ddgs` entirely onto a real search
//! API (Brave Search was the original recommendation) with no runtime
//! dependency to install in the first place, once this is past its testing
//! stage. Kept simple until then rather than engineered for permanence.
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
pub const LIST_DIR: &str = "list_dir";
pub const GLOB: &str = "glob";
pub const GREP_SEARCH: &str = "grep_search";
pub const EDIT_FILE: &str = "edit_file";
pub const WEB_SEARCH: &str = "web_search";
pub const DEPLOY_PROJECT: &str = "deploy_project";
pub const PUBLISH_ARTIFACT: &str = "publish_artifact";
pub const EXIT_PLAN_MODE: &str = "exit_plan_mode";
pub const PLAN_PROGRESS: &str = "plan_progress";

/// Whether the model is allowed to change anything yet.
///
/// This is a *capability* switch, not an approval setting. `Plan` does not
/// mean "ask more carefully" -- approval prompts already do that, and one
/// mistaken `y` gets through them. It means the tools that write are not on
/// the model's list at all, so there is nothing to mistakenly approve. The
/// only way out is `exit_plan_mode`, which puts the plan itself in front of
/// the user as the thing being approved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Every tool available, each write and command approved as it comes.
    #[default]
    Normal,
    /// Read-only. Reads, listings, globs, searches and read-only commands
    /// work as usual; anything that could change the project is refused
    /// before it is ever offered for approval.
    Plan,
}

impl Mode {
    pub fn is_plan(self) -> bool {
        self == Mode::Plan
    }
}

/// Directories whose contents are build output or dependencies rather than the
/// project. Walking them turns a `glob` into thousands of irrelevant results and
/// buries whatever the model was looking for.
pub const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "dist", "build"];

/// Ceiling on `glob` results, so a careless `**/*` cannot fill the context
/// window with paths.
const MAX_GLOB_RESULTS: usize = 500;
/// Ceiling on `list_dir` entries, for the same reason.
const MAX_DIR_ENTRIES: usize = 300;
/// Ceiling on `grep_search` matched lines, for the same reason again -- a
/// pattern like `e` would otherwise return most of the project.
const MAX_GREP_MATCHES: usize = 200;
/// Files bigger than this are skipped by `grep_search` rather than read: at
/// this size they are lockfiles, bundles or data, and reading them costs more
/// than any match in them is worth.
const MAX_GREP_FILE_BYTES: u64 = 1_000_000;
/// How much surrounding context `grep_search` may be asked for per match.
const MAX_GREP_CONTEXT: u32 = 5;
/// A matched or context line is clipped to this many characters so one
/// minified line cannot swallow the whole result budget.
const MAX_GREP_LINE_CHARS: usize = 250;

/// Files read from the project root into every request's system prompt --
/// standing instructions the user (or `/init`) maintains. `BOXCODE.md` is
/// this app's own; `AGENTS.md` is the convention other coding tools read and
/// write, honoured so a project already carrying one works here unchanged.
pub const MEMORY_FILES: &[&str] = &["BOXCODE.md", "AGENTS.md"];

/// Ceiling on how much of one memory file reaches the prompt. These notes are
/// resent with every request for the rest of every session, so an unbounded
/// file taxes each turn from then on.
const MAX_MEMORY_CHARS: usize = 16_000;

fn is_skipped(path: &Path, workspace: &Path) -> bool {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .components()
        .any(|c| SKIP_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
}

/// A workspace-relative path for display, falling back to the full path if the
/// file somehow sits outside (which `resolve_in_workspace` should have caught).
fn relative_to(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .display()
        .to_string()
}

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

/// The tool list sent to the model, for the mode the session is in.
///
/// In `Mode::Plan` the writing tools are not filtered out at the approval
/// layer, they are never advertised in the first place: a tool the model was
/// never told about is one it cannot decide to call. `run_command` stays --
/// research needs `git log`, `grep`, `cargo tree` -- and is narrowed to
/// read-only commands by `plan_mode_block` instead.
///
/// `deploy` is the same idea for `[deploy] enabled = false`: a schema the
/// model can see is one it will eventually call, and answering "that is turned
/// off" afterwards is a worse experience than never offering it.
pub fn schemas(mode: Mode, active_plan: bool, deploy: bool) -> Vec<Value> {
    let (shell_name, shell_flag) = shell();
    let mut schemas = vec![
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
                                 shell quoting. For a big file, pass offset/limit to read just a \
                                 slice: sliced output comes back with a line number in front of each \
                                 line. The numbers are annotations, not file content -- never copy \
                                 them into edit_file's old_string.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file, relative to the project directory."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Line to start reading from, counted from 1. Omit to read from the top."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "How many lines to return. Omit to read to the end."
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
                "name": LIST_DIR,
                "description": format!(
                    "List the files and subdirectories of one directory in the project. \
                     Cheaper and more predictable than shelling out to `ls`/`dir`, and it \
                     skips {} automatically. Read-only.",
                    SKIP_DIRS.join(", ")
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory relative to the project root. Defaults to the root itself."
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": GLOB,
                "description": format!(
                    "Find files by path pattern, recursively. Use this to locate files when you \
                     know roughly what they are called or where they live -- it is faster and \
                     safer than `find`, and it skips {}. Read-only.",
                    SKIP_DIRS.join(", ")
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob relative to the project root, e.g. 'src/**/*.rs' or '**/Cargo.toml'."
                        }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": GREP_SEARCH,
                "description": format!(
                    "Search file CONTENTS for a regular expression, recursively, and get back \
                     matching lines as path:line: text. Use this to find code by what it says -- \
                     where a function is called, where a string appears -- where `glob` finds \
                     files by what they are called. Case-sensitive; prefix the pattern with \
                     `(?i)` for case-insensitive. Skips {} and binary files. Read-only.",
                    SKIP_DIRS.join(", ")
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regular expression to search for, e.g. 'fn resolve_\\w+' or '(?i)todo'. Matched against each line."
                        },
                        "path": {
                            "type": "string",
                            "description": "File or directory to search, relative to the project root. Defaults to the whole project."
                        },
                        "context": {
                            "type": "integer",
                            "description": format!("Lines of surrounding context to include per match, 0-{MAX_GREP_CONTEXT}. Defaults to 0.")
                        }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": EDIT_FILE,
                "description":
                    "Replace an exact span of text in an existing file, leaving the rest untouched. \
                     Prefer this over write_file for changing part of a file: write_file replaces \
                     the whole thing, so it loses anything you did not reproduce. Read the file \
                     first -- old_string must match byte for byte, including indentation. To make \
                     several replacements in the same file, pass them together under `edits` \
                     instead of calling this repeatedly: the user approves the whole batch once, \
                     and it applies in order, all of it or none of it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to edit, relative to the project directory."
                        },
                        "old_string": {
                            "type": "string",
                            "description": "Exact text to replace, including indentation. Must be unique unless replace_all is set."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Text to put in its place."
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace every occurrence instead of requiring a unique match."
                        },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_string": { "type": "string" },
                                    "new_string": { "type": "string" },
                                    "replace_all": { "type": "boolean" }
                                },
                                "required": ["old_string", "new_string"]
                            },
                            "description": "Several replacements to apply to this file in order, each seeing the previous ones' result. Use INSTEAD of old_string/new_string, never alongside them."
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": PUBLISH_ARTIFACT,
                "description": "Upload static files to a temporary public URL so the user can \
                                LOOK at them in a browser. Use this ONLY when the user asks to \
                                see, preview, open, share or check how something looks. Never \
                                call it on your own initiative, never to 'check your work', and \
                                never after merely creating or editing a file that has not been \
                                published before -- say the file is written and stop. Works for \
                                a built site or SPA (point at the output directory, e.g. dist/), \
                                a single HTML page, a chart or diagram, a CSV or a text file. \
                                Publishing the same path again (e.g. after editing it) updates \
                                that link in place rather than creating a new one, so there is no \
                                new link to hand over -- but that update only happens when you \
                                call this tool again. If a path has already been published and \
                                you edit it afterward, you must call this tool again on that path \
                                before telling the user it is live -- editing the file does not \
                                refresh the hosted copy by itself. The link is public to anyone \
                                who has it and stops working after 48 hours, which you must tell \
                                the user when you give it to them.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File or directory to publish, relative to the project. For a site, the BUILT output directory (dist/, build/, out/, public/) -- not the project root, which has no index.html."
                        }
                    },
                    "required": ["path"]
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
        json!({
            "type": "function",
            "function": {
                "name": DEPLOY_PROJECT,
                "description":
                    "Deploy this project to a hosting provider and get back the live URL. Detects \
                     the framework, build command and output directory automatically, links or \
                     creates the provider-side project, runs the build and uploads it. The user \
                     approves before anything is deployed, and anything it needs along the way -- \
                     installing the provider's CLI, signing in -- it asks the user for directly \
                     as it goes. Request it on its own, never alongside other tool calls. On \
                     failure the build log comes back with the error, so read it and fix the real \
                     problem before retrying.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "provider": {
                            "type": "string",
                            "enum": ["vercel", "netlify"],
                            "description": "Which host to deploy to."
                        },
                        "production": {
                            "type": "boolean",
                            "description": "True for the live production URL, false (the default) for a throwaway preview. Only pass true when the user has asked for production."
                        }
                    },
                    "required": ["provider"]
                }
            }
        }),
    ];

    if !deploy {
        schemas.retain(|schema| schema["function"]["name"] != DEPLOY_PROJECT);
    }
    if mode.is_plan() {
        schemas.retain(|schema| {
            let name = schema["function"]["name"].as_str().unwrap_or_default();
            // `deploy_project` belongs here with the writing tools. It changes
            // nothing in the working directory, which is exactly why it would
            // slip past a check that only looks at files -- but it builds the
            // project and puts it on the public internet, which is the least
            // reversible thing this program can do.
            name != WRITE_FILE && name != EDIT_FILE && name != DEPLOY_PROJECT
        });
        schemas.push(json!({
            "type": "function",
            "function": {
                "name": EXIT_PLAN_MODE,
                "description":
                    "Present your finished plan to the user and ask to start implementing it. \
                     You are in plan mode: nothing you do can change the project until the user \
                     approves a plan through this tool. Call it once you have investigated \
                     enough to say concretely what you would change. If the user approves, the \
                     plan is SAVED AS A FILE in the project, plan mode ends, and you implement \
                     it step by step; if they decline, you stay in plan mode -- read their \
                     reply, revise, and propose again. Because an approved plan becomes a file \
                     that outlives this conversation and that other people will read, write it \
                     for someone who was not here. Do not call this to ask a question or to \
                     report that you are blocked; say that in text instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "A short name for the work, e.g. 'Rate limiting for \
                                            the items API'. Becomes the filename and the \
                                            heading, so make it specific -- 'Fixes' or 'Changes' \
                                            is useless six weeks later."
                        },
                        "summary": {
                            "type": "string",
                            "description": "The approach, in a few sentences of markdown. WHY \
                                            this shape rather than another, and any decision \
                                            the user would want to disagree with. Not a restatement \
                                            of the steps."
                        },
                        "steps": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "The work, in order, one entry per step. Name the file \
                                            each step touches. Keep each to something you can \
                                            finish and verify in one go -- these get ticked off \
                                            one at a time as you implement, and a step like \
                                            'build the feature' can never be honestly ticked."
                        },
                        "not_doing": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Things deliberately left out, and why. Optional, but \
                                            this is where the user most often disagrees, so it is \
                                            worth stating."
                        }
                    },
                    "required": ["title", "summary", "steps"]
                }
            }
        }));
    }

    // Only alongside a plan that is actually being worked through. Offered
    // unconditionally, it becomes a tool the model calls to look diligent
    // about work no plan ever described.
    if active_plan {
        schemas.push(json!({
            "type": "function",
            "function": {
                "name": PLAN_PROGRESS,
                "description":
                    "Record that a step of the approved plan is finished, or that it cannot be. \
                     Call this immediately after the work for a step is done and verified -- not \
                     in a batch at the end, and never before. This writes to the plan file, so \
                     it is how the work survives the conversation: someone picking the plan up \
                     tomorrow sees exactly where it got to. Marking a step done that you did not \
                     actually finish makes the file lie.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "step": {
                            "type": "integer",
                            "description": "Which step, numbered from 1 as they appear in the plan."
                        },
                        "status": {
                            "type": "string",
                            "enum": ["done", "blocked"],
                            "description": "'done' when it is finished and verified. 'blocked' \
                                            when it cannot be finished -- say why in `note`, and \
                                            tell the user rather than quietly moving on."
                        },
                        "note": {
                            "type": "string",
                            "description": "Required for 'blocked': what is in the way. Recorded \
                                            in the plan file."
                        }
                    },
                    "required": ["step", "status"]
                }
            }
        }));
    }

    schemas
}

/// What the model is told about its situation.
///
/// The operating system is stated outright because the single most common way
/// this tool fails is a model reaching for `ls` on Windows. `tools_available`
/// goes false once the step budget is spent.
pub fn system_prompt(
    workspace: &Workspace,
    config: &ToolsConfig,
    steps_used: usize,
    mode: Mode,
    active_plan: Option<&crate::plan::Plan>,
) -> String {
    if steps_used >= config.max_steps {
        return format!(
            "You are boxcode, a terminal coding assistant working in {}.\n\
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

    // The writing tools are named only when they are actually on the model's
    // list. Describing a tool that was not sent invites a call that comes back
    // as an error, and spends a turn discovering what the prompt could have
    // said outright.
    let write_tools = if mode.is_plan() {
        String::new()
    } else {
        format!(
            "- {WRITE_FILE}(path, content): create a file, or overwrite one, with new content.\n\
             - {EDIT_FILE}(path, old_string, new_string, replace_all): replace an exact span of \
             text in an existing file, leaving the rest untouched. Several changes to the SAME \
             file belong in one call as edits: [{{old_string, new_string}}, ...] -- the user \
             approves the batch once, and it applies all-or-nothing.\n"
        )
    };

    let mut prompt = format!(
        "You are boxcode, a terminal coding assistant.\n\n\
         Working directory: {}\n\
         Operating system: {os} — shell commands run through `{shell_name} {shell_flag}`\n\n\
         Tools:\n\
         - {READ_FILE}(path, offset, limit): read a file's contents, or just a slice of a big \
           one -- offset is the 1-based line to start from, limit is how many lines. Sliced \
           output is line-numbered; the numbers are annotations, never file content.\n\
         {write_tools}\
         - {LIST_DIR}(path): list one directory. Read-only, runs without asking.\n\
         - {GLOB}(pattern): find files by path pattern, e.g. 'src/**/*.rs'. Read-only, runs \
           without asking.\n\
         - {GREP_SEARCH}(pattern, path, context): search file contents for a regular \
           expression, recursively; returns matching lines as path:line: text. Read-only, runs \
           without asking.\n\
         - {RUN_COMMAND}(command, purpose): run a shell command and get back its exit code, \
           stdout and stderr.\n\
         - {WEB_SEARCH}(query, max_results): search the web, get back titles/URLs/snippets. \
           Needs Python 3 + the `ddgs` package on the user's machine -- if that's missing you'll \
           get a clear error instead of results; tell the user plainly rather than retrying.\n\
         - {DEPLOY_PROJECT}(provider, production): deploy this project to Vercel or Netlify and \
           get back the live URL.\n\n\
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
         - Explore with {LIST_DIR} and {GLOB} before guessing at paths, and search contents \
           with {GREP_SEARCH} before guessing at what a file says. Prefer all three over \
           `ls`/`find`/`grep` through {RUN_COMMAND}: they need no approval, so they cost the \
           user nothing to answer.\n\
         - To change part of an existing file use {EDIT_FILE}, not {WRITE_FILE}. {WRITE_FILE} \
           replaces the entire file, so it silently destroys anything you did not reproduce; \
           reserve it for creating a file or rewriting one wholesale. Read the file first so \
           old_string matches byte for byte.\n\
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
         - {DEPLOY_PROJECT} puts this project on the public internet. Only use it when the user \
           asks to deploy, ship, publish or push something live. Default to a preview: pass \
           production only when they said production or live. Anything it needs along the way -- \
           installing the provider's CLI, signing in -- it asks the user for directly, so just \
           call it and let it handle that. Request it on its own, never alongside other tool \
           calls. If the build fails, the log comes back with the error: read it, fix the real \
           problem, and only then deploy again.\n\
         - For anything on GitHub -- repositories, pull requests, issues, releases, CI runs, \
           contributor or commit counts -- use the `gh` CLI through {RUN_COMMAND}. It is \
           already signed in, and its read commands run without asking the user, so there is \
           no reason to guess or to say you cannot check.\n\
         - `gh`'s list commands return only the FIRST 30 RESULTS unless you say otherwise. \
           `gh repo list`, `gh pr list`, `gh issue list`, `gh run list` and the rest all \
           default to 30, so a bare call quietly gives you a partial answer that looks \
           complete. Always pass an explicit `--limit` (e.g. `--limit 1000`), and for \
           anything that may exceed one API page use `gh api --paginate`. Never report a \
           count, a total, or \"all of them\" from a command you did not bound yourself.\n\
         - Ask for the fields you need rather than parsing the human-readable table: \
           `gh repo list --limit 1000 --json name,visibility,updatedAt` and `--jq` to filter. \
           It is exact, it is smaller, and it does not change shape between `gh` versions.\n\
         - If output comes back truncated, narrow the query -- more `--jq`, fewer fields, a \
           smaller `--limit` -- and run it again. Do not extrapolate from a partial result or \
           present it as the whole.\n\
         - {PUBLISH_ARTIFACT} is for when the user wants to LOOK at something: \"show me\", \
           \"let me see it\", \"how does it look\", \"preview\", \"open it\", \"share this\". \
           Only then, for a path never published before. Once a path HAS been published, an \
           edit to it does not update the live link by itself -- call {PUBLISH_ARTIFACT} again \
           on that path before saying it is live; never claim \"already live at the same URL\" \
           without actually calling it. Writing or changing a file you have not published yet \
           is not a reason to publish it, and neither is checking your own work -- say what you \
           did and stop. The link is public to whoever \
           has it and dies after 48 hours, so always say both when you hand it over.\n\
         - Answers appear in a terminal: keep narration to a sentence or two, not a report.\n\
         - That terminal renders markdown, so use it where it carries meaning and nowhere \
           else. `**bold**` for the few words that decide something; `backticks` around every \
           path, command, identifier and flag; `-` for an unordered list and `1.` for steps \
           that happen in order; fenced blocks with a language tag for anything meant to be \
           copied or run; `##` headings only when the answer really has separate sections.\n\
         - Reach for a markdown table whenever the answer compares several things on the same \
           few attributes -- options and what each does, files and what changed in them, \
           before and after, flags and their defaults. Prose that repeats the same shape for \
           every item is exactly the case a table exists for, and it is read at a glance where \
           the sentences are not. Give it a header row and keep the cells to a few words; the \
           pane is narrow and long cells wrap.\n\
         - Do not use `__bold__`, HTML, images, footnotes or nested tables: they are not \
           rendered and reach the user as raw punctuation.",
        workspace.root().display(),
        config.command_timeout_secs,
    );

    // Standing project notes ride along on every request, so a session picks
    // up where the project's own documentation of itself left off -- build
    // commands, layout, conventions -- without the model re-deriving them.
    // Read fresh each time rather than cached at startup: the user edits
    // these files mid-session, and stale instructions silently followed are
    // worse than a file read per request.
    for (name, content) in project_memory(workspace) {
        prompt.push_str(&format!(
            "\n\nPROJECT NOTES from {name}, kept in the project root by the user (and by \
             /init). Standing instructions for working in this project -- follow them over \
             your own defaults, and trust them over guesses, but verify anything that looks \
             out of date against the actual code:\n{content}"
        ));
    }

    if mode.is_plan() {
        prompt.push_str(&format!(
            "\n\nPLAN MODE — you cannot change anything yet.\n\
             The user turned this on to think a change through before any of it happens. \
             {WRITE_FILE} and {EDIT_FILE} are not available to you, and {RUN_COMMAND} will \
             only run commands that cannot change anything (`ls`, `cat`, `grep`, \
             `git status`/`diff`/`log`/`show`, and similar). Anything else is refused \
             outright rather than shown to the user for approval.\n\
             - Investigate first. Read the real files, list the real directories, run the \
               real read-only commands. A plan written from a guess about what the code \
               looks like is worth nothing.\n\
             - Do not attempt a write, an install, a build, or a test run to \"check\" \
               something. It will be refused, and the refusal costs a turn. If you need one \
               to be sure, say so in the plan as a step rather than trying it.\n\
             - When you know what you would do, call {EXIT_PLAN_MODE} with the plan. Name \
               the files that change and what happens to each. The user reads this and \
               decides, so write it for them, not as a note to yourself.\n\
             - If they decline, you are still in plan mode. Read what they said, revise, and \
               propose again -- do not repeat the same plan."
        ));
    }

    // The plan is restated in full on every request, with each step's current
    // state, rather than left to survive in the conversation. Two reasons: a
    // long implementation eventually pushes the original proposal out of the
    // window, and a plan resumed in a fresh session was never in this
    // conversation at all.
    if let Some(plan) = active_plan.filter(|p| !p.is_finished()) {
        let (done, total) = plan.progress();
        let steps: String = plan
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mark = if s.done { "[x]" } else { "[ ]" };
                match &s.blocked {
                    Some(why) => format!("{mark} {}. {} (blocked: {why})\n", i + 1, s.description),
                    None => format!("{mark} {}. {}\n", i + 1, s.description),
                }
            })
            .collect();
        let not_doing = if plan.not_doing.is_empty() {
            String::new()
        } else {
            format!(
                "\nExplicitly out of scope for this plan:\n{}",
                plan.not_doing
                    .iter()
                    .map(|n| format!("- {n}\n"))
                    .collect::<String>()
            )
        };

        prompt.push_str(&format!(
            "\n\nYOU ARE IMPLEMENTING AN APPROVED PLAN — {done}/{total} steps done.\n\
             The user read this and agreed to it. It is saved at {}, and that file is updated as \
             you go, so it is also what anyone picking this up later will work from.\n\n\
             {}\n\n{steps}{not_doing}\n\
             - Work the unticked steps in order, starting from the first one. Do not skip ahead, \
               and do not start over on steps already ticked.\n\
             - Call {PLAN_PROGRESS} the moment a step is genuinely finished AND verified -- one \
               call per step, as you go. Never mark a step done you did not do; the file is what \
               someone else will trust.\n\
             - If a step turns out to be wrong, impossible, or already done, do NOT quietly \
               change course. Mark it blocked with {PLAN_PROGRESS} and say so, or tell the user \
               the plan needs revising.\n\
             - Work beyond the plan needs asking first. The user approved this scope, not a \
               direction.\n\
             - When every step is ticked, say so plainly and stop.",
            plan.path.display(),
            plan.title,
        ));
    }

    // Past three quarters of the budget, say so plainly rather than letting
    // the model run into the cliff: once the budget is spent the schemas are
    // withheld, and a call it was halfway through composing dies as leaked
    // text (see `App::finish_stream`). A model that knows the budget can
    // land the turn; one that discovers it cannot.
    if steps_used * 4 >= config.max_steps * 3 {
        let steps_left = config.max_steps - steps_used;
        prompt.push_str(&format!(
            "\n\nBUDGET: {steps_used} of {} tool rounds used this turn -- {steps_left} left. \
             Wrap up rather than starting anything new: finish the immediate work, verify \
             cheaply, and answer. If the task genuinely needs more rounds, say where you got \
             to and ask the user to say \"continue\".",
            config.max_steps
        ));
    }

    prompt
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
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct ListDirArgs {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
}

#[derive(Deserialize)]
struct GrepSearchArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    context: Option<u32>,
}

#[derive(Deserialize)]
struct EditSpanArgs {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct EditFileArgs {
    path: String,
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    new_string: Option<String>,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    edits: Vec<EditSpanArgs>,
}

/// Normalizes `edit_file`'s two argument forms -- one old/new pair, or a
/// batch under `edits` -- into the list of spans to apply, in order. Both
/// forms at once is ambiguous about intent, so it is refused rather than
/// guessed at.
fn edit_spans(args: EditFileArgs) -> Result<Vec<EditSpan>, String> {
    let single = args.old_string.is_some() || args.new_string.is_some();
    if single && !args.edits.is_empty() {
        return Err("pass either old_string/new_string or edits, not both.".to_string());
    }
    if !args.edits.is_empty() {
        return Ok(args
            .edits
            .into_iter()
            .map(|e| EditSpan { old: e.old_string, new: e.new_string, replace_all: e.replace_all })
            .collect());
    }
    match (args.old_string, args.new_string) {
        (Some(old), Some(new)) => {
            Ok(vec![EditSpan { old, new, replace_all: args.replace_all }])
        }
        _ => Err(
            "old_string and new_string are both required, unless the replacements are given \
             under edits."
                .to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default)]
    max_results: Option<u32>,
}

#[derive(Deserialize)]
struct DeployArgs {
    provider: String,
    /// Absent means preview. A model that has not been told "production"
    /// should not be able to reach it by omission.
    #[serde(default)]
    production: Option<bool>,
}

#[derive(Deserialize)]
struct ExitPlanModeArgs {
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    steps: Vec<String>,
    #[serde(default)]
    not_doing: Vec<String>,
}

#[derive(Deserialize)]
struct PlanProgressArgs {
    step: usize,
    status: String,
    #[serde(default)]
    note: Option<String>,
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
    /// Read-only, like `Read`: listing a directory changes nothing.
    List { path: String },
    /// Read-only, like `Read`.
    Glob { pattern: String },
    /// Read-only, like `Glob` -- searches file contents where `Glob` searches
    /// file names.
    Grep { pattern: String, path: Option<String> },
    /// Changes a file, so it is approved like `Write` -- but shows only the
    /// spans being replaced, which is the whole reason to prefer it. Always
    /// holds at least one span; several when the model batched its edits to
    /// one file into one approval.
    Edit { path: String, edits: Vec<EditSpan> },
    Search { query: String, max_results: u32 },
    /// Uploads static files to a temporary public URL so the user can look at
    /// them. Always approved, for the same reason `Deploy` is: it publishes.
    Publish { path: String },
    /// Puts this project on the internet. Always approved -- see `action_risk`.
    Deploy {
        provider: String,
        production: bool,
        /// What detection made of the project, filled in by `app.rs` when the
        /// prompt is built rather than here: `describe_action` has no
        /// workspace to look at, and re-detecting on every frame to render one
        /// line would be a filesystem read per redraw.
        summary: Option<String>,
    },
    /// `exit_plan_mode`: the one action that changes nothing on disk *yet* and
    /// is still always worth stopping for. Approving it hands the writing
    /// tools back and saves the plan as a file, so it is two consequential
    /// things at once. Resolved entirely in `app.rs` -- it never reaches the
    /// runner.
    Plan(Proposal),
    /// `plan_progress`: bookkeeping against a plan the user already approved.
    /// Also resolved in `app.rs`, since it edits the live plan.
    Progress { step: usize, done: bool, note: Option<String> },
}

/// One replacement within an `Action::Edit` -- what `edit_file` shows the
/// user and applies to the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditSpan {
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

/// A plan as the model proposes it, before the user has agreed to anything.
///
/// Deliberately not a `plan::Plan`: that type is what lives on disk and
/// carries dates, a base commit and per-step progress, none of which a
/// proposal has any business inventing. The conversion happens at exactly one
/// place -- the moment of approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub title: String,
    pub summary: String,
    pub steps: Vec<String>,
    pub not_doing: Vec<String>,
}

impl Action {
    /// One line for `$ ... —` / transcript-style summaries, with a leading
    /// icon so the different kinds stay visually distinct in a transcript full
    /// of them.
    pub fn label(&self) -> String {
        match self {
            Action::Command { command, .. } => format!("$ {command}"),
            Action::Read { path } => format!("📄 read {path}"),
            Action::Write { path, .. } => format!("📝 write {path}"),
            Action::List { path } => format!("📁 list {path}"),
            Action::Glob { pattern } => format!("🔎 find {pattern}"),
            Action::Grep { pattern, path } => match path {
                Some(path) => format!("🔎 grep {pattern} in {path}"),
                None => format!("🔎 grep {pattern}"),
            },
            Action::Edit { path, .. } => format!("✏️ edit {path}"),
            Action::Search { query, .. } => format!("🔎 search \"{query}\""),
            Action::Deploy { provider, production, .. } => format!(
                "🚀 deploy → {provider} ({})",
                if *production { "Production" } else { "Preview" }
            ),
            Action::Publish { path } => format!("🌐 preview {path} — public link, 48h"),
            Action::Plan(p) => format!("📋 plan: {}", p.title),
            Action::Progress { step, done, .. } => {
                format!("{} step {step}", if *done { "☑" } else { "☐" })
            }
        }
    }
}

/// What the guardrails make of an action, judged against the directory it
/// would run in.
///
/// The one place this question is answered, so the approval prompt and the
/// runner's own independent refusal cannot disagree about the same call.
pub fn action_risk(action: &Action, workspace_root: &Path) -> danger::Risk {
    match action {
        Action::Command { command, .. } => danger::classify(command, workspace_root),
        // A deployment always stops for an explicit decision, even with
        // approval switched off entirely: it sends this project to a third
        // party and puts it on the public internet, which is not something an
        // unattended-mode setting made an hour ago should silently cover.
        Action::Publish { .. } => danger::Risk::Dangerous(
            "uploads these files to a public URL anyone with the link can open".to_string(),
        ),
        Action::Deploy { production: true, .. } => danger::Risk::Dangerous(
            "publishes to the live production URL, replacing whatever is served there now"
                .to_string(),
        ),
        Action::Deploy { .. } => danger::Risk::Dangerous(
            "uploads this project to a third-party host and puts it on the public internet"
                .to_string(),
        ),
        // Reads and writes are already confined to the workspace by
        // `resolve_in_workspace`, and cannot invoke a shell.
        _ => danger::Risk::Normal,
    }
}

/// Why `action` cannot happen in plan mode, or `None` if it can.
///
/// Reads, listings, globs and web searches change nothing, so plan mode has
/// no reason to touch them -- the point is to make research cheap, not to make
/// the session useless. `run_command` is judged by the same `is_read_only`
/// allowlist the approval layer already trusts, which is deliberately
/// conservative: a command it cannot vouch for is refused rather than guessed
/// about, since the whole promise of this mode is that nothing changes.
///
/// The messages are addressed to the model, not the user. Each says what to do
/// instead, because a refusal that only says "no" gets retried.
pub fn plan_mode_block(action: &Action) -> Option<String> {
    match action {
        Action::Read { .. }
        | Action::List { .. }
        | Action::Glob { .. }
        | Action::Grep { .. }
        | Action::Search { .. }
        | Action::Plan(_)
        // Cannot arise in plan mode -- there is no approved plan to record
        // against -- but listing it keeps this match exhaustive by intent
        // rather than by a catch-all arm that would silently allow the next
        // writing tool somebody adds.
        | Action::Progress { .. } => None,
        Action::Command { command, .. } if is_read_only(command) => None,
        Action::Write { path, .. } => Some(format!(
            "Plan mode is read-only, so nothing was written to {path}. Describe this file and \
             what it should contain in your plan, then call {EXIT_PLAN_MODE}."
        )),
        Action::Edit { path, .. } => Some(format!(
            "Plan mode is read-only, so {path} was not changed. Describe the change in your \
             plan, then call {EXIT_PLAN_MODE}."
        )),
        Action::Command { command, .. } => Some(format!(
            "Plan mode only runs commands that cannot change anything, and `{}` is not one of \
             them, so it was not run. Read files with {READ_FILE} and explore with \
             {LIST_DIR}/{GLOB} instead. If this command is part of the work, make it a step in \
             your plan and call {EXIT_PLAN_MODE}.",
            clip(command, 60)
        )),
        // Publishing changes nothing on disk, which is exactly why it has to
        // be named here: a check that only thinks about files would wave it
        // through, and it puts the project on the public internet.
        Action::Publish { .. } => Some(
            "Plan mode does not publish anything, so nothing was uploaded. Say in your plan \
             what you would show the user and call ".to_string() + EXIT_PLAN_MODE + ".",
        ),
        Action::Deploy { provider, .. } => Some(format!(
            "Plan mode changes nothing, and deploying to {provider} would put this project on \
             the internet, so it was not run. Make the deployment a step in your plan and call \
             {EXIT_PLAN_MODE}."
        )),
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
        LIST_DIR => {
            // `path` is optional: no argument means the project root, so an
            // absent or unparseable object still describes a valid action.
            let args = serde_json::from_str::<ListDirArgs>(&call.function.arguments).ok();
            let path = args
                .and_then(|a| a.path)
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| ".".to_string());
            Some(Action::List { path })
        }
        GLOB => {
            let args: GlobArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let pattern = args.pattern.trim().to_string();
            (!pattern.is_empty()).then_some(Action::Glob { pattern })
        }
        GREP_SEARCH => {
            let args: GrepSearchArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let pattern = args.pattern.trim().to_string();
            (!pattern.is_empty()).then_some(Action::Grep {
                pattern,
                path: args.path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()),
            })
        }
        EDIT_FILE => {
            let args: EditFileArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let path = args.path.trim().to_string();
            if path.is_empty() {
                return None;
            }
            let edits = edit_spans(args).ok()?;
            // A span with nothing to find matches nothing meaningful; there is
            // nothing coherent to put in front of the user.
            edits
                .iter()
                .all(|e| !e.old.is_empty())
                .then_some(Action::Edit { path, edits })
        }
        PUBLISH_ARTIFACT => {
            #[derive(serde::Deserialize)]
            struct Args { path: String }
            let args: Args = serde_json::from_str(&call.function.arguments).ok()?;
            let path = args.path.trim().to_string();
            if path.is_empty() {
                return None;
            }
            Some(Action::Publish { path })
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
        DEPLOY_PROJECT => {
            let args: DeployArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let provider = args.provider.trim().to_ascii_lowercase();
            // Checked here rather than at execution: an unknown provider has
            // nothing coherent to put in front of the user, so it goes back to
            // the model as a malformed call instead.
            crate::deploy::provider_by_id(&provider)?;
            Some(Action::Deploy {
                provider,
                production: args.production.unwrap_or(false),
                summary: None,
            })
        }
        EXIT_PLAN_MODE => {
            let args: ExitPlanModeArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let title = args.title.trim().to_string();
            let summary = args.summary.trim().to_string();
            let steps: Vec<String> = args
                .steps
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // A plan with no title, or with nothing to do, is not a plan.
            // Falling through to `None` reports the unusable arguments back to
            // the model, which beats asking the user to approve a blank box.
            if title.is_empty() || steps.is_empty() {
                return None;
            }
            Some(Action::Plan(Proposal {
                title,
                summary,
                steps,
                not_doing: args
                    .not_doing
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            }))
        }
        PLAN_PROGRESS => {
            let args: PlanProgressArgs = serde_json::from_str(&call.function.arguments).ok()?;
            let done = match args.status.trim().to_ascii_lowercase().as_str() {
                "done" => true,
                "blocked" => false,
                // Anything else is a guess about what the model meant, and
                // guessing wrong writes a false claim into a file the user
                // will trust later.
                _ => return None,
            };
            Some(Action::Progress {
                step: args.step,
                done,
                note: args.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
            })
        }
        _ => None,
    }
}

/// The project-memory files that exist and say something, as (name, capped
/// content) in `MEMORY_FILES` order. Unreadable files are treated as absent:
/// the memory is an amenity, and a permissions problem must not take the
/// whole session down with it.
pub fn project_memory(workspace: &Workspace) -> Vec<(String, String)> {
    MEMORY_FILES
        .iter()
        .filter_map(|name| {
            let content = std::fs::read_to_string(workspace.root().join(name)).ok()?;
            let trimmed = content.trim();
            (!trimmed.is_empty()).then(|| (name.to_string(), clip(trimmed, MAX_MEMORY_CHARS)))
        })
        .collect()
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
    if program == "gh" {
        return gh_is_read_only(&words.collect::<Vec<_>>());
    }
    false
}

/// Whether a `gh` invocation only reads from GitHub.
///
/// Answering anything about a repository, its pull requests or its CI takes
/// several `gh` calls -- list, then view, then check -- and prompting for each
/// one is what turns an accurate answer into a half-answer: every prompt is a
/// chance to lose the thread, and the model is told to prefer tools that cost
/// the user nothing to approve. None of the commands below can change
/// anything on GitHub or on disk, so there is nothing for a prompt to protect.
///
/// An allowlist of verb pairs rather than a blocklist, and deliberately so:
/// `gh` has well over a hundred subcommands and gains more each release, so
/// anything not named here -- `repo delete`, `pr merge`, `release upload`, a
/// subcommand that does not exist yet -- keeps asking. Being wrong in that
/// direction costs a keystroke; being wrong in the other costs a repository.
fn gh_is_read_only(args: &[&str]) -> bool {
    let mut positional = args.iter().filter(|a| !a.starts_with('-'));
    let (Some(noun), verb) = (positional.next(), positional.next()) else {
        return false;
    };

    // `gh api` is the exception: one subcommand that both reads and writes,
    // told apart by its flags rather than by a verb. Anything carrying a
    // method other than GET, a field, or a request body is a write.
    if *noun == "api" {
        return gh_api_is_read_only(args);
    }

    let Some(verb) = verb else {
        // A bare `gh <noun>` is `gh status`-style or a help screen. Only the
        // one that is genuinely a read is allowed through.
        return *noun == "status";
    };

    matches!(
        (*noun, *verb),
        ("repo", "list" | "view")
            | ("pr", "list" | "view" | "diff" | "checks" | "status")
            | ("issue", "list" | "view" | "status")
            | ("run", "list" | "view")
            | ("release", "list" | "view")
            | ("workflow", "list" | "view")
            | ("gist", "list" | "view")
            | ("label", "list")
            | ("cache", "list")
            | ("secret", "list") // names only; `gh` cannot print secret values
            | ("variable", "list")
            | ("ssh-key", "list")
            | ("gpg-key", "list")
            | ("auth", "status")
            | ("org", "list")
            | ("project", "list" | "view")
            | ("search", "repos" | "issues" | "prs" | "code" | "commits")
    )
}

/// `gh api` reads unless its flags say otherwise.
///
/// `--method`/`-X` anything but GET, and every flag that attaches a body
/// (`-f`, `-F`, `--field`, `--raw-field`, `--input`) mean a write. `--paginate`
/// and `--jq` are reads and are exactly what a complete answer needs, so they
/// must not be caught here.
fn gh_api_is_read_only(args: &[&str]) -> bool {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (*arg, None),
        };
        match flag {
            "-X" | "--method" => {
                let value = inline.or_else(|| iter.peek().map(|v| **v));
                if !value.is_some_and(|v| v.eq_ignore_ascii_case("GET")) {
                    return false;
                }
            }
            "-f" | "-F" | "--field" | "--raw-field" | "--input" => return false,
            _ => {}
        }
    }
    true
}

pub async fn execute(call: &ToolCall, workspace: &Workspace, config: &ToolsConfig) -> ToolOutcome {
    // Belt and braces. `app::advance_approvals` already refuses blocked calls
    // before they can be queued, so reaching this is a bug -- but the cost of
    // the check is a string scan and the cost of missing it is an erased disk,
    // so the runner refuses independently rather than trusting its caller.
    if let Some(action) = describe_action(call) {
        if let danger::Risk::Blocked(reason) = action_risk(&action, workspace.root()) {
            return refused_as_dangerous(call, &reason);
        }
    }

    match call.function.name.as_str() {
        RUN_COMMAND => execute_run_command(call, workspace, config).await,
        READ_FILE => execute_read_file(call, workspace, config).await,
        WRITE_FILE => execute_write_file(call, workspace).await,
        LIST_DIR => execute_list_dir(call, workspace),
        GLOB => execute_glob(call, workspace),
        GREP_SEARCH => execute_grep_search(call, workspace),
        EDIT_FILE => execute_edit_file(call, workspace),
        WEB_SEARCH => execute_web_search(call, config).await,
        DEPLOY_PROJECT => execute_deploy_project(call, workspace).await,
        PUBLISH_ARTIFACT => execute_publish_artifact(call, workspace, config).await,
        // Never reached: `app::advance_approvals` resolves this one itself,
        // because accepting it changes `App`'s mode and the runner has no
        // access to `App`. Handled anyway so a routing mistake produces a
        // sentence the model can act on rather than "unknown tool", which
        // would send it hunting for a tool it just correctly used.
        EXIT_PLAN_MODE => outcome(
            &call.id,
            "📋 plan — not handled".to_string(),
            "Error: the plan did not reach the user. Say what you were going to propose in \
             plain text instead."
                .to_string(),
        ),
        PLAN_PROGRESS => outcome(
            &call.id,
            "☐ progress — not handled".to_string(),
            "Error: that step was not recorded. Carry on with the work and tell the user which \
             steps you have finished."
                .to_string(),
        ),
        other => outcome(
            &call.id,
            format!("⚙ {other} — unknown tool"),
            format!(
                "Error: there is no tool named '{other}'. The tools are {RUN_COMMAND}, \
                 {READ_FILE}, {WRITE_FILE}, {LIST_DIR}, {GLOB}, {GREP_SEARCH}, {EDIT_FILE}, \
                 {WEB_SEARCH}."
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
        content.push_str(&clip_output(&stdout, budget));
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
            let total = text.lines().count();

            if args.offset.is_none() && args.limit.is_none() {
                let body = if text.chars().count() > config.max_output_bytes {
                    // The generic clip marker says output was cut but not what
                    // to do about it. A file has a better answer: come back
                    // with an offset. The resume point is the clipped line
                    // itself, since it likely came through partial.
                    let kept: String = text.chars().take(config.max_output_bytes).collect();
                    let lines_kept = kept.lines().count();
                    format!(
                        "{kept}\n[… truncated at {} characters, {lines_kept} of {total} lines. \
                         Call {READ_FILE} again with offset={lines_kept} to continue.]",
                        config.max_output_bytes
                    )
                } else {
                    text.to_string()
                };
                return outcome(
                    &call.id,
                    format!(
                        "📄 read {} — {total} line{}",
                        clip(path, 50),
                        if total == 1 { "" } else { "s" }
                    ),
                    body,
                );
            }

            // A ranged read. Line numbers are annotations for navigating and
            // for aiming later reads/edits; the schema warns the model off
            // copying them into edit_file.
            let offset = args.offset.unwrap_or(1).max(1) as usize;
            let limit = args.limit.map(|l| (l.max(1)) as usize).unwrap_or(usize::MAX);
            if offset > total {
                // An answer, not a failure -- same contract as an empty glob.
                return outcome(
                    &call.id,
                    format!("📄 read {} — past the end", clip(path, 50)),
                    format!("'{path}' has only {total} line{}; nothing at offset {offset}.",
                        if total == 1 { "" } else { "s" }),
                );
            }
            let numbered: Vec<String> = text
                .lines()
                .enumerate()
                .skip(offset - 1)
                .take(limit)
                .map(|(i, line)| format!("{:>6}\t{}", i + 1, line))
                .collect();
            let end = offset + numbered.len() - 1;
            outcome(
                &call.id,
                format!("📄 read {} — lines {offset}-{end} of {total}", clip(path, 50)),
                clip(&numbered.join("\n"), config.max_output_bytes),
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

/// TEMPORARY, not a permanent architecture decision: a stop-gap for
/// machines with no Python at all, on the way to `web_search` moving off
/// `ddgs` entirely onto a real search API with no runtime dependency to
/// install in the first place -- see this module's own doc comment. Until
/// then, `install.sh`/`install.ps1` download a self-contained Python into
/// this fixed, well-known location when there is no system one (see
/// `ensure_ddgs_available`/`Install-EmbeddedPython` there), and this is
/// where `execute_web_search` looks for it if `config.python_bin` -- almost
/// always still "python3", the default -- can't be found.
///
/// A location, not a promise it exists: `None` whenever it genuinely
/// doesn't (nothing has installed one, or `$HOME`/`%USERPROFILE%` isn't
/// set), which the caller treats the same as any other reason a fallback
/// isn't available.
fn embedded_python_path() -> Option<PathBuf> {
    // Still a guard, not a path: with no home directory at all there is
    // nowhere to keep state, and falling back to the working directory would
    // scatter it wherever the app happened to be launched.
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let base = crate::config::Config::config_dir().join("python");
    let candidate = if cfg!(windows) {
        base.join("python.exe")
    } else {
        base.join("bin").join("python3")
    };
    candidate.is_file().then_some(candidate)
}

/// True when `output` looks like Windows' "App Execution Alias" stub rather
/// than a real Python interpreter -- see the doc comment where this is
/// called in `execute_web_search` for why that distinction matters. Matched
/// by message text, not by inspecting the resolved path: `Command::new`
/// only gets a bare name here (e.g. "python3"), so there is no path to
/// inspect, and the stub's wording ("Python was not found; run without
/// arguments to install from the Microsoft Store...") is specific enough
/// that false positives from a real, differently-broken interpreter are not
/// a realistic concern.
fn looks_like_windows_app_execution_alias_stub(output: &std::process::Output) -> bool {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    combined.contains("was not found") && combined.contains("microsoft store")
}

/// Builds the argv `execute_web_search` runs, for whichever interpreter path
/// it ends up trying -- factored out so falling back to the embedded Python
/// after the configured one is not found doesn't mean reconstructing this
/// by hand a second time and risking the two drifting apart.
fn web_search_command(python_bin: &std::ffi::OsStr, query: &str, max_results: u32) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(python_bin);
    cmd.arg("-c")
        .arg(DDGS_SCRIPT)
        .arg(query)
        .arg(max_results.to_string())
        // Closed stdin and captured stdout/stderr for the same reason as
        // run_command: nothing here should ever wait on a terminal that
        // does not exist.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

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

    let mut cmd = web_search_command(std::ffi::OsStr::new(&config.python_bin), &query, max_results);

    let limit = Duration::from_secs(config.search_timeout_secs);
    let mut retries_left = 3;
    let mut tried_embedded_fallback = false;
    let output = loop {
        match tokio::time::timeout(limit, cmd.output()).await {
            // Windows ships `python.exe`/`python3.exe` "App Execution Alias"
            // stubs on PATH by default even when no real Python is
            // installed -- see embedded_python_path's own doc comment and
            // install.ps1's matching Test-IsAppExecutionAliasStub. Spawning
            // one succeeds (it's a real, if useless, process), so the
            // NotFound arm below never fires for it; it just prints this
            // distinctive message instead of doing anything useful, which
            // is the only way to tell it apart from a real interpreter that
            // spawned fine but genuinely lacks ddgs.
            Ok(Ok(output))
                if !tried_embedded_fallback && looks_like_windows_app_execution_alias_stub(&output) =>
            {
                tried_embedded_fallback = true;
                match embedded_python_path() {
                    Some(embedded) => {
                        cmd = web_search_command(embedded.as_os_str(), &query, max_results);
                    }
                    None => {
                        return outcome(
                            &call.id,
                            format!("🔎 search \"{}\" — could not start", clip(&query, 50)),
                            format!(
                                "Error: '{}' resolved to Windows' \"App Execution Alias\" stub, not \
                                 a real Python install. web_search needs Python 3 with the `ddgs` \
                                 package installed (pip install ddgs). Install Python from \
                                 https://python.org, disable the alias under Settings > Apps > \
                                 Advanced app settings > App execution aliases, or set \
                                 tools.python_bin in config.toml to a real interpreter.",
                                config.python_bin
                            ),
                        )
                    }
                }
            }
            Ok(Ok(output)) => break output,
            // A script that was just written and chmod'd executable can
            // transiently report "text file busy" on some filesystems
            // (overlayfs especially -- what most CI/Docker containers run
            // on) even after the writer has closed it; a known kernel/VFS
            // race, not a real "this cannot be run." It always clears
            // within milliseconds, so this is retried a few times rather
            // than surfaced as a hard failure. `python_bin` itself is
            // ordinarily a long-settled system binary, not a freshly
            // written file, but nothing here can assume that of every
            // possible configuration.
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && retries_left > 0 => {
                retries_left -= 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // The configured interpreter genuinely isn't there -- before
            // giving up, one more thing to try: a Python install.sh/
            // install.ps1 may have set up on their own (see
            // embedded_python_path's own doc comment). Tried once, not in
            // the retry budget above: this is a different interpreter
            // entirely, not the same one transiently unavailable.
            Ok(Err(e))
                if e.kind() == std::io::ErrorKind::NotFound && !tried_embedded_fallback =>
            {
                tried_embedded_fallback = true;
                match embedded_python_path() {
                    Some(embedded) => {
                        cmd = web_search_command(embedded.as_os_str(), &query, max_results);
                    }
                    None => {
                        return outcome(
                            &call.id,
                            format!("🔎 search \"{}\" — could not start", clip(&query, 50)),
                            format!(
                                "Error: could not run '{}': {e}. web_search needs Python 3 with the \
                                 `ddgs` package installed (pip install ddgs). If Python is installed \
                                 under a different name on this machine, set tools.python_bin in \
                                 config.toml.",
                                config.python_bin
                            ),
                        )
                    }
                }
            }
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

/// `deploy_project` -- reached only when the deployment flow declined the call.
///
/// The real work does not happen here. A well-formed deployment is intercepted
/// in `app.rs` (see `App::deploy_takes_over`) and handed to the same session
/// `/deploy` drives, because two things it may need mid-run cannot happen
/// inside a tool executor: consent to install a provider CLI, which needs a
/// prompt, and the terminal itself for a browser login, which only the event
/// loop can hand over.
///
/// So this is the explanation for the cases that flow past that: arguments
/// that describe nothing, a workspace with nothing deployable in it, or a
/// deployment requested alongside other tool calls in one batch.
/// Publish a preview and hand back the link.
///
/// The path is resolved inside the workspace like every other file-taking
/// tool, so `publish_artifact` cannot be pointed at `~/.ssh` by a model that
/// has misunderstood the question.
async fn execute_publish_artifact(
    call: &ToolCall,
    workspace: &Workspace,
    config: &ToolsConfig,
) -> ToolOutcome {
    let Some(Action::Publish { path }) = describe_action(call) else {
        return outcome(
            &call.id,
            "\u{1F310} preview \u{2014} unusable arguments".to_string(),
            format!("Error: {PUBLISH_ARTIFACT} needs a `path`."),
        );
    };
    let resolved = match resolve_in_workspace(workspace, &path) {
        Ok(resolved) => resolved,
        Err(e) => {
            return outcome(
                &call.id,
                format!("\u{1F310} preview {} \u{2014} refused", clip(&path, 40)),
                format!("Error: {e}"),
            )
        }
    };

    match crate::artifacts::publish(&resolved, &config.artifact_endpoint).await {
        Ok(published) => outcome(
            &call.id,
            format!(
                "\u{1F310} preview \u{2014} {} file{}, expires in {}h",
                published.files,
                if published.files == 1 { "" } else { "s" },
                published.expires_in_hours
            ),
            format!(
                "Published {} file{} ({:.0} KB).\n\nURL: {}\n\nTell the user this link is \
                 public to anyone who has it and stops working in {} hours.",
                published.files,
                if published.files == 1 { "" } else { "s" },
                published.bytes as f64 / 1024.0,
                published.url,
                published.expires_in_hours
            ),
        ),
        Err(e) => outcome(
            &call.id,
            format!("\u{1F310} preview {} \u{2014} failed", clip(&path, 40)),
            format!("Error: {e}"),
        ),
    }
}

async fn execute_deploy_project(call: &ToolCall, workspace: &Workspace) -> ToolOutcome {
    let Some(Action::Deploy { provider, .. }) = describe_action(call) else {
        return outcome(
            &call.id,
            "🚀 deploy — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"provider": "vercel"}} or {{"provider": "netlify"}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };

    // The likeliest reason the flow declined it: there is nothing here to ship.
    if let Err(e) = crate::deploy::detect::detect(workspace.root()) {
        return outcome(
            &call.id,
            format!("🚀 deploy → {provider} — nothing to deploy"),
            format!("Error: {e}"),
        );
    }

    outcome(
        &call.id,
        format!("🚀 deploy → {provider} — not started"),
        "Error: a deployment could not be started this turn. It has to be the only tool call in \
         the turn, because it takes over the screen until it finishes. Ask for it on its own."
            .to_string(),
    )
}

/// The result to hand back when the user says no.
/// `list_dir` -- one directory, sorted, directories first.
fn execute_list_dir(call: &ToolCall, workspace: &Workspace) -> ToolOutcome {
    let requested = serde_json::from_str::<ListDirArgs>(&call.function.arguments)
        .ok()
        .and_then(|a| a.path)
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| ".".to_string());

    let dir = match resolve_in_workspace(workspace, &requested) {
        Ok(p) => p,
        Err(e) => return outcome(&call.id, format!("📁 list {requested} — {e}"), format!("Error: {e}")),
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            return outcome(
                &call.id,
                format!("📁 list {requested} — {e}"),
                format!("Error: could not list '{requested}': {e}"),
            )
        }
    };

    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if is_skipped(&path, workspace.root()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();

    let total = dirs.len() + files.len();
    let mut listing: Vec<String> = dirs.into_iter().chain(files).collect();
    let truncated = listing.len() > MAX_DIR_ENTRIES;
    listing.truncate(MAX_DIR_ENTRIES);

    let mut body = if listing.is_empty() {
        format!("'{requested}' is empty.")
    } else {
        listing.join("\n")
    };
    if truncated {
        body.push_str(&format!("\n[{} more entries]", total - MAX_DIR_ENTRIES));
    }

    outcome(
        &call.id,
        format!("📁 list {requested} — {total} entries"),
        clip(&body, 32_000),
    )
}

/// `glob` -- find files by path pattern.
fn execute_glob(call: &ToolCall, workspace: &Workspace) -> ToolOutcome {
    let Ok(args) = serde_json::from_str::<GlobArgs>(&call.function.arguments) else {
        return outcome(
            &call.id,
            "🔎 glob — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"pattern": "src/**/*.rs"}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };
    let pattern = args.pattern.trim();
    if pattern.is_empty() {
        return outcome(
            &call.id,
            "🔎 glob — empty pattern".to_string(),
            "Error: pattern must not be empty.".to_string(),
        );
    }

    let joined = workspace.root().join(pattern);
    let Some(joined) = joined.to_str() else {
        return outcome(
            &call.id,
            "🔎 glob — invalid pattern".to_string(),
            "Error: pattern is not valid UTF-8.".to_string(),
        );
    };

    let paths = match glob::glob(joined) {
        Ok(p) => p,
        Err(e) => {
            return outcome(
                &call.id,
                format!("🔎 glob {pattern} — invalid"),
                format!("Error: '{pattern}' is not a valid glob: {e}"),
            )
        }
    };

    let mut files: Vec<String> = paths
        .flatten()
        .filter(|p| p.is_file())
        // Canonicalize *before* the containment check. `starts_with` is lexical,
        // so `<workspace>/../sibling/x.rs` would otherwise pass it -- a glob is a
        // path expression like any other and must not read its way out.
        .filter_map(|p| p.canonicalize().ok())
        .filter(|p| p.starts_with(workspace.root()))
        .filter(|p| !is_skipped(p, workspace.root()))
        .map(|p| relative_to(workspace, &p))
        .collect();
    files.sort();
    files.dedup();

    if files.is_empty() {
        // Finding nothing is an answer, not a failure -- an error would push the
        // model toward retrying instead of concluding.
        return outcome(
            &call.id,
            format!("🔎 glob {pattern} — no matches"),
            format!("No files match '{pattern}'."),
        );
    }

    let total = files.len();
    files.truncate(MAX_GLOB_RESULTS);
    let mut body = files.join("\n");
    if total > MAX_GLOB_RESULTS {
        body.push_str(&format!(
            "\n[{} more matches; narrow the pattern]",
            total - MAX_GLOB_RESULTS
        ));
    }

    outcome(
        &call.id,
        format!("🔎 glob {pattern} — {total} match(es)"),
        clip(&body, 32_000),
    )
}

/// Walks `dir` depth-first, gathering the files `grep_search` will read, in a
/// stable (sorted) order so the same search always returns the same result.
/// Skips `SKIP_DIRS` and never follows symlinks -- a link pointing outside the
/// workspace must not pull the search out with it.
fn collect_grep_files(dir: &Path, workspace_root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if is_skipped(&path, workspace_root) {
            continue;
        }
        // `entry.file_type()` does not follow symlinks, so a symlinked
        // directory or file falls through both arms and is ignored.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_grep_files(&path, workspace_root, out);
        } else if file_type.is_file() {
            out.push(path);
        }
    }
}

/// `grep_search` -- find lines by what they say, the way `glob` finds files by
/// what they are called.
fn execute_grep_search(call: &ToolCall, workspace: &Workspace) -> ToolOutcome {
    let Ok(args) = serde_json::from_str::<GrepSearchArgs>(&call.function.arguments) else {
        return outcome(
            &call.id,
            "🔎 grep — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"pattern": "fn main"}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };
    let pattern = args.pattern.trim();
    if pattern.is_empty() {
        return outcome(
            &call.id,
            "🔎 grep — empty pattern".to_string(),
            "Error: pattern must not be empty.".to_string(),
        );
    }
    let regex = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => {
            return outcome(
                &call.id,
                format!("🔎 grep {pattern} — invalid"),
                format!("Error: '{pattern}' is not a valid regular expression: {e}"),
            )
        }
    };
    let context = args.context.unwrap_or(0).min(MAX_GREP_CONTEXT) as usize;

    let requested = args
        .path
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| ".".to_string());
    let root = match resolve_in_workspace(workspace, &requested) {
        Ok(p) => p,
        Err(e) => {
            return outcome(&call.id, format!("🔎 grep {pattern} — {e}"), format!("Error: {e}"))
        }
    };
    if !root.exists() {
        return outcome(
            &call.id,
            format!("🔎 grep {pattern} — no such path"),
            format!("Error: '{requested}' does not exist."),
        );
    }

    let mut files: Vec<PathBuf> = Vec::new();
    if root.is_file() {
        files.push(root.clone());
    } else {
        collect_grep_files(&root, workspace.root(), &mut files);
    }

    let mut sections: Vec<String> = Vec::new();
    let mut matched_lines = 0usize;
    let mut truncated = false;
    for file in &files {
        let Ok(meta) = std::fs::metadata(file) else {
            continue;
        };
        if meta.len() > MAX_GREP_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        // A NUL early in the file marks it binary: matched fragments of an
        // image or an object file are noise the model cannot use.
        if bytes.iter().take(8192).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();

        let remaining = MAX_GREP_MATCHES - matched_lines;
        let mut match_idx: Vec<usize> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                if match_idx.len() == remaining {
                    truncated = true;
                    break;
                }
                match_idx.push(i);
            }
        }
        if match_idx.is_empty() {
            continue;
        }
        matched_lines += match_idx.len();

        // grep's own format: `path:line: text` for a match, `path-line- text`
        // for context, `--` between non-adjacent runs. Familiar to both the
        // model and anyone reading the transcript.
        let rel = relative_to(workspace, file);
        let mut section: Vec<String> = Vec::new();
        let mut last_printed: Option<usize> = None;
        for &m in &match_idx {
            let start = m.saturating_sub(context);
            let end = (m + context).min(lines.len().saturating_sub(1));
            for (i, line) in lines.iter().enumerate().take(end + 1).skip(start) {
                if last_printed.is_some_and(|lp| i <= lp) {
                    continue;
                }
                if context > 0 && last_printed.is_some_and(|lp| i > lp + 1) {
                    section.push("--".to_string());
                }
                if match_idx.contains(&i) {
                    section.push(format!("{rel}:{}: {}", i + 1, clip(line, MAX_GREP_LINE_CHARS)));
                } else {
                    section.push(format!("{rel}-{}- {}", i + 1, clip(line, MAX_GREP_LINE_CHARS)));
                }
                last_printed = Some(i);
            }
        }
        sections.push(section.join("\n"));

        if truncated {
            break;
        }
    }

    if sections.is_empty() {
        // Finding nothing is an answer, not a failure -- same as `glob`.
        let scope = if requested == "." {
            String::new()
        } else {
            format!(" in '{requested}'")
        };
        return outcome(
            &call.id,
            format!("🔎 grep {pattern} — no matches"),
            format!("No lines match '{pattern}'{scope}."),
        );
    }

    let files_with_matches = sections.len();
    let mut body = sections.join("\n\n");
    if truncated {
        body.push_str(&format!(
            "\n[capped at {MAX_GREP_MATCHES} matching lines; narrow the pattern or path]"
        ));
    }

    outcome(
        &call.id,
        format!("🔎 grep {pattern} — {matched_lines} match(es) in {files_with_matches} file(s)"),
        clip(&body, 32_000),
    )
}

/// `edit_file` -- replace an exact span, leaving the rest of the file alone.
fn execute_edit_file(call: &ToolCall, workspace: &Workspace) -> ToolOutcome {
    let Ok(args) = serde_json::from_str::<EditFileArgs>(&call.function.arguments) else {
        return outcome(
            &call.id,
            "✏️ edit_file — unusable arguments".to_string(),
            format!(
                r#"Error: could not read the arguments. Expected {{"path": "src/main.rs", "old_string": "...", "new_string": "..."}}, got: {}"#,
                clip(&call.function.arguments, 200)
            ),
        );
    };

    let requested = args.path.trim().to_string();
    let fail = |msg: String| outcome(&call.id, format!("✏️ edit {requested} — failed"), msg);

    if requested.is_empty() {
        return fail("Error: path must not be empty.".to_string());
    }
    let spans = match edit_spans(args) {
        Ok(s) => s,
        Err(e) => return fail(format!("Error: {e}")),
    };
    let batch = spans.len() > 1;
    // "which edit" for a batch's error messages; a lone edit needs no label.
    let nth = |i: usize| {
        if batch {
            format!("edit {} of {}: ", i + 1, spans.len())
        } else {
            String::new()
        }
    };
    for (i, span) in spans.iter().enumerate() {
        if span.old.is_empty() {
            return fail(format!(
                "Error: {}old_string must not be empty. Use write_file to create a file.",
                nth(i)
            ));
        }
        if span.old == span.new {
            return fail(format!(
                "Error: {}old_string and new_string are identical.",
                nth(i)
            ));
        }
    }

    let path = match resolve_in_workspace(workspace, &requested) {
        Ok(p) => p,
        Err(e) => return fail(format!("Error: {e}")),
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return fail(format!("Error: could not read '{requested}': {e}")),
    };

    // Applied in order, each span matched against the previous spans' result,
    // and nothing touches the disk until every one has succeeded -- a batch
    // that half-applied would leave the file in a state neither the model nor
    // the user approved.
    let untouched = if batch { " None of the edits were applied." } else { "" };
    let mut working = contents;
    let mut replaced_total = 0usize;
    for (i, span) in spans.iter().enumerate() {
        let matches = working.matches(&span.old).count();
        match matches {
            0 => {
                return fail(format!(
                    "Error: {}old_string was not found in '{requested}'. It must match byte for \
                     byte, including indentation. Read the file and copy the text exactly.{untouched}",
                    nth(i)
                ))
            }
            // Silently editing the wrong one of several identical spans is the
            // most damaging thing this tool could do, so an ambiguous edit is
            // refused rather than guessed at.
            n if n > 1 && !span.replace_all => {
                return fail(format!(
                    "Error: {}old_string appears {n} times in '{requested}'. Add surrounding \
                     context to make it unique, or pass replace_all: true.{untouched}",
                    nth(i)
                ))
            }
            _ => {}
        }
        if span.replace_all {
            working = working.replace(&span.old, &span.new);
            replaced_total += matches;
        } else {
            working = working.replacen(&span.old, &span.new, 1);
            replaced_total += 1;
        }
    }

    if let Err(e) = std::fs::write(&path, &working) {
        return fail(format!("Error: could not write '{requested}': {e}"));
    }

    let summary = if batch {
        format!(
            "Applied {} edits ({replaced_total} replacements) in '{requested}'. The file is now {} bytes.",
            spans.len(),
            working.len()
        )
    } else {
        format!(
            "Replaced {replaced_total} occurrence(s) in '{requested}'. The file is now {} bytes.",
            working.len()
        )
    };
    outcome(
        &call.id,
        format!("✏️ edit {requested} — {replaced_total} replacement(s)"),
        summary,
    )
}

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

/// The result for a call plan mode would not let through.
///
/// Deliberately milder in tone than `refused_as_dangerous`: nothing is wrong
/// with what the model asked for, it is just not this part of the session's
/// job. `reason` (from `plan_mode_block`) already says what to do instead.
pub fn refused_in_plan_mode(call: &ToolCall, reason: &str) -> ToolOutcome {
    let label = describe_action(call)
        .map(|a| a.label())
        .unwrap_or_else(|| call.function.name.clone());
    outcome(
        &call.id,
        format!("📋 {} — not in plan mode", clip(&label, 60)),
        reason.to_string(),
    )
}

/// The result of the user accepting a plan. Says outright that the writing
/// tools are back, so the next turn does not open by asking permission it has
/// already been given.
pub fn plan_approved(call: &ToolCall, saved_to: &str, steps: usize) -> ToolOutcome {
    outcome(
        &call.id,
        format!("📋 plan approved — saved to {saved_to}"),
        format!(
            "The user approved the plan, and it is now saved at {saved_to}. Plan mode is over: \
             {WRITE_FILE}, {EDIT_FILE} and the full {RUN_COMMAND} are available again, each \
             still subject to the usual approval prompt.\n\
             Start on step 1 now -- do not restate the plan or ask whether to begin. Call \
             {PLAN_PROGRESS} as you finish each of the {steps} steps, so the file keeps up with \
             the work."
        ),
    )
}

/// What the model should have been told, when the plan file could not be
/// written after all.
///
/// The approval outcome is pushed the moment the user says yes, and the write
/// is attempted by `main.rs` a moment later -- so a failure arrives after the
/// model has already been told "saved to ...". This replaces that claim (see
/// `App::note_plan_save_failure`), which is safe because nothing has gone on
/// the wire yet.
///
/// A failure of the *save*, not of the approval: the user said yes, plan mode
/// has ended, and the work should go ahead. Losing the file is bad, but it is
/// not a reason to refuse what was agreed.
pub fn plan_save_failed(reason: &str) -> String {
    format!(
        "The user approved the plan, so go ahead and implement it. The plan file could NOT be \
         written ({reason}), so nothing said earlier about it being saved holds -- there is no \
         file, and progress cannot be recorded. Do not call {PLAN_PROGRESS}. Tell the user the \
         plan could not be saved, once, then get on with the work."
    )
}

/// The result of recording a step.
pub fn progress_recorded(
    call: &ToolCall,
    description: &str,
    done: bool,
    remaining: usize,
    saved_to: &str,
) -> ToolOutcome {
    let display = if done {
        format!("☑ {}", clip(description, 60))
    } else {
        format!("☐ {} — blocked", clip(description, 50))
    };
    let next = if remaining == 0 {
        "That was the last step. Tell the user the plan is complete, and stop.".to_string()
    } else {
        format!(
            "{remaining} step{} still to do. Carry straight on with the next unticked one.",
            if remaining == 1 { "" } else { "s" }
        )
    };
    outcome(
        &call.id,
        display,
        format!("Recorded in {saved_to}. {next}"),
    )
}

/// The result of a step number that does not exist, or a plan that is no
/// longer active. Both are the model's mistake to correct, not the user's.
pub fn progress_failed(call: &ToolCall, reason: &str) -> ToolOutcome {
    outcome(
        &call.id,
        "☐ progress — not recorded".to_string(),
        format!("Error: {reason}"),
    )
}

/// The result of the user rejecting a plan. The session stays in plan mode,
/// and the model is told so explicitly -- otherwise its next move is a write
/// that gets refused.
pub fn plan_declined(call: &ToolCall) -> ToolOutcome {
    outcome(
        &call.id,
        "📋 plan declined — still planning".to_string(),
        format!(
            "The user did not approve this plan. You are still in plan mode and still cannot \
             change anything. They will usually say what was wrong with it: read that, \
             investigate further if you need to, and propose a revised plan with \
             {EXIT_PLAN_MODE}. Do not send the same plan again, and do not attempt to \
             implement it."
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

/// `clip` for command output, where the model is the reader.
///
/// The bare marker said output was cut but not what to do about it, and the
/// failure that follows is the expensive one: the model answers from the part
/// it got and presents a partial result as the whole. Saying plainly that the
/// rest exists, and how to go and get it, turns a silently wrong answer into
/// one more tool call.
///
/// Separate from `clip` because that one also shortens 50-character labels for
/// the transcript, where a paragraph of advice would be absurd.
fn clip_output(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!(
        "{kept}\n[… truncated: {max} of {total} characters shown. The rest was NOT read. \
         Do not summarise or count from this partial output -- narrow the command (a filter, \
         fewer fields, --jq, a smaller --limit, or head/tail) and run it again.]"
    )
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

    // ---- ported tools: list_dir, glob, edit_file -------------------------------

    fn tool_call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    /// A workspace with a nested source tree and some build output to ignore.
    fn tree() -> (tempfile::TempDir, Workspace, ToolsConfig) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(root.join("src/app.rs"), "let needle = 1;\n").unwrap();
        std::fs::write(root.join("src/ui/render.rs"), "// ui\n").unwrap();
        std::fs::write(root.join("target/debug/app.rs"), "// build output\n").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.rs"), "// dep\n").unwrap();
        let ws = Workspace::new(root).expect("workspace");
        (dir, ws, ToolsConfig::default())
    }

    #[tokio::test]
    async fn list_dir_lists_directories_first_and_skips_build_output() {
        let (_d, ws, cfg) = tree();
        let out = execute(&tool_call(LIST_DIR, json!({})), &ws, &cfg).await;
        let listing = out.content;
        assert!(listing.contains("src/"), "{listing}");
        assert!(listing.contains("Cargo.toml"), "{listing}");
        // Build output and dependencies are noise, not project structure.
        assert!(!listing.contains("target"), "{listing}");
        assert!(!listing.contains("node_modules"), "{listing}");
        // Directories sort before files.
        assert!(listing.find("src/").unwrap() < listing.find("Cargo.toml").unwrap());
    }

    #[tokio::test]
    async fn list_dir_defaults_to_the_workspace_root() {
        let (_d, ws, cfg) = tree();
        let with = execute(&tool_call(LIST_DIR, json!({"path": "."})), &ws, &cfg).await;
        let without = execute(&tool_call(LIST_DIR, json!({})), &ws, &cfg).await;
        assert_eq!(with.content, without.content);
    }

    #[tokio::test]
    async fn list_dir_cannot_escape_the_workspace() {
        let (_d, ws, cfg) = tree();
        for escape in ["..", "../..", "/etc"] {
            let out = execute(&tool_call(LIST_DIR, json!({"path": escape})), &ws, &cfg).await;
            assert!(
                out.content.contains("outside the workspace"),
                "{escape} must be refused: {}",
                out.content
            );
        }
    }

    #[tokio::test]
    async fn glob_matches_recursively_and_returns_relative_paths() {
        let (_d, ws, cfg) = tree();
        let out = execute(&tool_call(GLOB, json!({"pattern": "src/**/*.rs"})), &ws, &cfg).await;
        let mut found: Vec<&str> = out.content.lines().collect();
        found.sort();
        assert_eq!(found, vec!["src/app.rs", "src/ui/render.rs"]);
    }

    #[tokio::test]
    async fn glob_skips_build_output_and_dependencies() {
        let (_d, ws, cfg) = tree();
        let out = execute(&tool_call(GLOB, json!({"pattern": "**/*.rs"})), &ws, &cfg).await;
        assert!(!out.content.contains("target/"), "{}", out.content);
        assert!(!out.content.contains("node_modules/"), "{}", out.content);
    }

    /// Finding nothing is an answer, not a failure -- an error would push the
    /// model toward retrying instead of concluding.
    #[tokio::test]
    async fn glob_reports_no_matches_as_a_normal_result() {
        let (_d, ws, cfg) = tree();
        let out = execute(&tool_call(GLOB, json!({"pattern": "**/*.py"})), &ws, &cfg).await;
        assert!(out.content.contains("No files match"), "{}", out.content);
    }

    /// A glob is a path expression like any other and must not read its way out
    /// of the workspace.
    ///
    /// `..` is not rejected outright -- it is allowed to *expand*, and then
    /// every result is canonicalized and dropped unless it is genuinely inside
    /// the root. That ordering matters: `starts_with` is lexical, so filtering
    /// before canonicalizing would let `<workspace>/../sibling/x` through.
    #[tokio::test]
    async fn glob_cannot_escape_the_workspace() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("inside.rs"), "// in\n").unwrap();
        // A sibling of the workspace, which no pattern may reach.
        std::fs::write(dir.path().join("outside.rs"), "// SECRET\n").unwrap();
        let ws = Workspace::new(&root).expect("workspace");
        let cfg = ToolsConfig::default();

        for pattern in ["../*.rs", "../**/*.rs", "/etc/*"] {
            let out = execute(&tool_call(GLOB, json!({"pattern": pattern})), &ws, &cfg).await;
            assert!(
                !out.content.contains("outside.rs") && !out.content.contains("SECRET"),
                "{pattern} escaped the workspace: {}",
                out.content
            );
            for line in out.content.lines().filter(|l| l.ends_with(".rs")) {
                assert!(
                    !line.starts_with("..") && !line.starts_with('/'),
                    "{pattern} returned a path outside the root: {line}"
                );
            }
        }
    }

    #[tokio::test]
    async fn glob_rejects_an_invalid_pattern_with_an_actionable_message() {
        let (_d, ws, cfg) = tree();
        let out = execute(&tool_call(GLOB, json!({"pattern": "src/[unclosed"})), &ws, &cfg).await;
        assert!(out.content.contains("not a valid glob"), "{}", out.content);
    }

    #[tokio::test]
    async fn grep_finds_matches_as_path_line_text() {
        let (_d, ws, cfg) = tree();
        let out = execute(&tool_call(GREP_SEARCH, json!({"pattern": "needle"})), &ws, &cfg).await;
        assert!(out.content.contains("src/app.rs:1: let needle = 1;"), "{}", out.content);
        assert!(out.display.contains("1 match(es) in 1 file(s)"), "{}", out.display);
    }

    #[tokio::test]
    async fn grep_skips_build_output_and_dependencies() {
        let (_d, ws, cfg) = tree();
        // `//` appears in src/ui/render.rs, target/ and node_modules/ alike;
        // only the real source file may come back.
        let out = execute(&tool_call(GREP_SEARCH, json!({"pattern": "//"})), &ws, &cfg).await;
        assert!(out.content.contains("src/ui/render.rs"), "{}", out.content);
        assert!(!out.content.contains("target/"), "{}", out.content);
        assert!(!out.content.contains("node_modules/"), "{}", out.content);
    }

    /// Finding nothing is an answer, not a failure -- same contract as `glob`.
    #[tokio::test]
    async fn grep_reports_no_matches_as_a_normal_result() {
        let (_d, ws, cfg) = tree();
        let out =
            execute(&tool_call(GREP_SEARCH, json!({"pattern": "nowhere_to_be_found"})), &ws, &cfg)
                .await;
        assert!(out.content.contains("No lines match"), "{}", out.content);
    }

    #[tokio::test]
    async fn grep_rejects_an_invalid_regex_with_an_actionable_message() {
        let (_d, ws, cfg) = tree();
        let out = execute(&tool_call(GREP_SEARCH, json!({"pattern": "[unclosed"})), &ws, &cfg).await;
        assert!(out.content.contains("not a valid regular expression"), "{}", out.content);
    }

    #[tokio::test]
    async fn grep_cannot_escape_the_workspace() {
        let (_d, ws, cfg) = tree();
        for escape in ["..", "../..", "/etc"] {
            let out = execute(
                &tool_call(GREP_SEARCH, json!({"pattern": "x", "path": escape})),
                &ws,
                &cfg,
            )
            .await;
            assert!(
                out.content.contains("outside the workspace"),
                "{escape} must be refused: {}",
                out.content
            );
        }
    }

    #[tokio::test]
    async fn grep_scopes_to_a_subdirectory_or_single_file() {
        let (_d, ws, cfg) = tree();
        let dir_scoped =
            execute(&tool_call(GREP_SEARCH, json!({"pattern": "//", "path": "src/ui"})), &ws, &cfg)
                .await;
        assert!(dir_scoped.content.contains("src/ui/render.rs"), "{}", dir_scoped.content);
        assert!(!dir_scoped.content.contains("src/app.rs"), "{}", dir_scoped.content);

        let file_scoped = execute(
            &tool_call(GREP_SEARCH, json!({"pattern": "needle", "path": "src/app.rs"})),
            &ws,
            &cfg,
        )
        .await;
        assert!(file_scoped.content.contains("src/app.rs:1:"), "{}", file_scoped.content);
    }

    #[tokio::test]
    async fn grep_includes_context_lines_grep_style() {
        let (_d, ws, cfg) = tree();
        std::fs::write(ws.root().join("src/app.rs"), "one\ntwo\nthree\n").unwrap();
        let out = execute(
            &tool_call(GREP_SEARCH, json!({"pattern": "two", "path": "src/app.rs", "context": 1})),
            &ws,
            &cfg,
        )
        .await;
        assert!(out.content.contains("src/app.rs-1- one"), "{}", out.content);
        assert!(out.content.contains("src/app.rs:2: two"), "{}", out.content);
        assert!(out.content.contains("src/app.rs-3- three"), "{}", out.content);
    }

    /// Matched fragments of an image or an object file are noise the model
    /// cannot use, so binary files are skipped outright.
    #[tokio::test]
    async fn grep_skips_binary_files() {
        let (_d, ws, cfg) = tree();
        std::fs::write(ws.root().join("blob.bin"), b"needle\x00needle\n").unwrap();
        let out = execute(&tool_call(GREP_SEARCH, json!({"pattern": "needle"})), &ws, &cfg).await;
        assert!(!out.content.contains("blob.bin"), "{}", out.content);
    }

    /// A pattern like `e` would otherwise return most of the project, so the
    /// result is capped and says so rather than silently stopping.
    #[tokio::test]
    async fn grep_caps_runaway_matches_and_says_so() {
        let (_d, ws, cfg) = tree();
        let many = "needle\n".repeat(MAX_GREP_MATCHES + 50);
        std::fs::write(ws.root().join("src/app.rs"), many).unwrap();
        let out = execute(&tool_call(GREP_SEARCH, json!({"pattern": "needle"})), &ws, &cfg).await;
        assert!(
            out.content.contains(&format!("[capped at {MAX_GREP_MATCHES} matching lines")),
            "{}",
            out.content
        );
    }

    /// Searching contents changes nothing, so plan mode has no reason to
    /// refuse it -- the point of the mode is to make research cheap.
    #[test]
    fn grep_is_allowed_in_plan_mode_and_offered_to_the_model() {
        assert!(plan_mode_block(&Action::Grep { pattern: "x".into(), path: None }).is_none());
        let names: Vec<String> = schemas(Mode::Plan, false, true)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(names.contains(&GREP_SEARCH.to_string()), "{names:?}");
    }

    #[tokio::test]
    async fn edit_file_replaces_one_span_and_leaves_the_rest() {
        let (_d, ws, cfg) = tree();
        std::fs::write(ws.root().join("src/app.rs"), "keep\nlet needle = 1;\nkeep too\n").unwrap();

        let out = execute(
            &tool_call(EDIT_FILE, json!({
                "path": "src/app.rs", "old_string": "let needle = 1;", "new_string": "let needle = 2;"
            })),
            &ws,
            &cfg,
        )
        .await;

        assert!(out.content.contains("Replaced 1"), "{}", out.content);
        let after = std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap();
        assert_eq!(after, "keep\nlet needle = 2;\nkeep too\n");
    }

    /// Silently editing the wrong one of several identical spans is the most
    /// damaging thing this tool could do, so ambiguity is refused not guessed.
    #[tokio::test]
    async fn edit_file_refuses_an_ambiguous_match_unless_told_to_replace_all() {
        let (_d, ws, cfg) = tree();
        std::fs::write(ws.root().join("src/app.rs"), "x = 1;\nx = 1;\n").unwrap();

        let ambiguous = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "old_string": "x = 1;", "new_string": "x = 2;"})),
            &ws,
            &cfg,
        )
        .await;
        assert!(ambiguous.content.contains("appears 2 times"), "{}", ambiguous.content);
        // Nothing was written.
        assert_eq!(
            std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap(),
            "x = 1;\nx = 1;\n"
        );

        let all = execute(
            &tool_call(EDIT_FILE, json!({
                "path": "src/app.rs", "old_string": "x = 1;", "new_string": "x = 2;", "replace_all": true
            })),
            &ws,
            &cfg,
        )
        .await;
        assert!(all.content.contains("Replaced 2"), "{}", all.content);
        assert_eq!(
            std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap(),
            "x = 2;\nx = 2;\n"
        );
    }

    #[tokio::test]
    async fn edit_file_reports_a_missing_span_without_touching_the_file() {
        let (_d, ws, cfg) = tree();
        let before = std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap();
        let out = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "old_string": "not there", "new_string": "x"})),
            &ws,
            &cfg,
        )
        .await;
        assert!(out.content.contains("was not found"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap(), before);
    }

    #[tokio::test]
    async fn edit_file_rejects_an_empty_or_unchanged_span() {
        let (_d, ws, cfg) = tree();
        let empty = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "old_string": "", "new_string": "x"})),
            &ws,
            &cfg,
        )
        .await;
        assert!(empty.content.contains("must not be empty"), "{}", empty.content);

        let same = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "old_string": "a", "new_string": "a"})),
            &ws,
            &cfg,
        )
        .await;
        assert!(same.content.contains("identical"), "{}", same.content);
    }

    #[tokio::test]
    async fn a_batch_of_edits_applies_in_order_under_one_call() {
        let (_d, ws, cfg) = tree();
        std::fs::write(ws.root().join("src/app.rs"), "alpha\nbeta\ngamma\n").unwrap();
        let out = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "edits": [
                {"old_string": "alpha", "new_string": "ALPHA"},
                {"old_string": "gamma", "new_string": "GAMMA"},
            ]})),
            &ws,
            &cfg,
        )
        .await;
        assert!(out.content.contains("Applied 2 edits"), "{}", out.content);
        let after = std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap();
        assert_eq!(after, "ALPHA\nbeta\nGAMMA\n");
    }

    /// Each span is matched against the previous spans' result -- the batch is
    /// one edit session, not several independent views of the original file.
    #[tokio::test]
    async fn a_later_edit_sees_what_an_earlier_one_produced() {
        let (_d, ws, cfg) = tree();
        std::fs::write(ws.root().join("src/app.rs"), "one\n").unwrap();
        let out = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "edits": [
                {"old_string": "one", "new_string": "two"},
                {"old_string": "two", "new_string": "three"},
            ]})),
            &ws,
            &cfg,
        )
        .await;
        assert!(out.content.contains("Applied 2 edits"), "{}", out.content);
        let after = std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap();
        assert_eq!(after, "three\n");
    }

    /// A batch that half-applied would leave the file in a state neither the
    /// model nor the user approved, so a failure anywhere writes nothing.
    #[tokio::test]
    async fn a_failing_batch_names_the_edit_and_touches_nothing() {
        let (_d, ws, cfg) = tree();
        std::fs::write(ws.root().join("src/app.rs"), "alpha\nbeta\n").unwrap();
        let out = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "edits": [
                {"old_string": "alpha", "new_string": "ALPHA"},
                {"old_string": "no such text", "new_string": "x"},
            ]})),
            &ws,
            &cfg,
        )
        .await;
        assert!(out.content.contains("edit 2 of 2"), "{}", out.content);
        assert!(out.content.contains("None of the edits were applied"), "{}", out.content);
        let after = std::fs::read_to_string(ws.root().join("src/app.rs")).unwrap();
        assert_eq!(after, "alpha\nbeta\n", "the first edit must not have landed");
    }

    /// Both argument forms at once is ambiguous about intent, so it is refused
    /// rather than guessed at -- and the single form still needs both halves.
    #[tokio::test]
    async fn edit_file_refuses_mixed_or_incomplete_argument_forms() {
        let (_d, ws, cfg) = tree();
        let mixed = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs",
                "old_string": "a", "new_string": "b",
                "edits": [{"old_string": "c", "new_string": "d"}]})),
            &ws,
            &cfg,
        )
        .await;
        assert!(mixed.content.contains("not both"), "{}", mixed.content);

        let incomplete = execute(
            &tool_call(EDIT_FILE, json!({"path": "src/app.rs", "old_string": "a"})),
            &ws,
            &cfg,
        )
        .await;
        assert!(incomplete.content.contains("both required"), "{}", incomplete.content);
    }

    #[tokio::test]
    async fn edit_file_cannot_escape_the_workspace() {
        let (_d, ws, cfg) = tree();
        let out = execute(
            &tool_call(EDIT_FILE, json!({
                "path": "../outside.txt", "old_string": "a", "new_string": "b"
            })),
            &ws,
            &cfg,
        )
        .await;
        assert!(out.content.contains("outside the workspace"), "{}", out.content);
    }

    /// The approval prompt must be able to describe every tool, or a call would
    /// reach the runner with nothing shown to the user.
    #[test]
    fn every_ported_tool_describes_an_action() {
        assert!(matches!(
            describe_action(&tool_call(LIST_DIR, json!({"path": "src"}))),
            Some(Action::List { .. })
        ));
        assert!(matches!(
            describe_action(&tool_call(LIST_DIR, json!({}))),
            Some(Action::List { .. })
        ));
        assert!(matches!(
            describe_action(&tool_call(GLOB, json!({"pattern": "**/*.rs"}))),
            Some(Action::Glob { .. })
        ));
        assert!(matches!(
            describe_action(&tool_call(EDIT_FILE, json!({
                "path": "a.rs", "old_string": "x", "new_string": "y"
            }))),
            Some(Action::Edit { .. })
        ));
        // Unusable arguments describe nothing, so the runner reports them back
        // to the model rather than the user being asked to approve a blank.
        assert!(describe_action(&tool_call(GLOB, json!({"pattern": "  "}))).is_none());
        assert!(describe_action(&tool_call(EDIT_FILE, json!({"path": "a.rs"}))).is_none());
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
        assert!(out.content.contains("truncated: 200 of"), "{}", out.content);
        // The marker alone let the model answer from the part it got and
        // present a partial result as the whole; it has to say what to do.
        assert!(out.content.contains("run it again"), "{}", out.content);
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

    /// Answering a question about a repo takes several `gh` calls in a row.
    /// Prompting for each is what turned a complete answer into a partial one.
    #[test]
    fn read_only_gh_commands_are_recognised() {
        for cmd in [
            "gh repo list --limit 1000",
            "gh repo view HolboxAI/boxcode --json name,visibility",
            "gh pr list --state all --limit 500",
            "gh pr view 42",
            "gh pr diff 42",
            "gh pr checks 42",
            "gh issue list --limit 1000",
            "gh run list --limit 100",
            "gh release list",
            "gh workflow list",
            "gh search repos --owner HolboxAI",
            "gh auth status",
            "gh api repos/HolboxAI/boxcode",
            "gh api --paginate repos/HolboxAI/boxcode/commits",
            "gh api -X GET repos/HolboxAI/boxcode",
            "gh api --method=GET repos/HolboxAI/boxcode",
        ] {
            assert!(is_read_only(cmd), "expected read-only: {cmd}");
        }
    }

    /// The allowlist is verb pairs, so anything that writes -- named or not
    /// yet invented -- keeps asking. Being wrong this way costs a keystroke;
    /// being wrong the other way costs a repository.
    #[test]
    fn writing_gh_commands_still_need_approval() {
        for cmd in [
            "gh repo delete HolboxAI/boxcode",
            "gh repo create thing --public",
            "gh repo clone HolboxAI/boxcode",
            "gh pr merge 42",
            "gh pr close 42",
            "gh pr create --fill",
            "gh release create v9.9.9",
            "gh release upload v1 file.zip",
            "gh secret set TOKEN",
            "gh auth logout",
            "gh api -X DELETE repos/HolboxAI/boxcode",
            "gh api --method POST repos/HolboxAI/boxcode/issues",
            "gh api --method=DELETE repos/x/y",
            "gh api repos/x/y/issues -f title=oops",
            "gh api repos/x/y --input body.json",
            // A subcommand this allowlist has never heard of.
            "gh newthing whatever",
            "gh",
        ] {
            assert!(!is_read_only(cmd), "expected NOT read-only: {cmd}");
        }
    }

    /// Auto-approving reads must not smuggle a write in behind a pipe or a
    /// subshell -- the existing chaining guard covers `gh` like anything else.
    #[test]
    fn a_chained_gh_command_is_never_read_only() {
        for cmd in [
            "gh repo list && gh repo delete x",
            "gh repo list; rm -rf build",
            "gh api repos/x/y | sh",
            "gh repo list $(gh repo delete x)",
        ] {
            assert!(!is_read_only(cmd), "expected NOT read-only: {cmd}");
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
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);

        assert!(prompt.contains(std::env::consts::OS), "{prompt}");
        assert!(prompt.contains(shell().0), "{prompt}");
        assert!(prompt.contains("NON-INTERACTIVE"), "{prompt}");
        if cfg!(windows) {
            assert!(prompt.contains("Do NOT use ls/cat/grep"), "{prompt}");
        } else {
            assert!(prompt.contains("Unix-like"), "{prompt}");
        }

        let exhausted = system_prompt(&ws, &cfg, cfg.max_steps, Mode::Normal, None);
        assert!(exhausted.contains("Answer the user now"), "{exhausted}");
    }

    /// The budget must not be a cliff the model discovers by falling off it:
    /// past three quarters, the prompt says how many rounds are left and to
    /// wrap up; before that, no budget talk at all.
    #[test]
    fn the_system_prompt_warns_at_three_quarters_of_the_step_budget() {
        let (_dir, ws, cfg) = fixture();
        let three_quarters = cfg.max_steps * 3 / 4;

        let early = system_prompt(&ws, &cfg, three_quarters.saturating_sub(1), Mode::Normal, None);
        assert!(!early.contains("BUDGET:"), "{early}");

        let late = system_prompt(&ws, &cfg, three_quarters, Mode::Normal, None);
        assert!(late.contains("BUDGET:"), "{late}");
        assert!(
            late.contains(&format!("{} left", cfg.max_steps - three_quarters)),
            "{late}"
        );
        // Warned, but still working: the full tool list is still described.
        assert!(late.contains(GREP_SEARCH), "{late}");
    }

    /// BOXCODE.md and AGENTS.md ride along on every request when present, and
    /// leave no trace at all when absent.
    #[test]
    fn the_system_prompt_carries_the_project_memory_files() {
        let (_dir, ws, cfg) = fixture();

        let without = system_prompt(&ws, &cfg, 0, Mode::Normal, None);
        assert!(!without.contains("PROJECT NOTES"), "{without}");

        std::fs::write(ws.root().join("BOXCODE.md"), "Run `cargo test` before committing.\n")
            .unwrap();
        std::fs::write(ws.root().join("AGENTS.md"), "The API layer lives in src/api.\n").unwrap();
        let with = system_prompt(&ws, &cfg, 0, Mode::Normal, None);
        assert!(with.contains("PROJECT NOTES from BOXCODE.md"), "{with}");
        assert!(with.contains("Run `cargo test` before committing."), "{with}");
        assert!(with.contains("PROJECT NOTES from AGENTS.md"), "{with}");
        assert!(with.contains("The API layer lives in src/api."), "{with}");
    }

    /// The memory is resent with every request, so a runaway file is capped
    /// rather than allowed to tax every turn of every later session.
    #[test]
    fn an_oversized_memory_file_is_clipped_not_sent_whole() {
        let (_dir, ws, cfg) = fixture();
        std::fs::write(ws.root().join("BOXCODE.md"), "x".repeat(100_000)).unwrap();
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);
        assert!(prompt.contains("truncated"), "{prompt}");
        assert!(prompt.len() < 60_000, "the whole file went through: {}", prompt.len());
    }

    /// An empty or unreadable memory file is treated as absent -- an amenity
    /// must not inject blank sections or take the session down.
    #[test]
    fn an_empty_memory_file_is_ignored() {
        let (_dir, ws, cfg) = fixture();
        std::fs::write(ws.root().join("BOXCODE.md"), "   \n\n").unwrap();
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);
        assert!(!prompt.contains("PROJECT NOTES"), "{prompt}");
    }

    /// Regression: without this, a model that only emits tool calls leaves the
    /// transcript as a bare log of "$ ..."/"📝 ..." lines with nothing said
    /// about them -- what a user pointed at directly when comparing this to
    /// Claude Code's narrated "I'll just run it." / "Ran it — output: ...".
    #[test]
    fn the_system_prompt_requires_narration_before_and_after_tool_use() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);

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
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);

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
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);

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
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);

        assert!(
            prompt.contains("offer to open it with"),
            "{prompt}"
        );
        assert!(
            prompt.contains("it waits for the same approval as everything else, it does not skip the prompt"),
            "{prompt}"
        );
    }

    /// `enabled = false` has to mean the model never sees the tool. A schema
    /// it can see is one it will eventually call, and answering "that is
    /// turned off" is a worse experience than never offering it.
    #[test]
    fn disabling_deployment_withholds_the_schema_entirely() {
        let names: Vec<String> = schemas(Mode::Normal, false, false)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == DEPLOY_PROJECT), "{names:?}");
        // ...and the rest are untouched.
        assert!(names.iter().any(|n| n == RUN_COMMAND), "{names:?}");
        assert_eq!(names.len(), schemas(Mode::Normal, false, true).len() - 1);
    }

    /// The two gates are independent and must compose: plan mode withholds
    /// deployment because it is the least reversible thing here, and
    /// `enabled = false` withholds it because the user turned it off. Neither
    /// may accidentally re-admit it when the other is inactive.
    #[test]
    fn the_deploy_gates_compose() {
        let has_deploy = |mode, deploy| {
            schemas(mode, false, deploy)
                .iter()
                .any(|s| s["function"]["name"] == DEPLOY_PROJECT)
        };
        assert!(has_deploy(Mode::Normal, true), "the ordinary case offers it");
        assert!(!has_deploy(Mode::Normal, false), "turned off in config");
        assert!(!has_deploy(Mode::Plan, true), "plan mode changes nothing");
        assert!(!has_deploy(Mode::Plan, false), "both at once");
    }

    #[test]
    fn the_schemas_name_exactly_the_tools_that_execute() {
        let schemas = schemas(Mode::Normal, false, true);
        let names: Vec<_> = schemas.iter().map(|s| s["function"]["name"].clone()).collect();
        assert_eq!(
            names,
            vec![
                RUN_COMMAND,
                READ_FILE,
                WRITE_FILE,
                LIST_DIR,
                GLOB,
                GREP_SEARCH,
                EDIT_FILE,
                PUBLISH_ARTIFACT,
                WEB_SEARCH,
                DEPLOY_PROJECT
            ]
        );
    }

    // ---- deploy_project ----------------------------------------------------

    fn deploy_call(args: Value) -> ToolCall {
        tool_call(DEPLOY_PROJECT, args)
    }

    #[test]
    fn a_deploy_call_describes_a_deployment_the_user_can_read() {
        match describe_action(&deploy_call(json!({"provider": "vercel", "production": true}))) {
            Some(Action::Deploy { provider, production, .. }) => {
                assert_eq!(provider, "vercel");
                assert!(production);
            }
            other => panic!("expected a Deploy action, got {other:?}"),
        }
    }

    /// Absent means preview. A model that was never told "production" must not
    /// reach it by omission.
    #[test]
    fn a_deploy_without_an_explicit_production_flag_is_a_preview() {
        match describe_action(&deploy_call(json!({"provider": "netlify"}))) {
            Some(Action::Deploy { production, .. }) => assert!(!production),
            other => panic!("expected a Deploy action, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_provider_is_not_something_the_user_is_asked_to_approve() {
        assert!(describe_action(&deploy_call(json!({"provider": "heroku"}))).is_none());
        assert!(describe_action(&deploy_call(json!({}))).is_none());
    }

    /// The property that makes this tool safe to hand a model: a deployment
    /// always stops for a decision, even with approval switched off entirely.
    #[test]
    fn every_deployment_always_stops_for_an_explicit_decision() {
        let root = Path::new("/Users/dev/project");
        for production in [true, false] {
            let action = describe_action(&deploy_call(
                json!({"provider": "vercel", "production": production}),
            ))
            .expect("describes");
            let risk = action_risk(&action, root);
            assert!(
                risk.is_dangerous(),
                "production={production} must be dangerous, got {risk:?}"
            );
            // ...and the reason has to say which of the two it is, because the
            // difference is the whole question being asked.
            let reason = risk.reason().unwrap_or_default();
            if production {
                assert!(reason.contains("production"), "{reason}");
            } else {
                assert!(reason.contains("public internet"), "{reason}");
            }
        }
    }

    /// Reads and writes are unaffected by the new risk routing.
    #[test]
    fn ordinary_actions_keep_their_previous_risk() {
        let root = Path::new("/Users/dev/project");
        for action in [
            Action::Read { path: "a.rs".into() },
            Action::List { path: ".".into() },
            Action::Glob { pattern: "**/*.rs".into() },
            Action::Write { path: "a.rs".into(), content: String::new() },
        ] {
            assert_eq!(action_risk(&action, root), danger::Risk::Normal, "{action:?}");
        }
        // ...and a command is still judged by the classifier.
        let rm = Action::Command { command: "rm -rf /".into(), purpose: None };
        assert!(matches!(action_risk(&rm, root), danger::Risk::Blocked(_)));
    }

    #[tokio::test]
    async fn deploying_a_directory_with_nothing_in_it_fails_before_touching_a_cli() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = execute(&deploy_call(json!({"provider": "vercel"})), &ws, &ToolsConfig::default()).await;
        assert!(out.content.contains("nothing to build or serve"), "{}", out.content);
    }

    /// The flow takes a well-formed deployment before this ever runs, so what
    /// is left is the case it declined: a deployment asked for alongside other
    /// tool calls, which cannot be sequenced against something that owns the
    /// screen until it finishes.
    #[tokio::test]
    async fn a_deployment_the_flow_declined_explains_why_rather_than_half_running() {
        let (_dir, ws, cfg) = tree();
        std::fs::write(
            ws.root().join("package.json"),
            r#"{"name":"probe-app","scripts":{"build":"vite build"},"devDependencies":{"vite":"5"}}"#,
        )
        .unwrap();

        let out = execute(&deploy_call(json!({"provider": "vercel"})), &ws, &cfg).await;
        assert!(out.content.contains("only tool call"), "{}", out.content);
        assert!(out.content.contains("Ask for it on its own"), "{}", out.content);
    }

    /// The model must never be in a position to name an environment variable,
    /// let alone invent a value for one -- those are entered by hand in
    /// `/deploy`, where the user types them into a masked field.
    #[test]
    fn the_deploy_schema_gives_the_model_no_way_to_pass_a_secret() {
        let schema = schemas(Mode::Normal, false, true)
            .into_iter()
            .find(|s| s["function"]["name"] == DEPLOY_PROJECT)
            .expect("the deploy schema");
        let properties = &schema["function"]["parameters"]["properties"];
        let names: Vec<&str> = properties.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["production", "provider"], "no other input is accepted");
    }

    #[test]
    fn the_system_prompt_tells_the_model_when_and_when_not_to_deploy() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, None);
        assert!(prompt.contains(DEPLOY_PROJECT), "{prompt}");
        assert!(prompt.contains("public internet"), "{prompt}");
        assert!(prompt.contains("Default to a preview"), "{prompt}");
        // It should just call it: the flow asks the user for whatever it needs.
        assert!(prompt.contains("it asks the user for directly"), "{prompt}");
        assert!(prompt.contains("never alongside other tool calls"), "{prompt}");
    }

    // ---- plan mode ---------------------------------------------------------

    /// The strongest form the guarantee takes: in plan mode the writing tools
    /// are not on the model's list at all, so there is no call to refuse and
    /// no prompt to mistakenly accept.
    #[test]
    fn plan_mode_withholds_the_writing_tools_and_offers_the_way_out() {
        let names: Vec<String> = schemas(Mode::Plan, false, true)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();

        assert!(!names.contains(&WRITE_FILE.to_string()), "{names:?}");
        assert!(!names.contains(&EDIT_FILE.to_string()), "{names:?}");
        assert!(names.contains(&EXIT_PLAN_MODE.to_string()), "{names:?}");

        // Research is the whole point of the mode, so everything it needs
        // stays. `run_command` included -- narrowed to read-only commands by
        // `plan_mode_block`, not withheld.
        for tool in [RUN_COMMAND, READ_FILE, LIST_DIR, GLOB, WEB_SEARCH] {
            assert!(names.contains(&tool.to_string()), "{tool} missing: {names:?}");
        }
    }

    /// Deploying changes nothing in the working directory, which is exactly
    /// how it would slip past a plan-mode check that only thinks about files
    /// -- and it puts the project on the public internet, which is the least
    /// reversible thing this program does.
    #[test]
    fn plan_mode_withholds_and_refuses_deployment() {
        let names: Vec<String> = schemas(Mode::Plan, false, true)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.contains(&DEPLOY_PROJECT.to_string()), "{names:?}");

        // And refused by the second layer too, in case one ever arrives anyway.
        let action = Action::Deploy {
            provider: "vercel".to_string(),
            production: false,
            summary: None,
        };
        let reason = plan_mode_block(&action).expect("must not be allowed in plan mode");
        assert!(reason.contains(EXIT_PLAN_MODE), "{reason}");
    }

    /// The inverse: `exit_plan_mode` must not be advertised when there is no
    /// plan mode to exit, or the model will call it to announce intentions
    /// nobody asked to approve.
    #[test]
    fn normal_mode_does_not_offer_exit_plan_mode() {
        let names: Vec<String> = schemas(Mode::Normal, false, true)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.contains(&EXIT_PLAN_MODE.to_string()), "{names:?}");
    }

    #[test]
    fn plan_mode_blocks_writes_and_non_read_only_commands_only() {
        let allowed = [
            Action::Read { path: "src/main.rs".into() },
            Action::List { path: ".".into() },
            Action::Glob { pattern: "**/*.rs".into() },
            Action::Search { query: "rust".into(), max_results: 3 },
            Action::Plan(Proposal {
                title: "A plan".into(),
                summary: String::new(),
                steps: vec!["do it".into()],
                not_doing: Vec::new(),
            }),
            Action::Command { command: "git log".into(), purpose: None },
            Action::Command { command: "grep -rn TODO src".into(), purpose: None },
        ];
        for action in allowed {
            assert!(
                plan_mode_block(&action).is_none(),
                "{action:?} changes nothing and must stay available"
            );
        }

        let refused = [
            Action::Write { path: "a.py".into(), content: String::new() },
            Action::Edit {
                path: "a.py".into(),
                edits: vec![EditSpan { old: "a".into(), new: "b".into(), replace_all: false }],
            },
            Action::Command { command: "cargo build".into(), purpose: None },
            Action::Command { command: "rm -rf build".into(), purpose: None },
            // A read-only prefix chained into something else is not read-only.
            Action::Command { command: "cat a && rm b".into(), purpose: None },
        ];
        for action in refused {
            let reason = plan_mode_block(&action)
                .unwrap_or_else(|| panic!("{action:?} must not be allowed in plan mode"));
            // Every refusal has to say what to do instead, or the model just
            // retries it worded differently.
            assert!(
                reason.contains(EXIT_PLAN_MODE),
                "{action:?} refusal gives no way forward: {reason}"
            );
        }
    }

    #[test]
    fn the_plan_mode_prompt_says_what_is_unavailable_and_how_to_get_out() {
        let (_dir, ws, cfg) = fixture();
        let prompt = system_prompt(&ws, &cfg, 0, Mode::Plan, None);

        assert!(prompt.contains("PLAN MODE"), "{prompt}");
        assert!(prompt.contains(EXIT_PLAN_MODE), "{prompt}");
        // A tool that was not sent must not be described as available: the
        // call would come back as an error and cost a turn to discover.
        assert!(
            !prompt.contains(&format!("{WRITE_FILE}(path, content)")),
            "the prompt still advertises write_file: {prompt}"
        );
        assert!(
            !prompt.contains(&format!("{EDIT_FILE}(path,")),
            "the prompt still advertises edit_file: {prompt}"
        );
    }

    /// A plan with no title or no steps cannot be saved as a useful file and
    /// cannot be worked through, so it is not something to put in front of the
    /// user -- the unusable-arguments path tells the model instead.
    #[test]
    fn a_plan_without_a_title_or_steps_is_not_something_to_approve() {
        for args in [
            json!({ "title": "   ", "summary": "s", "steps": ["a"] }),
            json!({ "title": "Real title", "summary": "s", "steps": [] }),
            json!({ "title": "Real title", "summary": "s", "steps": ["  ", ""] }),
        ] {
            let call = tool_call(EXIT_PLAN_MODE, args.clone());
            assert_eq!(describe_action(&call), None, "{args}");
        }
    }

    #[test]
    fn a_plan_proposal_keeps_its_structure() {
        let call = tool_call(
            EXIT_PLAN_MODE,
            json!({
                "title": "Rate limiting",
                "summary": "Fixed window.",
                "steps": ["Add the limiter", "  Wrap the router  ", ""],
                "not_doing": ["Distributed limiting", "  "],
            }),
        );
        match describe_action(&call) {
            Some(Action::Plan(p)) => {
                assert_eq!(p.title, "Rate limiting");
                assert_eq!(p.steps, vec!["Add the limiter", "Wrap the router"]);
                assert_eq!(p.not_doing, vec!["Distributed limiting"]);
            }
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    /// `plan_progress` is only offered when there is a plan to record against.
    #[test]
    fn plan_progress_is_offered_only_alongside_an_active_plan() {
        let without: Vec<String> = schemas(Mode::Normal, false, true)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!without.contains(&PLAN_PROGRESS.to_string()), "{without:?}");

        let with: Vec<String> = schemas(Mode::Normal, true, true)
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(with.contains(&PLAN_PROGRESS.to_string()), "{with:?}");
    }

    /// A status that is neither done nor blocked is a guess about what the
    /// model meant, and guessing wrong writes a false claim into a file the
    /// user will trust later.
    #[test]
    fn an_unrecognised_progress_status_is_refused_rather_than_guessed() {
        let call = tool_call(PLAN_PROGRESS, json!({ "step": 1, "status": "partially" }));
        assert_eq!(describe_action(&call), None);

        let good = tool_call(PLAN_PROGRESS, json!({ "step": 2, "status": "done" }));
        assert_eq!(
            describe_action(&good),
            Some(Action::Progress { step: 2, done: true, note: None })
        );
    }

    /// The plan is restated on every request, with live step state, because a
    /// long implementation pushes the original proposal out of the context
    /// window and a resumed plan was never in the conversation at all.
    #[test]
    fn an_active_plan_is_restated_in_the_prompt_with_its_progress() {
        let (_dir, ws, cfg) = fixture();
        let mut plan = crate::plan::Plan {
            title: "Rate limiting".to_string(),
            summary: "Fixed window.".to_string(),
            steps: vec![
                crate::plan::Step::new("Add the limiter"),
                crate::plan::Step::new("Wrap the router"),
            ],
            not_doing: vec!["Distributed limiting".to_string()],
            created: "2026-08-11".to_string(),
            updated: "2026-08-11".to_string(),
            base_commit: None,
            model: "m".to_string(),
            path: std::path::PathBuf::from("/tmp/project/plan.md"),
        };
        plan.mark(1, true, None).unwrap();

        let prompt = system_prompt(&ws, &cfg, 0, Mode::Normal, Some(&plan));
        assert!(prompt.contains("1/2 steps done"), "{prompt}");
        assert!(prompt.contains("[x] 1. Add the limiter"), "{prompt}");
        assert!(prompt.contains("[ ] 2. Wrap the router"), "{prompt}");
        assert!(prompt.contains("Distributed limiting"), "{prompt}");
        assert!(prompt.contains(PLAN_PROGRESS), "{prompt}");

        // A finished plan is not restated: there is nothing left to follow,
        // and repeating it invites the model to redo work.
        plan.mark(2, true, None).unwrap();
        let done = system_prompt(&ws, &cfg, 0, Mode::Normal, Some(&plan));
        assert!(!done.contains("IMPLEMENTING AN APPROVED PLAN"), "{done}");
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
    async fn a_ranged_read_returns_a_numbered_slice() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(
            &tool_call(READ_FILE, json!({"path": "hello.txt", "offset": 2, "limit": 1})),
            &ws,
            &cfg,
        )
        .await;
        assert_eq!(out.content, "     2\ttwo");
        assert!(out.display.contains("lines 2-2 of 3"), "{}", out.display);
    }

    #[tokio::test]
    async fn offset_alone_reads_to_the_end_and_limit_alone_from_the_top() {
        let (_dir, ws, cfg) = fixture();
        let from_two = execute(
            &tool_call(READ_FILE, json!({"path": "hello.txt", "offset": 2})),
            &ws,
            &cfg,
        )
        .await;
        assert_eq!(from_two.content, "     2\ttwo\n     3\tthree");

        let first_two = execute(
            &tool_call(READ_FILE, json!({"path": "hello.txt", "limit": 2})),
            &ws,
            &cfg,
        )
        .await;
        assert_eq!(first_two.content, "     1\tone\n     2\ttwo");
    }

    /// Asking past the end is an answer, not a failure -- an error would push
    /// the model toward retrying instead of concluding the file is shorter.
    #[tokio::test]
    async fn an_offset_past_the_end_reports_the_real_length() {
        let (_dir, ws, cfg) = fixture();
        let out = execute(
            &tool_call(READ_FILE, json!({"path": "hello.txt", "offset": 99})),
            &ws,
            &cfg,
        )
        .await;
        assert!(out.content.contains("has only 3 lines"), "{}", out.content);
        assert!(!out.content.starts_with("Error:"), "{}", out.content);
    }

    /// A truncated full read tells the model how to get the rest -- the whole
    /// reason ranged reads exist -- instead of a bare "output was cut" marker.
    #[tokio::test]
    async fn a_truncated_full_read_says_which_offset_to_resume_from() {
        let (_dir, ws, mut cfg) = fixture();
        cfg.max_output_bytes = 8;
        let out = execute(&read_call("hello.txt"), &ws, &cfg).await;
        assert!(
            out.content.contains(&format!("Call {READ_FILE} again with offset=")),
            "{}",
            out.content
        );
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
    ///
    /// Isolates `$HOME` (locking config::test_support::HOME_LOCK the same
    /// way web_search_falls_back_to_the_embedded_python... does, and for
    /// the same reason `with_isolated_home` itself can't be used from an
    /// async test): this test's entire premise is "no interpreter can be
    /// found, not even a fallback", which the real machine running the
    /// suite may not actually be in -- an earlier test run's embedded
    /// Python left genuinely installed at the real `~/.boxcode/
    /// python` (deliberately, the same way a real ddgs reinstall is left in
    /// place elsewhere in this file) would otherwise make this assertion
    /// false on a second run, exactly as happened while writing this.
    #[tokio::test]
    async fn a_missing_python_interpreter_is_explained_rather_than_panicking() {
        let _guard = crate::config::test_support::HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fake_home = tempfile::tempdir().expect("temp home");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let (_dir, ws, mut cfg) = fixture();
        cfg.python_bin = "no-such-interpreter-xyz-123".to_string();
        let out = execute(&search_call("rust"), &ws, &cfg).await;

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

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

    /// embedded_python_path/the fallback in execute_web_search, proven
    /// against a real (fake-content, real-file) interpreter at the exact
    /// path install.sh's/install.ps1's embedded-Python installers use --
    /// not mocked at the Rust level, the environment is genuinely set up
    /// the way a machine with no system python3 but a working embedded one
    /// would be.
    ///
    /// Locks config::test_support::HOME_LOCK directly rather than going
    /// through with_isolated_home: this test is async (needs `execute`'s
    /// `.await`), and with_isolated_home's closure runs synchronously on
    /// the calling thread, which is already inside #[tokio::test]'s own
    /// runtime -- calling block_on again in there to bridge into async code
    /// would panic ("Cannot start a runtime from within a runtime").
    #[cfg(unix)]
    #[tokio::test]
    async fn web_search_falls_back_to_the_embedded_python_when_configured_one_is_missing() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = crate::config::test_support::HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let fake_home = tempfile::tempdir().expect("temp home");
        let embedded_bin_dir = fake_home.path().join(".boxcode").join("python").join("bin");
        std::fs::create_dir_all(&embedded_bin_dir).unwrap();
        let embedded_python = embedded_bin_dir.join("python3");
        std::fs::write(
            &embedded_python,
            "#!/bin/sh\necho '{\"results\": [{\"title\": \"T\", \"href\": \"https://embedded-python-worked\", \"body\": \"B\"}]}'\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&embedded_python).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&embedded_python, perms).unwrap();

        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let (_dir, ws, mut cfg) = fixture();
        // A configured interpreter that does not exist -- exactly the state
        // a fresh config.toml is in (python_bin defaults to "python3", and
        // "python3" is, by construction, not on PATH on the machine this
        // scenario is meant to model).
        cfg.python_bin = "no-such-system-python-xyz".to_string();

        let out = execute(&search_call("rust"), &ws, &cfg).await;

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(
            out.content.contains("https://embedded-python-worked"),
            "expected the embedded Python fallback to have actually run, got: {}",
            out.content
        );
    }

    /// The Windows-only bug this guards against: `Command::new` spawning
    /// the "App Execution Alias" stub does not fail like a missing binary
    /// would (see `looks_like_windows_app_execution_alias_stub`'s own doc
    /// comment) -- it succeeds, with garbage stdout. Modeled here with a
    /// fake interpreter that echoes the stub's real wording instead of
    /// relying on an actual Windows machine, the same way the sibling
    /// `_when_configured_one_is_missing` test above models a `NotFound`
    /// spawn without one.
    #[cfg(unix)]
    #[tokio::test]
    async fn web_search_falls_back_to_the_embedded_python_when_configured_one_is_the_windows_stub() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = crate::config::test_support::HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let fake_home = tempfile::tempdir().expect("temp home");
        let embedded_bin_dir = fake_home.path().join(".boxcode").join("python").join("bin");
        std::fs::create_dir_all(&embedded_bin_dir).unwrap();
        let embedded_python = embedded_bin_dir.join("python3");
        std::fs::write(
            &embedded_python,
            "#!/bin/sh\necho '{\"results\": [{\"title\": \"T\", \"href\": \"https://embedded-python-worked\", \"body\": \"B\"}]}'\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&embedded_python).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&embedded_python, perms).unwrap();

        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let (dir, ws, mut cfg) = fixture();
        cfg.python_bin = fake_interpreter(
            dir.path(),
            "echo 'Python was not found; run without arguments to install from the Microsoft \
             Store, or disable this shortcut from Settings > Manage App Execution Aliases.'",
        );

        let out = execute(&search_call("rust"), &ws, &cfg).await;

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(
            out.content.contains("https://embedded-python-worked"),
            "expected the stub to be detected and the embedded Python fallback to run, got: {}",
            out.content
        );
    }

    /// Same stub, but with no embedded Python to fall back to -- the error
    /// should name the actual problem (a Store stub, not a missing
    /// install) rather than the generic "could not run" message the
    /// `NotFound` path uses, since the stub did spawn successfully.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_windows_stub_with_no_embedded_fallback_is_explained_clearly() {
        let _guard = crate::config::test_support::HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fake_home = tempfile::tempdir().expect("temp home");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", fake_home.path());

        let (dir, ws, mut cfg) = fixture();
        cfg.python_bin = fake_interpreter(
            dir.path(),
            "echo 'Python was not found; run without arguments to install from the Microsoft \
             Store, or disable this shortcut from Settings > Manage App Execution Aliases.'",
        );

        let out = execute(&search_call("rust"), &ws, &cfg).await;

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert!(out.content.contains("App Execution Alias"), "{}", out.content);
        assert!(out.content.contains("pip install ddgs"), "{}", out.content);
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

    /// Regression test for a real CI flake: a script written and chmod'd
    /// executable moments before it's run can transiently report "text file
    /// busy" on some filesystems (overlayfs, notably -- what most CI/Docker
    /// containers run on), which surfaced in this exact suite. Rather than
    /// try to reproduce overlayfs's own internal caching race (environment-
    /// specific and not reliably forceable), this proves the *retry
    /// mechanism itself* against the deterministic, filesystem-agnostic
    /// case Linux guarantees: exec-ing a file that is genuinely still open
    /// for writing always fails with ETXTBSY, and always succeeds the
    /// moment the writer releases it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_transiently_busy_interpreter_is_retried_rather_than_failed() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, ws, mut cfg) = fixture();
        let script_path = dir.path().join("busy-interpreter.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\necho '{\"results\": [{\"title\": \"T\", \"href\": \"https://x\", \"body\": \"B\"}]}'\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        cfg.python_bin = script_path.to_string_lossy().into_owned();

        // Held open for writing (without even writing anything further) for
        // longer than one retry's backoff but well within the retry
        // budget, so the first attempt(s) must hit real ETXTBSY and only a
        // later retry can succeed -- proving the loop, not just its syntax.
        let held_open = std::fs::OpenOptions::new().write(true).open(&script_path).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            drop(held_open);
        });

        let out = execute(&search_call("rust"), &ws, &cfg).await;
        release.await.unwrap();

        assert!(
            out.content.contains("https://x"),
            "expected the retry to recover once the writer released the file, got: {}",
            out.content
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
