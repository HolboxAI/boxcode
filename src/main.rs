mod app;
mod config;
mod danger;
mod dateutil;
mod deploy;
mod llm;
mod notice;
mod paths;
mod providers;
mod quota;
mod telemetry;
mod tools;
mod theme;
mod ui;
mod upgrade;
mod usage;
mod workspace;

use app::{App, AppState};
use config::Config;
use workspace::Workspace;
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
    // Handle flags before touching the terminal, so `--version` works when
    // piped and `--upgrade` still runs even if the config file is broken.
    let mut upgrade = false;
    let mut force = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("boxcode {VERSION}");
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

    // Before anything reads or writes state: v1.0.0 renamed the directory it
    // all lives in, and every reader below must see one directory, not two.
    let migration = paths::migrate_legacy_state();

    let config = Config::load()?;
    // Before anything is drawn: the colours depend on the terminal's
    // background, and asking for that needs the terminal to itself, with
    // no alternate screen up and nothing else reading stdin.
    theme::init(theme::resolve_mode(&config.ui.theme));

    let (workspace, workspace_status) = open_workspace(&config);

    // Detached, not awaited: a slow or unreachable telemetry endpoint must
    // never delay the terminal coming up. See telemetry.rs -- this is a
    // no-op until a real endpoint is configured, and every failure inside it
    // is already silent.
    tokio::spawn(telemetry::ping_active_if_new_day(VERSION));

    let enhanced = setup_terminal()?;
    install_panic_hook(enhanced);

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config);
    // Loaded before the first prompt so a limit already spent today is in force
    // from the start, not after the first request slips through.
    if app.config.quota.enabled {
        app.quota = quota::DailyQuota::load(&quota::today());
    }
    app.workspace_status = workspace_status;
    // Said once, on the welcome screen: a migration nobody is told about is
    // one nobody can verify.
    app.startup_notices = migration.iter().map(|m| m.notice()).collect();
    for name in paths::legacy_env_vars_in_use() {
        app.startup_notices.push(format!(
            "{name} is deprecated. Rename it to {}_{} — the old name still works for now.",
            paths::ENV_PREFIX,
            name.trim_start_matches("BOXCODE_")
        ));
    }
    app.workspace_root = workspace
        .as_ref()
        .map(|ws| ws.root().display().to_string())
        .unwrap_or_default();
    let (tx, mut rx) = mpsc::channel::<(u64, StreamEvent)>(256);
    // A second channel rather than more variants on `StreamEvent`: a
    // deployment is not the model talking, and folding it into the LLM
    // transport's event type would make every match there carry cases that
    // cannot occur.
    let (deploy_tx, mut deploy_rx) = mpsc::channel::<deploy::DeployEvent>(256);

    let result = run_app(
        &mut terminal,
        &mut app,
        workspace.as_ref(),
        tx,
        &mut rx,
        deploy_tx,
        &mut deploy_rx,
        enhanced,
    )
    .await;

    restore_terminal(enhanced)?;
    if let Err(e) = &result {
        eprintln!("Error: {e}");
    }
    println!("Goodbye!");
    result
}

/// Resolve the root the model is confined to, and a line describing the outcome.
///
/// A workspace that cannot be opened must not stop the app: it still works as a
/// plain chat client, just without file access, so the failure degrades to a
/// notice on the welcome screen instead of a startup error.
fn open_workspace(config: &Config) -> (Option<Workspace>, String) {
    if !config.tools.enabled {
        return (None, "off (enabled = false in config.toml)".to_string());
    }
    match Workspace::new(&config.tools.workspace) {
        Ok(workspace) => {
            let root = workspace.root().display().to_string();
            // Every one of these is worth seeing before typing the first prompt:
            // a shell tool can change anything, and unattended mode means it can
            // do so without asking.
            let mut status = format!("commands run in {root}");
            if !config.tools.require_approval {
                status.push_str(" — UNATTENDED, no approval prompt");
            }
            if workspace.is_broad() {
                status.push_str(" — this is a very broad directory");
            }
            (Some(workspace), status)
        }
        Err(e) => (None, format!("off — {e}")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    workspace: Option<&Workspace>,
    tx: mpsc::Sender<(u64, StreamEvent)>,
    rx: &mut mpsc::Receiver<(u64, StreamEvent)>,
    deploy_tx: mpsc::Sender<deploy::DeployEvent>,
    deploy_rx: &mut mpsc::Receiver<deploy::DeployEvent>,
    enhanced: bool,
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
                StreamEvent::ToolCalls(calls) => app.request_tools(calls),
                StreamEvent::ToolsFinished(outcomes) => app.finish_tools(outcomes),
                StreamEvent::Usage(u) => app.record_exact_usage(u),
                StreamEvent::Done => app.finish_stream(),
                StreamEvent::Notice(note) => app.note(note),
                StreamEvent::Error(err) => app.fail_stream(err),
            }
        }

        // Deployment progress. Same shape as the token drain above, and drained
        // to exhaustion for the same reason: a build emits output in bursts,
        // and one line per frame would show a finished build still scrolling.
        while let Ok(event) = deploy_rx.try_recv() {
            app.handle_deploy_event(event);
        }

        // Work the deployment flow asked for. Spawned, never awaited inline:
        // a build takes minutes, and running it on the event loop would freeze
        // the whole UI -- no redraw, no spinner, no Esc.
        if let Some(action) = app.deploy_action.take() {
            match action {
                deploy::DeployAction::Run { step, command, cwd } => {
                    let deploy_tx = deploy_tx.clone();
                    let handle = tokio::spawn(async move {
                        let output = deploy::runner::run(&command, &cwd, Some(&deploy_tx)).await;
                        let _ = deploy_tx
                            .send(deploy::DeployEvent::Finished { step, output })
                            .await;
                    });
                    app.deploy_abort = Some(handle.abort_handle());
                }

                // A browser login needs the real terminal: the vendor CLIs
                // print a URL and wait, and with a closed stdin they cannot run
                // their own prompt at all. So the TUI stands down for the
                // duration and is rebuilt afterwards -- the same move an editor
                // makes when it hands over to `$EDITOR`. Deliberately awaited
                // inline rather than spawned: two things cannot own one
                // terminal, and there is nothing to draw while it is not ours.
                deploy::DeployAction::RunInteractive { step, command, cwd } => {
                    restore_terminal(enhanced)?;
                    println!("\n  Handing the terminal to `{}`.", command.program);
                    println!("  Finish signing in here; this app comes back when it is done.\n");

                    let output = deploy::runner::run_interactive(&command, &cwd).await;

                    setup_terminal()?;
                    terminal.clear()?;
                    app.handle_deploy_event(deploy::DeployEvent::Finished { step, output });
                }

                // The one deployment side effect small enough to do inline:
                // a single append, with nothing to stream and nothing to wait
                // for. It lives here rather than in `App` for the same reason
                // `pending_usage` does -- `App`'s tests must not write to a
                // real `$HOME`.
                deploy::DeployAction::Record(entry) => deploy::history::record(&entry),
            }
        }

        // The only place `finish_stream`/`fail_stream`/`cancel`'s queued
        // usage actually reaches disk -- see `App::pending_usage`'s doc
        // comment on why `app.rs` itself never writes this directly. Catches
        // both this loop's own draining above and anything `handle_key`
        // queued earlier this same iteration (a cancel via Esc).
        for (tokens, model) in app.pending_usage.drain(..) {
            usage::record_turn(tokens, &model);
        }
        // Same reasoning, same place: `App` marks the quota dirty, this loop is
        // the only thing that writes it.
        if app.quota_dirty {
            app.quota.save();
            app.quota_dirty = false;
        }

        // Fire a pending request.
        if app.state == AppState::Sending {
            app.request_id += 1;
            let id = app.request_id;
            let endpoint = app.config.llm.endpoint.clone();
            let model = app.config.llm.model.clone();
            let api_key = app.config.llm.api_key.clone();
            let max_tokens = app.config.llm.max_tokens;

            // Withholding the schemas once the budget is spent is what actually
            // stops a runaway loop: the model has nothing left to call, so it
            // answers. Saying "stop" in the prompt alone would only be a request.
            let budget_left = app.tool_steps < app.config.tools.max_steps;
            // Exact counts make the quota real; without them it falls back to the
            // same character estimate `usage.rs` uses.
            let include_usage = app.config.quota.enabled && app.config.quota.include_usage;
            let (schemas, system) = match workspace {
                Some(ws) => (
                    if budget_left { tools::schemas() } else { Vec::new() },
                    Some(tools::system_prompt(ws, &app.config.tools, budget_left)),
                ),
                None => (Vec::new(), None),
            };
            let history = app.history(system.as_deref());
            let tx_clone = tx.clone();

            let handle = tokio::spawn(async move {
                llm::stream_chat(
                    llm::Target { endpoint: &endpoint, model: &model, api_key: &api_key, max_tokens, include_usage },
                    history,
                    schemas,
                    id,
                    tx_clone,
                )
                .await;
            });

            app.abort = Some(handle.abort_handle());
            app.state = AppState::Streaming;
        }

        // Run the commands the user allowed.
        //
        // Spawned rather than run inline: a command may take a minute, and doing
        // it on the event loop would freeze the whole UI -- no redraw, no Esc, no
        // way to tell a slow build from a hang. Results come back on the same
        // channel as tokens, so the stale-request-id guard covers them too.
        if app.state == AppState::ExecutingTools && !app.approved_tools.is_empty() {
            let calls = std::mem::take(&mut app.approved_tools);
            let tools_config = app.config.tools.clone();
            match workspace {
                Some(ws) => {
                    let ws = ws.clone();
                    let id = app.request_id;
                    let tx_clone = tx.clone();
                    let handle = tokio::spawn(async move {
                        let mut outcomes = Vec::with_capacity(calls.len());
                        for call in &calls {
                            outcomes.push(tools::execute(call, &ws, &tools_config).await);
                        }
                        let _ = tx_clone
                            .send((id, StreamEvent::ToolsFinished(outcomes)))
                            .await;
                    });
                    app.abort = Some(handle.abort_handle());
                }
                // Only reachable if a model invents tool calls for a schema it
                // was never sent. Answer them anyway, or the history is left
                // invalid and the next prompt fails instead of this one.
                None => app.fail_stream(
                    "The model asked to run a command, but the command tool is not enabled."
                        .to_string(),
                ),
            }
        }

        if app.should_exit {
            break;
        }
    }

    Ok(())
}

fn print_help() {
    println!(
        "boxcode {VERSION}
Terminal UI for an OpenAI-compatible LLM endpoint.

USAGE:
    boxcode [FLAGS]

FLAGS:
    -V, --version    Print version and exit
    -h, --help       Print this help and exit
    -u, --upgrade    Update to the latest release
    -f, --force      With --upgrade: reinstall even if already up to date

CONFIG (environment overrides ~/.boxcode/config.toml):
    BOXCODE_ENDPOINT    Base URL, e.g. https://llm.internal:8443
    BOXCODE_MODEL       Model name
    BOXCODE_API_KEY     Bearer token

TOOLS (read_file, write_file, run_command; writes and commands need your
       approval each time -- see the [tools] table in config.toml):
    BOXCODE_WORKSPACE       Directory these operate in (default: cwd)
    BOXCODE_TOOLS_ENABLED   Set to 0 to send no tool schema at all
    BOXCODE_TOOLS_APPROVAL  Set to 0 to stop asking before each write/command.
                              For scripted testing only -- it hands the model
                              unattended file and shell access.
                              See the [tools] table in config.toml for
                              auto_approve_read_only, command_timeout_secs,
                              max_output_bytes, max_steps.

UPGRADE:
    BOXCODE_UPGRADE_URL_BASE
                          Fetch updates from a fork or internal mirror
                          instead of github.com

COMMANDS (type in the input box, press Enter):
    /provider             Pick a provider + model + API key, saved to config.toml
    /model                Pick a model for the currently configured provider
    /new                  Forget the conversation and start fresh
    /usage                What today cost, in tokens and money, plus history
    /quota                What is left of today's budget, and your own limits
    /quota set <what> <n> Set your own limit: requests, tokens or usd
    /quota clear          Remove your own limits
    /quota override       Keep working past today's limit
    /quota reset          Cancel an override
    /deploy               Ship this directory to Vercel or Netlify
    /deployments          Recent deployments from this machine

DEPLOYMENT (see [deploy] in config.toml):
    Uses the provider's own CLI (`vercel` / `netlify`), and offers to install
    it if it is missing -- never without asking. Signing in hands the terminal
    to the provider's own browser login; no secret is typed into this app.
    Environment-variable values and tokens are never logged, shown, or written
    to the deployment history.

DAILY LIMITS (optional, off by default -- every limit is 0 = no limit, so this
              only counts until you set one. See [quota] in config.toml):
    BOXCODE_QUOTA_ENABLED         Set to 0 to disable counting entirely
    BOXCODE_MAX_REQUESTS_PER_DAY  Requests before prompts are refused
    BOXCODE_MAX_TOKENS_PER_DAY    Prompt + completion tokens per UTC day
    BOXCODE_MAX_USD_PER_DAY       Spend per UTC day; needs [quota.pricing]
                                    entries for the models you use, or cost
                                    cannot be computed and reads as unpriced

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
