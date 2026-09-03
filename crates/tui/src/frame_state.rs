//! Pure data snapshot for one rendered frame. No AttaCore types anywhere in this file —
//! see docs/TUI_DESIGN.md for the Z/R/S coordinate this maps to.

use serde::{Deserialize, Serialize};

// ═══ transcript — Transcript / 转录区 ═══

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptState {
    pub header: HeaderState,
    pub body: TranscriptBodyState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderState {
    pub text: Option<String>,
    pub source: HeaderSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeaderSource {
    None,
    UserPrompt,
    SubAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptBodyState {
    pub entries: Vec<TranscriptEntry>,
    pub scroll: ScrollState,
    pub auto_follow: bool,
    /// `block_id` of the block transcript keys act on (expand/collapse). `None`
    /// means "no explicit selection" — the host then targets the most recent
    /// foldable block, which is what a user who never touches the navigation
    /// keys expects. Rendered as a marker on every line of the selected block.
    #[serde(default)]
    pub selected_block: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScrollState {
    pub offset: usize,
    pub total_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub kind: LineKind,
    pub text: String,
    /// Present when this entry belongs to a collapsible block (e.g. a large
    /// tool result folded to a summary line). The glue layer keeps the full
    /// content keyed by this id and re-derives the entry/entries on toggle;
    /// `None` for entries that are never foldable (plain text, headings, …).
    #[serde(default)]
    pub block_id: Option<String>,
    /// 这一条是不是一个**段**的第一行。
    ///
    /// 一段 = 转录里的一件事：一次用户输入、一段模型回答、一次工具调用（含它的
    /// 结果）、一条通知。一条 entry 是屏幕上的一行，所以一段通常是好几条。
    ///
    /// 两个消费方靠它：渲染那边在段与段之间留白（`LineKind::Spacer`），`crates/app`
    /// 那边把同一段的几行拼回一次提交。**都不能靠相邻推断**——恢复出来的转录里，
    /// 两次相邻的用户提交（发一句、Ctrl+C、再发一句）之间什么都没有，按相邻拼会把
    /// 两次提交粘成一条。
    #[serde(default)]
    pub starts_segment: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineKind {
    /// 段与段之间的空行。**是一条真 entry，不是渲染时插进去的**——整个滚动模型
    /// （`total_lines`、翻页步长、`scroll.offset`）都建立在"一条 entry = 屏幕一行"
    /// 上，渲染时凭空多画一行会让"跳过 N 条"不再等于"跳过 N 行"。
    Spacer,
    Banner,
    UserPrompt,
    AssistantText,
    ToolHeading,
    ToolResultOk,
    ToolResultErr,
    Note,
    Warning,
    Error,
    Thinking,
    DiffOld,
    DiffNew,
    DiffContext,
}

// ═══ operation_status — Operation Status / 状态区 ═══

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationStatusState {
    pub status_line: StatusLineState,
    pub task_list: TaskListState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusLineState {
    pub content: Option<StatusContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StatusContent {
    TurnRunning {
        spinner: char,
        activity: String,
        elapsed_secs: u64,
        token_in: u64,
        token_out: u64,
    },
    Compacting {
        stage: CompactStage,
        stage_index: u8,
        stage_total: u8,
        tokens_before: u64,
        tokens_after: Option<u64>,
        estimated_saved: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactStage {
    MicroCompact,
    Collapse,
    LlmSummarize,
}

impl CompactStage {
    pub fn label(self) -> &'static str {
        match self {
            CompactStage::MicroCompact => "micro-compact",
            CompactStage::Collapse => "collapse",
            CompactStage::LlmSummarize => "summarize",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskListState {
    pub items: Vec<TaskItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskItem {
    pub status: ItemStatus,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl ItemStatus {
    pub fn icon(self) -> &'static str {
        match self {
            ItemStatus::Running => "●",
            ItemStatus::Pending => "○",
            ItemStatus::Done => "✓",
            ItemStatus::Failed => "✗",
        }
    }
}

// ═══ composer — Composer / 输入区 ═══

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposerState {
    pub app_info: AppInfoLineState,
    pub top_rule: TopRuleState,
    pub content: ContentState,
    pub bottom_rule: BottomRuleState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfoLineState {
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopRuleState {
    pub color: SeparatorColor,
    pub right_label: Option<LabelSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeparatorColor {
    DarkGray,
    Cyan,
    Yellow,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelSource {
    SubAgent { name: String },
    Skill { name: String },
    Task { name: String },
    Tool { name: String },
}

impl LabelSource {
    pub fn text(&self) -> String {
        match self {
            LabelSource::SubAgent { name } => format!("[agent: {name}]"),
            LabelSource::Skill { name } => format!("[skill: {name}]"),
            LabelSource::Task { name } => format!("[task: {name}]"),
            LabelSource::Tool { name } => format!("[{name}]"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BottomRuleState {
    pub color: SeparatorColor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentState {
    pub editor: EditorState,
    pub picker: Option<PickerState>,
    pub ask: Option<AskState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorState {
    pub mode: InputMode,
    pub draft: String,
    pub cursor: usize,
    pub paste_placeholder: Option<PasteInfo>,
    pub locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    Normal,
    VimNormal,
    BashEscape,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PasteInfo {
    pub lines: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerState {
    pub kind: PickerKind,
    pub query: String,
    pub candidates: Vec<PickerCandidate>,
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PickerKind {
    SlashCommand,
    FileMention,
    /// `/resume` 的会话选择器。名字是完整的 session id，说明是"什么时候 / 多少条 /
    /// 关于什么"。
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerCandidate {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskState {
    pub pending: Vec<AskRequest>,
    pub active_idx: usize,
    pub view_mode: AskViewMode,
}

impl AskState {
    /// 现在归谁答。`None` 只可能是 `active_idx` 越界——调用方夹过之后就不会。
    pub fn active(&self) -> Option<&AskRequest> {
        self.pending.get(self.active_idx)
    }

    /// composer 该不该锁上。
    ///
    /// **这是那个唯一的判据。** 之前锁、键盘路由、弹窗可见性三处各自算了一遍：
    /// 锁看的是"队列里有没有"、路由看的是"当前这条是不是"、渲染看的是"队列空不空"。
    /// 三者一致时看不出问题，不一致时有三个症状——输入框画成灰的却能打字、排在
    /// 问答题后面的权限请求永远 Tab 不到、`/resume` 列表从屏幕上消失却还吃着键盘。
    /// 谁需要这个答案就调这里，别再各算各的。
    pub fn locks_composer(&self) -> bool {
        matches!(self.active(), Some(r) if r.answer_with == AnswerWith::Choose)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequest {
    /// Opaque identifier the glue layer uses to route the user's decision back
    /// to the originating request (e.g. `AgentEvent::PermissionPrompt.prompt_id`).
    /// Not shown in the UI — display uses `tool_name`/`message` only.
    pub prompt_id: String,
    pub tool_name: String,
    pub message: String,
    /// How this one is answered. Drives both rendering and key routing, so it
    /// is stated rather than inferred from `options` being empty — a request
    /// that renders a chooser with nothing to choose is a deadlock, and an
    /// inferred rule makes that one typo away.
    pub answer_with: AnswerWith,
    /// Empty exactly when `answer_with` is [`AnswerWith::Type`].
    pub options: Vec<AskOption>,
    pub selected_option: usize,
}

/// Where the answer comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerWith {
    /// Pick one of `options`. The composer is locked while this is up: the
    /// engine is blocked on the answer, so a keystroke that went to the draft
    /// instead would be typed at something that cannot read it.
    Choose,
    /// Type it. The composer stays open and the next submitted line is the
    /// answer — used when the model asked an open question rather than
    /// offering choices.
    Type,
}

/// Something the user can pick in the Ask Box.
///
/// Two unrelated questions share this dialog, which is why the four permission
/// answers and [`Answer`](Self::Answer) sit in one enum: *may this tool call
/// proceed* (asked by the engine's permission gate) and *the model would like
/// to know something* (`AskUserQuestion`). They look the same on screen and are
/// routed to completely different places — see `bridge::handle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskOption {
    PermitOnce,
    PermitSession,
    PermitProject,
    Deny,
    /// One of the model's own options. `key` goes back to it verbatim (it
    /// chose that string and may be matching on it); `label` is what the user
    /// reads.
    Answer {
        key: String,
        label: String,
    },
}

impl AskOption {
    pub fn label(&self) -> &str {
        match self {
            AskOption::PermitOnce => "Yes",
            AskOption::PermitSession => "Yes, allow for this session",
            AskOption::PermitProject => "Yes, allow for this project",
            AskOption::Deny => "No",
            AskOption::Answer { label, .. } => label,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskViewMode {
    TabView,
    ListView,
}

// ═══ sub_agent_bar — Sub-Agent Bar / 子代理条 ═══

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentBarState {
    pub agents: Vec<SubAgentStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentStatus {
    pub name: String,
    pub state: SubAgentState,
    pub token_usage: u64,
    pub elapsed_or_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubAgentState {
    Running,
    Done,
    Failed,
}

// ═══ footer_hints — Footer Hints / 底栏 ═══

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FooterHintsState {
    pub model: String,
    pub cwd: String,
    pub mode: AppMode,
    pub right_hint: String,
    /// Cumulative session usage — rendered as a persistent (always-visible)
    /// segment of the footer rather than a transcript entry.
    pub usage: SessionUsageState,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SessionUsageState {
    pub token_in: u64,
    pub token_out: u64,
    pub turn_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppMode {
    Normal,
    Plan,
    Auto,
}

impl AppMode {
    pub fn label(self) -> &'static str {
        match self {
            AppMode::Normal => "Normal",
            AppMode::Plan => "Plan",
            AppMode::Auto => "Auto",
        }
    }
}

// ═══ Top-level aggregate ═══

// ═══ btw — Side Question / 侧问区 ═══

/// 侧问区。`FrameState::btw` 为 `Some` 时它就是激活的。
///
/// 激活期间它**独占屏幕下半和键盘**：转录区压到约一半，它盖住状态区、输入区、底栏和
/// 子代理条。这是照 Claude Code 的 `/btw` 做的，包括"盖住之后主任务进度就看不见了"
/// ——CC 也是这样，那是这个形态的代价。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtwState {
    /// 当前正在看的那一问。
    pub question: String,
    /// 当前这一答。流式长出来的。
    pub answer: String,
    /// 还在等模型说话。
    pub streaming: bool,
    /// 答案的滚动位置（跳过的行数）。
    pub scroll: usize,
    /// 早前问答的问题行，新的在前，最多 5 条（照 CC）。
    pub earlier: Vec<String>,
    /// 除了上面那 5 条之外还有多少条更早的。
    pub older: usize,
    /// 正在看第几条（0 = 当前这条，往大 = 越早）。
    pub viewing: usize,
}

// ═══ FrameState ═══

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameState {
    pub transcript: TranscriptState,
    pub operation_status: OperationStatusState,
    pub composer: ComposerState,
    pub sub_agent_bar: SubAgentBarState,
    pub footer_hints: FooterHintsState,
    /// 侧问区。`Some` = 激活，它独占屏幕下半和键盘，上面那四个区域都不画。
    #[serde(default)]
    pub btw: Option<BtwState>,
}
