# 真机验收清单

> 提到 TUI 的某一块时，用 `docs/TUI_DESIGN.md` 里的规范名（代码路径 / English / 中文
> 三选一，比如 `composer.content.ask` = Ask Box = 输入·提问框）。那张表是唯一权威。

单元测试覆盖不到的东西：raw mode 下的渲染、和真 API 的时序、按键在真实终端里的编码。
每次动了渲染/事件循环/装配之后跑一遍这份清单。

## 打点：跑的时候看每个区域到底收到了什么

`ATTACODE_TRACE=<路径>` 打开之后，每一帧和每一次按键都会记一行 JSON——按键记
"收到了什么键、解析成了哪个 action"，帧记"这一刻各区域各有多少内容"。跑完：

```sh
scripts/trace_report.py /tmp/attacode-trace.jsonl
```

报告会直接指出**哪些区域从来没收到过内容**。两条经验：

- 打点打在 app 渲染前那一帧（合并之后），不是 bridge 广播那一刻——选中块、滚动、
  草稿都是 app 才加上去的，只打 bridge 那侧会把"块选中态"误报成红的。
- 按键那一条不是可有可无：真跑时 Ctrl+C 看着"没反应"，光看帧记录只能看出
  "取消没发生"，看不出是键没送到、没匹配上、还是没分派。有了按键记录一眼就看到
  `Action("repl.cancel")` 确实产生了，问题在别处（那次是 turn 已经结束了，
  `request_cancel` 按设计空转）。

## 先自动过一遍（不需要 API key）

`scripts/pty_drive.py` 在一个真 pty 里跑 attacode、按脚本喂键、把终端输出回放成
一屏一屏的文本。没有 API key 也能跑——用一个假 token 起来，除了"真的调模型"以外
的部分都能验（渲染、按键、编辑、补全、本地命令、错误路径、干净退出）。

```sh
cargo build -p app
cat > /tmp/keys.txt <<'KEYS'
2.0		起手
0.5	/model\r	提交 /model
1.5	hello\r	提交一句（假 token → 错误行）
7.0	\x15\x04	清空后 Ctrl+D 退出
KEYS
ANTHROPIC_AUTH_TOKEN=sk-fake TERM=xterm-256color \
  python3 scripts/pty_drive.py --cols 96 --rows 20 -- ./target/debug/attacode < /tmp/keys.txt
```

每行 `延迟秒 <TAB> 要发的字节 <TAB> 标签`；`\xHH` 走转义，其余字符原样发。输出是
每一步按键**之前**的屏幕，屏幕没变化的步骤自动跳过。

两个已经踩过的坑，别重复踩：
- **别用 `script(1)` 造 pty**——stdin 是管道时它给的窗口大小是 0x0，ratatui 一个字符
  都不画，看起来像"程序启动后白屏"，其实是量具坏了。
- 回放要按**东亚宽字符占两格**算，否则每个汉字都会让后面的列错位，屏幕上多出重影。

下面清单里带 🤖 的项目 pty 脚本已经能覆盖，人工跑时可以略过；其余的（流式、工具、
权限、resume 往返）必须真 API + 真终端。

**准备**

```sh
set -a; . .env; set +a          # ANTHROPIC_AUTH_TOKEN 或 ANTHROPIC_API_KEY
cargo build --workspace          # 先确认能编译，别在 alt screen 里看编译错误
```

## 0.5 靶子项目

`scripts/testbed/make_testbed.sh` 造一个固定内容的小项目（`/tmp/attacode-testbed`），
每个文件都冲着某个区域去；配套的提示词在 `scripts/testbed/prompts.md`，一条点一个
区域。别拿 AttaCode 自己当靶子——模型可能真去改源码，而且每次看到的东西都不一样，
跑出来没法比。

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

- [ ] 🤖 进入 alt screen，底部有输入框、状态栏显示模型名和 cwd
- [ ] 🤖 输入框光标是那个块（`█`），闪不闪都行，但必须在 `> ` 后面
- [ ] 🤖 `Ctrl+D`（空草稿）能退出（退出码 0），终端恢复正常、没有残留的 raw mode

## 2. 一轮普通问答

发一句 `用一句话解释 Rust 的所有权`。

- [ ] 🤖 用户输入立刻回显（不等 API）
- [ ] assistant 文本**流式**长出来，不是一次性整段出现
- [ ] 状态行有 spinner + 走字的秒数 + token 计数
- [ ] 转录顶部出现 sticky header，钉着你刚才那句问题 —— **只在那句已经滚出视口时**；还看得见时不该重复显示 🤖
- [ ] 结束后 spinner 消失，footer 的累计用量增加

- [ ] 🤖 让模型回一段**带空行和列表**的话（"分三点说明 Rust 的所有权，每点之间空一行"）
      → 转录里真的是分行分点的。挤成一长串再被宽度截断，就是转录的行模型塌了
      （一条 entry = 屏幕一行，而 ratatui 的 `Line` 会把里面的 `\n` 直接吞掉）

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
- [ ] 选"本项目一直允许" → `<项目>/.atta/settings.local.json` 里多出一条规则，
      **退出重开之后同类调用仍然不弹**（写盘那半在 Core，读回来那半在
      `bridge::permission::build`——只读 `settings.json` 一层的话，文件在那儿没人读）
- [ ] 对话框开着时 `Ctrl+C` 能中断整个 turn

## 5.5 模型向你提问（AskUserQuestion）

AttaCore 0.2.0 起这个工具是**真的会去问人**的（在那之前它把问题原样回给模型当答案）。
Core 自带的 `Elicitation` 只认权限提问，所以我们换了自己那版工具——见
`crates/bridge/src/ask.rs`。两种问法都要试。

发一句 `用 AskUserQuestion 问我：这个分支该叫什么，给我 feat/x 和 fix/y 两个选项`：

- [ ] 弹出对话框，标题是模型给的 header，选项就是模型给的那两个
- [ ] 输入框变灰（锁住）
- [ ] `↑`/`↓` + `Enter` 选一个 → 模型接下来的话里出现的是**选项的 key**，不是别的
- [ ] **`y`/`n`/`Esc` 在这个框上什么都不做**——它们是权限门的快捷键，这里没有对应的选项，
      按下去替你答一道没答过的题是最糟的一种"方便"

再发一句 `用 AskUserQuestion 问我这个分支该叫什么，不要给选项`：

- [ ] 对话框只显示问题，**输入框没有变灰**，底下提示是 "Type your answer below, then Enter"
- [ ] 打字进的是草稿（不是被丢掉）
- [ ] `Enter` 之后这一行成了**答案**，转录里**没有**多出一条"用户说了这句话"的新一轮对话
- [ ] 答案以 `/` 开头（比如打 `/tmp 那个目录`）时照样是答案，不会被当成 slash 命令
- [ ] 问题挂着的时候 `Ctrl+C` 中断 turn → 对话框跟着消失，输入框恢复可用
      （不消失的话它会一直占着 composer，人连字都打不了）
- [ ] 中断的**同一瞬间**按回车提交一行 → 转录里有一句"这个问题已经撤走了，
      这行答案没送出去"，而不是那一行凭空消失

### 5.6 两种问题排在一起（这一段最容易回归）

让模型在同一轮里先 `AskUserQuestion`（不带选项）再做一次要审批的写操作，凑出
`[自由文本题, 权限请求]` 这个队列：

- [ ] 当前是那道问答题时：输入框**没有变灰**，能打字
- [ ] `Tab` 能切到后面那个权限请求（切过去之后输入框变灰）
      ——切不过去的话它会一直挂到 300 秒超时被自动拒绝
- [ ] 切回问答题，输入框又能用了
- [ ] 队列里还有东西时打开 `/resume` → 列表根本不出现（有待确认请求时它会被收起来，
      因为屏幕上没有的东西不该能被操作）

## 6. 取消

发一个长任务（`把整个 crates/ 目录读一遍并总结`），中途 `Ctrl+C`。

- [ ] spinner 变成 `Cancelling…`
- [ ] 转录里出现 `Turn cancelled.`，spinner 停
- [ ] **会话还活着**：接着再发一句普通问题，能正常回答

## 7. 输入框编辑

打一段 `git commit -m 修一下中文注释`，然后：

- [ ] 🤖 `←`/`→` 逐字符移动；中文一次跨一个字，不会卡在半个字上
- [ ] `Alt+←`/`Alt+→` 按词跳
- [ ] 🤖 `Home`/`End` 到行首/行尾
- [ ] 🤖 光标移到中间时打字**插在光标处**，后面的文字不左右抖
- [ ] 🤖 `Delete` 删光标上的字符，`Backspace` 删前一个
- [ ] 🤖 `Ctrl+W` 从光标往前删一个词；`Ctrl+K` 删到行尾
- [ ] `Shift+Enter` 插换行（输入框长高），`Up`/`Down` 在行间移动且保持列
- [ ] `Ctrl+U` 清空，`Ctrl+L` 重画屏幕

## 8. slash 命令 / 补全 / 模型

- [ ] 🤖 打 `/` 弹出补全，里面既有 Core 的（`/help` `/compact` …）也有本地的（`/model` `/doctor` `/resume` `/quit`）
- [ ] 🤖 `↑`/`↓` 选，`Enter` 补全（不是提交），`Esc` 关掉且不动草稿；**已经打全的命令按 Enter 应该直接提交**
- [ ] 🤖 `/model` 回车 → 转录里报当前模型
- [ ] `/model claude-sonnet-5` → 状态栏立刻换，转录留一条 note；下一轮真的用新模型（看 smoke 或日志）
- [ ] `/help` 转发给 Core 并有回应
- [ ] `/doctor` → 转录里一份**五行**的报告（`model.provider` / `history.store` /
      `sandbox` / `permissions` / `settings.unused`），**每条各占一行**
      ——挤成一串说明转录的行模型又塌了，见 `push_lines`
- [ ] `/doctor` 的 `sandbox` 那行在 macOS 上是 `MacOSSandboxExec`（不是 `Unavailable`）
      ——是 `Unavailable` 就说明 `base/sandbox` 那个 feature 掉了，见 CLAUDE.md
- [ ] `model.provider` 那行说得出**选中的 provider id**，端点被 `ANTHROPIC_BASE_URL`
      改过时还会带上 `→ <url>`
- [ ] 往 `.atta/settings.json` 里加一段 `"scripts": [...]` → `/doctor` 的
      `settings.unused` 变成 `!`，并说清楚为什么不生效（这三段今天是接不上的，
      但绝不该是静默的）
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

会话选择器（`/resume`）。**这是唯一一条会把整个引擎重建的命令**，所以要看的不只是
列表本身：

- [ ] `/resume` 回车 → 弹出列表，每行是 `时间  N msgs  这次会话讲了什么`
- [ ] `↑`/`↓` 换选中项；**打字不落进草稿**（列表开着时键盘整体归它）
- [ ] `Esc` 关掉列表，草稿和转录都没动
- [ ] `Enter` 选中一个 → 转录换成那个会话的内容，接着提问模型**记得那边的上文**
- [ ] 换过去之后 `/resume` 再换回来，两边都还在
- [ ] `/resume 一个关键词` → 只列内容里有这个词的会话
- [ ] `/resume 一个绝对匹配不到的词` → 转录里一句 "nothing matches"，**不弹空列表**
- [ ] 一个全新的空项目里 `/resume` → 一句 "no earlier sessions in this project"
- [ ] 攒够 10 个以上会话再 `/resume` → **能一直往下翻到最后一条**，选中项始终在框里，
      边框上有 `n/总数`（以前固定只画 3 行、没有滚动，第 4 条往后的高亮在屏幕外面走，
      回车换到一个你从没看见过的会话）
- [ ] 列表里看到的是"时间 / 条数 / 讲了什么"，**不是**一串 BASE58 id
- [ ] turn 跑着的时候开 `/resume`，按 `Ctrl+C` → turn 被中断，**列表不关**

## 10. 异常与收尾

- [ ] 🤖 不设 `ANTHROPIC_AUTH_TOKEN` → 启动报错清楚（退出码 1），不是 panic 或空白屏
- [ ] 🤖 凭据/网络出错发一句 → 转录里是红色 Error 行，TUI 不卡死，还能继续输入
- [ ] 终端窗口拉宽/拉窄 → 布局跟着变，不错位
- [ ] 退出后 `~/.atta/projects/<项目>/` 下有本次会话的 jsonl，内容不是空的
      （0.1.5 前落在 `~/.atta/sessions/<项目>/`；老会话由 `history::migrate` 搬过来，
      搬完那边只剩按 session id 命名的 sidecar 目录）

---

发现问题时请连着记：**哪一步、看到什么、期望什么**，最好带一张终端截图或 `RUST_LOG=debug` 的 stderr。
