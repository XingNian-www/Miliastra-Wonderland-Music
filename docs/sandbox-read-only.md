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
4. 卡片提供“下载完整 JSON”和“下载完整 PNG”。完整 JSON 在点击下载后按四个分区读取，并按需把原始 PNG 放入兼容字段 imagePngBase64；原始 PNG 从独立二进制接口下载，不缩放、不重新截图。缺帧时仍可下载元数据、参数和历史，页面会说明缺图原因。
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
| /debug/read-only/read | POST | active 状态下采新帧；省略 response 保留完整旧响应，response="metadata" 返回轻量元数据 |
| /debug/read-only/summary | GET | instance、revision、当前 UI、队列和最近 5 个任务；不截图、不 OCR、不编码 PNG、不推送 |
| /debug/read-only/events?instance=I | GET | 专用触发 SSE；固定服务端身份，支持 since 与 Last-Event-ID 续传，缺口明确通知 |
| /debug/read-only/entries?since=N&limit=10&maxBytes=8192&taskId=ID&instance=I | GET | 分页事件 wrapper、生产序号游标和 gap/reset/loss |
| /debug/read-only/evidence?id=ID | GET | 旧 GET 兼容：不带 section 时返回与旧 POST 完整响应同形的完整证据，含 source.imagePngBase64；不受分区分页预算约束，供旧客户端按需使用 |
| /debug/read-only/evidence?id=ID&section=metadata&cursor=0&limit=10&maxBytes=8192 | GET | 显式分区的 UTF-8 JSON 文本分页，受 maxBytes 预算约束，不内联 PNG |
| /debug/read-only/image?id=ID&instance=I&revision=R | GET | 保留的精确原始帧 PNG 二进制；可固定身份与版本，返回像素及文件哈希响应头 |

推送状态用 {"enabled":true,"state":"push"} 开启，订阅 /debug/read-only/events。业务事件类型为 readonly.triggered、readonly.task.updated；data 是带 instance 的证据摘要，status 保持嵌套结构，生产序号在 SSE 的 id 行。新增 readonly.gap 明确报告重放或订阅缺口。注释 heartbeat 只用于连接保活，不算业务事件，也没有空闲周期 producer。事件中的 evidenceUrl 不带 section，指向旧兼容的**完整证据包**（含内联 PNG）；需要按预算分页时应显式加 section，SSE 本身不带 PNG。工具页在连接 URL 中携带 mode/summary 返回的 instance，静默重启也通过初始 readonly.gap reset 清除旧卡片并从新实例的序号 0 重新连接；注释 heartbeat 不触发取证。附加 drain=true 可一次获取当前重放后结束请求。

旧工具页的主动读取仍发不带 response 的 POST，因此继续得到包含 source.imagePngBase64 的完整证据。GET /debug/read-only/evidence 同样兼容旧客户端：不带 section 时是与旧 POST 完整响应同形的完整证据（含内联 PNG），分页预算不覆盖它；只有显式带 section 的请求才分页并省略内联 PNG。新客户端可显式发送 {"label":"检查当前聊天","ocr":true,"rect":null,"response":"metadata"}，再按需读取分区和图像。未知 DTO 字段会拒绝，请使用上述字段。

### 分区与版本一致性

section 支持 metadata、ocr、parameters、history。metadata 是证据身份、状态和原帧来源；parameters 是 {"task":...}，其中不包含根级 task.ocr；ocr 是原主动读取 OCR 对象或 null；history 是 {"status":...,"history":[...],"historyDropped":N}。组装兼容包时保留 metadata，将 parameters.task 作为 task，将非 null 的 ocr 放回 task.ocr，并加入 history 的字段。section 显式出现但为空或不是上述名字时返回 400 invalid_section，不会回退成完整包。

显式 section 的成功响应是完整信封（不带 section 的旧完整包是另一种形状），结构如下（text 的实际内容是所选分区 JSON 文本的一段）：

```json
{"ok":true,"evidence":{"id":"readonly-2","instance":"server-instance","section":"metadata","revision":7,"page":{"encoding":"json-utf8","items":[{"section":"metadata","text":"{}","offset":0}],"cursor":0,"nextCursor":2,"done":true,"revision":7}}}
```

首次请求 cursor=0 且不带 revision；需要固定身份时可以在同一请求带上 instance=I，并保存响应中的 instance/revision，后续页和同一次下载的其他分区继续携带该 instance 与 revision，并验证所有响应的 instance 一致。cursor、nextCursor 和片段 offset 都是 **UTF-8 字节偏移**，不是条目下标或 JavaScript 字符数。按顺序拼接 items[].text，等 done=true 后 JSON.parse/ConvertFrom-Json 才得到完整分区。年龄变化不驱动 revision。收到 HTTP 409 revision_conflict 或 instance 改变时，丢弃本次未完成组装，从第一分区重新读取，不能混页。instance 先于分区和版本解析；不带 section 的完整包在携带 revision 时同样核对（冲突仍为 409），证据不存在或已过期仍返回 404/410，不改抓新帧充当被请求的旧证据。

limit 默认 10、上限 100；maxBytes 默认 8192、上限 65536，预算覆盖 UTF-8 编码后的整个响应信封，**只约束显式带 section 的分页包**，不带 section 的旧完整包不受该预算限制。首片段装不下时返回 {"ok":false,"error":"page_budget_too_small","cursor":N,"nextCursor":N}；保持原游标，增大 maxBytes 重试。小错误信封允许超过预算以便说明错误。工具页和脚本会把预算逐步增大到上限，仍不足则明确报错。

原图 GET 返回 Content-Type: image/png，以及 X-Pixel-Sha256（原始像素 SHA-256）、X-File-Sha256（下载 PNG 文件字节 SHA-256）、X-ReadOnly-Instance 和 X-ReadOnly-Revision。下载客户端携带四区固定的 instance/revision，校验返回身份和版本；文件 hash 与像素 hash 不可互换。原帧不在时返回 image_unavailable，不取晚帧替代，也不调用截图接口。

### 事件分页

entries 响应为 {"ok":true,"entries":{"events":[...],"nextCursor":N,"hasMore":false,"instance":"...","revision":R,"gap":false,"reset":false,"latestSequence":N,"loss":{...}}}。events 每项是 {"sequence":N,"kind":"...","atMs":T,"payload":{...}} wrapper，与 SSE data 摘要不同；since/nextCursor 是生产事件序号。hasMore=true 时从返回的 nextCursor 继续，携带同一 instance；taskId 可过滤。服务器交付匹配项的前缀，不越过尚未交付的匹配项。预算不足时同样保持游标并增大预算。gap 表示有界重放历史已缺失，reset 表示实例已变化；客户端必须显式处理，不能把重启后的同名 readonly-ID 当作旧证据。loss 报告淘汰与历史截断，不能当成完整无限日志。

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

脚本需要 PowerShell 7（pwsh）。active 使用 response="metadata" 采一次，再下载四区；download 仅下载保留证据；push 分页读取一次当前事件重放，将 wrapper 和每页 gap/loss 写入 entries.json，再按证据 ID 下载，持续订阅请使用工具页或 SSE 客户端。它验证 UTF-8 字节游标、固定 instance/revision，版本冲突会失败而不组装混合 JSON。

OutputDirectory 指定保存目录，材料按返回的服务端 instance/证据 ID 分目录，避免进程重启后的同名 ID 混用。每份材料保存 evidence.json；有精确保留图像时另存 original.png、file-sha256.json（文件哈希、像素哈希、instance、revision），并在兼容 JSON 中放入相同 PNG 的 base64。非视觉来源或原帧已淘汰时只存 JSON，输出 sourceMissing=true 和 missingReason，不留下旧 PNG。Rect 提供 JSON 区域，NoOcr 可跳过 OCR。重复抓取推送时用 Since 指定上次 entries.nextCursor，并用 Instance 指定上次 entries.instance；相同 OutputDirectory 内可自动读取已保存 entries.json 的 instance。首次没有旧身份时只能从 Since 0 开始，避免把旧游标套到重启后的新实例。遇到实例 reset 会停止而保留旧材料，应从 Since 0 重新开始。download 也可带 Instance 核对证据所属实例。

## 证据与只读边界

- 沙盒只允许 Capture、Wait、EnsureWindow、EnsureForeground；后两项仅检查，不激活窗口。输入、点击、粘贴、恢复设备、发消息、点歌、播放控制、启动游戏和改队列均拒绝。
- 保存任务可以使用 mode=readOnly；任务在创建前和每步执行前检查白名单。只读运行不需要开启旧调试模式，且不会改变调试模式开关。
- 开启只读前等待 UI 无输入围栏排空旧沙盒操作；存在未完成 live 会话或围栏失败时不切换。
- 主动读取保存新帧。推送只以 OCR 原始捕获时间精确匹配原帧，不拿更新的画面冒充触发帧；非视觉来源可为 source=null，图像淘汰后 source 可保留来源元数据并标记 imageRetained=false，missingReason 说明原因。
- source.canvas=rawGame，尺寸与像素哈希属于原始图。命令的 messageY 等观察坐标可能属于程序规范画布，不能直接当原图坐标。
- capturedAtWallMillis 由单调捕获时间换算，是估算的墙钟时间；frameId 与原始捕获关联用于核对帧来源。
- 图像与任务元数据寿命分离：证据原图最多 8 张 / 32MiB；用于精确来源匹配的 rolling frame cache 最多 8 帧 / 64MiB；metadata/task store 最多 256 条 / 16MiB；各自先达到数量或字节上限就淘汰旧项。图像淘汰不删除任务记录，保留的未完成任务继续接收状态和终态；source 仍可保留来源信息，但 imageRetained=false 明确说明不能下载。
- 每条证据最多 32 次状态历史，historyDropped 明确记录截断；事件重放最多 256 条。metadata 最终也有界，不保证永久保留；metadata 淘汰、未完成元数据丢失、图像淘汰与历史截断通过 loss 字段报告。元数据过期明确返回 evidence_expired，图像过期返回 image_unavailable，事件重放缺口用 gap 标记；summary 仅展示最近 5 项，不能把它当作完整任务状态库。
- 监听模式切换、直接拉黑与后台投票结果保留原始命令来源、转换类型及实际 task ID，并按精确捕获时间关联原图。缓存已淘汰且无证据持有源帧时如实说明缺图；控制台等非视觉来源不伪造截图。公开游戏昵称和公开触发命令保留，私密上下文及私聊输入明确省略；对应私密源图也省略，不能据此完整重执行该业务。
- JSON/PNG 是复现和诊断材料；下载或读取不会重新派发原命令。已有模拟 replay 仍是独立沙盒功能。

新接口需要运行包含此功能的版本。旧服务返回404时需更新程序版本；仅刷新页面不能给旧可执行程序增加接口。
