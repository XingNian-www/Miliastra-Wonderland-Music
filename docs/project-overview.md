# 项目导览与维护地图

> 本文整理当前工作树，不等同于已发布版本说明。核对基线：主包 5.8.3，HEAD 557c3d9。只整理文档，不迁移目录、不修改业务代码、不处理已有未提交改动。后续优先级见 [发展路线](roadmap.md)。

## 1. 定位与能力现状

Miliastra Wonderland Music 是面向 Windows 的游戏内点歌与聊天自动化应用。音乐播放是主线，聊天观察和 UI 自动化是底座，多人娱乐、AI 辅助、本地 Web 控制是扩展能力。当前更接近有明确模块边界的桌面单体，而不是简单脚本或多服务平台。

| 能力 | 当前实现及入口 | 维护关注点 |
| --- | --- | --- |
| 游戏聊天观察 | src/observation/、src/runtime/ocr/；一级/二级监听、OCR、历史消息基线 | 漏识别、重复命令、画面过期、分辨率变化 |
| 点歌与播放 | src/features/song_request/、src/features/playback/、miliastra-playback crate | 选曲、审核、队列、播放确认与失败恢复 |
| 音乐源与账号 | QQ 音乐、网易云、酷狗、B 站适配；独立登录 helper/protocol | 凭据失效、平台返回变化、登录状态一致性 |
| UI 自动化 | src/ui/、src/runtime/ui.rs、src/adapters/windows/ | 串行输入、任务互斥、执行后复核 |
| 大厅与娱乐 | 邀请、管理、成语、牌类、海龟汤、谁是卧底 | 身份/权限、会话超时、消息投递与播放争用 |
| 管理控制面 | src/interfaces/http/、TUI、快捷键 | Web 与聊天语义一致；权限和脱敏 |
| 配置和持久化 | 启动 YAML + SQLite 功能配置和播放状态 | 迁移、备份、回滚、热更新与重启边界 |
| 发布 | Windows 主包、可选 CUDA MNN / OpenVINO 运行时工作流 | 本地与 CI 一致性、依赖完整、干净环境验证 |

**工作中功能：** 本次整理开始时已有 13 个未提交修改文件，包含“点歌榜”的播放确认、SQLite 事件、HTTP 接口和面板接入。docs/web-tools.md 定义只统计成功播放的点歌。应作为待收口变更验证，不能据此声称已随 5.8.3 发布。

## 2. 运行链路

以下是职责示意，不是逐函数调用图：

1. main / watchdog → composition：加载启动配置、日志、数据库、完整配置，组装运行时。
2. Windows 截图 / UiRuntime → observation + OCR：识别画面、消息和状态。
3. 聊天解析/路由，以及 Web、TUI、快捷键 → application + runtime：受理、调度和协调。
4. features：点歌、播放、大厅、娱乐等规则 → playback runtime / UI routines / 状态持久化。
5. 结果输出为状态快照、日志、Web 展示及游戏内回复。

已有两阶段日志初始化和配置重载/看门狗交接机制，见 src/composition.rs、src/lib.rs。扩展功能要复用已有生命周期，避免增加隐式后台循环。

## 3. 目录职责

| 位置 | 职责 | 适合放入的改动 |
| --- | --- | --- |
| src/composition/ | 依赖装配、应用流程、端口实现 | 能力接入、生命周期和跨模块协调 |
| src/features/ | 点歌、播放、大厅、身份、娱乐等业务 | 业务规则及对应回归测试 |
| src/runtime/ | 调度、OCR、播放器/UI 运行时、截止时间 | 并发、取消、背压、资源所有权 |
| src/observation/ | 聊天流、画面观察和决策输入 | 识别、去重、场景切换基线 |
| src/ui/ | 原子动作、定位、模板、操作例程 | 操作步骤及前后置状态验证 |
| src/adapters/ | Windows、文件、日志、登录与播放器适配 | 外部副作用实现 |
| src/interfaces/ | HTTP、聊天、TUI、快捷键 | 请求转换与展示，不另造业务状态机 |
| src/config/ | 配置模型、schema、数据库和生效策略 | 校验、迁移与配置语义 |
| tests/ | UI 集成测试、配置和图像 fixture | 跨模块回归、可复现输入 |
| examples/ | 示例与辅助程序 | 按用途单独运行，不是默认启动路径 |
| scripts/、.github/workflows/ | 本地打包与 CI 发布 | 质量门禁、产物验证与依赖组装 |
| assets/、vendor/ | 应用资源、随仓库依赖 | 保留版本及第三方许可说明 |
| docs/ | 用户手册与维护文档 | 真实行为、开发约束和路线 |

### Workspace crate

- **miliastra-contracts**：原子文件写入与状态存储等最小端口契约；不是所有 HTTP DTO 的统一容器。
- **miliastra-kernel**：AI、时钟、身份和定时器等共享基础能力。
- **miliastra-playback**：音乐源适配、账号/缓存、歌词、媒体引擎和播放运行时；与游戏点歌规则分开。
- **miliastra-login-protocol**：主程序与登录辅助器之间的版本化消息及校验。
- **miliastra-login-helper**：独立平台登录辅助程序，包含 WebView2 实现。

### 本地资源与工具

.gitignore 已排除 target/、.build/、dist/、models/、logs/、diagnostics/ 等生成物或本地资源。这不代表都可安全删除：模型、发布包和日志可能仍有用途。此次不删除、不移动。

本地存在 tools/ai-automation/ Python 工具和测试；本次 git ls-files tools/ai-automation 未列出跟踪文件。不能把它当成新克隆必然具备的正式组件，纳入 CI 前应先明确是否纳管。它是 Web 工具 API 的原子操作客户端，不是完整流程编排器。

## 4. 修改从哪里开始

| 需求 | 建议阅读顺序 |
| --- | --- |
| 新增聊天命令 | src/features/command.rs → src/interfaces/chat/ → 对应 feature → application 接入 |
| 调整搜索/选曲 | src/features/song_request/ → crates/miliastra-playback/src/catalog/ |
| 调整队列/播放状态 | src/features/playback/ → src/runtime/business/playback.rs → 播放端口/运行时 |
| 调整 Web 功能 | src/interfaces/http/ports.rs → routes/、protocol.rs → page.html/tools.html → 协议测试 |
| 调整 OCR/画面识别 | src/observation/ → src/runtime/ocr/ → src/ui/ → fixture 回归 |
| 调整配置 | src/config/ → 配置中心接口 → 热更新/重载流程 → 迁移测试 |
| 新增娱乐玩法 | 现有 feature → 身份/计时/投递能力 → 聊天与 Web 入口 |

## 5. 已有工程基础与限制

已有端口、运行时、原子持久化、版本化登录协议、HTTP 协议测试、UI fake device 测试，以及散布在 Rust 模块内的单元测试。不能仅凭 tests/ 文件少判断缺测试，也不能把大文件中的全部测试算作生产复杂度。

主发布工作流使用 --locked、OCR 模型哈希和发布包文件/目录检查，但没有通用 fmt、clippy、workspace 回归测试步骤，也未配置 PR 触发。OpenVINO 依赖包工作流另有指定 OCR fixture 测试；这不等于主应用回归门禁。

本地 scripts/package-release.ps1 与主发布工作流存在打包差异：CI 会构建/执行 ensure-config-db 并恢复空令牌模板，本地脚本没有相同预置数据库步骤。应明确统一的预置策略，不能仅凭差异断言本地包不可用。

## 6. 维护约定（建议）

1. 保留模块化单体；新抽象对应真实重复或清晰资源边界。
2. 业务不变量先形成测试，再拆模块；行为调整和纯重构分开提交。
3. 聊天与 Web 共用应用层语义，不分别复制队列、播放和权限逻辑。
4. 游戏输入区分未发送、已发送但结果未知、已验证成功，恢复不能一律重试。
5. 真实令牌、Cookie、账号及本地数据库不进入开发文档、fixture 或发布包。
6. README 保持用户入口；维护者从本文和 [发展路线](roadmap.md) 进入。
