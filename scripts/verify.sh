#!/usr/bin/env bash
# W0「看得见」：把质量门禁串成一条命令。
#
#   bash scripts/verify.sh          # 全量（privacy 失败只告警）
#   PRIVACY_STRICT=1 bash scripts/verify.sh
#
# 每一步都调用 Makefile 里同名目标，保证 CI / 本地 / 脚本只有一套命令。
# 失败会立刻停在该步并打印耗时与总表；privacy 是 optional（需要远端身份映射）。

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# 开发容器把 cargo 装在 /root/.cargo/bin；CI 由 rustup 注入 PATH。
if ! command -v cargo >/dev/null 2>&1 && [ -x /root/.cargo/bin/cargo ]; then
    export PATH="/root/.cargo/bin:${PATH}"
fi

PRIVACY_STRICT="${PRIVACY_STRICT:-0}"
STEPS=(fmt check test layers panel web)
declare -a RESULTS=()
FAILED=0

run_step() {
    local name="$1"
    shift
    local started
    started=$(date +%s)
    printf '\n===== %s =====\n' "$name"
    if "$@"; then
        local elapsed=$(( $(date +%s) - started ))
        RESULTS+=("PASS  ${name}  ${elapsed}s")
    else
        local status=$?
        local elapsed=$(( $(date +%s) - started ))
        RESULTS+=("FAIL  ${name}  ${elapsed}s (exit ${status})")
        FAILED=1
        return 1
    fi
}

for step in "${STEPS[@]}"; do
    if ! run_step "$step" make --no-print-directory "$step"; then
        break
    fi
done

# privacy 是 optional：没有远端映射时只告警；PRIVACY_STRICT=1 才阻断。
if [ "$FAILED" -eq 0 ]; then
    started=$(date +%s)
    printf '\n===== privacy（optional） =====\n'
    if make --no-print-directory privacy; then
        RESULTS+=("PASS  privacy  $(( $(date +%s) - started ))s")
    elif [ "$PRIVACY_STRICT" = "1" ]; then
        RESULTS+=("FAIL  privacy  $(( $(date +%s) - started ))s（PRIVACY_STRICT=1）")
        FAILED=1
    else
        RESULTS+=("SKIP  privacy  $(( $(date +%s) - started ))s（optional，未阻断）")
    fi
fi

printf '\n===== verify 汇总 =====\n'
for line in "${RESULTS[@]}"; do
    printf '  %s\n' "$line"
done
if [ "$FAILED" -ne 0 ]; then
    printf '\nverify: FAIL\n'
    exit 1
fi
printf '\nverify: PASS\n'
