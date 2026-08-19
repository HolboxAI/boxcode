use crate::app::{App, AppState, CustomStep, Message, Overlay, Role};
use crate::approval::ApprovalRequest;
use crate::deploy::{DeploySession, DeployStatus, Menu, Stage, StepState};
use crate::diff::{Change, FileDiff};
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
        Some(Overlay::ToolApproval(request)) => Some(request.clone()),
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
            Some(request) => {
                let inner_width = size.width.saturating_sub(4).max(1) as usize;
                let (_, lines) = tool_approval_lines(app, request, inner_width);
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
            Some(request) => render_tool_approval_inline(f, chunks[2], app, request),
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
        // A file change gets its diff underneath, indented under the tool
        // line it belongs to. This is the only tool output drawn in full
        // rather than summarised, and deliberately: "wrote 4kb" and "changed
        // these four lines" are not the same statement, and only one of them
        // can be checked.
        if let Some(diff) = &msg.diff {
            lines.extend(diff_lines(diff, width, 2));
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
        Role::Assistant => {
            lines.extend(wrapped_lines(msg.body(), width));
            // A reply that made no tool calls reads identically on screen to
            // one that did real work -- that gap is exactly what let a
            // fabricated "I've created the tables" pass as a real status
            // update with nothing to contradict it. Say which this was,
            // always: unremarkable on a plain answer to a plain question,
            // the one place it matters it is now impossible to miss without
            // reading a raw session log by hand.
            if msg.tool_calls.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (no tool call this turn)",
                    theme::faint(),
                )));
            }
        }
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
        // A `/compact` summary is drawn like a status event rather than like
        // prose: it is labelled, so it is never mistaken for a reply the model
        // gave to something, and its own body is shown in full -- what the
        // model will be working from next is worth being able to read.
        Role::System | Role::Summary | Role::Context => {
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

/// Tabs are drawn as this many spaces.
///
/// Not left as `\t`: a terminal resolves a tab against its own column, but a
/// diff row has already spent columns on the line number and the `+`, so the
/// stop lands somewhere different on every row and tab-indented code comes out
/// with a ragged left edge. Expanding it here means the gutter and the code
/// agree about where a column is.
const DIFF_TAB: &str = "    ";

/// One file change, drawn the way a diff is read: a line number, a `+`/`-`,
/// and the line itself, with removals in red and additions in green.
///
/// The unchanged lines around a change are kept and dimmed. They are what make
/// a diff answer "what is this doing" rather than only "what did it type" --
/// three lines of context is the difference between `+    return None` on its
/// own and the same line under the `if` it belongs to.
///
/// Long lines are hard-wrapped rather than word-wrapped, and the wrap is
/// indented under the text rather than under the gutter: code has no word
/// boundaries worth preserving, and re-flowing it would silently change the
/// indentation that is often the thing being reviewed.
fn diff_lines(diff: &FileDiff, width: usize, indent: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    if diff.is_empty() {
        return out;
    }

    // One number column, sized to the largest number actually shown, so a
    // 40-line file does not get the gutter of a 4000-line one. Removals show
    // where they were, everything else shows where it now is.
    let widest = diff
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter_map(|l| l.new_no.or(l.old_no))
        .max()
        .unwrap_or(0);
    let numw = widest.to_string().len().max(2);
    let pad = " ".repeat(indent);
    // gutter = indent + number + space + mark + space
    let text_width = width.saturating_sub(indent + numw + 3).max(8);

    for (h, hunk) in diff.hunks.iter().enumerate() {
        // Between hunks, and only between them: a marker before the first or
        // after the last would claim something was skipped at an end where
        // nothing was.
        if h > 0 {
            out.push(Line::from(Span::styled(
                format!("{pad}{:>numw$} ⋮", ""),
                theme::faint(),
            )));
        }
        for line in &hunk.lines {
            let (mark, style) = match line.change {
                Change::Added => ('+', Style::default().fg(theme::p().success)),
                Change::Removed => ('-', Style::default().fg(theme::p().danger)),
                Change::Context => (' ', theme::faint()),
            };
            let number = match line.change {
                Change::Removed => line.old_no,
                _ => line.new_no,
            };
            let number = number.map(|n| n.to_string()).unwrap_or_default();
            let body = line.text.replace('\t', DIFF_TAB);
            for (i, chunk) in hard_chunks(&body, text_width).into_iter().enumerate() {
                let gutter = if i == 0 {
                    format!("{pad}{number:>numw$} {mark} ")
                } else {
                    // A continuation carries neither a number nor a sign: it
                    // is not another line, and showing the `+` twice would
                    // read as two additions.
                    format!("{pad}{:>numw$}   ", "")
                };
                out.push(Line::from(vec![
                    Span::styled(gutter, theme::faint()),
                    Span::styled(chunk, style),
                ]));
            }
        }
    }

    if diff.clipped > 0 {
        out.push(Line::from(Span::styled(
            format!(
                "{pad}{:>numw$} … {} more line{}",
                "",
                diff.clipped,
                if diff.clipped == 1 { "" } else { "s" }
            ),
            theme::faint(),
        )));
    }
    out
}

/// The literal replacement spans, shown when no diff could be produced.
///
/// This was the whole of an edit approval before diffs existed, and it is kept
/// for the cases a diff cannot describe: the file is missing, or `old_string`
/// does not match anything in it. Both are edits that will fail -- but the
/// user is still being asked, so the question still has to show what was asked
/// about rather than an empty box.
fn render_edit_spans(
    lines: &mut Vec<Line<'static>>,
    edits: &[crate::tools::EditSpan],
    batch: bool,
    inner: usize,
) {
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
    for (i, edit) in edits.iter().enumerate() {
        let replace_label = if batch {
            let all = if edit.replace_all { ", all occurrences" } else { "" };
            format!("edit {} of {} — replace:{all}", i + 1, edits.len())
        } else {
            "replace:".to_string()
        };
        span(&replace_label, &edit.old, theme::p().danger);
        span("with:", &edit.new, theme::p().success);
    }
}

/// Split a line into `width`-column pieces without looking for word breaks.
/// An empty line still yields one (empty) piece, so it occupies a row.
fn hard_chunks(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width)
        .map(|c| c.iter().collect::<String>())
        .collect()
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
            // A tool that is still running gets the spinner where a finished
            // one gets `TOOL_MARK`, in the accent rather than the faint tone.
            // This is what tells a slow `cargo test` apart from one that has
            // already come back: both print the same label, and before this
            // the only thing moving on screen was the one summary line at the
            // bottom, which says how many are running but not which.
            let frame = theme::spinner(app.busy_started.map(|t| t.elapsed()).unwrap_or_default());
            for call in &app.running_tools {
                let label = crate::tools::describe_action(call)
                    .map(|a| a.label())
                    .unwrap_or_else(|| call.function.name.clone());
                lines.push(Line::from(vec![
                    Span::styled(format!("{frame} "), theme::accent()),
                    Span::styled(label, role_style(Role::Tool)),
                ]));
                // A running subagent gets one live sub-line: which round it
                // is on and the last thing it did. One line, not the whole
                // trail -- this area redraws every frame, and the full story
                // is `/subagents`' job once the child is done.
                if let Some(trail) = app.running_subagent_trail(&call.id) {
                    if let Some(step) = trail.steps.last() {
                        lines.push(Line::from(Span::styled(
                            format!(
                                "  {} round {} · {step}",
                                theme::BRANCH_MARK,
                                trail.rounds.max(1)
                            ),
                            theme::faint(),
                        )));
                    }
                }
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
            // One line of what the model is actually thinking about, under the
            // spinner. One line, not the stream: this redraws every frame, and
            // a chain of thought scrolling past at streaming speed is not
            // something anyone reads -- it is something that proves the thing
            // is alive, which one line does just as well.
            if let Some(thought) = app.thinking_line() {
                let room = width.saturating_sub(2).max(8);
                // The *end* of the line when it does not fit, not the start:
                // what the model is working through right now is the part that
                // shows it is still moving, and the head of a long thought
                // would sit frozen while the tail scrolled on invisibly.
                let shown = if thought.chars().count() > room {
                    let skip = thought.chars().count() - (room - 1);
                    format!("…{}", thought.chars().skip(skip).collect::<String>())
                } else {
                    thought.to_string()
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(shown, theme::faint()),
                ]));
            }
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
    // Two clocks, and they are not the same number. `request` is how long the
    // round in flight has taken, which is what the verb beside it describes.
    // `turn` is everything since Enter -- tool runs, npm downloads, every
    // completed round trip -- and is named as the turn so it cannot be read as
    // the other. Only shown once they have visibly diverged: on a plain
    // question they are the same figure, and printing it twice would be noise.
    let turn = app.busy_started.map(|t| t.elapsed());
    let request = app.request_started.map(|t| t.elapsed()).or(turn);
    let secs = request.map(|e| e.as_secs()).unwrap_or(0);
    let turn_secs = turn.map(|e| e.as_secs()).unwrap_or(0);
    let frame = theme::spinner(request.unwrap_or_default());

    let (verb, detail) = match app.state {
        AppState::AwaitingInput => return None,
        AppState::AwaitingApproval => return None,
        // "Compacting" for both phases of a `/compact`: from the outside it is
        // one operation, and watching it switch from Thinking to Responding
        // would suggest it had started answering something.
        AppState::Sending if app.compacting => ("Compacting".to_string(), String::new()),
        AppState::Streaming if app.compacting => (
            "Compacting".to_string(),
            format!(" · summarising {} messages", app.context_size().messages),
        ),
        AppState::Sending => ("Waiting".to_string(), String::new()),
        AppState::Streaming => {
            // See App::approx_tokens_this_turn -- the same estimate the
            // persisted usage log uses, always labelled "~" since it is one.
            let approx_tokens = app.approx_tokens_this_turn();
            let detail = if approx_tokens > 0 {
                format!(" · ~{approx_tokens} tokens")
            } else {
                String::new()
            };
            // "Thinking" while reasoning is arriving and no answer has started
            // -- which is the truth, and is the difference between a screen
            // that looks hung and one that looks busy. A reasoning model can
            // spend minutes here, and every byte of it used to be discarded
            // with nothing on screen to show for it.
            let verb = if app.thinking_line().is_some() {
                "Thinking"
            } else {
                "Responding"
            };
            (verb.to_string(), detail)
        }
        AppState::ExecutingTools => {
            let n = app.running_tools.len();
            // "command" would be a small lie about a subagent, which runs
            // many commands of its own -- and "waiting on a child" is worth
            // saying, since it can take noticeably longer than one command.
            let agents = app
                .running_tools
                .iter()
                .filter(|c| c.function.name == crate::tools::AGENT)
                .count();
            let noun = if agents == n && n > 0 { "subagent" } else { "command" };
            (
                format!("Running {n} {noun}{}", if n == 1 { "" } else { "s" }),
                String::new(),
            )
        }
    };

    // A second or more apart is where the two clocks start telling different
    // stories; below that the difference is rounding.
    let turn_note = if turn_secs > secs {
        format!(" · {turn_secs}s this turn")
    } else {
        String::new()
    };
    Some(Line::from(vec![
        Span::styled(format!("{frame} "), theme::accent()),
        Span::styled(format!("{verb}… "), Style::default().fg(theme::p().accent_soft)),
        Span::styled(
            format!("({secs}s{detail}{turn_note} · esc to interrupt)"),
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
        // A workspace that failed to open at all ("off") gets the same
        // treatment as UNATTENDED, not the milder "broad" warning: every
        // file tool has nothing to resolve against for the entire
        // session, which is a worse state than a working-but-wide
        // directory, and a single warning-coloured line at startup was
        // easy to read past -- confirmed live, a real /pull session ran
        // for several turns in this state before anyone noticed.
        let workspace_failed = app.workspace_status.starts_with("off");
        let alarming = app.workspace_status.contains("UNATTENDED") || workspace_failed;
        let colour = if alarming {
            theme::p().danger
        } else if app.workspace_status.contains("broad") {
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
    // Only when it is on -- which at this point means `--plan` was passed,
    // since the welcome panel is printed before anything has been typed. A
    // "mode: normal" row every launch would be a line of noise stating the
    // default.
    if app.mode.is_plan() {
        lines.push(field(
            "mode",
            "plan — read-only until you approve a plan".to_string(),
            Style::default()
                .fg(theme::p().accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Work agreed earlier, possibly in a session days ago, that this one has
    // already picked up. Stated on the way in because the model is following
    // it from the first prompt -- finding that out by watching it start
    // editing files would be a nasty surprise.
    if let Some(plan) = app.active_plan.as_ref().filter(|p| !p.is_finished()) {
        let (done, total) = plan.progress();
        lines.push(field(
            "plan",
            format!("{done}/{total} — {}", shorten(&plan.title, 40)),
            Style::default().fg(theme::p().accent_soft),
        ));
        if let Some((n, step)) = plan.next_step() {
            lines.push(field(
                "next",
                format!("{n}. {}", shorten(&step.description, 46)),
                theme::muted(),
            ));
        }
    }

    lines.push(Line::from(""));
    for (name, desc) in app.available_commands() {
        lines.push(Line::from(vec![
            Span::styled(format!("{name:<13}"), theme::key()),
            Span::styled(desc, theme::muted()),
        ]));
    }

    // `startup_notices` is also where a stale, finished, or unreadable plan
    // lands -- see `App::adopt_plan`. None of them are fatal, but they are all
    // things the user should know before typing rather than after.
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
    // The second line is a promise about what this program will do to your
    // machine, read before the first prompt is typed, so it has to track the
    // setting rather than state the stricter of the two and be wrong for
    // whoever is not in it.
    let approval_tip = match app.config.tools.approval {
        crate::config::ApprovalMode::Destructive => {
            "Destructive commands wait for your approval — deleting, force-pushing, publishing."
        }
        crate::config::ApprovalMode::Always => {
            "Every command and every write waits for your approval."
        }
    };
    for tip in [
        "Ask about this project — it can read files and run commands.",
        approval_tip,
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
/// Clip to `max` characters with an ellipsis, counting chars rather than
/// bytes so a title with an accent in it cannot be split mid-character.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

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
        // The accent, where System takes the muted tone: a summary is the
        // conversation now, not a note about it.
        Role::Summary => Style::default()
            .fg(theme::p().accent)
            .add_modifier(Modifier::BOLD),
        // Warning-toned: it is always reporting that something on disk is no
        // longer what the messages above it say it is.
        Role::Context => Style::default()
            .fg(theme::p().warning)
            .add_modifier(Modifier::BOLD),
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

    // `MAX_COMMAND_MENU_HEIGHT` guarantees the *cap* is never the thing that
    // hides a command, but the viewport is twelve rows in total, so the layout
    // can still hand this less room than the list needs. A `Paragraph` clips
    // from the bottom, which means arrowing onto an entry past the fold moved
    // a cursor nobody could see onto an entry nobody could read. Scroll the
    // window instead, keeping the selection inside it whatever height we get.
    let visible = area.height.saturating_sub(2) as usize;
    let start = if visible == 0 {
        0
    } else {
        selected
            .saturating_sub(visible - 1)
            .min(matches.len().saturating_sub(visible))
    };
    let shown = matches.iter().enumerate().skip(start).take(visible.max(1));

    let lines: Vec<Line> = shown
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
        } else if app.mode.is_plan() {
            "Describe the change — you'll get a plan to approve first…".to_string()
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

    // Where the cursor sits in the *wrapped* text. Needed before drawing, not
    // after, because it is also what decides how far the box is scrolled.
    let (cursor_row, screen_col) = {
        let (row, col) = app.cursor_position();
        let mut wrapped_row = 0usize;
        for (i, logical) in app.input_buffer.split('\n').enumerate() {
            if i == row {
                break;
            }
            wrapped_row += hard_wrap_rows(logical.chars().count(), width);
        }
        (wrapped_row + col / width, col % width)
    };

    // The box stops growing at `MAX_INPUT_HEIGHT`, so a prompt longer than
    // that has more rows than there is room for. A `Paragraph` clips from the
    // bottom, which meant the top of the prompt stayed pinned on screen while
    // everything being typed happened below the fold -- and the cursor, being
    // clamped to the last visible row, sat still while the text moved. Scroll
    // to follow the cursor instead, exactly as the command menu does with its
    // selection, so Up/Down walk through a long prompt and you can see where
    // you are.
    let total_rows = rendered.len();
    let visible = area.height.saturating_sub(2).max(1) as usize;
    let scroll = cursor_row
        .saturating_sub(visible - 1)
        .min(total_rows.saturating_sub(visible));

    f.render_widget(
        Paragraph::new(rendered).block(block).scroll((scroll as u16, 0)),
        area,
    );

    // Cursor: only meaningful while the user can actually type, and only one
    // widget may claim it per frame -- render_overlay claims it instead while
    // an overlay is active (f.set_cursor is last-write-wins).
    if !busy && app.overlay.is_none() && area.height > 2 && area.width > 2 {
        let x = area.x + 1 + PROMPT_GUTTER as u16 + screen_col.min(width - 1) as u16;
        let y = area.y + 1 + cursor_row.saturating_sub(scroll).min(visible - 1) as u16;
        f.set_cursor(x, y);
    }
}

/// A dim key bar under the prompt. What the app is *doing* is on the spinner
/// line in the transcript instead -- next to the work, not stranded at the
/// bottom of the screen.
fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys: &[(&str, &str)] = match &app.state {
        // First, so it wins over every other state: once a quit is pending it
        // is the most important thing on screen, and it is true whether or not
        // an approval or a deployment happens to be open. Said here, where the
        // keys are, because a confirmation printed into the transcript would
        // scroll away -- and this is only true until the next keystroke.
        _ if app.quit_armed => &[("^c", "press again to quit")],
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

    // Plan mode is stated on every frame, not just announced when it was
    // switched on. It changes what the next keystroke can possibly do, and
    // the line saying so scrolls away within a few exchanges -- after which
    // a session that quietly refuses to write anything looks like a bug.
    if app.mode.is_plan() {
        spans.push(Span::styled(
            "PLAN",
            Style::default()
                .fg(theme::p().accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("  ·  ", theme::faint()));
    } else if let Some(plan) = app.active_plan.as_ref().filter(|p| !p.is_finished()) {
        // Which plan, and how far through. The model is following this whether
        // or not the user remembers agreeing to it -- most of all in a session
        // that resumed one written days ago.
        let (done, total) = plan.progress();
        spans.push(Span::styled(
            format!("▸ {done}/{total}"),
            Style::default()
                .fg(theme::p().accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {}", shorten(&plan.title, 28)),
            theme::faint(),
        ));
        spans.push(Span::styled("  ·  ", theme::faint()));
    }

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
        Some(Overlay::ArtifactPicker { items, selected }) => {
            // Just the id, not the full path: a picker row is one line and a
            // canonicalized absolute path routinely runs past the popup
            // width with nothing to wrap it, so it silently clipped mid-word
            // (e.g. ".../hello-bo"). The id alone always fits; disambiguating
            // several ids from the same dev session is left to the dev for
            // now (they can still open the published URL to check).
            let labels: Vec<String> = items.iter().map(|(_, id)| id.clone()).collect();
            render_picker(f, area, " Pull a project (last 48h) ", &labels, *selected);
        }
        Some(Overlay::RollbackConfirm {
            steps,
            warning,
            confirmed,
        }) => render_rollback_confirm(f, area, steps, warning.as_deref(), *confirmed),
        // Drawn inline at the bottom of the frame by `render`, not as a
        // floating overlay -- see the comment there.
        Some(Overlay::ToolApproval { .. }) | Some(Overlay::Deploy) => {}
    }
}

/// How many files the rollback confirmation lists before it stops naming them
/// and just counts the rest. A popup that grows past the terminal would push
/// the yes/no line off screen, which is the one line that must always be
/// visible.
const ROLLBACK_LIST_LINES: usize = 12;

/// The `/rollback` confirmation: what will happen, why it might not be enough,
/// and a yes/no that starts on no.
///
/// Every entry is spelled out rather than summarised into a count. "12 files"
/// is not something anyone can agree to; `delete src/api.rs` is.
fn render_rollback_confirm(
    f: &mut Frame,
    area: Rect,
    steps: &[crate::rollback::Step],
    warning: Option<&str>,
    confirmed: bool,
) {
    let width = area.width.saturating_sub(8).clamp(MIN_POPUP_WIDTH, 76);
    let body_width = width.saturating_sub(4) as usize;

    let mut lines: Vec<Line> = Vec::new();
    for step in steps.iter().take(ROLLBACK_LIST_LINES) {
        let style = match step.action {
            crate::rollback::Action::Blocked(_) => theme::faint(),
            _ => theme::text(),
        };
        lines.push(Line::from(Span::styled(
            shorten(&step.label(), body_width),
            style,
        )));
    }
    if let Some(rest) = steps.len().checked_sub(ROLLBACK_LIST_LINES).filter(|n| *n > 0) {
        lines.push(Line::from(Span::styled(
            format!("… and {rest} more file(s)"),
            theme::faint(),
        )));
    }

    let blocked = steps.iter().filter(|s| !s.is_actionable()).count();
    if blocked > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{blocked} file(s) cannot be undone and will be left as they are."),
            Style::default().fg(theme::p().warning),
        )));
    }

    if let Some(warning) = warning {
        lines.push(Line::from(""));
        for wrapped in wrap(warning, body_width) {
            lines.push(Line::from(Span::styled(
                wrapped,
                Style::default().fg(theme::p().warning),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Undo these changes?  ", theme::text()),
        Span::styled(
            "  no  ",
            if confirmed {
                theme::faint()
            } else {
                Style::default()
                    .fg(theme::p().accent)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            },
        ),
        Span::styled("  ", theme::text()),
        Span::styled(
            "  yes  ",
            if confirmed {
                Style::default()
                    .fg(theme::p().danger)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                theme::faint()
            },
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "y / n · ←→ to move · Enter to pick · Esc cancels",
        theme::faint(),
    )));

    let popup = centered_rect(width, lines.len() as u16 + 2, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::p().warning))
        .title(Span::styled(
            format!(" Roll back {} file(s) ", steps.len()),
            Style::default()
                .fg(theme::p().warning)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(
        Paragraph::new(lines).block(block).style(theme::text()),
        popup,
    );
}

/// How many lines of a `write_file` preview to show before eliding the rest.
/// A cap, not a limit on the write itself -- the full content still gets
/// written; this only bounds how tall the popup gets.
const WRITE_PREVIEW_LINES: usize = 20;

/// The approval prompt's content, shared by sizing (`render` needs the line
/// count before it can lay out the frame) and drawing. This is the only thing
/// standing between the model and the machine, so a command is shown verbatim
/// and in full -- never elided, never summarised. Approving something you
/// cannot fully see is not approval.
///
/// A file change is shown as a diff of what it does to the file on disk, which
/// is the same principle applied properly rather than a relaxation of it: the
/// decision is about what changes, and the unchanged nine-tenths of a file
/// were never the thing being approved. Only the two bounds in
/// `tools::preview_change` limit it, and both announce themselves ("… N more
/// lines") rather than quietly shortening the answer.
fn tool_approval_lines(
    app: &App,
    request: &ApprovalRequest,
    inner: usize,
) -> (&'static str, Vec<Line<'static>>) {
    let (title, body, footer) = tool_approval_parts(app, request, inner);
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
    request: &ApprovalRequest,
    inner: usize,
) -> (&'static str, Vec<Line<'static>>, Vec<Line<'static>>) {
    let action = &request.action;
    let remaining = request.remaining;
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

    // `deny` is the word under `n`. For most actions "skip" is right -- the
    // model goes on without that one thing. Declining a plan skips nothing;
    // it sends the whole proposal back, so it says so.
    let (title, verb, deny) = match action {
        Action::Publish { path } => {
            for wrapped in wrap(path, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(theme::p().text).add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(""));
            // Said at the point of decision, not afterwards: "public" and
            // "expires" are the two things the answer actually turns on.
            for wrapped in wrap(
                "Uploads these files to a public link anyone can open. It expires after 48 hours.",
                inner,
            ) {
                lines.push(Line::from(Span::styled(wrapped, theme::faint())));
            }
            (" Publish a preview? ", "publish", "skip")
        }
        Action::EnableAuth { path } => {
            for wrapped in wrap(path, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(theme::p().text).add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(""));
            for wrapped in wrap(
                "Stands up a real sign-up/sign-in service for this project on the public internet.",
                inner,
            ) {
                lines.push(Line::from(Span::styled(wrapped, theme::faint())));
            }
            (" Add sign-up/sign-in? ", "provision", "skip")
        }
        Action::DbQuery { path, sql } => {
            for wrapped in wrap(path, inner) {
                lines.push(Line::from(Span::styled(wrapped, theme::faint())));
            }
            lines.push(Line::from(""));
            for wrapped in wrap(sql, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(theme::p().text).add_modifier(Modifier::BOLD),
                )));
            }
            (" Run this against the project's database? ", "run", "skip")
        }
        Action::ListChangeRequests { path } => {
            for wrapped in wrap(path, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(theme::p().text).add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(""));
            for wrapped in wrap(
                "Checks this project's change-request mailbox for pending requests.",
                inner,
            ) {
                lines.push(Line::from(Span::styled(wrapped, theme::faint())));
            }
            (" Check the mailbox? ", "check", "skip")
        }
        Action::ResolveChangeRequest { path, id } => {
            for wrapped in wrap(path, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(theme::p().text).add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(""));
            for wrapped in wrap(&format!("Mark request #{id} resolved."), inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(theme::p().text).add_modifier(Modifier::BOLD),
                )));
            }
            (" Resolve this request? ", "resolve", "skip")
        }
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
            (" Run this command? ", "run", "skip")
        }
        Action::Read { path } => {
            lines.push(Line::from(Span::styled(
                path.clone(),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            (" Read this file? ", "read", "skip")
        }
        Action::Write { path, content } => {
            lines.push(Line::from(Span::styled(
                path.clone(),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            // The diff, when there is one: what this write *changes* is the
            // decision, and for a file that already exists the whole content
                // is mostly the part that stays the same. Falling back to the
            // content listing is not a lesser answer, it is the right one for
            // the case it covers -- a brand new file has no "before" worth
            // drawing a gutter against, and an unreadable path has no diff at
            // all.
            match &request.preview {
                Some(diff) => {
                    lines.push(Line::from(Span::styled(diff.tally(), theme::faint())));
                    lines.extend(diff_lines(diff, inner, 0));
                }
                None if content.is_empty() => {
                    lines.push(Line::from(Span::styled("(empty file)", theme::faint())));
                }
                None => {
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
            }
            (" Write this file? ", "write", "skip")
        }
        Action::List { path } => {
            lines.push(Line::from(Span::styled(
                path.clone(),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            (" List this directory? ", "list", "skip")
        }
        Action::Glob { pattern } => {
            lines.push(Line::from(Span::styled(
                pattern.clone(),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            (" Search for these files? ", "search", "skip")
        }
        Action::Grep { pattern, path } => {
            let scope = path.as_deref().map(|p| format!(" in {p}")).unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!("{pattern}{scope}"),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            (" Search file contents? ", "search", "skip")
        }
        // Not reachable today: a subagent is read-only by construction, so it
        // is auto-approved in both modes. Kept because the match is exhaustive
        // by intent rather than by a catch-all arm -- the same reasoning
        // `plan_mode_block` gives -- and because the task is the whole decision
        // if a mode that asks about it ever exists again.
        Action::Agent { task, .. } => {
            for wrapped in wrap(task, inner) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default()
                        .fg(theme::p().text)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(""));
            for wrapped in wrap(
                "Researches this with read-only tools in a separate conversation and reports back.",
                inner,
            ) {
                lines.push(Line::from(Span::styled(wrapped, theme::faint())));
            }
            (" Spawn a research subagent? ", "spawn", "skip")
        }
        // An edit shows every span, because approving a replacement you cannot
        // see is not approval. Unlike a write it does not need the whole file --
        // showing only what changes is the reason to prefer this tool. A batch
        // is one approval covering all of its spans, so all of them are here.
        Action::Edit { path, edits } => {
            let batch = edits.len() > 1;
            let solo_all = !batch && edits.first().is_some_and(|e| e.replace_all);
            let suffix = if batch {
                format!("  ({} edits, all or none)", edits.len())
            } else if solo_all {
                "  (all occurrences)".to_string()
            } else {
                String::new()
            };
            lines.push(Line::from(Span::styled(
                format!("{path}{suffix}"),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            // A diff of the file as it is now, produced by the very code that
            // will apply the edit, so the popup cannot promise something the
            // runner then does differently. It also answers the question the
            // old replace/with pair never could: *where* in the file this is.
            if let Some(diff) = &request.preview {
                lines.push(Line::from(Span::styled(diff.tally(), theme::faint())));
                lines.extend(diff_lines(diff, inner, 0));
            } else {
                // No diff means the edit could not be resolved against the
                // file -- a path that does not exist, or an `old_string` that
                // does not match. The spans are still shown, because the
                // answer is still a real decision and the model still has to
                // be told one way or the other; it is the runner that reports
                // the mismatch, and it does so in words the model can act on.
                render_edit_spans(&mut lines, edits, batch, inner);
            }
            if batch {
                (" Apply these edits? ", "edit", "skip")
            } else {
                (" Apply this edit? ", "edit", "skip")
            }
        }
        Action::Deploy { provider, production, summary } => {
            lines.push(Line::from(Span::styled(
                format!(
                    "{provider} · {}",
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
            (" Deploy this project? ", "deploy", "skip")
        }
        Action::Search { query, max_results } => {
            lines.push(Line::from(Span::styled(
                query.clone(),
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
            (" Search the web? ", "search", "skip")
        }
        // Neither of these two is ever actually offered a prompt to render --
        // `needs_approval` auto-approves both unconditionally (they touch no
        // file and no network) -- but the match still has to be exhaustive,
        // same reasoning `plan_mode_block`'s own comment gives for the same
        // situation: a catch-all arm here would silently cover whatever
        // writing tool gets added next, which is exactly the mistake this
        // exhaustiveness exists to catch.
        Action::DesignStarter => (" Fetch the design starter? ", "fetch", "skip"),
        Action::CheckContrast { pairs } => {
            lines.push(Line::from(Span::styled(
                format!("{} pair{}", pairs.len(), if pairs.len() == 1 { "" } else { "s" }),
                theme::faint(),
            )));
            (" Check contrast? ", "check", "skip")
        }
        // The plan is shown in full, never capped the way a file preview is.
        // A file preview elides the tail because the file is not the decision
        // -- "write this path" is. Here the text *is* the decision, and the
        // part scrolled out of sight would be exactly the part nobody agreed
        // to. The box scrolls; the plan does not get shortened.
        Action::Plan(proposal) => {
            lines.push(Line::from(Span::styled(
                proposal.title.clone(),
                Style::default()
                    .fg(theme::p().text)
                    .add_modifier(Modifier::BOLD),
            )));

            // Said before the plan, not after it: approving this writes a file
            // into the user's project, and that is part of what they are
            // agreeing to -- not a detail to discover afterwards.
            lines.push(Line::from(Span::styled(
                format!(
                    "saves to {}  ·  {} step{}",
                    crate::plan::PLAN_FILE,
                    proposal.steps.len(),
                    if proposal.steps.len() == 1 { "" } else { "s" }
                ),
                theme::faint(),
            )));

            // There is only one plan file, so approving a *different* plan
            // overwrites whatever is in it. That is the right behaviour -- one
            // project, one plan -- but doing it silently would throw away work
            // the user explicitly agreed to, so the cost is stated up front.
            if let Some(replaced) = app
                .active_plan
                .as_ref()
                .filter(|p| !p.title.trim().eq_ignore_ascii_case(proposal.title.trim()))
            {
                let (done, total) = replaced.progress();
                let progress = if done > 0 {
                    format!(" — {done}/{total} done")
                } else {
                    String::new()
                };
                for wrapped in wrap(
                    &format!("⚠ replaces \"{}\"{progress}", replaced.title),
                    inner,
                ) {
                    lines.push(Line::from(Span::styled(
                        wrapped,
                        Style::default().fg(theme::p().warning),
                    )));
                }
            }
            lines.push(Line::from(""));

            if !proposal.summary.trim().is_empty() {
                for line in proposal.summary.lines() {
                    if line.trim().is_empty() {
                        lines.push(Line::from(""));
                        continue;
                    }
                    for wrapped in wrap(line, inner) {
                        lines.push(Line::from(Span::styled(wrapped, theme::text())));
                    }
                }
                lines.push(Line::from(""));
            }

            // Numbered, because these are the same numbers the model reports
            // progress against -- "step 3 done" has to point at something the
            // user can find.
            for (i, step) in proposal.steps.iter().enumerate() {
                let label = format!("{}. ", i + 1);
                let indent = " ".repeat(label.len());
                for (n, wrapped) in wrap(step, inner.saturating_sub(label.len()).max(1))
                    .into_iter()
                    .enumerate()
                {
                    lines.push(Line::from(vec![
                        Span::styled(
                            if n == 0 { label.clone() } else { indent.clone() },
                            theme::accent(),
                        ),
                        Span::styled(wrapped, theme::text()),
                    ]));
                }
            }

            if !proposal.not_doing.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Not doing", theme::faint())));
                for item in &proposal.not_doing {
                    for wrapped in wrap(item, inner.saturating_sub(2).max(1)) {
                        lines.push(Line::from(Span::styled(
                            format!("- {wrapped}"),
                            theme::muted(),
                        )));
                    }
                }
            }
            (" Start on this plan? ", "start", "revise")
        }
        // Never prompted -- `advance_approvals` records it directly -- but the
        // renderer must stay total, and a panic here would take the UI down
        // over a bookkeeping call.
        Action::Progress { step, done, .. } => {
            lines.push(Line::from(Span::styled(
                format!("step {step} — {}", if *done { "done" } else { "blocked" }),
                theme::text(),
            )));
            (" Record this? ", "record", "skip")
        }
    };

    lines.push(Line::from(""));
    // A search is not scoped to the project directory, and a plan is not an
    // action happening in one at all -- for both, "in <workspace>" would be
    // claiming something untrue about what is being approved.
    if !matches!(action, Action::Search { .. } | Action::Plan(_)) {
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
        Span::styled(format!(" {deny}"), theme::faint()),
    ]));
    // The last two lines built above are the y/n choice; they become the
    // footer, with the key hint appended.
    let split = lines.len().saturating_sub(2);
    let mut footer = lines.split_off(split);
    footer.push(Line::from(Span::styled(
        format!("  ↑↓ choose · enter confirm · esc {deny}"),
        theme::faint(),
    )));

    (title, lines, footer)
}

/// Draws the approval prompt into its reserved region at the bottom of the
/// frame -- see the placement comment on `render`. No `Clear` and no
/// centering: unlike a floating popup, this area belongs to the prompt alone,
/// so there is nothing underneath it to protect or re-center against.
fn render_tool_approval_inline(f: &mut Frame, area: Rect, app: &App, request: &ApprovalRequest) {
    let action = &request.action;
    let inner = area.width.saturating_sub(4).max(1) as usize;
    let (title, body, mut footer) = tool_approval_parts(app, request, inner);

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
    let (_, body, footer) = deployment_parts(app, inner);
    body.into_iter().chain(footer).collect()
}

/// The panel's content, split into a body that may scroll and a footer that
/// never does.
///
/// The viewport is a fixed strip (`VIEWPORT_ROWS` in `main.rs`), so a panel
/// that simply grew would be clipped from the bottom -- taking the spinner and
/// the keys with it, which is exactly the half you need while something is
/// running. So the status line and the keys are pinned, and the checklist and
/// log scroll behind them. Same shape, and the same reasoning, as
/// `tool_approval_parts`.
fn deployment_parts(
    app: &App,
    inner: usize,
) -> (String, Vec<Line<'static>>, Vec<Line<'static>>) {
    let Some(session) = app.deploy.as_ref() else {
        return (String::new(), Vec::new(), Vec::new());
    };
    let mut lines: Vec<Line> = Vec::new();
    let mut footer: Vec<Line> = Vec::new();

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
            // The log scrolls; the spinner does not. While a build runs this
            // line is the whole answer to "is it stuck?", so it is pinned
            // where it cannot be pushed off by its own output.
            let elapsed = session.started.map(|t| t.elapsed()).unwrap_or_default();
            lines.extend(log_lines(session, inner, DEPLOY_LOG_LINES));
            footer.push(Line::from(vec![
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
            footer.push(Line::from(Span::styled(
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
            footer.push(Line::from(Span::styled(
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
                        format!("{} URL", session.target.label()),
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
                        "Next: open the URL to check it, or ask again to ship a change.",
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
            footer.push(Line::from(Span::styled("  enter close", theme::faint())));
        }
    }

    (session.title(), lines, footer)
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
    let (title, body, footer) = deployment_parts(app, inner);

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
        // Stuck to the tail: while a build streams, the newest lines are the
        // ones being read, and a panel anchored at the top would show the
        // beginning of a build forever.
        let scroll = (body.len() as u16).saturating_sub(body_height);
        let body_area = Rect { height: body_height, ..content };
        f.render_widget(Paragraph::new(body).scroll((scroll, 0)), body_area);
    }

    if footer_height > 0 {
        let footer_area = Rect {
            y: content.y + body_height,
            height: footer_height,
            ..content
        };
        f.render_widget(Paragraph::new(footer), footer_area);
    }

    // Only a text prompt has somewhere for the caret to be. Claimed here
    // rather than in `render_input`, which stands down while an overlay is up.
    if let Stage::Prompt(prompt) = &session.stage {
        if !prompt.masked() && body_height > 0 {
            let column = session.input[..session.input_cursor].chars().count();
            let x = content.x + (column.min(inner.saturating_sub(1))) as u16;
            let y = content.y + body_height.saturating_sub(1);
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
/// Two rules keep this honest, and both come from the same place: showing one
/// stray asterisk is a blemish, but silently deleting a character is data loss.
///
/// 1. **A marker only counts as a marker when it is unambiguous.** Emphasis
///    runs must match in length exactly, must not be followed by a space when
///    opening, must not be preceded by one when closing, and `_` never applies
///    inside a word. That is CommonMark's flanking rule, trimmed to the cases
///    that occur in practice. It is what leaves `snake_case`, `*args`,
///    `2 * 3`, and `**/*.rs` exactly as written -- see the tests, which are the
///    real specification here.
/// 2. **A run that fails to match is emitted whole and skipped past**, never
///    re-examined one character at a time. Without that, the leading `**` of
///    `**/*.rs` would fail as bold and then its first `*` would pair with the
///    later lone `*` as italic, eating the `/` between them.
///
/// Parsed *before* wrapping rather than after, so emphasis that spans a line
/// break stays emphasised, and so the width calculation counts what is drawn
/// rather than the markers that will not be. That is the whole reason
/// `wrap_styled` exists instead of reusing `wrap`.
/// One run of characters that share a style, carried through wrapping.
///
/// Per-character rather than per-run because wrapping cuts wherever it needs
/// to, and a run split across a boundary would otherwise have to be re-styled
/// on both sides. They are merged back into as few `Span`s as possible by
/// `to_spans` once the cuts are known.
type Styled = Vec<(char, Style)>;

/// How many of `m` there are in a row starting at `i`.
fn run_len(chars: &[char], i: usize, m: char) -> usize {
    chars[i..].iter().take_while(|&&c| c == m).count()
}

/// Whether a marker run at `i` can *open* emphasis: something non-blank has to
/// follow it, or it is punctuation rather than a marker (`2 * 3`).
///
/// `_` additionally may not open inside a word, which is the single rule that
/// makes `snake_case` and `__init__` survive contact with this renderer.
fn can_open(chars: &[char], i: usize, len: usize, m: char) -> bool {
    match chars.get(i + len) {
        Some(after) if !after.is_whitespace() => {}
        _ => return false,
    }
    if m == '_' {
        if let Some(before) = i.checked_sub(1).and_then(|p| chars.get(p)) {
            if before.is_alphanumeric() {
                return false;
            }
        }
    }
    true
}

/// The mirror of `can_open`: a closer needs something non-blank *before* it,
/// so `*args and *kwargs` finds no pair and stays as typed.
fn can_close(chars: &[char], i: usize, len: usize, m: char) -> bool {
    match i.checked_sub(1).and_then(|p| chars.get(p)) {
        Some(before) if !before.is_whitespace() => {}
        _ => return false,
    }
    if m == '_' {
        if let Some(after) = chars.get(i + len) {
            if after.is_alphanumeric() {
                return false;
            }
        }
    }
    true
}

/// Where the run opened at `i` closes: the *very next* run of `m`, and only if
/// it is exactly `len` long and allowed to close. `None` otherwise, which is
/// what keeps an unmatched marker from swallowing the rest of the line.
///
/// Deliberately stricter than CommonMark, which would keep looking past a
/// mismatched run. That difference is the whole safety margin here. Given
///
/// ```text
/// pass *args through, match **/*.rs
/// ```
///
/// CommonMark skips the `**` and pairs `*args` with the `*` inside `/*.rs`,
/// emphasising half the sentence and deleting two asterisks on the way.
/// Stopping at the first run instead means the mismatch simply ends the
/// search, and the line is printed as written. Real emphasis is a short span
/// with nothing else in it, so the cases this gives up on are vanishingly rare
/// next to the code-ish prose it protects.
fn find_run_close(chars: &[char], from: usize, m: char, len: usize) -> Option<usize> {
    let at = (from..chars.len()).find(|&i| chars[i] == m)?;
    if run_len(chars, at, m) == len && can_close(chars, at, len, m) {
        Some(at)
    } else {
        None
    }
}

/// Parse one logical line's inline markdown into styled characters.
///
/// Everything here is a pair of markers around a span: emphasis (`*`/`_`),
/// strikethrough (`~~`), code (`` ` ``) and links (`[text](url)`). A marker
/// with no partner is text.
fn inline_styled(line: &str, base: Style) -> Styled {
    let palette = theme::p();
    let code_style = Style::default()
        .fg(palette.accent_soft)
        .add_modifier(Modifier::BOLD);
    let link_style = base
        .fg(palette.accent)
        .add_modifier(Modifier::UNDERLINED);
    let url_style = theme::faint();

    let chars: Vec<char> = line.chars().collect();
    let mut out: Styled = Vec::new();
    let mut i = 0;

    let push = |out: &mut Styled, text: &str, style: Style| {
        out.extend(text.chars().map(|c| (c, style)));
    };

    while i < chars.len() {
        let c = chars[i];

        // `\*` is an asterisk the model meant literally. Only punctuation is
        // escapable, so a Windows path (`C:\Users`) keeps its backslash.
        if c == '\\' {
            if let Some(&next) = chars.get(i + 1) {
                if next.is_ascii_punctuation() {
                    out.push((next, base));
                    i += 2;
                    continue;
                }
            }
        }

        // Code first, and before emphasis: inside backticks a `*` is an
        // asterisk, and `` `**` `` has to survive being talked about.
        if c == '`' {
            let len = run_len(&chars, i, '`');
            if let Some(end) = find_backtick_close(&chars, i + len, len) {
                push(
                    &mut out,
                    &chars[i + len..end].iter().collect::<String>(),
                    code_style,
                );
                i = end + len;
                continue;
            }
            push(&mut out, &chars[i..i + len].iter().collect::<String>(), base);
            i += len;
            continue;
        }

        // `[label](url)` -- the label carries the styling, the URL stays
        // visible and dim. A terminal cannot be clicked, so hiding the URL
        // behind the label would lose the only part that can be copied.
        if c == '[' {
            if let Some(rendered) = parse_link(&chars, i) {
                let (label, url, next) = rendered;
                push(&mut out, &label, link_style);
                if !url.is_empty() {
                    push(&mut out, &format!(" ({url})"), url_style);
                }
                i = next;
                continue;
            }
        }

        if c == '*' || c == '_' || c == '~' {
            let len = run_len(&chars, i, c);
            let wanted = match c {
                // `~` is only ever a marker in pairs; a lone one is a home
                // directory or a shell path more often than anything else.
                '~' => Some(2),
                // Underscores emphasise only in singles. `__bold__` is valid
                // markdown and deliberately unsupported: `__init__`,
                // `__main__` and `__all__` are ordinary words in prose about
                // Python, and reading them as bold silently eats four
                // underscores off a name someone may be about to type. Models
                // write `**bold**` anyway, so this gives up almost nothing.
                '_' => Some(1),
                _ => Some(len.min(3)),
            };
            if let Some(wanted) = wanted {
                let close = if len == wanted && can_open(&chars, i, wanted, c) {
                    find_run_close(&chars, i + wanted, c, wanted)
                } else {
                    None
                };
                if let Some(end) = close {
                    let style = match (c, wanted) {
                        ('~', _) => base.add_modifier(Modifier::CROSSED_OUT),
                        (_, 1) => base.add_modifier(Modifier::ITALIC),
                        (_, 2) => base.add_modifier(Modifier::BOLD),
                        _ => base.add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    };
                    // Recursive, so `**bold with `code` inside**` keeps both.
                    let inner: String = chars[i + wanted..end].iter().collect();
                    out.extend(inline_styled(&inner, style));
                    i = end + wanted;
                    continue;
                }
            }
            // Emit the whole run and step past it. Re-examining it one
            // character at a time is what would pair the first `*` of `**/`
            // with a later lone `*` and delete what sits between them.
            push(&mut out, &chars[i..i + len].iter().collect::<String>(), base);
            i += len;
            continue;
        }

        out.push((c, base));
        i += 1;
    }

    out
}

/// The closing backtick run for an opener of `len`. Separate from
/// `find_run_close` because code spans have no flanking rule -- `` ` `` is
/// literal inside them, so the only question is where the fence of the same
/// width is.
fn find_backtick_close(chars: &[char], from: usize, len: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '`' {
            let here = run_len(chars, i, '`');
            if here == len {
                return Some(i);
            }
            i += here;
            continue;
        }
        i += 1;
    }
    None
}

/// `[label](url)` at `i`, as (label, url, index just past the closing paren).
///
/// Returns `None` for anything that is not the whole shape, so a bare `[1]`
/// citation or a Rust slice pattern is left as written.
fn parse_link(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    let close = (i + 1..chars.len()).find(|&j| chars[j] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&j| chars[j] == ')')?;
    let label: String = chars[i + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    if label.is_empty() {
        return None;
    }
    // A label that already *is* the URL would otherwise be printed twice.
    let url = if url == label { String::new() } else { url };
    Some((label, url, end + 1))
}

/// Split styled characters on spaces, keeping each word's styling with it and
/// each separating space's styling with the word that follows.
///
/// The space's own style is carried rather than re-derived from a neighbour.
/// It matters at the edge of a marker: in `~~struck~~ and`, the space belongs
/// to the plain text, and borrowing the style to its left would draw a line
/// through it. Inside `**a phrase**` the space was parsed as bold to begin
/// with, so it stays bold and the whole phrase merges into one span.
fn split_styled_words(input: &Styled) -> Vec<(Option<Style>, Styled)> {
    let mut words: Vec<(Option<Style>, Styled)> = vec![(None, Vec::new())];
    for &(c, style) in input {
        if c == ' ' {
            words.push((Some(style), Vec::new()));
        } else {
            words.last_mut().expect("seeded with one word").1.push((c, style));
        }
    }
    words
}

/// Word-wrap styled text to `width`, measuring what is drawn.
///
/// The plain-text `wrap` cannot be reused: it would count the markers that
/// have already been removed here, wrapping several columns early on any line
/// carrying emphasis.
fn wrap_styled(input: &Styled, width: usize) -> Vec<Styled> {
    let width = width.max(1);
    if input.len() <= width {
        return vec![input.clone()];
    }

    let mut out: Vec<Styled> = Vec::new();
    let mut current: Styled = Vec::new();

    for (space, word) in split_styled_words(input) {
        if !current.is_empty() && current.len() + 1 + word.len() > width {
            out.push(std::mem::take(&mut current));
        }
        if word.len() > width {
            // One unbreakable token wider than the pane -- a URL, a long
            // path. Cut it rather than let it run off the edge.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            for chunk in word.chunks(width) {
                out.push(chunk.to_vec());
            }
            current = out.pop().unwrap_or_default();
            continue;
        }
        if !current.is_empty() {
            current.push((' ', space.unwrap_or_default()));
        }
        current.extend(word);
    }
    out.push(current);
    out
}

/// Merge styled characters back into the fewest `Span`s that describe them.
fn to_spans(chars: &Styled) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buffer = String::new();
    let mut active: Option<Style> = None;

    for &(c, style) in chars {
        if active != Some(style) {
            if let Some(previous) = active {
                if !buffer.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buffer), previous));
                }
            }
            active = Some(style);
        }
        buffer.push(c);
    }
    if let Some(style) = active {
        if !buffer.is_empty() {
            spans.push(Span::styled(buffer, style));
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), Style::default()));
    }
    spans
}

/// What a line's block-level markdown says about how to draw it.
///
/// `MdBlock`, not `Block`: ratatui's own `Block` widget is already imported
/// into this module and drawn all over it.
struct MdBlock {
    /// The line with its block punctuation removed. Still carries inline
    /// markdown, which `inline_styled` handles afterwards.
    text: String,
    /// Drawn before the first row.
    prefix: String,
    /// Drawn before every row after the first, so a wrapped list item lines up
    /// under its own text rather than under its bullet.
    hanging: String,
    prefix_style: Style,
    body_style: Style,
}

impl MdBlock {
    fn plain(text: String, style: Style) -> Self {
        Self {
            text,
            prefix: String::new(),
            hanging: String::new(),
            prefix_style: style,
            body_style: style,
        }
    }
}

/// Strip a line's block-level markdown and say how to draw what is left.
///
/// `- item` becomes `• item` because a bullet is what was meant; `### Heading`
/// loses its hashes and gains bold, because the hashes were standing in for an
/// emphasis the terminal can just apply. Numbered lists keep their numbers --
/// the number is the content there, not punctuation.
fn block_markdown(line: &str, base: Style) -> MdBlock {
    let palette = theme::p();
    let trimmed = line.trim_start();
    let indent = line.chars().count() - trimmed.chars().count();
    let pad = " ".repeat(indent);

    if trimmed.is_empty() {
        return MdBlock::plain(String::new(), base);
    }

    // A rule: three or more of one marker and nothing else.
    if trimmed.len() >= 3
        && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_')
        && trimmed.chars().collect::<std::collections::HashSet<_>>().len() == 1
    {
        return MdBlock::plain(String::new(), base);
    }

    // `> quoted`
    if let Some(rest) = trimmed.strip_prefix('>') {
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        return MdBlock {
            text: rest.to_string(),
            prefix: format!("{pad}│ "),
            hanging: format!("{pad}│ "),
            prefix_style: theme::faint(),
            body_style: base.add_modifier(Modifier::ITALIC),
        };
    }

    // `# Heading`. The level is honoured rather than flattened: a model that
    // bothered to write `#` versus `###` was distinguishing two things.
    if trimmed.starts_with('#') {
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        let rest = &trimmed[hashes..];
        if hashes <= 6 && rest.starts_with(' ') {
            let style = if hashes <= 2 {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                base.add_modifier(Modifier::BOLD)
            };
            return MdBlock::plain(rest.trim_start().to_string(), style);
        }
    }

    // `- [ ] task` / `- [x] done`, checked before plain bullets so the box is
    // not mistaken for the item's first word.
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            if let Some(task) = rest
                .strip_prefix("[ ] ")
                .map(|t| ('\u{2610}', t))
                .or_else(|| rest.strip_prefix("[x] ").map(|t| ('\u{2611}', t)))
                .or_else(|| rest.strip_prefix("[X] ").map(|t| ('\u{2611}', t)))
            {
                let (box_glyph, text) = task;
                return MdBlock {
                    text: text.to_string(),
                    prefix: format!("{pad}{box_glyph} "),
                    hanging: format!("{pad}  "),
                    prefix_style: theme::accent(),
                    body_style: base,
                };
            }
            // Nested bullets get their own glyph, the way a printed list
            // does, so depth is visible without counting spaces.
            let glyph = match indent / 2 {
                0 => '\u{2022}', // •
                1 => '\u{25e6}', // ◦
                _ => '\u{25aa}', // ▪
            };
            return MdBlock {
                text: rest.to_string(),
                prefix: format!("{pad}{glyph} "),
                hanging: format!("{pad}  "),
                prefix_style: theme::accent(),
                body_style: base,
            };
        }
    }

    // `1. item` / `1) item` -- the marker is kept, since the number is the
    // content, but the continuation still hangs under the text.
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let after = &trimmed[digits..];
        for sep in [". ", ") "] {
            if let Some(rest) = after.strip_prefix(sep) {
                let marker = format!("{}{sep}", &trimmed[..digits]);
                let width = marker.chars().count();
                return MdBlock {
                    text: rest.to_string(),
                    prefix: format!("{pad}{marker}"),
                    hanging: format!("{pad}{}", " ".repeat(width)),
                    prefix_style: theme::accent(),
                    body_style: base,
                };
            }
        }
    }

    MdBlock::plain(line.to_string(), base)
}

/// Render a whole message body: block markdown, then inline markdown, then
/// wrapping -- in that order, so nothing is measured with punctuation that
/// will not be drawn, and no span is cut in half by a line break.
///
/// Fenced code blocks are passed through untouched and styled as a unit —
/// inside one, `**` and `#` are code, not formatting, and a wrapped line of
/// code is worse than a clipped one.
fn markdown_lines(body: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_fence = false;
    // Indexed rather than a `for` over the split: a table is several source
    // lines that produce one drawn block, so this loop has to be able to look
    // at the next line and then skip past what it consumed.
    let source: Vec<&str> = body.split('\n').collect();
    let mut i = 0;

    while i < source.len() {
        let logical = source[i];
        i += 1;

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

        // A table, if this line is a row and the next one is its alignment
        // row. Both are required: the alignment row is the only thing that
        // distinguishes a table from a sentence with a pipe in it.
        if logical.contains('|') {
            if let Some(table) = parse_table(&source, i - 1) {
                let consumed = table.consumed;
                lines.extend(render_table(&table, width));
                i = (i - 1) + consumed;
                continue;
            }
        }

        // A horizontal rule has no text to wrap; it is the whole line.
        if is_rule(logical) {
            lines.push(Line::from(Span::styled(
                "\u{2500}".repeat(width.min(48).max(1)),
                theme::faint(),
            )));
            continue;
        }

        let block = block_markdown(logical, theme::text());
        let styled = inline_styled(&block.text, block.body_style);
        let body_width = width
            .saturating_sub(block.prefix.chars().count())
            .max(1);

        for (row, chunk) in wrap_styled(&styled, body_width).into_iter().enumerate() {
            let marker = if row == 0 { &block.prefix } else { &block.hanging };
            let mut spans: Vec<Span<'static>> = Vec::new();
            if !marker.is_empty() {
                spans.push(Span::styled(marker.clone(), block.prefix_style));
            }
            spans.extend(to_spans(&chunk));
            lines.push(Line::from(spans));
        }
    }
    lines
}

// ---- tables --------------------------------------------------------------------

/// How a column's cells line up, taken from the table's alignment row.
#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A GitHub-flavoured pipe table, already split into cells.
struct Table {
    header: Vec<String>,
    align: Vec<Align>,
    rows: Vec<Vec<String>>,
    /// How many source lines it occupied, so the caller can skip them.
    consumed: usize,
}

/// Split one `| a | b |` row into its cells.
///
/// The outer pipes are optional, as in GFM, and `\|` is a literal pipe rather
/// than a cell boundary -- which is the only way to put a shell pipeline or a
/// Rust closure in a table cell.
fn split_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);

    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            current.push('|');
            chars.next();
            continue;
        }
        if c == '|' {
            cells.push(current.trim().to_string());
            current = String::new();
            continue;
        }
        current.push(c);
    }
    cells.push(current.trim().to_string());
    cells
}

/// The `|---|:--:|--:|` row, which is what actually declares a table.
///
/// Every cell must be dashes with optional colons and nothing else, so an
/// ordinary line of prose that happens to contain a pipe and a dash is not
/// mistaken for one.
fn parse_alignment_row(line: &str) -> Option<Vec<Align>> {
    if !line.contains('|') || !line.contains('-') {
        return None;
    }
    let cells = split_table_row(line);
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .map(|cell| {
            let cell = cell.trim();
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
                return None;
            }
            Some(match (left, right) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// A table starting at `start`, or `None` if that is just a line with a pipe.
///
/// The header and alignment rows must agree on how many columns there are.
/// GFM requires that too, and requiring it here is most of what stops a false
/// positive: two unrelated lines rarely both parse *and* match in width.
fn parse_table(source: &[&str], start: usize) -> Option<Table> {
    let align = parse_alignment_row(source.get(start + 1)?)?;
    let header = split_table_row(source[start]);
    if header.len() != align.len() || header.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    let mut i = start + 2;
    while let Some(line) = source.get(i) {
        if !line.contains('|') || line.trim().is_empty() {
            break;
        }
        rows.push(split_table_row(line));
        i += 1;
    }

    Some(Table {
        header,
        align,
        rows,
        consumed: i - start,
    })
}

/// Draw a table, fitted to `width`.
///
/// Column widths come from the *styled* cells, so they are measured in what
/// gets drawn rather than in markdown source -- a header of `**Name**` is four
/// columns wide, not eight. Anything too wide to fit is narrowed a column at a
/// time, widest first, and the cells wrap inside their column rather than the
/// table running off the edge of the pane.
fn render_table(table: &Table, width: usize) -> Vec<Line<'static>> {
    let columns = table.header.len();
    if columns == 0 {
        return Vec::new();
    }

    let border = theme::faint();
    let header_style = Style::default()
        .fg(theme::p().accent)
        .add_modifier(Modifier::BOLD);

    let head: Vec<Styled> = table
        .header
        .iter()
        .map(|cell| inline_styled(cell, header_style))
        .collect();
    let body: Vec<Vec<Styled>> = table
        .rows
        .iter()
        .map(|row| {
            (0..columns)
                .map(|c| {
                    inline_styled(row.get(c).map(String::as_str).unwrap_or(""), theme::text())
                })
                .collect()
        })
        .collect();

    let mut widths: Vec<usize> = (0..columns)
        .map(|c| {
            let widest = body.iter().map(|row| row[c].len()).max().unwrap_or(0);
            head[c].len().max(widest).max(1)
        })
        .collect();

    // `│ ` opening each cell, ` ` closing the last, plus the final `│`.
    let chrome = columns * 3 + 1;
    let budget = width.saturating_sub(chrome).max(columns);
    while widths.iter().sum::<usize>() > budget {
        // Widest first; ties to the leftmost, so the shape settles instead of
        // oscillating between two equal columns.
        let widest = widths
            .iter()
            .enumerate()
            .max_by_key(|&(index, w)| (*w, std::cmp::Reverse(index)))
            .map(|(index, _)| index)
            .expect("columns is non-zero");
        if widths[widest] <= 1 {
            break;
        }
        widths[widest] -= 1;
    }

    let rule = |left: &str, joint: &str, right: &str| {
        let mut drawn = String::from(left);
        for (index, w) in widths.iter().enumerate() {
            if index > 0 {
                drawn.push_str(joint);
            }
            drawn.push_str(&"\u{2500}".repeat(w + 2));
        }
        drawn.push_str(right);
        Line::from(Span::styled(drawn, border))
    };

    let row_lines = |cells: &[Styled]| -> Vec<Line<'static>> {
        let wrapped: Vec<Vec<Styled>> = cells
            .iter()
            .enumerate()
            .map(|(index, cell)| wrap_styled(cell, widths[index]))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);

        (0..height)
            .map(|row| {
                let mut spans = vec![Span::styled("\u{2502}".to_string(), border)];
                for (index, column) in wrapped.iter().enumerate() {
                    let blank: Styled = Vec::new();
                    let chunk = column.get(row).unwrap_or(&blank);
                    let pad = widths[index].saturating_sub(chunk.len());
                    let (before, after) = match table.align[index] {
                        Align::Left => (0, pad),
                        Align::Right => (pad, 0),
                        Align::Center => (pad / 2, pad - pad / 2),
                    };
                    spans.push(Span::raw(format!(" {}", " ".repeat(before))));
                    if !chunk.is_empty() {
                        spans.extend(to_spans(chunk));
                    }
                    spans.push(Span::raw(format!("{} ", " ".repeat(after))));
                    spans.push(Span::styled("\u{2502}".to_string(), border));
                }
                Line::from(spans)
            })
            .collect()
    };

    let mut lines = vec![rule("\u{250c}", "\u{252c}", "\u{2510}")];
    lines.extend(row_lines(&head));
    lines.push(rule("\u{251c}", "\u{253c}", "\u{2524}"));
    for row in &body {
        lines.extend(row_lines(row));
    }
    lines.push(rule("\u{2514}", "\u{2534}", "\u{2518}"));
    lines
}

/// `---`, `***` or `___` on a line of its own.
fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'))
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

    // ---- the /rollback confirmation ----------------------------------------

    fn rollback_overlay(files: usize, warning: Option<&str>) -> App {
        let mut app = App::new(crate::config::Config::default());
        let steps = (0..files)
            .map(|i| crate::rollback::Step {
                display: format!("src/module_{i}.rs"),
                path: std::path::PathBuf::from(format!("/tmp/module_{i}.rs")),
                touches: 1,
                action: if i % 2 == 0 {
                    crate::rollback::Action::Delete
                } else {
                    crate::rollback::Action::Restore("before\n".to_string())
                },
            })
            .collect();
        app.overlay = Some(Overlay::RollbackConfirm {
            steps,
            warning: warning.map(str::to_string),
            confirmed: false,
        });
        app
    }

    /// Every file is named, and both verbs are visible: "12 files" is not
    /// something anyone can agree to, `delete src/api.rs` is.
    #[test]
    fn the_rollback_popup_names_each_file_and_what_happens_to_it() {
        let mut app = rollback_overlay(2, None);
        let rows = rendered_rows(&mut app, 100, 30);
        let all = rows.join("\n");
        assert!(all.contains("module_0.rs"), "{all}");
        assert!(all.contains("module_1.rs"), "{all}");
        assert!(all.contains("delete"), "{all}");
        assert!(all.contains("restore"), "{all}");
        assert!(all.contains("Undo these changes?"), "{all}");
    }

    /// The shell caveat is the reason to say no, so it has to be on screen
    /// next to the question rather than buried in the transcript.
    #[test]
    fn the_rollback_popup_shows_the_shell_warning() {
        let mut app = rollback_overlay(1, Some("1 shell command(s) also ran: npm install"));
        let rows = rendered_rows(&mut app, 100, 30);
        assert!(rows.join("\n").contains("npm install"));
    }

    /// A long list is capped rather than growing the popup off the bottom of
    /// the terminal -- the yes/no line is the one that must always be visible.
    #[test]
    fn a_long_rollback_list_is_capped_and_says_how_many_it_hid() {
        let mut app = rollback_overlay(ROLLBACK_LIST_LINES + 5, None);
        let rows = rendered_rows(&mut app, 100, 40);
        let all = rows.join("\n");
        assert!(all.contains("and 5 more"), "{all}");
        assert!(all.contains("Undo these changes?"), "{all}");
    }

    /// Small terminals must not panic. `Clear` indexes the buffer without
    /// checking, so a popup wider or taller than the frame is a crash, not a
    /// clipped draw.
    #[test]
    fn the_rollback_popup_survives_a_tiny_terminal() {
        for (w, h) in [(20u16, 6u16), (40, 8), (1, 1), (80, 3)] {
            let mut app = rollback_overlay(6, Some("a warning that is quite long indeed"));
            let _ = rendered_rows(&mut app, w, h);
        }
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

    // ---- state marks, not pictographs ------------------------------------

    /// A running tool must look different from a finished one. The count at
    /// the bottom says how many are running; only the mark says *which*.
    #[test]
    fn a_running_tool_spins_where_a_finished_one_sits_still() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::ExecutingTools;
        app.busy_started = Some(std::time::Instant::now());
        app.running_tools = vec![command_call("call_1", "cargo test")];

        let running = rendered_rows(&mut app, 60, 12)
            .into_iter()
            .find(|r| r.contains("cargo test"))
            .expect("the running tool is on screen");
        assert!(
            !running.trim_start().starts_with(theme::TOOL_MARK),
            "a running tool should not wear the settled mark: {running}"
        );
        assert!(
            running.contains('⠋')
                || running.contains('⠙')
                || running.contains('⠹')
                || running.contains('⠸')
                || running.contains('⠼')
                || running.contains('⠴')
                || running.contains('⠦')
                || running.contains('⠧')
                || running.contains('⠇')
                || running.contains('⠏'),
            "expected a spinner frame: {running}"
        );

        // The same call, finished, is drawn with the settled mark instead.
        let done = Message {
            role: Role::Tool,
            content: "ok".into(),
            display: Some("$ cargo test — 805 passed".into()),
            tool_calls: Vec::new(),
            tool_call_id: Some("call_1".into()),
            diff: None,
        };
        let line = &message_lines(&done, 60)[0];
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with(theme::TOOL_MARK), "{text}");
    }

    /// The approval box is the other place icons lived. Its title already says
    /// what is being asked, so the body is the path and nothing else.
    #[test]
    fn an_approval_header_is_the_path_alone() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.workspace_root = "/tmp/project".into();
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest {
            call: Default::default(),
            action: Action::Read { path: "src/config.rs".into() },
            remaining: 0,
            preview: None,
        }));

        let header = rendered_rows(&mut app, 60, 12)
            .into_iter()
            .find(|r| r.contains("src/config.rs"))
            .expect("the path is on screen");
        let body = header.trim_matches(|c| c == '│' || c == ' ');
        assert_eq!(body, "src/config.rs", "expected a bare path, got {header:?}");
    }

    // ---- file-change diffs -----------------------------------------------

    mod diffs {
        use super::*;
        use crate::diff::FileDiff;
        use crate::tools::EditSpan;

        /// An app whose workspace is a real directory holding `files`.
        fn app_on(files: &[(&str, &str)]) -> (App, tempfile::TempDir) {
            let dir = tempfile::tempdir().expect("tempdir");
            for (name, body) in files {
                std::fs::write(dir.path().join(name), body).expect("write fixture");
            }
            let mut app = App::new(crate::config::Config::default());
            app.greeted = true;
            app.workspace_root = dir.path().to_string_lossy().to_string();
            (app, dir)
        }

        fn show(app: &mut App, action: Action, dir: &tempfile::TempDir) {
            let preview = crate::tools::preview_change(&action, dir.path());
            app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest {
                call: Default::default(),
                action,
                remaining: 0,
                preview,
            }));
        }

        /// Rows of the approval box, so an assertion can talk about one line
        /// rather than one long smear of the whole screen.
        fn rows(app: &mut App, w: u16, h: u16) -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
            terminal.draw(|f| render(f, app)).expect("draw");
            let buf = terminal.backend().buffer().clone();
            (0..h)
                .map(|y| {
                    (0..w)
                        .map(|x| buf.get(x, y).symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        }

        fn text_of(lines: &[Line<'static>]) -> Vec<String> {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        }

        const FILE: &str = "fn one() {\n    let a = 1;\n    let b = 2;\n    done(a, b);\n}\n";

        /// The point of the feature: an edit is approved by looking at the
        /// change, in place, with the line numbers it will land on.
        #[test]
        fn an_edit_is_approved_as_a_diff() {
            let (mut app, dir) = app_on(&[("lib.rs", FILE)]);
            show(
                &mut app,
                Action::Edit {
                    path: "lib.rs".into(),
                    edits: vec![EditSpan {
                        old: "    let b = 2;".into(),
                        new: "    let b = 20;\n    let c = 30;".into(),
                        replace_all: false,
                    }],
                },
                &dir,
            );
            let rows = rows(&mut app, 72, 22);
            let joined = rows.join("\n");
            assert!(
                joined.contains("2 additions and 1 removal"),
                "expected a headline count:\n{joined}"
            );
            assert!(
                rows.iter().any(|r| r.contains("3 -     let b = 2;")),
                "expected the old line, numbered and marked:\n{joined}"
            );
            assert!(
                rows.iter().any(|r| r.contains("3 +     let b = 20;")),
                "expected the new line, numbered and marked:\n{joined}"
            );
            assert!(
                rows.iter().any(|r| r.contains("4 +     let c = 30;")),
                "expected the second added line:\n{joined}"
            );
            // The unchanged line above it is what makes the change readable.
            assert!(
                rows.iter().any(|r| r.contains("2       let a = 1;")),
                "expected context around the change:\n{joined}"
            );
            // And the old prose form is gone.
            assert!(
                !joined.contains("replace:"),
                "the replace/with pair should have been superseded:\n{joined}"
            );
        }

        /// Overwriting an existing file is a change, not a new file, and the
        /// part worth reading is the part that differs.
        #[test]
        fn a_write_over_an_existing_file_shows_only_what_differs() {
            let (mut app, dir) = app_on(&[("lib.rs", FILE)]);
            show(
                &mut app,
                Action::Write {
                    path: "lib.rs".into(),
                    content: FILE.replace("done(a, b);", "done(a, b, c);"),
                },
                &dir,
            );
            let rows = rows(&mut app, 72, 22);
            let joined = rows.join("\n");
            assert!(
                rows.iter().any(|r| r.contains("- ") && r.contains("done(a, b);")),
                "expected the replaced line:\n{joined}"
            );
            assert!(
                rows.iter().any(|r| r.contains("+ ") && r.contains("done(a, b, c);")),
                "expected the new line:\n{joined}"
            );
            assert!(
                joined.contains("1 addition and 1 removal"),
                "expected a headline count:\n{joined}"
            );
        }

        /// There is no "before" to diff a brand new file against, so the old
        /// content listing is still the right answer -- and must still appear.
        #[test]
        fn a_brand_new_file_still_shows_its_contents() {
            let (mut app, dir) = app_on(&[]);
            show(
                &mut app,
                Action::Write {
                    path: "fresh.rs".into(),
                    content: "fn fresh() {}\n".into(),
                },
                &dir,
            );
            let joined = rows(&mut app, 72, 16).join("\n");
            assert!(joined.contains("fn fresh() {}"), "{joined}");
        }

        /// An edit that cannot be applied has no diff. The question is still
        /// being asked, so it must still show what it is asking about.
        #[test]
        fn an_unmatchable_edit_falls_back_to_the_spans() {
            let (mut app, dir) = app_on(&[("lib.rs", FILE)]);
            show(
                &mut app,
                Action::Edit {
                    path: "lib.rs".into(),
                    edits: vec![EditSpan {
                        old: "nothing like this is in the file".into(),
                        new: "replacement".into(),
                        replace_all: false,
                    }],
                },
                &dir,
            );
            let joined = rows(&mut app, 72, 20).join("\n");
            assert!(joined.contains("replace:"), "{joined}");
            assert!(joined.contains("nothing like this"), "{joined}");
        }

        /// A finished change is worth seeing too -- "wrote 4kb" and "changed
        /// these three lines" are not the same statement.
        #[test]
        fn a_finished_change_leaves_its_diff_in_the_transcript() {
            let d = crate::diff::diff("a\nb\nc\n", "a\nB\nc\n");
            let msg = Message {
                role: Role::Tool,
                content: "Replaced 1 occurrence(s)".into(),
                display: Some("edit lib.rs — 1 addition and 1 removal".into()),
                tool_calls: Vec::new(),
                tool_call_id: None,
                diff: Some(d),
            };
            let rendered = text_of(&message_lines(&msg, 60));
            assert!(rendered[0].contains("1 addition and 1 removal"), "{rendered:?}");
            assert!(rendered.iter().any(|l| l.contains("2 - b")), "{rendered:?}");
            assert!(rendered.iter().any(|l| l.contains("2 + B")), "{rendered:?}");
        }

        /// A tool result that changed no file must not sprout a gutter.
        #[test]
        fn a_tool_result_without_a_diff_is_unchanged() {
            let msg = Message {
                role: Role::Tool,
                content: "total 4".into(),
                display: Some("list .".into()),
                tool_calls: Vec::new(),
                tool_call_id: None,
                diff: None,
            };
            assert_eq!(text_of(&message_lines(&msg, 60)), vec!["· list ."]);
        }

        /// Colour is how a removal is told from an addition at a glance, so it
        /// is worth a test rather than left to the eye.
        #[test]
        fn additions_and_removals_are_coloured_apart() {
            let d = crate::diff::diff("a\nb\n", "a\nB\n");
            let lines = diff_lines(&d, 40, 0);
            let colour_of = |needle: &str| {
                lines
                    .iter()
                    .find(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                            .contains(needle)
                    })
                    .and_then(|l| l.spans.last())
                    .and_then(|s| s.style.fg)
                    .expect("a coloured span")
            };
            assert_eq!(colour_of("- b"), theme::p().danger);
            assert_eq!(colour_of("+ B"), theme::p().success);
        }

        /// A line wider than the box must still be readable in full, and the
        /// wrap must not look like a second changed line.
        #[test]
        fn a_long_line_wraps_without_repeating_its_marker() {
            let long = "x".repeat(60);
            let d = crate::diff::diff("a\n", &format!("a\n{long}\n"));
            let rendered = text_of(&diff_lines(&d, 30, 0));
            let added: Vec<&String> = rendered.iter().filter(|l| l.contains('+')).collect();
            assert_eq!(added.len(), 1, "one '+' for one added line: {rendered:?}");
            // Nothing of the line is dropped just because it did not fit.
            let carried = rendered.iter().map(|r| r.matches('x').count()).sum::<usize>();
            assert_eq!(carried, long.len(), "the whole line survives: {rendered:?}");
        }

        /// Tabs are expanded so the gutter and the code agree about columns.
        #[test]
        fn tabs_become_spaces() {
            let d = crate::diff::diff("a\n", "a\n\tindented\n");
            let rendered = text_of(&diff_lines(&d, 40, 0));
            assert!(
                rendered.iter().any(|l| l.ends_with("+     indented")),
                "{rendered:?}"
            );
        }

        /// A clipped diff must say so, or a shortened change passes for a
        /// whole one.
        #[test]
        fn a_clipped_diff_says_how_much_is_missing() {
            let old: String = (1..=60).map(|i| format!("line {i}\n")).collect();
            let new: String = (1..=60).map(|i| format!("changed {i}\n")).collect();
            let d = crate::diff::diff(&old, &new).clipped(10);
            let rendered = text_of(&diff_lines(&d, 60, 0));
            assert!(
                rendered.last().expect("a last line").contains("more lines"),
                "{rendered:?}"
            );
        }



        /// Nothing to draw is nothing to draw -- not a stray blank gutter.
        #[test]
        fn an_empty_diff_draws_nothing() {
            assert!(diff_lines(&FileDiff::default(), 40, 0).is_empty());
        }
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
        assert!(shown.contains(theme::MASCOT[2]), "{shown}");

        // The promise about what this will do to the machine has to match the
        // setting. Stating the stricter one to whoever is not in it would be a
        // safety claim that is simply untrue.
        assert!(shown.contains("Destructive commands wait for your approval"), "{shown}");
        app.config.tools.approval = crate::config::ApprovalMode::Always;
        let strict = welcome_text(&app, 96);
        assert!(strict.contains("Every command and every write waits"), "{strict}");
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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Command {
                command: "ls -la".to_string(),
                purpose: None,
            },
            remaining: 0,
            preview: None,
        }));

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

        // Two "❯"s are expected on screen at once: the menu's cursor and the
        // input box's own separate prompt marker (visible around the typed
        // "/") -- so this looks specifically for the menu's, not just any.
        let (first_name, first_desc) = crate::app::COMMANDS[0];
        let highlighted_row = rows
            .iter()
            .find(|r| r.contains('❯') && r.contains(first_name))
            .unwrap_or_else(|| {
                panic!("the menu's cursor should be on {first_name}, the first match")
            });
        assert!(highlighted_row.contains(first_desc), "{highlighted_row}");
    }

    /// The viewport is twelve rows, so the menu cannot always show every
    /// command at once. Arrowing onto one past the fold must scroll it into
    /// view -- moving a cursor onto an entry that is never drawn reads as the
    /// menu being stuck.
    #[test]
    fn the_command_menu_scrolls_to_keep_the_selection_visible() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.input_buffer = "/".to_string();
        app.cursor = 1;

        let last = crate::app::COMMANDS.len() - 1;
        let (last_name, last_desc) = crate::app::COMMANDS[last];
        app.command_menu_selected = last;

        let rows = rendered_rows(&mut app, 80, 24);
        let highlighted_row = rows
            .iter()
            .find(|r| r.contains('❯') && r.contains(last_name))
            .unwrap_or_else(|| panic!("{last_name} should have scrolled into view"));
        assert!(highlighted_row.contains(last_desc), "{highlighted_row}");

        // Every command is reachable this way, one selection at a time.
        for (i, (name, _)) in crate::app::COMMANDS.iter().enumerate() {
            app.command_menu_selected = i;
            let joined = rendered_rows(&mut app, 80, 24).concat();
            assert!(joined.contains(name), "{name} unreachable at selection {i}");
        }
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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Command { command: "ls -la".to_string(), purpose: None },
            remaining: 0,
            preview: None,
        }));

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

        // The request is out and nothing has come back: waiting is all that
        // can honestly be claimed. "Thinking" is reserved for the state where
        // reasoning is actually arriving.
        let rendered = rendered_text(&mut app, 80, 24);
        assert!(rendered.contains("Waiting…"), "{rendered}");
        assert!(rendered.contains("esc to interrupt"), "{rendered}");

        // Idle shows none of it.
        app.state = AppState::AwaitingInput;
        app.busy_started = None;
        let idle = rendered_text(&mut app, 80, 24);
        assert!(!idle.contains("esc to interrupt"), "{idle}");
    }

    /// The bug this whole change exists for: a reasoning model streams its
    /// chain of thought before a word of the answer, and with nothing on
    /// screen the app was indistinguishable from hung.
    #[test]
    fn reasoning_shows_as_thinking_with_the_thought_underneath() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::Streaming;
        app.busy_started = Some(std::time::Instant::now());
        app.append_reasoning("First I need to check what the scaffold generated.\nThe entry point is main.jsx");

        let rendered = rendered_text(&mut app, 80, 24);
        assert!(rendered.contains("Thinking…"), "{rendered}");
        assert!(
            rendered.contains("The entry point is main.jsx"),
            "the latest thought should be on screen: {rendered}"
        );

        // The answer starting is what ends it: a thought left standing under a
        // reply that has moved on reads as still deliberating.
        app.append_token("Here is the app.");
        let answering = rendered_text(&mut app, 80, 24);
        assert!(answering.contains("Responding…"), "{answering}");
        assert!(!answering.contains("The entry point"), "{answering}");
    }

    /// Two clocks, because they answer different questions. The turn total is
    /// shown only once it has diverged from the round in flight -- on a plain
    /// question the two are the same number, and printing it twice is noise.
    #[test]
    fn the_turn_total_is_shown_separately_from_the_round_in_flight() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::Streaming;
        app.busy_started =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(152));
        app.request_started =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(12));

        let rendered = rendered_text(&mut app, 100, 24);
        assert!(rendered.contains("(12s"), "the round in flight: {rendered}");
        assert!(rendered.contains("152s this turn"), "the turn total: {rendered}");

        // A first round, where they agree, says it once.
        app.busy_started = app.request_started;
        let single = rendered_text(&mut app, 100, 24);
        assert!(!single.contains("this turn"), "{single}");
    }


    /// The end of the same bug: rendering an overlay into a zero-cell frame.
    #[test]
    fn rendering_an_overlay_into_a_zero_size_frame_does_not_panic() {
        let mut app = App::new(crate::config::Config::default());
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Command {
                command: "rm -rf /".to_string(),
                purpose: Some("something alarming".to_string()),
            },
            remaining: 2,
            preview: None,
        }));

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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Command {
                command: "rm -rf build".to_string(),
                purpose: Some("clear stale output".to_string()),
            },
            remaining: 0,
            preview: None,
        }));

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
            app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(), action, remaining: 0, preview: None }));

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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Write {
                path: "big.txt".into(),
                content: (1..=200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
            },
            remaining: 0,
            preview: None,
        }));

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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Write {
                path: "big.txt".into(),
                content: (1..=40).map(|i| format!("marker{i}")).collect::<Vec<_>>().join("\n"),
            },
            remaining: 0,
            preview: None,
        }));

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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Command {
                command: "rm -rf build".to_string(),
                purpose: None,
            },
            remaining: 0,
            preview: None,
        }));

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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Write {
                path: "hello.py".to_string(),
                content: "print('hi')\n".to_string(),
            },
            remaining: 0,
            preview: None,
        }));

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
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Search {
                query: "rust async runtime comparison".to_string(),
                max_results: 5,
            },
            remaining: 0,
            preview: None,
        }));

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

    /// Emphasis has to survive a wrap. Parsing per already-wrapped row -- the
    /// obvious implementation -- leaves the opener on one row and the closer
    /// on the next, so both print as literal asterisks and the phrase loses
    /// its emphasis exactly when it is long enough to need it.
    #[test]
    fn bold_spanning_a_line_break_stays_bold_on_both_rows() {
        let width = 40;
        let lines = markdown_lines(
            "A very long **bold phrase that certainly runs past the wrap boundary** here.",
            width,
        );
        let bolded: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.to_string())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(bolded.contains("bold phrase"), "start not bold: {bolded:?}");
        assert!(bolded.contains("wrap boundary"), "end not bold: {bolded:?}");
        let joined: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!joined.contains('*'), "markers reached the screen: {joined}");
    }

    /// Regression: a live session had the model claim "I'm about halfway
    /// through... Prepared the db_query calls to create the tables" across
    /// several turns with zero tool calls behind any of it -- indistinguishable
    /// on screen from a turn that did real work, confirmed only by hand-reading
    /// the raw session log afterward. This is the fix: the gap between "said it
    /// happened" and "actually happened" must be visible on the message itself,
    /// not just inferable from a missing `· $` line a few messages later.
    #[test]
    fn a_reply_with_no_tool_calls_is_marked_as_such() {
        let msg = Message::new(
            Role::Assistant,
            "I'm about halfway through the implementation. Prepared the db_query calls \
             to create the tables.",
        );
        let joined: String = message_lines(&msg, 80)
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(joined.contains("no tool call this turn"), "{joined:?}");
    }

    /// The same reply, this time backed by a real tool call, must not carry
    /// the marker -- it would be false on the one turn where something
    /// actually did happen, exactly the kind of noise that trains people to
    /// stop reading it.
    #[test]
    fn a_reply_with_a_real_tool_call_is_not_marked() {
        let mut msg = Message::new(Role::Assistant, "Creating the tables now.");
        msg.tool_calls = vec![crate::llm::ToolCall::default()];
        let joined: String = message_lines(&msg, 80)
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!joined.contains("no tool call"), "{joined:?}");
    }

    /// The markers are removed before wrapping, so the width has to be spent
    /// on text. Measuring the raw source instead wraps several columns early
    /// on every line carrying emphasis.
    #[test]
    fn wrapping_measures_what_is_drawn_not_the_markers() {
        let width = 30;
        for line in markdown_lines("**aaaa** **bbbb** **cccc** **dddd** **eeee**", width) {
            let drawn: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(drawn <= width, "row of {drawn} exceeds {width}");
        }
        // 5 words of 4 plus 4 spaces is 24 columns once the markers are gone,
        // so it fits on one row. Counting the 20 asterisks would split it.
        assert_eq!(
            markdown_lines("**aaaa** **bbbb** **cccc** **dddd** **eeee**", width).len(),
            1
        );
    }

    #[test]
    fn italics_render_from_either_marker() {
        for body in ["a *word* here", "a _word_ here"] {
            let lines = markdown_lines(body, 60);
            let italic: String = lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .filter(|s| s.style.add_modifier.contains(Modifier::ITALIC))
                .map(|s| s.content.to_string())
                .collect();
            assert_eq!(italic, "word", "{body:?} did not italicise");
        }
    }

    #[test]
    fn strikethrough_and_bold_italic_render() {
        let spans: Vec<_> = markdown_lines("~~gone~~ and ***loud***", 60)
            .into_iter()
            .flat_map(|line| line.spans)
            .collect();
        let struck = spans
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(struck.map(|s| s.content.as_ref()), Some("gone"));
        let loud = spans
            .iter()
            .find(|s| {
                s.style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::ITALIC)
            });
        assert_eq!(loud.map(|s| s.content.as_ref()), Some("loud"));
    }

    /// A wrapped list item lines up under its own text. Without a hanging
    /// indent the continuation starts under the bullet and reads as a new
    /// item.
    #[test]
    fn a_wrapped_list_item_hangs_under_its_own_text() {
        let lines = markdown_lines(
            "1. a numbered step long enough that it certainly has to wrap somewhere",
            30,
        );
        let rows: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(rows.len() > 1, "expected a wrap: {rows:?}");
        assert!(rows[0].starts_with("1. "), "{rows:?}");
        assert!(rows[1].starts_with("   "), "continuation not hung: {rows:?}");
    }

    #[test]
    fn blockquotes_rules_and_tasks_render_as_themselves() {
        let joined: String = markdown_lines("> quoted\n\n---\n\n- [ ] todo\n- [x] done", 60)
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(joined.contains('│'), "no quote marker: {joined}");
        assert!(joined.contains('─'), "no rule: {joined}");
        assert!(joined.contains('☐') && joined.contains('☑'), "{joined}");
        assert!(!joined.contains("[ ]") && !joined.contains("[x]"), "{joined}");
    }

    // ---- tables ------------------------------------------------------------

    fn table_rows(body: &str, width: usize) -> Vec<String> {
        markdown_lines(body, width)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    const TABLE: &str = "\
| Command | What it does | Cost |
|---------|:------------:|-----:|
| `/new` | Forget it | 0 |
| `/compact` | Summarise and **keep** the gist | 1 request |";

    #[test]
    fn a_pipe_table_is_drawn_as_a_table() {
        let rows = table_rows(TABLE, 78);
        let joined = rows.join("\n");

        assert!(joined.contains('┌') && joined.contains('┐'), "{joined}");
        assert!(joined.contains('├') && joined.contains('┼'), "{joined}");
        assert!(joined.contains('└') && joined.contains('┘'), "{joined}");
        assert!(joined.contains("Command") && joined.contains("1 request"), "{joined}");
        // The alignment row is punctuation for a renderer, not content.
        assert!(!joined.contains("---"), "the alignment row was drawn: {joined}");
        // Every drawn row is one rectangle: same width, top to bottom.
        let widths: Vec<usize> = rows
            .iter()
            .filter(|r| r.starts_with('┌') || r.starts_with('│') || r.starts_with('├') || r.starts_with('└'))
            .map(|r| r.chars().count())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "ragged table: {widths:?}");
    }

    /// Column widths are measured on what is drawn. A header of `**Name**` is
    /// four columns wide, not eight, and cells keep their emphasis.
    #[test]
    fn table_cells_keep_their_inline_markdown() {
        let spans: Vec<_> = markdown_lines(TABLE, 78)
            .into_iter()
            .flat_map(|line| line.spans)
            .collect();
        assert!(
            spans.iter().any(|s| s.content.contains("keep")
                && s.style.add_modifier.contains(Modifier::BOLD)),
            "bold inside a cell was lost"
        );
        let joined: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!joined.contains('*') && !joined.contains('`'), "markers drawn: {joined}");
    }

    /// A table too wide for the pane narrows and wraps inside its columns.
    /// Letting it run off the edge would clip the last column entirely.
    #[test]
    fn a_wide_table_is_squeezed_to_fit_rather_than_clipped() {
        for width in [70, 46, 30] {
            for row in table_rows(TABLE, width) {
                assert!(
                    row.chars().count() <= width,
                    "row of {} exceeds {width}: {row}",
                    row.chars().count()
                );
            }
        }
    }

    /// The alignment row is the only thing that makes a table a table. Prose
    /// about shell pipelines is full of pipes and must stay prose.
    #[test]
    fn a_pipe_in_prose_is_not_a_table() {
        let joined = table_rows("run `ls | wc -l` to count them", 70).join("\n");
        assert!(joined.contains("ls | wc -l"), "{joined}");
        assert!(!joined.contains('┌'), "prose became a table: {joined}");
    }

    /// `\|` is how a cell carries a pipe of its own -- a shell pipeline or a
    /// closure -- without ending the cell.
    #[test]
    fn an_escaped_pipe_stays_inside_its_cell() {
        let joined = table_rows("| Cmd | Note |\n|---|---|\n| a \\| b | two |", 60).join("\n");
        assert!(joined.contains("a | b"), "{joined}");
        assert!(joined.contains("two"), "{joined}");
    }

    /// The URL is the only part of a link a terminal can do anything with, so
    /// it stays visible; the brackets do not.
    #[test]
    fn a_link_keeps_its_label_and_its_url() {
        let joined: String = markdown_lines("See [the docs](https://boxcode.sh) now.", 70)
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(joined.contains("the docs"), "{joined}");
        assert!(joined.contains("https://boxcode.sh"), "{joined}");
        assert!(!joined.contains('['), "{joined}");
    }

    /// `\*` is an asterisk the model meant literally.
    #[test]
    fn an_escaped_marker_is_printed_as_itself() {
        let joined: String = markdown_lines(r"a \*literal\* pair", 60)
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert_eq!(joined, "a *literal* pair");
    }

    /// `snake_case`, globs and `*args` are ordinary in prose about code, and a
    /// false italic there silently deletes characters.
    ///
    /// The last two are the ones that matter: they sit on one line together,
    /// which is what makes a CommonMark-faithful parser pair `*args` with the
    /// `*` inside `/*.rs` and eat everything between. See `find_run_close`.
    #[test]
    fn underscores_and_lone_asterisks_in_prose_are_untouched() {
        for body in [
            "use snake_case here",
            "pass *args through",
            "match **/*.rs",
            "pass *args through, match **/*.rs, and 2 * 3 is 6",
            "__init__ and __main__ are dunders",
        ] {
            let joined = assistant_rows(body).concat();
            let stripped: String = body.chars().filter(|c| !c.is_whitespace()).collect();
            let seen: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(seen.contains(&stripped), "{body:?} was mangled into {joined:?}");
        }
    }

    // ---- a prompt taller than its box --------------------------------------

    /// A prompt long enough to overflow the box, as separate lines so the
    /// row a given line lands on is predictable.
    fn long_prompt(lines: usize) -> String {
        (1..=lines)
            .map(|n| format!("line {n:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Type a long prompt, put the cursor on its last line, and draw.
    fn input_rows_with_cursor_at_end(lines: usize) -> Vec<String> {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        let text = long_prompt(lines);
        app.cursor = text.len();
        app.input_buffer = text;
        rendered_rows(&mut app, 60, 24)
    }

    /// The box stops growing at `MAX_INPUT_HEIGHT`, so anything longer used to
    /// keep the first few lines pinned on screen while everything typed after
    /// them happened below the fold -- with the cursor stuck on the last
    /// visible row, not moving. The view has to follow the cursor.
    #[test]
    fn a_prompt_taller_than_the_box_scrolls_to_the_cursor() {
        let rows = input_rows_with_cursor_at_end(30);
        let joined = rows.join("\n");

        assert!(
            joined.contains("line 30"),
            "the end of the prompt, where the cursor is, is off screen:\n{joined}"
        );
        assert!(
            !joined.contains("line 01"),
            "the box did not scroll; it is still showing the top:\n{joined}"
        );
    }

    /// A prompt that fits must not scroll: pinning it to the bottom would make
    /// a two-line prompt jump around inside a box with room to spare.
    #[test]
    fn a_prompt_that_fits_is_not_scrolled() {
        let joined = input_rows_with_cursor_at_end(3).join("\n");
        for expected in ["line 01", "line 02", "line 03"] {
            assert!(joined.contains(expected), "{expected} missing:\n{joined}");
        }
    }

    /// Walking back up a long prompt has to bring the earlier lines back into
    /// view, or ↑ moves a cursor nobody can see.
    ///
    /// This guards the opposite mistake to the test above: the window must
    /// follow the cursor in both directions, not simply pin itself to the end
    /// of the buffer. Fixing the first bug by always scrolling to the bottom
    /// would pass that test and fail this one.
    #[test]
    fn moving_the_cursor_up_a_long_prompt_scrolls_back() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        let text = long_prompt(30);
        app.cursor = text.len();
        app.input_buffer = text;
        for _ in 0..29 {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Up,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let joined = rendered_rows(&mut app, 60, 24).join("\n");
        assert!(
            joined.contains("line 01"),
            "the top of the prompt never came back:\n{joined}"
        );
        assert!(
            !joined.contains("line 30"),
            "the window stayed pinned to the end instead of following the cursor:\n{joined}"
        );
    }

    /// The viewport is a fixed strip (`VIEWPORT_ROWS` in `main.rs`), so the
    /// panel cannot grow its way out of trouble: anything past the bottom is
    /// simply not drawn. The status line and the keys are pinned for exactly
    /// that reason -- they are the half you need while something is running.
    #[test]
    fn the_deployment_panel_keeps_its_status_line_inside_a_short_viewport() {
        use crate::deploy::service::{tests_support, Step, StepLine};

        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.workspace_root = "/tmp".to_string();
        let mut session = tests_support::session("netlify");
        session.stage = Stage::Working(Step::Deploying);
        session.started = Some(std::time::Instant::now());
        // Far more content than the strip can hold, in both halves.
        for i in 0..20 {
            session.steps.push(StepLine {
                label: format!("finished step {i}"),
                state: StepState::Done,
            });
            session.log.push_back(format!("building chunk {i}"));
        }
        app.deploy = Some(session);
        app.overlay = Some(Overlay::Deploy);

        // The height `main.rs` actually gives it.
        let mut terminal = Terminal::with_options(
            TestBackend::new(76, 12),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(12),
            },
        )
        .unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf.get(x, y).symbol().to_string()).collect())
            .collect();
        let joined = rows.concat();

        assert!(joined.contains("Building and uploading"), "the status line must survive: {rows:?}");
        assert!(joined.contains("esc to stop"), "so must the way out: {rows:?}");
        // The tail of the log, not its beginning -- the newest lines are the
        // ones being read.
        assert!(joined.contains("building chunk 19"), "{rows:?}");
        assert!(!joined.contains("building chunk 0 "), "it should have scrolled past: {rows:?}");
    }

    /// The confirmation has to be on screen, or the first Ctrl-C looks like
    /// it did nothing and the second is a surprise.
    #[test]
    fn a_pending_quit_says_so_in_the_key_bar() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        assert!(!rendered_rows(&mut app, 80, 24).concat().contains("press again to quit"));

        app.request_quit();
        let shown = rendered_rows(&mut app, 80, 24).concat();
        assert!(shown.contains("press again to quit"), "{shown}");
    }

    /// It has to win over the other key-bar states too: a quit is pending
    /// whether or not an approval happens to be open.
    #[test]
    fn the_pending_quit_hint_outranks_the_busy_key_bar() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = crate::app::AppState::Streaming;
        app.request_quit();
        let shown = rendered_rows(&mut app, 80, 24).concat();
        assert!(shown.contains("press again to quit"), "{shown}");
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


    // ---- plan mode ---------------------------------------------------------

    /// The plan is the thing being approved, so it is shown whole. A file
    /// preview may elide its tail because the path is the decision; here the
    /// elided part would be exactly what nobody agreed to.
    #[test]
    fn a_plan_prompt_shows_the_whole_plan_and_offers_start_or_revise() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.mode = crate::tools::Mode::Plan;
        app.state = AppState::AwaitingApproval;
        app.workspace_root = "/tmp/project".to_string();

        let steps: Vec<String> = (1..=30)
            .map(|i| format!("step {i}: change file_{i}.rs"))
            .collect();
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Plan(crate::tools::Proposal {
                title: "Rate limiting for the items API".to_string(),
                summary: "Fixed window, keyed by API key.".to_string(),
                steps,
                not_doing: vec!["Distributed limiting".to_string()],
            }),
            remaining: 0,
            preview: None,
        }));

        let rows = rendered_rows(&mut app, 80, 40);
        let joined = rows.concat();

        assert!(joined.contains("Start on this plan?"), "{joined}");
        assert!(joined.contains("step 1:"), "{joined}");
        assert!(
            !joined.contains("more line"),
            "a plan must never be elided the way a file preview is: {joined}"
        );

        // "skip" is the wrong word for declining a whole proposal.
        assert!(joined.contains("y start"), "{joined}");
        assert!(joined.contains("n revise"), "{joined}");
        assert!(!joined.contains("n skip"), "{joined}");

        // A plan does not happen "in" a directory the way a write does.
        assert!(!joined.contains("in /tmp/project"), "{joined}");
    }

    /// The line announcing plan mode scrolls away within a few exchanges. What
    /// the next keystroke can do must be legible on every frame after that.
    #[test]
    fn plan_mode_is_stated_on_every_frame() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;

        let normal = rendered_rows(&mut app, 80, 24).concat();
        assert!(!normal.contains("PLAN"), "{normal}");

        app.mode = crate::tools::Mode::Plan;
        let planning = rendered_rows(&mut app, 80, 24).concat();
        assert!(planning.contains("PLAN"), "{planning}");
    }

    fn a_plan(title: &str, steps: &[&str], done: &[usize]) -> crate::plan::Plan {
        let mut plan = crate::plan::Plan {
            title: title.to_string(),
            summary: "Fixed window.".to_string(),
            steps: steps.iter().map(|s| crate::plan::Step::new(*s)).collect(),
            not_doing: Vec::new(),
            created: "2026-08-11".to_string(),
            updated: "2026-08-11".to_string(),
            base_commit: None,
            model: "m".to_string(),
            path: std::path::PathBuf::from("/tmp/project/plan.md"),
        };
        for &i in done {
            plan.mark(i, true, None).unwrap();
        }
        plan
    }

    /// There is one plan file, so approving a different plan overwrites it.
    /// That is intended, but doing it silently would throw away work the user
    /// agreed to -- so the cost is on screen before they can press y.
    #[test]
    fn a_plan_that_replaces_another_says_what_it_costs() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::AwaitingApproval;
        app.workspace_root = "/tmp/project".to_string();
        app.active_plan = Some(a_plan("Rate limiting", &["one", "two", "three", "four"], &[1, 2]));

        let proposal = crate::tools::Proposal {
            title: "Refactor auth".to_string(),
            summary: "Different work entirely.".to_string(),
            steps: vec!["Move to refresh tokens".to_string()],
            not_doing: Vec::new(),
        };
        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Plan(proposal.clone()),
            remaining: 0,
            preview: None,
        }));

        let joined = rendered_rows(&mut app, 80, 30).concat();
        assert!(joined.contains("saves to plan.md"), "{joined}");
        assert!(joined.contains("replaces"), "{joined}");
        assert!(joined.contains("Rate limiting"), "{joined}");
        assert!(joined.contains("2/4 done"), "the cost is the unfinished work: {joined}");
    }

    /// Revising the plan already in hand is not a replacement, and warning
    /// about it every time would train the warning out of meaning anything.
    #[test]
    fn revising_the_same_plan_is_not_flagged_as_a_replacement() {
        let mut app = App::new(crate::config::Config::default());
        app.greeted = true;
        app.state = AppState::AwaitingApproval;
        app.workspace_root = "/tmp/project".to_string();
        app.active_plan = Some(a_plan("Rate limiting", &["one", "two"], &[]));

        app.overlay = Some(Overlay::ToolApproval(crate::approval::ApprovalRequest { call: Default::default(),
            action: Action::Plan(crate::tools::Proposal {
                title: "Rate limiting".to_string(),
                summary: "Reworked.".to_string(),
                steps: vec!["A better first step".to_string()],
                not_doing: Vec::new(),
            }),
            remaining: 0,
            preview: None,
        }));

        let joined = rendered_rows(&mut app, 80, 30).concat();
        assert!(!joined.contains("replaces"), "{joined}");
    }

    /// The model follows the project's plan from the very first prompt.
    /// Finding that out by watching it start editing files would be a nasty
    /// surprise, so the welcome panel says so on the way in.
    #[test]
    fn the_welcome_panel_names_the_plan_already_in_the_project() {
        let mut app = App::new(crate::config::Config::default());
        app.active_plan = Some(a_plan(
            "Rate limiting for the items API",
            &["Add the limiter", "Wrap the router", "Add settings"],
            &[1],
        ));

        let panel = welcome_text(&app, 80);
        assert!(panel.contains("1/3"), "{panel}");
        assert!(panel.contains("Rate limiting"), "{panel}");
        assert!(panel.contains("Wrap the router"), "the next step: {panel}");

        // And nothing about a plan when the project has none.
        let bare = App::new(crate::config::Config::default());
        assert!(!welcome_text(&bare, 80).contains("next"), "no plan, no rows");
    }

    #[test]
    fn a_plan_notice_is_shown_before_anything_is_typed() {
        let mut app = App::new(crate::config::Config::default());
        app.startup_notices.push("plan.md was written against commit 3c21dfb".to_string());

        let panel = welcome_text(&app, 80);
        assert!(panel.contains("Before you start"), "{panel}");
        assert!(panel.contains("3c21dfb"), "{panel}");
    }
}


