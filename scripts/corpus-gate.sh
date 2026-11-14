#!/usr/bin/env bash
# task-113 全量图像分类提交门禁：读取 technical-docs/ui-state-corpus/manifest.tsv，
# 对全部 confirmed 断言 predicted==expected（transitional 只允许显式白名单且必须带理由）。
# 一键运行：scripts/corpus-gate.sh
set -euo pipefail
cd "$(dirname "$0")/.."
# 兼容非登录 shell：cargo 常装在 ~/.cargo/bin。
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null 2>&1 || { echo "找不到 cargo（请先安装 Rust 工具链）" >&2; exit 127; }
PROFILE_ARGS=()
if [[ "${CORPUS_GATE_RELEASE:-0}" == "1" ]]; then
  PROFILE_ARGS+=(--release)
fi
cargo test "${PROFILE_ARGS[@]}" --lib --no-default-features -- \
  --ignored --exact --nocapture --test-threads=1 \
  ui::state::tests::corpus_manifest_confirmed_classification_gate
