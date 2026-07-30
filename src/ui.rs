use crate::app::{App, AppState};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn render<B: Backend>(f: &mut Frame<B>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.size());

    // Header
    render_header(f, chunks[0], app);

    // Messages area (scrollable)
    render_messages(f, chunks[1], app);

    // Input area
    render_input(f, chunks[2], app);

    // Footer (stats)
    render_footer(f, chunks[3], app);
}

fn render_header<B: Backend>(f: &mut Frame<B>, area: Rect, app: &App) {
    let title = format!(
        "tuisample-code | {} | model: {}",
        app.config.llm.endpoint, app.config.llm.model
    );
    let header = Paragraph::new(title).style(Style::default().fg(Color::Cyan));
    f.render_widget(header, area);
}

fn render_messages<B: Backend>(f: &mut Frame<B>, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        let role_style = if msg.role == "You" {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Yellow)
        };

        lines.push(Line::from(vec![Span::styled(
            format!("{}: ", msg.role),
            role_style,
        )]));
        lines.push(Line::from(msg.content.clone()));
        lines.push(Line::from(""));
    }

    // Add streaming response if present
    if let AppState::Streaming { response } = &app.state {
        lines.push(Line::from(vec![Span::styled(
            "Assistant: ",
            Style::default().fg(Color::Yellow),
        )]));
        lines.push(Line::from(response.clone()));
        lines.push(Line::from(""));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Messages ");

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: true });

    f.render_widget(paragraph, area);
}

fn render_input<B: Backend>(f: &mut Frame<B>, area: Rect, app: &App) {
    let cursor_style = Style::default()
        .bg(Color::White)
        .fg(Color::Black);

    let input_text = if app.input_buffer.is_empty() {
        "Type your prompt... (Ctrl-Enter to send, Esc to cancel)".to_string()
    } else {
        app.input_buffer.clone()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Input ");

    let paragraph = Paragraph::new(input_text)
        .block(block)
        .style(Style::default().fg(Color::Cyan))
        .wrap(Wrap { trim: true });

    f.render_widget(paragraph, area);

    // Render cursor if in input mode
    if matches!(app.state, AppState::AwaitingInput) {
        if area.height > 1 && area.width > 2 {
            let cursor_x = area.x + (app.input_buffer.len() as u16).min(area.width - 3) + 1;
            let cursor_y = area.y + 1;
            f.set_cursor(cursor_x, cursor_y);
        }
    }
}

fn render_footer<B: Backend>(f: &mut Frame<B>, area: Rect, app: &App) {
    let status = match &app.state {
        AppState::AwaitingInput => "Ready",
        AppState::Sending { .. } => "Sending...",
        AppState::Streaming { .. } => "Streaming...",
        AppState::Done { .. } => "Complete",
    };

    let footer = format!("Status: {} | Press Ctrl-C to exit", status);
    let paragraph = Paragraph::new(footer).style(Style::default().fg(Color::DarkGray));

    f.render_widget(paragraph, area);
}
