# 给 AttaCore 的问题清单

> 来源：AttaCode（TUI 前端）用真模型跑通全部 UI 区块时撞出来的。**都不在 AttaCode 侧
> 修** —— 按约定 Core 的改动在 AttaCore 项目里单独做。Core 改完这份文件就可以删。

---

## 未解决

### A【轻微】detached HEAD 下 `frozen::tests::detects_git_repo_in_a_real_repo` 必挂

**位置：** `crates/core/src/frozen/mod.rs:458`

```rust
assert!(ctx.git_branch.is_some());
```

`git_branch` 是 `git symbolic-ref --short HEAD` 的结果，**detached HEAD 时这条命令报错**，
字段就是 `None`。而 `git submodule update` 留下的正是 detached HEAD —— 也就是说，
**任何把 AttaCore 当子模块用的仓库，CI 里跑 `cargo test --workspace` 都会红这一条**。
AttaCode 的 CI 现在只能加一步 `git -C core checkout -B ... HEAD` 绕过去。

被测代码没问题：detached 时确实没有分支名，返回 `None` 是对的。是测试假设了"HEAD 接在
分支上"。

**建议：** 断言放松成只查 `ctx.is_git`；或者 detached 时回落到 `rev-parse --short HEAD`
（真要展示的话，`<env>` 里写个 sha 比空着强）。

**验证：** `git checkout --detach && cargo test -p base --lib frozen`。

### B【轻微】`~/.atta/sessions/` 下混了两套目录结构

- history store：`sessions/<sanitized-cwd>/<session-id>.jsonl`
- session memory：`sessions/<session-id>/session_memory.md`

同一个根目录下两种布局并存。目前不冲突（sanitized-cwd 和 session-id 撞不上），但按目录
名扫会话的代码会同时看到两类条目，得靠猜来区分。建议分成两个根，或统一到一层。

> v0.1.1 把 session memory 的**路径解析**修正到了 `history::path::sessions_root()`
> （之前少一层、还用了另一个环境变量，写完就找不回来）。这里说的是剩下的那半：
> 修正之后两种布局仍然共用一个根。

---

## 已在 v0.1.1（`15d6574`）解决 —— 保留记录，勿重复提

| # | 问题 | Core 的处理 |
|---|---|---|
| 1 | 失败/取消的 turn 不落盘，`--continue` 接不上 | `run_user_turn` 拆成外层包装，所有早退路径都 persist；取消的 turn 也留下用户那条消息 |
| 2 | 子代理委派无深度上限 → `stack overflow` 进程被干掉 | `EngineConfig::max_agent_depth` 真正接上（此前无人读取），在 `Inner::spawn_guard` 统一拦；到顶给模型一个普通工具错误 |
| 3 | `AgentEvent::AgentSpawned`/`AgentCompleted` 声明了没人发 | 现在在每次委派前后发到父通道，带同一个 `agent_label` |
| 4 | `EngineCommand::UpdateModel` 被 `_ =>` 吃掉，`/model` 静默无效 | 已处理（`RefreshMcp` 同病同治），并且 match 改成穷尽的，以后新增变体会编译失败而不是静默 |

AttaCode 侧的对应适配已完成，见 commit `AttaCode: bump AttaCore to 15d6574`。
