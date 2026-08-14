# AttaCode

AttaCode 是 AttaCore 引擎的终端 UI（TUI）前端。它本身不包含 agent 引擎逻辑 —— 所有引擎能力来自 [AttaCore](https://github.com/openatta/AttaCore) submodule。

## 架构

```
AttaCode/
├── core/                     AttaCore git submodule（引擎运行时 + 工具 + 权限 + …）
├── crates/
│   ├── app/                  bin（产出 attacode）— 终端事件循环、按键分派、本地 slash 分流
│   ├── bridge/               TUI↔Core 粘合层 — 装配 Agent、归约 AgentEvent、派生 FrameState
│   ├── tui/                  ratatui 渲染层 — 纯数据快照 FrameState → 画面，零 AttaCore 依赖
│   └── keybindings/          键盘快捷键 DSL — 解析、校验、合并默认 + 用户覆盖
├── docs/
│   ├── reqs/                 需求规格
│   ├── design/               架构设计文档
│   └── TUI_DESIGN.md         Z/R/S 布局坐标系
├── scripts/                  开发辅助脚本 / patch 规格
│   └── lsp-process-pool.patch.md   AttaCore LspTool 进程池改进方案（待贡献）
├── Cargo.toml                workspace: app + bridge + tui + keybindings
└── 3rds/                     gitignored，第三方参考代码
```

**核心原则：AttaCode = AttaCore + TUI。** 引擎实现一律在 AttaCore，这里只做装配、渲染和用户交互。

三层各自守住一条边界，任一层都能单独替换/测试：

| crate | 不许依赖 | 理由 |
|---|---|---|
| `tui` | 任何 AttaCore 类型 | `frame_state.rs` 是纯数据快照，可序列化、可脱离引擎测试 |
| `bridge` | ratatui / crossterm | 朝向 Core 的一侧不该知道终端长什么样 |
| `app` | AttaCore 类型 | 只通过 `bridge::EngineHandle` 说话 |

## 数据流

```
用户按键
  → keybindings::Resolver（快捷键匹配 → action 名）
  → app 分流：本地动作（草稿/滚动/块选择/ /model /quit）或 EngineHandle::dispatch
  → bridge → runtime::InputMessage → runtime::Agent::process_turn()
  → base::interface::event::AgentEvent 流
  → bridge 归约器（流式文本、工具块配对、折叠态、用量、权限队列）
  → watch::Sender<tui::FrameState> → app 渲染循环 → tui::layout::render
```

slash 命令有两个来源，提交时在 `app` 分流：

- **本地**（`crates/app`）：`/model`、`/quit`、`/exit` —— 不联系 Core。
- **Core**：其余一律原样转发，由 `runtime::commands::CommandRegistry` 解析（内置 `/help` `/skills` `/clear` `/compact` `/cost` + 实时技能 + 插件/MCP prompts）。补全弹窗里的候选就是这份实时表 + 上面三条本地命令，所以选中的命令提交后一定解析得出来。

## 依赖

AttaCode 直接依赖 AttaCore 的以下 crate（全部走 `core/crates/*` 的 path 依赖）：

| AttaCore crate | 用途 |
|---|---|
| `base` (`core/crates/core`) | 基础类型：AgentEvent, Settings, SessionId, PermissionMode, ConfigPaths … |
| `runtime` (`core/crates/runtime`) | Agent 生命周期 + turn loop + CommandRegistry |
| `model` | Anthropic 客户端 / 适配器 |
| `scene` | 场景定义（CodingScene 等） |
| `permissions` | 权限规则引擎（RuleSetPermission） |
| `compaction` | 上下文压缩 |
| `history` | 会话转录落盘（JsonlHistoryStore） |
| `skills` | 技能目录（仅 bridge 单测用；生产路径由 Core 的 registry 持有） |

> **注意：`core/` 在 workspace 目录内，那 16 个 crate 会被 cargo 自动吸收成本 workspace 的成员。**
> 也就是说 core crate 里的 `xxx = { workspace = true }` 是拿**根 `Cargo.toml`** 的
> `[workspace.dependencies]` 解析的。AttaCore 新增第三方依赖时，这里必须同步加，否则编译不过。
> 附带好处：`cargo test --workspace` 会连 core 的测试一起跑。

## 本地开发

```sh
# 初始化 submodule
git submodule update --init --recursive

# 构建 / 检查 / 测试（都会覆盖 core）
cargo build --workspace
cargo check --workspace
cargo test  --workspace

# 跑起来
export ANTHROPIC_AUTH_TOKEN=...        # 或 ANTHROPIC_API_KEY
cargo run -p app                        # attacode
cargo run -p app -- --model claude-sonnet-5
cargo run -p app -- --continue          # 接着本项目最近一次会话
cargo run -p app -- --resume <id>       # 接着指定会话（id 见 ~/.atta/sessions/<项目>/）

# 无终端冒烟（真 API，验装配→事件流→归约那半条链）
cargo run -p bridge --example smoke
```

模型优先级（高→低）：`--model` → `ANTHROPIC_MODEL` → 项目 `.atta/settings.json` → 场景 →
全局 → `bridge::DEFAULT_MODEL`。运行中用 `/model <name>` 切换，下一个 turn 起效。

改了渲染/事件循环/装配之后，按 `docs/manual-smoke-checklist.md` 在真终端里过一遍
——那些东西单元测试验不了。

## 已删除（不在 scope）

- **CLI crate**：AttaCode 不再提供 headless CLI。TUI 是唯一入口。
- **agent / slash / lsp shim crates**：Triaged —— 直接使用 AttaCore 类型，零适配层。
- **v1 归档**：已删除。完整历史在 git log 中可追溯。

## AttaCore 补丁

AttaCore 本身不在本仓库修改。需要改进 AttaCore 时：

1. `cd core` 进入 submodule
2. 切分支 → 修改 → `cargo test --workspace`
3. 提 PR 到 `openatta/AttaCore`
4. PR 合并后 `git pull` 更新 submodule

`scripts/` 目录存放尚未提交的 patch 规格说明。

## 代码风格

- Rust：`cargo fmt` + `cargo clippy -- -D warnings`
- 提交前缀：`AttaCode:`
- Submodule 更新单独提交，消息格式：`AttaCode: bump AttaCore to <sha>`
