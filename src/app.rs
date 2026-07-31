use crate::config::Config;
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
        }
    }
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

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            // Enter submits. Alt/Shift-Enter (and Ctrl-Enter, on terminals that can
            // actually distinguish it) insert a newline instead.
            KeyCode::Enter => {
                if alt || shift {
                    self.insert_str("\n");
                } else {
                    self.submit();
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
    /// not be interpreted as a series of Enter presses.
    pub fn handle_paste(&mut self, text: String) {
        let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_str(&cleaned);
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
                Role::Error => None,
            })
            .collect()
    }

    // ---- input buffer editing -------------------------------------------------

    fn insert_str(&mut self, s: &str) {
        self.input_buffer.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn delete_before(&mut self) {
        let prev = self.prev_boundary();
        if prev != self.cursor {
            self.input_buffer.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    fn delete_after(&mut self) {
        let next = self.next_boundary();
        if next != self.cursor {
            self.input_buffer.drain(self.cursor..next);
        }
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
        self.input_buffer[..self.cursor]
            .chars()
            .next_back()
            .map_or(0, |c| self.cursor - c.len_utf8())
    }

    /// Next char boundary (byte index), saturating at the end of the buffer.
    fn next_boundary(&self) -> usize {
        self.input_buffer[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

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
}
