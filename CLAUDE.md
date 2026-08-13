# AttaCode

AttaCode 是 AttaCore 引擎的终端 UI（TUI）前端。它本身不包含 agent 引擎逻辑 —— 所有引擎能力来自 [AttaCore](https://github.com/openatta/AttaCore) submodule。

## 架构

```
AttaCode/
├── core/                     AttaCore git submodule（引擎运行时 + 工具 + 权限 + …）
├── crates/
│   ├── tui/                  ratatui TUI 前端 — 消费 AttaCore 的 AgentEvent 流
│   │   └── src/slash/        内置 slash 命令系统（/quit /clear /model /compact …）
│   └── keybindings/          键盘快捷键 DSL — 解析、校验、合并默认 + 用户覆盖
├── scripts/                  开发辅助脚本 / patch 规格
│   └── lsp-process-pool.patch.md   AttaCore LspTool 进程池改进方案（待贡献）
├── Cargo.toml                workspace: tui + keybindings
└── 3rds/                     gitignored，第三方参考代码
```

**核心原则：AttaCode = AttaCore + TUI。** TUI 不包含任何引擎实现，只做渲染和用户交互。

## 数据流

```
用户按键
  → keybindings::Resolver（快捷键匹配）
  → slash 命令分派（/xxx 前缀）或 直接发送给 Agent
  → runtime::Agent::process_turn()
  → base::interface::event::AgentEvent 流
  → TUI 渲染（转录行、工具调用、权限对话框）
```

## 依赖

TUI 直接依赖 AttaCore 的以下 crate：

| AttaCore crate | 用途 |
|---|---|
| `base` (`core/crates/core`) | 基础类型：AgentEvent, Id, Message, PermissionMode, SkillEntry … |
| `runtime` (`core/crates/runtime`) | Agent 生命周期 + turn loop |
| `scene` | 场景定义（CodingScene 等） |
| `tools` | 工具注册 + LSP 工具 |
| `permissions` | 权限引擎 |
| `telemetry` | 用量统计（UsageAccumulator, UsageDelta） |
| `history` | 会话持久化路径 |
| `skills` / `mcp` / `hooks` | 技能、MCP、生命周期钩子 |

## 本地开发

```sh
# 初始化 submodule
git submodule update --init --recursive

# 构建
cargo build --workspace

# 检查
cargo check --workspace    # 0 errors, 4 warnings (dead code)

# 测试
cargo test --workspace
```

## 已删除（不在 scope）

- **CLI crate**：AttaCode 不再提供 headless CLI。TUI 是唯一入口。
- **agent / slash / lsp shim crates**：Triaged —— TUI 直接使用 AttaCore 类型，零适配层。
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
