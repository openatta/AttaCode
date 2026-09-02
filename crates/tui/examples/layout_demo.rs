//! Self-cycling visual demo — drives `FrameState` through a scripted scenario sequence so the
//! whole Z0..Z4 layout can be eyeballed live. Run with: cargo run -p tui --example layout_demo
//!
//! Controls: q/Esc/Ctrl-C quit · space pause/resume auto-advance · n/p next/prev scenario.

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::stdout;
use std::time::{Duration, Instant};
use tui::frame_state::*;
use tui::regions::style::spinner_frame;

type Builder = fn(u64) -> FrameState;

struct Scenario {
    name: &'static str,
    build: Builder,
}

fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let result = run(&mut terminal);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> anyhow::Result<()> {
    let scenarios = scenarios();
    let mut idx = 0usize;
    let mut scenario_started = Instant::now();
    let mut paused = false;
    let advance_every = Duration::from_secs(2);

    loop {
        let now_ms = now_ms();
        let spinner = spinner_frame(now_ms);
        let elapsed = scenario_started.elapsed().as_secs();
        let state = (scenarios[idx].build)(elapsed);
        let name = scenarios[idx].name;

        terminal.draw(|f| {
            let full = f.area();
            let rows = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    ratatui::layout::Constraint::Min(0),
                    ratatui::layout::Constraint::Length(1),
                ])
                .split(full);
            let [app_area, hint_area]: [ratatui::layout::Rect; 2] =
                rows.as_ref().try_into().expect("2 rows");

            tui::layout::render(f, app_area, &state, spinner);

            let hint = format!(
                " [{}/{}] {}  ·  q=quit space=pause n/p=scenario  {}",
                idx + 1,
                scenarios.len(),
                name,
                if paused { "[PAUSED]" } else { "" }
            );
            f.render_widget(
                ratatui::widgets::Paragraph::new(hint)
                    .style(ratatui::style::Style::default().bg(ratatui::style::Color::DarkGray)),
                hint_area,
            );
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                    (KeyCode::Char(' '), _) => paused = !paused,
                    (KeyCode::Char('n'), _) => {
                        idx = (idx + 1) % scenarios.len();
                        scenario_started = Instant::now();
                    }
                    (KeyCode::Char('p'), _) => {
                        idx = (idx + scenarios.len() - 1) % scenarios.len();
                        scenario_started = Instant::now();
                    }
                    _ => {}
                }
            }
        }

        if !paused && scenario_started.elapsed() >= advance_every {
            idx = (idx + 1) % scenarios.len();
            scenario_started = Instant::now();
        }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ── Scenarios ──────────────────────────────────────────────────────────

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "idle",
            build: idle,
        },
        Scenario {
            name: "turn running",
            build: turn_running,
        },
        Scenario {
            name: "turn running + tasks",
            build: turn_running_with_tasks,
        },
        Scenario {
            name: "compacting",
            build: compacting,
        },
        Scenario {
            name: "scrolled header",
            build: scrolled_header,
        },
        Scenario {
            name: "slash completion",
            build: slash_completion,
        },
        Scenario {
            name: "single approval",
            build: single_approval,
        },
        Scenario {
            name: "multi approval (tabs)",
            build: multi_approval,
        },
        Scenario {
            name: "sub agents running",
            build: sub_agents,
        },
        Scenario {
            name: "app info + focus label",
            build: app_info_and_focus,
        },
        Scenario {
            name: "footer mode cycling",
            build: footer_modes,
        },
    ]
}

fn base_entries() -> Vec<TranscriptEntry> {
    vec![
        entry(LineKind::UserPrompt, "帮我看看 src/parser.rs 里的死循环"),
        entry(LineKind::ToolHeading, "Read(src/parser.rs)"),
        entry(LineKind::ToolResultOk, "read 214 lines"),
        entry(
            LineKind::AssistantText,
            "看起来问题在第 87 行的 while 循环……",
        ),
        entry(LineKind::ToolHeading, "Edit(src/parser.rs)"),
        entry(LineKind::ToolResultOk, "applied"),
        entry(LineKind::AssistantText, "修好了，加了个边界检查。"),
    ]
}

fn entry(kind: LineKind, text: &str) -> TranscriptEntry {
    TranscriptEntry {
        kind,
        text: text.to_string(),
        block_id: None,
    }
}

fn base_frame(entries: Vec<TranscriptEntry>) -> FrameState {
    FrameState {
        transcript: TranscriptState {
            header: HeaderState {
                text: None,
                source: HeaderSource::None,
            },
            body: TranscriptBodyState {
                entries,
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
                completion: None,
                approval: None,
            },
            bottom_rule: BottomRuleState {
                color: SeparatorColor::DarkGray,
            },
        },
        sub_agent_bar: SubAgentBarState { agents: vec![] },
        footer_hints: FooterHintsState {
            model: "claude-sonnet-5".into(),
            cwd: "~/proj".into(),
            mode: AppMode::Normal,
            right_hint: "/=cmds · @=file · F4=help".into(),
            usage: SessionUsageState::default(),
        },
    }
}

fn idle(_elapsed: u64) -> FrameState {
    base_frame(base_entries())
}

fn turn_running(elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    f.operation_status.status_line.content = Some(StatusContent::TurnRunning {
        spinner: spinner_frame(now_ms()),
        activity: "Editing src/parser.rs…".into(),
        elapsed_secs: elapsed,
        token_in: 1200 + elapsed * 40,
        token_out: 300 + elapsed * 15,
    });
    f.composer.top_rule.color = SeparatorColor::Cyan;
    f.composer.top_rule.right_label = Some(LabelSource::Tool {
        name: "Edit".into(),
    });
    f.composer.bottom_rule.color = SeparatorColor::Cyan;
    f.composer.content.editor.locked = true;
    f
}

fn turn_running_with_tasks(elapsed: u64) -> FrameState {
    let mut f = turn_running(elapsed);
    f.operation_status.task_list.items = vec![
        TaskItem {
            status: ItemStatus::Done,
            label: "Build passed".into(),
        },
        TaskItem {
            status: ItemStatus::Running,
            label: "Running tests…".into(),
        },
        TaskItem {
            status: ItemStatus::Pending,
            label: "Update docs".into(),
        },
    ];
    f
}

fn compacting(elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    let stage_index = ((elapsed / 2) % 3) as u8;
    f.operation_status.status_line.content = Some(StatusContent::Compacting {
        stage: match stage_index {
            0 => CompactStage::MicroCompact,
            1 => CompactStage::Collapse,
            _ => CompactStage::LlmSummarize,
        },
        stage_index,
        stage_total: 3,
        tokens_before: 128_000,
        tokens_after: if stage_index == 2 { Some(96_000) } else { None },
        estimated_saved: if stage_index == 2 { Some(32_000) } else { None },
    });
    f
}

fn scrolled_header(_elapsed: u64) -> FrameState {
    let mut entries = base_entries();
    for i in 0..20 {
        entries.push(entry(LineKind::Note, &format!("filler line {i}")));
    }
    let mut f = base_frame(entries);
    f.transcript.header = HeaderState {
        text: Some("帮我看看 src/parser.rs 里的死循环".into()),
        source: HeaderSource::UserPrompt,
    };
    f.transcript.body.auto_follow = false;
    f.transcript.body.scroll = ScrollState {
        offset: 5,
        total_lines: 27,
    };
    f
}

fn slash_completion(_elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    f.composer.content.editor.draft = "/com".into();
    f.composer.content.completion = Some(CompletionPopupState {
        kind: CompletionKind::SlashCommand,
        query: "com".into(),
        candidates: vec![
            CompletionCandidate {
                name: "/compact".into(),
                description: "compact context window".into(),
            },
            CompletionCandidate {
                name: "/command".into(),
                description: "show available commands".into(),
            },
            CompletionCandidate {
                name: "/complete".into(),
                description: "trigger completion manually".into(),
            },
        ],
        selected: 0,
    });
    f
}

fn single_approval(_elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    f.composer.content.editor.locked = true;
    f.composer.content.approval = Some(ApprovalState {
        pending: vec![ApprovalRequest {
            answer_with: AnswerWith::Choose,
            prompt_id: "demo-1".into(),
            tool_name: "Bash".into(),
            message: "git push --force origin main".into(),
            options: vec![
                ApprovalOption::PermitOnce,
                ApprovalOption::PermitProject,
                ApprovalOption::Deny,
            ],
            selected_option: 0,
        }],
        active_idx: 0,
        view_mode: ApprovalViewMode::TabView,
    });
    f
}

fn multi_approval(elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    f.composer.content.editor.locked = true;
    f.composer.content.approval = Some(ApprovalState {
        pending: vec![
            ApprovalRequest {
                answer_with: AnswerWith::Choose,
                prompt_id: "demo-1".into(),
                tool_name: "Bash".into(),
                message: "git push --force origin main".into(),
                options: vec![ApprovalOption::PermitOnce, ApprovalOption::Deny],
                selected_option: 0,
            },
            ApprovalRequest {
                answer_with: AnswerWith::Choose,
                prompt_id: "demo-2".into(),
                tool_name: "Edit".into(),
                message: "src/parser.rs".into(),
                options: vec![
                    ApprovalOption::PermitOnce,
                    ApprovalOption::PermitSession,
                    ApprovalOption::Deny,
                ],
                selected_option: 0,
            },
            ApprovalRequest {
                answer_with: AnswerWith::Choose,
                prompt_id: "demo-3".into(),
                tool_name: "Agent".into(),
                message: "spawn sub-agent: code-reviewer".into(),
                options: vec![ApprovalOption::PermitOnce, ApprovalOption::Deny],
                selected_option: 0,
            },
        ],
        active_idx: ((elapsed / 3) % 3) as usize,
        view_mode: ApprovalViewMode::TabView,
    });
    f
}

fn sub_agents(elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    f.sub_agent_bar.agents = vec![
        SubAgentStatus {
            name: "code-reviewer".into(),
            state: SubAgentState::Running,
            token_usage: 12_300 + elapsed * 100,
            elapsed_or_status: format!("{elapsed}s"),
        },
        SubAgentStatus {
            name: "test-runner".into(),
            state: SubAgentState::Done,
            token_usage: 45_800,
            elapsed_or_status: "done".into(),
        },
        SubAgentStatus {
            name: "doc-writer".into(),
            state: SubAgentState::Failed,
            token_usage: 2_100,
            elapsed_or_status: "crashed".into(),
        },
    ];
    f.composer.top_rule.right_label = Some(LabelSource::SubAgent {
        name: "code-reviewer".into(),
    });
    f.composer.top_rule.color = SeparatorColor::Cyan;
    f
}

fn app_info_and_focus(_elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    f.composer.app_info = AppInfoLineState {
        text: Some("Update available: v0.3.0 — /upgrade".into()),
    };
    f.composer.top_rule = TopRuleState {
        color: SeparatorColor::Cyan,
        right_label: Some(LabelSource::Skill {
            name: "code-review".into(),
        }),
    };
    f
}

fn footer_modes(elapsed: u64) -> FrameState {
    let mut f = base_frame(base_entries());
    f.footer_hints.mode = match (elapsed / 2) % 3 {
        0 => AppMode::Normal,
        1 => AppMode::Plan,
        _ => AppMode::Auto,
    };
    f
}
