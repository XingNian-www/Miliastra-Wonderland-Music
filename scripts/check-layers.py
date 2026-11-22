#!/usr/bin/env python3
"""分层方向检查：只允许上层依赖下层。

为什么单独放在 scripts/ 而不是复用 technical-docs/code-map.py：
technical-docs/ 整个目录被 .gitignore 忽略（.gitignore:19），不能作为 CI 闸门。

规则（scripts/layer-policy.json）：
- forbid: 明确禁止的层间边，出现即失败
- allow_files: 对少数既有设计边，按**文件白名单**放行；新增文件不自动豁免

检查范围（历史漏检已修，见每条注释）：
- 跳过 `*_tests.rs`、`tests.rs`、以及 `src/**/tests/` 目录（测试代码不算生产边）；
- `lib.rs` / `main.rs` 不再整文件跳过：只剥掉模块声明行与 `#[cfg(test)]` 块，
  其余 `use` 正常参与检查；
- `use super::super::...` / `super::...` 按 `super::` 层数还原真实目标模块，
  不再只认 `crate::` 第一段。

用法：
    python3 scripts/check-layers.py           # 检查，违规退出码 1
    python3 scripts/check-layers.py --list    # 打印当前全部层间边
    python3 scripts/check-layers.py --self-test  # 三类历史漏检的最小复现用例
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

# `lib.rs` / `main.rs` 是 crate 根，不能因为「根级叶子」整文件跳过：
# 它们内部的 `use` 也是真实的依赖边。只剥离模块声明行（`mod x;`）与测试块。
ROOT_ENTRY_FILES = {'lib', 'main'}


def normalize_module(name):
    if name.startswith('debug'):
        return 'debug'
    if name in ('lib', 'main', 'privacy', 'text', 'test_support'):
        return 'root'
    return name


USE_RE = re.compile(r'(?<!\w)crate::([A-Za-z_]\w*)')
# `super::super::x` / `super::super::*`：按层数还原到真实模块。
# 旧实现只匹配 `crate::`，形如 `super::super::AppConfig` 的跨层引用完全不可见。
SUPER_RE = re.compile(r'(?<![\w:])((?:super::)+)(\*|[A-Za-z_]\w*)')


# `#[cfg(test)]` 与 `#[cfg(all(test, ...))]` / `#[cfg(any(test, ...))]`；
# 刻意不匹配 `#[cfg(not(test))]`（那是生产分支）。
TEST_ATTR = (r'#\[cfg\((?:test'
             r'|all\(\s*[^)\n]*\btest\b[^)\n]*\)'
             r'|any\(\s*[^)\n]*\btest\b[^)\n]*\))\)\]')
TEST_MOD_DECL = re.compile(r'(?m)^[ \t]*' + TEST_ATTR + r'\s*\n\s*mod\s+\w+\s*;')
TEST_MOD_BLOCK = re.compile(r'(?m)^[ \t]*' + TEST_ATTR + r'\s*\n\s*mod\s+\w+\s*\{')
# `mod x;` / `pub mod x;` / `pub(crate) mod x;`（lib.rs/main.rs 的声明区）。
MODULE_DECL_RE = re.compile(
    r'(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_]\w*\s*;')
# crate 根文件只统计**真正的 use 声明**：它们顶部的文档注释也会写
# `crate::debug_mode` 之类的路径（见 lib.rs），那是说明文字不是依赖边。
USE_STMT_RE = re.compile(
    r'(?ms)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\b[^;]*;')


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
        #
        # 必须识别**字符字面量**（`'x'` / `'"'` / `'\\''`）：测试里出现 `find('"')`
        # 这类写法时，若只处理双引号字符串，那个单引号包住的引号会被当成字符串起点，
        # 后续扫描整体错位，整个 `#[cfg(test)]` 块剥不干净，测试代码里的
        # `crate::ui::…` 就被误算成生产层的层间依赖（曾因此让边数从 240 变 241）。
        depth = 0
        j = marker.end() - 1
        prev_sig = ''  # 上一个非空白字符，用于区分「字符字面量」与「生命周期标注」
        while j < len(text):
            ch = text[j]
            if ch == '/' and j + 1 < len(text) and text[j + 1] == '/':
                nl = text.find('\n', j)
                j = len(text) if nl < 0 else nl + 1
                continue
            if ch == '"':
                j += 1
                while j < len(text) and text[j] != '"':
                    j += 2 if text[j] == '\\' else 1
                j += 1
                prev_sig = '"'
                continue
            if ch == "'":
                # 字符字面量：''' / '"' / 'a' / '\\n' 形态；生命周期 'a 与标签 'outer 不匹配。
                nxt = text[j + 1] if j + 1 < len(text) else ''
                if nxt == '\\':
                    k = j + 3
                    while k < len(text) and text[k] != "'":
                        k += 1
                    j = k + 1
                    prev_sig = "'"
                    continue
                if j + 2 < len(text) and text[j + 2] == "'":
                    j += 3
                    prev_sig = "'"
                    continue
                # 否则是生命周期标注，按普通字符跳过。
            if ch == '{':
                depth += 1
            elif ch == '}':
                depth -= 1
                if depth == 0:
                    j += 1
                    break
            if not ch.isspace():
                prev_sig = ch
            j += 1
        i = j
    return "".join(out)


def strip_module_declarations(text):
    """删除 `mod x;` 声明行（对 crate 根的「模块声明区」限定）。"""
    return MODULE_DECL_RE.sub('', text)


def only_use_statements(text):
    """只保留 `use` 声明（含跨行的 `use ... { ... };`）。

    仅用于 lib.rs/main.rs：这两个文件不再整文件跳过，但也只应统计真正的
    `use` 声明；文件顶部的文档注释里出现 `crate::...` 时不能算层间边。
    """
    return '\n'.join(USE_STMT_RE.findall(text))


def is_excluded_source(rel):
    """测试代码不算生产边：`*_tests.rs`、`tests.rs` 与 `src/**/tests/` 目录。"""
    parts = rel.split('/')
    if 'tests' in parts[:-1]:
        return True
    name = parts[-1]
    return name.endswith('_tests.rs') or name == 'tests.rs'


def module_path(rel):
    """`src/a/b.rs` → ['a', 'b']；`src/a/b/mod.rs` → ['a', 'b']。

    `super::` 的层数必须相对本文件所属模块计算，因此需要这个映射。
    """
    parts = rel.split('/')[1:]
    if parts[-1] == 'mod.rs':
        parts = parts[:-1]
    else:
        parts[-1] = parts[-1][:-3]
    return parts


def super_target(module, levels, leaf):
    """把 `super::`（可连续多次）还原成真实目标模块的顶层名。

    `super` 向上弹一层；弹到 crate 根后，路径里剩下的第一段本身就是顶层模块，
    必须把 `leaf` 也算进去，否则 `super::super::interfaces::X` 会被误判成根：

    - `src/ui/routines/friend_delivery/navigation.rs` 里 `super::super::residency`
      → `ui::routines::residency` → `ui`；
    - `src/features/ai_chat/reply.rs` 里 `super::super::super::interfaces::X`
      → `crate::interfaces::X` → `interfaces`；
    - `super::super::*` 弹到 crate 根且没有具体下一段 → `root`。
    """
    if levels >= len(module):
        return 'root' if leaf == '*' else normalize_module(leaf)
    prefix = module[:len(module) - levels]
    return normalize_module(prefix[0])


def walk():
    for dirpath, dirnames, filenames in os.walk(SRC):
        # 不进入 src/**/tests/：该目录只由 #[cfg(test)] 模块引用。
        dirnames[:] = sorted(d for d in dirnames if d not in {'target', 'tests'})
        for name in sorted(filenames):
            if not name.endswith('.rs'):
                continue
            if is_excluded_source(name):
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, ROOT).replace(os.sep, '/')
            if is_excluded_source(rel):
                continue
            yield rel


def collect():
    edges = {}
    for rel in walk():
        layer = layer_of(rel)
        if layer in ROOT_LEAVES and layer not in ROOT_ENTRY_FILES | {'composition'}:
            continue
        with open(os.path.join(ROOT, rel), encoding='utf-8', errors='replace') as handle:
            text = strip_test_code(handle.read())
        if layer in ROOT_ENTRY_FILES:
            text = only_use_statements(strip_module_declarations(text))
        module = module_path(rel)
        for match in USE_RE.finditer(text):
            target = normalize_module(match.group(1))
            _record(edges, layer, target, rel)
        for match in SUPER_RE.finditer(text):
            levels = match.group(1).count('super::')
            target = super_target(module, levels, match.group(2))
            _record(edges, layer, target, rel)
    return edges


def _record(edges, layer, target, rel):
    if target == layer:
        return
    edges.setdefault((layer, target, rel), 0)
    edges[(layer, target, rel)] += 1


def self_test():
    """三类历史漏检的最小复现用例（不依赖真实 src/ 的布局）。"""
    cases = []

    def check(name, actual, expected):
        cases.append((name, actual == expected, actual, expected))

    # 1. 路径过滤：src/**/tests/ 目录、*_tests.rs、tests.rs 都必须跳过。
    check('tests 目录被排除',
          is_excluded_source('src/features/card_games/tests/rules.rs'), True)
    check('*_tests.rs 被排除',
          is_excluded_source('src/ui/routines/friend_delivery/tests.rs'), True)
    check('生产文件不被排除',
          is_excluded_source('src/features/card_games/rules.rs'), False)

    # 2. lib.rs/main.rs：测试块可剥，生产 use 保留，模块声明行可剥。
    entry = (
        'mod inner;\n'
        '#[cfg(test)]\n'
        'mod tests;\n'
        'use crate::ui::overlay;\n'
    )
    stripped = strip_module_declarations(strip_test_code(entry))
    check('lib.rs 测试声明被剥掉', 'mod tests;' not in stripped, True)
    check('lib.rs 生产 use 保留', 'use crate::ui::overlay;' in stripped, True)
    check('模块声明行被剥掉', 'mod inner;' not in stripped, True)

    documented = (
        '// 说明文字：同层引用走 `crate::debug_mode`。\n'
        'use crate::ui::overlay;\n'
    )
    scanned = only_use_statements(documented)
    check('lib.rs 文档注释里的 crate:: 不计边',
          'crate::debug_mode' not in scanned, True)
    check('lib.rs 真正的 use 仍被统计',
          'crate::ui::overlay' in scanned, True)

    block = (
        'fn before() {}\n'
        '#[cfg(all(test, target_os = "windows"))]\n'
        'mod tests {\n'
        '    use crate::features::ai_chat::service;\n'
        '}\n'
        'use crate::debug::sandbox;\n'
    )
    stripped = strip_test_code(block)
    check('cfg(all(test,..)) 内联块被剥掉',
          'crate::features::ai_chat::service' not in stripped, True)
    check('测试块之后的生产代码保留',
          'crate::debug::sandbox' in stripped, True)

    # 3. super:: 层数还原。
    nav = module_path('src/ui/routines/friend_delivery/navigation.rs')
    check('super::super 从 4 层模块还原到 ui',
          super_target(nav, 2, 'residency'), 'ui')
    reply = module_path('src/features/ai_chat/reply.rs')
    check('super::super::super 弹到根后取 leaf 顶层模块',
          super_target(reply, 3, 'interfaces'), 'interfaces')
    shallow = module_path('src/features/real.rs')
    check('super::super 从 2 层模块弹到根后取 leaf',
          super_target(shallow, 2, 'interfaces'), 'interfaces')
    check('super::super::* 弹到根且无具体段 → root',
          super_target(shallow, 2, '*'), 'root')

    failed = 0
    for name, ok, actual, expected in cases:
        print('%s %s' % ('PASS' if ok else 'FAIL', name))
        if not ok:
            failed += 1
            print('     实际：%r 期望：%r' % (actual, expected))
    print('自测%s：%d/%d 通过' % ('通过' if not failed else '失败',
                                  len(cases) - failed, len(cases)))
    return 1 if failed else 0


def main(argv):
    if '--self-test' in argv[1:]:
        return self_test()

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
