#!/usr/bin/env bash
# scripts/release-pipeline.sh
#
# 7.4.x 及后续版本的「发布 + 双机部署」可复用流程脚本（本地留存的 runbook）。
#
# 为什么有这个脚本：每次发版都要记住一串容易漏的约束——
#   * 推送必须绕开全局 insteadOf / ghfast.top 镜像，否则 TLS 被改写；
#   * 版本号提交会自动触发 CI，手动 workflow_dispatch 不能重复触发同一个 Release；
#   * GitHub 大文件直连会反复截断，必须走 socks5h 代理 + 断点续传 + 精确字节数校验；
#   * Windows exe 被运行中的进程锁住，必须先停进程再替换，且必须用计划任务启动；
#   * 两台克隆虚机（.119/.242）同名同配置，只能按 IP 逐台部署并各自验收。
#
# 设计原则：
#   1) **默认 dry-run**：只打印将执行的命令；只有显式 `--execute` 才做远端写操作
#      （push / workflow_dispatch / 下载 / 部署 / commit）。
#   2) 不重复造轮子：部署复用 scripts/deploy-exe.sh，代理下载 + .119 单机链路
#      复用 technical-docs/scripts/deploy-remote.sh（该文件在 gitignored 的
#      technical-docs 下，因此本脚本是本机 runbook，不随仓库分发）。
#   3) 提交信息不允许任何 trailer（AGENTS.md 明确要求）。
#   4) 验收只注入「应被拒绝」的命令，避免在真实公共大厅刷屏。
#   5) 远程面板令牌**绝不写进文件/提交/报告**：只从环境变量
#      `MILIASTRA_PANEL_TOKEN` 读取；`--execute` 且跑 verify / announce 时为空直接报错。
#      dry-run 不要求该变量。不要用命令行参数传令牌（会留在 shell history）。
#   6) 部署前先经 `POST /chat/send` **发言一次**，提示「5 秒后将更新版本，
#      期间会有短暂停止」，再 sleep 5 秒才动 exe；默认只在第一台机器发一次，
#      避免两台克隆机重复刷屏（`--announce-hosts` 可显式指定）。
#
# 用法：
#   scripts/release-pipeline.sh --version 7.4.1 --step all
#   scripts/release-pipeline.sh --version 7.4.1 --step all --execute \
#       --message "fix(chat): ..." --dispatch
#   # 需要 --execute 且涉及 announce / verify 时，先从环境/密钥管理器注入令牌（不要写进文件）：
#   export MILIASTRA_PANEL_TOKEN='<从密钥管理器取得的令牌>'
#   scripts/release-pipeline.sh --version 7.4.1 --step verify --execute
#   # 只想单独发一次更新预告（5 秒后由调用方自行部署）：
#   scripts/release-pipeline.sh --version 7.4.1 --step announce --execute
#   # CI 已完成、只想补部署：
#   scripts/release-pipeline.sh --version 7.4.1 --step deploy --execute \
#       --run-id 42205610473
#   # 已有 artifact：
#   scripts/release-pipeline.sh --version 7.4.1 --step deploy --execute \
#       --artifact-id <ID> --expected-bytes <BYTES>
#
# 步骤（--step）：all | preflight | test | commit | push | dispatch | wait |
#                release | artifact | download | announce | deploy | verify
#   all = preflight→test→commit(可选)→push→dispatch(可选)→wait→release→
#         artifact→announce→deploy→verify

set -euo pipefail

REPO='XingNian-www/Miliastra-Wonderland-Music'
WORKFLOW='build-windows-exe.yml'
BRANCH='main'
PROXY_HOST='192.168.31.183'
PROXY_PORT='7893'
HOSTS_DEFAULT='192.168.31.119 192.168.31.242'
REMOTE_USER='XingNian'
SSH_KEY="$HOME/.ssh/yae_server"
REMOTE_DIR='C:\Users\XingNian\Desktop\5.1'
EXE_NAME='miliastra-wonderland-music.exe'
PANEL_TOKEN="${MILIASTRA_PANEL_TOKEN:-}"
ANNOUNCE_TEXT='5秒后将更新版本，期间会有短暂停止，请稍候。'
ANNOUNCE_DELAY=5
ANNOUNCE_HOSTS=''   # 空=默认只在 HOSTS 的第一台发言一次
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
HOSTS="$HOSTS_DEFAULT"
DEPLOY_MODE='components'   # components=复用两个既有脚本；local=自己下载后逐台 deploy-exe.sh
WORK=''

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 1; }

log()  { printf '\n== %s\n' "$*"; }
warn() { printf '!! %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

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
  [[ -f ".github/release-notes/v${VERSION}.md" ]] || die "缺少发布说明 .github/release-notes/v${VERSION}.md"
  [[ -f "$DEPLOY_EXE" ]] || die "缺少组件 $DEPLOY_EXE"
  [[ -f "$DEPLOY_REMOTE" ]] || warn "缺少组件 $DEPLOY_REMOTE（--deploy-mode local 可不用）"
  for cmd in git gh curl unzip ssh scp awk python3; do command -v "$cmd" >/dev/null || die "缺少命令 $cmd"; done
  command -v cargo >/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
  command -v cargo >/dev/null || die '缺少 cargo（~/.cargo/bin 也不在 PATH）'
  printf '  版本=%s  分支=%s  仓库=%s\n' "$VERSION" "$BRANCH" "$REPO"
  printf '  主机=%s\n' "$HOSTS"
  printf '  工作树：\n'
  git status --short | sed 's/^/    /'
  printf '  发布说明：.github/release-notes/v%s.md\n' "$VERSION"
  printf '  本脚本仅本地留存，commit 步骤会排除它\n'
  # --execute 且本步骤会跑到 announce / verify 时提前要求令牌，
  # 避免先做了一串远端写操作才在最后报错。
  local need_token=0
  case "$STEP" in
    all|verify) need_token=1 ;;
    announce|deploy)
      if [[ $SKIP_ANNOUNCE -eq 0 ]]; then need_token=1; fi
      ;;
  esac
  if [[ $DRY -eq 0 && $need_token -eq 1 && -z "$PANEL_TOKEN" ]]; then
    die '请先导出环境变量 MILIASTRA_PANEL_TOKEN（远程面板令牌不得写入文件/提交/报告）'
  fi
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
  run '暂存改动（排除本脚本：按要求只本地留存，不进仓库）' \
    git add -A -- . ':(exclude)scripts/release-pipeline.sh'
  run '提交（无 trailer）' git commit -m "$MESSAGE"
}

step_push() {
  # 绕开 ghfast.top：全局 insteadOf 会破坏 TLS，必须清空全局配置并直连 GitHub。
  if [[ $DRY -eq 1 ]]; then
    printf '  [dry-run] 推送 main（直连 GitHub，GIT_CONFIG_GLOBAL=/dev/null，令牌隐藏）\n'
    printf '            $ GIT_CONFIG_GLOBAL=/dev/null GIT_HTTP_VERSION=HTTP/1.1 git push "https://x-access-token:***@github.com/%s.git" %s\n' "$REPO" "$BRANCH"
    return 0
  fi
  local token url
  token="$(GH_HOST=github.com gh auth token)"
  [[ -n "$token" ]] || die 'gh auth token 为空'
  url="https://x-access-token:${token}@github.com/${REPO}.git"
  log '推送 main（直连 GitHub，绕过 ghfast.top rewrite）'
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

# 部署前经面板 API `POST /chat/send` 发言一次，再等 5 秒才动 exe。
# 只发一次（默认第一台机器），避免两台克隆机在大厅重复刷屏。
# 端点语义见 src/interfaces/http/routes/tasks_chat.rs:98：query 参数 text /
# usePrefix，mutating 路由必须 POST，鉴权头 x-miliastra-token。
step_announce() {
  if [[ $SKIP_ANNOUNCE -eq 1 ]]; then
    warn '已指定 --skip-announce，跳过更新预告'
    return 0
  fi
  if [[ -z "$ANNOUNCE_HOSTS" ]]; then
    ANNOUNCE_HOSTS="$(printf '%s\n' $HOSTS | head -1)"
  fi
  if [[ $DRY -eq 1 ]]; then
    for host in $ANNOUNCE_HOSTS; do
      printf '  [dry-run] POST http://%s:18888/chat/send?text=<URL编码>&usePrefix=false（发言一次）\n' "$host"
      printf '            文本=%s\n' "$ANNOUNCE_TEXT"
    done
    printf '  [dry-run] sleep %s（给玩家缓冲）后进入部署\n' "$ANNOUNCE_DELAY"
    return 0
  fi
  [[ -n "$PANEL_TOKEN" ]] || die 'announce 需要环境变量 MILIASTRA_PANEL_TOKEN（令牌不得写入文件/提交/报告）'
  [[ "$ANNOUNCE_DELAY" =~ ^[0-9]+$ ]] || die '--announce-delay 必须是整数秒'
  local encoded url response
  encoded="$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$ANNOUNCE_TEXT")"
  for host in $ANNOUNCE_HOSTS; do
    url="http://$host:18888/chat/send?text=$encoded&usePrefix=false"
    log "更新预告：调用 $host 的 /chat/send 发言一次"
    response="$(curl -sS -m 10 -X POST -H "x-miliastra-token: $PANEL_TOKEN" "$url")"
    printf '%s' "$response" | grep -Eq '"ok"[[:space:]]*:[[:space:]]*true' \
      || die "$host /chat/send 未返回 ok:true：$response"
    printf '  已入队：%s\n' "$response"
  done
  log "等待 ${ANNOUNCE_DELAY}s 后开始更新（期间会有短暂停止）"
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
  # 无论 all 流程还是单独 --step deploy，部署前都先发言一次并等 5 秒。
  step_announce
  if [[ "$DEPLOY_MODE" == 'components' && "$HOSTS" == "$HOSTS_DEFAULT" ]]; then
    # 组件 1：deploy-remote.sh 自带「代理下载 + .119 哈希校验 + 备份/停进程/替换 + 计划任务启动 + 面板健康检查」。
    # TMPDIR 固定到我们可控的目录，便于复用它下载并解压出的同一份 exe 给 .242。
    if [[ $DRY -eq 1 ]]; then
      printf '  [dry-run] 组件1：TMPDIR=<workdir> %s %s %s（下载并部署 .119）\n' \
        "$DEPLOY_REMOTE" "${ARTIFACT_ID:-<id>}" "${EXPECTED_BYTES:-<bytes>}"
      printf '  [dry-run] 组件2：%s --host 192.168.31.242 --exe <同一份 exe>\n' "$DEPLOY_EXE"
      return 0
    fi
    WORK="$(mktemp -d)"
    log '组件 1：deploy-remote.sh（代理下载 + .119 部署）'
    TMPDIR="$WORK" bash "$DEPLOY_REMOTE" "$ARTIFACT_ID" "$EXPECTED_BYTES"
    EXE_PATH="$(find "$WORK" -type f -path "*/x/$EXE_NAME" -print -quit)"
    [[ -n "$EXE_PATH" ]] || die "deploy-remote.sh 未留下 $EXE_NAME"
    log '组件 2：deploy-exe.sh（.242）'
    bash "$DEPLOY_EXE" --host 192.168.31.242 --exe "$EXE_PATH" --key "$SSH_KEY"
  else
    download_artifact
    for host in $HOSTS; do
      run "部署 $host" bash "$DEPLOY_EXE" --host "$host" --exe "${EXE_PATH:-<exe>}" --key "$SSH_KEY"
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
      code="$(curl -s -m 3 -o /dev/null -w '%{http_code}' "http://$host:18888/health" || true)"
      [[ "$code" == '200' ]] && break
      sleep 3
    done
    [[ "$code" == '200' ]] || die "$host 面板 /health 未就绪（$code）"
    printf '  /health=200\n'
    # 只注入「应被拒绝」的命令：不会在真实大厅产生回复，避免刷屏。
    curl -s -m 10 -X POST "http://$host:18888/debug/simulate-chat" \
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
    # step_deploy 内部会先发一次更新预告并等 5 秒，再下载/停进程/替换/重启；
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
