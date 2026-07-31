use crate::app::{App, AppState, CustomStep, Overlay, Role};
use crate::providers;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

const MIN_INPUT_HEIGHT: u16 = 3;
const MAX_INPUT_HEIGHT: u16 = 10;
const MIN_POPUP_WIDTH: u16 = 40;
const MIN_POPUP_HEIGHT: u16 = 6;

pub fn render(f: &mut Frame, app: &mut App) {
    let size = f.size();
    let input_height = input_height(app, size.width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(size);

    render_header(f, chunks[0], app);
    render_messages(f, chunks[1], app);
    render_input(f, chunks[2], app);
    render_footer(f, chunks[3], app);
    // Last, so it draws over everything already painted this frame.
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
            lines.push(Line::from(vec![Span::styled(
                format!("{}: ", msg.role.label()),
                role_style(msg.role),
            )]));
            for wrapped in wrap(&msg.content, width) {
                lines.push(Line::from(wrapped));
            }
            lines.push(Line::from(""));
        }

        if app.state == AppState::Streaming {
            lines.push(Line::from(vec![Span::styled(
                "Assistant: ",
                role_style(Role::Assistant),
            )]));
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
        Line::from(""),
        Line::from("📝 How to use:"),
        Line::from("  • Type your prompt below"),
        Line::from("  • Press Enter to send"),
        Line::from("  • Alt+Enter (or Shift+Enter) for a new line"),
        Line::from("  • Press Esc to cancel a running request"),
        Line::from(""),
    ];

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
        AppState::Sending { .. } => ("Sending…", Color::Yellow),
        AppState::Streaming => ("Streaming…", Color::Yellow),
    };

    let footer = Line::from(vec![
        Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
        Span::styled(status, Style::default().fg(color)),
        Span::styled(
            " | Enter send · Alt+Enter newline · Esc cancel · Ctrl-C exit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    f.render_widget(Paragraph::new(footer), area);
}

// ---- /provider and /model overlays ---------------------------------------------

fn render_overlay(f: &mut Frame, area: Rect, app: &App) {
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
    }
}

/// Centers a popup sized to its content within `area`, clamped so it never
/// exceeds the available space, with an absolute floor so tiny terminals don't
/// produce an unreadably small popup.
fn centered_rect(desired_width: u16, desired_height: u16, area: Rect) -> Rect {
    let width = desired_width.max(MIN_POPUP_WIDTH).min(area.width.max(1));
    let height = desired_height.max(MIN_POPUP_HEIGHT).min(area.height.max(1));
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
