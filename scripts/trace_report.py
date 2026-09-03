#!/usr/bin/env python3
"""把 `ATTACODE_TRACE` 打出来的帧记录汇总成一张"各区块收到内容了吗"的表。

真跑一轮之后拿它看：转录里出现过哪些行类型、状态行动过没有、任务清单/子代理条/
权限队列有没有过内容、用量涨了没有。**没收到过内容的区块**是重点——那正是
"接了但其实是死的"藏身的地方。

    ATTACODE_TRACE=/tmp/t.jsonl cargo run -p app
    scripts/trace_report.py /tmp/t.jsonl
"""
import json
import re
import sys
from collections import Counter

# 单个可见字符、且只按了 Shift 或什么都没按 = 用户在打字，不是在按快捷键。
# （大写字母带 SHIFT，一样是打字——漏掉它会让 "TODO" 这种输入混进快捷键统计。）
TYPING = re.compile(r"^KeyModifiers\((0x0|SHIFT)\)\+Char\(")

# 每个区域：怎么判断"这一帧它有内容"，以及没收到时该往哪儿看。
#
# 名字用 docs/TUI_DESIGN.md 的规范名（代码路径 + 中文），不再用 Z/R/S 坐标——那套
# 已经废止，而且这张表里曾经就把任务清单标成 Z1（实际 Z1.R1）、把提问框标成 Z2
# （实际 Z2.R2.S2）：要数三层才写得对的东西，写着写着就不对了。
REGIONS = [
    ("转录·正文 transcript.body", lambda f: f["entries"] > 0, "reducer 的 apply_event"),
    ("转录·顶栏 transcript.header", lambda f: f["header"] is not None, "reducer::current_prompt + app::merge"),
    ("状态·状态行 operation_status.status_line", lambda f: f["status"] is not None, "reducer::refresh_running_status"),
    ("状态·任务清单 operation_status.task_list", lambda f: f["tasks"] > 0, "TodoWrite 工具的 input"),
    ("子代理条 sub_agent_bar", lambda f: f["sub_agents"] > 0, "AgentEvent::SubagentProgress"),
    ("输入·提问框 composer.content.ask", lambda f: f["asks"] > 0, "AgentEvent::PermissionPrompt / bridge::ask"),
    ("转录块选中态 transcript.body.selected_block", lambda f: f["selected_block"] is not None, "app 的 Alt+↑/↓"),
    ("底栏·用量 footer_hints.usage", lambda f: f["tok_in"] > 0 or f["tok_out"] > 0, "TurnComplete.usage"),
]


def main():
    path = sys.argv[1]
    frames, keys = [], []
    for line in open(path):
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        # 一个文件里两种记录：帧和按键。按键那种只有 key/outcome，没有区块字段，
        # 混进帧统计里会直接 KeyError。
        (keys if rec.get("event") == "key" else frames).append(rec)
    if not frames:
        print("打点文件里一帧都没有——是不是没设 ATTACODE_TRACE 就跑了？")
        return 1

    last = frames[-1]
    print(f"# 帧 {len(frames)}  按键 {len(keys)}")

    print("\n## 收到的事件")
    for name, n in Counter(f["event"] for f in frames).most_common():
        print(f"  {n:5}  {name}")

    if keys:
        # 普通打字（无修饰键的可见字符）一定是 Unmatched——它就该落进草稿，不是
        # 快捷键。全列出来只会把真正要看的东西刷下去，折成一行。
        typing = [r for r in keys if TYPING.match(r["key"])]
        rest = [r for r in keys if not TYPING.match(r["key"])]
        print("\n## 按键")
        if typing:
            print(f"  {len(typing):5}  （普通输入，落进草稿）")
        for (k, o), n in Counter((r["key"], r["outcome"]) for r in rest).most_common(15):
            # 绑了却没分派出去的才是问题：解析成了 action 却看不到后续效果，
            # 或者压根没解析出来而它本该是个快捷键。
            mark = " ⚠️" if "none" in o.lower() or "unhandled" in o.lower() else ""
            print(f"  {n:5}  {k:<40} → {o}{mark}")

    print("\n## 转录里出现过的行类型")
    kinds = Counter()
    for f in frames:
        for k, n in f["kinds"].items():
            kinds[k] = max(kinds[k], n)  # 取各帧里的峰值，不是累加
    for k, n in kinds.most_common():
        print(f"  {n:5}  {k}")
    if not kinds:
        print("  （一条都没有）")

    print("\n## 各区块")
    missing = []
    for name, has_content, where in REGIONS:
        hit = sum(1 for f in frames if has_content(f))
        if hit:
            print(f"  ✅ {name:<24} {hit} 帧有内容")
        else:
            print(f"  ❌ {name:<24} 从来没有过内容 —— 看 {where}")
            missing.append(name)

    print("\n## 最后一帧")
    print(f"  模型 {last['model']}  用量 {last['tok_in']}↑ {last['tok_out']}↓  轮数 {last['turns']}")

    if missing:
        print(f"\n{len(missing)} 个区块从没收到内容：{'、'.join(missing)}")
        print("如果这次的提示词本来就没打算触发它们，那是正常的；否则就是链路断了。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
