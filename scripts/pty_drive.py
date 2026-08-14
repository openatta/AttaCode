#!/usr/bin/env python3
"""在一个真 pty 里跑 TUI，按脚本喂键，把终端输出回放成一帧帧文本。

为什么不是 tmux：这台机器上没有。为什么不是 `script(1)`：它给 pty 的窗口大小是
0x0（stdin 是管道时拿不到 TIOCGWINSZ），ratatui 于是一个字符都不画——那次"空白屏"
是量具坏了，不是程序坏了。这里显式 TIOCSWINSZ，尺寸自己说了算。

用法:
    scripts/pty_drive.py [--cols N] [--rows N] [--out FILE] [--frames N] -- <命令...>

键盘脚本从 stdin 读，每行一条，`延迟秒<TAB>要发的字节`（字节走 Python 转义）:
    2.0\thello
    0.5\t\x1b[D
"""
import argparse
import codecs
import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time
import unicodedata

CSI = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])")


def spawn(cmd, cols, rows):
    """fork 出一个带控制终端的子进程，返回 (pid, master_fd)。"""
    pid, fd = pty.fork()
    if pid == 0:  # 子进程：pty.fork 已经把 slave 接成 0/1/2 并设好控制终端
        os.execvp(cmd[0], cmd)
        os._exit(127)
    # 窗口大小要在子进程问之前设好——TUI 起来第一件事就是 query size。
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    return pid, fd


def drive(fd, script, tail=2.0):
    """按时间表喂键，同时不停读输出（不读的话 pty 缓冲满了子进程会阻塞）。

    返回 `(全部输出, 断点表)`。断点表是每一步**发键之前**输出流的长度——回放到
    那个长度就是"这一步按下去之前屏幕长什么样"，正好对上人工验收的节奏。
    ratatui 用的是差分渲染，不会每帧从左上角重画，所以只能这样切步。
    """
    out = []
    marks = []
    start = time.time()
    # 增量解码：一次 read 可能正好切在多字节字符中间，逐块 decode 会把它变成
    # 两个替换字符（屏幕上冒出 `��`，看着像程序输出坏了）。
    decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def pump(deadline):
        while time.time() < deadline:
            r, _, _ = select.select([fd], [], [], 0.05)
            if not r:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                return False
            if not chunk:
                return False
            out.append(decoder.decode(chunk))
        return True

    for label, delay, data in script:
        if not pump(time.time() + delay):
            break
        marks.append((label, sum(map(len, out))))
        os.write(fd, data)
    pump(time.time() + tail)
    marks.append(("最终", sum(map(len, out))))
    sys.stderr.write(f"[drive] {time.time() - start:.1f}s, {sum(map(len, out))} bytes\n")
    return "".join(out), marks


class Screen:
    """够用的终端回放：CUP / ED / EL / 可见字符。SGR 直接丢——这里看的是版面。"""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.clear()

    def clear(self):
        self.buf = [[" "] * self.cols for _ in range(self.rows)]
        self.r = self.c = 0

    def put(self, ch):
        if ch == "\n":
            self.r, self.c = min(self.r + 1, self.rows - 1), 0
        elif ch == "\r":
            self.c = 0
        elif ch in "\x07\x08":
            pass
        else:
            # 宽字符占两格。不算这个的话每写一个汉字，模型里的列就比真实终端少
            # 一格，后续的光标定位全体错位，屏幕上会多出重影字符——第一次跑就是
            # 被这个骗了，差点当成程序 bug。
            width = 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
            if 0 <= self.r < self.rows and 0 <= self.c < self.cols:
                self.buf[self.r][self.c] = ch
                for pad in range(1, width):
                    if self.c + pad < self.cols:
                        self.buf[self.r][self.c + pad] = ""
            self.c += width

    def text(self):
        rows = ["".join(r).rstrip() for r in self.buf]
        while rows and not rows[-1]:
            rows.pop()
        return "\n".join(rows)


def replay(data, cols, rows):
    """回放整段输出，返回**最终屏幕**的文本。

    ratatui 是差分渲染（只重画变化的格子），所以这里不切帧——回放到哪里，
    屏幕就是那一刻的样子。分步看是靠调用方截断 `data`（见 `drive` 的断点表）。
    """
    scr = Screen(cols, rows)
    i = 0
    while i < len(data):
        ch = data[i]
        if ch == "\x1b":
            m = CSI.match(data, i)
            if not m:
                i += 2
                continue
            nums = [int(p) for p in m.group(1).split(";") if p.isdigit()]
            cmd = m.group(2)
            if cmd == "H":
                r, c = (nums + [1, 1])[:2]
                scr.r, scr.c = r - 1, c - 1
            elif cmd == "J":
                scr.clear()
            elif cmd == "K":
                for c in range(scr.c, scr.cols):
                    scr.buf[scr.r][c] = " "
            i = m.end()
            continue
        scr.put(ch)
        i += 1
    return scr.text()


ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "e": "\x1b", "\\": "\\"}


def unescape(s):
    """只认 `\\xHH` 和几个常见转义，其余字符**原样**当 UTF-8 发。

    不能图省事用 `bytes.decode("unicode_escape")`：它按 latin-1 解每个字节，
    多字节字符会被拆成两个 U+00XX 再重新编码成 UTF-8——发进去就是乱码。
    第一版正是这么翻的车，屏幕上的"重构"变成了"éæ"。
    """
    out = bytearray()
    i = 0
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s):
            nxt = s[i + 1]
            if nxt == "x" and i + 3 < len(s):
                out.append(int(s[i + 2 : i + 4], 16))
                i += 4
                continue
            if nxt in ESCAPES:
                out += ESCAPES[nxt].encode()
                i += 2
                continue
        out += s[i].encode("utf-8")
        i += 1
    return bytes(out)


def parse_script(text):
    """每行 `延迟<TAB>字节`，可选第三列作为这一步的标签（只为报告好读）。"""
    out = []
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split("\t")
        delay = float(parts[0])
        payload = parts[1] if len(parts) > 1 else ""
        label = parts[2] if len(parts) > 2 else payload[:20]
        out.append((label, delay, unescape(payload)))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cols", type=int, default=100)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--out")
    ap.add_argument("--frames", type=int, default=4)
    ap.add_argument("cmd", nargs="+")
    args = ap.parse_args()

    script = parse_script(sys.stdin.read())
    pid, fd = spawn(args.cmd, args.cols, args.rows)
    raw, marks = drive(fd, script)
    try:
        os.close(fd)
    except OSError:
        pass
    _, status = os.waitpid(pid, 0)
    code, signal = status >> 8, status & 0x7F
    sys.stderr.write(f"[drive] child exit code={code} signal={signal}\n")
    # 关掉 master 会给子进程发 SIGHUP（signal=1）——那是量具收尾，不是程序崩了。
    # 想验"干净退出"就在脚本里发 Ctrl+D 并留出 tail 时间，这里应该看到 code=0。

    if args.out:
        with open(args.out, "w") as f:
            f.write(raw)

    print(f"# 终端 {args.cols}x{args.rows}，退出 code={code} signal={signal}")
    previous = None
    for label, upto in marks:
        screen = replay(raw[:upto], args.cols, args.rows)
        if screen == previous:
            continue  # 这一步屏幕没变化，跳过
        previous = screen
        print("─" * args.cols)
        print(f"◆ {label}")
        print("─" * args.cols)
        print(screen)
    print("─" * args.cols)


if __name__ == "__main__":
    main()
