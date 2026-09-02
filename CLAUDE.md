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
  → app 分流：本地动作（草稿/滚动/块选择/ /model /quit /doctor /resume）或 EngineHandle::dispatch
  → bridge → runtime::InputMessage → runtime::Agent::process_turn()
  → base::event::AgentEvent 流
  → bridge 归约器（流式文本、工具块配对、折叠态、用量、权限队列）
  → watch::Sender<tui::FrameState> → app 渲染循环 → tui::layout::render
```

**还有第二条通往同一个归约器的流：模型的提问。** `AgentEvent` 里没有"模型想问点什么"
这一项，而 `AskUserQuestion` 从 AttaCore 0.2.0 起真的会去问人。见
`crates/bridge/src/ask.rs`：

```
模型调用 AskUserQuestion
  → bridge::ask::TuiAskUserQuestion（`ToolRegistry::replace` 换掉 Core 那版）
  → bridge::ask::Questions（mpsc）→ 归约器 → 和权限请求同一个审批对话框
  → 用户选一项 / 打一行 → BridgeCommand::Respond|AnswerQuestion
  → oneshot 回到还在 await 的那次工具调用
```

slash 命令有两个来源，提交时在 `app` 分流：

- **本地**（`crates/app`）：`/model`、`/doctor`、`/resume`、`/quit`、`/exit` —— 不联系 Core。
- **Core**：其余一律原样转发，由 `runtime::commands::CommandRegistry` 解析（内置 `/help` `/skills` `/clear` `/compact` `/cost` + 实时技能 + 插件/MCP prompts）。补全弹窗里的候选就是这份实时表 + 上面那几条本地命令，所以选中的命令提交后一定解析得出来。

`/resume` 是唯一一条会把整个引擎重建一遍的命令：模型上下文、工具表、权限门、转录
全绑在一个 `runtime::Agent` 上，而 `Agent::run` 已经 `&mut self` 借走了它。所以换会话
= 关掉这个、起一个新的（`app::sessions` 那个循环），终端本身不重进。

## 依赖

AttaCode 直接依赖 AttaCore 的以下 crate（全部走 `core/crates/*` 的 path 依赖）：

| AttaCore crate | 用途 |
|---|---|
| `base` (`core/crates/core`) | 基础类型 + 注入契约：AgentEvent, Settings, SessionId, PermissionMode, ConfigPaths, HealthCheck, CredentialSource … |
| `runtime` (`core/crates/runtime`) | Agent 生命周期 + turn loop + CommandRegistry |
| `model` | 模型协议 + `factory::builtin_registry`（anthropic / openai_compatible） |
| `scene` | 场景定义（CodingScene 等） |
| `permissions` | 权限规则引擎（RuleSetPermission） |
| `compaction` | 上下文压缩 |
| `history` | 会话转录落盘（JsonlHistoryStore）+ `/resume` 的会话查询 |
| `skills` | 技能目录（仅 bridge 单测用；生产路径由 Core 的 registry 持有） |

> **注意：`core/` 在 workspace 目录内，被我们 path 依赖到的 core crate 会被 cargo
> 自动吸收成本 workspace 的成员**（截至 AttaCore 0.2.3 是 15 个，用
> `cargo metadata --no-deps` 数当前值，别背这个数字）。
> 也就是说 core crate 里的 `xxx = { workspace = true }` 是拿**根 `Cargo.toml`** 的
> `[workspace.dependencies]` 解析的。AttaCore 新增第三方依赖时，这里必须同步加，否则编译不过
> （0.1.8 就是这样要求补了 `semver`；0.2.3 没有新增要求）。
> 附带好处：`cargo test --workspace` 会连这些 crate 的测试一起跑。
>
> 但**只有被吸收的这些**——AttaCore 自己那些 `daemon` / `rpc-client` / `test-runner` /
> `mcp-toy-server` / `plugin-compiler` / `plugin-host` / `wasm-host` 不在其中。要跑
> AttaCore 全套得 `cd core` 再 `cargo test --workspace`，那是**另一个 workspace**，
> 数字对不上不是测试丢了。（在 `core/` 里跑完忘了 `cd` 回来，接着跑的
> `cargo test --workspace` 就悄悄跑了 AttaCore 的套件——测试数会变，但一个我们的
> crate 都没测到。）

## 本地开发

```sh
# 初始化 submodule（.gitmodules 里 branch = main）
git submodule update --init --recursive
git -C core checkout main          # 挂到分支上，别停在 detached HEAD

# 跟到 AttaCore 最新（我们不钉版本，一直跟 main）
git submodule update --remote --merge core

# 构建 / 检查 / 测试（都会覆盖 core）
cargo build --workspace
cargo check --workspace
cargo test  --workspace

# 跑起来
export ANTHROPIC_AUTH_TOKEN=...        # 或 ANTHROPIC_API_KEY
cargo run -p app                        # attacode
cargo run -p app -- --model claude-sonnet-5
cargo run -p app -- --continue          # 接着本项目最近一次会话
cargo run -p app -- --resume <id>       # 接着指定会话（transcript 见 ~/.atta/projects/<项目>/）

# 无终端冒烟（真 API，验装配→事件流→归约那半条链）
cargo run -p bridge --example smoke
```

模型优先级（高→低）：`--model` → `ANTHROPIC_MODEL` → 三层 `settings.json` 的
`model.model_name` → **选中 provider 的 `default_model`** → `bridge::DEFAULT_MODEL`。
运行中用 `/model <name>` 切换，下一个 turn 起效。

**用哪个 provider**（`bootstrap::resolve_provider`）：`settings.providers` 里有东西就用
`default_provider` 点名的那个；没点名而恰好只有一个就是它；没点名又有好几个是**错误**
（静默挑一个的后果是模型和账单都跑到了用户没打算用的地方）。什么都没配时合成一个
anthropic provider，`base_url` 取 `ANTHROPIC_BASE_URL`。凭据先看 provider 自己的
`api_key`，再看 `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`。

`/doctor` 把上面这些**实际生效成了什么**打出来（provider、模型、转录落没落盘、沙箱后端、
权限模式和规则数）。三层 settings 合并之后的结果，人对着源文件是推不出来的。

改了渲染/事件循环/装配之后，按 `docs/manual-smoke-checklist.md` 在真终端里过一遍
——那些东西单元测试验不了。

## 两条 AttaCore 0.2.x 的边界，改之前先看

**沙箱是编译期 feature，我们显式打开了它。** `crates/bridge/Cargo.toml` 里的
`base = { features = ["sandbox"] }`。AttaCore 0.2.1 起这个 feature 默认关（上游的理由：
机主坐在机器前时沙箱只是权限门后面的纵深防御）。关掉它有两个后果：模型跑的 shell
命令不再被 `sandbox-exec` / `bubblewrap` 包一层，而且设了
`sandbox.require_enforcement = true` 的人会发现**每一条命令都被拒**——`wrap` 报
`Unavailable`。别顺手把这个 feature 拿掉。

**写路径的控制清单现在真的在执行。** AttaCore 0.2.0 之前那份检查是死代码；接上之后
`.env` / `.gitignore` / lockfile / `.atta` / `.claude` 一律拒写，而**唯一**的豁免口子是
`sandbox.allow_write`。这条设置只能经由 `RuleSetPermission` 的 `sandbox` 字段抵达工具，
也就是必须走 `RuleSetPermission::from_settings`（`bridge::permission::build` 就是这么做的）。
换回 `RuleSetPermission::new` 的话，用户对着的就是一堵没有门的墙——而且不会有任何报错。

## 已删除（不在 scope）

- **CLI crate**：AttaCode 不再提供 headless CLI。TUI 是唯一入口。
- **agent / slash / lsp shim crates**：Triaged —— 直接使用 AttaCore 类型，零适配层。
- **v1 归档**：已删除。完整历史在 git log 中可追溯。

## AttaCore 补丁

AttaCore 本身不在本仓库修改。需要改进 AttaCore 时：

1. `cd core` 进入 submodule
2. 切分支 → 修改 → `cargo test --workspace`
3. 提 PR 到 `openatta/AttaCore`
4. PR 合并后 `git submodule update --remote --merge core` 跟到最新

`scripts/` 目录存放尚未提交的 patch 规格说明。

**submodule 不钉版本，一直跟 `origin/main`。** 记录在 AttaCode 提交里的仍然是具体
SHA（git 的 gitlink 只能是 SHA），所以任何一次 checkout 依旧可复现；"跟最新"指的是
我们主动 bump 的节奏，不是构建时去解析分支。

**别让 `core/` 停在 detached HEAD。** 那是 `git submodule update` 的默认状态，但
Core 的 `base::frozen::tests::detects_git_repo_in_a_real_repo` 拿 cwd 所在仓库的
分支名做断言，detached 时 `git_branch` 是 `None`，`cargo test --workspace` 就会挂
在这一条上——不是我们改坏了。`checkout main` 之后即可。

## 代码风格

- Rust：`cargo fmt` + `cargo clippy -- -D warnings`
- 提交前缀：`AttaCode:`
- Submodule 更新单独提交，消息格式：`AttaCode: bump AttaCore to <sha>`
