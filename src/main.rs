mod agent;
mod app;
mod config;
mod llm;
mod permission;
mod providers;
mod tools;
mod ui;
mod upgrade;

use agent::{AgentEvent, RunCtx};
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
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Handle flags before touching the terminal, so `--version` works when
    // piped and `--upgrade` still runs even if the config file is broken.
    let mut upgrade = false;
    let mut force = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("tuisample-code {VERSION}");
                return Ok(());
            }
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-u" | "--upgrade" => upgrade = true,
            "-f" | "--force" => force = true,
            other => {
                eprintln!("Unknown argument: {other}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }

    if upgrade {
        // Handled here rather than returned: the default runtime handler prints
        // Err via Debug, which turns a connection failure into a wall of
        // struct-dump instead of a sentence.
        if let Err(e) = upgrade::run(force).await {
            eprintln!("❌ Upgrade failed: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if force {
        eprintln!("--force only means something alongside --upgrade.\n");
        print_help();
        std::process::exit(2);
    }

    let config = Config::load()?;
    // The directory the tool was launched from is the workspace. Everything the
    // agent reads or writes has to resolve inside it.
    let workspace = std::env::current_dir()?;

    let enhanced = setup_terminal()?;
    install_panic_hook(enhanced);

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config);
    let (tx, mut rx) = mpsc::channel::<(u64, AgentEvent)>(512);

    let result = run_app(&mut terminal, &mut app, workspace, tx, &mut rx).await;

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
    workspace: PathBuf,
    tx: mpsc::Sender<(u64, AgentEvent)>,
    rx: &mut mpsc::Receiver<(u64, AgentEvent)>,
) -> Result<(), Box<dyn Error>> {
    // One client for the whole session: a fresh one per turn would pay for a TLS
    // handshake on every step of every run.
    let client = llm::build_client()?;

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

        // Drain every event that has arrived; doing only one per frame caps
        // throughput at ~60 tokens/sec and looks like the app is stalling.
        while let Ok((id, event)) = rx.try_recv() {
            app.handle_agent_event(id, event);
        }

        // Start a pending run.
        if let AppState::Sending { prompt } = &app.state {
            let prompt = prompt.clone();
            app.request_id += 1;
            let id = app.request_id;

            let ctx = RunCtx {
                run_id: id,
                client: client.clone(),
                // Re-read from config every time, so `/provider` mid-session
                // takes effect on the very next prompt.
                target: llm::Target {
                    endpoint: app.config.llm.endpoint.clone(),
                    model: app.config.llm.model.clone(),
                    api_key: app.config.llm.api_key.clone(),
                    max_tokens: app.config.agent.max_tokens,
                },
                tools: tools::ToolCtx::new(
                    workspace.clone(),
                    Duration::from_secs(app.config.agent.shell_timeout_secs),
                ),
                allowlist: app.allowlist.clone(),
                max_iterations: app.config.agent.max_iterations,
                cancel: app.cancel.clone(),
                tx: tx.clone(),
            };

            let spec = agent::default_agent();
            let messages = app.session_messages.clone();
            let done = tx.clone();

            let handle = tokio::spawn(async move {
                let (result, messages) = agent::run::run(spec, prompt, messages, ctx).await;
                let _ = done
                    .send((id, AgentEvent::Finished { result, messages }))
                    .await;
            });

            app.abort = Some(handle.abort_handle());
            app.state = AppState::Working;
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
Agentic coding assistant in your terminal, on any OpenAI-compatible endpoint.

Run it from the root of the project you want it to work on -- that directory is
the workspace, and the agent cannot read or write outside it.

USAGE:
    tuisample-code [FLAGS]

FLAGS:
    -V, --version    Print version and exit
    -h, --help       Print this help and exit
    -u, --upgrade    Update to the latest release
    -f, --force      With --upgrade: reinstall even if already up to date

CONFIG (environment overrides ~/.tuisample-code/config.toml):
    TUISAMPLE_ENDPOINT    Base URL, e.g. https://llm.internal:8443
    TUISAMPLE_MODEL       Model name
    TUISAMPLE_API_KEY     Bearer token

    [agent] in config.toml also takes max_iterations, shell_timeout_secs and
    max_tokens.

UPGRADE:
    TUISAMPLE_UPGRADE_URL_BASE
                          Fetch updates from a fork or internal mirror
                          instead of github.com

COMMANDS (type in the input box, press Enter):
    /provider             Pick a provider + model + API key, saved to config.toml
    /model                Pick a model for the currently configured provider
    /new                  Forget the conversation and start fresh

APPROVALS:
    Reads and searches run on their own. Before writing a file or running a
    command the agent asks: [a] allow once, [s] allow for the session, [d] deny.

KEYS:
    Enter                 Send prompt
    Alt/Shift-Enter       New line
    Esc                   Cancel the run
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
