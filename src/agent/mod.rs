//! Agents: who is working, what they are allowed to touch, and what the rest of
//! the app hears about it while they work.

pub mod run;

use crate::llm::{self, ChatMessage};
use crate::permission::{Allowlist, Decision};
use crate::tools::ToolCtx;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// One specialist. Step 2 fills this registry out; the shape is already the one
/// the specialists need, so adding them is data rather than code.
pub struct AgentSpec {
    pub id: &'static str,
    pub label: &'static str,
    /// How the orchestrator will be told what this specialist is for. Unused
    /// until step 2 adds `delegate`, but part of what defines an agent.
    #[allow(dead_code)]
    pub description: &'static str,
    pub system_prompt: &'static str,
    /// Names from `tools::TOOLS`. A call to anything outside this list is
    /// refused with a message the model can act on.
    pub tools: &'static [&'static str],
}

pub const ALL_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "glob",
    "grep",
    "write_file",
    "edit_file",
    "run_shell",
];

pub const CODER_PROMPT: &str = "\
You are a coding agent working directly in a real repository on the user's machine.

You have tools that read, search, write and edit files, and run shell commands. Use \
them. Do not describe what you would do, and do not print a patch and ask the user to \
apply it -- make the change yourself and confirm it worked.

How to work:
- Look before you edit. Read the file, or grep for the symbol, before changing it. \
Never guess at a file's contents.
- Prefer edit_file over write_file for existing files; write_file replaces the whole \
file and will silently destroy anything you did not read first.
- Match the surrounding code: its naming, its error handling, its comment density. A \
change should be hard to pick out as yours.
- Verify. If the project has a build or a test suite, run it after you change \
something, and fix what you broke.
- A tool returning an error is normal. Read the message, adjust, and continue.
- If the user denies a command, do not retry it. Find another route or ask what they \
would prefer.

When you are done, say briefly what you changed and how you know it works. Keep it to \
a few sentences -- the user can see every file you touched in the transcript above, so \
do not re-list them or paste the code back.";

pub const AGENTS: &[AgentSpec] = &[AgentSpec {
    id: "coder",
    label: "Coder",
    description: "Reads, writes and edits code, and runs builds and tests.",
    system_prompt: CODER_PROMPT,
    tools: ALL_TOOLS,
}];

/// The agent a bare user prompt is handed to.
pub const DEFAULT_AGENT: &str = "coder";

pub fn find(id: &str) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|a| a.id == id)
}

pub fn default_agent() -> &'static AgentSpec {
    find(DEFAULT_AGENT).expect("the default agent must exist in the registry")
}

/// What the event loop hears while an agent works.
#[derive(Debug)]
pub enum AgentEvent {
    /// Prose from the model, streaming as it arrives.
    Token { agent: &'static str, text: String },
    ToolStarted {
        agent: &'static str,
        call_id: String,
        summary: String,
    },
    ToolFinished {
        call_id: String,
        ok: bool,
        detail: String,
    },
    /// Blocks the agent until the UI answers.
    NeedsPermission(PermissionRequest),
    Finished {
        result: Result<String, String>,
        /// The conversation including every tool call and result, handed back so
        /// the next user prompt continues from it.
        messages: Vec<ChatMessage>,
    },
}

#[derive(Debug)]
pub struct PermissionRequest {
    pub summary: String,
    /// Key a session grant would be stored under. `None` means this call is not
    /// safe to grant for a whole session, so the UI must not offer that option.
    pub grant: Option<String>,
    pub respond: oneshot::Sender<Decision>,
}

/// Everything a run needs, assembled once per prompt.
pub struct RunCtx {
    /// Tags every event so the event loop can discard anything belonging to a
    /// run the user already cancelled.
    pub run_id: u64,
    pub client: reqwest::Client,
    pub target: llm::Target,
    pub tools: ToolCtx,
    pub allowlist: Allowlist,
    pub max_iterations: usize,
    pub cancel: Arc<AtomicBool>,
    pub tx: mpsc::Sender<(u64, AgentEvent)>,
}

impl RunCtx {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// A closed channel means the app is shutting down; the run will notice via
    /// its cancel flag, so there is nothing useful to do about the error here.
    pub async fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send((self.run_id, event)).await;
    }
}

/// Grounding facts prepended to the system prompt. Without this the model has no
/// idea where it is and burns its first turns on `list_dir(".")`.
pub fn workspace_preamble(ctx: &ToolCtx) -> String {
    let mut entries: Vec<String> = std::fs::read_dir(&ctx.workspace)
        .map(|dir| {
            dir.flatten()
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .filter(|name| !name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    entries.sort();

    let listing = if entries.is_empty() {
        "(empty)".to_string()
    } else {
        entries.join("  ")
    };

    format!(
        "Workspace root: {}\nTop level: {listing}\n\nAll paths you pass to tools are \
         relative to the workspace root. You cannot read or write outside it.",
        ctx.workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools;

    #[test]
    fn every_agent_names_tools_that_exist() {
        for agent in AGENTS {
            assert!(!agent.tools.is_empty(), "{} has no tools", agent.id);
            for name in agent.tools {
                assert!(
                    tools::find(name).is_some(),
                    "{} lists unknown tool '{name}'",
                    agent.id
                );
            }
        }
    }

    #[test]
    fn every_agent_has_a_unique_id_and_the_default_resolves() {
        let mut seen = std::collections::HashSet::new();
        for agent in AGENTS {
            assert!(seen.insert(agent.id), "duplicate agent id {}", agent.id);
        }
        assert_eq!(default_agent().id, DEFAULT_AGENT);
    }

    #[test]
    fn all_tools_covers_the_whole_registry() {
        // Otherwise a newly added tool would be silently unreachable.
        assert_eq!(ALL_TOOLS.len(), tools::TOOLS.len());
        for tool in tools::TOOLS {
            assert!(ALL_TOOLS.contains(&tool.name), "{} is not in ALL_TOOLS", tool.name);
        }
    }

    #[test]
    fn the_preamble_names_the_root_and_its_visible_entries() {
        let (_dir, ctx) = tools::test_support::ctx();
        tools::test_support::write(&ctx, "src/app.rs", "");
        tools::test_support::write(&ctx, "Cargo.toml", "");
        tools::test_support::write(&ctx, ".hidden", "");

        let preamble = workspace_preamble(&ctx);
        assert!(preamble.contains(&ctx.workspace.display().to_string()));
        assert!(preamble.contains("src/"));
        assert!(preamble.contains("Cargo.toml"));
        assert!(!preamble.contains(".hidden"));
    }
}
