# AttaCode TUI 布局设计（v3 — Z/R/S 层级坐标）

v3 相对 v2 的核心变化：不再用平铺的 `Z1/Z2/Z3` + `R1..R5` 描述，改为**层级坐标**（`Z.R.S`，0 起始），并修正了几处区域关系——`OperationStatus` 独立出来、`Composer` 成为容器（内含 `Editor`/`CompletionPopup`/`Approval` 三层叠加）、子 Agent 列表与"聚焦标签"拆成两个不同职责的区域。

---

## 一、坐标命名体系

```
Frame
  └── Z (顶层区域)         固定垂直堆叠，当前 5 个：Z0..Z4，0 起始编号
        └── R (二级子区域)  Z 内的嵌套/叠加子结构，0 起始，各 Z 内独立编号
              └── S (三级子区域)  R 内的子结构，0 起始，各 R 内独立编号
```

规则：
- **只在真正有子结构时才编下一级**——叶子区域（如 `Z3 SubAgentBar`）直接用 `Z3` 引用，不必写 `Z3.R0`。
- **编号只表示层级位置，不表示布局顺序**——调整某个区域在屏幕上的先后位置不需要重新编号；语义名字（`Composer`、`Approval`）始终是第一身份，坐标是沟通时的速记代号。
- **`Overlay`（Help / Settings / Resume / Doctor / Search）不在这套坐标里**——它是盖在整个 Frame 之上的全屏浮层，跟 `Z0..Z4` 的堆叠是两回事，不共享坐标空间，引用时直接写 `Overlay:<kind>`（如 `Overlay:Help`）。

### 完整对照表

| 坐标 | 语义名 | 层级关系 |
|---|---|---|
| `Z0` | Transcript | 顶层 |
| `Z0.R0` | Header | 嵌套在 Transcript 内 |
| `Z0.R1` | Body（转录主体） | 嵌套在 Transcript 内 |
| `Z1` | OperationStatus | 顶层 |
| `Z1.R0` | StatusLine | 嵌套，互斥内容 |
| `Z1.R1` | TaskList | 嵌套，独立显隐 |
| `Z2` | Composer | 顶层，容器 |
| `Z2.R0` | AppInfoLine | 嵌套 |
| `Z2.R1` | TopRule | 嵌套 |
| `Z2.R2` | Content | 嵌套，叠加容器（无自身显示内容） |
| `Z2.R2.S0` | Editor | 叠加层，底 |
| `Z2.R2.S1` | CompletionPopup | 叠加层，悬浮 |
| `Z2.R2.S2` | Approval | 叠加层，最上 |
| `Z2.R3` | BottomRule | 嵌套 |
| `Z3` | SubAgentBar | 顶层，叶子 |
| `Z4` | FooterHints | 顶层，叶子 |
| — | `Overlay:<kind>` | 独立层，不参与 Z0-Z4 坐标 |

### 三种区域关系（沟通时用来说明某个坐标之间是什么关系）

| 关系 | 含义 | 例子 |
|---|---|---|
| **替换**（互斥内容） | 同一坐标，几种内容二选一 | `Z1.R0` StatusLine：TurnRunning / Compacting / 隐藏 |
| **叠加**（常驻+追加） | 一个区域固定存在，另一个视条件出现在旁边，不影响前者渲染 | `Z2.R2.S0` Editor 常驻，`S2` Approval 出现时叠加在其上方 |
| **嵌套**（父子） | 子区域是父区域渲染范围内的一部分，不是独立坐标层级之外的东西 | `Z0.R0` Header 是 Transcript 自己顶部一行，不是 frame 顶部独立区域 |

---

## 二、总体布局图

```
┌──────────────────────────────────────────────────────────────┐
│ Z0 Transcript                              动态高度，填满剩余空间 │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Z0.R0 Header      0-1 行；已滚动且未贴底时显示               │ │
│ ├──────────────────────────────────────────────────────────┤ │
│ │ Z0.R1 Body        flex-grow；转录主体，默认自动贴底          │ │
│ └──────────────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────────────┤
│ Z1 OperationStatus                          条件显示             │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Z1.R0 StatusLine   TurnRunning | Compacting | 隐藏          │ │
│ │ Z1.R1 TaskList     有 todo/task 才显示，独立于 R0 是否隐藏    │ │
│ └──────────────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────────────┤
│ Z2 Composer                                 常驻容器             │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ Z2.R0 AppInfoLine   1 行，右对齐，程序级信息                 │ │
│ │ Z2.R1 TopRule       1 行分隔线，右侧聚焦上下文标签            │ │
│ │ Z2.R2 Content       叠加层容器：                            │ │
│ │   Z2.R2.S0 Editor          底层，常驻                       │ │
│ │   Z2.R2.S1 CompletionPopup 悬浮，仅 Editor 内容态下有意义     │ │
│ │   Z2.R2.S2 Approval        叠加在 Editor 上方，条件显示       │ │
│ │ Z2.R3 BottomRule    1 行分隔线，纯分隔                       │ │
│ └──────────────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────────────┤
│ Z3 SubAgentBar                              条件显示             │
├──────────────────────────────────────────────────────────────┤
│ Z4 FooterHints                              常驻，1 行            │
└──────────────────────────────────────────────────────────────┘
Overlay:{Help|ModelPicker|SessionResume|Settings|Doctor|TranscriptSearch}
  — 全帧浮层，盖在以上所有区域之上，独立于 Z0-Z4 坐标，一次只有一个
```

---

## 三、逐区域规格

### Z0 Transcript

#### Z0.R0 — Header

| | |
|---|---|
| 内容 | 当前滚动位置对应的那条用户输入的首行（`> xxx`） |
| 显隐 | 已向上滚动 **且** 未贴底时显示；贴底自动跟随时隐藏 |
| 键盘/鼠标 | 无（纯展示，随 Z0.R1 的滚动状态派生） |

```rust
struct HeaderState {
    text: Option<String>,
    source: HeaderSource,       // None | UserPrompt | SubAgent { agent_name }
}
```

样例：
```
> 帮我重构一下这个函数的错误处理
```

#### Z0.R1 — Body

| | |
|---|---|
| 内容 | 用户输入回显、assistant markdown 正文、thinking（折叠+暗色）、工具调用卡片（折叠一行摘要，可展开）、diff 内联渲染、系统通知（Note/Warning/Error）、子 Agent 折叠面板、压缩标记 |
| 接口-输入 | `entries: Vec<TranscriptEntry>` + `scroll: ScrollState` + `auto_follow: bool` |
| 接口-输出 | `toggle_expand(entry_id)` / `scroll(delta)` / `focus_subagent(id)` |

```rust
struct TranscriptBodyState {
    entries: Vec<TranscriptEntry>,
    scroll: ScrollState,
    auto_follow: bool,
}
struct TranscriptEntry { kind: LineKind, text: String, full_text: Option<String> }
enum LineKind {
    Banner, UserPrompt, AssistantText, ToolHeading, ToolResultOk, ToolResultErr,
    Note, Warning, Error, Thinking, Separator, Spacer,
    DiffOld, DiffNew, DiffContext, PermissionAsk, PermissionDenied, System, ToolOutput,
}
```

键盘：PageUp/PageDown、Ctrl-U/Ctrl-D 翻页；Home/End 跳首尾（非编辑态）；Ctrl-G 转录内搜索；F5 全局展开/折叠工具输出。
鼠标：滚轮逐行滚动；点击折叠卡片展开/折叠；拖拽走终端原生文本选择。

样例：
```
> 帮我看看 src/parser.rs 里的死循环

⏺ Read(src/parser.rs)
  ✓ read 214 lines

看起来问题在第 87 行的 while 循环……

⏺ Edit(src/parser.rs)
  ✓ applied
```

---

### Z1 OperationStatus

#### Z1.R0 — StatusLine（互斥内容）

```rust
struct StatusLineState { content: Option<StatusContent> }

enum StatusContent {
    TurnRunning {
        spinner: char, activity: String, elapsed_secs: u64,
        token_in: u64, token_out: u64,
    },
    Compacting {
        stage: CompactStage, stage_index: u8, stage_total: u8,
        tokens_before: u64, tokens_after: Option<u64>, estimated_saved: Option<u64>,
    },
}
enum CompactStage { MicroCompact, Collapse, LlmSummarize }
```

键盘：Esc（首次待确认，再按取消）/ Ctrl-C——绑定在全局，效果反映在这一行上，不算这个区域自己的接口。
鼠标：无。

样例：
```
⠋ Running tests…  12s   in:1.2K  out:456
⠴ Compacting (2/3: collapse)…  128K → 96K tok
```

#### Z1.R1 — TaskList（独立于 R0 是否隐藏）

```rust
struct TaskListState { items: Vec<TaskItem> }   // 空 = 高度 0
struct TaskItem { kind: TaskItemKind, status: ItemStatus, label: String }
enum TaskItemKind { TodoTask, PlanStep }         // 不含 SubAgent —— 子Agent 在 Z3
enum ItemStatus { Pending, Running, Done, Failed }
```

键盘：无直接操作。
鼠标：无。

样例：
```
✓ Build passed     ○ Update docs
● Lint check        (+2 more hidden)
```

---

### Z2 Composer

#### Z2.R0 — AppInfoLine

| | |
|---|---|
| 内容 | 与当前 turn/agent 无关的程序级通知（如版本更新提示），跟 Z1 的语义完全分开 |

```rust
struct AppInfoLineState { text: Option<String> }
```

样例：
```
                                          Update available: v0.3.0 — /upgrade
```

键盘：无。鼠标：点击可选跳转 `/upgrade`（可选功能，非必需）。

#### Z2.R1 — TopRule

```rust
struct TopRuleState {
    color: SeparatorColor,             // DarkGray | Cyan | Yellow | Red
    right_label: Option<LabelSource>,
}
enum LabelSource { None, SubAgent { name: String }, Skill { name: String }, Task { name: String }, Tool { name: String } }
```

> **待确认**：这次讨论的例子只提到"切到子 Agent 视图时显示子 Agent 名"，本版本暂时沿用现状的 `LabelSource` 四态（子Agent/Skill/Task/Tool 都能显示），如果要收窄成只显示子 Agent 名，告诉我再改接口。

样例：
```
───────────────────────────────────────── [agent: code-reviewer]
```

键盘/鼠标：无（纯展示，随当前聚焦上下文派生）。

#### Z2.R2 — Content（叠加容器，自身无显示内容）

##### Z2.R2.S0 — Editor（底层，常驻）

```rust
struct EditorState {
    mode: InputMode,        // Normal | MultiLine | VimNormal | BashEscape
    draft: String,
    cursor: usize,
    multiline: bool,
    paste_placeholder: Option<PasteInfo>,
    locked: bool,            // S2 Approval 激活时是否禁止编辑
}
```

> **待确认+默认值**：`locked` 在 Approval 激活时如何取值，本版本默认 `true`（只读，等待确认完再恢复可编辑），沿用 v2 里"权限对话框激活时输入区只读"的既有行为。如果产品上想让用户在等待确认时仍能继续排队打字，把默认改成 `false` 并在 FooterHints/StatusLine 上加"已排队 N 条"的提示位。

接口-输出：`submit(text, attachments)` / `request_completion(prefix)` / `request_history(dir)` / `open_external_editor()` / `bash_escape(cmd)`

键盘：Enter 提交（行尾 `\` 转字面换行）；Shift/Alt-Enter 强制换行；Up/Down 空/首尾行翻历史；Ctrl-A/E 行首尾；Ctrl-W 删词；Ctrl-U 清空；行首 `/` 开 slash 补全；`@` 开文件提及；行首 `!` 进 bash-escape；一键开 `$EDITOR`；Alt-V 切 vim 模式。
鼠标：点击定位光标；粘贴插入/占位符。

##### Z2.R2.S1 — CompletionPopup（悬浮，仅 Editor 内容态有意义）

```rust
struct CompletionPopupState {
    kind: CompletionKind,    // SlashCommand | FileMention | HistorySearch
    query: String,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}
```

键盘：Up/Down 移动；Tab/Enter 接受；Esc 关闭；输入字符继续过滤。
鼠标：点击行接受；滚轮滚动列表。

##### Z2.R2.S2 — Approval（叠加在 Editor 上方，条件显示）

```rust
struct ApprovalState {
    pending: Vec<ApprovalRequest>,
    active_idx: usize,
    view_mode: ApprovalViewMode,     // TabView | ListView
}
struct ApprovalRequest {
    id: String, tool_name: String, message: String,
    preview: Option<String>, options: Vec<ApprovalOption>,
}
enum ApprovalOption { PermitOnce, PermitSession, PermitProject, Deny }
enum ApprovalViewMode { TabView, ListView }
```

> **待确认+默认值**：`TabView` 与 `ListView` 谁在什么条件下用，本版本默认：`pending.len() <= 1` 时不需要 tab（等价于单卡片视图，UI 上可以直接复用 TabView 只是不画 tab 条）；`pending.len() > 1` 时默认 `TabView`；`ListView` 暂定为用户可手动切换的"看全部摘要"视图（比如按一个键从 TabView 切过去看汇总，再按回去继续逐个处理）。如果实际是别的触发逻辑（比如按请求类型自动选、或者只有某几类工具用 ListView），告诉我再改。

接口-输出：`switch_tab(idx)` / `set_view_mode(mode)` / `respond(id, decision)`——响应后从队列移除，若还有剩余自动选中下一项，全部清空后 `Z2.R2` 内容态切回只有 `S0 Editor`。

键盘（TabView）：Ctrl-Tab / Ctrl-Shift-Tab 切 tab；数字键跳指定 tab；Up/Down 或 j/k 移动当前卡片内选项；Enter 确认；y/n 快捷允许一次/拒绝；s 快捷"本次会话总是允许"；Esc/Ctrl-C 拒绝当前项（不影响队列里其他项）。
键盘（ListView）：Up/Down 在列表项间移动；Enter 展开为该项的确认卡片（等价于跳回 TabView 并选中它）；Esc 拒绝当前选中项。
鼠标：点击 tab 切换；点击选项确认；ListView 下点击某行展开。

样例（TabView，2 项待确认）：
```
 [Bash#1] [Edit#2]
┌──────────────────────────────────────────┐
│ Bash:                                     │
│   git push --force origin main            │
│                                            │
│   ❯ Yes                                   │
│     Yes, allow for this project           │
│     No                                    │
│                                            │
│   Enter=confirm  Esc=deny  Ctrl-Tab=next  │
└──────────────────────────────────────────┘
```

样例（ListView）：
```
┌──────────────────────────────────────────┐
│ 2 pending approvals                       │
│   ❯ Bash: git push --force origin main    │
│     Edit: src/parser.rs                   │
│                                            │
│   Enter=expand  ↑/↓=move                  │
└──────────────────────────────────────────┘
```

#### Z2.R3 — BottomRule

```rust
struct BottomRuleState { color: SeparatorColor }   // 纯分隔，无标签
```

> 目前没有信息表明这条线要带标签或其他内容，先按"纯视觉分隔"处理；如果它也该显示点什么（字数统计、输入合法性提示之类），告诉我再加字段。

---

### Z3 SubAgentBar（叶子，Composer 下方独立区域）

```rust
struct SubAgentBarState { agents: Vec<SubAgentStatus> }   // 空 = 高度 0
struct SubAgentStatus {
    id: String, name: String, state: SubAgentState,
    token_usage: u64, elapsed_or_status: String,
}
enum SubAgentState { Running, Done, Failed }
```

跟 `Z2.R1 TopRule` 的关系：`Z3` 是"有哪些子 Agent 在跑"的全量 roster，`Z2.R1` 是"我现在正聚焦看哪一个"的单点面包屑——两者同时存在，不冲突。

键盘：一键切换"仅运行中 / 含最近完成"。
鼠标：点击某行 → `focus_subagent(id)`，联动 `Z2.R1` 的聚焦标签与 `Z0.R1` 定位到该子 Agent 的折叠面板。

样例：
```
⠙ code-reviewer      12.3K tok  3m22s
✓ test-runner        45.8K tok  done
```

---

### Z4 FooterHints（叶子，常驻）

```rust
struct FooterHintsState {
    model: String, cwd: String, mode: AppMode,   // Normal | Plan | Auto
    right_hint: String, ctx_pct: Option<u8>,
}
```

键盘：Shift-Tab（或等效键）循环权限模式——绑定在全局，效果反映在这一行。
鼠标：点击模式 chip 循环切换（可选）。

样例：
```
claude-sonnet-5 · ~/proj  [Normal]              /=cmds · @=file · F4=help
```

---

### Overlay（全帧浮层，独立于 Z0-Z4 坐标）

覆盖 `Help` / `ModelPicker` / `SessionResume` / `Settings` / `Doctor` / `TranscriptSearch`，统一外框：标题栏 + 可滚动主体 + 底部 "Esc 关闭" 提示。

```rust
struct OverlayState { kind: OverlayKind, /* kind 各自的载荷 */ }
enum OverlayKind { Help, ModelPicker, SessionResume, Settings, Doctor, TranscriptSearch }
```

键盘：Esc 统一关闭；Up/Down/PageUp/PageDown 滚动导航；Enter 激活选中项；可搜索的浮层里 `/` 过滤。
鼠标：滚轮滚动；点击行激活。

---

## 四、颜色体系（沿用 v2，未改动）

| 语义 | 颜色 | 用途 |
|---|---|---|
| 主文本 | White | Assistant 输出 |
| 次文本 | DarkGray | 提示、分隔线、元信息 |
| 强调/交互 | Cyan | 选中项、活跃分隔线、spinner |
| 成功 | Green | `✓` 工具结果、完成状态、Diff 添加 |
| 警告 | Yellow | `⚠` 限流、context 用量、压缩中 |
| 错误 | Red | `✗` 错误、失败、Diff 删除 |
| 思考 | DarkGray + Italic | Thinking block |
| 子Agent | Magenta | 子Agent 标签、agent ID |
| 用户输入块 | `Rgb(220,220,235)` fg / `Rgb(65,65,85)` bg | UserPrompt |
| Diff 删除/添加 | Red/`Rgb(60,20,20)` · Green/`Rgb(20,60,20)` | DiffOld/DiffNew |

---

## 五、交互快捷键汇总

| 快捷键 | 行为 | 坐标 |
|---|---|---|
| Enter | 提交输入 | Z2.R2.S0 |
| `\` + Enter | 续行 | Z2.R2.S0 |
| ↑/↓ | 空输入翻历史；补全/选项导航 | Z2.R2.S0 / S1 / S2 |
| PgUp/PgDn/Home/End | 转录翻页/跳转 | Z0.R1 |
| Ctrl-C / Esc | 取消当前 turn（二次确认） | 全局 |
| Ctrl-G | 转录内搜索 | Z0.R1 → Overlay:TranscriptSearch |
| F5 | 全局展开/折叠工具输出 | Z0.R1 |
| Alt-V | Vim Normal/Insert 切换 | Z2.R2.S0 |
| `/` | 触发 slash 补全 | Z2.R2.S0 → S1 |
| `@` | 触发文件补全 | Z2.R2.S0 → S1 |
| `!` | Bash Escape | Z2.R2.S0 |
| Ctrl-Tab / 数字键 | 切换待确认项 | Z2.R2.S2 (TabView) |
| y/n/s/数字 | 快捷确认/拒绝/会话允许 | Z2.R2.S2 |
| Shift-Tab | 循环权限模式 | Z4 |
| F1/F2/F3/F4 | Doctor / Resume / Settings / Help | 全局 → Overlay |

---

## 六、待确认事项汇总

1. `Z2.R1 TopRule` 的 `right_label`：沿用现状 `LabelSource` 四态，还是收窄为只显示子 Agent 名？
2. `Z2.R2.S0 Editor` 的 `locked`：Approval 激活时输入框是只读还是仍可继续打字排队？本版本默认只读。
3. `Z2.R2.S2 Approval` 的 `TabView`/`ListView` 切换逻辑：自动按数量选，还是用户手动切？本版本默认"单项不显示 tab 条 / 多项默认 TabView，ListView 靠手动切换键"。
4. `Z2.R2 Content` 高度分配：Approval 出现时是动态挤压 Transcript（本版本默认），还是预留固定空间？
5. `Z2.R3 BottomRule` 要不要也带信息（目前按纯分隔处理）？
