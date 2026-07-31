mod app;
mod config;
mod llm;
mod ui;

use app::{App, AppState};
use config::Config;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use llm::StreamEvent;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::error::Error;
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Handle flags before touching the terminal, so `--version` works when piped.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("tuisample-code {VERSION}");
                return Ok(());
            }
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other => {
                eprintln!("Unknown argument: {other}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }

    let config = Config::load()?;

    let enhanced = setup_terminal()?;
    install_panic_hook(enhanced);

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config);
    let (tx, mut rx) = mpsc::channel::<(u64, StreamEvent)>(256);

    let result = run_app(&mut terminal, &mut app, tx, &mut rx).await;

    restore_terminal(enhanced)?;
    if let Err(e) = &result {
        eprintln!("Error: {e}");
    }
    println!("Goodbye!");
    result
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    tx: mpsc::Sender<(u64, StreamEvent)>,
    rx: &mut mpsc::Receiver<(u64, StreamEvent)>,
) -> Result<(), Box<dyn Error>> {
    loop {
        terminal.draw(|f| ui::render(f, app))?;

        // Keyboard / paste input.
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Char('c') | KeyCode::Char('d') if ctrl => break,
                        _ => app.handle_key(key),
                    }
                }
                Event::Paste(text) => app.handle_paste(text),
                _ => {}
            }
        }

        // Drain every token that has arrived; doing only one per frame caps
        // throughput at ~60 tokens/sec and looks like the app is stalling.
        while let Ok((id, event)) = rx.try_recv() {
            if id != app.request_id {
                continue; // stale: belongs to a cancelled request
            }
            match event {
                StreamEvent::Token(token) => app.append_token(&token),
                StreamEvent::Done => app.finish_stream(),
                StreamEvent::Error(err) => app.fail_stream(err),
            }
        }

        // Fire a pending request.
        if let AppState::Sending { .. } = &app.state {
            app.request_id += 1;
            let id = app.request_id;
            let endpoint = app.config.llm.endpoint.clone();
            let model = app.config.llm.model.clone();
            let api_key = app.config.llm.api_key.clone();
            let history = app.history();
            let tx_clone = tx.clone();

            let handle = tokio::spawn(async move {
                llm::stream_chat(&endpoint, &model, &api_key, history, id, tx_clone).await;
            });

            app.abort = Some(handle.abort_handle());
            app.state = AppState::Streaming;
        }

        if app.should_exit {
            break;
        }
    }

    Ok(())
}

fn print_help() {
    println!(
        "tuisample-code {VERSION}
Terminal UI for an OpenAI-compatible LLM endpoint.

USAGE:
    tuisample-code [FLAGS]

FLAGS:
    -V, --version    Print version and exit
    -h, --help       Print this help and exit

CONFIG (environment overrides ~/.tuisample-code/config.toml):
    TUISAMPLE_ENDPOINT    Base URL, e.g. https://llm.internal:8443
    TUISAMPLE_MODEL       Model name
    TUISAMPLE_API_KEY     Bearer token

KEYS:
    Enter                 Send prompt
    Alt/Shift-Enter       New line
    Esc                   Cancel request
    Ctrl-C                Exit"
    );
}

/// Returns true if the kitty keyboard protocol was enabled (so it can be popped later).
fn setup_terminal() -> Result<bool, Box<dyn Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

    // Optional: lets terminals that support it distinguish Shift/Ctrl-Enter.
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        crossterm::execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok(enhanced)
}

fn restore_terminal(enhanced: bool) -> Result<(), Box<dyn Error>> {
    let mut stdout = io::stdout();
    if enhanced {
        let _ = crossterm::execute!(stdout, PopKeyboardEnhancementFlags);
    }
    crossterm::execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

/// Without this a panic leaves the terminal in raw mode on the alternate screen,
/// with the backtrace invisible.
fn install_panic_hook(enhanced: bool) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(enhanced);
        default_hook(info);
    }));
}
