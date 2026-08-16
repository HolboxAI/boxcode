mod agent;
mod app;
mod approval;
mod artifacts;
mod auth;
mod config;
mod danger;
mod dateutil;
mod db;
mod deploy;
mod llm;
mod notice;
mod plan;
mod providers;
mod quota;
mod requests;
mod session;
mod telemetry;
mod tools;
mod theme;
mod ui;
mod upgrade;
mod usage;
mod workspace;

use app::App;
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
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::{Terminal, TerminalOptions, Viewport};
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
    let mut plan = false;
    let mut resume = false;
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
            "-p" | "--plan" => plan = true,
            "-r" | "--resume" => resume = true,
            other => {
                eprintln!("Unknown argument: {other}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }

    if upgrade {
        // Always a force install. Startup now offers the upgrade whenever
        // there is a newer release, so reaching for `--upgrade` by hand means
        // "reinstall regardless" -- either main has moved without a version
        // bump, or the install itself is suspect. Answering "already up to
        // date, nothing to do" to someone who typed it deliberately is the
        // unhelpful reading. `--force` is still accepted and now redundant.
        let _ = force;
        // Handled here rather than returned: the default runtime handler prints
        // Err via Debug, which turns a connection failure into a wall of
        // struct-dump instead of a sentence.
        if let Err(e) = upgrade::run(true).await {
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

    // Asked before `Config::load`, because loading is what performs the
    // adoption (see `config::adopt_legacy_dir`) and afterwards there is
    // nothing left to detect.
    let migrating = config::legacy_dir_pending();

    let config = Config::load()?;
    // Before anything is drawn: the colours depend on the terminal's
    // background, and asking for that needs the terminal to itself, with
    // no alternate screen up and nothing else reading stdin.
    theme::init(theme::resolve_mode(&config.ui.theme));

    // Before the terminal is taken over, so this is an ordinary question on an
    // ordinary shell: the answer may be to run the installer, which prints its
    // own progress and needs stdout to itself.
    if let Some(latest) = upgrade::check_on_start(config.update.check_on_start).await {
        if offer_upgrade(&latest).await {
            return Ok(());
        }
    }

    // Set only by a `/pull` relaunch (see `relaunch_in`), never by a user --
    // this is how the new process knows both where to root itself and that
    // it should check for pending change requests below, instead of every
    // ordinary launch paying for that check.
    let pull_dir = std::env::var("BOXCODE_PULL_DIR").ok();
    let (workspace, workspace_status) = open_workspace(&config, pull_dir.as_deref());

    // Detached, not awaited: a slow or unreachable telemetry endpoint must
    // never delay the terminal coming up. See telemetry.rs -- this is a
    // no-op until a real endpoint is configured, and every failure inside it
    // is already silent.
    tokio::spawn(telemetry::ping_active_if_new_day(VERSION));

    let enhanced = setup_terminal()?;

    let backend = CrosstermBackend::new(io::stdout());
    // An inline viewport owns only the bottom strip; everything above it is
    // ordinary terminal output. `VIEWPORT_ROWS` is fixed because ratatui takes
    // the inline height at construction -- which is workable precisely because
    // the approval prompt scrolls inside its own box rather than growing.
    const VIEWPORT_ROWS: u16 = 12;
    // Setting up an inline viewport asks the terminal where the cursor is and
    // waits for the answer. Practically every terminal replies -- it is a far
    // older and better-supported query than the OSC background one that had to
    // be removed for hanging -- but "practically every" is not "every", and a
    // terminal that stays silent must not leave the app unable to start at all.
    //
    // So: fall back to the full screen. That loses the scrollback this whole
    // change exists to provide, which is worth saying out loud rather than
    // failing silently, but it does start.
    let (mut terminal, alternate_screen) = match Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_ROWS),
        },
    ) {
        Ok(terminal) => (terminal, false),
        Err(e) => {
            eprintln!(
                "Note: this terminal did not report its cursor position ({e}), so session \
                 history will not go to its scrollback."
            );
            crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
            (Terminal::new(backend)?, true)
        }
    };
    install_panic_hook(enhanced, alternate_screen);

    let mut app = App::new(config);
    if plan {
        app.mode = tools::Mode::Plan;
    }
    // Loaded before the first prompt so a limit already spent today is in force
    // from the start, not after the first request slips through.
    if app.config.quota.enabled {
        app.quota = quota::DailyQuota::load(&quota::today());
    }
    app.workspace_status = workspace_status;
    // Said once, on the welcome screen: a migration nobody is told about is
    // one nobody can verify.
    if migrating {
        app.startup_notices.push(
            "Renamed to boxcode: your settings and history moved from ~/.tuisample-code to \
             ~/.boxcode."
                .to_string(),
        );
    }
    for name in config::deprecated_env_vars_in_use() {
        app.startup_notices.push(format!(
            "{name} is deprecated. Rename it to BOXCODE_{} — the old name still works for now.",
            name.trim_start_matches("TUISAMPLE_")
        ));
    }
    app.workspace_root = workspace
        .as_ref()
        .map(|ws| ws.root().display().to_string())
        .unwrap_or_default();
    // The project's plan, if it has one. Read before the welcome panel is
    // drawn, and picked up without being asked: a `plan.md` sitting in the
    // project is the plan, and needing a command to say so would just be a
    // step between the file and the obvious meaning of it being there. What
    // was found is stated on the welcome panel either way.
    if let Some(ws) = workspace.as_ref() {
        match plan::open(ws.root()) {
            Some(Ok(found)) => app.adopt_plan(found),
            Some(Err(e)) => app.note_unreadable_plan(&e),
            None => {}
        }
    }
    // Only checked on a `/pull` relaunch, not every ordinary launch -- the
    // whole point of `/pull` landing here is "select, then see the changes"
    // in one motion (see the design discussion this came out of), so this is
    // where that promise gets kept. A failure here (no endpoint configured,
    // not actually published, the control-plane unreachable) says nothing
    // rather than turning a step /pull was never really about into a reason
    // landing in the project fails.
    if pull_dir.is_some() {
        if let Some(ws) = workspace.as_ref() {
            if let Ok(requests) =
                requests::list_pending(ws.root(), &app.config.tools.requests_endpoint).await
            {
                if !requests.is_empty() {
                    app.startup_notices.push(format!(
                        "{} pending change request{} for this project -- call \
                         list_change_requests to see them.",
                        requests.len(),
                        if requests.len() == 1 { "" } else { "s" }
                    ));
                }
            }
        }
    }
    // Everything said in this conversation lands in a session file as it
    // happens; `--resume` reloads the last one before the first keystroke.
    // The log itself creates no file until there is a message to put in it.
    let mut session_log = session::SessionLog::new(&app.workspace_root);
    if resume {
        app.resume_latest();
    }
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
        &mut session_log,
        tx,
        &mut rx,
        deploy_tx,
        &mut deploy_rx,
        enhanced,
        alternate_screen,
    )
    .await;

    restore_terminal(enhanced, alternate_screen)?;
    // `/pull` set this instead of just exiting -- checked only after the
    // terminal is back to normal (raw mode and the alternate screen both
    // released), since the relaunched process needs the real terminal to set
    // up its own. See `relaunch_in` for why this is a fresh process at all
    // rather than an in-place switch.
    if let Some(dir) = app.pending_relaunch.take() {
        relaunch_in(&dir)?;
        // Reaching here at all means `relaunch_in` could not replace this
        // process (the non-Unix path exits directly on success) -- fall
        // through to the ordinary exit rather than pretending nothing
        // happened.
    }
    if let Err(e) = &result {
        eprintln!("Error: {e}");
    }
    println!("Goodbye!");
    result
}

/// Re-launches the boxcode binary rooted at `dir`, in place of this process
/// on Unix (`exec` -- never returns on success) or by spawning and waiting on
/// platforms without one. `Workspace` is built once at startup and held for
/// the life of the process (see `workspace.rs`); there is no in-place way to
/// point a running session at a different project, so `/pull` is a fresh
/// process, not a live switch.
fn relaunch_in(dir: &std::path::Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.env("BOXCODE_PULL_DIR", dir);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns at all if exec itself failed -- success replaces this
        // process outright, so there is nothing after this line to reach.
        Err(cmd.exec())
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

/// Resolve the root the model is confined to, and a line describing the outcome.
///
/// A workspace that cannot be opened must not stop the app: it still works as a
/// plain chat client, just without file access, so the failure degrades to a
/// notice on the welcome screen instead of a startup error.
/// Print any newly-finished messages above the viewport.
///
/// `insert_before` needs the height up front, so each message is laid out
/// twice: once to count the lines, once to draw them. That is cheap next to
/// the alternative of guessing and either clipping the message or leaving a
/// gap.
fn flush_to_scrollback<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    if app.welcome_flushed && app.drainable().is_empty() && app.streamed_ready().is_none() {
        return Ok(());
    }
    let width = terminal.size()?.width;
    // Same gutter the viewport uses, so a message does not shift sideways as
    // it crosses from one to the other.
    let text_width = width.saturating_sub(3).max(1) as usize;

    if !app.welcome_flushed {
        let lines = ui::welcome_lines(app, text_width);
        let height = lines.len() as u16;
        terminal.insert_before(height, |buf| {
            let area = Rect { x: 0, y: 0, width, height };
            ratatui::widgets::Paragraph::new(lines)
                .block(
                    ratatui::widgets::Block::default()
                        .padding(ratatui::widgets::Padding::new(2, 1, 0, 0)),
                )
                .render(area, buf);
        })?;
        app.welcome_flushed = true;
    }

    // Push finished lines of the in-flight reply up as they complete, so a long
    // answer scrolls the terminal like ordinary output instead of being trimmed
    // to whatever fits the strip at the bottom.
    if let Some(ready) = app.streamed_ready() {
        let text = ready.to_string();
        // One trailing newline, not all of them: that one is the last line's
        // own terminator and would otherwise print an extra blank row. Any
        // before it are deliberate blank lines in the reply, and a chunk now
        // spans several lines (see `safe_flush_end`), so stripping the lot
        // would swallow the paragraph break after a table or code block.
        let lines: Vec<_> = ui::wrapped_lines(
            text.strip_suffix('\n').unwrap_or(&text),
            text_width,
        );
        if !lines.is_empty() {
            let height = lines.len() as u16;
            terminal.insert_before(height, |buf| {
                let area = Rect { x: 0, y: 0, width, height };
                ratatui::widgets::Paragraph::new(lines)
                    .block(
                        ratatui::widgets::Block::default()
                            .padding(ratatui::widgets::Padding::new(2, 1, 0, 0)),
                    )
                    .render(area, buf);
            })?;
        }
        app.stream_printed += text.len();
    }

    let pending: Vec<_> = app.drainable().to_vec();
    for msg in pending {
        let lines = ui::message_lines(&msg, text_width);
        if lines.is_empty() {
            app.flushed += 1;
            continue;
        }
        let height = lines.len() as u16;
        terminal.insert_before(height, |buf| {
            let area = Rect { x: 0, y: 0, width, height };
            ratatui::widgets::Paragraph::new(lines)
                .block(
                    ratatui::widgets::Block::default()
                        .padding(ratatui::widgets::Padding::new(2, 1, 0, 0)),
                )
                .render(area, buf);
        })?;
        app.flushed += 1;
    }
    Ok(())
}

/// `override_root` is `/pull`'s doing: a relaunch (see `relaunch_in`) passes
/// the chosen project's path via `BOXCODE_PULL_DIR` rather than trusting
/// `config.tools.workspace`, since a developer who has customized that
/// setting to something other than the default "." must not have `/pull`
/// silently land in the wrong place.
fn open_workspace(config: &Config, override_root: Option<&str>) -> (Option<Workspace>, String) {
    if !config.tools.enabled {
        return (None, "off (enabled = false in config.toml)".to_string());
    }
    let root = override_root.unwrap_or(&config.tools.workspace);
    match Workspace::new(root) {
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
    session_log: &mut session::SessionLog,
    tx: mpsc::Sender<(u64, StreamEvent)>,
    rx: &mut mpsc::Receiver<(u64, StreamEvent)>,
    deploy_tx: mpsc::Sender<deploy::DeployEvent>,
    deploy_rx: &mut mpsc::Receiver<deploy::DeployEvent>,
    enhanced: bool,
    alternate_screen: bool,
) -> Result<(), Box<dyn Error>> {
    loop {
        // Hand finished messages to the terminal before drawing. Once printed
        // they are the terminal's -- its scrollback, its selection, its search
        // -- and this loop never touches them again.
        flush_to_scrollback(terminal, app)?;
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
        // The dispatch itself (and the stale-request guard) lives in
        // `agent::handle_event` -- the loop's job is only to pump the channel.
        while let Ok((id, event)) = rx.try_recv() {
            agent::handle_event(app, id, event);
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
                    restore_terminal(enhanced, alternate_screen)?;
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
        // The session record, same place as the other files this loop owns.
        // A length comparison almost every tick; messages hit the disk the
        // tick they appear, so a Ctrl-C loses nothing said before it.
        if app.session_reset {
            app.session_reset = false;
            session_log.reset();
        }
        session_log.append(&app.messages);
        // Same reasoning again: `App` marks the plan dirty, this loop writes
        // it. A failed write is reported rather than swallowed -- the whole
        // value of an approved plan is that it is on disk, so silently not
        // being there is the one outcome the user must not be left guessing at.
        if app.plan_dirty {
            app.plan_dirty = false;
            if let Some(plan) = &app.active_plan {
                if let Err(e) = plan.save() {
                    app.note_plan_save_failure(&e);
                }
            }
        }

        // The agent loop's two active steps: fire the request `App` queued,
        // and run what the user allowed. Both are no-ops unless `App` is in
        // the matching state, and both live in `agent.rs` -- this loop only
        // decides *when* they get a chance to run, not what they do.
        agent::fire_request(app, workspace, &tx);
        agent::execute_approved(app, workspace, &tx);

        if app.should_exit {
            break;
        }
    }

    Ok(())
}

/// Offer the newer release, and install it if asked to. `true` means the
/// process should stop here rather than carry on into the app.
///
/// Defaults to no. An update prompt is not the thing anyone opened the
/// terminal for, so a stray Enter must let them get on with what they were
/// doing rather than start replacing the binary underneath them.
async fn offer_upgrade(latest: &str) -> bool {
    use std::io::{IsTerminal, Write};

    // Nothing to prompt with. A piped or redirected stdin means a script, a
    // CI job or an editor integration -- all of which would hang forever on a
    // question nobody is there to answer.
    if !io::stdin().is_terminal() {
        return false;
    }

    println!();
    println!("⬆️  boxcode {latest} is available (you have {VERSION}).");
    print!("   Install it now? [y/N] ");
    let _ = io::stdout().flush();

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        println!("   Skipped. Run `boxcode --upgrade` whenever you want it.");
        println!();
        return false;
    }

    // Said before the installer starts, not left to be discovered. It writes
    // to /usr/local/bin via `sudo`, so on most machines a password prompt is
    // about to appear -- and an unexplained "Password:" arriving in the middle
    // of starting a coding assistant reads as the app having hung, which is
    // exactly how it was reported.
    println!();
    println!("   Installing to /usr/local/bin — sudo may ask for your password.");
    println!();

    let outcome = upgrade::run(true).await;

    // Whatever happened, hand back a usable terminal.
    //
    // `sudo` switches off echo and canonical mode to read a password. If it is
    // interrupted mid-read -- Ctrl-C, a wrong password, the user giving up --
    // those settings can be left as they are, and the shell that follows has
    // no line editing, no working backspace and no working Ctrl-C. The
    // symptom is a terminal that looks frozen while echoing `^M` and `^C`,
    // and it outlives this process, which makes it far worse than the failed
    // upgrade that caused it.
    restore_cooked_mode();

    if let Err(e) = outcome {
        // Not fatal: the install failed, but the build already here still
        // runs, and refusing to start it would turn a missed update into an
        // unusable tool.
        eprintln!("❌ Upgrade failed: {e}");
        eprintln!("   Carrying on with {VERSION}.");
        eprintln!();
        return false;
    }
    println!();
    println!("✓ Updated to {latest}. Start boxcode again to use it.");
    true
}

/// Put the terminal back into ordinary line-editing mode.
///
/// Belt and braces, and both are needed. `disable_raw_mode` undoes anything
/// crossterm itself put in place and is a no-op otherwise; `stty sane` undoes
/// what a *child process* left behind, which crossterm knows nothing about and
/// cannot restore, because it never saved those settings in the first place.
///
/// Every failure is ignored: this runs on the path where something has already
/// gone wrong, and a terminal that could not be reset is not a reason to also
/// refuse to start.
fn restore_cooked_mode() {
    let _ = disable_raw_mode();

    #[cfg(unix)]
    {
        use std::process::{Command, Stdio};
        // `sane` rather than a saved-and-restored termios: this is recovering
        // from another program's mess, so there is no earlier state of ours
        // worth returning to -- only a known-good one.
        let _ = Command::new("stty")
            .arg("sane")
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
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
    -u, --upgrade    Reinstall the latest release, whether or not the version
                       number changed. Starting boxcode already offers an
                       upgrade when there is a newer release, so reaching for
                       this by hand means \"reinstall regardless\".
    -f, --force      Accepted and now redundant: --upgrade always forces.
    -p, --plan       Start in plan mode: the model researches and proposes a
                       plan, and cannot write, edit, or run anything that
                       changes the project until you approve one. Toggle it
                       any time with /plan.
    -r, --resume     Pick up this directory's most recent session where it
                       left off. Sessions are recorded as you work, under
                       ~/.boxcode/sessions/. Also available mid-session
                       as /resume.

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
    Starting boxcode checks for a newer release at most once a day and offers
    to install it. It answers no by default, gives up after two seconds, and
    says nothing at all when it cannot reach the network.

    BOXCODE_UPGRADE_URL_BASE
                          Fetch updates from a fork or internal mirror
                          instead of github.com
    BOXCODE_NO_UPDATE_CHECK
                          Set to anything to skip the startup check. Same as
                          check_on_start = false in the [update] table.

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

DEPLOYMENT (ask the model, e.g. 'deploy this to Vercel'; see [deploy] in
            config.toml):
    Not a slash command -- a deployment needs a provider and a target to mean
    anything, and asking carries both. Uses the provider's own CLI
    (`vercel` / `netlify`) and offers to install it if it is missing, never
    without asking. Signing in hands the terminal to the provider's own
    browser login; no secret is typed into this app. Environment-variable
    values and tokens are never logged, shown, or written to
    ~/.boxcode/deployments.jsonl.

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
    // Deliberately no alternate screen. That buffer has no scrollback of its
    // own, so everything the session had ever printed became unreachable the
    // moment it left the viewport -- and vanished entirely on exit. Staying on
    // the normal buffer hands the history to the terminal, where the wheel,
    // text selection and the terminal's own search already work.
    crossterm::execute!(stdout, EnableBracketedPaste)?;

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

fn restore_terminal(enhanced: bool, alternate_screen: bool) -> Result<(), Box<dyn Error>> {
    let mut stdout = io::stdout();
    if enhanced {
        let _ = crossterm::execute!(stdout, PopKeyboardEnhancementFlags);
    }
    crossterm::execute!(stdout, DisableBracketedPaste)?;
    if alternate_screen {
        crossterm::execute!(stdout, LeaveAlternateScreen)?;
    }
    disable_raw_mode()?;
    Ok(())
}

/// Without this a panic leaves the terminal in raw mode, with the backtrace
/// invisible -- and on the alternate screen too, when that fallback is in use.
fn install_panic_hook(enhanced: bool, alternate_screen: bool) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(enhanced, alternate_screen);
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `restore_cooked_mode` runs on the failure path, where something has
    /// already gone wrong, and it runs in CI and under `cargo test` where
    /// stdin is a pipe rather than a terminal. It must therefore never block,
    /// never panic, and never care that `stty` failed -- a terminal that could
    /// not be reset is not a reason to also refuse to start.
    ///
    /// The repair itself is a terminal side effect and is verified against a
    /// real pty rather than here: an interrupted `sudo` password read clears
    /// ECHO, ICANON and ISIG (which is precisely "Enter, backspace and Ctrl-C
    /// stop working"), and `stty sane` restores all three.
    #[test]
    fn restoring_the_terminal_is_safe_when_there_is_no_terminal() {
        restore_cooked_mode();
        // Twice, because the failure path can reach it more than once.
        restore_cooked_mode();
    }

    /// `/pull`'s relaunch passes the chosen project via `override_root` --
    /// this has to win over `config.tools.workspace`, or a developer who
    /// customized that setting away from the default "." would find `/pull`
    /// silently landing back in their configured directory instead of the
    /// one they picked.
    #[test]
    fn a_pull_relaunch_overrides_the_configured_workspace() {
        let configured = tempfile::tempdir().expect("temp dir");
        let picked = tempfile::tempdir().expect("temp dir");

        let mut config = Config::default();
        config.tools.workspace = configured.path().display().to_string();

        let (ws, _status) =
            open_workspace(&config, Some(&picked.path().display().to_string()));
        let ws = ws.expect("workspace should open");
        assert_eq!(ws.root(), picked.path().canonicalize().expect("canonicalize"));
    }

    /// No override at all (an ordinary launch, not a `/pull` relaunch) still
    /// falls back to whatever the config says, exactly as before this
    /// override existed.
    #[test]
    fn no_override_falls_back_to_the_configured_workspace() {
        let configured = tempfile::tempdir().expect("temp dir");
        let mut config = Config::default();
        config.tools.workspace = configured.path().display().to_string();

        let (ws, _status) = open_workspace(&config, None);
        let ws = ws.expect("workspace should open");
        assert_eq!(ws.root(), configured.path().canonicalize().expect("canonicalize"));
    }
}
