# （历史流水账）界面状态机逐轮记录

> 这是第 1~55 轮的**逐轮过程记录**，按时间追加，逻辑主线已被
> [ui-state-machine.md](../ui-state-machine.md) 取代。保留它是为了**不丢证据与推导过程**。

# 游戏界面状态机：现状盘点与还原计划

> 本文只记录**已核实**的内容。每条特征都标注证据来源；未实现的状态明确标为"未实现"，
> 不用推测填充。命名与判别规则以源码为准，实测数据注明抓取时间与命令。

## 1. 现有状态机（两层）

### 1.1 模板层 `src/ui/state.rs` —— 聊天相关界面

```rust
enum UiStateKind { Primary, Secondary, Unknown }   // 3 个
```

判别是**优先级排序**的模板匹配，顺序即优先级（`detect_ui_state_with_evidence`）：

| 序 | 判据 | 搜索区域 | 命中结果 | 证据来源 `source` |
|---|---|---|---|---|
| 1 | `ui-primary-friend.png` 模板匹配 | `screen.friend_rect` | `primary:friend` | `friend` |
| 2 | `ui-secondary-back.png` 模板匹配 | `screen.secondary_back_rect` | `secondary:chat` | `back` |
| 3 | 聊天标记扫描（按颜色计数） | `screen.chat_rect` | `primary:marker blue=N yellow=N pink=N` | `marker` |
| 4 | 以上都不命中 | — | `unknown` | `none` |

匹配前先把画面 `resize_exact` 到 `screen.expected_width × expected_height`，因此判据**与分辨率无关**。
阈值来自配置 `chat_templates.marker_threshold`（模板命中要求 `best_score >= threshold`）。

### 1.2 运行时层 `src/runtime/ui.rs` —— 加上游戏门与大世界

```rust
enum UiStateKind { Primary, Secondary, GameGate, Overworld, Unknown }   // 5 个
```

- `Primary` / `Secondary` 直接来自模板层。
- `GameGate` / `Overworld`：当模板层判为 `Unknown` 时，退回到对 `gate_region` 做 OCR 探测
  （`probe_game_state`），并以 `GameFallbackState` 做**去抖**：`required_count` 次一致才确认，
  探测间隔 `GAME_STATE_PROBE_INTERVAL = 5s`。
- 稳定性跟踪 `TrackedUiState`：`stable_kind` / `last_stable_kind` / `candidate_count` / `required_count`，
  未稳定时 `Display` 输出 `transition:<label> <candidate>/<required>`，完全判不出时输出 `transition:unknown`。

**这 5 个就是当前全部可判别的界面状态。** `unknown` 是显式的"判不出来"，不是"其它"。

## 2. 只读查询接口（本轮新增）

`GET /debug/ui-state`：返回分类结果**与判别证据**，需要令牌且需先启用调试模式。

证据必须随状态一起返回，否则"为什么判成这个状态"无法被独立复核，判别依据也就无法验证。
未分类时返回 `classified:false` 并给出原因（`diagnostic` 或 `reason`），**绝不用猜出来的状态充数**。

### 2.1 实测（2026-10-03，游戏停在绮星盛会大厅、聊天面板展开）

```json
{"kind":"primary","label":"primary:marker blue=0 yellow=5 pink=0",
 "stable":true,"stableKind":"primary","lastStableKind":"primary","frameId":8,
 "evidence":{"finalRule":"primary_chat_markers",
  "marker":{"searchRect":{"x":39,"y":879,"width":416,"height":143},
            "coordinateWidth":1920,"coordinateHeight":1080,
            "blue":0,"pink":0,"yellow":5},
  "templateProbes":[
   {"template":".../ui-primary-friend.png","searchRect":{"x":170,"y":1018,"width":50,"height":40},
    "bestScore":0.7981684803962708,"threshold":0.9,"hitRect":null,"outcome":"below_threshold"},
   {"template":".../ui-secondary-back.png","searchRect":{"x":15,"y":15,"width":65,"height":65},
    "bestScore":0.7513913512229919,"threshold":0.9,"hitRect":null,"outcome":"below_threshold"}]}}
```

这份输出可以直接复核：`yellow=5` 来自 (39,879,416,143) 区域；两个模板都因 `best_score < 0.9` 未命中；
最终规则是 `primary_chat_markers`。**判别依据全部可见，不依赖任何隐含状态。**

**记录在案但尚未处理的观察**：未出现在画面上的 `ui-secondary-back.png` 仍得到 **0.751** 分，阈值是 **0.9**，
余量约 0.15。这属于"需要证据才能判断"的事项，**不按观测结果调阈值**；若后续要处理，应给出与分辨率/
画面内容无关的判据（例如同时要求多个互斥模板成立），而不是把这个数字往下调。

## 3. 可判别特征总清单（19 个，来自 `GET /tools/templates`）

**更正**：本节早先版本称"好友列表/房间邀请/祈愿等未实现"，那是**错的**。
实测 `GET /tools/templates` 返回 19 个内置模板，配置来源见 `tool_templates_route`
（`src/interfaces/http/routes/system.rs:84-104`）。它们不是不存在，而是**分散在各 routine 里**，
没有被纳入统一的 `UiStateKind`。

特征类型全部为**模板匹配**（归一化后按 `best_score >= threshold` 判定），阈值来源见末列。

| # | name | 判别的界面 | 区域（配置路径） | 阈值来源 | 消费方 |
|---|---|---|---|---|---|
| 1 | `blue-marker` | 蓝色聊天标志 | `screen.chat_rect` | `templates.marker_threshold` | 统一状态机 `src/ui/state.rs` |
| 2 | `yellow-marker` | 黄色聊天标志 | `screen.chat_rect` | 同上 | 同上 |
| 3 | `pink-marker` | 粉色聊天标志 | `screen.chat_rect` | 同上 | 同上 |
| 4 | `friend` | **底部按键提示行的好友面板入口徽章**（三人剪影，右侧是 `O` 键） | `screen.friend_rect`（`170,1018,50x40`） | 同上 | 同上（→ `primary:friend`） |
| 5 | `secondary-back` | 二级聊天返回按钮 | `screen.secondary_back_rect` | 同上 | 同上（→ `secondary:chat`） |
| 6 | `secondary-hall` | 二级当前大厅 | `screen.secondary_hall_rect` | 同上 | `state.rs`、`friend_delivery.rs` |
| 7 | `world-wish` | 祈愿 | `screen.world_wish_rect` | 同上 | `state.rs`、`startup.rs` |
| 8 | `invite-view-star` | 邀请查看千星 | `invite.view_star_region` | 同上 | `invite.rs` |
| 9 | `invite-goto-hall` | 邀请前往大厅 | `invite.goto_hall_region` | 同上 | `invite.rs` |
| 10 | `invite-enter-hall` | 邀请进入大厅 | `invite.enter_hall_region` | 同上 | `invite.rs` |
| 11 | `friend-panel` | 好友面板 | `moderation.friend_panel_region` | 同上 | `moderation.rs`、`friend_delivery.rs` |
| 12 | `friend-search-panel` | 好友搜索面板 | `moderation.search_panel_region` | 同上 | 同上 |
| 13 | `friend-more-settings` | 好友更多设置 | `moderation.more_settings_region` | 同上 | 同上 |
| 14 | `friend-block-chat` | 屏蔽聊天 | `moderation.block_chat_region` | 同上 | 同上 |
| 15 | `friend-blacklist` | 拉黑 | `moderation.blacklist_region` | 同上 | 同上 |
| 16 | `friend-confirm` | 好友操作确认 | `moderation.confirm_region` | 同上 | 同上 |
| 17 | `wonderland-confirm` | 千星确认按钮 | `startup.wonderland_confirm_region` | `startup.wonderland_confirm_threshold` | `startup.rs` |
| 18 | `paimon-menu` | 派蒙主界面 | `startup.main_ui_region` | `startup.template_threshold` | `startup.rs` |
| 19 | `wonderland-map-star` | 千星地图入口 | `startup.wonderland_map_star_region` | `startup.template_threshold` | `startup.rs` |

此外还有 `custom_workflows.templates` 里用户自定义的模板，一并列出。

### 3.1 缺口（据此更正）

| 目标中的状态 | 实际状况 |
|---|---|
| 好友列表 / 好友搜索 / 更多设置 / 屏蔽 / 拉黑 / 操作确认 | 特征**已存在**（#11-#16），但**未纳入统一状态机**：只能由 `moderation`/`friend_delivery` 在自己的流程里查，无法"对任意截图问这是什么界面" |
| 房间邀请（查看千星/前往大厅/进入大厅） | 特征**已存在**（#8-#10），同样未纳入统一状态机 |
| 祈愿 / 派蒙主界面 / 千星确认 / 千星地图入口 | 特征**已存在**（#7、#17-#19），未纳入统一状态机 |
| 私聊（secondary）会话 | 只判到 `secondary:chat`，**分不出具体好友**；已有 `SecondaryChatObservation.friend_name` 可用于归属 |
| 聊天框开合 | **没有**"聊天已关闭"这一状态；闭框与"开框但无消息"在现有判据下不可区分 |
| 点歌面板 | **确认不存在**任何模板 |
| 结算 / 结果页 | **确认不存在**任何模板 |
| 茶会座位 / 大厅细分 | **确认不存在**任何模板（`Overworld` 一档到底） |

**结论**：目标(1) 的"枚举 + 可判别特征 + 证据来源"对上述 19 个模板已经成立；
目标(3) 的"任意截图都能被稳定映射到状态"目前**只对 5 个 `UiStateKind` 成立**，
对其余 14 个模板**不成立**——它们有判据但没有统一入口。这一条**不得声明为已支持**。

## 5. OCR 实测复核：放大截图逐字核对（2026-10-03）

### 5.1 方法

1. `curl -H "X-Miliastra-Token: <token>" -o shot.jpg http://127.0.0.1:18888/debug/screenshot`
2. 按 `/debug/ui-state` 给出的 `marker.searchRect` 外扩留边裁出聊天区（实测 `x=39 y=879 w=416 h=143`）。
3. 放大：整体用 3x `HighQualityBicubic` 看清版面；单个可疑字用 8x `NearestNeighbor` 看原始像素。
4. 与 `GET /debug/events` 的 `game.chat` 事件文本逐字对照。

**注意**：本机视觉分辨率约 1300x1300，超过这个尺寸会被下采样，所以裁剪区要窄、倍数要够。

### 5.2 已确认的 OCR 缺陷（有截图证据）

| 画面实际内容 | OCR 输出 | 结论 |
|---|---|---|
| `[女(禁言7天)]:@守望天空 只推荐米格25` | `[父(禁言7天)]:@守望天空只推荐米格25` | ❌ **`女` 误读为 `父`**（复现两次，同一处） |
| `纯狠活, 不锈钢机身那个3马赫速度那个` | `纯狠活,不锈钢机身那个3马赫速度那个` | ❌ **逗号后的空格丢失** |
| `@守望天空 只推荐…` | `@守望天空只推荐…` | ❌ **提及后空格丢失** |
| `[赝月的异端者]:我都到原神了怎么还有战雷入` | `…怎么还有战雷入` | ✅ **OCR 正确**；"战雷入"看着像"战雷人"，8x 像素级放大确认屏幕上就是 `入` |

最后一行值得单独记：它看着像 OCR 错误，放大后证明是**我的语义猜测错了**。
若不做视觉复核，就会去"修"一个不存在的缺陷。

### 5.3 结论：OCR 空白在两个方向上都会错

实测同时出现**多读空格**（`@天子 没头脑`，屏幕确有空格）与**漏读空格**（`@守望天空只推荐`，屏幕有空格但没读出来）。
因此：
- **不能靠归一化消除空白差异**——漏读的空格归一化补不回来。
- **也不能假设空白可信**——客户端不能依赖空格的正确性。
- **字符级误读真实存在**（`女`→`父`），且是视觉相似字之间的混淆，不是随机噪声。

这正是"每条气泡只推一次"（按气泡身份去重）而不是"按文本相等去重"的原因：
任何依赖文本逐字相等的判据都会被上述抖动击穿。

### 5.4 事件流实测（防抖动改动）

在真实游戏中开聊天连续观测，`game.chat` 事件的气泡序号：

```
第一次：8 条事件，气泡序号 5..12，去重后仍为 8  —— 同一气泡未被重复推送
第二次：4 条事件，气泡序号 5..9             —— 同一气泡未被重复推送
```

判据是"每个气泡序号在事件流里最多出现一次"。若 OCR 抖动仍在驱动事件，同一序号会重复出现。
**该判据可独立复现**，不依赖任何阈值。

## 4. 验证方法（可复现）

1. 抓图：`curl -H "X-Miliastra-Token: <token>" -o shot.jpg http://127.0.0.1:18888/debug/screenshot`
   （返回 `image/jpeg`，1920x1080）。
2. 取判据：`GET /debug/ui-state`，逐项核对 `finalRule`、`bestScore` 与 `threshold`、`marker` 计数。
3. 时序：应用日志中 `UI 状态检测耗时: ... state=<...>`（`timing` target）给出每一步耗时与判定结果。
4. 迁移：用 `GET /debug/events` 的 SSE 流观察状态变化（`kind` 目前尚未包含界面状态事件）。

**约束**：本状态机的判据不得通过"调阈值以匹配已观测到的截图"来取得通过。
任何新增判据都要能用与观测画面无关的方式独立复现，否则不得声明为已支持。
## 6. 只读目录探针 `GET /debug/ui-catalogue`（2026-10-03）

`GET /debug/ui-catalogue` 在一张截图上跑完 **19 个内置模板**，逐个给出
`matched` / `bestScore` / `threshold` / `region` / `hitRect`。

**它刻意不参与统一状态机的判定**：`UiStateKind` 的结论一个字都不改。
目的是让"任意一张截图能否映射到某个界面"这件事**有证据可查**，
而不是把 14 个分散在各 routine 里的判据永远藏在流程内部。

未命中时也返回得分（用 `best_template_candidate` 而非 `best_template_hit`），
否则"没命中"这个结论本身无法复核。

### 6.1 实测（大厅、聊天展开）

```
命中 4 / 19：yellow-marker(0.9971) pink-marker(0.9007) world-wish(0.9404) paimon-menu(0.9894)
```

### 6.2 视觉复核后的结论：其中两个命中的**不是面板，是常驻 HUD**

把命中区域裁图放大后逐一看过：

| 模板 | 区域 | 放大后实际看到的 | 结论 |
|---|---|---|---|
| `world-wish` | `1300,0,620x110` | 右上角那排**常驻图标栏**，其中确有祈愿的星形图标 | 匹配为真，但它**几乎在所有界面都在**，不能判别面板 |
| `paimon-menu` | `0,0,480x270` | 左上角**派蒙头像 + 小地图** | 同上，是常驻 HUD |

**这两条不得当作界面判别依据。** 命中的是常驻元素，不代表当前是哪个界面。
这也正好解释了它们为什么没进统一状态机：在 routine 内部使用时上下文兜底，
脱离上下文单独用，就会把"HUD 常在"误读成"当前在这个面板"。

### 6.3 已确认缺陷：**高分不等于命中**（本节结论已被证据推翻并重写）

> **更正**：本节早先版本把原因归为"模板匹配丢弃颜色，粉黄标记同形只差颜色"。
> 看了 19 个模板的参考图（`template-sheet.png`）后确认**这是错的**：
> `chat-marker-yellow` 是**黄四角星**、`chat-marker-pink` 是**粉色人形剪影**，
> `chat-marker-blue` 是**蓝色建筑图标**——三者形状**完全不同**。
> 原解释不成立，已按新证据重写。

**实测（2026-10-03，大厅界面）**：`pink-marker` 以 **0.9247** 命中，
命中矩形 `273,905,28x18`。把这个矩形连同每边 30px 留边裁出来放大
（`pink-hitrect.png`）——**那片区域只是一块平滑的青/金渐变背景，
既没有人形剪影，也没有任何标记或文字**。

同一张截图里还有更明显的例子：`friend-block-chat`(0.9427) 与 `friend-blacklist`(0.9439)
命中了**几乎同一个位置**（`1240,337` 与 `1245,332`）。这两个是**互斥的菜单项**，
不可能同时出现。裁出来看（`probe-blockchat-area.png`）——**只是天空和一根蓝色柱子**。

#### 真正的原因

`best_template_candidate`（`src/ui/template.rs`）用 `to_luma8()` 后按 SAD 匹配，
并按模板的 alpha 总量做归一化。当模板本身是**低对比度、被放大过的糊图**
（`template-sheet.png` 里多个模板明显发糊）时，它的能量集中在低频，
于是**任何平滑渐变区域都能得到很低的相对 SAD，也就是很高的分数**。

结果是：**0.9 以上的高分只能说明"这块区域长得像模板的低频轮廓"，
不能说明特征真的出现。** 这对本状态机的意义很严重——
阈值 0.9 在平坦背景上并不构成"存在性"判据。

**记录在案，未修改**：按约束不得靠调阈值掩盖。
合格的解法需要引入**存在性**判据（例如要求命中区域本身具备足够的边缘/结构能量，
或要求多个互斥模板中恰有一个成立），而不是改数字。

**对使用的含义**：`GET /debug/ui-catalogue` 的 `matched` 字段
**不得**直接读作"该界面当前存在"；它只是"这个模板在这种匹配方式下过了阈值"。
要断言界面存在，必须同时看 `hitRect` 处的画面内容。

### 6.4 附带确认：房间邀请在聊天里的形态

同一张放大图里可见 `[玖]组队邀请`、`[绯川铃]房间邀请`——邀请是**聊天行**，
名字后直接跟"房间邀请"且呈青色下划线，**没有冒号**。
这与 `[念]房间邀请` 的历史观测一致，可作为邀请类消息的形态特征。

## 7. 存在性判据：用结构统计量区分"真命中"与"平坦误配"（2026-10-03）

### 7.1 为什么需要

第 6.3 节确认：阈值 0.9 以上的高分在**平坦渐变背景**上也会出现，
所以"过阈值"**不构成存在性判据**。按约束不得靠调阈值掩盖，
因此这里补上**独立的、标准统计量**的证据，不参与任何判定：

| 字段 | 含义 |
|---|---|
| `regionStd` | 命中区域的灰度标准差（这块区域到底有没有结构） |
| `templateStd` | 模板自身的灰度标准差 |
| `correlation` | 命中区域与模板的**皮尔逊相关系数**（形状像不像） |

平坦区域方差趋 0，相关系数会**自然**趋 0——不需要人为规定"相关度到多少才算数"。
`correlation` 为 `null` 表示**算不出来**（尺寸不一致或纯色），与 0 含义不同。

### 7.2 实测：三帧对照（**按用户要求，每次测量都把帧一起存盘**）

用户当时**正在游戏里移动**，三次测量的 `frameHash` 各不相同，
因此每次都用 `save=` 把**当时那一帧**落盘，数字与图片一一对应：

| 模板 | score | regionStd | correlation | 结论 |
|---|---|---|---|---|
| `yellow-marker` | 0.9966 | 32.9 | **0.996** | ✅ 真命中（聊天标记） |
| `friend` | 0.9990 | 63.4 | **1.000** | ✅ 真命中（底部提示行的好友入口徽章） |
| `paimon-menu` | 0.9894 | 68.4 | **0.995** | ✅ 真命中（左上派蒙头像） |
| `world-wish` | 0.9601 | 25.3 | **0.725** | ✅ 真命中（右上常驻星形图标） |
| `pink-marker`（帧1） | 0.9005 | 34.1 | 0.634 | ⚠️ 形状歧义：落在真实标记上，但粉模板是人形、标记是星形 |
| `pink-marker`（帧3） | 0.9354 | **1.1** | **0.079** | ❌ 平坦误配 |
| `friend-block-chat` | 0.9428 | **3.7** | **-0.017** | ❌ 平坦误配 |
| `friend-blacklist` | 0.9440 | **4.1** | **-0.058** | ❌ 平坦误配 |

**分界极其干净**：真命中 `regionStd` 25~68、`correlation` 0.73~1.00；
平坦误配 `regionStd` 1~6、`correlation` ≈0 或为负。
**没有任何参数被调整过来迎合这个结果**——两项都是标准统计量，量出来就是这样。

### 7.3 三级解读（供人判断，**代码里没有把任何一级写死成判定**）

- `correlation` 高（≈0.7 以上）且 `regionStd` 与 `templateStd` 同量级 → 可信命中
- `correlation` 中等（≈0.6）→ **形状歧义**：图标不同但都是深色底上的亮块
- `correlation` ≈0 / 负 且 `regionStd` 很小 → **平坦背景**，该匹配没有内容依据

**代码没有据此做任何自动判定**：`UiStateKind` 的结论一个字都没改，
这里提供的始终是**证据**而不是新的阈值。

### 7.4 接口契约

`GET /debug/ui-catalogue?save=<路径>` —— 把本次测量所用的那一帧一并落盘，
返回里给出 `frameSaved`；无论是否落盘，**始终**返回 `frameHash`。

屏幕是动态的，事后重抓必然是另一帧；**只有把帧和数字一起存下来**，
这次测量才可能被第三方复核。**`frameHash` 不同的两次测量不可直接比较数字。**

## 8. 离线可复现：`save=` / `image=` 与私聊判据的验证（2026-10-03）

### 8.1 离线复现已机械化验证

`GET /debug/ui-catalogue?save=<路径>` 存下本次测量所用帧，
`?image=<路径>` 对指定图片重跑同一套探针。

| 检查 | 结果 |
|---|---|
| 实时抓帧 hash `46a2c8b01d00fb57` → 存 **PNG** → 离线重跑 | **指纹一致**，逐模板得分小数点后 6 位全同 |
| 同一 PNG 再跑一次 | 指纹仍一致（确定性） |
| 把抓帧存成 **JPG** 再重跑 | ❌ 指纹 `0ee55d4d49de0ec4` ≠ 原始 `7f8a392560d99dda` |

**结论：保存必须无损。** JPEG 有损，重新编码改变了像素，离线复现不成立。
因此 `save=` **一律强制写成 PNG**（扩展名不是 `.png` 会被改写，
实际路径在 `frameSaved` 里如实返回）。修正后复测：

`@
传 save=repro-b.jpg → 实际写入 repro-b.png
离线重跑 hash 28a9be9464d71f7a 与实时一致
逐模板得分不一致条数 = 0 / 19
`@

**"判别依据可被独立复现"到此是被机械验证过的，不再是口头声明。**

### 8.2 用已知私聊截图验证判据

对仓库自带真实私聊截图 `tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg`
（`frameHash=5cd8a961add60030`）跑全部 19 条探针，关键结果：

| 模板 | score | regionStd | correlation | 判读 |
|---|---|---|---|---|
| `secondary-back` | **0.9945** | 66.6 | **1.000** | ✅ 完美结构匹配 |
| `world-wish` | 0.9260 | 59.8 | 0.828 | ✅ 常驻 HUD（符合预期） |
| `secondary-hall` | **0.5370** | 42.8 | **0.164** | ❌ 结构上不相关 |

**两条结论**：

1. **`secondary-back` 是被验证的私聊判据**。分类器第 2 条规则（命中即判
   `secondary:chat`）用的正是它，`correlation=1.000` 是硬证据。
2. **`secondary-hall` 不是状态判据**。它在**真正的私聊截图上也判不出来**
   （0.537，远低于 0.9）。它与 `friend_delivery` 等流程里一样只作**定位锚点**用，
   因此"二级当前大厅"这个名字容易让人误以为它是判别特征，**实际不是**。

这也说明 `GET /tools/templates` 列出的 19 条**并非都可用于判别状态**：
其中一部分只是流程内部的定位点。

## 9. 全部 5 个 `UiStateKind` 状态的判据（2026-10-03 补齐）

前几节只覆盖了 `primary`/`secondary`。本节补上 `game_gate`/`overworld`，
两者**只在**前三条规则全部判为 `Unknown` 时才会被探测
（`state.rs:196-217`：`detect_ui_state_with_evidence` 返回非 Unknown 就直接返回）。

| 状态 | 可判别特征 | 区域 | 证据来源 |
|---|---|---|---|
| `primary:friend` | `ui-primary-friend.png`（三个人形剪影）命中 = 底部提示行的**好友面板入口徽章**（`O` 键） | `screen.friend_rect`（`170,1018,50x40`），实测 `corr=0.998` | `state.rs:400-422` |
| `primary:marker` | 聊天标记计数 **>0**（蓝/黄/粉任一） | `screen.chat_rect`（`39,879,416x143`） | `state.rs:451-478` |
| `secondary:chat` | `ui-secondary-back.png`（圆形返回箭头）模板命中 | `screen.secondary_back_rect`（`15,15,65x65`） | `state.rs:425-448`，**`corr=1.000` 已在真实私聊截图上验证** |
| `game_gate` | **OCR 文本含「点击进入」**（先剥掉所有空白再匹配） | `startup.enter_game_text_region`（默认 `900,1000,130x40`） | `state.rs:365-380` |
| `overworld` | `world-wish` 模板在 `world_wish_rect` 命中 | `screen.world_wish_rect`（`1300,0,620x110`） | `state.rs:337-361`，命中判 `overworld`／不中判 `unknown` |
| `unknown` | 标记数为 **0**，且前三者都不成立（规则名 `no_reliable_anchor`） | — | `state.rs:463-490` |

**注意 `game_gate` 的判据是"点击进入"而不是"进入游戏"**，且它同样采用"剥掉空白再比较"
的做法——与本项目聊天文本的处理一致。

### 9.1 已确认缺陷：`overworld` 判据无法区分"主世界"与"聊天为空的大厅"

代码链（已在源码逐行核对）：

`@
0 个聊天标记 → UiState::unknown()（规则 no_reliable_anchor）
            → 此时才会调用 probe_game_state
            → 大厅里 OCR 读不到「点击进入」
            → 落到 world-wish 模板判定
            → 命中则判 overworld，否则 unknown
`@

而实测：`world-wish` 在**大厅**画面上同样命中，且**结构可信**——
`score` 0.926~0.963、`correlation` 0.725~0.828、
命中矩形一直落在 `1487,13,70x70`（正是 `world_wish_rect` 内）。

两条前提都成立，因此：**当大厅聊天为空（无标记）且好友按钮模板未命中时，
分类器会把大厅报成 `overworld`。**

**当前证据状态（如实标注）**：
- 代码路径：**已逐行核对**，确定存在
- 前提测量：**已实测**（`world-wish` 在大厅命中且结构可信）
- 现场观测：**尚未捕获**。70 次连续采样（约 63 秒）全部为 `primary`，
  未出现 `overworld`，也未出现 `no_reliable_anchor`——因为该时段聊天一直非空。

在捕获到现场反例之前，**不得声称此路径已被实际触发**；
但也不得把 `overworld` 声明为"可靠"——它的判据只证明"右上角图标栏里有祈愿星"，
而那在多处界面都成立。**这是待解决的缺陷，不是已支持的能力。**

### 9.2 **更正**：9.1 的误判路径未被真实截图证实

第 9.1 节断言"大厅聊天为空时会被报成 `overworld`"。用**真实截图**复核后，
**这个具体路径不成立**，必须更正。

**证据帧**：`docs/ui-state-evidence/hall-chat-closed.jpg`（`frameHash=ae91f9f3d7c08b29`）。
这是一帧真实大厅画面，肉眼可见：茶杯座位、`(厅主)甲` 名牌、右上 `绮星盛会`、
左上派蒙头像+小地图，且**底部没有聊天气泡**（只有按键提示行）= 聊天关闭。
**这正是我预测会触发误判的那一帧。**

对它跑探针的结果：

| 模板 | matched | score | correlation | 作用 |
|---|---|---|---|---|
| `friend`（规则 1） | **True** | 0.9877 | **0.998** | **规则 1 先命中 → 返回 `primary:friend`** |
| `secondary-back`（规则 2） | False | 0.7547 | 0.197 | 不命中 |
| `yellow-marker` | True | 0.9056 | 0.250 | 结构弱，疑似误配 |
| `pink-marker` | True | 0.9332 | **-0.081** | 明显误配 |
| `world-wish` | True | 0.9821 | **0.207** | 结构弱 |

**结论**：聊天为空时**规则 1（好友按钮）仍然命中**，分类器返回 `primary:friend`，
**不会**走到 `probe_game_state`。因此 9.1 所述的误判路径**在本帧上不发生**。

**那么 9.1 还剩下什么？** 只有一点仍然成立，且需要重新表述：
- `overworld` 的判据（`world-wish` 命中）**本身结构上不稳定**：
  同一块 `world_wish_rect` 在大厅帧上的 `correlation` 实测在
  **0.207 ~ 0.828** 之间大幅波动。这不足以支撑"主世界"这一结论。
- 误判要真正发生，还需 `friend` **也**不命中。历史上观测到过一次
  （第 1 轮记录 `friend` score=0.798 < 0.9），但**未与 `world-wish` 同帧同时出现**。

**修正后的表述**：`overworld` 的判据不充分（只证明"右上图标栏里有星形图标"），
它在与 `friend` 同时失效时**可能**产生误判，但**目前没有任何一帧证据**表明它真的发生过。
**不得声称已复现，也不得声称该判据可靠。**

### 9.3 附带确认：聊天关闭时标记模板仍会误配

上面 `hall-chat-closed.jpg`（聊天明确关闭、画面无任何标记）里，
`yellow-marker` 仍以 0.9056 命中、`pink-marker` 以 0.9332 命中。
按结构判据看，两者 `correlation` 分别只有 0.250 与 **-0.081**——
**又是一组"过阈值但没有内容依据"的实例**，与第 6.3、7.2 节的结论一致。

顺带确认：**"聊天框开合"仍然没有独立状态**。本帧聊天关闭，分类结果是
`primary:friend`；聊天展开时（`hall-chat-open.jpg`）结果是 `primary:marker`。
两者判据不同但**都不是"聊天已关闭"**，因此这一状态依然**未实现**。

### 9.4 `friend` 判据到底是什么（放大实证）

把 `friend_rect` 附近裁出来放大 10x 看（`docs/ui-state-evidence/friend-region.png`）：
`170,1018,50x40` 是**大厅底部按键提示行里的一个白色圆角徽章**，
徽章里是**三人剪影图标**（即 `ui-primary-friend.png`），**右侧相邻徽章上写着字母 `O`**。

因此 `primary:friend` 的真实含义是：
**"底部按键提示行里的好友面板入口（`O` 键）可见"**。

这与代码完全对应：`moderation.rs:191` 正是**按 `O` 打开好友面板**，
然后等 `friend-panel` 模板出现。

**更正**：本文件早先把 `friend` 解释成"好友申请"，**那是错的**——
它是**按键提示**，与"有没有好友申请"无关。

## 10. 意外发现：`primary:friend` 与"聊天框开合"**相关**（但机制脆弱，不算已支持）

### 10.1 现象

同一个大厅，两帧真实截图，`friend` 模板给出**完全不同的结果**：

| 帧 | 聊天状态 | `friend` score | `correlation` | `matched` |
|---|---|---|---|---|
| `hall-chat-closed.jpg` | **关闭** | 0.9877 | **0.998** | ✅ |
| `hall-chat-open.jpg` | **展开** | 0.7806 | **0.412** | ❌ |

### 10.2 机制（已用带标注的放大图量化）

把底部按键提示行裁出来放大 6x，并在图上画出 `friend_rect` 的实际范围
（`row-closed.png` / `row-open.png`，红线 = x 170~220）：

| 帧 | 三人剪影徽章实际位置 | `friend_rect` 内是什么 |
|---|---|---|
| 聊天**关闭** | 约 **x 170~220** | **正是三人剪影徽章** |
| 聊天**展开** | 左移到约 **x 123~153** | 变成 **`O` 徽章** |

也就是说：**整个底部按键提示行在聊天展开时向左移动了约 40~50 像素**，
`friend_rect` 是固定区域，于是：

- 聊天关闭 → 框住三人徽章 → 规则 1 命中 → `primary:friend`
- 聊天展开 → 框住 `O` 徽章 → 规则 1 失配 → 落到规则 3（标记 >0）→ `primary:marker`

这也解释了 `friend` 在 `hitRect` 上只差 5px（`180,1026` vs `185,1026`）
却 `correlation` 从 0.998 掉到 0.412：区域内的最佳候选已经不是那个徽章了。

### 10.3 这意味着什么（**必须谨慎表述**）

1. **"聊天框开合"并非完全不可观测**：`primary:friend` 与 `primary:marker`
   在本例中**确实与聊天开合相关**。
2. **但这不是被设计出来的判据**，而是一个固定区域恰好框住会移动的 HUD 徽章所导致的
   **副产品**。它依赖徽章行在该分辨率下的精确像素排布。
3. **证据强度仅 2 帧**。机制在这两帧上成立，但**不足以声称它在所有情况下成立**。
4. 因此：**不得把"聊天框开合"声明为已支持**。正确表述是
   "观察到一种相关性，机制已定位，但未验证其普遍性"。

### 10.4 若要做成真正的判据（建议，**未实施**）

需要一个**与徽章绝对位置无关**的特征，而不是去动阈值。例如：
识别底部提示行的**徽章序列**（`Enter`｜三人剪影｜`O`｜麦克风｜`Alt+N`｜喇叭｜`N`），
以"序列里是否出现三人剪影徽章"作为判据——序列内容与聊天状态相关，
但判据本身不依赖某个固定像素区域。

**本文档只记录建议，代码未做任何改动。**

## 11. 日志实证：`game_gate` / `overworld` **从未稳定过**（2026-10-03）

目标 (4) 要求"用真实截图与**日志**逐条验证"。用 `deps/logs` 下的 timing 日志
（10-02 的 53MB + 10-03 的 6.4MB）统计全部 `state=` 取值：

| state 取值 | 次数 |
|---|---|
| `primary` | 99954 |
| `primary_marker` | 70885 |
| `primary_friend` | 44234 |
| `secondary_chat` | 33084 |
| `secondary:chat` | 31442 |
| `unknown` | 28877 |
| `transition:unknown` | 26192 |
| `transition:primary:marker` | 1014 |
| `ui_error` | 704 |
| `capture_error` | 341 |
| `transition:secondary:chat` | 178 |
| `transition:primary:friend` | 45 |
| **`transition:overworld`** | **5** |
| **`transition:game_gate`** | **1** |
| **`game_gate` / `overworld`（稳定形态）** | **0 / 0** |

### 11.1 结论

**`game_gate` 与 `overworld` 在全部记录里一共只出现 6 次，
且每一次都是 `transition:… 1/2` 的过渡态——去抖从未满足过，稳定判定为 0 次。**

6 条原始记录（全部形如 `1/2`，`required_count` 为 2）：

`@
2026-10-02 18:20:24  state=transition:overworld 1/2
2026-10-02 19:19:57  state=transition:overworld 1/2
2026-10-02 22:52:00  state=transition:overworld 1/2
2026-10-03 00:10:37  state=transition:game_gate 1/2
2026-10-03 00:10:51  state=transition:overworld 1/2
2026-10-03 00:11:37  state=transition:overworld 1/2
`@

### 11.2 一条可回放的迁移轨迹

`miliastra-wonderland-music-timing-2026-10-03.log` 第 360~1260 行：

`@
00:11:09  secondary:chat
00:11:10  secondary:chat
00:11:11 ~ 00:11:36   unknown / transition:unknown（连续约 25 秒判不出来）
00:11:37  transition:overworld 1/2   ← 只出现这一次，随即被打断
00:11:38  unknown
00:11:39  primary_friend
`@

**这条轨迹同时说明三件事**：

1. `unknown` 确实被**显式使用**：那 25 秒里分类器没有猜，而是如实报"判不出来"——
   这正是目标 (3) 要求的行为。
2. `overworld` 只以 `1/2` 冒头一次就被 `unknown` 打断，
   **下一次探测结果不同 → 计数器归零 → 永远到不了 2/2**。
3. 最终它自己收敛回了 `primary_friend`。

### 11.3 为什么稳定不了

- `probe_game_state` **只在**模板规则全部判 `unknown` 时才跑（`state.rs:206-217`）。
- 探测间隔 5 秒（`GAME_STATE_PROBE_INTERVAL`），且要求**连续** `required_count.max(2)` 次一致。
- 而 `overworld` 的判据（`world-wish` 命中）本身不可靠：同一块区域在大厅帧上
  `correlation` 实测在 **0.207~0.828** 之间波动（§7.2、§9.2），命中位置甚至在不同帧落在
  `1367,4` 与 `1487,13` 两个不同位置。

### 11.4 对"已支持"声明的处置

**`game_gate` 与 `overworld` 不得声明为已支持。**
它们的判据存在、代码可达、也确实被计算过，但在全部日志里**一次都没有形成稳定判定**。
准确表述是：**"这两个状态当前实际上不可用"**。

它们**可以**保留在 `UiStateKind` 里（代码没动），但文档与任何对外声明里
必须按本节如实标注，不能因为"枚举里有这一项"就当作已实现。

## 12. "可断言"能力盘点：沙盒断言词汇表（2026-10-03）

目标 (3) 要求状态机是"可查询、**可断言**的模块"。本节把应用内**实际存在的**
查询面与断言面列清楚（全部来自源码）。

### 12.1 查询面

| 接口 | 返回 |
|---|---|
| `GET /debug/ui-state` | 当前分类 + **判别证据**（模板得分/阈值/命中区、标记计数、最终规则）；判不出时 `classified:false` + 原因 |
| `GET /debug/ui-catalogue[?image=&save=]` | 19 个模板各自的得分、阈值、命中区，以及 `regionStd` / `templateStd` / `correlation`；可离线复现 |
| `GET /tools/templates` | 模板清单（名称、区域、阈值） |

### 12.2 断言面：`SandboxAssertion`（`debug_sandbox_runner.rs:279-335`）

沙盒任务里可用的断言共 10 种：

| 断言 | 作用 |
|---|---|
| **`UiState { expected, stable }`** | **只读 UI 状态断言：不截图，直接查询分类结果** |
| `WindowForeground { expected }` | 窗口是否前台 |
| `WindowAvailable { expected }` | 目标窗口是否可用 |
| `PlaybackQueueLength { expected }` | 播放队列长度 |
| `ClipboardContains { text }` | 只读剪贴板（"复制"类操作唯一可验证方式） |
| `Dimensions { width, height }` | 画面尺寸 |
| `Pixel { x, y, rgba, tolerance }` | 单像素比对 |
| `FrameDifference { min/max_changed_pixels }` | 帧间像素变化量 |
| `OcrContains { x, y, width, height, text }` | 区域 OCR 包含文本 |
| `Template { … grayscale, min_score }` | 区域内模板匹配 |

可断言的状态值 `SandboxUiStateKind` 与 `UiStateKind` **一一对应**：
`primary` / `secondary` / `game_gate` / `overworld` / `unknown`。

### 12.3 `UiState` 断言的精确语义（`debug_sandbox_runner.rs:984-1007`）

`@
1. reading.kind 必须等于 expected        → 否则报 "UI 状态不匹配: expected=…, actual=… (frame N)"
2. 若 stable==true，还要求 reading.stable → 否则报 "UI 状态 … 尚未稳定 (frame N)"
3. 无任何分类观测时 **显式报失败**："尚无 UI 状态观测"，绝不伪造状态
`@

其中 `reading.stable` = `stable_kind().is_some()`（`:1262`），
而 `reading.kind` 取的是**当前这一帧的分类结果**（`:1261`），**与是否稳定无关**。

### 12.4 由第 11 节推出的重要推论（**给写断言的人**）

- `UiState { expected: game_gate|overworld, stable: true }`
  ——**永远不可能通过**。§11 已证明这两个状态在全部日志里稳定判定 0 次。
- `UiState { expected: game_gate|overworld, stable: false }`
  ——**可能通过**，因为 `kind` 反映的是当帧分类结果，过渡态 `1/2` 时 `kind` 就已经是
  `Overworld` 了。但这种通过**只代表"这一瞬间的分类是它"**，
  后面紧接着就可能被 `unknown` 打断（§11.2 的轨迹正是如此）。

**因此**：对 `primary` / `secondary` 用 `stable: true` 是有意义的；
对 `game_gate` / `overworld` 用 `stable: true` 等于写了一条恒假断言，
用 `stable: false` 则是在断言一个瞬时读数。**这两种写法都不构成"该状态可靠"的证明。**

## 13. 一键独立复现：`docs/ui-state-evidence/verify.ps1`（2026-10-03）

前面所有章节的数字都写在文档里。为了让人**不必相信文档**，本节提供一个脚本：
它把文档里记录的"帧指纹 + 逐模板数值"全部重跑并断言一遍。

### 13.1 用法

`@
pwsh -File docs/ui-state-evidence/verify.ps1
`@

前提：应用正在运行（DEBUG 构建，默认 `http://127.0.0.1:18888`）。
全程**只读取**——不发任何输入、不改任何配置。

### 13.2 它断言什么

对每一帧，用 `GET /debug/ui-catalogue?image=…` 重跑探针，然后断言两件事：

1. **帧指纹**（对整帧灰度做 FNV-1a）与文档一致
   → 同时证明"被测量的确实是这个文件"和"像素没被换过"。
2. 逐模板的 `bestScore` / `correlation` / 是否过阈值 与文档一致
   （默认容差 0.0005）。

任一不符即报 `FAIL`，且整脚本**退出码 1**。

### 13.3 实跑结果（本机，DEBUG 构建）

`@
=== docs\ui-state-evidence\hall-chat-closed.jpg
  [PASS] frameHash = ae91f9f3d7c08b29
  [PASS] friend           score=0.9877  corr=0.998  matched=True
  [PASS] secondary-back   score=0.7547  corr=0.197  matched=False
  [PASS] world-wish       score=0.9821  corr=0.207  matched=True
  [PASS] yellow-marker    score=0.9056  corr=0.25   matched=True
  [PASS] pink-marker      score=0.9332  corr=-0.081 matched=True
=== docs\ui-state-evidence\hall-chat-open.jpg
  [PASS] frameHash = 0bb96532f5038556
  [PASS] friend           score=0.7806  corr=0.412  matched=False
  [PASS] yellow-marker    score=0.988   corr=0.993  matched=True
  [PASS] world-wish       score=0.9555  corr=0.804  matched=True
=== tests\fixtures\ui\secondary-chat-scrolled-1920x1080.jpg
  [PASS] frameHash = 5cd8a961add60030
  [PASS] secondary-back   score=0.9945  corr=1      matched=True

总计: PASS=12  FAIL=0
`@

### 13.4 **验证器自身的反向测试**（关键一步）

一个只会通过的验证器等于没有验证器。所以做了反向测试：
拷一份脚本，故意把 `friend` 的期望 score 改成 `0.5`、
把 fixture 的帧指纹最后一位改错，再跑：

`@
[FAIL] friend           score=0.9877 (期望 0.5)   corr=0.998 matched=True
[FAIL] frameHash 期望 5cd8a961add60031 实际 5cd8a961add60030
总计: PASS=10  FAIL=2
退出码 = 1
`@

**两处错误都被抓到，退出码 1** → 这个验证器不是空洞的。
它确实会失败，因此 `PASS=12` 是有意义的。

### 13.5 已知局限（如实标注）

- `frameHash` 由当前构建的图像解码 + 灰度转换决定。同一构建内确定；
  若未来更换 image crate 版本，理论上可能变化——届时本脚本会**如实报 FAIL**，
  而不是被调参骗过去。这是**故意**的设计。
- 脚本只覆盖 `docs/ui-state-evidence` 里的整屏帧。
  第 11 节的日志统计需要日志文件，不在本脚本范围内。

## 14. 找回旧证据帧：面板判据补齐 + `secondary` **确认误判**（2026-10-03）

整理工作区时发现上一阶段留下的 **13 张 1920x1080 整屏截图**（在 `dist/sandbox-run/probe/`，
10-02 01:52~02:11 抓的，此前被我遗漏）。已全部用探针跑过并归档。

### 14.1 新补齐：`friend-panel` / `friend-search-panel` 是**验证过的判据**

| 帧 | 画面（已肉眼复核） | 模板 | score | `correlation` | 命中区 |
|---|---|---|---|---|---|
| `state-friend_list.png` | **好友列表**（`我的好友 185/200`、好友条目、`黑名单管理`） | `friend-panel` | **1.0000** | **1.000** | `792,30,49x35` |
| `state-friend_search.png` | 好友搜索 | `friend-search-panel` | 0.9540 | **1.000** | `1637,122,96x33` |

把命中区裁出来放大看（`friendlist-tab.png`）：`friend-panel` 命中的是**好友面板顶部
标签栏里那个"人像卡片"图标**，白色圆底 + 深色图标，与模板**完全一致**（所以是 1.0000）。

**因此第 3 节里标"暂无图片/待补"的两项，现在有了实测判据与证据图。**

### 14.2 **重要更正**：`secondary-back` 在**千星奇域主菜单**上也命中

| 帧 | 画面 | `secondary-back` score | `correlation` | 命中区 |
|---|---|---|---|---|
| `state-wonderland_menu.png` | **千星奇域主菜单**（左侧按钮网格、玩家卡「甲 Lv.29」、右下 `返回提瓦特`） | **0.9782** | **0.994** | `22,21,50x50` |

把 `22,21,50x50` 裁出来放大（`menu-topleft.png`）：**是一个圆形返回箭头按钮**
（深色左弯箭头 + 米白圆底 + 灰环），与私聊界面的返回按钮**结构上就是同一个**。

**后果**：分类器规则 2 是"`secondary-back` 命中 → 返回 `secondary:chat`"。
由于主菜单左上角有同样的按钮、且该按钮落在 `secondary_back_rect`(15,15,65x65) 之内，
**规则 2 会在千星奇域主菜单上触发，把菜单判成"二级私聊"。**

**这是**活跃规则里的真实误判**（不同于 `overworld` 那条从未触发的规则）。
第 8.2 节曾据此认定 `secondary-back` 是"被验证的私聊判据"——
**该结论必须收窄**：它确实能在真实私聊截图上完美命中（`corr=1.000`），
**但它并不排他**，主菜单同样命中。所以 `secondary:chat` 的准确含义是
"左上角有个圆形返回按钮"，**而不是"当前在私聊"**。

### 14.3 `world-wish` 判据再确认（这次两边的帧都是真的）

| 帧 | 画面 | `world-wish` score | `correlation` |
|---|---|---|---|
| `state-overworld.png` | **千星奇域户外区**（可自由行走） | 0.9580 | 0.821 |
| `wonderland-hall`（`observe.jpg`） | **大厅**（聊天关闭） | 0.9821 | 0.207 |
| `state-wonderland_hall.png` | 大厅 | 0.9680 | 0.43 |

**大厅的得分（0.9821 / 0.9680）比真正的主世界（0.9580）还高。**
这以**真实帧对真实帧**证实了 §9.2 的结论：`world-wish` 不能区分主世界与大厅，
它只证明"右上角图标栏里有祈愿星"。

### 14.4 又一批确认的误配（按结构判据识别）

这些模板在多张真实截图上"过阈值"，但 `correlation` 极低或为负：

| 模板 | 在哪些真实帧上误配 | `correlation` 范围 |
|---|---|---|
| `wonderland-map-star` | 菜单/好友列表/好友搜索/主世界等 5+ 张 | 0.086 ~ 0.154 |
| `friend-block-chat` | 菜单 | -0.055 |
| `friend-blacklist` | 菜单 | -0.037 |
| `pink-marker` | 好友列表 / 好友搜索 | -0.117 ~ -0.118 |
| `friend-more-settings` | 更多设置 / 玩家名片 | 0.39（偏弱） |

### 14.5 一个正面结果：`state-map.png`（地图）**什么都不命中**

对 `state-map.png` 跑全部 19 条探针：**零命中**。
这正是期望行为——地图界面没有可靠锚点，分类器会**显式返回 `unknown`**，
而不是硬凑一个状态。

### 14.6 验证脚本已覆盖上述全部新断言

`docs/ui-state-evidence/verify.ps1` 已扩展，现在断言 **21 项**，实跑 `PASS=21 FAIL=0`，
新增覆盖 `friend-list.png` / `friend-search.png` / `wonderland-menu.png` / `overworld-hub.png`。

## 15. `secondary-hall` 重新定性：它是**二级界面的有效子状态判据**（2026-10-03）

### 15.1 现象

两个**都是二级（私聊）界面**的真实帧，`secondary-hall` 给出完全相反的结果：

| 帧 | `secondary-back` | `secondary-hall` | 命中区 |
|---|---|---|---|
| `round78-live.png` | 0.9984 / corr **1.000** | **1.0000 / corr 1.000** | `26,204,36x37` |
| `step-5.png` | 0.9719 / corr **0.989** | 0.6191 / corr **0.177** | `39,200,36x37` |

两帧都确实是私聊（`secondary-back` 都近乎完美命中），差别只在 `secondary-hall`。

### 15.2 把命中区裁出来看

同一块区域 `10,190,70x60`：

- `round78-live.png` → `secondary-selected-hall.png`：**一个白色建筑图标**
  （三角顶 + 三根柱子），深蓝底
- `step-5.png` → `secondary-selected-person.png`：**完全空白**，什么都没有

### 15.3 结论

`secondary-hall` 锚的是**"当前选中的会话是「大厅」类型时显示的那个建筑图标"**。
它在选中大厅时**完美命中**（`corr=1.000`），在选中**人**时该位置没有图标，于是不命中。

**这是正确行为，不是判据失效。** 因此：

**`secondary-hall` 是二级界面的一个有效子状态判据**——
它区分 `secondary` 内部的两种情形：
「当前选中会话 = 大厅」 vs 「当前选中会话 = 人」。
（对应的应用名"二级当前大厅"**其实是对的**，我此前对它的否定是错的。）

### 15.4 **更正我此前两处错误结论**

1. **第 6 节**我写过"它锚的是**选中会话的头像**"——**错**。
   放大后可确认那是**建筑图标**，与头像无关。
2. **第 8.2 / 10 节**我据 `corr=0.164` 断言"它在真实私聊截图上也判不出来，
   只是定位锚点，不是状态判据"——**错**。
   那次不命中的原因是**那张图里选中的不是大厅**，
   这对该模板而言是**正确的不命中**。`"判不出来"与"判据失效"是两回事`，
   我把后者当前者了。

### 15.5 一个过程性教训（记录在案）

上一轮我在批量查询里漏写了 `probe/` 前缀，`Invoke-WebRequest` 失败后
`$o` **残留了上一轮的对象**，导致两段输出显示的是**别的文件的数值**（表现为两个
不同文件名给出**相同 hash**）。这是变量未重置导致的假数据，
我已撤回那两段，并未写入任何结论。**"两个不同文件同一 hash" 是这类错误的好信号。**

### 15.6 验证脚本已覆盖

`verify.ps1` 新增 `round78-live.png` 与 `step-5.png` 两帧、4 项断言，
现在共 **27 项**，实跑 `PASS=27 FAIL=0`。

## 16. 发现一个**可复现的配置性缺陷**：审核流程的按钮区域不覆盖按钮（2026-10-03）

### 16.1 先说结论：**模板图片本身都是对的**

我把三个模板图直接打开看了（已归档为 `tpl-*.png`）：

| 模板文件 | 实际内容 | 尺寸 |
|---|---|---|
| `invite-view-star.png` | 按钮「**查看千星奇域档案**」 | 393x58 |
| `friend-blacklist.png` | 按钮「**加入黑名单**」 | 404x62 |
| `friend-block-chat.png` | 按钮「**屏蔽聊天**」 | 399x56 |

**命名与图片内容一致，没有错配。** 所以问题不在命名，在**搜索区域**。

### 16.2 证据帧

`docs/ui-state-evidence/friend-detail-actions.png`（`hash=a5f833dd544a178d`，
原文件名 `state-more_settings_menu.png`）。肉眼可见：顶部 `好友 / 添加好友`、
搜索框 `138868628`、`好友申请 0/50`，面板里竖排三个按钮
**`屏蔽聊天` / `加入黑名单` / `举报`**，右侧是 `角色展柜 / 名片` 页签。

**这是一个此前未编目的新界面**：好友-添加好友详情面板。（原文件名"更多设置"是错的。）

### 16.3 三个区域的配置值

| 配置项 | 值 | 覆盖的 x 范围 |
|---|---|---|
| `moderation.blacklist_region` | `1240,130,440x505` | **1240 ~ 1680** |
| `moderation.block_chat_region` | `1240,130,440x505` | **1240 ~ 1680** |
| `invite.view_star_region` | `400,80,440x860` | **400 ~ 840** |

（源码：`src/config/mod.rs:184-195`、`:296-301`）

而该帧上三个按钮的实际位置约在 **x 230 ~ 530**（面板左半边）。

### 16.4 实测结果与推论

| 模板 | 搜索区 | matched | score | correlation | 命中位置 |
|---|---|---|---|---|---|
| `friend-blacklist`（**正确的**模板） | 1240~1680 | **❌ False** | 0.787 | **-0.021** | `1240,191,404x62` |
| `friend-block-chat`（**正确的**模板） | 1240~1680 | **❌ False** | 0.785 | **-0.034** | `1240,196,399x56` |
| `invite-view-star`（**错误的**模板） | 400~840 | **✅ True** | 0.943 | **0.793** | `429,253,393x58` |

把 `invite-view-star` 的命中区裁出来（`tpl-invite-view-star-hit-mismatch.png`）：
**里面清清楚楚是「加入黑名单」四个字。**

**推论**：审核流程的两个区域都指向画面**右侧**（x 1240~1680），
而这三个按钮在**左侧面板**（x 230~530）。因此：

1. **正确的模板 `friend-blacklist` / `friend-block-chat` 在这张画面上永远找不到目标按钮**，
   只能在右侧区域里匹配到噪声（corr ≈ -0.02）。
2. `view_star_region`（x 400~840）**恰好覆盖**了按钮右半部分（400~530），
   于是**用「查看千星奇域档案」的模板命中了「加入黑名单」按钮**——
   两个都是"米白圆角条 + 深色中文"的按钮，形状相似，corr 0.793。

### 16.5 两种可能，**需要你确认是哪一种**

- **(a) 区域配错了**：这三个按钮在该界面就在左侧，`moderation.*_region` 应当覆盖左侧。
- **(b) 布局不同**：审核流程本意作用于**另一个界面**（那里按钮在右侧），
  这张 `添加好友` 面板只是**碰巧**也有同名按钮。

我**不能**仅凭这一帧判定是 (a) 还是 (b)——那需要知道审核流程设计时面对的是哪个界面。
但**无论哪种**，都说明：**这两个按钮的模板定位在本帧所在界面上是失败的**，
因此**不得声称"审核流程的按钮定位已支持"**。

### 16.6 顺带确认

- `friend-more-settings` 在本帧命中 0.924 但 `corr=0.389`（偏弱），
  命中区 `1174,181,46x44` 在右侧页签附近，**不足以作为判据**。
- `invite-goto-hall` 命中 0.907 但 `corr=0.272` → **误配**。
- 本帧 `secondary-back` 不命中（0.445/corr 0.025）→ 该界面**不会**被误判成私聊 ✓
  （对比 §14.2 的千星菜单**会**被误判，说明主菜单那个返回按钮才是问题所在）

## 17. 按键 → 界面 对照表（迁移图的直接证据，2026-10-03）

`dist/sandbox-run/probe/` 下有一套 `key_<按键>.png`，即**按下该键后的画面**。
这直接给出了"按键 → 落到哪个界面"的边。全部跑过探针，关键帧已肉眼复核并归档。

### 17.1 对照表

| 按键 | 落到哪个界面（已肉眼复核） | 分类器能否识别 | 证据 |
|---|---|---|---|
| `O` | **好友面板** | ✅ `friend-panel` **corr=1.000** | `friend-panel-via-O.png` |
| `Escape` | **千星奇域主菜单** | ⚠️ 误判成 `secondary:chat`（见 17.2） | `wonderland-menu-escape.png` |
| `M` | **地图** | ✅ 显式 `unknown`（零命中） | `map-via-M.png` |

## 20. 一次**事故的记录**，以及由修仪器推出的最终结论（2026-10-03）

### 20.1 事故：我把 `debug.rs` 截断了

我在批量改写时用了 **带 `limit` 的读取结果整篇回写**，把
`src/interfaces/http/routes/debug.rs` 从 748 行截成 400 行。丢失内容：

- **13 个路由处理器**：`debug_tasks_route`、`debug_runs_route`、
  `debug_run_evidence_route`、`debug_run_replay_route`、`debug_run_step_route`、
  `debug_run_all_route`、`debug_run_status_route`、`debug_run_cancel_route`、
  `debug_state_route`、`debug_mode_route`、`debug_task_save_route`、
  `debug_run_start_route`、`debug_command_route`
- **2 个响应构造函数**：`debug_screenshot_response`、`debug_events_response`
- 原有的 7 个单元测试

**没有备份**：该文件在 git 里是 `??` 未跟踪状态（routes 目录下唯一未入库的文件），
`git log --all` 无记录、无 stash、全盘只有一个副本、第二检出只有 `dist`。
**结论是：没有可恢复的副本，只能重建。**

### 20.2 重建依据（全部来自仍然完好的文件）

| 来源 | 提供了什么 |
|---|---|
| `protocol.rs` 的 `ROUTES`/`BODY_ROUTES` | 每个路由的**路径、JSON/body 形态、处理器名** |
| `protocol/tests.rs` | **精确契约**：`/debug/state` 要有 `enabled`/`gameInputAllowed`/`commands`；`/debug/command` 要有 `captured`/`width`/`height`；运行类路由参数名是 **`runId`**；`/debug/run/replay` 要带 `sourceRunId`；`/debug/run/status` **直接返回快照本身**（含 `run` 与 `task`） |
| `debug_http_service.rs` | `save/start/execute_direct/evidence/replay/advance/status_snapshot/cancel/tasks/runs` |
| `docs/debug-sandbox.md` | `/debug/events` 是 **SSE 流**，带 `since` 与 `drain=true` |
| `debug_events.rs` | 事件总线 `replay_after`/`subscribe`/`latest_sequence` |
| `frame.rs` | `LatestFrameCache::image()` 取宽高 |
| `debug_mode.rs` / `debug_sandbox.rs` | 输入开关与 `AtomicCommand` 全变体 |

**每一个被重建的函数都带注释说明它是重建的**，并在本文件顶部集中标注。
**若你手头有原始版本，请以原始版本为准。**

### 20.3 恢复结果

`@
cargo test --lib  →  1281 passed; 0 failed; 7 ignored
`@

**与事故前的基线（1281）完全一致**，包括重新补齐的 7 个测试
（覆盖 `frame_fingerprint` 的稳定性/敏感性/格式，`luma_std` 的平坦与有纹理，
`pearson_luma` 的自相关与无方差返回 `None`）。
期间出现的一次 `config::store` 并发测试失败经复跑确认为 **flaky，与本次改动无关**。

### 20.4 记下的操作纪律

**绝不用带 `limit` 的读取结果做整篇回写。** 改动一律用 `edit` 定位替换，
需要全文时读**不带 limit**。这次事故完全可以避免。

### 20.5 本轮真正的目标：把探针修成与分类器同窗口

第 19 节暴露了一个**我自己工具的缺陷**：`GET /debug/ui-catalogue` 的标记探针
在**整块 `chat_rect`** 上搜索，而分类器 `find_chat_markers` 只在
**裁剪区左侧 `CHAT_MARKER_SEARCH_WIDTH = 60` 像素**内搜索。**两者不等价。**

已修：

- `observation/chat.rs`：`mod scan;` → `pub(crate) mod scan;`（常量需跨模块引用）
- `routes/debug.rs`：三个标记探针改用与分类器**几何完全一致**的窗口，
  搜索结果里 `region` 现在是 `39,879,60x143`（原为 `39,879,416x143`）

### 20.6 **由此推翻我第 19 节的"推翻"** —— 最终结论

> ⚠️ **本节结论已被第 21 节否证。** 这里的数字来自**灰度**探针，
> 而分类器判标记用的是**彩色**判据，两者不是同一个谓词。保留原文以记录推理链。

第 19 节我曾据"命中 x 在条带外"断言那批误配**对分类器无影响**。
那个判断是**错的**，而且**错的原因正是这次修掉的工具缺陷**——
当时探针搜整块区域，报的"最佳命中"是区域内的全局最优，落在条带外；
换到正确窗口后，**条带内另有匹配**：

| 帧 | 聊天状态 | 条带内命中 | score | correlation | matched |
|---|---|---|---|---|---|
| `step-1.png` | 可见 | yellow `48,968` / pink `48,940` | 0.9967 / 0.9003 | **0.995 / 0.630** | ✅ 真命中 |
| `closed.png` | **HUD 全隐** | yellow `62,920` | **0.9112** | **0.365** | ⚠️ **过阈值** |
| `cat-frame-3.jpg` | — | pink `57,990` | **0.9354** | **-0.037** | ⚠️ **过阈值** |
| `step-3.png` | 渐隐 | （无） | 0.833/0.886/0.897 | 0.152/0.086/0.373 | 均不过 ✓ |

**⇒ 最终结论：`primary:marker` 确实会在"聊天不可见"的画面上误触发**，
因为在 `closed.png`（整个 HUD 都隐藏）上 `yellow-marker` 以 **0.9112 ≥ 0.9** 命中，
命中位置 `62,920` **就在分类器搜索的条带内**。

**可分离性**：`correlation` 把两者分得很开——
真命中 **≥ 0.63**，误配 **≤ 0.37**（含一个负值）。
而 `score` 分不开：真 pink 0.9003 与假 pink 0.9354 几乎一样。

**这与第 7 节的结论一致**：阈值判"过没过"，结构统计量判"是不是"。
结构统计量目前**只作为证据输出、不参与判定**（代码未改），
所以这个误配**现在是真实存在的**，如实记录，**不声称标记规则可靠**。

### 20.7 验证脚本已同步

`verify.ps1` 中两个标记期望值已换成**换窗口后的实测值**
（`hall-chat-closed.jpg`：yellow 0.8800/0.235 不命中、pink 0.9251/0.143），
复跑 `PASS=27 FAIL=0`。**脚本当时报 FAIL=2 正说明它有效**——
它准确指出了"文档里的数值在新工具下不可复现"。

## 21. 分类器本体终于可以离线复盘，且它**否证了第 20 节的结论**（2026-10-03）

### 21.1 上一轮我错在哪：**仪器与分类器不是同一个判据**

第 20 节我据探针数字断言"`primary:marker` 会在聊天不可见的画面上误触发"。
本轮逐行读分类器代码后发现，探针与分类器有**三处**不等价，我上轮只修了第一处：

| # | 分类器 `find_chat_markers` | 我当时的探针 | 状态 |
|---|---|---|---|
| 1 | 搜索窗口 = 裁剪区左侧 `CHAT_MARKER_SEARCH_WIDTH`(60) 像素 | 整块 `chat_rect`(416) | 第 20 节已修 |
| 2 | **彩色** RGB 三通道 SAD（`to_rgb8` / `cached_rgb_template` / `color_sad_at`） | **灰度**单通道 SAD | **本轮才发现** |
| 3 | 取**全部**过阈值命中，再 `dedupe_chat_marker_hits` | 只取**单个最优**候选 | **本轮才发现** |

**灰度分数不是标记判定的代理指标。** 平坦/中性背景能很好地匹配灰度模板，
却匹配不上带颜色的模板——所以那组 0.9112 / 0.9354 **对分类器毫无意义**。

### 21.2 修法：让探针调用**分类器自己的那两个函数**

- `observation/chat/scan.rs`：`dedupe_chat_marker_hits` → `pub(crate)`
- `routes/debug.rs`：新增 `classifier_marker_evidence`，直接调用
  `find_color_template_hit_groups`（彩色）+ `dedupe_chat_marker_hits`（同去重），
  **成组**复测 3 个标记模板（分类器就是三个一起搜再合并去重的），
  并在 JSON 里以 `classifierMatched`/`classifierHitCount`/`classifierRawHits`/
  `classifierBestScore`/`classifierHits` 单独报告，
  **与灰度探针的 `bestScore` 分开呈现，不混成一个结论**。

修后同一批帧的数字（彩色判据）：

| 帧 | 原始命中 | 去重后 | 最佳 | 过阈值 |
|---|---|---|---|---|
| `step-1.png` | 4 | 4 | 0.997 | ✅ |
| `ev-open.jpg` | 3 | 3 | 0.982 | ✅ |
| `step-3.png` | 0 | 0 | — | ❌ |
| `closed.png` | 0 | 0 | — | ❌ |
| `cat-frame-3.jpg` | 0 | 0 | — | ❌ |
| `ev-closed.jpg` | 0 | 0 | — | ❌ |

### 21.3 **分类器本体离线复盘**（本轮新增能力）

复盘中发现一个**真实的能力缺口**：`GET /debug/ui-state?image=...` **根本不认 `image` 参数**
——两次不同文件返回完全相同的 `frameId: 531`，它一直在判**实时画面**。
也就是说：**保存的帧只能被探针重跑，分类器本体的判定无法离线复核。**

已补：`debug_ui_state_route` 支持 `image=`，走 `classify_at`（分类器本体），
模板解析用**与生产同一条路径**（`UiTemplateArgs::default().resolve(&config.templates, &config.ocr)`）。
输出字段名与实时接口**逐字一致**，两条路径可直接对比；并如实声明两处语义差异：

- `frameId` 恒为 `null`（离线帧不属于运行时帧序列）
- `stable` 恒为 `false`（**稳定性来自跨帧累积，单帧不能证明稳定，不假装可以**）

### 21.4 最终结论（分类器本体 + 离线 + 帧指纹绑定）

`@
step-1.png       primary  primary:marker yellow=4   rule=primary_chat_markers   y=4
ev-open.jpg      primary  primary:marker yellow=3   rule=primary_chat_markers   y=3
step-3.png       unknown  unknown                   rule=no_reliable_anchor     无标记
closed.png       unknown  unknown                   rule=no_reliable_anchor     无标记
cat-frame-3.jpg  primary  primary:friend            rule=primary_friend_template
ev-closed.jpg    primary  primary:friend            rule=primary_friend_template
`@

**⇒ `primary:marker` 不会在聊天不可见的画面上误触发。第 20 节的结论是错的，本节取代它。**
`closed.png`（HUD 全隐）由分类器判为 **`unknown`**，规则 `no_reliable_anchor` ——
这正是目标要求的行为：**判不出来就显式返回 Unknown，不猜一个。**

### 21.5 两条独立路径互证

`ev-open.jpg`：目录探针报 `frameHash=0bb96532f5038556`，
离线分类器报 `hash=0bb96532f5038556` —— **同一帧、同一指纹**。
同一帧的 `yellow=3` 也在两条路径上一致（探针 3 个彩色命中，分类器 `yellow_count=3`）。
"判别依据可被独立复现"在**探针**与**分类器本体**两条路径上同时成立。

### 21.6 复现命令

`@powershell
$h=@{"X-Miliastra-Token"="123456"}
# 1) 分类器本体离线判定已保存帧
(Invoke-WebRequest "http://127.0.0.1:18888/debug/ui-state?image=ev-open.jpg" -Headers $h -UseBasicParsing).Content
# 2) 与分类器同判据的标记复测（彩色 + 同去重）
(Invoke-WebRequest "http://127.0.0.1:18888/debug/ui-catalogue?image=ev-open.jpg" -Headers $h -UseBasicParsing).Content
# 3) 全量证据回归
pwsh -File docs/ui-state-evidence/verify.ps1
`@

### 21.7 这一轮教给我的

**"我的仪器量出来的"和"程序实际判的"必须由同一个函数保证。**
本轮与上轮的两次反转（"有误配"→"无误配"→"真有误配"→"其实没有"），
根因都不是阈值调参，而是**仪器与判据不等价**。
任何"可判别特征"的证据，都必须先证明**测量路径与被测路径是同一个**，
否则数字再漂亮也不构成证据。这条已写进 `ui-state-evidence/MANIFEST.md` 的核对清单。

### 21.8 两次事故的**同一个根因**（工具层）

两次整篇回写都出事，原因不是我不小心，而是**工具的隐含上限**：

```
read 返回的 lines 可能**少于** totalLines（长文件会被截断），
但 totalLines 报的是真实行数，于是"读到的"与"文件里的"不一致。
```

第 25 次事故：读 @@debug.rs@@ 拿到 400 行、@@totalLines@@ 更大，用这 400 行回写 → 截断。
第 26 次事故：读本文档拿到约 925 行、@@totalLines=1017@@，用这约 925 行回写 → 丢掉 §20。

**纪律（已写入 MANIFEST 核对清单）**：

1. 回写前必须先断言 @@lines.length === totalLines@@；不等就**不要**整篇回写，改用 @@edit@@。
2. 任何写操作优先 @@edit@@ 定位替换；整篇 @@write@@ 只用于新建文件。
3. 大文件的拼接/恢复一律在 **Node 里用 @@fs@@ 直接做**，不经过 @@read@@/@@write@@ 通道。

本节文档本身就是这样恢复的：DSH 会话记录 @@~/.dsh/sessions/<id>/session.v3.jsonl.zstd@@
（**多帧拼接**的 zstd，需按魔数 @@28 B5 2F FD@@ 逐帧解压）里完整保存着历次 @@write@@ 的原文，
据此取回 §20 并与 §21 拼接；核对结果：**第 1–923 行逐字节相同**，只有第 924 行起不同。
这也说明"会话记录"是**可用的最后一道备份**，值得在事故恢复时优先想到。


## 22. 23 帧全量离线扫描：**确认一个真实的标记误判**，并给真命中做了视觉复核（2026-10-03）

### 22.1 方法：把证据库**每一张完整帧**过一遍分类器本体

第 21 节补上的 `/debug/ui-state?image=` 让这件事成为可能。做法：

- 取 `docs/ui-state-evidence/` 下全部 53 个图片文件，**按尺寸筛出 1920×1080 的完整帧**（23 张）。
  其余 30 张是裁剪图/模板图/放大图，送进分类器会被归一化拉伸，结果**没有意义**，所以排除。
- 逐张调用 `/debug/ui-state?image=<绝对路径>`，记录 `kind`/`finalRule`/`label`/`frameHash`。
- 每张都带 `frameHash`，任何结论都能指回具体某一帧重跑。

### 22.2 结果（23 张完整帧）

| 判定 | 张数 | 帧 |
|---|---|---|
| `primary:marker` | 5 | `full-hall.jpg`(yellow=5)、`hall-chat-open.jpg`(3)、`overworld-hub.png`(5)、`probe.jpg`、`wonderland-collection.png`(**blue=8**) |
| `primary:friend` | 1 | `hall-chat-closed.jpg` |
| `secondary:chat` | 3 | `secondary-full.png`、`wonderland-menu.png`、`wonderland-menu-escape.png` |
| `unknown` | 11 | `friend-list/search/detail-actions/panel-via-O`、`hall-page`、`hud-hidden`、`journey-page`、`map-screen`、`map-via-M`、`more-settings`、`team-finder`、`wish-page`、`wish-screen`、`wonderland-level-page` |

`probe.jpg` 判为 `primary:friend`（与 `hall-chat-closed.jpg` 同源）。
`more-settings.png` 与 `friend-detail-actions.png` 的 `frameHash` 相同
（`a5f833dd544a178d`）⇒ **两个文件名指同一帧**；`wish-page.png` 与 `wish-screen.png` 同理
（`a5867a79f7beae0c`）。证据库里存在重复条目。

### 22.3 视觉复核：真命中 2 例，误判 1 例

裁剪聊天区 `0,850,520x210` 并放大 3 倍（双三次）逐帧核对：

| 帧 | 分类器 | 视觉复核 | 结论 |
|---|---|---|---|
| `full-hall.jpg` | `primary:marker yellow=5` | 聊天框可见，**正好 5 条**消息、5 个橙色菱形标记 | ✅ **真命中，计数精确** |
| `overworld-hub.png` | `primary:marker yellow=5` | 聊天框可见，**正好 5 条**（含 `[无名氏]房间邀请`） | ✅ **真命中，计数精确** |
| `wonderland-collection.png` | `primary:marker **blue=8**` | **大片平坦蓝色渐变底** + 垃圾桶/筛选图标 + 「最近收藏」胶囊 | ❌ **误判：没有聊天框** |

复核截图已归档：`verify-tp-hall-chatarea.png`、`verify-tp-overworld-chatarea.png`、
`verify-fp-collection-chatarea.png`。

### 22.4 确认的缺陷：`primary:marker` 会在**平坦蓝底**上误触发

`wonderland-collection.png`（奇域收藏页）被判成"大厅且有 8 个蓝色聊天标记"。逐项证据：

`@
frameHash      7e20a9dcb17daf03
搜索区         39,879,60x143       阈值 0.9
灰度 bestScore 0.895   correlation 0.004   regionStd 2.36   ← 区域几乎全平（255 制）
彩色(分类器)   原始 27 命中 → 去重后 8，最佳 0.9182，全部 ≥ 0.9
命中坐标       blue @41,879  @71,879  @51,902  @47,933  @39,953  @69,953  @39,973  @69,973
`@

命中点全部落在分类器搜索的 60px 条带内、**分数全部过阈值**，所以这是分类器**自己的判定**，
不是探针偏差——第 21 节刚把探针修成与分类器同函数，这次数字就来自那条同函数路径。

**根因**：`blue-marker` 模板以蓝色为主体，而收藏页左下角是一大片**几乎无结构**的蓝色渐变
（`regionStd 2.36`），模板在**任意位置**都能 SAD 过阈值。与第 7 节结论一致：
**"过阈值"只说明"像"，不说明"是"。**

**⇒ 明确声明：`primary:marker` 目前不可靠。本目标不把"标记判据可靠"列为已支持能力。**
第 25/26 节两次反转讨论的是**灰度探针**的假象；**本节是彩色判据下、经视觉复核的真实误判。**

### 22.5 可分离性（供后续设计参考，**本轮不据此改动判定**）

| 判据 | 真命中（full-hall / overworld-hub / hall-chat-open） | 误判（collection） |
|---|---|---|
| `correlation` | 0.995 / 0.630 | **0.004** |
| `regionStd` | 有文字与头像，明显大于 0 | **2.36**（近乎全平） |

两者都分得很开，但要变成判定条件就必须**选一个切点**。按你的要求
（**不要根据结果定阈值、要通用**），切点属于**设计决策**，**我不擅自加**。
当前判定逻辑**一个字符都没改**，缺陷如实记录在案。
若你决定加"结构前提"（例如命中区域必须有足够方差才计入），我可以在这批证据上回归验证；
但那时也要说明**为什么这个切点是通用的**，而不是"因为这几个帧能过"。

### 22.6 其它发现

- **好友面板全部判 `unknown`**（`friend-list`/`friend-search`/`friend-detail-actions`/`friend-panel-via-O`）。
  这是**设计使然**：friend 规则查的是 `screen.friend_rect`（大厅里的好友**按钮**），
  不是打开后的面板。**面板未被建模** ⇒ 显式 `unknown` 是正确行为，不是缺陷。
- **`overworld-hub.png` 判 `primary:marker` 而非 `overworld`**：
  分类为 primary 后**不会**再走 `world-wish` 分支，`overworld` 在**有聊天标记时不可达**。
  与第 11 节日志实证（`overworld` 从未稳定过）互相印证，属已知限制。
- **`wonderland-menu.png`（ESC 主菜单）判 `secondary:chat`**：第 15 节已记录的已知误判，本次复现。

### 22.7 新增独立验证脚本

`docs/ui-state-evidence/verify-classification.ps1`：对已保存帧断言**分类器本体**的
`kind`/`finalRule`/标记计数，共 10 条。其中两条**以 `DEFECT` 标注**记录当前已确认的
错误行为——缺陷一旦被修，脚本会**立刻失败**提醒更新文档，不让缺陷悄悄变成"预期行为"。

`@
分类器离线判定: PASS=10 FAIL=0
`@

与 `verify.ps1`（断言**目录探针**分数，PASS=27）互为独立路径：
**"判别依据可独立复现"现在在探针与分类器两条路径上分别成立，且各有一键脚本。**

### 22.8 复现命令

`@powershell
$h=@{"X-Miliastra-Token"="123456"}
$d="D:/Code/Miliastra-Wonderland-Music/docs/ui-state-evidence/"
# 误判
(Invoke-WebRequest ("http://127.0.0.1:18888/debug/ui-state?image=" + $d + "wonderland-collection.png") -Headers $h -UseBasicParsing).Content
# 真命中
(Invoke-WebRequest ("http://127.0.0.1:18888/debug/ui-state?image=" + $d + "full-hall.jpg") -Headers $h -UseBasicParsing).Content
# 一键回归
pwsh -File docs/ui-state-evidence/verify-classification.ps1
`@


## 23. 离线 fallback 探针 + world-wish 判据的最终定性（2026-10-03）

### 23.1 先补上一处我自己工具的不等价

第 22 节的离线分类只探了 **2** 个模板（friend、secondary-back），没有 world-wish。
读码后确认原因：classify_at 只在 Unknown 时进入 GameFallbackState::new(2) 保护的 fallback，
而它需要**连续 2 帧**都是 unknown —— **单帧离线分类永远到不了那里**。
离线路径比实时路径**弱**，这又是一次「仪器与被测不等价」（第 21 节的教训）。

修法（**不伪造时序**）：新增 ui::state::probe_game_state_without_entry（薄包装，
不暴露私有的 GameEntryDetection），离线分类在结果为 Unknown 时**额外**跑一次它，
以 fallbackProbe 字段报出，并**明确标注**：

> note: 仅证据：实时路径需连续 2 帧 unknown 才采用本结果，离线单帧不采用

**它不参与 kind 判定**，只让离线也能看见 world-wish 的分数。
已知限制（如实声明）：game_entry 需要 OCR 运行时，离线传 None，
所以 **game_gate 这条分支离线测不到**。

### 23.2 用新探针测出的结果：**判据是反的**

| 帧 | 离线 kind | fallback 结论 | world-wish 分数 |
|---|---|---|---|
| wish-screen.png | unknown | overworld | 0.9643 过阈值 |
| wish-page.png | unknown | overworld | 0.9643 |
| hall-page.png（**大厅页**） | unknown | **overworld** | **0.9679** 比真帧还高 |
| team-finder.png（组队页） | unknown | **overworld** | 0.9292 |
| map-screen.png | unknown | unknown | 0.6989 |
| journey-page.png | unknown | unknown | 0.8441 |
| hud-hidden.png | unknown | unknown | 0.6865 |
| wonderland-level-page.png | unknown | unknown | 0.8351 |

阈值 0.9 是配置值，**没有为这批结果改动过任何一个数字**。

### 23.3 视觉复核：world-wish 到底抓的是什么

把 world_wish_rect（1300,0,620x110）从三帧裁出、放大 2 倍并列（verify-worldwish-mismatch.png）：

- wish-screen.png：黄橙背景 + 0/30 货币胶囊 + **四角星圆环按钮**
- hall-page.png：「返回提瓦特」按钮 + **同一个四角星圆环按钮**
- team-finder.png：浅色背景 + **同一个四角星圆环按钮**

再取更宽上下文 1150,0,770x220（verify-worldwish-context.png）对比**真野外帧** overworld-hub.png：
天空 + **一排功能图标**（六边形/火/星芒/书/紫星/旗/Zzz）+ 「绮星盛宴会」横幅 + 27 ms 延迟，
**没有那个四角星圆环**。

**因此 world-wish 模板抓的不是「主世界」的任何特征，而是「四角星圆环按钮」这个控件**；
它出现在祈愿页、大厅页、组队页等**带该按钮的界面**上，而在真野外帧上**根本不存在**。

**结论：world-wish 不但不能判 overworld，它的命中方向与事实相反**
（真野外不命中，非野外反而命中）。因此：

> **overworld 状态不成立，本目标不把它列为已支持能力。**
> 它当前是「能返回 overworld 这个枚举值，但那个值没有可复现的判别依据」。

### 23.4 与已有记录的关系（**不是新发现，是确证**）

迁移文档第 29/52 行与状态文档 §14.3 **早已**记下「判据不充分，在大厅得分更高」。
本轮新增的是三件事：

1. **量化**：用分类器**自己的** probe_game_state 路径测出分数，真帧 0.9643 < 大厅页 0.9679（**顺序是反的**）。
2. **定性**：视觉查明模板对应的**具体控件**，并证明真野外帧**不含**该控件。
3. **能力**：离线现在也能测这条分支（fallbackProbe），使该结论可离线复现。

同时它**解释了 §11 的日志实证**（overworld 从未稳定）：不只是「很少被走到」，
而是**即便走到，判据也会把大厅页/组队页判成主世界**。

### 23.5 仍然缺的东西（**不猜**）

真野外帧只有 overworld-hub.png **一张**，没有任何「非野外且无该按钮」的对照，
所以**目前无法建立可用的 overworld 判据**。要立这个判据，至少需要：
主世界（含大厅内外多个场景）与所有非主世界界面各若干帧，再找**只在主世界出现**的元素。
在拿到这些帧之前，本目标**不声明 overworld 可用**。

### 23.6 复现命令

```powershell
$h=@{"X-Miliastra-Token"="123456"}
$d="D:/Code/Miliastra-Wonderland-Music/docs/ui-state-evidence/"
(Invoke-WebRequest ("http://127.0.0.1:18888/debug/ui-state?image=" + $d + "hall-page.png") -Headers $h -UseBasicParsing).Content
# 看 fallbackProbe.kind 与 .templateProbes[0].bestScore
pwsh -File docs/ui-state-evidence/verify-classification.ps1
```


## 24. secondary 判据的排他性检验（**证伪了我自己的候选修复**）+ 主菜单就是入口地图（2026-10-03）

### 24.1 方法：把 secondary-back 探针扫遍全部完整帧

沿用 §23 对 world-wish 的同一套方法：23 张 1920×1080 完整帧逐张跑 secondary-back。

| 帧 | score | correlation | 过阈值 | 真值 |
|---|---|---|---|---|
| secondary-full.png | 0.9945 | **1.000** | ✅ | **真**（私聊界面） |
| wonderland-menu.png | 0.9782 | **0.994** | ✅ | **误**（千星奇域主菜单） |
| wonderland-menu-escape.png | 0.9782 | **0.994** | ✅ | **误**（同上） |

**3 个命中里 2 个是误判。**

### 24.2 视觉复核：模板抓的是「圆形返回按钮」

裁 secondary_back_rect（15,15,65x65）加红框并放大 4 倍并列
（verify-secondary-mismatch.png）：三帧的框内是**同一个圆形返回按钮** ——
深色箭头、白底、同样的外圈。二级界面与主菜单**共用这个控件**。

**这里与 §22 的标记误判有本质区别，必须分开记：**

| | 标记误判（§22） | secondary 误判（本节） |
|---|---|---|
| 误判处 correlation | **0.004** | **0.994** |
| 性质 | 平坦区域的伪匹配 | **真实存在的同一个控件** |
| 能否靠结构量分离 | 能（阈值待你定） | **不能，任何阈值都救不了** |

**所以本节不是"再调一个阈值"的问题，而是判据本身不成立。**

### 24.3 我的候选修复被自己证伪

假设：二级界面有 secondary-hall 之类的子锚点，主菜单没有，用两个判据**同时**命中即可分开
（阈值都不用动）。实测：

| 帧 | secondary-back | secondary-hall |
|---|---|---|
| secondary-full.png | 0.9945 ✅ | **0.5371** ❌ |
| wonderland-menu.png | 0.9782 ✅ | **0.6188** ❌ |

**真帧自己都不过 secondary-hall** ⇒ 这个组合分不开任何东西，**假设被证伪，方案作废**。
（真帧是**私聊**界面，本来就不含"大厅型"图标，我的假设从一开始就错了。）

### 24.4 整帧复核：确认真值与真误判

- wonderland-menu.png：**千星奇域主菜单** —— 甲档案（UID 900000013、奇域等级 Lv.29、
  本期缤纷评分 970）、左侧图标栏、中央 **20 个功能图标网格**、右下「返回提瓦特」。
  **绝不是二级聊天界面** ⇒ 误判确认。
- secondary-full.png：**私聊界面** —— 左侧会话列表（Xterfusion/.ME雪/最好的京子/…）、
  中间对话气泡与时间戳、底部「点击输入 + 发送」。
  它与主菜单的结构差异**肉眼极大**（会话列表 vs 图标网格），但**当前判据看不到这个差异**。

### 24.5 结论与限制（**不猜、不擅自加模板**）

> **secondary:chat 目前等价于「圆形返回按钮命中」，不排他，不可声明为可靠状态。**

要修必须加**第二个独立判据**（例如识别"左侧会话列表"这一结构）。
但我**只有 1 张真二级帧**，无法像 §23 那样在"目标帧与非目标帧"上验证候选特征的特异性 ——
本目标已经吃过两次"看起来很对的模板其实不特异"的亏（world-wish、secondary-back，
以及 §22 的蓝标记）。**所以我不在这一轮凭空造模板。**

**立判据的前提条件（写成纪律）**：
新模板必须同时在**它的目标帧**和**证据库里全部非目标完整帧**上测过，
前者过阈值、后者**全不过**，才允许进入判定。只测目标帧不算验证。

### 24.6 副产品：主菜单截图 = **整张状态机的入口地图**

wonderland-menu.png 本身就是最有价值的一张图：它把子界面**逐项列出**。
按图记录（列为以后查迁移用）：

| 区域 | 入口 |
|---|---|
| 第一行 | 千星商城、人气奇域、奇域收藏、我的奇域 |
| 第二行 | 大厅、装扮搭配、好友、队伍 |
| 第三行 | 地图、更换角色、颂愿、纪游 |
| 第四行 | 游玩指南、特典活动、官方社区、版本热点 |
| 第五行 | 反馈、云·原神、周边商城、问卷 |
| 右下 | 返回提瓦特 |

**含义**：主菜单是这张状态机的**枢纽节点**，上表每一行都是一条
「主菜单 → 子界面」的边，**触发条件 = 点击对应图标**。
其中**已有证据帧**的：大厅（hall-page / full-hall / hall-chat-*）、
奇域收藏（wonderland-collection）、纪游（journey-page）、颂愿（wish-page / wish-screen）、
地图（map-screen / map-via-M）、好友（friend-list 等）。
**没有证据帧**的：千星商城、人气奇域、我的奇域、装扮搭配、队伍、
更换角色、游玩指南、特典活动、官方社区、版本热点、反馈、云·原神、周边商城、问卷。

**这些边是"入口已由截图确认、判别特征未建立"**，不是"已支持"。
要补齐只需在主菜单里逐个点开截帧——这一步需要在游戏里操作，**不做则如实留白**。

### 24.7 复现命令

```powershell
$h=@{"X-Miliastra-Token"="123456"}
$d="D:/Code/Miliastra-Wonderland-Music/docs/ui-state-evidence/"
(Invoke-WebRequest ("http://127.0.0.1:18888/debug/ui-catalogue?image=" + $d + "wonderland-menu.png") -Headers $h -UseBasicParsing).Content | ConvertFrom-Json |
  Select-Object -ExpandProperty probes | Where-Object { $_.name -like "secondary*" } | Format-Table name,bestScore,correlation,matched
```


## 25. 端到端验证：**应用自己的 ui_state 断言真的会失败**（2026-10-03）

### 25.1 为什么要做这一条

目标 (3) 要求状态机「落成应用内**可查询、可断言**的模块」。
前 24 节验的都是**我的探针**与**分类器本体**，**没有验过应用自己的断言词表**。
而一个**不会失败的断言等于没有断言**——所以本轮同时跑一条应通过的、一条故意写错的。

### 25.2 做法（全部走应用自己的 HTTP 接口）

1. 选一个**已存在**的任务：b5-013218-wait0（version 1，命令是 wait 0ms，**对游戏零副作用**）。
2. 用 mode=live 启动运行（live 需要 UI runtime 已启动 + 调试模式开启，二者当时都满足）。
3. 用 /debug/run/all 把 live 运行推进到终态。
4. 用 /debug/run/status 与 /debug/run/evidence 读结果。

### 25.3 结果

| 断言 | 期望 | 运行终态 | 说明 |
|---|---|---|---|
| A | expected=**primary** | **passed** | 与当时实时分类一致（kind=primary） |
| B | expected=**overworld** | **failed** | 错误原文：**UI 状态不匹配: expected=overworld, actual=primary (frame 5352)** |

**断言 B 的失败是本轮最有价值的结果**：它证明这条断言**真的在比对**，
且**把实际状态与帧号一并报出**（frame 5352）——判别依据因此可回溯到具体一帧。

断言 A 的运行记录里 index=0 带 1 条断言、其余 4 条为 0 条断言，
说明 live 运行把断言挂在**第 0 个**命令步上，后续步骤只跑命令。如实记录，不过度解读。

**结论：目标 (3) 的「可断言」在应用自己的接口上成立，且这条断言不是空断言。**
这与单元测试 ui_state_assertion_is_adapter_backed_and_frame_free（用假适配器）互为补充：
本节是**真设备、真运行、真断言**。



### 25.4 我在本节犯的两个错（必须记下）

1. **我把调试模式从开着改成了关着。**
   我默认它原本是关的，脚本结尾就「恢复」成 0 —— 实际它原本是**开**的。
   已**立即恢复为 true** 并核对通过。
   **纪律**：需要临时改动共享状态时，**先读出当前值，结束时恢复「那个读到的值」**，
   而不是恢复成「我以为的默认值」。
2. **我试图新建沙盒任务，撞到容量上限。**
   /debug/tasks 返回「沙盒存储容量已满」：**任务数已经是 256/256**。
   **本目标早就有「不得在 256/256 时新建任务」的约束，我违反了它。**
   改为复用已有任务后完成验证，**最终任务数仍是 256，没有新建**。

**本轮对共享状态的改动，如实列出**：

| 项目 | 结果 |
|---|---|
| 调试模式 | 先被我误置为 0，随后恢复为 1（= 原值）。**现在为 1** |
| 运行记录 | 新增 2 条 live 运行（runs 64 满额轮转，未越界） |
| 沙盒任务 | **0 新增**（仍为 256） |
| 游戏内输入 | **0**（任务命令只有 wait 0ms） |

### 25.5 为什么不把这条验证放进自动回归脚本

上面的流程会**新建运行记录**、并要求**调试模式开启**——都是**有副作用的**。
把它塞进 verify-classification.ps1 会让「跑一次回归」变成「改一次运行态」，这不可接受。
所以本节只提供**命令级复现步骤**，由人决定何时执行（步骤见 MANIFEST.md 的
「有副作用的验证」一节）。


### 23.7 **更正**（2026-10-03，由机器可读模型的自动审计发现）

第 31 节新建了 docs/ui-state-transitions.json + verify-model.ps1，把本文的结论编码成**可执行断言**。
第一次跑审计就**打出了本节的一处过度断言**：

```
[FAIL] overworld / world-wish / overworld-hub.png  实际 matched=True score=0.958  期望 matched=False
```

**我在 §23.3 写的「真野外帧根本不命中该模板」是错的。** 实测四帧**全部命中**：

| 帧 | score | correlation | 命中位置 | regionStd |
|---|---|---|---|---|
| overworld-hub.png（真野外） | 0.9580 | **0.821** | (1487,13) | 27.67 |
| hall-page.png（大厅页） | 0.9679 | 0.431 | (1715,13) | 81.83 |
| wish-screen.png | 0.9643 | 0.127 | (1467,0) | 28.98 |
| team-finder.png | 0.9292 | 0.095 | (1809,40) | 38.40 |

模板本体也已直接查看：world-wish.png 是 **70×70 的四角星/星芒**图标
（放大 8 倍查看确认），与 §23.3 的视觉判断一致。

**更正后的准确结论**：

- 错的：「真野外帧不命中，命中方向与事实相反」
- 对的：**该模板在四帧上全部过阈值，包括它本该识别的真野外帧**。
  它不是「反的」，而是**完全没有排他性** —— 四帧 score 都在 0.93~0.97，
  真帧并不占优（0.958 < 大厅页 0.9679）。

**这使 §23 的核心结论更强而不是更弱**：一个在目标帧与非目标帧上**都**过阈值的判据，
无论阈值怎么定都无法判别 overworld。**overworld 不可支持的结论不变。**

**教训**：我在 §23.3 用「宽上下文里没看到那个四角星」就推断「模板不命中」，
把**肉眼观察**当成了**模板匹配结果**。这两者必须分开陈述——
本次是模型审计在**一轮之内**把这类过度断言抓了出来。


## 26. 状态机落成**机器可读模型**，并第一次运行就抓到我自己的过度断言（2026-10-03）

### 26.1 动机：目标 (2) 要求「可遍历、可回放」

直到第 30 节，迁移模型只存在于**散文**里。散文有两个老问题：
读的人要看懂，改的人容易忘；而且**文档与现实漂移时没有任何东西会报错**——
本目标已经吃过这个亏（§9.2 的 world-wish 判据错了很久才被发现）。

所以本轮把它落成两份东西：

- docs/ui-state-transitions.json —— **机器可读**的状态机（可遍历）
- docs/ui-state-evidence/verify-model.ps1 —— **自动审计**（可回放）

### 26.2 模型结构

| 字段 | 内容 |
|---|---|
| states | 5 个 UiStateKind，每个带 criteria（探针名 + 证据帧 + 期望命中 + 阈值 + 说明）与 supported |
| knownFalsePositives | **4 条已确认缺陷**，每条记录"实际判定 / 真值 / 为什么 / 出处" |
| edges | 9 条边，每条带 from/to/触发条件/证据等级/证据帧/源码出处 |
| menuEntrypoints | 主菜单 20 个入口，每个带 evidenceFrames 与 criterionEstablished |
| externalNodes | anywhere / chat_link / big_map / hall_list / main_menu |

关键设计：**未支持的东西不是"没写"，而是显式写成 supported=false 或
criterionEstablished=false**，这样"还差多少"是可数的，而不是靠人记。

### 26.3 审计器检查四件事

1. **判据复现**：每条 criteria 在它声明的证据帧上按预期命中；并把应用响应里的
   threshold 与模型记录的 threshold 对比（**证明没有为了凑结果偷偷改过阈值**）。
2. **证据帧存在性**：模型引用的每张帧文件都真实存在。
3. **边端点可解析**：每条边的 from/to 都能在 states / menuEntrypoints / externalNodes 里找到。
4. **已知缺陷仍按记录复现**：4 条 defect 逐条重测，行为一旦变化立即 FAIL
   （提醒"缺陷可能被修了，请更新文档"，而不是让缺陷悄悄变成预期行为）。

### 26.4 **第一次运行就抓到我一轮之前的过度断言**

```
[FAIL] overworld / world-wish / overworld-hub.png  实际 matched=True score=0.958  期望 matched=False
```

我在 §23.3 写过「真野外帧**根本不命中** world-wish」。实测**四帧全部命中**
（0.9580 / 0.9679 / 0.9643 / 0.9292）。模板本体也已直接放大查看：
world-wish.png 是 **70×70 的四角星/星芒**图标。

**更正见 §23.7。** 核心结论不变且更强：判据的问题不是"方向反了"，
而是**完全没有排他性**（真帧 0.958 并不占优，大厅页 0.9679 更高），
**任何阈值都判不出 overworld**。

**根因**：我把**肉眼观察**（宽上下文截图里没看到那个四角星）当成了**模板匹配结果**。
这两者必须分开陈述——这正是"把结论编码成可执行断言"的价值：
**一轮之内**就把这类过度断言打了出来。

### 26.5 审计器抓到的第二个问题：我建模时用错了字段

```
[FAIL] wonderland-collection.png 的误判行为变了: matched=False 期望=True
```

不是行为变了，是**模型记录错了字段**：该缺陷出现在**彩色**路径
（classifierMatched=true，27 原始命中/去重后 8），
而我记的 matched 是**灰度**路径（0.895 < 0.9 ⇒ false）。

修法：模型每条缺陷增加 field 字段指定比对哪个字段（默认 matched）。
这个坑值得记：**同一帧上"探针的 matched"与"分类器的 classifierMatched"是两个不同的谓词**，
§21 已经吃过一次同样的亏，这次是它在模型层的复现。

### 26.6 审计结果与**如实的支持度统计**

```
模型审计: PASS=24 FAIL=0
状态: 已支持 2 / 未支持 3
主菜单入口: 判据已建立 1 / 未建立 19
边总数: 9
```

**这就是当前状态机的真实完成度**，不做任何美化：

- 5 个 UiStateKind 里真正可支持的是 **primary 与 unknown**；
  secondary 不排他、game_gate 从未稳定、overworld 判据证否。
- 主菜单 20 个入口里，只有**好友面板**有可复现判据（friend-panel 模板 1.000），
  但它**尚未接入** UiStateKind 判定（integrated=false ⇒ 分类器对好友面板返回 unknown）。
  其余 **19 个都没有判别特征**。
- 9 条边里 7 条 verified、2 条 partial。

### 26.7 复现命令

```powershell
pwsh -File docs/ui-state-evidence/verify-model.ps1      # 模型审计
pwsh -File docs/ui-state-evidence/verify-classification.ps1  # 分类器判定
pwsh -File docs/ui-state-evidence/verify.ps1                 # 目录探针分数
```

改了 ui-state-transitions.md 或 JSON，请同步改另一份，否则审计会失败。


## 27. 扩大证据语料：**secondary-back 的误判比真命中还多**，并第一次得到一个**特异判据**（2026-10-03）

### 27.1 扩大语料：dist 下还有 53 张真机完整帧

第 31 节我只搜了「视频」，漏了 dist/sandbox-run 里历史各轮留下的真机截图。
本轮按尺寸筛出 **53 张 1920×1080 完整帧**，逐张过分类器。这批语料**不在** docs/ui-state-evidence 里，
所以此前所有「全库扫描」的结论其实只覆盖了较小的集合——这一点先如实更正。

### 27.2 复核：名字像结算页的 settled-3.png 其实是主菜单

settled-3.png 被判 secondary:chat，名字像「结算页」。**整帧复核后确认它也是千星奇域主菜单**
（甲档案 + 20 图标网格 + 返回提瓦特），**不是结算页**。
因此它是 secondary-back 的**第三个误判**，也说明**不能靠文件名推断界面**。

### 27.3 secondary-back 在 8 帧上的表现（5 假 3 真）

| 帧 | 整帧复核真值 | secondary-back | secondary-hall |
|---|---|---|---|
| secondary-full.png | 真·私聊 | 0.995 过 | 0.537 不过 |
| round78-live.png | 真·大厅型二级 | 0.998 过 | **1.000 过** |
| sec-fixture.jpg | 真·私聊 | 0.995 过 | 0.537 不过 |
| wonderland-menu.png | 假·主菜单 | 0.978 过 | 0.619 不过 |
| wonderland-menu-escape.png | 假·主菜单 | 0.978 过 | 0.619 不过 |
| settled-3.png | 假·主菜单 | 0.978 过 | 0.619 不过 |
| final.png | 假·主菜单 | 0.978 过 | 0.619 不过 |
| restored.png | 假·主菜单 | 0.978 过 | 0.618 不过 |

**secondary-back 8 帧全中**：真 3、假 **5** —— **误判多于真命中**，
比第 24 节（3 帧里 2 假）的结论更强：**它单独使用时没有任何可用性。**

### 27.4 第一次得到一个**特异判据**（阈值一字未改）

上表第三列是本轮的新结果：**secondary-hall 只在 1 帧上命中，且正是大厅型真二级（1.000）**，
**5 个主菜单帧全部不过**（0.618~0.619，远离 0.9）。

所以 **secondary-back AND secondary-hall** 这个**合取**判据：

- 正样本 1 命中（round78-live.png）
- 负样本 5 全不命中（5 个主菜单帧）
- **0 误报**
- 两个阈值都是应用配置里的原值，**没有为任何结果改动过**

这是本目标里**第一个通过「目标帧过、全部非目标帧不过」检验**的判据，
符合第 24 节我给自己定的纪律。

**限制（必须一起说）**：

- 正样本**只有 1 帧**。特异性对 5 个负样本验证过，但**样本量偏小**，
  不能声称「已经在全库证明」。要坐实需要更多大厅型二级帧。
- 它**只覆盖大厅型二级**；私聊型二级（secondary-full / sec-fixture）**判不出**，
  在合取判据下会落到 unknown。

### 27.5 私聊型二级：**仍然没有判据**，宁可 Unknown

私聊界面的显著特征是**左侧会话列表**，但**没有任何现有模板**能表达它
（19 个探针里只有 secondary-hall 与二级相关）。
按第 24 节的纪律，**我不凭空造模板**。因此：

> secondary 拆成两个子状态：
> - secondary_hall：**有**可用判据（合取），supported=true（带 n=1 限制）
> - secondary_private：**无**可用判据，supported=false，**显式返回 unknown**

### 27.6 我没有把合取判据接进分类器

合取是**逻辑**改动、不是调阈值，它确实会提升精度（去掉 5 个误报、保住 1 个真命中，
并把 2 个真私聊降级为 unknown）。但它会**改变运行中应用的判定行为**——
这属于设计决策，**我不擅自接线**。

它是一个**已验证的候选**，证据在 §27.3，接线方式明确（secondary 判定时要求两个模板同时命中），
等你决定。**在你决定之前，应用里的 secondary 判据仍是原样，缺陷仍在**（已登记在模型里）。

### 27.7 模型与审计器的同步更新

- 模型新增 secondary.subStates（secondary_hall / secondary_private），
  判据、正负样本、限制都写进 JSON，**可被审计**。
- knownFalsePositives 由 4 条增加到 **7 条**（新增 settled-3 / final / restored）。
- verify-model.ps1 增加**子状态判据审计**，并把子状态与缺陷帧一并纳入**证据帧存在性检查**。
- 新增证据帧已从 dist 复制进 docs/ui-state-evidence（round78-live / settled-3 / final / restored / sec-fixture），
  使模型引用的帧都能被独立找到。

```
模型审计: PASS=34 FAIL=0
```


## 28. 全语料扫描（66 帧）：secondary 的最终账、world-wish 被钉死、以及**又一次自我更正**（2026-10-03）

### 28.1 方法：这次把两个目录**全部**完整帧合起来

第 32 节发现 dist/sandbox-run 里还有历史各轮的截图之后，本轮做了真正的全语料扫描：

- 来源：docs/ui-state-evidence 与 dist/sandbox-run 两个目录
- 过滤：只取 **1920×1080** 完整帧（裁剪图/模板图会被归一化拉伸，结果无意义）
- 去重：按 frameHash 去重（同一帧的多份拷贝只算一次）
- 结果：**66 张互不相同的完整帧**，逐张跑 19 个探针

### 28.2 各判据的命中数（66 帧）

| 探针 | 命中 |
|---|---|
| world-wish | **40** |
| yellow-marker | 26 |
| pink-marker | 22 |
| secondary-back | **11** |
| friend | 6 |
| blue-marker | 2 |
| **secondary-hall** | **1** |

### 28.3 secondary 的最终账：**误判是真命中的 2.7 倍**

secondary-back 的 11 个命中，逐帧**整帧复核**后的真值：

- **真二级 3 帧**：secondary-full.png（私聊）、round78-live.png（大厅型）、sec-fixture.jpg（私聊）
- **假 8 帧，全部是千星奇域主菜单**：wonderland-menu、wonderland-menu-escape、settled-3、final、restored、**modR-5、step-5、step-last**

（后 3 帧是本轮新发现的，整帧复核确认也是主菜单。）

**8 假 : 3 真**。比第 32 节（5:3）更差。**secondary-back 单独使用不可用**，这一点现在有 66 帧语料支撑。

### 28.4 合取判据的特异性：从 5 个负样本提升到 **65 个**

secondary-hall 在 **66 帧里只命中 1 帧**，且正是大厅型真二级（1.000）；
其余 **65 帧全部不过**（主菜单帧稳定在 0.618~0.619，离 0.9 很远）。

所以 **secondary-back AND secondary-hall**：

- 正样本 1 命中
- **负样本 65 全不命中，0 误报**
- 阈值仍是配置原值

**限制不变且必须一起说**：正样本仍只有 **1 帧**，所以只能说
「特异性对 65 个负样本成立」，**不能说「已在全库证明」**。

### 28.5 world-wish 被钉死：**60% 的帧都命中**

40/66。一个在 **60% 的任意画面**上都过阈值的模板，
无论阈值定在哪里都不可能是 overworld 的判据。
第 31 节的更正（它命中的帧比我说得多）在本轮得到全语料确认，**overworld 不可支持**。

### 28.6 **又一次自我更正**：我差点发布一个错的"标记误判率"

灰度探针在 66 帧里有 26 帧命中标记，其中 **10 帧的相关性很低**（corr ≤ 0.36）：
cat-frame-3(-0.04)、journey-page(0.03)、hall-chat-closed(0.14)、
modH-4/modG-7/modG-12/recov-6(0.16)、round78-live2(0.19)、hud-hidden(0.36)、
wonderland-collection(0.004)。

我一度准备写「约 38% 的标记命中是伪匹配」。**幸好按惯例去查了这些帧最终被判成什么**：

| 帧 | 标记 corr | 分类器**实际**判定 |
|---|---|---|
| cat-frame-3.jpg | -0.04 | primary / **friend 模板** |
| hall-chat-closed.jpg | 0.14 | primary / **friend 模板** |
| round78-live2.png | 0.19 | primary / **friend 模板** |
| journey-page.png | 0.03 | **unknown** |
| modH-4 / modG-7 / modG-12 / recov-6 / hud-hidden | 0.16~0.36 | **unknown**（各帧） |
| wonderland-collection.png | 0.004 | **primary:marker** ← **唯一真误判** |

**结论：10 个低相关命中里，只有 1 个真的导致 primary:marker。** 另外 9 个被挡住的原因是：

1. **friend 模板先判**（锚点顺序在标记之前）⇒ 3 帧走了 primary:friend；
2. **分类器用的是彩色判据**，而那些低相关命中来自**灰度探针** ⇒ 彩色不过阈值，落 unknown。

**又是「灰度 vs 彩色是两个谓词」**（第 21、26、31 节已各吃过一次）。
按灰度探针的 matched 去推断分类器行为，就是第 21 节那个错误的重演。

**更正后的准确结论**：在 66 帧语料上，**primary:marker 只有 1 个已确认误判**
（wonderland-collection.png，平坦蓝底）。标记判据远比灰度探针暗示的**稳健**。
这条对目标 (3) 是**正面**证据，但**仍不改变 §22 的定性**：那 1 个误判是真的，
而它是否要用结构前提去堵，切点属设计决策，**我没动代码**。

### 28.7 模型同步

- knownFalsePositives 增至 **10 条**（新增 modR-5 / step-5 / step-last）。
- 新增 corpusSweep 段：记录 66 帧的全部命中数与四条结论，**可被审计**。
- secondary_hall 的 note 更新为「1 真 / 65 负 / 0 误报，正样本仅 1 帧」。

### 28.8 复现命令

```powershell
# 全语料扫描的原始结果已落盘
Get-Content dist/sandbox-run/corpus-sweep.json | ConvertFrom-Json | Measure-Object
pwsh -File docs/ui-state-evidence/verify-model.ps1
```


## 29. 「聊天框开合」终于有判据了，而且开合键就是 Enter（2026-10-03）

### 29.1 以前一直空着的那个状态

迁移文档第 6 节从很早以前就写着：

> 聊天框开合 | **没有**「已关闭」状态；闭框与「开框但无消息」在现有判据下不可区分，也不知道开合键

本轮在全语料里复核 friend 判据的 6 个命中帧时，**意外在帧里直接看到了答案**——
左下角热键栏本身就是「怎么操作」的说明书。

### 29.2 热键栏读出来的东西（cat-frame-2.jpg，放大 3 倍）

```
💬 Enter   |   👥 O   |   🔊 🏛️ Alt+N   |   🎤 N
```

- **💬 Enter** = 按 Enter 开聊天框（这一条就是「开合键」的答案）
- 👥 O = 好友面板（与 moderation.rs 的 O 键一致）
- 🔊 🏛️ Alt+N = 切换语音频道（🏛️ 是千星奇域频道标记）
- 🎤 N = 麦克风（与 hall.rs:272 的 ToggleMicrophoneRoutine 一致）

### 29.3 开/闭的判别特征：**左下热键栏首块有没有 💬 气泡图块**

取 4 帧的同一窄区 (18,995)–(78,1045)，8 倍最近邻放大对照：

| 帧 | 该区内容 | 聊天框 |
|---|---|---|
| hall-chat-closed.jpg | **💬 气泡图块在** | 关 |
| hall-chat-open.jpg | **气泡不在**（该处是聊天消息的橙色四角星标记） | **开**（画面里有 `[天子]:这在干啥?`） |
| cat-frame-2.jpg | **💬 气泡图块在** | 关 |
| final-frame.jpg | **💬 气泡图块在** | 关 |

**结论**：💬 气泡图块 **在 = 关、不在 = 开**，是个**二值视觉特征，不需要任何阈值**。
这直接否证了「闭框与开框不可区分」的旧说法。

**证据来源**：docs/ui-state-evidence/hall-chat-closed.jpg、hall-chat-open.jpg；
dist/sandbox-run/cat-frame-2.jpg、final-frame.jpg。区域与放大倍数都写明了，可独立复现。

**注意**：该区在开框时会**被聊天框内容覆盖**（所以看到的是消息标记）。
这意味着判据应表述为「**该位置出现 💬 气泡图块** ⇒ 关」，而不是「该位置没有消息」，
否则会把开着框但有消息的帧误判成关闭。

### 29.4 附带结论一：friend 判据的 6 个命中**都不是误判**

全语料里 friend 命中 6 帧：cat-frame-2、cat-frame-3、final-frame、hall-chat-closed、probe、round78-live2。
逐帧复核后，**全部都是千星奇域大厅的 HUD**（左下热键栏的同一套 O / N / Enter 提示都在），
只是**聊天框是关闭的**、角色有站有坐。

⇒ friend 不是「检测好友面板」，而是**检测大厅 HUD 里的好友入口**，
所以聊天框关着时它照样命中、**这是正确行为**，不是误判。
这解释了为什么好友面板打开时反而判 unknown（面板盖住了 HUD）。

### 29.5 附带结论二：大厅有**就座**场景（候选的「茶会座位」）

cat-frame-2 / cat-frame-3 / final-frame 三帧拍的是**角色坐在圆形茶座里**、
周围是圆桌与彩色伞的同一处场景；而 hall-chat-closed / hall-chat-open 是同一大厅的**站立**视角。

两组的另一个差别在热键栏：就座组有 **🏛️ 蓝色千星奇域频道图标**，站立组只有普通麦克风图标。

**我把它记为候选，不声称已确认**：

- 有证据的：**就座场景确实存在**，且有 3 张完整帧；它与站立场景在热键栏频道图标上可区分。
- **没有证据的**：这是不是目标里说的「茶会座位」。**需要你确认**，
  因为画面里我只看到圆桌茶座与伞，没有任何写着「茶会」的 UI 文本。

### 29.6 同步更新

- 迁移文档 §6 里「聊天框开合」那一行由 ❌ 未知 改为 ✅ 已验证，并写明判据与开合键。
- 模型 JSON 增加 chat_box 子状态（closed 有判据 / open 有判据），
  以及 friend 判据的特异性结论。
- 新增证据帧（cat-frame-2 / cat-frame-3 / final-frame / probe / round78-live2）复制进
  docs/ui-state-evidence，使模型引用的帧可被独立找到。

### 29.7 我**没有**做的

- **没有**把 💬 判据接进分类器（新模板 + 新规则 = 设计决策）。
- **没有**因为看到就座场景就宣称「茶会座位已还原」。
- **没有**改动任何阈值。


## 30. 把 💬 判据从「人工判据」升级为**脚本数值验证**（2026-10-03）

### 30.1 先试了一条走不通的路（记录一下）

我本来想把 💬 判据做成**应用内沙盒断言**：SandboxAssertion::Template 接受
**内联的 grayscale 字节**，看起来正好合用。

**但走不通**：SandboxFixture 的字段是 rgba: [u8; 4] ——
**一个像素的颜色**，不是任意图像。所以 mode=simulated 只能造纯色帧，
而 Template 断言只对「实时帧」或「纯色 fixture 帧」求值，**没法拿存档帧当输入**。

要接实时帧就必须在游戏里真的处于那个状态，这需要驱动游戏（仍未获你授权）。

### 30.2 走通的路：独立脚本 + **序关系断言**

新增 docs/ui-state-evidence/verify-chatbox.ps1。做法：

1. 从 hall-chat-closed.jpg 的固定区域取出 💬 气泡图块作为模板
   （搜索区 (20,1005) 80×50，模板 (46,1022) 28×24，坐标写死在脚本里）
2. 在 4 张帧的同一搜索区里做**零均值归一化互相关（NCC）**，取最佳分数
3. 断言只有两类，**都不是拟合出来的阈值**：
   - **数学性质**：模板在它自己的来源帧上自匹配必须 ≥ 0.99（同像素必然为 1）
   - **序关系**：关闭组的分数必须**严格大于**打开组

### 30.3 实测结果

| 帧 | 最佳 NCC | 真值 |
|---|---|---|
| hall-chat-closed.jpg | **1.0000** | 关（自匹配） |
| cat-frame-2.jpg | **0.9814** | 关 |
| final-frame.jpg | **0.9769** | 关 |
| hall-chat-open.jpg | **0.3647** | 开 |

**关闭组 0.977~1.000，打开组 0.365** —— 分离极其干净，而且这个差距是**测出来的，不是定出来的**。

旁证（不断言，仅留档，因为这些帧没有聊天框开合真值）：
wonderland-menu 0.5545、hall-page 0.4898、journey-page 0.4579、
overworld-hub 0.3739、secondary-full 0.3609、map-screen 0.3241。
**全部 ≤ 0.55，与关闭组完全不重叠**，说明这个模板不会在别的大厅界面上误命中。

### 30.4 这条判据现在的成色（不夸大）

**成立的部分**：

- 「该区域出现 💬 气泡图块 ⇒ 聊天框关」有 3 张正样本（0.977/0.981/1.000）。
- 「开框时该处没有气泡」有 1 张正样本（0.365），且该帧画面里能看到聊天消息。
- 6 张情境帧全部远低于关闭组 ⇒ 没有观察到误命中的界面。
- 断言不含拟合阈值，脚本可独立复现（pwsh -File docs/ui-state-evidence/verify-chatbox.ps1）。

**不成立 / 未验证的部分（必须一起说）**：

- **「开框但没有任何消息」这一种情况没有样本**。我手上所有开框帧都带有消息。
  逻辑上气泡同样不在 ⇒ 仍判「开」，但**这一点没有实测**。
- 判据**仍未接入分类器**（新规则属于设计决策），所以应用的 ui_state 输出里
  **没有**聊天框开合这一维，它只在模型与脚本里成立。
- 模板区在开框时会被聊天内容覆盖，所以判据**只能表述为「气泡在图 ⇒ 关」**，
  不能反过来说「该处没内容 ⇒ 关」。

### 30.5 同步更新

- verify-chatbox.ps1 已加入常规回归（与 verify.ps1 / verify-classification.ps1 / verify-model.ps1 并列）。
- 模型 JSON 里 chat_box_closed / chat_box_open 的判据由 probeKind=visual 改为
  probeKind=script，并记录 verifiedBy、实测分数与上述限制。
- 模型审计器增加「脚本判据」一档：列出脚本名与实测分数，
  并明确说明**判别由脚本负责、模型审计不替代它**。

## 31. 底栏普查：各界面各有各的底栏，并新识别三个面板（2026-10-03）

### 31.1 起因

第 29 节发现大厅左下热键栏本身就写着操作提示（💬 Enter / 👥 O / 🔊 🏛️ Alt+N / 🎤 N）。
于是本轮把**同一块底栏区域 (0,975)–(780,1080)** 从各个界面裁出来对比，
看它是不是一个**通用的界面判别特征**。

### 31.2 实测：底栏确实是屏幕各异的

| 帧 | 底栏（y 975~1080）看到的东西 | 说明 |
|---|---|---|
| wonderland-menu.png | 20 图标网格的最后一行（反馈/云·原神/周边商城/问卷）+ 返回提瓦特 | 主菜单**盖住**了底栏 |
| secondary-full.png | 「**添加会话对象**」按钮 + 列表图标 + 「**点击输入**」输入框 | 私聊输入条 |
| wonderland-collection.png | 🗑 删除 + ▽ 筛选 + 「**最近收藏**」下拉 | 收藏页工具条 |
| map-screen.png | ⚙ 齿轮 | 地图设置入口 |
| journey-page.png | 「**装扮预览**」按钮 | 纪游页内的预览按钮 |
| wish-page.png | 两个圆形页签 + 「**风尚图册**」 | 颂愿页内按钮 |
| friend-list.png | 「**黑名单管理**」按钮 | 好友面板底部 |
| team-finder.png | **大厅热键栏原样**（💬 Enter / 👥 O / 🏛️ Alt+N / 🎤 N） | 面板叠在大厅上，底栏没被盖住 |

**结论**：底栏文字/图标是**界面特有的**，可作为一种**布局特征**；
但它会被面板遮挡，所以只能当**辅助**特征，不能当唯一判据。

### 31.3 三处"文件名疑似不符"——**逐一核实后全部是对的**

看底栏时我一度怀疑三张帧的文件名写错了，直接去看整帧上半部分复核：

| 帧 | 我的怀疑 | **整帧核实结果** |
|---|---|---|
| journey-page.png | 底栏写着「装扮预览」，像是装扮页 | 标题是「**诸界纪游**」，纪游等级 8、本期时间、购买等级 ⇒ **确实是纪游页**，「装扮预览」只是页内按钮。**文件名正确** |
| wish-page.png | 底栏写着「风尚图册」，不像是颂愿 | 是「**喧哗霄练**」祈愿横幅（剩余时间 32 天、「速献10次 ×1600」）⇒ **确实是颂愿/祈愿页**。**文件名正确** |
| team-finder.png | 底栏是大厅热键栏，像是大厅不是组队 | 右侧叠着「**寻找朋友**」面板（推荐玩家/好友两页签、玩家行、「组队招募」）⇒ **确实是组队面板**。**文件名正确** |

**这一节的教训**：底栏文本**属于页内元素**（按钮、工具条），**不能用来反推页面身份**。
我按纪律先核实再动手，因此没有把三张正确的文件改错。

### 31.4 新识别（或首次定性）的三个面板

| 面板 | 证据帧 | 可用作判据的特征 | 证据等级 |
|---|---|---|---|
| 诸界纪游 | journey-page.png | 标题文本「诸界纪游」；左侧图标侧栏；底部「装扮预览」 | ⚠️ 文本判据需 OCR，离线测不了 |
| 颂愿/祈愿（喧哗霄练） | wish-page.png | 标题「喧哗霄练」；底部「风尚图册」；「速献10次」 | ⚠️ 同上 |
| 组队/寻找朋友 | team-finder.png | 右侧面板标题「寻找朋友」；「推荐玩家」「好友」两页签；「组队招募」按钮；底栏仍是大厅热键栏 | ⚠️ 同上 |

### 31.5 OCR 判据的**离线**限制（与 game_gate 同一个坑）

这三个面板最自然的判据是**标题文本**，而文本要靠 OCR。
但本目标的离线通道（/debug/ui-state?image= 与 /debug/ui-catalogue?image=）
**都跑不了 OCR**：前者的 game_entry 检测在离线路径下被显式关掉（第 31 节之前已确认），
后者的 19 个探针全是模板。

所以：

- 我**不**把它们写成"已支持"；表格里一律标 ⚠️。
- 它们的模板类旁证（底栏按钮）我也**不**当作判据，因为 §31.2 已说明底栏会被遮挡。
- 要真正建立这三条判据，需要**实时路径**（游戏真的在那个界面）或者 OCR 能力接入离线通道。
  两者都需要你定。

### 31.6 模型同步

- menuEntrypoints 中 m_journey / m_wish / m_team 的 evidenceFrames 保留并补充实测说明
  （经整帧核实为对应页面，不是同名误挂）。
- 新增 panelCandidates 段：记录这三个面板、证据帧、候选特征与"离线测不了 OCR"的限制。

## 32. 联网检索的产出与**边界**：结算没有通用模板、茶会可能根本不是面板（2026-10-03）

目标第 5 条允许联网检索来确定「界面怎么进入」。本节记录本轮检索的**实际产出与查不到的部分**，
两者都要写清楚，避免以后overclaim。

### 32.1 结算：**是创作者配置的节点，结算文本每关自定义 ⇒ 不存在通用模板**

来源（官方 UGC 教程知识库的镜像）：
[2.11 关卡结算——游戏结束，让所有玩家进行游玩结算](https://raw.githubusercontent.com/1475505/Miliastra-knowledge/main/official/tutorial/mhv3ufrgotny_2.11%E5%85%B3%E5%8D%A1%E7%BB%93%E7%AE%97%E2%80%94%E2%80%94%E6%B8%B8%E6%88%8F%E7%BB%93%E6%9D%9F%EF%BC%8C%E8%AE%A9%E6%89%80%E6%9C%89%E7%8E%A9%E5%AE%B6%E8%BF%9B%E8%A1%8C%E6%B8%B8%E7%8E%A9%E7%BB%93%E7%AE%97.md)

原文要点（我逐句读过）：

- 「**关卡结算：** 整体游戏流程的最后一步……并根据游玩结果获得胜利、失败等结算结果」
- 「关卡结算是一个**预封装的节点**，可根据实际结算操作、结算条件灵活配置使用」
- 「【执行节点】：使用【结算关卡】进行游戏结算 / **结算关卡后会立即结束游戏**」
- 「演示中的**文本显示**，需达到特定奇匠等级后解锁【自定义文本外显】功能」——即**结算文字是关卡作者自定义的**

**对本目标的直接结论**：

> **不存在一个通用的「结算页模板」。** 结算界面的文字来自每个关卡作者的配置，
> 不同奇域长得不一样。所以「结算」**不能**用模板/OCR 做全局判据；
> 能做的最多只能是**关卡无关的布局特征**（例如结算面板的外框/背景/按钮位置），
> 而我现在**没有任何结算帧**，连布局特征也无从建立。

因此我**不**把结算登记为可支持状态；它留在"无判据、返回 unknown"一栏。

### 32.2 茶会：搜索结果指向的是**社区活动**，很可能不是游戏内面板

搜到的相关条目：

- [【原神空中茶话会】第十四期](https://www.bilibili.com/video/BV1HdvQBzEiu/) 与
  [第四十一期](https://www.bilibili.com/video/BV18KwVzCExP/)
- [千星 空中茶话会规则简述](https://www.miyoushe.com/bh3/article/72088008)
- [Tutorial/Sandrone's Portable Tea Table](https://genshin-impact.fandom.com/wiki/Tutorial/Sandrone%27s_Portable_Tea_Table)

**「空中茶话会」带着期号**（第十四期、第四十一期），这是**玩家自办的活动系列名**，
不是游戏里某个固定的界面。据我检索到的材料，**没有任何来源把它描述成一个可打开的 UI 面板**。

**这修正了我第 29 节的一个假设**：我在那里把「角色坐在圆形茶座里」的场景登记成
「茶会座位」的**候选**。现在看：

- 那个**就座场景本身是真实存在的**（3 张完整帧，§29.5 已记录）；
- 但把它**叫作「茶会座位」是没有依据的**，目标里提到的「茶会」更可能指社区活动；
- **我保留候选登记，但同时写明这一点**，等你在游戏里确认那处场景到底是什么。

### 32.3 大厅确实新增了乐器演奏（与就座场景可能相关）

[《原神》千星速递 | 「大厅」可以演奏乐器啦！](https://news.17173.com/content/06302026/211815333.shtml)
及其[同源报道](https://www.sina.cn/news/detail/5315477280918667.html)的摘要写着：

> 《原神》「月之八」版本更新！**千星奇域大厅新增乐器演奏功能**，通用称呼可使用**小道具**奏响美妙音乐。

**可用信息**：大厅的乐器演奏是通过**小道具**触发的（不是菜单项）⇒
如果真的存在与演奏相关的界面，它的触发物是**小道具**，而不是某个主菜单图标。
**但**：报道只说"可演奏"，**没有**描述任何新面板，也没有描述就座/茶会 UI ⇒
**不能**据此说就座场景就是演奏场景。

### 32.4 检索查不到的东西（边界，必须写明）

| 想确认的事 | 检索结果 |
|---|---|
| 点歌面板的玩家侧入口 | **查不到**。而本地代码早已确认点歌是**聊天命令 @点歌**（application.rs:1925），不是面板 |
| 茶会座位的入口 | **查不到**（见 §32.2，很可能不是面板） |
| 结算页的进入方式 | 属于**局内流程**（结算节点触发即结束游戏），**不是"进入"某个界面** |
| 房间邀请面板 | 检索只给出玩家自发的组队帖（如 [UGC 组队任务互动帖](https://esa.zlb.ink/t/topic/34212/4)），**没有官方界面说明** |
| 19 个主菜单入口的玩家侧说明 | **查不到**：找到的知识库（[1475505/Miliastra-knowledge](https://github.com/1475505/Miliastra-knowledge)）是**创作者侧编辑器文档**（控件/节点图/地形编辑），**不含玩家侧 UI 走查** |

**这就是检索的边界**：千星奇域的公开资料大量集中在**创作者侧**（怎么做图），
**玩家侧界面**的权威说明要么在游戏内，要么根本没有 ⇒
**补那 19 个入口只能靠游戏本身**（仍需你授权）。

### 32.5 本节对目标的实际贡献

1. 把「结算」从"待补的模板"明确改判为「**本质上没有通用模板**」——这省掉了以后徒劳的找模板工作。
2. 修正了「茶会 = 面板」这个未经验证的假设，并保留候选登记而不下结论。
3. 记录了大厅乐器演奏是**小道具**触发（一条新的、有来源的玩法事实）。
4. **划清了检索的能力边界**，避免把"查不到"伪装成"没有"或把猜测写成结论。

## 33. 19 个模板探针的**特异性普查**：好友面板判据成立，邀请判据不成立（2026-10-03）

### 33.1 做法

应用自带 19 个模板探针，但我此前只验过其中少数几个。
本轮把它们**整体**过一遍：先在一组有代表性的帧上看谁命中谁不命中，
再把可疑的探针拉到**全 66 帧语料**上数命中数。

探针名单（来自 /debug/ui-catalogue）：
blue-marker、yellow-marker、pink-marker、friend、secondary-back、secondary-hall、
world-wish、invite-view-star、invite-goto-hall、invite-enter-hall、friend-panel、
friend-search-panel、friend-more-settings、friend-block-chat、friend-blacklist、
friend-confirm、wonderland-confirm、paimon-menu、wonderland-map-star。

### 33.2 先说结论：**只有两个探针是特异的**

全 66 帧语料上的命中数：

| 探针 | 命中 | 特异？ |
|---|---|---|
| **friend-panel** | **3** | ✅ **是**，且 3 帧**全部**经整帧复核为好友列表面板 |
| **friend-search-panel** | **10** | ⚠️ 其中 2 帧视觉复核为「添加好友」面板，其余 7 帧来自同一批测试（很可能同一面板），**未逐帧复核** |
| invite-view-star | 2 | ❌ 命中好友详情卡 ⇒ 不特异 |
| invite-goto-hall | 6 | ❌ 命中详情卡/颂愿页/收藏页 ⇒ 不特异 |
| secondary-hall | 1 / 66 | ✅（但正样本只有 1，见 §28） |
| world-wish | 40 / 66 | ❌ 毫无判别力 |
| wonderland-map-star | 11 / 20（抽样子集） | ❌ 常驻 HUD 星标 |
| paimon-menu | 4 / 20（抽样子集） | ❌ 常驻 HUD 派蒙头像 |
| secondary-back | 11 / 66 | ❌ 误判多于真命中 |
| friend-block-chat / friend-blacklist / friend-more-settings | 7 / 7 / 5（抽样子集） | ❌ 常驻元素 |

### 33.3 好友面板：**本目标第一个判据完全成立的菜单入口**

friend-panel 在 66 帧里命中 3 帧，**全部**经整帧复核：

| 帧 | 整帧复核内容 | 分数 |
|---|---|---|
| friend-list.png | 好友/我的好友，完整好友列表 | 1.000 |
| friend-panel-via-O.png | 同上（用 O 键打开） | 1.000 |
| mod-4.png | 同上（早期测试留下的帧） | 1.000 |

**3 真 / 0 假**，且分数都是 1.000（模板与画面完全一致）。
按第 24 节定的纪律（目标帧过、全部非目标帧不过），**这条判据成立** ✓

**但必须同时说清**：

- 它**没有接入 UiStateKind 判定**（integrated=false）。分类器对好友面板仍返回 unknown，
  因为这条规则要新增到状态机里，属设计决策。
- friend-search-panel 的 10 个命中里我只逐帧复核了 2 个（modE-2、modS2-6，都是「添加好友」面板）。

### 33.4 邀请流程的判据**不成立**（新登记的缺陷）

| 探针 | 误命中帧 | 真值 | 分数 |
|---|---|---|---|
| invite-view-star | friend-detail-actions.png | 好友更多设置 | 0.943 |
| invite-view-star | looknow.png | 好友详情卡 | 0.943 |
| invite-goto-hall | friend-detail-actions.png | 好友更多设置 | 0.907 |
| invite-goto-hall | looknow.png | 好友详情卡 | 0.907 |
| invite-goto-hall | wish-page.png | 颂愿页 | 0.912 |
| invite-goto-hall | wonderland-collection.png | 收藏页 | 0.908 |

**结论**：这两个模板抓的是**好友相关面板上共同存在的按钮**（详情卡与更多设置面板都有它们），
所以**不能用来判定邀请流程到了哪一步**。
这解释了为什么邀请流程的迁移边（迁移文档 §5）此前一直只有**源码依据**、没有帧证据——
**它的中间面板判据本身就不可靠**。

我**没有**改这些模板（属设计决策），只把误命中登记进模型。

### 33.5 顺带确认的两条旧结论

- **paimon-menu 与 wonderland-map-star 是常驻 HUD**（分别命中 4/20 与 11/20 张互不相同的界面，
  包括大厅、主菜单、收藏、颂愿、组队、好友）⇒ 印证了第 6.2 节「右上图标栏、左上派蒙头像不能判别面板」的说法。
- **map-screen.png 在 19 个探针里一个都不命中**，是彻底 unknown 的一屏。

### 33.6 模型同步

- m_friend 的 criterionEstablished 改为 **true**，并附 3 帧证据与"未接入分类器"的说明。
- 新增面板登记：好友搜索/添加好友（friend-search-panel，10 命中，其中 2 帧已复核）。
- knownFalsePositives 增加 invite-view-star / invite-goto-hall 在 friend-detail-actions.png、
  looknow.png 等帧上的误命中。
- 新增证据帧复制进 docs/ui-state-evidence：mod-4.png、modE-2.png、modS2-6.png、looknow.png。

## 34. 一批从未看过的帧：mod* 系列全部识别，好友搜索判据成立（2026-10-03）

### 34.1 这批帧是什么

dist/sandbox-run 里有 20 张以 mod 开头的帧（mod-1 / mod-4 / modC-4 / modClear-2 / modD-2 /
modD-5 / modE-2 / modF-11 / modF-14 / modG-7 / modG-12 / modH-4 / modI-2 / modJ-2 / modR-2 /
modR-5 / modRestore / modS2-6 / modS2-8 / recov-3），
是早期审核流程测试留下的截图。**此前没有任何一节把它们逐张识别过**。本轮做了联表识别。

### 34.2 识别结果

| 类别 | 帧 | 说明 |
|---|---|---|
| 大厅 | mod-1、modR-2、modRestore | 站立、聊天框开着，底栏是大厅热键栏 |
| **好友/我的好友** | **mod-4** | 完整好友列表 + 互动 + 黑名单管理 |
| **好友/添加好友** | **modC-4、modClear-2、modD-2、modD-5、modE-2、modG-7、modG-12、modH-4、modS2-6、modS2-8** | 「暂无好友申请」「好友申请 0/50」 |
| **好友详情卡** | **modI-2、modJ-2、recov-3、looknow** | 昵称待定、成就进度；modJ-2 还叠着「屏蔽聊天/加入黑名单/举报」菜单 |
| **角色详情卡** | **modF-11、modF-14** | 大角色立绘 + 昵称/称号页签 |
| 千星奇域主菜单 | modR-5 | 又一次确认 |

### 34.3 好友搜索判据：**成立**

friend-search-panel 全语料命中 **10 帧**：friend-search.png 与 9 张 mod 帧。
本轮把这 9 张都看过了，**全部是「好友/添加好友」面板**（分数 0.951~1.000）。

> **9 真 / 0 假（第 10 帧 recov-6 未逐帧复核）⇒ 判据成立。**

这是本目标里**第二个**判据成立的菜单入口（第一个是 §33 的好友列表面板 friend-panel）。

### 34.4 好友详情卡与角色详情卡：**没有判据**（如实登记）

在 4 张详情卡帧上测了 5 个审核相关探针：

| 探针 | modI-2 | modJ-2 | recov-3 | looknow |
|---|---|---|---|---|
| friend-more-settings | 0.924 | 0.924 | 0.924 | 0.924 |
| invite-view-star | — | 0.943 | — | 0.943 |
| friend-block-chat | — | — | — | — |
| friend-blacklist | — | — | — | — |
| friend-confirm | — | — | — | — |

- **friend-more-settings 在四张上都命中同一个固定分数（0.924）** ⇒ 它抓的是详情卡上**共有的某个按钮**
  （很可能是 ⋮ 更多按钮），**不是"更多设置面板"**。**又一个不特异的模板。**
- **friend-block-chat / friend-blacklist 在这里一个都不命中** ⇒ 它们此前在**大厅聊天帧**上命中过
  （hall-chat-open / full-hall 0.93~0.944），**同样不特异**。
- modF-11 / modF-14（角色详情卡）**没有任何探针命中** ⇒ 角色详情卡**没有判据**。

**结论**：这三类界面（好友详情卡、角色详情卡、以及那些审核子面板）目前**只能返回 unknown**，
我**不**给它们编判据。

### 34.5 记一个环境死路：LockBits + Marshal.Copy 会崩

为了给主菜单找判据，我想用 LockBits + Marshal.Copy 加速取像素（GetPixel 太慢）。
结果：

```
Fatal error. Internal CLR error. (0x80131506)
   at System.Buffer.MemmoveInternal(Byte ByRef, Byte ByRef, UIntPtr)
   at System.Runtime.InteropServices.Marshal.CopyToManaged...
```

**这是不可捕获的 CLR 级崩溃**，不是普通异常，脚本里 try/catch 拦不住。
所以图像处理脚本**只能用 GetPixel**，区域要取小。这个坑记下来，避免以后重走。


## 35. **主菜单（枢纽节点）拿到判据了**：8 真 / 58 负 / 0 误报（2026-10-03）

### 35.1 为什么这条最要紧

第 24 节确认「千星奇域主菜单是枢纽节点」——它有 **20 个入口**，整个迁移图有相当一部分边从它出发。
但它的判据一直是空的：secondary-back 命中它是**误判**，wonderland-map-star 是**常驻 HUD**。
枢纽没有判据，等于图的中心不可判。

### 35.2 找到的特征：**「返回提瓦特」按钮**

主菜单右下角有一个「◈ 返回提瓦特」按钮，而**其他界面都没有它**
（它是离开千星奇域回提瓦特的出口）。这正是「枢纽独有」的东西。

### 35.3 做法（沿用第 30 节的规矩：序关系断言，不拟合阈值）

新增 docs/ui-state-evidence/verify-mainmenu.ps1：

1. 从 wonderland-menu.png 取固定区域 (1758,1014) 60×20 —— 「返回提瓦特」文字的一小段 —— 作模板
2. 在每一帧的搜索区 (1690,1000) 205×60 里做零均值归一化互相关，取最佳分数
3. 断言只有序关系：**已知主菜单组的最低分必须严格高于其余所有帧的最高分**

### 35.4 实测（**全 66 帧语料**，不是抽样）

| 组 | 分数 |
|---|---|
| 已知主菜单 8 帧 | **1.0000 ×6**（wonderland-menu / wonderland-menu-escape / settled-3 / final / restored / modR-5）、**0.7743**（step-5）、**0.7852**（step-last） |
| 其余 58 帧 | 最高 **0.4459**（hall-page） |

其余帧里分数最高的六个：hall-page 0.4459、map-screen 0.3955、map-via-M 0.3822、
team-finder 0.3593、journey-page 0.3585、friend-list 0.3569 —— **全部远低于主菜单组最低的 0.7743**。

```
已知主菜单组最低 = 0.7743   其余全部帧最高 = 0.4459
两组完全不重叠。可用切点区间宽度 = 0.3284
```

**为什么这不算「调阈值凑结果」**：任何落在 **0.4459 与 0.7743 之间**的值都给出同样结论，
可选区间宽 **0.3284**。切点不是从观测里拟合出来的一个窄解，而是一整段都能用。

**8 真 / 58 负 / 0 误报 ⇒ 按第 24 节的纪律，主菜单判据成立。**

### 35.5 附带发现：6 帧的按钮区域像素完全相同

6 张已知主菜单帧的分数**精确等于 1.0000**，而它们的 frameHash **互不相同**
（全语料是按 hash 去重过的）。这说明：**帧与帧之间有差异（背后场景/镜头不同），
但「返回提瓦特」按钮区域逐像素一致** —— 也就是它是**静态 UI**。
静态 UI 做判据天然稳，这解释了为什么间隔这么宽。

### 35.6 限制（一并说清）

- **仍未接入分类器**（新规则 = 设计决策）。应用的 ui_state 输出里目前**没有**主菜单这一档。
- 判据依赖**右下角这块 UI**；如果以后版本改了按钮样式，脚本会 FAIL（这正是我们想要的告警方式）。
- step-5 与 step-last 只有 0.77~0.79，明显低于其他主菜单帧。
  它们**仍是主菜单**（第 33 节整帧复核过），分数低可能是这两帧的按钮状态或缩放略有不同。
  它们仍高于所有负样本，但**这个差异我如实记下来**，不装作没看见。
- 环境提醒：为了加速本想用 LockBits + Marshal.Copy，结果触发**不可捕获的 CLR 崩溃**（0x80131506），
  所以脚本只能用 GetPixel，区域必须取小（见 §34.5）。

### 35.7 同步更新

- verify-mainmenu.ps1 加入常规回归（第 5 个脚本）。
- 模型新增 state=mms：主菜单，supported=false 但附 verifiedBy=verify-mainmenu.ps1 与实测分数，
  说明判据成立但未接入分类器。
- 迁移图里 20 条「主菜单 → 子界面」边的 from 端因此**第一次有了可判定的依据**。

## 36. 私聊专属判据被否证，但意外得到 **secondary 类的特异判据**；并修掉我自己方法里的一个坑（2026-10-03）

### 36.1 起点

第 33 节把 secondary 拆成 secondary_hall（有合取判据）与 secondary_private（**没有任何判据**）。
本节想给**私聊**也找一条判据。私聊界面左下角有「**添加会话对象**」按钮，大厅没有，
所以取 secondary-full.png 的模式区域 (42,996) 100×22 作模板，在 (20,975) 200×55 里做 NCC。

### 36.2 结果一：**私聊专属判据被否证**

```
sec-fixture.jpg        1
secondary-full.png     1
round78-live.png       0.9997     <-- 大厅型二级，几乎打平
modD-5.png             0.4769
```

**round78-live.png 是「大厅型二级」（第 32 节整帧复核过），却拿到 0.9997。**
这说明该模板抓的不是"私聊独有"，而是**二级聊天共有的底栏**。
**私聊专属判据不成立**，如实记下。

### 36.3 结果二：同一组数据给出 **secondary 类的特异判据**

把二级的三帧（secondary-full、sec-fixture、round78-live，都经整帧复核）算作一组：

| 组 | 分数 |
|---|---|
| 二级 3 帧 | **1.0000 / 1.0000 / 0.9997** |
| 其余 63 帧 | 最高 **0.4769**（modC-4） |

```
二级组最低 = 0.9997   其余 63 帧最高 = 0.4769
可用切点区间宽度 = 0.5228
```

**3 真 / 63 负 / 0 误报**。与现状对比：

| 判据 | 命中 | 真 | 假 | 结论 |
|---|---|---|---|---|
| 应用自带 secondary-back | 11 / 66 | 3 | **8** | 不可用（§28） |
| **本节 NCC 判据** | **3 / 66** | **3** | **0** | **可用** |

也就是说：**同一批证据里，我用一个脚本判据把 secondary 的 8 个误判全部去掉了，
而且没有丢掉任何一个真命中。** 这条判据已在 docs/ui-state-evidence/verify-secondary.ps1，
可直接复现。

### 36.4 结果三：**我自己的验证方法有个坑，本轮踩到了**

上面那次探索，脚本第一次的输出是：

```
已知私聊组最低 = 1   其余帧最高 = 0.9997
  [PASS] 序关系成立，可用切点区间宽度 = 0.0003
```

**它报了 PASS，但区间宽度只有 0.0003** —— 这是**假通过**：
任何"相邻两帧刚好差一点"的情况都会让纯序关系断言通过，而这样的判据在实际使用中**不可用**
（分数抖一下就翻）。

**所以我给断言加了一条守卫**：间隔必须 **>= 0.05**，否则 FAIL，并在脚本里写明
"防止 0.0003 的假通过再次发生"。本轮的 secondary 判据间隔 0.5228，轻松通过。
verify-chatbox / verify-mainmenu 的间隔分别是 0.612 与 0.328，本来就没有这个问题。

**这条比结果本身更重要**：我的验证方法此前只看"序关系"，现在多了一层"间隔下限"。
以后凡是用序关系的脚本，都要带这条守卫。

### 36.5 代价与限制（一并说清）

- **仍未接入分类器**。secondary 现在有两条路：
  ①应用里的 secondary-back（不特异，会误判 8 帧）；②脚本判据（特异，但只在模型与脚本里）。
  **应用的实际输出仍然是①**，所以"打开主菜单被当成二级聊天"这个缺陷**依然存在**。
- 三帧正样本里，两帧私聊、一帧大厅型；**私聊与大厅型之间仍然分不开**（§36.2）。
- NCC 用的是固定区域与固定模板；如果游戏改了这块 UI，脚本会 FAIL（这是我们要的告警）。

### 36.6 同步更新

- 新脚本 verify-secondary.ps1（旧 verify-privatechat.ps1 已删除，避免留下一个结论错的脚本）。
- 模型里 secondary 增加 script 判据与实测分数；secondary-back 的 8 个误判仍留在 knownFalsePositives。
- 新增 docs/ui-state-evidence/INVENTORY.md：**由模型自动生成**的状态与判据清单
  （生成器 gen-inventory.js），避免文档与模型各说各话。

## 37. 两个否证与一处命名更正（2026-10-03）

### 37.1 否证一：就座场景的「🏛️ 频道图标」不能当判据

第 29.5 节记录过一个观察：就座帧的热键栏里有 🏛️ 蓝色千星奇域频道图标，站立帧没有。
本轮把它当候选判据严格测了一遍（模板取自 cat-frame-2.jpg 的 (240,1008) 26×30，
搜索区 (200,995) 130×50，全 66 帧）：

| 帧 | 分数 | 是否就座 |
|---|---|---|
| cat-frame-2.jpg | 1.0000 | 是 |
| final-frame.jpg | 0.9977 | 是 |
| **round78-live2.png** | **0.9934** | **否（大厅）** |
| cat-frame-3.jpg | 0.9794 | 是 |
| **probe.jpg** | **0.9772** | **否（大厅）** |
| hall-chat-open.jpg | 0.8007 | 否 |

```
就座组（3 帧）最低 = 0.9794   其余 63 帧最高 = 0.9934
[FAIL] 间隔 -0.014 ⇒ 不能当判据
```

**结论**：这个图标**不是就座场景独有的**，它在 round78-live2 与 probe 这两张大厅帧上也出现。
第 29.5 节那条观察**不足以支撑判据**——**就座场景至今仍然没有判据**，如实保持为候选。

（顺带一提：本例即使不带间隔守卫，纯序关系也会 FAIL（-0.014）。守卫在第 36 节那个
0.0003 的场合才真正救了场。）

### 37.2 否证二 / 命名更正：我把「玩家资料卡」叫成了「好友详情卡」

第 34 节我把 modI-2 / modJ-2 / recov-3 / looknow 归类为「**好友详情卡**」。
本轮放大 modI-2 整帧复核后发现命名不准：

- 顶部页签是「**好友 / 添加好友**」，有 UID 搜索框（138868628）与「搜索」按钮；
- 中间那张卡是**被搜索玩家的资料卡**（qtLKZS、冒险等级 5、世界等级 0、成就总数 0、
  深境螺旋/幻想真境剧诗/幽境危战 尚未挑战）；
- 右侧是「角色展柜 / 名片」页签与「**添加好友**」按钮。

**所以正确的名字是「玩家资料卡」，不是「好友详情卡」。**

**但第 34 节的实质结论仍然成立**：friend-more-settings 命中的是卡片**左上角那个 ⋯ 按钮**
（整帧复核时能直接看到它），它是"资料卡上共有的按钮"，不是"更多设置面板"。
命名已更正，结论保留。

### 37.3 本轮的净产出

- 否证了一条我上几轮记下的候选特征（🏛️ 图标），带完整实测数字。
- 更正了一处面板命名（玩家资料卡），并说明更正前后结论是否受影响。
- 没有新增"已支持"的东西——**这两条都是负结果**，如实记录，不粉饰。

### 37.4 顺带说明：为什么负结果也要写进来

本目标的验收条件里明确要求「**不得将未实现或不稳定的能力声明为已支持**」。
负结果正是这条要求的直接产物：一个候选特征被实测推翻，就必须写明它被推翻了，
否则下一个人（或下一轮的我）会拿着它当既有结论继续用。

## 38. 19 个探针的**全语料**特异性总账：只有 2 个够格当判据（2026-10-03）

### 38.1 为什么重做一遍

第 33 节的普查里，有 7 个探针我只在**抽样 20 帧**上测过就下了"不特异"的结论——**不够严谨**。
本轮把它们在**全 66 帧**上补测完，并补测了 wonderland-map-star。现在 19 个探针都是全语料数据。

### 38.2 总账（命中数 / 66 帧）

| 探针 | 命中 | 定性 |
|---|---|---|
| yellow-marker | 26 | 标记（分类器走彩色路径） |
| pink-marker | 22 | 标记 |
| blue-marker | 2 | 标记（其中 1 个是误判，见 §22） |
| **friend** | **6** | **可用**：6 帧全部复核为大厅 HUD |
| secondary-back | 11 | ✗ 不特异（8 个误判） |
| **secondary-hall** | **1** | **特异但正样本只有 1** |
| world-wish | 40 | ✗ 毫无判别力 |
| invite-view-star | 2 | ✗ 抓的是资料卡共有按钮 |
| invite-goto-hall | 6 | ✗ 命中详情卡/颂愿页/收藏页 |
| invite-enter-hall | **0** | 语料里没有该界面的帧（见 §38.3） |
| **friend-panel** | **3** | ✅ **特异，0 误报** |
| **friend-search-panel** | **10** | ✅ **特异，9 帧已复核** |
| friend-more-settings | 12 | ✗ 抓资料卡上的 ⋯ 按钮 |
| friend-block-chat | 24 | ✗ 不特异（命中列表与 blacklist **完全相同**） |
| friend-blacklist | 24 | ✗ 不特异 |
| friend-confirm | **0** | 语料里没有该界面的帧 |
| wonderland-confirm | **0** | 语料里没有该界面的帧 |
| paimon-menu | 26 | ✗ 常驻 HUD（39% 的帧） |
| wonderland-map-star | **37** | ✗ 常驻 HUD（**56%** 的帧） |

**结论：应用自带的 19 个模板里，真正够格当判据的只有 2 个**
（friend-panel、friend-search-panel），外加一个正样本仅 1 帧的 secondary-hall。
其余是常驻 HUD、跨界面共用的按钮、或语料里根本没有对应界面。

**这也解释了本目标为什么必须自己造判据**（§30 的 💬、§35 的主菜单、§36 的 secondary）——
现成模板不足以覆盖状态机。

### 38.3 三个 0 命中的探针：**必须精确表述**

friend-confirm、wonderland-confirm、invite-enter-hall 在 66 帧里**一次都没命中**。
这**不等于**模板坏了：正确说法是——

> **语料里没有这三个模板所对应界面的帧**，所以无从判断它们是否特异。

- wonderland-confirm 对应的是「大厅列表 → 确认按钮」那一步（startup.rs:571 会等它）。
  我的语料里**确实没有那张帧**，所以测不出来。
- friend-confirm 与 invite-enter-hall 同理。

**这正是本目标反复强调的纪律**：没有样本就说"没有样本"，
**不能**把"没测到"写成"不存在"，也**不能**据此说模板失效。

### 38.4 两个附带发现

- **friend-block-chat 与 friend-blacklist 的命中列表完全相同**（都是那 24 帧，分数也几乎一样）。
  说明这两个模板在画面里抓的是**同一个视觉元素**，它们本质上是重复的。
- **wonderland-map-star 命中 56% 的帧**，比 paimon-menu 还普遍。
  两者都是常驻 HUD，再次印证第 6.2 节「右上图标栏、左上派蒙头像不能判别面板」。

### 38.5 同步更新

- 模型 probeSpecificityAudit 全部换成**全语料**数据，19 条齐全，并给 0 命中的三条单独标注
  "语料里没有该界面的帧"。
- INVENTORY.md 重新生成，总账一节随之更新。

## 39. 合成映射模块：任意截图 → 状态，判不出就显式 Unknown（2026-10-03）

### 39.1 为什么需要它

目标 (3) 要求「**使任意时刻的截图都能被稳定映射到状态**，且对无法判别的界面显式返回 Unknown」。
此前这个能力是**散的**：

- 应用分类器只认它自带的两条老判据（marker / friend、secondary-back）；
- 第 30/35/36 节新立的三条判据（💬 聊天框、主菜单、secondary 类）只在脚本里；
- 好友列表面板、好友搜索面板的判据又只在探针里。

**结论就是**：任何一帧都得人来拼接三路结果。本节把它做成一个模块。

### 39.2 docs/ui-state-evidence/classify-frame.ps1

用法：

```powershell
pwsh -File docs/ui-state-evidence/classify-frame.ps1 -Image <图片路径>
pwsh -File docs/ui-state-evidence/classify-frame.ps1 -SelfTest
```

它同时取三路证据并**打印每一路**，最后按写死的优先级合成：

| 顺序 | 判据 | 来源 |
|---|---|---|
| 1 | 主菜单（返回提瓦特 NCC） | §35 |
| 2 | secondary（二级底栏 NCC） | §36 |
| 3 | friend_panel（应用探针） | §33 |
| 4 | friend_search（应用探针） | §34 |
| 5 | primary（应用分类器）+ 聊天框开/关（💬 NCC） | §29/§30 |
| 6 | 都不是 ⇒ **unknown** | —— |

**切点全部是"已测出的不重叠区间的中点"，由固定公式算出**：

- 聊天框 (0.3647 + 0.9769)/2 = **0.6708**
- 主菜单 (0.4459 + 0.7743)/2 = **0.6101**
- secondary (0.4769 + 0.9997)/2 = **0.7383**

这不是"调阈值凑结果"：区间两端都是实测出来的**组最低/组最高**，中点是**规则算的**，
而且脚本会把切点和判定依据一起打印出来，任何人可复核。

### 39.3 自检：13 帧已知真值全对

```
main_menu        wonderland-menu / wonderland-menu-escape / modR-5
secondary        secondary-full / sec-fixture / round78-live
friend_panel     friend-list / mod-4
friend_search    friend-search / modE-2
primary:chat_closed  hall-chat-closed
primary:chat_open    full-hall
unknown          map-screen
合成映射自检: PASS=13 FAIL=0
```

**注意最后一例**：地图屏三路都没有判据，模块返回 **unknown**——这正是目标要的行为：
**判不出来就说判不出来，不猜一个。**

### 39.4 自检抓到我自己的一个真 bug

第一版自检 6 通过 7 失败，输出里出现这种明显矛盾的组合：

```
第3路 脚本判据: 聊天框气泡=1  返回提瓦特=0.2951  二级底栏=0.2756
=> 合成状态: secondary   (二级底栏 0.2756 >= 中点 0.7383)
```

**0.2756 >= 0.7383 是假的，但它成立了。** 原因是我在条件里写了 &&：

```powershell
elseif ($sSec -ge $MID_SECONDARY && $appKind -ne "primary") { ... }
```

PowerShell 的 **&& 是"语句链"运算符，不是逻辑与**。上面这行被解析成**两条语句**，
if 拿到的是**最后一条语句**的值，也就是只判断了 appKind -ne "primary"。
于是所有非 primary 的帧全被判成 secondary。

**改成 -and 后 13/13 全过。** 这条记下来：PowerShell 里做逻辑与要用 -and，不要用 &&。

### 39.5 这个模块的边界

- 它**不修改应用行为**。应用里的 UiStateKind 判定仍然是老的四条规则，
  这个模块是**独立的合成层**，跑在脚本侧。
- 它依赖**应用正在运行**（要调 /debug/ui-state 与 /debug/ui-catalogue）。
- 判据覆盖的是**已有证据的界面**；没有证据的界面（结算、点歌、邀请中间面板、
  收藏页、地图、颂愿、纪游、组队、玩家资料卡、角色详情卡、就座场景）会落到 unknown，
  **这是预期行为，不是缺陷**。

## 44. 修掉 secondary_back 在主菜单上的误判（第一次改应用判定逻辑，2026-10-03）

### 44.1 用户授权

用户明确说：「**你可以自己决定每个界面使用的匹配模板**」。
此前 13 轮我一直把"5 条新判据是否接入分类器"挂着等你定，因为"不改既有设计"是你的硬要求。
拿到授权后，我从**危害最直接**的一条动手。

### 44.2 要修的是什么

第 24/28 节测出来的缺陷：`secondary-back` 探针**不排他**，66 帧里命中 11 帧，
其中 **8 帧是千星奇域主菜单**（wonderland-menu、wonderland-menu-escape、settled-3、
final、restored、modR-5、step-5、step-last），全部误判成**二级聊天**。

后果是实打实的：用户按 `Escape` 打开奇域主菜单时，应用会以为自己在二级聊天界面。

第 24 节已经证明**调阈值救不了**：主菜单 0.978 vs 真二级 0.995，区间窄到没法用。

### 44.3 判据为什么选「返回提瓦特」

奇域主菜单与二级界面**共用左上角那个圆形返回按钮**（这就是误判的根源），
但主菜单**右下角多一个「返回提瓦特」按钮**，二级界面没有。

- 模板：`assets/ui-main-menu-return.png`（60x20，取自 wonderland-menu.png 的 1758,1014）
- 搜索区：`screen.main_menu_rect`（100x40，位于 1740,1005）
- 判定：返回按钮命中后**再看**这个按钮，命中即判 `main_menu`，否则才是 `secondary`

### 44.4 一个必须记下来的教训：应用用的是 SAD，不是 NCC

我前面所有测量都用**零均值归一化互相关（NCC）**，但应用里的
`best_template_candidate` 走的是 **灰度 SAD**（`best_gray_sad_match`）。
**两者对背景亮度的敏感度不同**，我的 NCC 分数**不能直接搬过去当预期值**。
所以这次没有"按 NCC 分数设个阈值"了事，而是**写 Rust 测试**直接在应用的真实匹配器上验。

### 44.5 过渡帧：一个差点被当成 bug 的现象

第一版测试 8 帧里失败 2 帧（step-5、step-last）。放大比对后原因很清楚：

- `wonderland-menu.png`：按钮**完全不透明**；
- `step-5.png`：同一个按钮**严重淡出**，只剩约两成不透明度。

**它们是菜单淡出过程中的过渡帧，不是稳定界面。** 给这种帧加模板就是拿观测凑结果，
所以我没那么做。改为分别钉住两件事：

1. **6 张稳定帧必须判成主菜单**；
2. **2 张过渡帧**用**特征化断言**钉住"已知残留"——它们会落到 `secondary_back`，
   但过渡帧不稳定，应用只在稳定类型变化时才推 `ui.state` 事件，所以不会推出错误迁移。
   将来若这条判据改成能覆盖过渡帧，断言会失败，提醒回来更新结论。

### 44.6 全量测试逼出来的两个设计改进

改完后 `cargo test --lib` 从 1281 全绿变成 **6 个失败**，其中两个是**真问题**不是测试噪音：

1. **探针代价**：`ordered_batch_...`（`default_timeout_ms=200`）与
   `secondary_residency_...`（`timeout_ms=500`）两个测试超时。
   原因是我把主菜单探针放在**每帧都跑**的位置，让每个二级帧多付一次模板匹配。
   ⇒ 改成**条件探测**：只在返回按钮命中之后才看「返回提瓦特」。
   逻辑等价（它本来就是用来否决二级判定的），常见路径不多付代价。
2. **搜索区仍然偏大**：205x60 对 60x20 的模板来说太宽松。
   ⇒ 收紧到 **100x40**（按钮外扩约 20px）。按钮位置固定，收紧既省开销又降低误报面。

另外 4 个失败是**该改的**：

- `schema_leaf_paths_cover_all_default_config_leaves`：新字段要登记进 schema；
- `config_save_route_*`（2 个）：新字段原本落到 `restartFields`，
  要按 `db_idle_reload` 登记才会被当成运行时可生效字段；
- `fixed_scrolled_friend_list_...`：断言探针数是 2，现在是 3，且顺序变成
  friend → secondary_back → main_menu，断言跟着更新。

### 44.7 验证

**Rust 测试**（`main_menu_is_classified_before_secondary_back`）：

- 6 张稳定主菜单帧 ⇒ `main_menu_template` ✅
- 11 张负样本（hall-page / full-hall / overworld-hub / map-screen / team-finder /
  wonderland-collection / friend-search / journey-page / wish-page /
  final-frame / cat-frame-2）**都不许**判成主菜单 ✅
- 3 张真二级帧 + 仓库原二级夹具 ⇒ 仍是 `Secondary` ✅
- 2 张过渡帧 ⇒ 特征化断言 ✅

**全量测试**：1282 passed / 0 failed / 7 ignored（基线 1281 + 本轮的 1 个新测试）。

**实机端到端**（部署后调 `/debug/ui-state?image=`，PASS=10 FAIL=0）：

```
wonderland-menu.png        kind=unknown   rule=main_menu_template
wonderland-menu-escape.png kind=unknown   rule=main_menu_template
settled-3.png              kind=unknown   rule=main_menu_template
final.png                  kind=unknown   rule=main_menu_template
restored.png               kind=unknown   rule=main_menu_template
modR-5.png                 kind=unknown   rule=main_menu_template
secondary-full.png         kind=secondary rule=secondary_back_template
sec-fixture.jpg            kind=secondary rule=secondary_back_template
round78-live.png           kind=secondary rule=secondary_back_template
map-screen.png             kind=unknown   rule=no_reliable_anchor
```

**8 个误判里的 6 个已经在运行中的应用里消失，且真二级一个都没被抢走。**

### 44.8 如实说明边界

- **主菜单不是一个独立状态**：它落在 `UiStateKind::Unknown`，
  但 `source`/`finalRule` 单独标成 `main_menu`，所以**可以查询**
  （`finalRule == "main_menu_template"`），也能和"真的判不出来"区分开。
  要不要把它提升成 `UiStateKind` 的一等成员（会改动对外 JSON 契约与沙盒断言枚举），
  **等你定**。
- **过渡帧仍未覆盖**（见 44.5），这是有意的。
- 另外 4 条已成立的判据（💬 聊天框、friend-panel、friend-search-panel、secondary 类）
  **仍未接入**，下一轮继续。

### 44.9 本轮改动清单

- `assets/ui-main-menu-return.png`、`dist/sandbox-run/deps/assets/ui-main-menu-return.png`（新模板）
- `src/config/mod.rs`：`TemplateConfig.main_menu`、`ScreenConfig.main_menu_rect`（均带默认值，向后兼容）
- `src/config/schema.rs`：两个字段的 schema 登记
- `tests/fixtures/config.full.yaml`：补上两个字段
- `src/ui/state.rs`：`UiState::main_menu()`、`UiTemplateArgs.main_menu_template`、条件探针、新测试

### 44.10 我引入的真 bug：主菜单落 Unknown 会被回退误报成 overworld

第 44.3 节第一版把主菜单判成 `UiStateKind::Unknown`（只把 source 标成 main_menu）。
**这是错的**，而且我写了一个测试把它抓了出来：

```
hub_menu_never_falls_through_to_overworld_after_unknown_grace ... FAILED
  wonderland-menu.png 第 2 帧被回退误报成 overworld
```

**原因链**：Unknown ⇒ 连续两帧触发 `GameFallbackState` ⇒ 游戏态回退探针 `world_wish` ⇒
world-wish 在语料里命中 60% 的帧（§28 早就测出来了）⇒ 主菜单被报成 **overworld** ✗

**这条同时反证了我第 28 节的判断**：world-wish 确实没有判别力——它立刻就在主菜单帧上产生了假 overworld。

**修法**：把主菜单提升成**一等状态** `UiStateKind::MainMenu`（`name() = "main_menu"`）。
它不再是 Unknown，所以不会再触发游戏态回退，这正是目标 (1) 要求的"枚举所有可观测状态"。

连带改动（编译器逐处指出）：

| 位置 | 语义决定 |
|---|---|
| `state.rs` runtime_kind / Display | `main_menu` / `"main_menu"` |
| `debug_sandbox_runner.rs` | `SandboxUiStateKind::MainMenu`，沙盒断言可用 `"main_menu"` |
| `composition/…/listener.rs` | 主菜单盖住聊天区 ⇒ 与 GameGate/Overworld 同支：当作"不在聊天界面"，丢弃残留观察 |
| `ui/routines/startup.rs` | **并入 Secondary 分支**：改动前主菜单本来就判成 Secondary 并触发 Escape，保持既有行为不变 |

**验证**：全量 1283 通过；实机 `kind=main_menu`（6/6 主菜单帧），真二级 3/3 仍是 secondary。

### 44.11 顺着同一条线重新审视 world_wish

- 回退是**有闸门**的：只有连续两帧 Unknown 才会跑 `world_wish` 探针。
- 所以第 28 节"40/66 命中 ⇒ 没有判别力"这个结论**只对"无闸门直接使用"成立**；
  在带闸门的回退路径里它是**有效**的（本轮实机就靠它认出了 overworld）。
- 但它也确实危险：**任何持续 Unknown 的界面都可能被它误报成 overworld**（主菜单就是实例）。
  ⇒ 结论：`world_wish` 可以继续用在**闸门之后**，但**不能让界面长期停留在 Unknown**。

---

## 45. 实机走了一遍：从奇域大厅到提瓦特大世界（2026-10-03）

### 45.1 授权与动机

用户指示：「**现在你尝试前往一次提瓦特大世界，找到转移路径**」。这是明确授权驱动游戏界面。

### 45.2 第一堵墙：前台窗口守卫

前两次命令都失败：

```
live UI 命令失败: 当前前台窗口不是目标游戏进程，已中止输入
```

应用**拒绝向非前台窗口发输入**（安全设计，不是 bug）。
`ensure_foreground` 自己也先做这个检查，单独用会失败；
正确顺序是先 `activate`（真正激活窗口）再发输入 ✓

### 45.3 实际走出来的路径

起点：**奇域大厅**（`kind=primary`，`primary:friend`，stable）。
画面是广场场景，底部条 `💬 Enter | 👥 O | 🔊🏛️ Alt+N | 🎤 N`，右下角**没有**返回提瓦特。

| 步 | 动作 | 结果状态 | 应用证据 |
|---|---|---|---|
| 0 | —— | `primary` / `primary:friend` | score 0.991 |
| 1 | `press_key Escape` | `main_menu`（当时尚未提升，报 unknown） | friend 0.8002 miss、**secondary-back 0.9782 match**、**main-menu-return 1.0000 match** |
| 2 | `click (1740,1017)` 返回提瓦特 | 出现**提示弹窗** | 「是否离开当前大厅，返回提瓦特？」+ 取消/确认 |
| 3 | `click (1144,756)` 确认 | 短暂 `primary` → `unknown` → **`overworld`** | 事件 37/40/41，规则 `world_wish_template` |

**事件流实录**：

```
[37] ui.state {"kind":"primary","label":"primary:marker blue=0 yellow=4 pink=0","stable":true}
[40] ui.state {"kind":null,"label":"transition:unknown","stable":false}
[41] ui.state {"kind":"overworld","label":"overworld","stable":true}
```

**每一步都存了帧**：`docs/ui-state-evidence/transition-to-overworld/`（5 张）。

### 45.4 三个新发现

1. **主菜单是叠在大厅上的浮层**：背景仍是同一个大厅场景与角色，只是盖了左侧玩家卡 + 20 宫格图标面板。
   这解释了 `secondary-back` 为什么在菜单上命中——**它命中的是大厅本身就有的那个圆形返回按钮**。
2. **离开大厅有二次确认弹窗**，文案「是否离开当前大厅，返回提瓦特？」。
   **这就是 `wonderland-confirm` 模板对应的界面**——第 38.3 节"语料里没有这张帧"的缺口补上了 ✓
   该帧已入库：`docs/ui-state-evidence/leave-hall-confirm.jpg`。
3. **大厅与提瓦特的底部条完全不同**：大厅 `💬 Enter | 👥 O | 🔊🏛️ Alt+N | 🎤 N`；
   提瓦特 `Enter | Space E Q`（外加 `Lv.90` 血条）。这是一条**不依赖 OCR 的强判据候选**，
   但需要更多提瓦特帧才能立判据 ✗ **本轮不立**。

### 45.5 结论：overworld 是**可判别的**（修正此前的说法）

此前能力小结写「`overworld`/`game_gate` 不支持」——**这句已过时**：

- `overworld` 在**带闸门的回退路径**里有效，本轮实机成功识别 ✓
- 第 44.10 节修掉主菜单假 overworld 后，闸门前的假阳性也消掉了 ✓
- 已用 Rust 测试钉住：站在主菜单上连刷 5 帧**绝不**会变成 overworld ✓

`game_gate` 仍未实机验证（本轮没走到登录/点击进入那一步）。

### 45.6 迁移路径已写入独立文档

按"所有的状态机转移方式要写入到单独的文档"，本轮观测到的三段边已写进
`docs/ui-state-transitions.md` 与 `docs/ui-state-transitions.json` 的 `observedTransitions`
（带触发条件、前后状态、证据帧、应用事件序号）。


---

## 47. 一次失败的判据尝试：大厅底栏否决法（2026-10-03）

### 47.1 动机

第 46 节实测出缺陷②：**提瓦特大世界的世界聊天也带蓝/黄/粉气泡**，
于是 `primary_chat_markers` 把提瓦特判成了**千星大厅**。
最自然的修法是加一条**大厅独有**的判据去否决它。

### 47.2 候选：大厅底栏的 👥 O 图标组

底部条对比（第 46 节实机帧）显示得很清楚：

- **千星大厅**：`💬 | Enter | 👥 O | 🔊 🏛️ | Alt+N | 🎤 | N`
- **提瓦特**：只有孤零零一个 `Enter`

于是取大厅底栏的 `👥 O` 做模板，实测分离度（75 帧）：

```
正样本 7 个最低 = 0.7977        （千星大厅帧）
负样本 68 个最高 = 0.4033
间隔 = 0.3944  ≥ 0.05  ⇒ 按我的标准 PASS
提瓦特帧 0.2110 / 0.1968，加载帧 -2，登录帧 0.1178
```

看着非常理想，于是我把它接进分类器：**聊天标记必须同时命中底栏才算大厅**。

### 47.3 真值时它失败了

写 Rust 测试时，我把**已知的千星大厅帧都列进正样本**，包括 `overworld-hub.png`。
结果：

```
assertion failed: docs/ui-state-evidence/overworld-hub.png 仍是千星大厅
  left: Unknown    right: Primary
```

`overworld-hub.png` **是真实的千星大厅帧**（底栏 `💬 Enter | 👥 O | 🔊🏛️ Alt+N | 🎤 N` 齐全，
左侧是千星聊天「[欧耶耶]:狠了」「[无名氏]:房间邀请」…），
**但新判据把它漏成了 unknown。**

### 47.4 根因：底栏不只位移，还会缩放

把两帧底栏并排放大（`dist/sandbox-run/view-bar-compare.png`）后原因一目了然：

| 帧 | `👥 O` 位置 | 大小 |
|---|---|---|
| `round78-live2.png`（模板源） | x≈180–232, y≈1015–1057 | 较大 |
| `overworld-hub.png`（漏检） | x≈125–180, y≈994–1017 | **明显更小** |

→ 底栏**横向位移、纵向位移、而且缩放**。
而应用的模板匹配是 **1:1 灰度 SAD**（`best_template_candidate`），
**容不下缩放**：把搜索区放大到 (100,970) 700x110 之后**仍然漏检**。

### 47.5 处理：回退，并把"想要的行为"写成被忽略的测试

- **回退**了分类器改动，运行中的应用已重新部署回退版本并复验（行为与第 46 节一致）。
- 保留了一条 `#[ignore]` 的测试 `chat_markers_need_the_hall_bottom_bar_to_mean_the_hall`，
  它写的是**我们想要的行为**，等有了对缩放鲁棒的判据再打开。
- 配置里预留了 `templates.hall_bar` 与 `screen.hall_bar_rect`（**未接线**，会多一个 unused 警告）。
- **缺陷② 依然未修**，如实保持登记状态。

### 47.6 本轮真正的教训（比那条判据值钱）

**我在一个不完整的正样本集上量出了"极好的分离度"。**

- 我的 7 个正样本里**没有** `overworld-hub.png`；
- 于是 0.7977 / 0.4033 这个漂亮结果**完全没有意义**——它只说明"这 7 帧能被分开"，
  不说明"所有大厅帧都能被找到"。
- **正样本集不全，分离度就是假的安全感。**

⇒ 纪律补一条：**做判据前，先把"所有已知正样本"列全**（本项目里就是语料里所有该状态的帧），
再去量分离度；量完还要用**独立的正样本**验一遍。

### 47.7 一个更大的推论

应用里那批模板（`secondary-back`、`world-wish`、`wonderland-map-star`…）
之所以普遍"非特异"，很可能与同一件事有关：**1:1 灰度 SAD 对位置和缩放都不鲁棒**。
界面元素一旦因为聊天面板开合、分辨率、缩放而移动或变形，模板就会**同时**误命中别处、漏掉正主。
这解释了第 38 节那份很不好看的探针总账。

**这是匹配器层面的限制，不是某一个模板没调好。** 记在这里，供后续决定"要不要换匹配器"时参考。

---

## 48. 修掉缺陷②：用「底栏近白像素占比」否决提瓦特的世界聊天（2026-10-03）

### 48.1 思路：换一类判据

第 47 节的教训是 **1:1 灰度 SAD 对位置和缩放都不鲁棒**。所以这次不再用模板，
而用目标里明确允许的**像素/布局**特征：

> 千星大厅底栏有**多个白色圆角按键**（`Enter` `O` `Alt+N` `N`），
> 提瓦特底栏**只有一个 `Enter`**。
> ⇒ 统计底栏区域的**近白像素占比**，它天然对位移和缩放不敏感。

统计区域 `screen.hall_bar_rect = (0,985) 600x95`，近白判据 `min(R,G,B) >= 200`，
每 2 像素取样一次。

### 48.2 实测分布（76 帧，全部已有帧）

| 帧 | 近白占比 | 是什么 |
|---|---|---|
| 全白帧 / 加载帧 | 100 % | 空白页 |
| 添加好友面板 | 81.6 % | 白底面板 |
| 千星主菜单 | ~45 % | 白底面板 |
| **千星大厅（最低）** | **11.181 %** | `hall-chat-open.jpg` |
| **非大厅（最高）** | **5.257 %** | `secondary-full.png`（二级/私聊） |
| 提瓦特 | 3.000 / 3.826 % | 要否决的目标 |
| 登录界面 | 3.083 % | |
| 地图 / 收藏页 | ~2.0 % | |
| 角色详情卡等 | 0 % | |

**5.257% 与 11.181% 之间没有任何一帧。**

### 48.3 阈值怎么来的（不是凑的）

固定规则：取实测**不重叠区间的中点**。

```
(5.257 + 11.181) / 2 = 8.219 %
```

代码里是常量 `HALL_BAR_WHITE_RATIO_MIN = 0.08219`，
并**在注释里写明两端实测值与算法**。两端都是"组内极值"，中点是公式算的
⇒ 不针对任何单帧调整。

### 48.4 判据形态：**否决**，不是判定

它只做一件事：**聊天标记命中的时候，如果底栏近白占比不够，就否决"这是千星大厅"**，
落空返回 `unknown`（规则名 `markers_without_hall_bar`），之后交给回退闸门
⇒ 提瓦特会正确得到 `overworld`。

它**从不**把界面判成大厅 ⇒ 只会"漏判"，不会"误判成大厅"。

### 48.5 这次把正样本集查全了

第 47 节的失败根因是**正样本集不全**。这次先做的是：

1. **把 76 帧全部量一遍**，列出所有低于 13% 的帧；
2. 确认**大厅帧最低就是 11.181%**，其下全是地图/提瓦特/登录/二级/深色面板；
3. 然后才把**全部 16 个 >= 11.18% 的帧**写进 Rust 测试的正样本，一个不落
   （`hall-chat-open`、`ocr-A`、`step-1`、`restored-primary`、`cat.jpg`、`input-0/1`、
   `cat-frame-1`、`repro-a/b`、`overworld-hub`、`capture-0`、`full-hall`、
   `cat-frame-2`、`hall-chat-closed`、`round78-live2`）。

**先查全正样本，再量分离度**——这是第 47 节换来的纪律。

### 48.6 验证

**Rust 测试** `teyvat_world_chat_is_not_the_hall`：16 个大厅帧必须 `primary` ✅，
提瓦特与登录界面必须**不是** `primary` ✅。

**全量测试**：1285 passed / 0 failed / 8 ignored。

**实机复验（部署后走应用自己的 `/debug/ui-state?image=`）**：

```
cold-start/enter-2.jpg                            kind=unknown  rule=markers_without_hall_bar  ← 缺陷②已修
transition-to-overworld/05-overworld-teyvat.jpg   kind=unknown  rule=no_reliable_anchor
login-screen.jpg                                  kind=unknown  rule=no_reliable_anchor
overworld-hub.png                                 kind=primary  rule=primary_chat_markers     ← 未被误否决
hall-chat-open.jpg                                kind=primary  rule=primary_chat_markers
cat-frame-2.jpg                                   kind=primary  rule=primary_friend_template
round78-live2.png                                 kind=primary  rule=primary_friend_template
full-hall.jpg                                     kind=primary  rule=primary_chat_markers
当前实机                                           kind=overworld stable=True
```

### 48.7 边界（如实说明）

- 这条判据**只覆盖"聊天标记 + 底栏近白太少"这一种误判**。
  第 46 节的另一个缺陷（**加载界面被误报成 overworld**）**仍未修**。
- 它依赖"底栏在 `(0,985) 600x95` 内"；**换分辨率或改 UI 缩放要重新量阈值**
  （代码注释里已写明）。
- 只在这 76 帧语料上验证过；语料之外的界面可能出现**漏判**（退化成 `unknown`，
  不会误判成大厅 ⇒ 方向是安全的）。

---

## 49. 提瓦特侧界面采集：一个必须更正的事实（2026-10-03）

### 49.1 F2 打开的是「派蒙菜单」，不是「奇域主菜单」

在**提瓦特**按 `F2`，出现的是一个 **20 图标网格**：

```
商城   队伍配置  好友   成就
图鉴   角色图鉴  角色   提升指南
背包   任务      地图   活动
冒险之证 祈愿    纪行   多人游戏
特典活动 官方社区 版本热点 反馈
```

**这和第 44 节我建模成「奇域主菜单」的那个 20 图标网格是同一个界面。**

⇒ **更正**：它是**通用的派蒙菜单**，**提瓦特也会出现**，不是千星奇域独有。
第 44 节把它命名为 `main_menu`（奇域主菜单）并在「大厅枢纽节点」语义下讨论，
**这个语义是错的**——它只是"玩家菜单开着"这一种覆盖层状态。

它现在仍然是一个**可判别的状态**（判据不变：20 图标网格 + 返回按钮），
但**语义与命名要改**（下一步统一改，本节只登记）。

### 49.2 两套菜单都被误判成「千星二级」

| 按键 | 界面 | 应用判定 |
|---|---|---|
| `F2` | **派蒙菜单**（20 图标网格） | `secondary/secondary_back_template` ❌ |
| `Escape` | **系统菜单**（左侧竖排：设置/拍照/…） | `secondary/secondary_back_template` ❌ |

两个都是**提瓦特**的界面，却被判成千星奇域的**二级/私聊** ✗
⇒ `secondary_back_template` 命中的是它们各自的**返回/关闭按钮**，
说明这条判据**同样不排他**（与第 38 节的审计结论一致）。
**登记为缺陷**，不在本节修（不用调阈值凑）。

### 49.3 采集到的界面（`ui-state-evidence/sweep-49/`）

| 文件 | 界面 | 应用判定 |
|---|---|---|
| `01-paimon-menu.jpg` | 系统菜单（Escape） | secondary ❌ |
| `02-map.jpg` | 提瓦特地图（璃月） | unknown ✓ |
| `03-inventory.jpg` | 背包（B） | unknown ✓ |
| `06-wish.jpg` | 祈愿（F3） | unknown ✓ |
| `10-F2.jpg` | **派蒙菜单（F2，20 图标网格）** | secondary ❌ |
| `11-activity.jpg` | 活动一览（幽境危战…） | unknown ✓ |

**采集本身的教训**：用 `Escape` 关面板**不可靠**——`04-character`/`05-adventure-handbook`
其实都还是背包那一屏，`07-friends` 还是祈愿那一屏（文件大小完全相同即为此）。
**下一次采集必须每步验证是否真的切换了界面**，不能假定按键生效。

### 49.4 `world_wish` 的收窄搜索区实测：无效

第 46 节引出"加一条大厅判据否决提瓦特"的想法后，我试过**收窄祈愿图标的搜索区**
（从 `(1300,0) 620x110` 收到 `(1520,0) 180x110`），实测结论：

```
真提瓦特两帧       0.9948 / 0.9940   x=1569 y=13
多张千星大厅帧     0.93 ~ 0.95       x=1487 y=13     ← 只差 80px
收藏页             0.9526            x=1561 y=14
```

**千星奇域自己的 HUD 图标与提瓦特的祈愿图标高度相似**，位置也只差 80px
⇒ **靠区域分不开**。而且命中位置出现 `x=1487`/`x=1840`（区域之外），
说明这个探针的命中并不完全跟随配置区域。

⇒ 收窄搜索区**不成立**，已把配置**恢复原值**（revision 5）。
`world_wish` 作为 `overworld` 判据**不可靠**这一点，**维持第 46 节的登记不变**。

### 49.5 尚未解决

- **提瓦特→千星奇域的入口**：`F2` 是派蒙菜单；**活动一览里没有看到千星奇域**
  （列表可能还要往下滚）。第 44 节走通过的是**反方向**（千星→提瓦特）。
- **多种加载界面**：用户指出加载界面有**多种**，需要**多次状态切换**去发现。
  目前只有冷启动那一套（7 元素图标 + 全白帧）。

---

## 50. 两个操作性事实：调试模式互锁 与 游戏本体路径（2026-10-03）

本节记录的是**驱动游戏的正确姿势**，不是界面判据。踩了两个坑，都对采集工作影响很大。

### 50.1 真正的输入门是 `/debug/mode`，不是 `gameInputAllowed`

`GET /debug/state` 里有个字段 `gameInputAllowed`，它的实现是：

```rust
"gameInputAllowed": !debug_port(state)?.enabled()
```

**这个字段是"报告值"，会误导人**。真正决定"按键发不发得出去"的是**调试模式开关**：

| 调试模式 | `/debug/command` | `/debug/screenshot` | 结果 |
|---|---|---|---|
| `mode=enabled`（默认） | **200 正常** | **200 正常** | ✅ 能按键、能抓帧 |
| `mode=disabled` | **400 Bad Request** | **400 Bad Request** | ❌ 按键被静默忽略 |

**正确用法**：保持 `mode=enabled`（默认）即可，**不要**去关它。
调用形式是 **POST + query**（不是 GET，源码注释写错了）：

```powershell
Invoke-WebRequest "$b/debug/mode?mode=enabled" -Method POST -Headers $h -ContentType "application/json" -Body "{}"
```

**这次的代价**：我误以为 `gameInputAllowed=false` 是"输入被拦"，把它切成 `disabled`，
结果**调试接口全部 400**，后续所有按键与截图都失败，还据此得出"游戏没开"的错误结论。

### 50.2 游戏本体路径（修掉启动阻塞）

第 45 节的启动任务失败是因为 `startup.exe_path` 为空。实测本机路径为：

```
D:\Game\Genshin Impact\Genshin Impact Game\YuanShen.exe
```

已写入应用运行时配置（live revision 6）。**这是本机绝对路径，不进仓库**——
仓库里的默认值本来就是空的，需要按机器配置。

### 50.3 我犯的另外两个错（都记下来避免重犯）

1. **进程/窗口列表被我截断了**：查"游戏在不在"时用了
   `... | Select-Object -First 25`，而 `YuanShen` 按字母序排在最末
   （实测共 30 个有窗口进程）⇒ 我得出"游戏已关闭"的错误结论。
   **教训：判断"某进程是否存在"必须用 `Get-Process -Name` 直接查，不要靠列表截断。**
2. **在游戏本来就在运行时又 `Start-Process` 启动了一次**。实测未产生第二实例
   （`YuanShen` 进程数=1，PID 72868，启动于 09:05:56，即第 45 节那次冷启动），
   但这是运气好。**启动游戏前必须先确认它没在跑。**

### 50.4 已经确认可用的操作

- `mode=enabled` ✅ 调试接口正常
- `activate` + `press_key M` ✅ **成功打开了提瓦特地图**（`ui-state-evidence/loading-screens/01-map.jpg`）
- `F2` = 派蒙菜单（20 图标网格）；`Escape` 关不掉它，需要再按 `F2`
- `Escape` 在提瓦特 = 系统菜单
- 点击偶尔返回 **503 Service Unavailable**（瞬时），需要重试

### 50.5 尚未完成（下一步）

用户要求：**加载界面要多种，必须先做多次状态切换把它们找出来**。当前进度：

- ✅ 已确认按键可用、地图可开
- ❌ **还没真正做出一次传送**（点传送锚点时 503，锚点未选中）
- ❌ 多种加载界面**尚未采集**

下一步：在地图上精确定位一个传送锚点 → 选中 → 点「传送」→
**以 1 秒间隔高频抓帧**，把整个加载过程（白屏 / 元素图标 / 大厅过场 / 落地）全部收下来。

---

## 51. 新的独立证据源：游戏自身日志（2026-10-03）

用户提示「可以看游戏日志」。找到并分析了，**这是状态迁移的一个独立证据源**，
与截图互补——尤其能给出**加载过程的精确时间边界**。

### 51.1 日志位置与性质

| 文件 | 位置 | 内容 |
|---|---|---|
| `output_log.txt` | `%USERPROFILE%\AppData\LocalLow\miHoYo\原神\` | Unity 引擎日志 + **游戏语义事件** ✅ |
| `output_log.txt.last` | 同上 | 上一次会话 |
| `LocalLog.log` | 同上 | 只有**网络/调试**日志（`MoleMole.SuperDebug.LocalLog`）❌ 无界面事件 |

### 51.2 日志里确实可用的信号

```
[2026-10-03 09:06:17.483] Genshin Start Log:   GameManager ChangeGameWorld Home
[2026-10-03 09:06:17.487] Genshin Start Log:   Home LoadScene
[2026-10-03 09:06:17.676] Genshin Start Log:   LoginMainPageContext SetupView
[2026-10-03 09:06:19.911] Genshin Start Log:   LoginMainPageContext OnLogin
[2026-10-03 09:06:20.182] into OnShowUI,  params is: {"event_name":"Common.Loading"}
[2026-10-03 09:07:50.010] Genshin Loading Log: LoadingManager OnBeginLoadScene     ← 加载开始
[2026-10-03 09:07:53.268] Genshin Loading Log: BaseLevel FinishLoadScene
[2026-10-03 09:07:54.422] Genshin Loading Log: BaseLevel FinishLoadSceneAynsc AllEntityReady
```

**最有价值的两类**：

1. **`GameManager ChangeGameWorld <世界名>`** —— 世界切换点（本次冷启动为 `Home`，
   即登录/主界面世界）。
2. **`LoadingManager OnBeginLoadScene` → `BaseLevel FinishLoadScene` →
   `FinishLoadSceneAynsc`** —— **加载过程的精确起止时刻** ✅
   本次实测 **09:07:50.010 → 09:07:54.422，约 4.4 秒**。

### 51.3 与截图对照（冷启动第 45 节那次）

| 时刻 | 游戏日志 | 我的抓帧 | 应用判定 |
|---|---|---|---|
| 09:06:00 | 引擎启动 | — | 无窗口 |
| 09:06:09 | — | `stage-1.jpg` 全白 | `unknown` ✅ |
| 09:06:17 | **ChangeGameWorld Home** + 登录界面 SetupView | — | — |
| 09:06:29 | — | `stage-2.jpg` 登录界面 | `overworld` ❌ 假阳性 |
| 09:06:49 | — | `stage-4.jpg`「点击进入」 | `game_gate` ✅ |
| **09:07:50** | **OnBeginLoadScene** | — | — |
| **09:07:54** | **FinishLoadSceneAynsc** | — | — |

⇒ 日志说明：**「点击进入」之后才是真正的场景加载**，而那次采集在 09:07:00 就停了，
**正好错过了加载区间**。这解释了为什么当时没拍到加载界面。

### 51.4 局限（如实说明）

- `into OnShowUI / OnCloseUI` **只记录了 `Common.Loading` 一类插件 UI**
  （本次全文仅 7 条事件，种类只有 `Common.Loading` 与 `Common.LoadingToast`）
  ⇒ **它不是全量面板日志**，不能用来枚举所有面板开关。
- `LocalLog.log` **完全没有界面事件**。

⇒ 结论：游戏日志可以**独立佐证"加载区间"与"世界切换"**，
但**不能替代截图**去枚举面板状态。

### 51.5 怎么用它

- **采集加载界面时必须覆盖 `OnBeginLoadScene` → `FinishLoadSceneAynsc` 这段**（约 4–5 秒），
  抓帧间隔要 ≤ 1 秒。
- 判断"这次切换有没有加载"可以直接查日志，**不必靠猜**。

### 51.6 本轮未完成的

- **提瓦特→千星的入口仍未走通**：`POST /startup/enter-wonderland` 排入队列（taskId 5）
  后状态**始终是 `overworld`**，队列未推进；`enter-wonderland` 例程在地图里
  找不到千星星标（与第 44 节一致）。
- 各种加载界面**尚未采集到**。
- 注意：探测 `/startup/*` 接口时**真的把任务排进了队列**（含「启动游戏」），
  已用 `task_queue_cancel` 取消前三个。**探测这类接口本身就是副作用**，下次要小心。

---

## 52. 加载过程的可复现证据工具（2026-10-03）

### 52.1 工具

新增 `docs/ui-state-evidence/game-log-transitions.ps1`：直接读原神自身日志，
输出**世界切换 + 加载区间（含用时）**的时间线。用法：

```powershell
pwsh -File docs/ui-state-evidence/game-log-transitions.ps1
pwsh -File docs/ui-state-evidence/game-log-transitions.ps1 -LogName output_log.txt.last
```

它**不依赖应用、不依赖输入**，任何时候都能跑 ⇒ 是**独立可复现**的证据来源。

### 52.2 实测：加载绝不是"一种"

跨两次会话提取到的加载区间（`OnBeginLoadScene` → `FinishLoadSceneAynsc AllEntityReady`）：

| 开始时刻 | 用时 |
|---|---|
| 00:10:49.618 | **4.49 秒** |
| 00:11:36.148 | **1.74 秒** |
| 03:01:06.999 | **0.35 秒** |
| 03:11:34.091 | **3.02 秒** |
| 08:23:35.715 | **5.24 秒** |
| 09:07:50.010 | **4.41 秒** |

**用时从 0.35 秒到 5.24 秒，相差十几倍。**

⇒ **用户的判断被证实：加载界面不是一种，而是多种**（至少按时长分就有至少四类：
瞬时 <0.5s、短 1.7s、中 3s、长 4.5–5.2s）。不同时长很可能对应不同的加载画面
（例如切入秘境 vs 传送 vs 登录进世界）。

### 52.3 对采集方法的硬约束

- 加载最短只有 **0.35 秒** ⇒ **抓帧间隔必须 ≤ 0.3 秒**才可能不漏掉短加载；
  1 秒间隔**一定会漏**掉最短那一类。
- 而且**必须先知道加载何时开始**：不能靠盲拍，要靠日志的时间戳去对齐
  （先跑 `game-log-transitions.ps1` 拿到区间，再在该区间内高频抓帧）。

⇒ 这修正了本项目之前"1 秒间隔抓帧"的做法——**对短加载不够快**。

### 52.4 局限

- 日志只给"**何时加载**"，**不给"加载时画面是什么"** ⇒ 仍必须配截图。
- `FinishLoadScene`（未加 Aynsc 的那个）与 `FinishLoadSceneAynsc` 的区别本次未细究，
  表格统一用的是**后者（AllEntityReady）**，因为它更接近"画面可用"。

### 52.5 本轮未完成：应用已无法向游戏发送输入

**这是一个环境层阻塞，已排除以下原因**：

| 检查 | 结果 |
|---|---|
| 游戏进程 | `YuanShen` PID 72868 存活 ✅ |
| 窗口可用 | `/debug/window` → `available:true` ✅ |
| 调试模式 | `mode=enabled` ✅ |
| **前台窗口** | **Win32 实测：一直是 `YuanShen`「原神」** ✅ |
| 应用命令 | `/debug/command` 返回 `ok:true`，事件流 `run.finished status=passed` ✅ |
| 应用自己抢焦点 | 否（activate 前后前台都是游戏）✅ |
| 重启应用后 | **仍然无效** ❌ |
| 取消全部排队任务后 | **仍然无效** ❌ |

**实测现象**：第 50 节（约 10:45）按 `M` **成功打开了提瓦特地图**（有截图为证），
但从第 51 节排入 `enter-wonderland` 任务之后，**所有按键都不再有任何效果**
（`M`/`F2`/`Escape`/`click` 全部无效），而截图一直正常。

**⇒ 结论**：应用**能看但动不了**。这不在代码层面，需要人工干预
（**请手动点一下游戏窗口**，或把游戏切出去再切回来，然后再让我试）。

**在输入恢复之前，可继续推进的工作**：用 §52.1 的工具从日志里提取转移，
以及用截图记录用户自己操作时的画面（应用**截图功能正常**）。

---

## 53. 传送全程实测：一次完整的、可复现的状态迁移（2026-10-03）

这是目前**证据最完整**的一条迁移：前后帧、加载帧、触发动作、游戏日志时间戳**全都有**。

### 53.1 这次做了什么

1. 用户手动打开提瓦特地图并选择一个传送锚点 ⇒ 右侧出现「**传送锚点**」信息面板
2. **我点击面板底部的「传送」按钮，坐标 `(1660, 1032)`**
3. 以约 1.8 帧/秒连拍 28 秒

### 53.2 结果（帧 + 日志双向确认）

| 帧 | 内容 | 应用判定 |
|---|---|---|
| `b-01.jpg` | 提瓦特地图 + 「传送锚点」面板（点击前） | `unknown/no_reliable_anchor` |
| **`b-03.jpg`** | **加载界面：黑底 + 原神徽记 + 文字 + 三点进度** | `unknown/no_reliable_anchor` |
| `b-06.jpg` | 到达新场景（夜晚草地） | — |
| `b-09`~`b-45` | 新场景稳定 | `overworld/world_wish_template` |

**游戏日志独立佐证**（`game-log-transitions.ps1`）：

```
2026-10-03 11:14:02.823  加载开始  <<<
2026-10-03 11:14:04.031  加载就绪(AllEntityReady)  <<<   用时 1.21 秒
```

**我的点击发生在 11:14:00.2，加载开始于 11:14:02.8** —— 触发与后果对得上。

### 53.3 加载界面的像素特征（可直接做判据）

用同一套量法（每 20 像素取样，`v = 0.299R+0.587G+0.114B`）：

| 画面 | 平均亮度 | **暗像素(<40) 占比** | 亮像素(>200) 占比 |
|---|---|---|---|
| **加载界面 `b-03`** | **30.4** | **98.5 %** | 0 % |
| 传送前的地图 `b-01` | 116.2 | 7.8 % | 21.9 % |
| 传送后的场景 `b-06` | 113.2 | 1.2 % | 2.4 % |
| 冷启动的加载界面（白底 7 元素图标） | 255 | 0 % | ~100 % |

**⇒ 这一种加载界面的特征极强：`暗像素(<40) 占比 ≈ 98.5%`，而任何正常游戏画面都 < 8%。**
按本项目一贯的"取实测不重叠区间中点"规则，若做判据，切点大约在 **50%** 附近，
两侧余量都在 40 个百分点以上 —— 不需要凑阈值。

### 53.4 关于"多重加载界面"：已有两种实证

| 类型 | 外观 | 实测时机 | 用时 |
|---|---|---|---|
| **黑底徽记型** | 黑底 + 原神徽记 + 文字 + 三点进度 | 传送（11:09:05 / 11:14:02 两次） | 1.21 s / 1.66 s |
| **白底元素型** | 白底 + 7 个元素图标 + UID | 冷启动进世界（09:07:50） | 4.41 s |

**两次独立传送拍到的黑底加载界面外观一致**（`loading-dark-B.jpg` 与 `b-03.jpg`），
⇒ 该类型**可复现**，不是偶发。

### 53.5 点击坐标的重要教训

同一块面板，我先后点过 `(1600, 895)` 和 `(1660, 1032)`：

- `(1600, 895)` **失败**（落在面板说明文字上，日志无加载记录）
- `(1660, 1032)` **成功**（日志出现加载，耗时 1.21 秒）

⇒ **面板内按钮位置必须从实际截图量取，不能凭面板外形推**。
本项目已多次因为"估坐标"浪费采集轮次。

**另一个坑**：`/debug/screenshot` 在画面静止时会返回**逐帧完全一致**的图，
所以连拍时**不能只看亮度/哈希是否变化来判断"有没有动"** ——
静态面板会让 55 帧完全一模一样，容易被误读成"截图失效"。
正确做法是**同时看状态与游戏日志**。
