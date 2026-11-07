#!/usr/bin/env bash
# scripts/release-pipeline.sh
#
# 7.4.x 及后续版本的「发布 + 多机部署」可复用流程脚本（已纳入仓库，随版本发布）。
#
# 为什么有这个脚本：每次发版都要记住一串容易漏的约束——
#   * 推送必须绕开全局 insteadOf / 镜像重写，否则 TLS 被改写；
#   * 版本号提交会自动触发 CI，手动 workflow_dispatch 不能重复触发同一个 Release；
#   * GitHub 大文件直连会反复截断，必须走 socks5h 代理 + 断点续传 + 精确字节数校验；
#   * Windows exe 被运行中的进程锁住，必须先停进程再替换，且必须用计划任务启动；
#   * 多台部署主机同名同配置，只能按 --hosts 逐台部署并各自验收。
#
# 环境变量（--execute 时按步骤需要；dry-run 全部不需要，缺值自动打印占位符）：
#   MILIASTRA_REPO            仓库 owner/name；--execute 缺省时从 git remote / gh repo view 推导，dry-run 用占位符
#   MILIASTRA_DEPLOY_HOSTS    空格分隔的部署主机（或 --hosts）
#   MILIASTRA_PROXY_HOST / MILIASTRA_PROXY_PORT         下载用 socks5h 代理（或 --proxy）
#   MILIASTRA_SSH_USER / MILIASTRA_SSH_KEY / MILIASTRA_REMOTE_DIR   Windows 目标信息
#   MILIASTRA_PANEL_PORT      面板端口（或 --panel-port）
#   MILIASTRA_PANEL_TOKEN     面板令牌（只从环境/密钥管理器取，绝不写进文件）
#   MILIASTRA_TASK_NAME       计划任务名（可选）
#
# 设计原则：
#   1) **默认 dry-run**：只打印将执行的命令；只有显式 `--execute` 才做远端写操作
#      （push / workflow_dispatch / 下载 / 部署 / commit）。
#   2) 不重复造轮子：部署复用 scripts/deploy-exe.sh，代理下载 + 首台单机链路
#      复用 technical-docs/scripts/deploy-remote.sh（后者在 gitignored 的
#      technical-docs 下，克隆仓库后需在本机补齐该组件）。
#      本脚本自身已随仓库版本化：commit 步骤用 `git add -A -- .` 正常暂存。
#   3) 提交信息不允许任何 trailer（AGENTS.md 明确要求）。
#   4) 验收只注入「应被拒绝」的命令，避免在真实公共大厅刷屏。
#   5) 远程面板令牌**绝不写进文件/提交/报告**：只从 `MILIASTRA_PANEL_TOKEN` 读取。
#   6) 部署前经 `POST /chat/send` 对**每台被部署主机各发一次**预告，
#      提示「5 秒后将更新版本，期间会有短暂停止」，全部发完后统一 sleep 5 秒
#      才动 exe；避免只提醒其中一台导致另一台玩家毫无准备。
#      默认主机集合 = 去重后的 `--hosts`，`--announce-hosts` 可显式覆盖，
#      `--skip-announce` 可整体跳过。
#   7) 环境拓扑/账号/路径/密钥/端口全部外置；仓库名缺省时动态推导。
#
# 用法：
#   scripts/release-pipeline.sh --version 7.4.1 --step all
#   scripts/release-pipeline.sh --version 7.4.1 --step all --execute \
#       --message "fix(chat): ..." --dispatch
#   # 需要 --execute 且涉及 announce / verify 时，先从环境/密钥管理器注入令牌：
#   export MILIASTRA_PANEL_TOKEN='<从密钥管理器取得的令牌>'
#   scripts/release-pipeline.sh --version 7.4.1 --step verify --execute
#   # 只想单独对每台部署主机各发一次更新预告：
#   scripts/release-pipeline.sh --version 7.4.1 --step announce --execute
#   # CI 已完成、只想补部署：
#   scripts/release-pipeline.sh --version 7.4.1 --step deploy --execute \
#       --run-id <RUN_ID>
#   # 已有 artifact：
#   scripts/release-pipeline.sh --version 7.4.1 --step deploy --execute \
#       --artifact-id <ID> --expected-bytes <BYTES>
#
# 步骤（--step）：all | preflight | test | commit | push | dispatch | wait |
#                release | artifact | download | announce | deploy | verify
#   all = preflight→test→commit(可选)→push→dispatch(可选)→wait→release→
#         artifact→announce→deploy→verify

set -euo pipefail

# 环境拓扑/账号/路径/密钥/端口只从环境变量或 CLI 取，绝不写死。
REPO="${MILIASTRA_REPO:-}"
WORKFLOW='build-windows-exe.yml'
BRANCH='main'
PROXY_HOST="${MILIASTRA_PROXY_HOST:-}"
PROXY_PORT="${MILIASTRA_PROXY_PORT:-}"
REMOTE_USER="${MILIASTRA_SSH_USER:-}"
SSH_KEY="${MILIASTRA_SSH_KEY:-}"
REMOTE_DIR="${MILIASTRA_REMOTE_DIR:-}"
PANEL_PORT="${MILIASTRA_PANEL_PORT:-}"
TASK_NAME="${MILIASTRA_TASK_NAME:-Miliastra}"
EXE_NAME='miliastra-wonderland-music.exe'
PANEL_TOKEN="${MILIASTRA_PANEL_TOKEN:-}"
ANNOUNCE_TEXT='5秒后将更新版本，期间会有短暂停止，请稍候。'
ANNOUNCE_DELAY=5
ANNOUNCE_HOSTS=''   # 空=默认对 HOSTS 去重后的每一台被部署主机各发一次
SKIP_ANNOUNCE=0
ARTIFACT_NAME='miliastra-wonderland-music-windows-x64-openvino'
DEPLOY_EXE="scripts/deploy-exe.sh"
DEPLOY_REMOTE="technical-docs/scripts/deploy-remote.sh"

DRY=1
VERSION=''
STEP='all'
RUN_ID=''
ARTIFACT_ID=''
EXPECTED_BYTES=''
MESSAGE=''
DISPATCH=0
HOSTS="${MILIASTRA_DEPLOY_HOSTS:-}"
DEPLOY_MODE='components'   # components=复用两个既有脚本；local=自己下载后逐台 deploy-exe.sh
WORK=''

usage() {
  awk 'NR>1 { if ($0 ~ /^set -euo pipefail$/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
  exit 1
}

log()  { printf '\n== %s\n' "$*"; }
warn() { printf '!! %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# 从当前仓库 origin 推导 owner/name，支持 https / 代理镜像 / SCP 形式；不写死账号。
detect_repo_from_remote() {
  local url slug
  url="$(git remote get-url origin 2>/dev/null || true)"
  [[ -n "$url" ]] || return 1
  slug="$(printf '%s\n' "$url" \
    | sed -E 's#\.git$##; s#^[^@]+@[^:]+:#/#; s#^[a-zA-Z0-9+.-]+://##' \
    | awk -F/ 'NF>=2 { print $(NF-1) "/" $NF }')"
  [[ "$slug" =~ ^[^/[:space:]]+/[^/[:space:]]+$ ]] || return 1
  printf '%s' "$slug"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="${2#v}"; shift 2 ;;
    --step) STEP="$2"; shift 2 ;;
    --execute) DRY=0; shift ;;
    --message) MESSAGE="$2"; shift 2 ;;
    --dispatch) DISPATCH=1; shift ;;
    --run-id) RUN_ID="$2"; shift 2 ;;
    --artifact-id) ARTIFACT_ID="$2"; shift 2 ;;
    --expected-bytes) EXPECTED_BYTES="$2"; shift 2 ;;
    --hosts) HOSTS="$2"; shift 2 ;;
    --deploy-mode) DEPLOY_MODE="$2"; shift 2 ;;
    --proxy) PROXY_HOST="${2%%:*}"; PROXY_PORT="${2##*:}"; shift 2 ;;
    --ssh-key) SSH_KEY="$2"; shift 2 ;;
    --repo) REPO="$2"; shift 2 ;;
    --ssh-user) REMOTE_USER="$2"; shift 2 ;;
    --remote-dir) REMOTE_DIR="$2"; shift 2 ;;
    --panel-port) PANEL_PORT="$2"; shift 2 ;;
    --task-name) TASK_NAME="$2"; shift 2 ;;
    --announce-hosts) ANNOUNCE_HOSTS="$2"; shift 2 ;;
    --announce-text) ANNOUNCE_TEXT="$2"; shift 2 ;;
    --announce-delay) ANNOUNCE_DELAY="$2"; shift 2 ;;
    --skip-announce) SKIP_ANNOUNCE=1; shift ;;
    -h|--help) usage ;;
    *) die "未知参数: $1" ;;
  esac
done

# dry-run 只打印；execute 才真正执行。命令数组不含令牌，令牌只在实际执行时注入。
run() {
  local desc="$1"; shift
  if [[ $DRY -eq 1 ]]; then
    printf '  [dry-run] %s\n' "$desc"
    printf '            $ %s\n' "$*"
    return 0
  fi
  log "$desc"
  "$@"
}

read_package_version() {
  awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
      if (match($0, /"[^"]+"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
    }
  ' Cargo.toml
}

preflight() {
  log '前置检查（只读）'
  [[ -f Cargo.toml ]] || die '请在仓库根目录运行'
  local cargo_version
  cargo_version="$(read_package_version)"
  [[ -n "$cargo_version" ]] || die '无法从 Cargo.toml 读取 [package].version'
  if [[ -z "$VERSION" ]]; then VERSION="${cargo_version%-dev*}"; fi
  [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "版本号必须是 X.Y.Z：$VERSION"
  [[ "$VERSION" == "$cargo_version" ]] || warn "Cargo.toml=$cargo_version 与脚本版本=$VERSION 不一致"
  # 仓库名：CLI/环境变量 > 执行期从 git remote / gh repo view 推导 > dry-run 占位符。
  # dry-run 不打印本机 remote 里的账号名，避免日志外泄。
  if [[ -z "$REPO" ]]; then
    if [[ $DRY -eq 1 ]]; then
      REPO='<OWNER>/<REPO>'
      warn 'dry-run 用 <OWNER>/<REPO> 占位；--execute 时从 git remote / gh repo view 推导'
    else
      REPO="$(detect_repo_from_remote || true)"
      if [[ -z "$REPO" ]]; then
        REPO="$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)"
      fi
      [[ -n "$REPO" ]] || die '请设置 MILIASTRA_REPO / --repo，或让 git remote / gh repo view 可用'
    fi
  fi
  # dry-run 不需要任何环境：缺值统一用占位符，保证 --step all 能完整走完。
  if [[ $DRY -eq 1 ]]; then
    [[ -n "$HOSTS" ]] || HOSTS='<HOST>'
    [[ -n "$PROXY_HOST" ]] || PROXY_HOST='<PROXY_HOST>'
    [[ -n "$PROXY_PORT" ]] || PROXY_PORT='<PROXY_PORT>'
    [[ -n "$REMOTE_USER" ]] || REMOTE_USER='<USER>'
    [[ -n "$SSH_KEY" ]] || SSH_KEY='<SSH_KEY>'
    [[ -n "$REMOTE_DIR" ]] || REMOTE_DIR='<REMOTE_DIR>'
    [[ -n "$PANEL_PORT" ]] || PANEL_PORT='<PORT>'
  else
    # --execute 提前校验必要环境，避免跑一半才失败。
    local requires=''
    case "$STEP" in
      all) requires='hosts ssh remote_dir panel_port token proxy' ;;
      deploy)
        requires='hosts ssh remote_dir panel_port proxy'
        [[ $SKIP_ANNOUNCE -eq 1 ]] || requires="$requires token"
        ;;
      verify) requires='hosts ssh remote_dir panel_port token' ;;
      announce)
        if [[ $SKIP_ANNOUNCE -eq 0 ]]; then requires='hosts panel_port token'; fi
        ;;
      download) requires='proxy' ;;
    esac
    for need in $requires; do
      case "$need" in
        hosts) [[ -n "$HOSTS" ]] || die '请设置 MILIASTRA_DEPLOY_HOSTS 或 --hosts' ;;
        ssh) [[ -n "$REMOTE_USER" && -n "$SSH_KEY" ]] || die '请设置 MILIASTRA_SSH_USER / MILIASTRA_SSH_KEY（或 --ssh-user / --ssh-key）' ;;
        remote_dir) [[ -n "$REMOTE_DIR" ]] || die '请设置 MILIASTRA_REMOTE_DIR 或 --remote-dir' ;;
        panel_port) [[ -n "$PANEL_PORT" ]] || die '请设置 MILIASTRA_PANEL_PORT 或 --panel-port' ;;
        token) [[ -n "$PANEL_TOKEN" ]] || die '请先导出环境变量 MILIASTRA_PANEL_TOKEN（令牌不得写入文件/提交/报告）' ;;
        proxy) [[ -n "$PROXY_HOST" && -n "$PROXY_PORT" ]] || die '请设置 MILIASTRA_PROXY_HOST / MILIASTRA_PROXY_PORT 或 --proxy' ;;
      esac
    done
    [[ "$PANEL_PORT" =~ ^[0-9]+$ ]] || die 'MILIASTRA_PANEL_PORT 必须是数字端口'
  fi
  # commit / test / preflight 不创建 Release；dry-run 缺发布说明只告警。
  if [[ "$STEP" != 'commit' && "$STEP" != 'test' && "$STEP" != 'preflight' ]]; then
    if [[ ! -f ".github/release-notes/v${VERSION}.md" ]]; then
      if [[ $DRY -eq 1 ]]; then
        warn "缺少发布说明 .github/release-notes/v${VERSION}.md（dry-run 继续）"
      else
        die "缺少发布说明 .github/release-notes/v${VERSION}.md"
      fi
    fi
  fi
  [[ -f "$DEPLOY_EXE" ]] || die "缺少组件 $DEPLOY_EXE"
  [[ -f "$DEPLOY_REMOTE" ]] || warn "缺少组件 $DEPLOY_REMOTE（--deploy-mode local 可不用）"
  for cmd in git gh curl unzip ssh scp awk python3; do command -v "$cmd" >/dev/null || die "缺少命令 $cmd"; done
  command -v cargo >/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
  command -v cargo >/dev/null || die '缺少 cargo（~/.cargo/bin 也不在 PATH）'
  printf '  版本=%s  分支=%s  仓库=%s\n' "$VERSION" "$BRANCH" "$REPO"
  printf '  主机=%s  代理=%s:%s  面板端口=%s\n' "$HOSTS" "$PROXY_HOST" "$PROXY_PORT" "$PANEL_PORT"
  printf '  工作树：\n'
  git status --short | sed 's/^/    /'
  printf '  发布说明：.github/release-notes/v%s.md\n' "$VERSION"
  printf '  脚本已入库，commit 步骤会一并提交脚本改动\n'
  printf '  模式：%s\n' "$([[ $DRY -eq 1 ]] && echo dry-run || echo execute)"
}
step_test() {
  run '全量单线程 lib 测试' cargo test --lib --no-default-features -- --test-threads=1
}

step_commit() {
  [[ -n "$MESSAGE" ]] || { warn '未提供 --message，跳过 commit（工作树应已干净/已提交）'; return 0; }
  case "$MESSAGE" in
    *Co-authored-by:*|*Signed-off-by:*|*Reviewed-by:*|*Co-Authored-By:*)
      die '提交信息禁止任何 trailer（AGENTS.md）' ;;
  esac
  run '暂存改动（脚本已入库，改动一并提交）' git add -A -- .
  run '提交（无 trailer）' git commit -m "$MESSAGE"
}

step_push() {
  # 绕开全局 insteadOf / 镜像重写：否则 TLS 被改写，必须清空全局配置直连 GitHub。
  if [[ $DRY -eq 1 ]]; then
    printf '  [dry-run] 推送 main（直连 GitHub，GIT_CONFIG_GLOBAL=/dev/null，令牌隐藏）\n'
    printf '            $ GIT_CONFIG_GLOBAL=/dev/null GIT_HTTP_VERSION=HTTP/1.1 git push "https://x-access-token:***@github.com/%s.git" %s\n' "$REPO" "$BRANCH"
    return 0
  fi
  local token url
  token="$(GH_HOST=github.com gh auth token)"
  [[ -n "$token" ]] || die 'gh auth token 为空'
  url="https://x-access-token:${token}@github.com/${REPO}.git"
  log '推送 main（清空全局 git 配置，直连 GitHub）'
  GIT_CONFIG_GLOBAL=/dev/null GIT_HTTP_VERSION=HTTP/1.1 git push "$url" "$BRANCH"
  unset token url
}

step_dispatch() {
  # 推送 Cargo.toml 版本变化会自动触发 CI；只有需要重跑/版本号没变时才手动触发。
  run '手动触发构建工作流' gh workflow run "$WORKFLOW" --repo "$REPO" --ref "$BRANCH" -f "version=v$VERSION"
  if [[ $DRY -eq 0 && -z "$RUN_ID" ]]; then
    sleep 5
    RUN_ID="$(gh run list --repo "$REPO" --workflow "$WORKFLOW" --event workflow_dispatch --limit 1 \
      --json databaseId --jq '.[0].databaseId // empty')"
    [[ -n "$RUN_ID" ]] || die '未能从 workflow_dispatch 找到 run id，请用 --run-id 指定'
    printf '  run id=%s\n' "$RUN_ID"
  fi
}

step_wait() {
  if [[ -z "$RUN_ID" ]]; then
    if [[ $DRY -eq 1 ]]; then
      printf '  [dry-run] 等待最新一次 CI run（--run-id 可显式指定）\n'
      return 0
    fi
    RUN_ID="$(gh run list --repo "$REPO" --workflow "$WORKFLOW" --limit 1 \
      --json databaseId --jq '.[0].databaseId // empty')"
    [[ -n "$RUN_ID" ]] || die '找不到 CI run，请用 --run-id 指定'
  fi
  run "等待 CI run $RUN_ID 完成" gh run watch "$RUN_ID" --repo "$REPO" --exit-status --interval 30
}

step_release() {
  local full="miliastra-wonderland-music-windows-x64-openvino-v${VERSION}.zip"
  local basic="miliastra-wonderland-music-windows-x64-mnn-v${VERSION}.zip"
  if [[ $DRY -eq 1 ]]; then
    printf '  [dry-run] 核对 Release v%s 的两个资产：%s / %s\n' "$VERSION" "$full" "$basic"
    printf '            $ gh release view v%s --repo %s --json tagName,name,assets\n' "$VERSION" "$REPO"
    return 0
  fi
  log "核对 Release v$VERSION"
  local names
  names="$(gh release view "v$VERSION" --repo "$REPO" --json tagName,assets --jq '.assets[].name')"
  grep -Fxq "$full" <<<"$names" || die "Release 缺少 $full"
  grep -Fxq "$basic" <<<"$names" || die "Release 缺少 $basic"
  printf '  资产齐全：\n%s\n' "$names" | sed 's/^/    /'
}

step_artifact() {
  if [[ -n "$ARTIFACT_ID" && -n "$EXPECTED_BYTES" ]]; then
    printf '  artifact id=%s expected=%s\n' "$ARTIFACT_ID" "$EXPECTED_BYTES"
    return 0
  fi
  if [[ $DRY -eq 1 ]]; then
    printf '  [dry-run] 从 CI run 查询 %s artifact 的 id / size_in_bytes\n' "$ARTIFACT_NAME"
    return 0
  fi
  [[ -n "$RUN_ID" ]] || die '查询 artifact 需要 --run-id'
  local pair
  pair="$(gh api "repos/$REPO/actions/runs/$RUN_ID/artifacts" \
    --jq ".artifacts[] | select(.name==\"$ARTIFACT_NAME\") | [.id,.size_in_bytes] | @tsv" | head -1)"
  [[ -n "$pair" ]] || die "run $RUN_ID 里找不到 $ARTIFACT_NAME artifact"
  read -r ARTIFACT_ID EXPECTED_BYTES <<<"$pair"
  printf '  artifact id=%s expected=%s\n' "$ARTIFACT_ID" "$EXPECTED_BYTES"
}

# 自定义主机列表时，用与 deploy-remote.sh 相同口径的代理下载（续传 + 精确字节 + zip 校验）。
download_artifact() {
  if [[ $DRY -eq 1 ]]; then
    printf '  [dry-run] 代理下载 artifact %s（期望 %s 字节）→ <workdir>/ov.zip\n' \
      "${ARTIFACT_ID:-<id>}" "${EXPECTED_BYTES:-<bytes>}"
    return 0
  fi
  [[ -n "$ARTIFACT_ID" && -n "$EXPECTED_BYTES" ]] || die '下载需要 --artifact-id 与 --expected-bytes'
  WORK="$(mktemp -d)"
  local zip="$WORK/ov.zip" url token
  url="https://api.github.com/repos/$REPO/actions/artifacts/$ARTIFACT_ID/zip"
  token="$(GH_HOST=github.com gh auth token)"
  log "代理下载 artifact $ARTIFACT_ID（socks5h://$PROXY_HOST:$PROXY_PORT）"
  for i in 1 2 3 4 5 6; do
    local size; size="$(stat -c%s "$zip" 2>/dev/null || echo 0)"
    [[ "$size" -ge "$EXPECTED_BYTES" ]] && break
    printf '    attempt %s (size=%s)\n' "$i" "$size"
    curl -sL -C - -m 1500 --socks5-hostname "$PROXY_HOST:$PROXY_PORT" \
      -H "Authorization: Bearer $token" -o "$zip" "$url"
  done
  unset token
  local size; size="$(stat -c%s "$zip" 2>/dev/null || echo 0)"
  [[ "$size" -eq "$EXPECTED_BYTES" ]] || die "字节数不符：$size != $EXPECTED_BYTES"
  unzip -t "$zip" >/dev/null || die 'zip 损坏'
  unzip -oq "$zip" -d "$WORK/x"
  EXE_PATH="$WORK/x/$EXE_NAME"
  [[ -f "$EXE_PATH" ]] || die "zip 里找不到 $EXE_NAME"
  printf '  本地 exe=%s sha256=%s\n' "$EXE_PATH" "$(sha256sum "$EXE_PATH" | cut -d' ' -f1)"
}

step_download() { download_artifact; }

# 部署前经面板 API `POST /chat/send` 对每台被部署主机各发一次预告，
# 全部发完后**统一**等 5 秒才动 exe（不要每台各睡一次）。
# 默认主机 = 去重后的 `--hosts`；`--announce-hosts` 显式覆盖时也去重。
# 端点语义见 src/interfaces/http/routes/tasks_chat.rs:98：query 参数 text /
# usePrefix，mutating 路由必须 POST，鉴权头 x-miliastra-token。
step_announce() {
  if [[ $SKIP_ANNOUNCE -eq 1 ]]; then
    warn '已指定 --skip-announce，跳过更新预告'
    return 0
  fi
  local announce_source
  if [[ -n "$ANNOUNCE_HOSTS" ]]; then
    announce_source="$ANNOUNCE_HOSTS"
  else
    announce_source="$HOSTS"
  fi
  # 按空白切分并去重；--hosts 里写重复项也只发一次。
  ANNOUNCE_HOSTS="$(printf '%s\n' $announce_source | awk 'NF && !seen[$0]++')"
  if [[ -z "$ANNOUNCE_HOSTS" ]]; then
    die '没有可发送更新预告的主机'
  fi
  if [[ $DRY -eq 1 ]]; then
    for host in $ANNOUNCE_HOSTS; do
      printf '  [dry-run] POST http://%s:%s/chat/send?text=<URL编码>&usePrefix=false（每台各发一次）\n' "$host" "$PANEL_PORT"
      printf '            文本=%s\n' "$ANNOUNCE_TEXT"
    done
    printf '  [dry-run] sleep %s（全部发完后统一给玩家缓冲）后进入部署\n' "$ANNOUNCE_DELAY"
    return 0
  fi
  [[ -n "$PANEL_TOKEN" ]] || die 'announce 需要环境变量 MILIASTRA_PANEL_TOKEN（令牌不得写入文件/提交/报告）'
  [[ "$ANNOUNCE_DELAY" =~ ^[0-9]+$ ]] || die '--announce-delay 必须是整数秒'
  local encoded url response
  encoded="$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$ANNOUNCE_TEXT")"
  for host in $ANNOUNCE_HOSTS; do
    url="http://$host:$PANEL_PORT/chat/send?text=$encoded&usePrefix=false"
    log "更新预告：调用 $host 的 /chat/send（每台各发一次）"
    response="$(curl -sS -m 10 -X POST -H "x-miliastra-token: $PANEL_TOKEN" "$url")"
    printf '%s' "$response" | grep -Eq '"ok"[[:space:]]*:[[:space:]]*true' \
      || die "$host /chat/send 未返回 ok:true：$response"
    printf '  已入队：%s\n' "$response"
  done
  log "全部主机已预告，等待 ${ANNOUNCE_DELAY}s 后开始更新（期间会有短暂停止）"
  sleep "$ANNOUNCE_DELAY"
}

step_deploy() {
  if [[ -z "$ARTIFACT_ID" || -z "$EXPECTED_BYTES" ]]; then
    if [[ $DRY -eq 1 ]]; then
      printf '  [dry-run] 部署前需要 artifact id / expected bytes（all 流程会自动查询）\n'
    else
      die '部署需要 artifact id / expected bytes，先跑 --step artifact'
    fi
  fi
  # 无论 all 流程还是单独 --step deploy，部署前都对每台主机各发一次预告，
  # 全部发完后统一等 5 秒。
  step_announce
  if [[ "$DEPLOY_MODE" == 'components' ]]; then
    local -a hosts_arr=()
    read -r -a hosts_arr <<< "$HOSTS"
    local first="${hosts_arr[0]:-<HOST>}"
    local -a rest=("${hosts_arr[@]:1}")
    if [[ $DRY -eq 1 ]]; then
      printf '  [dry-run] 组件1：MILIASTRA_DEPLOY_HOST=%s TMPDIR=<workdir> %s %s %s（代理下载并部署首台）\n' \
        "$first" "$DEPLOY_REMOTE" "${ARTIFACT_ID:-<id>}" "${EXPECTED_BYTES:-<bytes>}"
      if [[ ${#rest[@]} -eq 0 ]]; then
        printf '  [dry-run] 组件2：（无其余主机，跳过）\n'
      else
        for host in "${rest[@]}"; do
          printf '  [dry-run] 组件2：%s --host %s --exe <同一份 exe> --user %s --key <SSH_KEY> --dir <REMOTE_DIR>\n' \
            "$DEPLOY_EXE" "$host" "$REMOTE_USER"
        done
      fi
      return 0
    fi
    WORK="$(mktemp -d)"
    log "组件 1：deploy-remote.sh（代理下载 + $first 部署）"
    MILIASTRA_DEPLOY_HOST="$first" \
    MILIASTRA_SSH_USER="$REMOTE_USER" \
    MILIASTRA_SSH_KEY="$SSH_KEY" \
    MILIASTRA_REMOTE_DIR="$REMOTE_DIR" \
    MILIASTRA_PROXY_HOST="$PROXY_HOST" \
    MILIASTRA_PROXY_PORT="$PROXY_PORT" \
    MILIASTRA_PANEL_PORT="$PANEL_PORT" \
    MILIASTRA_REPO="$REPO" \
    MILIASTRA_TASK_NAME="$TASK_NAME" \
    TMPDIR="$WORK" bash "$DEPLOY_REMOTE" "$ARTIFACT_ID" "$EXPECTED_BYTES"
    EXE_PATH="$(find "$WORK" -type f -path "*/x/$EXE_NAME" -print -quit)"
    [[ -n "$EXE_PATH" ]] || die "deploy-remote.sh 未留下 $EXE_NAME"
    for host in "${rest[@]}"; do
      log "组件 2：deploy-exe.sh（$host）"
      bash "$DEPLOY_EXE" --host "$host" --exe "$EXE_PATH" \
        --user "$REMOTE_USER" --key "$SSH_KEY" --dir "$REMOTE_DIR" --task "$TASK_NAME"
    done
  else
    download_artifact
    for host in $HOSTS; do
      run "部署 $host" bash "$DEPLOY_EXE" --host "$host" --exe "${EXE_PATH:-<exe>}" \
        --user "$REMOTE_USER" --key "$SSH_KEY" --dir "$REMOTE_DIR" --task "$TASK_NAME"
    done
  fi
}
step_verify() {
  # 远端面板令牌只认环境变量：--execute 且需要注入 simulate-chat 时为空就退出，
  # 绝不回落到任何字面量。dry-run 只打印流程，不要求变量。
  if [[ $DRY -eq 0 && -z "$PANEL_TOKEN" ]]; then
    die 'verify 需要环境变量 MILIASTRA_PANEL_TOKEN（远程面板令牌不得写入文件/提交/报告）'
  fi
  for host in $HOSTS; do
    if [[ $DRY -eq 1 ]]; then
      printf '  [dry-run] %s：/health 200、simulate-chat 拒绝用例、远端 exe ProductVersion=%s\n' "$host" "$VERSION"
      continue
    fi
    log "验收 $host"
    local code=''
    for _ in $(seq 1 20); do
      code="$(curl -s -m 3 -o /dev/null -w '%{http_code}' "http://$host:$PANEL_PORT/health" || true)"
      [[ "$code" == '200' ]] && break
      sleep 3
    done
    [[ "$code" == '200' ]] || die "$host 面板 /health 未就绪（$code）"
    printf '  /health=200\n'
    # 只注入「应被拒绝」的命令：不会在真实大厅产生回复，避免刷屏。
    curl -s -m 10 -X POST "http://$host:$PANEL_PORT/debug/simulate-chat" \
      -H 'Content-Type: application/json' -H "x-miliastra-token: $PANEL_TOKEN" \
      -d '{"role":"friend","text":"@这个命令不存在"}' >/dev/null || warn "$host simulate-chat 失败"
    # 版本核对：exe 的 ProductVersion 应等于发布版本（失败只告警，不阻断）。
    local remote_version
    remote_version="$(ssh -i "$SSH_KEY" -o BatchMode=yes "$REMOTE_USER@$host" \
      "powershell -NoProfile -Command \"(Get-Item '$REMOTE_DIR/$EXE_NAME').VersionInfo.ProductVersion\"" \
      2>/dev/null | tr -d '\r' | tail -1 || true)"
    if [[ -n "$remote_version" && "$remote_version" != *"$VERSION"* ]]; then
      warn "$host 远端 exe 版本=$remote_version，期望包含 $VERSION"
    else
      printf '  远端 exe 版本=%s\n' "${remote_version:-未知}"
    fi
  done
}

case "$STEP" in
  preflight) preflight ;;
  test) preflight; step_test ;;
  commit) preflight; step_commit ;;
  push) preflight; step_push ;;
  dispatch) preflight; step_dispatch ;;
  wait) preflight; step_wait ;;
  release) preflight; step_release ;;
  artifact) preflight; step_artifact ;;
  download) preflight; step_artifact; step_download ;;
  announce) preflight; step_announce ;;
  deploy) preflight; step_artifact; step_deploy ;;
  verify) preflight; step_verify ;;
  all)
    preflight
    step_test
    step_commit
    step_push
    if [[ $DISPATCH -eq 1 ]]; then step_dispatch; fi
    step_wait
    step_release
    step_artifact
    # step_deploy 内部会对每台主机各发一次预告、统一等 5 秒，再下载/停进程/替换/重启；
    # 下载由 deploy 步骤负责：components 模式交给 deploy-remote.sh（避免重复下载），
    # local 模式由 deploy 步骤内的 download_artifact 完成。
    step_deploy
    step_verify
    ;;
  *) die "未知步骤: $STEP" ;;
esac

if [[ $DRY -eq 1 ]]; then
  log 'dry-run 结束：未执行任何远端写操作。确认无误后加 --execute。'
else
  log "步骤 $STEP 完成。"
fi
