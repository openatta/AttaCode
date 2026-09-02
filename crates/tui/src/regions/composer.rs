//! Z2 Composer — Z2.R0 AppInfoLine, Z2.R1 TopRule, Z2.R2 Content (Z2.R2.S0 Editor base +
//! Z2.R2.S1 CompletionPopup floating + Z2.R2.S2 Approval stacked above Editor), Z2.R3 BottomRule.

use crate::frame_state::{
    AnswerWith, ApprovalRequest, ApprovalState, ApprovalViewMode, BottomRuleState,
    CompletionPopupState, ComposerState, ContentState, EditorState, InputMode, TopRuleState,
};
use crate::regions::style::{self, separator_color, COLOR_ACCENT, COLOR_SECONDARY};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

/// Editor content is prefixed with `"> "` (or `"! "` in bash-escape mode) — reserve
/// that much width when estimating wrapped row count.
const EDITOR_PREFIX_WIDTH: u16 = 2;

pub fn height(state: &ComposerState, width: u16) -> u16 {
    let app_info = if state.app_info.text.is_some() { 1 } else { 0 };
    let top_rule = 1;
    let bottom_rule = 1;
    app_info + top_rule + content_height(&state.content, width) + bottom_rule
}

fn content_height(state: &ContentState, width: u16) -> u16 {
    editor_height(&state.editor, width)
        + state
            .approval
            .as_ref()
            .map(|a| approval_height(a, width))
            .unwrap_or(0)
}

fn editor_height(state: &EditorState, width: u16) -> u16 {
    let avail = width.saturating_sub(EDITOR_PREFIX_WIDTH).max(1);
    let text_lines = wrapped_line_count(&state.draft, avail);
    let paste = if state.paste_placeholder.is_some() {
        1
    } else {
        0
    };
    (text_lines + paste).clamp(1, 12)
}

/// Number of visual rows `text` occupies once soft-wrapped at `avail_width` columns.
/// Explicit `\n`s always start a new row; a single long line without `\n` still wraps
/// across multiple rows once it exceeds `avail_width` (mirrors the `Wrap` applied in
/// `render_editor` — this must stay in sync with that or the allocated area will be
/// too short for what actually gets rendered).
fn wrapped_line_count(text: &str, avail_width: u16) -> u16 {
    let avail = avail_width.max(1) as usize;
    let mut total: u16 = 0;
    for line in text.split('\n') {
        let w = line.width();
        let rows = w.saturating_add(avail).saturating_sub(1) / avail;
        total = total.saturating_add(rows.max(1) as u16);
    }
    total.max(1)
}

fn approval_height(state: &ApprovalState, _width: u16) -> u16 {
    let border = 2;
    match state.view_mode {
        ApprovalViewMode::TabView => {
            let tabs = if state.pending.len() > 1 { 1 } else { 0 };
            let card = state
                .pending
                .get(state.active_idx)
                .map(card_inner_height)
                .unwrap_or(0);
            border + tabs + card
        }
        ApprovalViewMode::ListView => border + 1 + state.pending.len() as u16 + 2,
    }
}

fn card_inner_height(req: &ApprovalRequest) -> u16 {
    let msg_lines = req.message.lines().count().max(1) as u16;
    // 自由文本题的 `options` 是空的，那一段自然是 0 行——高度跟着选项数走就对了，
    // 不需要为它开一个分支。
    1 /* tool_name header */ + msg_lines + 1 /* blank */ + req.options.len() as u16 + 1 /* blank */ + 1
    /* footer */
}

pub fn render(frame: &mut Frame, area: Rect, state: &ComposerState) {
    let app_info_h = if state.app_info.text.is_some() { 1 } else { 0 };
    let content_h = content_height(&state.content, area.width);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(app_info_h),
            Constraint::Length(1),
            Constraint::Length(content_h),
            Constraint::Length(1),
        ])
        .split(area);

    if app_info_h > 0 {
        render_app_info_line(frame, rows[0], state.app_info.text.as_deref().unwrap_or(""));
    }
    render_top_rule(frame, rows[1], &state.top_rule);
    render_content(frame, rows[2], &state.content);
    render_bottom_rule(frame, rows[3], &state.bottom_rule);
}

fn render_app_info_line(frame: &mut Frame, area: Rect, text: &str) {
    let w = area.width as usize;
    let pad = w.saturating_sub(text.len());
    let line = Line::from(Span::styled(
        format!("{}{text}", " ".repeat(pad)),
        Style::default().fg(COLOR_ACCENT),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_top_rule(frame: &mut Frame, area: Rect, state: &TopRuleState) {
    let color = separator_color(state.color);
    let label = state
        .right_label
        .as_ref()
        .map(|l| l.text())
        .unwrap_or_default();
    let dash_w = area.width.saturating_sub(label.len() as u16) as usize;
    let line = Line::from(vec![
        Span::styled("─".repeat(dash_w), Style::default().fg(color)),
        Span::styled(label, Style::default().fg(color)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_bottom_rule(frame: &mut Frame, area: Rect, state: &BottomRuleState) {
    let color = separator_color(state.color);
    let line = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(color),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_content(frame: &mut Frame, area: Rect, state: &ContentState) {
    let editor_h = editor_height(&state.editor, area.width);
    let approval_h = state
        .approval
        .as_ref()
        .map(|a| approval_height(a, area.width))
        .unwrap_or(0);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(approval_h), Constraint::Length(editor_h)])
        .split(area);

    render_editor(frame, rows[1], &state.editor);
    if let Some(approval) = &state.approval {
        render_approval(frame, rows[0], approval);
    } else if let Some(completion) = &state.completion {
        render_completion_popup(frame, rows[1], completion);
    }
}

fn render_editor(frame: &mut Frame, area: Rect, state: &EditorState) {
    let prefix = match state.mode {
        InputMode::BashEscape => "! ",
        _ => "> ",
    };
    let style = if state.locked {
        Style::default().fg(COLOR_SECONDARY)
    } else {
        Style::default().fg(style::COLOR_PRIMARY)
    };
    let mut lines = editor_lines(state, prefix, style);
    if let Some(paste) = &state.paste_placeholder {
        lines.push(Line::from(Span::styled(
            format!(
                "  [Pasted text +{} lines, {} bytes]",
                paste.lines, paste.bytes
            ),
            Style::default().fg(COLOR_SECONDARY),
        )));
    }
    if matches!(state.mode, InputMode::VimNormal) {
        lines.push(Line::from(Span::styled(
            "[NORMAL]",
            Style::default()
                .fg(style::COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// 草稿的每一行，光标画在 `state.cursor`（草稿里的**字节**下标）指的位置上。
///
/// 光标压在字符上时用反色，不额外占一格——插一个 `█` 进去会把后面的文字整体
/// 右推一列，行内移动光标时整行来回抖。只有光标在行尾（下面没字符可压）时才画
/// 那个块，也就是这个函数以前唯一的行为。
///
/// 换行：`prefix`（`> `）只出现在第一行，续行不带——和 `wrapped_line_count`
/// 估高时的假设一致。
fn editor_lines(state: &EditorState, prefix: &str, style: Style) -> Vec<Line<'static>> {
    let cursor_style = style.add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut offset = 0usize;
    for (i, segment) in state.draft.split('\n').enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if i == 0 {
            spans.push(Span::styled(prefix.to_string(), style));
        }
        // 光标在本行：`offset..=offset+len`。行尾那个位置归本行（下一行从
        // `offset+len+1` 起，`\n` 自己占一个字节），不会两行都认领。
        let local = state
            .cursor
            .checked_sub(offset)
            .filter(|rel| !state.locked && *rel <= segment.len());
        match local {
            Some(rel) => {
                let (before, rest) = segment.split_at(floor_boundary(segment, rel));
                spans.push(Span::styled(before.to_string(), style));
                match rest.chars().next() {
                    Some(c) => {
                        spans.push(Span::styled(c.to_string(), cursor_style));
                        spans.push(Span::styled(rest[c.len_utf8()..].to_string(), style));
                    }
                    None => spans.push(Span::styled("█".to_string(), style)),
                }
            }
            None => spans.push(Span::styled(segment.to_string(), style)),
        }
        lines.push(Line::from(spans));
        offset += segment.len() + 1; // +1 = 那个 '\n'
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(prefix.to_string(), style)));
    }
    lines
}

/// 把一个可能落在多字节字符中间的下标退回最近的字符边界。宿主本该只给合法下标，
/// 但快照是可序列化的公开结构，一个错的 `cursor` 不该让渲染 panic。
fn floor_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn render_completion_popup(frame: &mut Frame, editor_area: Rect, state: &CompletionPopupState) {
    let h = (state.candidates.len() as u16 + 2).min(5);
    let w = editor_area.width.min(60);
    let area = Rect {
        x: editor_area.x,
        y: editor_area.y.saturating_sub(h),
        width: w,
        height: h,
    };
    frame.render_widget(Clear, area);
    let lines: Vec<Line<'static>> = state
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let selected = i == state.selected;
            let marker = if selected { "❯ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(COLOR_ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!("{marker}{}", c.name), style),
                Span::raw("  "),
                Span::styled(c.description.clone(), Style::default().fg(COLOR_SECONDARY)),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_ACCENT));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_approval(frame: &mut Frame, area: Rect, state: &ApprovalState) {
    match state.view_mode {
        ApprovalViewMode::TabView => render_approval_tabs(frame, area, state),
        ApprovalViewMode::ListView => render_approval_list(frame, area, state),
    }
}

fn render_approval_tabs(frame: &mut Frame, area: Rect, state: &ApprovalState) {
    let mut area = area;
    if state.pending.len() > 1 {
        let tab_area = Rect { height: 1, ..area };
        let tabs: Vec<Span<'static>> = state
            .pending
            .iter()
            .enumerate()
            .flat_map(|(i, req)| {
                let style = if i == state.active_idx {
                    Style::default()
                        .fg(COLOR_ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(COLOR_SECONDARY)
                };
                vec![Span::styled(
                    format!(" [{}#{}]", req.tool_name, i + 1),
                    style,
                )]
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(tabs)), tab_area);
        area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
    }
    let Some(req) = state.pending.get(state.active_idx) else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let head = vec![Line::from(Span::styled(
        format!("{}:", req.tool_name),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    let mut body: Vec<Line<'static>> = Vec::new();
    for l in req.message.lines() {
        body.push(Line::from(format!("  {l}")));
    }
    body.push(Line::from(""));
    let mut tail: Vec<Line<'static>> = Vec::new();
    for (i, opt) in req.options.iter().enumerate() {
        let marker = if i == req.selected_option {
            "❯ "
        } else {
            "  "
        };
        let style = if i == req.selected_option {
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        tail.push(Line::from(Span::styled(
            format!("  {marker}{}", opt.label()),
            style,
        )));
    }
    tail.push(Line::from(""));
    // 自由文本题没有可选的东西，Enter 送的是 composer 里那一行，Esc 也不代表拒绝
    // ——照着选择题写会指错三个键里的三个。
    let footer = match (req.answer_with, state.pending.len() > 1) {
        (AnswerWith::Type, _) => "Type your answer below, then Enter",
        (AnswerWith::Choose, true) => "Enter=confirm  Esc=deny  Tab=next",
        (AnswerWith::Choose, false) => "Enter=confirm  Esc=deny",
    };
    tail.push(Line::from(Span::styled(
        format!("  {footer}"),
        Style::default().fg(COLOR_SECONDARY),
    )));
    frame.render_widget(
        Paragraph::new(fit_card(head, body, tail, inner.height)),
        inner,
    );
}

/// 高度不够时**先砍说明文字**，别砍选项和最后那行按键提示。
///
/// 原来是把整卡片交给 `Paragraph` 从下往上截——终端矮一点，被截掉的正好是
/// "Enter=confirm  Esc=deny" 那行，于是用户面对一个不知道怎么答的对话框。
/// 选项和提示是**操作说明**，说明文字是上下文：挤的时候后者让路，并留一个 `…`
/// 说明这里被截了（而不是假装消息就这么短）。
fn fit_card(
    head: Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    tail: Vec<Line<'static>>,
    height: u16,
) -> Vec<Line<'static>> {
    let height = height as usize;
    let must = head.len() + tail.len();
    let mut out = head;
    if must + body.len() <= height {
        out.extend(body);
    } else {
        // 给说明文字留下的行数（最后一行用来放省略号）。
        let room = height.saturating_sub(must);
        if room > 0 {
            out.extend(body.into_iter().take(room - 1));
            out.push(Line::from(Span::styled(
                "  …",
                Style::default().fg(COLOR_SECONDARY),
            )));
        }
    }
    out.extend(tail);
    out
}

fn render_approval_list(frame: &mut Frame, area: Rect, state: &ApprovalState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(Span::styled(
        format!("{} pending approvals", state.pending.len()),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    for (i, req) in state.pending.iter().enumerate() {
        let selected = i == state.active_idx;
        let marker = if selected { "❯ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{marker}{}: {}",
                req.tool_name,
                req.message.lines().next().unwrap_or("")
            ),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter=expand  ↑/↓=move",
        Style::default().fg(COLOR_SECONDARY),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_state::{ApprovalOption, SeparatorColor};

    fn empty_editor() -> EditorState {
        EditorState {
            mode: InputMode::Normal,
            draft: String::new(),
            cursor: 0,
            paste_placeholder: None,
            locked: false,
        }
    }

    #[test]
    fn app_info_hidden_when_empty() {
        let state = ComposerState {
            app_info: crate::frame_state::AppInfoLineState { text: None },
            top_rule: TopRuleState {
                color: SeparatorColor::DarkGray,
                right_label: None,
            },
            content: ContentState {
                editor: empty_editor(),
                completion: None,
                approval: None,
            },
            bottom_rule: BottomRuleState {
                color: SeparatorColor::DarkGray,
            },
        };
        assert_eq!(
            height(&state, 80),
            1 /*top_rule*/ + 1 /*editor*/ + 1 /*bottom_rule*/
        );
    }

    #[test]
    fn single_pending_approval_has_no_tab_strip_height() {
        let req = ApprovalRequest {
            answer_with: AnswerWith::Choose,
            prompt_id: "test-1".into(),
            tool_name: "Bash".into(),
            message: "git status".into(),
            options: vec![ApprovalOption::PermitOnce, ApprovalOption::Deny],
            selected_option: 0,
        };
        let with_one = ApprovalState {
            pending: vec![req.clone()],
            active_idx: 0,
            view_mode: ApprovalViewMode::TabView,
        };
        let with_two = ApprovalState {
            pending: vec![req.clone(), req],
            active_idx: 0,
            view_mode: ApprovalViewMode::TabView,
        };
        assert_eq!(
            approval_height(&with_two, 80) - approval_height(&with_one, 80),
            1
        );
    }

    #[test]
    fn editor_height_grows_with_explicit_newlines() {
        let mut state = empty_editor();
        state.draft = "line one\nline two\nline three".into();
        assert_eq!(editor_height(&state, 80), 3);
    }

    #[test]
    fn editor_height_soft_wraps_a_long_single_line() {
        let mut state = empty_editor();
        // no '\n' at all, but far longer than a narrow terminal width
        state.draft = "x".repeat(100);
        // avail width = 20 - EDITOR_PREFIX_WIDTH(2) = 18 → ceil(100/18) = 6 rows
        assert_eq!(editor_height(&state, 20), 6);
    }

    #[test]
    fn editor_height_clamps_at_twelve_rows() {
        let mut state = empty_editor();
        state.draft = "x".repeat(1000);
        assert_eq!(editor_height(&state, 20), 12);
    }

    /// 一行的可见文本（含 `> ` 前缀）。
    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// 光标压在字符上时不额外占格——行内移动光标不该让后面的文字左右抖。
    #[test]
    fn a_mid_line_cursor_does_not_shift_the_text() {
        let mut state = empty_editor();
        state.draft = "hello".into();
        state.cursor = 2;
        let lines = editor_lines(&state, "> ", Style::default());
        assert_eq!(line_text(&lines[0]), "> hello");
        // 光标那一格是被反色的那个 span，正好是第 3 个字符。
        let cursor_span = lines[0]
            .spans
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .expect("应该有一个反色的光标格");
        assert_eq!(cursor_span.content.as_ref(), "l");
    }

    /// 光标在行尾时下面没字符可压，画那个块——这也是以前唯一的行为。
    #[test]
    fn a_cursor_at_the_end_is_drawn_as_a_block() {
        let mut state = empty_editor();
        state.draft = "hi".into();
        state.cursor = 2;
        let lines = editor_lines(&state, "> ", Style::default());
        assert_eq!(line_text(&lines[0]), "> hi█");
    }

    /// 多行草稿：前缀只在第一行，光标只落在它所在的那一行。
    #[test]
    fn the_cursor_lands_on_its_own_line_only() {
        let mut state = empty_editor();
        state.draft = "one\ntwo".into();
        state.cursor = 5; // "one\n" 之后的 't','w' 之间
        let lines = editor_lines(&state, "> ", Style::default());
        assert_eq!(line_text(&lines[0]), "> one");
        assert_eq!(line_text(&lines[1]), "two");
        assert!(!lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED)));
        assert!(lines[1]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED)));
    }

    /// 权限对话框开着时 composer 是锁的，不该画光标。
    #[test]
    fn a_locked_editor_draws_no_cursor() {
        let mut state = empty_editor();
        state.draft = "hi".into();
        state.cursor = 1;
        state.locked = true;
        let lines = editor_lines(&state, "> ", Style::default());
        assert_eq!(line_text(&lines[0]), "> hi");
    }

    /// 快照是公开可序列化结构，一个落在多字节字符中间的 cursor 不该让渲染 panic。
    #[test]
    fn an_out_of_bounds_or_mid_char_cursor_does_not_panic() {
        let mut state = empty_editor();
        state.draft = "重构".into();
        state.cursor = 1; // 第一个汉字的中间
        let lines = editor_lines(&state, "> ", Style::default());
        assert_eq!(line_text(&lines[0]), "> 重构");
        state.cursor = 999;
        let _ = editor_lines(&state, "> ", Style::default());
    }

    #[test]
    fn wrapped_line_count_matches_manual_expectation() {
        assert_eq!(wrapped_line_count("", 10), 1);
        assert_eq!(wrapped_line_count("hello", 10), 1);
        assert_eq!(wrapped_line_count("hello world!", 5), 3); // ceil(12/5)
        assert_eq!(wrapped_line_count("a\nb\nc", 10), 3);
    }
}
