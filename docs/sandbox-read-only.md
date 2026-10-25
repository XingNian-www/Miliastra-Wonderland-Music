# 沙盒只读观测

用于查看游戏状态、保存当前画面和接收实际触发的任务证据。只读观测不会暂停正常业务。

## 页面使用

1. 打开高级控制页：/tools。
2. 填写访问令牌并启用“用户沙盒 · 游戏状态观测”。
3. 选择：
   - **主动读取**：点击“读取一次”查看当前画面和 OCR。
   - **推送**：接收实际触发的命令或流程及后续状态。
4. 主动读取完成后，可下载 JSON 或 PNG。
5. 使用完关闭只读开关。

推送不会报告普通聊天、每帧 OCR 或播放进度。原图可能包含聊天和昵称，请按敏感材料处理。

## PowerShell

脚本：scripts/sandbox-read-only.ps1

需要 PowerShell 7，并通过环境变量提供令牌：

~~~powershell
$env:MILIASTRA_HTTP_TOKEN = 'your-token'

# 主动读取
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State active

# 推送，从头读取事件
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State push -Since 0

# 下载已有证据
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State download -EvidenceId readonly-2

# 关闭
pwsh -File scripts/sandbox-read-only.ps1 -BaseUrl http://127.0.0.1:18888 -State off
~~~

常用参数：

- BaseUrl：运行新版程序的地址，必填。
- State：active、push、download 或 off。
- OutputDirectory：保存目录，默认 technical-docs/sandbox-read-only/evidence。
- EvidenceId：download 模式的证据 ID。
- Label：主动读取的标签。
- Rect：OCR 区域，例如 {"x":410,"y":502,"width":378,"height":65}。
- NoOcr：主动读取时跳过 OCR。
- Since：推送事件起始游标。
- Instance：固定推送续传或下载使用的服务端实例。

Since 大于 0 时必须使用对应的 Instance。实例变化后，脚本会保留旧材料并停止；确认新实例后从 Since 0 重新开始。

材料按“实例/证据 ID/”保存。原图可用时保存 original.png；原图已淘汰或没有视觉来源时，只保存 JSON 并记录原因。

## HTTP 调用

请求使用 `X-Miliastra-Token` 请求头；订阅事件流（SSE）时改用查询参数 `?token=`——浏览器不允许 EventSource 设置自定义请求头。

> 服务端没有 Cookie 鉴权。面板把令牌存进 Cookie 只是为了本地保存，它从不参与鉴权。

- POST /debug/read-only/mode：启用或关闭 active、push。
- POST /debug/read-only/read：主动读取；新客户端可使用 response=metadata。
- GET /debug/read-only/summary：查询当前摘要。
- GET /debug/read-only/events：订阅推送事件。
- GET /debug/read-only/evidence：读取证据。
- GET /debug/read-only/image：下载保留的原始 PNG。

证据下载过程中遇到实例或版本冲突时，从头重新下载。原图已淘汰时保留 JSON 即可；收到 404 时请确认运行的是包含只读功能的新版本。
