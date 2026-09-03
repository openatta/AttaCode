//! `btw` — Side Question / 侧问区。
//!
//! 激活时独占屏幕下半：转录区压到约一半，这里盖住状态区、输入区、底栏和子代理条。
//!
//! **它自带一条状态栏**，因为它把底栏盖住了——用户在这期间没有别的地方可以看见
//! "现在能按什么"。这条线也是为什么这个区域画得起边框：它是一个明确"进去了、要出来"
//! 的模式，边框让人一眼知道自己不在主 UI 里。

use crate::frame_state::BtwState;
use crate::regions::style::{COLOR_ACCENT, COLOR_SECONDARY};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
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
    let max_skip = lines.len().saturating_sub(body.height as usize);
    let skip = state.scroll.min(max_skip);
    let shown: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
    frame.render_widget(Paragraph::new(shown).wrap(Wrap { trim: false }), body);

    let hint = if state.streaming {
        "Esc 退出 · 正在回答…"
    } else if state.earlier.is_empty() && state.older == 0 {
        "↑↓ 滚动 · Esc/Enter 退出"
    } else {
        "↑↓ 滚动 · ←→ 翻看早前 · x 清空 · Esc/Enter 退出"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {hint}"),
            Style::default().fg(COLOR_SECONDARY),
        ))),
        hint_row,
    );
    let _ = Color::Reset;
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
