# 文档导航

这组文档用于从代码角度理解项目。阅读顺序建议先看总览，再按你要排查或改造的链路跳转。

## 总览入口

| 文档 | 适合回答的问题 |
| --- | --- |
| [代码梳理](code-walkthrough.md) | 程序整体如何启动、扫描、入队、执行、播放、监控。 |
| [执行器流程](executor-flow.md) | 待执行任务队列如何串行执行游戏操作。 |
| [命令模型与屏幕锁](command-model-locks.md) | 命令如何去重、屏幕锁如何避免同屏重复执行。 |
| [支撑运行模块](supporting-runtime-modules.md) | 调试工具、热键、截图、OCR 底层、日志、TUI 和持久化分别在哪里。 |

## 聊天、OCR 与 UI

| 文档 | 适合回答的问题 |
| --- | --- |
| [聊天命令进入队列](chat-command-ingestion.md) | OCR 消息如何变成命令，哪些来源能触发业务。 |
| [成语接龙](idiom-chain.md) | 成语词库、合法性规则、命令和配置如何工作。 |
| [谁是卧底](undercover.md) | 报名、描述、投票、平票、胜负、词库和 Web 控制规则。 |
| [海龟汤](turtle-soup.md) | 题库、永久使用记录、AI 裁决、分段发送和 Web 控制如何工作。 |
| [DeepSeek 海龟汤 API 调研](research/deepseek-turtle-soup-api.md) | DeepSeek 官方 endpoint、模型、JSON Output、思考模式和采样参数如何配置。 |
| [DeepSeek 海龟汤稳定性评测](research/deepseek-turtle-soup-evaluation.md) | 553 次真实请求如何筛选提示词、思考模式和最大输出 Token。 |
| [二级聊天监听](secondary-chat-listener.md) | 二级界面如何监听大厅和好友私聊，红点、气泡、恢复与回退如何协作。 |
| [OCR 与 UI 检测](ocr-ui-detection-flow.md) | 主扫描循环、聊天切块、OCR、模板匹配和耗时日志如何工作。 |
| [UI 自动化与原子动作](ui-automation-atoms.md) | 点击、按键、粘贴、模板等待、OCR 点击和像素稳定如何组合。 |
| [自定义工作流、邀请与管理流程](custom-workflow-moderation-flow.md) | 自定义流程、好友发言、邀请确认、拉黑/屏蔽投票如何执行。 |

## 点歌、播放与审核

| 文档 | 适合回答的问题 |
| --- | --- |
| [点歌流程](song-request-flow.md) | 普通点歌、AI 点歌、搜索确认、入队和直接播放如何决策。 |
| [AI 点歌与候选歌曲审核](ai-song-review-flow.md) | 点歌 AI、候选选择、审核 Provider、审核强度和失败策略如何分工。 |
| [播放器控制器](player-controller-flow.md) | 播放器后端观测、确认播放状态、暂停原因和队列推进如何统一。 |
| [播放队列与自动出队](playback-queue-flow.md) | 音乐播放队列、当前歌曲保护、临近结束暂停和自动出队如何工作。 |

## 启动、Web 与观测

| 文档 | 适合回答的问题 |
| --- | --- |
| [启动游戏与进入千星](startup-wonderland-flow.md) | 启动游戏、开门、进入千星两个任务如何拆分与串联。 |
| [Web 监控与 HTTP API](web-monitor-api.md) | 远程面板如何读监控、提交控制台发言、点歌和启动任务。 |
| [配置迁移与观测](config-observability-flow.md) | 配置版本迁移、日志分流、Monitor、TUI 和状态文件如何组织。 |

## 领域词汇

项目根目录的 [CONTEXT.md](../CONTEXT.md) 是领域词汇表，只记录本项目特有概念的标准叫法，不记录实现细节。
