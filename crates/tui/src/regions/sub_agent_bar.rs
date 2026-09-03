//! `sub_agent_bar` — Sub-Agent Bar / 子代理条。叶子区域，列出在跑的和刚跑完的子代理。

use crate::frame_state::{SubAgentBarState, SubAgentState};
use crate::regions::style::{self, COLOR_ERROR, COLOR_SECONDARY, COLOR_SUCCESS};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub fn height(state: &SubAgentBarState) -> u16 {
    state.agents.len() as u16
}

pub fn render(frame: &mut Frame, area: Rect, state: &SubAgentBarState, spinner: char) {
    if state.agents.is_empty() {
        return;
    }
    let lines: Vec<Line<'static>> = state
        .agents
        .iter()
        .map(|agent| {
            let (icon, color) = match agent.state {
                SubAgentState::Running => (spinner.to_string(), style::COLOR_ACCENT),
                SubAgentState::Done => ("✓".to_string(), COLOR_SUCCESS),
                SubAgentState::Failed => ("✗".to_string(), COLOR_ERROR),
            };
            Line::from(vec![
                Span::styled(icon, Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(agent.name.clone(), Style::default().fg(color)),
                Span::styled(
                    format!("  {} tok  {}", agent.token_usage, agent.elapsed_or_status),
                    Style::default().fg(COLOR_SECONDARY),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}
