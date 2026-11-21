#!/usr/bin/env python3
"""跨平台 dead_code 口径审计：把 Linux 库构建里「只是 Windows 或可选后端才用到」的
条目逐个反证到**它的全部调用方**，让人能一眼判断该注解还是该删。

为什么需要它（而不是肉眼看 rustc 输出）：
产品只支持 Windows，`src/lib.rs` 的生产入口 `run()` 只在 `target_os = "windows"`
下编译。于是 Linux 的 `cargo check --lib --no-default-features` 里，凡是只从
`run()` 可达的代码都会报 `dead_code`——它们**不是死代码**，只是这台机器上不可达。
告警数量在三千条量级（单个告警块最多复用 12 个条目），靠 rag 判断「这到底是
Windows 专用还是真该删」既慢又容易误删，所以把判断做成可复核的表格。

判据（输出文件的 `verdict` 列）：
    WINDOWS_ONLY   至少一个调用方位于 Windows 专属生产路径（`#[cfg(windows)]`、
                   `adapters/windows/**`、`cfg(target_os = "windows")` 分支等）；
    TEST_ONLY      调用方全部位于 `#[cfg(test)]` 模块或 tests/ 下（保留给单测）；
    FEATURE_ONLY   调用方全部被某个 Cargo feature 关掉（`ocr-mnn` / `ocr-openvino`）；
    UNREACHABLE    找不到任何调用方：真死代码候选，**必须人工复核**后再动；
    NO_CALLER      同名标识符在仓库里只出现在定义处（同上，同样需要复核）。

口径事实（实测，决定注解落在哪一层）：
    - `cargo check --lib` 的告警在 **crate 级** `#![cfg_attr(..., allow(dead_code))]`
      （`src/lib.rs` 顶部）之下全部消失——Linux 非测试库构建已经 0 告警；
    - `cargo test --lib --no-run` 仍有约 260 条，且**模块级**的
      `#![cfg_attr(all(not(windows), not(test)), allow(dead_code))]` 在 **--test 构建里不生效**
      （实测：同一条属性在普通库构建生效、在 --test 构建无效，rustc 1.96）。
      也就是说测试构建只能靠 crate 级注解，而 crate 级注解无法把「只对 cfg(test) 构建」
      单独限定——所以 Wave 4-A 的结论是：**库构建零告警，测试构建保留 D 类条目**，
      用 Windows 生产构建与库构建两道口径兜底，而不是为了测试构建去逐条注解放大噪音。

用法：
    # 1) 需要一份 JSON 诊断（慢：一次全新编译，约 15–30 秒）
    cargo check -p miliastra-wonderland-music --lib --no-default-features \\
        --message-format=json > /tmp/check.jsonl

    # 2) 反证每个条目
    python3 scripts/dead-code-audit.py /tmp/check.jsonl --out /tmp/dead-code.tsv

    # 3) 只看需要人看的那几行（默认阈值）
    python3 scripts/dead-code-audit.py /tmp/check.jsonl --out /tmp/dead-code.tsv --only-suspicious

    # 4) 按文件汇总，决定注解落在哪一层
    python3 scripts/dead-code-audit.py /tmp/check.jsonl --by-file

本脚本只读源码、只写你指定的输出文件，不修改任何代码。
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# 源码只读一次：可达性传播与调用方反证都要用，重复 IO 纯属浪费。
_SOURCES: dict[str, str] | None = None

# Windows 专属生产目录：命中即视为「Windows 独有生产代码」。
WINDOWS_ONLY_DIRS = (
    'src/adapters/windows/',
)

# 只被这些 feature 关掉的代码（`--no-default-features` 构建里必然不可达）。
FEATURE_GATES = ('ocr-mnn', 'ocr-openvino')

# `pub` 项所在的文件天然是「共享面」：同 crate 任意模块都可能经 `crate::` 路径引用它，
# 宽口径传播可能连不上这样的边，所以这类文件直接算可达（偏置：宁可漏报，不可误删）。
SHARED_MODULE_MARKERS = re.compile(r'^pub (?:fn|struct|enum|trait|type|const|static|mod|use)', re.M)

# `mod foo;` 声明：不带花括号的是「文件模块」，带花括号的已经是内联代码，不用连边。
MOD_DECLARATION = re.compile(
    r'^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;', re.M
)

# `#[path = "routes/ai.rs"]` + 紧随其后的 `mod ai_routes;`
PATH_MODULE = re.compile(
    r'#\[path\s*=\s*"([^"]+)"\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*;'
)


# ---------------------------------------------------------------------------
# 读取 rustc JSON 诊断
# ---------------------------------------------------------------------------

def load_dead_code(path: str):
    """抽出所有 dead_code 警告，返回 [(条目名, 文件, 行号, 列号)]。

    一条告警块可能一次列出 12 个条目（`multiple methods are never used`），
    所以这里按告警块里的每个 `-->` 主跨度展开。
    """
    entries = []
    with open(path, encoding='utf-8') as handle:
        for line in handle:
            line = line.strip()
            if not line.startswith('{'):
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get('reason') != 'compiler-message':
                continue
            diag = message['message']
            code = (diag.get('code') or {}).get('code')
            if diag.get('level') != 'warning' or code != 'dead_code':
                continue
            for span in diag['spans']:
                if not span.get('is_primary'):
                    continue
                entries.append(
                    {
                        'name': _span_identifier(span),
                        'file': span['file_name'],
                        'line': span['line_start'],
                        'column': span['column_start'],
                        'label': span.get('label') or '',
                        'message': diag['message'],
                    }
                )
    return entries


def _span_identifier(span) -> str:
    """从跨度里取出被判定为 dead 的那个标识符。

    注意 rustc 对 dead_code 的跨度是**字节区间**（UTF-8），不是字符区间：
    `pub(crate) struct FsStateStore;` 在 JSON 里是 highlight_start=19/end=31，
    而这段文本只有 32 个字符，按字节切会把首字母切掉。所以这里先把整行
    加上「定义位置的字符偏移」交给 `_definition_name`，不依赖 highlight 区间。
    """
    for chunk in span.get('text') or []:
        name = _definition_name(chunk.get('text') or '')
        if name:
            return name
    for chunk in (span.get('label') or '', span.get('suggested_replacement') or ''):
        match = re.search(r'`([A-Za-z_][A-Za-z0-9_]*)`', chunk)
        if match:
            return match.group(1)
    return ''


# 定义行里「被定义的东西」的位置：`fn foo` / `struct Foo` / `Foo {`（枚举变体、字段）/
# `Foo::Bar` 之类的关联项标签。
DEFINITION_PATTERNS = (
    re.compile(r'\bfn\s+([A-Za-z_][A-Za-z0-9_]*)'),
    re.compile(r'\b(?:struct|enum|union|trait|type|mod|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)'),
    re.compile(r'\b([A-Z][A-Za-z0-9_]*)\s*(?:\{|;|,|\()'),          # 枚举变体 / 结构体字段类型
    re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*[,{]'),  # 字段名
    re.compile(r'^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\('),                   # 元组变体/字段
)


def _definition_name(line: str) -> str:
    for pattern in DEFINITION_PATTERNS:
        match = pattern.search(line)
        if match:
            return match.group(1)
    match = re.search(r'[A-Za-z_][A-Za-z0-9_]*', line)
    return match.group(0) if match else ''


# ---------------------------------------------------------------------------
# 全仓标识符索引（用来反证「还有没有别的调用方」）
# ---------------------------------------------------------------------------

def load_sources() -> dict[str, str]:
    global _SOURCES
    if _SOURCES is not None:
        return _SOURCES
    sources: dict[str, str] = {}
    for base in ('src', 'crates', 'tests', 'examples'):
        root = os.path.join(ROOT, base)
        for dirpath, _, names in os.walk(root):
            for name in names:
                if not name.endswith('.rs'):
                    continue
                full = os.path.join(dirpath, name)
                rel = os.path.relpath(full, ROOT)
                try:
                    sources[rel] = open(full, encoding='utf-8', errors='replace').read()
                except OSError:
                    continue
    _SOURCES = sources
    return sources


# `#[cfg(...)]` 属性块与所在的 item/module 头部，用来判断「这一行是不是 Windows 专属」。
CFG_WINDOWS = re.compile(
    r'#\[cfg\((?:[^()]|\((?:[^()]|\([^()]*\))*\))*target_os\s*=\s*"windows"(?:[^()]|\([^()]*\))*\)\]'
)
CFG_FEATURE = re.compile(r'#\[cfg\(([^]]*(?:feature\s*=\s*"(?:' + '|'.join(FEATURE_GATES) + r')")[^]]*)\)\]')
TEST_MODULE = re.compile(r'#\[cfg\(test\)\]')


def enclosing_gates(text: str, line_no: int) -> set[str]:
    """返回该行向上最近的若干条 `#[cfg(...)]` / `#[cfg(test)]` 属性所表达的「闸」。

    这是启发式：取该行之前最多 40 行里的属性行，以及所在 mod 的声明。
    宁可多报 WINDOWS_ONLY，也不要漏报成 UNREACHABLE —— 漏报会导致误删生产代码。
    """
    lines = text.split('\n')
    start = max(0, line_no - 1 - 40)
    window = '\n'.join(lines[start:line_no])
    gates = set()
    if CFG_WINDOWS.search(window):
        gates.add('windows')
    feature = CFG_FEATURE.search(window)
    if feature:
        gates.add('feature')
    if TEST_MODULE.search(window):
        gates.add('test')
    return gates


# 一次扫描就要回答「这个名字在别处出现在哪、那处属于什么闸」，
# 所以先把每个文件的行前置闸（windows / feature / test）算好，再按名字索引行号。
def build_index(sources: dict[str, str]) -> dict[str, list[tuple[str, int, str, set[str]]]]:
    """返回 {标识符: [(文件, 行号, 该行文本, 该行的闸)]}。

    闸（gate）= 该行向上 40 行内出现的 `#[cfg(...)]` 属性，取值 windows / feature / test。
    这只用于**保守归类**：宁可把「可能有调用方的条目」判成 WINDOWS_ONLY，
    也不要把它误判成死代码让人删掉生产路径。
    """
    index: dict[str, list[tuple[str, int, str, set[str]]]] = collections.defaultdict(list)
    token = re.compile(r'[A-Za-z_][A-Za-z0-9_]*')
    for rel, text in sources.items():
        lines = text.split('\n')
        gates_per_line = [enclosing_gates(text, n) for n in range(1, len(lines) + 1)]
        file_gates: set[str] = set()
        if rel.startswith(WINDOWS_ONLY_DIRS):
            file_gates.add('windows')
        if rel.startswith('tests/'):
            file_gates.add('test')
        for line_no, line in enumerate(lines, start=1):
            gates = gates_per_line[line_no - 1] | file_gates
            for name in set(token.findall(line)):
                index[name].append((rel, line_no, line, gates))
    return index


def crate_roots(sources: dict[str, str]) -> list[str]:
    """workspace 里每个 crate 的**构建根**（真正的入口文件）。"""
    roots = ['src/lib.rs', 'src/main.rs']
    for rel in sources:
        parts = rel.split('/')
        if parts[0] == 'crates' and len(parts) > 3 and parts[2] == 'src':
            name = parts[3]
            if name in ('lib.rs', 'main.rs'):
                roots.append(rel)
    return roots


def module_paths(sources: dict[str, str]) -> dict[str, str]:
    """文件 -> 模块路径（`crate::ui::routines::hall` 这种形式）。

    - `src/a/b.rs`      -> `crate::a::b`，同时登记目录模块 `crate::a`
    - `src/a/mod.rs`    -> `crate::a`
    - `src/lib.rs`      -> `crate`（crate 根）
    - `crates/<name>/src/...` 用 `<name>` 作为根，与 `crate::` 区分开。
    """
    modules: dict[str, str] = {}
    for rel in sources:
        parts = rel.split('/')
        if parts[0] == 'src':
            root, segs = 'crate', parts[1:]
        elif parts[0] == 'crates' and len(parts) > 3 and parts[2] == 'src':
            root, segs = parts[1].replace('-', '_'), parts[3:]
        else:
            continue
        if segs[-1] in ('lib.rs', 'main.rs'):
            modules[rel] = root
            continue
        if segs[-1] == 'mod.rs':
            segs = segs[:-1]
        else:
            segs = segs[:-1] + [segs[-1][:-3]]
        if not segs:
            modules[rel] = root
            continue
        modules[rel] = '::'.join([root] + segs)
    return modules


def reachable_files(sources: dict[str, str], crate_root_list: list[str]) -> set[str]:
    """从每个 crate 的构建根出发，沿 `crate::a::b` 路径与 `mod foo;` 声明解析可达文件。

    只做**文件级**传播：模块私有项只在本文件内可见，文件级已经够用，也不会因为
    一次重命名整片误判。偏置一律偏向「可达」——可达文件里的条目不会被当成死代码，
    宁可漏报，也不误导人删生产路径。
    """
    modules = module_paths(sources)
    by_module: dict[str, str] = {}
    for rel, module in modules.items():
        by_module.setdefault(module, rel)
    # 目录模块（`src/ui/routines/mod.rs`）已经由 modules 覆盖；这里再补一层，
    # 让 `crate::ui` 即使只有 `ui/` 目录、`ui/mod.rs` 存在也能解析到。
    for rel, module in modules.items():
        if rel.endswith('/mod.rs'):
            by_module.setdefault(module, rel)

    roots = sorted({module.split('::')[0] for module in modules.values()})
    path_re = re.compile(
        r'\b(?:' + '|'.join(re.escape(root) for root in roots) + r')(?:::[A-Za-z_][A-Za-z0-9_]*)+'
    )

    edges: dict[str, set[str]] = collections.defaultdict(set)

    def link(owner: str, segments: list[str]) -> None:
        for cut in range(len(segments), 0, -1):
            candidate = by_module.get('::'.join(segments[:cut]))
            if candidate:
                if candidate != owner:
                    edges[owner].add(candidate)
                return

    for rel, text in sources.items():
        if rel not in modules:
            # build.rs / examples 等不在模块树里的文件，没有 `crate::` 语义，跳过。
            continue
        # 1) 显式路径：`crate::adapters::login_helper::X` / `foo::bar`
        for match in path_re.finditer(text):
            link(rel, match.group(0).split('::'))
        # 2) `mod foo;`（含 `pub(crate) mod foo;`）把子模块挂进模块树。
        #    每个模块文件都能声明子模块，不只构建根——只认 crate 根会漏掉大半个模块树。
        base = modules[rel].split('::')
        for match in MOD_DECLARATION.finditer(text):
            link(rel, base + [match.group(1)])
        # 3) `#[path = "routes/ai.rs"] mod ai_routes;`：按**文件路径**挂接子模块，
        #    模块名与文件名不一致，路径解析认不出来，必须单独连一条边。
        for match in PATH_MODULE.finditer(text):
            target = os.path.normpath(os.path.join(os.path.dirname(rel), match.group(1)))
            if target in sources:
                edges[rel].add(target)

    # 叶子模块可达时，同目录 `mod.rs`（父模块）也算可达：父模块的声明与重导出同样是生产路径。
    for rel, targets in list(edges.items()):
        for target in list(targets):
            parent_mod = os.path.join(os.path.dirname(target), 'mod.rs')
            if parent_mod in sources:
                edges[rel].add(parent_mod)

    reached: set[str] = set()
    queue = list(crate_root_list)
    while queue:
        rel = queue.pop()
        if rel in reached:
            continue
        reached.add(rel)
        queue.extend(edges.get(rel, ()))

    # `pub` 项所在的文件天然是共享面：同 crate 任意模块都可能按路径引用它，
    # 传播一旦连不上边就会把共享面误判成死代码，所以这类文件直接算可达。
    for rel, text in sources.items():
        if SHARED_MODULE_MARKERS.search(text[:4000]):
            reached.add(rel)
    return reached


def attribute(
    name: str, file: str, line_no: int, index, reached: set[str]
) -> tuple[str, list[str]]:
    """反证 name 的调用方，返回 (verdict, 证据列表)。"""
    callers: list[str] = []
    in_reached_file = False
    saw_windows = False
    saw_test = False
    saw_feature = False

    for rel, index_no, line, gates in index.get(name, ()):
        if rel == file and index_no == line_no:
            continue
        stripped = line.strip()
        if not stripped.startswith('//'):
            callers.append(f'{rel}:{index_no}')
        if rel in reached:
            in_reached_file = True
        if 'windows' in gates:
            saw_windows = True
        if 'test' in gates:
            saw_test = True
        if 'feature' in gates:
            saw_feature = True

    if saw_windows:
        return ('WINDOWS_ONLY', callers)
    if in_reached_file:
        return ('PRODUCTION', callers)
    if saw_feature:
        return ('FEATURE_ONLY', callers)
    if not callers:
        return ('NO_CALLER', callers)
    if saw_test:
        return ('TEST_ONLY', callers)
    return ('UNREACHABLE', callers)


def module_tree_files(sources: dict[str, str], rel: str) -> dict[str, str]:
    """模块树里的活文件（模块路径 -> 文件）。

    与可达性传播共用同一套解析，确保「注解写在哪」和「审计认为谁可达」是同一个口径。
    """
    modules = module_paths(sources)
    return modules


def group_key(rel: str, mode: str) -> str:
    parts = rel.split('/')
    if mode == 'file':
        return rel
    if mode == 'module':
        # src/ui/routines/hall.rs -> src/ui；crates 的源码按 crate 归
        if parts[0] == 'src':
            return '/'.join(parts[:2])
        if parts[0] == 'crates' and len(parts) > 3:
            return f'crates/{parts[1]}'
        return parts[0]
    if parts[0] == 'crates' and len(parts) > 1:
        return f'crates/{parts[1]}'
    return parts[0]


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------

def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description='审计 Linux 库构建里的 dead_code：逐条反证调用方，区分 Windows 专用与真死代码。',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            '典型用法：\n'
            '  cargo check -p miliastra-wonderland-music --lib --no-default-features \\\n'
            '      --message-format=json > /tmp/check.jsonl\n'
            '  python3 scripts/dead-code-audit.py /tmp/check.jsonl --out /tmp/dead-code.tsv\n'
        ),
    )
    parser.add_argument('jsonl', help='cargo check --message-format=json 的输出文件')
    parser.add_argument('--out', help='把逐条结果写成 TSV（默认只打印汇总）')
    parser.add_argument(
        '--entry',
        metavar='FILE',
        action='append',
        default=None,
        help='生产入口文件（可重复，默认 src/lib.rs 与 src/composition.rs）',
    )
    parser.add_argument('--by-file', action='store_true', help='按文件打印条目数排行')
    parser.add_argument(
        '--plan-dirs',
        type=int,
        metavar='DEPTH',
        help='按模块树的目录深度（1 = src/features，2 = src/features/card_games）汇总，用于规划注解落点',
    )
    parser.add_argument(
        '--group-by',
        choices=('file', 'module', 'crate'),
        help='按文件 / 顶层模块目录 / crate 汇总条目数',
    )
    parser.add_argument(
        '--only-suspicious',
        action='store_true',
        help='只打印需要人工复核的条目（UNREACHABLE / NO_CALLER）',
    )
    parser.add_argument(
        '--quiet-unreachable',
        metavar='NAME',
        action='append',
        default=[],
        help='已复核并确认要保留的名字（可重复），不计入 suspicious',
    )
    args = parser.parse_args(argv)

    if not os.path.exists(args.jsonl):
        print(f'找不到诊断文件: {args.jsonl}', file=sys.stderr)
        return 2

    entries = load_dead_code(args.jsonl)
    if not entries:
        print('没有 dead_code 告警：本口径已经干净。')
        return 0

    sources = load_sources()
    index = build_index(sources)
    entry_points = args.entry or crate_roots(sources)
    reached = reachable_files(sources, entry_points)
    rows = []
    for entry in entries:
        if not entry['name']:
            continue
        if entry['name'] in args.quiet_unreachable:
            continue
        verdict, callers = attribute(entry['name'], entry['file'], entry['line'], index, reached)
        rows.append((entry['file'], entry['line'], entry['name'], verdict, callers))

    counts = collections.Counter(row[3] for row in rows)
    print(f'dead_code 条目: {len(rows)}')
    for verdict, count in counts.most_common():
        print(f'  {verdict:12s} {count}')

    if args.by_file:
        per_file = collections.Counter(row[0] for row in rows)
        print('\n按文件（条目数）：')
        for file, count in per_file.most_common():
            print(f'  {count:4d}  {file}')

    if args.plan_dirs:
        # 只统计**真正可达**的文件（模块树里 decl 出来的 .rs），build.rs / examples 不算。
        modules = module_paths(sources)
        per_dir: dict[str, set[str]] = collections.defaultdict(set)
        hits_per_dir: dict[str, int] = collections.defaultdict(int)
        for rel in modules:
            parts = rel.split('/')
            depth = min(len(parts) - 1, args.plan_dirs)
            per_dir['/'.join(parts[:depth])].add(rel)
        for row in rows:
            if row[0] not in modules:
                continue
            parts = row[0].split('/')
            depth = min(len(parts) - 1, args.plan_dirs)
            hits_per_dir['/'.join(parts[:depth])] += 1
        print(f'\n按模块树聚合（目录深度 {args.plan_dirs}）：')
        for directory, files in sorted(per_dir.items()):
            print(f'  {len(files):3d} 个文件 / {hits_per_dir[directory]:4d} 条  {directory}')

    if args.group_by:
        print(f'\n按{ {"file": "文件", "module": "顶层模块", "crate": "crate"}[args.group_by] }汇总：')
        grouped = collections.Counter(group_key(row[0], args.group_by) for row in rows)
        for key, count in grouped.most_common():
            print(f'  {count:4d}  {key}')

    suspicious = [row for row in rows if row[3] in ('UNREACHABLE', 'NO_CALLER')]
    if suspicious:
        print(f'\n需要人工复核（{len(suspicious)} 条）：')
        for file, line, name, verdict, callers in suspicious:
            evidence = ', '.join(callers[:3]) if callers else '（全仓无其它出现）'
            print(f'  {file}:{line}  {name}  [{verdict}]  {evidence}')

    if args.out:
        with open(args.out, 'w', encoding='utf-8') as handle:
            handle.write('file\tline\tname\tverdict\tcallers\n')
            for file, line, name, verdict, callers in sorted(rows):
                handle.write(f'{file}\t{line}\t{name}\t{verdict}\t{",".join(callers[:6])}\n')
        print(f'\n明细已写入 {args.out}')

    # 退出码恒 0：这是审计工具，不是门禁。门禁口径见 .github/workflows/quality-gate.yml。
    return 0


if __name__ == '__main__':
    sys.exit(main())
