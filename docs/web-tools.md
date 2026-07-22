# Web 高级控制

主监控页为 `/`，高级控制页为 `/tools`。高级页替代了原有的命令行诊断与手动输入入口。

## 执行规则

1. HTTP 请求只创建工具任务并立即返回任务编号。
2. 工具任务不进入正式待处理任务队列。
3. 独立工具线程只会在正式待处理队列为空且未执行正式命令时取出一个工具任务。
4. 正式聊天命令、远程点歌、播放控制、游戏启动和控制台发言始终优先于工具任务。
5. 工具执行失败只记录为 `failed` 结果；不会退出主程序，也不会触发返回一级界面等业务恢复动作。
6. 会产生游戏输入的坐标点击、按键和模板点击会短暂声明屏幕独占；正式命令在这段时间内不会开始，输入结束后立即恢复。面板响应基准每次输入前都会再次检查正式队列，一旦有业务任务到来就取消剩余测试并立即让出。
7. OCR、模板、UI 和大厅工具，以及截图，都读取主扫描最近一次成功捕获且仍有效的画面；不会为了 Web 请求额外抓取游戏窗口。主扫描尚未产生画面或目标窗口已关闭时，请稍后重试。

## 路由

所有 `/tools/*` 提交接口均要求 `POST`，返回 JSON 任务编号。使用 `GET /tools/task?id=<任务编号>` 读取状态和结果。

| 接口 | 功能 |
| --- | --- |
| `/tools/ocr` | 全屏或指定区域 OCR，`rect=x,y,width,height` 可选 |
| `/tools/scan-chat` | 扫描当前游戏聊天区域 |
| `/tools/ui-state` | 检测当前 UI 状态 |
| `/tools/hall-name` | 识别大厅名称区域 |
| `/tools/template` | 使用配置内命名模板匹配或点击，`rect=x,y,width,height` 可选 |
| `/tools/click` | 点击画布内坐标 |
| `/tools/key` | 发送一个按键 |
| `/tools/chat-change-samples` | 有上限地采样聊天区域变化，最多 30 次 |
| `/tools/panel-benchmark` | 有上限地测试 Enter 打开、Esc 关闭聊天面板，最多 10 轮 |
| `/tools/ocr-backends` | 检测 OCR 后端可用性 |
| `/tools/ai-preview` | 仅返回 AI 点歌候选与选择结果，不播放 |

模板接口允许以下命名模板，不能由 Web 请求传入任意本地路径：

- 聊天与界面：`blue-marker`、`yellow-marker`、`pink-marker`、`friend`、`secondary-back`、`secondary-hall`。
- 邀请：`invite-view-star`、`invite-goto-hall`、`invite-enter-hall`。
- 好友管理：`friend-panel`、`friend-search-panel`、`friend-more-settings`、`friend-block-chat`、`friend-blacklist`、`friend-confirm`。
- 启动与千星：`wonderland-enter-button`、`paimon-menu`、`wonderland-close`。
- `custom_workflows.templates` 中已配置的自定义模板名称。

`GET /tools/templates` 返回每个模板的显示名称、默认匹配区域和默认阈值。高级页的“读取配置区域”按钮会把这些值回填到表单；自定义模板只有文件路径映射，没有固定区域时需要手动填写。

模板名称必须与代码中的白名单完全一致：一级大厅锚点是 `friend`，二级返回和当前大厅锚点分别是 `secondary-back`、`secondary-hall`；不存在通用的 `enter` 模板。`friend` 只用于一级聊天驻留判断，不代表启动游戏或进入千星成功。

面板响应基准覆盖聊天区与聊天输入区附近的联合检测区域，完成或失败时会额外发送一次 `Esc` 尝试收起聊天面板。

## 远程访问

默认 `http.host` 为 `127.0.0.1`。配置为非本机监听地址时，必须设置非空的 `http.access_token`，否则 HTTP 服务拒绝启动。

Web 页面的“访问令牌”输入框只保存到当前浏览器会话，并通过 `X-Miliastra-Token` 请求头发送。令牌不会写入接口历史或 URL 查询参数。

工具结果最大保留 48 KiB 文本；超过时会在任务结果中标注截断。
