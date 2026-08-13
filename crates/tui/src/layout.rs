//! Top-level Z0..Z4 composition — the only place that computes region heights and stacks them.

use crate::frame_state::FrameState;
use crate::regions::{composer, footer_hints, operation_status, sub_agent_bar, transcript};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

pub fn render(frame: &mut Frame, area: Rect, state: &FrameState, spinner: char) {
    let header_h = transcript::header_height(&state.transcript.header);
    let status_h = operation_status::status_line_height(&state.operation_status.status_line);
    let task_h = operation_status::task_list_height(&state.operation_status.task_list);
    let composer_h = composer::height(&state.composer, area.width);
    let sub_agent_h = sub_agent_bar::height(&state.sub_agent_bar);
    let footer_h = footer_hints::HEIGHT;

    let fixed_below_transcript = status_h + task_h + composer_h + sub_agent_h + footer_h;
    let body_h = area
        .height
        .saturating_sub(header_h)
        .saturating_sub(fixed_below_transcript)
        .max(1);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(body_h),
            Constraint::Length(status_h),
            Constraint::Length(task_h),
            Constraint::Length(composer_h),
            Constraint::Length(sub_agent_h),
            Constraint::Length(footer_h),
        ])
        .split(area);

    let [header_r, body_r, status_r, task_r, composer_r, sub_agent_r, footer_r]: [Rect; 7] =
        rows.as_ref().try_into().expect("7 rows");

    if header_h > 0 {
        transcript::render_header(frame, header_r, &state.transcript.header);
    }
    transcript::render_body(frame, body_r, &state.transcript.body);
    if status_h > 0 {
        operation_status::render_status_line(frame, status_r, &state.operation_status.status_line);
    }
    if task_h > 0 {
        operation_status::render_task_list(frame, task_r, &state.operation_status.task_list);
    }
    composer::render(frame, composer_r, &state.composer);
    if sub_agent_h > 0 {
        sub_agent_bar::render(frame, sub_agent_r, &state.sub_agent_bar, spinner);
    }
    footer_hints::render(frame, footer_r, &state.footer_hints);
}
