# Miliastra Wonderland Music

Windows-only Rust music request bot for Miliastra Wonderland chat. It watches the game chat area with template matching and OCR, parses `@` commands, controls FeelUOwn through TCP RPC, and sends responses back into the game chat through keyboard and mouse automation.

## Features

- Captures the target game window client area, not the full desktop
- Locates chat messages with blue/yellow/pink marker templates, then runs Chinese OCR on each message block
- Supports song requests, queue control, playback control, volume, status, lyrics, hall detection, invitations, and microphone toggling
- Uses FeelUOwn TCP RPC for search, playback, status, lyrics, and queue transitions
- Provides a local Web/API panel on `127.0.0.1:18888`
- Persists queue and runtime state under `data/`
- Sends Windows notifications and writes file logs
- Includes manual debug tools for screenshots, OCR, chat scanning, UI state, template matching, chat sending, chat change monitoring, and panel response benchmarking
- Supports global hotkeys: `F7` pause/resume scanning, `F12` exit

The previous BGI script path has been removed. This repository is now a standalone Rust application.

## Build Windows Exe

GitHub Actions includes `.github/workflows/build-windows-exe.yml`. It does not run on push by default. Run `Build Windows exe` manually from the GitHub Actions page. A successful run creates or updates a GitHub Release and uploads the Windows x64 zip package.

The workflow builds `x86_64-pc-windows-msvc` release artifacts and downloads PP-OCRv6 small models:

```text
models/PP-OCRv6_small_det.mnn
models/PP-OCRv6_small_rec.mnn
models/ppocr_keys_v6_small.txt
```

Uploaded artifact:

```text
miliastra-wonderland-music-windows-x64/
├── miliastra-wonderland-music.exe
├── MNN.dll
├── assets/
├── models/
└── README.md
```

## Local Build

Official release builds target Windows MSVC and use the vendored `alibaba/MNN` 3.6.0 Windows x64 dynamic library:

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

The repository includes the required MNN runtime files:

```text
vendor/mnn/3.6.0/windows-x64/include/
vendor/mnn/3.6.0/windows-x64/lib/MNN.lib
vendor/mnn/3.6.0/windows-x64/bin/MNN.dll
```

`MNN.dll` is copied next to the release executable during build. The OCR backend defaults to CPU. CUDA, Vulkan, OpenCL, source-built MNN, and `zibo-chen/MNN-Prebuilds` are not supported release paths.

Target machines also need Microsoft Visual C++ Redistributable 2015-2022 x64 because the official MNN dynamic library depends on `MSVCP140.dll`, `VCRUNTIME140.dll`, and `VCRUNTIME140_1.dll`.

Linux can run checks for the Windows GNU target, but this is not a release build path:

```bash
cargo check --target x86_64-pc-windows-gnu --features ocr-rs/docsrs
```

## Run

```powershell
miliastra-wonderland-music.exe
```

With explicit config:

```powershell
miliastra-wonderland-music.exe --config config.yaml run
```

The first launch creates `config.yaml` with comments and defaults.

## Requirements

- Windows
- Target game process, default `yuanshen.exe`
- FeelUOwn with TCP RPC enabled, default `127.0.0.1:23333`
- OCR models in `models/`
- Template images in `assets/`
- Microsoft Visual C++ Redistributable 2015-2022 x64

## Chat Commands

Commands must start with `@` after the chat name separator.

Examples:

```text
用户: @点歌 晴天
用户: @QQ点歌 晴天
用户: @网易点歌 晴天
用户: @暂停
用户: @继续
用户: @下一首
用户: @上一首
用户: @音量 30
用户: @状态
用户: @歌词
用户: @队列
用户: @帮助
用户: @大厅检测
用户: @大厅时间
```

Friend-only pink message commands:

```text
@邀请1
@麦克风
```

Queue commands:

```text
@队列删除 1
@队列清空
```

When a requested song cannot be confidently matched, the bot can ask for confirmation and accepts:

```text
@确认
@跳过
@换源
```

## Manual Tools

```powershell
miliastra-wonderland-music.exe --config config.yaml manual
```

Manual menu includes:

- Screenshot capture
- OCR region recognition
- Chat area scanning
- UI state detection
- Template matching
- Coordinate click
- Key press
- Chat send test
- OCR GPU support probe
- Chat change monitor
- Panel response benchmark

Panel response benchmark presses `Enter` to open chat and `Esc` to close it, then samples the configured detection area until the panel is stable. It prints per-round first-change time, stable time, average, maximum, and failure count.

## Web/API Panel

The local panel listens on:

```text
http://127.0.0.1:18888
```

The server rejects cross-site control requests. Mutating endpoints accept local or same-origin requests only.

Main endpoints:

```text
/status
/play
/pause
/skip-next
/skip-prev
/volume
/searchPlay
/queue
/queue/add
/queue/remove
/queue/clear
/state
/state/save
/history
/clear-history
/active-window
/notify
/ai/recognize
/ai/match
```

## Configuration Notes

Default chat area:

```yaml
screen:
  chat_rect:
    x: 39
    y: 879
    width: 416
    height: 143
```

Current timing defaults:

```yaml
timing:
  scan_loop_idle_ms: 60
  chat_scan_fallback_ms: 2000
  chat_change_debounce_ms: 120
  chat_change_cooldown_ms: 250
  post_command_settle_ms: 500
  command_ui_timeout_ms: 15000
```

Game chat replies are capped at display width 80, roughly 40 full-width Chinese characters or 80 ASCII characters.

## Logs And State

- Logs: `logs/miliastra-wonderland-music.log`
- Runtime state: `data/runtime-state.json`
- Queue: `data/queue.json`

Log prefix format:

```text
[MM-DD HH:MM:SS][INFO ] : message
```

The timestamp uses UTC+8.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE).

Third-party components keep their own licenses. The vendored MNN binary and headers are distributed under Apache-2.0; see `vendor/mnn/3.6.0/LICENSE.txt`.
