# Miliastra Wonderland Music

这是一个仅支持 Windows 的 Rust 点歌机器人。程序会截取目标游戏窗口客户区，识别聊天区里的命令，通过 FeelUOwn 控制搜索、播放、队列和状态，再用键盘鼠标把回复发回游戏聊天

## 主要功能

- 只截取目标游戏窗口客户区，不截取整个桌面
- 通过蓝色、黄色、粉色聊天标志切分聊天块；命令只处理蓝色大厅消息和粉色好友私聊
- 支持大厅点歌、AI 点歌、搜索确认、队列、播放控制、音量、状态、歌词、大厅检测和大厅剩余时间
- 支持好友私聊点歌、邀请、麦克风切换、非粉色命令开关、闲置退出、UID 拉黑和 UID 屏蔽聊天
- 支持运行期切换一级/二级聊天监听；二级监听会监听当前大厅气泡，并通过好友未读红点处理最新私聊命令
- 点歌会先搜索候选，再等待 `@确认`、`@跳过`、`@换源`、`@AI`；超时按确认处理
- 播放统一走搜索结果 URI，减少播放器内部模糊搜索选错歌的情况
- AI 点歌会先从音乐源搜索候选，再让 AI 从候选里选择，不让 AI 直接编歌曲 URI
- 可选启用候选歌曲 AI 审核，按最终候选歌曲/URI 的风险强度决定是否允许点歌
- 队列会持久化，当前歌曲接近结束、暂停或停止时可以自动播放下一首，并可按实际播放历史限制短期重复点同一首歌
- 命令执行在后台任务里跑，聊天扫描会持续运行
- 支持启动时自动启动官服/国际服游戏、自动开门，并进入千星奇域大厅后停止，不自动返回提瓦特
- 提供本地网页和 HTTP 接口，初始监听 `127.0.0.1:18888`
- 提供终端 TUI，显示日志、OCR 和队列状态
- 提供 Web 二级高级控制页，用于截图、OCR、聊天扫描、UI 状态、命名模板匹配、输入测试和 AI 候选诊断
- 支持全局热键：`F7` 暂停/恢复扫描，`F12` 退出程序

## 代码与流程文档导航

代码梳理文档入口见 [docs/index.md](docs/index.md)。常用阅读路径：

- 想理解整体架构，先看 [代码梳理](docs/code-walkthrough.md)
- 想排查命令重复、锁失效或入队问题，看 [聊天命令进入队列](docs/chat-command-ingestion.md) 和 [命令模型与屏幕锁](docs/command-model-locks.md)
- 想了解二级聊天界面、好友未读红点和回退规则，看 [二级聊天监听](docs/secondary-chat-listener.md)
- 想排查 OCR、UI 检测或性能耗时，看 [OCR 与 UI 检测](docs/ocr-ui-detection-flow.md)
- 想调整点击、模板、像素稳定或自定义步骤，看 [UI 自动化与原子动作](docs/ui-automation-atoms.md)
- 想看邀请、好友反馈、拉黑/屏蔽投票，看 [自定义工作流、邀请与管理流程](docs/custom-workflow-moderation-flow.md)
- 想看启动游戏、开门和进入千星，看 [启动游戏与进入千星](docs/startup-wonderland-flow.md)
- 想看 Web 面板和 HTTP 接口，看 [Web 监控与 HTTP API](docs/web-monitor-api.md)；高级控制规则见 [Web 高级控制](docs/web-tools.md)

## 发布包

GitHub Actions 工作流在 `.github/workflows/build-windows-exe.yml`

触发方式：

- 在 GitHub Actions 页面手动执行 `workflow_dispatch`
- 推送到 `main` 或 `dev`，并且本次推送包含 EXE 构建相关文件

推送触发时会自动构建并上传可运行构建产物。创建 GitHub Release 仍然只在手动执行工作流，或 `Cargo.toml` 的 `[package].version` 发生变化时进行

构建产物是 `x86_64-pc-windows-msvc`，会下载 PP-OCRv6 小模型：

```text
models/PP-OCRv6_small_det.mnn
models/PP-OCRv6_small_rec.mnn
models/ppocr_keys_v6_small.txt
```

发布包结构：

```text
miliastra-wonderland-music-windows-x64/
├── miliastra-wonderland-music.exe
├── MNN.dll
├── config.yaml
├── assets/
├── models/
├── README.md
└── THIRD_PARTY_NOTICES.md
```

`config.yaml` 是运行必需文件。程序不会在运行时创建新配置文件

## 本地构建

正式发布使用 Windows MSVC 目标，并使用仓库内置的 MNN 3.6.0 Windows x64 头文件和导入库编译 EXE：

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

仓库包含所需的 MNN 编译和运行文件：

```text
vendor/mnn/3.6.0/windows-x64/include/
vendor/mnn/3.6.0/windows-x64/lib/MNN.lib
vendor/mnn/3.6.0/windows-x64/bin/MNN.dll
```

`cargo build` 只编译 EXE，不负责生成或复制 `MNN.dll`。发布包工作流会显式把仓库内置的 CPU 版 `MNN.dll` 放到 EXE 旁边；本地直接运行 target 里的 EXE 时，需要手动把 ABI 兼容的 `MNN.dll` 放到 EXE 旁边，或把 DLL 所在目录加入 `PATH`

EXE 与 `MNN.dll` 分开编译。普通发布包默认使用 CPU OCR；CUDA MNN 运行时包可以单独构建，使用时把其中的 `MNN.dll` 和 CUDA runtime DLL 放到 EXE 旁边，并把 `config.yaml` 中的 `ocr.backend_priority` 改为先 `cuda` 后 `cpu`。默认的 CUDA 12.4.1 运行时面向 P100/Pascal，要求 Windows NVIDIA 驱动 551.78 或更新版本

运行机器还需要安装 Microsoft Visual C++ Redistributable 2015-2022 x64，因为官方 MNN 动态库依赖 `MSVCP140.dll`、`VCRUNTIME140.dll` 和 `VCRUNTIME140_1.dll`

Linux 可以用来做 Windows GNU 目标检查，但这不是正式发布构建路径：

```bash
cargo check --target x86_64-pc-windows-gnu --features ocr-rs/docsrs
```

## 运行

直接运行：

```powershell
miliastra-wonderland-music.exe
```

程序固定从启动工作目录读取 `config.yaml` 并进入常驻监听模式。不再提供命令行子命令、手动菜单或通过参数切换配置文件的运行方式。

OCR、模板、坐标点击、按键和 AI 候选诊断请在 `http://127.0.0.1:18888/tools` 的高级控制页执行。工具任务只会在正式业务队列为空时启动；失败只显示在工具结果中，不会退出程序或中断监听。

## 运行要求

- Windows 系统
- 程序必须以管理员权限启动
- `config.yaml` 的 `window.target_process` 已填写目标游戏进程名；官服/国际服可用逗号分隔，例如 `yuanshen.exe,GenshinImpact.exe`
- 如启用 `startup.launch_game`，游戏未启动时会优先使用 `startup.exe_path`；该项可以填写完整 exe 文件路径，也可以填写 exe 所在目录，留空时会尝试从米哈游启动器注册表查找官服/国际服安装路径
- FeelUOwn 已启动并开启 TCP RPC，初始地址为 `127.0.0.1:23333`
- `models/` 目录里有 OCR 模型
- `assets/` 目录里有聊天标志和界面按钮模板
- 已安装 Microsoft Visual C++ Redistributable 2015-2022 x64
- 游戏画面、配置坐标和模板素材需要对应同一套分辨率，初始配置按 1920x1080 客户区写坐标

## 启动游戏与进入千星

默认配置会在程序启动后按顺序排队执行“启动游戏”和“进入千星”两个任务。它们和点歌、邀请、控制台发言共用主业务队列，不会在 HTTP 线程直接操作窗口。

启动游戏任务流程：

1. 查找 `window.target_process` 中的官服/国际服游戏窗口
2. 找不到窗口时按 `startup.exe_path` 或注册表路径启动游戏
3. 检测到游戏窗口后聚焦游戏窗口一次
4. 每轮先匹配全局 `templates.enter` 在 `screen.enter_rect` 中是否已出现；出现即认为已经进入一级界面并完成任务
5. 未出现时才在 `startup.enter_game_text_region` 循环 OCR，最多 60 秒等待“点击进入”四个字
6. 识别到“点击进入”后点击文本框中心，并持续点击直到该文字不再出现
7. 文字消失后等待全局 `templates.enter` 出现，出现后认为启动游戏任务完成

进入千星任务流程：

1. 查找并聚焦已有游戏窗口；找不到窗口时直接失败，需要先执行启动游戏任务
2. 按 `startup.wonderland_home_retries` 和 `startup.wonderland_home_retry_ms` 周期性按 `F6`，直到在右上角区域检测到千星奇域主页的 `wonderland_close` 模板
3. 按 `startup.wonderland_card_retries` 和 `startup.wonderland_card_retry_ms` 点击配置的第一个奇域卡片坐标，在 `(1400,850,360,150)` 快速轮询并匹配“前往大厅”按钮模板
4. 匹配成功后只点击一次“前往大厅”按钮
5. 点击后继续快速轮询同一区域，最多等待 `startup.wonderland_confirm_absent_timeout_ms` 让模板消失，再最多等待 `startup.wonderland_confirm_stable_timeout_ms` 让区域像素稳定
6. 进入千星后执行返回一级界面流程，然后任务完成，后续点歌、邀请等任务可以继续执行

返回一级界面只回到千星内可操作的主界面，不自动退出千星。

远程面板里的两个按钮分别调用：

```text
POST /startup/game
POST /startup/wonderland
```

它们只会把对应任务加入待执行队列。执行结果通过事件日志、待执行任务和 TUI 查看。

## 聊天命令规则

普通大厅命令来自蓝色聊天消息，格式需要包含用户名分隔符，并以 `@` 开头：

```text
用户: @点歌 晴天
用户: @状态
```

好友私聊命令来自粉色聊天消息，用户名需要能从中括号里识别：

```text
[好友名]：@麦克风
```

黄色聊天标志只用于辅助切分聊天块，不作为命令来源。`@帮助` 只展示基础命令，不展示隐藏命令和管理命令

## 二级聊天监听

`@监听模式 二级` 仅接受好友私聊，或由 Web 面板/API 发起。切换任务会进入待执行任务队列：程序按 `Enter → 点击当前大厅模板 → OCR 确认当前大厅` 打开二级界面，最多尝试两次；第二次前会先用 `Esc` 回到一级界面。失败后自动回退一级监听。

二级监听不使用一级聊天的命令屏幕锁。它只对最下方最新的他人深色气泡做 OCR：当前大厅映射为 `blue`，好友会话映射为 `pink`。左侧好友未读红点只扫描当前可见区域；首次进入会逐个清掉可见旧红点而不执行命令，之后每次只处理一位好友的最新消息，并返回当前大厅监听一轮。陌生人消息不会触发命令；公开频道会尝试切回当前大厅，失败后退回一级监听。

`@监听模式 一级` 切回原有一级监听，`@监听模式 状态` 只记录当前模式和等待切换状态。普通命令不再统一先回一级：每个动作只在需要时转换界面，任务完成后恢复当前监听驻留目标。点歌和邀请确认可直接读取二级当前大厅的新气泡；管理投票仅在等待多位好友表决期间显示为“二级监听（临时一级阶段）”，最后一个投票结束后恢复当前大厅。

## 大厅命令

| 命令 | 说明 |
| --- | --- |
| `@点歌 歌名 歌手` / `@搜索 歌名 歌手` | 使用 QQ 音乐搜索并点歌 |
| `@AI点歌 歌名 歌手` / `@AI搜索 歌名 歌手` | 先搜索候选，再让 AI 选择 |
| `@QQ点歌 歌名 歌手` / `@QQ搜索 歌名 歌手` | 强制使用 QQ 音乐 |
| `@网易点歌 歌名 歌手` / `@网易搜索 歌名 歌手` | 强制使用网易云音乐 |
| `@暂停` | 暂停播放 |
| `@继续` / `@恢复` / `@播放` | 继续播放 |
| `@下一首` / `@下一曲` | 播放下一首 |
| `@上一首` / `@上一曲` | 播放上一首 |
| `@音量 30` | 设置音量，范围 `0-100` |
| `@状态` | 查询当前播放状态 |
| `@歌词` | 查询当前歌词 |
| `@队列` / `@列表` | 查看点歌队列 |
| `@队列删除 1` | 删除队列第 1 首。支持写多个数字，如 `@队列删除 13` |
| `@队列清空` | 清空队列 |
| `@大厅检测` | 打开 F2 大厅页识别当前大厅 |
| `@大厅时间` | 回复已识别到的大厅剩余时间 |
| `@接龙 开始 成语` / `@接龙 成语` | 开始或继续严格同字成语接龙 |
| `@同音接龙 开始 成语` | 开始忽略声调的同音接龙，后续仍可用 `@接龙 成语` |
| `@提示` / `@解释 [成语]` | 推荐不会立即封龙的可接成语；查看指定成语或当前成语的来源与解释 |
| `@海龟汤` / `@海龟汤状态` | 开始海龟汤、重发汤面或查看对局状态 |
| `#问题` | 在进行中的海龟汤里提交 AI 裁决问题 |
| `##编号内容` / `##提交` | 按编号暂存超过聊天长度的海龟汤答案，完整合并后只裁决一次 |
| `@帮助` | 发送基础帮助 |

点歌说明：

- 歌名里带 `伴奏` 或 `伴唱` 时，会优先匹配伴奏/伴唱候选
- `@点歌`、`@搜索`、`@AI点歌`、`@AI搜索` 在大厅里不接受 B 站源
- `@B站点歌` 和 `@B站搜索` 只在粉色好友私聊里生效
- AI 功能需要配置 `ai.api_key`，留空时 AI 命令会回复未启用

## 海龟汤

海龟汤默认关闭，只在当前大厅接收 `#` 提问，并与成语接龙全局互斥。题目选中后会先永久写入已使用记录，再分段发送汤面；汤面全部发完后才开始计时和接收提问。完整还原需要两次独立 AI 裁决都返回“完全正确”。长答案可以依次发送 `##1第一段`、`##2第二段`，最后发送 `##提交`；机器人只短回复 `暂存[昵称]:##编号`，提交时按编号合并成一次 AI 裁决。普通裁决会附带问题摘要，例如 `[星念]的男人是灯....理员吗？回复：否`。昵称和正文分别需要连续 OCR 一致后才会处理，默认各为 2 次，可通过 `turtle_soup.nickname_stable_count` 和 `turtle_soup.content_stable_count` 调整；问题相同且昵称只有一个规范化字符差异时按同一条 OCR 消息去重。

正式题库使用被 Git 忽略的 `turtle_soup.yaml`，格式参考 `turtle_soup.example.yaml`。Web 面板可以随机开局、按未使用 ID 开局、查看状态和主动结束，但不能查看进行中的汤底、编辑题库或重置使用记录。详细规则见 [海龟汤](docs/turtle-soup.md)。

## 长时间同歌去重

`song_dedup` 用实际播放历史保护同一首歌的短时间重复播放。它按最终候选歌曲判断，不按原始点歌文本判断；URI 相同一定视为同一首歌，跨平台歌曲会用歌名和歌手相似度兜底。伴奏和原唱默认视为不同歌曲。

去重会在直接播放前、加入音乐播放队列前、音乐播放队列出队准备播放前检查。入队前检查只负责拒绝确定的近期重复歌曲，不会写入历史；只有确认播放开始成功后才会写入 `history_path`，所以搜索失败、入队、审核拒绝、取消确认都不会污染历史。

超出限制时，直接播放或入队会在大厅回复 `歌曲名近期已播放过,请稍后再点`；队列出队会移除该项并回复 `歌曲名近期已播放过,已跳过`，然后继续尝试下一首。控制台来源默认豁免，可以通过 `song_dedup.console_bypass` 关闭。

## 候选歌曲审核

`song_review` 是独立于 `ai` 的候选歌曲审核 Provider，目前仅支持阿里云百炼 OpenAI 兼容 Chat Completions 接口。它审核的是已经搜索、确认或 AI 选出的最终候选歌曲和 URI，不审核原始点歌意图。审核请求会强制携带 `enable_search: true`、`search_options.forced_search: true` 和 `enable_thinking: false`，要求模型使用联网搜索信息判断歌曲是否适合舒缓、轻松、不吵闹的房间氛围。控制台是最高权限入口：远程点歌、远程 AI 点歌、控制台发言和直接队列接口都无视审核；游戏内大厅和好友私聊点歌会在播放或入队前审核。

审核模型必须返回 JSON：

```json
{"level":4,"reason":"整体偏舒缓，适合当前房间氛围","tags":["calm","soft"]}
```

`level` 是 1-10 的打扰强度，1 表示很安静很舒缓，10 表示极端吵闹混乱。程序用 `song_review.max_allowed_level` 做本地阈值判断，`level <= max_allowed_level` 通过，超过阈值会拒绝本次点歌并在游戏内回复简短原因。审核请求失败会按 `retry_count` 重试，仍失败时按 `failure_policy` 决定拒绝或放行。

审核条件可以直接改 `song_review.policy_prompt`，但不要在里面要求模型改变 JSON 输出格式：

```text
审核目标：只通过整体听感偏舒缓、柔和、轻松、安静、治愈、抒情、慢节奏或中低强度的歌曲。
拒绝明显炸场、吵闹、压迫感强、节奏过快、情绪过激、强烈电子噪音、重金属、硬核、鬼畜、洗脑循环、尖锐喊叫、强烈攻击性或明显破坏房间氛围的歌曲。
尽量使用联网搜索得到的曲风、歌词摘要、歌曲介绍和公开听感描述判断。
如果信息不足，请保守判断；不确定时应给较高强度等级，而不是因为歌曲热门、用户喜欢或歌手知名就放宽标准。
```

`song_review.custom_prompt` 会附加在审核条件后面，适合临时补充房间口径。

## 点歌确认命令

当搜到候选或匹配不确定时，机器人会发类似内容：

```text
搜索到:歌曲名,@确认@跳过@换源@AI
```

可用回复：

| 命令 | 说明 |
| --- | --- |
| `@确认` | 接受当前候选 |
| `@跳过` | 取消本次点歌 |
| `@换源` | 在 QQ 音乐和网易云音乐之间换源重搜 |
| `@AI` | 改用 AI 从候选里选择 |

20 秒内没有选择时，按 `@确认` 处理

## 好友私聊命令

粉色好友私聊支持点歌，也支持管理类命令

| 命令 | 说明 |
| --- | --- |
| `@点歌 歌名 歌手` / `@搜索 歌名 歌手` | 好友私聊点歌，使用 QQ 音乐 |
| `@AI点歌 歌名 歌手` / `@AI搜索 歌名 歌手` | 好友私聊 AI 点歌 |
| `@QQ点歌 歌名 歌手` / `@QQ搜索 歌名 歌手` | 好友私聊 QQ 音乐点歌 |
| `@网易点歌 歌名 歌手` / `@网易搜索 歌名 歌手` | 好友私聊网易云音乐点歌 |
| `@B站点歌 关键词` / `@B站搜索 关键词` | 好友私聊 B 站源点歌 |
| `@邀请1` / `@邀请123456` | 邀请机器人前往该好友大厅；1-3 位数字表示防冲突序号，6 位数字表示大厅密码并会在最后一步键盘输入 |
| `@麦克风` | 按 `N` 切换麦克风状态；当前在公共大厅时跳过 |
| `@监听模式 一级` / `@监听模式 二级` / `@监听模式 状态` | 切换或查询聊天监听模式；大厅蓝色消息不能使用 |
| `@海龟汤结束` | 以好友权限主动结束当前海龟汤；结算和汤底统一发往当前大厅 |

邀请确认命令在蓝色大厅消息里发送：

| 命令 | 说明 |
| --- | --- |
| `@邀请确认` / `@同意邀请` | 同意好友邀请 |
| `@邀请拒绝` / `@拒绝邀请` | 拒绝好友邀请 |

邀请流程会先检测当前是否公共大厅。当前是公共大厅时直接执行；不是公共大厅时会在大厅发起确认，30 秒内无人回复则按同意处理

同意或默认同意时，机器人会先向发起者发送私聊反馈。反馈发送成功后会保留当前好友会话，直接在该会话继续查找并点击邀请入口，不再额外返回一级界面后重复打开好友聊天；私聊反馈失败时才回退到原有的一级界面打开流程。拒绝邀请和普通好友反馈仍会返回一级界面。

## 隐藏命令

这些命令不会出现在 `@帮助` 里，但只要粉色好友私聊能被识别就会执行

| 命令 | 说明 |
| --- | --- |
| `@禁用` | 关闭非粉色命令识别。大厅命令和蓝色自定义流程会被跳过，粉色好友命令仍可用 |
| `@启用` | 恢复非粉色命令识别 |
| `@闲置退出` | 设置 30 分钟无新命令后关闭目标游戏进程，软件主进程继续运行 |
| `@闲置退出 20` / `@闲置退出 20分钟` | 设置闲置退出时间，最小值为 15 分钟 |
| `@拉黑UID123456789` / `@拉黑123456789` | 发起 UID 拉黑投票，UID 必须是 9 位数字 |
| `@屏蔽UID123456789` / `@屏蔽123456789` | 发起 UID 屏蔽聊天投票，UID 必须是 9 位数字 |

`@禁用` 不会影响粉色私聊里的 `@启用`、`@麦克风`、UID 操作等命令。进入新大厅后程序会重置命令识别状态

## UID 拉黑和屏蔽聊天

UID 操作只接受粉色好友私聊命令：

```text
[管理员]：@拉黑UID123456789
[管理员]：@屏蔽UID123456789
```

流程：

1. 机器人在大厅通告本次请求
2. 后台等待粉色好友私聊投票，不阻塞普通点歌、状态、切歌等命令
3. 好友发送 `@同意` 或 `@不同意`
4. 同一好友同一判决需要连续稳定识别 `stable_vote_samples` 次才计入
5. `同意人数 - 不同意人数 >= required_vote_margin` 时立即通过
6. 等待超时后，如果稳定反对人数为 `0`，也按通过处理
7. 投票通过后进入好友界面搜索 UID，执行拉黑或屏蔽聊天，并回大厅通告结果

初始投票参数：

```yaml
timing:
  moderation:
    vote_timeout_ms: 120000
    vote_poll_ms: 2000
moderation:
  stable_vote_samples: 3
  required_vote_margin: 3
```

同一个 `动作 + UID` 同时只会有一个投票或执行流程，重复请求会回复正在处理中

## 自定义流程

`config.yaml` 的 `custom_workflows` 可以添加配置驱动的大厅或好友命令。内置命令优先于自定义命令；自定义命令不带参数时能直接匹配，需要参数时给流程设置 `allow_args: true`

顶层参数：

| 参数 | 说明 |
| --- | --- |
| `enabled` | 是否启用配置驱动的自定义流程命令 |
| `default_threshold` | 模板匹配阈值，用于 `click_template`、`wait_template`、`wait_template_absent` |
| `templates` | 模板别名到图片路径的映射 |
| `workflows` | 自定义流程列表 |

自定义流程的默认等待时间统一放在 `timing.workflow.default_timeout_ms`、`timing.workflow.default_poll_ms` 和 `timing.workflow.default_step_wait_ms`。

单个 `workflow` 参数：

| 参数 | 说明 |
| --- | --- |
| `enabled` | 是否启用这个流程 |
| `name` | 流程名，用于日志、执行定位和去重；为空时使用命令名 |
| `commands` | 触发命令列表，写命令名本身即可，例如 `测试流程`，聊天里使用 `@测试流程` |
| `allow_args` | 是否允许命令后带参数 |
| `message_types` | 允许触发的聊天类型，支持 `blue`、`pink`；留空表示不限制这两类 |
| `confirm_before_run` | 执行步骤前是否需要确认 |
| `confirm_message` | 确认提示内容，支持变量 |
| `confirm_message_types` | 确认命令来源。`[blue]` 只接受大厅确认，`[pink]` 只接受好友私聊确认，留空表示不限 |
| `confirm_timeout_ms` | 确认等待超时时间，单位毫秒；不填时使用 `timing.decision.timeout_ms` |
| `confirm_poll_ms` | 确认等待轮询间隔，单位毫秒；不填时使用 `timing.decision.poll_ms` |
| `steps` | 按顺序执行的步骤列表，任一步骤失败会中止流程 |
| `success_message` | 全部步骤成功后发送到大厅的消息；空字符串表示不发送 |

步骤通用参数：

| 参数 | 说明 |
| --- | --- |
| `type` | 步骤类型，必填 |
| `wait_ms` | 对 `sleep/wait` 表示等待时长；对其他步骤表示完成后的额外等待时长 |
| `timeout_ms` | 覆盖等待模板或 OCR 文字的超时时间 |
| `poll_ms` | 覆盖等待模板或 OCR 文字时的轮询间隔 |
| `threshold` | 覆盖模板匹配阈值 |
| `region` | 模板匹配或 OCR 找文字的屏幕区域，格式 `{ x, y, width, height }` |
| `point` | 固定点击点，格式 `{ x, y }` |
| `click_offset` | 模板或文字命中点的点击偏移，格式 `{ x, y }` |
| `template` | 模板别名或图片路径 |
| `key` | 按键名，用于 `key/press_key`，支持 `Enter`、`Esc`、`F1` 到 `F12` 和单字符按键 |
| `text` | 文字内容，用于 `click_text`、`wait_text`、`paste`，也可作为发送消息的兜底内容 |
| `message` | 发送内容，用于 `send_chat`、`send_current_chat`、`send_friend_message`，优先于 `text` |
| `target` | 好友名，用于 `send_friend_message`；不填时发送给触发命令的好友 |

支持的步骤类型：

| 类型 | 说明 |
| --- | --- |
| `sleep` / `wait` | 等待一段时间 |
| `key` / `press_key` | 向游戏窗口发送按键 |
| `click` | 点击固定坐标 `point` |
| `click_template` | 在 `region` 内等待模板出现，命中后点击 |
| `wait_template` | 在 `region` 内等待模板出现，不点击 |
| `wait_template_absent` | 在 `region` 内等待模板消失 |
| `wait_stable` / `wait_pixels_stable` | 等待 `region` 内像素变化稳定，适合点击后等待面板动画结束 |
| `click_text` | 在 `region` 内 OCR 查找 `text`，命中后点击 |
| `wait_text` | 在 `region` 内 OCR 查找 `text`，命中后继续 |
| `paste` / `paste_text` | 把 `text` 粘贴到当前焦点 |
| `send_chat` / `reply` | 按普通大厅回复流程发送 `message` |
| `send_current_chat` | 向当前已打开的聊天输入框发送 `message`，不会重新激活窗口或点击全局聚焦点 |
| `send_friend_message` / `friend_reply` | 复用或打开二级好友会话，发送 `message`，然后恢复当前监听驻留界面 |
| `invite_user` / `invite_current_user` | 调用内置邀请流程，目标为触发命令的好友，也可用 `target` 指定 |
| `ensure_primary` / `return_primary` | 检测并通过 ESC 逐级到达一级界面 |
| `ensure_current_hall` | 检测并到达二级当前大厅 |

变量：

| 变量 | 说明 |
| --- | --- |
| `{{username}}` / `{{user}}` | 触发命令的用户名 |
| `{{args}}` / `{{param}}` / `{{params}}` | 命令后的完整参数 |
| `{{arg1}}`、`{{arg2}}` | 按空白分隔后的第 1、2 个参数，序号从 1 开始 |
| `{{command}}` / `{{command_name}}` | 匹配到的命令名 |
| `{{workflow}}` / `{{workflow_name}}` | 流程名 |
| `{{message_type}}` | 消息类型，例如 `blue` 或 `pink` |
| `{{user_command}}` | 用户原始命令文本 |

内置好友发言、邀请和 UID 管理流程也使用同一套原子动作：任务入口会显式激活并聚焦游戏一次，后续点击、按键、粘贴默认游戏已经可接收输入；返回一级界面只使用 ESC 逐级返回。等待 OCR 文字时会轮询到超时，等待模板时会轮询到出现或消失；好友发言在点击好友后短暂等待输入框接管焦点，邀请流程由下一步 OCR 直接确认页面状态。

## 高级控制

主面板为 `http://127.0.0.1:18888/`，高级控制页为 `http://127.0.0.1:18888/tools`。高级页提供截图、OCR、聊天扫描、UI/大厅识别、配置内命名模板匹配、坐标点击、按键、OCR 后端探测和 AI 候选诊断。

只有坐标点击、按键和模板点击会短暂占用游戏输入；其余工具在后台执行，不占用主业务执行状态。详情见 [Web 高级控制](docs/web-tools.md)。

## 本地网页和接口

初始监听地址：

```text
http://127.0.0.1:18888
```

会拒绝跨站控制请求。会改变播放、队列或状态的接口只接受本机或同源请求。主面板为 `/`，高级控制页为 `/tools`。

如将 `http.host` 改为非本机地址，必须设置非空的 `http.access_token`，否则 HTTP 服务不会启动。页面中的访问令牌仅保存于当前浏览器会话，并通过请求头发送。

接口列表：

| 接口 | 说明 |
| --- | --- |
| `/status` | FeelUOwn 播放状态 |
| `/play` | 远程继续命令入主业务队列 |
| `/pause` | 远程暂停命令入主业务队列 |
| `/skip-next` | 远程下一首命令入主业务队列，会走游戏内出队逻辑 |
| `/skip-prev` | 远程上一首命令入主业务队列 |
| `/volume?volume=30` | 远程音量命令入主业务队列 |
| `/searchPlay?keyword=...&source=qqmusic` | 远程点歌入主业务队列 |
| `/searchSource?keyword=...&source=netease` | 指定源远程点歌入主业务队列 |
| `/search?keyword=...&source=qqmusic` | 搜索歌曲 |
| `/search/candidates?keyword=...&source=qqmusic` | 返回结构化候选歌曲和 URI，供 Web 面板选择后入队 |
| `/player/play-uri?uri=fuo://...` | FeelUOwn URI 作为控制台高权限项加入音乐播放队列 |
| `/queue` | 查看队列 |
| `/queue/add?keyword=...` | 控制台最高权限直接加入队列，不经过候选歌曲审核 |
| `/queue/remove?id=...` | 按持久队列项 ID 删除；旧的 `index=0` 形式仍可用 |
| `/queue/clear` | 清空队列 |
| `/state` | 查看运行状态 |
| `/state/save` | 保存运行状态字段 |
| `/history` | 查看最近 30 条接口调用记录 |
| `/clear-history` | 清空接口调用记录 |
| `/monitor` | 查看监控面板数据 |
| `/turtle-soup` | 查看海龟汤状态；不返回进行中的汤底 |
| `/turtle-soup/start` / `/turtle-soup/start?id=...` | 随机或按未使用题目 ID 开局，只接受 POST |
| `/turtle-soup/end` | 主动结束并在当前大厅公布结算与汤底，只接受 POST |
| `/chat/send?text=...&usePrefix=1&prefix=...` | 控制台发言入主业务队列；默认带 `[控制台]: ` 前缀，可关闭或自定义 |
| `/chat-listener/mode?mode=primary` / `secondary` | 切换聊天监听模式，任务进入主业务队列 |
| `/tasks/cancel?id=...` | 撤销尚未开始的正式任务 |
| `/decisions/submit?id=...&action=confirm` | 提交当前点歌候选决策，支持 `confirm`、`skip`、`switch_source`、`ai` |
| `/operator/lyrics` | 发送歌词命令入主业务队列 |
| `/operator/hall-detect` | 大厅检测命令入主业务队列 |
| `/operator/hall-time` | 大厅时间命令入主业务队列 |
| `/operator/microphone` | 麦克风命令入主业务队列 |
| `/operator/commands?enabled=1` | 启用或禁用游戏内命令识别 |
| `/operator/idle-exit?minutes=30` | 设置闲置退出；`enabled=0` 表示取消 |
| `/operator/workflows` | 查看已启用的自定义工作流 |
| `/operator/workflows/run?name=...&args=...` | 以控制台权限执行自定义工作流 |
| `/screenshot?quality=88` | 手动获取一次游戏截图，返回 JPEG |
| `/health` | 健康检查 |
| `/ai/recognize` | AI 文本识别测试 |
| `/ai/match` | AI 匹配测试 |
| `/ai/pick` | AI 候选选择测试 |
| `/ai/search?keyword=...` | 远程 AI 点歌入主业务队列 |

正式业务接口会返回 `taskId`；组合启动流程返回 `taskIds`。`/monitor.tasks` 可查看等待、执行中、完成、失败和已撤销状态，只有尚未开始的任务允许撤销。远程播放控制、远程点歌、远程 AI 点歌和控制台发言的具体搜索、AI 匹配、播放、出队和游戏内反馈仍由主业务线程串行执行。控制台来源拥有最高权限，不受候选歌曲审核限制。程序必须以管理员权限启动，Web 面板不提供管理员状态或窗口状态诊断。

## 配置说明

`config.yaml` 包含 `config_version`。启动时如果检测到旧配置，程序会用当前发布包里的 `config.yaml` 模板重写配置，把旧值迁移到新位置，创建带时间戳的 `.bak-*` 备份，并把无法自动迁移的旧字段追加到文件末尾作为注释。追加的注释字段不会影响运行

如果配置文件不存在，程序会报错退出。请把发布包里的 `config.yaml` 放在程序启动工作目录。

主要配置段：

| 配置段 | 说明 |
| --- | --- |
| `window` | 目标进程名、画面尺寸、业务流程激活窗口参数 |
| `screen` | 截图尺寸、聊天区、一级/二级界面检测区、大厅 OCR 区域 |
| `timing` | 扫描、点击、发送、邀请、点歌确认、播放监控等时间参数 |
| `ocr` | OCR 模型、置信度、文本框参数、聊天变化检测参数 |
| `templates` | 聊天标志、UI、邀请、好友 UID 操作模板 |
| `output` | 游戏内回复开关和聊天输入坐标 |
| `moderation` | UID 拉黑/屏蔽投票参数和按钮搜索区域 |
| `feeluown` | FeelUOwn TCP RPC 地址 |
| `http` | 本地网页和接口监听配置 |
| `logging` | 日志目录和日志等级 |
| `tui` | 终端 TUI 开关和刷新参数 |
| `state` | 运行状态、队列和已执行命令记录路径 |
| `queue` | 队列长度、自动出队和当前歌曲保护 |
| `song_dedup` | 长时间同歌去重开关、统计窗口、允许次数、控制台豁免和历史路径 |
| `idiom_chain` | 成语接龙词库、历史、超时和结束权限 |
| `turtle_soup` | 海龟汤题库、永久使用记录、OCR 稳定次数、超时、批量答案段数、AI 并发、独立 Provider 和追加提示词 |
| `ai` | AI 供应商、API Key、模型和 OpenAI-compatible 地址 |
| `song_review` | 候选歌曲审核开关、阿里云百炼 Provider、打扰强度阈值、失败策略和审核条件 |
| `matching` | 歌名、歌手和 OCR 噪声匹配阈值 |
| `hotkeys` | 全局热键开关和按键 |
| `invite` | 好友列表 OCR 区域和邀请按钮模板搜索区域 |
| `custom_workflows` | 配置驱动的自定义命令流程 |

`timing` 已按业务场景分组，旧版本的扁平字段会在启动时自动迁移到新位置：

| 分组 | 说明 |
| --- | --- |
| `timing.chat_scan` | 聊天区变化检测、兜底扫描和变化 OCR 防抖 |
| `timing.command` | 命令执行前后等待、返回一级界面重试和帮助消息间隔 |
| `timing.input` | 激活窗口、打开聊天、点击、输入、发送和手动调试聚焦后的等待 |
| `timing.workflow` | 自定义原子流程的默认超时、轮询和步骤后等待 |
| `timing.hall` | F2 大厅页稳定等待和大厅信息 OCR 采样间隔 |
| `timing.invite` | 邀请流程的面板等待、步骤等待和邀请确认扫描 |
| `timing.moderation` | UID 拉黑/屏蔽投票、搜索结果等待和确认后等待 |
| `timing.playback` | 点歌后播放状态轮询、切歌状态轮询和播放监控线程 |
| `timing.decision` | 点歌确认、AI 匹配确认和自定义流程确认的默认等待 |
| `timing.external` | FeelUOwn RPC、音量平滑步进和 AI HTTP 请求超时 |

关键初始值：

```yaml
screen:
  chat_rect: { x: 39, y: 879, width: 416, height: 143 }

queue:
  max_size: 5
  auto_advance_seconds: 1
  protect_current_song_until_finished: true
  external_playback_protect_after_seconds: 20

song_dedup:
  enabled: true
  window_seconds: 3600
  max_count: 1
  console_bypass: true
  history_path: data/song-dedup-history.json

http:
  host: 127.0.0.1
  port: 18888
  enabled: true

hotkeys:
  pause_key: F7
  exit_key: F12
```

`protect_current_song_until_finished` 保护已确认的机器人点歌；队列自动出队仍由 `auto_advance_seconds` 控制，`@下一首` 仍会立即消费队列。`external_playback_protect_after_seconds` 用于非点歌歌曲：只有同一首外部歌曲保持可识别的 `playing` 状态达到配置秒数后，才加入当前歌曲保护；切歌瞬态、暂停、停止、歌曲身份变化和程序重启都会重新计时。设为 `0` 时，外部播放永不保护，音乐播放队列可以直接接管；未知状态只观察，不自动出队。

游戏内回复会限制显示宽度为 80，约等于 40 个全角中文字符或 80 个半角字符

## 日志和状态文件

| 文件 | 说明 |
| --- | --- |
| `logs/miliastra-wonderland-music-YYYY-MM-DD.log` | 程序日志，记录业务事件、扫描结果、错误和状态变化 |
| `logs/miliastra-wonderland-music-timing-YYYY-MM-DD.log` | 阶段耗时和性能诊断日志，包含 UI 检测、OCR、主循环、原子动作和输入阶段耗时 |

默认按日期每天分文件，并保留最近 `7` 个自然日；可通过 `logging.rotate_daily` 和 `logging.retain_days` 调整。日志轮转或清理失败不会中断监听。
| `data/runtime-state.json` | 运行状态 |
| `data/queue.json` | 点歌队列 |
| `data/song-dedup-history.json` | 长时间同歌去重历史，只记录确认播放成功的歌曲 |
| `data/executed-commands.log` | 已执行命令记录 |

日志前缀格式：

```text
[MM-DD HH:MM:SS][INFO ] : message
```

时间使用 UTC+8

聊天扫描的业务结果写入普通日志，格式类似：

```text
聊天扫描结果: markers=4 messages=4 [blue] 用户: @状态
```

终端 TUI 和网页监控页会把最新 OCR 内容显示在单独的 OCR 面板中，事件日志面板不会重复显示完整聊天扫描结果。TUI 的 OCR 面板只显示最新 5 条聊天识别结果，队列面板只显示前 5 首待播歌曲，默认布局会把 OCR 和队列压成短窗口，把更多高度留给命令和事件日志。

耗时和阶段拆分只写入性能日志，例如：

```text
UI 状态检测耗时: total=218ms enter=12ms hall=8ms marker=198ms state=primary_marker blue=4 yellow=0 pink=0
```

## 许可证

本项目使用 MIT 许可证，详见 [LICENSE](LICENSE)

第三方组件保留各自许可证。仓库内置的 MNN 二进制文件和头文件使用 Apache-2.0，详见 `vendor/mnn/3.6.0/LICENSE.txt`
