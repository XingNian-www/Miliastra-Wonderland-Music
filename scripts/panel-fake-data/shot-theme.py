#!/usr/bin/env python3
"""按指定主题截图（面板视觉核验用）。

用法：shot-theme.py <dist 目录> <路由> <dark|light> <输出 png>

为什么需要单独一个脚本：`panel-visual-check.sh` 只截默认（暗色）主题。
浅色主题的样式回归没法靠暗色截图发现——历史上它就被漏掉过一次。

关键：主题必须写进 **localStorage** 再加载页面。`main.js` 首帧前会读
`miliastraTheme` 并覆盖 `data-theme`，只在 `<head>` 里设 dataset 会被冲掉，
截出来仍是暗色（看起来像「浅色主题坏了」）。
"""
import functools, http.server, socketserver, subprocess, tempfile, threading, os, sys
site, route, theme, out = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
# 必须在 main.js 之前写 localStorage：main.js 首帧前会读 miliastraTheme
# 并**覆盖** data-theme，直接设 dataset 会被冲掉（第一版就踩了这个坑，
# 截出来仍是暗色，看着像「浅色主题坏了」）。
INJECT = f"""
<script>
try {{ localStorage.setItem('miliastraTheme', {theme!r}); }} catch (e) {{}}
document.documentElement.dataset.theme = {theme!r};
</script>
"""
tmp=tempfile.mkdtemp(); os.system(f"cp -r {site} {tmp}/site")
idx=f"{tmp}/site/index.html"; html=open(idx,encoding="utf-8").read()
open(idx,"w",encoding="utf-8").write(html.replace("</head>", INJECT+"</head>"))
class Q(socketserver.TCPServer):
    allow_reuse_address=True
    def handle_error(self,*a): pass
srv=Q(("127.0.0.1",0), functools.partial(http.server.SimpleHTTPRequestHandler, directory=f"{tmp}/site"))
port=srv.server_address[1]
threading.Thread(target=srv.serve_forever,daemon=True).start()
subprocess.run(["chromium","--headless=new","--no-sandbox","--disable-gpu","--disable-web-security",
  "--user-data-dir="+tempfile.mkdtemp(),"--window-size=1500,1200","--virtual-time-budget=11000",
  f"--screenshot={out}", f"http://127.0.0.1:{port}/#{route}"],capture_output=True)
print(out)
