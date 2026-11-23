#!/usr/bin/env bash
# 面板假数据视觉核验：一条命令，不需要游戏进程、不需要真实访问令牌。
#
# 为什么需要单独一个脚本（而不是直接跑 panel-layout-audit.py）：
#   `scripts/panel-layout-audit.py` 本身是对着**真实面板**跑的：它把 token 注入
#   cookie、把 fetch 前缀改成远端 base，然后逐路由截图。没有后端时它只会截出
#   「正在连接后端…」。假数据模式把这件事反过来：先把面板用 `--mode mock`
#   打成一份自包含产物（api.js 换成内置响应），再让审计脚本对着**本地静态产物**
#   跑（--base 留空 => 不重写 URL）。审计脚本本身一行都不改，测量口径与真实面板
#   完全相同——这样「用假数据看到的布局问题」才有意义。
#
# 为什么假数据只换 api.js 这一层：页面里加 `if (mock)` 会让核验对象变成另一份
# 代码，测出来的对齐/高度不代表生产路径。详见 web/src/mock/index.js 顶部。
#
# 用法：
#   scripts/panel-visual-check.sh                    # 构建 + 全部 14 路由
#   scripts/panel-visual-check.sh --rebuild          # 强制重建假数据产物
#   scripts/panel-visual-check.sh --routes a,b       # 只跑指定路由
#   scripts/panel-visual-check.sh --shots-only       # 只出截图，跳过测量汇总
#   scripts/panel-visual-check.sh --widths 1500,1024 # 指定视口宽度
#
# 产物（已被 .gitignore 的 **/target/ 覆盖，不入库）：
#   target/panel-visual/web-mock/            假数据构建产物（静态站点根）
#   target/panel-visual/shots/<路由>-<宽度>.png
#   target/panel-visual/report.json          测量结果（卡片对齐/控件高度/大片空白）
#   target/panel-visual/site/                审计脚本注入测量代码后的临时站点
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# 开发容器把 node/npm 装在 /usr/local/bin；CI 由 setup-node 注入 PATH。
export PATH="/usr/local/bin:${PATH}"

OUT_DIR="target/panel-visual"
WEB_MOCK="${OUT_DIR}/web-mock"
SHOTS="${OUT_DIR}/shots"
ROUTES="overview,playback,login,game,entertainment,commands,aiconfig,config,visual,tools,debug,tasks,identity,settings"
WIDTHS="1500,1024"
REBUILD=0
SHOTS_ONLY=0

sed_help() {
  awk 'NR>1 { if ($0 ~ /^set -euo pipefail$/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rebuild) REBUILD=1; shift ;;
    --shots-only) SHOTS_ONLY=1; shift ;;
    --routes) ROUTES="${2:?--routes 需要一个逗号分隔的路由列表}"; shift 2 ;;
    --routes=*) ROUTES="${1#*=}"; shift ;;
    --widths) WIDTHS="${2:?--widths 需要逗号分隔的宽度列表}"; shift 2 ;;
    --widths=*) WIDTHS="${1#*=}"; shift ;;
    -h|--help) sed_help; exit 0 ;;
    *) echo "未知参数：$1（--help 查看用法）" >&2; exit 2 ;;
  esac
done

command -v npm >/dev/null 2>&1 || { echo "找不到 npm（面板构建需要 Node 18+）" >&2; exit 127; }
command -v chromium >/dev/null 2>&1 || { echo "找不到 chromium（截图与测量需要无头浏览器）" >&2; exit 127; }
command -v python3 >/dev/null 2>&1 || { echo "找不到 python3（审计脚本需要）" >&2; exit 127; }

# 假数据 fixture 来自 Rust schema 快照；不同步时截图里的字段名/默认值就是错的，
# 而这种错误只会在肉眼比对时才发现。先挡住，给出可执行的修复命令。
if ! python3 scripts/panel-mock-gen.py --check >/dev/null; then
  echo "假数据与 Rust schema 不同步，重新生成中…" >&2
  python3 scripts/panel-mock-gen.py
fi

# 为什么默认不重建：Vite 构建约 2 秒，但每次都重建会把「上一轮截图」对应的
# 产物冲掉，对比改动前后差异时反而容易看错版本。--rebuild 明确要求才重建。
if [[ "$REBUILD" -eq 1 || ! -f "${WEB_MOCK}/index.html" ]]; then
  echo "构建面板假数据产物：${WEB_MOCK}"
  ( cd web && npx vite build --mode mock --outDir "../${WEB_MOCK}" --logLevel error )
else
  echo "复用已有假数据产物：${WEB_MOCK}（--rebuild 可强制重建）"
fi

# 局部性自检：假数据产物必须真的带着内置响应，否则面板会去连不存在的后端，
# 截出来全是「正在连接后端…」——那种截图比没有截图更误导。
if ! grep -q 'mockFetch' "${WEB_MOCK}"/assets/app-*.js; then
  echo "::error::${WEB_MOCK} 里没有假数据代码：构建模式可能没生效（应为 --mode mock）" >&2
  exit 1
fi

rm -rf "${SHOTS}"
mkdir -p "${SHOTS}"

# 审计脚本自己会起一个静态服务（SPA 的入口是 index.html + hash 路由，
# `python3 -m http.server` 也能响应，但那样还要管端口与进程；脚本已内置
# ThreadingHTTPServer，复用它可以少一份退出清理逻辑）。
#
# --base 留空：INJECT 里的 `if (typeof url === 'string' && url.startsWith('/'))`
# 只在 base 非空时才重写 URL；空 base 下请求留在当前静态站点，交给假数据层。
# --token 也留空：假数据不看令牌，注入 cookie 反而会让页面显示「已授权」的假象。
AUDIT_ARGS=(
  --dist "${WEB_MOCK}"
  --out "${OUT_DIR}"
  --routes "${ROUTES}"
  --widths "${WIDTHS}"
  --base ""
  --token ""
)
echo "开始核验：路由=${ROUTES} 宽度=${WIDTHS}"
python3 scripts/panel-layout-audit.py "${AUDIT_ARGS[@]}"

# 审计脚本按「路由-宽度.png」把截图写在 --out 根目录；收进 shots/ 便于整目录带走。
shopt -s nullglob
moved=0
for file in "${OUT_DIR}"/*.png; do
  mv "$file" "${SHOTS}/"
  moved=$((moved + 1))
done
shopt -u nullglob

echo
if [[ "$moved" -eq 0 ]]; then
  echo "::error::没有生成任何截图（${OUT_DIR}/*.png）" >&2
  exit 1
fi
if [[ "$SHOTS_ONLY" -eq 0 ]]; then
  echo "截图 ${moved} 张：${SHOTS}/"
  ls -1 "${SHOTS}"
  echo "测量报告：${OUT_DIR}/report.json"
else
  echo "截图 ${moved} 张：${SHOTS}/（--shots-only，未整理问题清单）"
fi
