#!/usr/bin/env python3
"""轮询 /monitor，丢掉窗口枚举噪声，把其余日志追加到文件。

背景：`tui.log_lines` 默认只有 200 行，而
`src/adapters/windows/window.rs` 的 `enum_windows_proc` 每枚举一个顶层窗口就打一条
`DEBUG 枚举到窗口，所属进程是「…」`。1 秒能刷掉几十行，200 行的环里往往只剩不到
2 秒的内容，`UI 状态检测耗时` 这类 debug 计时行根本留不住。

/monitor 没有游标、只回滑动窗口尾部，所以本脚本只能靠**高频轮询 + 重叠去重**尽量
不丢行；轮询间隔大于环被刷满的时间时，中间的行仍然会丢，脚本会在输出里标出缺口。
真正不丢的办法是把 `tui.log_lines` 调大，或给窗口枚举日志降噪。

用法：
    MILIASTRA_TOKEN=... python3 scripts/watch-monitor-logs.py
    python3 scripts/watch-monitor-logs.py --url http://192.168.31.242:18888 \
        --token "$MILIASTRA_TOKEN" --interval 0.3 --seconds 120 \
        --out target/monitor-logs.txt

令牌只从 --token 或环境变量 MILIASTRA_TOKEN 读取，不写进仓库、不回显。
默认排除「枚举到窗口」「无法读取窗口所属进程名」两类窗口枚举日志；
--keep-noise 可保留全部。Ctrl-C 结束并打印统计。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

DEFAULT_URL = "http://127.0.0.1:18888"
# 窗口枚举回调逐窗口打的 debug（src/adapters/windows/window.rs）。
NOISE_MARKERS = (
    "枚举到窗口，所属进程是",
    "无法读取窗口所属进程名",
)


def fetch_logs(url: str, token: str, timeout: float) -> list[str]:
    request = urllib.request.Request(
        url.rstrip("/") + "/monitor",
        headers={"X-Miliastra-Token": token} if token else {},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        payload = json.loads(response.read().decode("utf-8"))
    logs = payload.get("logs")
    if not isinstance(logs, list):
        raise ValueError("/monitor 响应里没有 logs 数组")
    return [line for line in logs if isinstance(line, str)]


def overlap(previous: list[str], current: list[str]) -> int:
    """current 开头与 previous 尾部重合的最长长度；找不到时返回 0（整段当新行）。"""
    limit = min(len(previous), len(current))
    for size in range(limit, 0, -1):
        if previous[-size:] == current[:size]:
            return size
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", default=os.environ.get("MILIASTRA_URL", DEFAULT_URL))
    parser.add_argument("--token", default=os.environ.get("MILIASTRA_TOKEN", ""))
    parser.add_argument("--interval", type=float, default=0.3, help="轮询间隔秒，默认 0.3")
    parser.add_argument("--seconds", type=float, default=0, help="运行时长秒，0 表示一直跑")
    parser.add_argument("--out", default="", help="追加写入的文件；缺省只打印到标准输出")
    parser.add_argument("--timeout", type=float, default=5.0, help="单次请求超时秒")
    parser.add_argument("--keep-noise", action="store_true", help="保留窗口枚举日志")
    args = parser.parse_args()

    destination = open(args.out, "a", encoding="utf-8") if args.out else None
    previous: list[str] = []
    kept = dropped = gaps = errors = 0
    started = time.monotonic()
    print(
        f"开始轮询 {args.url}/monitor，间隔 {args.interval}s"
        + ("（保留窗口枚举日志）" if args.keep_noise else "（排除窗口枚举日志）"),
        file=sys.stderr,
    )
    try:
        while args.seconds <= 0 or time.monotonic() - started < args.seconds:
            try:
                current = fetch_logs(args.url, args.token, args.timeout)
            except (urllib.error.URLError, TimeoutError, ValueError, json.JSONDecodeError) as error:
                errors += 1
                print(f"[轮询失败] {error}", file=sys.stderr)
                time.sleep(args.interval)
                continue
            fresh = current[overlap(previous, current):]
            # 上一轮见过的尾部一行都对不上：环在两次轮询之间被刷过头，中间有行丢失。
            if previous and fresh == current:
                gaps += 1
                note = f"[缺口] 与上一轮无重叠，期间可能丢行（本轮环内 {len(current)} 行）"
                print(note, file=sys.stderr)
                _emit(note, destination)
            for line in fresh:
                if not args.keep_noise and any(marker in line for marker in NOISE_MARKERS):
                    dropped += 1
                    continue
                kept += 1
                _emit(line, destination)
            previous = current
            time.sleep(args.interval)
    except KeyboardInterrupt:
        pass
    finally:
        if destination:
            destination.close()
    print(
        f"结束：保留 {kept} 行，排除窗口枚举 {dropped} 行，疑似缺口 {gaps} 次，请求失败 {errors} 次",
        file=sys.stderr,
    )
    return 0


def _emit(line: str, destination) -> None:
    print(line, flush=True)
    if destination:
        destination.write(line + "\n")
        destination.flush()


if __name__ == "__main__":
    sys.exit(main())
