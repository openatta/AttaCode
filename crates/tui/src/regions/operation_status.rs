//! `operation_status` — Operation Status / 状态区。含
//! `operation_status.status_line`（状态·状态行，几种内容互斥）与
//! `operation_status.task_list`（状态·任务清单，独立显隐）。

use crate::frame_state::{ItemStatus, StatusContent, StatusLineState, TaskItem, TaskListState};
use crate::regions::style::{self, COLOR_ACCENT, COLOR_SECONDARY, COLOR_SUCCESS, COLOR_WARNING};
use ratatui::{
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

const MAX_TASK_ROWS: usize = 5;

pub fn status_line_height(state: &StatusLineState) -> u16 {
    if state.content.is_some() {
        1
    } else {
        0
    }
}

pub fn render_status_line(frame: &mut Frame, area: Rect, state: &StatusLineState) {
    let Some(content) = &state.content else {
        return;
    };
    let line = match content {
        StatusContent::TurnRunning {
            spinner,
            activity,
            elapsed_secs,
            token_in,
            token_out,
        } => Line::from(vec![
            Span::styled(spinner.to_string(), Style::default().fg(COLOR_ACCENT)),
            Span::raw(" "),
            Span::styled(activity.clone(), Style::default().fg(style::COLOR_PRIMARY)),
            Span::raw(format!(
                "  {elapsed_secs}s   in:{token_in}  out:{token_out}"
            ))
            .fg(COLOR_SECONDARY),
        ]),
        StatusContent::Compacting {
            stage,
            stage_index,
            stage_total,
            tokens_before,
            tokens_after,
            ..
        } => {
            let after = tokens_after
                .map(|t| t.to_string())
                .unwrap_or_else(|| "…".into());
            Line::from(vec![
                Span::styled("⠴", Style::default().fg(COLOR_WARNING)),
                Span::raw(format!(
                    " Compacting ({}/{}: {})…  {tokens_before} → {after} tok",
                    stage_index + 1,
                    stage_total,
                    stage.label()
                ))
                .fg(COLOR_WARNING),
            ])
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

pub fn task_list_height(state: &TaskListState) -> u16 {
    state.items.len().min(MAX_TASK_ROWS) as u16
}

pub fn render_task_list(frame: &mut Frame, area: Rect, state: &TaskListState) {
    if state.items.is_empty() {
        return;
    }
    let overflow = state.items.len() > MAX_TASK_ROWS;
    let shown = if overflow {
        MAX_TASK_ROWS - 1
    } else {
        state.items.len()
    };
    let mut lines: Vec<Line<'static>> = state.items[..shown].iter().map(task_line).collect();
    if overflow {
        lines.push(Line::from(Span::styled(
            format!("  (+{} more hidden)", state.items.len() - shown),
            Style::default().fg(COLOR_SECONDARY),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn task_line(item: &TaskItem) -> Line<'static> {
    let color = match item.status {
        ItemStatus::Running => COLOR_ACCENT,
        ItemStatus::Pending => COLOR_SECONDARY,
        ItemStatus::Done => COLOR_SUCCESS,
        ItemStatus::Failed => style::COLOR_ERROR,
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(item.status.icon(), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(item.label.clone(), Style::default().fg(color)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_hidden_when_none() {
        assert_eq!(status_line_height(&StatusLineState { content: None }), 0);
    }

    #[test]
    fn task_list_hidden_when_empty() {
        assert_eq!(task_list_height(&TaskListState { items: vec![] }), 0);
    }

    #[test]
    fn task_list_caps_at_max_rows() {
        let items = (0..12)
            .map(|i| TaskItem {
                status: ItemStatus::Pending,
                label: format!("t{i}"),
            })
            .collect();
        assert_eq!(
            task_list_height(&TaskListState { items }),
            MAX_TASK_ROWS as u16
        );
    }
}
