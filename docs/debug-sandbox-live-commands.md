# 调试沙盒：命令清单、实测效果与延迟

本文档记录调试沙盒原子命令的**实测行为、实测延迟和失败模式**，用于流程优化与 AI 自动调试。

> 内置业务流程的真机排查见 [debug-sandbox-flow-audit.md](debug-sandbox-flow-audit.md)（已包含拉黑流程）。

- 数据来源：2026-10-02 在本机对**真实运行中的原神**（管理员权限）实跑采集。
- 运行前提：原神与调试程序**同为管理员权限**；先 `POST /debug/mode?enabled=true`；请求带 `X-Miliastra-Token`。
- 所有耗时均为**端到端 HTTP 往返**（客户端 `Stopwatch` 计），单位毫秒，取多次重复的每次均值。
- 未实测的能力不会写成本文结论。

## 1. 实测延迟总表

测量方式：把一个命令重复 N 次放进同一任务，`captureAfterEach=false`，测 `POST /debug/run/all` 总耗时后除以 N；每次测量前的重聚焦与建运行不计入。

| 命令 | 重复数 | 每次耗时 | 说明 |
|---|---|---|---|
| `activate` | 5 | **191–195** | 激活游戏窗口并等待 `after_activate_ms` |
| `focus` | 2 | **191** | 含一次聚焦点点击 |
| `ensure_window` | 5 | **203–207** | 仅校验窗口存在 |
| `ensure_ready` | 2 | **190** | 激活 + 就绪检查 |
| `ensure_foreground` | — | **失败** | 只校验、不激活；游戏不在前台时明确报错 |
| `wait {0}` | 5 | **196–197** | 纯排队开销基线 |
| `wait {100}` | 5 | **290–292** | 基线 + 约 95ms |
| `wait {1000}` | 2 | **1197** | 基线 + 精确 1000ms |
| `capture` | 1/2/3 | **1556–1588** | 真实截图，见 §4 瓶颈分析 |
| `press_key` | 4 | **209** | 受前台校验保护 |
| `key_down` | 5 | **195** | 见 §5 异常观测 |
| `key_up` | 5 | **162–191** | |
| `mouse_down` / `mouse_up` | 5 | **202 / 187** | |
| `click` | 5 | **199** | 需前台为目标窗口 |
| `scroll` | 5 | **193** | |
| `drag` | 2 | **1335–1342** | 内置固定 settle 抖动，见 §4 |
| 建运行 `POST /debug/run` | — | **3–5** | 仅创建，不执行 |
| 证据取回（每帧） | — | **约 170** | 含 PNG 编码 + base64 传输 |

## 2. 命令效果实测

### 窗口与焦点

| 命令 | 实测效果 | 备注 |
|---|---|---|
| `ensure_window` | 校验目标窗口存在，返回成功 | 不改变任何状态 |
| `window_available`（`GET /debug/window`） | 返回 `available=true` | 只读，不激活、不截图 |
| `ensure_foreground` | 游戏不在前台时**报错**`当前前台窗口不是目标游戏进程，已中止输入` | 是**校验**不是激活 |
| `activate` | 把游戏切到前台；随后 `window_foreground` 断言成立 | 后续输入的前提 |
| `focus` | 先在聚焦点点击再确保前台 | 会产生一次真实点击 |

### 输入

| 命令 | 实测效果 | 备注 |
|---|---|---|
| `press_key escape` | **真实改变游戏状态**：千星奇域菜单开合，证据帧可见差异 | 需先 `activate` |
| `key_down` / `key_up` | 按下/抬起单个键 | 未做长按组合实测 |
| `mouse_down` / `mouse_up` | 按下/抬起鼠标键 | |
| `click` | 在客户区坐标点击 | 前台校验通过才发送 |
| `scroll` | 滚轮 | |
| `drag` | 按下→分步移动→释放 | 内置约 1140ms 固定 settle |

### 观测

| 命令 | 实测效果 | 备注 |
|---|---|---|
| `capture` | 返回 1920x1080 真实画面，归档为 PNG 证据 | 调试路径单次约 1.6s |
| `wait` | 可取消等待；`wait1000` 实测 1197ms | |

## 3. 组合任务实测

**菜单开合往返**（`activate → capture → escape → capture → escape → capture`）：
6 条命令全部 `passed`；三张证据帧字节数分别为 3797712 / 3708096 / 3312440，画面确实变化；图像确认第一次 ESC 后菜单展开、再切换后回到大世界。

**只读任务 + 断言**（`capture` + `dimensions 1920x1080`）：运行 `passed`，证据含真实 PNG。

**live 回放**：`POST /debug/run/replay` 返回 `live运行不可回放`（设计如此，未伪造）。

## 4. 延迟瓶颈分析

### 4.1 固定排队开销约 195ms/步

`wait {0}` 就要 196ms，且几乎所有"无截图"命令都在 190–210ms —— 这是提交到 UI worker 并等其让出主循环的固定代价。

### 4.2 主循环是主要争用源

应用主循环持续运行，实测每轮约 240ms：

- 截图 `frame=120–140ms`
- UI 状态分类 `total=95–117ms`
- 每约 2s 叠加一次聊天 OCR 扫描 `聊天扫描耗时: total=460–600ms`

调试命令与它共享同一个单线程 UI worker，因此**必须等当前轮次结束**。

### 4.3 调试截图约 1.6s（最重要发现）

`DebugAtomicRoutine` 在**每条命令末尾无条件截图**：

```rust
// src/composition/application/http_facade.rs
Ok(Arc::new(context.capture_frame()?.image().clone()))
```

实测对照（同一次会话，通过 `/debug/command`）：

| 命令 | 耗时 | captured |
|---|---|---|
| `wait {0}` | 1617ms | true |
| `capture` | 1643ms | true |
| `wait {0}` | 1553ms | true |
| `capture` | 1647ms | true |

结论：**该路径下 wait 与 capture 同价**，约 1.6s 全部是"排到 UI worker + 重新截图 + UI 状态分类"的成本；而主循环自己的截图只要约 130ms。

对照 SandboxRun 路径：`captureAfterEach=false` 时 `wait` 只需 196ms，因为不触发截图。**同一命令两条路径差约 8 倍。**

### 4.4 drag 约 1.3s 是设计内的

`src/adapters/windows/window.rs`：

- `DRAG_START_SETTLE_MS` 100
- `DRAG_PRESS_SETTLE_MS` 100
- `DRAG_STEPS` 8 × `DRAG_STEP_MS` 30 = 240
- `DRAG_BEFORE_RELEASE_MS` 200
- `DRAG_RELEASE_SETTLE_MS` 500

合计约 1140ms 固定等待，实测 1335ms 与之相符。

**P0 已实现说明（契约变化）**：`POST /debug/command` 以前把每条命令都包成 `captureAfterEach: true` 的合成任务，
因此每条命令都会付一次截图 + UI 状态分类的代价。现在改为 `capture_after_each: false`，
由运行器既有的规则决定是否截图：只有命令本身就是 `capture`，或有需要画面的断言时才拍。

响应契约随之变化：非 `capture` 命令返回 `captured:false`、`width:0`、`height:0`、`imagePngBase64:null`。
需要画面的调用方应显式再发一次 `{"command":"capture"}`，或改用 `GET /debug/screenshot`。
回归测试 `direct_command_only_screenshots_when_asked` 用一个"一旦截图就失败"的 `UiDevice` 证明：
非 capture 命令能成功走完（即没有截图），而 `capture` 仍会尝试截图。

同一改动也覆盖插件 worker 的 `input.execute`（它走同一个 `execute_direct`）。
**尚未实测**：真实环境下的 8x 收益未验证（§8.0(B) 只验了契约正确性；应用跑的是 debug 构建，时间数字不可用）；"复用运行时已发布帧"这一条未实现。

## 5. 已知限制与失败模式

1. **前台校验**：任何输入命令都要求目标窗口已在后台；否则明确失败并中止输入（防止误输入到别的窗口）。
2. **归档上限 12 MiB**：单条运行记录超过 12 MiB 会被拒绝。1920x1080 PNG 约 2.9MB → base64 约 3.9MB，因此**单运行最多约 3 帧全分辨率截图**。
3. **归档超限曾永久阻断后续运行**（本次实测命中并已修复）：失败记录留在会话表里被反复重试，导致之后**每一次** `POST /debug/run` 都失败。修复后超限时按"从最新帧往前丢弃画面、保留命令/结果/断言元数据"降级归档，并标记 `evidenceTruncated` 与 `imageDropped`。
4. **截图时机**：紧跟输入命令立即截图会拍到过渡帧（菜单动画中）。需要 `wait` 让画面稳定后再截图，否则会误判。
5. **live 运行不可回放**，回放仅支持 simulated。
6. **异常观测**：`key_down` 曾有整批 5 次仅 12ms（每次 2.4ms）的记录，与同批其他命令 195ms 不符；未复现，可能与 worker 恰好空闲有关，暂不作为结论。

## 6. 优化建议（按收益排序）

| 优先级 | 建议 | 预期收益 | 状态 |
|---|---|---|---|
| P0 | 归档超限降级为裁剪而非报错 | 解除"一次超限永久阻断所有运行" | **已实现**（含回归测试） |
| P0 | 调试路径不做无条件截图：仅在命令为 `capture` 或断言需要帧时截图 | 1.6s → 约 0.2s（约 8x） | **已实现**（含回归测试；真实环境 8x 未实测，仍为文档估算） |
| P1 | 调试截图跳过 UI 状态分类（调试只要画面，不要状态） | 省约 100ms/帧 | 未实现 |
| P1 | 调试模式下降低主循环频率或暂停内部自动化轮询 | 消除约 240ms/轮争用与约 500ms OCR 峰值 | 未实现 |
| P2 | 证据截帧降采样或改用 JPEG | 单帧 3.9MB → 数百 KB，缓解 12MiB 上限与约 170ms/帧传输 | 未实现 |
| P2 | 批量命令走单次 UI 提交，减少每步排队 | 每步省部分排队开销 | 未实现 |
| P3 | 长流程用 `captureAfterEach=false` + 显式 `capture` | 立即可用的流程级优化 | 使用建议 |

**流程优化结论（当前即可用）**：把截图从"每步都拍"改成"只在关键节点拍"，并避免在紧跟输入的同一帧上做判断；需要画面证据时先 `wait` 再 `capture`。

### 6.1 已修复：UI 运行时错误信息会说谎（本轮）

`UiWaitTimeoutError` 以前把 `RecvTimeoutError::Timeout` 和 `RecvTimeoutError::Disconnected` 映射成同一个值，
两者都显示 `UI runtime operation exceeded {timeout}`。于是"worker 根本没回传结果（几十毫秒就返回）"
会被报成"超过了 30 秒"，排查方向完全跑偏。

现在区分二者：真正等待超时保留原文，通道断开显示
`UI runtime stopped before returning the operation result`，并新增 `disconnected()` / `timeout()` 访问器。
修复后立即定位到一个既有问题（见 §8.1）。

### 6.2 已修复：全量测试的间歇性失败（既有问题）

`cargo test --lib` 约 10 次里有 2–4 次失败，失败用例随机落在**任意一个使用 `UiRuntime` 的测试**上，
表现为 `left: Cancelled/Failed, right: Passed`。加上诊断输出后拿到真实错误：

```text
results=[SandboxCommandResult { index: 0, ok: false, status: "failed",
  error: Some("等待 live UI 命令失败: UI runtime stopped before returning the operation result") }]
```

**根因**（`src/runtime/ui.rs:1305-1312`）：UI worker 在执行任务前校验入队时记录的 `debug_mode` generation：

```rust
let Some(_execution_guard) = crate::debug_mode::execution_guard(generation, external) else {
    // Mode changed after enqueue; dropping the response makes wait() fail without executing.
    return;
};
```

generation 变了就**丢弃任务且不回传**（这是防止陈旧外部命令执行的正确安全行为），调用方的等待通道随即断开。

测试里 `debug_mode::set_enabled` 是**进程全局**状态。全量测试并行执行时，少数几个会切换调试模式的用例
一旦切换，**其他测试正在等待的 live 操作就被丢弃**。已证实与本轮改动无关：跳过本轮新增的两个用例后，
8 次里仍复现 1 次。

**为什么不能按"谁切换谁加锁"来修**：受害者（例如 `live_foreground_assertion_observes_without_capture_or_activation`）
自己并不切换调试模式，它只是被别的用例切换所伤。因此必须让**所有创建 `UiRuntime` 的用例互斥**，
而不是只串行化切换方。

**已采用的修法**：在 `UiRuntime::start_configured` 中获取一个**仅测试期存在**的进程级凭据
（`src/runtime/ui.rs` 的 `test_runtime_serial` 模块），凭据随 runtime 一起释放。
它是**可重入**的（哈希按深度计数），所以同一个用例创建两个 runtime 也不会自锁；
Drop 不校验线程，即使 runtime 被移到别的线程释放，计数仍保持平衡、不会永久占锁。
生产构建里这个字段和模块都被 `#[cfg(test)]` 完全移除，运行时行为不变。

**验证**：连续 12 次 `cargo test --lib` 全部 `1243 passed; 0 failed`，无挂起
（修复前为 10 次里失败 2–4 次）。若修复无效，12 次全绿的概率只有约 1.4%。

**代价**：测试时长从约 4.5s 增至约 12.9s（用例串行化的直接结果）。
对于"全量验证必须可信"这个前提，这个代价是划算的——修复前任何一次失败都无法区分是回归还是 flake。

## 7. 复现方式

```powershell
# 1) 启动（管理员）发布包运行时 + 当前构建的 exe
#    dist/sandbox-run/ 下放置 exe、config.yaml、deps/
# 2) 启用调试模式
Invoke-WebRequest "http://127.0.0.1:18888/debug/mode?enabled=true" -Method POST `
  -Headers @{"X-Miliastra-Token"="123456"}
# 3) 只读验证
Invoke-WebRequest "http://127.0.0.1:18888/debug/window" -Headers @{"X-Miliastra-Token"="123456"}
# 4) 延迟基准（脚本为本轮实测所用）
pwsh -NoProfile -File dist/sandbox-run/bench5.ps1
pwsh -NoProfile -File dist/sandbox-run/bench6.ps1
```

任务与运行模型：`POST /debug/tasks` 保存（版本单调递增）→ `POST /debug/run` 创建并钉住版本 → `POST /debug/run/all?runId=` 推进到终态 → `GET /debug/run/evidence?runId=` 取证据 → `POST /debug/run/cancel?runId=` 取消。

## 8. 尚未实测

### 8.0 2026-10-02 真机复查（针对**当前源码**）

游戏（YuanShen）与应用同时在运行时做的真机检查。

**（A）真机只读用例：7/7 通过**

`cargo test --lib -- --ignored --nocapture --test-threads=1` —— 项目自带的 7 个真机只读用例
**全部通过（7 passed / 0 failed，1.5s）**。它们直接操作真实桌面，不经过正在运行的应用：

| 用例 | 真机观测 |
| --- | --- |
| `live_window_read_only_capture_smoke` | 捕获真实窗口 **1280x720**，`foreground=true`，`varied=true` |
| `live_capture_run_read_only_smoke` | live 沙盒帧 **1280x720** |
| `debug_http_live_capture_authenticated_read_only_smoke` | 带 token 的 loopback live 截图通过 |
| `debug_http_live_wait_cancel_smoke` | live `wait` + 取消通过 |
| `live_cancel_before_first_step_without_target_window` | 无目标窗口时取消通过 |
| `live_service_read_only_capture_smoke` | 服务层只读截图通过 |
| `live_executor_rejects_unavailable_playback_state_adapter_before_run_creation` | 运行创建前拒绝不支持的适配器 |

**（B）新构建的 HTTP 真机验证：通过**

用当前源码构建 debug 二进制（约 15s 增量）并部署到 `dist/sandbox-run` 后重启应用，
对真实游戏逐个验证：

| 检查项 | 真机结果 |
| --- | --- |
| P0 契约：`{"command":"wait"}` | **`captured:false` / `width:0` / `height:0` / `frameSequence:null`** |
| P0 契约：`{"command":"capture"}` | `captured:true` / **1920x1080** / `frameSequence:1` |
| `ui_state`（expected=primary） | **passed** —— 真机分类读数正确 |
| `window_available` | passed |
| `window_foreground`（expected=true） | failed，`actual=false` —— **查询成功**，拿到的是真实读数 |
| `clipboard_contains` 命中 | **passed** |
| `clipboard_contains` 未命中 | failed，`actual="MILIASTRA-CLIP-PROBE-20261002"` —— 如实报出实际内容 |

这补齐了此前"P0 收益未实测"与"`ui_state` 断言尚未实测"两项中**契约正确性**的部分
（性能倍数仍未测：应用跑的是 debug 构建，时间数字不可用）。

**（C）部署陷阱：必须停进程才能覆盖 exe**

`dist/sandbox-run/miliastra-wonderland-music.exe` 与 `target/debug/...` **大小、时间戳完全一致**，
即应用跑的是 **debug 构建**（不是 release）。覆盖它之前必须先杀掉正在运行的进程，否则 `Copy-Item` 会以
`The process cannot access the file ... because it is being used by another process` 失败，
而脚本继续往下跑——于是**验证的仍是旧二进制**。这一步曾让我误判了好几轮：P0 看起来生效了（旧二进制恰好也有），
而新增的设备方法看起来"没生效"。

可靠流程：`Stop-Process` → 等 3–5s → `Copy-Item -Force` → 校验大小/时间戳 → `restart-app.ps1`。

**（D）实际抓到的跨模块耦合缺陷（已修）**

运行时把设备包成了 `IsolatedDevice<WindowsUiDevice>`（`src/runtime/ui.rs`），
它是**泛型**装饰器，逐方法显式转发：只读观测转发给底层设备，输入方法额外加 `debug_mode::require_input()`。

我新增的 `UiDevice::clipboard_text` **只加在了 trait 和 `WindowsUiDevice` 上，没加进转发列表**，
于是真机上静默退回 trait 默认实现，报 `UI device does not support clipboard observation`。
而 `capture` / `window_*` 因为早已转发，看起来一切正常——单靠单测发现不了，
因为单测直接用 `WindowsUiDevice` 或测试替身，**不经过这层装饰器**。

已在 `IsolatedDevice` 上补转发、在装饰器定义处写明"新增方法必须同步转发"的维护约定，
并加了 `isolated_device_forwards_read_only_observations` 锁住该行为（用会返回特征值的探针设备）。

诊断手段也保留了：trait 默认实现现在会把**具体类型名**写进错误信息
（`(type=...IsolatedDevice<...WindowsUiDevice>)`），同类问题下次能一眼定位。

### 8.2 OCR 与模板：真机结果与验证方法（本轮）

**OCR：真机端到端已通过。**

- OCR 后端是**真实可用的 CPU 后端**，不是 stub：
  `/tools/ocr-backends` → `cpu [CPU] 可用: 初始化=18ms 检测=33ms 识别=42ms`。
- `ocr_contains` 在真实游戏画面上命中：区域 `(1650,80,200,100)` 期望 `绮星盛会` → **passed**
  （该文字在 `/tools/ocr` 的全屏结果里置信度 0.999）。
- 反例同样如实失败：期望 `绝不会出现的文字ZZZ` → `OCR 文本未命中`。
- 需要注意的能力边界：这个 OCR 认不出**聊天侧栏小字**与底部输入栏
  （`甲`、`发送`、`点击输入` 都未命中），同一区域全屏 OCR 也只返回 7 项。
  这是后端识别能力问题，不是断言缺陷；选区域时要挑大字。

**模板：只验证了与画面无关的通用不变量，不做事后拟合。**

先说结论：**live 游戏画面不能作为模板匹配的阈值测试**。连拍两帧在同一个 64x64 区域的平均差异是
**90.77（RGB 三通道之和，即每通道约 30）**，画面本身在变。因此"从这一帧取模板、下一帧自匹配应当得 1.0"
这个前提在真机上**不成立**；此时再去挑一个能通过的阈值，只是在拟合结果，证明不了任何东西。

本轮在真机上检查的是**与画面内容无关、因而与阈值无关**的性质（阈值取 0.1 / 0.5 / 0.9 / 0.999，未挑选）：

| 性质 | 结果 |
| --- | --- |
| 闸门单调：低阈值通过、高阈值失败 | 0.1/0.5 → passed；0.9/0.999 → failed |
| 失败时报告自洽：`best < min` | 0.8405 < 0.9；0.8405 < 0.999 —— 无违例 |
| 分数落在 [0,1] | 是 |
| 分数可复现（两次独立运行的 `best` 只差 1e-5） | 0.8404948 / 0.8405063 |

匹配器本身的正确性由**确定性测试**承担，而不是真机阈值：

- 既有：`fixture_cpu_matcher_matches_reference`（124 组对拍）、GPU 与 CPU 逐位一致、
  分片精确性 900 个位置全一致。
- 本轮新增：`inline_template_self_match_is_exact_across_sizes_and_positions` ——
  模板取自同一张图的同一区域时，**自匹配必须在 `min_score = 1.0` 下通过**。
  阈值用的是**理论上界 1.0 而不是拟合值**；覆盖 5 种模板尺寸 × 3 个位置共 13 个用例。
  模板字节直接调用库自身的 `to_luma8()` 生成，不手写灰度公式。

**顺带记下一个坑**：真机上我先把灰度公式写成 Rec.601 浮点（`0.299/0.587/0.114`），
自匹配只得到 0.707 的假失败。`image` 0.25 用的是 **Rec.709 整数截断**：
`(2126*R + 7152*G + 722*B) / 10000`（见 `image-0.25.10/src/color.rs:606`）。
换成正确公式后升到 0.841，剩余差距来自画面本身的变化。
**构造内联模板时不要手写灰度公式，直接复用库的转换。**

## 9. 真机联合观测（截图 + 日志，2026-10-03）

### 9.1 游戏侧当前状态（截图证据）

`GET /debug/screenshot` 返回 `image/jpeg` 1920x1080（不是 PNG，约 330-370 KB）。画面为**绮星盛会**大厅（茶杯座位、浮空岛），玩家角色铭牌 `(厅主)甲`，右下角 `UID: 900000013`。**注意：这与只读检索用的 UID 138868628 不是同一账号。**

聊天面板收起时，左下角无气泡，日志稳定输出 `markers=0 messages=0 []`；打开聊天面板后立刻变为 `markers=3 messages=3 [yellow] [韩雅]房间邀请 | [yellow] [珐露珊]: | [yellow] [天子]:这在干啥？`。**说明扫描本身正常，"扫不到"通常只是聊天框没开。**

`[珐露珊]:` 正文为空**不是 OCR 失败**：截图显示冒号后是一个小表情图标，即该消息本身就是表情而非文字。事件如实表达"有人发了东西、正文为空"。

### 9.2 两个必须知道的实际问题

**(A) OCR 抖动会驱动事件风暴。** 同一批消息在连续事件里只差 OCR 噪声：

```
seq 8:  [韩雅]房间邀请 | [珐露珊]:
seq 9:  [韩雅]房间邀请 | [珐露珊]: | []:这在干啥？        ← 说话人还没认出来
seq 10: [韩雅]房间邀请 | [珐露珊]: | [天子]:这在干啥？     ← 补上了说话人
seq 11: [韩雅]房间邀请 | [珐露珊]：| [天子]:这在干啥？     ← 只差半角/全角冒号
```

4 秒内 3 条事件，语义上只有 1 次真实变化（天子发言）。当前按"内容签名"去重能压住**完全一致**的重复帧，但压不住 `:` 与 `：`、以及说话人迟一拍认出的抖动。**尚未修复**——需要先确认是归一化 OCR 文本还是加滞回，避免又变成"按结果调参"。

**(B) 日志 91% 是同一行。** 本次日志 1033 行里有 941 行（91.1%）是 `聊天扫描结果`，且**每秒刷 2 次**（`-timing` 日志一天涨到 55 MB）。这是纯运维噪声，与业务无关。

### 9.3 日志暴露的其他真实阻塞

- **音乐源凭据失效**：`恢复上次会话的活动歌曲失败: playback operation failed: Failure { code: "relogin_required", message: "source credential was rejected" }`，播放器 `Starting -> previous reason=dispatch_failed`。**播放/点歌类业务命令目前无法真正生效**，需要先重新登录音源。
- **调试模式下聊天监听驻留建不起来**：`未能建立 UI 驻留：UI routine failed at prepare_residency (BeforeInput): 调试模式已禁用内部游戏输入`，反复重试。这与"调试模式不跑自动化"一致，属设计内行为。
- **内部按需截图会间歇失败**：`UI runtime 按需截图失败: 未找到目标游戏窗口进程`（8 次），而 `GET /debug/screenshot` 同一时刻可用——两者走的是不同路径。
- **退出热键注册失败**：`热键已注册 (0x80070581)`，全局热键不可用，但不影响其余功能。

### 9.4 受控测试内容注入通道

BOT2 是用户**已部署的机器人账号**，在游戏好友列表里可见。向 BOT2 发送 `@通告 私聊我[要的命令,含前缀]`，即可由朋友以**私聊**方式喂入指定内容。用途：验证 `scope: "secondary"` 的私聊观测路径（公开大厅走 `primary`，私聊走 `secondary`，后者**尚未实测**），以及为断言/回放提供确定性的聊天输入。

### 9.5 仍未实测

`chat_send`（聊天发送）已完成实现与单元测试（校验、审计顺序、无 adapter 时预运行拒绝），但**尚未真机运行**——调试模式下内部游戏输入被禁用，入队后的任务无法真正发出。
### 8.1 仍未实测

- 鼠标点击/滚轮/拖拽在游戏内的**具体业务效果**（仅测得延迟与成功状态，未逐项确认游戏语义）。
- 长按组合键、`input_text`/`paste` 在游戏内的实际落点与副作用（有向聊天框输入的风险，未做）。
- ~~真实 OCR 断言在 live 下的端到端效果~~：OCR 已验证通过（见 8.2）；模板的 live 阈值测试被证明不成立，改由确定性自匹配测试覆盖（见 8.2）。
- ~~`ui_state` 断言的真实分类读数~~：已在 8.0(B) 的真机构建上验证通过。
- 阻塞 OS 调用期间的强制取消。
