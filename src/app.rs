use crate::config::Config;
use crossterm::event::KeyEvent;

#[derive(Clone, Debug)]
pub enum AppState {
    AwaitingInput,
    Sending { prompt: String },
    Streaming { response: String },
    Done { response: String },
}

#[derive(Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

pub struct App {
    pub state: AppState,
    pub messages: Vec<Message>,
    pub input_buffer: String,
    pub config: Config,
    pub should_exit: bool,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self {
            state: AppState::AwaitingInput,
            messages: vec![Message {
                role: "Assistant".to_string(),
                content: "Welcome to tuisample-code. Type your prompt and press Ctrl-Enter to send.".to_string(),
            }],
            input_buffer: String::new(),
            config,
            should_exit: false,
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Enter if key.modifiers == KeyModifiers::CONTROL => {
                if !self.input_buffer.is_empty() {
                    let prompt = self.input_buffer.drain(..).collect();
                    self.messages.push(Message {
                        role: "You".to_string(),
                        content: format!("> {}", prompt),
                    });
                    self.state = AppState::Sending { prompt };
                }
            }
            KeyCode::Esc => {
                if let AppState::Streaming { response } = &self.state {
                    self.messages.push(Message {
                        role: "Assistant".to_string(),
                        content: response.clone(),
                    });
                    self.state = AppState::Done {
                        response: response.clone(),
                    };
                }
            }
            KeyCode::Tab => {
                self.input_buffer.push('\t');
            }
            _ => {}
        }
    }

    pub fn append_token(&mut self, token: String) {
        if let AppState::Streaming { response } = &mut self.state {
            response.push_str(&token);
        }
    }
}
