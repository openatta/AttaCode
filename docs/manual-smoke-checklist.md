# 真机验收清单

单元测试覆盖不到的东西：raw mode 下的渲染、和真 API 的时序、按键在真实终端里的编码。
每次动了渲染/事件循环/装配之后跑一遍这份清单。

**准备**

```sh
set -a; . .env; set +a          # ANTHROPIC_AUTH_TOKEN 或 ANTHROPIC_API_KEY
cargo build --workspace          # 先确认能编译，别在 alt screen 里看编译错误
```

## 0. 无终端冒烟（先跑这个）

```sh
cargo run -p bridge --example smoke
RUST_LOG=debug cargo run -p bridge --example smoke   # 出问题时看 Core 的 warn/debug
```

- [ ] 打印的 `model = …` 是你期望的那个（三层 settings.json 合并后的结果）
- [ ] 转录里出现 `[AssistantText] …` 且内容合理
- [ ] 末尾 `turn complete: N in / M out tokens`，两个数都 > 0
- [ ] 进程干净退出，没有 `failed to persist session` 之类的 warn

这一步过了，说明装配 → 真 API → 事件流 → 归约器这条链是通的；下面才是终端那一半。

## 1. 起手

```sh
cargo run -p app
```

- [ ] 进入 alt screen，底部有输入框、状态栏显示模型名和 cwd
- [ ] 输入框光标是那个块（`█`），闪不闪都行，但必须在 `> ` 后面
- [ ] `Ctrl+D`（空草稿）能退出，终端恢复正常、没有残留的 raw mode（试着敲几个字看回显）

## 2. 一轮普通问答

发一句 `用一句话解释 Rust 的所有权`。

- [ ] 用户输入立刻回显（不等 API）
- [ ] assistant 文本**流式**长出来，不是一次性整段出现
- [ ] 状态行有 spinner + 走字的秒数 + token 计数
- [ ] 转录顶部出现 sticky header，钉着你刚才那句问题
- [ ] 结束后 spinner 消失，footer 的累计用量增加

## 3. 工具调用 + 折叠 + diff

发一句 `读一下 Cargo.toml，然后把 README.md 里第一行的标题改成 AttaCode（改完再改回去）`。

- [ ] 每次工具调用是独立一块：`⏺ Read(...)` 之后跟结果
- [ ] 大输出默认折叠，末行是 `… N more lines (toggle to expand)`
- [ ] `F5` 展开最新那块，再按一次收起
- [ ] Edit 的结果里 diff 上色：`-` 行红底、`+` 行绿底、`@@`/上下文灰
- [ ] 模型写 TODO 清单时（长任务会），状态区出现勾选列表，进行中那条是 `●`

## 4. 转录块选择（场景 7）

在上一步之后（转录里至少两个工具块）：

- [ ] `Alt+↑` 出现左侧竖条 `▌`，标在**最新**那个块上
- [ ] 再按 `Alt+↑` 竖条移到更早的块，且视口自动滚过去
- [ ] 此时 `F5` 展开的是**被标记的那个块**，不是最新的
- [ ] `Alt+↓` 一路往回，越过最新块之后竖条消失（回到"跟最新的"）
- [ ] `Esc` 也能清掉选择
- [ ] `PageUp`/`PageDown` 翻页，底部出现 `── N lines above ──`，翻到底自动回到跟随

## 5. 权限对话框

发一句 `用 bash 跑 rm -i /tmp/attacode-probe`（或任何会触发确认的写操作）。

- [ ] 弹出对话框，四个选项，输入框变灰（锁住）
- [ ] `↑`/`↓` 换选项，`Enter` 确认，`Esc` 拒绝
- [ ] `y`/`n` 快捷键有效；**没有**对话框时打 "yes" 不会丢字母
- [ ] 选"本会话一直允许"之后，同类调用不再弹
- [ ] 对话框开着时 `Ctrl+C` 能中断整个 turn

## 6. 取消

发一个长任务（`把整个 crates/ 目录读一遍并总结`），中途 `Ctrl+C`。

- [ ] spinner 变成 `Cancelling…`
- [ ] 转录里出现 `Turn cancelled.`，spinner 停
- [ ] **会话还活着**：接着再发一句普通问题，能正常回答

## 7. 输入框编辑

打一段 `git commit -m 修一下中文注释`，然后：

- [ ] `←`/`→` 逐字符移动；中文一次跨一个字，不会卡在半个字上
- [ ] `Alt+←`/`Alt+→` 按词跳
- [ ] `Home`/`End` 到行首/行尾
- [ ] 光标移到中间时打字**插在光标处**，后面的文字不左右抖
- [ ] `Delete` 删光标上的字符，`Backspace` 删前一个
- [ ] `Ctrl+W` 从光标往前删一个词；`Ctrl+K` 删到行尾
- [ ] `Shift+Enter` 插换行（输入框长高），`Up`/`Down` 在行间移动且保持列
- [ ] `Ctrl+U` 清空，`Ctrl+L` 重画屏幕

## 8. slash 命令 / 补全 / 模型

- [ ] 打 `/` 弹出补全，里面既有 Core 的（`/help` `/compact` …）也有本地的（`/model` `/quit`）
- [ ] `↑`/`↓` 选，`Enter` 补全（不是提交），`Esc` 关掉且不动草稿
- [ ] `/model` 回车 → 转录里报当前模型
- [ ] `/model claude-sonnet-5` → 状态栏立刻换，转录留一条 note；下一轮真的用新模型（看 smoke 或日志）
- [ ] `/help` 转发给 Core 并有回应
- [ ] `/quit` 退出

## 9. 会话 resume

```sh
cargo run -p app                 # 聊两句，记下退出前的内容，Ctrl+D 退出
cargo run -p app -- --continue   # 或 --resume <session-id>
```

- [ ] 转录区恢复出上次的对话（用户/assistant/工具块都在）
- [ ] 接着提问时模型**记得**上文（问"我刚才问的第一个问题是什么"）
- [ ] `--resume 不存在的id` 报错退出，不是静默开新会话
- [ ] 没有任何历史时 `--continue` 正常开新会话（不报错）

## 10. 异常与收尾

- [ ] 故意把 `ANTHROPIC_AUTH_TOKEN` 改错 → 启动报错清楚，不是 panic 或空白屏
- [ ] 断网发一句 → 转录里是红色 Error 行，TUI 不卡死，还能继续输入
- [ ] 终端窗口拉宽/拉窄 → 布局跟着变，不错位
- [ ] 退出后 `~/.atta/sessions/<项目>/` 下有本次会话的 jsonl，内容不是空的

---

发现问题时请连着记：**哪一步、看到什么、期望什么**，最好带一张终端截图或 `RUST_LOG=debug` 的 stderr。
