#!/usr/bin/env python3
"""按身份映射对文本做隐私替换。

映射默认直接从远端面板读取：GET /config/section?name=identity 的
values.mappings（与面板「身份映射」页同源）。扫描 real_nickname、note、
nickname 与账号 UID，命中后统一替换成稳定代号（玩家A、玩家B…）。

用法：
  # 扫描文件，打印到 stdout
  MILIASTRA_PANEL_TOKEN=xxx python3 scripts/privacy-filter.py prompts.txt

  # 原地替换（会保留 .bak 备份）
  python3 scripts/privacy-filter.py --write logs/*.log

  # 自定义映射文件 / 远端
  python3 scripts/privacy-filter.py --mappings identity.json input.txt
  python3 scripts/privacy-filter.py --source http://192.168.31.119:18888 --token 123456 input.txt

  # CI：命中即失败
  python3 scripts/privacy-filter.py --check report.md
选项：
  --fields nickname,real_nickname,note,uid   参与替换的字段（默认全选）
  --prefix 示例玩家                          代号前缀（代码扫描默认用「示例玩家」）
  --repo                                     不给 paths 时扫描仓库代码（src、web/src、scripts、technical-docs）
  --mode code|text                            code 用「示例玩家A」这类中性代号；text 用「玩家A」
  --keep-uid                                 保留 UID 原样（不参与替换）
"""
import argparse
import json
import os
import re
import sys
import urllib.request
from pathlib import Path

DEFAULT_SOURCE = os.environ.get("MILIASTRA_PANEL_URL", "http://192.168.31.119:18888")
DEFAULT_TOKEN = os.environ.get("MILIASTRA_PANEL_TOKEN", "")
REPO_CODE_ROOTS = ["src", "web/src", "scripts", "technical-docs"]
CODE_SUFFIXES = {".rs", ".vue", ".js", ".ts", ".mjs", ".py", ".md", ".toml", ".json", ".html", ".css", ".yml", ".yaml", ".sh"}
SKIP_PARTS = {"node_modules", "target", "dist", ".git", ".cargo", "__pycache__"}


def fetch_mappings(source, token, timeout=15):
    url = source.rstrip("/") + "/config/section?name=identity"
    request = urllib.request.Request(url, headers={"X-Miliastra-Token": token} if token else {})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        payload = json.loads(response.read().decode("utf-8"))
    return payload


def load_mappings(args):
    if args.mappings:
        payload = json.loads(Path(args.mappings).read_text(encoding="utf-8"))
    else:
        payload = fetch_mappings(args.source, args.token)
    if isinstance(payload, dict):
        if isinstance(payload.get("values"), dict):
            mappings = payload["values"].get("mappings")
        elif isinstance(payload.get("sections"), dict):
            mappings = payload["sections"].get("identity", {}).get("mappings")
        else:
            mappings = payload.get("mappings")
    else:
        mappings = payload
    if not isinstance(mappings, list):
        raise SystemExit("找不到身份映射：期望 values.mappings / sections.identity.mappings / mappings 数组")
    return mappings


def aliases_of(mapping, fields):
    """返回 (别名, 类型) 列表；跳过空值、纯 id、过短拉丁串。"""
    out = []

    def add(value, kind):
        if value is None:
            return
        text = str(value).strip()
        if not text:
            return
        if kind == "uid":
            if len(text) < 5 or not text.isdigit():
                return
        elif len(text) < 2:
            return
        out.append((text, kind))

    if "nickname" in fields:
        add(mapping.get("nickname"), "nickname")
    if "real_nickname" in fields:
        add(mapping.get("real_nickname"), "real_nickname")
    if "note" in fields:
        add(mapping.get("note"), "note")
    if "uid" in fields:
        for account in mapping.get("accounts") or []:
            if isinstance(account, dict):
                add(account.get("uid"), "uid")
    return out


def build_alias_map(mappings, fields, prefix):
    """别名 → 稳定代号；同一玩家所有别名指向同一个代号。按名字长度降序匹配。"""
    alias_to_label = {}
    entries = []
    for index, mapping in enumerate(mappings):
        if not isinstance(mapping, dict):
            continue
        label = f"{prefix}{code_for(index)}" if prefix else code_for(index)
        aliases = aliases_of(mapping, fields)
        if not aliases:
            continue
        entries.append((label, aliases))
        for text, _kind in aliases:
            alias_to_label.setdefault(text, label)
    # 长别名优先：短名字先匹配会吃掉长名字的一部分
    ordered = sorted(alias_to_label.items(), key=lambda item: len(item[0]), reverse=True)
    return ordered


def code_for(index):
    """0..25 → A..Z，之后 27、28…"""
    if index < 26:
        return chr(ord("A") + index)
    return str(index + 1)


def compile_pattern(aliases):
    if not aliases:
        return None
    # 中文按原样；拉丁别名忽略大小写；整体按长度降序
    parts = []
    for text in aliases:
        parts.append(re.escape(text))
    return re.compile("|".join(parts), re.IGNORECASE if any(text.isascii() for text in aliases) else 0)


def sanitize(text, pattern, alias_to_label):
    if not pattern:
        return text, 0
    counts = {}

    def replace(match):
        original = match.group(0)
        label = alias_to_label.get(original)
        if label is None:
            # 忽略大小写命中时找回原 key
            for alias, candidate in alias_to_label.items():
                if alias.lower() == original.lower():
                    label = candidate
                    break
        if label is None:
            return original
        counts[label] = counts.get(label, 0) + 1
        return label

    return pattern.sub(replace, text), sum(counts.values())


def iter_targets(paths, repo=False):
    if repo and not paths:
        paths = REPO_CODE_ROOTS
    for raw in paths:
        if raw == "-":
            yield Path("<stdin>")
            continue
        path = Path(raw)
        if path.is_dir():
            for child in sorted(path.rglob("*")):
                if not child.is_file():
                    continue
                if any(part in SKIP_PARTS for part in child.parts):
                    continue
                if child.suffix.lower() not in CODE_SUFFIXES:
                    continue
                yield child
        else:
            yield path


def main():
    parser = argparse.ArgumentParser(description="按身份映射替换文本中的真实昵称/备注/UID")
    parser.add_argument("paths", nargs="*", help="文件/目录，- 表示 stdin；配合 --repo 可省略")
    parser.add_argument("--source", default=DEFAULT_SOURCE, help="面板地址（默认环境变量 MILIASTRA_PANEL_URL）")
    parser.add_argument("--token", default=DEFAULT_TOKEN, help="访问令牌（默认环境变量 MILIASTRA_PANEL_TOKEN）")
    parser.add_argument("--mappings", help="离线身份映射 JSON（/config/section?name=identity 的响应）")
    parser.add_argument("--fields", default="nickname,real_nickname,note,uid", help="参与替换的字段，逗号分隔")
    parser.add_argument("--keep-uid", action="store_true", help="不替换 UID")
    parser.add_argument("--prefix", default=None, help="代号前缀；默认 code 模式「示例玩家」，text 模式「玩家」")
    parser.add_argument("--repo", action="store_true", help="不给 paths 时扫描仓库代码目录")
    parser.add_argument("--mode", choices=["code", "text"], default="code", help="code=中性示例代号；text=玩家A")
    parser.add_argument("--write", action="store_true", help="原地写回（.bak 备份）")
    parser.add_argument("--check", action="store_true", help="仅检查；有命中时退出码 1")
    args = parser.parse_args()

    fields = {item.strip() for item in args.fields.split(",") if item.strip()}
    if args.keep_uid:
        fields.discard("uid")
    if args.prefix is None:
        args.prefix = "示例玩家" if args.mode == "code" else "玩家"
    if args.repo and not args.paths:
        args.paths = list(REPO_CODE_ROOTS)

    mappings = load_mappings(args)
    alias_to_label = build_alias_map(mappings, fields, args.prefix)
    if not alias_to_label:
        print("没有可用的身份映射别名（检查映射是否为空或字段过滤过严）", file=sys.stderr)
        return 2
    pattern = compile_pattern([alias for alias, _label in alias_to_label])
    lookup = dict(alias_to_label)

    total = 0
    changed_files = 0
    for path in iter_targets(args.paths, repo=args.repo):
        try:
            if str(path) == "<stdin>":
                text = sys.stdin.read()
            else:
                text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            print(f"跳过 {path}：{error}", file=sys.stderr)
            continue
        sanitized, count = sanitize(text, pattern, lookup)
        total += count
        if count:
            changed_files += 1
            print(f"{path}: 替换 {count} 处", file=sys.stderr)
        if args.write and count:
            if str(path) != "<stdin>":
                backup = path.with_suffix(path.suffix + ".bak")
                backup.write_text(text, encoding="utf-8")
                path.write_text(sanitized, encoding="utf-8")
        elif not args.check and count:
            if str(path) != "<stdin>":
                print(sanitized)
    print(f"共替换 {total} 处，涉及 {changed_files} 个文件；映射 {len(mappings)} 位玩家、{len(alias_to_label)} 个别名", file=sys.stderr)
    if args.check and total:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
