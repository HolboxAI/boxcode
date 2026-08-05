use crate::config::Config;
use crate::danger;
use crate::llm::{ChatMessage, ToolCall};
use crate::providers;
use crate::tools::{self, ToolOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{HashSet, VecDeque};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub enum AppState {
    AwaitingInput,
    /// Transient: the event loop picks this up, fires the request, and moves to `Streaming`.
    Sending,
    Streaming,
    /// A command is on screen waiting for the user to allow or refuse it. The
    /// only thing standing between the model and the machine, so the turn stops
    /// dead here until a key is pressed.
    AwaitingApproval,
    /// Commands are running in a spawned task; results arrive on the channel.
    ExecutingTools,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Error,
    /// Confirmations from `/provider` and `/model`, e.g. "Switched to deepseek /
    /// deepseek-v4-flash." Distinct from Assistant (would wrongly imply the
    /// model said it) and Error (wrong tone/color for a success message).
    System,
    /// The result of one tool call, sent back to the model as `role: "tool"`.
    Tool,
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    /// What goes on the wire. For a tool result this is the entire file, which is
    /// why it is not what gets drawn.
    pub content: String,
    /// What the transcript shows, when that differs from `content`.
    pub display: Option<String>,
    /// Tool calls the assistant asked for. Only ever set on `Role::Assistant`.
    pub tool_calls: Vec<ToolCall>,
    /// Which call this message answers. Only ever set on `Role::Tool`.
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            display: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// The text to draw, which for a tool result is a one-line summary rather
    /// than the file it fetched.
    pub fn body(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.content)
    }
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::User => "You",
            Role::Assistant => "Assistant",
            Role::Error => "Error",
            Role::System => "System",
            Role::Tool => "Tool",
        }
    }
}

/// State of the `/provider` and `/model` overlays. `None` means the normal input
/// box is active; every other variant intercepts all keyboard input in
/// `handle_key` before it reaches the normal editing logic.
#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    ProviderPicker {
        selected: usize,
    },
    ModelPicker {
        provider_id: &'static str,
        selected: usize,
    },
    ApiKeyPrompt {
        provider_id: &'static str,
        model: String,
    },
    CustomEndpoint(CustomStep),
    /// Asks about `pending_tools.front()`. Unlike the other overlays this one
    /// appears while the app is busy, mid-turn.
    ToolApproval {
        action: tools::Action,
        /// How many more calls are queued behind this one.
        remaining: usize,
    },
}

/// Sequential manual entry used when the user picks "Custom endpoint..." instead
/// of a known provider -- preserves the tool's "any OpenAI-compatible endpoint"
/// generality rather than limiting it to the built-in registry.
#[derive(Clone, Debug, PartialEq)]
pub enum CustomStep {
    Endpoint,
    Model { endpoint: String },
    ApiKey { endpoint: String, model: String },
}

pub struct App {
    pub state: AppState,
    pub messages: Vec<Message>,
    /// Raw text of the prompt box. May contain '\n' (Alt/Shift-Enter inserts one).
    pub input_buffer: String,
    /// Cursor position as a *byte* index into `input_buffer`. Always on a char boundary.
    pub cursor: usize,
    /// Text accumulated from the in-flight response.
    pub streaming_response: String,
    /// Incremented per request so tokens from a cancelled request are ignored.
    pub request_id: u64,
    /// Abort handle for the in-flight request task, used by Esc.
    pub abort: Option<tokio::task::AbortHandle>,
    pub scroll: u16,
    /// While true the message pane sticks to the bottom as new text arrives.
    pub follow_tail: bool,
    /// Which choice is highlighted at a `ToolApproval` prompt: `true` for
    /// "yes", `false` for "no". A plain `App` field rather than a variant on
    /// `Overlay::ToolApproval` itself so it resets independently of the
    /// action/remaining-count data -- Up/Down toggles it, Enter reads it, and
    /// every new prompt starts back on "yes" to match bare-Enter's long-
    /// standing meaning.
    pub approval_selected: bool,
    pub config: Config,
    pub should_exit: bool,
    /// Set once the user has interacted, so the welcome panel gives way to the transcript.
    pub greeted: bool,
    /// `Some` while `/provider` or `/model` is active; see `Overlay`.
    pub overlay: Option<Overlay>,
    /// Single-line buffer for overlay text entry (API key, custom endpoint/model).
    /// Kept separate from `input_buffer` so the (possibly masked) overlay text
    /// never renders in the base input box behind the popup, and so the two
    /// never fight over `f.set_cursor(...)` in the same frame.
    pub overlay_input: String,
    pub overlay_cursor: usize,
    /// Calls still awaiting a yes or no, front first.
    pub pending_tools: VecDeque<ToolCall>,
    /// Calls the user allowed, waiting for the event loop to spawn them.
    pub approved_tools: Vec<ToolCall>,
    /// A snapshot of `approved_tools` taken the moment execution starts, kept
    /// around purely for display. `main.rs` drains `approved_tools` as soon as
    /// it spawns the runner task, so by the next frame that list is empty --
    /// without this copy "Running N commands…" would show N for one frame and
    /// then silently go blank while the commands were still running.
    pub running_tools: Vec<ToolCall>,
    /// Tool rounds spent on the current prompt, reset by `submit`. Once this hits
    /// the configured ceiling the schemas stop being sent, which is what makes a
    /// model that will not stop calling tools produce an answer instead.
    pub tool_steps: usize,
    /// When the current turn started, for the elapsed-time shown in the
    /// footer. `None` while idle; set once in `submit`, cleared on every path
    /// back to `AwaitingInput` (`finish_stream`, `fail_stream`, `cancel`).
    pub busy_started: Option<std::time::Instant>,
    /// Characters streamed so far this turn. There is no authoritative token
    /// count until the endpoint's final usage field (most don't send one by
    /// default), so the footer shows `streamed_chars / 4` as a rough live
    /// estimate -- the same kind of approximation Claude Code's own live
    /// counter is understood to show mid-stream.
    pub streamed_chars: usize,
    /// One line for the welcome screen describing where commands will run, or
    /// why the tool is off. Set by `main` once the workspace has been resolved.
    pub workspace_status: String,
    /// The resolved working directory, shown on the approval prompt so it is
    /// always clear *where* a command is about to run.
    pub workspace_root: String,
    /// Prompts already sent this session, oldest first, for ↑/↓ recall.
    pub prompt_history: Vec<String>,
    /// Where ↑/↓ currently sit in `prompt_history`. `None` means "not
    /// browsing" -- the input box holds whatever was typed rather than a
    /// recalled entry, which is what makes the first ↑ land on the most recent
    /// prompt instead of the second-most-recent.
    pub history_index: Option<usize>,
    /// What was in the input box when browsing started, restored by pressing ↓
    /// past the newest entry. Without it, reaching for an old prompt and
    /// changing your mind silently eats a half-written one.
    pub history_draft: String,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self {
            state: AppState::AwaitingInput,
            messages: Vec::new(),
            input_buffer: String::new(),
            cursor: 0,
            streaming_response: String::new(),
            request_id: 0,
            abort: None,
            scroll: 0,
            follow_tail: true,
            approval_selected: true,
            config,
            should_exit: false,
            greeted: false,
            overlay: None,
            overlay_input: String::new(),
            overlay_cursor: 0,
            pending_tools: VecDeque::new(),
            approved_tools: Vec::new(),
            running_tools: Vec::new(),
            tool_steps: 0,
            busy_started: None,
            streamed_chars: 0,
            workspace_status: String::new(),
            workspace_root: String::new(),
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
        }
    }

    pub fn is_busy(&self) -> bool {
        !matches!(self.state, AppState::AwaitingInput)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Terminals that support the kitty keyboard protocol also report key *releases*.
        // Without this guard every keystroke would be inserted twice.
        if key.kind == KeyEventKind::Release {
            return;
        }

        // The overlay intercepts all input while active; none of the normal
        // editing/submit logic below ever sees these keys.
        if self.overlay.is_some() {
            self.handle_overlay_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            // Enter submits, unless the buffer holds exactly a known command, in
            // which case it opens the matching overlay instead. Alt/Shift-Enter
            // (and Ctrl-Enter, on terminals that can distinguish it) insert a
            // newline instead of either.
            KeyCode::Enter => {
                if alt || shift {
                    self.insert_str("\n");
                } else {
                    match self.input_buffer.trim() {
                        "/provider" if !self.is_busy() => {
                            self.input_buffer.clear();
                            self.cursor = 0;
                            self.open_provider_picker();
                        }
                        "/model" if !self.is_busy() => {
                            self.input_buffer.clear();
                            self.cursor = 0;
                            self.open_model_picker_from_config();
                        }
                        _ => self.submit(),
                    }
                }
            }

            KeyCode::Char('u') if ctrl => {
                self.input_buffer.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                self.input_buffer.truncate(self.cursor);
            }
            KeyCode::Char('w') if ctrl => self.delete_word_before(),
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Char('j') if ctrl => self.insert_str("\n"),

            // Any other Ctrl-chord is a command, not text: never let it reach the buffer.
            KeyCode::Char(_) if ctrl => {}

            KeyCode::Char(c) => self.insert_str(&c.to_string()),
            KeyCode::Tab => self.insert_str("    "),

            KeyCode::Backspace => self.delete_before(),
            KeyCode::Delete => self.delete_after(),

            KeyCode::Left => self.cursor = self.prev_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),

            // Up/Down recall previous prompts rather than scrolling the
            // transcript -- the arrows are next to the thing you are typing, so
            // that is what they should act on. PgUp/PgDn keep the transcript.
            // Inside a multi-line prompt they move between its lines first,
            // because losing a half-written paragraph to a stray Up is worse
            // than having to press PgUp to scroll.
            KeyCode::Up => {
                if self.cursor_line() > 0 {
                    self.move_cursor_line(-1);
                } else {
                    self.recall_previous();
                }
            }
            KeyCode::PageUp => {
                self.follow_tail = false;
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::Down => {
                if self.cursor_line() + 1 < self.input_buffer.split('\n').count() {
                    self.move_cursor_line(1);
                } else {
                    self.recall_next();
                }
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
            }

            KeyCode::Esc => self.cancel(),

            _ => {}
        }
    }

    /// Bracketed paste — a multi-line paste must land in the buffer verbatim,
    /// not be interpreted as a series of Enter presses. Routed into the overlay's
    /// text field while a text-entry overlay is active (pasting an API key is
    /// the realistic common case), and ignored while a list-picker overlay is
    /// active (nothing to paste into).
    pub fn handle_paste(&mut self, text: String) {
        let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
        match &self.overlay {
            Some(Overlay::ApiKeyPrompt { .. }) | Some(Overlay::CustomEndpoint(_)) => {
                insert_into(&mut self.overlay_input, &mut self.overlay_cursor, &cleaned);
            }
            Some(_) => {}
            None => self.insert_str(&cleaned),
        }
    }

    fn submit(&mut self) {
        if self.is_busy() {
            return;
        }
        let prompt = self.input_buffer.trim().to_string();
        if prompt.is_empty() {
            // Nothing to send; clear stray whitespace so the box looks responsive.
            self.input_buffer.clear();
            self.cursor = 0;
            return;
        }

        self.input_buffer.clear();
        self.cursor = 0;
        self.greeted = true;
        self.follow_tail = true;
        self.streaming_response.clear();
        self.tool_steps = 0;
        self.busy_started = Some(std::time::Instant::now());
        self.streamed_chars = 0;
        // Recall skips consecutive duplicates: pressing Enter twice on the same
        // prompt should not mean pressing Up twice to get past it.
        if self.prompt_history.last().map(String::as_str) != Some(prompt.as_str()) {
            self.prompt_history.push(prompt.clone());
        }
        self.history_index = None;
        self.history_draft.clear();

        self.messages.push(Message::new(Role::User, prompt));
        self.state = AppState::Sending;
    }

    fn cancel(&mut self) {
        if !self.is_busy() {
            return;
        }
        if let Some(handle) = self.abort.take() {
            handle.abort();
        }
        // Bump the id so any tokens already in flight on the channel are discarded.
        self.request_id += 1;
        self.pending_tools.clear();
        self.approved_tools.clear();
        self.running_tools.clear();
        self.overlay = None;

        // Before anything else is appended: synthetic results have to sit
        // directly after the calls they answer.
        self.settle_unanswered_tool_calls("The user cancelled before this command ran.");

        let partial = std::mem::take(&mut self.streaming_response);
        if !partial.trim().is_empty() {
            self.messages.push(Message::new(
                Role::Assistant,
                format!("{partial}\n[cancelled]"),
            ));
        } else {
            self.messages
                .push(Message::new(Role::Error, "Request cancelled."));
        }
        self.busy_started = None;
        self.state = AppState::AwaitingInput;
    }

    /// The model asked to run something. Commit whatever prose it streamed
    /// alongside the request, then start asking the user about each command.
    pub fn request_tools(&mut self, calls: Vec<ToolCall>) {
        if self.state != AppState::Streaming || calls.is_empty() {
            return;
        }
        self.abort = None;
        self.follow_tail = true;
        let content = std::mem::take(&mut self.streaming_response);
        self.messages.push(Message {
            role: Role::Assistant,
            content: content.trim().to_string(),
            display: None,
            tool_calls: calls.clone(),
            tool_call_id: None,
        });
        self.pending_tools = calls.into();
        self.tool_steps += 1;
        self.advance_approvals();
    }

    /// Walk the queue until something needs a decision, or it is empty.
    ///
    /// Called once when the calls arrive and again after every keypress, so the
    /// prompt advances one command at a time. When the queue empties, the turn
    /// moves on: to `ExecutingTools` if anything was allowed, or straight back to
    /// `Sending` if everything was refused (the model still gets told, and can
    /// answer without it).
    fn advance_approvals(&mut self) {
        while let Some(call) = self.pending_tools.front() {
            // Refused outright, and never put in front of the user at all.
            // Offering `rm -rf /` as a y/n question is itself the bug: it takes
            // one mistyped keystroke to accept, and there is no undo. There is
            // deliberately no key, flag, or config value that reaches this.
            if let danger::Risk::Blocked(reason) = self.risk_of(call) {
                let call = self.pending_tools.pop_front().expect("front just matched");
                let label = tools::describe_action(&call)
                    .map(|a| a.label())
                    .unwrap_or_else(|| call.function.name.clone());
                self.messages.push(Message::new(
                    Role::Error,
                    format!("Blocked: {label}\n{reason}"),
                ));
                self.push_tool_outcome(tools::refused_as_dangerous(&call, &reason));
                self.follow_tail = true;
                continue;
            }
            if !self.needs_approval(call) {
                let call = self.pending_tools.pop_front().expect("front just matched");
                self.approved_tools.push(call);
                continue;
            }
            match tools::describe_action(call) {
                Some(action) => {
                    self.overlay = Some(Overlay::ToolApproval {
                        action,
                        remaining: self.pending_tools.len().saturating_sub(1),
                    });
                    self.approval_selected = true;
                    self.state = AppState::AwaitingApproval;
                    return;
                }
                // Nothing coherent to show, so nothing to approve. Let it through
                // to the runner, which reports the malformed arguments back to
                // the model rather than asking the user about gibberish.
                None => {
                    let call = self.pending_tools.pop_front().expect("front just matched");
                    self.approved_tools.push(call);
                }
            }
        }

        self.overlay = None;
        self.state = if self.approved_tools.is_empty() {
            AppState::Sending
        } else {
            // Snapshot for display: `main.rs` takes `approved_tools` the
            // moment it spawns the runner, so this copy is what stays on
            // screen for the rest of the run -- see the field doc.
            self.running_tools = self.approved_tools.clone();
            AppState::ExecutingTools
        };
    }

    /// What the guardrails make of this call, judged against the directory it
    /// would actually run in.
    pub fn risk_of(&self, call: &ToolCall) -> danger::Risk {
        match tools::describe_action(call) {
            Some(tools::Action::Command { command, .. }) => {
                danger::classify(&command, Path::new(&self.workspace_root))
            }
            // Reads and writes are already confined to the workspace by
            // `tools::resolve_in_workspace`, and cannot invoke a shell.
            _ => danger::Risk::Normal,
        }
    }

    /// Whether `call` needs a human decision before it runs.
    ///
    /// Order matters, and the destructive check comes first *because* it must
    /// outrank the session-wide escape hatches: "yes to everything this
    /// session" and `require_approval = false` must not silently cover
    /// `rm -rf build` an hour later. Below that, the hatches short-circuit
    /// regardless of the call, and `auto_approve_read_only` waives the prompt
    /// only for a narrow slice -- `read_file` unconditionally (it cannot write
    /// anything), and shell commands via `tools::is_read_only`. `write_file`
    /// never qualifies: unlike a shell command's read-only-ness, which has to
    /// be inferred, "this writes a file" is certain, so it always asks.
    fn needs_approval(&self, call: &ToolCall) -> bool {
        if self.risk_of(call).is_dangerous() {
            return true;
        }
        if !self.config.tools.require_approval {
            return false;
        }
        if self.config.tools.auto_approve_read_only {
            match tools::describe_action(call) {
                Some(tools::Action::Read { .. }) => return false,
                Some(tools::Action::Command { command, .. }) if tools::is_read_only(&command) => {
                    return false;
                }
                _ => {}
            }
        }
        true
    }

    /// y allow · n refuse · Esc refuse · Up/Down choose · Enter confirms the
    /// highlighted choice.
    ///
    /// Esc means refuse rather than cancel-the-turn: at a prompt asking whether
    /// to run something, the reflexive keypress has to be the safe one. y/n
    /// stay as direct shortcuts alongside arrow navigation -- picking is fine
    /// for someone reading the prompt for the first time, but a fast typist
    /// answering the tenth one in a row shouldn't be made to arrow over.
    ///
    /// There is deliberately no "allow everything from now on" key. A decision
    /// made once, while impatient, would otherwise silently cover every command
    /// for the rest of the session -- including ones the model had not thought
    /// of yet. `[tools] require_approval = false` still exists for scripted
    /// runs, where turning it off is an explicit, visible act rather than a
    /// keystroke.
    fn handle_command_approval_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.approval_selected = !self.approval_selected;
            return;
        }

        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            KeyCode::Enter => Some(self.approval_selected),
            _ => None,
        };

        let Some(allowed) = decision else {
            return; // unrecognised key: leave the prompt exactly as it was
        };
        let Some(call) = self.pending_tools.pop_front() else {
            return;
        };

        if allowed {
            self.approved_tools.push(call);
        } else {
            self.push_tool_outcome(tools::declined(&call));
        }
        self.follow_tail = true;
        self.advance_approvals();
    }

    /// Results of the commands that ran, from the spawned runner.
    pub fn finish_tools(&mut self, outcomes: Vec<ToolOutcome>) {
        if self.state != AppState::ExecutingTools {
            return;
        }
        for outcome in outcomes {
            self.push_tool_outcome(outcome);
        }
        self.running_tools.clear();
        self.follow_tail = true;
        // Back around: the model needs a turn to use what it just got.
        self.state = AppState::Sending;
    }

    pub fn push_tool_outcome(&mut self, outcome: ToolOutcome) {
        self.messages.push(Message {
            role: Role::Tool,
            content: outcome.content,
            display: Some(outcome.display),
            tool_calls: Vec::new(),
            tool_call_id: Some(outcome.call_id),
        });
    }

    /// Answer every tool call that never got a result.
    ///
    /// Providers require each `tool_calls` entry to be matched by a `tool` message
    /// quoting its id. A turn abandoned mid-loop -- Esc, or a failed request --
    /// otherwise leaves a hole, and the resulting 400 lands on the user's *next*
    /// prompt, where it looks like an unrelated failure. So the hole gets filled
    /// rather than left.
    fn settle_unanswered_tool_calls(&mut self, reason: &str) {
        let answered: HashSet<&str> = self
            .messages
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();

        let unanswered: Vec<ToolCall> = self
            .messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .filter(|call| !answered.contains(call.id.as_str()))
            .cloned()
            .collect();

        for call in unanswered {
            self.push_tool_outcome(tools::unanswered(&call, reason));
        }
    }

    /// A note from the transport that is neither the model talking nor a
    /// failure -- currently only "your answer was truncated". Pushed as a
    /// System message so it reads as status, and kept out of `history` so the
    /// model is never told about our own plumbing.
    pub fn note(&mut self, note: String) {
        self.messages.push(Message::new(Role::System, note));
        self.follow_tail = true;
    }

    pub fn append_token(&mut self, token: &str) {
        if self.state == AppState::Streaming {
            self.streaming_response.push_str(token);
            self.streamed_chars += token.chars().count();
        }
    }

    /// Terminates the turn. Deliberately a no-op unless still `Streaming`: a
    /// response carrying tool calls sends `ToolCalls` and *then* `Done`, and by
    /// the time `Done` arrives the turn has moved on to `ExecutingTools`.
    pub fn finish_stream(&mut self) {
        if self.state != AppState::Streaming {
            return;
        }
        self.abort = None;
        let response = std::mem::take(&mut self.streaming_response);
        if response.trim().is_empty() {
            self.messages.push(Message::new(
                Role::Error,
                "The endpoint returned an empty response.",
            ));
        } else {
            self.messages.push(Message::new(Role::Assistant, response));
        }
        self.busy_started = None;
        self.state = AppState::AwaitingInput;
    }

    pub fn fail_stream(&mut self, error: String) {
        self.abort = None;
        self.pending_tools.clear();
        self.approved_tools.clear();
        self.running_tools.clear();
        self.overlay = None;
        // First, so the results land against the calls they belong to.
        self.settle_unanswered_tool_calls("The request failed before this command ran.");

        let partial = std::mem::take(&mut self.streaming_response);
        if !partial.trim().is_empty() {
            self.messages.push(Message::new(Role::Assistant, partial));
        }
        self.messages.push(Message::new(Role::Error, error));
        self.busy_started = None;
        self.state = AppState::AwaitingInput;
    }

    /// Conversation so far, in wire form.
    ///
    /// Error and System messages are local commentary and never sent. Everything
    /// else must survive intact, tool calls included -- an assistant message whose
    /// `tool_calls` were dropped here would leave the following `tool` messages
    /// answering nothing.
    pub fn history(&self, system: Option<&str>) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        if let Some(system) = system {
            out.push(ChatMessage::text("system", system));
        }
        for message in &self.messages {
            match message.role {
                Role::User => out.push(ChatMessage::text("user", message.content.clone())),
                Role::Assistant => out.push(ChatMessage {
                    role: "assistant".to_string(),
                    // None rather than "" when the model only asked for tools.
                    content: Some(message.content.clone()).filter(|c| !c.trim().is_empty()),
                    tool_calls: message.tool_calls.clone(),
                    tool_call_id: None,
                }),
                Role::Tool => out.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(message.content.clone()),
                    tool_calls: Vec::new(),
                    tool_call_id: message.tool_call_id.clone(),
                }),
                Role::Error | Role::System => {}
            }
        }
        out
    }

    // ---- input buffer editing -------------------------------------------------
    // Thin wrappers around the free functions below, which are also used by the
    // overlay's single-line text entry (see `handle_api_key_prompt_key` /
    // `handle_custom_endpoint_key`) so the UTF-8-boundary-safe logic exists once.

    fn insert_str(&mut self, s: &str) {
        insert_into(&mut self.input_buffer, &mut self.cursor, s);
    }

    fn delete_before(&mut self) {
        delete_before_in(&mut self.input_buffer, &mut self.cursor);
    }

    fn delete_after(&mut self) {
        delete_after_in(&mut self.input_buffer, &mut self.cursor);
    }

    fn delete_word_before(&mut self) {
        let head = &self.input_buffer[..self.cursor];
        let trimmed = head.trim_end_matches(|c: char| c.is_whitespace());
        let start = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0);
        self.input_buffer.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Previous char boundary (byte index), saturating at 0.
    fn prev_boundary(&self) -> usize {
        prev_char_boundary(&self.input_buffer, self.cursor)
    }

    /// Next char boundary (byte index), saturating at the end of the buffer.
    fn next_boundary(&self) -> usize {
        next_char_boundary(&self.input_buffer, self.cursor)
    }

    /// Which line of a multi-line prompt the caret is on.
    fn cursor_line(&self) -> usize {
        self.cursor_position().0
    }

    /// Move the caret one line up or down inside the prompt, keeping its column
    /// where it can. Only called when such a line exists, so `delta` never runs
    /// off either end.
    fn move_cursor_line(&mut self, delta: isize) {
        let (row, col) = self.cursor_position();
        let target = if delta < 0 { row.saturating_sub(1) } else { row + 1 };

        let lines: Vec<&str> = self.input_buffer.split('\n').collect();
        let Some(line) = lines.get(target) else { return };

        // Byte offset of the target line, plus `col` characters into it (or the
        // end of it, when the target line is shorter than the current column).
        let mut offset = 0usize;
        for l in &lines[..target] {
            offset += l.len() + 1; // +1 for the '\n'
        }
        let within: usize = line
            .char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        self.cursor = offset + within;
    }

    /// ↑ -- step back through prompts already sent.
    fn recall_previous(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        let next = match self.history_index {
            // First press: remember what was being typed, then jump to the
            // newest entry.
            None => {
                self.history_draft = self.input_buffer.clone();
                self.prompt_history.len() - 1
            }
            Some(0) => return, // already at the oldest
            Some(i) => i - 1,
        };
        self.history_index = Some(next);
        self.set_input(self.prompt_history[next].clone());
    }

    /// ↓ -- step forward, ending back at whatever was being typed.
    fn recall_next(&mut self) {
        let Some(current) = self.history_index else {
            return;
        };
        if current + 1 < self.prompt_history.len() {
            self.history_index = Some(current + 1);
            self.set_input(self.prompt_history[current + 1].clone());
        } else {
            self.history_index = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.set_input(draft);
        }
    }

    /// Replace the prompt, caret at the end -- where you want it when a
    /// recalled prompt is about to be edited or resent.
    fn set_input(&mut self, text: String) {
        self.cursor = text.len();
        self.input_buffer = text;
    }

    fn line_start(&self) -> usize {
        self.input_buffer[..self.cursor]
            .rfind('\n')
            .map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.input_buffer[self.cursor..]
            .find('\n')
            .map_or(self.input_buffer.len(), |i| self.cursor + i)
    }

    /// (row, column) of the cursor within the input buffer, counting characters.
    pub fn cursor_position(&self) -> (usize, usize) {
        let head = &self.input_buffer[..self.cursor];
        let row = head.matches('\n').count();
        let col = head[head.rfind('\n').map_or(0, |i| i + 1)..].chars().count();
        (row, col)
    }

    // ---- /provider and /model overlays -----------------------------------------

    fn open_provider_picker(&mut self) {
        self.overlay = Some(Overlay::ProviderPicker { selected: 0 });
    }

    /// Entry point for standalone `/model` (no fresh `/provider` first) — scopes
    /// to whichever provider is already in `config.llm.provider`, if any.
    fn open_model_picker_from_config(&mut self) {
        match providers::find_provider(&self.config.llm.provider) {
            Some(provider) => {
                self.overlay = Some(Overlay::ModelPicker {
                    provider_id: provider.id,
                    selected: 0,
                });
            }
            None => {
                self.messages.push(Message::new(
                    Role::Error,
                    "No provider configured yet. Run /provider first.",
                ));
            }
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        let overlay = match self.overlay.take() {
            Some(o) => o,
            None => return,
        };
        match overlay {
            Overlay::ProviderPicker { selected } => self.handle_provider_picker_key(key, selected),
            Overlay::ModelPicker {
                provider_id,
                selected,
            } => self.handle_model_picker_key(key, provider_id, selected),
            Overlay::ApiKeyPrompt { provider_id, model } => {
                self.handle_api_key_prompt_key(key, provider_id, model)
            }
            Overlay::CustomEndpoint(step) => self.handle_custom_endpoint_key(key, step),
            // Put back first: an unrecognised key must leave the prompt standing
            // rather than silently dismissing it, and `handle_overlay_key` took
            // the overlay before dispatching here.
            approval @ Overlay::ToolApproval { .. } => {
                self.overlay = Some(approval);
                self.handle_command_approval_key(key);
            }
        }
    }

    fn handle_provider_picker_key(&mut self, key: KeyEvent, selected: usize) {
        // +1 for the trailing "Custom endpoint..." entry.
        let last = providers::PROVIDERS.len();
        match key.code {
            KeyCode::Up => {
                self.overlay = Some(Overlay::ProviderPicker {
                    selected: selected.saturating_sub(1),
                });
            }
            KeyCode::Down => {
                self.overlay = Some(Overlay::ProviderPicker {
                    selected: (selected + 1).min(last),
                });
            }
            KeyCode::Esc => {}
            KeyCode::Enter => {
                if selected < last {
                    let provider = &providers::PROVIDERS[selected];
                    self.overlay = Some(Overlay::ModelPicker {
                        provider_id: provider.id,
                        selected: 0,
                    });
                } else {
                    self.overlay = Some(Overlay::CustomEndpoint(CustomStep::Endpoint));
                }
            }
            _ => {
                self.overlay = Some(Overlay::ProviderPicker { selected });
            }
        }
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent, provider_id: &'static str, selected: usize) {
        let provider = providers::find_provider(provider_id)
            .expect("provider_id on a ModelPicker overlay always names a registry entry");
        let last = provider.models.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                self.overlay = Some(Overlay::ModelPicker {
                    provider_id,
                    selected: selected.saturating_sub(1),
                });
            }
            KeyCode::Down => {
                self.overlay = Some(Overlay::ModelPicker {
                    provider_id,
                    selected: (selected + 1).min(last),
                });
            }
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let model = provider.models[selected].to_string();
                let env_name = providers::env_var_name(provider_id);
                let env_key = std::env::var(&env_name)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty());

                if let Some(api_key) = env_key {
                    self.apply_llm_config(
                        provider_id.to_string(),
                        provider.endpoint.to_string(),
                        model,
                        api_key,
                    );
                } else {
                    self.overlay = Some(Overlay::ApiKeyPrompt { provider_id, model });
                }
            }
            _ => {
                self.overlay = Some(Overlay::ModelPicker { provider_id, selected });
            }
        }
    }

    fn handle_api_key_prompt_key(&mut self, key: KeyEvent, provider_id: &'static str, model: String) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.overlay_input.clear();
                self.overlay_cursor = 0;
            }
            KeyCode::Enter => {
                let provider = providers::find_provider(provider_id)
                    .expect("provider_id on an ApiKeyPrompt overlay always names a registry entry");
                let api_key = self.overlay_input.trim().to_string();
                self.overlay_input.clear();
                self.overlay_cursor = 0;
                if api_key.is_empty() {
                    self.messages
                        .push(Message::new(Role::Error, "No API key entered; cancelled."));
                    return;
                }
                self.apply_llm_config(
                    provider_id.to_string(),
                    provider.endpoint.to_string(),
                    model,
                    api_key,
                );
            }
            KeyCode::Backspace => {
                delete_before_in(&mut self.overlay_input, &mut self.overlay_cursor);
                self.overlay = Some(Overlay::ApiKeyPrompt { provider_id, model });
            }
            KeyCode::Char(c) if !ctrl => {
                insert_into(&mut self.overlay_input, &mut self.overlay_cursor, &c.to_string());
                self.overlay = Some(Overlay::ApiKeyPrompt { provider_id, model });
            }
            _ => {
                self.overlay = Some(Overlay::ApiKeyPrompt { provider_id, model });
            }
        }
    }

    fn handle_custom_endpoint_key(&mut self, key: KeyEvent, step: CustomStep) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.overlay_input.clear();
                self.overlay_cursor = 0;
            }
            KeyCode::Enter => {
                let value = self.overlay_input.trim().to_string();
                self.overlay_input.clear();
                self.overlay_cursor = 0;
                match step {
                    CustomStep::Endpoint => {
                        if value.is_empty() {
                            self.messages.push(Message::new(
                                Role::Error,
                                "Endpoint cannot be empty; cancelled.",
                            ));
                            return;
                        }
                        self.overlay = Some(Overlay::CustomEndpoint(CustomStep::Model { endpoint: value }));
                    }
                    CustomStep::Model { endpoint } => {
                        if value.is_empty() {
                            self.messages.push(Message::new(
                                Role::Error,
                                "Model cannot be empty; cancelled.",
                            ));
                            return;
                        }
                        self.overlay = Some(Overlay::CustomEndpoint(CustomStep::ApiKey {
                            endpoint,
                            model: value,
                        }));
                    }
                    // The API key may legitimately be blank for some local, unauthenticated servers.
                    CustomStep::ApiKey { endpoint, model } => {
                        self.apply_llm_config(String::new(), endpoint, model, value);
                    }
                }
            }
            KeyCode::Backspace => {
                delete_before_in(&mut self.overlay_input, &mut self.overlay_cursor);
                self.overlay = Some(Overlay::CustomEndpoint(step));
            }
            KeyCode::Char(c) if !ctrl => {
                insert_into(&mut self.overlay_input, &mut self.overlay_cursor, &c.to_string());
                self.overlay = Some(Overlay::CustomEndpoint(step));
            }
            _ => {
                self.overlay = Some(Overlay::CustomEndpoint(step));
            }
        }
    }

    /// Single completion path for every overlay flow (env-var shortcut, masked
    /// prompt, custom wizard). Updates the in-memory config -- which `main.rs`'s
    /// event loop re-reads fresh on every `Sending` transition, so this takes
    /// effect on the very next request with no restart needed -- and persists it.
    ///
    /// Any test that reaches this function MUST wrap the call in
    /// `config::test_support::with_isolated_home`, or it will write to the real
    /// developer/CI `~/.tuisample-code/config.toml`.
    fn apply_llm_config(&mut self, provider: String, endpoint: String, model: String, api_key: String) {
        self.config.llm.provider = provider;
        self.config.llm.endpoint = endpoint;
        self.config.llm.model = model;
        self.config.llm.api_key = api_key;

        let label = if self.config.llm.provider.is_empty() {
            self.config.llm.endpoint.as_str()
        } else {
            self.config.llm.provider.as_str()
        };
        let message = match self.config.save() {
            Ok(()) => Message::new(
                Role::System,
                format!("Switched to {label} / {}.", self.config.llm.model),
            ),
            Err(e) => Message::new(
                Role::Error,
                format!("Using it for this session, but failed to save to config.toml: {e}"),
            ),
        };
        self.messages.push(message);
        self.overlay = None;
        self.overlay_input.clear();
        self.overlay_cursor = 0;
    }
}

/// Previous char boundary (byte index) in `s` before `cursor`, saturating at 0.
/// Shared by `input_buffer` and `overlay_input` editing.
fn prev_char_boundary(s: &str, cursor: usize) -> usize {
    s[..cursor]
        .chars()
        .next_back()
        .map_or(0, |c| cursor - c.len_utf8())
}

/// Next char boundary (byte index) in `s` after `cursor`, saturating at `s.len()`.
fn next_char_boundary(s: &str, cursor: usize) -> usize {
    s[cursor..]
        .chars()
        .next()
        .map_or(cursor, |c| cursor + c.len_utf8())
}

fn insert_into(buf: &mut String, cursor: &mut usize, s: &str) {
    buf.insert_str(*cursor, s);
    *cursor += s.len();
}

fn delete_before_in(buf: &mut String, cursor: &mut usize) {
    let prev = prev_char_boundary(buf, *cursor);
    if prev != *cursor {
        buf.drain(prev..*cursor);
        *cursor = prev;
    }
}

fn delete_after_in(buf: &mut String, cursor: &mut usize) {
    let next = next_char_boundary(buf, *cursor);
    if next != *cursor {
        buf.drain(*cursor..next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::with_isolated_home;
    use crate::config::Config;
    use crate::providers;
    use std::sync::Mutex;

    /// Serializes tests that mutate DEEPSEEK_API_KEY -- it's global process
    /// state, so two tests toggling it concurrently would race (mirrors
    /// config::test_support::HOME_LOCK's reasoning for $HOME).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// The existing approval-flow tests below use `ls`/`cat`/`pwd` purely as
    /// stand-ins for "some command" and assert every one of them stops for a
    /// human decision -- that is what they are testing, not read-only
    /// classification, which gets its own dedicated tests further down. So the
    /// fixture turns the read-only fast path off by default; those tests would
    /// otherwise start silently skipping their own approval prompts the moment
    /// their example command happened to match `tools::is_read_only`.
    fn app() -> App {
        let mut app = App::new(Config::default());
        app.config.tools.auto_approve_read_only = false;
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    /// The reported bug: typing a prompt and pressing Enter did nothing, because
    /// submission required Ctrl-Enter, which terminals cannot send.
    #[test]
    fn plain_enter_submits_the_prompt() {
        let mut a = app();
        type_str(&mut a, "hello world");
        assert_eq!(a.input_buffer, "hello world");

        a.handle_key(key(KeyCode::Enter));

        assert_eq!(a.state, AppState::Sending);
        assert!(a.input_buffer.is_empty());
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].content, "hello world");
    }

    /// The footer's elapsed-time display reads `busy_started`; it must be set
    /// the moment a turn begins and cleared on every path back to idle, or
    /// the clock would either never start or keep running after the turn
    /// that started it is long over.
    #[test]
    fn submitting_a_prompt_starts_the_busy_timer_and_resets_the_token_estimate() {
        let mut a = app();
        assert!(a.busy_started.is_none());

        type_str(&mut a, "hello");
        a.handle_key(key(KeyCode::Enter));

        assert!(a.busy_started.is_some());
        assert_eq!(a.streamed_chars, 0);
    }

    #[test]
    fn streaming_tokens_accumulate_the_character_count() {
        let mut a = streaming_app();
        a.append_token("Hello, ");
        a.append_token("world!");
        assert_eq!(a.streamed_chars, "Hello, world!".chars().count());
    }

    #[test]
    fn the_busy_timer_clears_when_a_turn_ends_however_it_ends() {
        let mut finished = streaming_app();
        finished.append_token("hi");
        finished.finish_stream();
        assert!(finished.busy_started.is_none());

        let mut failed = streaming_app();
        failed.fail_stream("boom".to_string());
        assert!(failed.busy_started.is_none());

        let mut cancelled = streaming_app();
        cancelled.cancel();
        assert!(cancelled.busy_started.is_none());
    }

    #[test]
    fn ctrl_enter_still_submits_where_the_terminal_reports_it() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(a.state, AppState::Sending);
    }

    #[test]
    fn alt_enter_inserts_a_newline_instead_of_sending() {
        let mut a = app();
        type_str(&mut a, "line1");
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        type_str(&mut a, "line2");

        assert_eq!(a.input_buffer, "line1\nline2");
        assert_eq!(a.state, AppState::AwaitingInput);

        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::Sending);
        assert_eq!(a.messages[0].content, "line1\nline2");
    }

    #[test]
    fn empty_or_whitespace_prompt_is_not_sent() {
        let mut a = app();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::AwaitingInput);

        type_str(&mut a, "   ");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a.messages.is_empty());
    }

    #[test]
    fn key_release_events_do_not_double_type() {
        let mut a = app();
        let press = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let mut release = press;
        release.kind = KeyEventKind::Release;

        a.handle_key(press);
        a.handle_key(release);

        assert_eq!(a.input_buffer, "x");
    }

    #[test]
    fn ctrl_chords_never_leak_into_the_buffer() {
        let mut a = app();
        for c in ['a', 'b', 'z', 'l'] {
            a.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        }
        assert!(a.input_buffer.is_empty());
    }

    #[test]
    fn editing_keys_are_utf8_safe() {
        let mut a = app();
        type_str(&mut a, "héllo→");
        a.handle_key(key(KeyCode::Backspace));
        assert_eq!(a.input_buffer, "héllo");

        a.handle_key(key(KeyCode::Home));
        assert_eq!(a.cursor, 0);
        a.handle_key(key(KeyCode::Right));
        a.handle_key(key(KeyCode::Right));
        a.handle_key(key(KeyCode::Delete));
        assert_eq!(a.input_buffer, "hélo");

        a.handle_key(key(KeyCode::End));
        assert_eq!(a.cursor, a.input_buffer.len());
    }

    #[test]
    fn ctrl_w_deletes_the_previous_word() {
        let mut a = app();
        type_str(&mut a, "write a hello world");
        a.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(a.input_buffer, "write a hello ");
    }

    #[test]
    fn pasted_multiline_text_stays_in_the_buffer() {
        let mut a = app();
        a.handle_paste("fn main() {\r\n    println!(\"hi\");\r\n}".to_string());
        assert_eq!(a.input_buffer, "fn main() {\n    println!(\"hi\");\n}");
        assert_eq!(a.state, AppState::AwaitingInput);
    }

    #[test]
    fn cannot_submit_a_second_prompt_while_streaming() {
        let mut a = app();
        type_str(&mut a, "one");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Streaming;

        type_str(&mut a, "two");
        a.handle_key(key(KeyCode::Enter));

        assert_eq!(a.state, AppState::Streaming);
        assert_eq!(a.messages.len(), 1);
    }

    #[test]
    fn stream_completion_commits_the_response_and_returns_to_ready() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Streaming;

        a.append_token("Hel");
        a.append_token("lo!");
        a.finish_stream();

        assert_eq!(a.state, AppState::AwaitingInput);
        assert_eq!(a.messages.len(), 2);
        assert_eq!(a.messages[1].content, "Hello!");
        assert!(a.messages[1].role == Role::Assistant);
    }

    #[test]
    fn errors_surface_in_the_transcript_and_unblock_input() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Streaming;

        a.fail_stream("HTTP 401 Unauthorized".to_string());

        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a.messages.iter().any(|m| m.role == Role::Error
            && m.content.contains("401")));

        // The user can immediately try again.
        type_str(&mut a, "retry");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::Sending);
    }

    #[test]
    fn esc_cancels_and_keeps_partial_output() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Streaming;
        a.append_token("partial");

        a.handle_key(key(KeyCode::Esc));

        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a.messages.last().unwrap().content.contains("partial"));
    }

    #[test]
    fn history_carries_the_conversation_and_drops_errors() {
        let mut a = app();
        type_str(&mut a, "first");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Streaming;
        a.append_token("answer");
        a.finish_stream();
        a.fail_stream("boom".to_string());

        type_str(&mut a, "second");
        a.handle_key(key(KeyCode::Enter));

        let history = a.history(None);
        assert_eq!(
            history,
            vec![
                ChatMessage::text("user", "first"),
                ChatMessage::text("assistant", "answer"),
                ChatMessage::text("user", "second"),
            ]
        );
    }

    #[test]
    fn the_system_prompt_is_prepended_when_one_is_given() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));

        let history = a.history(Some("you are a robot"));
        assert_eq!(history[0], ChatMessage::text("system", "you are a robot"));
        assert_eq!(history[1].role, "user");
    }

    // ---- commands and approval -----------------------------------------------

    fn command_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: crate::llm::FunctionCall {
                name: crate::tools::RUN_COMMAND.to_string(),
                arguments: serde_json::json!({ "command": command }).to_string(),
            },
        }
    }

    fn read_file_call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: crate::llm::FunctionCall {
                name: crate::tools::READ_FILE.to_string(),
                arguments: serde_json::json!({ "path": path }).to_string(),
            },
        }
    }

    fn write_file_call(id: &str, path: &str, content: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: crate::llm::FunctionCall {
                name: crate::tools::WRITE_FILE.to_string(),
                arguments: serde_json::json!({ "path": path, "content": content }).to_string(),
            },
        }
    }

    fn outcome(call_id: &str, content: &str) -> ToolOutcome {
        ToolOutcome {
            call_id: call_id.to_string(),
            display: format!("$ … — {content}"),
            content: content.to_string(),
        }
    }

    fn streaming_app() -> App {
        let mut a = app();
        a.workspace_root = "/tmp/project".to_string();
        type_str(&mut a, "what does main.rs do?");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Streaming;
        a
    }

    /// Nothing runs until a human says so. If this ever regresses, the model has
    /// an unattended shell.
    #[test]
    fn a_command_is_not_runnable_until_it_is_approved() {
        let mut a = streaming_app();
        a.append_token("Let me look.");
        a.request_tools(vec![command_call("call_1", "cat src/main.rs")]);

        assert_eq!(a.state, AppState::AwaitingApproval);
        assert!(
            a.approved_tools.is_empty(),
            "nothing may reach the runner before approval"
        );
        assert_eq!(a.tool_steps, 1);

        match &a.overlay {
            Some(Overlay::ToolApproval { action: tools::Action::Command { command, .. }, .. }) => {
                assert_eq!(command, "cat src/main.rs")
            }
            other => panic!("expected an approval prompt, got {other:?}"),
        }
        // The prose streamed alongside the call is kept.
        assert_eq!(a.messages.last().unwrap().content, "Let me look.");
    }

    #[test]
    fn pressing_y_releases_the_command_to_the_runner() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.handle_key(key(KeyCode::Char('y')));

        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.approved_tools.len(), 1);
        assert_eq!(a.overlay, None);
    }

    /// A fresh prompt starts on "yes" so bare Enter keeps its long-standing
    /// meaning; Down moves the highlight to "no" without deciding anything.
    #[test]
    fn a_fresh_approval_prompt_starts_selected_on_yes() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        assert!(a.approval_selected);

        a.handle_key(key(KeyCode::Down));
        assert!(!a.approval_selected, "Down must move the highlight");
        assert_eq!(a.state, AppState::AwaitingApproval, "arrows alone must not decide anything");
        assert_eq!(a.pending_tools.len(), 1, "nothing should have been popped yet");
    }

    /// Enter confirms whichever choice is currently highlighted -- not always
    /// "yes" -- once Up/Down has moved off the default.
    #[test]
    fn enter_confirms_the_highlighted_choice_not_always_yes() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "rm -rf build")]);
        a.handle_key(key(KeyCode::Down)); // move to "no"
        a.handle_key(key(KeyCode::Enter));

        assert!(a.approved_tools.is_empty(), "Enter on \"no\" must decline, not approve");
        let told = a.messages.last().unwrap();
        assert!(told.content.contains("declined"), "{}", told.content);
    }

    /// Up and Down only ever move between the two choices here -- there's
    /// nothing to wrap past -- so either key from either state lands on the
    /// other choice.
    #[test]
    fn up_and_down_both_toggle_between_the_two_choices() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);

        a.handle_key(key(KeyCode::Up));
        assert!(!a.approval_selected);
        a.handle_key(key(KeyCode::Up));
        assert!(a.approval_selected);
        a.handle_key(key(KeyCode::Down));
        assert!(!a.approval_selected);
    }

    /// y/n remain direct shortcuts regardless of where the highlight is --
    /// someone who already knows their answer shouldn't have to arrow over.
    #[test]
    fn y_and_n_still_work_directly_regardless_of_the_highlight() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.handle_key(key(KeyCode::Down)); // highlight is now on "no"
        a.handle_key(key(KeyCode::Char('y'))); // but 'y' still means yes

        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.approved_tools.len(), 1);
    }

    /// Each new prompt resets to "yes", regardless of where the previous one
    /// was left -- a run of approvals shouldn't inherit a stale highlight.
    #[test]
    fn a_new_prompt_resets_the_highlight_even_if_the_previous_one_left_it_on_no() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.handle_key(key(KeyCode::Down));
        a.handle_key(key(KeyCode::Char('n')));

        a.state = AppState::Streaming;
        a.request_tools(vec![command_call("call_2", "pwd")]);
        assert!(a.approval_selected, "the new prompt must start back on \"yes\"");
    }

    /// Regression: `main.rs` takes `approved_tools` (empties it) the instant
    /// it spawns the runner task, so a "Running N commands…" display reading
    /// straight off `approved_tools` would show N for one frame and then
    /// nothing for the rest of the run, while commands were still executing.
    /// `running_tools` is the snapshot that stays put until the run finishes.
    #[test]
    fn approving_a_command_snapshots_it_for_display_independent_of_approved_tools() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.handle_key(key(KeyCode::Char('y')));
        assert_eq!(a.running_tools.len(), 1);

        // Simulate what main.rs does the moment it spawns the runner.
        a.approved_tools.clear();
        assert_eq!(a.running_tools.len(), 1, "the snapshot must survive approved_tools being taken");

        a.finish_tools(vec![outcome("call_1", "ok")]);
        assert!(a.running_tools.is_empty(), "the snapshot must clear once the run is over");
    }

    /// Esc at an approval prompt means "no", not "cancel the turn": the
    /// reflexive keypress has to be the safe one.
    #[test]
    fn esc_refuses_the_command_rather_than_cancelling_the_turn() {
        for refuse in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut a = streaming_app();
            // Dangerous but not blocked: this test is about the *prompt*, and a
            // blocked command never reaches one.
            a.request_tools(vec![command_call("call_1", "rm -rf build")]);
            a.handle_key(key(refuse));

            assert!(a.approved_tools.is_empty(), "{refuse:?} must not run anything");
            // The model is told, so it can take another route.
            let told = a.messages.last().unwrap();
            assert_eq!(told.tool_call_id.as_deref(), Some("call_1"));
            assert!(told.content.contains("declined"), "{}", told.content);
            assert_eq!(a.state, AppState::Sending);
            assert_history_is_well_formed(&a.history(None));
        }
    }

    #[test]
    fn each_queued_command_is_asked_about_separately() {
        let mut a = streaming_app();
        a.request_tools(vec![
            command_call("call_1", "ls"),
            command_call("call_2", "cat Cargo.toml"),
        ]);

        match &a.overlay {
            Some(Overlay::ToolApproval { action: tools::Action::Command { command, .. }, remaining }) => {
                assert_eq!(command, "ls");
                assert_eq!(*remaining, 1);
            }
            other => panic!("expected the first prompt, got {other:?}"),
        }

        a.handle_key(key(KeyCode::Char('y')));
        match &a.overlay {
            Some(Overlay::ToolApproval { action: tools::Action::Command { command, .. }, remaining }) => {
                assert_eq!(command, "cat Cargo.toml");
                assert_eq!(*remaining, 0);
            }
            other => panic!("expected the second prompt, got {other:?}"),
        }

        a.handle_key(key(KeyCode::Char('n')));
        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.approved_tools.len(), 1); // only `ls`
    }

    #[test]
    fn a_stray_keypress_leaves_the_prompt_standing() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);

        for stray in [KeyCode::Char('q'), KeyCode::Down, KeyCode::Backspace] {
            a.handle_key(key(stray));
            assert_eq!(a.state, AppState::AwaitingApproval, "{stray:?} dismissed the prompt");
            assert!(a.overlay.is_some(), "{stray:?} dismissed the prompt");
        }
    }

    // ---- destructive-command guardrails -------------------------------------

    /// The whole point of the blocked tier: it is never put in front of the
    /// user as a y/n question, because one mistyped keystroke would accept it.
    #[test]
    fn a_catastrophic_command_is_refused_without_ever_prompting() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "rm -rf /")]);

        assert_eq!(a.overlay, None, "must never be offered for approval");
        assert!(a.approved_tools.is_empty(), "must never reach the runner");
        assert_eq!(a.state, AppState::Sending);

        let told = a.messages.last().unwrap();
        assert_eq!(told.tool_call_id.as_deref(), Some("call_1"));
        assert!(told.content.contains("Blocked"), "{}", told.content);
        assert!(
            told.content.contains("no setting can permit it"),
            "the model must be told this is settled: {}",
            told.content
        );
        assert_history_is_well_formed(&a.history(None));
    }

    /// The bypasses are the reason this feature exists. Before it,
    /// `require_approval = false` made `needs_approval` return false for
    /// *everything*, `rm -rf /` included.
    #[test]
    fn no_setting_can_unblock_a_catastrophic_command() {
        type Bypass = (&'static str, fn(&mut App));
        let bypasses: [Bypass; 3] = [
            ("unattended mode", |a| {
                a.config.tools.require_approval = false
            }),
            ("read-only fast path", |a| {
                a.config.tools.auto_approve_read_only = true
            }),
            ("both at once", |a| {
                a.config.tools.require_approval = false;
                a.config.tools.auto_approve_read_only = true;
            }),
        ];

        for (label, setup) in bypasses {
            let mut a = streaming_app();
            setup(&mut a);
            a.request_tools(vec![command_call("call_1", "sudo rm -rf /")]);

            assert!(
                a.approved_tools.is_empty(),
                "{label} let a blocked command through"
            );
            assert_eq!(a.overlay, None, "{label} turned it into a prompt");
        }
    }

    /// The other half: a destructive-but-legitimate command must still stop,
    /// even with approval switched off entirely.
    #[test]
    fn dangerous_commands_still_ask_in_unattended_mode() {
        let mut a = streaming_app();
        a.config.tools.require_approval = false;

        a.request_tools(vec![command_call("call_1", "rm -rf build")]);

        assert_eq!(
            a.state,
            AppState::AwaitingApproval,
            "`rm -rf build` must not ride the unattended fast path"
        );
        assert!(a.overlay.is_some());
    }

    /// ...while ordinary work is untouched by any of this.
    #[test]
    fn ordinary_commands_are_unaffected_by_the_guardrails() {
        let mut a = streaming_app();
        a.config.tools.require_approval = false;
        a.request_tools(vec![command_call("call_1", "cargo build")]);

        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.approved_tools.len(), 1);
    }

    /// A blocked call still has to be answered, or the next prompt 400s.
    #[test]
    fn a_blocked_call_mixed_with_a_normal_one_leaves_a_valid_history() {
        let mut a = streaming_app();
        a.config.tools.require_approval = false;
        a.request_tools(vec![
            command_call("call_1", "rm -rf /"),
            command_call("call_2", "ls"),
        ]);

        assert_eq!(a.approved_tools.len(), 1, "only `ls` may run");
        a.state = AppState::ExecutingTools;
        a.finish_tools(vec![outcome("call_2", "ok")]);
        assert_history_is_well_formed(&a.history(None));
    }

    /// "Allow everything from now on" was removed deliberately. `a` is now an
    /// ordinary unrecognised key, which means the prompt stays up rather than
    /// being dismissed -- a stray keystroke must never be read as consent.
    #[test]
    fn there_is_no_key_that_approves_everything_for_the_session() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);

        for stray in [KeyCode::Char('a'), KeyCode::Char('A')] {
            a.handle_key(key(stray));
            assert_eq!(
                a.state,
                AppState::AwaitingApproval,
                "{stray:?} approved something"
            );
            assert!(a.approved_tools.is_empty(), "{stray:?} approved something");
            assert!(a.overlay.is_some(), "{stray:?} dismissed the prompt");
        }

        // Each later command is asked about on its own, with no memory of past
        // answers.
        a.handle_key(key(KeyCode::Char('y')));
        a.finish_tools(vec![outcome("call_1", "ok")]);
        a.state = AppState::Streaming;
        a.request_tools(vec![command_call("call_2", "cat Cargo.toml")]);
        assert_eq!(a.state, AppState::AwaitingApproval, "the second command must ask too");
    }

    #[test]
    fn approval_is_skipped_entirely_when_the_config_turns_it_off() {
        let mut a = app();
        a.config.tools.require_approval = false;
        type_str(&mut a, "go");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Streaming;

        a.request_tools(vec![command_call("call_1", "ls")]);
        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.overlay, None);
    }

    #[test]
    fn read_only_commands_skip_the_prompt_when_the_fast_path_is_on() {
        let mut a = streaming_app();
        a.config.tools.auto_approve_read_only = true;
        a.request_tools(vec![command_call("call_1", "cat src/main.rs")]);

        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.overlay, None);
        assert_eq!(a.approved_tools.len(), 1);
    }

    /// The fast path is narrow on purpose: it must not become a second way to
    /// turn approval off entirely.
    #[test]
    fn non_read_only_commands_still_ask_even_with_the_fast_path_on() {
        let mut a = streaming_app();
        a.config.tools.auto_approve_read_only = true;
        a.request_tools(vec![command_call("call_1", "rm -rf build")]);

        assert_eq!(a.state, AppState::AwaitingApproval);
        assert!(a.approved_tools.is_empty());
    }

    /// A read-only call chained into something else (via `;`, `|`, `&&`, ...)
    /// must not ride the fast path just because it starts with `cat`/`ls`/etc.
    #[test]
    fn a_read_only_prefix_chained_into_something_else_still_asks() {
        let mut a = streaming_app();
        a.config.tools.auto_approve_read_only = true;
        // Chained into a *dangerous* second command rather than a blocked one:
        // blocking is a separate mechanism, and this test is about the fast path
        // not being fooled by the `cat` prefix.
        a.request_tools(vec![command_call("call_1", "cat file; rm -rf build")]);

        assert_eq!(a.state, AppState::AwaitingApproval);
        assert!(a.approved_tools.is_empty());
    }

    /// Queued calls are judged independently: the read-only one runs with no
    /// prompt, the other still stops and asks -- with `remaining` counting only
    /// what is left in the queue, not what already went straight through.
    #[test]
    fn a_read_only_call_and_a_risky_one_in_the_same_queue_are_judged_separately() {
        let mut a = streaming_app();
        a.config.tools.auto_approve_read_only = true;
        a.request_tools(vec![
            command_call("call_1", "ls"),
            command_call("call_2", "rm -rf build"),
        ]);

        assert_eq!(a.approved_tools.len(), 1, "the read-only call ran with no prompt");
        match &a.overlay {
            Some(Overlay::ToolApproval { action: tools::Action::Command { command, .. }, remaining }) => {
                assert_eq!(command, "rm -rf build");
                assert_eq!(*remaining, 0);
            }
            other => panic!("expected a prompt for the risky call, got {other:?}"),
        }
    }

    #[test]
    fn read_file_skips_the_prompt_when_the_fast_path_is_on() {
        let mut a = streaming_app();
        a.config.tools.auto_approve_read_only = true;
        a.request_tools(vec![read_file_call("call_1", "src/main.rs")]);

        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.overlay, None);
        assert_eq!(a.approved_tools.len(), 1);
    }

    /// Unlike a shell command's read-only-ness, "this writes a file" is
    /// certain rather than inferred -- so it must never ride the fast path,
    /// no matter how permissive `auto_approve_read_only` is.
    #[test]
    fn write_file_always_asks_even_with_the_fast_path_on() {
        let mut a = streaming_app();
        a.config.tools.auto_approve_read_only = true;
        a.request_tools(vec![write_file_call("call_1", "hello.py", "print('hi')\n")]);

        assert_eq!(a.state, AppState::AwaitingApproval);
        assert!(a.approved_tools.is_empty());
        match &a.overlay {
            Some(Overlay::ToolApproval { action: tools::Action::Write { path, content }, .. }) => {
                assert_eq!(path, "hello.py");
                assert_eq!(content, "print('hi')\n");
            }
            other => panic!("expected a write approval prompt, got {other:?}"),
        }
    }

    /// A response carrying tool calls emits ToolCalls and *then* Done. If that
    /// trailing Done were honoured it would end the turn before anything ran.
    #[test]
    fn the_done_that_follows_tool_calls_does_not_end_the_turn() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.finish_stream();

        assert_eq!(a.state, AppState::AwaitingApproval);
        assert_eq!(a.pending_tools.len(), 1);
    }

    #[test]
    fn results_go_back_as_tool_messages_and_are_summarised_on_screen() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "cat a.rs")]);
        a.handle_key(key(KeyCode::Char('y')));
        a.finish_tools(vec![ToolOutcome {
            call_id: "call_1".to_string(),
            display: "$ cat a.rs — 3 lines".to_string(),
            content: "exit code: 0\n--- stdout ---\nfn main() {}\n".to_string(),
        }]);

        assert_eq!(a.state, AppState::Sending);
        let wire = a.history(None);
        let tool = wire.last().unwrap();
        assert_eq!(tool.role, "tool");
        assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));
        assert!(tool.content.as_deref().unwrap().contains("fn main"));

        // The transcript shows the summary, never the whole output.
        assert_eq!(a.messages.last().unwrap().body(), "$ cat a.rs — 3 lines");
    }

    /// An assistant turn that is nothing but tool calls must serialize with no
    /// content field at all; `""` is rejected by several providers.
    #[test]
    fn an_assistant_turn_of_pure_tool_calls_carries_no_content() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);

        let assistant = a
            .history(None)
            .into_iter()
            .find(|m| m.role == "assistant")
            .expect("the assistant turn must be in the history");
        assert_eq!(assistant.content, None);
        assert_eq!(assistant.tool_calls.len(), 1);
    }

    /// The subtle one. Abandoning a turn between "the model asked" and "we ran
    /// it" leaves a tool_calls entry with no answer. Providers 400 on that -- and
    /// the 400 surfaces on the *next* prompt, looking unrelated.
    #[test]
    fn cancelling_mid_tool_loop_leaves_a_history_the_endpoint_will_accept() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls"), command_call("call_2", "pwd")]);
        a.handle_key(key(KeyCode::Char('y'))); // allow the first
        a.handle_key(key(KeyCode::Char('y'))); // allow the second, now ExecutingTools
        a.cancel();

        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a.pending_tools.is_empty());
        assert!(a.approved_tools.is_empty());
        assert_history_is_well_formed(&a.history(None));
    }

    #[test]
    fn a_failure_mid_tool_loop_also_leaves_a_valid_history() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.fail_stream("HTTP 500".to_string());

        assert_eq!(a.state, AppState::AwaitingInput);
        assert_eq!(a.overlay, None);
        assert_history_is_well_formed(&a.history(None));
    }

    /// A call already answered must not be answered twice -- duplicate
    /// tool_call_ids are just as invalid as missing ones.
    #[test]
    fn already_answered_calls_are_not_settled_again() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls"), command_call("call_2", "pwd")]);
        a.handle_key(key(KeyCode::Char('n'))); // call_1 declined, already answered
        a.handle_key(key(KeyCode::Char('y'))); // call_2 approved, still unanswered
        a.cancel();

        let answers: Vec<&str> = a
            .messages
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(answers, vec!["call_1", "call_2"]);
        assert_history_is_well_formed(&a.history(None));
    }

    #[test]
    fn a_new_prompt_resets_the_step_budget() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.handle_key(key(KeyCode::Char('y')));
        a.finish_tools(vec![outcome("call_1", "ok")]);
        assert_eq!(a.tool_steps, 1);

        a.state = AppState::AwaitingInput;
        type_str(&mut a, "another question");
        a.handle_key(key(KeyCode::Enter));

        assert_eq!(a.tool_steps, 0);
    }

    /// Results from a turn the user already abandoned must not be spliced into
    /// the next one -- the runner is spawned, so it can land late.
    #[test]
    fn late_results_from_an_abandoned_turn_are_ignored() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "sleep 30")]);
        a.handle_key(key(KeyCode::Char('y')));
        a.cancel();

        let before = a.messages.len();
        a.finish_tools(vec![outcome("call_1", "too late")]);
        assert_eq!(a.messages.len(), before, "a late result was appended anyway");
    }

    /// Every `tool_calls` entry answered exactly once, by a `tool` message, and
    /// no `tool` message answering a call that was never made.
    fn assert_history_is_well_formed(history: &[ChatMessage]) {
        let requested: Vec<&str> = history
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .map(|c| c.id.as_str())
            .collect();
        let mut answered: Vec<&str> = history
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();

        let mut expected = requested.clone();
        expected.sort_unstable();
        answered.sort_unstable();
        assert_eq!(
            answered, expected,
            "every tool call must be answered exactly once\nhistory: {history:#?}"
        );
    }

    #[test]
    fn cursor_position_tracks_rows_and_columns() {
        let mut a = app();
        type_str(&mut a, "ab");
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        type_str(&mut a, "cde");
        assert_eq!(a.cursor_position(), (1, 3));

        a.handle_key(key(KeyCode::Home));
        assert_eq!(a.cursor_position(), (1, 0));
    }

    // ---- /provider and /model overlays -----------------------------------------

    /// Navigates from a freshly opened ProviderPicker down to the registry entry
    /// whose id is `provider_id`, then presses Enter to select it (opening its
    /// scoped ModelPicker).
    fn select_provider(a: &mut App, provider_id: &str) {
        let idx = providers::PROVIDERS
            .iter()
            .position(|p| p.id == provider_id)
            .expect("provider_id must be in the registry");
        for _ in 0..idx {
            a.handle_key(key(KeyCode::Down));
        }
        a.handle_key(key(KeyCode::Enter));
    }

    #[test]
    fn up_and_down_walk_back_through_previous_prompts() {
        let mut a = app();
        for prompt in ["first", "second", "third"] {
            type_str(&mut a, prompt);
            a.handle_key(key(KeyCode::Enter));
            a.state = AppState::AwaitingInput; // pretend the turn finished
        }

        // Newest first, then further back, clamping at the oldest.
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.input_buffer, "third");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.input_buffer, "second");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.input_buffer, "first");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.input_buffer, "first", "must clamp at the oldest entry");

        // And forwards again.
        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.input_buffer, "second");
        // The caret sits at the end, ready to edit or resend.
        assert_eq!(a.cursor, "second".len());
    }

    /// Reaching for an old prompt and changing your mind must not eat the
    /// half-written one that was already in the box.
    #[test]
    fn stepping_forward_past_the_newest_entry_restores_the_draft() {
        let mut a = app();
        type_str(&mut a, "sent");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::AwaitingInput;

        type_str(&mut a, "half writ");
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.input_buffer, "sent");

        a.handle_key(key(KeyCode::Down));
        assert_eq!(a.input_buffer, "half writ", "the draft must come back");
    }

    /// Inside a multi-line prompt the arrows belong to the text, not to
    /// history -- losing a paragraph to a stray Up is worse than pressing PgUp.
    #[test]
    fn arrows_move_between_lines_of_a_multi_line_prompt_before_touching_history() {
        let mut a = app();
        type_str(&mut a, "old one");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::AwaitingInput;

        type_str(&mut a, "alpha");
        a.insert_str("\n");
        type_str(&mut a, "beta");
        assert_eq!(a.cursor_position(), (1, 4));

        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.cursor_position().0, 0, "should move within the prompt");
        assert_eq!(a.input_buffer, "alpha\nbeta", "history must not have fired");

        // Only once the caret is on the first line does Up reach history.
        a.handle_key(key(KeyCode::Up));
        assert_eq!(a.input_buffer, "old one");
    }

    #[test]
    fn page_up_and_page_down_still_scroll_the_transcript() {
        let mut a = app();
        a.scroll = 20;
        a.handle_key(key(KeyCode::PageUp));
        assert_eq!(a.scroll, 10);
        assert!(!a.follow_tail);
        a.handle_key(key(KeyCode::PageDown));
        assert_eq!(a.scroll, 20);
    }

    /// Pressing Enter twice on the same prompt should not mean pressing Up
    /// twice to get past it.
    #[test]
    fn resending_the_same_prompt_does_not_duplicate_it_in_history() {
        let mut a = app();
        for _ in 0..3 {
            type_str(&mut a, "same");
            a.handle_key(key(KeyCode::Enter));
            a.state = AppState::AwaitingInput;
        }
        assert_eq!(a.prompt_history, vec!["same".to_string()]);
    }

    #[test]
    fn slash_provider_opens_the_picker_and_clears_the_input() {
        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));

        assert_eq!(a.overlay, Some(Overlay::ProviderPicker { selected: 0 }));
        assert!(a.input_buffer.is_empty());
    }

    #[test]
    fn provider_picker_arrow_keys_navigate_and_clamp_at_the_bounds() {
        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));

        a.handle_key(key(KeyCode::Up)); // already at 0: stays clamped
        assert_eq!(a.overlay, Some(Overlay::ProviderPicker { selected: 0 }));

        for _ in 0..10 {
            a.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            a.overlay,
            Some(Overlay::ProviderPicker {
                selected: providers::PROVIDERS.len()
            })
        );
    }

    #[test]
    fn provider_picker_esc_cancels_back_to_normal_input() {
        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.overlay.is_some());

        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.overlay, None);
        assert_eq!(a.state, AppState::AwaitingInput);
    }

    #[test]
    fn selecting_a_provider_opens_its_scoped_model_picker() {
        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));
        select_provider(&mut a, "deepseek");

        assert_eq!(
            a.overlay,
            Some(Overlay::ModelPicker {
                provider_id: "deepseek",
                selected: 0
            })
        );
    }

    #[test]
    fn selecting_custom_endpoint_starts_the_manual_wizard() {
        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));
        for _ in 0..providers::PROVIDERS.len() {
            a.handle_key(key(KeyCode::Down));
        }
        a.handle_key(key(KeyCode::Enter));

        assert_eq!(a.overlay, Some(Overlay::CustomEndpoint(CustomStep::Endpoint)));
    }

    #[test]
    fn model_selection_uses_existing_env_var_when_present() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::set_var("DEEPSEEK_API_KEY", "sk-from-env");

        with_isolated_home(|| {
            let mut a = app();
            type_str(&mut a, "/provider");
            a.handle_key(key(KeyCode::Enter));
            select_provider(&mut a, "deepseek");
            a.handle_key(key(KeyCode::Enter)); // select first model -> env var found

            assert_eq!(a.overlay, None);
            assert_eq!(a.config.llm.provider, "deepseek");
            assert_eq!(a.config.llm.api_key, "sk-from-env");
            assert!(a.messages.iter().any(|m| m.role == Role::System));
        });

        match prev {
            Some(v) => std::env::set_var("DEEPSEEK_API_KEY", v),
            None => std::env::remove_var("DEEPSEEK_API_KEY"),
        }
    }

    #[test]
    fn model_selection_without_env_var_prompts_for_a_masked_api_key() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("DEEPSEEK_API_KEY");

        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));
        select_provider(&mut a, "deepseek");
        a.handle_key(key(KeyCode::Enter)); // select first model -> no env var

        match &a.overlay {
            Some(Overlay::ApiKeyPrompt { provider_id, .. }) => assert_eq!(*provider_id, "deepseek"),
            other => panic!("expected ApiKeyPrompt, got {other:?}"),
        }

        if let Some(v) = prev {
            std::env::set_var("DEEPSEEK_API_KEY", v);
        }
    }

    #[test]
    fn typing_into_the_api_key_prompt_updates_overlay_input_not_input_buffer() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("DEEPSEEK_API_KEY");

        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));
        select_provider(&mut a, "deepseek");
        a.handle_key(key(KeyCode::Enter));

        type_str(&mut a, "sk-secret");
        assert_eq!(a.overlay_input, "sk-secret");
        assert!(a.input_buffer.is_empty());

        if let Some(v) = prev {
            std::env::set_var("DEEPSEEK_API_KEY", v);
        }
    }

    #[test]
    fn submitting_the_api_key_prompt_saves_config_and_returns_to_normal_input() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("DEEPSEEK_API_KEY");

        with_isolated_home(|| {
            let mut a = app();
            type_str(&mut a, "/provider");
            a.handle_key(key(KeyCode::Enter));
            select_provider(&mut a, "deepseek");
            a.handle_key(key(KeyCode::Enter));

            type_str(&mut a, "sk-typed-key");
            a.handle_key(key(KeyCode::Enter));

            assert_eq!(a.overlay, None);
            assert_eq!(a.config.llm.provider, "deepseek");
            assert_eq!(a.config.llm.api_key, "sk-typed-key");
            assert!(a.overlay_input.is_empty());
            assert!(a.messages.iter().any(|m| m.role == Role::System));

            let reloaded = Config::load().expect("load should succeed");
            assert_eq!(reloaded.llm.api_key, "sk-typed-key");
        });

        if let Some(v) = prev {
            std::env::set_var("DEEPSEEK_API_KEY", v);
        }
    }

    #[test]
    fn standalone_model_without_a_provider_configured_shows_an_inline_error() {
        let mut a = app();
        type_str(&mut a, "/model");
        a.handle_key(key(KeyCode::Enter));

        assert_eq!(a.overlay, None);
        assert!(a
            .messages
            .iter()
            .any(|m| m.role == Role::Error && m.content.contains("/provider")));
    }

    #[test]
    fn standalone_model_scoped_to_the_configured_provider() {
        let mut a = app();
        a.config.llm.provider = "deepseek".to_string();

        type_str(&mut a, "/model");
        a.handle_key(key(KeyCode::Enter));

        assert_eq!(
            a.overlay,
            Some(Overlay::ModelPicker {
                provider_id: "deepseek",
                selected: 0
            })
        );
    }

    #[test]
    fn custom_endpoint_wizard_walks_all_three_steps_and_saves() {
        with_isolated_home(|| {
            let mut a = app();
            type_str(&mut a, "/provider");
            a.handle_key(key(KeyCode::Enter));
            for _ in 0..providers::PROVIDERS.len() {
                a.handle_key(key(KeyCode::Down));
            }
            a.handle_key(key(KeyCode::Enter)); // -> CustomEndpoint(Endpoint)

            type_str(&mut a, "http://localhost:9000");
            a.handle_key(key(KeyCode::Enter)); // -> CustomEndpoint(Model)
            assert_eq!(
                a.overlay,
                Some(Overlay::CustomEndpoint(CustomStep::Model {
                    endpoint: "http://localhost:9000".to_string()
                }))
            );

            type_str(&mut a, "local-llama");
            a.handle_key(key(KeyCode::Enter)); // -> CustomEndpoint(ApiKey)
            assert_eq!(
                a.overlay,
                Some(Overlay::CustomEndpoint(CustomStep::ApiKey {
                    endpoint: "http://localhost:9000".to_string(),
                    model: "local-llama".to_string(),
                }))
            );

            type_str(&mut a, "sk-custom");
            a.handle_key(key(KeyCode::Enter)); // finish

            assert_eq!(a.overlay, None);
            assert_eq!(a.config.llm.provider, "");
            assert_eq!(a.config.llm.endpoint, "http://localhost:9000");
            assert_eq!(a.config.llm.model, "local-llama");
            assert_eq!(a.config.llm.api_key, "sk-custom");
        });
    }

    #[test]
    fn esc_at_any_overlay_step_cancels_without_mutating_config() {
        let mut a = app();
        let before = a.config.clone();

        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));
        select_provider(&mut a, "deepseek");
        a.handle_key(key(KeyCode::Esc));

        assert_eq!(a.overlay, None);
        assert_eq!(a.config.llm.endpoint, before.llm.endpoint);
        assert_eq!(a.config.llm.model, before.llm.model);
        assert_eq!(a.config.llm.api_key, before.llm.api_key);
        assert_eq!(a.config.llm.provider, before.llm.provider);
    }

    #[test]
    fn pasting_into_the_api_key_prompt_lands_in_overlay_input_not_input_buffer() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("DEEPSEEK_API_KEY");

        let mut a = app();
        type_str(&mut a, "/provider");
        a.handle_key(key(KeyCode::Enter));
        select_provider(&mut a, "deepseek");
        a.handle_key(key(KeyCode::Enter));

        a.handle_paste("sk-pasted-key".to_string());
        assert_eq!(a.overlay_input, "sk-pasted-key");
        assert!(a.input_buffer.is_empty());

        if let Some(v) = prev {
            std::env::set_var("DEEPSEEK_API_KEY", v);
        }
    }
}
