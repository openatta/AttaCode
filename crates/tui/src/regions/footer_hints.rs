//! Z4 FooterHints — leaf region, always 1 line.

use crate::frame_state::{AppMode, FooterHintsState};
use crate::regions::style::{COLOR_ACCENT, COLOR_SECONDARY};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub const HEIGHT: u16 = 1;

pub fn render(frame: &mut Frame, area: Rect, state: &FooterHintsState) {
    let mut left: Vec<Span<'static>> = vec![
        Span::styled(
            state.model.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}  ", state.cwd),
            Style::default().fg(COLOR_SECONDARY),
        ),
    ];
    for mode in [AppMode::Normal, AppMode::Plan, AppMode::Auto] {
        let style = if mode == state.mode {
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_SECONDARY)
        };
        left.push(Span::styled(format!("[{}] ", mode.label()), style));
    }

    let usage_text = format!(
        "{}↑ {}↓  ",
        format_tokens(state.usage.token_in),
        format_tokens(state.usage.token_out)
    );
    left.push(Span::styled(
        usage_text.clone(),
        Style::default().fg(COLOR_SECONDARY),
    ));

    let left_w: usize = left.iter().map(|s| s.content.len()).sum();
    let right_w = state.right_hint.len();
    let pad = (area.width as usize)
        .saturating_sub(left_w)
        .saturating_sub(right_w);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(
        state.right_hint.clone(),
        Style::default().fg(COLOR_SECONDARY),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `12300` → `"12.3k"`, `900` → `"900"`.
fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}
