#!/usr/bin/env python3
"""面板 API 覆盖率：解析 Rust 路由表 vs web/src 实际调用。

用法：python3 scripts/panel-api-coverage.py [--json]
退出码：0；仅用于报告。"""
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
        for match in re.findall(r"[`'\"](/[a-z][a-z0-9_/-]*)[`'\"?#$]", text):
            used.add(match)
    return used


def main() -> int:
    routes = rust_routes()
    used = frontend_calls()
    missing = [route for route in routes if route not in used and not route.startswith("/assets/") and route != "/legacy"]
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
