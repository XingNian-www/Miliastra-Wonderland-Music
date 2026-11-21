#!/usr/bin/env python3
"""面板 API 覆盖率：解析 Rust 路由表 vs web/src 实际调用。

用法：python3 scripts/panel-api-coverage.py [--json]
退出码：0；仅用于报告。

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


def main() -> int:
    routes = rust_routes()
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
