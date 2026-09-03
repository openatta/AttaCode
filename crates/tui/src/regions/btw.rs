//! `btw` — Side Question / 侧问区。
//!
//! 激活时独占屏幕下半：转录区压到约一半，这里盖住状态区、输入区、底栏和子代理条。
//!
//! **它自带一条状态栏**，因为它把底栏盖住了——用户在这期间没有别的地方可以看见
//! "现在能按什么"。这条线也是为什么这个区域画得起边框：它是一个明确"进去了、要出来"
//! 的模式，边框让人一眼知道自己不在主 UI 里。

use crate::frame_state::BtwState;
use crate::regions::style::{COLOR_ACCENT, COLOR_SECONDARY, COLOR_WARNING};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

/// 侧问区吃掉屏幕的这个比例。剩下的留给转录区。
///
/// 一半是需求定的：再小放不下一段像样的回答，再大就把转录区挤成没法参照的一条。
const SHARE: u16 = 2;

/// 给定整块画布，侧问区应该占多高。
pub fn height(area: Rect) -> u16 {
    // 至少 6 行：边框 2 + 问题 1 + 答案 1 + 状态栏 1 还得留一行余量。屏幕矮到连这个
    // 都放不下时，宁可让它占满，也不要画一个自己都装不下自己的框。
    (area.height / SHARE).max(6).min(area.height)
}

/// 最多能跳过多少条，才不至于把内容滚出视口外。
///
/// 从最后一条往回数它们软换行之后各占几行，加到装满 `body` 为止——`Paragraph` 的
/// `Wrap` 是按屏幕列宽折的，所以能滚多远这件事只有算过宽度才知道。
fn max_skip(lines: &[Line<'static>], body: Rect) -> usize {
    let avail = body.width.max(1) as usize;
    let cap = body.height as usize;
    // 至少留最后一条在屏幕上：它自己就比整块区域高时，也还是要看得见。
    let mut skip = lines.len().saturating_sub(1);
    let mut used = 0usize;
    for (i, line) in lines.iter().enumerate().rev() {
        used += line.width().max(1).div_ceil(avail);
        if used > cap {
            break;
        }
        skip = i;
    }
    skip
}

pub fn render(frame: &mut Frame, area: Rect, state: &BtwState) {
    frame.render_widget(Clear, area);

    let title = if state.viewing == 0 {
        " btw ".to_string()
    } else {
        // 翻看早前问答时要说清楚"你现在看的不是最新那条"，否则用户会以为答案变了。
        format!(" btw · 第 {} 条（往回）", state.viewing)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_ACCENT))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }
    // 最后一行留给自己的状态栏。
    let body = Rect {
        height: inner.height - 1,
        ..inner
    };
    let hint_row = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // 早前问答：暗色列在上面，新的在前。只列问题——答案要看就用 ←/→ 翻过去。
    for question in &state.earlier {
        lines.push(Line::from(Span::styled(
            format!("  · {question}"),
            Style::default().fg(COLOR_SECONDARY),
        )));
    }
    if state.older > 0 {
        lines.push(Line::from(Span::styled(
            format!("  · 还有 {} 条更早的", state.older),
            Style::default().fg(COLOR_SECONDARY),
        )));
    }
    if !state.earlier.is_empty() || state.older > 0 {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(Span::styled(
        format!("> {}", state.question),
        Style::default()
            .fg(COLOR_ACCENT)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    if state.answer.is_empty() && state.streaming {
        lines.push(Line::from(Span::styled(
            "  想一下……",
            Style::default().fg(COLOR_SECONDARY),
        )));
    }
    for line in state.answer.lines() {
        lines.push(Line::from(format!("  {line}")));
    }

    // 滚动：`scroll` 是跳过的行数，夹在能滚的范围内——答案变短（翻到别的条）之后
    // 悬空的偏移会让整个区域看起来空白一片。
    //
    // 上限按**软换行之后占几行**算，不能按 `lines.len()` 算：一段中文回答在 60 列
    // 底下一条会摊成三四行，按条数夹的话尾巴永远滚不到——而尾巴正是回答的结论。
    let skip = state.scroll.min(max_skip(&lines, body));
    let shown: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
    frame.render_widget(Paragraph::new(shown).wrap(Wrap { trim: false }), body);

    // 外面有请求在等的话，这条线先说那件事——**流式期间也说**，那正是最容易撞上
    // 的时候（模型跑着工具、你在这儿问闲话）。侧问区把审批框整个盖住了，主 turn 就
    // 停在那儿等，默认 300 秒之后引擎按"未作答不是同意"拒掉它。不说的话，用户看到
    // 的只是"主任务怎么不动了"。
    let (hint, color) = if state.waiting > 0 {
        (
            format!(
                "⚠ 外面有 {} 个请求在等你答 · Esc 退出侧问区去答",
                state.waiting
            ),
            COLOR_WARNING,
        )
    } else if state.streaming {
        ("Esc 退出 · 正在回答…".to_string(), COLOR_SECONDARY)
    } else if state.earlier.is_empty() && state.older == 0 {
        ("↑↓ 滚动 · Esc/Enter 退出".to_string(), COLOR_SECONDARY)
    } else {
        (
            "↑↓ 滚动 · ←→ 翻看早前 · x 清空 · Esc/Enter 退出".to_string(),
            COLOR_SECONDARY,
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {hint}"),
            Style::default().fg(color),
        ))),
        hint_row,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn state() -> BtwState {
        BtwState {
            question: "那个配置文件叫什么".into(),
            answer: "叫 settings.json，在 .atta 下面。".into(),
            streaming: false,
            scroll: 0,
            earlier: Vec::new(),
            older: 0,
            viewing: 0,
            waiting: 0,
        }
    }

    /// 画一屏，返回**去掉所有空格**的内容。
    ///
    /// TestBackend 把双宽字符画成"字 + 一个占位空格"，所以中文原样比对是对不上的
    /// ——挤掉空格之后再 `contains`。屏幕原样另外返回，断言失败时给人看。
    fn draw(s: &BtwState, w: u16, h: u16) -> (String, String) {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, f.area(), s)).unwrap();
        let buf = t.backend().buffer().clone();
        let screen = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let squeezed = screen.chars().filter(|c| !c.is_whitespace()).collect();
        (squeezed, screen)
    }

    /// 半屏是需求定的，别悄悄改。
    #[test]
    fn it_takes_about_half_the_canvas() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        };
        assert_eq!(height(area), 20);
    }

    /// 屏幕矮到装不下自己时，占满也不要画一个装不下自己的框。
    #[test]
    fn on_a_tiny_screen_it_takes_what_there_is() {
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 5,
        };
        assert_eq!(height(tiny), 5);
    }

    #[test]
    fn it_shows_the_question_the_answer_and_its_own_hint_bar() {
        let (found, screen) = draw(&state(), 60, 12);
        assert!(found.contains("那个配置文件叫什么"), "{screen}");
        assert!(found.contains("settings.json"), "{screen}");
        assert!(
            screen.contains("Esc"),
            "它盖住了底栏，必须自己说清楚怎么出去:\n{screen}"
        );
    }

    /// 还在等模型说话的时候要有明确的"在想"，否则半屏空白看着像卡死。
    #[test]
    fn while_streaming_it_says_so_instead_of_showing_a_blank_panel() {
        let mut s = state();
        s.answer.clear();
        s.streaming = true;
        let (found, screen) = draw(&s, 60, 12);
        assert!(found.contains("想一下"), "{screen}");
        assert!(found.contains("正在回答"), "{screen}");
    }

    /// 早前问答暗色列在上面，更早的只报个数。
    #[test]
    fn earlier_questions_are_listed_above_with_a_count_of_the_rest() {
        let mut s = state();
        s.earlier = vec!["第一问".into(), "第二问".into()];
        s.older = 7;
        let (found, screen) = draw(&s, 60, 14);
        assert!(found.contains("第一问"), "{screen}");
        assert!(found.contains("还有7条更早的"), "{screen}");
        assert!(found.contains("x清空"), "有早前问答时才提示清空:\n{screen}");
    }

    /// 翻到早前那条时，标题要说清楚"你看的不是最新的"。
    #[test]
    fn stepping_back_says_which_one_you_are_looking_at() {
        let mut s = state();
        s.viewing = 2;
        let (found, screen) = draw(&s, 60, 12);
        assert!(found.contains("第2条"), "{screen}");
    }

    /// 侧问区盖住了审批框，那期间到达的请求在屏幕上没有任何痕迹，主 turn 停着等，
    /// 五分钟后被引擎拒掉。至少得让人知道有东西在等、知道怎么去答。
    #[test]
    fn it_says_when_something_outside_is_waiting_to_be_answered() {
        let mut s = state();
        s.waiting = 2;
        let (found, screen) = draw(&s, 60, 12);
        assert!(found.contains("2个请求在等你答"), "{screen}");
        assert!(found.contains("Esc退出侧问区去答"), "{screen}");
    }

    /// 而且**流式期间也要说** —— 模型跑着工具、你在这儿问闲话，正是最容易撞上的
    /// 时候，那会儿的状态栏本来只写"正在回答…"。
    #[test]
    fn the_warning_outranks_the_streaming_hint() {
        let mut s = state();
        s.answer.clear();
        s.streaming = true;
        s.waiting = 1;
        let (found, screen) = draw(&s, 60, 12);
        assert!(found.contains("1个请求在等你答"), "{screen}");
    }

    /// 长回答滚得到底。夹回去的上限得按**软换行之后**占几行算——按条数算的话，
    /// 一段中文回答的最后几行永远露不出来，而结论恰好在那儿。
    #[test]
    fn a_long_wrapped_answer_can_be_scrolled_all_the_way_down() {
        let mut s = state();
        let body: Vec<String> = (0..30)
            .map(|i| format!("第{i}段，这一段长到在四十列的框里要折成好几行才放得下。"))
            .collect();
        s.answer = format!("{}\n结尾标记", body.join("\n"));
        s.scroll = 9999;
        let (found, screen) = draw(&s, 40, 12);
        assert!(
            found.contains("结尾标记"),
            "滚到底应该看得见最后一行:\n{screen}"
        );
    }

    /// 滚动偏移超出范围时要夹回来——答案变短之后悬空的偏移会让整片变空白。
    #[test]
    fn an_out_of_range_scroll_does_not_blank_the_panel() {
        let mut s = state();
        s.scroll = 9999;
        let (found, screen) = draw(&s, 60, 12);
        assert!(
            found.contains("settings.json"),
            "夹回去之后至少还看得见最后几行:\n{screen}"
        );
    }
}
