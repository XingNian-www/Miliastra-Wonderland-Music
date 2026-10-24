#!/usr/bin/env python3
"""分层方向检查：只允许上层依赖下层。

为什么单独放在 scripts/ 而不是复用 technical-docs/code-map.py：
technical-docs/ 整个目录被 .gitignore 忽略（.gitignore:19），不能作为 CI 闸门。

规则（scripts/layer-policy.json）：
- forbid: 明确禁止的层间边，出现即失败
- allow_files: 对少数既有设计边，按**文件白名单**放行；新增文件不自动豁免

用法：
    python3 scripts/check-layers.py           # 检查，违规退出码 1
    python3 scripts/check-layers.py --list    # 打印当前全部层间边
"""

import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, 'src')
POLICY = os.path.join(ROOT, 'scripts', 'layer-policy.json')

# 顶层模块 → 层名。debug_* 的扁平别名（lib.rs 里的 pub(crate) use）也要归到 debug。
def layer_of(rel):
    if rel.startswith('src/crates/'):
        return 'crates'
    rest = rel[4:]
    top = rest.split('/')[0]
    if '.' in top:
        top = top[:-3]
    if top.startswith('debug'):
        return 'debug'
    return top


# 根级叶子模块：只被引用、不引用业务层。
ROOT_LEAVES = {'lib', 'main', 'privacy', 'text', 'test_support', 'geometry',
               'change_detection', 'input_gate', 'composition'}


def normalize_module(name):
    if name.startswith('debug'):
        return 'debug'
    if name in ('lib', 'main', 'privacy', 'text', 'test_support'):
        return 'root'
    return name


USE_RE = re.compile(r'(?<!\w)crate::([A-Za-z_]\w*)')


TEST_MOD_DECL = re.compile(r'(?m)^[ \t]*#\[cfg\(test\)\]\s*\n\s*mod\s+\w+\s*;')
TEST_MOD_BLOCK = re.compile(r'(?m)^[ \t]*#\[cfg\(test\)\]\s*\n\s*mod\s+\w+\s*\{')


def strip_test_code(text):
    """删除顶层 #[cfg(test)] mod 声明，保留其后的生产代码。

    旧实现从第一个匹配截断到文件末尾：只要文件里有一行
    `#[cfg(test)] mod xxx_tests;`，它后面的**全部生产代码**都不再参与检查。
    `composition/application.rs` 因此整份文件被跳过（95 处 crate:: 引用不可见）。

    现在只删 `mod x;` 这一行声明；内联的 `mod x { ... }` 用花括号配对删除。
    """
    out = []
    i = 0
    while i < len(text):
        decl = TEST_MOD_DECL.search(text, i)
        block = TEST_MOD_BLOCK.search(text, i)
        candidates = [m for m in (decl, block) if m]
        if not candidates:
            out.append(text[i:])
            break
        marker = min(candidates, key=lambda m: m.start())
        out.append(text[i:marker.start()])
        if marker is decl:
            i = marker.end()
            continue
        # 内联块：从 '{' 起做花括号配对，跳过字符串与字符字面量里的括号。
        depth = 0
        j = marker.end() - 1
        while j < len(text):
            ch = text[j]
            if ch == '"':
                j += 1
                while j < len(text) and text[j] != '"':
                    j += 2 if text[j] == '\\' else 1
            elif ch == '{':
                depth += 1
            elif ch == '}':
                depth -= 1
                if depth == 0:
                    j += 1
                    break
            j += 1
        i = j
    return "".join(out)


def walk():
    for dirpath, dirnames, filenames in os.walk(SRC):
        dirnames[:] = sorted(d for d in dirnames if d not in {'target'})
        for name in sorted(filenames):
            if not name.endswith('.rs'):
                continue
            if name.endswith('_tests.rs') or name == 'tests.rs':
                continue
            full = os.path.join(dirpath, name)
            yield os.path.relpath(full, ROOT).replace(os.sep, '/')


def collect():
    edges = {}
    for rel in walk():
        layer = layer_of(rel)
        if layer in ROOT_LEAVES and layer != 'composition':
            continue
        with open(os.path.join(ROOT, rel), encoding='utf-8', errors='replace') as handle:
            text = strip_test_code(handle.read())
        for match in USE_RE.finditer(text):
            target = normalize_module(match.group(1))
            if target == layer:
                continue
            edges.setdefault((layer, target, rel), 0)
            edges[(layer, target, rel)] += 1
    return edges


def main(argv):
    with open(POLICY, encoding='utf-8') as handle:
        policy = json.load(handle)
    forbid = {tuple(edge) for edge in policy.get('forbid', [])}
    allow_files = {tuple(edge) for edge in policy.get('allow_files', [])}

    edges = collect()
    if '--list' in argv[1:]:
        for (source, target, rel), count in sorted(edges.items()):
            print('%-14s -> %-14s %-52s %d' % (source, target, rel, count))
        return 0

    violations = []
    for (source, target, rel), count in sorted(edges.items()):
        if (source, target) in forbid and (source, target, rel) not in allow_files:
            violations.append((source, target, rel, count))

    if violations:
        print('层间引用违规（%d 处）：' % len(violations), file=sys.stderr)
        for source, target, rel, count in violations:
            print('  %s -> %s  %s (%d)' % (source, target, rel, count), file=sys.stderr)
        print('若这是有意的设计边，请加入 scripts/layer-policy.json 的 allow_files；'
              '否则说明分层正在退化。', file=sys.stderr)
        return 1
    print('层间引用检查通过：%d 条边无违规' % len(edges))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
