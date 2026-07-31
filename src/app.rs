use crate::config::Config;
use crate::providers;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Debug, PartialEq)]
pub enum AppState {
    AwaitingInput,
    /// Transient: the event loop picks this up, fires the request, and moves to `Streaming`.
    Sending { prompt: String },
    Streaming,
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
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::User => "You",
            Role::Assistant => "Assistant",
            Role::Error => "Error",
            Role::System => "System",
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

        self.input_buffer.clear();
        self.cursor = 0;
        self.greeted = true;
        self.follow_tail = true;
        self.streaming_response.clear();
        self.messages.push(Message {
            role: Role::User,
            content: prompt.clone(),
        });
        self.state = AppState::Sending { prompt };
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

        let partial = std::mem::take(&mut self.streaming_response);
        if !partial.trim().is_empty() {
            self.messages.push(Message {
                role: Role::Assistant,
                content: format!("{partial}\n[cancelled]"),
            });
        } else {
            self.messages.push(Message {
                role: Role::Error,
                content: "Request cancelled.".to_string(),
            });
        }
        self.state = AppState::AwaitingInput;
    }

    pub fn append_token(&mut self, token: &str) {
        if self.state == AppState::Streaming {
            self.streaming_response.push_str(token);
        }
    }

    pub fn finish_stream(&mut self) {
        if self.state != AppState::Streaming {
            return;
        }
        self.abort = None;
        let response = std::mem::take(&mut self.streaming_response);
        if response.trim().is_empty() {
            self.messages.push(Message {
                role: Role::Error,
                content: "The endpoint returned an empty response.".to_string(),
            });
        } else {
            self.messages.push(Message {
                role: Role::Assistant,
                content: response,
            });
        }
        self.state = AppState::AwaitingInput;
    }

    pub fn fail_stream(&mut self, error: String) {
        self.abort = None;
        let partial = std::mem::take(&mut self.streaming_response);
        if !partial.trim().is_empty() {
            self.messages.push(Message {
                role: Role::Assistant,
                content: partial,
            });
        }
        self.messages.push(Message {
            role: Role::Error,
            content: error,
        });
        self.state = AppState::AwaitingInput;
    }

    /// Conversation so far, for sending as request context.
    pub fn history(&self) -> Vec<(String, String)> {
        self.messages
            .iter()
            .filter_map(|m| match m.role {
                Role::User => Some(("user".to_string(), m.content.clone())),
                Role::Assistant => Some(("assistant".to_string(), m.content.clone())),
                Role::Error | Role::System => None,
            })
            .collect()
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
                self.messages.push(Message {
                    role: Role::Error,
                    content: "No provider configured yet. Run /provider first.".to_string(),
                });
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
                    self.messages.push(Message {
                        role: Role::Error,
                        content: "No API key entered; cancelled.".to_string(),
                    });
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
                            self.messages.push(Message {
                                role: Role::Error,
                                content: "Endpoint cannot be empty; cancelled.".to_string(),
                            });
                            return;
                        }
                        self.overlay = Some(Overlay::CustomEndpoint(CustomStep::Model { endpoint: value }));
                    }
                    CustomStep::Model { endpoint } => {
                        if value.is_empty() {
                            self.messages.push(Message {
                                role: Role::Error,
                                content: "Model cannot be empty; cancelled.".to_string(),
                            });
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
            Ok(()) => Message {
                role: Role::System,
                content: format!("Switched to {label} / {}.", self.config.llm.model),
            },
            Err(e) => Message {
                role: Role::Error,
                content: format!("Using it for this session, but failed to save to config.toml: {e}"),
            },
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
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].content, "hello world");
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
        assert!(matches!(a.state, AppState::Sending { .. }));
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

        let history = a.history();
        assert_eq!(
            history,
            vec![
                ("user".to_string(), "first".to_string()),
                ("assistant".to_string(), "answer".to_string()),
                ("user".to_string(), "second".to_string()),
            ]
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
}
