# 某动漫游戏点歌机器人

这是一个仅支持 Windows 的 Rust 点歌机器人。程序会识别某动漫游戏聊天区里的 `@` 命令，通过 FeelUOwn 控制搜索、播放、歌词和状态，并用键盘鼠标自动把回复发回游戏聊天

## 主要功能

- 只截取目标游戏窗口客户区，不截取整个桌面
- 通过蓝色、黄色、粉色聊天标志定位消息，再对消息块做中文识别
- 支持点歌、AI 点歌、队列、播放控制、音量、状态、歌词、大厅检测、大厅时间、邀请和麦克风切换
- 点歌默认使用 QQ 音乐源，也可以用命令指定网易云音乐源
- 点歌会先搜索候选，搜到后让用户确认、跳过、换源或改用 AI；超时会自动确认
- 播放统一走搜索结果 URI，避免播放器内部模糊搜索选错歌
- AI 点歌会先从各音乐源搜索候选，再让 AI 从候选里选择最合适的一项
- 队列支持持久化，当前歌曲快结束、暂停或停止时可自动出队播放下一首
- 命令执行在后台线程进行，聊天扫描会持续运行
- 好友粉色命令支持邀请、麦克风切换、禁用/启用大厅命令识别
- 提供本地网页和接口面板，默认监听 `127.0.0.1:18888`
- 运行状态、队列和日志都会写入本地文件
- 提供手动调试工具，用于截图、识别、聊天扫描、界面状态、模板匹配、发送测试、AI 搜索测试等
- 支持全局热键：`F7` 暂停/恢复扫描，`F12` 退出

## 构建 Windows 程序

仓库内置 GitHub Actions 工作流：`.github/workflows/build-windows-exe.yml`

这个工作流不会在每次推送时自动运行，需要在 GitHub Actions 页面手动执行构建。构建成功后会创建或更新 GitHub Release，并上传 Windows x64 压缩包

构建会生成 `x86_64-pc-windows-msvc` 发布产物，并下载 PP-OCRv6 小模型：

```text
models/PP-OCRv6_small_det.mnn
models/PP-OCRv6_small_rec.mnn
models/ppocr_keys_v6_small.txt
```

发布包结构大致如下：

```text
程序发布包/
├── 程序.exe
├── MNN.dll
├── config.yaml
├── assets/
├── models/
└── README.md
```

## 本地构建

正式发布建议使用 Windows MSVC 目标，并使用仓库内置的 MNN 3.6.0 Windows x64 动态库：

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

仓库包含所需的 MNN 运行文件：

```text
vendor/mnn/3.6.0/windows-x64/include/
vendor/mnn/3.6.0/windows-x64/lib/MNN.lib
vendor/mnn/3.6.0/windows-x64/bin/MNN.dll
```

构建时会把 `MNN.dll` 复制到发布程序旁边。识别后端默认使用 CPU。发布路径不支持 CUDA、Vulkan、OpenCL、源码编译版 MNN 或其他预编译 MNN 包

运行机器还需要安装 Microsoft Visual C++ Redistributable 2015-2022 x64，因为官方 MNN 动态库依赖 `MSVCP140.dll`、`VCRUNTIME140.dll` 和 `VCRUNTIME140_1.dll`

Linux 可以用来做 Windows GNU 目标的检查，但这不是正式发布构建路径：

```bash
cargo check --target x86_64-pc-windows-gnu --features ocr-rs/docsrs
```

## 运行

直接运行：

```powershell
程序.exe
```

指定配置文件运行：

```powershell
程序.exe --config config.yaml run
```

发布包内已包含带注释的 `config.yaml`。程序不会自动生成配置文件；如果配置文件不存在，请把发布包里的 `config.yaml` 放在程序工作目录，或用 `--config` 指定路径

## 运行要求

- Windows 系统
- 某动漫游戏进程正在运行，并已在 `config.yaml` 的 `window.target_process` 中填写进程名
- FeelUOwn 已启动并开启 TCP RPC，默认地址 `127.0.0.1:23333`
- `models/` 目录里有识别模型
- `assets/` 目录里有聊天标志和界面按钮模板
- 已安装 Microsoft Visual C++ Redistributable 2015-2022 x64

## 聊天命令

普通聊天命令需要出现在聊天名称分隔符之后，并以 `@` 开头

```text
用户: @点歌 晴天
用户: @AI点歌 晴天 周杰伦
用户: @QQ点歌 晴天
用户: @网易点歌 晴天
用户: @暂停
用户: @继续
用户: @播放
用户: @下一首
用户: @上一首
用户: @音量 30
用户: @状态
用户: @歌词
用户: @队列
用户: @队列删除 1
用户: @队列清空
用户: @帮助
用户: @大厅检测
用户: @大厅时间
```

粉色好友命令：

```text
@邀请1
@过来
@麦克风
@禁用
@启用
```

说明：

- `@点歌` 默认使用 QQ 音乐源
- `@QQ点歌` 强制使用 QQ 音乐源
- `@网易点歌` 强制使用网易云音乐源
- 歌名里带“伴奏”时会优先匹配伴奏版本
- `@麦克风` 只执行一次状态切换，不再判断当前开关状态
- `@过来` 是默认 `custom_workflows` 参考命令，好友私聊触发，效果等同邀请 BOT 前往该好友大厅
- `@禁用` 会停止识别蓝色/黄色大厅命令，但粉色好友命令仍然可用
- `@启用` 会恢复蓝色/黄色大厅命令识别

点歌确认命令：

```text
@确认
@跳过
@换源
@AI
```

当点歌搜到候选时，机器人会回复类似：

```text
搜索到:歌曲名,@确认@跳过@换源@AI
```

20 秒内无人选择时会自动确认。没有搜到候选时会提示当前平台没有对应音源，并允许换源或改用 AI

## 自定义流程

`config.yaml` 的 `custom_workflows` 可以添加配置驱动的大厅或好友命令。内置命令优先于自定义命令；默认自定义命令不接受参数，需要参数时给流程设置 `allow_args: true`

顶层参数：

- `enabled`：是否启用配置驱动的自定义流程命令。关闭后只识别内置命令
- `default_threshold`：模板匹配默认阈值，用于 `click_template`、`wait_template`、`wait_template_absent`，单个步骤可用 `threshold` 覆盖
- `default_timeout_ms`：等待模板或 OCR 文字出现/消失的默认超时时间，单位毫秒，单个步骤可用 `timeout_ms` 覆盖
- `default_poll_ms`：等待模板或 OCR 文字时的默认轮询间隔，单位毫秒，实际最小值为 50ms，单个步骤可用 `poll_ms` 覆盖
- `default_step_wait_ms`：非 `sleep/wait` 步骤执行后的默认等待时间，单位毫秒，单个步骤可用 `wait_ms` 覆盖
- `templates`：模板别名到图片路径的映射。步骤里的 `template` 会先按别名查这里，查不到时把 `template` 当作实际文件路径
- `workflows`：自定义流程列表，每一项定义一个可触发的命令流程

单个 `workflow` 参数：

- `enabled`：是否启用这个流程
- `name`：流程名，用于日志、执行定位和去重；为空时使用命令名作为流程名
- `commands`：触发命令列表，写命令名本身即可，例如 `测试流程`，聊天里使用 `@测试流程`
- `allow_args`：是否允许命令后带参数。为 `false` 时 `@测试流程 参数` 不会匹配；为 `true` 时参数可通过变量读取
- `message_types`：允许触发的聊天类型，常用 `blue` 或 `pink`；留空表示不限制类型
- `confirm_before_run`：执行步骤前是否需要确认。为 `true` 时会先发送确认提示，收到允许来源里的 `@确认` 才继续执行，收到 `@跳过` 或超时会取消
- `confirm_message`：确认提示内容。为空时使用默认提示；支持变量
- `confirm_message_types`：确认命令来源。`[blue]` 只接受大厅确认，`[pink]` 只接受好友私聊确认，`[blue, pink]` 两者都接受，留空表示不限制来源
- `confirm_timeout_ms`：确认等待超时时间，单位毫秒；不填时使用 `timing.decision_timeout_ms`
- `confirm_poll_ms`：确认等待轮询间隔，单位毫秒；不填时使用 `timing.decision_poll_ms`，实际最小值为 50ms
- `steps`：按顺序执行的步骤列表。任一步骤失败会中止流程，并走普通命令失败后的返回处理
- `success_message`：全部步骤成功后发送到大厅的消息；空字符串表示不发送

步骤通用参数：

- `type`：步骤类型，必填
- `wait_ms`：对 `sleep/wait` 表示等待时长；对其他步骤表示该步骤完成后的额外等待时长
- `timeout_ms`：覆盖等待模板或 OCR 文字的超时时间；`sleep/wait` 没有 `wait_ms` 时也会使用它
- `poll_ms`：覆盖等待模板或 OCR 文字时的轮询间隔
- `threshold`：覆盖模板匹配阈值
- `region`：模板匹配或 OCR 找文字的屏幕区域，格式为 `{ x, y, width, height }`
- `point`：固定点击点，格式为 `{ x, y }`
- `click_offset`：模板或文字命中点的点击偏移，格式为 `{ x, y }`
- `template`：模板别名或图片路径，用于模板相关步骤
- `key`：按键名，用于 `key/press_key`，支持 `Enter`、`Esc`、`F1` 到 `F12` 和单字符按键
- `text`：文字内容，用于 `click_text`、`wait_text`、`paste`，也可作为发送消息的兜底内容
- `message`：发送内容，用于 `send_chat`、`send_current_chat`、`send_friend_message`，优先于 `text`
- `target`：好友名，用于 `send_friend_message`；不填时默认发送给触发命令的好友

支持的步骤类型：

- `sleep` / `wait`：等待一段时间
- `key` / `press_key`：向游戏窗口发送按键
- `click`：点击固定坐标 `point`
- `click_template`：在 `region` 内等待模板出现，命中后点击模板中心加 `click_offset`
- `wait_template`：在 `region` 内等待模板出现，不点击
- `wait_template_absent`：在 `region` 内等待模板消失
- `click_text`：在 `region` 内 OCR 查找 `text`，命中后点击文字中心加 `click_offset`
- `wait_text`：在 `region` 内 OCR 查找 `text`，命中后继续，不点击
- `paste` / `paste_text`：把 `text` 粘贴到当前焦点
- `send_chat` / `reply`：按普通大厅回复流程发送 `message`
- `send_current_chat`：向当前已打开的聊天输入框发送 `message`
- `send_friend_message` / `friend_reply`：打开 `target` 好友聊天，发送 `message`，然后返回一级界面
- `invite_user` / `invite_current_user`：调用内置邀请流程，默认邀请触发命令的好友；也可以用 `target` 指定好友名
- `return_primary`：尝试返回一级界面

步骤里的 `text`、`message`、`target`、`template`、`key`、`success_message` 支持变量：

- `{{username}}` / `{{user}}`：触发命令的用户名
- `{{args}}` / `{{param}}` / `{{params}}`：命令后的完整参数
- `{{arg1}}`、`{{arg2}}`：按空白分隔后的第 1、2 个参数，序号从 1 开始
- `{{command}}` / `{{command_name}}`：匹配到的命令名
- `{{workflow}}` / `{{workflow_name}}`：流程名
- `{{message_type}}`：消息类型，例如 `blue` 或 `pink`
- `{{user_command}}`：用户原始命令文本

## 手动调试工具

启动手动工具：

```powershell
程序.exe --config config.yaml manual
```

手动菜单包含：

- 截图保存
- 指定区域文字识别
- 聊天区扫描
- 界面状态检测
- 模板匹配
- 坐标点击
- 按键测试
- 聊天发送测试
- 识别后端支持检测
- 聊天变化监听
- 面板响应耗时测试
- AI 点歌搜索测试

面板响应耗时测试会按 `Enter` 打开聊天，再按 `Esc` 关闭聊天，采样配置中的检测区域，输出首次变化耗时、稳定耗时、平均值、最大值和失败次数

## 本地网页和接口

默认监听地址：

```text
http://127.0.0.1:18888
```

会拒绝跨站控制请求。会改变状态的接口只接受本机或同源请求

主要接口：

```text
/status
/play
/pause
/skip-next
/skip-prev
/volume
/searchPlay
/searchSource
/search
/open-scheme
/queue
/queue/add
/queue/remove
/queue/clear
/state
/state/save
/history
/clear-history
/health
/admin-status
/restart-admin
/active-window
/ai/recognize
/ai/match
/ai/pick
/ai/search
```

`/ai/search` 用于测试 AI 点歌搜索。它会先调用 FeelUOwn 搜索候选，再让 AI 从候选中选择 URI，不会让 AI 自己改写搜索词

## 配置说明

`config.yaml` 包含 `config_version`。如果检测到旧版本配置，程序会用最新带注释模板重写配置，把旧值迁移到新位置，创建带时间戳的 `.bak-*` 备份，并把无法自动迁移的旧字段追加到文件末尾作为注释。追加的注释字段不会影响运行

默认聊天区域：

```yaml
screen:
  chat_rect:
    x: 39
    y: 879
    width: 416
    height: 143
```

当前主要时间参数默认值：

```yaml
timing:
  scan_loop_idle_ms: 60
  chat_scan_fallback_ms: 2000
  chat_change_debounce_ms: 120
  chat_change_cooldown_ms: 250
  post_command_settle_ms: 500
  command_ui_timeout_ms: 15000
  decision_timeout_ms: 20000
```

队列默认配置：

```yaml
queue:
  max_size: 5
  auto_advance_seconds: 2
```

游戏内回复会限制显示宽度为 80，约等于 40 个全角中文字符或 80 个半角字符

## 日志和状态文件

- 日志：`logs/程序日志.log`
- 运行状态：`data/runtime-state.json`
- 点歌队列：`data/queue.json`

日志前缀格式：

```text
[MM-DD HH:MM:SS][INFO ] : message
```

时间使用 UTC+8

## 许可证

本项目使用 MIT 许可证，详见 [LICENSE](LICENSE)

第三方组件保留各自许可证。仓库内置的 MNN 二进制文件和头文件使用 Apache-2.0，详见 `vendor/mnn/3.6.0/LICENSE.txt`
