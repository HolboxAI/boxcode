use crate::app::{App, AppState, CustomStep, Overlay, Role};
use crate::providers;
use crate::tools::Action;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
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
        Some((action, remaining)) => render_tool_approval_inline(f, chunks[2], app, action, *remaining),
        None => render_input(f, chunks[2], app),
    }
    render_footer(f, chunks[3], app);

    // Everything else here (pickers, text prompts) is a one-shot choice made
    // before a turn even starts, with no transcript underneath it yet to stay
    // faithful to -- floating and centered is fine for those.
    render_overlay(f, size, app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let title = format!(
        "tuisample-code | {} | model: {}",
        app.config.llm.endpoint, app.config.llm.model
    );
    let header = Paragraph::new(title).style(Style::default().fg(Color::Cyan));
    f.render_widget(header, area);
}

fn render_messages(f: &mut Frame, area: Rect, app: &mut App) {
    let width = area.width.saturating_sub(2).max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();

    if !app.greeted && app.messages.is_empty() {
        lines.extend(welcome_lines(app));
    } else {
        for msg in &app.messages {
            // Tool activity is scaffolding, not conversation: one dim line each,
            // no speaker label and no blank separator, so a run of six reads as a
            // compact block rather than three screens of transcript. The full
            // result still goes to the model -- it is just not drawn.
            if msg.role == Role::Tool {
                for wrapped in wrap(msg.body(), width) {
                    lines.push(Line::from(Span::styled(wrapped, role_style(Role::Tool))));
                }
                continue;
            }
            // An assistant turn that was nothing but tool calls has no prose to
            // show; the calls speak for themselves on the lines that follow.
            if msg.role == Role::Assistant && !msg.tool_calls.is_empty() && msg.content.trim().is_empty()
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
                    for wrapped in wrap(msg.body(), width) {
                        lines.push(Line::from(vec![
                            Span::styled("> ", role_style(Role::User)),
                            Span::raw(wrapped),
                        ]));
                    }
                }
                Role::Assistant => {
                    for wrapped in wrap(msg.body(), width) {
                        lines.push(Line::from(wrapped));
                    }
                }
                Role::Error | Role::System => {
                    lines.push(Line::from(vec![Span::styled(
                        format!("{}: ", msg.role.label()),
                        role_style(msg.role),
                    )]));
                    for wrapped in wrap(msg.body(), width) {
                        lines.push(Line::from(wrapped));
                    }
                }
                Role::Tool => unreachable!("handled above"),
            }
            lines.push(Line::from(""));
        }

        if app.state == AppState::ExecutingTools {
            for call in &app.approved_tools {
                let label = crate::tools::describe_action(call)
                    .map(|a| a.label())
                    .unwrap_or_else(|| call.function.name.clone());
                lines.push(Line::from(Span::styled(
                    format!("{label} …"),
                    role_style(Role::Tool),
                )));
            }
        }

        if app.state == AppState::Streaming {
            let body = if app.streaming_response.is_empty() {
                "…".to_string()
            } else {
                app.streaming_response.clone()
            };
            for wrapped in wrap(&body, width) {
                lines.push(Line::from(wrapped));
            }
            lines.push(Line::from(""));
        }
    }

    // Clamp the scroll offset to the content, and stick to the bottom while the
    // user has not scrolled away.
    let viewport = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(viewport) as u16;
    if app.follow_tail {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.follow_tail = true;
        }
    }

    let title = if app.scroll < max_scroll {
        " Messages (↓ for more) ".to_string()
    } else {
        " Messages ".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(lines).block(block).scroll((app.scroll, 0));

    f.render_widget(paragraph, area);
}

fn welcome_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "🚀 Welcome to tuisample-code",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Connected to: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.config.llm.model.clone(),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(vec![
            Span::styled("Endpoint:     ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.config.llm.endpoint.clone(),
                Style::default().fg(Color::Green),
            ),
        ]),
    ];

    // Where commands will run, stated up front. A user should never have to
    // guess this about a tool that can change their files -- and the two
    // dangerous configurations shout rather than blend in.
    if !app.workspace_status.is_empty() {
        let alarming = app.workspace_status.contains("UNATTENDED");
        let colour = if alarming {
            Color::Red
        } else if app.workspace_status.starts_with("off") || app.workspace_status.contains("broad")
        {
            Color::Yellow
        } else {
            Color::Green
        };
        let mut style = Style::default().fg(colour);
        if alarming {
            style = style.add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled("Commands:     ", Style::default().fg(Color::DarkGray)),
            Span::styled(app.workspace_status.clone(), style),
        ]));
    }

    lines.extend([
        Line::from(""),
        Line::from("📝 How to use:"),
        Line::from("  • Type your prompt below"),
        Line::from("  • Press Enter to send"),
        Line::from("  • Alt+Enter (or Shift+Enter) for a new line"),
        Line::from("  • Press Esc to cancel a running request"),
        Line::from(""),
    ]);

    let warnings = app.config.warnings();
    if warnings.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "✓ Ready to go!",
            Style::default().fg(Color::Green),
        )]));
    } else {
        for w in warnings {
            lines.push(Line::from(vec![Span::styled(
                format!("⚠ {w}"),
                Style::default().fg(Color::Yellow),
            )]));
        }
    }
    lines.push(Line::from(""));
    lines
}

fn role_style(role: Role) -> Style {
    match role {
        Role::User => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        Role::Assistant => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Role::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Role::System => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        Role::Tool => Style::default().fg(Color::Blue),
    }
}

fn input_height(app: &App, total_width: u16) -> u16 {
    let width = total_width.saturating_sub(2).max(1) as usize;
    let rows: usize = app
        .input_buffer
        .split('\n')
        .map(|l| hard_wrap_rows(l.chars().count(), width))
        .sum();
    ((rows as u16) + 2).clamp(MIN_INPUT_HEIGHT, MAX_INPUT_HEIGHT)
}

fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(2).max(1) as usize;
    let busy = app.is_busy();

    let (text, style) = if app.input_buffer.is_empty() {
        let hint = if busy {
            "Waiting for the model… (Esc to cancel)".to_string()
        } else {
            "Type your prompt… (Enter to send, Alt+Enter for newline, Ctrl-C to exit)".to_string()
        };
        (hint, Style::default().fg(Color::DarkGray))
    } else {
        (
            app.input_buffer.clone(),
            Style::default().fg(if busy { Color::DarkGray } else { Color::White }),
        )
    };

    // Hard-wrap ourselves so the cursor position below matches exactly what is drawn.
    let mut rendered: Vec<Line> = Vec::new();
    for logical in text.split('\n') {
        for chunk in hard_wrap(logical, width) {
            rendered.push(Line::from(chunk));
        }
    }

    let border_style = if busy {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(if busy { " Input (busy) " } else { " Input " });

    f.render_widget(Paragraph::new(rendered).block(block).style(style), area);

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
        let x = area.x + 1 + screen_col.min(width - 1) as u16;
        let y = area.y + 1 + screen_row.min(max_row) as u16;
        f.set_cursor(x, y);
    }
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let (status, color) = match &app.state {
        AppState::AwaitingInput => ("Ready", Color::Green),
        AppState::Sending => ("Sending…", Color::Yellow),
        AppState::Streaming => ("Streaming…", Color::Yellow),
        AppState::AwaitingApproval => ("Waiting for you", Color::Magenta),
        AppState::ExecutingTools => ("Running command…", Color::Blue),
    };

    let keys = match &app.state {
        AppState::AwaitingApproval => " | y run · n or Esc skip · Ctrl-C exit",
        _ => " | Enter send · Alt+Enter newline · Esc cancel · Ctrl-C exit",
    };

    let footer = Line::from(vec![
        Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
        Span::styled(status, Style::default().fg(color)),
        Span::styled(keys, Style::default().fg(Color::DarkGray)),
    ]);

    f.render_widget(Paragraph::new(footer), area);
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
            let mut items: Vec<String> = providers::PROVIDERS.iter().map(|p| p.label.to_string()).collect();
            items.push("Custom endpoint...".to_string());
            render_picker(f, area, " Select a provider ", &items, *selected);
        }
        Some(Overlay::ModelPicker { provider_id, selected }) => {
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
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            for wrapped in wrap(reason, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(Color::Red),
                )));
            }
            lines.push(Line::from(""));
        }
    }

    let (title, verb) = match action {
        Action::Command { command, purpose } => {
            if let Some(purpose) = purpose {
                for wrapped in wrap(purpose, inner) {
                    lines.push(Line::from(Span::styled(
                        wrapped,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::from(""));
            }
            for wrapped in wrap(command, inner) {
                lines.push(Line::from(Span::styled(
                    format!("$ {wrapped}"),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                )));
            }
            (" Run this command? ", "run")
        }
        Action::Read { path } => {
            lines.push(Line::from(Span::styled(
                format!("📄 {path}"),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )));
            (" Read this file? ", "read")
        }
        Action::Write { path, content } => {
            lines.push(Line::from(Span::styled(
                format!("📝 {path}"),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            if content.is_empty() {
                lines.push(Line::from(Span::styled(
                    "(empty file)",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                let total = content.lines().count();
                for (i, line) in content.lines().enumerate() {
                    if i >= WRITE_PREVIEW_LINES {
                        lines.push(Line::from(Span::styled(
                            format!("… {} more line{}", total - i, if total - i == 1 { "" } else { "s" }),
                            Style::default().fg(Color::DarkGray),
                        )));
                        break;
                    }
                    for wrapped in wrap(line, inner) {
                        lines.push(Line::from(Span::styled(wrapped, Style::default().fg(Color::White))));
                    }
                }
            }
            (" Write this file? ", "write")
        }
    };

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("in {}", app.workspace_root),
        Style::default().fg(Color::DarkGray),
    )));
    if remaining > 0 {
        lines.push(Line::from(Span::styled(
            format!("({remaining} more queued after this one)"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {verb}   "), Style::default().fg(Color::DarkGray)),
        Span::styled("n", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled(" skip   ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled(" skip", Style::default().fg(Color::DarkGray)),
    ]));

    (title, lines)
}

/// Draws the approval prompt into its reserved region at the bottom of the
/// frame -- see the placement comment on `render`. No `Clear` and no
/// centering: unlike a floating popup, this area belongs to the prompt alone,
/// so there is nothing underneath it to protect or re-center against.
fn render_tool_approval_inline(f: &mut Frame, area: Rect, app: &App, action: &Action, remaining: usize) {
    let inner = area.width.saturating_sub(4).max(1) as usize;
    let (title, lines) = tool_approval_lines(app, action, remaining, inner);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(title);
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
    Rect { x, y, width, height }
}

fn render_picker(f: &mut Frame, area: Rect, title: &str, items: &[String], selected: usize) {
    let popup = centered_rect(50, items.len() as u16 + 2, area);
    f.render_widget(Clear, popup);

    let list_items: Vec<ListItem> = items.iter().map(|label| ListItem::new(label.clone())).collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title.to_string());
    let list = List::new(list_items)
        .block(block)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, popup, &mut state);
}

/// Single-line text entry (masked or plain). The cursor always sits at the end
/// of `value` -- the overlay's editing model only supports insert/backspace, no
/// repositioning, so there is nothing else for it to reflect.
fn render_text_prompt(f: &mut Frame, area: Rect, title: &str, hint: &str, value: &str, masked: bool) {
    let popup = centered_rect(60, 5, area);
    f.render_widget(Clear, popup);

    let display = if masked {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title.to_string());

    let value_line = if display.is_empty() {
        Line::from(Span::styled(
            "(type here)",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::styled(display.clone(), Style::default().fg(Color::White)))
    };

    let lines = vec![
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn frame(width: u16, height: u16) -> Rect {
        Rect { x: 0, y: 0, width, height }
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
        app.messages.push(Message::new(Role::User, "delete the build directory"));
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
        let row_text = |y: u16| -> String {
            (0..area.width).map(|x| buffer.get(x, y).symbol()).collect()
        };
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

    /// Regression: labelling every line "You: " / "Assistant: " was what made
    /// this read as a Q&A chat log instead of one continuous stream, the thing
    /// a user compared unfavourably to Claude Code's transcript. The user's own
    /// words still get a "> " quote marker; the assistant's prose gets nothing.
    #[test]
    fn the_transcript_reads_as_a_continuous_stream_not_a_labelled_chat_log() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.messages.push(Message::new(Role::User, "write a hello world function"));
        app.messages.push(Message::new(Role::Assistant, "Here's the function..."));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(rendered.contains("> write a hello world function"), "{rendered}");
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

        assert!(rendered.contains("rm -rf build"), "the command must be shown");
        assert!(rendered.contains("Run this command?"), "{rendered}");
        assert!(rendered.contains("/tmp/project"), "where it runs must be shown");
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
        assert!(rendered.contains("y write"), "the keys must say write, not run");
    }
}
