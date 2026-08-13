//! Z0 Transcript — Z0.R0 Header (nested) + Z0.R1 Body.

use crate::frame_state::{
    HeaderSource, HeaderState, LineKind, TranscriptBodyState, TranscriptEntry,
};
use crate::regions::style::{self, COLOR_ACCENT, COLOR_SUBAGENT};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

pub fn header_height(state: &HeaderState) -> u16 {
    if state.text.is_some() && !matches!(state.source, HeaderSource::None) {
        1
    } else {
        0
    }
}

pub fn render_header(frame: &mut Frame, area: Rect, state: &HeaderState) {
    let Some(text) = &state.text else { return };
    let style = match state.source {
        HeaderSource::SubAgent => Style::default().fg(COLOR_SUBAGENT),
        _ => Style::default().fg(style::COLOR_SECONDARY),
    };
    let line = Line::from(Span::styled(
        format!("{}{text}", style::USER_PROMPT_PREFIX),
        style,
    ));
    frame.render_widget(Paragraph::new(line), area);
}

pub fn render_body(frame: &mut Frame, area: Rect, state: &TranscriptBodyState) {
    if area.height == 0 {
        return;
    }
    let visible = area.height as usize;
    let lines: Vec<Line<'static>> = if state.auto_follow {
        state
            .entries
            .iter()
            .rev()
            .take(visible)
            .map(entry_line)
            .rev()
            .collect()
    } else {
        let skip = state.scroll.offset.min(state.entries.len());
        state
            .entries
            .iter()
            .skip(skip)
            .take(visible)
            .map(entry_line)
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    if !state.auto_follow {
        let indicator_y = area.y + area.height.saturating_sub(1);
        let above = state.scroll.total_lines.saturating_sub(state.scroll.offset);
        let text = format!("── {above} lines above ──");
        let x = area.x + area.width.saturating_sub(text.len() as u16) / 2;
        frame.render_widget(
            Paragraph::new(Span::styled(text, Style::default().fg(COLOR_ACCENT))),
            Rect {
                x,
                y: indicator_y,
                width: area.width.saturating_sub(x - area.x),
                height: 1,
            },
        );
    }
}

fn entry_line(entry: &TranscriptEntry) -> Line<'static> {
    let (prefix, fg, bold) = match entry.kind {
        LineKind::UserPrompt => (style::USER_PROMPT_PREFIX, Color::White, false),
        LineKind::AssistantText => ("  ", Color::White, false),
        LineKind::ToolHeading => (style::TOOL_HEADING_PREFIX, style::COLOR_SUCCESS, true),
        LineKind::ToolResultOk => (style::TOOL_RESULT_OK_PREFIX, style::COLOR_SUCCESS, false),
        LineKind::ToolResultErr => (style::TOOL_RESULT_ERR_PREFIX, style::COLOR_ERROR, true),
        LineKind::Note => (style::NOTE_PREFIX, style::COLOR_SECONDARY, false),
        LineKind::Warning => (style::WARNING_PREFIX, style::COLOR_WARNING, true),
        LineKind::Error => (style::ERROR_PREFIX, style::COLOR_ERROR, true),
        LineKind::Thinking => ("  ", style::COLOR_SECONDARY, false),
        LineKind::DiffOld => (style::DIFF_OLD_PREFIX, style::COLOR_ERROR, false),
        LineKind::DiffNew => (style::DIFF_NEW_PREFIX, style::COLOR_SUCCESS, false),
        LineKind::DiffContext => ("    ", Color::Rgb(200, 200, 210), false),
        LineKind::Banner => ("", Color::White, false),
    };
    let mut style = Style::default().fg(fg);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if matches!(entry.kind, LineKind::Thinking) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    let bg = match entry.kind {
        LineKind::DiffOld => Some(style::COLOR_DIFF_OLD_BG),
        LineKind::DiffNew => Some(style::COLOR_DIFF_NEW_BG),
        _ => None,
    };
    if let Some(bg) = bg {
        style = style.bg(bg);
    }
    Line::from(Span::styled(format!("{prefix}{}", entry.text), style))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_hidden_when_no_text() {
        let s = HeaderState {
            text: None,
            source: HeaderSource::None,
        };
        assert_eq!(header_height(&s), 0);
    }

    #[test]
    fn header_visible_with_text_and_source() {
        let s = HeaderState {
            text: Some("hi".into()),
            source: HeaderSource::UserPrompt,
        };
        assert_eq!(header_height(&s), 1);
    }
}
