#!/usr/bin/env python3
"""绑定多重集比对：改动前后每个 .vue 的 @click / v-model / :disabled / v-if / v-for / @change。

为什么需要：视觉改造只允许动模板结构与 <style>，任何一处 @click / v-model /
:disabled / v-if / v-for / @change 的增删改都会改业务行为。肉眼看 diff 靠不住
（重排、换行、缩进都会掩盖真正丢掉的绑定），所以这里把每个绑定抽成一条
「指令 + 规范化后的表达式」，按多重集逐文件比对。

用法：
    python3 scripts/panel-binding-diff.py --base <改动前的快照目录> [--files a.vue,b.vue]

--base 里按**相同相对路径**放改动前的文件；--files 省略时比对 base 目录里所有 .vue。
只比多重集，不比顺序：模板重排是允许的，丢一个绑定、多一个绑定都不允许。
"""
from __future__ import annotations

import argparse
import re
import sys
from collections import Counter
from pathlib import Path

# 只看这几类会影响行为的绑定。@click.stop / @keydown.enter.prevent 这类修饰符
# 保留在指令名里，因为去掉 .stop 会真的改变事件冒泡行为。
DIRECTIVE = re.compile(
    r'(?<![\w-])(@[a-zA-Z][\w.-]*|v-model(?::[a-zA-Z][\w.-]*)?|:[a-zA-Z][\w.-]*|\bv-if|\bv-else-if|\bv-else|\bv-for|\bv-show|\bv-bind(?::[a-zA-Z][\w.-]*)?|\bv-on:[a-zA-Z][\w.-]*)(\s*=\s*)',
)

# 只关心这些「行为类」指令名；:class / :style / :title / :key 属于纯展示，改视觉时
# 允许调整（例如给行加 selected 类），因此不计入比对。
BEHAVIOR = re.compile(
    r'^(v-model.*|v-if|v-else-if|v-else|v-for|v-show|v-bind.*|v-on:.*'
    r'|@(click|change|input|keydown|keyup|submit|focus|blur|scroll)(\.[\w.-]+)?'
    # `:` 简写里只有这些是行为开关；:class / :style / :title / :key 属于纯展示，不计。
    r'|:(disabled|value|checked|readonly|maxlength|min|max|step|aria-selected|aria-expanded))',
)

ATTR_VALUE = re.compile(r'"([^"]*)"|\'([^\']*)\'')


def strip_comments(text: str) -> str:
    text = re.sub(r'<!--.*?-->', ' ', text, flags=re.S)
    # 去掉 <script> 与 <style> 块：它们不是模板绑定，且样式改动是本任务允许的。
    text = re.sub(r'<script\b.*?</script>', ' ', text, flags=re.S | re.I)
    text = re.sub(r'<style\b.*?</style>', ' ', text, flags=re.S | re.I)
    return text


def norm_expr(expr: str) -> str:
    """归一化绑定表达式：折叠空白，去掉语法上无意义的空格差异。"""
    return re.sub(r'\s+', ' ', expr).strip()


def bindings(path: Path) -> Counter:
    raw = strip_comments(path.read_text(encoding='utf-8'))
    out: Counter = Counter()
    for match in DIRECTIVE.finditer(raw):
        name = match.group(1)
        if not BEHAVIOR.match(name):
            continue
        tail = raw[match.end():match.end() + 4000]
        value_match = ATTR_VALUE.match(tail)
        expr = norm_expr(value_match.group(1) if value_match.group(1) is not None else value_match.group(2)) if value_match else ''
        out[f'{name}="{expr}"'] += 1
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument('--base', required=True, help='改动前快照目录')
    parser.add_argument('--files', default='', help='逗号分隔的相对路径；省略则比对 base 下全部 .vue')
    parser.add_argument('--root', default='.', help='改动后的仓库根（默认当前目录）')
    args = parser.parse_args()

    base_root = Path(args.base)
    root = Path(args.root)
    if args.files:
        paths = [Path(item.strip()) for item in args.files.split(',') if item.strip()]
    else:
        paths = sorted(p.relative_to(base_root) for p in base_root.rglob('*.vue'))

    failed = 0
    total = 0
    for rel in paths:
        old = bindings(base_root / rel)
        new_path = root / rel
        if not new_path.exists():
            print(f'MISSING {rel}: 改动后文件不存在')
            failed += 1
            continue
        new = bindings(new_path)
        total += sum(old.values())
        status = 'OK  ' if old == new else 'DIFF'
        if old != new:
            failed += 1
        print(f'[{status}] {rel}: 改动前 {sum(old.values())} 条 / 改动后 {sum(new.values())} 条 / 多重集{"相等" if old == new else "不等"}')
        for key in sorted(set(old) | set(new)):
            if old[key] != new[key]:
                print(f'        {key!r}: {old[key]} -> {new[key]}')

    print(f'\n合计比对 {len(paths)} 个文件、{total} 条行为绑定；不一致文件 {failed} 个。')
    return 0 if failed == 0 else 1


if __name__ == '__main__':
    sys.exit(main())
