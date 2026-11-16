#!/usr/bin/env python3
"""Vue 面板运行时审计：逐路由无头加载，收集 JS 错误、非 2xx 请求与页面文本。

前置：web/dist 已构建；远程面板可访问并带 x-miliastra-token。
用法：
  python3 scripts/panel-audit.py --base http://192.168.31.119:18888 --token 123456 \
      --dist web/dist --out /tmp/panel-audit
每条路由输出：截图、DOM、__audit JSON；汇总写入 report.json。
"""
import argparse
import http.server
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

INJECT = """
<script>
(() => {
  const target = '__BASE__';
  const token = '__TOKEN__';
  try { localStorage.setItem('miliastraAccessToken', token); } catch (e) {}
  const original = window.fetch.bind(window);
  window.fetch = (input, init) => {
    let url = input;
    if (typeof url === 'string' && url.startsWith('/')) url = target + url;
    return original(url, init);
  };
  const audit = { errors: [], rejects: [], consoleErrors: [], http: [] };
  window.__audit = audit;
  window.addEventListener('error', (event) => audit.errors.push(String(event.message || event.error)));
  window.addEventListener('unhandledrejection', (event) => audit.rejects.push(String(event.reason)));
  const consoleError = console.error.bind(console);
  console.error = (...args) => { audit.consoleErrors.push(args.map(String).join(' ')); consoleError(...args); };
  const originalFetch = window.fetch;
  window.fetch = async (...args) => {
    const response = await originalFetch(...args);
    const url = typeof args[0] === 'string' ? args[0] : String(args[0] && args[0].url || '');
    if (!response.ok && response.status !== 401) audit.http.push(`${response.status} ${url.replace(target, '')}`);
    return response;
  };
  setTimeout(() => {
    const pre = document.createElement('pre');
    pre.id = '__audit_out';
    pre.textContent = JSON.stringify(audit);
    document.body.appendChild(pre);
  }, 5000);
})();
</script>
"""


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def serve(directory: str):
    handler = lambda *a, **kw: Quiet(*a, directory=directory, **kw)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, server.server_address[1]


def run_chromium(url: str, screenshot: Path, timeout: int = 60) -> str:
    cmd = [
        "chromium", "--headless=new", "--no-sandbox", "--disable-gpu",
        "--disable-web-security", f"--user-data-dir={tempfile.mkdtemp(prefix='audit-chrome-')}",
        "--window-size=1500,1200", "--virtual-time-budget=12000",
        "--screenshot=" + str(screenshot), "--dump-dom", url,
    ]
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return result.stdout
    except subprocess.TimeoutExpired:
        return ""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://192.168.31.119:18888")
    parser.add_argument("--token", default=os.environ.get("MILIASTRA_PANEL_TOKEN", ""))
    parser.add_argument("--dist", default="web/dist")
    parser.add_argument("--out", default="/tmp/panel-audit")
    parser.add_argument("--routes", default="overview,playback,login,game,entertainment,commands,aiconfig,tasks,tools,config,visual,debug,identity,settings")
    args = parser.parse_args()

    dist = Path(args.dist).resolve()
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    if not (dist / "index.html").exists():
        print(f"缺少 {dist}/index.html，请先 cd web && npm run build", file=sys.stderr)
        return 2

    index = (dist / "index.html").read_text(encoding="utf-8")
    injected = INJECT.replace("__BASE__", args.base).replace("__TOKEN__", args.token)
    patch_dir = out / "site"
    if patch_dir.exists():
        shutil.rmtree(patch_dir)
    shutil.copytree(dist, patch_dir)
    (patch_dir / "index.html").write_text(index.replace("</head>", injected + "</head>"), encoding="utf-8")

    server, port = serve(str(patch_dir))
    report = {"base": args.base, "routes": {}}
    try:
        for route in [item for item in args.routes.split(",") if item]:
            url = f"http://127.0.0.1:{port}/index.html#{route}"
            shot = out / f"{route}.png"
            dom = run_chromium(url, shot)
            audit = {"errors": [], "rejects": [], "consoleErrors": [], "http": []}
            match = re.search(r'<pre id="__audit_out">(.*?)</pre>', dom, re.S)
            if match:
                try:
                    audit = json.loads(match.group(1))
                except json.JSONDecodeError:
                    pass
            text = re.sub(r"<[^>]+>", " ", dom)
            text = re.sub(r"\s+", " ", text).strip()
            report["routes"][route] = {
                "audit": audit,
                "screenshot": str(shot) if shot.exists() else "",
                "dom_bytes": len(dom),
                "text_excerpt": text[:400],
            }
            problems = len(audit.get("errors", [])) + len(audit.get("rejects", [])) + len(audit.get("http", []))
            print(f"[{route}] errors={len(audit.get('errors', []))} rejects={len(audit.get('rejects', []))} "
                  f"http={len(audit.get('http', []))} console={len(audit.get('consoleErrors', []))} shot={'ok' if shot.exists() else 'missing'}")
            if problems:
                for key in ("errors", "rejects", "http", "consoleErrors"):
                    for item in audit.get(key, [])[:5]:
                        print(f"    {key}: {item}")
    finally:
        server.shutdown()

    (out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"报告：{out/'report.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
