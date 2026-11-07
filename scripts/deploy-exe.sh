#!/usr/bin/env bash
# 把指定 exe 部署到远程 Windows 机器。
#
# 为什么单独成脚本：手工 scp + 停进程 + 换文件 + 起计划任务这套动作，
# 每次都要记住「先停进程否则文件被锁」「必须用计划任务启动否则随 SSH 会话被杀」，
# 漏一步就得到「进程起来十几秒后无声退出」。脚本把这些约束固定下来。
#
# 用法：
#   scripts/deploy-exe.sh --host <HOST> --exe <EXE> \
#       --user <USER> --key <SSH_KEY> --dir <REMOTE_DIR> --task <TASK>
#   scripts/deploy-exe.sh --host <HOST> --exe <EXE> --dry-run
#
# 环境变量（--execute 时必填；CLI 参数优先）：
#   MILIASTRA_SSH_USER / MILIASTRA_SSH_KEY / MILIASTRA_REMOTE_DIR / MILIASTRA_TASK_NAME
# dry-run 不要求任何环境信息。
set -euo pipefail

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

HOST=''
USER_NAME="${MILIASTRA_SSH_USER:-}"
KEY="${MILIASTRA_SSH_KEY:-}"
REMOTE_DIR="${MILIASTRA_REMOTE_DIR:-}"
TASK_NAME="${MILIASTRA_TASK_NAME:-}"
EXE_NAME='miliastra-wonderland-music.exe'
EXE=''
DRY_RUN=0

usage() {
  awk 'NR>1 { if ($0 ~ /^set -euo pipefail$/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --user) USER_NAME="$2"; shift 2 ;;
    --key) KEY="$2"; shift 2 ;;
    --exe) EXE="$2"; shift 2 ;;
    --dir) REMOTE_DIR="$2"; shift 2 ;;
    --task) TASK_NAME="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage ;;
    *) die "未知参数: $1" ;;
  esac
done

[[ -n "$HOST" ]] || die '缺少 --host'
[[ -n "$EXE" ]] || die '缺少 --exe'
[[ -f "$EXE" ]] || die "找不到可执行文件: $EXE"

if [[ $DRY_RUN -eq 0 ]]; then
  [[ -n "$USER_NAME" ]] || die '缺少 --user 或 MILIASTRA_SSH_USER'
  [[ -n "$KEY" ]] || die '缺少 --key 或 MILIASTRA_SSH_KEY'
  [[ -n "$REMOTE_DIR" ]] || die '缺少 --dir 或 MILIASTRA_REMOTE_DIR'
  [[ -n "$TASK_NAME" ]] || die '缺少 --task 或 MILIASTRA_TASK_NAME'
fi

LOCAL_SHA=$(sha256sum "$EXE" | awk '{print $1}')
LOCAL_SIZE=$(stat -c%s "$EXE")
TS=$(date +%Y%m%d-%H%M%S)

echo "== 目标 ${USER_NAME:-<USER>}@$HOST:${REMOTE_DIR:-<REMOTE_DIR>}"
echo "== 文件 $(basename "$EXE") ($LOCAL_SIZE 字节)"
echo "== sha256 $LOCAL_SHA"

if [[ $DRY_RUN -eq 1 ]]; then
  echo '== dry-run：只打印将要执行的动作'
  exit 0
fi

echo '== 1/4 上传到 .new'
scp -i "$KEY" -o BatchMode=yes "$EXE" "$USER_NAME@$HOST:$REMOTE_DIR/$EXE_NAME.new"

echo '== 2/4 远端校验哈希'
REMOTE_SHA=$(ssh -i "$KEY" -o BatchMode=yes "$USER_NAME@$HOST" \
  "powershell -NoProfile -Command \"(Get-FileHash '$REMOTE_DIR/$EXE_NAME.new' -Algorithm SHA256).Hash.ToLower()\"" \
  | tr -d '\r' | tail -1)
if [[ "$REMOTE_SHA" != "$LOCAL_SHA" ]]; then
  echo "哈希不一致，中止。远端=$REMOTE_SHA" >&2
  ssh -i "$KEY" -o BatchMode=yes "$USER_NAME@$HOST" \
    "powershell -NoProfile -Command \"Remove-Item '$REMOTE_DIR/$EXE_NAME.new' -Force\"" || true
  exit 1
fi
echo '   一致'

echo "== 3/4 备份现有 exe（.bak-$TS）并替换"
ssh -i "$KEY" -o BatchMode=yes "$USER_NAME@$HOST" \
  "powershell -NoProfile -Command \"Copy-Item '$REMOTE_DIR/$EXE_NAME' '$REMOTE_DIR/$EXE_NAME.bak-$TS' -Force; \""

echo '== 4/4 停进程 → 替换 → 计划任务启动'
# 顺序不能变：Windows 会锁住正在运行的 exe，未停进程就 Move 会失败。
# 必须用计划任务启动：Start-Process 起的进程会随 SSH 会话结束被杀。
ssh -i "$KEY" -o BatchMode=yes "$USER_NAME@$HOST" \
  "powershell -NoProfile -Command \"Stop-Process -Name 'miliastra-wonderland-music' -Force -ErrorAction SilentlyContinue; Start-Sleep -Seconds 4; Move-Item '$REMOTE_DIR/$EXE_NAME.new' '$REMOTE_DIR/$EXE_NAME' -Force; Start-ScheduledTask -TaskName '$TASK_NAME'; Start-Sleep -Seconds 12; (Get-Process miliastra-wonderland-music -ErrorAction SilentlyContinue | Measure-Object).Count\""

echo '== 完成。请用日志确认新版本已生效：'
echo "   ssh -i $KEY $USER_NAME@$HOST 'powershell -NoProfile -Command \"Get-Content $REMOTE_DIR\\deps\\logs\\miliastra-wonderland-music-*.log -Tail 5 -Encoding UTF8\"'"
