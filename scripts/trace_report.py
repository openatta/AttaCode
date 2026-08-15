#!/usr/bin/env python3
"""把 `ATTACODE_TRACE` 打出来的帧记录汇总成一张"各区块收到内容了吗"的表。

真跑一轮之后拿它看：转录里出现过哪些行类型、状态行动过没有、任务清单/子代理条/
权限队列有没有过内容、用量涨了没有。**没收到过内容的区块**是重点——那正是
"接了但其实是死的"藏身的地方。

    ATTACODE_TRACE=/tmp/t.jsonl cargo run -p app
    scripts/trace_report.py /tmp/t.jsonl
"""
import json
import sys
from collections import Counter

# 每个区块：怎么判断"这一帧它有内容"，以及没收到时该往哪儿看。
REGIONS = [
    ("转录 (Z0.R1)", lambda f: f["entries"] > 0, "reducer 的 apply_event"),
    ("sticky header (Z0.R0)", lambda f: f["header"] is not None, "reducer::current_prompt + app::merge"),
    ("状态行 (Z1)", lambda f: f["status"] is not None, "reducer::refresh_running_status"),
    ("任务清单 (Z1)", lambda f: f["tasks"] > 0, "TodoWrite 工具的 input"),
    ("子代理条 (Z3)", lambda f: f["sub_agents"] > 0, "AgentEvent::SubagentProgress"),
    ("权限对话框 (Z2)", lambda f: f["approvals"] > 0, "AgentEvent::PermissionPrompt"),
    ("块选中态", lambda f: f["selected_block"] is not None, "app 的 Alt+↑/↓"),
    ("用量 (Z4)", lambda f: f["tok_in"] > 0 or f["tok_out"] > 0, "TurnComplete.usage"),
]


def main():
    path = sys.argv[1]
    frames = []
    for line in open(path):
        line = line.strip()
        if line:
            frames.append(json.loads(line))
    if not frames:
        print("打点文件是空的——是不是没设 ATTACODE_TRACE 就跑了？")
        return 1

    last = frames[-1]
    print(f"# 帧数 {len(frames)}")

    print("\n## 收到的事件")
    for name, n in Counter(f["event"] for f in frames).most_common():
        print(f"  {n:5}  {name}")

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
