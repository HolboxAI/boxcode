mod app;
mod config;
mod llm;
mod ui;

use app::App;
use config::Config;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::error::Error;
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load configuration
    let config = Config::load()?;
    tracing::info!("Loaded config: {} @ {}", config.llm.model, config.llm.endpoint);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new(config.clone());

    // Create channel for LLM tokens
    let (tx, mut rx) = mpsc::channel::<String>(100);

    // Main event loop
    loop {
        // Render
        terminal.draw(|f| {
            ui::render(f, &app);
        })?;

        // Poll events with timeout
        if crossterm::event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('c') if key.modifiers == event::KeyModifiers::CONTROL => {
                        break;
                    }
                    KeyCode::Char('d') if key.modifiers == event::KeyModifiers::CONTROL => {
                        break;
                    }
                    _ => {
                        app.handle_key(key);
                    }
                }
            }
        }

        // Check for incoming tokens from LLM
        if let Ok(token) = rx.try_recv() {
            app.append_token(token);
        }

        // If in Sending state and not yet streaming, spawn LLM request
        if let app::AppState::Sending { prompt } = &app.state {
            let prompt_clone = prompt.clone();
            let endpoint = app.config.llm.endpoint.clone();
            let model = app.config.llm.model.clone();
            let api_key = app.config.llm.api_key.clone();
            let tx_clone = tx.clone();

            tokio::spawn(async move {
                if let Err(e) = llm::stream_chat(&endpoint, &model, &api_key, &prompt_clone, tx_clone).await {
                    tracing::error!("LLM error: {}", e);
                }
            });

            // Transition to Streaming
            app.state = app::AppState::Streaming {
                response: String::new(),
            };
        }

        // Check if should exit
        if app.should_exit {
            break;
        }
    }

    // Cleanup terminal
    disable_raw_mode()?;
    crossterm::execute!(io::stdout(), LeaveAlternateScreen)?;
    println!("Goodbye!");
    Ok(())
}
