#!/usr/bin/env python3
"""面板 API 覆盖率：解析 Rust 路由表 vs web/src 实际调用。

用法：python3 scripts/panel-api-coverage.py [--json] [--min-routes N]
退出码：
  0  报告正常产出，且解析到的 Rust 路由数 ≥ 下限（默认 1）。
  1  **路由表一条都没解析到**。这说明 protocol.rs 的路由表写法已经和下面的
     正则脱钩（改名/换结构体/换文件），此时「未覆盖 0 个」是假的 100% 覆盖，
     绝不能当成门禁通过——门禁脚本据此直接红灯，而不是报一串 0。
  2  protocol.rs 或 web/src 不存在（跑错工作目录）。

口径（2026-11-21 校订）：
  - 前端已 Vue 3 + Vite 化，入口是 `web/src/pages/ToolsPage.vue` 等 SPA 页面；
    旧的单文件 `page.html` 与 `/legacy` 回退页已删除，不再有 iframe 内嵌的 `tools.html`。
  - 「高级控制」页（hash 路由 `#tools`）直接 fetch `/tools/*`，因此这些路由由静态扫描
    `web/src` 自然覆盖，不需要 `covered_by_tools` 这类整页放行分支。
  - 仅 `/assets` 是协议特判的静态入口（编译期嵌入 web/dist），不在路由表里。
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROTOCOL = ROOT / "src/interfaces/http/protocol.rs"
WEB = ROOT / "web/src"
# 路由数下限。0 条说明正则已经和路由表写法脱钩，「未覆盖 0 个」不再有任何含义。
DEFAULT_MIN_ROUTES = 1


def rust_routes() -> list[str]:
    text = PROTOCOL.read_text(encoding="utf-8")
    paths = re.findall(r'BodyRouteSpec\s*\{\s*path:\s*"([^"]+)"', text)
    paths += re.findall(r'RouteSpec\s*\{\s*path:\s*"([^"]+)"', text)
    return sorted(set(paths))


def frontend_calls() -> set[str]:
    used: set[str] = set()
    for path in list(WEB.rglob("*.vue")) + list(WEB.rglob("*.js")):
        text = path.read_text(encoding="utf-8")
        for match in re.findall(r"[`'\"](/[A-Za-z][A-Za-z0-9_/-]*)[`'\"?#$}]", text):
            used.add(match)
    return used


def min_routes_from_argv() -> int:
    for index, argument in enumerate(sys.argv):
        if argument == "--min-routes" and index + 1 < len(sys.argv):
            try:
                return int(sys.argv[index + 1])
            except ValueError:
                raise SystemExit(f"--min-routes 需要整数，当前为 {sys.argv[index + 1]!r}")
        if argument.startswith("--min-routes="):
            try:
                return int(argument.split("=", 1)[1])
            except ValueError:
                raise SystemExit(f"--min-routes 需要整数，当前为 {argument!r}")
    return DEFAULT_MIN_ROUTES


def main() -> int:
    if not PROTOCOL.is_file():
        print(f"::error::找不到路由表 {PROTOCOL}（请在仓库根目录执行）", file=sys.stderr)
        return 2
    if not WEB.is_dir():
        print(f"::error::找不到前端目录 {WEB}（请在仓库根目录执行）", file=sys.stderr)
        return 2

    routes = rust_routes()
    minimum = min_routes_from_argv()
    if len(routes) < minimum:
        # 关键：绝不能在这里继续往下走。路由表解析失败时 missing 必然为空，
        # 输出「未覆盖 0 个」，quality-gate.yml 的 `$missing -ne 0` 断言会直接放行。
        print(
            f"::error::只从 {PROTOCOL.relative_to(ROOT)} 解析到 {len(routes)} 条路由"
            f"（下限 {minimum}）。路由表写法可能已变，覆盖率报告失效，不能当作通过。",
            file=sys.stderr,
        )
        print(
            "  排查：src/interfaces/http/protocol.rs 里的路由是否仍写成 "
            'RouteSpec { path: "/..." } / BodyRouteSpec { path: "/..." }（path 必须是第一个字段）。',
            file=sys.stderr,
        )
        return 1

    used = frontend_calls()

    def covered(route: str) -> bool:
        if route in used:
            return True
        # 协议特判的嵌入静态入口（web/dist），不在路由表匹配范围内。
        return route in {"/assets"}

    missing = [route for route in routes if not covered(route)]
    report = {
        "routes": len(routes),
        "covered": len(routes) - len(missing),
        "missing": missing,
        "frontend_only": sorted(used - set(routes)),
    }
    if "--json" in sys.argv:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    else:
        print(f"Rust 路由 {report['routes']} 个，前端已调用 {report['covered']} 个，未覆盖 {len(missing)} 个")
        for route in missing:
            print(f"  - {route}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
