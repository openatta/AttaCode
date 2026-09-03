//! 顶层区域的组合 —— 唯一计算各区域高度并自上而下堆叠它们的地方。
//!
//! 区域的规范名（代码路径 / English / 中文）见 `docs/TUI_DESIGN.md`。

use crate::frame_state::FrameState;
use crate::regions::{btw, composer, footer_hints, operation_status, sub_agent_bar, transcript};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

/// 转录正文区的高度——所有其他区块都是内容定高的，正文吃掉剩下的。
///
/// 单独暴露出来是因为"翻一页"是多少行只有这里知道，而滚动位置是 UI-本地状态、
/// 归 `crates/app` 管（同 draft/补全选择）。app 拿这个值算翻页步长，`render`
/// 自己也用它，两边不会各算一套。
pub fn transcript_body_height(area: Rect, state: &FrameState) -> u16 {
    let header_h = transcript::header_height(&state.transcript.header);
    let status_h = operation_status::status_line_height(&state.operation_status.status_line);
    let task_h = operation_status::task_list_height(&state.operation_status.task_list);
    let composer_h = composer::height(&state.composer, area.width);
    let sub_agent_h = sub_agent_bar::height(&state.sub_agent_bar);
    let footer_h = footer_hints::HEIGHT;

    let fixed_below_transcript = status_h + task_h + composer_h + sub_agent_h + footer_h;
    area.height
        .saturating_sub(header_h)
        .saturating_sub(fixed_below_transcript)
        .max(1)
}

pub fn render(frame: &mut Frame, area: Rect, state: &FrameState, spinner: char) {
    // 侧问区激活时，屏幕只有两块：上半的转录区和下半的它自己。状态区、输入区、底栏、
    // 子代理条**都不画**——它独占。盖住之后主任务的进度就看不见了，这是照 Claude Code
    // 的 `/btw` 做的，CC 也是这样；那是这个形态的代价，不是遗漏。
    if let Some(btw_state) = &state.btw {
        let btw_h = btw::height(area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(area.height.saturating_sub(btw_h)),
                Constraint::Length(btw_h),
            ])
            .split(area);
        let [top, bottom]: [Rect; 2] = rows.as_ref().try_into().expect("2 rows");
        if top.height > 0 {
            transcript::render_body(frame, top, &state.transcript.body);
        }
        btw::render(frame, bottom, btw_state);
        return;
    }

    let header_h = transcript::header_height(&state.transcript.header);
    let status_h = operation_status::status_line_height(&state.operation_status.status_line);
    let task_h = operation_status::task_list_height(&state.operation_status.task_list);
    let composer_h = composer::height(&state.composer, area.width);
    let sub_agent_h = sub_agent_bar::height(&state.sub_agent_bar);
    let footer_h = footer_hints::HEIGHT;
    let body_h = transcript_body_height(area, state);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(body_h),
            Constraint::Length(status_h),
            Constraint::Length(task_h),
            Constraint::Length(composer_h),
            // 底栏在子代理条**上面**：底栏是常驻的一行（模型/cwd/权限模式/用量），
            // 子代理条是条件显示的，把常驻的那条钉在固定位置上，眼睛才不用每次
            // 重新找它——子代理条一出现就把底栏顶走一行的话，人会先愣一下。
            Constraint::Length(footer_h),
            Constraint::Length(sub_agent_h),
        ])
        .split(area);

    let [header_r, body_r, status_r, task_r, composer_r, footer_r, sub_agent_r]: [Rect; 7] =
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
    footer_hints::render(frame, footer_r, &state.footer_hints);
    if sub_agent_h > 0 {
        sub_agent_bar::render(frame, sub_agent_r, &state.sub_agent_bar, spinner);
    }
}
