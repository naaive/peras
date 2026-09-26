//! Rendering of a [`Model`] into a ratatui frame. Stateless: everything comes
//! from the model.

use crate::model::{Entry, InputMode, Model};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Height of the input box (with borders).
const INPUT_HEIGHT: u16 = 3;
/// Height of the approval panel (with borders).
const APPROVAL_HEIGHT: u16 = 4;

/// The screen areas for a given terminal size.
pub struct Areas {
    pub transcript: Rect,
    pub approval: Option<Rect>,
    pub input: Rect,
    pub status: Rect,
}

pub fn areas(model: &Model, area: Rect) -> Areas {
    let approval = if model.pending.is_empty() { 0 } else { APPROVAL_HEIGHT };
    let [transcript, approval_area, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(approval),
        Constraint::Length(INPUT_HEIGHT),
        Constraint::Length(1),
    ])
    .areas(area);
    Areas { transcript, approval: (approval > 0).then_some(approval_area), input, status }
}

/// Draw the whole screen.
pub fn render(frame: &mut Frame, model: &Model) {
    let a = areas(model, frame.area());
    render_transcript(frame, model, a.transcript);
    if let Some(area) = a.approval {
        render_approval(frame, model, area);
    }
    render_input(frame, model, a.input);
    render_status(frame, model, a.status);
}

fn render_transcript(frame: &mut Frame, model: &Model, area: Rect) {
    let lines = transcript_lines(model, area.width.max(1) as usize);
    let height = area.height as usize;
    let visible = visible_window(lines.len(), height, model.scroll);
    let shown: Vec<Line> = lines[visible].to_vec();
    frame.render_widget(Paragraph::new(shown), area);
}

/// The slice of `total` lines shown in a viewport of `height`, scrolled up
/// `scroll` lines from the bottom (clamped).
pub fn visible_window(total: usize, height: usize, scroll: usize) -> std::ops::Range<usize> {
    let max_scroll = total.saturating_sub(height);
    let scroll = scroll.min(max_scroll);
    let end = total - scroll;
    end.saturating_sub(height)..end
}

/// The transcript as display lines, wrapped to `width`.
pub fn transcript_lines(model: &Model, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![];
    for e in &model.transcript {
        match e {
            Entry::User(text) => push_wrapped(&mut out, "> ", text, Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD), width),
            Entry::Assistant { text, live, .. } => {
                let mut t = text.clone();
                if *live {
                    t.push_str(" _");
                }
                push_wrapped(&mut out, "", &t, Style::new(), width)
            }
            Entry::Tool { name, args, progress, result, .. } => {
                let (mark, style, tail) = match (result, progress) {
                    (Some(r), _) if r.is_error => ("x", Style::new().fg(Color::Red), r.text.clone()),
                    (Some(r), _) => ("+", Style::new().fg(Color::Green), r.text.clone()),
                    (None, Some(p)) => ("~", Style::new().fg(Color::Yellow), p.clone()),
                    (None, None) => ("~", Style::new().fg(Color::Yellow), "running".into()),
                };
                let line = format!("{mark} {name}({args}) -> {tail}");
                push_wrapped(&mut out, "", &truncate(&line, width), style, width);
            }
            Entry::Notice(text) => push_wrapped(&mut out, "* ", text, Style::new().fg(Color::DarkGray), width),
            Entry::Error(text) => push_wrapped(&mut out, "! ", text, Style::new().fg(Color::Red), width),
        }
        out.push(Line::default());
    }
    out
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(width.saturating_sub(3)).collect();
        t.push_str("...");
        t
    }
}

/// Hard-wrap `text` (prefixed on its first line) to `width` columns.
fn push_wrapped(out: &mut Vec<Line<'static>>, prefix: &str, text: &str, style: Style, width: usize) {
    let width = width.max(1);
    let mut first = true;
    for raw in text.split('\n') {
        let mut line = String::new();
        if first {
            line.push_str(prefix);
            first = false;
        }
        line.push_str(raw);
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            out.push(Line::default());
            continue;
        }
        for chunk in chars.chunks(width) {
            out.push(Line::from(Span::styled(chunk.iter().collect::<String>(), style)));
        }
    }
}

fn render_approval(frame: &mut Frame, model: &Model, area: Rect) {
    let Some(q) = model.pending.first() else { return };
    let more = if model.pending.len() > 1 { format!(" (+{} more)", model.pending.len() - 1) } else { String::new() };
    let title = format!(" approval needed{more} ");
    let keys = match &model.mode {
        InputMode::DenyReason(_) => "type a reason, Enter = deny, Esc = cancel".to_string(),
        InputMode::Message => "y = allow   a = allow always   n = deny with reason".to_string(),
    };
    let text = vec![
        Line::from(Span::styled(q.prompt.clone(), Style::new().add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(keys, Style::new().fg(Color::Yellow))),
    ];
    let block = Block::default().borders(Borders::ALL).title(title).border_style(Style::new().fg(Color::Yellow));
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn render_input(frame: &mut Frame, model: &Model, area: Rect) {
    let title = match &model.mode {
        InputMode::DenyReason(_) => " deny reason ",
        InputMode::Message if model.busy => " steer (Enter) / queue (Alt+Enter) / Esc = interrupt ",
        InputMode::Message => " message (Enter = send, Alt+Enter = queue) ",
    };
    let inner = area.width.saturating_sub(2) as usize;
    // Keep the end of long input visible.
    let chars: Vec<char> = model.input.chars().collect();
    let start = chars.len().saturating_sub(inner.saturating_sub(1));
    let shown: String = chars[start..].iter().collect();
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(Paragraph::new(shown.clone()).block(block), area);
    let x = area.x + 1 + shown.chars().count() as u16;
    frame.set_cursor_position((x.min(area.right().saturating_sub(2)), area.y + 1));
}

/// The status line text.
pub fn status_text(model: &Model) -> String {
    let conn = if model.connected { "" } else { " [disconnected]" };
    let phase = if model.thinking { "thinking" } else { model.phase.label() };
    let taint = if model.tainted { " | TAINTED" } else { "" };
    let model_name = if model.model.is_empty() { "-" } else { model.model.as_str() };
    let hint = model.hint.as_deref().map(|h| format!(" | {h}")).unwrap_or_default();
    let scroll = if model.scroll > 0 { format!(" | scrolled +{}", model.scroll) } else { String::new() };
    format!(
        "{}{conn} | {phase} | {model_name} | tok {}/{}{taint}{scroll}{hint}",
        model.session, model.input_tokens, model.output_tokens
    )
}

fn render_status(frame: &mut Frame, model: &Model, area: Rect) {
    let style = if model.tainted {
        Style::new().bg(Color::Red).fg(Color::White)
    } else {
        Style::new().bg(Color::DarkGray).fg(Color::White)
    };
    frame.render_widget(Paragraph::new(status_text(model)).style(style), area);
}
