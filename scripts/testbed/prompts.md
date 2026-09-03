# 真跑用的提示词

每条提示词冲着**一个 TUI 区域**去（区域名见 `docs/TUI_DESIGN.md`），一条一条发，跑完对着 `scripts/trace_report.py`
的输出核。靶子项目由 `scripts/testbed/make_testbed.sh` 生成，attacode 要在那个
目录里启动（`cd /tmp/attacode-testbed && attacode`）。

写提示词的三条原则：

1. **说清楚要哪个行为，别说要哪个 UI**。跟模型说"用 Grep 找一下"它会照做，说
   "让任务清单亮起来"它只会困惑。
2. **一条只点一个区域**。混在一起时，哪个环节断了看不出来。
3. **可复现**。别用"随便找个文件"，指名道姓；否则两次跑出来的东西没法比。

| # | 目标区域 | 提示词 | 该看到什么 |
|---|---|---|---|
| 1 | 转录：流式文本 | `用一句话说明 Python 里 list 和 tuple 的区别，不要用任何工具。` | assistant 文本逐字长出来；状态行有 spinner + 秒数；结束后 footer 用量涨 |
| 2 | 工具块 + 折叠 | `读一下 docs/CHANGELOG.md，然后告诉我里面一共有多少条记录。` | `⏺ Read(...)` 独立成块；结果默认折叠成 8 行 + `… N more lines`；F5 展开 |
| 3 | diff 渲染 | `把 src/greet.py 里的 GREETING 改成 "你好"，只改这一处。` | `⏺ Edit(...)`；结果里 `-` 行红底、`+` 行绿底、`@@` 灰 |
| 4 | 多结果工具 | `在 src/ 里找出所有带 TODO(perf) 标记的地方，列个清单。` | Grep/Glob 的块；三个文件都在结果里 |
| 5 | 状态·任务清单 `operation_status.task_list` | `按这四步做，每完成一步就更新一次待办清单：1) 读 README.md 2) 数一下 src 下有几个 .py 3) 把结果写进 docs/summary.md 4) 复述你写了什么。` | 状态区出现勾选列表；进行中的那条是 `●`，做完变 `✓` |
| 6 | 输入·提问框 `composer.content.ask` | `用 bash 跑一下 ./noisy.sh。` | 弹确认框（bash 属于要确认的）；四个选项；输入框变灰；答完继续 |
| 7 | 大命令输出折叠 | （接上一条批准之后）| 40 行输出折叠成 8 行 + 提示；F5 展开 |
| 8 | 子代理条 `sub_agent_bar` | `派一个子代理去把 src/ 下三个模块的实现各总结一句话，你自己不要直接读文件。` | 子代理条出现一行；跑完变 `✓` 并显示 token 数 |
| 9 | 取消 | `把 docs/CHANGELOG.md 逐条读一遍，每一条都展开讲讲。` → 中途 `Ctrl+C` | 状态行变 `Cancelling…`；转录出现 `Turn cancelled.`；**会话还活着**，下一句能正常回答 |
| 10 | 块选择（场景 7） | （前面已经有至少两个工具块）`Alt+↑` ×2 再 `F5` | 竖条 `▌` 标在更早那个块上；F5 展开的是被标记的那个 |
| 11 | 错误不中断 | `读一下 不存在的文件.md` | 工具结果是红色 `✗`；TUI 不卡死，还能继续发 |
| 12 | 模型切换 | `/model` 然后 `/model <另一个模型名>` | 先报当前模型；换完 footer 立刻变，转录留一条 note |
| 13 | resume | `Ctrl+D` 退出后 `attacode --continue` | 上次的转录回来了；`↑` 能翻到上次的输入；接着问"我刚才第一句问的是什么"模型答得上来 |

## 一次完整的跑法

```sh
set -a; . ../Core/.env; set +a          # LLM 配置
scripts/testbed/make_testbed.sh          # 造靶子
cd /tmp/attacode-testbed
ATTACODE_TRACE=/tmp/attacode-trace.jsonl /path/to/attacode
# …按上表一条条发…
# 退出后：
scripts/trace_report.py /tmp/attacode-trace.jsonl
```

报告里每个区域都该是 ✅。出现 ❌ 就对着那一行写的位置查——要么是这次没发对应的
提示词，要么是那条链路断了。
