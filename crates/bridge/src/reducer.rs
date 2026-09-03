//! `AgentEvent` 归约器 — 消费 `EventReceiver`，维护富领域模型，派生 `tui::FrameState`。
//!
//! `tui::FrameState` 混合了两类信息：Core 权威状态（转录、权限请求、子代理、用量）
//! 和纯 UI-本地状态（草稿/光标/滚动位置/补全弹窗）。这里派生的快照只负责前者——
//! composer 的编辑器/滚动字段留默认值，由 `crates/app` 的事件循环在渲染前用本地
//! UI 状态覆盖（bridge 不知道、也不需要知道用户正在编辑到第几个字符）。

use crate::ask::{PendingQuestion, QuestionEvent};
use crate::commands::CommandCatalog;
use base::event::AgentEvent;
use runtime::agent::EventReceiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;
// 不是 `std::time::Instant`：这个认 tokio 的假时钟，于是"状态行秒数在走"
// 能用 `time::advance` 验，不必 sleep 真实时间（见本文件的 ticker 测试）。
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
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
    /// 打点（`ATTACODE_TRACE=<路径>` 才开）。挂在这里是因为这里是 `FrameState`
    /// 唯一的产地——每一帧都要经过 `broadcast`，一个都漏不掉。
    trace: Option<crate::trace::Trace>,
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

/// 一条正在等人回答的东西。两种来源共用这个队列，因为屏幕上它们是同一个对话框：
/// 引擎的权限门（`AgentEvent::PermissionPrompt`），和模型自己的提问
/// （`AskUserQuestion`，见 [`crate::ask`]）。区别全在 `answer_with`/`options` 上，
/// 以及答案回到哪儿去——那是 `handle` 的事。
struct PendingApproval {
    prompt_id: String,
    tool_name: String,
    message: String,
    answer_with: AnswerWith,
    options: Vec<ApprovalOption>,
}

/// 权限门那一档的四个答案。每个待批准请求都是同一套，所以只写一次。
fn permission_options() -> Vec<ApprovalOption> {
    vec![
        ApprovalOption::PermitOnce,
        ApprovalOption::PermitSession,
        ApprovalOption::PermitProject,
        ApprovalOption::Deny,
    ]
}

/// 模型的提问 → 队列里的一条。
///
/// `options` 为空是 Core schema 明说的一档（自由文本），这里翻成
/// [`AnswerWith::Type`]：对话框只显示问题，composer 不锁，用户提交的下一行就是
/// 答案。**不能**退回成"没有选项的多选题"——那是个选不动也退不出的死框。
fn pending_from_question(q: PendingQuestion) -> PendingApproval {
    let answer_with = if q.options.is_empty() {
        AnswerWith::Type
    } else {
        AnswerWith::Choose
    };
    PendingApproval {
        prompt_id: q.id,
        tool_name: q.header,
        message: q.question,
        answer_with,
        options: q
            .options
            .into_iter()
            .map(|(key, label)| ApprovalOption::Answer { key, label })
            .collect(),
    }
}

/// 子代理条上的一行。
///
/// **键是那个 `agent_label`**（`explore#3f2a1b7c` 那种）。三种事件共用它：
/// `AgentSpawned.agent_id` / `AgentCompleted.agent_id` / `SubagentProgress.agent_label`
/// 是同一个串，所以不用额外记账就能落到同一行上。
///
/// 三者分工不同，缺一不可：spawn/complete 是**父时间线**上的括号（这里开始委派、
/// 这里结束），`SubagentProgress` 是子代理自己那条流被原样转发过来。用量只能从
/// 后者拿——转发过来的 `TurnComplete` 里带子代理这一轮的 `usage`，括号里没有。
///
/// > AttaCore v0.1.1 之前 spawn/complete 是**声明了但没人发**的死变体（同名的
/// > telemetry payload 是另一条通道），子代理边界只能从 `agent_label` 反推。
/// > 现在真发了，反推那条路留着也无妨：`SubagentProgress` 先到就先建行。
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
        mut questions_rx: mpsc::UnboundedReceiver<QuestionEvent>,
        model_name: String,
        cwd: String,
        commands: Arc<CommandCatalog>,
        restored: Vec<base::message::Message>,
    ) -> (Arc<Self>, watch::Receiver<FrameState>) {
        let (reducer, frame_rx) = Self::build(model_name, cwd, Some(commands), restored);

        let task_reducer = reducer.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                task_reducer.apply_event(event);
            }
        });

        // 模型的提问走的是**另一条**流，不是 `AgentEvent`。`AgentEvent` 是 Core 的
        // 词汇表，里面没有"模型想问点什么"这一项，而 `AskUserQuestion` 的替身工具
        // （见 [`crate::ask`]）跑在引擎里、够得着的是我们自己的通道。两条流都只往
        // 同一个 `DomainState` 里写，锁在 `apply_*` 内部，互不干扰。
        let question_reducer = reducer.clone();
        tokio::spawn(async move {
            while let Some(event) = questions_rx.recv().await {
                question_reducer.apply_question(event);
            }
        });

        // 没有订阅方了就收工。以前这里是个 `loop {}`——进程一辈子只有一个归约器，
        // 泄漏一个 task 看不出来。`/resume` 换会话之后就不是了：每换一次留一个永远
        // 醒着的 500ms 定时器，各自还攥着一个 `Arc<Reducer>` 和一整棵 `DomainState`。
        //
        // 判据是"还有没有人在看"而不是某个取消信号：这个 task 的全部作用就是让画面
        // 上的秒数走字，没人看的时候它没有任何事情可做。
        let tick_reducer = reducer.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(STATUS_TICK);
            loop {
                interval.tick().await;
                if tick_reducer.frame_tx.is_closed() {
                    return;
                }
                tick_reducer.tick();
            }
        });

        (reducer, frame_rx)
    }

    fn build(
        model_name: String,
        cwd: String,
        commands: Option<Arc<CommandCatalog>>,
        restored: Vec<base::message::Message>,
    ) -> (Arc<Self>, watch::Receiver<FrameState>) {
        let initial = DomainState {
            turns: restore_turns(restored),
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
            trace: crate::trace::Trace::from_env(),
        });
        (reducer, frame_rx)
    }

    /// 给 `handle` 的测试用：一个不接 `Agent`、也不带命令目录的空归约器。
    /// 放在这里而不是那边，是因为 `build` 是私有的（外面只该走 `spawn`）。
    #[cfg(test)]
    pub(crate) fn build_for_test() -> (Arc<Self>, watch::Receiver<FrameState>) {
        Self::build("test-model".into(), "/tmp".into(), None, Vec::new())
    }

    /// 给 app 用：把**真正要渲染的那一帧**记一笔。
    ///
    /// bridge 这边打的点只到"Core 给了什么"为止——选中块、滚动位置、草稿、补全
    /// 弹窗都是 app 在 `merge` 里覆盖上去的，bridge 根本看不见。只打 bridge 那一
    /// 侧的话，报告会把"块选中态从没有过内容"报成红的，而屏幕上明明标着竖条。
    pub fn trace_render(&self, frame: &FrameState) {
        if let Some(trace) = &self.trace {
            trace.record("render", frame);
        }
    }

    /// 给 app 用：记一次按键（诊断用，`ATTACODE_TRACE` 没开就是空操作）。
    pub fn trace_key(&self, key: &str, outcome: &str) {
        if let Some(trace) = &self.trace {
            trace.record_key(key, outcome);
        }
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

    /// 模型提了个问题，或者不等了。见 [`crate::ask`]。
    pub fn apply_question(&self, event: QuestionEvent) {
        match event {
            QuestionEvent::Ask(q) => {
                let mut state = self.state.lock().unwrap();
                state.pending_approvals.push(pending_from_question(q));
                self.broadcast_as("Question", &state);
            }
            QuestionEvent::Withdraw(id) => self.resolve_prompt(&id),
        }
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
        // 打点要的事件名。在 `match` 消费掉 `event` 之前先取出来。
        let event_name = event_name(&event);
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
                    answer_with: AnswerWith::Choose,
                    options: permission_options(),
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
            // 这一对是**父时间线**上的括号，`agent_id` 和 `SubagentProgress` 的
            // `agent_label` 是同一个串，所以两边落在同一行上。
            //
            // `AgentCompleted` **最后到**，而它带的 outcome 是委派层面的判词
            // （`completed`/`failed`/`cancelled`），比子代理自己那条 `stop_reason`
            // 粗。所以这里只用它定**状态**，不拿它覆盖已经有的 outcome 文本——
            // 屏幕上 `end_turn` 比 `completed` 有信息量。失败/取消必须照实标出来：
            // 无条件写 Done 会把一个刚 Failed 的子代理抹成正常结束。
            AgentEvent::AgentCompleted {
                agent_id, outcome, ..
            } => {
                let a = sub_agent(&mut state.sub_agents, &agent_id);
                a.state = match outcome.as_str() {
                    "failed" => SubAgentState::Failed,
                    _ => SubAgentState::Done,
                };
                a.outcome.get_or_insert(outcome);
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
        self.broadcast_as(&event_name, &state);
    }

    fn broadcast(&self, state: &DomainState) {
        self.broadcast_as("local", state)
    }

    /// 广播一帧，并打一个点。`what` 是触发它的事件名，只给打点看。
    fn broadcast_as(&self, what: &str, state: &DomainState) {
        let frame = render(state);
        if let Some(trace) = &self.trace {
            trace.record(what, &frame);
        }
        let _ = self.frame_tx.send(frame);
    }
}

/// Recompute `state.status` from `active_turn_started`/`activity`/`usage`. No-op if no
/// turn is currently active. `token_in`/`token_out` reflect *cumulative session* usage,
/// not this turn's usage alone, and they only move when a turn ends — the number is
/// exact per completed turn and stale for the turn in flight.
///
/// Per-*call* numbers do exist as of AttaCore 0.2.2 (`ApiRequestPayload`: model,
/// latency, stop reason, tokens), but they go out through telemetry, not through
/// `AgentEvent`. Making the counter tick mid-turn means registering an `event.sink`
/// or a telemetry recorder, not reading harder here.
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
/// `TurnComplete` for every one of them (they used to return in total silence, which
/// is what left a cancelled turn's spinner running forever), but the stop reason is
/// the only place the *why* survives — without a note the transcript just stops.
/// `end_turn`/`max_tokens`/`stop_sequence` and friends need no explanation, hence
/// `None`.
///
/// **This list has to be kept level with Core's.** A reason nobody has a sentence for
/// is not a rendering gap you can see — it is a turn that stops with no explanation,
/// which is the exact failure this function was written to fix. `context_exceeded`
/// and `host_ceiling` arrived in AttaCore 0.2.x (the latter from the new
/// `TurnPolicy`) and spent that release unexplained here.
fn early_stop_note(stop_reason: &str) -> Option<String> {
    let text = match stop_reason {
        "cancelled" => "Turn cancelled.",
        "max_turns" => "Turn stopped: hit the per-turn API call limit (max_turns).",
        "budget_exceeded" => "Turn stopped: token budget for this turn exhausted.",
        "max_structured_output_retries" => {
            "Turn stopped: the model kept returning unparseable structured output."
        }
        // Compaction ran and the context was *still* over the hard cap. Unlike the
        // others this one will not fix itself by waiting: the next turn starts from
        // the same oversized history, so say what to do about it.
        "context_exceeded" => {
            "Turn stopped: the conversation is still over the context limit after \
             compaction. Try /clear, or /compact with a narrower instruction."
        }
        "host_ceiling" => "Turn stopped: a host-configured turn limit was reached.",
        "stopped_by_hook" => "Turn stopped by a configured hook.",
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

/// resume 时把读回来的历史消息铺回转录区。
///
/// 和实时事件流的形状对齐：一条用户文本开一个新 turn，assistant 的文本/思考/工具
/// 调用挂在当前 turn 上，`ToolResult` 按 `tool_use_id` 回填到对应的工具块里
/// （历史里工具结果是**下一条 user 消息**的内容块，不是独立消息）。
///
/// turn id 用 `restored-N`：Core 的 turn id 是 `Id::new()` 生成的，不会撞；
/// 之后的实时事件按自己的 id 找/建自己的 turn，不会误挂到恢复出来的 turn 上。
fn restore_turns(messages: Vec<base::message::Message>) -> Vec<Turn> {
    use base::message::{ContentBlock, Message, ToolResultContent};

    let mut turns: Vec<Turn> = Vec::new();
    let push_block = |turns: &mut Vec<Turn>, block: Block| {
        if turns.is_empty() {
            turns.push(Turn {
                id: format!("restored-{}", 0),
                blocks: Vec::new(),
            });
        }
        turns.last_mut().unwrap().blocks.push(block);
    };

    for message in messages {
        match message {
            Message::User { content } => {
                for block in content {
                    match block {
                        ContentBlock::Text { text, .. } => match user_text(&text) {
                            Some(text) => turns.push(Turn {
                                id: format!("restored-{}", turns.len()),
                                blocks: vec![Block::UserPrompt(text)],
                            }),
                            None => continue,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let text = match content {
                                ToolResultContent::Text(t) => t,
                                // 结构化结果（图片等）在转录里只留个占位，别把
                                // base64 糊一屏。
                                ToolResultContent::Blocks(blocks) => blocks
                                    .iter()
                                    .map(|b| match b {
                                        ContentBlock::Text { text, .. } => text.clone(),
                                        other => format!("[{}]", block_kind(other)),
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            };
                            fill_tool_result(&mut turns, &tool_use_id, text, is_error);
                        }
                        other => {
                            push_block(&mut turns, Block::Note(format!("[{}]", block_kind(&other))))
                        }
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for block in content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            push_block(&mut turns, Block::AssistantText(text))
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            push_block(&mut turns, Block::Thinking(thinking))
                        }
                        ContentBlock::ToolUse { id, name, input } => push_block(
                            &mut turns,
                            Block::Tool {
                                id,
                                name,
                                input_summary: summarize_input(&input),
                                result: None,
                                expanded: false,
                            },
                        ),
                        other => {
                            push_block(&mut turns, Block::Note(format!("[{}]", block_kind(&other))))
                        }
                    }
                }
            }
            // `project_messages` 不产出 System（它只投影进 API 的那部分），
            // 这条分支是为了把 `Message` 穷举掉，将来加了别的来源也不会静默丢。
            Message::System { content, .. } => push_block(&mut turns, Block::Note(content)),
        }
    }
    turns
}

/// 一条历史 `user` 消息里**真正是用户打的**那部分，没有就是 `None`。
///
/// Core 会往对话里塞给模型读的上下文——CLAUDE.md、git status、各种提醒——用
/// `<system-reminder>` 包着，在 jsonl 里和用户敲的字一样是 `user` 消息，
/// `project_messages` 也照样投影出来。恢复转录时必须滤掉：resume 之后第一屏
/// 全是那坨 CLAUDE.md，真正的对话被挤到看不见（真机跑一次就发现了）。
/// 顺带也会污染输入历史——`crates/app` 的历史正是从恢复出来的用户输入里读的。
fn user_text(text: &str) -> Option<String> {
    // 提醒块可能整条就是它，也可能跟在真话后面；截到标记为止。
    let head = match text.find("<system-reminder>") {
        Some(at) => &text[..at],
        None => text,
    };
    let head = head.trim();
    (!head.is_empty()).then(|| head.to_string())
}

/// 把工具结果回填到它的工具块上。找不到对应块时落成一条 note——和实时链路里
/// 孤儿 `ToolResult` 的处理一致，宁可显示得难看也不静默丢。
fn fill_tool_result(turns: &mut Vec<Turn>, tool_use_id: &str, text: String, is_error: bool) {
    for turn in turns.iter_mut().rev() {
        for block in turn.blocks.iter_mut().rev() {
            if let Block::Tool { id, result, .. } = block {
                if id == tool_use_id {
                    *result = Some(ToolOutcome { text, is_error });
                    return;
                }
            }
        }
    }
    push_note(turns, format!("[orphan tool result {tool_use_id}] {text}"));
}

/// 内容块的类型名，用于给不展示的块留个占位。
fn block_kind(block: &base::message::ContentBlock) -> &'static str {
    use base::message::ContentBlock as B;
    match block {
        B::Text { .. } => "text",
        B::Image { .. } => "image",
        B::ToolUse { .. } => "tool_use",
        B::ToolResult { .. } => "tool_result",
        B::Thinking { .. } => "thinking",
        _ => "block",
    }
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

/// 事件的类型名，只给打点用。`AgentEvent` 没有 `Display`，也不该为了打点给它加。
fn event_name(event: &AgentEvent) -> String {
    let name = match event {
        AgentEvent::TextDelta { .. } => "TextDelta",
        AgentEvent::ThinkingDelta { .. } => "ThinkingDelta",
        AgentEvent::ToolUse { name, .. } => return format!("ToolUse:{name}"),
        AgentEvent::ToolResult { name, .. } => return format!("ToolResult:{name}"),
        AgentEvent::PermissionPrompt { .. } => "PermissionPrompt",
        AgentEvent::TurnComplete { .. } => "TurnComplete",
        AgentEvent::SystemInit { .. } => "SystemInit",
        AgentEvent::System { .. } => "System",
        AgentEvent::CompactAction { .. } => "CompactAction",
        AgentEvent::SessionChanged { .. } => "SessionChanged",
        AgentEvent::SessionPersisted { .. } => "SessionPersisted",
        AgentEvent::SkillsChanged { .. } => "SkillsChanged",
        AgentEvent::AgentSpawned { .. } => "AgentSpawned",
        AgentEvent::AgentCompleted { .. } => "AgentCompleted",
        AgentEvent::SubagentProgress { event, .. } => {
            return format!("SubagentProgress({})", event_name(event))
        }
        AgentEvent::TeamProgress { .. } => "TeamProgress",
        AgentEvent::Error { .. } => "Error",
    };
    name.to_string()
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
                    answer_with: p.answer_with,
                    options: p.options.clone(),
                    selected_option: 0,
                })
                .collect(),
            active_idx: 0,
            view_mode: ApprovalViewMode::TabView,
        })
    };
    // 判据只有一个，在 `ApprovalState::locks_composer` 上。这里算出来的是**默认值**：
    // `active_idx` 是 UI-本地状态（用户 Tab 到第几个），bridge 不知道，所以这一帧
    // 先按 0 算，`crates/app` 的 `merge` 夹完 `active_idx` 之后会用同一个方法重算。
    // 两处调的是同一个函数，不会各说各话。
    let locked = approval.as_ref().is_some_and(|a| a.locks_composer());

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
        Block::UserPrompt(text) => push_lines(entries, LineKind::UserPrompt, text),
        Block::AssistantText(text) => push_lines(entries, LineKind::AssistantText, text),
        Block::Thinking(text) => push_lines(entries, LineKind::Thinking, text),
        Block::Note(text) => push_lines(entries, LineKind::Note, text),
        Block::Error(text) => push_lines(entries, LineKind::Error, text),
        Block::Tool {
            id,
            name,
            input_summary,
            result,
            expanded,
        } => {
            entries.push(TranscriptEntry {
                continues_previous: false,
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
                        continues_previous: false,
                        kind: base,
                        text: String::new(),
                        block_id: Some(id.clone()),
                    });
                }
                for (i, line) in lines.iter().enumerate() {
                    let kind = kind(i, line);
                    entries.push(TranscriptEntry {
                        continues_previous: false,
                        kind,
                        text: strip_diff_marker(kind, line),
                        block_id: Some(id.clone()),
                    });
                }
            } else {
                for (i, line) in lines[..FOLD_LINE_THRESHOLD].iter().enumerate() {
                    let kind = kind(i, line);
                    entries.push(TranscriptEntry {
                        continues_previous: false,
                        kind,
                        text: strip_diff_marker(kind, line),
                        block_id: Some(id.clone()),
                    });
                }
                let hidden = lines.len() - FOLD_LINE_THRESHOLD;
                entries.push(TranscriptEntry {
                    continues_previous: false,
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

/// 一行 diff 去掉行首那个标记之后的文本。
///
/// 渲染那边按 `LineKind` 自己加前缀（`DiffOld` → `  - `，`DiffNew` → `  + `），
/// 原文里的 `-`/`+` 再留着就成了 `- -GREETING = "hello"`——真跑一次一眼看见。
/// 上下文行同理：unified diff 的上下文以一个空格开头，去掉之后才和加减行对齐。
fn strip_diff_marker(kind: LineKind, line: &str) -> String {
    match kind {
        // 表头两行本身要连标记一起留着——`--- a.rs (before)` 去掉一个 `-` 就成了
        // `-- a.rs`，反而看不懂。
        LineKind::DiffContext if line.starts_with("--- ") || line.starts_with("+++ ") => {
            line.to_string()
        }
        LineKind::DiffOld | LineKind::DiffNew | LineKind::DiffContext => {
            let mut chars = line.chars();
            match chars.next() {
                Some('-') | Some('+') | Some(' ') => chars.as_str().to_string(),
                _ => line.to_string(),
            }
        }
        _ => line.to_string(),
    }
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

/// 一段文本 → 一行一条 entry。
///
/// **一条 entry 就是屏幕上的一行**，这是 `tui::regions::transcript::render_body` 的
/// 前提（它按 `entries.take(视口高度)` 取，一条画一行）。而 ratatui 的 `Line` 对里面
/// 的 `\n` 不是换行、是**直接吞掉**：`Line::from("aaa\nbbb")` 画出来是 `aaabbb`。
///
/// 所以带 `\n` 的文本塞进一条 entry，屏幕上得到的是一串挤在一起、再被宽度截断的字
/// ——模型每一条带分段或列表的回答都是这样。工具结果那条路径一直是按行拆的
/// （`result.text.lines()`），只有这五种文本块不是；正是这个不对称让它活了这么久。
///
/// 空串给一条空 entry 而不是零条：一个空的 note/error 仍然占一行，和以前一样。
fn push_lines(entries: &mut Vec<TranscriptEntry>, kind: LineKind, text: &str) {
    if text.is_empty() {
        entries.push(plain(kind, ""));
        return;
    }
    // `lines()` 而不是 `split('\n')`：前者认 `\r\n`，也不会为结尾那个换行多造一条
    // 空行（流式文本经常以换行收尾）。段落之间的空行照样保留。
    //
    // 第二行起打上 `continues_previous`：谁要还原"这原本是一段"，读的必须是这个
    // 标记。恢复出来的转录里两次相邻的用户提交（发一句、Ctrl+C、再发一句）之间
    // 什么都没有，按相邻拼会把它们粘成一条。
    for (i, line) in text.lines().enumerate() {
        let mut entry = plain(kind, line);
        entry.continues_previous = i > 0;
        entries.push(entry);
    }
}

fn plain(kind: LineKind, text: &str) -> TranscriptEntry {
    TranscriptEntry {
        continues_previous: false,
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
        Reducer::build("test-model".into(), "/tmp".into(), None, Vec::new())
    }

    fn frame(rx: &watch::Receiver<FrameState>) -> FrameState {
        rx.borrow().clone()
    }

    fn question(id: &str, options: &[(&str, &str)]) -> QuestionEvent {
        QuestionEvent::Ask(PendingQuestion {
            id: id.into(),
            header: "Branch name".into(),
            question: "叫什么好？".into(),
            options: options
                .iter()
                .map(|(k, l)| (k.to_string(), l.to_string()))
                .collect(),
        })
    }

    fn only_pending(f: &FrameState) -> ApprovalRequest {
        let approval = f
            .composer
            .content
            .approval
            .as_ref()
            .expect("a dialog should be up");
        assert_eq!(approval.pending.len(), 1);
        approval.pending[0].clone()
    }

    /// 多选题：选项原样上屏，`key` 一个字都不能改——模型可能在拿它做匹配。
    #[test]
    fn a_multiple_choice_question_becomes_a_chooser() {
        let (r, rx) = reducer();
        r.apply_question(question("t1", &[("a", "feat/x"), ("b", "fix/y")]));

        let f = frame(&rx);
        let req = only_pending(&f);
        assert_eq!(req.prompt_id, "t1");
        assert_eq!(req.tool_name, "Branch name");
        assert_eq!(req.answer_with, AnswerWith::Choose);
        assert_eq!(
            req.options,
            vec![
                ApprovalOption::Answer {
                    key: "a".into(),
                    label: "feat/x".into()
                },
                ApprovalOption::Answer {
                    key: "b".into(),
                    label: "fix/y".into()
                },
            ]
        );
        assert!(
            f.composer.content.editor.locked,
            "选择题期间 composer 该锁住"
        );
    }

    /// 自由文本题：**不能**退化成一个没有选项的选择器（那是个推不动的死框），
    /// 而且 composer 必须留着能用——答案就是要在那儿打出来的。
    #[test]
    fn a_free_form_question_leaves_the_composer_usable() {
        let (r, rx) = reducer();
        r.apply_question(question("t2", &[]));

        let f = frame(&rx);
        let req = only_pending(&f);
        assert_eq!(req.answer_with, AnswerWith::Type);
        assert!(req.options.is_empty());
        assert!(
            !f.composer.content.editor.locked,
            "自由文本题锁住 composer 就没有任何一个键能推进它了"
        );
    }

    /// 提问方不等了，框就得收走——尤其是选择题，它占着 composer。
    #[test]
    fn withdrawing_a_question_takes_the_dialog_away() {
        let (r, rx) = reducer();
        r.apply_question(question("t3", &[("a", "A")]));
        assert!(frame(&rx).composer.content.approval.is_some());

        r.apply_question(QuestionEvent::Withdraw("t3".into()));
        let f = frame(&rx);
        assert!(f.composer.content.approval.is_none());
        assert!(!f.composer.content.editor.locked);
    }

    /// 权限提问和模型提问排在同一个队列里，各自保留自己的答法。
    #[test]
    fn a_permission_prompt_and_a_question_keep_their_own_shapes() {
        let (r, rx) = reducer();
        r.apply_event(AgentEvent::PermissionPrompt {
            prompt_id: "p1".into(),
            tool_name: "Bash".into(),
            message: "run it?".into(),
            paths: Vec::new(),
            turn_id: String::new(),
        });
        r.apply_question(question("t4", &[]));

        let f = frame(&rx);
        let pending = &f.composer.content.approval.as_ref().unwrap().pending;
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].answer_with, AnswerWith::Choose);
        assert_eq!(pending[0].options, permission_options());
        assert_eq!(pending[1].answer_with, AnswerWith::Type);
        assert!(
            f.composer.content.editor.locked,
            "队列里只要还有一道选择题，composer 就得锁着"
        );
    }

    /// 换会话（`/resume`）会重建整个 bridge，包括归约器。定时器不收工的话，每换
    /// 一次就留一个永远醒着的 500ms task，各自攥着一整棵 `DomainState`。
    #[tokio::test(start_paused = true)]
    async fn the_ticker_stops_once_nobody_is_watching() {
        let catalog = crate::commands::CommandCatalog::new(Arc::new(
            runtime::commands::CommandRegistry::new(),
        ))
        .0;
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reducer, frame_rx) = Reducer::spawn(
            event_rx,
            crate::ask::Questions::new().1,
            "m".into(),
            "/tmp".into(),
            catalog,
            Vec::new(),
        );
        // 一个订阅方也没有 = 这一帧没有人会看见。
        drop(frame_rx);
        let watching = Arc::downgrade(&reducer);
        drop(reducer);
        drop(event_tx);

        // 定时器 + 事件 task 各自认出自己没事可做，最后一个 `Arc` 才会落地。
        for _ in 0..8 {
            tokio::time::advance(STATUS_TICK).await;
            tokio::task::yield_now().await;
        }
        assert!(
            watching.upgrade().is_none(),
            "没人看了之后归约器还被后台 task 攥着"
        );
    }

    /// **模型每一条带分段或列表的回答都栽在这里。** 一条 entry 是屏幕上的一行，
    /// 而 ratatui 的 `Line` 会把里面的 `\n` 直接吞掉（不是换行，连空格都不给），
    /// 于是整段答案挤成一串再被宽度截断。工具结果那条路径一直是按行拆的，只有
    /// 这五种文本块不是。
    #[test]
    fn multi_line_text_becomes_one_entry_per_line() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("q".into());
        r.apply_event(AgentEvent::TextDelta {
            text: "第一段。\n\n第二段。\n- 甲\n- 乙".into(),
            turn_id,
        });

        let f = frame(&rx);
        let said: Vec<&str> = f
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::AssistantText)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(said, ["第一段。", "", "第二段。", "- 甲", "- 乙"]);
        assert!(
            f.transcript
                .body
                .entries
                .iter()
                .all(|e| !e.text.contains('\n')),
            "任何一条 entry 里都不该再有换行——它到屏幕上会被吞掉"
        );

        // 拆出来的第二行起要打上标记：`crates/app` 靠它把一次提交/一段回答拼回去，
        // 而"相邻"这个依据是不成立的（恢复出来的转录里相邻的两条可能是两次提交）。
        let flags: Vec<bool> = f
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::AssistantText)
            .map(|e| e.continues_previous)
            .collect();
        assert_eq!(flags, [false, true, true, true, true]);
    }

    /// `/doctor` 的报告是多行的，必须真的画成多行。
    ///
    /// 这条把 `doctor::render` 和转录的行模型钉在一起：报告那边改成一行一条
    /// （或者这边不再拆行）时，它会先挂。
    #[test]
    fn the_doctor_report_lands_as_one_entry_per_check() {
        let settings = base::settings::Settings::defaults_for("m");
        let report = base::interface::health::HealthChecks::from_vec(crate::doctor::checks(
            &settings, None, true,
        ))
        .report();

        let (r, rx) = reducer();
        r.note(crate::doctor::render(&report));

        let f = frame(&rx);
        let notes: Vec<&str> = f
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::Note)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(
            notes.len(),
            report.checks.len() + 1,
            "标题一行 + 每条检查一行，实际: {notes:#?}"
        );
        assert!(notes.iter().all(|t| !t.contains('\n')));
        assert!(notes[0].starts_with("doctor:"));
    }

    /// 空文本仍然占一行，和以前一样；结尾的换行不该凭空多造一条空行（流式文本
    /// 经常以换行收尾）。
    #[test]
    fn empty_and_trailing_newline_text_keep_their_shape() {
        let (r, rx) = reducer();
        r.note(String::new());
        r.note("尾巴上有换行\n".into());

        let f = frame(&rx);
        let notes: Vec<&str> = f
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| e.kind == LineKind::Note)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(notes, ["", "尾巴上有换行"]);
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

    /// `spawn` 起的那两个后台 task 从来没被测过——所有测试都直接调 `apply_event`，
    /// 把"事件真的从通道流进来、快照真的广播出去"这一段整个绕开了。这里走真链路：
    /// 往 `EventReceiver` 里塞事件，从 `watch` 那头等广播。
    #[tokio::test]
    async fn spawn_consumes_the_event_channel_and_broadcasts_snapshots() {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (catalog, _cmds) = crate::commands::CommandCatalog::new(Arc::new(
            runtime::commands::CommandRegistry::new(),
        ));
        let (r, mut frame_rx) = Reducer::spawn(
            event_rx,
            crate::ask::Questions::new().1,
            "m".into(),
            "/tmp".into(),
            catalog,
            Vec::new(),
        );

        let turn_id = r.begin_turn("问".into());
        event_tx
            .send(AgentEvent::TextDelta {
                text: "答".into(),
                turn_id,
            })
            .unwrap();

        // 等的是"答出现了"，不是"广播了一次"——`begin_turn` 自己就会广播一次，
        // 只等一次 `changed()` 会拿到那一帧然后误判。
        let texts = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let texts: Vec<String> = frame_rx
                    .borrow()
                    .transcript
                    .body
                    .entries
                    .iter()
                    .map(|e| e.text.clone())
                    .collect();
                if texts.len() >= 2 {
                    return texts;
                }
                frame_rx.changed().await.unwrap();
            }
        })
        .await
        .expect("事件应该被后台 task 消费并广播");
        assert_eq!(texts, vec!["问", "答"]);
    }

    /// 事件通道关掉之后后台 task 就该收工，不该空转——`Agent` 退出时就是这条路。
    #[tokio::test]
    async fn the_event_task_ends_when_the_channel_closes() {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (catalog, _cmds) = crate::commands::CommandCatalog::new(Arc::new(
            runtime::commands::CommandRegistry::new(),
        ));
        let (r, frame_rx) = Reducer::spawn(
            event_rx,
            crate::ask::Questions::new().1,
            "m".into(),
            "/tmp".into(),
            catalog,
            Vec::new(),
        );
        drop(event_tx);
        tokio::task::yield_now().await;
        // 通道关了，reducer 本身照常可用（app 还要靠它显示最后那一屏）。
        r.note("engine gone".into());
        assert!(frame_rx
            .borrow()
            .transcript
            .body
            .entries
            .iter()
            .any(|e| e.text == "engine gone"));
    }

    /// 状态行的秒数靠 500ms 的 tick 走字，不靠事件。这条走 tokio 的假时钟：
    /// 不 sleep 真实时间，也就不会因为机器慢而 flaky。
    #[tokio::test(start_paused = true)]
    async fn the_ticker_keeps_the_elapsed_time_moving_without_any_events() {
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (catalog, _cmds) = crate::commands::CommandCatalog::new(Arc::new(
            runtime::commands::CommandRegistry::new(),
        ));
        let (r, frame_rx) = Reducer::spawn(
            event_rx,
            crate::ask::Questions::new().1,
            "m".into(),
            "/tmp".into(),
            catalog,
            Vec::new(),
        );
        r.begin_turn("跑一个长任务".into());

        let elapsed = |f: &FrameState| match &f.operation_status.status_line.content {
            Some(StatusContent::TurnRunning { elapsed_secs, .. }) => *elapsed_secs,
            other => panic!("状态行应该是 TurnRunning，实际: {other:?}"),
        };
        assert_eq!(elapsed(&frame_rx.borrow()), 0);

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(elapsed(&frame_rx.borrow()) >= 3, "3 秒之后状态行的秒数没动");
    }

    /// 需求场景 5：上一轮还在跑的时候又提交一句。
    ///
    /// 排队本身是 Core 的语义（`Agent::run` 是串行 `recv → process_turn`），
    /// 这里守的是**转录别串行错乱**：后到的第一轮增量必须落回第一轮那个块，
    /// 而不是追加到刚开的新 turn 上。以前这条路径一个测试都没有。
    #[test]
    fn a_second_prompt_while_the_first_turn_is_still_streaming() {
        let (r, rx) = reducer();
        let first = r.begin_turn("第一句".into());
        r.apply_event(AgentEvent::TextDelta {
            text: "第一轮答".into(),
            turn_id: first.clone(),
        });
        // 还没等到 TurnComplete 就又发了一句。
        let second = r.begin_turn("第二句".into());
        // 第一轮的后续增量这时才到。
        r.apply_event(AgentEvent::TextDelta {
            text: "案".into(),
            turn_id: first,
        });
        r.apply_event(AgentEvent::TextDelta {
            text: "第二轮答案".into(),
            turn_id: second,
        });

        let texts: Vec<String> = frame(&rx)
            .transcript
            .body
            .entries
            .iter()
            .map(|e| e.text.clone())
            .collect();
        assert_eq!(
            texts,
            vec!["第一句", "第一轮答案", "第二句", "第二轮答案"],
            "两轮的内容不能串在一起"
        );
    }

    /// 并发的两次工具调用：结果按 `id` 各回各家。
    ///
    /// 原来只有"一个工具块"的测试——那种情况下"按 id 配对"和"谁在前给谁"行为
    /// 一样，配对逻辑其实没被验证过（变异测试把 `id` 比较删掉，全绿）。
    #[test]
    fn tool_results_go_to_their_own_call_when_several_are_in_flight() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("并行读两个文件".into());
        for (id, name) in [("t1", "Read"), ("t2", "Grep")] {
            r.apply_event(AgentEvent::ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
                turn_id: turn_id.clone(),
            });
        }
        // 结果**乱序**回来——真实情况就是谁先跑完谁先回。
        r.apply_event(AgentEvent::ToolResult {
            id: "t2".into(),
            name: "Grep".into(),
            content: "grep 的结果".into(),
            is_error: Some(false),
            turn_id: turn_id.clone(),
        });
        r.apply_event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "Read".into(),
            content: "read 的结果".into(),
            is_error: Some(false),
            turn_id,
        });

        let entries = frame(&rx).transcript.body.entries;
        let of = |id: &str| -> Vec<String> {
            entries
                .iter()
                .filter(|e| e.block_id.as_deref() == Some(id) && e.kind == LineKind::ToolResultOk)
                .map(|e| e.text.clone())
                .collect()
        };
        assert_eq!(of("t1"), vec!["read 的结果"], "t1 拿到了别人的结果");
        assert_eq!(of("t2"), vec!["grep 的结果"], "t2 拿到了别人的结果");

        // 每个块的标题行也挂着自己的 block_id——展开/折叠和块选择都靠它。
        let headings: Vec<Option<&str>> = entries
            .iter()
            .filter(|e| e.kind == LineKind::ToolHeading)
            .map(|e| e.block_id.as_deref())
            .collect();
        assert_eq!(headings, vec![Some("t1"), Some("t2")]);
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

    /// 只钉 `kind` 不钉 `text` 曾经放跑过一个真会被看见的 bug：渲染层按 kind
    /// 自己画 `- ` / `+ `，而文本里原来那个标记还在，屏幕上就是 `- -    old();`。
    /// 所以这里钉的是**送给渲染层的文本已经不含行首标记**。
    ///
    /// `--- a.rs (before)` / `+++ a.rs (after)` 是例外：那是表头不是内容，砍掉一个
    /// `-` 会变成 `-- a.rs (before)`，反而看不懂——原样保留。
    #[test]
    fn diff_text_reaches_the_renderer_without_its_leading_marker() {
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
            content: "--- a.rs (before)\n\
                      +++ a.rs (after)\n\
                      @@ -1,3 +1,3 @@\n\
                      \x20fn main() {\n\
                      -    old();\n\
                      +    new();\n"
                .into(),
            is_error: Some(false),
            turn_id,
        });

        let f = frame(&rx);
        let diff: Vec<(LineKind, &str)> = f
            .transcript
            .body
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    LineKind::DiffOld | LineKind::DiffNew | LineKind::DiffContext
                )
            })
            .map(|e| (e.kind, e.text.as_str()))
            .collect();

        assert_eq!(
            diff,
            vec![
                (LineKind::DiffContext, "--- a.rs (before)"),
                (LineKind::DiffContext, "+++ a.rs (after)"),
                (LineKind::DiffContext, "@@ -1,3 +1,3 @@"),
                (LineKind::DiffContext, "fn main() {"),
                (LineKind::DiffOld, "    old();"),
                (LineKind::DiffNew, "    new();"),
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

    /// 用量**只能**从 `SubagentProgress` 来：父时间线上的 spawn/complete 括号里
    /// 没有 token 数，只有转发过来的 `TurnComplete` 带子代理这一轮的 `usage`。
    /// 以前这条事件被整个丢掉，于是子代理条永远是空的。
    ///
    /// 这里故意不发 `AgentSpawned` —— `SubagentProgress` 先到就得先建行（真跑时
    /// 两条来自不同的发送点，谁先到不该由我们赌）。
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

    /// AttaCore v0.1.1 起 `AgentSpawned`/`AgentCompleted` 真的会发了。这条钉的是
    /// 三种事件按**真实顺序**（spawn → progress → complete）走完，落在同一行上，
    /// 而且最后那条括号**不会把已有的 outcome 覆盖掉**：`end_turn` 是子代理自己的
    /// stop_reason，比委派层面的 `completed` 有信息量，屏幕上要留前者。
    #[test]
    fn the_spawn_complete_bracket_and_the_forwarded_stream_land_on_one_row() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("查一下".into());
        let label = "explore#3f2a1b7c";

        r.apply_event(AgentEvent::AgentSpawned {
            agent_id: label.into(),
            parent_turn: 1,
            turn_id: "s1".into(),
        });
        let bar = frame(&rx).sub_agent_bar.agents;
        assert_eq!(bar.len(), 1, "spawn 自己就该把行建出来");
        assert_eq!(bar[0].name, label);

        r.apply_event(AgentEvent::SubagentProgress {
            agent_label: label.into(),
            agent_session_id: "s1".into(),
            agent_type: Some("explore".into()),
            parent_session_id: "p1".into(),
            parent_turn: 1,
            event: Box::new(AgentEvent::TurnComplete {
                stop_reason: "end_turn".into(),
                api_calls: 1,
                tool_calls: 2,
                usage: base::interface::model::Usage {
                    input_tokens: 120,
                    output_tokens: 30,
                },
                turn_id,
            }),
        });

        r.apply_event(AgentEvent::AgentCompleted {
            agent_id: label.into(),
            outcome: "completed".into(),
            turn_id: "s1".into(),
        });

        let bar = frame(&rx).sub_agent_bar.agents;
        assert_eq!(bar.len(), 1, "三种事件是同一个 label，不该分成两行");
        assert_eq!(bar[0].state, SubAgentState::Done);
        assert_eq!(bar[0].token_usage, 150);
        assert_eq!(
            bar[0].elapsed_or_status, "end_turn",
            "委派层面的 completed 不该盖掉子代理自己的 stop_reason"
        );
    }

    /// 失败的子代理必须照实标成 Failed。
    ///
    /// `AgentCompleted` 是**最后到**的那条，无条件写 `Done` 就会把一个刚报错的
    /// 子代理抹成正常结束——屏幕上看不出出过事。这在 Core 真发这对事件之前是
    /// 摸不到的死路径，所以以前没人发现。
    #[test]
    fn a_failed_sub_agent_is_not_whitewashed_by_the_closing_bracket() {
        let (r, rx) = reducer();
        let turn_id = r.begin_turn("查一下".into());
        let label = "explore#deadbeef";

        r.apply_event(AgentEvent::AgentSpawned {
            agent_id: label.into(),
            parent_turn: 1,
            turn_id: "s1".into(),
        });
        r.apply_event(AgentEvent::SubagentProgress {
            agent_label: label.into(),
            agent_session_id: "s1".into(),
            agent_type: Some("explore".into()),
            parent_session_id: "p1".into(),
            parent_turn: 1,
            event: Box::new(AgentEvent::Error {
                code: "turn_error".into(),
                message: "delegation depth limit reached\n还有第二行".into(),
                turn_id,
            }),
        });
        assert_eq!(
            frame(&rx).sub_agent_bar.agents[0].state,
            SubAgentState::Failed
        );

        r.apply_event(AgentEvent::AgentCompleted {
            agent_id: label.into(),
            outcome: "failed".into(),
            turn_id: "s1".into(),
        });

        let bar = frame(&rx).sub_agent_bar.agents;
        assert_eq!(bar[0].state, SubAgentState::Failed, "收尾括号不许洗白失败");
        assert_eq!(
            bar[0].elapsed_or_status, "delegation depth limit reached",
            "留着报错原文的第一行，别换成 failed"
        );
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

    /// resume：读回来的历史铺成转录，形状要和实时事件流一致——用户文本开新 turn，
    /// 工具调用配对上它的结果，思考块保留。
    #[test]
    fn restored_history_is_laid_out_like_a_live_transcript() {
        use base::message::{ContentBlock, Message, ToolResultContent};

        let history = vec![
            Message::User {
                content: vec![ContentBlock::Text {
                    text: "读一下 Cargo.toml".into(),
                    cache_control: None,
                }],
            },
            Message::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "好的".into(),
                        cache_control: None,
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "Cargo.toml"}),
                    },
                ],
                stop_reason: None,
                model: None,
            },
            // 历史里工具结果是**下一条 user 消息**的内容块，不是独立消息。
            Message::User {
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::Text("[workspace]".into()),
                    is_error: false,
                }],
            },
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "是个 workspace".into(),
                    cache_control: None,
                }],
                stop_reason: None,
                model: None,
            },
        ];

        let (_r, rx) = Reducer::build("m".into(), "/tmp".into(), None, history);
        let f = frame(&rx);
        let kinds: Vec<LineKind> = f.transcript.body.entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::UserPrompt,
                LineKind::AssistantText,
                LineKind::ToolHeading,
                LineKind::ToolResultOk,
                LineKind::AssistantText,
            ]
        );
        // 工具结果回填到了它自己的块上（block_id 和 heading 一致）。
        let heading = f.transcript.body.entries[2].block_id.clone();
        assert_eq!(heading.as_deref(), Some("t1"));
        assert_eq!(f.transcript.body.entries[3].block_id, heading);
        // header 钉的是恢复出来的最后一个用户输入。
        assert_eq!(
            f.transcript.header.text.as_deref(),
            Some("读一下 Cargo.toml")
        );
    }

    /// Core 塞进对话的 `<system-reminder>` 上下文在 jsonl 里也是 `user` 消息。
    /// 恢复时必须滤掉：不滤的话 resume 的第一屏全是 CLAUDE.md，真对话被挤没。
    #[test]
    fn injected_context_is_not_restored_as_a_user_prompt() {
        use base::message::{ContentBlock, Message};

        let text = |t: &str| ContentBlock::Text {
            text: t.into(),
            cache_control: None,
        };
        let history = vec![
            Message::User {
                content: vec![text(
                    "<system-reminder>\n# claudeMd\n一大坨…\n</system-reminder>",
                )],
            },
            Message::User {
                content: vec![text("\n<system-reminder>M .gitignore</system-reminder>")],
            },
            // 真话后面跟一段提醒：留前半截。
            Message::User {
                content: vec![text(
                    "真正的问题\n<system-reminder>别忘了…</system-reminder>",
                )],
            },
        ];

        let (_r, rx) = Reducer::build("m".into(), "/tmp".into(), None, history);
        let entries = frame(&rx).transcript.body.entries;
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["真正的问题"]);
    }

    /// 恢复出来的 turn 用 `restored-N` 做 id，之后的实时事件按自己的 turn_id
    /// 另起一个 turn，不会误挂到历史上。
    #[test]
    fn live_events_after_a_resume_do_not_merge_into_restored_turns() {
        use base::message::{ContentBlock, Message};

        let history = vec![Message::User {
            content: vec![ContentBlock::Text {
                text: "旧问题".into(),
                cache_control: None,
            }],
        }];
        let (r, rx) = Reducer::build("m".into(), "/tmp".into(), None, history);
        let turn_id = r.begin_turn("新问题".into());
        r.apply_event(AgentEvent::TextDelta {
            text: "新回答".into(),
            turn_id,
        });

        let texts: Vec<String> = frame(&rx)
            .transcript
            .body
            .entries
            .iter()
            .map(|e| e.text.clone())
            .collect();
        assert_eq!(texts, vec!["旧问题", "新问题", "新回答"]);
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

    /// Core 会发、而这里必须有话可说的每一个 stop_reason。
    ///
    /// 这张表是**手抄**的——Core 那边这些是散落在 `runtime::turn` 和
    /// `TurnPolicy` 里的字面量，没有可以遍历的枚举。所以它会过期，而过期的症状
    /// 在屏幕上看不见：turn 无声停住，跟卡死一模一样。改 Core 之后请对着
    /// `core/crates/runtime/src/turn.rs` 里的 `stop_reason:` 和
    /// `core/crates/core/src/interface/turn_policy.rs` 重新数一遍。
    #[test]
    fn other_early_stop_reasons_explain_themselves_in_the_transcript() {
        for (stop_reason, expected) in [
            ("max_turns", "max_turns"),
            ("budget_exceeded", "budget"),
            ("max_structured_output_retries", "structured output"),
            ("context_exceeded", "context limit"),
            ("host_ceiling", "host-configured"),
            ("stopped_by_hook", "hook"),
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
