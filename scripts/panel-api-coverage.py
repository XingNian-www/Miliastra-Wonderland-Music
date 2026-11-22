#!/usr/bin/env python3
"""面板 API 覆盖率：解析 Rust 路由表 vs web/src 实际调用。

用法：python3 scripts/panel-api-coverage.py [--json] [--min-routes N]
退出码：
  0  报告正常产出，且解析到的 Rust 路由数 ≥ 下限（默认 1）。
  1  **路由表一条都没解析到**。这说明 protocol.rs 的路由表写法已经和下面的
     正则脱钩（改名/换结构体/换文件），此时「未覆盖 0 个」是假的 100% 覆盖，
     绝不能当成门禁通过——门禁脚本据此直接红灯，而不是报一串 0。
  2  protocol.rs 或 web/src 不存在（跑错工作目录）。
  3  `OPS_ONLY_ROUTES` 例外清单失效：声明的运维端点已不在 Rust 路由表里
     （路由被删除/改名却没同步清单），或某条例外已被前端调用（说明它不是
     运维专属）。例外清单必须跟着路由表一起维护，否则它会腐烂成盲区。

口径（2026-11-21 校订）：
  - 前端已 Vue 3 + Vite 化，入口是 `web/src/pages/ToolsPage.vue` 等 SPA 页面；
    旧的单文件 `page.html` 与 `/legacy` 回退页已删除，不再有 iframe 内嵌的 `tools.html`。
  - 「高级控制」页（hash 路由 `#tools`）直接 fetch `/tools/*`，因此这些路由由静态扫描
    `web/src` 自然覆盖，不需要 `covered_by_tools` 这类整页放行分支。
  - 仅 `/assets` 是协议特判的静态入口（编译期嵌入 web/dist），不在路由表里。
  - 面板不消费的**运维/诊断端点**可以在 `OPS_ONLY_ROUTES` 里显式声明例外；
    例外必须写明具体理由，并被下面的路由表守护检查。**例外只能加运维端点，
    不许加业务路由**——面板字段是前后端冻结契约，业务路由加了却没人调就是
    面板功能静默缺失，必须让门禁报红，而不是塞进白名单掩盖。
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

# 运维/诊断端点例外清单（路由 -> 面板刻意不消费的具体理由）。
#
# 规则：**只能加运维端点，不许加业务路由**。面板字段是前后端冻结契约，业务路由
# 加了却没人调就是功能静默缺失，必须由门禁报红；把它塞进这里等于掩盖问题。
# 每条例外都必须写清「为什么面板不需要它」，供评审与后续审计；路由被删除或
# 改名后必须同步删除对应条目，否则脚本按退出码 3 报错（守护检查见 main）。
OPS_ONLY_ROUTES: dict[str, str] = {
    "/health/services": "运维/诊断端点：进程级只读服务快照，供运维与外部探针拉取，面板刻意不消费",
}


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

    route_set = set(routes)
    # 守护 1：例外清单不能腐烂。路由删除/改名后，残留的例外会让脚本永远
    # 静默放行一条根本不存在（或已改名）的路由。
    stale_ops_routes = sorted(route for route in OPS_ONLY_ROUTES if route not in route_set)
    if stale_ops_routes:
        print(
            "::error::运维端点例外清单已失效："
            f"{', '.join(stale_ops_routes)} 不在 {PROTOCOL.relative_to(ROOT)} 的路由表里。",
            file=sys.stderr,
        )
        print(
            "  OPS_ONLY_ROUTES 只能声明仍然存在的运维/诊断端点；路由删除或改名后必须同步清理。",
            file=sys.stderr,
        )
        return 3
    # 守护 2：例外必须自带理由，否则日后无法审计「为什么面板不消费」。
    unexplained = sorted(route for route, reason in OPS_ONLY_ROUTES.items() if not reason.strip())
    if unexplained:
        print(
            f"::error::运维端点例外缺少理由：{', '.join(unexplained)}",
            file=sys.stderr,
        )
        return 3

    used = frontend_calls()
    # 守护 3：前端已经在调用的路由不可能是「运维专属」。出现这种条目说明
    # 清单过期（或有人把业务路由误加进来），按失效处理而不是继续放行。
    ops_but_used = sorted(route for route in OPS_ONLY_ROUTES if route in used)
    if ops_but_used:
        print(
            f"::error::运维端点例外已被前端调用：{', '.join(ops_but_used)}",
            file=sys.stderr,
        )
        print(
            "  它并非面板不消费的运维端点，请从 OPS_ONLY_ROUTES 删除（业务路由不许走例外）。",
            file=sys.stderr,
        )
        return 3

    def covered(route: str) -> bool:
        if route in used:
            return True
        # 协议特判的嵌入静态入口（web/dist），不在路由表匹配范围内。
        return route in {"/assets"}

    missing = [route for route in routes if route not in OPS_ONLY_ROUTES and not covered(route)]
    report = {
        "routes": len(routes),
        "covered": len(routes) - len(missing) - len(OPS_ONLY_ROUTES),
        "missing": missing,
        "ops_only": dict(OPS_ONLY_ROUTES),
        "ops_only_count": len(OPS_ONLY_ROUTES),
        "frontend_only": sorted(used - route_set),
    }
    if "--json" in sys.argv:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    else:
        # 文案仍以「未覆盖 N 个」为主句：CI 与 Makefile 用
        # `sed -n 's/.*未覆盖 \([0-9][0-9]*\) 个.*/\1/p'` 抽取 N，后缀不参与解析。
        suffix = (
            f"（另有 {report['ops_only_count']} 条声明为运维端点，详见 --json）"
            if report["ops_only_count"]
            else ""
        )
        print(
            f"Rust 路由 {report['routes']} 个，前端已调用 {report['covered']} 个，"
            f"未覆盖 {len(missing)} 个{suffix}"
        )
        for route in missing:
            print(f"  - {route}")
        for route, reason in report["ops_only"].items():
            print(f"  [运维端点] {route}：{reason}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
