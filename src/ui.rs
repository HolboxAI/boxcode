use crate::app::{App, AppState, CustomStep, Overlay, Role};
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

    let bottom_height = match &approval {
        Some((action, remaining)) => {
            let inner_width = size.width.saturating_sub(4).max(1) as usize;
            let (_, lines) = tool_approval_lines(app, action, *remaining, inner_width);
            (lines.len() as u16 + 2).clamp(MIN_INPUT_HEIGHT, MAX_APPROVAL_HEIGHT)
        }
        None => input_height(app, size.width),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(bottom_height),
            Constraint::Length(1),
        ])
        .split(size);

    render_header(f, chunks[0], app);
    render_messages(f, chunks[1], app);
    match &approval {
        Some((action, remaining)) => {
            render_tool_approval_inline(f, chunks[2], app, action, *remaining)
        }
        None => render_input(f, chunks[2], app),
    }
    render_footer(f, chunks[3], app);

    // Everything else here (pickers, text prompts) is a one-shot choice made
    // before a turn even starts, with no transcript underneath it yet to stay
    // faithful to -- floating and centered is fine for those.
    render_overlay(f, size, app);
}

/// A single quiet line: the mark on the left, the model on the right.
///
/// Deliberately understated. The endpoint and the working directory are on the
/// welcome panel where they are read once; repeating them across the top of
/// every frame competes with the transcript for attention and wins, which is
/// exactly backwards.
fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let left = Line::from(Span::styled(
        format!(" {}", theme::LOGO),
        theme::accent_bold(),
    ));
    f.render_widget(Paragraph::new(left), area);

    // The welcome panel names the model in its own column; repeating it in the
    // header while that panel is up says the same thing twice on one screen.
    let on_welcome = !app.greeted && app.messages.is_empty();
    let model = format!("{} ", app.config.llm.model);
    if !on_welcome && (model.chars().count() as u16) < area.width.saturating_sub(20) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(model, theme::faint())))
                .alignment(Alignment::Right),
            area,
        );
    }
}

/// The transcript, drawn without a box around it.
///
/// A border here would frame the conversation as a widget in an application;
/// without one the text simply occupies the terminal, which is what a terminal
/// session should feel like. The two-column indent does the job the border was
/// doing -- separating the stream from the edge of the screen -- at a quarter
/// of the visual weight.
fn render_messages(f: &mut Frame, area: Rect, app: &mut App) {
    const GUTTER: usize = 2;
    let width = area.width.saturating_sub(GUTTER as u16 + 1).max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();

    if !app.greeted && app.messages.is_empty() {
        lines.extend(welcome_lines(app, width));
        f.render_widget(
            Paragraph::new(lines).block(Block::default().padding(Padding::new(GUTTER as u16, 1, 0, 0))),
            area,
        );
        return;
    }

    lines.push(Line::from(""));
    for msg in &app.messages {
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
                    Span::styled(format!("{marker} "), Style::default().fg(theme::FAINT)),
                    Span::styled(wrapped, role_style(Role::Tool)),
                ]));
            }
            continue;
        }
        // An assistant turn that was nothing but tool calls has no prose to
        // show; the calls speak for themselves on the lines that follow.
        if msg.role == Role::Assistant
            && !msg.tool_calls.is_empty()
            && msg.content.trim().is_empty()
        {
            continue;
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
            Role::Assistant => {
                for wrapped in wrap(msg.body(), width) {
                    lines.push(Line::from(Span::styled(wrapped, theme::text())));
                }
            }
            Role::Error | Role::System => {
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
    }

    if app.state == AppState::ExecutingTools {
        for call in &app.running_tools {
            let label = crate::tools::describe_action(call)
                .map(|a| a.label())
                .unwrap_or_else(|| call.function.name.clone());
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", theme::TOOL_MARK),
                    Style::default().fg(theme::FAINT),
                ),
                Span::styled(label, role_style(Role::Tool)),
            ]));
        }
    }

    if app.state == AppState::Streaming && !app.streaming_response.is_empty() {
        for wrapped in wrap(&app.streaming_response, width) {
            lines.push(Line::from(Span::styled(wrapped, theme::text())));
        }
    }

    // The live status sits at the end of the transcript rather than in the
    // footer, so the thing you are waiting on appears where you are already
    // looking -- directly above the prompt, in the flow of the turn.
    if let Some(status) = activity_line(app) {
        if app.state == AppState::Streaming && !app.streaming_response.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(status);
    }
    lines.push(Line::from(""));

    // Clamp the scroll offset to the content, and stick to the bottom while the
    // user has not scrolled away. No border any more, so the whole area is
    // viewport -- an off-by-two here silently hides the newest two lines.
    let viewport = area.height as usize;
    let max_scroll = lines.len().saturating_sub(viewport) as u16;
    if app.follow_tail {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.follow_tail = true;
        }
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().padding(Padding::new(GUTTER as u16, 1, 0, 0)))
        .scroll((app.scroll, 0));

    f.render_widget(paragraph, area);

    // A quiet marker that there is more above, since without a border there is
    // no title bar left to say so.
    if app.scroll < max_scroll && area.height > 0 {
        let more = Line::from(Span::styled(" ↓ more ", theme::faint()));
        let hint_area = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(1),
            width: area.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(more).alignment(Alignment::Right), hint_area);
    }
}

/// The spinner line: what the app is doing, how long it has been doing it, and
/// how to stop it. `None` when nothing is running.
fn activity_line(app: &App) -> Option<Line<'static>> {
    let elapsed = app.busy_started.map(|t| t.elapsed());
    let secs = elapsed.map(|e| e.as_secs()).unwrap_or(0);
    let frame = theme::spinner(elapsed.unwrap_or_default());

    let (verb, detail) = match app.state {
        AppState::AwaitingInput => return None,
        AppState::AwaitingApproval => return None,
        AppState::Sending => ("Thinking".to_string(), String::new()),
        AppState::Streaming => {
            // No endpoint used here sends a token count mid-stream -- that only
            // ever arrives, if at all, on the final chunk. This is the same
            // rough characters-per-token estimate a live counter has to use
            // before that arrives, so it is always labelled "~".
            let approx_tokens = app.streamed_chars / 4;
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
        Span::styled(format!("{verb}… "), Style::default().fg(theme::ACCENT_SOFT)),
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
fn welcome_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];

    // Beside the mascot: the wordmark, what this is, and who is using it.
    // Blank entries keep the two columns the same height so the rule below
    // lands flush regardless of which side is taller.
    let beside: [Vec<Span>; 5] = [
        vec![
            Span::styled("tuisample-code", theme::accent_bold()),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), theme::faint()),
        ],
        vec![Span::styled("a terminal coding assistant", theme::faint())],
        vec![],
        vec![
            Span::styled("Welcome back", theme::text()),
            Span::styled(
                greeting_name().map(|n| format!(", {n}")).unwrap_or_default(),
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
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
        Style::default().fg(theme::BORDER),
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
        Style::default().fg(theme::ACCENT_SOFT),
    ));
    lines.push(field(
        "endpoint",
        app.config.llm.endpoint.clone(),
        theme::muted(),
    ));
    if !app.workspace_status.is_empty() {
        let alarming = app.workspace_status.contains("UNATTENDED");
        let colour = if alarming {
            theme::DANGER
        } else if app.workspace_status.starts_with("off") || app.workspace_status.contains("broad") {
            theme::WARNING
        } else {
            theme::MUTED
        };
        let mut style = Style::default().fg(colour);
        if alarming {
            style = style.add_modifier(Modifier::BOLD);
        }
        lines.push(field("cwd", shorten_home(&app.workspace_status), style));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<10}", "/provider"), theme::key()),
        Span::styled("switch provider or endpoint", theme::muted()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<10}", "/model"), theme::key()),
        Span::styled("switch model", theme::muted()),
    ]));

    let warnings = app.config.warnings();
    if !warnings.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Before you start",
            Style::default().fg(theme::WARNING).add_modifier(Modifier::BOLD),
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
            .fg(theme::USER)
            .add_modifier(Modifier::BOLD),
        Role::Assistant => Style::default().fg(theme::TEXT),
        Role::Error => Style::default()
            .fg(theme::DANGER)
            .add_modifier(Modifier::BOLD),
        Role::System => Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
        Role::Tool => Style::default().fg(theme::TOOL),
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
            Style::default().fg(if busy { theme::MUTED } else { theme::TEXT }),
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
            Style::default().fg(theme::BORDER)
        } else {
            Style::default().fg(theme::ACCENT)
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
            let mut items: Vec<String> = providers::PROVIDERS
                .iter()
                .map(|p| p.label.to_string())
                .collect();
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
        Some(Overlay::ToolApproval { .. }) => {}
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
    let mut lines: Vec<Line> = Vec::new();

    // A destructive command gets a banner before anything else. The prompt for
    // `rm -rf build` must not look identical to the one for `cargo build` --
    // that sameness is what trains people to press `y` without reading.
    if let Action::Command { command, .. } = action {
        let verdict = crate::danger::classify(command, Path::new(&app.workspace_root));
        if let Some(reason) = verdict.reason() {
            lines.push(Line::from(Span::styled(
                "⚠  DESTRUCTIVE",
                theme::danger_bold(),
            )));
            for wrapped in wrap(reason, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(theme::DANGER),
                )));
            }
            lines.push(Line::from(""));
        }
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
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            (" Run this command? ", "run")
        }
        Action::Read { path } => {
            lines.push(Line::from(Span::styled(
                format!("📄 {path}"),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            )));
            (" Read this file? ", "read")
        }
        Action::Write { path, content } => {
            lines.push(Line::from(Span::styled(
                format!("📝 {path}"),
                Style::default()
                    .fg(theme::TEXT)
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
    };

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("in {}", app.workspace_root),
        theme::faint(),
    )));
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
                    .fg(theme::SUCCESS)
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
                    .fg(theme::DANGER)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        Span::styled(" skip", theme::faint()),
    ]));
    lines.push(Line::from(Span::styled(
        "  ↑↓ choose · enter confirm · esc skip",
        theme::faint(),
    )));

    (title, lines)
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
    let (title, lines) = tool_approval_lines(app, action, remaining, inner);

    let destructive = matches!(action, Action::Command { command, .. }
        if crate::danger::classify(command, Path::new(&app.workspace_root)).is_dangerous());
    let accent = if destructive {
        theme::DANGER
    } else {
        theme::ACCENT
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
    f.render_widget(Paragraph::new(lines).block(block), area);
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
        .border_style(Style::default().fg(theme::ACCENT))
        .title(Span::styled(title.to_string(), theme::accent_bold()));
    let list = List::new(list_items)
        .block(block)
        .style(theme::text())
        .highlight_style(
            Style::default()
                .fg(theme::ACCENT)
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
        .border_style(Style::default().fg(theme::ACCENT))
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

    /// The welcome screen answers the three launch questions -- which model,
    /// where do commands run, what do I type -- in one glance.
    #[test]
    fn the_welcome_screen_shows_the_identity_the_mascot_and_the_tips() {
        let mut cfg = crate::config::Config::default();
        cfg.llm.model = "deepseek-chat".to_string();
        cfg.llm.api_key = "sk-set".to_string();
        cfg.llm.endpoint = "https://api.deepseek.com".to_string();
        let mut app = App::new(cfg);
        app.workspace_status = "/srv/project".to_string();

        // Tall enough for the whole screen: on a short terminal the tips fall
        // below the fold, which is deliberate -- they are the least important
        // thing on it -- but it is not what this test is checking.
        let rendered = rendered_text(&mut app, 100, 30);

        assert!(rendered.contains("Welcome back"), "{rendered}");
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "the version banner: {rendered}"
        );
        assert!(rendered.contains("deepseek-chat"), "{rendered}");
        assert!(
            rendered.contains("/srv/project"),
            "where commands run: {rendered}"
        );
        assert!(rendered.contains("waits for your approval"), "{rendered}");
        assert!(
            rendered.contains(theme::MASCOT[2]),
            "the mascot: {rendered}"
        );
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
    /// it instead of overlapping it -- and nothing is lost either way.
    #[test]
    fn a_narrow_terminal_stacks_the_wordmark_below_the_mascot() {
        let mut app = App::new(crate::config::Config::default());
        app.workspace_status = "/srv".to_string();

        let row_of = |app: &mut App, w: u16, needle: &str| -> usize {
            let mut terminal = Terminal::new(TestBackend::new(w, 30)).unwrap();
            terminal.draw(|f| render(f, app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..30)
                .find(|&y| {
                    (0..w)
                        .map(|x| buffer.get(x, y).symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("{needle:?} not found at width {w}")) as usize
        };

        // Wide: the wordmark shares its row with the mascot's first line.
        assert_eq!(
            row_of(&mut app, 100, "tuisample-code"),
            row_of(&mut app, 100, theme::MASCOT[0])
        );
        // Narrow: it has moved below the mascot's last line.
        assert!(
            row_of(&mut app, 46, "tuisample-code") > row_of(&mut app, 46, theme::MASCOT[4]),
            "the wordmark should stack under the mascot on a narrow terminal"
        );
    }

    /// An unconfigured setup has to say so on the launch screen, not fail on
    /// the first prompt.
    #[test]
    fn a_missing_api_key_is_reported_on_the_welcome_screen() {
        let mut app = App::new(crate::config::Config::default());
        let rendered = rendered_text(&mut app, 100, 22);

        assert!(rendered.contains("Before you start"), "{rendered}");
        assert!(rendered.contains("TUISAMPLE_API_KEY"), "{rendered}");
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

        let highlighted: Vec<usize> = (0..h)
            .filter(|&y| (0..w).any(|x| buffer.get(x, y).bg == theme::SURFACE))
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
                .find(|&x| buffer.get(x, y as u16).bg == theme::SURFACE)
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
            (0..w).all(|x| buffer.get(x, reply_row).bg != theme::SURFACE),
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
}


