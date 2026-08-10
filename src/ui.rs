use crate::app::{App, AppState, CustomStep, Message, Overlay, Role};
use crate::deploy::{DeploySession, DeployStatus, Menu, Stage, StepState};
use crate::providers;
use crate::theme;
use crate::tools::Action;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
};
use ratatui::Frame;
use std::path::Path;

const MIN_INPUT_HEIGHT: u16 = 3;
const MAX_INPUT_HEIGHT: u16 = 10;
const MAX_APPROVAL_HEIGHT: u16 = 24;
/// Tall enough for every command in `app::COMMANDS` plus its border. There is
/// a test.
const MAX_COMMAND_MENU_HEIGHT: u16 = 14;
/// The deployment panel is allowed to be taller than the approval prompt: it
/// carries a checklist, a menu and a live log at the same time, and clipping
/// the log is what makes a failed build undiagnosable.
const MAX_DEPLOY_HEIGHT: u16 = 28;
const MIN_POPUP_WIDTH: u16 = 40;
const MIN_POPUP_HEIGHT: u16 = 6;

pub fn render(f: &mut Frame, app: &mut App) {
    let size = f.size();

    // A tool approval takes the input box's spot at the bottom instead of
    // floating a popup over the transcript above it -- it answers "what do
    // you want to do about the thing just proposed", which is exactly what
    // that spot is for. The transcript stays fully visible and in place, the
    // way it does for every other kind of turn.
    let approval = match &app.overlay {
        Some(Overlay::ToolApproval { action, remaining }) => Some((action.clone(), *remaining)),
        _ => None,
    };

    // `/deploy` takes the same spot for the same reason: it is a conversation
    // about what to do next, and it streams. Floating it over the transcript
    // would hide the thing being deployed while asking about it.
    let deploying = app.overlay == Some(Overlay::Deploy) && app.deploy.is_some();

    let bottom_height = if deploying {
        let inner_width = size.width.saturating_sub(4).max(1) as usize;
        let lines = deployment_lines(app, inner_width);
        // Deliberately not clamped against the terminal height: on one shorter
        // than `MIN_INPUT_HEIGHT` that would put the ceiling below the floor,
        // and `clamp` panics on that. `Layout` already caps a constraint at
        // the space it actually has.
        (lines.len() as u16 + 2).clamp(MIN_INPUT_HEIGHT, MAX_DEPLOY_HEIGHT)
    } else {
        match &approval {
            Some((action, remaining)) => {
                let inner_width = size.width.saturating_sub(4).max(1) as usize;
                let (_, lines) = tool_approval_lines(app, action, *remaining, inner_width);
                (lines.len() as u16 + 2).clamp(MIN_INPUT_HEIGHT, MAX_APPROVAL_HEIGHT)
            }
            None => input_height(app, size.width),
        }
    };

    // Slash-command autocomplete: `matching_commands` is already empty
    // whenever a tool approval is showing (that requires `is_busy()`, which
    // the menu also refuses to be active under), so no extra guard is needed
    // to keep the two from appearing at once.
    let command_matches = app.matching_commands();
    let menu_height: u16 = if command_matches.is_empty() {
        0
    } else {
        // The cap has to stay above the number of commands, or the last one
        // added is silently clipped out of its own menu -- which is how a
        // command comes to look like it does not exist.
        (command_matches.len() as u16 + 2).min(MAX_COMMAND_MENU_HEIGHT)
    };

    // The viewport is a strip at the bottom of the real terminal, not a whole
    // screen: finished messages have already been printed above it and belong
    // to the terminal's scrollback now. What is left here is the turn in
    // progress and the controls -- everything that still changes.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(menu_height),
            Constraint::Length(bottom_height),
            Constraint::Length(1),
        ])
        .split(size);

    render_live(f, chunks[0], app);
    if !command_matches.is_empty() {
        render_command_menu(f, chunks[1], app, &command_matches);
    }
    if deploying {
        render_deployment(f, chunks[2], app);
    } else {
        match &approval {
            Some((action, remaining)) => {
                render_tool_approval_inline(f, chunks[2], app, action, *remaining)
            }
            None => render_input(f, chunks[2], app),
        }
    }
    render_footer(f, chunks[3], app);

    // Everything else here (pickers, text prompts) is a one-shot choice made
    // before a turn even starts, with no transcript underneath it yet to stay
    // faithful to -- floating and centered is fine for those.
    render_overlay(f, size, app);

    // Last: rewrite every cell's colours in place if this terminal can't be
    // trusted with the 24-bit RGB the rest of this file draws in -- see
    // `theme::supports_truecolor`. Everything above stays written in terms of
    // `theme`'s real palette either way; this is the one place that acts on
    // whether the terminal can actually show it.
    adapt_colors_for_terminal(f);
}

fn adapt_colors_for_terminal(f: &mut Frame) {
    if theme::supports_truecolor() {
        return;
    }
    for cell in f.buffer_mut().content.iter_mut() {
        cell.fg = theme::adapt(cell.fg);
        cell.bg = theme::adapt(cell.bg);
    }
}


/// The transcript, drawn without a box around it.
///
/// A border here would frame the conversation as a widget in an application;
/// without one the text simply occupies the terminal, which is what a terminal
/// session should feel like. The two-column indent does the job the border was
/// doing -- separating the stream from the edge of the screen -- at a quarter
/// of the visual weight.
/// Assistant prose as drawable lines, wrapped to `width`, with its markdown
/// rendered rather than shown. Shared with the streaming flush so a paragraph
/// looks identical whether it was printed line-by-line as it arrived or all at
/// once when the turn ended -- which is also why the markdown lives here and
/// not at one of the two call sites.
pub fn wrapped_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    markdown_lines(text, width)
}

/// One transcript message as drawable lines.
///
/// Split out of the renderer so the very same lines can be pushed into the
/// terminal's own scrollback (see `main`'s flush loop) and drawn live. If these
/// two ever diverged, a message would change appearance the moment it scrolled
/// out of the viewport.
pub fn message_lines(msg: &Message, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    // Nothing left to draw: a reply whose text was already streamed out line by
    // line arrives here empty, and blank lines in the scrollback are noise.
    if msg.body().trim().is_empty() && msg.tool_calls.is_empty() {
        return lines;
    }

    // Tool activity is scaffolding, not conversation: one dim line each,
    // no speaker label and no blank separator, so a run of six reads as a
    // compact block rather than three screens of transcript. The full
    // result still goes to the model -- it is just not drawn.
    if msg.role == Role::Tool {
        for (i, wrapped) in wrap(msg.body(), width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let marker = if i == 0 { theme::TOOL_MARK } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(theme::p().faint)),
                Span::styled(wrapped, role_style(Role::Tool)),
            ]));
        }
        // Tool lines get no trailing blank: a run of six should read as one
        // compact block, not six separated ones.
        return lines;
    }
    // An assistant turn that was nothing but tool calls has no prose to
    // show; the calls speak for themselves on the lines that follow.
    if msg.role == Role::Assistant
        && !msg.tool_calls.is_empty()
        && msg.content.trim().is_empty()
    {
        return Vec::new();
    }

    // A user turn keeps a marker -- it's the one place the human's own
    // words appear verbatim, and a "> " quote prefix reads as "you typed
    // this" without naming a speaker. The assistant's prose gets none:
    // narration and tool activity share one continuous stream, the way
    // Claude Code renders a turn, rather than a labelled reply to a
    // labelled question. System/Error stay labelled -- they're status
    // events, not a side of the conversation.
    match msg.role {
        Role::User => {
            // Padded to the full width on purpose: a background only
            // colours the cells a span actually occupies, so without this
            // the block would be ragged down its right edge, tracking the
            // length of each wrapped line instead of forming one shape.
            // The marker stays outside the block so the block starts at a
            // consistent column on every line, wrapped or not.
            let text_width = width.saturating_sub(2);
            for (i, wrapped) in wrap(msg.body(), text_width).into_iter().enumerate() {
                let marker = if i == 0 { theme::USER_MARK } else { " " };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} "), role_style(Role::User)),
                    Span::styled(
                        format!("{wrapped:<text_width$}"),
                        theme::user_turn(),
                    ),
                ]));
            }
        }
        Role::Assistant => lines.extend(wrapped_lines(msg.body(), width)),
        Role::Error => {
            // Classified rather than uniformly red: "you have used today's
            // allowance" and "the endpoint is unreachable" are different
            // situations, and a wall of identical red trains people to stop
            // reading the one that mattered.
            let kind = crate::notice::classify(msg.body());
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", kind.icon()), kind.style()),
                Span::styled(kind.headline(), kind.style()),
            ]));
            for wrapped in wrap(msg.body(), width.saturating_sub(2).max(1)) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(wrapped, theme::text()),
                ]));
            }
            if let Some(hint) = kind.hint() {
                for wrapped in wrap(hint, width.saturating_sub(4).max(1)) {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("→ ", Style::default().fg(kind.color())),
                        Span::styled(wrapped, theme::faint()),
                    ]));
                }
            }
        }
        Role::System => {
            lines.push(Line::from(vec![Span::styled(
                format!("{}: ", msg.role.label()),
                role_style(msg.role),
            )]));
            for wrapped in wrap(msg.body(), width) {
                lines.push(Line::from(Span::styled(wrapped, theme::text())));
            }
        }
        Role::Tool => unreachable!("handled above"),
    }
    lines.push(Line::from(""));

    lines.push(Line::from(""));
    lines
}

/// The part of the transcript that is still moving: the welcome panel before
/// anything has been said, then whatever the current turn has produced so far.
///
/// Anything finished has already been printed above the viewport, so drawing it
/// here too would show it twice.
fn render_live(f: &mut Frame, area: Rect, app: &mut App) {
    const GUTTER: u16 = 2;
    if area.height == 0 {
        return;
    }
    let width = area.width.saturating_sub(GUTTER + 1).max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();

    {
        // Messages the flush loop has not taken yet -- during a turn that is
        // everything it produced, since flushing waits for the turn to end.
        for msg in app.messages.iter().skip(app.flushed) {
            lines.extend(message_lines(msg, width));
        }
        if app.state == AppState::ExecutingTools {
            for call in &app.running_tools {
                let label = crate::tools::describe_action(call)
                    .map(|a| a.label())
                    .unwrap_or_else(|| call.function.name.clone());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", theme::TOOL_MARK),
                        Style::default().fg(theme::p().faint),
                    ),
                    Span::styled(label, role_style(Role::Tool)),
                ]));
            }
        }
        if app.state == AppState::Streaming {
            // Only the tail: everything before `stream_printed` is already
            // above the viewport, in the terminal's scrollback.
            let unprinted = app
                .streaming_response
                .get(app.stream_printed..)
                .unwrap_or_default();
            if !unprinted.is_empty() {
                lines.extend(wrapped_lines(unprinted, width));
            }
        }
        if let Some(status) = activity_line(app) {
            lines.push(status);
        }
    }

    // Only the tail fits, and the tail is the part that is still arriving --
    // the rest is a moment away from being printed above anyway.
    let height = area.height as usize;
    let skip = lines.len().saturating_sub(height);
    let shown: Vec<Line> = lines.into_iter().skip(skip).collect();

    f.render_widget(
        Paragraph::new(shown)
            .block(Block::default().padding(Padding::new(GUTTER, 1, 0, 0))),
        area,
    );
}


/// The spinner line: what the app is doing, how long it has been doing it, and
/// how to stop it. `None` when nothing is running.
fn activity_line(app: &App) -> Option<Line<'static>> {
    // The deployment panel draws its own spinner, for its own step, with its
    // own elapsed time. Without this the transcript sat underneath it saying
    // "Running 0 commands…" -- counting a `running_tools` list that a
    // deployment never fills, because it is not run as an ordinary tool.
    if app.overlay == Some(Overlay::Deploy) {
        return None;
    }
    let elapsed = app.busy_started.map(|t| t.elapsed());
    let secs = elapsed.map(|e| e.as_secs()).unwrap_or(0);
    let frame = theme::spinner(elapsed.unwrap_or_default());

    let (verb, detail) = match app.state {
        AppState::AwaitingInput => return None,
        AppState::AwaitingApproval => return None,
        AppState::Sending => ("Thinking".to_string(), String::new()),
        AppState::Streaming => {
            // See App::approx_tokens_this_turn -- the same estimate the
            // persisted usage log uses, always labelled "~" since it is one.
            let approx_tokens = app.approx_tokens_this_turn();
            let detail = if approx_tokens > 0 {
                format!(" · ~{approx_tokens} tokens")
            } else {
                String::new()
            };
            ("Responding".to_string(), detail)
        }
        AppState::ExecutingTools => {
            let n = app.running_tools.len();
            (
                format!("Running {n} command{}", if n == 1 { "" } else { "s" }),
                String::new(),
            )
        }
    };

    Some(Line::from(vec![
        Span::styled(format!("{frame} "), theme::accent()),
        Span::styled(format!("{verb}… "), Style::default().fg(theme::p().accent_soft)),
        Span::styled(
            format!("({secs}s{detail} · esc to interrupt)"),
            theme::faint(),
        ),
    ]))
}

/// The greeting shown until the first prompt.
///
/// Deliberately not a bordered panel: the mascot sits beside the wordmark on
/// one block, a rule divides that from the facts, and the facts are a plain
/// aligned list. One glance should answer the three questions someone actually
/// has on launch -- which model, where do commands run, what do I type -- and
/// then get out of the way, because the first prompt replaces all of it.
pub fn welcome_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];

    // Beside the mascot: the wordmark, what this is, and who is using it.
    // Blank entries keep the two columns the same height so the rule below
    // lands flush regardless of which side is taller.
    let beside: [Vec<Span>; 5] = [
        vec![
            Span::styled("boxcode", theme::accent_bold()),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), theme::faint()),
        ],
        vec![Span::styled("a terminal coding assistant", theme::faint())],
        vec![],
        vec![
            Span::styled("Welcome back", theme::text()),
            Span::styled(
                greeting_name().map(|n| format!(", {n}")).unwrap_or_default(),
                Style::default().fg(theme::p().text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("!", theme::text()),
        ],
        vec![],
    ];

    let mascot_width = theme::MASCOT[0].chars().count();
    // Below this the two columns collide, so the mascot goes above the text
    // instead of beside it rather than overlapping.
    let side_by_side = width > mascot_width + 34;

    for (row, extra) in theme::MASCOT.iter().zip(beside.iter()) {
        let mut spans = vec![Span::styled(*row, theme::accent())];
        if side_by_side {
            spans.push(Span::raw("    "));
            spans.extend(extra.iter().cloned());
        }
        lines.push(Line::from(spans));
    }
    if !side_by_side {
        lines.push(Line::from(""));
        for extra in beside.iter().filter(|e| !e.is_empty()) {
            lines.push(Line::from(extra.clone()));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "─".repeat(width.min(64)),
        Style::default().fg(theme::p().border),
    )));
    lines.push(Line::from(""));

    // The facts, as an aligned list. `cwd` is the one a user should never have
    // to guess about a tool that can change their files, so the two dangerous
    // configurations shout rather than blend in.
    let field = |name: &str, value: String, style: Style| {
        Line::from(vec![
            Span::styled(format!("{name:<10}"), theme::faint()),
            Span::styled(value, style),
        ])
    };
    lines.push(field(
        "model",
        app.config.llm.model.clone(),
        Style::default().fg(theme::p().accent_soft),
    ));
    lines.push(field(
        "endpoint",
        app.config.llm.endpoint.clone(),
        theme::muted(),
    ));
    if !app.workspace_status.is_empty() {
        let alarming = app.workspace_status.contains("UNATTENDED");
        let colour = if alarming {
            theme::p().danger
        } else if app.workspace_status.starts_with("off") || app.workspace_status.contains("broad") {
            theme::p().warning
        } else {
            theme::p().muted
        };
        let mut style = Style::default().fg(colour);
        if alarming {
            style = style.add_modifier(Modifier::BOLD);
        }
        lines.push(field("cwd", shorten_home(&app.workspace_status), style));
    }

    lines.push(Line::from(""));
    for (name, desc) in app.available_commands() {
        lines.push(Line::from(vec![
            Span::styled(format!("{name:<13}"), theme::key()),
            Span::styled(desc, theme::muted()),
        ]));
    }

    let mut warnings = app.startup_notices.clone();
    warnings.extend(app.config.warnings());
    if !theme::supports_truecolor() {
        warnings.push(
            "This terminal doesn't report reliable 24-bit colour support, so colours are \
             approximated to a close 256-colour palette instead of the exact theme."
                .to_string(),
        );
    }
    if !warnings.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Before you start",
            Style::default().fg(theme::p().warning).add_modifier(Modifier::BOLD),
        )));
        for w in warnings {
            // Wrapped, not clipped: a warning that runs off the right edge
            // loses the half that says what to actually do about it.
            for part in wrap(&w, width) {
                lines.push(Line::from(Span::styled(part, theme::muted())));
            }
        }
    }

    lines.push(Line::from(""));
    for tip in [
        "Ask about this project — it can read files and run commands.",
        "Every command and every write waits for your approval.",
    ] {
        for part in wrap(tip, width) {
            lines.push(Line::from(Span::styled(part, theme::faint())));
        }
    }
    lines.push(Line::from(""));
    lines
}

/// The name to greet, from the environment. `None` rather than a guess when
/// there is nothing to go on -- "Welcome back, unknown!" is worse than
/// "Welcome back!".
fn greeting_name() -> Option<String> {
    for var in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(name) = std::env::var(var) {
            let name = name.trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// `/Users/you/project` -> `~/project`, so a deep path still fits.
fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

fn role_style(role: Role) -> Style {
    match role {
        Role::User => Style::default()
            .fg(theme::p().user)
            .add_modifier(Modifier::BOLD),
        Role::Assistant => Style::default().fg(theme::p().text),
        Role::Error => Style::default()
            .fg(theme::p().danger)
            .add_modifier(Modifier::BOLD),
        Role::System => Style::default()
            .fg(theme::p().accent)
            .add_modifier(Modifier::BOLD),
        Role::Tool => Style::default().fg(theme::p().tool),
    }
}

/// Columns the prompt marker and its trailing space occupy.
const PROMPT_GUTTER: usize = 2;

fn input_width(total_width: u16) -> usize {
    total_width.saturating_sub(2 + PROMPT_GUTTER as u16).max(1) as usize
}

fn input_height(app: &App, total_width: u16) -> u16 {
    let width = input_width(total_width);
    let rows: usize = app
        .input_buffer
        .split('\n')
        .map(|l| hard_wrap_rows(l.chars().count(), width))
        .sum();
    ((rows as u16) + 2).clamp(MIN_INPUT_HEIGHT, MAX_INPUT_HEIGHT)
}

/// The slash-command autocomplete list, shown directly above the input box
/// while the buffer is a bare "/word" matching at least one command -- see
/// `App::matching_commands`. Highlights whichever entry Up/Down has landed on
/// with the same "❯" cursor style the approval prompt uses, so the two menus
/// in this app read as the same kind of thing rather than two different ones.
fn render_command_menu(f: &mut Frame, area: Rect, app: &App, matches: &[(&str, &str)]) {
    let selected = app.command_menu_selected.min(matches.len().saturating_sub(1));
    let lines: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(i, (name, desc))| {
            let on = i == selected;
            let marker = if on { "❯ " } else { "  " };
            let name_style = if on {
                Style::default().fg(theme::p().accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::p().text)
            };
            Line::from(vec![
                Span::styled(marker, theme::accent()),
                Span::styled(format!("{name:<11}"), name_style),
                Span::styled(*desc, theme::faint()),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::p().border));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The prompt box: a rounded rule with a `❯` marker on the first row.
///
/// The marker is drawn as part of the content rather than as a border
/// decoration so it scrolls with the text, and the cursor maths below indents
/// past it by exactly `PROMPT_GUTTER`. Getting those two out of step puts the
/// caret two cells from where the characters actually land, which is the kind
/// of bug that makes an input box feel broken without being obviously wrong.
fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let width = input_width(area.width);
    let busy = app.is_busy();

    let (text, style) = if app.input_buffer.is_empty() {
        let hint = if busy {
            "working… esc to interrupt".to_string()
        } else {
            "Ask anything, or describe a change…".to_string()
        };
        (hint, theme::faint())
    } else {
        (
            app.input_buffer.clone(),
            Style::default().fg(if busy { theme::p().muted } else { theme::p().text }),
        )
    };

    // Hard-wrap ourselves so the cursor position below matches exactly what is drawn.
    let mut rendered: Vec<Line> = Vec::new();
    for logical in text.split('\n') {
        for chunk in hard_wrap(logical, width) {
            let marker = if rendered.is_empty() {
                theme::PROMPT_MARK
            } else {
                " "
            };
            let marker_style = if busy {
                theme::faint()
            } else {
                theme::accent()
            };
            rendered.push(Line::from(vec![
                Span::styled(format!("{marker} "), marker_style),
                Span::styled(chunk, style),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if busy {
            Style::default().fg(theme::p().border)
        } else {
            Style::default().fg(theme::p().accent)
        });

    f.render_widget(Paragraph::new(rendered).block(block), area);

    // Cursor: only meaningful while the user can actually type, and only one
    // widget may claim it per frame -- render_overlay claims it instead while
    // an overlay is active (f.set_cursor is last-write-wins).
    if !busy && app.overlay.is_none() && area.height > 2 && area.width > 2 {
        let (row, col) = app.cursor_position();
        let mut screen_row = 0usize;
        for (i, logical) in app.input_buffer.split('\n').enumerate() {
            if i == row {
                break;
            }
            screen_row += hard_wrap_rows(logical.chars().count(), width);
        }
        screen_row += col / width;
        let screen_col = col % width;

        let max_row = area.height.saturating_sub(3) as usize;
        let x = area.x + 1 + PROMPT_GUTTER as u16 + screen_col.min(width - 1) as u16;
        let y = area.y + 1 + screen_row.min(max_row) as u16;
        f.set_cursor(x, y);
    }
}

/// A dim key bar under the prompt. What the app is *doing* is on the spinner
/// line in the transcript instead -- next to the work, not stranded at the
/// bottom of the screen.
fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys: &[(&str, &str)] = match &app.state {
        // Same reasoning as the approval box below: the deployment panel
        // prints its own keys, directly under the choices they act on.
        _ if app.overlay == Some(Overlay::Deploy) => &[("^c", "exit")],
        // The approval box prints y/n/esc itself, directly under the command
        // they act on. Repeating them here put the same three keys on screen
        // twice, one row apart, which reads as two different prompts.
        AppState::AwaitingApproval => &[("^c", "exit")],
        _ if app.is_busy() => &[("esc", "interrupt"), ("^c", "exit")],
        _ => &[
            ("↵", "send"),
            ("⌥↵", "newline"),
            ("↑↓", "history"),
            ("^c", "exit"),
        ],
    };

    let mut spans = vec![Span::raw("  ")];
    for (i, (key, label)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", theme::faint()));
        }
        spans.push(Span::styled(*key, theme::key()));
        spans.push(Span::styled(format!(" {label}"), theme::faint()));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---- /provider and /model overlays ---------------------------------------------

fn render_overlay(f: &mut Frame, area: Rect, app: &App) {
    // Nothing can be drawn into a frame with no cells, and trying panics inside
    // `Clear`, which indexes the buffer without checking. Terminals do report
    // 0x0 -- during a resize, and on a pty opened without a window size.
    if area.width == 0 || area.height == 0 {
        return;
    }
    match &app.overlay {
        None => {}
        Some(Overlay::ProviderPicker { selected }) => {
            let mut items: Vec<String> =
                providers::PROVIDERS.iter().map(|p| p.label.to_string()).collect();
            items.push("Custom endpoint...".to_string());
            render_picker(f, area, " Select a provider ", &items, *selected);
        }
        Some(Overlay::ModelPicker {
            provider_id,
            selected,
        }) => {
            let provider = providers::find_provider(provider_id)
                .expect("provider_id on a ModelPicker overlay always names a registry entry");
            let items: Vec<String> = provider.models.iter().map(|m| m.to_string()).collect();
            render_picker(
                f,
                area,
                &format!(" Select a model ({}) ", provider.label),
                &items,
                *selected,
            );
        }
        Some(Overlay::ApiKeyPrompt { provider_id, .. }) => {
            let provider = providers::find_provider(provider_id)
                .expect("provider_id on an ApiKeyPrompt overlay always names a registry entry");
            render_text_prompt(
                f,
                area,
                &format!(" API key for {} ", provider.label),
                &format!(
                    "No {} found in env -- paste or type it (Enter to confirm, Esc to cancel)",
                    providers::env_var_name(provider_id)
                ),
                &app.overlay_input,
                true,
            );
        }
        Some(Overlay::CustomEndpoint(step)) => {
            let (title, hint, masked) = match step {
                CustomStep::Endpoint => (
                    " Custom endpoint ",
                    "e.g. https://llm.internal:8443 (Enter to confirm, Esc to cancel)",
                    false,
                ),
                CustomStep::Model { .. } => (
                    " Custom model ",
                    "exact model name the endpoint expects (Enter to confirm, Esc to cancel)",
                    false,
                ),
                CustomStep::ApiKey { .. } => (
                    " API key ",
                    "leave blank if the endpoint needs none (Enter to confirm, Esc to cancel)",
                    true,
                ),
            };
            render_text_prompt(f, area, title, hint, &app.overlay_input, masked);
        }
        // Drawn inline at the bottom of the frame by `render`, not as a
        // floating overlay -- see the comment there.
        Some(Overlay::ToolApproval { .. }) | Some(Overlay::Deploy) => {}
    }
}

/// How many lines of a `write_file` preview to show before eliding the rest.
/// A cap, not a limit on the write itself -- the full content still gets
/// written; this only bounds how tall the popup gets.
const WRITE_PREVIEW_LINES: usize = 20;

/// The approval prompt's content, shared by sizing (`render` needs the line
/// count before it can lay out the frame) and drawing. This is the only thing
/// standing between the model and the machine, so a command or a write's
/// content is shown verbatim and in full -- never elided, never summarised
/// (`write_file` content is capped at `WRITE_PREVIEW_LINES` purely so one huge
/// file cannot produce an unusably tall prompt). Approving something you
/// cannot fully see is not approval.
fn tool_approval_lines(
    app: &App,
    action: &Action,
    remaining: usize,
    inner: usize,
) -> (&'static str, Vec<Line<'static>>) {
    let (title, body, footer) = tool_approval_parts(app, action, remaining, inner);
    let mut lines = body;
    lines.extend(footer);
    (title, lines)
}

/// The prompt split into the part that may scroll and the part that must not.
///
/// `footer` is the y/n choice and its key hint. It is drawn into rows reserved
/// at the bottom of the block rather than appended to the content, because a
/// `Paragraph` clips from the bottom: a command or a file preview long enough
/// to fill the box used to push the only instructions for answering it clean
/// off the screen. The keys still worked, which made it worse -- nothing on
/// screen said what to press.
fn tool_approval_parts(
    app: &App,
    action: &Action,
    remaining: usize,
    inner: usize,
) -> (&'static str, Vec<Line<'static>>, Vec<Line<'static>>) {
    let mut lines: Vec<Line> = Vec::new();

    // A consequential action gets a banner before anything else. The prompt
    // for `rm -rf build` must not look identical to the one for `cargo build`
    // -- that sameness is what trains people to press `y` without reading.
    //
    // The word differs by kind: a deployment is not destroying anything
    // locally, and calling it "destructive" would be the same dishonesty in
    // the other direction.
    if let Some(reason) = crate::tools::action_risk(action, Path::new(&app.workspace_root)).reason()
    {
        let banner = match action {
            Action::Deploy { .. } => "⚠  PUBLISHES",
            _ => "⚠  DESTRUCTIVE",
        };
        lines.push(Line::from(Span::styled(banner, theme::danger_bold())));
        for wrapped in wrap(reason, inner) {
            lines.push(Line::from(Span::styled(
                wrapped,
                Style::default().fg(theme::p().danger),
            )));
        }
        lines.push(Line::from(""));
    }

    let (title, verb) = match action {
        Action::Command { command, purpose } => {
            if let Some(purpose) = purpose {
                for wrapped in wrap(purpose, inner) {
                    lines.push(Line::from(Span::styled(wrapped, theme::faint())));
                }
                lines.push(Line::from(""));
            }
            for wrapped in wrap(command, inner) {
                lines.push(Line::from(Span::styled(
                    format!("$ {wrapped}"),
                    Style::default()
                        .fg(theme::p().text)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            (" Run this command? ", "run")
        }
        Action::Read { path } => {
            lines.push(Line::from(Span::styled(
                format!("📄 {path}"),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            (" Read this file? ", "read")
        }
        Action::Write { path, content } => {
            lines.push(Line::from(Span::styled(
                format!("📝 {path}"),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            if content.is_empty() {
                lines.push(Line::from(Span::styled("(empty file)", theme::faint())));
            } else {
                let total = content.lines().count();
                for (i, line) in content.lines().enumerate() {
                    if i >= WRITE_PREVIEW_LINES {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "… {} more line{}",
                                total - i,
                                if total - i == 1 { "" } else { "s" }
                            ),
                            theme::faint(),
                        )));
                        break;
                    }
                    for wrapped in wrap(line, inner) {
                        lines.push(Line::from(Span::styled(wrapped, theme::text())));
                    }
                }
            }
            (" Write this file? ", "write")
        }
        Action::List { path } => {
            lines.push(Line::from(Span::styled(
                format!("📁 {path}"),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            (" List this directory? ", "list")
        }
        Action::Glob { pattern } => {
            lines.push(Line::from(Span::styled(
                format!("🔎 {pattern}"),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            (" Search for these files? ", "search")
        }
        // An edit shows both spans, because approving a replacement you cannot
        // see is not approval. Unlike a write it does not need the whole file --
        // showing only what changes is the reason to prefer this tool.
        Action::Edit { path, old, new, replace_all } => {
            lines.push(Line::from(Span::styled(
                format!("✏️ {path}{}", if *replace_all { "  (all occurrences)" } else { "" }),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            let mut span = |label: &str, body: &str, colour| {
                lines.push(Line::from(Span::styled(label.to_string(), theme::faint())));
                let total = body.lines().count();
                for (i, line) in body.lines().enumerate() {
                    if i >= WRITE_PREVIEW_LINES {
                        lines.push(Line::from(Span::styled(
                            format!("… {} more line(s)", total - i),
                            theme::faint(),
                        )));
                        break;
                    }
                    for wrapped in wrap(line, inner) {
                        lines.push(Line::from(Span::styled(wrapped, Style::default().fg(colour))));
                    }
                }
                lines.push(Line::from(""));
            };
            span("replace:", old, theme::p().danger);
            span("with:", new, theme::p().success);
            (" Apply this edit? ", "edit")
        }
        Action::Deploy { provider, production, summary } => {
            lines.push(Line::from(Span::styled(
                format!(
                    "🚀 {provider} · {}",
                    if *production { "Production" } else { "Preview" }
                ),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            // What the deployment will actually do, so "deploy this" is not a
            // yes/no about something unspecified.
            if let Some(summary) = summary {
                for wrapped in wrap(summary, inner) {
                    lines.push(Line::from(Span::styled(wrapped, theme::faint())));
                }
            }
            (" Deploy this project? ", "deploy")
        }
        Action::Search { query, max_results } => {
            lines.push(Line::from(Span::styled(
                format!("🔎 {query}"),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "up to {max_results} result{} — sent to a web search service",
                    if *max_results == 1 { "" } else { "s" }
                ),
                theme::faint(),
            )));
            (" Search the web? ", "search")
        }
    };

    lines.push(Line::from(""));
    // A search is not scoped to the project directory the way every other
    // action is -- showing "in <workspace>" here would claim a boundary this
    // one doesn't actually have.
    if !matches!(action, Action::Search { .. }) {
        lines.push(Line::from(Span::styled(
            format!("in {}", app.workspace_root),
            theme::faint(),
        )));
    }
    if remaining > 0 {
        lines.push(Line::from(Span::styled(
            format!("({remaining} more queued after this one)"),
            theme::faint(),
        )));
    }
    lines.push(Line::from(""));

    // Two choices, one highlighted -- Up/Down move the cursor between them,
    // Enter confirms whichever one it is on, and y/n/esc still work directly
    // for anyone who already knows which they want. The highlighted key stays
    // bold and in its own colour either way, so a fast glance at the colour
    // alone (not just the cursor) still tells you which one is live.
    let cursor = |on: bool| if on { "❯ " } else { "  " };
    let dim_unless = |on: bool, base: Style| if on { base } else { theme::faint() };

    lines.push(Line::from(vec![
        Span::styled(cursor(app.approval_selected), theme::accent()),
        Span::styled(
            "y",
            dim_unless(
                app.approval_selected,
                Style::default()
                    .fg(theme::p().success)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        Span::styled(format!(" {verb}"), theme::faint()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(cursor(!app.approval_selected), theme::accent()),
        Span::styled(
            "n",
            dim_unless(
                !app.approval_selected,
                Style::default()
                    .fg(theme::p().danger)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        Span::styled(" skip", theme::faint()),
    ]));
    // The last two lines built above are the y/n choice; they become the
    // footer, with the key hint appended.
    let split = lines.len().saturating_sub(2);
    let mut footer = lines.split_off(split);
    footer.push(Line::from(Span::styled(
        "  ↑↓ choose · enter confirm · esc skip",
        theme::faint(),
    )));

    (title, lines, footer)
}

/// Draws the approval prompt into its reserved region at the bottom of the
/// frame -- see the placement comment on `render`. No `Clear` and no
/// centering: unlike a floating popup, this area belongs to the prompt alone,
/// so there is nothing underneath it to protect or re-center against.
fn render_tool_approval_inline(
    f: &mut Frame,
    area: Rect,
    app: &App,
    action: &Action,
    remaining: usize,
) {
    let inner = area.width.saturating_sub(4).max(1) as usize;
    let (title, body, mut footer) = tool_approval_parts(app, action, remaining, inner);

    let destructive = matches!(action, Action::Command { command, .. }
        if crate::danger::classify(command, Path::new(&app.workspace_root)).is_dangerous());
    let accent = if destructive {
        theme::p().danger
    } else {
        theme::p().accent
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(
            title,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));
    let content = block.inner(area);
    f.render_widget(block, area);
    if content.height == 0 {
        return;
    }

    // The footer gets its rows first and keeps them whatever the body does.
    let footer_height = (footer.len() as u16).min(content.height);
    let body_height = content.height - footer_height;

    if body_height > 0 {
        let body_area = Rect { height: body_height, ..content };
        let scroll = approval_scroll(app, body.len(), body_height);
        f.render_widget(
            Paragraph::new(body.clone()).scroll((scroll, 0)),
            body_area,
        );

        // Say so when there is more, and which way -- an approval box that
        // silently hides half a command is how someone approves the half they
        // could see.
        let hidden_below = body.len().saturating_sub(body_height as usize + scroll as usize);
        // Only mention the scroll keys when there is something to scroll --
        // an unconditional hint is noise on the short prompts, which are most
        // of them.
        if (scroll > 0 || hidden_below > 0) && !footer.is_empty() {
            let last = footer.len() - 1;
            footer[last] = Line::from(Span::styled(
                "  ↑↓ choose · enter confirm · esc skip · PgUp/PgDn scroll",
                theme::faint(),
            ));
        }
        if scroll > 0 || hidden_below > 0 {
            let marker = match (scroll > 0, hidden_below > 0) {
                (true, true) => format!(" ↕ {hidden_below} more "),
                (false, true) => format!(" ↓ {hidden_below} more "),
                (true, false) => " ↑ top ".to_string(),
                (false, false) => String::new(),
            };
            let strip = Rect {
                y: body_area.bottom().saturating_sub(1),
                height: 1,
                ..body_area
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(marker, theme::faint())))
                    .alignment(Alignment::Right),
                strip,
            );
        }
    }

    let footer_area = Rect {
        y: content.bottom() - footer_height,
        height: footer_height,
        ..content
    };
    f.render_widget(Paragraph::new(footer), footer_area);
}

/// The body's scroll offset, clamped to what there actually is to scroll.
///
/// Clamped here rather than where the key is handled: only the renderer knows
/// how tall the box ended up, and that changes with the terminal.
fn approval_scroll(app: &App, body_len: usize, body_height: u16) -> u16 {
    let max = body_len.saturating_sub(body_height as usize) as u16;
    app.approval_scroll.min(max)
}

// ---- /deploy -------------------------------------------------------------------

/// Streamed log lines kept on screen while something runs, and after the user
/// asks to see the detail. A window, not the whole log: the panel shares the
/// screen with the transcript, and the newest lines are the ones being read.
const DEPLOY_LOG_LINES: usize = 8;
const DEPLOY_LOG_LINES_EXPANDED: usize = 18;

/// The deployment panel's content, shared by sizing and drawing exactly as
/// `tool_approval_lines` is -- the frame layout needs the line count before it
/// can allocate the region the panel is then drawn into.
fn deployment_lines(app: &App, inner: usize) -> Vec<Line<'static>> {
    let Some(session) = app.deploy.as_ref() else {
        return Vec::new();
    };
    let mut lines: Vec<Line> = Vec::new();

    // What has already happened, as a checklist. It stays on screen through
    // every later screen, so "where did it get to" never needs scrollback.
    for step in &session.steps {
        let colour = match step.state {
            StepState::Done => theme::p().success,
            StepState::Failed => theme::p().danger,
            StepState::Running => theme::p().accent,
            StepState::Skipped => theme::p().faint,
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", step.state.glyph()), Style::default().fg(colour)),
            Span::styled(step.label.clone(), theme::text()),
        ]));
    }
    if !session.steps.is_empty() {
        lines.push(Line::from(""));
    }

    for (label, value) in session.summary() {
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<17}"), theme::faint()),
            Span::styled(value, theme::text()),
        ]));
    }
    if !session.summary().is_empty() {
        lines.push(Line::from(""));
    }

    match &session.stage {
        Stage::Working(step) => {
            let elapsed = session.started.map(|t| t.elapsed()).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", theme::spinner(elapsed)), theme::accent()),
                Span::styled(
                    format!("{}… ", step.verb()),
                    Style::default().fg(theme::p().accent_soft),
                ),
                Span::styled(
                    format!("({}s · esc to stop)", elapsed.as_secs()),
                    theme::faint(),
                ),
            ]));
            lines.extend(log_lines(session, inner, DEPLOY_LOG_LINES));
        }

        Stage::Prompt(prompt) => {
            // Wrapped, not clipped: these hints carry the half that says what
            // happens to what you are about to type.
            for wrapped in wrap(prompt.hint(), inner) {
                lines.push(Line::from(Span::styled(wrapped, theme::faint())));
            }
            lines.push(Line::from(""));
            // A masked field shows dots for what has been typed, so the length
            // is visible and the value is not.
            let shown = if prompt.masked() {
                "•".repeat(session.input.chars().count())
            } else {
                session.input.clone()
            };
            lines.push(Line::from(if shown.is_empty() {
                Span::styled("(type here)", theme::faint())
            } else {
                Span::styled(shown, theme::text())
            }));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  enter confirm · esc back",
                theme::faint(),
            )));
        }

        Stage::Menu(menu) => {
            if *menu == Menu::Failure {
                if let Some(reason) = &session.failure {
                    lines.push(Line::from(Span::styled("✖  FAILED", theme::danger_bold())));
                    for wrapped in wrap(reason, inner) {
                        lines.push(Line::from(Span::styled(
                            wrapped,
                            Style::default().fg(theme::p().danger),
                        )));
                    }
                    lines.push(Line::from(""));
                }
                if session.show_full_log {
                    lines.extend(log_lines(session, inner, DEPLOY_LOG_LINES_EXPANDED));
                    lines.push(Line::from(""));
                }
            }
            if *menu == Menu::InstallCli {
                for wrapped in wrap(
                    "The provider's CLI is not installed. Nothing is installed without your \
                     say-so.",
                    inner,
                ) {
                    lines.push(Line::from(Span::styled(wrapped, theme::muted())));
                }
                // The guardrails' own verdict on the install command, in the
                // same words the ordinary command-approval prompt would use.
                if let Some(reason) = &session.install_reason {
                    lines.push(Line::from(Span::styled(
                        "⚠  DESTRUCTIVE",
                        theme::danger_bold(),
                    )));
                    for wrapped in wrap(reason, inner) {
                        lines.push(Line::from(Span::styled(
                            wrapped,
                            Style::default().fg(theme::p().danger),
                        )));
                    }
                }
                lines.push(Line::from(""));
            }

            let options = session.options();
            let selected = session.selected.min(options.len().saturating_sub(1));
            for (i, option) in options.iter().enumerate() {
                let on = i == selected;
                lines.push(Line::from(vec![
                    Span::styled(if on { "❯ " } else { "  " }, theme::accent()),
                    Span::styled(
                        option.label.clone(),
                        if on {
                            Style::default()
                                .fg(theme::p().accent)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            theme::text()
                        },
                    ),
                ]));
                if let Some(detail) = &option.detail {
                    for wrapped in wrap(detail, inner.saturating_sub(6).max(1)) {
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled(wrapped, theme::faint()),
                        ]));
                    }
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ↑↓ choose · enter confirm · esc back",
                theme::faint(),
            )));
        }

        Stage::Finished => {
            match (&session.url, &session.failure) {
                (Some(url), _) => {
                    lines.push(Line::from(Span::styled(
                        "Deployment successful!",
                        Style::default()
                            .fg(theme::p().success)
                            .add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        format!("🌐 {} URL", session.target.label()),
                        theme::faint(),
                    )));
                    // Never wrapped: a URL broken across two rows cannot be
                    // copied out of a terminal in one go, which is the only
                    // thing anyone wants to do with it.
                    lines.push(Line::from(Span::styled(
                        url.clone(),
                        Style::default()
                            .fg(theme::p().accent)
                            .add_modifier(Modifier::BOLD),
                    )));
                    if let Some(detail) = &session.status_detail {
                        lines.push(Line::from(""));
                        for wrapped in wrap(detail, inner) {
                            lines.push(Line::from(Span::styled(wrapped, theme::muted())));
                        }
                    }
                    lines.push(Line::from(""));
                    for wrapped in wrap(
                        "Next: open the URL to check it, or run /deploy again to ship a change. \
                         /deployments lists what this machine has shipped.",
                        inner,
                    ) {
                        lines.push(Line::from(Span::styled(wrapped, theme::faint())));
                    }
                }
                (None, Some(reason)) => {
                    lines.push(Line::from(Span::styled(
                        "Deployment did not finish",
                        theme::danger_bold(),
                    )));
                    for wrapped in wrap(reason, inner) {
                        lines.push(Line::from(Span::styled(wrapped, theme::text())));
                    }
                }
                (None, None) => {
                    lines.push(Line::from(Span::styled("Nothing was deployed.", theme::muted())));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  enter close", theme::faint())));
        }
    }

    lines
}

/// The tail of the streamed log, wrapped to the panel.
fn log_lines(session: &DeploySession, inner: usize, budget: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if session.log.is_empty() {
        return lines;
    }
    lines.push(Line::from(""));
    let start = session.log.len().saturating_sub(budget);
    for line in session.log.iter().skip(start) {
        // Truncated rather than wrapped: build output is one long line per
        // event, and wrapping it turns eight lines of log into thirty.
        let clipped: String = line.chars().take(inner.saturating_sub(2).max(1)).collect();
        lines.push(Line::from(vec![
            Span::styled("  ", theme::faint()),
            Span::styled(clipped, Style::default().fg(theme::p().tool)),
        ]));
    }
    lines
}

/// Draws the deployment panel into its reserved region at the bottom of the
/// frame -- the same placement, and the same reasoning, as the tool approval.
fn render_deployment(f: &mut Frame, area: Rect, app: &App) {
    let Some(session) = app.deploy.as_ref() else {
        return;
    };
    let inner = area.width.saturating_sub(4).max(1) as usize;
    let lines = deployment_lines(app, inner);

    // The border colour carries the outcome, so a glance at the shape of the
    // screen answers "did it work" before any text is read.
    let accent = match (&session.stage, &session.finished_status) {
        (Stage::Menu(Menu::Failure), _) => theme::p().danger,
        (Stage::Finished, Some(DeployStatus::Success)) => theme::p().success,
        (Stage::Finished, Some(DeployStatus::Failed)) => theme::p().danger,
        _ => theme::p().accent,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(
            session.title(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));

    // Stick to the bottom: while a build streams, the newest lines are the
    // ones being read, and a panel that scrolled off the top of its own region
    // would show the beginning of a build forever.
    let overflow = (lines.len() as u16).saturating_sub(area.height.saturating_sub(2));
    f.render_widget(
        Paragraph::new(lines).block(block).scroll((overflow, 0)),
        area,
    );

    // Only a text prompt has somewhere for the caret to be. Claimed here
    // rather than in `render_input`, which stands down while an overlay is up.
    if let Stage::Prompt(prompt) = &session.stage {
        if !prompt.masked() && area.height > 2 && area.width > 4 {
            let column = session.input[..session.input_cursor].chars().count();
            let x = area.x + 2 + (column.min(inner.saturating_sub(1))) as u16;
            let y = area.bottom().saturating_sub(4).max(area.y + 1);
            f.set_cursor(x, y);
        }
    }
}

/// Centers a popup sized to its content within `area`, clamped so it never
/// exceeds the available space, with an absolute floor so tiny terminals don't
/// produce an unreadably small popup.
///
/// The floor is applied *before* the clamp, never after: `max(MIN).min(area)`
/// stays inside the frame, while `min(area).max(MIN)` would grow a popup back
/// out past the edge of a small terminal and index off the end of the buffer.
fn centered_rect(desired_width: u16, desired_height: u16, area: Rect) -> Rect {
    let width = desired_width.max(MIN_POPUP_WIDTH).min(area.width);
    let height = desired_height.max(MIN_POPUP_HEIGHT).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn render_picker(f: &mut Frame, area: Rect, title: &str, items: &[String], selected: usize) {
    let popup = centered_rect(50, items.len() as u16 + 2, area);
    f.render_widget(Clear, popup);

    let list_items: Vec<ListItem> = items
        .iter()
        .map(|label| ListItem::new(label.clone()))
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::p().accent))
        .title(Span::styled(title.to_string(), theme::accent_bold()));
    let list = List::new(list_items)
        .block(block)
        .style(theme::text())
        .highlight_style(
            Style::default()
                .fg(theme::p().accent)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("❯ ");

    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, popup, &mut state);
}

/// Single-line text entry (masked or plain). The cursor always sits at the end
/// of `value` -- the overlay's editing model only supports insert/backspace, no
/// repositioning, so there is nothing else for it to reflect.
fn render_text_prompt(
    f: &mut Frame,
    area: Rect,
    title: &str,
    hint: &str,
    value: &str,
    masked: bool,
) {
    let popup = centered_rect(60, 5, area);
    f.render_widget(Clear, popup);

    let display = if masked {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::p().accent))
        .title(Span::styled(title.to_string(), theme::accent_bold()));

    let value_line = if display.is_empty() {
        Line::from(Span::styled("(type here)", theme::faint()))
    } else {
        Line::from(Span::styled(display.clone(), theme::text()))
    };

    let lines = vec![
        Line::from(Span::styled(hint, theme::faint())),
        Line::from(""),
        value_line,
    ];

    f.render_widget(Paragraph::new(lines).block(block), popup);

    let inner_width = popup.width.saturating_sub(2) as usize;
    let col = display.chars().count().min(inner_width);
    f.set_cursor(popup.x + 1 + col as u16, popup.y + 3);
}

// ---- text layout helpers ------------------------------------------------------

fn hard_wrap_rows(len: usize, width: usize) -> usize {
    if width == 0 {
        1
    } else {
        (len / width) + 1
    }
}

/// Split a single logical line into fixed-width chunks (no word breaking), always
/// yielding at least one chunk so empty lines still occupy a row.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

// ---- lightweight markdown ------------------------------------------------------

/// Models write markdown whether or not they are asked to, and a terminal that
/// does not read it shows the punctuation instead of the emphasis: `**live**`
/// where the point was *live*. Rendering a small, safe subset is less work than
/// fighting the model's training, and degrades better -- an unmatched marker
/// stays as literal text rather than eating the rest of the line.
///
/// Deliberately small. Bold, inline code, bullets and headings cover what
/// actually shows up in a terminal answer. Italics are **not** handled: `*` and
/// `_` appear constantly in real prose about code (`snake_case`, globs, `*args`)
/// and a false positive there silently deletes characters, which is worse than
/// showing one asterisk.
///
/// Applied per already-wrapped row, so a bold span broken across a wrap renders
/// as two separate spans rather than one continuous one. That is a cosmetic
/// edge, and the alternative -- styling before wrapping -- means teaching the
/// wrapper about styled runs for very little gain.
fn inline_markdown(line: &str, base: Style) -> Vec<Span<'static>> {
    let code = Style::default()
        .fg(theme::p().accent_soft)
        .add_modifier(Modifier::BOLD);
    let bold = base.add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    // Flush whatever unstyled text has accumulated.
    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut plain), base));
            }
        };
    }

    while i < chars.len() {
        let rest = &chars[i..];
        // `**bold**`
        if rest.starts_with(&['*', '*']) {
            if let Some(end) = find_closer(&chars, i + 2, &['*', '*']) {
                flush!();
                spans.push(Span::styled(chars[i + 2..end].iter().collect::<String>(), bold));
                i = end + 2;
                continue;
            }
        }
        // `` `code` ``
        if chars[i] == '`' {
            if let Some(end) = find_closer(&chars, i + 1, &['`']) {
                flush!();
                spans.push(Span::styled(chars[i + 1..end].iter().collect::<String>(), code));
                i = end + 1;
                continue;
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush!();

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

/// Render a whole message body: block markdown, then wrapping, then inline
/// markdown per row.
///
/// Fenced code blocks are passed through untouched and styled as a unit —
/// inside one, `**` and `#` are code, not formatting, and a wrapped line of
/// code is worse than a clipped one.
fn markdown_lines(body: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_fence = false;

    for logical in body.split('\n') {
        if logical.trim_start().starts_with("```") {
            // The fence itself is punctuation for a renderer, not content.
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            let code = Style::default().fg(theme::p().accent_soft);
            for row in hard_wrap(logical, width.saturating_sub(2).max(1)) {
                lines.push(Line::from(vec![
                    Span::styled("  ", theme::faint()),
                    Span::styled(row, code),
                ]));
            }
            continue;
        }

        let (text, indent, heading) = block_markdown(logical);
        let base = if heading {
            theme::text().add_modifier(Modifier::BOLD)
        } else {
            theme::text()
        };
        for row in wrap(&text, width.saturating_sub(indent.len())) {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if !indent.is_empty() {
                spans.push(Span::raw(indent));
            }
            spans.extend(inline_markdown(&row, base));
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// Where `marker` next closes, at or after `from`. `None` when it never does,
/// which is what keeps an unmatched `*` from swallowing the rest of the line.
fn find_closer(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    if from >= chars.len() {
        return None;
    }
    (from..=chars.len().saturating_sub(marker.len()))
        .find(|&i| chars[i..].starts_with(marker) && i > from)
}

/// Strip a line's block-level markdown, returning the text to draw, how far to
/// indent it, and whether the whole line is a heading.
///
/// `- item` becomes `• item` because a bullet is what was meant; `### Heading`
/// loses its hashes and gains bold, because the hashes were standing in for an
/// emphasis the terminal can just apply.
fn block_markdown(line: &str) -> (String, &'static str, bool) {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();

    if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        return (format!("• {rest}"), if indent >= 2 { "    " } else { "  " }, false);
    }
    if trimmed.starts_with('#') {
        let rest = trimmed.trim_start_matches('#');
        if rest.starts_with(' ') {
            return (rest.trim_start().to_string(), "", true);
        }
    }
    (line.to_string(), "", false)
}

/// Word-wrap message text, preserving explicit newlines.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for logical in text.split('\n') {
        if logical.chars().count() <= width {
            out.push(logical.to_string());
            continue;
        }
        let mut current = String::new();
        let mut current_len = 0usize;
        for word in logical.split(' ') {
            let word_len = word.chars().count();
            if current_len > 0 && current_len + 1 + word_len > width {
                out.push(std::mem::take(&mut current));
                current_len = 0;
            }
            if word_len > width {
                // A single unbreakable token (a URL, a long path): hard-wrap it.
                if current_len > 0 {
                    out.push(std::mem::take(&mut current));
                }
                let mut chunks = hard_wrap(word, width);
                let last = chunks.pop().unwrap_or_default();
                out.extend(chunks);
                current_len = last.chars().count();
                current = last;
                continue;
            }
            if current_len > 0 {
                current.push(' ');
                current_len += 1;
            }
            current.push_str(word);
            current_len += word_len;
        }
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Message;
    use crate::llm::{FunctionCall, ToolCall};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn command_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: crate::tools::RUN_COMMAND.to_string(),
                arguments: serde_json::json!({ "command": command }).to_string(),
            },
        }
    }

    fn rendered_rows(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buffer.get(x, y).symbol()).collect())
            .collect()
    }

    /// The one row (as rendered text) that contains `needle`. Panics if none
    /// or more than one row does -- both mean the assertion that follows
    /// can't mean what it says.
    fn row_containing(rows: &[String], needle: &str) -> String {
        let matches: Vec<&String> = rows.iter().filter(|r| r.contains(needle)).collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one row containing {needle:?}, found {}: {rows:?}",
            matches.len()
        );
        matches[0].clone()
    }

    /// The welcome panel is printed into the terminal's scrollback rather than
    /// drawn in the viewport, so these assert on the lines that get printed --
    /// which is exactly what `main`'s flush loop hands to `insert_before`.
    fn welcome_text(app: &App, width: usize) -> String {
        welcome_lines(app, width)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_text(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn frame(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Regression: a popup wider or taller than the frame indexes off the end of
    /// the buffer, and `Clear` panics rather than clipping. Terminals really do
    /// report sizes this small -- mid-resize, and on a pty with no window size.
    /// The point of the whole module: two different failures must not render
    /// identically. A wall of the same red is what teaches people to skip the
    /// one that mattered.
    #[test]
    fn different_failures_render_differently() {
        let cases = [
            ("Daily limit reached — requests 5 of 5.", "Daily limit reached", "◔"),
            (
                "The reply hit the 16384-token output cap and was cut off.",
                "Reply cut off",
                "✂",
            ),
            (
                "This conversation is too long for the model's context window.",
                "Conversation too long",
                "▣",
            ),
            ("Could not reach http://x:8000: connection refused", "Endpoint unreachable", "⊘"),
        ];

        for (body, headline, icon) in cases {
            let mut app = App::new(crate::config::Config::default());
            app.greeted = true;
            app.messages.push(crate::app::Message::new(Role::Error, body));

            let rendered = rendered_text(&mut app, 100, 30);
            assert!(rendered.contains(headline), "missing headline {headline}: {rendered}");
            assert!(rendered.contains(icon), "missing icon {icon} for {headline}");
            // The undifferentiated label is gone.
            assert!(!rendered.contains("Error: "), "still using the generic label: {rendered}");
        }
    }

    /// A hint has to name the remedy for the failure it sits under, not a
    /// generic one -- that is the whole reason failures are classified.
    #[test]
    fn the_hint_names_the_remedy_that_actually_applies() {
        let mut spent = App::new(crate::config::Config::default());
        spent.greeted = true;
        spent.messages.push(crate::app::Message::new(
            Role::Error,
            "Daily limit reached — requests 5 of 5.",
        ));
        let out = rendered_text(&mut spent, 100, 30);
        assert!(out.contains("/quota"), "should point at the user's own limits: {out}");

        let mut full = App::new(crate::config::Config::default());
        full.greeted = true;
        full.messages.push(crate::app::Message::new(
            Role::Error,
            "This conversation is too long for the model's context window.",
        ));
        let out = rendered_text(&mut full, 100, 30);
        assert!(out.contains("/new"), "should point at the command that clears it: {out}");
    }

    #[test]
    fn an_unrecognised_failure_still_renders_readably() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages
            .push(crate::app::Message::new(Role::Error, "something went sideways"));
        let out = rendered_text(&mut app, 100, 30);
        assert!(out.contains("Error"), "{out}");
        assert!(out.contains("something went sideways"), "{out}");
    }

    /// System messages are not failures and keep their existing presentation.
    #[test]
    fn system_messages_are_untouched_by_the_error_styling() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages
            .push(crate::app::Message::new(Role::System, "Switched to deepseek."));
        let out = rendered_text(&mut app, 100, 30);
        assert!(out.contains("System"), "{out}");
    }

    #[test]
    fn a_notice_survives_a_narrow_terminal() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages.push(crate::app::Message::new(
            Role::Error,
            "Daily limit reached — requests 5 of 5. Resets in 3h.",
        ));
        for (w, h) in [(1, 1), (20, 6), (40, 12), (200, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| render(f, &mut app))
                .unwrap_or_else(|e| panic!("{w}x{h} failed: {e}"));
        }
    }

    #[test]
    fn a_popup_never_escapes_a_small_frame() {
        for (w, h) in [(0, 0), (1, 1), (5, 3), (20, 4), (39, 5), (200, 60)] {
            let area = frame(w, h);
            let popup = centered_rect(60, 12, area);
            assert!(
                popup.width <= area.width && popup.height <= area.height,
                "{w}x{h}: popup {}x{} escaped the frame",
                popup.width,
                popup.height
            );
            assert!(
                popup.right() <= area.right() && popup.bottom() <= area.bottom(),
                "{w}x{h}: popup runs past the frame edge"
            );
        }
    }

    /// The markers and gutters added around the transcript and the prompt all
    /// subtract from the usable width, and every one of those subtractions is a
    /// chance to underflow or to index past the end of a line on a narrow
    /// terminal. Draw every state at every awkward size.
    #[test]
    fn every_screen_renders_at_any_terminal_size() {
        let states = [
            AppState::AwaitingInput,
            AppState::Sending,
            AppState::Streaming,
            AppState::ExecutingTools,
        ];

        for state in &states {
            for greeted in [false, true] {
                let mut app = App::new(crate::config::Config::default());
                app.greeted = greeted;
                app.state = state.clone();
                app.busy_started = Some(std::time::Instant::now());
                app.workspace_status = "/some/rather/long/project/path".to_string();
                app.streaming_response = "a fairly long streamed sentence to force wrapping".into();
                app.running_tools = vec![command_call("c1", "cargo test --all-features")];
                app.messages.push(Message::new(
                    Role::User,
                    "a prompt long enough to wrap around",
                ));
                app.messages
                    .push(Message::new(Role::Tool, "$ cargo build — 12 lines"));
                app.input_buffer = "typed text\nsecond line".to_string();
                app.cursor = app.input_buffer.len();

                for (w, h) in [
                    (1, 1),
                    (2, 3),
                    (4, 5),
                    (10, 6),
                    (20, 8),
                    (40, 12),
                    (80, 24),
                    (200, 60),
                ] {
                    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                    terminal.draw(|f| render(f, &mut app)).unwrap_or_else(|e| {
                        panic!("{state:?} greeted={greeted} at {w}x{h} failed: {e}")
                    });
                }
            }
        }
    }

    /// The welcome panel answers the three launch questions -- which model,
    /// where do commands run, what do I type -- in one glance.
    #[test]
    fn the_welcome_screen_shows_the_identity_the_mascot_and_the_tips() {
        let mut cfg = crate::config::Config::default();
        cfg.llm.model = "deepseek-chat".to_string();
        cfg.llm.api_key = "sk-set".to_string();
        cfg.llm.endpoint = "https://api.deepseek.com".to_string();
        let mut app = App::new(cfg);
        app.workspace_status = "/srv/project".to_string();

        let shown = welcome_text(&app, 96);

        assert!(shown.contains("Welcome back"), "{shown}");
        assert!(shown.contains(env!("CARGO_PKG_VERSION")), "{shown}");
        assert!(shown.contains("deepseek-chat"), "{shown}");
        assert!(shown.contains("/srv/project"), "{shown}");
        assert!(shown.contains("waits for your approval"), "{shown}");
        assert!(shown.contains(theme::MASCOT[2]), "{shown}");
    }


    /// It is a launch screen, not furniture: the first prompt must replace it
    /// with the transcript entirely.
    #[test]
    fn the_welcome_screen_disappears_once_the_conversation_starts() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages.push(Message::new(Role::User, "hello there"));

        let rendered = rendered_text(&mut app, 100, 22);

        assert!(!rendered.contains("Welcome back"), "{rendered}");
        assert!(!rendered.contains("a terminal coding assistant"), "{rendered}");
        assert!(rendered.contains("hello there"), "{rendered}");
    }

    /// Beside the mascot there is only room for the wordmark on a wide
    /// terminal. Narrower than that the two collide, so the text stacks below
    /// it instead of overlapping -- and nothing is lost either way.
    #[test]
    fn a_narrow_terminal_stacks_the_wordmark_below_the_mascot() {
        let mut app = App::new(crate::config::Config::default());
        app.workspace_status = "/srv".to_string();

        let row_of = |width: usize, needle: &str| -> usize {
            welcome_text(&app, width)
                .lines()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not found at width {width}"))
        };

        // Wide: the wordmark shares its row with the mascot's first line.
        assert_eq!(row_of(96, "boxcode"), row_of(96, theme::MASCOT[0]));
        // Narrow: it has moved below the mascot's last line.
        assert!(
            row_of(42, "boxcode") > row_of(42, theme::MASCOT[4]),
            "the wordmark should stack under the mascot on a narrow terminal"
        );
    }


    /// An unconfigured setup has to say so on the launch screen, not fail on
    /// the first prompt.
    #[test]
    fn a_missing_api_key_is_reported_on_the_welcome_screen() {
        let app = App::new(crate::config::Config::default());
        let shown = welcome_text(&app, 96);

        assert!(shown.contains("Before you start"), "{shown}");
        assert!(shown.contains("BOXCODE_API_KEY"), "{shown}");
    }


    /// Regression, reframed: the setup warning used to be clipped off the
    /// bottom of a short welcome panel with no way to reach it. The panel is
    /// now printed into the terminal's scrollback instead of drawn into a
    /// viewport, so nothing about it can be clipped -- but it still has to
    /// actually be in what gets printed, however much else is on it.
    #[test]
    fn the_setup_warning_is_always_part_of_the_printed_welcome() {
        let mut app = App::new(crate::config::Config::default());
        app.workspace_status = "/srv/project".to_string();

        for width in [42, 60, 96, 200] {
            let shown = welcome_text(&app, width);
            assert!(
                shown.contains("BOXCODE_API_KEY"),
                "the setup warning went missing at width {width}: {shown}"
            );
        }
    }


    /// The user's own turns sit on a raised block so scrolling back finds
    /// "where did I ask that" by shape rather than by reading. It has to be a
    /// rectangle -- every wrapped line padded to the same right edge -- or it
    /// reads as highlighting rather than as a block.
    #[test]
    fn the_users_own_turns_sit_on_a_full_width_raised_block() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages.push(Message::new(
            Role::User,
            "a prompt long enough that it has to wrap onto a second line in this terminal",
        ));
        app.messages
            .push(Message::new(Role::Assistant, "a reply from the model"));

        let (w, h) = (60u16, 24u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // `render` adapts every colour for this machine's actual terminal
        // (see `theme::adapt`), so the block's background may already be a
        // downgraded 256-colour index here rather than raw SURFACE -- compare
        // against the same adaptation, not the pre-adaptation constant.
        let surface = theme::adapt(theme::p().surface);
        let highlighted: Vec<usize> = (0..h)
            .filter(|&y| (0..w).any(|x| buffer.get(x, y).bg == surface))
            .map(|y| y as usize)
            .collect();

        assert_eq!(highlighted.len(), 2, "both wrapped lines of the prompt");
        assert_eq!(
            highlighted[1],
            highlighted[0] + 1,
            "the block must be contiguous"
        );

        // Every highlighted row ends at the same column, or the block is ragged.
        let right_edge = |y: usize| -> u16 {
            (0..w)
                .rev()
                .find(|&x| buffer.get(x, y as u16).bg == surface)
                .expect("row is highlighted")
        };
        assert_eq!(right_edge(highlighted[0]), right_edge(highlighted[1]));

        // The model's prose is not on a block -- that is the whole contrast.
        let reply_row = (0..h)
            .find(|&y| {
                (0..w)
                    .map(|x| buffer.get(x, y).symbol())
                    .collect::<String>()
                    .contains("a reply from the model")
            })
            .expect("the reply must be on screen");
        assert!(
            (0..w).all(|x| buffer.get(x, reply_row).bg != surface),
            "the assistant's prose must not be highlighted"
        );
    }

    /// Regression: the approval box prints y/n/esc directly under the command,
    /// and the footer printed the same three keys one row below it. Two
    /// identical key bars a row apart read as two separate prompts.
    #[test]
    fn the_approval_keys_appear_exactly_once() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::AwaitingApproval;
        app.workspace_root = "/tmp/project".to_string();
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Command {
                command: "ls -la".to_string(),
                purpose: None,
            },
            remaining: 0,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let rows_with_keys = (0..24)
            .filter(|&y| {
                (0..80)
                    .map(|x| buffer.get(x, y).symbol())
                    .collect::<String>()
                    .contains("y run")
            })
            .count();

        assert_eq!(rows_with_keys, 1, "the y/n/esc bar must be drawn once, not twice");
    }

    /// Regression: no live suggestions existed at all -- typing "/" showed
    /// nothing, and the exact full command had to be typed before Enter did
    /// anything. Confirms the menu actually renders (not just that the
    /// underlying `matching_commands()` logic is right, which app.rs's own
    /// tests already cover) and that the highlighted entry is visually
    /// distinguishable from the rest.
    #[test]
    fn the_slash_command_menu_renders_with_the_highlighted_entry_marked() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.input_buffer = "/".to_string();
        app.cursor = 1;

        let rows = rendered_rows(&mut app, 80, 24);
        let joined = rows.concat();

        for (name, _) in crate::app::COMMANDS {
            assert!(joined.contains(name), "{name} missing from menu: {joined}");
        }

        // Two "❯"s are expected on screen at once: the menu's cursor and the
        // input box's own separate prompt marker (visible around the typed
        // "/") -- so this looks specifically for the menu's, not just any.
        let highlighted_provider_row = rows
            .iter()
            .find(|r| r.contains('❯') && r.contains("/provider"))
            .expect("the menu's cursor should be on /provider, the first match");
        assert!(highlighted_provider_row.contains("switch provider"), "{highlighted_provider_row}");
    }

    /// Regression: Up/Down had no effect at an approval prompt, so the only
    /// way to answer it was typing y/n even though the rest of the app (the
    /// provider/model pickers, prompt history) already used arrow navigation.
    /// The cursor ("❯") must actually move between "y" and "n" as the
    /// highlight changes, not just the underlying state.
    #[test]
    fn the_cursor_moves_between_yes_and_no_as_the_highlight_changes() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::AwaitingApproval;
        app.workspace_root = "/tmp/project".to_string();
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Command { command: "ls -la".to_string(), purpose: None },
            remaining: 0,
        });

        app.approval_selected = true;
        let on_yes = rendered_rows(&mut app, 80, 24);
        let yes_row = row_containing(&on_yes, "y run");
        let no_row = row_containing(&on_yes, "n skip");
        assert!(yes_row.contains('❯'), "cursor should be on \"yes\": {yes_row}");
        assert!(!no_row.contains('❯'), "cursor should not be on \"no\": {no_row}");

        app.approval_selected = false;
        let on_no = rendered_rows(&mut app, 80, 24);
        let yes_row = row_containing(&on_no, "y run");
        let no_row = row_containing(&on_no, "n skip");
        assert!(!yes_row.contains('❯'), "cursor should have moved off \"yes\": {yes_row}");
        assert!(no_row.contains('❯'), "cursor should be on \"no\": {no_row}");

        let joined = on_no.concat();
        assert!(joined.contains("↑↓ choose"), "{joined}");
        assert!(joined.contains("enter confirm"), "{joined}");
    }

    /// The spinner is the only thing that says a turn is still alive, and it
    /// has to say what stops it.
    #[test]
    fn a_running_turn_shows_a_spinner_and_how_to_interrupt_it() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::Sending;
        app.busy_started = Some(std::time::Instant::now());

        let rendered = rendered_text(&mut app, 80, 24);
        assert!(rendered.contains("Thinking…"), "{rendered}");
        assert!(rendered.contains("esc to interrupt"), "{rendered}");

        // Idle shows none of it.
        app.state = AppState::AwaitingInput;
        app.busy_started = None;
        let idle = rendered_text(&mut app, 80, 24);
        assert!(!idle.contains("esc to interrupt"), "{idle}");
    }

    /// The end of the same bug: rendering an overlay into a zero-cell frame.
    #[test]
    fn rendering_an_overlay_into_a_zero_size_frame_does_not_panic() {
        let mut app = App::new(crate::config::Config::default());
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Command {
                command: "rm -rf /".to_string(),
                purpose: Some("something alarming".to_string()),
            },
            remaining: 2,
        });

        for (w, h) in [(1, 1), (2, 2), (10, 4), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| render(f, &mut app))
                .unwrap_or_else(|e| panic!("{w}x{h} failed to render: {e}"));
        }
    }

    /// Regression: a tool approval used to float as a centered popup that
    /// `Clear`ed and covered whatever transcript was underneath it -- the
    /// "separate popup" a user compared unfavourably to Claude Code's inline
    /// confirmation. It must now sit in its own reserved region at the bottom,
    /// leaving the transcript above it fully intact and visible.
    #[test]
    fn a_tool_approval_leaves_the_transcript_visible_above_it() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages
            .push(Message::new(Role::User, "delete the build directory"));
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Command {
                command: "rm -rf build".to_string(),
                purpose: Some("clear stale output".to_string()),
            },
            remaining: 0,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rendered: String = buffer.content().iter().map(|c| c.symbol()).collect();

        assert!(
            rendered.contains("delete the build directory"),
            "the transcript must still be visible: {rendered}"
        );
        assert!(rendered.contains("rm -rf build"), "{rendered}");
        assert!(rendered.contains("Run this command?"), "{rendered}");

        // The prompt must sit flush against the footer -- no gap below it the
        // way a centered floating popup would leave -- and the transcript line
        // must be above the prompt, not swallowed by it.
        let area = buffer.area();
        let row_text =
            |y: u16| -> String { (0..area.width).map(|x| buffer.get(x, y).symbol()).collect() };
        let transcript_row = (0..area.height)
            .find(|&y| row_text(y).contains("delete the build directory"))
            .expect("the earlier message must still be on screen");
        let prompt_bottom_row = (0..area.height)
            .rev()
            .find(|&y| row_text(y).contains("skip"))
            .expect("the y/n key hint must be on screen");

        assert!(
            transcript_row < prompt_bottom_row,
            "transcript (row {transcript_row}) must be above the prompt (row {prompt_bottom_row})"
        );
        // Row height-1 is the footer, height-2 is the prompt box's bottom
        // border: it must be non-blank, i.e. the box sits flush against the
        // footer with no gap -- a floating centered popup would leave one.
        assert!(
            !row_text(area.height - 2).trim().is_empty(),
            "the prompt's border should be flush against the footer, not floating mid-screen with a gap"
        );
    }

    /// Regression: `ExecutingTools` used to draw straight from
    /// `app.approved_tools`, which `main.rs` empties the instant it spawns the
    /// runner -- so the on-screen "N commands" count went stale after one
    /// frame even though the run was still going. It must read `running_tools`
    /// (the snapshot) instead, and the footer must show a live command count
    /// and elapsed time the way "Running 5 shell commands · 42s…" does.
    #[test]
    fn the_footer_shows_a_live_running_command_count_while_tools_execute() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::ExecutingTools;
        app.busy_started = Some(std::time::Instant::now());
        app.running_tools = vec![
            command_call("call_1", "ls"),
            command_call("call_2", "cat Cargo.toml"),
        ];
        // The queue `main.rs` would already have taken by this point -- the
        // footer and transcript must not depend on it still being populated.
        app.approved_tools.clear();

        let rendered = rendered_text(&mut app, 80, 24);

        assert!(rendered.contains("Running 2 commands"), "{rendered}");
        assert!(
            rendered.contains("(0s") || rendered.contains("(1s"),
            "{rendered}"
        );
        // …and it says how to stop, since that is the other thing you want to
        // know while watching something run.
        assert!(rendered.contains("esc to interrupt"), "{rendered}");
    }

    /// The token count is a live estimate, so it must read as one ("~N
    /// tokens") rather than a bare number that looks authoritative.
    #[test]
    fn the_footer_shows_an_approximate_token_count_while_streaming() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::Streaming;
        app.busy_started = Some(std::time::Instant::now());
        app.streamed_chars = 400; // -> ~100 tokens at the chars/4 estimate

        let rendered = rendered_text(&mut app, 80, 24);

        assert!(rendered.contains("~100 tokens"), "{rendered}");
    }

    /// Regression: labelling every line "You: " / "Assistant: " was what made
    /// this read as a Q&A chat log instead of one continuous stream, the thing
    /// a user compared unfavourably to Claude Code's transcript. The user's own
    /// words still get a "> " quote marker; the assistant's prose gets nothing.
    #[test]
    fn the_transcript_reads_as_a_continuous_stream_not_a_labelled_chat_log() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages
            .push(Message::new(Role::User, "write a hello world function"));
        app.messages
            .push(Message::new(Role::Assistant, "Here's the function..."));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(
            rendered.contains(&format!(
                "{} write a hello world function",
                theme::USER_MARK
            )),
            "{rendered}"
        );
        assert!(rendered.contains("Here's the function"), "{rendered}");
        assert!(!rendered.contains("You:"), "{rendered}");
        assert!(!rendered.contains("Assistant:"), "{rendered}");
    }

    /// Regression: a long command or file preview filled the box and pushed the
    /// y/n choice clean off the bottom. The keys still worked, which made it
    /// worse -- nothing on screen said what to press.
    #[test]
    fn the_answer_keys_stay_visible_however_long_the_content_is() {
        for (label, action) in [
            (
                "long command",
                Action::Command {
                    command: (1..=60)
                        .map(|i| format!("--flag-number-{i}=some-fairly-long-value"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    purpose: Some("a purpose line as well".into()),
                },
            ),
            (
                "long file",
                Action::Write {
                    path: "src/generated.rs".into(),
                    content: (1..=200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
                },
            ),
        ] {
            let mut app = App::new(crate::config::Config::default());
            app.greeted = true;
            app.workspace_root = "/tmp/project".into();
            app.overlay = Some(Overlay::ToolApproval { action, remaining: 0 });

            for (w, h) in [(80, 24), (120, 40), (60, 12)] {
                let rendered = rendered_text(&mut app, w, h);
                assert!(
                    rendered.contains("y run") || rendered.contains("y write"),
                    "{label} at {w}x{h}: the answer keys were pushed off screen"
                );
                assert!(
                    rendered.contains("enter confirm"),
                    "{label} at {w}x{h}: the key hint was pushed off screen"
                );
            }
        }
    }

    /// Content that does not fit has to say so, or someone approves the half
    /// they happened to be shown.
    #[test]
    fn overflowing_content_advertises_that_there_is_more() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.workspace_root = "/tmp/project".into();
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Write {
                path: "big.txt".into(),
                content: (1..=200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
            },
            remaining: 0,
        });

        let rendered = rendered_text(&mut app, 80, 24);
        assert!(rendered.contains("more"), "expected a 'N more' marker: {rendered}");
    }

    /// Scrolling has to actually move the content, and stop at the end rather
    /// than running off into blank space.
    #[test]
    fn scrolling_reveals_later_content_and_clamps_at_the_bottom() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.workspace_root = "/tmp/project".into();
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Write {
                path: "big.txt".into(),
                content: (1..=40).map(|i| format!("marker{i}")).collect::<Vec<_>>().join("\n"),
            },
            remaining: 0,
        });

        let top = rendered_text(&mut app, 80, 24);
        assert!(top.contains("marker1"), "{top}");

        app.approval_scroll = 6;
        let scrolled = rendered_text(&mut app, 80, 24);
        assert_ne!(top, scrolled, "PageDown must move the content");
        // The keys survive scrolling too.
        assert!(scrolled.contains("y write"), "{scrolled}");

        // Far past the end clamps rather than scrolling into emptiness.
        app.approval_scroll = 9_999;
        let bottom = rendered_text(&mut app, 80, 24);
        assert!(bottom.contains("y write"), "{bottom}");
        assert!(
            bottom.trim().len() > 40,
            "clamping failed; the box scrolled past its content: {bottom}"
        );
    }

    /// The command must appear verbatim: approving something you cannot read is
    /// not approval.
    #[test]
    fn the_approval_prompt_shows_the_command_and_the_keys() {
        let mut app = App::new(crate::config::Config::default());
        app.workspace_root = "/tmp/project".to_string();
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Command {
                command: "rm -rf build".to_string(),
                purpose: None,
            },
            remaining: 0,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(
            rendered.contains("rm -rf build"),
            "the command must be shown"
        );
        assert!(rendered.contains("Run this command?"), "{rendered}");
        assert!(
            rendered.contains("/tmp/project"),
            "where it runs must be shown"
        );
        assert!(rendered.contains("y run"), "the keys must be shown");
    }

    /// A write shows its content, not a shell command -- and the verb in the
    /// key hints matches the action ("write", not "run").
    #[test]
    fn the_write_approval_prompt_shows_the_path_and_content() {
        let mut app = App::new(crate::config::Config::default());
        app.workspace_root = "/tmp/project".to_string();
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Write {
                path: "hello.py".to_string(),
                content: "print('hi')\n".to_string(),
            },
            remaining: 0,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains("hello.py"), "the path must be shown");
        assert!(rendered.contains("print"), "the content must be shown");
        assert!(rendered.contains("Write this file?"), "{rendered}");
        assert!(
            rendered.contains("y write"),
            "the keys must say write, not run"
        );
    }

    /// A search shows the query and that it leaves the machine, and -- unlike
    /// every other action -- does not claim to be scoped to the workspace
    /// directory, since it isn't.
    #[test]
    fn the_search_approval_prompt_shows_the_query_and_the_keys() {
        let mut app = App::new(crate::config::Config::default());
        app.workspace_root = "/tmp/project".to_string();
        app.overlay = Some(Overlay::ToolApproval {
            action: Action::Search {
                query: "rust async runtime comparison".to_string(),
                max_results: 5,
            },
            remaining: 0,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(
            rendered.contains("rust async runtime comparison"),
            "the query must be shown"
        );
        assert!(rendered.contains("Search the web?"), "{rendered}");
        assert!(
            rendered.contains("web search service"),
            "the prompt must disclose this leaves the machine"
        );
        assert!(
            !rendered.contains("/tmp/project"),
            "a search is not scoped to the workspace, so it must not claim to be"
        );
        assert!(rendered.contains("y search"), "the keys must say search");
    }

    // ---- /deploy ---------------------------------------------------------

    /// An app sitting in the deployment overlay, at whichever stage is wanted.
    fn deploying(stage: Stage) -> App {
        use crate::deploy::service::tests_support;
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.workspace_root = "/Users/dev/my-app".to_string();
        let mut session = tests_support::session("vercel");
        session.stage = stage;
        app.deploy = Some(session);
        app.overlay = Some(Overlay::Deploy);
        app
    }

    #[test]
    fn the_provider_picker_lists_both_providers_with_the_first_highlighted() {
        let mut app = deploying(Stage::Menu(Menu::Provider));
        let rows = rendered_rows(&mut app, 80, 24);
        let joined = rows.concat();

        assert!(joined.contains("Select deployment provider"), "{joined}");
        assert!(joined.contains("Vercel"), "{joined}");
        assert!(joined.contains("Netlify"), "{joined}");

        let highlighted = rows
            .iter()
            .find(|r| r.contains('❯') && r.contains("Vercel"))
            .expect("the cursor should start on the first provider");
        assert!(!highlighted.is_empty());
    }

    /// The detected project and framework are the whole point of the confirm
    /// screen -- without them it is a yes/no about nothing.
    #[test]
    fn the_confirmation_screen_names_what_was_detected() {
        let mut app = deploying(Stage::Menu(Menu::Confirm));
        let text = rendered_text(&mut app, 80, 24);
        assert!(text.contains("my-app"), "{text}");
        assert!(text.contains("Vite"), "{text}");
        assert!(text.contains("Yes"), "{text}");
        assert!(text.contains("No"), "{text}");
    }

    #[test]
    fn a_running_step_shows_a_spinner_its_elapsed_time_and_how_to_stop_it() {
        let mut app = deploying(Stage::Working(crate::deploy::service::Step::Deploying));
        if let Some(session) = app.deploy.as_mut() {
            session.started = Some(std::time::Instant::now());
            session.log.push_back("Building...".to_string());
        }
        let text = rendered_text(&mut app, 80, 24);
        assert!(text.contains("Building and uploading"), "{text}");
        assert!(text.contains("esc to stop"), "{text}");
        assert!(text.contains("Building..."), "the streamed log must show: {text}");
    }

    /// The URL is the one thing anyone wants out of this screen, and a URL
    /// broken across two rows cannot be copied out of a terminal in one go.
    #[test]
    fn the_success_screen_shows_the_url_unbroken() {
        let mut app = deploying(Stage::Finished);
        if let Some(session) = app.deploy.as_mut() {
            session.url = Some("https://my-app.vercel.app".to_string());
            session.finished_status = Some(DeployStatus::Success);
        }
        let rows = rendered_rows(&mut app, 80, 24);
        assert!(
            rows.iter().any(|r| r.contains("https://my-app.vercel.app")),
            "the URL must sit on one row: {rows:?}"
        );
        assert!(rows.concat().contains("Deployment successful"), "{rows:?}");
    }

    /// The failure screen has to carry the reason *and* the way out, or it is
    /// a dead end.
    #[test]
    fn the_failure_screen_shows_the_reason_and_the_recovery_options() {
        let mut app = deploying(Stage::Menu(Menu::Failure));
        if let Some(session) = app.deploy.as_mut() {
            session.failure = Some("The build command failed on Vercel.".to_string());
        }
        let text = rendered_text(&mut app, 80, 30);
        assert!(text.contains("FAILED"), "{text}");
        assert!(text.contains("build command failed"), "{text}");
        assert!(text.contains("View detailed logs"), "{text}");
        assert!(text.contains("Retry deployment"), "{text}");
        assert!(text.contains("Cancel"), "{text}");
    }

    /// The install prompt must show the exact command and the guardrails'
    /// verdict on it -- approving something you cannot see is not approval.
    #[test]
    fn the_install_prompt_shows_the_command_and_why_it_is_flagged() {
        let mut app = deploying(Stage::Menu(Menu::InstallCli));
        if let Some(session) = app.deploy.as_mut() {
            session.install_reason = Some("installs globally, outside the project".to_string());
        }
        let text = rendered_text(&mut app, 80, 24);
        assert!(text.contains("npm install -g vercel"), "{text}");
        assert!(text.contains("DESTRUCTIVE"), "{text}");
        assert!(text.contains("installs globally"), "{text}");
    }

    /// The property the whole secret story rests on, asserted against real
    /// rendered cells rather than against the data model.
    #[test]
    fn a_secret_value_never_reaches_the_screen() {
        use crate::deploy::{EnvVar, Secret};

        // While it is being typed...
        let mut app = deploying(Stage::Prompt(crate::deploy::service::Prompt::EnvValue));
        if let Some(session) = app.deploy.as_mut() {
            session.input = "hunter2-super-secret".to_string();
            session.input_cursor = session.input.len();
        }
        let typing = rendered_text(&mut app, 80, 24);
        assert!(!typing.contains("hunter2"), "a value echoed while typing: {typing}");
        assert!(typing.contains('•'), "it should render as dots: {typing}");

        // ...and once it is set.
        let mut app = deploying(Stage::Menu(Menu::Env));
        if let Some(session) = app.deploy.as_mut() {
            session.env.push(EnvVar {
                key: "API_KEY".to_string(),
                value: Secret::new("hunter2-super-secret"),
            });
        }
        let listed = rendered_text(&mut app, 80, 24);
        assert!(listed.contains("API_KEY"), "the name should show: {listed}");
        assert!(!listed.contains("hunter2"), "a value leaked into the list: {listed}");
    }

    /// The same sweep the rest of the app gets: no deployment screen may panic
    /// on a terminal too small to hold it.
    #[test]
    fn every_deployment_screen_renders_at_any_terminal_size() {
        use crate::deploy::service::{Prompt, Step};

        let stages = [
            Stage::Menu(Menu::Provider),
            Stage::Menu(Menu::Confirm),
            Stage::Menu(Menu::Settings),
            Stage::Menu(Menu::EditField),
            Stage::Menu(Menu::Env),
            Stage::Menu(Menu::Target),
            Stage::Menu(Menu::Link),
            Stage::Menu(Menu::InstallCli),
            Stage::Menu(Menu::Login),
            Stage::Menu(Menu::Failure),
            Stage::Prompt(Prompt::Name),
            Stage::Prompt(Prompt::Token),
            Stage::Working(Step::Deploying),
            Stage::Finished,
        ];

        for stage in stages {
            let mut app = deploying(stage.clone());
            if let Some(session) = app.deploy.as_mut() {
                session.started = Some(std::time::Instant::now());
                session.failure = Some("a failure long enough to need wrapping somewhere".into());
                session.url = Some("https://a-rather-long-project-name.vercel.app".into());
                session.show_full_log = true;
                for i in 0..30 {
                    session.log.push_back(format!("log line number {i} with some text"));
                }
            }
            for (w, h) in [(1, 1), (2, 3), (4, 5), (10, 6), (20, 8), (40, 12), (80, 24), (200, 60)] {
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal
                    .draw(|f| render(f, &mut app))
                    .unwrap_or_else(|e| panic!("{stage:?} at {w}x{h} failed: {e}"));
            }
        }
    }

    /// The transcript is the context for the question being asked, exactly as
    /// it is for a tool approval.
    #[test]
    fn the_deployment_panel_leaves_the_transcript_visible_above_it() {
        let mut app = deploying(Stage::Menu(Menu::Confirm));
        app.messages.push(Message::new(
            Role::User,
            "deploy this thing for me please",
        ));
        let text = rendered_text(&mut app, 80, 30);
        assert!(text.contains("deploy this thing"), "{text}");
        assert!(text.contains("Continue with deployment"), "{text}");
    }

    /// The model can deploy too, and that prompt has to say what is about to
    /// happen: which host, which kind of deployment, and what will be built.
    #[test]
    fn the_deploy_approval_prompt_distinguishes_a_preview_from_production() {
        use crate::llm::FunctionCall;

        for (production, expected) in [(false, "Preview"), (true, "Production")] {
            let mut app = App::new(crate::config::Config::default());
            app.greeted = true;
            app.workspace_root = "/tmp".to_string();
            app.state = AppState::Streaming;
            app.request_tools(vec![ToolCall {
                id: "c1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: crate::tools::DEPLOY_PROJECT.to_string(),
                    arguments: format!("{{\"provider\":\"vercel\",\"production\":{production}}}"),
                },
            }]);

            let text = rendered_text(&mut app, 80, 26);
            assert!(text.contains("Deploy this project?"), "{text}");
            assert!(text.contains(expected), "{text}");
            // A deployment is not destroying anything locally, so calling it
            // "destructive" would be dishonest in the other direction.
            assert!(text.contains("PUBLISHES"), "{text}");
            assert!(!text.contains("DESTRUCTIVE"), "{text}");
            // Asserted on a phrase short enough to survive wrapping: rows are
            // padded to the full width, so a phrase split across two of them
            // is not `contains`-able even though it renders correctly.
            if production {
                assert!(text.contains("live production URL"), "{text}");
            } else {
                assert!(text.contains("third-party host"), "{text}");
            }
        }
    }

    // ---- markdown ----------------------------------------------------------

    /// Render one assistant message and return the visible rows.
    fn assistant_rows(body: &str) -> Vec<String> {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages.push(Message::new(Role::Assistant, body));
        rendered_rows(&mut app, 80, 24)
    }

    /// Regression: the model writes markdown whether or not it is asked to,
    /// and the transcript showed the punctuation instead of the emphasis --
    /// `**https://…**` with the asterisks visible.
    #[test]
    fn markdown_punctuation_does_not_reach_the_screen() {
        let rows = assistant_rows(
            "Deployed. The real URL is:\n\n\
             **https://deploy-demo.vercel.app**\n\n\
             Note: Vercel lists it under the **Production** environment.",
        );
        let joined = rows.concat();

        assert!(joined.contains("https://deploy-demo.vercel.app"), "{joined}");
        assert!(joined.contains("Production"), "{joined}");
        assert!(!joined.contains("**"), "asterisks reached the screen: {joined}");
    }

    #[test]
    fn inline_code_loses_its_backticks_but_keeps_its_text() {
        let joined = assistant_rows("Check `index.html` and `vite.config.js` first.").concat();
        assert!(joined.contains("index.html"), "{joined}");
        assert!(joined.contains("vite.config.js"), "{joined}");
        assert!(!joined.contains('`'), "backticks reached the screen: {joined}");
    }

    #[test]
    fn bullets_and_headings_render_as_themselves() {
        let joined = assistant_rows("## What I found\n\n- public/ and src/\n- index.html").concat();
        assert!(joined.contains("What I found"), "{joined}");
        assert!(!joined.contains('#'), "hashes reached the screen: {joined}");
        assert!(joined.contains('•'), "a bullet should render as one: {joined}");
        assert!(joined.contains("index.html"), "{joined}");
    }

    #[test]
    fn a_fenced_code_block_is_shown_without_its_fences() {
        let joined = assistant_rows("Run this:\n\n```bash\nnpm run build\n```\n\nThen deploy.").concat();
        assert!(joined.contains("npm run build"), "{joined}");
        assert!(!joined.contains("```"), "fences reached the screen: {joined}");
        assert!(joined.contains("Then deploy."), "{joined}");
    }

    /// An unmatched marker must stay as text. Swallowing the rest of the line
    /// looking for a closer that never comes would lose real content.
    #[test]
    fn an_unmatched_marker_is_left_alone_rather_than_eating_the_line() {
        let joined = assistant_rows("2 * 3 is 6, and `unclosed stays put").concat();
        assert!(joined.contains("2 * 3 is 6"), "{joined}");
        assert!(joined.contains("unclosed stays put"), "{joined}");
    }

    /// `snake_case`, globs and `*args` are ordinary in prose about code, and a
    /// false italic there silently deletes characters.
    #[test]
    fn underscores_and_lone_asterisks_in_prose_are_untouched() {
        for body in ["use snake_case here", "pass *args through", "match **/*.rs"] {
            let joined = assistant_rows(body).concat();
            let stripped: String = body.chars().filter(|c| !c.is_whitespace()).collect();
            let seen: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(seen.contains(&stripped), "{body:?} was mangled into {joined:?}");
        }
    }

    /// The cap on the menu's height has to stay above the number of commands.
    /// Adding the two deployment commands pushed the list past the old cap of
    /// 8, and the last one was silently clipped out of its own menu -- which
    /// is how a command comes to look like it does not exist.
    #[test]
    fn the_command_menu_is_tall_enough_for_every_command() {
        assert!(
            MAX_COMMAND_MENU_HEIGHT as usize >= crate::app::COMMANDS.len() + 2,
            "{} commands do not fit in {MAX_COMMAND_MENU_HEIGHT} rows",
            crate::app::COMMANDS.len()
        );
    }


}




