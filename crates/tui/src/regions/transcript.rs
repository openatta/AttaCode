//! `transcript` — Transcript / 转录区。含 `transcript.header`（转录·顶栏）
//! 与 `transcript.body`（转录·正文）。

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
    let selected = |e: &TranscriptEntry| match (&e.block_id, &state.selected_block) {
        (Some(id), Some(sel)) => id == sel,
        _ => false,
    };
    let lines: Vec<Line<'static>> = if state.auto_follow {
        state
            .entries
            .iter()
            .rev()
            .take(visible)
            .map(|e| entry_line(e, selected(e)))
            .rev()
            .collect()
    } else {
        let skip = state.scroll.offset.min(state.entries.len());
        state
            .entries
            .iter()
            .skip(skip)
            .take(visible)
            .map(|e| entry_line(e, selected(e)))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    if !state.auto_follow {
        let indicator_y = area.y + area.height.saturating_sub(1);
        // `offset` 就是被跳过的条数，也就是视口上方有多少行。这里原本写的是
        // `total_lines - offset`（视口**下方**还剩多少），和 "lines above" 的措辞正好
        // 相反——在 `auto_follow` 一直为 true、这个分支从来没渲染过的时候没人发现。
        let above = state.scroll.offset.min(state.entries.len());
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

/// One transcript line. `selected` marks it as part of the block the transcript
/// keys currently act on — drawn as a left gutter bar rather than a background
/// wash: a folded tool block can be dozens of lines tall, and inverting all of
/// them to say "this is selected" is louder than the information deserves. The
/// gutter column is rendered for *every* line (a blank when unselected) so
/// selecting something doesn't shift the whole transcript sideways by one cell.
fn entry_line(entry: &TranscriptEntry, selected: bool) -> Line<'static> {
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
    let gutter = if selected {
        Span::styled(style::SELECTION_GUTTER, Style::default().fg(COLOR_ACCENT))
    } else {
        Span::raw(" ")
    };
    Line::from(vec![
        gutter,
        Span::styled(format!("{prefix}{}", entry.text), style),
    ])
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

    fn body(offset: usize, auto_follow: bool) -> TranscriptBodyState {
        use crate::frame_state::ScrollState;
        let entries: Vec<TranscriptEntry> = (0..50)
            .map(|i| TranscriptEntry {
                continues_previous: false,
                kind: LineKind::AssistantText,
                text: format!("line{i}"),
                block_id: None,
            })
            .collect();
        TranscriptBodyState {
            scroll: ScrollState {
                offset,
                total_lines: entries.len(),
            },
            entries,
            auto_follow,
            selected_block: None,
        }
    }

    /// 两条 block_id 相同的行 + 一条别的块的行，用来看选中标记落在哪。
    fn body_with_blocks(selected: Option<&str>) -> TranscriptBodyState {
        use crate::frame_state::ScrollState;
        let entries: Vec<TranscriptEntry> = ["a", "a", "b"]
            .iter()
            .enumerate()
            .map(|(i, id)| TranscriptEntry {
                continues_previous: false,
                kind: LineKind::ToolResultOk,
                text: format!("row{i}"),
                block_id: Some((*id).to_string()),
            })
            .collect();
        TranscriptBodyState {
            scroll: ScrollState {
                offset: 0,
                total_lines: entries.len(),
            },
            entries,
            auto_follow: true,
            selected_block: selected.map(str::to_string),
        }
    }

    fn rendered(state: &TranscriptBodyState) -> String {
        let backend = ratatui::backend::TestBackend::new(40, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_body(f, f.area(), state)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// `auto_follow` 一直是 true，所以滚动分支从来没被渲染过——包括那句
    /// "N lines above"，它以前算的是视口**下方**还剩多少行。
    #[test]
    fn scrolled_body_starts_at_the_offset_and_counts_the_lines_above_it() {
        let out = rendered(&body(30, false));
        assert!(out.contains("line30"), "视口应该从 offset 那条开始:\n{out}");
        assert!(!out.contains("line29"), "offset 之前的不该出现:\n{out}");
        assert!(out.contains("30 lines above"), "提示条数字不对:\n{out}");
    }

    #[test]
    fn following_body_shows_the_tail_and_no_indicator() {
        let out = rendered(&body(0, true));
        assert!(out.contains("line49"), "跟随时应该看到最后一条:\n{out}");
        assert!(!out.contains("lines above"));
    }

    /// 选中标记只落在选中块自己的行上——块 "a" 有两行，块 "b" 那行不该带标记。
    #[test]
    fn selection_gutter_marks_only_the_selected_blocks_lines() {
        let out = rendered(&body_with_blocks(Some("a")));
        let marks = out.matches(style::SELECTION_GUTTER).count();
        assert_eq!(marks, 2, "应该只有块 a 的两行带标记:\n{out}");
    }

    /// 没有选中时一个标记都不画（此时 F5 作用于最后一个可折叠块，不需要提示）。
    #[test]
    fn no_gutter_when_nothing_is_selected() {
        let out = rendered(&body_with_blocks(None));
        assert!(!out.contains(style::SELECTION_GUTTER), "不该有标记:\n{out}");
    }
}
