//! 整屏渲染快照 —— 唯一一层"把 `FrameState` 画成人能看的样子"的断言。
//!
//! 之前所有测试都停在数据层（`LineKind` 序列、字段值），于是"同一句话在屏幕上
//! 出现了两次"这种问题一个都发现不了，只有真机跑起来才看得见。这里用
//! `TestBackend` 把整帧画出来比对文本：不需要终端、不需要 API key，能进 CI。
//!
//! 断言的是**文本版面**，不是颜色/样式——样式在各 region 的单测里按 span 断言。
//!
//! 精确比对的用例一律用 ASCII 文本：宽字符在 `TestBackend` 里占两格，第二格是
//! 空的，逐格读出来会变成"看 一 下"，那是量具的形状不是程序的。中文只在
//! `contains` 类断言里出现。

use tui::frame_state::*;
use tui::FrameState;

const W: u16 = 60;
const H: u16 = 12;
const RULE: &str = "────────────────────────────────────────────────────────────";
const FOOTER: &str = "claude-opus-5 · /w  [Normal] [Plan] [Auto] 0↑ 0↓  /=cmds";

/// 画一帧，返回逐行文本（右侧空白去掉）。行数恒等于终端高度。
fn draw_at(frame: &FrameState, w: u16, h: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| tui::layout::render(f, f.area(), frame, '|'))
        .unwrap();
    let buf = terminal.backend().buffer();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn draw(frame: &FrameState) -> Vec<String> {
    draw_at(frame, W, H)
}

/// `["a", "b"]` + 补空行到 `h` 行高——转录区没占满时下面就是空的。
fn transcript_then(lines: &[&str], tail: &[&str], h: usize) -> Vec<String> {
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    out.resize(h - tail.len(), String::new());
    out.extend(tail.iter().map(|s| s.to_string()));
    out
}

fn entry(kind: LineKind, text: &str, block: Option<&str>) -> TranscriptEntry {
    TranscriptEntry {
        starts_segment: false,
        kind,
        text: text.into(),
        block_id: block.map(str::to_string),
    }
}

/// 一帧最小可用的快照，各测试在它上面改自己关心的那部分。
fn base() -> FrameState {
    FrameState {
        transcript: TranscriptState {
            header: HeaderState {
                text: None,
                source: HeaderSource::None,
            },
            body: TranscriptBodyState {
                entries: vec![],
                scroll: ScrollState {
                    offset: 0,
                    total_lines: 0,
                },
                auto_follow: true,
                selected_block: None,
            },
        },
        operation_status: OperationStatusState {
            status_line: StatusLineState { content: None },
            task_list: TaskListState { items: vec![] },
        },
        composer: ComposerState {
            app_info: AppInfoLineState { text: None },
            top_rule: TopRuleState {
                color: SeparatorColor::DarkGray,
                right_label: None,
            },
            content: ContentState {
                editor: EditorState {
                    mode: InputMode::Normal,
                    draft: String::new(),
                    cursor: 0,
                    paste_placeholder: None,
                    locked: false,
                },
                picker: None,
                ask: None,
            },
            bottom_rule: BottomRuleState {
                color: SeparatorColor::DarkGray,
            },
        },
        sub_agent_bar: SubAgentBarState { agents: vec![] },
        footer_hints: FooterHintsState {
            model: "claude-opus-5".into(),
            cwd: "/w".into(),
            mode: AppMode::Normal,
            right_hint: "/=cmds".into(),
            usage: SessionUsageState::default(),
        },
    }
}

#[test]
fn an_empty_session_is_just_the_composer_and_the_footer() {
    assert_eq!(
        draw(&base()),
        transcript_then(&[], &[RULE, "> █", RULE, FOOTER], H as usize)
    );
}

/// 一轮完整的问答 + 一个工具块：转录的基本形状。
/// 注意转录行左边那一列是**选中竖条的槽位**（没选中时是空格），composer 没有。
#[test]
fn a_turn_with_a_tool_call_renders_prompt_answer_and_tool_block() {
    let mut f = base();
    f.transcript.body.entries = vec![
        entry(LineKind::UserPrompt, "read Cargo.toml", None),
        entry(LineKind::ToolHeading, "Read(Cargo.toml)", Some("t1")),
        entry(LineKind::ToolResultOk, "[workspace]", Some("t1")),
        entry(LineKind::AssistantText, "it is a workspace.", None),
    ];
    assert_eq!(
        draw(&f),
        transcript_then(
            &[
                " > read Cargo.toml",
                " ⏺ Read(Cargo.toml)",
                "   ✓ [workspace]",
                "   it is a workspace.",
            ],
            &[RULE, "> █", RULE, FOOTER],
            H as usize
        )
    );
}

/// diff 行靠前缀区分（`-`/`+`/上下文缩进），一眼要能看出来。
#[test]
fn diff_lines_are_visually_distinct() {
    let mut f = base();
    f.transcript.body.entries = vec![
        entry(LineKind::ToolHeading, "Edit(a.rs)", Some("t1")),
        entry(LineKind::DiffContext, "@@ -1,2 +1,2 @@", Some("t1")),
        entry(LineKind::DiffOld, "    old();", Some("t1")),
        entry(LineKind::DiffNew, "    new();", Some("t1")),
    ];
    assert_eq!(
        draw(&f),
        transcript_then(
            &[
                " ⏺ Edit(a.rs)",
                "     @@ -1,2 +1,2 @@",
                "   -     old();",
                "   +     new();",
            ],
            &[RULE, "> █", RULE, FOOTER],
            H as usize
        )
    );
}

/// 选中块的竖条只画在**这个块**的行上。
#[test]
fn the_selection_gutter_marks_exactly_one_block() {
    let mut f = base();
    f.transcript.body.entries = vec![
        entry(LineKind::ToolHeading, "Read(a)", Some("t1")),
        entry(LineKind::ToolResultOk, "aaa", Some("t1")),
        entry(LineKind::ToolHeading, "Read(b)", Some("t2")),
        entry(LineKind::ToolResultOk, "bbb", Some("t2")),
    ];
    f.transcript.body.selected_block = Some("t2".into());
    assert_eq!(
        draw(&f),
        transcript_then(
            &[" ⏺ Read(a)", "   ✓ aaa", "▌⏺ Read(b)", "▌  ✓ bbb",],
            &[RULE, "> █", RULE, FOOTER],
            H as usize
        )
    );
}

/// 权限对话框弹出时：输入框锁住（不画光标）、对话框压在它上面。
#[test]
fn an_open_approval_dialog_locks_the_composer() {
    let mut f = base();
    f.composer.content.editor.locked = true;
    f.composer.content.ask = Some(AskState {
        pending: vec![AskRequest {
            answer_with: AnswerWith::Choose,
            prompt_id: "p1".into(),
            tool_name: "Bash".into(),
            message: "rm -rf /tmp/x".into(),
            options: vec![AskOption::PermitOnce, AskOption::Deny],
            selected_option: 1,
        }],
        active_idx: 0,
        view_mode: AskViewMode::TabView,
    });
    let screen = draw_at(&f, W, 16).join("\n");
    for expected in [
        "│Bash:",
        "│  rm -rf /tmp/x",
        "  ❯ No",
        "Enter=confirm  Esc=deny",
    ] {
        assert!(screen.contains(expected), "缺 {expected:?}:\n{screen}");
    }
    assert!(!screen.contains('█'), "锁住时不该画光标:\n{screen}");
}

/// 终端矮到放不下整张卡片时，**先砍说明文字**——选项和按键提示必须活下来，
/// 否则用户面对一个不知道怎么答的对话框。被砍的地方留个 `…`。
#[test]
fn a_cramped_approval_dialog_keeps_the_options_and_the_key_hint() {
    let mut f = base();
    f.composer.content.editor.locked = true;
    f.composer.content.ask = Some(AskState {
        pending: vec![AskRequest {
            answer_with: AnswerWith::Choose,
            prompt_id: "p1".into(),
            tool_name: "Bash".into(),
            message: (1..=8)
                .map(|i| format!("说明第{i}行"))
                .collect::<Vec<_>>()
                .join("\n"),
            options: vec![AskOption::PermitOnce, AskOption::Deny],
            selected_option: 0,
        }],
        active_idx: 0,
        view_mode: AskViewMode::TabView,
    });
    let screen = draw_at(&f, W, 12).join("\n");
    for expected in ["Bash:", "❯ Yes", "No", "Enter=confirm  Esc=deny", "…"] {
        assert!(screen.contains(expected), "缺 {expected:?}:\n{screen}");
    }
    assert!(
        !screen.contains("说明第8行"),
        "挤不下时该砍的是说明文字:\n{screen}"
    );
}

/// 多个待确认请求：顶上出现 tab 条，当前那个高亮，提示里给出切换键。
/// 提示写的键必须真绑得上——以前写的是 `Ctrl-Tab`，而传统终端根本区分不出
/// Ctrl+Tab 和 Tab，等于教用户按一个不存在的键。
#[test]
fn several_pending_approvals_get_a_tab_strip() {
    let mut f = base();
    f.composer.content.editor.locked = true;
    let req = |id: &str, tool: &str| AskRequest {
        answer_with: AnswerWith::Choose,
        prompt_id: id.into(),
        tool_name: tool.into(),
        message: format!("{tool} wants in"),
        options: vec![AskOption::PermitOnce, AskOption::Deny],
        selected_option: 0,
    };
    f.composer.content.ask = Some(AskState {
        pending: vec![req("p1", "Bash"), req("p2", "Write")],
        active_idx: 1,
        view_mode: AskViewMode::TabView,
    });
    let screen = draw_at(&f, W, 18).join("\n");
    assert!(screen.contains("[Bash#1]"), "缺 tab 条:\n{screen}");
    assert!(screen.contains("[Write#2]"), "缺 tab 条:\n{screen}");
    assert!(
        screen.contains("│Write:"),
        "画的应该是 active_idx 指的那个:\n{screen}"
    );
    assert!(
        screen.contains("Tab=next"),
        "提示要给出真能按的键:\n{screen}"
    );
}

/// 状态行 + 任务清单 + 子代理条同屏。终端要够高，否则子代理条会被挤掉——
/// 这本身也是要盯的：三块信息抢的是同一片纵向空间。
#[test]
fn a_running_turn_shows_status_tasks_and_sub_agents() {
    let mut f = base();
    f.operation_status.status_line.content = Some(StatusContent::TurnRunning {
        spinner: '|',
        activity: "Running Bash".into(),
        elapsed_secs: 12,
        token_in: 1500,
        token_out: 300,
    });
    f.operation_status.task_list.items = vec![
        TaskItem {
            status: ItemStatus::Done,
            label: "read code".into(),
        },
        TaskItem {
            status: ItemStatus::Running,
            label: "run tests".into(),
        },
    ];
    f.sub_agent_bar.agents = vec![SubAgentStatus {
        name: "explore#3f2a1b7c".into(),
        state: SubAgentState::Running,
        token_usage: 150,
        elapsed_or_status: "3s".into(),
    }];
    f.footer_hints.usage = SessionUsageState {
        token_in: 1500,
        token_out: 300,
        turn_count: 1,
    };
    let screen = draw_at(&f, W, 16).join("\n");
    for expected in [
        "Running Bash",
        "12s",
        "✓ read code",
        "● run tests",
        "explore#3f2a1b7c",
        "150 tok",
        "1.5k↑ 300↓", // footer 会把上千的数缩写
    ] {
        assert!(screen.contains(expected), "缺 {expected:?}:\n{screen}");
    }

    // 自上而下的堆叠顺序。只断言"内容都在"是不够的——那样任何一次调序都测不出来，
    // 而屏幕上东西的位置正是这份布局的全部意义。
    let rows = draw_at(&f, W, 16);
    let row_of = |needle: &str| {
        rows.iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("找不到 {needle:?}:\n{}", rows.join("\n")))
    };
    let status = row_of("Running Bash");
    let tasks = row_of("✓ read code");
    let footer = row_of("1.5k↑ 300↓");
    let agents = row_of("explore#3f2a1b7c");
    assert!(status < tasks, "状态行在任务清单上面");
    assert!(tasks < footer, "任务清单在输入区/底栏上面");
    assert!(
        footer < agents,
        "底栏在子代理条**上面**：底栏常驻，子代理条条件显示，\
         常驻的那条得钉在固定位置上"
    );
}

/// 补全弹窗浮在输入框**上方**，选中项带 `❯`。
#[test]
fn the_completion_popup_floats_above_the_editor() {
    let mut f = base();
    f.composer.content.editor.draft = "/mod".into();
    f.composer.content.editor.cursor = 4;
    f.composer.content.picker = Some(PickerState {
        kind: PickerKind::SlashCommand,
        query: "mod".into(),
        candidates: vec![PickerCandidate {
            name: "/model".into(),
            description: "Switch the model".into(),
        }],
        selected: 0,
    });
    let lines = draw(&f);
    let screen = lines.join("\n");
    assert!(screen.contains("❯ /model"), "{screen}");
    assert!(screen.contains("> /mod█"), "草稿和光标照常:\n{screen}");
    let popup_row = lines.iter().position(|l| l.contains("/model")).unwrap();
    let editor_row = lines.iter().position(|l| l.contains("> /mod")).unwrap();
    assert!(popup_row < editor_row, "弹窗要在输入框上方:\n{screen}");
}

/// 滚上去之后：视口从 offset 起，底部有"上面还有多少行"的提示条。
#[test]
fn a_scrolled_transcript_shows_the_lines_above_indicator() {
    let mut f = base();
    f.transcript.body.entries = (0..40)
        .map(|i| entry(LineKind::AssistantText, &format!("line{i}"), None))
        .collect();
    f.transcript.body.auto_follow = false;
    f.transcript.body.scroll.offset = 20;
    let screen = draw(&f).join("\n");
    assert!(screen.contains("line20"), "视口从 offset 起:\n{screen}");
    assert!(
        !screen.contains("line19"),
        "offset 之前的不该出现:\n{screen}"
    );
    assert!(screen.contains("20 lines above"), "{screen}");
}

/// 窄终端不该错位/panic——真机验收清单里"拉宽拉窄"那一条的自动化版本。
#[test]
fn a_narrow_terminal_still_renders_every_region() {
    let mut f = base();
    f.transcript.body.entries = vec![entry(LineKind::AssistantText, "hello", None)];
    for (w, h) in [(20, 8), (40, 10), (200, 30)] {
        let lines = draw_at(&f, w, h);
        assert_eq!(lines.len(), h as usize, "{w}x{h} 行数不对");
        assert!(
            lines.iter().any(|l| l.contains("hello")),
            "{w}x{h} 转录没画出来"
        );
        assert!(
            lines.iter().any(|l| l.contains("claude-opus-5")),
            "{w}x{h} footer 没画出来"
        );
    }
}
