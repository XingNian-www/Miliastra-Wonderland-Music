#!/usr/bin/env bash
# 把指定 exe 部署到远程 Windows 机器。
#
# 为什么单独成脚本：手工 scp + 停进程 + 换文件 + 起计划任务这套动作，
# 每次都要记住「先停进程否则文件被锁」「必须用计划任务启动否则随 SSH 会话被杀」，
# 漏一步就得到「进程起来十几秒后无声退出」。脚本把这些约束固定下来。
#
# 用法：
#   scripts/deploy-exe.sh --host 192.168.31.119 --exe /tmp/build/miliastra-wonderland-music.exe
#   scripts/deploy-exe.sh --host 192.168.31.119 --exe ... --dry-run
#
# 前置：本机已能免密 ssh 到目标（默认用户 XingNian，密钥 ~/.ssh/yae_server）。
set -euo pipefail

HOST=''
USER_NAME='XingNian'
KEY="$HOME/.ssh/yae_server"
REMOTE_DIR='C:\\Users\\XingNian\\Desktop\\5.1'
EXE_NAME='miliastra-wonderland-music.exe'
TASK_NAME='Miliastra'
EXE=''
DRY_RUN=0

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --user) USER_NAME="$2"; shift 2 ;;
    --key) KEY="$2"; shift 2 ;;
    --exe) EXE="$2"; shift 2 ;;
    --dir) REMOTE_DIR="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage ;;
    *) echo "未知参数: $1" >&2; usage ;;
  esac
done

[[ -n "$HOST" ]] || { echo '缺少 --host' >&2; exit 1; }
[[ -n "$EXE" ]] || { echo '缺少 --exe' >&2; exit 1; }
[[ -f "$EXE" ]] || { echo "找不到可执行文件: $EXE" >&2; exit 1; }

LOCAL_SHA=$(sha256sum "$EXE" | awk '{print $1}')
LOCAL_SIZE=$(stat -c%s "$EXE")
TS=$(date +%Y%m%d-%H%M%S)

echo "== 目标 $USER_NAME@$HOST:$REMOTE_DIR"
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
