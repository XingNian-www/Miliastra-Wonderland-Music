# 大世界 ⇄ 千星奇域 来回方法与冷启动状态转移

> 这份文档是**独立**的：只讲"怎么在两个世界之间来回"，以及"游戏关掉重开之后状态怎么走"。
> 界面判据本身见 [ui-state-machine.md](ui-state-machine.md)；
> 状态机模型（机器可读）见 [ui-state-transitions.json](ui-state-transitions.json)。

最后更新：2026-10-03

---

## 0. 一句话版本

| 方向 | 方法 |
|---|---|
| **千星 → 大世界** | `Escape` 开奇域主菜单 → 点右下角「**返回提瓦特**」→ 弹窗点「**确认**」 |
| **大世界 → 千星** | `M` 开地图 → 点地图右下角的**千星奇域星标** → 右侧选**大厅** → 点「**确认**」 |
| **游戏关掉重开** | 应用启动游戏 → OCR 找「**点击进入**」并点击 → 落到大世界 → 再走上一行 |

每条路径的**实机验证程度不同**，下面逐条标明。**没走通的地方我不写成走通了。**

---

## 1. 两个世界与相关状态

| 界面 | 应用中的 kind | 判据 | 出处 |
|---|---|---|---|
| 提瓦特大世界 | `overworld` | `world_wish` 模板（**在回退闸门之后**） | ui-state-machine §45.5 |
| 登录/点击进入 | `game_gate` | OCR 找「点击进入」 | 本文档 §4 |
| 千星大厅（一级聊天） | `primary` | 好友按钮模板 / 聊天标记 | §28 |
| 奇域主菜单（枢纽） | `main_menu` | 「返回提瓦特」按钮模板 | §44 |
| 二级聊天 / 私聊 | `secondary` | 左上返回按钮 + 底栏 | §36 |
| 地图、好友列表、结算等 | `unknown` | 无判据，**显式判不出来** | §32.1 |

> `unknown` 是"判不出来"，不是"其它"。上面的地图/好友列表就落在 `unknown`。

---

## 2. 出程：千星奇域 → 提瓦特大世界　✅ **已实机验证**

2026-10-03 用应用自己的 `/debug/command` 直接走通，每一步留帧。

```
起点  奇域大厅                  kind=primary  primary:friend (stable, score 0.991)
  │   press_key Escape          —— 打开奇域主菜单（叠在大厅上的浮层）
  ▼
中间  奇域主菜单                kind=main_menu（规则 main_menu_template, score 1.0000）
  │   ↑ 同一帧上 secondary-back 也命中 0.9782（这就是历史误判源，§44 已修）
  │   click (1740,1017) 「返回提瓦特」
  ▼
确认  提示弹窗                  「是否离开当前大厅，返回提瓦特？」取消 / 确认
  │   click (1144,756) 「确认」
  ▼
过渡  kind=primary(marker) → unknown(transition)     应用事件 37 → 40
  ▼
终点  提瓦特大世界              kind=overworld (stable) 规则 world_wish_template —— 事件 41
```

**应用事件流实录**：

```
[37] ui.state {"kind":"primary","label":"primary:marker blue=0 yellow=4 pink=0","stable":true}
[40] ui.state {"kind":null,"label":"transition:unknown","stable":false}
[41] ui.state {"kind":"overworld","label":"overworld","stable":true}
```

**证据帧**：`ui-state-evidence/transition-to-overworld/`

| 文件 | 内容 |
|---|---|
| 01-hall-entrance.jpg | 起点：奇域大厅 |
| 02-hub-menu.jpg | 按 Escape 后的奇域主菜单 |
| 03-leave-confirm.jpg | 「是否离开当前大厅」确认弹窗 |
| 04-after-confirm.jpg | 点确认之后 |
| 05-overworld-teyvat.jpg | 终点：提瓦特（璃月） |

---

## 3. 入程：提瓦特大世界 → 千星奇域　⚠️ **依应用实现整理；本次未实机走通**

### 3.1 应用里权威的步骤（src/ui/routines/startup.rs `execute_enter_wonderland`）

```
ensure_window → focus(after_activate_ms)
→ 等「派蒙菜单」模板稳定（确认已在大世界）
→ press_key M                    开地图
→ 在 wonderland_map_star_region 里等「千星奇域星标」模板命中
→ click(星标)
→ 等星标模板**消失**（说明已经切到下一层界面）
→ OCR 在 wonderland_hall_ocr_region 里找「千星奇域 / 大厅」选项
→ click(大厅)
→ 在 wonderland_confirm_region 里等「确认」按钮模板命中
→ click(确认)
→ 千星大厅
```

**配置坐标（tests/fixtures/config.full.yaml `startup:`）**

| 键 | 值 |
|---|---|
| `wonderland_map_star_region` | x=1700, y=850, w=220, h=230（地图右下角） |
| `wonderland_hall_ocr_region` | x=1280, y=0, w=640, h=1040（右侧） |
| `wonderland_confirm_region` | x=900, y=650, w=500, h=250 |
| `main_ui_region`（派蒙菜单） | x=0, y=0, w=480, h=270 |
| `wonderland_map_star_retries` / `_retry_ms` | 120 次 / 2500ms（最长 300s） |
| `wonderland_hall_retries` / `_retry_ms` | 90 次 / 2000ms（最长 180s） |
| `wonderland_transition_timeout_ms` | 60000 |
| `wonderland_confirm_threshold` | 0.9 |

### 3.2 本次实机观察到的事实（部分）

- **`M` 确实能开地图**，但**第一次会没反应**：在关掉好友面板（Escape）之后立刻按 `M`，
  画面还停在关面板的动画里，这一下被吞掉；**等约 2 秒再按一次就开了**。
  证据：`ui-state-evidence/`back-03-map.jpg``（没开）与 `back-04-map2.jpg`（开了，璃月地图）。
  **⇒ 每个界面操作之后必须留出动画时间，这条是硬要求。**
- **本次打开的地图上看不到千星奇域星标**：在配置指定的 (1700,850)-(1920,1080) 区域内，
  `wonderland-map-star` 模板只拿到 **0.7095**（阈值 0.9），**未命中** ✗
  最好位置 (1700,890) 落在「云来海 / 探索度」文字上，判断为**假位置**。
  **⇒ 所以"大世界 → 千星"这一步我这次没有走通，不写成走通。**
  可能原因（未验证，仅列出来供排查）：星标只在特定区域的地图视图出现 / 星标随活动开放状态变化 /
  当局地图缩放或平移位置不同。
  **顺带记一笔**：同一个 `wonderland-map-star` 模板在语料普查里命中 **37/66 帧（56%）**，
  在本该命中的真地图上却**漏检**——它同时"过宽"和"过窄"，是个需要重做的模板（§33/§38）。
- **好友列表里有另一条线索**：好友面板里「千星奇域游玩中」的好友会显示 **「前往其大厅」** 按钮，
  这正是 `invite-goto-hall` 模板的真实出处。
  证据帧：`ui-state-evidence/friend-list-goto-hall.jpg`。
  这给了入程一个**备用入口**：从好友列表点「前往其大厅」也能进千星（**未实机验证**）。

---

## 4. 冷启动：游戏关闭后重新启动　⚠️ **依应用实现整理**

### 4.1 应用的处理链（`startup:` 段 + `StartupRoutine`）

```
游戏未运行
  │ launch_game: true        —— 没找到游戏窗口就启动它（exe_path 留空则从米哈游启动器注册表找官服/国际服）
  ▼                          最多检查 launch_retries=12 次，每次等 launch_wait_ms=5000ms（≈60s）
游戏客户端 / 登录界面          kind 判不出来 → unknown
  │ enter_game: true         —— 持续未知时，OCR 在 enter_game_text_region(900,1000,130x40) 找「点击进入」并点击
  ▼                          最长 enter_game_timeout_ms=60000
提瓦特大世界                  kind=overworld（world_wish 在回退闸门之后生效）
  │ enter_wonderland: true   —— 按 §3.1 那串步骤进入千星
  ▼                          最长 enter_wonderland_timeout_ms=300000
千星大厅                      kind=primary
  │ 最后等「派蒙菜单」模板     final_primary_timeout_ms=120000，确认已经进到游戏里
  ▼
完成
```

### 4.2 状态转移（关掉重开时逐步对应）

| 阶段 | 画面 | kind | 可靠吗 |
|---|---|---|---|
| 游戏未运行 | 无窗口 | —— | 窗口可用性由 `/debug/window` 报 |
| 启动器/登录/公告 | 客户端界面 | `unknown` | ✅ 显式 unknown |
| 「点击进入」已出现 | 大门界面 | `game_gate` | ⚠️ 需要 OCR，**离线无法验证** |
| 已进入游戏 | 提瓦特 | `overworld` | ✅ 实机验证过（§45.5） |
| 千星大厅 | 大厅 | `primary` | ✅ 实机验证过 |
| 奇域主菜单 | 主菜单浮层 | `main_menu` | ✅ 实机验证过（§44） |

### 4.3 必须注意的两件事

1. **`startup.enabled` 默认是 `false`**。也就是说**自动进入流程默认不跑**，
   要它自己从冷启动一路进到千星，必须先把这一项打开。
2. **冷启动期间界面会长时间停在 `unknown`**，于是会走**回退闸门**去试 `world_wish`。
   而 `world_wish` 在语料里命中 60% 的帧（§28），
   **任何长时间 Unknown 的界面都可能被误报成 overworld**（主菜单就出过这个事故，§44.10）。
   ⇒ 冷启动阶段不要把"报 overworld"当成"真的进了大世界"，要看画面确认。

---

## 5. 操作硬性要求（都是踩过的坑）

1. **必须先 `activate` 再发输入。**
   应用会拒绝向非前台窗口发送输入，报
   「当前前台窗口不是目标游戏进程，已中止输入」。
   而 `ensure_foreground` 自己也要先过这个检查，**单独调用会失败**。
   正确顺序：`activate`（`after_activate_ms` 给足）→ 再发按键/点击。
2. **每次操作之后留足动画时间再抓帧或发下一个键。**
   关面板后立刻按 `M` 会被吞掉（§3.2 实测）。
   抓帧也同理：点完「确认」要等 2.5s 以上才看得到 `overworld`。
3. **点击坐标用的是 1920×1080 原始坐标**，不是预览图坐标。
4. **不要让界面长期停在 `unknown`**（会触发 `world_wish` 回退，见 §4.3）。

---

## 6. 本文件里"已验证/未验证"的界线

| 结论 | 状态 |
|---|---|
| 千星 → 大世界（Escape → 返回提瓦特 → 确认） | ✅ 实机走通，有帧有事件 |
| 大世界 → 千星（M → 星标 → 大厅 → 确认） | ⚠️ 只有应用实现与配置；**本次星标没找到，未走通** |
| 好友列表「前往其大厅」入口 | ⚠️ 只看到按钮，未点击验证 |
| 冷启动（启动游戏 → 点击进入 → 大世界 → 千星） | ⚠️ 依实现与配置整理；其中 `overworld`/`primary`/`main_menu` 三段**判据**已实机验证，`game_gate` 的 OCR 未验证 |
| `M` 能开地图、关面板后按键会被吞 | ✅ 实机验证 |

**把没走通的地方标成没走通**，是这份文档能用的前提。


---

## 9. 冷启动实测序列（2026-10-03，游戏确实处于关闭状态）

按用户要求"启动游戏到打开千星这个流程必须关闭游戏再进行"，本轮**真的先把游戏关掉**
（`close_window` → `/debug/window` 返回 `available:false`、截图返回 HTTP 500），
再由用户手动启动客户端，应用侧以 10 秒间隔记录：

| 时刻 | 画面 | 应用判定 | 对不对 |
|---|---|---|---|
| +10s | **全白帧**（窗口已出现但内容未渲染） | `unknown / no_reliable_anchor` | ✅ **正确**：判不出来就说不出来 |
| +20s | **登录界面**（云上天城、12+ CADPA、版本号、无任何 HUD） | `overworld / world_wish_template` stable=true | ❌ **假阳性！** 见 §9.1 |
| +30s | 过渡 | `unknown` | ✅ |
| +40s | **「点击进入」界面**（中央大门 + 底部「点击进入」） | `game_gate / game_gate` stable=true | ✅ **正确** |

**证据帧**：`ui-state-evidence/cold-start/stage-1..4.jpg`，
另存两份单帧：`blank-frame.jpg`（全白）、`login-screen.jpg`（登录界面）。

### 9.1 记一个真缺陷：登录界面被误报成 overworld

`stage-2.jpg` 是**登录界面**，画面里**没有任何 HUD**（没有地图、没有队伍、没有血条），
左下是版本号 `CNRELWin7.1.0_R43379043_S48518469_D48539834`，右上只有 `12+ CADPA` 适龄提示。
**应用却给出 `overworld stable=true`。**

原因就是 §44.11 预告的那条链：
登录界面既不是一级也不是二级 ⇒ 一直 `unknown` ⇒ **连续两帧后闯过回退闸门** ⇒
`world_wish` 探针被问到 ⇒ 它在语料里本就命中 60% 的帧（§28）⇒ 于是误报 `overworld`。

**这是"闸门也拦不住"的实证**，也再次证明 `world_wish` **不能单独承担 overworld 判定**。
已登记为缺陷（模型 `knownFalsePositives`）。
**不要**据此刻意去调 `world_wish` 的阈值——那正是本项目反复禁止的"拿观测凑结果"。

### 9.2 收获：`game_gate` 首次被实机验证

此前能力小结里 `game_gate` 一直是"需要 OCR、离线无法验证"。
本轮冷启动把它**实机验证了**：+40s 那一帧就是「点击进入」界面，
判定 `game_gate` 正确 ✓，而且「点击进入」四个字的位置
正好落在配置的 `enter_game_text_region (900,1000) 130x40` 里 ✓ **配置与画面对上了**。

### 9.3 顺带记录：窗口没了之后状态是**陈旧的**

游戏关闭期间 `/debug/ui-state` 一直返回**关闭前的最后一个判定**
（`secondary/secondary_back_template`），而不是 `unknown` 或报错。
⇒ **调用方不能只看 kind，必须同时看 `/debug/window` 的 `available`**。

### 9.4 应用侧发现的一个不一致（未能绕过）

应用自报配置 `startup.launch_game = true`（保存后 revision 2，且重启过应用），
但启动任务的日志仍然是：

```
UI routine failed at ensure_game_window (BeforeInput):
game window is missing and startup.launch_game is false
```

⇒ 该字段虽然被保存接口报成"无需重启即可生效"，
但 **routine 拿到的是启动时构建的另一份副本**，热更新与重启都没能让它生效。
另外 `startup.exe_path` 为空时，标准安装路径与注册表都没能定位到 `YuanShen.exe`。
**这两条合起来导致应用无法自行拉起游戏**，本轮改由用户手动启动。
（我改动过的 `launch_game` 已**恢复为原值 false**。）


---

## 10. 点击「点击进入」之后的实测：两个假阳性（2026-10-03）

接 §9，游戏停在「点击进入」界面时点击 `enter_game_text_region` 的中心 **(965,1020)**，
每 7 秒抓一帧并查判定：

| 时刻 | 画面 | 应用判定 | 对不对 |
|---|---|---|---|
| 点击前 | 「点击进入」界面 | `game_gate` stable=true | ✅ |
| +7s | **加载界面**（白底 + 7 个元素图标 + UID） | `overworld` stable=true | ❌ **假阳性** |
| +14s | **提瓦特大世界**（提瓦特 HUD + 左下世界聊天） | `primary` stable=true | ❌ **假阳性** |
| +21s ~ +35s | 同上 | `primary` 一直 stable | ❌ 持续错 |

证据帧：`ui-state-evidence/cold-start/enter-1.jpg`（加载界面）、`enter-2.jpg`（提瓦特）。
另存 `loading-screen.jpg`。

### 10.1 缺陷一：加载界面被误报成 overworld

白底 + 7 个元素图标那一帧（`enter-1.jpg`）**没有任何 HUD**，是**加载界面**，
却被判 `overworld stable=true`。原因同 §9.1：加载界面持续 `unknown` ⇒ 闯过回退闸门 ⇒
`world_wish` 命中。

### 10.2 缺陷二：提瓦特的世界聊天触发了「千星大厅」判据

`enter-2.jpg` 是**提瓦特大世界**，画面里是标准的提瓦特 HUD
（左上地图、右侧队伍列表 八重神子/奥莫塔/薇新纳/沃蕾妮莎、底部 `Lv.90 19434/19434`、
`Enter | Space E Q`）。**左下角是提瓦特的世界聊天**：

```
[咸鱼小呆瓜]: 摘计算机
```

这条聊天**带着蓝/黄/粉聊天气泡** ⇒ `primary_chat_markers` 命中 ⇒ 判成 **千星大厅** ✗

**结论：蓝/黄/粉聊天标记不是千星奇域独有的**，提瓦特的世界聊天也有。
⇒ `primary:marker` 这条判据**在提瓦特会误报**。

### 10.3 必须下调上一轮的说法

第 45.5 节我写过「`overworld` 是可判别的」。**按本轮实测，这句要收紧**：

- ✅ 它**确实**在真正的提瓦特帧上命中过（第 44 节事件 41，画面已复核是提瓦特）；
- ❌ 但它同时命中**加载界面**和**登录界面**（都是持续 `unknown` 的界面）。

⇒ 准确说法是：**`overworld` 判定不可靠**——它会命中，但**不能只凭它就认为进了大世界**，
必须同时看画面（或看 `/debug/window` 之外更多证据）。
**不得声明「overworld 已稳定支持」。**

### 10.4 冷启动全链的真实状态序列

把 §9 与本节合起来，**关掉游戏重开**一直到进入千星，真实序列是：

```
无窗口
 → 全白帧                 unknown   ✅
 → 登录界面               overworld ❌ 假阳性（§9.1）
 → 过渡                   unknown   ✅
 → 「点击进入」界面        game_gate ✅
 → 点击(965,1020)
 → 加载界面（7 元素）      overworld ❌ 假阳性（§10.1）
 → 提瓦特大世界（世界聊天） primary   ❌ 假阳性（§10.2）
```

**只有 `unknown` 与 `game_gate` 两段是可靠的。**
这正是本目标"不得将未实现或不稳定的能力声明为已支持"要防的事，
所以我把它们全部登记成缺陷，而不是包装成"已支持"。


---

## 11. 【已实机走通】提瓦特 → 千星奇域·大厅 的完整入口链（2026-10-03）

**这是本项目第一次把"从大世界进入千星"整条链走通并留下全套证据。**
之前 §3.2 记录的是"没走通"，原因是**坐标没点准**——不是流程不存在。

### 11.1 链路（每一步都实机确认）

| 步 | 起点状态 | 动作 | 终点状态 | 证据帧 |
|---|---|---|---|---|
| 1 | 提瓦特大世界 | 按 `Escape` 打开派蒙菜单 | 派蒙菜单（20 图标网格） | — |
| 2 | 派蒙菜单 | 点「**地图**」按钮 `(519, 684)` | 提瓦特地图 | `qianxing-entry-56/01-map-with-star.jpg` |
| 3 | 提瓦特地图 | 点右下角 **4 角星** `(1832, 1022)` | **地图→区域列表** | `02-region-list.jpg` |
| 4 | 区域列表 | 点「**千星奇域·大厅**」`(1690, 967)` | **确认框** | `03-confirm-dialog.jpg` |
| 5 | 确认框 | 点「**确认**」`(1180, 750)` | **加载界面** | — |
| 6 | 加载界面 | 等待约 4 秒 | **千星奇域·大厅** | `04-hall-entered.jpg` |

**确认框原文**：「提示 / **即将前往随机公共大厅，是否继续？** / 取消 · 确认」

### 11.2 关键坐标（都是放大 3~6 倍、画 20 像素网格量出来的）

| 目标 | 坐标 | 量法 | 对应模板/区域 |
|---|---|---|---|
| 派蒙菜单「地图」按钮 | `(519, 684)` | 3 倍网格 | — |
| 地图右下角 **4 角星** | `(1832, 1022)` | 4 倍网格，星心 | `startup-wonderland-map-star.png`，区域 `(1700,850,220,230)` |
| 区域列表「千星奇域·大厅」 | `(1690, 967)` | 3 倍网格，按钮中心 | 按钮范围 x1505–1875、y925–1010 |
| 确认框「确认」 | `(1180, 750)` | 4 倍网格 | `wonderland_confirm`，区域 `(900,650,500,250)` |

放大图：`zoom-star-region.png` / `zoom-region-hall-entry.png` / `zoom-confirm-button.png`

### 11.3 进入过程中观测到的状态序列（应用自己的判定）

点「确认」之后每约 1.3 秒查一次 `/debug/ui-state`：

```
t+1.2s   loading / loading_white_screen
t+2.5s   loading / loading_white_screen
t+3.8s   loading / loading_white_screen
t+5.1s   primary / primary_friend_template     ← 千星大厅
```

**⇒ 三件事同时得到验证**：
1. 第 55 节新增的 `loading` 状态**在这次真实进入中正确触发**，且认出是**白底**那种（`loading_white_screen`）；
2. 加载界面不是一闪而过，**持续约 4 秒**，1.3 秒的采样都能连中三次；
3. 落地状态是 `primary`，与千星大厅的既有判据一致。

### 11.4 大厅画面的可判别特征（落地帧）

`04-hall-entered.jpg` 底栏为：

```
💬 Enter   👥 O   🔊 🏛️ Alt+N   🎤 N
```

与提瓦特底栏（`Enter / Space E Q` + `Lv.90 19434/19434`）**完全不同**，这是千星大厅最直观的特征。
右上角还有「绮星盛会」标签 + 星形图标。

### 11.5 为什么之前失败（教训）

- 我此前在同一个区域里点 `(1855, 1017)`——**偏了 23 像素**，落在星标外侧，没有任何反应；
- 更早还在**没放大**的情况下凭整屏预览估坐标（`(1774,54)` 点 ✕ 偏 59 像素、`(1796,56)` 点 ✕ 偏 32 像素），**全部落空**；
- **⇒ 本项目所有界面点击，坐标必须放大后画网格量。** 凭预览估的坐标错了 4 次，量出来的坐标一次就中。
