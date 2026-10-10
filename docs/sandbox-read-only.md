# 沙盒只读模式

在高级控制页 /tools 的“用户沙盒 · 游戏状态观测”卡片中开启只读观测，再选择主动读取或推送。它是观察游戏状态的功能，程序继续正常监听、点歌、播放和执行原有流程。观测默认关闭；启用后只限制沙盒自身发起的输入和业务写操作，不切换旧调试模式、不暂停正常业务。

| 状态 | 什么时候观测 | 返回内容 |
| --- | --- | --- |
| 主动读取 active | 点击“读取一次”或调用读取接口 | 新采集的原图、同帧 UI 分类、可选指定区域 OCR、请求参数和步骤 |
| 推送 push | 命令实际触发，或正式流程成功入队后发生状态变化 | 触发参数、任务标识、状态与结果、原始触发帧证据链接 |

推送不会发送普通聊天、每帧 OCR、大厅倒计时、播放进度或其他周期状态。重复入队、未触发的文本和被跳过的命令也不会生成新的触发记录。即时管理员命令和监听状态查询会记录为完成的即时触发，不伪造正式任务 ID。

## 页面操作

1. 打开运行程序的高级控制页，填写既有 HTTP 访问令牌。
2. 在“用户沙盒 · 游戏状态观测”中勾选启用，选择“主动读取”；勾选和切换会立即提交状态。
3. 点击“读取一次”，查看完整游戏原图与同帧识别结果。每次点击只采一次，不定时截图。
4. 卡片提供“下载完整 JSON”和“下载原始 PNG”。JSON 保留请求、流程信息、原帧元数据与图像数据；PNG 不缩放。
5. 切换为“推送”后，页面订阅专用事件流。正常业务触发命令或流程时，出现证据卡片，后续状态更新同一卡片。
6. 用完取消启用。关闭页面只断开订阅，不等于关闭服务端只读模式。

原始截图按实际画面保存在本地，可能包含昵称、聊天或画面中其他敏感文字；原图不做文字替换。结构化任务参数过滤口令、配置密钥和私密处理参数，被省略的参数有说明。

## HTTP 流程

沿用现有鉴权：X-Miliastra-Token 请求头或已经建立的访问 cookie。浏览器 SSE 使用 cookie，不将令牌放入 URL。

开启主动读取：

```json
{"enabled":true,"state":"active"}
```

POST /debug/read-only/mode，然后 POST /debug/read-only/read：

```json
{"label":"检查当前聊天","ocr":true,"rect":null}
```

rect=null 表示整张原图。指定区域时使用原始游戏图像坐标，如 {"x":410,"y":502,"width":378,"height":65}。这组坐标只适用于对应1920×1080截图中的区域；不要直接套用到其他分辨率或其他时刻。

| 接口 | 方法 | 用途 |
| --- | --- | --- |
| /debug/read-only/mode | GET / POST | 查询、切换启用状态与 active/push |
| /debug/read-only/read | POST | active 状态下采新帧并返回完整证据 |
| /debug/read-only/events | GET | 专用触发 SSE；支持 since 与 Last-Event-ID 续传 |
| /debug/read-only/entries?since=N | GET | 获取触发事件重放、游标和缺口提示 |
| /debug/read-only/evidence?id=ID | GET | 下载保留中的完整证据 |

推送状态用 {"enabled":true,"state":"push"} 开启，订阅 /debug/read-only/events。事件类型仅 readonly.triggered、readonly.task.updated。事件中的 evidenceUrl 指向完整 JSON；SSE 本身不携带巨大截图。附加 drain=true 可一次获取当前重放后结束请求。

## 用脚本保存可复现材料

脚本位于 [scripts/sandbox-read-only.ps1](../scripts/sandbox-read-only.ps1)。令牌通过本机环境变量 MILIASTRA_HTTP_TOKEN 提供，不写入脚本或报告。BaseUrl 填当前实际运行新版程序的地址。

```powershell
# 主动读取：保存请求、完整证据、原图及原图文件哈希
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State active

# 开启推送；获取当前已积累的触发记录，随后仍保持 push 状态
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State push -Since 0

# 按事件中的证据 ID 下载，避免切换状态
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State download -EvidenceId readonly-2

# 关闭只读模式
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State off
```

脚本的 push 请求只读取一次事件重放；持续订阅请使用工具页或 SSE 客户端。可用 OutputDirectory 指定保存目录，用 Rect 提供 JSON 区域，或用 NoOcr 只保存画面。重复抓取推送时用 Since 指定上次收到的序号。

## 证据与只读边界

- 沙盒只允许 Capture、Wait、EnsureWindow、EnsureForeground；后两项仅检查，不激活窗口。输入、点击、粘贴、恢复设备、发消息、点歌、播放控制、启动游戏和改队列均拒绝。
- 保存任务可以使用 mode=readOnly；任务在创建前和每步执行前检查白名单。只读运行不需要开启旧调试模式，且不会改变调试模式开关。
- 开启只读前等待 UI 无输入围栏排空旧沙盒操作；存在未完成 live 会话或围栏失败时不切换。
- 主动读取保存新帧。推送只以 OCR 原始捕获时间精确匹配原帧，不拿更新的画面冒充触发帧；缺图时 source=null，missingReason 说明原因。
- source.canvas=rawGame，尺寸与像素哈希属于原始图。命令的 messageY 等观察坐标可能属于程序规范画布，不能直接当原图坐标。
- capturedAtWallMillis 由单调捕获时间换算，是估算的墙钟时间；frameId 与原始捕获关联用于核对帧来源。
- 缓存最多8帧/64MiB，完整证据最多8包/32MiB，先达到任何上限就淘汰旧项；每包最多32次状态历史。事件重放最多256条。超过保留期明确返回 evidence_expired，重放缺口用 gap 标记。
- 监听模式切换、直接拉黑与后台投票结果保留原始命令来源、转换类型及实际 task ID，并按精确捕获时间关联原图。缓存已淘汰且无证据持有源帧时如实说明缺图；控制台等非视觉来源不伪造截图。公开游戏昵称和公开触发命令保留，私密上下文及私聊输入明确省略；对应私密源图也省略，不能据此完整重执行该业务。
- JSON/PNG 是复现和诊断材料；下载或读取不会重新派发原命令。已有模拟 replay 仍是独立沙盒功能。

新接口需要运行包含此功能的版本。旧服务返回404时需更新程序版本；仅刷新页面不能给旧可执行程序增加接口。
