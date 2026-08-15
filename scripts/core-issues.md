# 给 AttaCore 的问题清单

> 来源：AttaCode（TUI 前端）在 2026-08-13/14 用真模型（DeepSeek endpoint）跑通全部
> UI 区块时撞出来的。**都不在 AttaCode 侧修** —— 按约定 Core 的改动在 AttaCore 项目
> 里单独做。这份文件是交接说明，Core 那边改完就可以删。
>
> 引用的行号基于 submodule 当前指向的 commit，改动前请先核对。

---

## 1【重要】失败/取消的 turn 不落盘，会话事后接不上

**位置：** `crates/runtime/src/turn.rs:1468` 附近，`run_user_turn` 的正常返回路径。

**现象：** `session.persist()` 只在 turn 正常走完时调用。turn 报错（模型 5xx、工具异常、
上下文超限）或被 `request_cancel` 取消时，控制流从早退分支离开，绕过 persist。

**后果：** 一个**每个 turn 都失败**的会话在磁盘上不留任何东西。用户看到满屏对话，
退出后 `--continue` / `--resume` 找不到它，就像从没发生过。半路失败的会话则丢掉最后
那一轮的用户输入——恰恰是用户最想接着重试的那一轮。

**为什么值得改：** 会话恢复的价值集中在"出错了想接着弄"这个场景，而这正是当前唯一
不落盘的场景。

**建议：** 把 persist 挪到早退路径也会经过的地方（`defer` 风格的 guard，或在每个
`return`/`?` 之前显式 persist）。**取消的 turn 也要留下用户那条消息**——用户按 Ctrl+C
是"这轮不要了"，不是"这句话我没说过"。

**验证：** 起一个会话，让第一个 turn 必失败（比如把模型端点指到不存在的地址），退出，
`--continue`。期望能恢复出那条用户输入。

---

## 2【重要】子代理委派没有深度上限，会把进程递归到 stack overflow

**现象（AttaCode 真跑时复现）：** 我们这边漏注册了工具注册表，于是模型调 `Read` 拿回
`Tool not found: Read`。模型的应对是**派一个子代理去读**；子代理继承同一个注册表，
于是它也读不了，于是它再派一个……一路下去到

```
fatal runtime error: stack overflow
```

**AttaCode 侧的诱因已经修好了**（`crates/bridge/src/bootstrap.rs` 现在会
`register_builtin_tools` + `register_web_search`，并有回归测试钉住）。但**触发器不该
只有这一个**：任何让子代理反复失败又反复重试的情形——工具挂了、权限一直拒、场景配错
——都能走到同一个结局。而且这不是报错退出，是**整个进程被干掉**，宿主（TUI/daemon）
没有任何机会挽救或提示。

**建议：**
- 给委派链加**深度上限**（比如 5 层），到顶就让 `Agent`/`Task` 工具返回一个正常的
  错误结果，让模型看见"不能再派了"，而不是继续递归。
- 顺带考虑同一 turn 内的**子代理总数上限**，防同层横向爆炸。

**验证：** 造一个空工具注册表的 agent，给它一个必须读文件的任务。期望：拿到"委派层数
超限"的错误结果，进程活着。

---

## 3【建议】`AgentEvent::AgentSpawned` / `AgentCompleted` 从来没有人发

`base::interface::event::AgentEvent` 里声明了这两个变体，但全仓库搜不到任何发射点
（同名的 telemetry payload 是另一回事，别混）。嵌入方（我们）照着枚举去接子代理事件，
接完发现是死的——AttaCode 现在改成从子代理的 `agent_label` 反推，能用，但那是绕路。

**建议：** 要么在委派开始/结束处真的发出来，要么从枚举里删掉。留着最费时间：它看起来
像个能用的 API。

---

## 4【轻微】`~/.atta/sessions/` 下混了两套目录结构

- history store：`sessions/<sanitized-cwd>/<session-id>.jsonl`
- session memory：`sessions/<session-id>/session_memory.md`

同一个根目录下两种布局并存。目前不冲突（sanitized-cwd 和 session-id 撞不上），但按目录
名扫会话的代码会同时看到两类条目，得靠猜来区分。建议分成两个根，或统一到一层。

---

## 附：已经在 submodule 分支上改掉、等提 PR 的

`EngineCommand::UpdateModel` 在 `turn.rs` 里没有处理分支——发过去石沉大海，`/model`
切换在引擎侧不生效。本机分支 `live-commands-and-per-turn-cancel`（commit `481420e`）
已经补了处理和测试，**尚未推送**。请在 AttaCore 项目里以正式流程重做或采纳，然后
AttaCode 这边 bump submodule 指针。
