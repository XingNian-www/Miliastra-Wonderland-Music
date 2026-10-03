# 架构评审：与 GitHub 最新代码的对比（2026-10-03）

## 0. 一句话结论

**GitHub 上的 `main`（v5.9.4, 5c4c5cf, 2026-10-01）根本没有这套 UI 状态机能力。**
本地相对于 `main` 有 **25 个文件、+6588 / -165 行**的未提交改动，外加 **28 项约 845 KB 的未跟踪文件**。
也就是说：**当前最有价值的工作全都还在工作区里，没有进版本控制。**

## 1. 网络与对比基线（必须先说清）

- 远端：`https://github.com/XingNian-www/Miliastra-Wonderland-Music.git`（HTTPS）
- **`git fetch` 连不上**：试了 4 次，先是 `curl 56 Recv failure: Connection was reset`，
  后两次是 `Failed to connect to github.com port 443`。**本机到 GitHub 的网络被阻断。**
- 因此"GitHub 最新"只能用**最近一次成功 fetch 的远端引用**判断：

| 引用 | 提交 | 日期 | 与本地关系 |
|---|---|---|---|
| `origin/main` | `5c4c5cf` | 2026-10-01 | **与本地 `main` 完全一致** |
| `origin/dev` | `7619819` | 2026-08-10 | 本地 `dev` 领先 2 天 |
| `origin/v2` | `2ba5db0` | 2026-07-21 | `main` 领先 v2 **160 个提交** |

**⇒ 「最新代码」= 本地 `main` = v5.9.4。**它和远端最后一次同步是一致的，
所以只要 GitHub 在 10-01 之后没有新提交，下面的对比就是准的；这一点无法在线确认。

## 2. v2 → main：160 个提交的演进（功能主线）

按提交类型统计：

| 类型 | 数量 |
|---|---|
| fix | 32 |
| feat | 26 |
| docs | 9 |
| chore / ci | 8 / 8 |
| refactor | 7 |
| release | 2 |
| 其它 | 56 |

最近这些提交说明主线已经**从"把机器人跑起来"转向"把推荐和风控做扎实"**：

- `literal match score` 决定本地曲库推荐（v5.9.4）
- 在线与本地结果按字面匹配分比较、本地曲库兜底
- 酷狗风控加固（设备身份、请求加固）
- AI 搜索来源与大厅映射权限（好友及以上）
- 塔罗牌命令、特权黑名单、邀请就绪

**⇒ v2 → main 是"点歌/播放/卡牌玩法/风控"这条产品线的成熟过程。**

## 3. 本地工作区相对 main 改了什么（+6588 行）

| 文件 | 行数变化 | 内容 |
|---|---|---|
| `src/ui/state.rs` | **+1910** | **全部 UI 判据**（世界判别、加载、主菜单、二级、千星地图、F2 面板…）|
| `src/ui/template.rs` | **+1993** | 模板匹配能力与审计 |
| `src/interfaces/http/protocol/tests.rs` | +678 | HTTP 协议用例 |
| `src/runtime/ui.rs` | +602 | 运行时 UI 设备接口、状态枚举 |
| `src/composition/application/http_facade.rs` | +356 | HTTP 门面 |
| `src/composition/application/listener.rs` | +352 | 监听器 |
| `src/adapters/windows/device.rs` | +197 | 设备输入 |
| `src/interfaces/http/ports.rs` | +138 | 端口定义 |
| `src/interfaces/http/protocol.rs` | +128 | 协议 |
| `src/adapters/windows/window.rs` | +79 | 窗口与点击（含 80ms 静默修复）|
| `src/runtime/monitor.rs` | +73 | 监控 |
| `src/ui/routines/startup.rs` | +76 | 启动例程（含"地图是否打开"检查）|
| `src/config/mod.rs` / `schema.rs` | +54 / +24 | 配置项 |

**未跟踪（不在任何提交里）**：

- `src/debug_*.rs` **11 个**、`src/interfaces/http/routes/debug.rs` —— 调试沙箱整套
- `docs/ui-state-criteria.md`（112 KB）、`ui-state-machine.md`、`ui-state-transitions.md/json`
- `docs/ui-state-evidence/`（**1099 个文件**的证据）
- `assets/world-hud-ref.bin`、`assets/qianxing-f2-ref.bin`、`assets/ui-hall-bar.png`、`assets/ui-main-menu-return.png`

**⇒ 结论：这套"UI 状态机 + 调试沙箱 + 证据链"是本地独立长出来的一整层能力，
GitHub 上完全没有。**

## 4. 功能是什么

### 4.1 main 上已有的（产品功能）

- **点歌**：在线多平台搜索 + 本地曲库兜底 + 字面匹配打分
- **播放控制**：队列、音量、酷狗设备身份与风控规避
- **卡牌玩法**：`features/card_games.rs`（4572 行）—— 塔罗、海龟汤、谁是卧底等
- **主持/邀请/审核**：大厅映射、好友及以上权限、黑名单
- **HTTP 接口 + 热键**：对外的控制面
- **配置热重载**：看门狗子进程 + ready 标记（`lib.rs` 里那套 `CONFIG_RELOAD_*`）

### 4.2 本地新增的（工程能力）

- **`ui/state.rs`：可查询、可断言的 UI 状态分类器**
  - 输出 7 种状态：`primary` / `secondary` / `main_menu` / `game_gate` / `overworld` / `loading` / `unknown`
  - 判不出来时显式 `unknown`，不猜
  - 世界判别（提瓦特 vs 千星）用"左上角 HUD 归一化描述子 + 双参考组"
  - 千星地图用整屏 `dark%` + `darkTeal%`；F2 面板用底栏描述子 + ±2 平移搜索
- **调试沙箱**：HTTP `/debug/*`（截图、状态、原子命令 `click`/`press_key`/`scroll`/`drag`）
  - 让"改一次代码就能对着真实游戏验证一次"成为可能
- **证据与文档**：1099 个证据文件 + 26 章判据文档 + 迁移 JSON
- **BOT 用的界面转移清单**：12 条转移里 6 条有判据、6 条没有

## 5. 架构评估

### 5.1 模块规模（`src/`）

| 目录 | 文件 | 行数 |
|---|---|---|
| `features` | 36 | **41425** |
| `runtime` | 20 | 20010 |
| `ui` | 18 | 13711 |
| `composition` | 16 | 13498 |
| `interfaces` | 19 | 12509 |
| `config` | 4 | 8953 |
| `adapters` | 13 | 5256 |
| `observation` | 12 | 4371 |
| `src/` 根目录 | 17 | 约 8000（**其中 11 个是 `debug_*`**）|

### 5.2 合理的地方 ✅

1. **分层方向是对的。** `adapters`（Windows/文件）→ `interfaces`（HTTP/热键）→ `composition`（组装）
   → `features`（业务）→ `runtime`（运行时）→ `ui`（界面驱动），加上 `config` / `observation`，
   这是干净的分层结构，职责名词本身没有含混。
2. **CI 有多后端矩阵**：CUDA / OpenVINO / 纯 exe 三个构建工作流 —— 对应本地 AI 推理的多条落地路径。
3. **配置有集中校验**：`config/schema.rs`（2770 行）+ `validate()`，把"配置写错"挡在启动前。
4. **热重载设计有工程含量**：看门狗 + 替代进程 + ready 标记文件 + 专用退出码，
   解决了"重启会丢状态"这个真问题。
5. **文档量充足**：用户指南、功能说明、调试沙箱说明都成文。

### 5.3 不合理的地方 ❌（每条都有上面的数据支撑）

1. **❌ 调试沙箱 11 个模块直接堆在 crate 根目录（约 8036 行）。**
   `src/` 下 17 个文件里 11 个是 `debug_*`，`debug_sandbox_runner.rs` 单文件 3524 行。
   正确形态是一个 `src/debug/` 目录装它的子模块。
   **这是当前最明显的结构问题**，而且它是未跟踪文件，说明这层能力是"贴"上去的。
2. **❌ 单文件普遍过大。** 超过 2000 行的有 18 个：
   `runtime/business.rs` 5010、`features/playback/controller.rs` 4968、`features/card_games.rs` 4572、
   `interfaces/http/protocol/tests.rs` 4442、`features/song_request/application.rs` 3756 ……
   文件大小本身不是罪，但同时**没有 lint 约束**（见第 4 条）就没人拦得住继续膨胀。
3. **❌ 我自己的 `ui/state.rs` 2654 行也犯同样的毛病。**
   世界判别、加载、主菜单、二级面板、千星地图、F2 面板、聊天标记……十几套判据全塞一个文件，
   只有 37 个常量和一堆自由函数。按状态拆成 `ui/state/world.rs`、`ui/state/map.rs` 这样才是对的。
4. **❌ 没有 lint 与架构约束。** 没有 `clippy.toml`、没有 `deny.toml`、没有 `rustfmt.toml`；
   CI 只有 3 个 **build** 工作流，**没有 test、没有 clippy、没有 fmt**。
   也就是说分层边界**完全靠人自觉**：谁都可以从 `features` 直接调 `adapters::windows::*`，没人会拦。
5. **⚠️ `ui` / `observation` / `runtime::ui` 三处边界模糊。** 三者都在做"看屏幕、认界面"：
   `ui`（13711 行）、`observation`（4371 行）、`runtime/ui.rs`（2192 行）。
   实测已经有重复：裁剪/归一化逻辑在 `ui/state.rs` 与 `observation/chat.rs` 各有一份。
6. **❌ 最大的一层能力没进版本控制。** 6588 行改动 + 845 KB 未跟踪，包括 1099 个证据文件。
   这是**流程问题而非代码问题**，但它比任何单文件过大都更危险：
   `git clean` 或一次误操作就会让整套证据链消失。

### 5.4 总评

**分层设计的骨架是合理的，问题出在"骨架没有被约束住，也没有被提交住"。**

| 维度 | 评价 |
|---|---|
| 分层划分 | ✅ 合理（8 个目录职责清楚）|
| 依赖方向 | ⚠️ 看起来是向内依赖，但**没有机器校验** |
| 文件粒度 | ❌ 偏差（18 个文件 > 2000 行，最大 5010 行）|
| 质量门禁 | ❌ 缺失（CI 只构建，不测试/不 lint/不 fmt）|
| 版本控制 | ❌ **最重要的新增能力全部未提交** |
| 文档 | ✅ 充足（但 `ui-state-criteria.md` 112 KB 单文件也该拆）|

### 5.5 建议的处置顺序

1. **先把工作区提交住**（最高优先）：把 `debug_*` 收进 `src/debug/` 后再提交，
   证据目录考虑用 Git LFS 或只提交代表性帧（1099 个文件不适合直接进 git）。
2. **给 `ui/state.rs` 拆目录**：按状态一族一文件，`mod.rs` 只做分发。
3. **补 CI**：`cargo test` + `cargo clippy -- -D warnings` + `cargo fmt --check` 三条先加进去；
   再考虑用 `deny.toml` / 自定义 lint 锁住分层方向。
4. **明确 `ui` / `observation` / `runtime::ui` 的分工**并把重复的裁剪归一化抽成一处。
5. 之后再谈把 `runtime/business.rs`、`features/playback/controller.rs` 这些巨无霸拆分。

## 6. 复现命令

```powershell
git remote -v
git fetch origin                     # 本机实测：443 连不上
git rev-list --left-right --count HEAD...origin/v2    # 160  0
git diff --stat HEAD                 # 25 files, +6588 -165
git status --porcelain | Where-Object { $_ -match "^\?\?" }   # 28 项未跟踪
```
