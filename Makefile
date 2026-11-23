# 质量门禁入口（W0「看得见」）。
#
# 设计原则：CI（.github/workflows/quality-gate.yml）与本地跑**同一组命令**，
# 不在这里发明第二套规则。单个目标可单独跑，`make verify` 把它们串起来：
#
#   make fmt        cargo fmt --all --check
#   make check      cargo check --lib --no-default-features（0 warning 硬门禁）
#   make test       cargo test --lib --no-default-features（根 crate；CI 另跑 --workspace）
#   make windows    Windows 目标编译检查（mingw 交叉，补 Linux 门禁盲区）
#   make layers     scripts/check-layers.py --check + --self-test
#   make panel      面板 API 覆盖率（默认 report-only，PANEL_STRICT=1 才阻断）
#   make web        web/dist 构建（build.rs 以 web/dist/index.html 为就绪判据）
#   make privacy    隐私扫描（optional：未配置 MILIASTRA_PANEL_URL 时告警跳过）
#   make verify     依次执行以上全部（privacy 失败不阻断，除非 PRIVACY_STRICT=1）
#
# 说明：
# - `check` 用 `RUSTFLAGS=-D warnings` 把 compiler warning 提升为错误。依赖 crate
#   由 cargo 以 `--cap-lints allow` 编译，不会被本仓库的 warning 门禁误伤。
# - `panel` 默认 report-only：W0 阶段先建立可见度（与 `check-layers.py` 的
#   `--report` 一致）。CI 的 repo-scripts job 仍按严格口径（未覆盖 > 0 即红）。
#   本地要模拟 CI：`PANEL_STRICT=1 make panel`。
# - `privacy` 需要远端身份映射；没有 `MILIASTRA_PANEL_URL` 时脚本无法扫描，
#   这里如实告警并跳过（绝不冒充「零命中」）。CI 有 Secret，失败就是真失败。

SHELL := /bin/bash
# 本仓库的开发容器把 cargo 装在 /root/.cargo/bin，默认 PATH 可能没有它；
# CI runner 由 rustup 注入 PATH。两种都照顾：先找 PATH，再退回固定位置。
CARGO := $(shell command -v cargo 2>/dev/null || echo /root/.cargo/bin/cargo)
PYTHON ?= python3
WEB_DIR := web
PANEL_STRICT ?= 0
PRIVACY_STRICT ?= 0

.PHONY: help fmt check test test-workspace layers panel web privacy verify

help:
	@sed -n '2,32p' Makefile

fmt:
	$(CARGO) fmt --all --check

# Windows 目标的编译检查：`check` 跑的是 Linux 目标，Windows 专属代码在那边完全不可见
# （2026-11-23 就因此把 error[E0432] 推到 Windows Release 构建才发现）。
# 用 mingw 交叉到 x86_64-pc-windows-gnu，不需要 MSVC 工具链；详见脚本头部的能力边界。
windows:
	bash scripts/check-windows.sh

check:
	# `--locked`：CI 的 Windows Release 构建用 `cargo build --locked`。改了
	# Cargo.toml 的版本号却没同步 Cargo.lock 时，本地 check 会**静默自动改写**
	# Cargo.lock 并照样通过，而 CI 直接报
	# 「cannot update the lock file because --locked was passed」。2026-11-23 就是这样
	# 让 7.7.0-dev.1 的 Windows 构建挂掉的。这里带上 --locked，本地立刻暴露。
	RUSTFLAGS="-D warnings" $(CARGO) check --lib --no-default-features --locked

# 显式检查 Cargo.lock 与 Cargo.toml 一致：`cargo metadata` 会按需改写锁文件，
# 一旦它产生 diff 就说明有人改了版本号/依赖却没提交锁文件。
lock:
	$(CARGO) metadata --no-deps --format-version 1 > /dev/null
	@git diff --quiet -- Cargo.lock Cargo.toml || { \
		echo "Cargo.lock 与 Cargo.toml 不一致（cargo 已改写锁文件），请提交 Cargo.lock"; \
		git diff --stat -- Cargo.lock Cargo.toml; \
		exit 1; \
	}

# 本地默认只跑根 crate：本开发容器的 `miliastra-playback` ffmpeg 单测在
# `engine::ffmpeg::tests::resampler_flush_...` 上 SIGSEGV（容器 ffmpeg/内存问题，
# 与 W0 改动无关，HEAD 同样复现）。CI 的 rust-gate 仍跑 `--workspace`。
# 要本地复刻 CI 全量：`make test-workspace`。
test:
	$(CARGO) test --lib --no-default-features -- --test-threads=1
	$(CARGO) test -p miliastra-login-helper --bins --no-default-features -- --test-threads=1

test-workspace:
	$(CARGO) test --workspace --lib --no-default-features -- --test-threads=1
	$(CARGO) test -p miliastra-login-helper --bins --no-default-features -- --test-threads=1

layers:
	$(PYTHON) scripts/check-layers.py --check
	$(PYTHON) scripts/check-layers.py --self-test

panel:
	@set -euo pipefail; \
	report=$$($(PYTHON) scripts/panel-api-coverage.py); \
	printf '%s\n' "$$report"; \
	missing=$$(printf '%s\n' "$$report" | sed -n 's/.*未覆盖 \([0-9][0-9]*\) 个.*/\1/p' | tail -n 1); \
	if [ -z "$$missing" ]; then echo "无法从 panel-api-coverage.py 输出解析未覆盖数（脚本格式可能已变）"; exit 1; fi; \
	if [ "$$missing" -ne 0 ]; then \
	  if [ "$(PANEL_STRICT)" = "1" ]; then \
	    echo "面板 API 覆盖率未满：$$missing 条路由前端未调用（PANEL_STRICT=1）"; exit 1; \
	  fi; \
	  echo "面板 API 覆盖率未满：$$missing 条（report-only；PANEL_STRICT=1 才阻断）"; \
	else echo "面板 API 覆盖率：全部路由已被前端调用"; fi

web:
	cd $(WEB_DIR) && npm run build
	test -f $(WEB_DIR)/dist/index.html

privacy:
	@if [ -z "$${MILIASTRA_PANEL_URL:-}" ]; then \
	  echo "privacy（optional）：未配置 MILIASTRA_PANEL_URL，无法取身份映射，跳过扫描"; \
	  echo "  本地如需扫描：MILIASTRA_PANEL_URL=... MILIASTRA_PANEL_TOKEN=... python3 scripts/privacy-filter.py --repo --check"; \
	  exit 0; \
	fi; \
	$(PYTHON) scripts/privacy-filter.py --repo --check

verify:
	@if [ "$(PRIVACY_STRICT)" = "1" ]; then PRIVACY_STRICT=1 bash scripts/verify.sh; \
	else bash scripts/verify.sh; fi
