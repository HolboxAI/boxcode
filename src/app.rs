use crate::agent::{AgentEvent, PermissionRequest};
use crate::config::Config;
use crate::llm::ChatMessage;
use crate::permission::{Allowlist, Decision};
use crate::providers;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq)]
pub enum AppState {
    AwaitingInput,
    /// Transient: the event loop picks this up, starts the run, and moves to `Working`.
    Sending { prompt: String },
    /// An agent is running: thinking, calling tools, or waiting on approval.
    Working,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

/// One thing that happened, in order. Unlike the old flat transcript this
/// distinguishes what the model *said* from what it *did*, which is most of what
/// makes an agent run readable.
#[derive(Clone)]
pub enum Entry {
    User(String),
    Agent {
        agent: &'static str,
        text: String,
    },
    Tool {
        call_id: String,
        summary: String,
        status: ToolStatus,
        detail: String,
    },
    /// Confirmations from `/provider` and `/model`.
    System(String),
    Error(String),
}

/// State of the overlays. `None` means the normal input box is active; every
/// other variant intercepts all keyboard input in `handle_key` before it reaches
/// the normal editing logic.
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
    /// An agent is blocked waiting for this answer.
    Permission {
        summary: String,
        /// `None` when the call is not safe to grant for a whole session, in
        /// which case the "allow for session" option is not offered at all.
        grant: Option<String>,
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
    pub entries: Vec<Entry>,
    /// The conversation as the model sees it, including tool calls and results.
    /// Distinct from `entries`: tool results belong here and not on screen, and
    /// `/provider` confirmations belong on screen and not here.
    pub session_messages: Vec<ChatMessage>,
    /// Raw text of the prompt box. May contain '\n' (Alt/Shift-Enter inserts one).
    pub input_buffer: String,
    /// Cursor position as a *byte* index into `input_buffer`. Always on a char boundary.
    pub cursor: usize,
    /// Prose from the turn in flight, not yet committed to an `Entry`.
    pub streaming_response: String,
    /// Which agent is currently talking, for the streaming label.
    pub active_agent: &'static str,
    /// Incremented per run so events from a cancelled run are ignored.
    pub request_id: u64,
    /// Abort handle for the in-flight run task, used by Esc.
    pub abort: Option<tokio::task::AbortHandle>,
    /// Checked by the agent loop between steps so cancellation is cooperative as
    /// well as abrupt -- an aborted task cannot answer its outstanding tool calls.
    pub cancel: Arc<AtomicBool>,
    /// Session grants, shared with every agent task.
    pub allowlist: Allowlist,
    /// Held while a permission overlay is up; answering it resolves the agent.
    pub pending_permission: Option<oneshot::Sender<Decision>>,
    pub scroll: u16,
    /// While true the message pane sticks to the bottom as new text arrives.
    pub follow_tail: bool,
    pub config: Config,
    pub should_exit: bool,
    /// Set once the user has interacted, so the welcome panel gives way to the transcript.
    pub greeted: bool,
    /// `Some` while an overlay is active; see `Overlay`.
    pub overlay: Option<Overlay>,
    /// Single-line buffer for overlay text entry (API key, custom endpoint/model).
    /// Kept separate from `input_buffer` so the (possibly masked) overlay text
    /// never renders in the base input box behind the popup, and so the two
    /// never fight over `f.set_cursor(...)` in the same frame.
    pub overlay_input: String,
    pub overlay_cursor: usize,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self {
            state: AppState::AwaitingInput,
            entries: Vec::new(),
            session_messages: Vec::new(),
            input_buffer: String::new(),
            cursor: 0,
            streaming_response: String::new(),
            active_agent: crate::agent::DEFAULT_AGENT,
            request_id: 0,
            abort: None,
            cancel: Arc::new(AtomicBool::new(false)),
            allowlist: Allowlist::new(),
            pending_permission: None,
            scroll: 0,
            follow_tail: true,
            config,
            should_exit: false,
            greeted: false,
            overlay: None,
            overlay_input: String::new(),
            overlay_cursor: 0,
        }
    }

    pub fn is_busy(&self) -> bool {
        !matches!(self.state, AppState::AwaitingInput)
    }

    /// True while an agent is blocked on the user's answer, which the footer
    /// shows differently from ordinary work.
    pub fn awaiting_permission(&self) -> bool {
        self.pending_permission.is_some()
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
                        "/new" if !self.is_busy() => {
                            self.input_buffer.clear();
                            self.cursor = 0;
                            self.reset_session();
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
                self.scroll = self
                    .scroll
                    .saturating_sub(if key.code == KeyCode::Up { 1 } else { 10 });
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_add(if key.code == KeyCode::Down { 1 } else { 10 });
            }

            KeyCode::Esc => self.cancel(),

            _ => {}
        }
    }

    /// Bracketed paste — a multi-line paste must land in the buffer verbatim,
    /// not be interpreted as a series of Enter presses. Routed into the overlay's
    /// text field while a text-entry overlay is active (pasting an API key is
    /// the realistic common case), and ignored while a list-picker or permission
    /// overlay is active (nothing to paste into).
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
        self.cancel.store(false, Ordering::Relaxed);
        self.entries.push(Entry::User(prompt.clone()));
        self.state = AppState::Sending { prompt };
    }

    /// Drop the conversation but keep the transcript on screen, so a long session
    /// can be cleared without restarting the process.
    fn reset_session(&mut self) {
        self.session_messages.clear();
        self.entries.push(Entry::System(
            "Started a new conversation. The agent no longer remembers earlier turns.".to_string(),
        ));
        self.greeted = true;
        self.follow_tail = true;
    }

    fn cancel(&mut self) {
        if !self.is_busy() {
            return;
        }
        // Cooperative first: the loop checks this between steps and unwinds
        // cleanly, answering any tool calls it is abandoning.
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.abort.take() {
            handle.abort();
        }
        // Bump the id so events already in flight on the channel are discarded.
        self.request_id += 1;

        // An agent blocked on a permission prompt would otherwise wait forever.
        if let Some(respond) = self.pending_permission.take() {
            let _ = respond.send(Decision::Deny);
        }
        self.overlay = None;

        self.flush_streaming();
        self.entries.push(Entry::Error("Cancelled.".to_string()));
        self.mark_running_tools_cancelled();
        self.state = AppState::AwaitingInput;
    }

    // ---- agent events ---------------------------------------------------------

    /// Route one event from the run. Returns false if the event was stale (it
    /// belonged to a cancelled run) and should be ignored.
    pub fn handle_agent_event(&mut self, id: u64, event: AgentEvent) -> bool {
        if id != self.request_id {
            // Still answer a stale permission request, or its agent task never
            // unblocks and the process cannot exit.
            if let AgentEvent::NeedsPermission(request) = event {
                let _ = request.respond.send(Decision::Deny);
            }
            return false;
        }

        match event {
            AgentEvent::Token { agent, text } => {
                self.active_agent = agent;
                self.streaming_response.push_str(&text);
            }
            AgentEvent::ToolStarted {
                agent,
                call_id,
                summary,
            } => {
                self.active_agent = agent;
                // Commit whatever the model said before reaching for the tool, so
                // the reasoning reads above the action rather than after it.
                self.flush_streaming();
                self.entries.push(Entry::Tool {
                    call_id,
                    summary,
                    status: ToolStatus::Running,
                    detail: String::new(),
                });
            }
            AgentEvent::ToolFinished {
                call_id,
                ok,
                detail,
            } => {
                self.finish_tool(&call_id, ok, detail);
            }
            AgentEvent::NeedsPermission(request) => {
                self.begin_permission(request);
            }
            AgentEvent::Finished { result, messages } => {
                self.finish_run(result, messages);
            }
        }
        true
    }

    fn finish_tool(&mut self, call_id: &str, ok: bool, detail: String) {
        for entry in self.entries.iter_mut().rev() {
            if let Entry::Tool {
                call_id: id,
                status,
                detail: slot,
                ..
            } = entry
            {
                if id == call_id {
                    *status = if ok { ToolStatus::Ok } else { ToolStatus::Failed };
                    *slot = detail;
                    return;
                }
            }
        }
    }

    fn begin_permission(&mut self, request: PermissionRequest) {
        let PermissionRequest {
            summary,
            grant,
            respond,
        } = request;
        self.pending_permission = Some(respond);
        self.overlay = Some(Overlay::Permission { summary, grant });
        self.follow_tail = true;
    }

    fn finish_run(&mut self, result: Result<String, String>, messages: Vec<ChatMessage>) {
        self.abort = None;
        self.session_messages = messages;
        self.flush_streaming();
        match result {
            Ok(text) if text.trim().is_empty() => {
                // The agent may legitimately end with no closing prose if its last
                // word already streamed out; only complain when nothing landed.
                if !matches!(self.entries.last(), Some(Entry::Agent { .. }) | Some(Entry::Tool { .. }))
                {
                    self.entries.push(Entry::Error(
                        "The endpoint returned an empty response.".to_string(),
                    ));
                }
            }
            Ok(_) => {}
            Err(e) => self.entries.push(Entry::Error(e)),
        }
        self.mark_running_tools_cancelled();
        self.state = AppState::AwaitingInput;
    }

    /// Commit streamed prose into an entry. Tokens already accumulated in
    /// `streaming_response` are the same text the final turn carries, so this is
    /// the only place an `Agent` entry is created.
    fn flush_streaming(&mut self) {
        let text = std::mem::take(&mut self.streaming_response);
        if !text.trim().is_empty() {
            self.entries.push(Entry::Agent {
                agent: self.active_agent,
                text: text.trim_end().to_string(),
            });
        }
    }

    /// A run that ends while a tool is still Running would leave a spinner on
    /// screen forever.
    fn mark_running_tools_cancelled(&mut self) {
        for entry in self.entries.iter_mut() {
            if let Entry::Tool { status, detail, .. } = entry {
                if *status == ToolStatus::Running {
                    *status = ToolStatus::Failed;
                    if detail.is_empty() {
                        *detail = "Cancelled.".to_string();
                    }
                }
            }
        }
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

    // ---- overlays --------------------------------------------------------------

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
                self.entries.push(Entry::Error(
                    "No provider configured yet. Run /provider first.".to_string(),
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
            Overlay::Permission { summary, grant } => {
                self.handle_permission_key(key, summary, grant)
            }
        }
    }

    /// Answers the blocked agent. Every path through here either resolves
    /// `pending_permission` or puts the overlay back -- leaving both cleared
    /// would hang the run.
    fn handle_permission_key(&mut self, key: KeyEvent, summary: String, grant: Option<String>) {
        let decision = match key.code {
            KeyCode::Char('a') | KeyCode::Char('y') | KeyCode::Enter => Some(Decision::AllowOnce),
            KeyCode::Char('s') if grant.is_some() => Some(Decision::AllowSession),
            KeyCode::Char('d') | KeyCode::Char('n') => Some(Decision::Deny),
            // Esc is a denial *and* a cancellation: the user wants out, not just
            // this one command refused.
            KeyCode::Esc => {
                if let Some(respond) = self.pending_permission.take() {
                    let _ = respond.send(Decision::Deny);
                }
                self.cancel();
                return;
            }
            _ => None,
        };

        match decision {
            Some(decision) => {
                if let Some(respond) = self.pending_permission.take() {
                    let _ = respond.send(decision);
                }
                if decision == Decision::AllowSession {
                    if let Some(key) = grant {
                        self.allowlist.allow(key);
                    }
                }
            }
            // An unrecognised key must not dismiss the prompt.
            None => self.overlay = Some(Overlay::Permission { summary, grant }),
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

    fn handle_model_picker_key(
        &mut self,
        key: KeyEvent,
        provider_id: &'static str,
        selected: usize,
    ) {
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
                self.overlay = Some(Overlay::ModelPicker {
                    provider_id,
                    selected,
                });
            }
        }
    }

    fn handle_api_key_prompt_key(
        &mut self,
        key: KeyEvent,
        provider_id: &'static str,
        model: String,
    ) {
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
                    self.entries
                        .push(Entry::Error("No API key entered; cancelled.".to_string()));
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
                insert_into(
                    &mut self.overlay_input,
                    &mut self.overlay_cursor,
                    &c.to_string(),
                );
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
                            self.entries.push(Entry::Error(
                                "Endpoint cannot be empty; cancelled.".to_string(),
                            ));
                            return;
                        }
                        self.overlay = Some(Overlay::CustomEndpoint(CustomStep::Model {
                            endpoint: value,
                        }));
                    }
                    CustomStep::Model { endpoint } => {
                        if value.is_empty() {
                            self.entries
                                .push(Entry::Error("Model cannot be empty; cancelled.".to_string()));
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
                insert_into(
                    &mut self.overlay_input,
                    &mut self.overlay_cursor,
                    &c.to_string(),
                );
                self.overlay = Some(Overlay::CustomEndpoint(step));
            }
            _ => {
                self.overlay = Some(Overlay::CustomEndpoint(step));
            }
        }
    }

    /// Single completion path for every provider overlay flow (env-var shortcut,
    /// masked prompt, custom wizard). Updates the in-memory config -- which
    /// `main.rs`'s event loop re-reads fresh on every `Sending` transition, so
    /// this takes effect on the very next request with no restart needed -- and
    /// persists it.
    ///
    /// Any test that reaches this function MUST wrap the call in
    /// `config::test_support::with_isolated_home`, or it will write to the real
    /// developer/CI `~/.tuisample-code/config.toml`.
    fn apply_llm_config(
        &mut self,
        provider: String,
        endpoint: String,
        model: String,
        api_key: String,
    ) {
        self.config.llm.provider = provider;
        self.config.llm.endpoint = endpoint;
        self.config.llm.model = model;
        self.config.llm.api_key = api_key;

        let label = if self.config.llm.provider.is_empty() {
            self.config.llm.endpoint.as_str()
        } else {
            self.config.llm.provider.as_str()
        };
        let entry = match self.config.save() {
            Ok(()) => Entry::System(format!(
                "Switched to {label} / {}.",
                self.config.llm.model
            )),
            Err(e) => Entry::Error(format!(
                "Using it for this session, but failed to save to config.toml: {e}"
            )),
        };
        self.entries.push(entry);
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

    fn app() -> App {
        App::new(Config::default())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn entry_text(entry: &Entry) -> String {
        match entry {
            Entry::User(t) | Entry::System(t) | Entry::Error(t) => t.clone(),
            Entry::Agent { text, .. } => text.clone(),
            Entry::Tool { summary, .. } => summary.clone(),
        }
    }

    /// Drive a run the way `main.rs` does: bump the id, then feed events.
    fn start_run(app: &mut App) -> u64 {
        app.request_id += 1;
        app.state = AppState::Working;
        app.request_id
    }

    fn permission_request(summary: &str, grant: Option<&str>) -> (AgentEvent, oneshot::Receiver<Decision>) {
        let (respond, receive) = oneshot::channel();
        (
            AgentEvent::NeedsPermission(PermissionRequest {
                summary: summary.to_string(),
                grant: grant.map(str::to_string),
                respond,
            }),
            receive,
        )
    }

    /// The reported bug: typing a prompt and pressing Enter did nothing, because
    /// submission required Ctrl-Enter, which terminals cannot send.
    #[test]
    fn plain_enter_submits_the_prompt() {
        let mut a = app();
        type_str(&mut a, "hello world");
        assert_eq!(a.input_buffer, "hello world");

        a.handle_key(key(KeyCode::Enter));

        assert_eq!(
            a.state,
            AppState::Sending {
                prompt: "hello world".to_string()
            }
        );
        assert!(a.input_buffer.is_empty());
        assert_eq!(a.entries.len(), 1);
        assert_eq!(entry_text(&a.entries[0]), "hello world");
    }

    #[test]
    fn ctrl_enter_still_submits_where_the_terminal_reports_it() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert!(matches!(a.state, AppState::Sending { .. }));
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
        assert_eq!(
            a.state,
            AppState::Sending {
                prompt: "line1\nline2".to_string()
            }
        );
    }

    #[test]
    fn empty_or_whitespace_prompt_is_not_sent() {
        let mut a = app();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::AwaitingInput);

        type_str(&mut a, "   ");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a.entries.is_empty());
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
    fn cannot_submit_a_second_prompt_while_an_agent_is_working() {
        let mut a = app();
        type_str(&mut a, "one");
        a.handle_key(key(KeyCode::Enter));
        a.state = AppState::Working;

        type_str(&mut a, "two");
        a.handle_key(key(KeyCode::Enter));

        assert_eq!(a.state, AppState::Working);
        assert_eq!(a.entries.len(), 1);
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

    // ---- agent events -----------------------------------------------------------

    #[test]
    fn streamed_prose_commits_to_one_entry_when_the_run_ends() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        let id = start_run(&mut a);

        a.handle_agent_event(
            id,
            AgentEvent::Token {
                agent: "coder",
                text: "Hel".to_string(),
            },
        );
        a.handle_agent_event(
            id,
            AgentEvent::Token {
                agent: "coder",
                text: "lo!".to_string(),
            },
        );
        a.handle_agent_event(
            id,
            AgentEvent::Finished {
                result: Ok("Hello!".to_string()),
                messages: vec![ChatMessage::user("hi")],
            },
        );

        assert_eq!(a.state, AppState::AwaitingInput);
        assert_eq!(a.entries.len(), 2);
        assert!(matches!(a.entries[1], Entry::Agent { .. }));
        assert_eq!(entry_text(&a.entries[1]), "Hello!");
        // The conversation is kept for the next prompt.
        assert_eq!(a.session_messages.len(), 1);
    }

    /// Reasoning must read above the action it led to, not after it.
    #[test]
    fn prose_is_committed_before_the_tool_entry_it_precedes() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        let id = start_run(&mut a);

        a.handle_agent_event(
            id,
            AgentEvent::Token {
                agent: "coder",
                text: "Let me look.".to_string(),
            },
        );
        a.handle_agent_event(
            id,
            AgentEvent::ToolStarted {
                agent: "coder",
                call_id: "c1".to_string(),
                summary: "read_file(a.rs)".to_string(),
            },
        );

        assert!(matches!(a.entries[1], Entry::Agent { .. }));
        assert_eq!(entry_text(&a.entries[1]), "Let me look.");
        assert!(matches!(
            a.entries[2],
            Entry::Tool {
                status: ToolStatus::Running,
                ..
            }
        ));
        assert!(a.streaming_response.is_empty());
    }

    #[test]
    fn a_tool_entry_resolves_to_ok_or_failed_by_call_id() {
        let mut a = app();
        let id = start_run(&mut a);

        for call in ["c1", "c2"] {
            a.handle_agent_event(
                id,
                AgentEvent::ToolStarted {
                    agent: "coder",
                    call_id: call.to_string(),
                    summary: format!("read_file({call})"),
                },
            );
        }
        a.handle_agent_event(
            id,
            AgentEvent::ToolFinished {
                call_id: "c2".to_string(),
                ok: false,
                detail: "no such file".to_string(),
            },
        );

        match (&a.entries[0], &a.entries[1]) {
            (
                Entry::Tool { status: first, .. },
                Entry::Tool {
                    status: second,
                    detail,
                    ..
                },
            ) => {
                assert_eq!(*first, ToolStatus::Running, "c1 is still going");
                assert_eq!(*second, ToolStatus::Failed);
                assert_eq!(detail, "no such file");
            }
            _ => panic!("expected two tool entries"),
        }
    }

    #[test]
    fn stale_events_from_a_cancelled_run_are_ignored() {
        let mut a = app();
        let id = start_run(&mut a);
        a.request_id += 1; // as if cancelled and restarted

        let accepted = a.handle_agent_event(
            id,
            AgentEvent::Token {
                agent: "coder",
                text: "ghost".to_string(),
            },
        );
        assert!(!accepted);
        assert!(a.streaming_response.is_empty());
    }

    /// A stale permission request still has an agent task blocked behind it.
    #[test]
    fn a_stale_permission_request_is_denied_rather_than_dropped() {
        let mut a = app();
        let id = start_run(&mut a);
        a.request_id += 1;

        let (event, receive) = permission_request("run_shell(rm -rf /)", None);
        assert!(!a.handle_agent_event(id, event));
        assert_eq!(receive.blocking_recv().unwrap(), Decision::Deny);
        assert!(a.overlay.is_none());
    }

    #[test]
    fn errors_surface_in_the_transcript_and_unblock_input() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        let id = start_run(&mut a);

        a.handle_agent_event(
            id,
            AgentEvent::Finished {
                result: Err("HTTP 401 Unauthorized".to_string()),
                messages: Vec::new(),
            },
        );

        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Error(t) if t.contains("401"))));

        // The user can immediately try again.
        type_str(&mut a, "retry");
        a.handle_key(key(KeyCode::Enter));
        assert!(matches!(a.state, AppState::Sending { .. }));
    }

    #[test]
    fn esc_cancels_and_keeps_partial_output() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        let id = start_run(&mut a);
        a.handle_agent_event(
            id,
            AgentEvent::Token {
                agent: "coder",
                text: "partial".to_string(),
            },
        );

        a.handle_key(key(KeyCode::Esc));

        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a.cancel.load(Ordering::Relaxed));
        assert!(a
            .entries
            .iter()
            .any(|e| entry_text(e).contains("partial")));
    }

    /// A run that dies mid-tool must not leave a spinner on screen forever.
    #[test]
    fn cancelling_resolves_a_still_running_tool_entry() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        let id = start_run(&mut a);
        a.handle_agent_event(
            id,
            AgentEvent::ToolStarted {
                agent: "coder",
                call_id: "c1".to_string(),
                summary: "run_shell(sleep 100)".to_string(),
            },
        );

        a.handle_key(key(KeyCode::Esc));

        assert!(!a
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Tool { status: ToolStatus::Running, .. })));
    }

    #[test]
    fn slash_new_clears_the_conversation_but_keeps_the_transcript() {
        let mut a = app();
        a.session_messages = vec![ChatMessage::user("old")];
        a.entries.push(Entry::User("old".to_string()));

        type_str(&mut a, "/new");
        a.handle_key(key(KeyCode::Enter));

        assert!(a.session_messages.is_empty());
        assert_eq!(a.entries.len(), 2);
        assert!(matches!(a.entries[1], Entry::System(_)));
    }

    // ---- permission overlay ------------------------------------------------------

    #[test]
    fn a_permission_request_opens_an_overlay_and_blocks_the_agent() {
        let mut a = app();
        let id = start_run(&mut a);
        let (event, mut receive) = permission_request("run_shell(cargo test)", Some("run_shell:cargo"));

        a.handle_agent_event(id, event);

        assert!(a.awaiting_permission());
        assert!(matches!(a.overlay, Some(Overlay::Permission { .. })));
        assert!(receive.try_recv().is_err(), "the agent must still be waiting");
    }

    #[test]
    fn allow_once_answers_without_recording_a_grant() {
        let mut a = app();
        let id = start_run(&mut a);
        let (event, receive) = permission_request("write_file(a.rs)", Some("write_file"));
        a.handle_agent_event(id, event);

        a.handle_key(key(KeyCode::Char('a')));

        assert_eq!(receive.blocking_recv().unwrap(), Decision::AllowOnce);
        assert!(a.overlay.is_none());
        assert!(!a.awaiting_permission());
        assert!(!a.allowlist.allows("write_file"));
    }

    #[test]
    fn allow_for_session_records_the_grant() {
        let mut a = app();
        let id = start_run(&mut a);
        let (event, receive) = permission_request("run_shell(cargo test)", Some("run_shell:cargo"));
        a.handle_agent_event(id, event);

        a.handle_key(key(KeyCode::Char('s')));

        assert_eq!(receive.blocking_recv().unwrap(), Decision::AllowSession);
        assert!(a.allowlist.allows("run_shell:cargo"));
    }

    /// A compound command carries no grant key, so 's' must do nothing at all
    /// rather than silently granting something broader than it looks.
    #[test]
    fn allow_for_session_is_inert_when_the_call_is_not_grantable() {
        let mut a = app();
        let id = start_run(&mut a);
        let (event, mut receive) = permission_request("run_shell(cd x && rm -rf /)", None);
        a.handle_agent_event(id, event);

        a.handle_key(key(KeyCode::Char('s')));

        assert!(receive.try_recv().is_err(), "the prompt must still be open");
        assert!(matches!(a.overlay, Some(Overlay::Permission { .. })));
    }

    #[test]
    fn deny_answers_the_agent_and_leaves_the_run_going() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        let id = start_run(&mut a);
        let (event, receive) = permission_request("write_file(a.rs)", Some("write_file"));
        a.handle_agent_event(id, event);

        a.handle_key(key(KeyCode::Char('d')));

        assert_eq!(receive.blocking_recv().unwrap(), Decision::Deny);
        assert_eq!(a.state, AppState::Working, "denial is not cancellation");
        assert!(!a.cancel.load(Ordering::Relaxed));
    }

    /// Esc means "get me out", not "refuse this one thing".
    #[test]
    fn esc_at_a_permission_prompt_denies_and_cancels_the_run() {
        let mut a = app();
        type_str(&mut a, "hi");
        a.handle_key(key(KeyCode::Enter));
        let id = start_run(&mut a);
        let (event, receive) = permission_request("run_shell(rm -rf /)", None);
        a.handle_agent_event(id, event);

        a.handle_key(key(KeyCode::Esc));

        assert_eq!(receive.blocking_recv().unwrap(), Decision::Deny);
        assert_eq!(a.state, AppState::AwaitingInput);
        assert!(a.cancel.load(Ordering::Relaxed));
        assert!(a.overlay.is_none());
    }

    #[test]
    fn an_unrecognised_key_does_not_dismiss_the_permission_prompt() {
        let mut a = app();
        let id = start_run(&mut a);
        let (event, mut receive) = permission_request("write_file(a.rs)", Some("write_file"));
        a.handle_agent_event(id, event);

        a.handle_key(key(KeyCode::Char('q')));
        a.handle_key(key(KeyCode::Up));

        assert!(matches!(a.overlay, Some(Overlay::Permission { .. })));
        assert!(a.awaiting_permission());
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn typing_while_a_permission_prompt_is_up_never_reaches_the_input_box() {
        let mut a = app();
        let id = start_run(&mut a);
        let (event, _receive) = permission_request("write_file(a.rs)", Some("write_file"));
        a.handle_agent_event(id, event);

        type_str(&mut a, "q");
        assert!(a.input_buffer.is_empty());
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
            assert!(a.entries.iter().any(|e| matches!(e, Entry::System(_))));
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
            assert!(a.entries.iter().any(|e| matches!(e, Entry::System(_))));

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
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Error(t) if t.contains("/provider"))));
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
