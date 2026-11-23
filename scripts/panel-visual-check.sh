#!/usr/bin/env bash
# scripts/panel-visual-check.sh
#
# 面板改动的**视觉核验**：假数据起服 → chromium 截图 → 布局审计。
#
# 为什么需要它：面板是 Vue SPA，任何前端改动都得有人肉眼看一眼才发现
# 「数字渲染错 / 卡片错位 / 整块空白」。但 `scripts/panel-layout-audit.py`
# 默认连**真实面板**（要 token + 一台正在跑游戏的机器），于是每次改样式都要
# 找人开服；实际上前端改动的视觉核验根本不需要真实后端。
#
# 两个已踩过的坑（都写进用法里，别再踩）：
#   1) 审计工具给 `window.fetch` 打 base 前缀后是**跨端口**请求，chromium 必须带
#      `--disable-web-security`，否则所有接口都是 `Failed to fetch`，
#      页面看起来像「后端没起来」。
#   2) 直接打开 `/` 会停在默认页（配置中心）。要看某个页面必须用 hash 路由
#      （`#entertainment`）；页签是 Vue 内部状态，URL 切不了，得点一下按钮。
#
# 用法：
#   scripts/panel-visual-check.sh                     # 审计全部路由
#   scripts/panel-visual-check.sh --routes entertainment
#   scripts/panel-visual-check.sh --tab 其他玩法 --route entertainment
#
# 产物：target/panel-visual/{report.json,<route>-<width>.png}（target/ 已 gitignore）
set -euo pipefail

cd "$(dirname "$0")/.."

ROUTES="overview,playback,login,game,entertainment,commands,aiconfig,config,visual,tools,debug,tasks,identity,settings"
WIDTHS="1500,1024,400"
TAB=""
PORT_FAKE="${PORT_FAKE:-18899}"

while [ $# -gt 0 ]; do
    case "$1" in
        --routes) ROUTES="$2"; shift 2 ;;
        --widths) WIDTHS="$2"; shift 2 ;;
        --tab) TAB="$2"; shift 2 ;;
        --port) PORT_FAKE="$2"; shift 2 ;;
        *) echo "未知参数: $1" >&2; exit 2 ;;
    esac
done

[ -f web/dist/index.html ] || {
    echo "缺少 web/dist，先构建：cd web && npm run build" >&2
    exit 2
}

OUT="target/panel-visual"
mkdir -p "$OUT"

# 1) 假数据服务（后台，退出时清理）
python3 scripts/panel-fake-data/server.py "$PORT_FAKE" &
FAKE_PID=$!
trap 'kill "$FAKE_PID" 2>/dev/null || true' EXIT
sleep 1
curl -sf -o /dev/null "http://127.0.0.1:${PORT_FAKE}/monitor" || {
    echo "假数据服务未就绪（端口 ${PORT_FAKE}）" >&2
    exit 1
}

# 2) 布局审计（截图 + 测量）
python3 scripts/panel-layout-audit.py \
    --base "http://127.0.0.1:${PORT_FAKE}" \
    --token fake-token \
    --out "$OUT" \
    --routes "$ROUTES" \
    --widths "$WIDTHS"

# 3) 可选：点开某个页签再截一张（页签是 Vue 内部状态，审计工具切不了）
if [ -n "$TAB" ]; then
    ROUTE="${ROUTES%%,*}"
    python3 scripts/panel-fake-data/shot-tab.py "$ROUTE" "$TAB" "$OUT/tab-${TAB}.png"
fi

echo
echo "产物："
find "$OUT" -maxdepth 1 -name '*.png' -o -maxdepth 1 -name 'report.json' | sort
