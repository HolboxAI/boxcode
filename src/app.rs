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
        if let AppState::Streaming { response } = &self.state {
            let mut new_response = response.clone();
            new_response.push_str(&token);
            self.state = AppState::Streaming {
                response: new_response,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_state_transitions() {
        let config = Config {
            llm: crate::config::LlmConfig {
                endpoint: "http://localhost:8000".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            },
        };
        let mut app = App::new(config);

        // Test initial state
        assert!(matches!(app.state, AppState::AwaitingInput));
        assert_eq!(app.input_buffer, "");

        // Test input buffer
        app.input_buffer.push_str("test");
        assert_eq!(app.input_buffer, "test");
    }

    #[test]
    fn test_append_token() {
        let config = Config {
            llm: crate::config::LlmConfig {
                endpoint: "http://localhost:8000".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            },
        };
        let mut app = App::new(config);

        // Start streaming
        app.state = AppState::Streaming {
            response: "Hello ".to_string(),
        };

        // Append token
        app.append_token("world".to_string());

        if let AppState::Streaming { response } = &app.state {
            assert_eq!(response, "Hello world");
        }
    }
}
