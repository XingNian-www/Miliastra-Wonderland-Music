#!/usr/bin/env python3
"""浅色 / 暗色主题对照截图（复用 panel-layout-audit.py 的注入方式）。

为什么单独一个脚本：
  `scripts/panel-visual-check.sh` 截的是「浏览器 localStorage 里的主题」，无头浏览器
  每次都是全新 profile，所以永远只能拿到默认暗色。要核验浅色主题，必须在页面脚本
  之前把 `document.documentElement.dataset.theme` 定死。

做法与 panel-layout-audit.py 完全一致（不另造一套）：
  - 把已经构建好的假数据产物整目录复制到 <out>/site；
  - 在 index.html 的 </head> 前注入一段脚本；
  - 用 ThreadingHTTPServer 起静态服务，chromium --headless=new 截图。

注入脚本只做三件事：写 dataset.theme、拦掉 localStorage 的读写（避免面板自己在
initTheme() 里把主题改回存储值）、可选点击一个页签。

用法：
  python3 scripts/panel-theme-shot.py --dist target/panel-visual/web-mock \\
      --out target/panel-visual/light --theme light --routes overview,config --widths 1500
产物：<out>/<route>-<width>-<theme>.png
"""
import argparse
import http.server
import shutil
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

INJECT = """
<style>
/* __NOTRANSITION__ 占位：headless 的 --virtual-time-budget 会让 CSS transition
   停在中间帧（实测 focus 光晕算出 rgba(0,0,0,0)），截图里就看不到它。
   加 --no-transition 时这里换成禁用过渡的规则，让截图反映「过渡结束」的真实样式；
   这是截图工具的处理，产品样式本身没有关过渡。 */
__NOTRANSITION__
</style>
<script>
(() => {
  // 与 panel-layout-audit.py 同样在 </head> 前注入，但这里锁死主题。
  // 关键：主题真正生效靠的是 **HTML 属性** `data-theme`（CSS 选择器
  // `html[data-theme="light"]` 只认属性，不认 JS 的 dataset 代理对象）。
  // 所以这里既要写属性，也要盯住后续写入——面板的 initTheme()/toggleTheme()
  // 会再写一次 dataset.theme，不盯住就会被它覆盖回存储值。
  const THEME = '__THEME__';
  const CLICK = '__CLICK__';
  const FOCUS = '__FOCUS__';
  const root = document.documentElement;
  const pin = () => {
    if (root.getAttribute('data-theme') !== THEME) root.setAttribute('data-theme', THEME);
  };
  pin();
  new MutationObserver(pin).observe(root, { attributes: true, attributeFilter: ['data-theme'] });
  // 存储层也钉死：initTheme() 读 localStorage 决定写什么，这里让它读到目标主题。
  try {
    Object.defineProperty(window, 'localStorage', {
      configurable: true,
      get() {
        return {
          getItem: (k) => (k === 'miliastraTheme' ? THEME : null),
          setItem: () => {},
          removeItem: () => {},
          clear: () => {},
          key: () => null,
          length: 0,
        };
      },
    });
  } catch (e) {}
  if (CLICK) {
    setTimeout(() => {
      const target = [...document.querySelectorAll('button, a, [role="tab"]')]
        .find((el) => (el.textContent || '').includes(CLICK));
      if (target) target.click();
    }, 1800);
  }
  // --focus <selector>：把焦点打到指定控件上，用来核验 focus 光晕实际长什么样。
  // 用 .focus() 而不是 :hover，因为无头截图里 hover 状态没法自然触发。
  if (FOCUS) {
    setTimeout(() => {
      const el = document.querySelector(FOCUS);
      if (el) el.focus();
      else console.warn('[panel-theme-shot] 没找到要聚焦的元素：' + FOCUS);
    }, 2400);
  }
})();
</script>
"""


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def serve(directory):
    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), lambda *a, **kw: Quiet(*a, directory=directory, **kw)
    )
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def shot(url, path, width, theme, timeout=90):
    cmd = [
        "chromium",
        "--headless=new",
        "--no-sandbox",
        "--disable-gpu",
        "--disable-web-security",
        f"--user-data-dir={tempfile.mkdtemp(prefix='theme-chrome-')}",
        f"--window-size={width},1200",
        "--virtual-time-budget=12000",
        f"--screenshot={path}",
        url,
    ]
    # 强制浅色：chromium 自己的 prefers-color-scheme 也一起钉住，避免
    # `@media (prefers-color-scheme)` 与 data-theme 打架。
    if theme == "light":
        cmd.insert(1, "--force-prefers-color-scheme=light")
    try:
        subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        print(f"超时：{url}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dist", default="target/panel-visual/web-mock")
    ap.add_argument("--out", default="target/panel-visual/light")
    ap.add_argument("--routes", default="overview,config")
    ap.add_argument("--widths", default="1500")
    ap.add_argument("--theme", default="light", choices=["light", "dark"])
    ap.add_argument("--click", default="")
    ap.add_argument("--focus", default="", help="截图前聚焦的 CSS 选择器（核验 focus 光晕）")
    ap.add_argument(
        "--no-transition",
        action="store_true",
        help="截图前禁用 CSS 过渡（headless 虚拟时间会把过渡冻结在中间帧）",
    )
    ap.add_argument("--suffix", default="", help="文件名附加后缀，避免覆盖已有截图")
    args = ap.parse_args()

    dist = Path(args.dist).resolve()
    if not (dist / "index.html").is_file():
        print(f"缺少前端产物：{dist}/index.html（先跑 scripts/panel-visual-check.sh）", file=sys.stderr)
        return 2

    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    site = out / "site"
    if site.exists():
        shutil.rmtree(site)
    shutil.copytree(dist, site)

    inject = (
        INJECT.replace("__THEME__", args.theme)
        .replace("__CLICK__", args.click.replace("\\", "\\\\").replace("'", "\\'"))
        .replace("__FOCUS__", args.focus.replace("\\", "\\\\").replace("'", "\\'"))
        .replace(
            "__NOTRANSITION__",
            "*, *::before, *::after { transition: none !important; }" if args.no_transition else "",
        )
    )
    index = (site / "index.html").read_text(encoding="utf-8")
    (site / "index.html").write_text(index.replace("</head>", inject + "</head>"), encoding="utf-8")

    server, port = serve(str(site))
    made = 0
    try:
        for width in [w for w in args.widths.split(",") if w]:
            for route in [r for r in args.routes.split(",") if r]:
                url = f"http://127.0.0.1:{port}/index.html#{route}"
                png = out / f"{route}-{width}-{args.theme}{args.suffix}.png"
                shot(url, png, width, args.theme, )
                if png.is_file():
                    made += 1
                    print(f"{png.relative_to(Path.cwd())}")
    finally:
        server.shutdown()
    if not made:
        print("没有生成任何截图", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
