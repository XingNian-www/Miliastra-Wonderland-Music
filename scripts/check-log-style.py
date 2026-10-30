#!/usr/bin/env python3
"""日志文案自检：按 technical-docs/logging-style.md 的禁止项扫描 src/。

为什么要有这个脚本（而不是靠 grep）：
logging-style.md §七 最初给的是三条**单行** grep，但 Rust 的 log 宏参数常跨行：

    log::warn!(
        "回复模板 {} 缺少变量 {:?}",
        spec.key, missing
    );

单行 grep 对这类写法**全部漏检**，跑出来是「0 命中」的假绿灯。
本脚本按**宏整体的括号配对**取出完整调用再判断，跨行也能抓到。

用法：
    python3 scripts/check-log-style.py           # 违规时退出码 1
    python3 scripts/check-log-style.py --list    # 逐条打印
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, 'src')

# 这两个 target 是给脚本消费的字段化日志流，logging-style.md §六 明确豁免。
EXEMPT_TARGETS = ('target: "timing"', 'target: "performance"')

LEVELS = ('trace', 'debug', 'info', 'warn', 'error')

LOG_START = re.compile(r'log::(trace|debug|info|warn|error)!\s*\(')

# 允许保留的通用缩写（不允许被「英文变量名」规则误判）。
ALLOWED_ABBR = {
    'OCR', 'HTTP', 'HTTPS', 'JSON', 'GPU', 'CPU', 'GDI', 'WGC', 'API', 'UID',
    'URL', 'URI', 'ID', 'AI', 'UI', 'TUI', 'WS', 'IO', 'CPU', 'OpenAI', 'WebSocket',
    'DX12', 'Dx12', 'Vulkan', 'Metal', 'OpenGL', 'WebGPU', 'PID', 'ChatBridge',
    'QQ', 'B站', 'BV', 'SAD', 'IR', 'ROI', 'EXE',
}

# 常见「可直译却仍用英文」的技术词，出现在正文里即算违规。
BAD_TECH_WORDS = re.compile(
    r'\b(runtime|worker|sequence|actor|keyword|pending|handler|payload|'
    r'callback|buffer|state machine|slot|token bucket)\b', re.IGNORECASE)


def iter_log_calls(text):
    """按括号配对取出每个 log 宏调用的完整文本与起始行号。"""
    for match in LOG_START.finditer(text):
        start = match.end() - 1  # 指向 '('
        depth = 0
        i = start
        while i < len(text):
            ch = text[i]
            if ch == '"':
                i += 1
                while i < len(text) and text[i] != '"':
                    i += 2 if text[i] == '\\' else 1
                i += 1
                continue
            if ch == "'":
                nxt = text[i + 1] if i + 1 < len(text) else ''
                if nxt == '\\':
                    k = i + 3
                    while k < len(text) and text[k] != "'":
                        k += 1
                    i = k + 1
                    continue
                if i + 2 < len(text) and text[i + 2] == "'":
                    i += 3
                    continue
            if ch == '(':
                depth += 1
            elif ch == ')':
                depth -= 1
                if depth == 0:
                    i += 1
                    break
            i += 1
        body = text[start:i]
        line = text[:match.start()].count('\n') + 1
        yield match.group(1), body, line


def first_literal(body):
    """取宏里的第一个字符串字面量（即日志正文模板）。"""
    m = re.search(r'"((?:[^"\\]|\\.)*)"', body)
    return m.group(1) if m else ''


def has_cjk(s):
    return any('\u4e00' <= ch <= '\u9fff' for ch in s)


def check():
    problems = []
    for dirpath, dirnames, filenames in os.walk(SRC):
        dirnames[:] = sorted(d for d in dirnames if d != 'target')
        for name in sorted(filenames):
            if not name.endswith('.rs'):
                continue
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, ROOT).replace(os.sep, '/')
            with open(path, encoding='utf-8', errors='replace') as handle:
                text = handle.read()
            for level, body, line in iter_log_calls(text):
                if any(t in body for t in EXEMPT_TARGETS):
                    continue
                msg = first_literal(body)
                where = '%s:%d' % (rel, line)

                # 1) 调试格式：{:?} / {:#?}
                if '{:?' in msg or '{:#?' in msg:
                    problems.append((where, 'debug-format', msg))
                # 2) 字段堆砌：name={value}
                if re.search(r'[A-Za-z_]\w*\s*=\s*\{', msg):
                    problems.append((where, 'field-dump', msg))
                # 3) 全英文消息（不含任何中日韩字符，且长度足够）
                stripped = re.sub(r'\{[^}]*\}', '', msg).strip()
                if stripped and not has_cjk(msg) and len(stripped) >= 4:
                    problems.append((where, 'english-only', msg))
                # 4) 可直译却仍用英文的技术词（warn/error 才严格要求）
                if level in ('warn', 'error'):
                    hit = BAD_TECH_WORDS.search(msg)
                    if hit:
                        problems.append((where, 'english-word:' + hit.group(1), msg))
    return problems


def main():
    problems = check()
    listing = '--list' in sys.argv
    if not problems:
        print('日志文案自检通过：无禁止项命中')
        return 0
    kinds = {}
    for _, kind, _ in problems:
        kinds[kind] = kinds.get(kind, 0) + 1
    print('日志文案自检发现 %d 处违规：' % len(problems))
    for kind, count in sorted(kinds.items(), key=lambda kv: -kv[1]):
        print('  %-22s %d' % (kind, count))
    if listing:
        print()
        for where, kind, msg in problems:
            print('  %-40s [%s] %s' % (where, kind, msg[:90]))
    else:
        print('\n用 --list 查看逐条明细')
    return 1


if __name__ == '__main__':
    sys.exit(main())
