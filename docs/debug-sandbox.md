# 调试模式与 AI 沙盒协议

> 各原子命令的**实测效果、实测延迟、失败模式与优化建议**见 [debug-sandbox-live-commands.md](debug-sandbox-live-commands.md)。

调试模式打开后，程序内置的聊天监听、热键和自动化流程不能向游戏窗口发送输入；AI 只能通过外部调试协议逐个提交原子命令。截图必须走 UI runtime 的同步捕获，而不能默认使用可能过期的缓存帧。

## 建议的外部协议

- GET /debug/state：返回调试模式、输入闸门、当前任务/运行状态。
- POST /debug/mode?enabled=1|0：切换调试模式；启用后内部输入受闸门阻止。运行中设备调用不保证立即停止。
- POST /debug/command：一次执行一个 live 原子命令，自动建立 version=1 的临时任务快照和可查询、可归档的运行。**直接命令与任务模型共用同一个 `AtomicCommand` 词表**：请求体就是任务 `commands` 里的那一项，单元变体是裸字符串（`{"command":"playback_pause"}`），结构变体是单键对象（`{"command":{"playback_set_volume":{"volume":30}}}`）。历史写法（把命令字段平铺，如 `{"command":"click","x":3,"y":4}`）仍然接受，但它只是**兼容垫片**，不是第二份能力定义——此前两套词表曾导致"任务模型能跑、/debug/command 报 unknown variant"的真实分裂。边界校验也统一由任务模型负责，HTTP 层不再重复一份坐标/长度/时间限制；响应含 `runId`、`taskVersion`、`result`、`captured`、`width` 和 `height`，可按 runId 查询同步 PNG 证据。**该路径默认不截图**：只有 `{"command":"capture"}` 会产生画面，其他命令返回 `captured:false`、`width:0`、`height:0`、`imagePngBase64:null`（此前每条命令都强制截图，单次要付约 1.6s）；需要画面请显式再发一次 `capture`，或改用 `GET /debug/screenshot`。单次 UI 操作等待上限 30 秒，命令执行和后续截图分别等待，完整请求可能超过 30 秒；等待超时会标记运行取消并跳过后续截图及步骤。排队命令会取消，运行中的 Wait 会协作取消；设备调用若阻塞，超时不保证它已经停止。同步截图入口单独保留。
- POST /debug/tasks：保存带版本号的 SandboxTask，并执行 schema/边界校验。
- POST /debug/run：创建运行快照；`mode=simulated` 需要受限 fixture，`mode=live` 需要已启动的 UI runtime 和调试模式；启用逐命令截图或图片断言时，同一 UI routine 执行命令并同步采集该步截图。未要求截图的命令不会隐式采集；失败命令仍尽力单独采集错误证据。live 不接受 fixture。
- GET /debug/tasks、GET /debug/runs：查询可编辑任务和运行历史；GET /debug/run/status?runId= 返回运行状态及启动时的不可变任务快照（即使任务之后升级版本）；已完成运行的快照可从 SQLite 归档恢复；POST /debug/run/cancel?runId= 请求取消运行。
- GET /debug/events：**SSE 事件流**，用来替代轮询。需要与其它调试接口相同的令牌。连上后先补发缓存中的事件（最多 256 条），再转入实时推送；两者共用同一个 sequence 空间，客户端按 `id`（即 sequence）去重即可。断线重连时带上 `Last-Event-ID` 或 `?since=N`，服务端只补发**严格更新**的部分，既不重复也不遗漏边界事件。`?drain=true` 只回放积压事件后关闭连接（用于"只补课、不订阅"，也让无限的流可以被自动化测试断言）。响应头固定带 `Cache-Control: no-store` 与 `X-Accel-Buffering: no`，避免被中间层攒成"批量轮询"；每 15 秒发一次 keep-alive 注释。事件类型放在 data 的 `kind` 字段里，而不是 SSE 的 `event:` 字段，这样任何客户端都能从默认监听里拿到，不必事先知道事件名。
  - 当前发布的事件：`debug.mode`（调试模式切换）、`run.started` / `run.finished`（沙盒运行生命周期，含 runId、taskId、taskVersion、终态 status）、`decision.submitted`（提交决策）。
  - 调试模式下的**游戏内观测也会被推送**（`game.chat` / `game.observation_gap`）：调试模式原本会丢弃游戏内聊天命令与问题、不执行自动化（`listener.rs` 的 `dispatch_chat_observations`），所以那个模式下应用**自己不会产生**任何状态变化事件。现在改为把原本要丢弃的观测原样推出来——**本进程当观察者、AI 当执行者**，安全边界（不执行自动化）完全不变。主聊天窗口按**内容**去重，只有可见状态真的变了才推（曾按 `is_new` 去重，实测 44 秒推出 13 条几乎全同的事件，等于把推送退化成 2 秒轮询，已修正）。
  - 观测缺口（`observation_gap`）也会推送：否则 AI 会把不完整的状态当完整状态用。
  - **尚未推送**：应用内部的状态变化（大厅计时、队列变化、播放控制器）以及"出现了待裁决项"这一通知。后者已挂钩（`MonitorShared::publish_decision` → `decision.pending` / `decision.cleared`，带 `deadlineAtMs`），但在调试模式下应用不跑自动化，因此该事件**在 AI 实际工作的模式下不会自然产生**；它只在非调试模式（正常运行时）才有意义。所以"外部 AI 无需轮询即可闭环"目前**只对游戏内观测成立**，应用内部状态仍需轮询查询接口。
  - 真机实测（curl）：连接前最新序号为 3，连接保持打开，随后产生的 `run.started`/`run.finished` 以 id 4/5/6/7 被主动推送，且**没有重放** 1-3。
- GET /debug/run/evidence?runId=：返回每步 `command`、`assertions`、结果及 `imagePngBase64`（如该步启用截图）；运行中的 live `Wait` 每 25 毫秒检查取消，其余设备调用在原子命令边界检查；取消或失败时会尝试恢复设备并释放由本设备记录的 KeyDown/MouseDown 输入。只有游戏窗口仍处前台、设备调用已返回且系统接受释放事件时才可能成功；无法强制中断阻塞的操作系统调用；步骤间取消时恢复失败会报告错误、保持待取消运行，可再次调用 step/all 重试清理；已写入失败结果的运行则可能已终止，不能保证后续重试。已结束 session 会被回收。
- POST /debug/run/replay?runId=：仅对模拟运行使用原任务版本、fixture 和断言创建新运行，返回新 runId；随后调用 step/all 重新执行并生成新证据。内存中保留最近 32 个配方；已完成并成功归档的模拟运行可在重启后从最近 64 条归档恢复配方。未完成或归档已淘汰的运行不可在重启后回放；live 不可回放。
- 插件运行必须同时提供 `pluginId` 与 manifest；宿主校验 API 版本、插件 ID 和 capability。live 插件还必须声明 `task` 与 `input`，且调试模式必须开启。
- GET /debug/screenshot：同步捕获 JPEG；当前调试截图路由始终请求新捕获，不提供 fresh 缓存切换。

## 沙盒安全边界

1. 任务必须有 id 与递增 version；未知字段拒绝。运行创建时固定已校验版本；创建过程中任务若升级，旧版本请求会被拒绝，不会按另一版本执行。
2. 限制命令数量、坐标、文本长度、等待时长；禁止任意脚本、任意进程启动和任意文件读写。
3. 输入执行必须经过 UI runtime 单线程队列；调试 API 取得外部输入许可后才能绕过输入闸门。
4. 任务执行默认串行，命令结果不可被后续命令隐式吞掉；每一步包含 ok/error/frameSequence；服务用全局 runners 锁串行化所有 run 的推进与证据读取，长命令期间其他 run 的推进/证据读取可能等待；取消请求直接写 store，不必等待该锁。第一步前取消在推进时直接形成无步骤结果的终态，无需执行目标命令或截图；`CloseWindow` 在投递 WM_CLOSE 后最多等待 3 秒确认原窗口句柄消失（不验证进程退出），超时视为失败；成功后不再尝试从已关闭窗口截图，该步无图片证据，不能绑定图片断言。
5. 每次编辑产生新版本；正在运行的版本不可原地覆盖，采用 copy-on-write 快照。任务保存时将 `InputText` 与 `Paste` 文本限制在 16 KiB，将 `InputText.settle_ms`、`EnsureReady`/`Activate`/`Focus.after_activate_ms` 和 `Wait.milliseconds` 限制在 30 秒以内。live UI 操作通常在 30 秒后超时；显式等待可达到 30 秒的命令额外保留 5 秒排队与截图余量，最长 35 秒。超时仅发出协作取消信号，不能强制中断已阻塞的 OS 调用。
6. 当前验证器支持尺寸、像素、前后帧差异、内联灰度模板断言和只读剪贴板包含断言；帧差异支持 `min_changed_pixels` 与 `max_changed_pixels`（旧请求省略下限时默认 0），可验证截图确实发生变化；模板搜索区域最大 128x128、模板最大 32x32、每运行最多 4 个模板断言，不允许指定文件路径。断言失败会在结果终止前尝试设备恢复，立即停止后续步骤并保留当前运行内的证据截图；恢复失败会追加在步骤错误中，不保证设备已恢复。OCR `ocr_contains` 断言对本步同步截图按 x/y/width/height 裁剪识别并精确大小写匹配子串；裁剪不超过 1048576 像素、目标文本不超过 128 字节。生产 HTTP 调试服务接入现有 OCR runtime；无 OCR runtime 时启动前拒绝断言，OCR 后端失败或超时使本步失败并保留截图。`window_foreground: {expected: true|false}` 可由 live UI worker 只读检查现存目标窗口是否处于前台；窗口查找失败会使步骤失败，模拟模式启动前拒绝该断言。`window_available: {expected: true|false}` 只读检查按当前窗口查找规则可用的可见、非最小化目标窗口；未找到可用窗口返回 false，枚举或配置错误使步骤失败。进程名读取失败可能被窗口枚举跳过，因此 false 不证明目标进程或窗口绝对不存在。该断言可绑定 `CloseWindow`；模拟模式启动前拒绝。仅有窗口状态断言且未启用逐步截图时不会采集图片。`ui_state: {expected, stable?}` 是已接入的只读 UI 状态断言：live 执行器在 UI worker 内读取最新 UI 分类结果，与 `expected`（primary/secondary/game_gate/overworld/unknown）比较；`stable: true` 还要求该状态在稳定性窗口上已经稳定。它不截图、不激活窗口。分类失败使步骤失败；尚无分类观测时如实返回"尚无 UI 状态观测"，不会伪造通过。运行器只依赖一个窄 DTO（状态判别值、稳定性、frame id），不依赖 UI 分类内部结构，UI 侧实现可独立替换；模拟 executor 支持可控读数，未配置该 adapter 的执行器在创建 run 前拒绝断言。该断言也可绑定 `CloseWindow`。**尚未实测**：真实环境下的端到端验证未进行（游戏未运行），`stable` 在分类抖动边界上的行为也没有实测。这不等于其他 UI/业务状态断言已实现。`playback_queue_length: {expected}` 是 `SandboxAssertion` 中通过 `SandboxExecutor` 可替换状态查询端口执行的只读业务断言；模拟 executor 支持可控队列长度；生产 live executor 通过 `PlaybackQueueLengthReader` 窄 trait 读取 business runtime 的只读队列快照并返回长度。未配置该 adapter 的 live/plugin/test 实例仍会在创建 run 前拒绝断言；因此该断言不通过截图推断状态，也不代表点歌、播放或队列修改操作已接入。HTTP loopback 测试覆盖模拟模式下该 JSON 断言的鉴权、创建与执行；`GET /debug/window` 是 authenticated 的只读目标窗口可用性检查，复用 live UI runtime 的窗口枚举，不激活、不截图、不输入；测试设备未接线时明确返回错误而不伪造 available。Windows opt-in 测试覆盖 adapter 缺失时提前拒绝。`playback_queue_clear` 与 `playback_queue_remove_front` 是已接入的业务 mutation 原子命令：生产 live executor 分别通过 `PlaybackQueueClearer` 调用 business runtime 的 `clear_playback_queue()` 与 `remove_playback_queue(QueueRemoval::Front)`，模拟 executor 支持清空及移除首项；命令成功/失败、步骤序号和后续 `playback_queue_length` 断言均归档到同一 SandboxRun evidence。未配置对应 adapter 的 live/plugin/test 实例会在创建 run 前拒绝命令；空队列移除会作为失败步骤记录。这不等于点歌、播放或其他队列修改操作已接入。`task_queue_cancel { task_id }` 是已接入的 formal task 队列取消业务命令：production live executor 通过 `TaskQueueCanceller` 调用 `TaskEngineHandle::cancel_formal`，模拟 executor 对正数 task id 记录成功、对 0 在 run 创建前拒绝；取消结果、task id 和步骤错误进入同一 SandboxRun evidence。它不等于任意后台任务、插件请求或 UI 操作都可被强制中断；其他 UI/业务状态断言仍未实现。
7. 插件使用 manifest 声明能力 observe、input、task、assert，默认只读；worker `task.start` 与携带 pluginId/manifest 的 HTTP `/debug/run` 在提交非空断言时同时需要 task 与 assert 授权，空断言不需要 assert。HTTP 仍依赖现有服务鉴权；请求附带的 manifest 本身不是插件身份凭据，尚未实现独立插件 API token。

## 模块化边界

实现按以下边界组织，模块之间只依赖稳定 trait/DTO，不直接访问彼此的内部实现：

1. `protocol`：版本化原子命令、任务、运行记录、插件 manifest 和错误模型。`AtomicCommand` 是**唯一**的原子命令词表，直接调试命令与任务模型都解析它，新增命令不需要改两处。
2. `device`：`UiDevice` 及 Windows、模拟设备适配；运行器不直接依赖具体设备。
3. `runner`：逐步调度、取消边界、超时和命令结果；通过 `SandboxExecutor` 注入设备执行。
4. `assertions`：当前实现尺寸、像素、帧差异、受限内联灰度模板、截图 OCR 包含文本、只读窗口前台/可用性、只读 UI 状态断言，以及只读剪贴板包含断言（`clipboard_contains`）；其他 UI 与业务状态断言尚未实现。
   - `clipboard_contains`（`{"text":"..."}`）直接读取剪贴板文本并判断是否**包含**目标串，不截图、不做输入授权检查。它是"复制"类操作唯一可验证的方式——`paste` 命令只写不读。空串或超过 256 字符的目标会被拒绝；运行前若适配器不支持则在创建运行之前报 `当前执行器不支持剪贴板断言`；失败时错误信息会给出**实际内容（超 64 字符截断）**，而不是只报失败。
5. `evidence`：当前负责截图编码、运行内证据和容量回收；终态成功归档后立即释放会话；归档单条记录上限 12 MiB，**超限时先缩小截图、再考虑丢帧**：缩小保留全部步骤的画面（只是分辨率下降），只有缩到 0.125 倍仍放不下才回退到按最新优先丢帧。缩小过的证据条目会带 `imageDownscaled`、`imageScale`、`imageOriginalWidth`/`imageOriginalHeight`，且 `width`/`height` 始终描述归档后**实际返回的那张图**，不会出现元数据与内容不一致；走丢帧路径时则标记 `evidenceTruncated` 与 `droppedImageFrames`。无 SQLite 时保留最多 8 条进程内 PNG 证据，历史状态仍可从运行存储查询。内存归档满后旧截图会过期；配置 SQLite 时终态运行和证据持久化到数据库，成功写入后释放会话及内存运行快照，可跨进程读取最近 64 条；归档失败时保留终态会话供重试。
6. `plugin`：manifest capability 校验与内部受限 stdio worker 处理器已实现；进程监管及认证边界见下文，非可信插件隔离、及时取消和全面崩溃恢复尚未实现。
7. `business_commands`：业务原子命令适配。已接入**播放控制**（`playback_play` / `playback_pause` / `playback_skip_next` / `playback_skip_previous` / `playback_set_volume`）与**队列操作**（`playback_queue_clear` / `playback_queue_remove_front` / `task_queue_cancel`）。播放控制只通过一个窄边界 `PlaybackAction` 暴露，沙盒**不认识应用内部的 `PlaybackCommand`**；适配层复用与公开 HTTP **完全相同**的入队路径，不另造旁路，因此"沙盒能做的"与"API 能做的"始终一致。语义上"入队成功"即该原子步骤成功，**不把"已下发"当成"已生效"**——真实效果由断言层验证。**点歌入队**（`request_song`：`keyword` / `source` / `prefer_accompaniment`）已按同一边界接入，走 `HttpCommandPort::remote_song`。**聊天发送**（`chat_send`：`text` / `prefix`，`prefix` 省略时默认沿用应用控制台前缀 `[控制台]: `，显式传空串即"不带前缀"）已按同一边界接入，走 `HttpCommandPort::chat_send` → `enqueue_console_chat`，与 `/chat/send` **完全相同**的入队路径。正文为空或超过 `max_text_bytes` 时在**任务校验阶段**被显式拒绝（"聊天内容不能为空" / "聊天内容超过上限" / "聊天前缀超过上限"），不会静默失败。

**游戏启动**、**队列按索引删除**等其余业务操作**尚未接入**，调用会得到明确的"当前执行器不支持…"拒绝，而不是静默失败。

**必须知道的语义边界（真机实测）**：业务原子命令的成功只代表**请求已成功下发**，不代表业务已生效。真机上提交一次 `request_song` 后，沙盒步骤报 `passed`、证据齐全，而应用日志显示下游真实执行：

```
[INFO ] 待处理任务开始: 控制台命令: 酷狗点歌 ZZZ-NOT-A-REAL-SONG-9F3K2
[ERROR] 命令执行失败 酷狗点歌 ...: 建立一级聊天确认基线失败：
        UI routine failed at prepare_residency_observation (BeforeInput):
        调试模式已禁用内部游戏输入，请通过明确授权的外部 API 操作
```

即：**调试模式为了把输入权交给外部 AI，禁用了程序内部的游戏输入；而点歌这类业务操作自身需要在聊天框里发送确认**。因此在调试模式下，凡是要走内部游戏输入才能完成的业务动作都会在下游失败——沙盒如实记录"已下发"，但业务没有生效。自动化要么由沙盒显式编排等价步骤（打开聊天、输入、发送），要么在明确授权下对特定业务任务放开内部输入。**不要因为沙盒步骤 `passed` 就宣称该业务动作已能自动完成。**
8. `http`：请求解析、鉴权、路由和响应映射；不实现设备操作或业务逻辑。

每个模块都应能够使用 fake/mock adapter 独立测试；禁止 HTTP 层直接创建 Windows 设备、插件 worker 或持有跨模块可变状态。

## 当前实现边界

当前可用能力包括版本化任务 DTO、窗口/键盘/鼠标原子命令与剪贴板写入（paste）、播放控制业务原子命令（播放/暂停/切歌/音量，入队语义）、只读剪贴板断言、模拟与 live runner、同步截图、运行内证据、取消边界、live 等待超时、设备恢复入口、manifest capability 校验，以及尺寸/像素/前后帧差异/内联灰度模板断言。

已有带 token 的真实 loopback HTTP 测试覆盖模拟任务保存、运行、状态快照、步骤推进、证据查询及模拟回放；尚未覆盖真实设备输入或插件 worker。生产 HTTP 服务连接引导配置的统一 SQLite 数据库；通过服务推进至终态的运行会同步归档运行状态、任务快照和每步 PNG 证据。按 runId 的状态与证据查询可在重启后读取最近 64 条已完成运行；归档单条上限 12 MiB，写入失败会返回错误并保留终态 session/证据供重试；服务重试只归档既有结果，不重新执行命令。测试已验证解除一次性数据库故障后可重试并跨进程读取完整证据；进程在重试前退出时，未归档记录仍可能丢失。可编辑任务按版本单调递增持久化，服务启动时恢复最多 256 个；GET /debug/runs 合并当前运行和最近 64 条已归档运行。运行中记录和未归档的模拟回放配方仍只在内存，进程异常终止前未完成归档的步骤可能丢失；归档淘汰后旧状态、证据和回放配方不可查询。内部 `debug_plugin_session` 已实现受限 JSON-RPC 请求解析、随机会话 token、manifest/policy 鉴权、请求 ID 递增、最多 32 个待处理请求、取消标记与崩溃时待处理 ID 清理；内部 `debug_plugin_stdio` 提供最多 64 KiB 的换行分帧、JSON-RPC 响应和可注入的命令处理接口，EOF、超长帧与写入错误会关闭会话。内部 `debug_plugin_dispatch` 提供仅模拟的 task.start/task.step/task.cancel、按会话 runId 隔离的 observe.status/observe.evidence，以及经统一 `HttpDebugCommand` 校验后进入 `DebugHttpService::execute_direct` 的 `input.execute` 处理器；direct input 产生的 SandboxRun 会归入该 worker 的 owned runs，可查询状态与 evidence。task.cancel 需要 task 能力及该 worker 所有的 runId，只能在同步请求之间取消运行并保留已完成步骤证据，不会中断正在处理的 task.step；禁止通过 worker 参数请求 live 或委托其他 manifest。stdio JSON-RPC 集成测试已覆盖鉴权失败、task.step、task.cancel、状态与保留证据查询；该测试在同一处理器预先创建待取消的 run，不验证子进程执行中取消。worker 会话释放时尝试取消并完成其尚在运行的模拟任务，保留已有证据；归档或服务故障时仅为尽力清理，不保证恢复。内部 `debug_plugin_process` 可由宿主显式提供可信可执行文件，以随机 token 启动 stdio 子进程并接入上述模拟命令；Windows 测试验证握手、正常 EOF、直接子进程超时终止和后代继承 stdout 的超时回收；使用 `KILL_ON_JOB_CLOSE` Job Object 约束成功绑定的进程树，绑定失败即终止直接子进程，未通过 HTTP 暴露任意可执行路径。这不是 OS 级沙箱：spawn 与 Job Object 绑定之间仍有竞态窗口，且不限制文件系统或网络访问；同步处理器阻塞时也不能及时读取取消帧，不能作为可靠的非可信插件执行环境。`assert.evaluate` 尚无实际分发：当前断言只能在 `task.start` 的 step index 上绑定，以保持统一 runner/evidence 语义；插件应在 `task.start` 请求中提交断言。除 `window_foreground`、`window_available` 外的 UI/窗口状态及业务状态断言、完整跨进程运行中状态和独立 worker 认证链路仍未实现。Windows 只读冒烟测试需显式执行 `cargo test --lib live_window_read_only_capture_smoke -- --ignored --nocapture`、`cargo test --lib live_capture_run_read_only_smoke -- --ignored --nocapture`、`cargo test --lib live_service_read_only_capture_smoke -- --ignored --nocapture` 与 `cargo test --lib debug_http_live_capture_authenticated_read_only_smoke -- --ignored --nocapture`；本机 YuanShen 窗口已验证可查找、可截图、通过 live UI worker 产生 1280x720 的 SandboxRun 证据，并能通过服务对象及带 token 的 loopback HTTP 查询状态与 PNG 证据；未授权的 HTTP live 创建请求返回 401。另可显式执行 `cargo test --lib debug_http_live_wait_cancel_smoke -- --ignored --nocapture`：带 token 的并发请求能取消正在执行的 live `Wait`，返回 `cancelled` 终态并关闭 UI worker。前述只读冒烟测试不发送输入、激活或关闭窗口。2026-10-02 已在真实环境完成端到端验证：原神以管理员权限运行，调试程序同样以管理员权限启动（发布包运行时 + 当前源码编译的 exe，置于 `dist/sandbox-run`）。`POST /debug/mode?enabled=true` 后 `/debug/state` 报告 `enabled=true`、`gameInputAllowed=false`；`GET /debug/window` 返回 `available=true`；`POST /debug/command {"command":"capture"}` 得到 1920x1080 真实截图；保存 live 任务后经 `/debug/run` 与 `/debug/run/all` 推进至 `passed`，`dimensions` 断言成立，`/debug/run/evidence` 返回带 PNG 的证据；`window_foreground` 断言在 `activate` 后成立；`press_key escape` 的合成输入真实改变了游戏状态（千星奇域菜单开合，证据帧可见差异）。同时实测确认的边界：输入前必须先 `activate`，`ensure_foreground` 只做前台校验、不会激活窗口；前台不是目标进程时输入被明确拒绝（`当前前台窗口不是目标游戏进程，已中止输入`）；`/debug/run/replay` 对 live 运行返回 `live运行不可回放`；截图紧跟输入命令时会捕捉到过渡帧，需插入 `wait` 等待画面稳定。真实设备输入已在单机验证，但 OCR 识别、真实 OCR 断言与阻塞 OS 调用期间的取消恢复仍未实测；OCR 仅有 fake backend 与服务级测试。
