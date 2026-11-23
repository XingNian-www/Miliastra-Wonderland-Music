#!/usr/bin/env bash
# scripts/check-windows.sh
#
# 在 Linux 上做 **Windows 目标的编译检查**，补上 `make check` 的最大盲区。
#
# 为什么需要它：产品只支持 Windows（`src/main.rs` 对非 Windows 是 `compile_error!`），
# 而 `make check` 跑的是 `--lib --no-default-features`（Linux 目标）。于是
# 「只在 Windows 才编译」的代码**在本地与 Linux CI 里完全不可见**——2026-11-23
# 就是这样把一个 `error[E0432]`（`crate::config::WindowCaptureMethod` 漏导出）
# 一路推到 Windows Release 构建才炸。
#
# 能力边界（务必知道）：
#   * 这是 `cargo check`，**不链接**，所以不需要 MSVC 工具链/Windows SDK；
#   * 用的是 `x86_64-pc-windows-gnu`（mingw）而不是 msvc：msvc 目标的 C 依赖
#     （aws-lc-sys）需要 Windows 版 cl，Linux 上没有；gnu 目标用 mingw 的 gcc 即可。
#     `cfg(windows)` 在两者下都成立，本仓库的 Windows 专属代码没有 `target_env`
#     分支（已 grep 确认），因此 gnu 检查对本项目等价；
#   * 默认特性里的 OCR 后端（OpenVINO/MNN）需要 Windows 版预编译库，这里用
#     `--no-default-features` 关掉；因此**不覆盖** OCR 后端的编译错误。
#     那部分只能由 CI 的 Windows 构建兜底。
#
# 用法：
#   scripts/check-windows.sh          # 检查（静默，只报错误）
#   scripts/check-windows.sh -v       # 同时显示 warning 数
#
# 环境要求：rustup target 已装 `x86_64-pc-windows-gnu`；`x86_64-w64-mingw32-gcc`。
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "缺少 mingw 交叉编译器（apt-get install gcc-mingw-w64-x86-64）" >&2
    exit 1
fi
if ! rustup target list --installed 2>/dev/null | grep -qx 'x86_64-pc-windows-gnu'; then
    echo "缺少 Rust 目标（rustup target add x86_64-pc-windows-gnu）" >&2
    exit 1
fi

# C 依赖（aws-lc-sys 等）必须用 mingw 的编译器，否则会拿 Linux gcc 去编 Windows 目标。
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++

# ffmpeg-sys-next 的 bindgen 会遍历 vendor 里的 Windows 头。其中
# `hwcontext_d3d12va.h` 依赖 Windows SDK 的 `d3d12video.h`，mingw 没有该头
# （有 d3d12.h 但没有 d3d12video.h）。项目自身不使用 d3d12va 绑定，
# 因此在**临时副本**里剔除这两个头再指过去——不动仓库里的 vendor 文件。
FFMPEG_STAGE="${TMPDIR:-/tmp}/miliastra-ffmpeg-win"
VENDOR_FFMPEG="vendor/ffmpeg/8.1/windows-x64"
if [ ! -d "$FFMPEG_STAGE" ]; then
    cp -r "$VENDOR_FFMPEG" "$FFMPEG_STAGE"
    rm -f "$FFMPEG_STAGE/include/libavutil/hwcontext_d3d12va.h" \
          "$FFMPEG_STAGE/include/libavutil/hwcontext_d3d11va.h"
fi
export FFMPEG_DIR="$FFMPEG_STAGE"

LOG="${TMPDIR:-/tmp}/check-windows.log"
set +e
cargo check --target x86_64-pc-windows-gnu --lib --no-default-features \
    --message-format=short >"$LOG" 2>&1
status=$?
set -e

errors=$(grep -cE '^[^ ]+\.rs:[0-9]+:[0-9]+: error' "$LOG" || true)
warnings=$(grep -cE '^[^ ]+\.rs:[0-9]+:[0-9]+: warning' "$LOG" || true)

if [ "$status" -ne 0 ]; then
    echo "Windows 目标编译检查失败（$errors 条错误）：" >&2
    grep -E '^[^ ]+\.rs:[0-9]+:[0-9]+: error' "$LOG" | head -40 >&2
    exit 1
fi

echo "Windows 目标编译检查通过：0 错误，$warnings 条警告（详见 $LOG）"
if [ "${1:-}" = "-v" ] && [ "$warnings" -gt 0 ]; then
    grep -E '^[^ ]+\.rs:[0-9]+:[0-9]+: warning' "$LOG" | head -20
fi
