//! `AgentEvent` 归约器 — 消费 `EventReceiver`，维护富领域模型，派生 `tui::FrameState`。
//!
//! `tui::FrameState` 混合了两类信息：Core 权威状态（转录、权限请求、子代理、用量）
//! 和纯 UI-本地状态（草稿/光标/滚动位置/补全弹窗）。这里派生的快照只负责前者——
//! composer 的编辑器/滚动字段留默认值，由 `crates/app` 的事件循环在渲染前用本地
//! UI 状态覆盖（bridge 不知道、也不需要知道用户正在编辑到第几个字符）。

use crate::commands::CommandCatalog;
use base::interface::event::AgentEvent;
use runtime::agent::EventReceiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tui::frame_state::*;

const FOLD_LINE_THRESHOLD: usize = 8;
/// Core 里维护待办清单的工具名（`core/crates/tools/src/todo_write.rs`）。
const TODO_TOOL: &str = "TodoWrite";
const STATUS_TICK: Duration = Duration::from_millis(500);
const SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];

pub struct Reducer {
    state: Mutex<DomainState>,
    frame_tx: watch::Sender<FrameState>,
    /// 收到 `AgentEvent::SkillsChanged` 时重拉命令表。归约器是事件流的唯一消费者
    /// （`EventReceiver` 是 mpsc，分不出第二个订阅方），所以这件事只能挂在这里。
    /// 单测里为 `None`——它们不关心补全弹窗。
    commands: Option<Arc<CommandCatalog>>,
}

struct DomainState {
    turns: Vec<Turn>,
    pending_approvals: Vec<PendingApproval>,
    sub_agents: Vec<SubAgentInfo>,
    /// 模型自己维护的待办清单，来自 `TodoWrite` 工具调用的入参。Core 没有"清单变了"
    /// 这类事件——清单是那个工具的 input，所以这里从 `ToolUse` 里读。每次调用是
    /// 全量替换（工具语义如此），空列表就等于收起这块区域。
    tasks: Vec<TaskItem>,
    usage: SessionUsageState,
    status: Option<StatusContent>,
    /// When the in-flight turn started, if any. Drives `StatusContent::TurnRunning`'s
    /// `elapsed_secs`/`spinner` — refreshed by the periodic tick in [`Reducer::spawn`]
    /// so the status line keeps animating even between `AgentEvent`s (e.g. while a
    /// tool call is running and no `TextDelta` is arriving).
    active_turn_started: Option<Instant>,
    /// 用户已经按过中断键、但 Core 还没回 `TurnComplete`。只影响状态行文案——
    /// turn 真正的收尾一律等 Core 的事件，不在本地抢跑。
    cancel_requested: bool,
    activity: String,
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
    Thinking(String),
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

/// 子代理条上的一行。
///
/// **键是 `SubagentProgress.agent_label`**（`explore#3f2a1b7c` 那种），不是
/// `AgentSpawned.agent_id`：全仓搜下来 Core 根本没有发过 `AgentSpawned`/
/// `AgentCompleted`（只有同名的 telemetry payload），子代理唯一的实时信号就是
/// `SubagentProgress` —— 它把子代理自己的事件流原样转发到父通道上。用量也只能
/// 从这里来：转发过来的 `TurnComplete` 里带子代理这一轮的 `usage`。
/// 那两个变体仍然照接，万一哪天 Core 真发了，它们按 `agent_id` 落到同一张表里。
struct SubAgentInfo {
    id: String,
    state: SubAgentState,
    tokens: u64,
    started: Instant,
    /// 收尾原因（stop_reason / outcome / 错误摘要）。还在跑时是 `None`，
    /// 渲染时换算成已耗时。
    outcome: Option<String>,
}

impl Reducer {
    /// 启动归约循环：后台 task 消费 `event_rx`，每次事件后重新派生并广播 `FrameState`。
    /// 返回 `Reducer`（供 `EngineHandle` 驱动 `begin_turn`/`toggle_expand`/`resolve_prompt`）
    /// 和渲染循环订阅用的 `watch::Receiver`。
    pub fn spawn(
        mut event_rx: EventReceiver,
        model_name: String,
        cwd: String,
        commands: Arc<CommandCatalog>,
    ) -> (Arc<Self>, watch::Receiver<FrameState>) {
        let (reducer, frame_rx) = Self::build(model_name, cwd, Some(commands));

        let task_reducer = reducer.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                task_reducer.apply_event(event);
            }
        });

        let tick_reducer = reducer.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(STATUS_TICK);
            loop {
                interval.tick().await;
                tick_reducer.tick();
            }
        });

        (reducer, frame_rx)
    }

    fn build(
        model_name: String,
        cwd: String,
        commands: Option<Arc<CommandCatalog>>,
    ) -> (Arc<Self>, watch::Receiver<FrameState>) {
        let initial = DomainState {
            turns: Vec::new(),
            pending_approvals: Vec::new(),
            sub_agents: Vec::new(),
            tasks: Vec::new(),
            usage: SessionUsageState::default(),
            status: None,
            active_turn_started: None,
            cancel_requested: false,
            activity: String::new(),
            model_name,
            cwd,
        };
        let initial_frame = render(&initial);
        let (frame_tx, frame_rx) = watch::channel(initial_frame);
        let reducer = Arc::new(Self {
            state: Mutex::new(initial),
            frame_tx,
            commands,
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
        state.active_turn_started = Some(Instant::now());
        state.cancel_requested = false;
        state.activity = "Working…".into();
        refresh_running_status(&mut state);
        self.broadcast(&state);
        turn_id
    }

    /// Called when `BridgeCommand::CancelTurn` fires — acknowledges the keypress in
    /// the status line and nothing more. The turn is *not* over yet: `EngineCommand::
    /// CancelTurn` cancels the turn's token, and the turn loop notices at its next
    /// checkpoint and emits `TurnComplete { stop_reason: "cancelled" }`. That event
    /// is what ends the turn here (stops the spinner, leaves the note), so a
    /// cancelled turn and a finished one wind down through exactly one path.
    ///
    /// A no-op when nothing is running, mirroring Core: `CancelTurn` arriving while
    /// idle cancels an already-finished turn's token and does not arm the next one.
    pub fn request_cancel(&self) {
        let mut state = self.state.lock().unwrap();
        if state.active_turn_started.is_none() {
            return;
        }
        state.cancel_requested = true;
        state.activity = "Cancelling…".into();
        refresh_running_status(&mut state);
        self.broadcast(&state);
    }

    /// Periodic (500ms) refresh — keeps `elapsed_secs`/`spinner` animating while a
    /// turn is in flight, independent of whether any `AgentEvent` has arrived recently
    /// (e.g. a slow tool call produces no `TextDelta` in the meantime).
    fn tick(&self) {
        let mut state = self.state.lock().unwrap();
        if state.active_turn_started.is_none() {
            return;
        }
        refresh_running_status(&mut state);
        self.broadcast(&state);
    }

    /// 用户对某个待确认请求做出决定：立即从 pending 列表移除（不等 Core 确认）。
    pub fn resolve_prompt(&self, prompt_id: &str) {
        let mut state = self.state.lock().unwrap();
        state.pending_approvals.retain(|p| p.prompt_id != prompt_id);
        self.broadcast(&state);
    }

    /// `/model <name>` 落地：更新状态栏显示的模型名，并往转录里留一条记录。
    ///
    /// 乐观更新——不等 Core 回话。`EngineCommand::UpdateModel` 到了 Core 那边就是
    /// `Agent::set_model`（改 `settings.model.model_name` 一个字符串），没有失败路径，
    /// 也没有对应的确认事件；等一个不会来的事件只会让状态栏一直显示旧模型。
    /// 下一个 turn 起效：`runtime::turn` 每轮开头重读 `settings.model.model_name`。
    pub fn set_model(&self, name: String) {
        let mut state = self.state.lock().unwrap();
        push_note(
            &mut state.turns,
            format!("model switched to {name} (takes effect next turn)"),
        );
        state.model_name = name;
        self.broadcast(&state);
    }

    /// 往转录里写一条 app 侧的提示（未知的本地命令、`/model` 的用法等）。
    /// app 不持有转录，这是它唯一能往里说话的口子。
    pub fn note(&self, text: String) {
        let mut state = self.state.lock().unwrap();
        push_note(&mut state.turns, text);
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
                state.activity = "Streaming response…".into();
                refresh_running_status(&mut state);
            }
            AgentEvent::ThinkingDelta { text, turn_id } => {
                let turn = find_or_create_turn(&mut state.turns, &turn_id);
                match turn.blocks.last_mut() {
                    Some(Block::Thinking(buf)) => buf.push_str(&text),
                    _ => turn.blocks.push(Block::Thinking(text)),
                }
                state.activity = "Thinking…".into();
                refresh_running_status(&mut state);
            }
            AgentEvent::ToolUse {
                id,
                name,
                input,
                turn_id,
            } => {
                let input_summary = summarize_input(&input);
                state.activity = format!("Running {name}…");
                if name == TODO_TOOL {
                    if let Some(tasks) = parse_todos(&input) {
                        state.tasks = tasks;
                    }
                }
                let turn = find_or_create_turn(&mut state.turns, &turn_id);
                turn.blocks.push(Block::Tool {
                    id,
                    name,
                    input_summary,
                    result: None,
                    expanded: false,
                });
                refresh_running_status(&mut state);
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
            AgentEvent::TurnComplete {
                stop_reason, usage, ..
            } => {
                state.usage.token_in += usage.input_tokens as u64;
                state.usage.token_out += usage.output_tokens as u64;
                state.usage.turn_count += 1;
                state.active_turn_started = None;
                state.cancel_requested = false;
                state.status = None;
                if let Some(note) = early_stop_note(&stop_reason) {
                    push_note(&mut state.turns, note);
                }
            }
            AgentEvent::SystemInit { .. } => {}
            AgentEvent::System { message } => {
                push_note(&mut state.turns, message);
            }
            AgentEvent::CompactAction {
                strategy,
                messages_before,
                messages_after,
                dropped_rounds,
                estimated_tokens_saved,
                ..
            } => {
                // `CompactAction` fires once, *after* compaction already happened — Core
                // doesn't stream multi-stage progress for it, so `StatusContent::Compacting`
                // (which implies an in-progress spinner) doesn't actually fit here. Also,
                // `messages_before`/`messages_after` are *message counts*, not token counts —
                // force-fitting them into `Compacting`'s `tokens_before`/`tokens_after` fields
                // would mislabel the data. A transcript note is the honest representation.
                let mut note = format!(
                    "Context compacted ({strategy}): {messages_before} → {messages_after} messages"
                );
                if let Some(rounds) = dropped_rounds {
                    note.push_str(&format!(", {rounds} rounds dropped"));
                }
                if let Some(saved) = estimated_tokens_saved {
                    note.push_str(&format!(", ~{saved} tokens saved"));
                }
                push_note(&mut state.turns, note);
            }
            AgentEvent::SessionChanged { session_id } => {
                push_note(&mut state.turns, format!("session changed: {session_id}"));
            }
            AgentEvent::SessionPersisted { .. } => {}
            AgentEvent::SkillsChanged { .. } => {
                // 只重拉命令表，不往转录里写东西：技能文件的增删是用户在别的窗口
                // 干的事，转录是对话记录。`added`/`removed` 这里不看——目录是重算的，
                // 增量对不上就是错，全量拉一次反而没有对不上的可能。
                if let Some(catalog) = &self.commands {
                    catalog.refresh();
                }
            }
            AgentEvent::AgentSpawned { agent_id, .. } => {
                sub_agent(&mut state.sub_agents, &agent_id);
            }
            AgentEvent::AgentCompleted {
                agent_id, outcome, ..
            } => {
                let a = sub_agent(&mut state.sub_agents, &agent_id);
                a.state = SubAgentState::Done;
                a.outcome = Some(outcome);
            }
            // 子代理把自己的整条事件流转发到父通道上。这里只取子代理条要的三件事，
            // 不把子代理的文本灌进父转录——那是另一个层级的内容，混在一起会让
            // "谁在说话"彻底看不清。
            AgentEvent::SubagentProgress {
                agent_label, event, ..
            } => {
                let a = sub_agent(&mut state.sub_agents, &agent_label);
                match *event {
                    AgentEvent::TurnComplete {
                        usage, stop_reason, ..
                    } => {
                        a.tokens += usage.input_tokens as u64 + usage.output_tokens as u64;
                        a.state = SubAgentState::Done;
                        a.outcome = Some(stop_reason);
                    }
                    AgentEvent::Error { message, .. } => {
                        a.state = SubAgentState::Failed;
                        a.outcome = Some(first_line(&message));
                    }
                    // 还在动就是还在跑——子代理多轮时（收完一个 TurnComplete 又来事件）
                    // 这条会把它从 Done 拨回 Running。
                    _ => {
                        a.state = SubAgentState::Running;
                        a.outcome = None;
                    }
                }
            }
            // 团队编排进度：`SubAgentBarState` 只有单个代理的概念，没有"阶段/成员"
            // 这一层，硬塞进去会把两种东西画成一种。留到有对应结构再接。
            AgentEvent::TeamProgress { .. } => {}
            AgentEvent::Error {
                message, turn_id, ..
            } => {
                let turn = find_or_create_turn(&mut state.turns, &turn_id);
                turn.blocks.push(Block::Error(message));
                state.active_turn_started = None;
                state.status = None;
            }
        }
        self.broadcast(&state);
    }

    fn broadcast(&self, state: &DomainState) {
        let _ = self.frame_tx.send(render(state));
    }
}

/// Recompute `state.status` from `active_turn_started`/`activity`/`usage`. No-op if no
/// turn is currently active. `token_in`/`token_out` reflect *cumulative session* usage
/// (Core doesn't stream incremental per-turn usage), not this turn's usage alone — that's
/// a deliberate approximation, not exact per-turn accounting.
fn refresh_running_status(state: &mut DomainState) {
    let Some(started) = state.active_turn_started else {
        return;
    };
    state.status = Some(StatusContent::TurnRunning {
        spinner: spinner_char(),
        activity: state.activity.clone(),
        elapsed_secs: started.elapsed().as_secs(),
        token_in: state.usage.token_in,
        token_out: state.usage.token_out,
    });
}

/// Turns that ended short of the model finishing on its own. Core emits
/// `TurnComplete` for all four (they used to return in total silence, which is
/// what left a cancelled turn's spinner running forever), but the stop reason is
/// the only place the *why* survives — without a note the transcript just stops.
/// `end_turn`/`max_tokens`/`stop_sequence` and friends need no explanation, hence
/// `None`.
fn early_stop_note(stop_reason: &str) -> Option<String> {
    let text = match stop_reason {
        "cancelled" => "Turn cancelled.",
        "max_turns" => "Turn stopped: hit the per-turn API call limit (max_turns).",
        "budget_exceeded" => "Turn stopped: token budget for this turn exhausted.",
        "max_structured_output_retries" => {
            "Turn stopped: the model kept returning unparseable structured output."
        }
        _ => return None,
    };
    Some(text.to_string())
}

fn spinner_char() -> char {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    SPINNER_FRAMES[((ms / 150) % SPINNER_FRAMES.len() as u128) as usize]
}

/// 最后一个 turn 的用户输入的第一行，用作转录区顶上的 sticky header。
/// 合成 turn（`push_note` 在第一条消息之前建的那个）没有 `UserPrompt` 块，
/// 自然返回 `None`。
fn current_prompt(turns: &[Turn]) -> Option<String> {
    turns.iter().rev().find_map(|t| {
        t.blocks.iter().find_map(|b| match b {
            Block::UserPrompt(text) => Some(first_line(text)),
            _ => None,
        })
    })
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

/// 往最后一个 turn 里追加一条 note。
///
/// 一条 turn 都还没有时**开一个**，而不是把消息丢掉：`/model`、Core 的
/// `System` 事件、会话开始时的提示都可能发生在用户提交第一条消息之前，
/// 原来那版（`turns.last_mut()` 拿不到就静默返回）会把它们全咽掉。
/// 这个合成 turn 的 id 不与任何 Core turn 冲突，后续事件照常按自己的
/// `turn_id` 找/建自己的 turn。
fn push_note(turns: &mut Vec<Turn>, message: String) {
    if turns.is_empty() {
        turns.push(Turn {
            id: String::new(),
            blocks: Vec::new(),
        });
    }
    if let Some(turn) = turns.last_mut() {
        turn.blocks.push(Block::Note(message));
    }
}

/// 按 id/label 找子代理，没有就建一个（`SubagentProgress` 的第一次出现就是"它开始了"，
/// 不需要另一个 spawn 事件）。
fn sub_agent<'a>(agents: &'a mut Vec<SubAgentInfo>, id: &str) -> &'a mut SubAgentInfo {
    if let Some(idx) = agents.iter().position(|a| a.id == id) {
        return &mut agents[idx];
    }
    agents.push(SubAgentInfo {
        id: id.to_string(),
        state: SubAgentState::Running,
        tokens: 0,
        started: Instant::now(),
        outcome: None,
    });
    agents.last_mut().unwrap()
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// `TodoWrite` 的入参 → 任务清单区。
///
/// 只认这一个工具名，和 diff 那边的"按内容认"相反：待办清单是这个工具**私有的**
/// 数据结构（`{"todos":[{content,status,active_form}]}`），不是一种通用输出格式，
/// 别的工具凑巧有个 `todos` 字段也不该被当成清单。
fn parse_todos(input: &serde_json::Value) -> Option<Vec<TaskItem>> {
    let todos = input.get("todos")?.as_array()?;
    Some(
        todos
            .iter()
            .filter_map(|t| {
                let status = match t.get("status").and_then(|s| s.as_str())? {
                    "in_progress" => ItemStatus::Running,
                    "completed" => ItemStatus::Done,
                    _ => ItemStatus::Pending,
                };
                // 进行中的那条用 `active_form`（"Running tests" 这种现在进行时），
                // 这正是模型写这个字段的用意；其余用 `content`。
                let active_form = t.get("active_form").and_then(|s| s.as_str());
                let content = t.get("content").and_then(|s| s.as_str());
                let label = match (status, active_form) {
                    (ItemStatus::Running, Some(f)) if !f.is_empty() => f,
                    _ => content?,
                };
                Some(TaskItem {
                    status,
                    label: label.to_string(),
                })
            })
            .collect(),
    )
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
            // 转录区顶上钉住"当前在回答哪个问题"。滚回历史时它是唯一还告诉你
            // 上下文的东西；取最后一个 turn 的用户输入，多行只取第一行——这条
            // 是一行高的固定区域。
            header: match current_prompt(&state.turns) {
                Some(text) => HeaderState {
                    text: Some(text),
                    source: HeaderSource::UserPrompt,
                },
                None => HeaderState {
                    text: None,
                    source: HeaderSource::None,
                },
            },
            body: TranscriptBodyState {
                scroll: ScrollState {
                    offset: 0,
                    total_lines: entries.len(),
                },
                entries,
                auto_follow: true,
                // 选中态和滚动位置一样是纯 UI-本地的：bridge 只知道有哪些块，
                // 不知道光标停在哪个上面。由 app 的 `merge` 覆盖。
                selected_block: None,
            },
        },
        operation_status: OperationStatusState {
            status_line: StatusLineState {
                content: state.status.clone(),
            },
            task_list: TaskListState {
                items: state.tasks.clone(),
            },
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
                    name: a.id.clone(),
                    state: a.state,
                    token_usage: a.tokens,
                    elapsed_or_status: a
                        .outcome
                        .clone()
                        .unwrap_or_else(|| format!("{}s", a.started.elapsed().as_secs())),
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
        Block::Thinking(text) => entries.push(plain(LineKind::Thinking, text)),
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
            let base = if result.is_error {
                LineKind::ToolResultErr
            } else {
                LineKind::ToolResultOk
            };
            let lines: Vec<&str> = result.text.lines().collect();
            let diff_from = diff_section_start(&lines);
            let kind = |idx: usize, line: &str| classify(idx, line, diff_from, base);
            if lines.len() <= FOLD_LINE_THRESHOLD || *expanded {
                if lines.is_empty() {
                    entries.push(TranscriptEntry {
                        kind: base,
                        text: String::new(),
                        block_id: Some(id.clone()),
                    });
                }
                for (i, line) in lines.iter().enumerate() {
                    entries.push(TranscriptEntry {
                        kind: kind(i, line),
                        text: line.to_string(),
                        block_id: Some(id.clone()),
                    });
                }
            } else {
                for (i, line) in lines[..FOLD_LINE_THRESHOLD].iter().enumerate() {
                    entries.push(TranscriptEntry {
                        kind: kind(i, line),
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

/// 在一段工具输出里找 unified diff 的起点：`--- <路径>` 紧跟 `+++ <路径>` 那一对
/// 表头。返回表头第一行的下标，没有就是 `None`。
///
/// **按内容认，不按工具名认。** Core 里目前只有 `Edit` 会产出 diff（`file_edit.rs`
/// 的 `render_diff`，`--- {path} (before)` / `+++ {path} (after)` / `@@`），但
/// 钉一张 `["Edit"]` 白名单的话，哪天 `Write` 也开始返回 diff 就会静默退回纯文本。
/// 顺带的好处：`Bash` 跑 `git diff` 出来的那段一样会上色——那正是用户想看到的。
/// 要求表头成对出现是为了不把 `-v` 开头的普通输出误判成删除行。
fn diff_section_start(lines: &[&str]) -> Option<usize> {
    lines
        .windows(2)
        .position(|w| w[0].starts_with("--- ") && w[1].starts_with("+++ "))
}

/// 单行的展示类别。`diff_from` 之前的行是工具自己的摘要（"Applied 1 edit …"），
/// 保持 `base`；从表头开始按 unified diff 的语法上色。
fn classify(idx: usize, line: &str, diff_from: Option<usize>, base: LineKind) -> LineKind {
    let Some(start) = diff_from else { return base };
    if idx < start {
        return base;
    }
    // 表头两行自己以 `-`/`+` 开头，先摘出去，不然会被当成删除/新增行。
    if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("@@") {
        return LineKind::DiffContext;
    }
    match line.as_bytes().first() {
        Some(b'-') => LineKind::DiffOld,
        Some(b'+') => LineKind::DiffNew,
        _ => LineKind::DiffContext,
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
    fn reducer() -> (Arc<Reducer>, watch::Receiver<FrameState>) {
        Reducer::build("test-model".into(), "/tmp".into(), None)
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

    /// `Edit` 的结果是"一段摘要 + 一段 unified diff"。摘要保持普通工具结果的样子，
    /// diff 那段按增/删/上下文分色——需求里"不同工具类型的结果内容（如文件变更）"
    /// 说的就是这个。
    #[test]
    fn a_unified_diff_in_a_tool_result_is_rendered_as_diff_lines() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("改个字".into());
        r.apply_event(AgentEvent::ToolUse {
            id: "t1".into(),
            name: "Edit".into(),
            input: serde_json::json!({"file_path": "a.rs"}),
            turn_id: turn_id.clone(),
        });
        r.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "Edit".into(),
            content: "Applied 1 edit (1 replacement) to a.rs (10 → 12 bytes)\n\n\
                      --- a.rs (before)\n\
                      +++ a.rs (after)\n\
                      @@ -1,3 +1,3 @@\n\
                      \x20fn main() {\n\
                      -    old();\n\
                      +    new();\n"
                .into(),
            is_error: Some(false),
            turn_id,
        });

        let kinds: Vec<LineKind> = frame(&rx)
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind != LineKind::UserPrompt && e.kind != LineKind::ToolHeading)
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::ToolResultOk, // 摘要
                LineKind::ToolResultOk, // 空行
                LineKind::DiffContext,  // --- a.rs (before)
                LineKind::DiffContext,  // +++ a.rs (after)
                LineKind::DiffContext,  // @@ hunk 头
                LineKind::DiffContext,  //  fn main() {
                LineKind::DiffOld,      // -    old();
                LineKind::DiffNew,      // +    new();
            ]
        );
    }

    /// 只有成对的 `---`/`+++` 表头才算 diff。命令输出里一行 `-v` 开头的普通文本
    /// 不该被当成删除行涂红。
    #[test]
    fn ordinary_output_starting_with_a_dash_is_not_treated_as_a_diff() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("跑一下".into());
        r.apply_event(AgentEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
            turn_id: turn_id.clone(),
        });
        r.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "Bash".into(),
            content: "-rw-r--r--  1 me  staff  12 a.txt\n+ 还有一行".into(),
            is_error: Some(false),
            turn_id,
        });

        let f = frame(&rx);
        assert!(
            f.transcript.body.entries.iter().all(|e| !matches!(
                e.kind,
                LineKind::DiffOld | LineKind::DiffNew | LineKind::DiffContext
            )),
            "没有 diff 表头就不该出现 diff 行"
        );
    }

    /// 子代理条的数据只能从 `SubagentProgress` 来：Core 从不发 `AgentSpawned`/
    /// `AgentCompleted`（全仓只有同名的 telemetry payload），用量也只在转发过来的
    /// `TurnComplete` 里。以前这条事件被整个丢掉，于是子代理条永远是空的。
    #[test]
    fn subagent_progress_drives_the_bar_including_token_usage() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("查一下".into());

        let forwarded = |inner: AgentEvent| AgentEvent::SubagentProgress {
            agent_label: "explore#3f2a1b7c".into(),
            agent_session_id: "s1".into(),
            agent_type: Some("explore".into()),
            parent_session_id: "p1".into(),
            parent_turn: 1,
            event: Box::new(inner),
        };

        // 第一次出现就等于"它开始了"——不需要另一个 spawn 事件。
        r.apply_event(forwarded(AgentEvent::TextDelta {
            text: "looking".into(),
            turn_id: turn_id.clone(),
        }));
        let bar = frame(&rx).sub_agent_bar.agents;
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].name, "explore#3f2a1b7c");
        assert_eq!(bar[0].state, SubAgentState::Running);

        r.apply_event(forwarded(AgentEvent::TurnComplete {
            stop_reason: "end_turn".into(),
            api_calls: 1,
            tool_calls: 2,
            usage: base::interface::model::Usage {
                input_tokens: 120,
                output_tokens: 30,
            },
            turn_id,
        }));
        let bar = frame(&rx).sub_agent_bar.agents;
        assert_eq!(bar[0].state, SubAgentState::Done);
        assert_eq!(bar[0].token_usage, 150, "用量来自转发过来的 TurnComplete");
        assert_eq!(bar[0].elapsed_or_status, "end_turn");
    }

    /// 待办清单区的数据来自 `TodoWrite` 的入参——Core 没有"清单变了"这种事件。
    #[test]
    fn todo_write_fills_the_task_list() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("做三件事".into());
        r.apply_event(AgentEvent::ToolUse {
            id: "t1".into(),
            name: "TodoWrite".into(),
            input: serde_json::json!({"todos": [
                {"content": "看代码", "status": "completed", "active_form": "看代码中"},
                {"content": "跑测试", "status": "in_progress", "active_form": "跑测试中"},
                {"content": "提交", "status": "pending", "active_form": ""},
            ]}),
            turn_id: turn_id.clone(),
        });

        let items = frame(&rx).operation_status.task_list.items;
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, ItemStatus::Done);
        assert_eq!(items[0].label, "看代码");
        assert_eq!(items[1].status, ItemStatus::Running);
        assert_eq!(items[1].label, "跑测试中", "进行中的那条用 active_form");
        assert_eq!(items[2].status, ItemStatus::Pending);

        // 每次调用全量替换，空列表就等于收起这块区域。
        r.apply_event(AgentEvent::ToolUse {
            id: "t2".into(),
            name: "TodoWrite".into(),
            input: serde_json::json!({"todos": []}),
            turn_id,
        });
        assert!(frame(&rx).operation_status.task_list.items.is_empty());
    }

    /// 别的工具凑巧带个 `todos` 字段不该被当成清单。
    #[test]
    fn only_the_todo_tool_touches_the_task_list() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"todos": [{"content": "x", "status": "pending"}]}),
            turn_id,
        });
        assert!(frame(&rx).operation_status.task_list.items.is_empty());
    }

    /// 转录顶上钉住"当前在回答哪个问题"，多行只取第一行。
    #[test]
    fn the_header_sticks_the_current_prompt() {
        let (r, rx) = reducer();
        assert_eq!(frame(&rx).transcript.header.text, None);

        r.begin_turn("第一个问题".into());
        assert_eq!(
            frame(&rx).transcript.header.text.as_deref(),
            Some("第一个问题")
        );
        assert_eq!(
            frame(&rx).transcript.header.source,
            HeaderSource::UserPrompt
        );

        r.begin_turn("第二个问题\n还有第二行".into());
        assert_eq!(
            frame(&rx).transcript.header.text.as_deref(),
            Some("第二个问题")
        );
    }

    /// `/model` 走的是乐观更新：状态栏立刻换掉，同时留一条转录记录。
    #[test]
    fn set_model_updates_the_footer_and_leaves_a_note() {
        let (r, rx) = reducer();
        assert_eq!(frame(&rx).footer_hints.model, "test-model");

        r.set_model("claude-opus-5".into());

        let f = frame(&rx);
        assert_eq!(f.footer_hints.model, "claude-opus-5");
        assert!(
            f.transcript
                .body
                .entries
                .iter()
                .any(|e| e.kind == LineKind::Note && e.text.contains("claude-opus-5")),
            "换模型应该在转录里留痕:\n{:?}",
            f.transcript.body.entries
        );
    }

    /// 第一个 turn 之前来的 note 以前会被静默吞掉（`turns.last_mut()` 拿不到就返回）。
    #[test]
    fn a_note_before_the_first_turn_is_not_swallowed() {
        let (r, rx) = reducer();
        r.note("hello".into());
        let entries = frame(&rx).transcript.body.entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LineKind::Note);
        assert_eq!(entries[0].text, "hello");
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

    #[test]
    fn begin_turn_sets_running_status_and_turn_complete_clears_it() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        let f = frame(&rx);
        assert!(matches!(
            f.operation_status.status_line.content,
            Some(StatusContent::TurnRunning { .. })
        ));

        r.apply_event(AgentEvent::TurnComplete {
            stop_reason: "end_turn".into(),
            api_calls: 1,
            tool_calls: 0,
            usage: base::interface::model::Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
            turn_id,
        });
        let f = frame(&rx);
        assert!(f.operation_status.status_line.content.is_none());
    }

    #[test]
    fn error_clears_running_status() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::Error {
            code: "boom".into(),
            message: "something broke".into(),
            turn_id,
        });
        let f = frame(&rx);
        assert!(f.operation_status.status_line.content.is_none());
    }

    #[test]
    fn compact_action_pushes_a_transcript_note_not_a_status() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::CompactAction {
            strategy: "Snip".into(),
            messages_before: 40,
            messages_after: 12,
            turn_id,
            dropped_rounds: Some(3),
            dropped_messages: Some(28),
            estimated_tokens_saved: Some(5000),
        });
        let f = frame(&rx);
        let note = f
            .transcript
            .body
            .entries
            .iter()
            .find(|e| e.kind == LineKind::Note && e.text.contains("compacted"));
        assert!(
            note.is_some(),
            "expected a compaction note in the transcript"
        );
        // still running — CompactAction doesn't imply the turn ended
        assert!(f.operation_status.status_line.content.is_some());
    }

    /// 按下中断键的那一刻 turn 还没结束——Core 要跑到下一个检查点才认。状态行改文案
    /// 但 spinner 不停，是这段等待期唯一诚实的呈现。
    #[test]
    fn request_cancel_keeps_the_turn_running_until_core_confirms() {
        let (r, rx) = reducer();
        r.begin_turn("q".into());

        r.request_cancel();
        let f = frame(&rx);
        match f.operation_status.status_line.content {
            Some(StatusContent::TurnRunning { ref activity, .. }) => {
                assert!(activity.contains("Cancelling"), "activity was {activity:?}");
            }
            ref other => panic!("expected the turn to still be running, got {other:?}"),
        }
        assert!(
            !f.transcript
                .body
                .entries
                .iter()
                .any(|e| e.text.contains("cancelled")),
            "the note belongs to TurnComplete, not to the keypress"
        );
    }

    #[test]
    fn cancelled_turn_complete_stops_the_spinner_and_leaves_a_note() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.request_cancel();

        r.apply_event(AgentEvent::TurnComplete {
            stop_reason: "cancelled".into(),
            api_calls: 1,
            tool_calls: 0,
            usage: base::interface::model::Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            turn_id,
        });

        let f = frame(&rx);
        assert!(f.operation_status.status_line.content.is_none());
        assert!(f
            .transcript
            .body
            .entries
            .iter()
            .any(|e| e.kind == LineKind::Note && e.text.contains("cancelled")));
    }

    /// 这三个 stop_reason 以前一个事件都不发，接上 Core 的修复后才会到达。
    #[test]
    fn other_early_stop_reasons_explain_themselves_in_the_transcript() {
        for (stop_reason, expected) in [
            ("max_turns", "max_turns"),
            ("budget_exceeded", "budget"),
            ("max_structured_output_retries", "structured output"),
        ] {
            let (r, rx) = reducer();
            let turn_id = r.begin_turn("q".into());
            r.apply_event(AgentEvent::TurnComplete {
                stop_reason: stop_reason.into(),
                api_calls: 1,
                tool_calls: 0,
                usage: base::interface::model::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
                turn_id,
            });
            let f = frame(&rx);
            assert!(
                f.transcript
                    .body
                    .entries
                    .iter()
                    .any(|e| e.kind == LineKind::Note && e.text.contains(expected)),
                "no note explaining {stop_reason}"
            );
        }
    }

    /// 正常收尾不该在转录里多留一行。
    #[test]
    fn end_turn_leaves_no_note() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::TurnComplete {
            stop_reason: "end_turn".into(),
            api_calls: 1,
            tool_calls: 0,
            usage: base::interface::model::Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            turn_id,
        });
        let f = frame(&rx);
        assert!(f
            .transcript
            .body
            .entries
            .iter()
            .all(|e| e.kind != LineKind::Note));
    }

    #[test]
    fn request_cancel_is_a_no_op_when_nothing_is_running() {
        let (r, rx) = reducer();
        r.request_cancel();
        let f = frame(&rx);
        assert!(f.transcript.body.entries.is_empty());
        assert!(f.operation_status.status_line.content.is_none());
    }

    #[test]
    fn thinking_delta_accumulates_into_its_own_line_kind() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::ThinkingDelta {
            text: "let me ".into(),
            turn_id: turn_id.clone(),
        });
        r.apply_event(AgentEvent::ThinkingDelta {
            text: "check".into(),
            turn_id,
        });
        let f = frame(&rx);
        let thinking: Vec<_> = f
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::Thinking)
            .collect();
        assert_eq!(thinking.len(), 1);
        assert_eq!(thinking[0].text, "let me check");
    }
}
