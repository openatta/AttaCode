# AttaCode

AttaCode 是 [AttaCore](https://github.com/openatta/AttaCore) AI agent 引擎的终端界面（TUI）。它**不是** agent 引擎本身 —— 所有推理、工具调用、权限、会话逻辑均由 AttaCore 提供。AttaCode 是 AttaCore `AgentEvent` 流之上的一层薄粘合层，加一个 ratatui 前端。

## 架构

```
┌────────────────────────────────────────────────────────────────────┐
│                        AttaCode（本仓库）                            │
│                                                                       │
│  crates/app/          bin `attacode` —— 终端 I/O、按键分派、          │
│                        渲染前把 UI 本地 composer 状态合并到            │
│                        FrameState 快照上                             │
│         │                                                            │
│         ├─ crates/tui/         纯 ratatui 渲染。                     │
│         │                      输入 FrameState，输出终端帧。          │
│         │                      零 AttaCore 依赖。                    │
│         │                                                            │
│         ├─ crates/bridge/      粘合层本体：装配 runtime::Agent，      │
│         │                      把 AgentEvent 归约成                  │
│         │                      tui::FrameState，对外暴露              │
│         │                      EngineHandle。唯一同时认识             │
│         │                      AttaCore 类型和 tui 类型的 crate。     │
│         │                                                            │
│         └─ crates/keybindings/ 快捷键/组合键解析器 + 匹配器           │
│                                                                       │
└───────────────────────────────┬───────────────────────────────────┘
                                 │ cargo path 依赖
                                 ▼
┌────────────────────────────────────────────────────────────────────┐
│                   AttaCore（git submodule，core/）                   │
│  core/crates/{base, runtime, model, scene, tools, permissions,       │
│               mcp, hooks, skills, session, history, compaction, ...} │
│  core/daemon/    JSON-RPC 参考消费实现（AttaCode 不使用它）           │
└────────────────────────────────────────────────────────────────────┘
```

**核心原则**：AttaCode = AttaCore + TUI。`crates/tui` 从不接触 AttaCore 类型；`crates/bridge` 从不接触 ratatui/crossterm。`crates/app` 是唯一同时依赖两者的地方，且只通过 `EngineHandle` + `tui::FrameState` —— 从不直接依赖 `runtime::Agent`/`AgentEvent`。

完整设计理由与已知的 Core 侧缺口（交互式权限确认目前还没接进 `runtime::turn`，依赖它之前请先看这份文档），见 `docs/design/2026-08-13-tui-core-glue-layer.md`。

## 数据流

```
crossterm KeyEvent
  │
  ▼
keybindings::Resolver::on_key  ──►  action 名（或 Unmatched → composer 编辑）
  │
  ▼
crates/app: dispatch_action()
  ├─ 本地动作（编辑草稿、滚动）  ──►  只改 LocalUi，不涉及 Core
  └─ 面向 Core 的动作            ──►  EngineHandle::dispatch(BridgeCommand)
                                                    │
                                                    ▼
                                    bridge: InputSender.send(InputMessage)
                                    → runtime::Agent 自身的串行输入循环
                                    （"当前轮次运行时提交新输入会排队"这个
                                    需求就是靠这个天然满足的，bridge 不需要
                                    自己再实现一套队列）
                                                    │
                                                    ▼
                                        AttaCore AgentEvent 流
                                    (TextDelta | ToolUse | ToolResult |
                                     PermissionPrompt | TurnComplete | ...)
                                                    │
                                                    ▼
                                bridge::reducer::Reducer::apply_event()
                                  更新内部领域模型（逐 turn 文本缓冲、
                                  以 id 关联的工具调用块、会话累计用量），
                                  然后派生一份新的 tui::FrameState，
                                  通过 tokio::sync::watch 广播出去
                                                    │
                                                    ▼
                        crates/app 渲染循环：merge(bridge快照, LocalUi)
                          → tui::layout::render(frame, area, &state, spinner)
```

## FrameState（`crates/tui/src/frame_state.rs`）

`FrameState` 是渲染器消费的唯一可序列化、不含 AttaCore 类型的快照。完整的区域树和中英文对照名见 `docs/TUI_DESIGN.md`。它混合了两类状态：

- **Core 权威状态**，由 `bridge` 拥有并派生：转录条目、待确认的权限请求、子代理栏、累计会话用量。
- **UI 本地状态**，由 `crates/app` 拥有，渲染前才合并进去：composer 草稿/光标、滚动位置。

## AgentEvent → FrameState 映射（`bridge::reducer`）

| AttaCore `AgentEvent` | 归约器行为 |
|---|---|
| `TextDelta` | 追加到当前 turn 的流式 assistant-text 块 |
| `ToolUse` | 新建一个以 `id` 为键的工具块，附带 `ToolHeading` 条目 |
| `ToolResult` | 按 `id` 匹配到对应 `ToolUse`；超过 8 行自动折叠成摘要行（可通过 `BridgeCommand::ToggleExpand` 展开） |
| `PermissionPrompt` | 推入 `ApprovalState.pending`；composer 锁定直到解决。**目前实际上不会触发** —— `runtime::turn::execute_tool_inner` 在执行工具前并不会调用权限门禁，所以 Core 现在根本不会发出这个事件 |
| `TurnComplete` | 累加会话 token 用量（footer，常驻显示） |
| `AgentSpawned` / `AgentCompleted` | 更新子代理栏 |
| `CompactAction` | 清除临时的"运行中"状态行 |
| `Error` | 作为 `Error` 转录条目推入，不会中断循环 |
| `SystemInit` / `SessionPersisted` | 目前是空操作 |

## 快捷键

默认值来自 `keybindings::default_bindings()`。`crates/app` 目前接通了：

| Action | 按键 | 行为 |
|---|---|---|
| `editor.submit` | `Enter` | 提交草稿（或本地执行 `/quit`/`/exit`） |
| `editor.clear` | `Ctrl-U` | 清空草稿 |
| `repl.cancel` | `Ctrl-C` | `BridgeCommand::CancelTurn` |
| `repl.exit` | `Ctrl-D` | 草稿为空时退出 |
| `ask.confirm` / `ask.yes-shortcut` | `Enter` / `y`（在权限确认中） | 对当前激活的确认项回应 `PermitOnce` |
| `ask.no-shortcut` / `repl.dismiss` | `n` / `Esc` | 对当前激活的确认项回应 `Deny` |

`keybindings` 还内置了历史导航、删词/删至行尾、多行插入、滚动等组合键（`editor.history.*`、`editor.kill-to-eol`、`repl.scroll-*`、`ask.prev`/`ask.next`）——`Resolver` 能解析出这些 action，但 `crates/app` 还没有把它们接到具体行为上。Composer 编辑本身目前也很简化：只支持在草稿末尾追加/退格，不支持行内光标移动。

## Slash 命令

目前没有 slash 命令子系统。`crates/app::submit()` 只在本地识别 `/quit` 和 `/exit`（不联系 Core 直接退出）；其余内容——包括其他 `/` 前缀文本——都原样转发给 Core 当作普通文本处理。

## 项目结构

```
AttaCode/
├── core/                     AttaCore git submodule（只读依赖）
├── crates/
│   ├── tui/                  纯 ratatui 渲染
│   │   ├── src/frame_state.rs    FrameState + 各区域子状态
│   │   ├── src/layout.rs         顶层区域组合
│   │   ├── src/regions/          每个区域一个渲染模块
│   │   └── examples/layout_demo.rs   脚本化可视化 demo，不涉及 Core
│   ├── bridge/                粘合层本体
│   │   ├── src/bootstrap.rs      装配 Settings/Model/Scene → runtime::agent::Builder
│   │   ├── src/handle.rs         EngineHandle / BridgeCommand
│   │   ├── src/reducer.rs        AgentEvent → FrameState
│   │   └── src/permission.rs     GatePermission（已实现，尚未接入——见上文）
│   ├── app/                   bin `attacode` —— 终端事件循环
│   └── keybindings/           快捷键/组合键解析器 + 匹配器
├── docs/
│   ├── TUI_DESIGN.md              区域设计 + 中英文对照名（唯一权威）
│   ├── tui-region-map.html        同一张表的图，浏览器打开，悬停联动
│   ├── reqs/, design/             各特性的需求/架构文档
│   └── README_CN.md               本文件
├── scripts/                   开发辅助脚本 / AttaCore patch 规格
├── Cargo.toml                 workspace: tui, keybindings, bridge, app
└── README.md                  英文版
```

## 快速开始

```sh
# 1. 带 submodule 克隆
git clone --recurse-submodules https://github.com/openatta/AttaCode.git
cd AttaCode

# 2. 配置凭证（参考 .env.example）
cp .env.example .env
# 编辑 .env —— 至少需要 ANTHROPIC_AUTH_TOKEN 或 ANTHROPIC_API_KEY

# 3. 构建
cargo build --workspace

# 4. 测试
cargo test --workspace

# 5. 运行
set -a; . .env; set +a
cargo run -p app
```

**前置条件**：Rust（见 `rust-toolchain.toml`）、C 编译器（AttaCore 原生依赖需要）、一个 Anthropic 兼容的 API key。

## 开发

```sh
# 格式化 + 检查
cargo fmt --all
cargo clippy --workspace -- -D warnings
```

注意：`core/` 是独立的 Cargo workspace（submodule）；`crates/bridge` 通过 path 依赖深入其中。因此 `cargo clippy --workspace -D warnings` 和 `cargo fmt --all --check` 也会暴露 `core/` 自身既有的 lint/格式状态——那部分代码不在本仓库改动范围内（见下），排查失败时先确认是不是出在 `core/` 下，再判断是不是本仓库引入的回归。

## 与 AttaCore 的关系

AttaCode 把 `core/` 当作**只读**依赖。对引擎的修改必须走 [AttaCore](https://github.com/openatta/AttaCore) 仓库：

1. `cd core` → 开分支 → 修改 → `cargo test --workspace`
2. 提 PR 到 `openatta/AttaCore`
3. 合并后：submodule 内 `git pull origin main`
4. 在 AttaCode 提交 submodule 指针更新：`AttaCode: bump AttaCore to <sha>`

尚未上游化的 patch 提案放在 `scripts/` 目录。

## License

Apache-2.0（见 `Cargo.toml` 的 `[workspace.package]`）。
