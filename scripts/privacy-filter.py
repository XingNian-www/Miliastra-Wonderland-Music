#!/usr/bin/env python3
"""按身份映射对文本做隐私替换。

映射默认直接从远端面板读取：GET /config/section?name=identity 的
values.mappings（与面板「身份映射」页同源）。扫描 real_nickname、note、
nickname 与账号 UID，命中后统一替换成稳定代号（玩家A、玩家B…）。

用法：
  # 扫描文件，打印到 stdout
  MILIASTRA_PANEL_TOKEN=xxx python3 scripts/privacy-filter.py prompts.txt

  # 原地替换（还原点默认写到 .local/privacy-backups/，该目录已被 .gitignore 忽略）
  python3 scripts/privacy-filter.py --write logs/*.log

  # 换还原点位置（不允许落在会被 git 跟踪的目录；那样会被拒绝并回退到默认目录）
  python3 scripts/privacy-filter.py --write --backup-dir /tmp/privacy-backup logs/*.log

  # 完全不留还原点
  python3 scripts/privacy-filter.py --write --no-backup logs/*.log

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
  --backup-dir DIR                           --write 的还原点目录（默认 .local/privacy-backups/）
  --no-backup                                --write 时完全不保留还原点

备份去哪儿了（2026-11-22 修，审阅 B21-P1-9）：
  旧实现把 `<file>.bak` 写在**源码旁边**，内容是替换前的**未脱敏原文**。而
  .gitignore 没有 `*.bak` 规则（`!/src/`、`!/scripts/` 是整目录放行），发布脚本
  release-pipeline.sh:258 又用 `git add -A -- .` 暂存 —— 一次 `--write` 后随手提交，
  真实昵称/UID 就随 `.bak` 进了仓库，脱敏等于白做。
  现在：
    - 还原点只写到 `.local/privacy-backups/`（`.gitignore` 的 `/.local/` 已忽略），
      或 `--backup-dir` 指定的、经 `git check-ignore` 确认忽略的目录；
    - 指定的目录若会被 git 跟踪，**拒绝写入**并回退到默认目录（宁可不符合调用方
      期望，也不重犯泄漏）；
    - 关掉备份用 `--no-backup`。
  这样与 release-pipeline.sh 的 `git add -A` 不冲突，也不需要改 `.gitignore`
  或那个脚本。注意：还原点里的原文仍在本地磁盘上，用完请自行删除。

数据型文件与二进制扫描（2026-11-23 修，W5a-2 / 审查 §4.3-D2）：
  旧实现只扫 15 种文本后缀（`CODE_SUFFIXES`），目录模式遇到 `.sqlite3` 直接跳过。
  所以「把真实 v3 配置库当夹具提交」时 CI 会报 0 命中——而 Git 历史**不可回退**。
  现在：
    - **禁止入库的数据型文件**：`git ls-files` 里出现
      `.sqlite3/.sqlite/.db/.db-wal/.db-shm/.bak/.log` 等后缀即失败并列出文件
      （`--check` 与 `--write` 都失败：脱敏不能把已跟踪的数据文件变回没跟踪）；
    - **二进制读法**：`--repo` 下对**全部已跟踪文件**按字节搜索 UTF-8 别名，
      不再受 `CODE_SUFFIXES` 限制，避免把敏感值藏进不常见后缀；二进制文件无法
      自动替换，只报告命中文件数，命中即让 `--check` 失败。
  夹具的正确做法是程序化合成（见 `src/config/store/fixtures/README.md`）。
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

DEFAULT_SOURCE = os.environ.get("MILIASTRA_PANEL_URL", "http://192.168.31.119:18888")
DEFAULT_TOKEN = os.environ.get("MILIASTRA_PANEL_TOKEN", "")
REPO_CODE_ROOTS = ["src", "web/src", "scripts", "technical-docs"]
CODE_SUFFIXES = {".rs", ".vue", ".js", ".ts", ".mjs", ".py", ".md", ".toml", ".json", ".html", ".css", ".yml", ".yaml", ".sh"}
# 数据型文件：内容可能是真实配置库、日志或未脱敏备份，**一律禁止入库**。
# 审查 §4.3-D2 确认旧版目录扫描对后缀不在 CODE_SUFFIXES 的文件直接 continue，
# 于是带真实 UID/昵称的 .sqlite3 进 Git 也会报 0 命中；Git 历史不可回退，只能在
# 入口处拦住。用 endswith 元组匹配，能覆盖 `.sqlite3-wal` 这类复合后缀。
FORBIDDEN_TRACKED_SUFFIXES = (
    ".sqlite3",
    ".sqlite",
    ".sqlite3-wal",
    ".sqlite3-shm",
    ".sqlite-wal",
    ".sqlite-shm",
    ".db",
    ".db-wal",
    ".db-shm",
    ".db3",
    ".mdb",
    ".ldb",
    ".bak",
    ".log",
)
SKIP_PARTS = {"node_modules", "target", "dist", ".git", ".cargo", "__pycache__"}
ROOT = Path(__file__).resolve().parent.parent
# 备份默认落在仓库根下的 `.local/privacy-backups/`：`.gitignore` 里的 `/.local/`
# 把整个目录忽略掉，`git add -A` 收不到它的内容。
#
# 为什么必须换地方（审阅 B21-P1-9）：旧实现把 `<原文件>.bak` 写在源码旁边，
# 内容是**替换前的未脱敏原文**；而 `.gitignore` 里没有 `*.bak` 规则
# （`!/src/`、`!/scripts/` 是整目录放行），`scripts/release-pipeline.sh` 又用
# `git add -A -- .` 暂存 —— 一次 `--write` 之后随手提交，真实昵称/UID 就随
# `.bak` 进了仓库，脱敏等于白做。
DEFAULT_BACKUP_DIR = ROOT / ".local" / "privacy-backups"


def is_git_ignored(path: Path) -> bool:
    """确认 path 会被 git 忽略（`git check-ignore`）。

    判断“备份放这儿安不安全”必须问 git 本身，而不是在脚本里再抄一份 `.gitignore`
    规则：规则会变，抄的那份一定先过期。git 不可用 / 不是仓库时返回 False
    （保守：当作没忽略）。
    """
    try:
        result = subprocess.run(
            ["git", "check-ignore", "-q", "--", str(path)],
            cwd=str(ROOT),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except OSError:
        return False
    return result.returncode == 0


def resolve_backup_dir(requested: str | None) -> Path:
    """决定备份目录；返回一个**确认不会被 git 收走**的位置。

    - 没给 `--backup-dir` 的值 → 用默认的 `.local/privacy-backups/`；
    - 给了值但它落在仓库内且没被忽略 → 拒绝在那里写未脱敏原文，
      改写到默认目录并告警（宁可不按用户指定的路径写，也不重犯泄漏）。
    """
    target = Path(requested).expanduser() if requested else DEFAULT_BACKUP_DIR
    try:
        target = target.resolve()
    except OSError:
        target = DEFAULT_BACKUP_DIR
    target.mkdir(parents=True, exist_ok=True)
    if not is_git_ignored(target / ".keep"):
        if target == DEFAULT_BACKUP_DIR.resolve():
            # 默认目录居然没被忽略：说明 .gitignore 改坏了。退到系统临时目录。
            target = Path(tempfile.gettempdir()) / "miliastra-privacy-backups"
            target.mkdir(parents=True, exist_ok=True)
            print(
                f"warning: {DEFAULT_BACKUP_DIR} 未被 .gitignore 忽略（规则可能被改动），"
                f"备份改写到 {target}",
                file=sys.stderr,
            )
        else:
            print(
                f"WARNING: 指定的备份目录 {target} 会被 git 跟踪；备份含未脱敏原文，"
                f"拒绝写入，改用 {DEFAULT_BACKUP_DIR}",
                file=sys.stderr,
            )
            target = DEFAULT_BACKUP_DIR
            target.mkdir(parents=True, exist_ok=True)
    return target


def write_backup(backup_dir: Path, path: Path, text: str) -> Path:
    """把**替换前**的原文备份到 backup_dir，返回备份路径。

    文件名用「仓库内相对路径扁平化 + 绝对路径哈希」：既避免不同目录同名文件
    互相覆盖，也能一眼看出备份来自哪个文件。
    """
    digest = hashlib.sha256(str(path.resolve()).encode("utf-8")).hexdigest()[:12]
    try:
        flat = str(path.resolve().relative_to(ROOT)).replace(os.sep, "__")
    except ValueError:
        flat = path.name
    backup = backup_dir / f"{flat}.{digest}.bak"
    backup.write_text(text, encoding="utf-8")
    return backup


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


def tracked_files():
    """`git ls-files` 的仓库内相对路径；git 不可用或不是仓库时返回 None。"""
    try:
        result = subprocess.run(
            ["git", "ls-files", "-z"],
            cwd=str(ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    return [
        Path(item)
        for item in result.stdout.decode("utf-8", "surrogateescape").split("\0")
        if item
    ]


def forbidden_tracked_files(tracked):
    """已跟踪的数据型文件（后缀命中 FORBIDDEN_TRACKED_SUFFIXES）。"""
    return [
        path
        for path in tracked
        if str(path).lower().endswith(FORBIDDEN_TRACKED_SUFFIXES)
    ]


def binary_alias_hits(path, aliases):
    """按字节在已跟踪文件里搜索 UTF-8 别名，返回命中的别名个数。

    分块读取（1 MiB）+ 最大别名长度重叠，避免把大二进制整个读进内存；别名
    跨块边界时仍能命中。`bytes.lower()` 只折叠 ASCII A-Z，非 ASCII 字节原样，
    正好对应既有文本扫描「中文精确、拉丁忽略大小写」的口径。
    """
    needles = {}
    for alias in aliases:
        if not alias:
            continue
        raw = alias.encode("utf-8")
        key = raw.lower() if alias.isascii() else raw
        needles.setdefault(key, []).append(alias)
    if not needles:
        return 0
    overlap = max(len(key) for key in needles) - 1
    hit_keys = set()
    tail = b""
    try:
        with (ROOT / path).open("rb") as handle:
            while True:
                chunk = handle.read(1 << 20)
                if not chunk:
                    break
                window = tail + chunk
                lowered = window.lower()
                for key in needles:
                    if key in lowered:
                        hit_keys.add(key)
                # 只保留可能跨块的上一个别名尾部，避免窗口无限增长。
                tail = window[-overlap:] if overlap > 0 else b""
    except OSError:
        return 0
    return sum(len(values) for key, values in needles.items() if key in hit_keys)


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
    parser.add_argument("--write", action="store_true", help="原地写回（默认写到 .local/privacy-backups/ 留还原点）")
    parser.add_argument(
        "--backup-dir",
        default=None,
        help="--write 的还原点目录（默认 .local/privacy-backups/；落在仓库内且未被忽略时会被拒绝并回退）",
    )
    parser.add_argument(
        "--no-backup",
        action="store_true",
        help="--write 时完全不保留还原点（替换幂等，重跑无害）",
    )
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
    # --write 默认留还原点，但只写到**确认被 git 忽略**的目录（见 resolve_backup_dir）。
    # `--no-backup` 可以完全关掉；`--backup-dir` 可覆盖位置，但落在仓库内且未被忽略时
    # 会被拒绝并回退到安全目录 —— 未脱敏原文绝不写进会被 `git add -A` 收走的地方。
    backup_dir = None
    if args.write and not args.no_backup:
        try:
            backup_dir = resolve_backup_dir(args.backup_dir)
        except OSError as error:
            raise SystemExit(f"无法准备备份目录：{error}")
    written_backups: list[Path] = []

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
                # 备份必须在写入**之前**落盘（存的是替换前原文）；写备份失败就不改动源文件，
                # 避免“改了却无法还原”。
                if backup_dir is not None:
                    written_backups.append(write_backup(backup_dir, path, text))
                path.write_text(sanitized, encoding="utf-8")
        elif not args.check and count:
            if str(path) != "<stdin>":
                print(sanitized)
    # 仓库级守卫（W5a-2 / 审查 D2）：数据型文件禁止入库 + 二进制读法兜底。
    # 两者都基于 `git ls-files`，与目录扫描的 CODE_SUFFIXES 白名单无关。
    tracked = tracked_files()
    if tracked is None and (args.repo or args.check):
        print(
            "warning: 无法读取 git ls-files（不是仓库或 git 不可用），"
            "跳过「数据型文件禁止入库」与二进制扫描",
            file=sys.stderr,
        )
    forbidden = forbidden_tracked_files(tracked) if tracked is not None else []
    binary_hit_files: list[Path] = []
    if args.repo and tracked is not None:
        aliases = [alias for alias, _label in alias_to_label]
        for path in tracked:
            # 文本后缀已在主循环里按 UTF-8 扫过；这里专治「不常见后缀」。
            if path.suffix.lower() in CODE_SUFFIXES:
                continue
            hits = binary_alias_hits(path, aliases)
            if hits:
                binary_hit_files.append(path)
                print(
                    f"{path}: 二进制内容命中 {hits} 个身份别名"
                    "（无法自动替换：必须移出仓库或改存占位符）",
                    file=sys.stderr,
                )

    print(f"共替换 {total} 处，涉及 {changed_files} 个文件；映射 {len(mappings)} 位玩家、{len(alias_to_label)} 个别名", file=sys.stderr)
    if args.repo and tracked is not None:
        print(
            f"二进制扫描：{len(tracked)} 个已跟踪文件中 {len(binary_hit_files)} 个命中"
            "（二进制内容不参与自动替换）",
            file=sys.stderr,
        )
    if written_backups:
        print(
            f"已把 {len(written_backups)} 份**含未脱敏原文**的还原点写到 {backup_dir}"
            "（该目录已被 .gitignore 忽略，不会被 git add -A 收走）：",
            file=sys.stderr,
        )
        for backup in written_backups:
            print(f"  {backup}", file=sys.stderr)
        print("确认无误后请自行删除这些还原点。", file=sys.stderr)
    elif args.write and changed_files:
        print("未保留还原点（--no-backup）。", file=sys.stderr)
    if forbidden:
        print(
            "ERROR: 以下数据型文件已被 git 跟踪，禁止入库"
            "（内容可能是真实配置库/日志/未脱敏备份，Git 历史不可回退）：",
            file=sys.stderr,
        )
        for path in forbidden:
            print(f"  {path}", file=sys.stderr)
        print(
            "处理方式：git rm --cached <文件>，并把后缀加入 .gitignore；"
            "迁移夹具请改为程序化合成（见 src/config/store/fixtures/README.md）。",
            file=sys.stderr,
        )
    if args.check and (total or binary_hit_files):
        return 1
    # 数据型文件是硬红线：即使 `--write` 已经完成脱敏，只要它还在 git 索引里就失败，
    # 提醒调用方 `git rm --cached`（脱敏不能把「已跟踪」变回「没跟踪」）。
    if forbidden:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
