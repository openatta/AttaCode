#!/bin/bash
# 造一个供 attacode 真跑用的小项目。
#
# 为什么要专门造一个：拿 AttaCode 自己当靶子有两个问题——模型可能真去改我们的
# 源码，而且它每次看到的东西都随着仓库变，跑出来的结果没法比。这里的靶子是
# 固定的、可丢弃的、每次重建都一模一样。
#
# 每个文件都是冲着某个 UI 区块去的，见下面的注释。
#
#   scripts/testbed/make_testbed.sh [目录]     默认 /tmp/attacode-testbed
set -euo pipefail
DIR="${1:-/tmp/attacode-testbed}"
rm -rf "$DIR"
mkdir -p "$DIR/src" "$DIR/docs"

# ── 给 Edit 用：短、结构清楚，改一处就能看到干净的 diff ──
cat > "$DIR/src/greet.py" <<'EOF'
"""问候语工具。"""

GREETING = "hello"


def greet(name):
    return f"{GREETING}, {name}!"


def farewell(name):
    return f"bye, {name}!"
EOF

# ── 给 Read + 折叠用：够长，一屏放不下，必须折叠 ──
{
  echo "# 变更记录"
  echo
  for i in $(seq 1 60); do
    echo "- 第 $i 条：调整了模块 $((i % 7)) 的行为，影响面很小。"
  done
} > "$DIR/docs/CHANGELOG.md"

# ── 给 Grep/Glob 用：同一个标记散在多个文件里 ──
for m in alpha beta gamma; do
  cat > "$DIR/src/mod_${m}.py" <<EOF
"""模块 ${m}。"""

# TODO(perf): 这里有一次多余的遍历，${m} 模块
def run_${m}(items):
    total = 0
    for it in items:
        total += it
    return total
EOF
done

# ── 给"大命令输出"用：跑起来会刷屏，验折叠 ──
cat > "$DIR/noisy.sh" <<'EOF'
#!/bin/bash
# 故意刷屏，用来验大输出会不会折叠
for i in $(seq 1 40); do echo "输出行 $i / 40"; done
EOF
chmod +x "$DIR/noisy.sh"

cat > "$DIR/README.md" <<'EOF'
# attacode 测试靶子

固定内容的小项目，专门用来把 TUI 的各个区块都点亮一遍。别在这里放真东西——
每次 `make_testbed.sh` 都会整个删掉重建。

- `src/greet.py`      改一行 → 看 diff 渲染
- `src/mod_*.py`      三个文件里都有 `TODO(perf)` → 看 Grep 多结果
- `docs/CHANGELOG.md` 60 行 → 看大输出折叠
- `noisy.sh`          刷 40 行 → 看命令输出折叠
EOF

echo "$DIR"
