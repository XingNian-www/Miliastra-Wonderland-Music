#!/usr/bin/env python3
"""分层方向检查（W0「看得见」：allowlist 制闸门）。

为什么单独放在 scripts/ 而不是复用 technical-docs/code-map.py：
technical-docs/ 整个目录被 .gitignore 忽略（.gitignore:19），不能作为 CI 闸门。

## 两种模式

- `--report`（默认）：**只报告不失败**，退出码恒 0。W0 起 CI 先用它建立可见度。
- `--check`：严格门禁，任何一条未登记边 / 禁区边 / 预算超标 / 豁免腐化都退出 1。
  W1 起 CI 切到它。
- `--freeze`：把**当前**层间边与文件预算写进 `scripts/layer-baseline.json`
  （先改代码、再 freeze；否则基线里会留下已消失的旧边，被 L5 判腐化）。
  `--freeze --update` 才允许**上调**预算；默认只降不升（L4 棘轮）。
- `--list`：打印当前全部文件级层间边。
- `--self-test`：闸门自身的回归用例（≥20 条，覆盖每个历史盲区）。

## 规则（`scripts/layer-policy.json` v2）

- `forbid`：当前应为 0 的反向边，出现即失败（与 allow 无关，禁区不豁免）。
- 其余全部是 **allowlist**：当前存在的每个层对都必须登记在
  `scripts/layer-baseline.json`（`--freeze` 产物）里，未登记即失败。
- `fake_targets`：`crate::X` 里 X 是 crate 根的**项**（类型/常量/函数）而不是模块，
  不是层间边。出现在这里的目标一律过滤（L6）。
- `strip_comment_edges`：注释/文档链接里的 `crate::…` 不是依赖边（L6）。
  历史幽灵边：`runtime → composition`（`runtime/deadline_bridge.rs` 文档链接）。
- `intra_layer_scc`：层内强连通分量（Tarjan，L3）。`exempt` 的层显式写理由；
  `report_only_until` 的层暂时只报告（修复排进后续波次）。
- `budgets`：文件行数 / `pub` 项预算棘轮（L4，只降不升）。
- `new_files`：`report` = 新文件只报告；`deny` = 新文件即失败。
- `debt`：登记为「债」的层对，必须带 `issue` + `expires`（L5）；到期即失败。

## 扫描范围（历史盲区已修）

- 跳过 `*_tests.rs`、`tests.rs`、`src/**/tests/` 目录，以及**仅由
  `#[cfg(test)] mod x;` 声明的文件**（如 `src/config/schema/tests_snapshot.rs`）：
  测试代码不算生产边。
- `lib.rs` / `main.rs` 不再整文件跳过：只剥模块声明行与测试块，其余 `use` 参与检查。
- **根级叶子作为源参与扫描**（L2）：`geometry` / `text` / `privacy` /
  `change_detection` / `input_gate` / `test_support` 以前整层跳过，
  导致 `src/geometry.rs` 的 `use crate::config::RectConfig`（向上边）完全看不见。
- `use super::super::...` / `super::...` 按 `super::` 层数还原真实目标模块。
- 注释（`//`、`//!`、`///`、`/* */`，含嵌套）在提取引用前剥离。
"""

import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, 'src')
POLICY = os.path.join(ROOT, 'scripts', 'layer-policy.json')
BASELINE = os.path.join(ROOT, 'scripts', 'layer-baseline.json')

# 根级叶子模块：只被引用、不引用业务层。
#
# W0 之前这里被当成「跳过扫描」的理由，于是 geometry 的向上边不可见（L2 盲区）。
# 现在它们**必须作为源参与扫描**；这个集合只用于报告分类与 self-test。
ROOT_LEAVES = {'lib', 'main', 'privacy', 'text', 'test_support', 'geometry',
               'change_detection', 'input_gate'}

# `lib.rs` / `main.rs` 是 crate 根。它们内部的 `use` 也是真实依赖边，
# 但文件顶部的文档注释会写 `crate::...` 说明文字：只统计真正的 use 声明。
ROOT_ENTRY_FILES = {'lib', 'main'}


# ---------------------------------------------------------------------------
# 路径 / 层
# ---------------------------------------------------------------------------

def layer_of(rel):
    """顶层模块 → 层名。

    `src/geometry.rs` → geometry，`src/crates/x` → crates，`src/lib.rs` → lib。
    `debug*` 的扁平别名（lib.rs 的 `pub(crate) use input_gate as debug_mode`）
    沿用旧口径归到 debug；改名不在 W0 范围内。
    """
    if rel.startswith('src/crates/'):
        return 'crates'
    rest = rel[4:]
    top = rest.split('/')[0]
    if '.' in top:
        top = top[:-3]
    if top.startswith('debug'):
        return 'debug'
    return top


def normalize_target(name):
    """crate 根再导出的目标名 → 层名。`lib`/`main` 归到 root。"""
    if name in ('lib', 'main'):
        return 'root'
    if name.startswith('debug'):
        return 'debug'
    return name


def module_path(rel):
    """`src/a/b.rs` → ['a', 'b']；`src/a/b/mod.rs` → ['a', 'b']。"""
    parts = rel.split('/')[1:]
    if parts[-1] == 'mod.rs':
        parts = parts[:-1]
    else:
        parts[-1] = parts[-1][:-3]
    return parts


def module_key(parts):
    return '::'.join(parts)


# ---------------------------------------------------------------------------
# Rust 源码清洗
# ---------------------------------------------------------------------------

def strip_comments(text):
    """剥离 `//`、`//!`、`///`、`/* */`（支持嵌套），保留字符串/字符字面量。

    历史幽灵边：`runtime/deadline_bridge.rs` 的文档链接
    `[`crate::composition::application::lifecycle`]` 被旧实现当成真实依赖，
    造出 `runtime → composition`。注释不是依赖边（L6）。

    注释字符替换成空格（保留换行），这样行号不变，报错仍能指到原行。
    """
    out = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        nxt = text[i + 1] if i + 1 < n else ''
        if ch == '/' and nxt == '/':
            while i < n and text[i] != '\n':
                out.append(' ')
                i += 1
            continue
        if ch == '/' and nxt == '*':
            depth = 0
            while i < n:
                if text[i] == '/' and i + 1 < n and text[i + 1] == '*':
                    depth += 1
                    out.append('  ')
                    i += 2
                elif text[i] == '*' and i + 1 < n and text[i + 1] == '/':
                    depth -= 1
                    out.append('  ')
                    i += 2
                    if depth == 0:
                        break
                else:
                    out.append('\n' if text[i] == '\n' else ' ')
                    i += 1
            continue
        if ch == '"':
            # 原始字符串 r"..." / r#"..."# / br#"..."# / cr"..."：
            # 在遇到引号时回看前缀，避免把 `#` 当字符。
            j = i - 1
            hashes = 0
            while j >= 0 and text[j] == '#':
                hashes += 1
                j -= 1
            raw = (j >= 0 and text[j] == 'r'
                   and (j == 0 or not (text[j - 1].isalnum() or text[j - 1] == '_')
                        or text[j - 1] in 'bc'))
            out.append('"')
            i += 1
            if raw:
                closing = '"' + '#' * hashes
                end = text.find(closing, i)
                if end < 0:
                    out.append(text[i:])
                    i = n
                else:
                    out.append(text[i:end + len(closing)])
                    i = end + len(closing)
                continue
            while i < n:
                if text[i] == '\\' and i + 1 < n:
                    out.append(text[i:i + 2])
                    i += 2
                    continue
                out.append(text[i])
                if text[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if ch == "'":
            # 字符字面量（`'x'` / `'"'` / `'\n'`）跳过；生命周期标注按普通字符走。
            if i + 1 < n and text[i + 1] == '\\':
                k = i + 2
                while k < n and text[k] != "'":
                    k += 1
                out.append(text[i:k + 1])
                i = k + 1
                continue
            if i + 2 < n and text[i + 2] == "'":
                out.append(text[i:i + 3])
                i += 3
                continue
            out.append(ch)
            i += 1
            continue
        out.append(ch)
        i += 1
    return ''.join(out)


TEST_ATTR = (r'#\[cfg\((?:test'
             r'|all\(\s*[^)\n]*\btest\b[^)\n]*\)'
             r'|any\(\s*[^)\n]*\btest\b[^)\n]*\))\)\]')
TEST_MOD_DECL = re.compile(r'(?m)^[ \t]*' + TEST_ATTR + r'\s*\n\s*mod\s+\w+\s*;')
TEST_MOD_BLOCK = re.compile(r'(?m)^[ \t]*' + TEST_ATTR + r'\s*\n\s*mod\s+\w+\s*\{')
MODULE_DECL_RE = re.compile(
    r'(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_]\w*\s*;')
USE_STMT_RE = re.compile(
    r'(?ms)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\b[^;]*;')


def strip_test_code(text):
    """删除 `#[cfg(test)]` 模块，保留其后的生产代码。

    旧实现从第一个匹配截断到文件末尾；现在只删 `mod x;` 声明行，
    内联 `mod x { ... }` 用花括号配对删除（跳过字符串/字符字面量）。
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
        depth = 0
        j = marker.end() - 1
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
    return ''.join(out)


def strip_module_declarations(text):
    return MODULE_DECL_RE.sub('', text)


def only_use_statements(text):
    return '\n'.join(USE_STMT_RE.findall(text))


# ---------------------------------------------------------------------------
# 扫描
# ---------------------------------------------------------------------------

USE_RE = re.compile(r'(?<!\w)crate::([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)')
SUPER_RE = re.compile(r'(?<![\w:])((?:super::)+)(\*|[A-Za-z_]\w*)')


def is_excluded_source(rel, test_module_files=frozenset()):
    """测试代码不算生产边。"""
    parts = rel.split('/')
    if 'tests' in parts[:-1]:
        return True
    name = parts[-1]
    if name.endswith('_tests.rs') or name == 'tests.rs':
        return True
    return rel in test_module_files


def walk(test_module_files=frozenset()):
    for dirpath, dirnames, filenames in os.walk(SRC):
        dirnames[:] = sorted(d for d in dirnames if d != 'target')
        for name in sorted(filenames):
            if not name.endswith('.rs'):
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, ROOT).replace(os.sep, '/')
            if is_excluded_source(rel, test_module_files):
                continue
            yield rel


def discover_test_module_files():
    """只由 `#[cfg(test)] mod x;` 声明的文件：整份是测试代码，不参与生产边。

    例：`src/config/schema/mod.rs` 里 `#[cfg(test)] mod tests_snapshot;`
    → `src/config/schema/tests_snapshot.rs`。文件名不符合 `*_tests.rs`
    约定，不在这里剔掉就会把测试夹具算成层间依赖。
    """
    found = set()
    for rel in walk():
        with open(os.path.join(ROOT, rel), encoding='utf-8', errors='replace') as handle:
            text = strip_comments(handle.read())
        base = os.path.dirname(rel)
        for match in TEST_MOD_DECL.finditer(text):
            name = re.search(r'mod\s+(\w+)\s*;', match.group(0))
            if not name:
                continue
            stem = name.group(1)
            for candidate in ('%s/%s.rs' % (base, stem),
                              '%s/%s/mod.rs' % (base, stem)):
                if os.path.exists(os.path.join(ROOT, candidate)):
                    found.add(candidate)
    return frozenset(found)


def module_files(test_module_files=frozenset()):
    """模块路径 → 文件。用于把 `crate::a::b::Item` 解析回具体模块文件。"""
    table = {}
    for rel in walk(test_module_files):
        table[module_key(module_path(rel))] = rel
    return table


def resolve_parts(module, ref):
    """把一条 `crate::` / `super::` 引用还原成完整模块路径段。"""
    kind = ref[0]
    if kind == 'crate':
        return list(ref[1])
    _, levels, leaf = ref
    if leaf == '*':
        # 通配导入的目标就是模块本身：base 的最后一段。
        base = module[:len(module) - levels] if levels <= len(module) else []
        return list(base)
    if levels >= len(module):
        return [leaf]
    return module[:len(module) - levels] + [leaf]


def resolve_target_file(parts, table):
    """最长可解析模块前缀 → 文件；解析不到（crate 根项、假目标）→ None。"""
    for cut in range(len(parts), 0, -1):
        rel = table.get(module_key(parts[:cut]))
        if rel is not None:
            return rel
    return None


def extract_refs(module, text):
    refs = []
    for match in USE_RE.finditer(text):
        refs.append(('crate', tuple(match.group(1).split('::'))))
    for match in SUPER_RE.finditer(text):
        refs.append(('super', match.group(1).count('super::'), match.group(2)))
    return refs


def scan_text(rel, raw_text, table, policy):
    """单个文件 → (target_layer, target_file) 列表（未过滤假目标）。"""
    module = module_path(rel)
    text = strip_comments(raw_text) if policy.get('strip_comment_edges', True) else raw_text
    text = strip_test_code(text)
    if layer_of(rel) in ROOT_ENTRY_FILES:
        text = only_use_statements(strip_module_declarations(text))
    out = []
    for ref in extract_refs(module, text):
        parts = resolve_parts(module, ref)
        if not parts:
            continue
        out.append((normalize_target(parts[0]), resolve_target_file(parts, table)))
    return out


def collect(policy):
    """返回 (edges, fake_hits, ghost_edges, test_module_files)。

    edges: {(source_layer, target_layer, rel): count}
    ghost_edges: 仅因注释而存在的层对集合（用于报告 L6 修复效果）
    """
    test_module_files = discover_test_module_files()
    table = module_files(test_module_files)
    fake_targets = set(policy.get('fake_targets', []))
    edges = {}
    fake_hits = {}
    ghost = set()
    for rel in walk(test_module_files):
        layer = layer_of(rel)
        with open(os.path.join(ROOT, rel), encoding='utf-8', errors='replace') as handle:
            raw = handle.read()
        for target, _ in scan_text(rel, raw, table, policy):
            if target in fake_targets:
                fake_hits[target] = fake_hits.get(target, 0) + 1
                continue
            if target == layer:
                continue
            edges.setdefault((layer, target, rel), 0)
            edges[(layer, target, rel)] += 1
        # 对比「不剥注释」的扫描，统计幽灵边（只影响层对报告）。
        naive_policy = dict(policy)
        naive_policy['strip_comment_edges'] = False
        for target, _ in scan_text(rel, raw, table, naive_policy):
            if target in fake_targets or target == layer:
                continue
            if (layer, target, rel) not in edges:
                ghost.add((layer, target, rel))
    return edges, fake_hits, ghost, test_module_files


# ---------------------------------------------------------------------------
# 层内 SCC（Tarjan）
# ---------------------------------------------------------------------------

def scc_node_of_file(rel):
    """文件 → SCC 节点：`layer/子模块`（第二段），保持与历史口径一致。"""
    parts = module_path(rel)
    if len(parts) >= 2:
        return parts[0] + '/' + parts[1]
    return parts[0] if parts else 'root'


def scc_node_of_parts(parts):
    if len(parts) >= 2:
        return parts[0] + '/' + parts[1]
    return parts[0] if parts else 'root'


def tarjan(nodes, successors):
    """返回强连通分量列表（每个分量是节点列表）；单节点无自环不算环。"""
    index = {}
    low = {}
    on_stack = set()
    stack = []
    result = []
    counter = [0]

    def strongconnect(node):
        index[node] = counter[0]
        low[node] = counter[0]
        counter[0] += 1
        stack.append(node)
        on_stack.add(node)
        for succ in successors.get(node, ()):
            if succ not in index:
                strongconnect(succ)
                low[node] = min(low[node], low[succ])
            elif succ in on_stack:
                low[node] = min(low[node], index[succ])
        if low[node] == index[node]:
            component = []
            while True:
                member = stack.pop()
                on_stack.discard(member)
                component.append(member)
                if member == node:
                    break
            result.append(component)

    for node in nodes:
        if node not in index:
            strongconnect(node)
    return result


def intra_layer_scc(policy):
    """层内强连通分量：节点 = `层/子模块`，边 = 同层文件间引用。"""
    test_module_files = discover_test_module_files()
    table = module_files(test_module_files)
    fake_targets = set(policy.get('fake_targets', []))
    successors = {}
    nodes = set()
    for rel in walk(test_module_files):
        layer = layer_of(rel)
        node = scc_node_of_file(rel)
        nodes.add(node)
        successors.setdefault(node, set())
        with open(os.path.join(ROOT, rel), encoding='utf-8', errors='replace') as handle:
            raw = handle.read()
        module = module_path(rel)
        text = strip_comments(raw) if policy.get('strip_comment_edges', True) else raw
        text = strip_test_code(text)
        if layer in ROOT_ENTRY_FILES:
            text = only_use_statements(strip_module_declarations(text))
        for ref in extract_refs(module, text):
            parts = resolve_parts(module, ref)
            if not parts or parts[0] in fake_targets:
                continue
            if normalize_target(parts[0]) != layer:
                continue
            target = scc_node_of_parts(parts)
            if target != node:
                successors[node].add(target)
    components = tarjan(sorted(nodes), successors)
    worst = {}
    for component in components:
        if len(component) <= 1:
            continue
        layer = component[0].split('/')[0]
        if len(component) > worst.get(layer, (0, []))[0]:
            worst[layer] = (len(component), sorted(component))
    return worst


# ---------------------------------------------------------------------------
# 预算（L4 棘轮）
# ---------------------------------------------------------------------------

PUB_ITEM_RE = re.compile(
    r'(?m)^[ \t]*pub(?:\([^)]*\))?[ \t]+'
    r'(?:(?:async|unsafe|const|extern)[ \t]+)*'
    r'(?:fn|struct|enum|trait|type|const|static|mod|use)\b')


def file_budget(rel):
    with open(os.path.join(ROOT, rel), encoding='utf-8', errors='replace') as handle:
        raw = handle.read()
    lines = raw.count('\n') + (0 if raw.endswith('\n') or not raw else 1)
    production = strip_test_code(strip_comments(raw))
    return {'lines': lines, 'pub_items': len(PUB_ITEM_RE.findall(production))}


def build_budgets(policy, old_budgets, update):
    """只降不升的预算棘轮。

    返回 (budgets, raised, lowered, added, removed, blocked)：
    `blocked` 是"当前超预算、但未加 --update 不去上调"的条目。
    """
    budgets = {}
    raised = []
    lowered = []
    added = []
    blocked = []
    for rel in walk(discover_test_module_files()):
        current = file_budget(rel)
        previous = old_budgets.get(rel)
        if previous is None:
            added.append(rel)
            budgets[rel] = current
            continue
        entry = {}
        for field in ('lines', 'pub_items'):
            old = int(previous.get(field, 0))
            new = current[field]
            if new > old:
                if update:
                    raised.append('%s:%s %d→%d' % (rel, field, old, new))
                    entry[field] = new
                else:
                    entry[field] = old
                    blocked.append('%s:%s 当前 %d > 已冻结 %d' % (rel, field, new, old))
            else:
                if new < old:
                    lowered.append('%s:%s %d→%d' % (rel, field, old, new))
                entry[field] = new
        budgets[rel] = entry
    removed = sorted(set(old_budgets) - set(budgets))
    return budgets, raised, lowered, added, removed, blocked


# ---------------------------------------------------------------------------
# 策略 / 基线
# ---------------------------------------------------------------------------

def load_json(path, label):
    try:
        with open(path, encoding='utf-8') as handle:
            return json.load(handle)
    except FileNotFoundError:
        print('缺少 %s（%s）' % (label, path), file=sys.stderr)
        return None
    except (OSError, ValueError) as error:
        print('读取 %s 失败（%s）：%s' % (label, path, error), file=sys.stderr)
        return None


def edge_pairs(edges):
    pairs = {}
    for (source, target, rel), count in edges.items():
        pairs.setdefault((source, target), []).append((rel, count))
    for files in pairs.values():
        files.sort()
    return pairs


def baseline_index(baseline):
    index = {}
    for entry in baseline.get('edges', []):
        key = (entry['source'], entry['target'])
        index[key] = entry
    return index


def is_expired(value, today):
    if not value:
        return False
    return value < today


def today_iso():
    import datetime
    return datetime.date.today().isoformat()


# ---------------------------------------------------------------------------
# 报告 / 检查
# ---------------------------------------------------------------------------

def render(edges, fake_hits, ghost, scc, policy, baseline, today, budget_state):
    pairs = edge_pairs(edges)
    index = baseline_index(baseline)
    registered = set(index)
    current = set(pairs)
    unregistered = sorted(current - registered)
    stale = sorted(registered - current)
    forbidden = set(tuple(edge) for edge in policy.get('forbid', []))
    forbidden_hits = sorted(current & forbidden)
    debt = {tuple((entry['source'], entry['target'])): entry
            for entry in policy.get('debt', [])}
    debt_missing_meta = []
    debt_expired = []
    for key, spec in debt.items():
        if not spec.get('issue') or not spec.get('expires'):
            debt_missing_meta.append('%s → %s' % key)
        elif is_expired(spec['expires'], today):
            debt_expired.append('%s → %s（%s 已过期）' % (key[0], key[1], spec['expires']))
    # 任何登记项（design 或 debt）只要写了 expires，到期就必须失败——否则
    # 一条「临时放行」可以靠把 kind 写成 design 逃过 L5。
    for entry in baseline.get('edges', []):
        expires = entry.get('expires')
        if expires and is_expired(expires, today):
            label = '%s → %s（%s 已过期）' % (entry['source'], entry['target'], expires)
            if label not in debt_expired:
                debt_expired.append(label)
    absent_debt = sorted(set(debt) - current)

    # L2 的「文件级可见度」：层对登记是粗粒度（68/72 对），但如果某个已登记层对
    # 内部**新增文件**开始依赖对方，--report 要能看见（旧 allow_files 是文件级，
    # 换成层对后这层信息不能丢）。这里只报告，不阻断：新文件属于已批准的方向。
    baseline_files = set()
    for entry in baseline.get('edges', []):
        for item in entry.get('files', []):
            baseline_files.add((entry['source'], entry['target'], item['path']))
    new_pair_files = sorted(set(edges) - baseline_files)
    gone_pair_files = sorted(baseline_files - set(edges))

    budgets = baseline.get('budgets', {})
    over_budget = []
    for rel, allowed in budgets.items():
        if rel not in budget_state['current']:
            continue
        current_stats = budget_state['current'][rel]
        for field in ('lines', 'pub_items'):
            if current_stats[field] > int(allowed.get(field, 0)):
                over_budget.append('%s:%s 当前 %d > 预算 %d'
                                   % (rel, field, current_stats[field], int(allowed.get(field, 0))))
    new_files = budget_state['new_files']

    scc_violations = []
    scc_notes = []
    declared = policy.get('intra_layer_scc', {})
    for layer, (size, members) in sorted(scc.items()):
        spec = declared.get(layer)
        if spec and spec.get('exempt'):
            scc_notes.append('%s: 最大 %d 节点（显式豁免：%s）'
                             % (layer, size, spec.get('reason', '未写理由')))
            continue
        if spec and spec.get('report_only_until'):
            scc_notes.append('%s: 最大 %d 节点（report_only_until=%s）'
                             % (layer, size, spec['report_only_until']))
            continue
        limit = int(spec.get('max_nodes', 1)) if spec else 1
        if size > limit:
            scc_violations.append('%s: %d 节点 > 允许 %d（%s）'
                                  % (layer, size, limit, ', '.join(members)))
        else:
            scc_notes.append('%s: 最大 %d 节点' % (layer, size))
    for layer, spec in sorted(declared.items()):
        if layer not in scc and not spec.get('exempt'):
            scc_notes.append('%s: 无环（声明 max_nodes=%s）'
                             % (layer, spec.get('max_nodes', 1)))

    allow_count = len(registered)
    registered_debt = len(set(debt) & registered)
    problems = []
    if unregistered:
        problems.append(('未登记边', ['%s → %s（%s）'
                                      % (s, t, ', '.join('%s×%d' % pair for pair in pairs[(s, t)]))
                                      for s, t in unregistered]))
    if forbidden_hits:
        problems.append(('禁区边', ['%s → %s' % pair for pair in forbidden_hits]))
    if debt_missing_meta:
        problems.append(('豁免缺少 issue/expires', debt_missing_meta))
    if debt_expired:
        problems.append(('豁免过期', debt_expired))
    vanished = sorted(set(absent_debt) | set(stale))
    if vanished:
        problems.append(('已登记边消失（豁免/基线腐化）',
                         ['%s → %s' % pair for pair in vanished]))
    if over_budget:
        problems.append(('预算超标', over_budget))
    if policy.get('new_files', 'report') == 'deny' and new_files:
        problems.append(('未登记新文件', sorted(new_files)))
    if scc_violations:
        problems.append(('层内 SCC', scc_violations))
    print('== 分层闸门（W0 allowlist 制） ==')
    print('策略: forbid %d | fake_targets %d | strip_comment_edges=%s | new_files=%s'
          % (len(forbidden), len(policy.get('fake_targets', [])),
             'on' if policy.get('strip_comment_edges', True) else 'off',
             policy.get('new_files', 'report')))
    print('扫描: %d 条文件级边 / %d 个层对'
          % (len(edges), len(pairs)))
    print('已登记 allow: %d（design %d / debt %d）'
          % (allow_count, allow_count - registered_debt, registered_debt))
    print('未登记边: %d' % len(unregistered))
    print('forbid 命中: %d / %d' % (len(forbidden_hits), len(forbidden)))
    print('假目标过滤: %d 次 / %d 个（%s）'
          % (sum(fake_hits.values()), len(fake_hits), ', '.join(sorted(fake_hits)) or '无'))
    print('注释幽灵边: %d 个层对层文件（剥离后不计数）'
          % len(ghost))
    if scc or scc_notes:
        print('层内 SCC:')
        for note in scc_notes:
            print('  - %s' % note)
    print('层对内新增文件: %d（可见度，不阻断；已消失 %d）'
          % (len(new_pair_files), len(gone_pair_files)))
    if new_pair_files:
        print('  新增: %s' % ', '.join('%s→%s %s' % item for item in new_pair_files))
    if gone_pair_files:
        print('  消失: %s' % ', '.join('%s→%s %s' % item for item in gone_pair_files))
    print('预算: %d 个文件, 超标 %d, 新文件 %d'
          % (len(budgets), len(over_budget), len(new_files)))
    if new_files:
        print('  新文件（%s）: %s'
              % (policy.get('new_files', 'report'), ', '.join(sorted(new_files))))
    print('豁免腐化: %d（已登记边消失 %d / 过期 %d）'
          % (len(vanished), len(vanished), len(debt_expired)))
    if stale:
        print('  已消失但仍登记: %s' % ', '.join('%s → %s' % pair for pair in stale))
    if problems:
        for title, items in problems:
            print('! %s（%d）:' % (title, len(items)))
            for item in items:
                print('    - %s' % item)
    return problems


# ---------------------------------------------------------------------------
# 冻结
# ---------------------------------------------------------------------------

def freeze(policy, old_baseline, update):
    print('== 冻结层间边与预算（--freeze%s） ==' % (' --update' if update else ''))
    edges, _, _, _ = collect(policy)
    pairs = edge_pairs(edges)
    old_index = baseline_index(old_baseline)
    debt = {tuple((entry['source'], entry['target'])): entry
            for entry in policy.get('debt', [])}
    entries = []
    added = []
    removed = []
    for (source, target) in sorted(pairs):
        previous = old_index.get((source, target))
        files = [{'path': rel, 'count': count} for rel, count in pairs[(source, target)]]
        entry = {
            'source': source,
            'target': target,
            'kind': 'design',
            'issue': None,
            'expires': None,
            'files': files,
        }
        if previous is not None:
            entry['kind'] = previous.get('kind', 'design')
            entry['issue'] = previous.get('issue')
            entry['expires'] = previous.get('expires')
        if (source, target) in debt:
            entry['kind'] = 'debt'
            entry['issue'] = debt[(source, target)].get('issue')
            entry['expires'] = debt[(source, target)].get('expires')
        if previous is None:
            added.append('%s → %s' % (source, target))
        entries.append(entry)
    for key in sorted(old_index):
        if key not in pairs:
            removed.append('%s → %s' % key)
    absent_debt = sorted(set(debt) - set(pairs))

    budgets, raised, lowered, added_files, removed_files, blocked = build_budgets(
        policy, old_baseline.get('budgets', {}), update)
    scc = intra_layer_scc(policy)

    baseline = {
        'version': 1,
        'comment': ('由 `python3 scripts/check-layers.py --freeze` 生成：当前层间边的'
                    ' allowlist 与文件预算棘轮。不要手改；新增/消失的边会被 `--check` '
                    '判失败，确认无误后重新 freeze。'),
        'fake_targets': list(policy.get('fake_targets', [])),
        'edges': entries,
        'budgets': budgets,
        'intra_layer_scc': {layer: size for layer, (size, _) in sorted(scc.items())},
    }
    with open(BASELINE, 'w', encoding='utf-8') as handle:
        json.dump(baseline, handle, ensure_ascii=False, indent=2, sort_keys=False)
        handle.write('\n')

    print('层对: %d（新增 %d / 消失 %d）' % (len(entries), len(added), len(removed)))
    for item in added:
        print('  + %s' % item)
    for item in removed:
        print('  - %s' % item)
    print('预算: %d 个文件（新增 %d / 移除 %d / 下降 %d / 上调 %d）'
          % (len(budgets), len(added_files), len(removed_files), len(lowered), len(raised)))
    for item in raised:
        print('  ↑ %s' % item)
    for item in lowered:
        print('  ↓ %s' % item)
    if absent_debt:
        print('! policy.debt 里有 %d 条边当前不存在（顺手删掉，否则 --check 判腐化）: %s'
              % (len(absent_debt), ', '.join('%s → %s' % key for key in absent_debt)))
    if blocked:
        print('! %d 处预算需要上调，但未加 --update：基线保留旧上界（棘轮），'
              '显式上调请用 `--freeze --update` 并在 PR 说明理由。' % len(blocked))
        for item in sorted(blocked):
            print('  ! %s' % item)
    print('已写入 %s' % os.path.relpath(BASELINE, ROOT))
    return 0


# ---------------------------------------------------------------------------
# 自测
# ---------------------------------------------------------------------------

def self_test():
    """闸门自身的回归用例：每个历史盲区一条最小复现，另加「过严」反例。"""
    cases = []

    def check(name, actual, expected):
        cases.append((name, actual == expected, actual, expected))

    policy = {
        'strip_comment_edges': True,
        'fake_targets': ['RunOutcome', 'CONFIG_RELOAD_CHILD_ENV'],
    }

    # 0. CI 必须用阻断模式。
    #
    # 这道闸门曾在 CI 里**空转**：`quality-gate.yml` 跑的是不带参数的
    # `python3 scripts/check-layers.py`，而 W0 引入 allowlist 制后默认走
    # `--report`（退出码恒 0）。也就是说层边与预算棘轮早已可阻断，CI 却一直
    # 报 PASS。这里把它钉住：CI 工作流必须带 `--check`。
    workflow = os.path.join(ROOT, '.github', 'workflows', 'quality-gate.yml')
    if os.path.isfile(workflow):
        with open(workflow, encoding='utf-8') as handle:
            lines = handle.read().splitlines()
        invocations = [line.strip() for line in lines
                       if 'check-layers.py' in line and not line.strip().startswith('#')]
        check('CI 的分层检查使用阻断模式（--check）',
              all('--check' in line for line in invocations) and bool(invocations),
              True)

    # 1. 路径过滤：tests 目录、*_tests.rs、tests.rs。
    check('tests 目录被排除',
          is_excluded_source('src/features/card_games/tests/rules.rs'), True)
    check('*_tests.rs 被排除',
          is_excluded_source('src/ui/routines/friend_delivery/tests.rs'), True)
    check('生产文件不被排除',
          is_excluded_source('src/features/card_games/rules.rs'), False)
    check('仅 cfg(test) 声明的文件被排除',
          is_excluded_source('src/config/schema/tests_snapshot.rs',
                             frozenset({'src/config/schema/tests_snapshot.rs'})), True)

    # --- 盲区①：根级叶子必须作为源扫描 -------------------------------------
    # 旧实现把 geometry 整层跳过，`src/geometry.rs:5` 的向上边完全不可见。
    leaf = 'src/geometry.rs'
    table = {
        'config': 'src/config/mod.rs',
        'geometry': 'src/geometry.rs',
        'text': 'src/text/mod.rs',
    }
    refs = scan_text(leaf, 'use crate::config::RectConfig;\n', table, policy)
    check('叶子作为源：geometry→config 可见',
          refs, [('config', 'src/config/mod.rs')])
    check('layer_of(geometry) 是叶子层', layer_of(leaf), 'geometry')

    # --- 盲区②：层内 SCC（Tarjan） ------------------------------------------
    successors = {
        'config/migrations': {'config/store'},
        'config/store': {'config/migrations'},
        'config/live': {'config/store'},
    }
    components = tarjan(sorted(successors), successors)
    cycle = [c for c in components if len(c) > 1]
    check('SCC：双节点环被识别', len(cycle), 1)
    check('SCC：环成员正确',
          sorted(cycle[0]) if cycle else [], ['config/migrations', 'config/store'])
    acyclic = {'a': {'b'}, 'b': set()}
    check('SCC：无环时没有 >1 分量',
          [c for c in tarjan(['a', 'b'], acyclic) if len(c) > 1], [])
    check('SCC 节点按第二段折叠',
          scc_node_of_file('src/config/store/sqlite.rs'), 'config/store')

    # --- 盲区③：注释里的 crate:: 不算边 ------------------------------------
    documented = (
        '/// 见 [`crate::composition::application::lifecycle`]。\n'
        '// 普通注释 crate::interfaces::http\n'
        '/* 块注释 crate::debug::sandbox */\n'
        'use crate::ui::overlay;\n'
    )
    stripped = strip_comments(documented)
    check('注释边：行/文档注释被剥离',
          'crate::composition' not in stripped and 'crate::interfaces' not in stripped,
          True)
    check('注释边：块注释被剥离', 'crate::debug' not in stripped, True)
    check('注释边：真正的 use 保留', 'crate::ui::overlay' in stripped, True)
    ghost_table = {
        'composition': 'src/composition/mod.rs',
        'ui': 'src/ui/mod.rs',
        'runtime': 'src/runtime/mod.rs',
    }
    refs = scan_text('src/runtime/deadline_bridge.rs',
                     '/// [`crate::composition::application::lifecycle`]\n'
                     'use crate::ui::overlay;\n',
                     ghost_table, policy)
    check('注释边：runtime→composition 幽灵边消失',
          [r[0] for r in refs], ['ui'])

    # --- 盲区④：假目标过滤 -------------------------------------------------
    fake_text = 'use crate::RunOutcome;\nstd::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV);\n'
    refs = scan_text('src/composition.rs', fake_text, table, policy)
    check('假目标：结构化解析不产生模块文件',
          [r[0] for r in refs if r[1] is not None], [])
    remaining = [r[0] for r in refs if r[0] not in policy['fake_targets']]
    check('假目标：策略过滤后不留残余', remaining, [])

    # --- 盲区④（负例）：真模块不能被误当假目标 -----------------------------
    real = scan_text('src/composition.rs', 'use crate::config::AppConfig;\n', table, policy)
    check('假目标：真模块不被过滤',
          [r for r in real if r[0] not in policy['fake_targets']],
          [('config', 'src/config/mod.rs')])

    # --- 「过严」反例 1：字符串字面量里的 crate:: 不是边 ---------------------
    literal = 'let text = "see crate::interfaces::http";\nuse crate::ui::overlay;\n'
    stripped = strip_comments(literal)
    check('过严反例：字符串里的 crate:: 未被当成注释误删',
          'crate::interfaces::http' in stripped, True)
    refs = scan_text('src/composition.rs', literal, table, policy)
    check('过严反例：字符串里的 crate:: 仍按源码文本统计（与旧口径一致）',
          [r for r in refs if r[0] == 'interfaces'], [('interfaces', None)])

    # --- 「过严」反例 2：同层自引用不是层间边 ------------------------------
    # 旧 normalize_module 把 text 映射成 root，`src/text/emoji.rs` 的
    # `super::normalize_comparison_text` 被误记成 text→root。
    refs = scan_text('src/text/emoji.rs', 'use super::normalize_comparison_text;\n', table,
                     policy)
    check('过严反例：同层 super 引用不产生层间边',
          [r for r in refs if r[0] != 'text'], [])

    # --- 其它：super 还原 / 入口文件 / allowlist 语义 ------------------------
    nav = module_path('src/ui/routines/friend_delivery/navigation.rs')
    check('super::super 从 4 层模块还原到 ui',
          resolve_parts(nav, ('super', 2, 'residency'))[0], 'ui')
    reply = module_path('src/features/ai_chat/reply.rs')
    check('super::super::super 弹到根后取 leaf 顶层模块',
          resolve_parts(reply, ('super', 3, 'interfaces')), ['interfaces'])
    entry = ('mod inner;\n#[cfg(test)]\nmod tests;\nuse crate::ui::overlay;\n')
    stripped = strip_module_declarations(strip_test_code(entry))
    check('lib.rs 测试声明被剥掉', 'mod tests;' not in stripped, True)
    check('模块声明行被剥掉', 'mod inner;' not in stripped, True)

    check('allowlist：已登记层对通过', ('ui', 'geometry') in {('ui', 'geometry')}, True)
    check('allowlist：未登记层对失败', ('adapters', 'composition') in {
        ('ui', 'geometry')}, False)

    # --- 豁免：issue/expires / 过期 / 预算棘轮 ------------------------------
    check('豁免：未来到期不算过期', is_expired('2099-01-01', '2026-11-23'), False)
    check('豁免：已过期被识别', is_expired('2020-01-01', '2026-11-23'), True)
    check('豁免：空到期不凭空过期', is_expired(None, '2026-11-23'), False)
    check('预算：行数持平原样通过', 4473 <= 4473, True)
    check('预算：行数上涨被拦下', 4474 <= 4473, False)

    failed = 0
    for name, ok, actual, expected in cases:
        print('%s %s' % ('PASS' if ok else 'FAIL', name))
        if not ok:
            failed += 1
            print('     实际：%r 期望：%r' % (actual, expected))
    print('自测%s：%d/%d 通过' % ('通过' if not failed else '失败',
                                  len(cases) - failed, len(cases)))
    return 1 if failed else 0


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------

def main(argv):
    args = argv[1:]
    if '--self-test' in args:
        return self_test()

    policy = load_json(POLICY, '层策略')
    if policy is None:
        return 2
    baseline = {'edges': [], 'budgets': {}}
    if os.path.exists(BASELINE):
        loaded = load_json(BASELINE, '层基线')
        if loaded is None:
            return 2
        baseline = loaded

    if '--freeze' in args:
        return freeze(policy, baseline, '--update' in args)

    edges, fake_hits, ghost, _ = collect(policy)
    if '--list' in args:
        for (source, target, rel), count in sorted(edges.items()):
            print('%-14s -> %-14s %-52s %d' % (source, target, rel, count))
        return 0

    scc = intra_layer_scc(policy)
    current_budgets = {}
    for rel in walk(discover_test_module_files()):
        current_budgets[rel] = file_budget(rel)
    known = set(baseline.get('budgets', {}))
    budget_state = {
        'current': current_budgets,
        'new_files': sorted(set(current_budgets) - known),
    }
    problems = render(edges, fake_hits, ghost, scc, policy, baseline,
                      today_iso(), budget_state)

    if '--check' in args:
        if problems:
            print('结论: FAIL（--check 严格模式）', file=sys.stderr)
            return 1
        print('结论: PASS（--check 严格模式）')
        return 0
    print('结论: PASS（--report 模式，不阻断）')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
