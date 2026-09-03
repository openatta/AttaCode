# AttaCode TUI 布局设计（v4 — 中英对照区域名）

v4 相对 v3 的核心变化：**废止 `Z/R/S` 字母+数字坐标**，改用「代码路径 + English + 中文」
三列对照的区域名。v3 那套坐标已于本版退役，任何地方看到 `Z0.R1` 都请按下表换算。

---

## 一、区域怎么称呼

**一个区域一个名字，三种写法指同一个东西：**

| 写法 | 长什么样 | 用在哪 |
|---|---|---|
| **代码路径**（唯一标识） | `composer.content.ask` | 代码、注释、提交信息、bug 报告里要精确的时候。它就是 `FrameState` 里的字段路径，能直接 grep |
| **English** | Ask Box | 英文讨论、英文注释 |
| **中文** | 输入·提问框 | 中文讨论、手工测试清单、打点报告 |

三者是**同一行表格的三列**，说哪个都行，但必须是表里有的那个词。

### 为什么废掉了 `Z/R/S`

- **它已经在说谎。** 面向测试的两处（`scripts/trace_report.py`、`scripts/testbed/prompts.md`）
  把任务清单标成 `Z1`（实际 `Z1.R1`）、把提问框标成 `Z2`（实际 `Z2.R2.S2`）——
  `Z2.R2.S2` 要数三层才写得对，于是大家写短，于是坐标失去了精度。
- **它和代码是两套词汇。** 代码里从来只用语义路径（模块名、结构体、`FrameState` 字段），
  坐标一次都没参与过计算，纯粹是文档里的第二套叫法，得靠人肉保持同步。
- **它经不起改布局。** 文档自己写着"编号不表示布局顺序"，可 `Z0..Z4` 实际就是自上而下的
  堆叠顺序；中间插一个区域，要么编号说谎，要么全体重编。
- **0 起始和文档自身的"一、二、三"对不上**，说"第二个区域"要在脑子里换算一次。

代码路径没有这些毛病：写错了 grep 不到、编译不过；插入或调序不用重编号；而且 bug 报告里
的名字**直接就是代码里的路径**。

> **一张能记住的图**：`docs/tui-region-map.html`（浏览器打开即可，鼠标移到任意一行，
> 图上对应区域会亮起来）。在线版：<https://claude.ai/code/artifact/99337c6b-bd10-452b-9737-96e3c31e4ae4>

### 完整对照表

| 代码路径（唯一标识） | English | 中文 | 关系 |
|---|---|---|---|
| `transcript` | Transcript | 转录区 | 顶层 |
| `transcript.header` | Transcript Header | 转录·顶栏 | 嵌套 |
| `transcript.body` | Transcript Body | 转录·正文 | 嵌套 |
| `operation_status` | Operation Status | 状态区 | 顶层 |
| `operation_status.status_line` | Status Line | 状态·状态行 | 嵌套，内容互斥 |
| `operation_status.task_list` | Task List | 状态·任务清单 | 嵌套，独立显隐 |
| `composer` | Composer | 输入区 | 顶层，容器 |
| `composer.app_info` | App Info Line | 输入·信息行 | 嵌套 |
| `composer.top_rule` | Top Rule | 输入·上分隔线 | 嵌套 |
| `composer.content` | Content | 输入·内容层 | 嵌套，叠加容器 |
| `composer.content.editor` | Editor | 输入·编辑器 | 叠加，底层常驻 |
| `composer.content.picker` | Picker | 输入·候选列表 | 叠加，悬浮 |
| `composer.content.ask` | Ask Box | 输入·提问框 | 叠加，最上 |
| `composer.bottom_rule` | Bottom Rule | 输入·下分隔线 | 嵌套 |
| `sub_agent_bar` | Sub-Agent Bar | 子代理条 | 顶层，叶子 |
| `footer_hints` | Footer Hints | 底栏 | 顶层，叶子 |
| `overlay` | Overlay | 浮层 | 独立层，盖在以上全部之上 |

> **`overlay` 不参与上面的堆叠**：它是盖在整个 frame 之上的全屏浮层，一次只有一个，
> 引用时写 `overlay:help` / `overlay:doctor` 这种形式。

### 两个改过名的区域，和改名的理由

| 旧名 | 新名 | 为什么 |
|---|---|---|
| `approval` / `ApprovalState` | `ask` / `AskState` | 它现在同时装**权限审批**和**模型提问**（`AskUserQuestion`）。"Approval" 只说中了一半，而键位层早就叫 `ask.confirm` / `ask.prev` / `ask.next-request`——新名字是去跟已有的词汇会合，不是另造一个 |
| `completion` / `CompletionPopupState` | `picker` / `PickerState` | 它现在装 slash 补全、文件提及、`/resume` 会话选择器**三种**。`PickerKind::Session` 这个变体本身就是"completion 这个名字撑不住了"的信号 |

### 三种区域关系

| 关系 | 含义 | 例子 |
|---|---|---|
| **替换**（内容互斥） | 同一个区域，几种内容二选一 | `operation_status.status_line`：TurnRunning / Compacting / 隐藏 |
| **叠加**（常驻 + 追加） | 一个区域固定存在，另一个视条件出现，不影响前者渲染 | `composer.content.editor` 常驻，`composer.content.ask` 出现时叠在它上方 |
| **嵌套**（父子） | 子区域是父区域渲染范围内的一部分 | `transcript.header` 是转录区自己顶部一行，不是 frame 顶部的独立区域 |

---

## 二、总体布局图

```
┌──────────────────────────────────────────────────────────────────────┐
│ transcript                     转录区        动态高度，填满剩余空间      │
│ ┌──────────────────────────────────────────────────────────────────┐ │
│ │ transcript.header            转录·顶栏     0-1 行；滚上去了才显示   │ │
│ ├──────────────────────────────────────────────────────────────────┤ │
│ │ transcript.body              转录·正文     撑满；默认自动贴底       │ │
│ └──────────────────────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────────────────────┤
│ operation_status               状态区        条件显示                  │
│ ┌──────────────────────────────────────────────────────────────────┐ │
│ │ operation_status.status_line  状态·状态行  跑turn / 压缩中 / 隐藏   │ │
│ │ operation_status.task_list    状态·任务清单 有待办才显示            │ │
│ └──────────────────────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────────────────────┤
│ composer                       输入区        常驻容器                  │
│ ┌──────────────────────────────────────────────────────────────────┐ │
│ │ composer.app_info            输入·信息行   1 行，程序级通知         │ │
│ │ composer.top_rule            输入·上分隔线                         │ │
│ │ composer.content             输入·内容层   三样叠在一起：           │ │
│ │   composer.content.editor      输入·编辑器    底层，常驻            │ │
│ │   composer.content.picker      输入·候选列表  悬浮（补全/@/会话）    │ │
│ │   composer.content.ask         输入·提问框    盖在编辑器上，条件显示 │ │
│ │ composer.bottom_rule         输入·下分隔线                         │ │
│ └──────────────────────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────────────────────┤
│ sub_agent_bar                  子代理条      条件显示                  │
├──────────────────────────────────────────────────────────────────────┤
│ footer_hints                   底栏          常驻，1 行                │
└──────────────────────────────────────────────────────────────────────┘
overlay:{help|model|resume|settings|doctor|search}          浮层
  — 盖在以上所有区域之上，不参与这个堆叠，一次只有一个
```

---

## 三、逐区域规格

### `transcript` — Transcript / 转录区

#### `transcript.header` — Transcript Header / 转录·顶栏

| | |
|---|---|
| 内容 | 当前滚动位置对应的那条用户输入的首行（`> xxx`） |
| 显隐 | 已向上滚动 **且** 未贴底时显示；贴底自动跟随时隐藏 |
| 键盘/鼠标 | 无（纯展示，随 `transcript.body` 的滚动状态派生） |

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

#### `transcript.body` — Transcript Body / 转录·正文

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

### `operation_status` — Operation Status / 状态区

#### `operation_status.status_line` — Status Line / 状态·状态行（内容互斥）

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

#### `operation_status.task_list` — Task List / 状态·任务清单（独立于状态行是否隐藏）

```rust
struct TaskListState { items: Vec<TaskItem> }   // 空 = 高度 0
struct TaskItem { kind: TaskItemKind, status: ItemStatus, label: String }
enum TaskItemKind { TodoTask, PlanStep }         // 不含 SubAgent —— 子代理在 sub_agent_bar
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

### `composer` — Composer / 输入区

#### `composer.app_info` — App Info Line / 输入·信息行

| | |
|---|---|
| 内容 | 与当前 turn/agent 无关的程序级通知（如版本更新提示），跟状态区的语义完全分开 |

```rust
struct AppInfoLineState { text: Option<String> }
```

样例：
```
                                          Update available: v0.3.0 — /upgrade
```

键盘：无。鼠标：点击可选跳转 `/upgrade`（可选功能，非必需）。

#### `composer.top_rule` — Top Rule / 输入·上分隔线

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

#### `composer.content` — Content / 输入·内容层（叠加容器，自身无显示内容）

##### `composer.content.editor` — Editor / 输入·编辑器（底层，常驻）

```rust
struct EditorState {
    mode: InputMode,        // Normal | MultiLine | VimNormal | BashEscape
    draft: String,
    cursor: usize,
    multiline: bool,
    paste_placeholder: Option<PasteInfo>,
    locked: bool,            // ask 里当前那条要用选的时候禁止编辑
}
```

> **待确认+默认值**：`locked` 在提问框激活时如何取值，本版本默认 `true`（只读，等待确认完再恢复可编辑），沿用 v2 里"权限对话框激活时输入区只读"的既有行为。如果产品上想让用户在等待确认时仍能继续排队打字，把默认改成 `false` 并在 FooterHints/StatusLine 上加"已排队 N 条"的提示位。

接口-输出：`submit(text, attachments)` / `request_completion(prefix)` / `request_history(dir)` / `open_external_editor()` / `bash_escape(cmd)`

键盘：Enter 提交（行尾 `\` 转字面换行）；Shift/Alt-Enter 强制换行；Up/Down 空/首尾行翻历史；Ctrl-A/E 行首尾；Ctrl-W 删词；Ctrl-U 清空；行首 `/` 开 slash 补全；`@` 开文件提及；行首 `!` 进 bash-escape；一键开 `$EDITOR`；Alt-V 切 vim 模式。
鼠标：点击定位光标；粘贴插入/占位符。

##### `composer.content.picker` — Picker / 输入·候选列表（悬浮）

```rust
struct PickerState {
    kind: PickerKind,        // SlashCommand | FileMention | Session
    query: String,
    candidates: Vec<PickerCandidate>,
    selected: usize,
}
struct PickerCandidate { name: String, description: String }
```

一个壳子装三种列表，靠 `kind` 区分：slash 补全、`@` 文件提及、`/resume` 会话选择器。
**会话那一档不画 `name`**（它是裸的 BASE58 session id，对人没有信息量），把宽度让给
`description`（什么时候 / 多少条 / 讲了什么）。

列表比屏幕长时开窗口滚动，边框上标 `第几条/共几条`；上方放不下一行内容加两条边框时
**整个不画**——画不出来却还吃着键盘，等于让人操作看不见的东西。

键盘：Up/Down 移动；Tab/Enter 接受；Esc 关闭；输入字符继续过滤。

##### `composer.content.ask` — Ask Box / 输入·提问框（叠加在编辑器上方，条件显示）

```rust
struct AskState {
    pending: Vec<AskRequest>,
    active_idx: usize,
    view_mode: AskViewMode,          // TabView | ListView
}
struct AskRequest {
    prompt_id: String, tool_name: String, message: String,
    answer_with: AnswerWith,         // Choose（选一个）| Type（打一行）
    options: Vec<AskOption>,         // Type 时为空
    selected_option: usize,
}
enum AnswerWith { Choose, Type }
enum AskOption {
    PermitOnce, PermitSession, PermitProject, Deny,   // 权限门的四个答案
    Answer { key: String, label: String },            // 模型自己给的选项
}
enum AskViewMode { TabView, ListView }
```

**一个框，两个来源。** 引擎的权限门（`AgentEvent::PermissionPrompt`）和模型的提问
（`AskUserQuestion`，见 `bridge::ask`）排在同一个队列里，屏幕上是同一个框，答案却回到
完全不同的地方——所以 `AskOption` 把两边的答案放在一个枚举里，由 `bridge::handle` 分流。

**`answer_with` 决定键盘归谁**，这是唯一的判据（`AskState::locks_composer`）：`Choose`
锁住编辑器、键盘归对话框；`Type` 不锁，用户在编辑器里打的下一行就是答案。锁、键盘路由、
候选列表可见性这三件事都读它，各算各的曾经造出过三个自相矛盾的状态。

> **待确认+默认值**：`TabView` 与 `ListView` 谁在什么条件下用，本版本默认：`pending.len() <= 1` 时不需要 tab（等价于单卡片视图，UI 上可以直接复用 TabView 只是不画 tab 条）；`pending.len() > 1` 时默认 `TabView`；`ListView` 暂定为用户可手动切换的"看全部摘要"视图（比如按一个键从 TabView 切过去看汇总，再按回去继续逐个处理）。如果实际是别的触发逻辑（比如按请求类型自动选、或者只有某几类工具用 ListView），告诉我再改。

接口-输出：`switch_tab(idx)` / `set_view_mode(mode)` / `respond(id, decision)`——响应后从队列移除，若还有剩余自动选中下一项，全部清空后 `composer.content` 切回只剩 `editor`。

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

#### `composer.bottom_rule` — Bottom Rule / 输入·下分隔线

```rust
struct BottomRuleState { color: SeparatorColor }   // 纯分隔，无标签
```

> 目前没有信息表明这条线要带标签或其他内容，先按"纯视觉分隔"处理；如果它也该显示点什么（字数统计、输入合法性提示之类），告诉我再加字段。

---

### `sub_agent_bar` — Sub-Agent Bar / 子代理条（叶子，输入区下方）

```rust
struct SubAgentBarState { agents: Vec<SubAgentStatus> }   // 空 = 高度 0
struct SubAgentStatus {
    id: String, name: String, state: SubAgentState,
    token_usage: u64, elapsed_or_status: String,
}
enum SubAgentState { Running, Done, Failed }
```

跟 `composer.top_rule` 的关系：`sub_agent_bar` 是"有哪些子代理在跑"的全量 roster，
`composer.top_rule` 是"我现在正聚焦看哪一个"的单点面包屑——两者同时存在，不冲突。

键盘：一键切换"仅运行中 / 含最近完成"。
鼠标：点击某行 → `focus_subagent(id)`，联动 `composer.top_rule` 的聚焦标签与
`transcript.body` 定位到该子代理的折叠面板。

样例：
```
⠙ code-reviewer      12.3K tok  3m22s
✓ test-runner        45.8K tok  done
```

---

### `footer_hints` — Footer Hints / 底栏（叶子，常驻）

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

### `overlay` — Overlay / 浮层（全帧浮层，独立于以上区域）

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

| 快捷键 | 行为 | 作用区域 |
|---|---|---|
| Enter | 提交输入 | `composer.content.editor` |
| `\` + Enter | 续行 | `composer.content.editor` |
| ↑/↓ | 空输入翻历史；候选列表/选项导航 | `composer.content` 的三层 |
| Tab | 待答队列里切下一条 | `composer.content.ask` |
| PgUp/PgDn/Home/End | 转录翻页/跳转 | `transcript.body` |
| Ctrl-C / Esc | 取消当前 turn（二次确认） | 全局 |
| Ctrl-G | 转录内搜索 | `transcript.body` → `overlay:search` |
| F5 | 全局展开/折叠工具输出 | `transcript.body` |
| Alt-V | Vim Normal/Insert 切换 | `composer.content.editor` |
| `/` | 触发 slash 补全 | `editor` → `picker` |
| `@` | 触发文件提及 | `editor` → `picker` |
| `!` | Bash Escape | `composer.content.editor` |
| Ctrl-Tab / 数字键 | 切换待答项 | `composer.content.ask`（TabView） |
| y/n/s/数字 | 快捷确认/拒绝/会话允许 | `composer.content.ask`（仅权限那一档） |
| Shift-Tab | 循环权限模式 | `footer_hints` |
| F1/F2/F3/F4 | Doctor / Resume / Settings / Help | 全局 → Overlay |

---

## 六、待确认事项汇总

1. `composer.top_rule` 的 `right_label`：沿用现状 `LabelSource` 四态，还是收窄为只显示子代理名？
2. `composer.content.editor` 的 `locked`：**已定**——由 `AskState::locks_composer` 决定，即
   "当前正在答的那条是不是要用选的"。`Choose` 锁、`Type` 不锁。
3. `composer.content.ask` 的 `TabView`/`ListView` 切换逻辑：自动按数量选，还是用户手动切？本版本默认"单项不显示 tab 条 / 多项默认 TabView，ListView 靠手动切换键"。
4. `composer.content` 高度分配：提问框出现时是动态挤压转录区（本版本默认），还是预留固定空间？
5. `composer.bottom_rule` 要不要也带信息（目前按纯分隔处理）？
