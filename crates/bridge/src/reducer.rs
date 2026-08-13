//! `AgentEvent` 归约器 — 消费 `EventReceiver`，维护富领域模型，派生 `tui::FrameState`。
//!
//! `tui::FrameState` 混合了两类信息：Core 权威状态（转录、权限请求、子代理、用量）
//! 和纯 UI-本地状态（草稿/光标/滚动位置/补全弹窗）。这里派生的快照只负责前者——
//! composer 的编辑器/滚动字段留默认值，由 `crates/app` 的事件循环在渲染前用本地
//! UI 状态覆盖（bridge 不知道、也不需要知道用户正在编辑到第几个字符）。

use base::interface::event::AgentEvent;
use runtime::agent::EventReceiver;
use std::sync::Mutex;
use tokio::sync::watch;
use tui::frame_state::*;

const FOLD_LINE_THRESHOLD: usize = 8;

pub struct Reducer {
    state: Mutex<DomainState>,
    frame_tx: watch::Sender<FrameState>,
}

struct DomainState {
    turns: Vec<Turn>,
    pending_approvals: Vec<PendingApproval>,
    sub_agents: Vec<SubAgentInfo>,
    usage: SessionUsageState,
    status: Option<StatusContent>,
    model_name: String,
    cwd: String,
}

struct Turn {
    id: String,
    blocks: Vec<Block>,
}

enum Block {
    UserPrompt(String),
    AssistantText(String),
    Tool {
        id: String,
        name: String,
        input_summary: String,
        result: Option<ToolOutcome>,
        expanded: bool,
    },
    Note(String),
    Error(String),
}

struct ToolOutcome {
    text: String,
    is_error: bool,
}

struct PendingApproval {
    prompt_id: String,
    tool_name: String,
    message: String,
}

struct SubAgentInfo {
    agent_id: String,
    state: SubAgentState,
    elapsed_or_status: String,
}

impl Reducer {
    /// 启动归约循环：后台 task 消费 `event_rx`，每次事件后重新派生并广播 `FrameState`。
    /// 返回 `Reducer`（供 `EngineHandle` 驱动 `begin_turn`/`toggle_expand`/`resolve_prompt`）
    /// 和渲染循环订阅用的 `watch::Receiver`。
    pub fn spawn(
        mut event_rx: EventReceiver,
        model_name: String,
        cwd: String,
    ) -> (std::sync::Arc<Self>, watch::Receiver<FrameState>) {
        let (reducer, frame_rx) = Self::new(model_name, cwd);

        let task_reducer = reducer.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                task_reducer.apply_event(event);
            }
        });

        (reducer, frame_rx)
    }

    fn new(model_name: String, cwd: String) -> (std::sync::Arc<Self>, watch::Receiver<FrameState>) {
        let initial = DomainState {
            turns: Vec::new(),
            pending_approvals: Vec::new(),
            sub_agents: Vec::new(),
            usage: SessionUsageState::default(),
            status: None,
            model_name,
            cwd,
        };
        let initial_frame = render(&initial);
        let (frame_tx, frame_rx) = watch::channel(initial_frame);
        let reducer = std::sync::Arc::new(Self {
            state: Mutex::new(initial),
            frame_tx,
        });
        (reducer, frame_rx)
    }

    /// 用户提交文本：立即回显为一个新 turn（UserPrompt 块），返回生成的 turn_id
    /// 供调用方送入 `InputMessage::User`。
    pub fn begin_turn(&self, text: String) -> String {
        let turn_id = base::id::Id::new().to_string();
        let mut state = self.state.lock().unwrap();
        state.turns.push(Turn {
            id: turn_id.clone(),
            blocks: vec![Block::UserPrompt(text)],
        });
        self.broadcast(&state);
        turn_id
    }

    /// 用户对某个待确认请求做出决定：立即从 pending 列表移除（不等 Core 确认）。
    pub fn resolve_prompt(&self, prompt_id: &str) {
        let mut state = self.state.lock().unwrap();
        state.pending_approvals.retain(|p| p.prompt_id != prompt_id);
        self.broadcast(&state);
    }

    /// 翻转某个可折叠工具块的展开态，重新派生并广播。
    pub fn toggle_expand(&self, block_id: &str) {
        let mut state = self.state.lock().unwrap();
        for turn in state.turns.iter_mut() {
            for block in turn.blocks.iter_mut() {
                if let Block::Tool { id, expanded, .. } = block {
                    if id == block_id {
                        *expanded = !*expanded;
                    }
                }
            }
        }
        self.broadcast(&state);
    }

    fn apply_event(&self, event: AgentEvent) {
        let mut state = self.state.lock().unwrap();
        match event {
            AgentEvent::TextDelta { text, turn_id } => {
                let turn = find_or_create_turn(&mut state.turns, &turn_id);
                match turn.blocks.last_mut() {
                    Some(Block::AssistantText(buf)) => buf.push_str(&text),
                    _ => turn.blocks.push(Block::AssistantText(text)),
                }
            }
            AgentEvent::ToolUse {
                id,
                name,
                input,
                turn_id,
            } => {
                let input_summary = summarize_input(&input);
                let turn = find_or_create_turn(&mut state.turns, &turn_id);
                turn.blocks.push(Block::Tool {
                    id,
                    name,
                    input_summary,
                    result: None,
                    expanded: false,
                });
            }
            AgentEvent::ToolResult {
                id,
                content,
                is_error,
                turn_id,
                ..
            } => {
                let turn = find_or_create_turn(&mut state.turns, &turn_id);
                let matched = turn.blocks.iter_mut().rev().find_map(|b| match b {
                    Block::Tool {
                        id: block_id,
                        result,
                        ..
                    } if *block_id == id => Some(result),
                    _ => None,
                });
                match matched {
                    Some(result) => {
                        *result = Some(ToolOutcome {
                            text: content,
                            is_error: is_error.unwrap_or(false),
                        });
                    }
                    None => {
                        // ToolResult 没有对应的 ToolUse（理论上不该发生）——作为独立笔记落地，
                        // 保证事件不会静默丢失。
                        turn.blocks
                            .push(Block::Note(format!("[orphan tool result {id}] {content}")));
                    }
                }
            }
            AgentEvent::PermissionPrompt {
                prompt_id,
                tool_name,
                message,
                ..
            } => {
                state.pending_approvals.push(PendingApproval {
                    prompt_id,
                    tool_name,
                    message,
                });
            }
            AgentEvent::TurnComplete { usage, .. } => {
                state.usage.token_in += usage.input_tokens as u64;
                state.usage.token_out += usage.output_tokens as u64;
                state.usage.turn_count += 1;
                state.status = None;
            }
            AgentEvent::SystemInit { .. } => {}
            AgentEvent::System { message } => {
                push_note(&mut state.turns, message);
            }
            AgentEvent::CompactAction { .. } => {
                // 折叠开始；结束由下一条事件（通常紧跟 TurnComplete 或另一条
                // TextDelta）隐式清掉 status，这里只标记进入折叠。
                state.status = None;
            }
            AgentEvent::SessionChanged { session_id } => {
                push_note(&mut state.turns, format!("session changed: {session_id}"));
            }
            AgentEvent::SessionPersisted { .. } => {}
            AgentEvent::AgentSpawned { agent_id, .. } => {
                state.sub_agents.push(SubAgentInfo {
                    agent_id,
                    state: SubAgentState::Running,
                    elapsed_or_status: "running".into(),
                });
            }
            AgentEvent::AgentCompleted {
                agent_id, outcome, ..
            } => {
                if let Some(a) = state.sub_agents.iter_mut().find(|a| a.agent_id == agent_id) {
                    a.state = SubAgentState::Done;
                    a.elapsed_or_status = outcome;
                }
            }
            AgentEvent::Error {
                message, turn_id, ..
            } => {
                let turn = find_or_create_turn(&mut state.turns, &turn_id);
                turn.blocks.push(Block::Error(message));
            }
        }
        self.broadcast(&state);
    }

    fn broadcast(&self, state: &DomainState) {
        let _ = self.frame_tx.send(render(state));
    }
}

fn find_or_create_turn<'a>(turns: &'a mut Vec<Turn>, turn_id: &str) -> &'a mut Turn {
    if let Some(idx) = turns.iter().position(|t| t.id == turn_id) {
        return &mut turns[idx];
    }
    turns.push(Turn {
        id: turn_id.to_string(),
        blocks: Vec::new(),
    });
    turns.last_mut().unwrap()
}

fn push_note(turns: &mut [Turn], message: String) {
    if let Some(turn) = turns.last_mut() {
        turn.blocks.push(Block::Note(message));
    }
}

fn summarize_input(input: &serde_json::Value) -> String {
    let s = serde_json::to_string(input).unwrap_or_default();
    if s.len() > 80 {
        format!("{}…", &s[..80])
    } else {
        s
    }
}

fn render(state: &DomainState) -> FrameState {
    let mut entries = Vec::new();
    for turn in &state.turns {
        for block in &turn.blocks {
            push_block_entries(&mut entries, block);
        }
    }

    let approval = if state.pending_approvals.is_empty() {
        None
    } else {
        Some(ApprovalState {
            pending: state
                .pending_approvals
                .iter()
                .map(|p| ApprovalRequest {
                    prompt_id: p.prompt_id.clone(),
                    tool_name: p.tool_name.clone(),
                    message: p.message.clone(),
                    options: vec![
                        ApprovalOption::PermitOnce,
                        ApprovalOption::PermitSession,
                        ApprovalOption::PermitProject,
                        ApprovalOption::Deny,
                    ],
                    selected_option: 0,
                })
                .collect(),
            active_idx: 0,
            view_mode: ApprovalViewMode::TabView,
        })
    };
    let locked = approval.is_some();

    FrameState {
        transcript: TranscriptState {
            header: HeaderState {
                text: None,
                source: HeaderSource::None,
            },
            body: TranscriptBodyState {
                scroll: ScrollState {
                    offset: 0,
                    total_lines: entries.len(),
                },
                entries,
                auto_follow: true,
            },
        },
        operation_status: OperationStatusState {
            status_line: StatusLineState {
                content: state.status.clone(),
            },
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
                    locked,
                },
                completion: None,
                approval,
            },
            bottom_rule: BottomRuleState {
                color: SeparatorColor::DarkGray,
            },
        },
        sub_agent_bar: SubAgentBarState {
            agents: state
                .sub_agents
                .iter()
                .map(|a| SubAgentStatus {
                    name: a.agent_id.clone(),
                    state: a.state,
                    token_usage: 0,
                    elapsed_or_status: a.elapsed_or_status.clone(),
                })
                .collect(),
        },
        footer_hints: FooterHintsState {
            model: state.model_name.clone(),
            cwd: state.cwd.clone(),
            mode: AppMode::Normal,
            right_hint: "/=cmds · @=file · F4=help".into(),
            usage: state.usage,
        },
    }
}

fn push_block_entries(entries: &mut Vec<TranscriptEntry>, block: &Block) {
    match block {
        Block::UserPrompt(text) => entries.push(plain(LineKind::UserPrompt, text)),
        Block::AssistantText(text) => entries.push(plain(LineKind::AssistantText, text)),
        Block::Note(text) => entries.push(plain(LineKind::Note, text)),
        Block::Error(text) => entries.push(plain(LineKind::Error, text)),
        Block::Tool {
            id,
            name,
            input_summary,
            result,
            expanded,
        } => {
            entries.push(TranscriptEntry {
                kind: LineKind::ToolHeading,
                text: format!("{name}({input_summary})"),
                block_id: Some(id.clone()),
            });
            let Some(result) = result else { return };
            let kind = if result.is_error {
                LineKind::ToolResultErr
            } else {
                LineKind::ToolResultOk
            };
            let lines: Vec<&str> = result.text.lines().collect();
            if lines.len() <= FOLD_LINE_THRESHOLD || *expanded {
                if lines.is_empty() {
                    entries.push(TranscriptEntry {
                        kind,
                        text: String::new(),
                        block_id: Some(id.clone()),
                    });
                }
                for line in &lines {
                    entries.push(TranscriptEntry {
                        kind,
                        text: line.to_string(),
                        block_id: Some(id.clone()),
                    });
                }
            } else {
                for line in &lines[..FOLD_LINE_THRESHOLD] {
                    entries.push(TranscriptEntry {
                        kind,
                        text: line.to_string(),
                        block_id: Some(id.clone()),
                    });
                }
                let hidden = lines.len() - FOLD_LINE_THRESHOLD;
                entries.push(TranscriptEntry {
                    kind: LineKind::Note,
                    text: format!("… {hidden} more lines (toggle to expand)"),
                    block_id: Some(id.clone()),
                });
            }
        }
    }
}

fn plain(kind: LineKind, text: &str) -> TranscriptEntry {
    TranscriptEntry {
        kind,
        text: text.to_string(),
        block_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `watch::Sender::send` is a no-op once the receiver count drops to zero, so tests
    // must keep the receiver alive for the whole test (bridge's real `spawn` doesn't hit
    // this because `EngineHandle::subscribe` always holds a live clone).
    fn reducer() -> (std::sync::Arc<Reducer>, watch::Receiver<FrameState>) {
        Reducer::new("test-model".into(), "/tmp".into())
    }

    fn frame(rx: &watch::Receiver<FrameState>) -> FrameState {
        rx.borrow().clone()
    }

    #[test]
    fn begin_turn_echoes_user_prompt() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("hello".into());
        assert!(!turn_id.is_empty());
        let f = frame(&rx);
        assert_eq!(f.transcript.body.entries.len(), 1);
        assert_eq!(f.transcript.body.entries[0].kind, LineKind::UserPrompt);
        assert_eq!(f.transcript.body.entries[0].text, "hello");
    }

    #[test]
    fn text_delta_accumulates_within_a_turn() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::TextDelta {
            text: "Hel".into(),
            turn_id: turn_id.clone(),
        });
        r.apply_event(AgentEvent::TextDelta {
            text: "lo".into(),
            turn_id,
        });
        let f = frame(&rx);
        let assistant: Vec<_> = f
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::AssistantText)
            .collect();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0].text, "Hello");
    }

    #[test]
    fn tool_use_and_result_pair_by_id() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::ToolUse {
            id: "t1".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "a.rs"}),
            turn_id: turn_id.clone(),
        });
        r.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "Read".into(),
            content: "line1".into(),
            is_error: Some(false),
            turn_id,
        });
        let f = frame(&rx);
        let heading = f
            .transcript
            .body
            .entries
            .iter()
            .find(|e| e.kind == LineKind::ToolHeading)
            .unwrap();
        assert_eq!(heading.block_id.as_deref(), Some("t1"));
        let result = f
            .transcript
            .body
            .entries
            .iter()
            .find(|e| e.kind == LineKind::ToolResultOk)
            .unwrap();
        assert_eq!(result.text, "line1");
        assert_eq!(result.block_id.as_deref(), Some("t1"));
    }

    #[test]
    fn large_tool_result_folds_then_expands_on_toggle() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        let long_output = (0..20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.apply_event(AgentEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
            turn_id: turn_id.clone(),
        });
        r.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "Bash".into(),
            content: long_output,
            is_error: Some(false),
            turn_id,
        });

        let collapsed = frame(&rx);
        let fold_note = collapsed
            .transcript
            .body
            .entries
            .iter()
            .find(|e| e.kind == LineKind::Note && e.block_id.as_deref() == Some("t1"));
        assert!(
            fold_note.is_some(),
            "expected a folded summary note for a 20-line result"
        );
        let result_lines = collapsed
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::ToolResultOk)
            .count();
        assert_eq!(result_lines, FOLD_LINE_THRESHOLD);

        r.toggle_expand("t1");
        let expanded = frame(&rx);
        let result_lines = expanded
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::ToolResultOk)
            .count();
        assert_eq!(result_lines, 20);
    }

    #[test]
    fn permission_prompt_populates_approval_and_resolve_clears_it() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::PermissionPrompt {
            prompt_id: "p1".into(),
            tool_name: "Bash".into(),
            message: "run rm -rf?".into(),
            paths: vec![],
            turn_id,
        });
        let f = frame(&rx);
        assert!(f.composer.content.approval.is_some());
        assert!(f.composer.content.editor.locked);

        r.resolve_prompt("p1");
        let f = frame(&rx);
        assert!(f.composer.content.approval.is_none());
        assert!(!f.composer.content.editor.locked);
    }

    #[test]
    fn turn_complete_accumulates_session_usage() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::TurnComplete {
            stop_reason: "end_turn".into(),
            api_calls: 1,
            tool_calls: 0,
            usage: base::interface::model::Usage {
                input_tokens: 100,
                output_tokens: 40,
            },
            turn_id,
        });
        let f = frame(&rx);
        assert_eq!(f.footer_hints.usage.token_in, 100);
        assert_eq!(f.footer_hints.usage.token_out, 40);
        assert_eq!(f.footer_hints.usage.turn_count, 1);
    }
}
