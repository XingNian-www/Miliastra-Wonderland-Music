#!/usr/bin/env python3
"""对面板某个**页签**截图（页签是 Vue 内部状态，URL 切不了）。

用法：shot-tab.py <route> <页签按钮文本> <输出 png>

做法：复制一份已注入 base 的 dist（由 `panel-layout-audit.py` 产出在
`<out>/site`），在副本的 index.html 里追加一段点击脚本，用完即弃。
**不改仓库里的任何前端代码。**

必须带 `--disable-web-security`：审计工具给 `window.fetch` 打的是跨端口的 base
前缀，不关掉 web security 就全是 `Failed to fetch`，页面看起来像后端没起。
"""
import functools
import http.server
import os
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading

if len(sys.argv) < 4:
    sys.exit("用法: shot-tab.py <route> <页签文本> <输出 png>")
route, tab_text, out = sys.argv[1], sys.argv[2], sys.argv[3]

# 审计工具把注入过 base 的副本放在这里；没有就先跑 panel-visual-check.sh。
SITE_SRC = os.path.join("target", "panel-visual", "site")
if not os.path.isdir(SITE_SRC):
    sys.exit(f"缺少 {SITE_SRC}，先跑 scripts/panel-visual-check.sh")

tmp = tempfile.mkdtemp(prefix="panel-tab-")
site = os.path.join(tmp, "site")
shutil.copytree(SITE_SRC, site)

inject = f"""
<script>
setTimeout(() => {{
  const button = [...document.querySelectorAll('button.tab')]
    .find((b) => (b.textContent || '').includes({tab_text!r}));
  if (button) button.click();
}}, 1500);
</script>
"""
index = os.path.join(site, "index.html")
html = open(index, encoding="utf-8").read()
# 必须以**副本**的 index.html 为底：SITE_SRC 的原始 html 没有注入 base/token，
# 用它覆盖会把注入冲掉，页面直接报「无法连接后端」。
open(index, "w", encoding="utf-8").write(html.replace("</body>", inject + "</body>"))

handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=site)


class Quiet(socketserver.TCPServer):
    allow_reuse_address = True
    def handle_error(self, request, client_address):
        pass


srv = Quiet(("127.0.0.1", 0), handler)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

os.makedirs(os.path.dirname(os.path.abspath(out)) or ".", exist_ok=True)
subprocess.run(
    [
        "chromium",
        "--headless=new",
        "--no-sandbox",
        "--disable-gpu",
        "--disable-web-security",
        "--user-data-dir=" + tempfile.mkdtemp(prefix="panel-chrome-"),
        "--window-size=1500,1500",
        "--virtual-time-budget=15000",
        "--hide-scrollbars",
        f"--screenshot={out}",
        f"http://127.0.0.1:{port}/#{route}",
    ],
    capture_output=True,
)
print(out)
