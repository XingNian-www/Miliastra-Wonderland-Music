#!/usr/bin/env python3
"""面板布局审计：逐路由测量卡片/按钮/输入框对齐与大片空白。

用法：
  MILIASTRA_PANEL_TOKEN=xxx python3 scripts/panel-layout-audit.py --out /tmp/layout-audit [--routes a,b]
前置：web/dist 已构建（缺 <dist>/index.html 时打印中文提示并以退出码 2 结束，不抛栈）；
      本地静态服务注入 token 与远端 base。
输出：report.json + 每路由截图 + 控制台汇总。
退出码：0 = 正常跑完；2 = 缺前端产物或参数错误。
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
    // 只比较同一父容器里的卡片；左右两栏各自纵向堆叠不算同行
    const parent = card.parentElement;
    const key = String([...rows.keys()].length) + ':' + Math.round(r.top / 6);
    let bucket = [...rows.entries()].find(([, group]) => group.parent === parent && Math.abs(group.top - r.top) < 6);
    if (!bucket) {
      bucket = [parent, { parent, top: r.top, items: [] }];
      rows.set(bucket[0] === null ? key : `${key}`, bucket[1]);
    }
    bucket[1].items.push({ el: card, r });
  }
  for (const [key, group] of rows) {
    if (!group.items || group.items.length < 2) continue;
    const bottoms = group.items.map((g) => Math.round(g.r.bottom));
    if (Math.max(...bottoms) - Math.min(...bottoms) > 2) {
      out.unevenCards.push({ row: key, label: group.items.map((g) => (g.el.className || '').split(' ')[0]), bottoms });
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
    if (field.clientWidth < 600) continue; // 窄屏字段纵向堆叠是预期，不做基线比较
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
  // 令牌注入必须写 cookie：面板自决策 5-c 起只从 cookie / 内存读令牌（localStorage 只剩
  // 一条一次性迁移路径）。继续依赖 localStorage 的结果是「现在还能跑，将来清理迁移函数时
  // 静默 401」。这里与 web/src/api.js 的写入格式保持一致（path=/、SameSite=Strict），
  // 值要 encodeURIComponent —— 令牌里可能有 +/=/; 等字符。
  // 注意：本脚本是把 dist 起在 http 静态服务上再用浏览器打开，cookie 不是 Secure 也没关系。
  if (token) {
    document.cookie = 'miliastraAccessToken=' + encodeURIComponent(token) + '; path=/; SameSite=Strict';
    // 顺手清掉旧版本可能留下的副本，避免它被迁移路径读出来盖掉本次注入的值。
    try { localStorage.removeItem('miliastraAccessToken'); } catch (e) {}
  }
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
  // 二级页签（AI 配置的「记忆/请求记录」、娱乐玩法的「其他玩法」、任务页的
  // 「接口记录」等）是 Vue 内部状态，`#route` 切不到。`--click` 给一段按钮文本，
  // 在测量之前点一下，这样截图与测量看的才是那个页签。
  // 点击放在测量前 1.2s：给页签内的异步取数留出渲染时间。
  const clickText = '__CLICK__';
  if (clickText) {
    setTimeout(() => {
      const candidates = [...document.querySelectorAll('button, a, [role="tab"]')];
      const target = candidates.find((el) => (el.textContent || '').includes(clickText));
      if (target) target.click();
      else console.warn('[panel-layout-audit] 没找到要点的页签：' + clickText);
    }, 1800);
  }
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


def require_dist(dist):
    """确认前端产物存在；缺失时给出中文提示并返回错误文案。

    面板已改成 Vue + Vite（`web/`），`build.rs` 只嵌入 `web/dist`；没有产物时
    面板根本不存在，继续跑只会抛 FileNotFoundError 栈。这里提前挡住。
    """
    index = dist / "index.html"
    if index.is_file():
        return None
    if not dist.exists():
        reason = f"前端产物目录不存在：{dist}"
    elif not dist.is_dir():
        reason = f"前端产物路径不是目录：{dist}"
    else:
        reason = f"前端产物不完整：缺少 {index}"
    return (
        f"{reason}\n"
        "面板布局审计需要已构建的前端：请先在 web/ 下构建，再重跑本脚本。\n"
        "  cd web && npm install && npm run build\n"
        "（构建后 web/dist/index.html 应存在；也可用 --dist 指向其他已构建目录。）"
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://192.168.31.119:18888")
    parser.add_argument("--token", default=os.environ.get("MILIASTRA_PANEL_TOKEN", ""))
    parser.add_argument("--dist", default="web/dist")
    parser.add_argument("--out", default="/tmp/panel-layout-audit")
    parser.add_argument("--routes", default="overview,playback,login,game,entertainment,commands,aiconfig,config,visual,tools,debug,tasks,identity,settings")
    parser.add_argument("--widths", default="1500,1024,400")
    parser.add_argument("--no-screenshot", action="store_true")
    parser.add_argument(
        "--click",
        default="",
        help="按按钮文本点击一次再测量（用于二级页签；页签是 Vue 内部状态，URL 切不到）",
    )
    args = parser.parse_args()

    dist = Path(args.dist).resolve()
    missing = require_dist(dist)
    if missing:
        print(missing, file=sys.stderr)
        return 2
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    index = (dist / "index.html").read_text(encoding="utf-8")
    injected = INJECT.replace("__BASE__", args.base).replace("__TOKEN__", args.token)
    injected = injected.replace("MEASURE_JS()", "(" + MEASURE + ")()")
    # 单引号转义：页签文本会直接进 JS 字符串字面量。
    injected = injected.replace("__CLICK__", args.click.replace("\\", "\\\\").replace("'", "\\'"))
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
