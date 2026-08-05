use crate::config::Config;
use crate::llm::{ChatMessage, ToolCall};
use crate::providers;
use crate::tools::{self, ToolOutcome};
use crate::usage::{self, DailyUsage, QuotaVerdict, TokenUsage};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{HashSet, VecDeque};

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
    /// Set by "a" at an approval prompt: stop asking for the rest of the session.
    /// Session-only and never persisted -- a permanent version of this belongs in
    /// config.toml, where turning it on is a deliberate act rather than a
    /// keystroke made while impatient.
    pub auto_approve: bool,
    /// Tool rounds spent on the current prompt, reset by `submit`. Once this hits
    /// the configured ceiling the schemas stop being sent, which is what makes a
    /// model that will not stop calling tools produce an answer instead.
    pub tool_steps: usize,
    /// One line for the welcome screen describing where commands will run, or
    /// why the tool is off. Set by `main` once the workspace has been resolved.
    pub workspace_status: String,
    /// The resolved working directory, shown on the approval prompt so it is
    /// always clear *where* a command is about to run.
    pub workspace_root: String,
    /// One line for the welcome screen describing free-tier enrolment, or why it
    /// is unavailable. Empty when the user brought their own key.
    pub free_tier_status: String,
    /// Today's request / token / spend tallies, loaded at startup and written
    /// back after every request.
    pub usage: DailyUsage,
    /// Set when a warning has already been shown for the current day, so an
    /// approaching-limit notice appears once rather than before every prompt.
    pub warned_today: bool,
}

impl App {
    pub fn new(config: Config) -> Self {
        let usage = if config.quota.enabled {
            DailyUsage::load(&usage::today_local())
        } else {
            DailyUsage::default()
        };
        Self {
            usage,
            warned_today: false,
            free_tier_status: String::new(),
            state: AppState::AwaitingInput,
            messages: Vec::new(),
            input_buffer: String::new(),
            cursor: 0,
            streaming_response: String::new(),
            request_id: 0,
            abort: None,
            scroll: 0,
            follow_tail: true,
            config,
            should_exit: false,
            greeted: false,
            overlay: None,
            overlay_input: String::new(),
            overlay_cursor: 0,
            pending_tools: VecDeque::new(),
            approved_tools: Vec::new(),
            auto_approve: false,
            tool_steps: 0,
            workspace_status: String::new(),
            workspace_root: String::new(),
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
                        "/usage" | "/quota" if !self.is_busy() => {
                            self.input_buffer.clear();
                            self.cursor = 0;
                            self.show_usage();
                        }
                        "/quota override" if !self.is_busy() => {
                            self.input_buffer.clear();
                            self.cursor = 0;
                            self.set_quota_override(true);
                        }
                        "/quota reset" if !self.is_busy() => {
                            self.input_buffer.clear();
                            self.cursor = 0;
                            self.set_quota_override(false);
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

            KeyCode::Up | KeyCode::PageUp => {
                self.follow_tail = false;
                self.scroll = self.scroll.saturating_sub(if key.code == KeyCode::Up { 1 } else { 10 });
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(if key.code == KeyCode::Down { 1 } else { 10 });
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

        // Before the limits are consulted, so a session open past midnight is
        // judged against today's allowance rather than yesterday's.
        self.roll_over_if_needed();

        // Checked before the prompt is accepted, and only here -- never mid-turn.
        // Blocking between tool rounds would strand `tool_calls` with no matching
        // results, which invalidates the conversation for every later request.
        // `tools.max_steps` already bounds how far a single turn can run.
        if let Some(message) = self.quota_block() {
            // The prompt deliberately stays in the input box. It was never sent,
            // and silently destroying something the user just typed -- possibly
            // at length -- is a worse outcome than the refusal itself.
            self.greeted = true;
            self.follow_tail = true;
            self.messages.push(Message::new(Role::Error, message));
            return;
        }

        self.input_buffer.clear();
        self.cursor = 0;
        self.greeted = true;
        self.follow_tail = true;
        self.streaming_response.clear();
        self.tool_steps = 0;
        self.messages.push(Message::new(Role::User, prompt));

        if let Some(warning) = self.quota_warning() {
            self.messages.push(Message::new(Role::System, warning));
        }
        self.state = AppState::Sending;
    }

    /// Start a new day if the local date has moved on since the counters were
    /// last touched.
    ///
    /// This app is a TUI that people leave open for days, so a rollover that only
    /// happened at startup would keep yesterday's spent allowance in force well
    /// into the morning -- and an override granted yesterday would never expire.
    fn roll_over_if_needed(&mut self) {
        if !self.config.quota.enabled {
            return;
        }
        let today = usage::today_local();
        if self.usage.date != today {
            self.usage.roll_over(&today);
            self.warned_today = false;
            let _ = self.usage.save();
        }
    }

    /// The reason this prompt cannot be sent, if any.
    fn quota_block(&self) -> Option<String> {
        match usage::evaluate(
            &self.usage,
            &self.config.quota,
            &usage::time_until_local_midnight(),
        ) {
            QuotaVerdict::Blocked(message) => Some(message),
            _ => None,
        }
    }

    /// A once-per-day nudge that a limit is close, or that an override is live.
    fn quota_warning(&mut self) -> Option<String> {
        if self.warned_today {
            return None;
        }
        match usage::evaluate(
            &self.usage,
            &self.config.quota,
            &usage::time_until_local_midnight(),
        ) {
            QuotaVerdict::Warn(message) => {
                self.warned_today = true;
                Some(message)
            }
            _ => None,
        }
    }

    /// Fold one finished request into today's totals and persist them.
    ///
    /// Called for every request the endpoint answered, including the extra ones a
    /// tool-using turn makes -- each is a real, billable call.
    pub fn record_usage(&mut self, tokens: TokenUsage) {
        if !self.config.quota.enabled {
            return;
        }
        self.roll_over_if_needed();
        let price = self.config.quota.price_for(&self.config.llm.model);
        self.usage.record(&tokens, price);
        // Written per request rather than at exit: the app is a TUI that people
        // close with Ctrl-C, and a quota that forgets on exit is not a quota.
        let _ = self.usage.save();
    }

    /// `/usage` -- today's totals, spelled out.
    fn show_usage(&mut self) {
        self.roll_over_if_needed();
        let quota = &self.config.quota;
        let mut lines = vec![format!("Usage for {} (local day)", self.usage.date)];

        let limit = |used: String, limit: String, unlimited: bool| {
            if unlimited {
                format!("{used} (no limit set)")
            } else {
                format!("{used} of {limit}")
            }
        };

        lines.push(format!(
            "  Requests: {}",
            limit(
                self.usage.requests.to_string(),
                quota.max_requests_per_day.to_string(),
                quota.max_requests_per_day == 0,
            )
        ));
        lines.push(format!(
            "  Tokens:   {}{}",
            limit(
                usage::format_tokens(self.usage.total_tokens()),
                usage::format_tokens(quota.max_tokens_per_day),
                quota.max_tokens_per_day == 0,
            ),
            if self.usage.any_estimated {
                "  (estimated — this endpoint does not report token counts)"
            } else {
                ""
            }
        ));
        lines.push(format!(
            "            {} prompt + {} completion",
            usage::format_tokens(self.usage.prompt_tokens),
            usage::format_tokens(self.usage.completion_tokens)
        ));
        lines.push(format!(
            "  Spend:    {}",
            limit(
                format!("${:.2}", self.usage.usd),
                format!("${:.2}", quota.max_usd_per_day),
                quota.max_usd_per_day == 0.0,
            )
        ));
        // Naming the gap matters more than the number: a total that silently
        // omits half the day's requests is worse than no total.
        if self.usage.unpriced_requests > 0 {
            lines.push(format!(
                "            excludes {} request(s) on a model with no price in [quota.pricing]",
                self.usage.unpriced_requests
            ));
        }
        if self.usage.override_active {
            lines.push("  Override: active for today".to_string());
        }
        lines.push(format!(
            "  Resets in {} (local midnight)",
            usage::time_until_local_midnight()
        ));

        // The free-tier budget is a different meter entirely -- enforced by the
        // gateway, on a UTC day, and not editable here. Showing them side by
        // side stops the local numbers being mistaken for the ones that bind.
        if !self.free_tier_status.is_empty() && !self.free_tier_status.starts_with("unavailable") {
            lines.push(String::new());
            lines.push(format!("Free tier (enforced by the gateway)\n  {}", self.free_tier_status));
        }

        self.greeted = true;
        self.follow_tail = true;
        self.messages
            .push(Message::new(Role::System, lines.join("\n")));
    }

    /// `/quota override` -- spend past the limit for the rest of today.
    fn set_quota_override(&mut self, active: bool) {
        // Otherwise an override could be granted against yesterday's record and
        // then be wiped by the next rollover, silently doing nothing.
        self.roll_over_if_needed();
        self.usage.override_active = active;
        let _ = self.usage.save();
        self.greeted = true;
        self.follow_tail = true;
        let text = if active {
            "Quota override active for the rest of today. It clears at local midnight."
        } else {
            "Quota override cleared; the daily limits apply again."
        };
        self.messages.push(Message::new(Role::System, text));
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
            AppState::ExecutingTools
        };
    }

    /// Whether `call` needs a human decision before it runs.
    ///
    /// Order matters: the session-wide escape hatches (`require_approval`,
    /// `auto_approve`) are checked first since they should short-circuit
    /// regardless of what the call is, and `auto_approve_read_only` only
    /// waives the prompt for a narrow, conservative slice of calls --
    /// `read_file` unconditionally (it cannot write anything), and shell
    /// commands via `tools::is_read_only`. `write_file` never qualifies:
    /// unlike a shell command's read-only-ness, which has to be inferred,
    /// "this writes a file" is certain, so it always asks.
    fn needs_approval(&self, call: &ToolCall) -> bool {
        if !self.config.tools.require_approval || self.auto_approve {
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

    /// y allow · n refuse · a stop asking for this session · Esc refuse.
    ///
    /// Esc means refuse rather than cancel-the-turn: at a prompt asking whether
    /// to run something, the reflexive keypress has to be the safe one.
    fn handle_command_approval_key(&mut self, key: KeyEvent) {
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.auto_approve = true;
                self.messages.push(Message::new(
                    Role::System,
                    "Running commands without asking for the rest of this session.",
                ));
                Some(true)
            }
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

    /// Route one event from the request task or the command runner.
    ///
    /// Lives here rather than inline in the event loop so the mapping from wire
    /// event to state transition can be tested directly -- `Usage` in particular
    /// must reach `record_usage` for the day's tallies to mean anything.
    pub fn handle_event(&mut self, event: crate::llm::StreamEvent) {
        use crate::llm::StreamEvent;
        match event {
            StreamEvent::Token(token) => self.append_token(&token),
            StreamEvent::ToolCalls(calls) => self.request_tools(calls),
            StreamEvent::ToolsFinished(outcomes) => self.finish_tools(outcomes),
            StreamEvent::Usage(tokens) => self.record_usage(tokens),
            StreamEvent::Done => self.finish_stream(),
            StreamEvent::Error(err) => self.fail_stream(err),
        }
    }

    pub fn append_token(&mut self, token: &str) {
        if self.state == AppState::Streaming {
            self.streaming_response.push_str(token);
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
        self.state = AppState::AwaitingInput;
    }

    pub fn fail_stream(&mut self, error: String) {
        self.abort = None;
        self.pending_tools.clear();
        self.approved_tools.clear();
        self.overlay = None;
        // First, so the results land against the calls they belong to.
        self.settle_unanswered_tool_calls("The request failed before this command ran.");

        let partial = std::mem::take(&mut self.streaming_response);
        if !partial.trim().is_empty() {
            self.messages.push(Message::new(Role::Assistant, partial));
        }
        self.messages.push(Message::new(Role::Error, error));
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
        // `App::new` reads ~/.tuisample-code/usage.json. Tests must not inherit
        // whatever the developer's real day happens to look like, so start every
        // fixture on a clean, known day.
        app.usage = DailyUsage {
            date: usage::today_local(),
            ..Default::default()
        };
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

    /// Esc at an approval prompt means "no", not "cancel the turn": the
    /// reflexive keypress has to be the safe one.
    #[test]
    fn esc_refuses_the_command_rather_than_cancelling_the_turn() {
        for refuse in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut a = streaming_app();
            a.request_tools(vec![command_call("call_1", "rm -rf /")]);
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

    #[test]
    fn a_stops_asking_for_the_rest_of_the_session() {
        let mut a = streaming_app();
        a.request_tools(vec![command_call("call_1", "ls")]);
        a.handle_key(key(KeyCode::Char('a')));
        assert!(a.auto_approve);
        assert_eq!(a.approved_tools.len(), 1);

        // A later round goes straight through with no prompt.
        a.finish_tools(vec![outcome("call_1", "ok")]);
        a.state = AppState::Streaming;
        a.request_tools(vec![command_call("call_2", "cat Cargo.toml")]);

        assert_eq!(a.state, AppState::ExecutingTools);
        assert_eq!(a.overlay, None);
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
        a.request_tools(vec![command_call("call_1", "cat file; rm -rf /")]);

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

    // ---- daily usage quota ----------------------------------------------------

    /// An app with a request ceiling and `spent` requests already used today.
    fn quota_app(limit: u64, spent: u64) -> App {
        let mut a = app();
        a.config.quota.max_requests_per_day = limit;
        a.usage.requests = spent;
        a
    }

    #[test]
    fn a_prompt_is_refused_once_the_daily_request_limit_is_spent() {
        let mut a = quota_app(5, 5);
        type_str(&mut a, "hello");
        a.handle_key(key(KeyCode::Enter));

        // Nothing was sent...
        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(
            !a.messages.iter().any(|m| m.role == Role::User),
            "a refused prompt must not enter the conversation"
        );
        // ...and the refusal explains itself.
        let error = a
            .messages
            .iter()
            .find(|m| m.role == Role::Error)
            .expect("a refusal must be shown");
        assert!(error.content.contains("Daily quota reached"), "{}", error.content);
        assert!(error.content.contains("/quota override"), "{}", error.content);
    }

    /// Losing a long prompt to a quota refusal would be a worse outcome than the
    /// refusal itself, so the text stays where the user can still get at it.
    #[test]
    fn a_refused_prompt_is_left_in_the_input_box() {
        let mut a = quota_app(1, 1);
        type_str(&mut a, "a carefully written prompt");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.input_buffer, "a carefully written prompt");
    }

    #[test]
    fn one_request_below_the_limit_still_sends() {
        let mut a = quota_app(5, 4);
        type_str(&mut a, "hello");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::Sending);
        assert!(a.messages.iter().any(|m| m.role == Role::User));
    }

    /// The upgrade-safety property, at the level that matters: a user who has set
    /// no limits must never see a prompt refused.
    #[test]
    fn with_no_limits_configured_nothing_is_ever_refused() {
        let mut a = app();
        a.usage.requests = 100_000;
        a.usage.prompt_tokens = 500_000_000;
        a.usage.usd = 4_000.0;
        type_str(&mut a, "hello");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::Sending);
    }

    #[test]
    fn a_token_limit_refuses_independently_of_the_request_count() {
        let mut a = app();
        a.config.quota.max_tokens_per_day = 1_000;
        a.usage.prompt_tokens = 600;
        a.usage.completion_tokens = 400;
        type_str(&mut a, "hello");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::AwaitingInput);
    }

    #[test]
    fn a_spend_limit_refuses_independently_of_tokens_and_requests() {
        let mut a = app();
        a.config.quota.max_usd_per_day = 2.50;
        a.usage.usd = 2.50;
        type_str(&mut a, "hello");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::AwaitingInput);
    }

    #[test]
    fn quota_override_unblocks_sending_for_the_rest_of_the_day() {
        with_isolated_home(|| {
            let mut a = quota_app(5, 5);
            type_str(&mut a, "/quota override");
            a.handle_key(key(KeyCode::Enter));
            assert!(a.usage.override_active);

            type_str(&mut a, "hello");
            a.handle_key(key(KeyCode::Enter));
            assert_eq!(a.state, AppState::Sending);
        });
    }

    #[test]
    fn quota_reset_puts_the_limit_back() {
        with_isolated_home(|| {
            let mut a = quota_app(5, 5);
            a.usage.override_active = true;
            type_str(&mut a, "/quota reset");
            a.handle_key(key(KeyCode::Enter));
            assert!(!a.usage.override_active);

            type_str(&mut a, "hello");
            a.handle_key(key(KeyCode::Enter));
            assert_eq!(a.state, AppState::AwaitingInput);
        });
    }

    #[test]
    fn approaching_the_limit_warns_once_rather_than_every_prompt() {
        let mut a = quota_app(10, 8); // 80%
        type_str(&mut a, "one");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::Sending);
        let warnings = |a: &App| {
            a.messages
                .iter()
                .filter(|m| m.role == Role::System && m.content.contains("Approaching"))
                .count()
        };
        assert_eq!(warnings(&a), 1);

        // A second prompt in the same day must not repeat it.
        a.state = AppState::AwaitingInput;
        type_str(&mut a, "two");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(warnings(&a), 1);
    }

    #[test]
    fn recording_a_request_updates_the_running_totals() {
        with_isolated_home(|| {
            let mut a = app();
            a.config.llm.model = "priced-model".to_string();
            a.config.quota.pricing.insert(
                "priced-model".to_string(),
                crate::usage::ModelPrice { input_per_mtok: 2.0, output_per_mtok: 4.0 },
            );

            a.record_usage(TokenUsage { prompt: 1_000_000, completion: 1_000_000, estimated: false });

            assert_eq!(a.usage.requests, 1);
            assert_eq!(a.usage.total_tokens(), 2_000_000);
            assert!((a.usage.usd - 6.0).abs() < 1e-9, "{}", a.usage.usd);
        });
    }

    /// Usage on a model the user never priced must not quietly read as free.
    #[test]
    fn recording_on_an_unpriced_model_counts_tokens_but_not_dollars() {
        with_isolated_home(|| {
            let mut a = app();
            a.config.llm.model = "some-local-model".to_string();
            a.record_usage(TokenUsage { prompt: 1_000, completion: 1_000, estimated: false });

            assert_eq!(a.usage.total_tokens(), 2_000);
            assert_eq!(a.usage.usd, 0.0);
            assert_eq!(a.usage.unpriced_requests, 1);
        });
    }

    #[test]
    fn recording_is_skipped_entirely_when_tracking_is_disabled() {
        let mut a = app();
        a.config.quota.enabled = false;
        a.record_usage(TokenUsage { prompt: 100, completion: 100, estimated: false });
        assert_eq!(a.usage.requests, 0);
    }

    /// Regression: the rollover used to happen only at startup and after a
    /// recorded request, so a TUI left open overnight kept refusing prompts
    /// against yesterday's spent allowance until it was restarted.
    #[test]
    fn a_session_open_past_midnight_is_judged_against_the_new_day() {
        with_isolated_home(|| {
            let mut a = quota_app(5, 5); // yesterday's limit, fully spent
            a.usage.date = "2000-01-01".to_string();

            type_str(&mut a, "hello");
            a.handle_key(key(KeyCode::Enter));

            assert_eq!(a.state, AppState::Sending, "a new day must start clean");
            assert_eq!(a.usage.date, usage::today_local());
            assert_eq!(a.usage.requests, 0);
        });
    }

    /// ...and an override granted yesterday must not still be in force today.
    #[test]
    fn an_override_does_not_survive_into_the_next_day() {
        with_isolated_home(|| {
            let mut a = quota_app(5, 5);
            a.usage.date = "2000-01-01".to_string();
            a.usage.override_active = true;

            type_str(&mut a, "/usage");
            a.handle_key(key(KeyCode::Enter));

            assert!(!a.usage.override_active, "yesterday's override must expire");
        });
    }

    /// A session left running overnight must start the new day clean rather than
    /// keep charging against yesterday's allowance.
    #[test]
    fn a_request_recorded_after_midnight_rolls_the_day_over() {
        with_isolated_home(|| {
            let mut a = quota_app(10, 9);
            a.usage.date = "2000-01-01".to_string(); // yesterday, by a wide margin
            a.warned_today = true;

            a.record_usage(TokenUsage { prompt: 10, completion: 10, estimated: false });

            assert_eq!(a.usage.date, usage::today_local());
            assert_eq!(a.usage.requests, 1, "yesterday's 9 must not carry over");
            assert!(!a.warned_today, "a new day gets its warning back");
        });
    }

    /// Every request a turn makes is billed, including the extra round trips a
    /// tool-using turn performs, so each must consume quota.
    #[test]
    fn each_recorded_request_counts_including_tool_round_trips() {
        with_isolated_home(|| {
            let mut a = app();
            for _ in 0..3 {
                a.record_usage(TokenUsage { prompt: 10, completion: 10, estimated: false });
            }
            assert_eq!(a.usage.requests, 3);
        });
    }

    #[test]
    fn the_usage_command_reports_all_three_metrics() {
        let mut a = app();
        a.usage.requests = 3;
        a.usage.prompt_tokens = 1_500;
        a.config.quota.max_requests_per_day = 10;

        type_str(&mut a, "/usage");
        a.handle_key(key(KeyCode::Enter));

        let report = a
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .expect("a report must be shown");
        assert!(report.content.contains("Requests: 3 of 10"), "{}", report.content);
        assert!(report.content.contains("Tokens:"), "{}", report.content);
        assert!(report.content.contains("Spend:"), "{}", report.content);
        assert!(report.content.contains("Resets in"), "{}", report.content);
        // An unset limit must read as unset, not as zero.
        assert!(report.content.contains("no limit set"), "{}", report.content);
        // /usage is a local command and never becomes a prompt.
        assert!(!a.messages.iter().any(|m| m.role == Role::User));
        assert_eq!(a.state, AppState::AwaitingInput);
    }

    /// The free-tier budget is the one that actually refuses requests, so it has
    /// to appear in `/usage` alongside the local counters -- otherwise a user
    /// reads "no limit set" three times and concludes there is no limit.
    #[test]
    fn the_usage_report_shows_the_free_tier_budget_when_enrolled() {
        let mut a = app();
        a.free_tier_status =
            "free tier — deepseek-v4-flash · $0.0021 of $0.25 used today (3 requests)".to_string();

        type_str(&mut a, "/usage");
        a.handle_key(key(KeyCode::Enter));

        let report = a
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .expect("a report must be shown");
        assert!(report.content.contains("Free tier"), "{}", report.content);
        assert!(report.content.contains("$0.25"), "{}", report.content);
        // ...and it must be labelled as the gateway's, not confused with the
        // local counters directly above it.
        assert!(report.content.contains("gateway"), "{}", report.content);
    }

    #[test]
    fn the_usage_report_omits_the_free_tier_block_for_a_byok_user() {
        let mut a = app(); // free_tier_status empty: user brought their own key
        type_str(&mut a, "/usage");
        a.handle_key(key(KeyCode::Enter));
        let report = a.messages.iter().find(|m| m.role == Role::System).unwrap();
        assert!(!report.content.contains("Free tier"), "{}", report.content);
    }

    #[test]
    fn the_usage_report_names_requests_it_could_not_price() {
        let mut a = app();
        a.usage.requests = 2;
        a.usage.unpriced_requests = 2;
        type_str(&mut a, "/usage");
        a.handle_key(key(KeyCode::Enter));

        let report = a.messages.iter().find(|m| m.role == Role::System).unwrap();
        assert!(report.content.contains("no price"), "{}", report.content);
    }

    #[test]
    fn the_usage_report_flags_estimated_token_counts() {
        let mut a = app();
        a.usage.any_estimated = true;
        type_str(&mut a, "/usage");
        a.handle_key(key(KeyCode::Enter));

        let report = a.messages.iter().find(|m| m.role == Role::System).unwrap();
        assert!(report.content.contains("estimated"), "{}", report.content);
    }

    #[test]
    fn quota_commands_are_ignored_while_a_request_is_in_flight() {
        let mut a = streaming_app();
        type_str(&mut a, "/usage");
        a.handle_key(key(KeyCode::Enter));
        // Still streaming, and the text was treated as ordinary input rather
        // than executed as a command mid-turn.
        assert_eq!(a.state, AppState::Streaming);
    }
}
