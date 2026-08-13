//! Pure data snapshot for one rendered frame. No AttaCore types anywhere in this file —
//! see docs/TUI_DESIGN.md for the Z/R/S coordinate this maps to.

use serde::{Deserialize, Serialize};

// ═══ Z0 Transcript ═══

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineKind {
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

// ═══ Z1 OperationStatus ═══

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

// ═══ Z2 Composer ═══

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
    pub completion: Option<CompletionPopupState>,
    pub approval: Option<ApprovalState>,
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
pub struct CompletionPopupState {
    pub kind: CompletionKind,
    pub query: String,
    pub candidates: Vec<CompletionCandidate>,
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionKind {
    SlashCommand,
    FileMention,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionCandidate {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalState {
    pub pending: Vec<ApprovalRequest>,
    pub active_idx: usize,
    pub view_mode: ApprovalViewMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Opaque identifier the glue layer uses to route the user's decision back
    /// to the originating request (e.g. `AgentEvent::PermissionPrompt.prompt_id`).
    /// Not shown in the UI — display uses `tool_name`/`message` only.
    pub prompt_id: String,
    pub tool_name: String,
    pub message: String,
    pub options: Vec<ApprovalOption>,
    pub selected_option: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalOption {
    PermitOnce,
    PermitSession,
    PermitProject,
    Deny,
}

impl ApprovalOption {
    pub fn label(self) -> &'static str {
        match self {
            ApprovalOption::PermitOnce => "Yes",
            ApprovalOption::PermitSession => "Yes, allow for this session",
            ApprovalOption::PermitProject => "Yes, allow for this project",
            ApprovalOption::Deny => "No",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalViewMode {
    TabView,
    ListView,
}

// ═══ Z3 SubAgentBar ═══

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

// ═══ Z4 FooterHints ═══

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameState {
    pub transcript: TranscriptState,
    pub operation_status: OperationStatusState,
    pub composer: ComposerState,
    pub sub_agent_bar: SubAgentBarState,
    pub footer_hints: FooterHintsState,
}
