use crate::agent;
use crate::app::{App, AppState, CustomStep, Entry, Overlay, ToolStatus};
use crate::permission;
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
/// How much of a tool's output to show inline. The model gets all of it; the
/// transcript only needs enough to see what happened.
const TOOL_DETAIL_LINES: usize = 6;

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
    let workspace = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| ".".to_string());
    let title = format!(
        "tuisample-code | {} | model: {} | workspace: {workspace}",
        app.config.llm.endpoint, app.config.llm.model
    );
    let header = Paragraph::new(title).style(Style::default().fg(Color::Cyan));
    f.render_widget(header, area);
}

fn render_messages(f: &mut Frame, area: Rect, app: &mut App) {
    let width = area.width.saturating_sub(2).max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();

    if !app.greeted && app.entries.is_empty() {
        lines.extend(welcome_lines(app));
    } else {
        for entry in &app.entries {
            lines.extend(entry_lines(entry, width));
        }

        // The turn in flight, not yet committed to an entry.
        if app.is_busy() && !app.streaming_response.is_empty() {
            lines.push(labelled(agent_label(app.active_agent), Color::Yellow));
            for wrapped in wrap(&app.streaming_response, width) {
                lines.push(Line::from(wrapped));
            }
            lines.push(Line::from(""));
        } else if app.is_busy() && !app.awaiting_permission() {
            lines.push(Line::from(Span::styled(
                "…",
                Style::default().fg(Color::DarkGray),
            )));
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

fn entry_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match entry {
        Entry::User(text) => {
            lines.push(labelled("You", Color::Green));
            lines.extend(wrapped_lines(text, width, Style::default()));
            lines.push(Line::from(""));
        }
        Entry::Agent { agent, text } => {
            lines.push(labelled(agent_label(agent), Color::Yellow));
            lines.extend(wrapped_lines(text, width, Style::default()));
            lines.push(Line::from(""));
        }
        Entry::Tool {
            summary,
            status,
            detail,
            ..
        } => {
            let (glyph, color) = match status {
                ToolStatus::Running => ("◐", Color::Yellow),
                ToolStatus::Ok => ("●", Color::Green),
                ToolStatus::Failed => ("✗", Color::Red),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(summary.clone(), Style::default().fg(Color::White)),
            ]));
            lines.extend(detail_lines(detail, width));
            lines.push(Line::from(""));
        }
        Entry::System(text) => {
            lines.push(labelled("System", Color::Magenta));
            lines.extend(wrapped_lines(text, width, Style::default()));
            lines.push(Line::from(""));
        }
        Entry::Error(text) => {
            lines.push(labelled("Error", Color::Red));
            lines.extend(wrapped_lines(text, width, Style::default()));
            lines.push(Line::from(""));
        }
    }
    lines
}

/// A preview of a tool's output, indented under its call. Truncated on purpose:
/// a 400-line `cargo test` run would otherwise bury the conversation.
fn detail_lines(detail: &str, width: usize) -> Vec<Line<'static>> {
    let detail = detail.trim();
    if detail.is_empty() {
        return Vec::new();
    }

    let style = Style::default().fg(Color::DarkGray);
    let inner = width.saturating_sub(4).max(1);
    let all: Vec<&str> = detail.lines().collect();
    let mut lines: Vec<Line<'static>> = Vec::new();

    for line in all.iter().take(TOOL_DETAIL_LINES) {
        for chunk in hard_wrap(line, inner) {
            lines.push(Line::from(Span::styled(format!("  │ {chunk}"), style)));
        }
    }
    if all.len() > TOOL_DETAIL_LINES {
        lines.push(Line::from(Span::styled(
            format!("  │ … {} more lines", all.len() - TOOL_DETAIL_LINES),
            style,
        )));
    }
    lines
}

fn labelled(label: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        format!("{label}: "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn wrapped_lines(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    wrap(text, width)
        .into_iter()
        .map(|w| Line::from(Span::styled(w, style)))
        .collect()
}

fn agent_label(id: &str) -> &'static str {
    agent::find(id).map(|a| a.label).unwrap_or("Assistant")
}

fn welcome_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "🚀 tuisample-code",
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
        Line::from(vec![
            Span::styled("Workspace:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| ".".to_string()),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(""),
        Line::from("📝 Ask for a change, not just an answer:"),
        Line::from("  • \"add a --json flag to the CLI and a test for it\""),
        Line::from("  • \"why does the auth test fail on CI but not locally?\""),
        Line::from(""),
        Line::from(vec![Span::styled(
            "The agent reads and searches on its own. It asks before writing files",
            Style::default().fg(Color::DarkGray),
        )]),
        Line::from(vec![Span::styled(
            "or running commands. Esc cancels a run at any point.",
            Style::default().fg(Color::DarkGray),
        )]),
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
        let hint = if app.awaiting_permission() {
            "Waiting for your answer above…".to_string()
        } else if busy {
            "Agent working… (Esc to cancel)".to_string()
        } else {
            "What should I change? (Enter to send, Alt+Enter for newline, Ctrl-C to exit)"
                .to_string()
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
    // a text-entry overlay is active (f.set_cursor is last-write-wins).
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
    let (status, color) = if app.awaiting_permission() {
        ("Needs approval", Color::Magenta)
    } else {
        match &app.state {
            AppState::AwaitingInput => ("Ready", Color::Green),
            AppState::Sending { .. } => ("Starting…", Color::Yellow),
            AppState::Working => ("Working…", Color::Yellow),
        }
    };

    let footer = Line::from(vec![
        Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
        Span::styled(status, Style::default().fg(color)),
        Span::styled(
            " | Enter send · Alt+Enter newline · Esc cancel · /new reset · Ctrl-C exit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    f.render_widget(Paragraph::new(footer), area);
}

// ---- overlays ------------------------------------------------------------------

fn render_overlay(f: &mut Frame, area: Rect, app: &App) {
    match &app.overlay {
        None => {}
        Some(Overlay::Permission { summary, grant }) => {
            render_permission(f, area, summary, grant.as_deref())
        }
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
    }
}

/// The gate between the model and the user's disk. It shows exactly the call
/// that is about to run, in the same wording the transcript will use afterwards.
fn render_permission(f: &mut Frame, area: Rect, summary: &str, grant: Option<&str>) {
    let Some(popup) = popup_area(72, 9, area) else {
        return;
    };
    f.render_widget(Clear, popup);

    let inner = popup.width.saturating_sub(2).max(1) as usize;
    let mut lines = vec![Line::from(Span::styled(
        "The agent wants to:",
        Style::default().fg(Color::DarkGray),
    ))];
    for chunk in wrap(summary, inner) {
        lines.push(Line::from(Span::styled(
            chunk,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[a]", Style::default().fg(Color::Green)),
        Span::raw(" allow once   "),
        Span::styled("[d]", Style::default().fg(Color::Red)),
        Span::raw(" deny"),
    ]));
    // Only offered when the call is actually safe to generalise -- a compound
    // shell command carries no grant key and must be answered every time.
    match grant {
        Some(key) => lines.push(Line::from(vec![
            Span::styled("[s]", Style::default().fg(Color::Cyan)),
            Span::raw(format!(" allow {} for this session", permission::grant_description(key))),
        ])),
        None => lines.push(Line::from(Span::styled(
            "(not offered for the session: this command combines several programs)",
            Style::default().fg(Color::DarkGray),
        ))),
    }
    lines.push(Line::from(Span::styled(
        "Esc cancels the whole run.",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(" Approve action ");
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Centers a popup sized to its content within `area`, clamped so it never
/// exceeds the available space, with an absolute floor so tiny terminals don't
/// produce an unreadably small popup.
///
/// Returns `None` when `area` has no room at all. It really can be zero-sized --
/// a terminal reporting 0x0 (a pty opened without a window size, a window
/// dragged shut) -- and drawing into it panics inside ratatui rather than
/// clipping, so every caller must skip instead.
fn popup_area(desired_width: u16, desired_height: u16, area: Rect) -> Option<Rect> {
    let width = desired_width.max(MIN_POPUP_WIDTH).min(area.width);
    let height = desired_height.max(MIN_POPUP_HEIGHT).min(area.height);
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    })
}

fn render_picker(f: &mut Frame, area: Rect, title: &str, items: &[String], selected: usize) {
    let Some(popup) = popup_area(50, items.len() as u16 + 2, area) else {
        return;
    };
    f.render_widget(Clear, popup);

    let list_items: Vec<ListItem> = items
        .iter()
        .map(|label| ListItem::new(label.clone()))
        .collect();
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
fn render_text_prompt(
    f: &mut Frame,
    area: Rect,
    title: &str,
    hint: &str,
    value: &str,
    masked: bool,
) {
    let Some(popup) = popup_area(60, 5, area) else {
        return;
    };
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
        Line::from(Span::styled(
            display.clone(),
            Style::default().fg(Color::White),
        ))
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
    use crate::app::CustomStep;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Draw a whole frame into a buffer of exactly this size.
    fn render_at(width: u16, height: u16, app: &mut App) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
    }

    fn app_with(overlay: Option<Overlay>) -> App {
        let mut app = App::new(Config::default());
        app.overlay = overlay;
        app
    }

    /// A terminal really can report 0x0 (a pty opened with no window size, a
    /// window dragged shut). ratatui's `Clear` indexes the buffer directly, so
    /// an overlay drawn into a space too small for it panics rather than
    /// clipping -- which took down the whole app the first time it happened.
    #[test]
    fn overlays_do_not_panic_in_a_terminal_too_small_to_hold_them() {
        let overlays = [
            Overlay::Permission {
                summary: "run_shell(cargo test --all)".to_string(),
                grant: Some("run_shell:cargo".to_string()),
            },
            Overlay::ProviderPicker { selected: 0 },
            Overlay::ModelPicker {
                provider_id: "deepseek",
                selected: 0,
            },
            Overlay::CustomEndpoint(CustomStep::Endpoint),
        ];

        for overlay in overlays {
            for (w, h) in [(0, 0), (1, 1), (4, 2), (20, 6), (80, 24)] {
                render_at(w, h, &mut app_with(Some(overlay.clone())));
            }
        }
    }

    #[test]
    fn the_base_layout_survives_a_zero_sized_terminal() {
        let mut app = app_with(None);
        app.entries.push(Entry::User("hello".to_string()));
        app.entries.push(Entry::Tool {
            call_id: "c1".to_string(),
            summary: "read_file(a.rs)".to_string(),
            status: ToolStatus::Running,
            detail: "some output".to_string(),
        });
        for (w, h) in [(0, 0), (1, 1), (3, 3), (80, 24)] {
            render_at(w, h, &mut app);
        }
    }

    fn text_of(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_tool_entry_renders_its_status_glyph_and_summary() {
        let entry = Entry::Tool {
            call_id: "c1".to_string(),
            summary: "run_shell(cargo test)".to_string(),
            status: ToolStatus::Ok,
            detail: "test result: ok.".to_string(),
        };
        let rendered = text_of(&entry_lines(&entry, 60));
        assert!(rendered.contains("● run_shell(cargo test)"), "{rendered}");
        assert!(rendered.contains("│ test result: ok."), "{rendered}");
    }

    #[test]
    fn a_failed_tool_entry_is_marked_distinctly_from_a_running_one() {
        let make = |status| {
            text_of(&entry_lines(
                &Entry::Tool {
                    call_id: "c1".to_string(),
                    summary: "edit_file(a.rs)".to_string(),
                    status,
                    detail: String::new(),
                },
                60,
            ))
        };
        assert!(make(ToolStatus::Failed).starts_with('✗'));
        assert!(make(ToolStatus::Running).starts_with('◐'));
        assert!(make(ToolStatus::Ok).starts_with('●'));
    }

    /// A long `cargo test` run must not bury the conversation.
    #[test]
    fn long_tool_output_is_truncated_with_a_count() {
        let detail = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entry = Entry::Tool {
            call_id: "c1".to_string(),
            summary: "run_shell(cargo test)".to_string(),
            status: ToolStatus::Ok,
            detail,
        };
        let rendered = text_of(&entry_lines(&entry, 60));
        assert!(rendered.contains("line 0"));
        assert!(!rendered.contains("line 30"));
        assert!(
            rendered.contains(&format!("… {} more lines", 40 - TOOL_DETAIL_LINES)),
            "{rendered}"
        );
    }

    #[test]
    fn a_tool_entry_with_no_output_yet_renders_only_its_call_line() {
        let entry = Entry::Tool {
            call_id: "c1".to_string(),
            summary: "read_file(a.rs)".to_string(),
            status: ToolStatus::Running,
            detail: String::new(),
        };
        // Call line plus the trailing blank separator, nothing else.
        assert_eq!(entry_lines(&entry, 60).len(), 2);
    }

    #[test]
    fn agent_entries_are_labelled_with_the_agents_display_name() {
        let entry = Entry::Agent {
            agent: "coder",
            text: "Done.".to_string(),
        };
        assert!(text_of(&entry_lines(&entry, 60)).starts_with("Coder: "));
    }

    /// An unknown id must not panic the renderer -- agent ids also arrive from
    /// stored conversations.
    #[test]
    fn an_unknown_agent_id_falls_back_to_a_generic_label() {
        assert_eq!(agent_label("nobody"), "Assistant");
        assert_eq!(agent_label("coder"), "Coder");
    }

    #[test]
    fn wrap_preserves_explicit_newlines_and_breaks_long_words() {
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
        // A path too long to break on spaces still has to fit.
        let wrapped = wrap("/very/long/path/that/cannot/break", 10);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 10));
    }
}
