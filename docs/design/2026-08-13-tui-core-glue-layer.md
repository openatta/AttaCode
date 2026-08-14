# TUI↔Core 粘合层 架构设计

**日期：** 2026-08-13
**基于需求：** `docs/reqs/2026-08-13-tui-core-glue-layer.md`

> **后续更新（Core 换到 `live-commands-and-per-turn-cancel` 之后）**
>
> 本文"现状核查"最后一条记的那个阻塞性缺口（Core 不调 `Agent.permission`、没有
> `AgentEvent::PermissionPrompt` 发送点）**已经不成立**。现在的 Core：
>
> - `runtime::turn` 把 `Agent.permission` 交给工具分派，`PermissionOutcome::Prompt`
>   会发 `AgentEvent::PermissionPrompt` 并挂起等 `InputMessage::PermissionResponse`，
>   超时（`execution.permission_prompt_timeout_secs`，默认 300s）按**拒绝**处理。
> - `permissions::rule_set_permission::RuleSetPermission` 就是那个官方实现，
>   `Builder::build()` 会给它 `bind_tool_registry` / `bind_session_state`。
>
> 因此本文里"`bridge` 自建 `GatePermission` 适配器"的决策（下方第 21/42/81 行附近）
> 已被撤销：`crates/bridge/src/permission.rs` 现在只负责按 settings 装配
> `RuleSetPermission`，不再自己实现 `Permission`。同理，`ApprovalOption` 的三档
> "允许"不再统一塌成 `Permit`，而是分别映射到 `PermitAlways { scope }`。

## 现状核查（只读探索结论）

- `crates/tui` 当前是纯渲染库：`lib.rs`/`frame_state.rs` 顶部注释明确写着"zero AttaCore dependency"，`Cargo.toml` 也确实不依赖任何 `core/crates/*`。整个仓库（`core/` 之外）没有任何 `main.rs`/`[[bin]]`——**目前没有可运行的程序，TUI 与 Core 之间完全没有接线**，与需求文档"从零设计"的前提一致。
- `crates/tui/src/frame_state.rs` 已经预先设计了较完整的分类模型：`LineKind`（含 ToolHeading/ToolResultOk/ToolResultErr/Thinking/Diff*/Error 等）、`ApprovalState`（权限确认）、`StatusContent::TurnRunning`（用量/耗时）、`SubAgentBarState`。这些是本次粘合层的渲染落点，无需重新设计分类维度，但缺少"可折叠工具块"和"常驻会话用量"两个字段（见下文"Crate/模块结构"）。
- CLAUDE.md 提到的 `crates/tui/src/slash/` 目录实际不存在——slash 命令系统同样是待建项，本次只定义其与粘合层的分流边界，不涉及具体命令表（需求文档已标注 Out of scope）。
- `core/daemon` 是最贴近的参考实现（JSON-RPC daemon，`SessionPool::new`/`SessionPool::create` 展示了 `runtime::agent::Builder` 的完整装配方式：`Model`/`AgentScene`/`Settings`/`Permission`/`MemoryStore` → `.build()` → `(Agent, EventReceiver, InputSender)` → `tokio::spawn(agent.run(cancel))`）。这一装配模式直接复用。
- **重要风险（非本次可解决）**：全仓搜索确认 `core/crates/runtime` 中，`execute_tool_inner`（`turn.rs`）在调用 `tool.call()` 前**没有**调用 `Agent.permission.check()`，且全 runtime crate 找不到 `AgentEvent::PermissionPrompt` 的发送点，遥测记录里 `user_approved` 被硬编码为 `true`。也就是说，`base::interface::permission::Permission` trait 和 `InputMessage::PermissionResponse` 的契约已定义，但 Core 侧尚未把"询问→挂起→等待响应"这条链路接进 turn 执行循环。daemon 的示例 `main.rs` 用 `AllowAllPermission` 桩、`session_pool::create` 强制 `PermissionMode::BypassPermissions`，回避了这个缺口。这直接影响需求场景 3（权限确认），已在"技术决策"中记录为阻塞性依赖，不在本仓库范围内解决（按 CLAUDE.md"AttaCore 补丁"流程，需要单独 `cd core` 开分支提 PR）。

## Crate/模块结构

| 模块 | 所在 Crate | 操作 | 职责 |
|------|-----------|------|------|
| Bridge 启动装配 | `crates/bridge`（新） | 新建 | 组装 `Model`/`AgentScene`/`Settings`/`MemoryStore`/`Compactor`/`HookRunner`/`InMemoryToolRegistry`/`HistoryStore`，调用 `runtime::agent::Builder::build()`，`tokio::spawn` 后台运行 `Agent::run()`（沿用 `core/daemon/src/session_pool.rs` 的装配顺序） |
| EngineHandle 命令 API | `crates/bridge` | 新建 | 对外暴露唯一入口：接收文本提交 / 权限响应 / 展开折叠 / 取消 / 关闭，转译为 `runtime::InputMessage` 送入 `InputSender` |
| AgentEvent 归约器 | `crates/bridge` | 新建 | 后台 task 持有 `EventReceiver`，消费 `AgentEvent`，维护富领域模型（逐 turn 流式文本缓冲、以 `id` 关联的工具调用块及其折叠态、会话累计用量），派生 `tui::FrameState` 并通过 `watch` 广播 |
| Permission 适配器 | `crates/bridge` | 新建 | 实现既有 `base::interface::permission::Permission` trait（`GatePermission`），包装既有 `permissions::gate::PermissionGate`，把 `PermissionDecision::Ask` 映射为 `PermissionOutcome::Prompt`；本身不解决上文的 Core 侧接线缺口，只保证 Core 补齐后可直接插入 |
| 应用入口 / 事件循环 | `crates/app`（新，bin，产出 `attacode`） | 新建 | 初始化 ratatui 终端；`tokio::select!` 同时驱动 crossterm 输入流与 `bridge` 的 `watch::Receiver<FrameState>` 变更；每次变更调用既有 `tui` 渲染函数 |
| 按键 → 动作分派 | `crates/app` | 新建 | 用既有 `keybindings::Resolver::on_key` 把 `KeyEvent` 解析成 action 名；本地动作（草稿编辑、滚动、展开/折叠某个 `block_id`）直接改 app 持有的 UI-only 状态，跨 Core 动作转 `EngineHandle::dispatch` |
| slash 命令分流入口 | `crates/app` | 新建 | 识别输入是否以 `/` 开头，做"本地处理 vs 转发 Core"的一次性分流；具体命令表/子命令解析不在本次范围，留给 CLAUDE.md 提到但尚未创建的 slash 子系统 |
| `TranscriptEntry` 扩展 | `crates/tui`（`frame_state.rs`） | 修改 | 增加可选 `block_id: Option<String>` 与 `collapsed: bool`（或等价字段），支撑场景 6"折叠大输出、按需展开"——不改变该文件"zero AttaCore dependency"的既有约束，字段仍是纯值类型 |
| `FooterHintsState` 扩展 | `crates/tui`（`frame_state.rs`） | 修改 | 增加会话累计用量字段（token_in/out、耗时等），落地需求"用量统计常驻显示"——复用既有 Z4 FooterHints（docs/TUI_DESIGN.md 中已是常驻区），不新增 Z 区 |
| workspace 成员登记 | 根 `Cargo.toml` | 修改 | `members` 增加 `"crates/bridge"`、`"crates/app"` |

## 数据流

- **状态位置**：渲染用的 `FrameState` 快照存在于 `app` 进程内存，由 `bridge` 的归约器计算、通过 `watch::channel` 广播；`bridge` 内部的富领域模型（逐 turn 缓冲、折叠态、累计用量、待响应权限表）是唯一权威来源，`app`/`tui` 不重复持有业务状态；Core 侧的会话消息历史仍归 `runtime::Agent` + `history` crate 管理（既有落盘机制），粘合层不重复实现持久化。
- **传递路径（用户输入）**：crossterm `KeyEvent` → `keybindings::Resolver::on_key` → action 名 → `app` 判定本地/跨 Core → `EngineHandle::dispatch(BridgeCommand::Submit { text })` → `bridge` 生成 `turn_id`（沿用 `daemon` 中 `Id::new().to_string()` 的做法）→ `InputSender.send(InputMessage::User { .. })` → `runtime::Agent` 既有的串行 `input_rx.recv().await → process_turn().await` 循环天然排队处理（满足场景 5，无需 `bridge` 自建队列）。
- **传递路径（Core 输出）**：`Agent` 内部 `event_tx.send(AgentEvent)` → `bridge` 后台 task 的 `EventReceiver` → 归约器按 `AgentEvent` 变体更新领域模型（`TextDelta` 追加流式文本；`ToolUse`/`ToolResult` 以 `id` 配对成一个折叠块；`PermissionPrompt` 推入 `ApprovalState.pending`；`TurnComplete` 累加常驻用量；`AgentSpawned`/`AgentCompleted` 更新 `SubAgentBarState`）→ 派生新 `FrameState` → `watch::Sender.send()` → `app` 渲染循环 `watch::Receiver.changed().await` 被唤醒 → 调用 `tui` 现有渲染函数。
- **传递路径（用户展开/折叠）**：`app` 本地捕获展开/折叠按键 → `EngineHandle::dispatch(BridgeCommand::ToggleExpand { block_id })` → 归约器在领域模型中翻转对应块的折叠态、重新派生该块的 `TranscriptEntry` 文本 → 广播新 `FrameState`（完整原文只在 `bridge` 内保存一份，`tui`/`app` 不重复持有）。
- **错误路径**：`EngineHandle::dispatch` 失败（对应 `InputSender`/`watch` 通道已关闭，即 Agent 后台 task 已退出）返回 `BridgeError`；`app` 将其转换为 `LineKind::Error` 转录条目展示，同时保持输入区可继续输入而不整体退出（满足场景 4"不中断整体交互流程"），仅在通道确认不可恢复时才提示重启会话。

## Trait 契约

| Trait | 所在 Crate | 关键方法 | 错误类型 | 使用者 |
|-------|-----------|---------|---------|--------|
| `EngineHandle` | `crates/bridge`（新定义） | `fn dispatch(&self, cmd: BridgeCommand) -> Result<(), BridgeError>`；`fn subscribe(&self) -> watch::Receiver<tui::FrameState>` | `BridgeError`（`thiserror`，变体覆盖：通道已关闭 / Agent 构建失败 / 未知 prompt_id） | `crates/app` |
| `Permission`（既有，`base::interface::permission`） | `crates/bridge` 新增实现类型 `GatePermission` | `async fn check(&self, tool_name: &str, tool_input: &Value, cwd: &Path, session_id: &str) -> PermissionOutcome`（签名沿用既有 trait，不改动） | 无（trait 本身不返回 `Result`） | `runtime::Agent`（Core 内部持有 `Arc<dyn Permission>` 调用；调用点本身待 Core 补齐，见"现状核查"） |

`BridgeCommand` / `ApprovalDecision`（`crates/bridge` 内新增的命令载荷类型，风格上与既有 `runtime::InputMessage` 对齐）：

```rust
enum BridgeCommand {
    Submit { text: String },
    RespondPermission { prompt_id: String, decision: ApprovalDecision },
    ToggleExpand { block_id: String },
    CancelTurn,
    Shutdown,
}

enum ApprovalDecision {
    PermitOnce,
    PermitSession,
    PermitProject,
    Deny,
}
```

## 状态管理

| 状态 | 类型 | 作用范围 | 持久化 |
|------|------|---------|--------|
| `FrameState` 快照 | `tui::FrameState` | `app` 进程内，每次变更触发重渲染 | 否 |
| 富领域模型（逐 turn 缓冲、工具块折叠态、累计用量、待响应权限表） | `bridge` 内部私有类型 | `bridge` 归约器 task 生命周期（进程内存） | 否（随进程退出丢失，会话历史另由 Core `history` crate 落盘，粘合层不重复） |
| Core 会话/消息历史 | `runtime::Agent` + `history` crate（既有） | 跨 turn / 跨进程重启（resume） | 是（沿用既有机制） |
| 键盘/输入本地 UI 状态（草稿、光标、滚动位置） | `tui::ComposerState` 等既有字段 | `app` 进程内，纯 UI 交互 | 否 |

## 技术决策

| 决策 | 选择 | 理由 | 替代方案 |
|------|------|------|---------|
| 粘合层拆分为几个 crate | 新增 `bridge`（Core 朝向，零 ratatui/crossterm 依赖）+ `app`（bin，事件循环，零 AttaCore 类型依赖） | 维持 `tui` 现有"zero AttaCore dependency"声明；`bridge` 对称地保持零终端库依赖，双向纯净，任一层都可独立替换/测试 | 把粘合逻辑直接塞进 `tui` 或塞进一个大而全的 `app` crate——会破坏 `tui` 已经写在代码注释里的既有约束 |
| Core→UI 状态广播机制 | `tokio::sync::watch::channel<FrameState>` | 渲染只关心"最新一帧"，`watch` 的覆盖式单值语义天然匹配；避免渲染侧再做一次 事件到状态的归约 | 把 `AgentEvent` 原样通过 `mpsc` 转发给 `app` 自行归约——会让 `app` 也要理解 AttaCore 类型，违反 `tui`/`app` 的纯净分层 |
| 新输入的排队策略 | 直接转发进 `runtime::Agent` 既有的单消费者 `InputReceiver`，`bridge` 不额外建队列 | `Agent::run()` 已是串行 `recv().await → process_turn().await` 循环，天然满足"排队、不打断当前轮次"（需求场景 5），重复实现队列是冗余状态源 | `bridge` 自建 `VecDeque` 缓冲后逐条转发——需要额外同步且与 Core 已有语义重复 |
| 折叠态与全量文本的存放位置 | 只存在 `bridge` 富领域模型里；`tui::TranscriptEntry` 只携带当前应显示文本 + `block_id` | 保持 `frame_state.rs` "pure data snapshot"定位；展开动作只需把 `block_id` 传回 `bridge` 触发重新派生，`app`/`tui` 不必持有两份文本 | 折叠状态放进 `FrameState`，`app` 原地改写文本——状态来源分裂，且 `app` 需要缓存全量文本 |
| 常驻用量统计的落点 | 扩展既有 `FooterHintsState`（docs/TUI_DESIGN.md 中已定义为常驻 Z4 区） | 复用已确定"始终渲染"的既有区域，不改动已定的 Z/R/S 布局树 | 新增独立 Z5 区域——需要改布局结构，超出粘合层本身的改动范围 |
| 交互式权限流程的实现边界 | `bridge` 提供 `GatePermission` 适配器（映射 `PermissionGate` 的 `Ask` → `PermissionOutcome::Prompt`），但明确记录 Core 侧尚未把该 outcome 接进 turn 执行循环，这部分标记为阻塞性外部依赖 | 实测确认：`execute_tool_inner` 直接调用 `tool.call()`，全 runtime crate 无 `AgentEvent::PermissionPrompt` 发送点，遥测里 `user_approved` 硬编码 `true`；这是 Core 能力缺口，按 CLAUDE.md 流程需要单独在 `core/` 开分支提 PR，不属于本仓库改动范围 | 在 `bridge`/`app` 层自建一套基于文本规则的"伪权限拦截"——治标不治本，且绕过 Core 规则引擎的权威判定，与"客户交付稳定性对标 Claude Code"的目标冲突 |
| slash 命令与粘合层的边界 | `app` 只做"是否 `/` 前缀 → 本地处理 or 转发 Core"的一次性分流；具体命令表留给尚未创建的 slash 子系统 | 与需求文档 Out of scope 一致，避免本次设计顺带吞掉一个独立子系统 | 本次一并设计完整 slash 命令分发表——超出需求范围 |
