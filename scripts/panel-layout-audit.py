#!/usr/bin/env python3
"""面板布局审计：逐路由测量卡片/按钮/输入框对齐与大片空白。

用法：
  MILIASTRA_PANEL_TOKEN=xxx python3 scripts/panel-layout-audit.py --out /tmp/layout-audit [--routes a,b]
前置：web/dist 已构建；本地静态服务注入 token 与远端 base。
输出：report.json + 每路由截图 + 控制台汇总。
"""
import argparse
import http.server
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

MEASURE = r"""
() => {
  const out = { overflow: null, unevenCards: [], unevenRows: [], bigBlanks: [], fieldMisalign: [] };
  const vw = document.documentElement.clientWidth;
  out.overflow = document.documentElement.scrollWidth > vw + 1 ? { scrollWidth: document.documentElement.scrollWidth, vw } : null;

  const visible = (el) => {
    const style = getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden') return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };

  // 同一网格行里的卡片底边是否对齐
  const cards = [...document.querySelectorAll('.panel, .config-group-card')].filter(visible);
  const rows = new Map();
  for (const card of cards) {
    const r = card.getBoundingClientRect();
    const key = Math.round(r.top / 6);
    if (!rows.has(key)) rows.set(key, []);
    rows.get(key).push({ el: card, r });
  }
  for (const [key, group] of rows) {
    if (group.length < 2) continue;
    const bottoms = group.map((g) => Math.round(g.r.bottom));
    if (Math.max(...bottoms) - Math.min(...bottoms) > 2) {
      out.unevenCards.push({ row: key, label: group.map((g) => (g.el.className || '').split(' ')[0]), bottoms });
    }
  }

  // 行内按钮/输入框高度与垂直中心
  const rowSelectors = ['.controls', '.tools-two', '.tools-grid-row', '.tools-xyz', '.tools-ai-row', '.config-field-control', '.top-actions', '.panel-head', '.config-status', '.play-mode-row', '.overview-quick-grid'];
  for (const sel of rowSelectors) {
    for (const container of document.querySelectorAll(sel)) {
      if (!visible(container)) continue;
      const children = [...container.children].filter((el) => visible(el) && (el.matches('button, .btn, input, select')));
      if (children.length < 2) continue;
      const r0 = children[0].getBoundingClientRect();
      const sameRow = children.every((el) => Math.abs(el.getBoundingClientRect().top - r0.top) < 4);
      if (!sameRow) continue;
      const heights = children.map((el) => Math.round(el.getBoundingClientRect().height));
      if (Math.max(...heights) - Math.min(...heights) > 2) {
        out.unevenRows.push({ selector: sel, heights, labels: children.map((el) => (el.textContent || el.value || el.tagName).trim().slice(0, 12)) });
      }
    }
  }

  // 卡片内大片空白：正文最后元素底部到卡片底部的距离
  for (const panel of document.querySelectorAll('.panel, .config-group-card')) {
    if (!visible(panel)) continue;
    const body = panel.querySelector('.panel-body');
    if (!body) continue;
    const bodyRect = body.getBoundingClientRect();
    if (bodyRect.height < 120) continue;
    const kids = [...body.children].filter(visible);
    if (!kids.length) continue;
    const lastBottom = Math.max(...kids.map((el) => el.getBoundingClientRect().bottom));
    const blank = Math.round(bodyRect.bottom - lastBottom);
    if (blank > 120) {
      out.bigBlanks.push({ panel: (panel.className || '').split(' ')[0], blank, height: Math.round(bodyRect.height) });
    }
  }

  // 配置字段：label / control / side 是否同一垂直中心
  for (const field of document.querySelectorAll('#configForm .config-field')) {
    const label = field.querySelector('.config-field-label');
    const control = field.querySelector('.config-field-control');
    const side = field.querySelector('.config-field-side');
    if (!label || !control) continue;
    const centers = [label, control, side].filter(Boolean).map((el) => {
      const r = el.getBoundingClientRect();
      return r.top + r.height / 2;
    });
    if (Math.max(...centers) - Math.min(...centers) > 6) {
      out.fieldMisalign.push({ label: (label.textContent || '').trim().slice(0, 16), centers: centers.map((c) => Math.round(c)) });
    }
  }
  return out;
}
"""

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
  setTimeout(() => {
    const pre = document.createElement('pre');
    pre.id = '__layout_measure';
    try { pre.textContent = JSON.stringify(MEASURE_JS()); } catch (error) { pre.textContent = JSON.stringify({ error: String(error) }); }
    document.body.appendChild(pre);
  }, 4500);
})();
</script>
"""


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def serve(directory):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), lambda *a, **kw: Quiet(*a, directory=directory, **kw))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def run_chromium(url, screenshot, width, timeout=60):
    cmd = [
        "chromium", "--headless=new", "--no-sandbox", "--disable-gpu", "--disable-web-security",
        f"--user-data-dir={tempfile.mkdtemp(prefix='layout-chrome-')}",
        f"--window-size={width},1200", "--virtual-time-budget=12000",
        f"--screenshot={screenshot}", "--dump-dom", url,
    ]
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout
    except subprocess.TimeoutExpired:
        return ""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://192.168.31.119:18888")
    parser.add_argument("--token", default=os.environ.get("MILIASTRA_PANEL_TOKEN", ""))
    parser.add_argument("--dist", default="web/dist")
    parser.add_argument("--out", default="/tmp/panel-layout-audit")
    parser.add_argument("--routes", default="overview,playback,login,game,entertainment,commands,aiconfig,config,visual,tools,debug,tasks,identity,settings")
    parser.add_argument("--widths", default="1500,1024,400")
    parser.add_argument("--no-screenshot", action="store_true")
    args = parser.parse_args()

    dist = Path(args.dist).resolve()
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    index = (dist / "index.html").read_text(encoding="utf-8")
    injected = INJECT.replace("__BASE__", args.base).replace("__TOKEN__", args.token)
    injected = injected.replace("MEASURE_JS()", "(" + MEASURE + ")()")
    site = out / "site"
    if site.exists():
        shutil.rmtree(site)
    shutil.copytree(dist, site)
    (site / "index.html").write_text(index.replace("</head>", injected + "</head>"), encoding="utf-8")
    server, port = serve(str(site))
    report = {"base": args.base, "routes": {}}
    try:
        for width in [int(item) for item in args.widths.split(",") if item]:
            for route in [item for item in args.routes.split(",") if item]:
                url = f"http://127.0.0.1:{port}/index.html#{route}"
                shot = out / f"{route}-{width}.png"
                dom = run_chromium(url, shot, width)
                result = {"error": "no measurement"}
                match = re.search(r'<pre id="__layout_measure">(.*?)</pre>', dom, re.S)
                if match:
                    try:
                        result = json.loads(match.group(1))
                    except json.JSONDecodeError:
                        pass
                report["routes"][f"{route}@{width}"] = result
                issues = sum(len(result.get(key, [])) for key in ("unevenCards", "unevenRows", "bigBlanks", "fieldMisalign") if isinstance(result.get(key), list))
                overflow = "overflow" if result.get("overflow") else ""
                print(f"[{route}@{width}] issues={issues} {overflow}")
                for key in ("unevenCards", "unevenRows", "bigBlanks", "fieldMisalign"):
                    for item in (result.get(key) or [])[:4]:
                        print(f"    {key}: {item}")
    finally:
        server.shutdown()
    (out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"报告：{out/'report.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
