# 模板匹配：CPU 统一与性能审计

> 本文记录一次完成的改造：**把两条匹配路径统一到 CPU**，并实测 CPU 与 GPU 的速度差异。
> 结论来自真实游戏截图（1920×1080）与 31 个生产模板的对拍，不是估算。

## 1. 改造前：应用里存在两条匹配路径

| | CPU 路径 `best_template_candidate` | GPU 路径 `template_matching` crate |
|---|---|---|
| 位置 | `src/ui/template.rs` | 沙盒 `Template` 断言、`/tools/template` |
| 实现 | 手写逐像素 SAD | wgpu compute shader |
| alpha 掩码 | 支持（透明像素不参与比较） | **不支持**（遇透明就退回 CPU） |
| 提前终止（PDE） | 有 | 无 |
| 模板缓存 | 有 | 无 |
| 调用开销 | 仅循环本身 | **每次调用新建 wgpu Instance/adapter/device + 编译 shader + 建 buffer** |

crate 的自由函数每次调用都会 `TemplateMatcher::new()`：

```rust
pub fn match_template(input, template, method) -> Image<'static> {
    let mut matcher = TemplateMatcher::new();   // 每次调用都重建 GPU 设备
    ...
}
```

## 2. 最终架构：GPU 为主 + CPU 兜底

### 2.1 GPU 主路径（GpuSearcher）

1. 设备、管线与缓冲**进程内只创建一次**。旧 crate 每次调用都新建 wgpu 设备，
   实测固定开销 210–250ms；复用之后每帧只剩提交与回读。
2. 适配器选择：**独显优先，其次集显**；软件适配器与虚拟显卡不参与。
3. 自带 WGSL shader，**带边界判断**（修掉 crate 的行首污染缺陷）。
4. **整数 SAD**：着色器用 u32 精确整数累加，与 CPU 的整数 SAD 完全一致，
   不再有 f32 舍入误差（旧实现最大相对误差 1.86e-4）。
5. **模板行分片并行**：把模板行按 `gid.z` 切片，多个线程对同一位置并行累加（`atomicAdd`）。
   原本单个线程要串行 `th*tw` 次、整个 65×65 输出只有十几到二十几个 workgroup，GPU 严重欠载。
6. **工作量阈值**：`positions × 模板面积 < 3e6` 时直接走 CPU，
   避免为小模板白付 GPU 的固定开销（小模板 CPU 与 GPU 相当，大模板 GPU 快 20–87 倍）。
7. 适配器/设备创建失败时**记日志并永久转为 CPU 兜底，不再 panic**。

### 2.2 CPU 兜底与精确性保证

关键设计：**GPU 只用来定位，分数由 CPU 精确给出**。

```text
1) GPU 全图扫描 → 得到候选最优点 p
2) CPU 在 p 上算出精确整数 SAD = s
3) 以 (p, s) 为初始上界，再跑一次 CPU 扫描（SEA 剪枝 + PDE）
   由于“下界 > 上界”的位置不可能更优，这次扫描几乎立刻结束，
   但结果是精确的全局最优，且并列时仍取栅格序最前
```

因此**位置与分数与纯 CPU 实现逐位一致**——已用 124 组真实帧×模板实测（§5）。

### 2.3 其余

5. 模板缓存改为 `Arc`，SEA 所需元数据在**首次载入时**预计算，之后每次匹配零额外开销。
6. 沙盒 `Template` 断言与 `find_template_hits`（`/tools/template`）都走同一个 `best_gray_match` 调度器。
7. 带 alpha 掩码的模板：GPU 侧忽略 alpha，故**直接走 CPU**（与旧行为一致，且更准）。
8. `MILIASTRA_TEMPLATE_MATCH=cpu` 可强制纯 CPU，用于排查驱动问题。


### 2.1 保持行为不变

改造**不改变任何分数**。分数定义仍是：

```text
score = 1 - sad / alpha_sum = 1 - MAD/255
```

与原实现逐位一致（见 §5 的 124 组对拍）。带 alpha 的模板仍按逐像素 `|Δ|×alpha/255` 整数除法累加，顺序与截断行为完全保留。

## 3. 精确加速手段（不牺牲正确性）

### 3.1 SEA（连续消除）下界剪枝

只在一块**权重恒为 1** 的矩形上使用数学下界：

```text
SAD ≥ |Σ画面 - Σ模板| = |盒和 - 模板和|
```

- 盒和用**积分图（前缀和）** O(1) 求得，且与模板大小无关。
- 按该下界**升序**处理候选位置，上界会尽快收紧；一旦下界超过当前最优即可整体跳出。
- 边界处理：跳出条件是 `bound > best_sad`（严格大于）。因为 `bound ≤ sad`，`bound > best` 的位置不可能更优；而 `bound == best` 的位置仍会实算，以保证**并列时取栅格序最前**这一语义与旧实现一致。

模板内最大的全不透明矩形用直方图法**在首次载入时算一次**；纯透明模板没有这样的矩形时，自动退化为普通扫描。

### 3.2 PDE（部分失真消除）

逐行累加，一旦超过当前上界立刻返回。返回值此时必然大于上界，调用方据此判定该位置不可能获胜。

### 3.3 向量化友好的快路径

完全不透明的模板走连续切片、无逐像素 alpha 读写的内层循环；带 alpha 的模板保留原有的逐像素路径。

### 3.4 一处需要澄清的算法（我先前说错了）

**列滑窗增量 SAD 对二维模板是错的**，不能用。推导：

```text
SAD(x)   = Σ_{r,c} |I[y+r][x+c]   - T[r][c]|
SAD(x+1) = Σ_{r,c} |I[y+r][x+1+c] - T[r][c]|
```

位移后**同一个模板列 c 对应了不同的画面列**，且与 `T[r][c+1]` 无关，因此不存在
`SAD(x+1) = SAD(x) - 出列 + 入列` 这种恒等式。该技巧只对**可分离/全 1 核**（盒滤波）成立，
或者用于"窗口在另一幅图上平移"的立体匹配，不适用于同图二维模板匹配。

## 4. CPU 与 GPU 速度实测

测试：`cargo test --release --lib ui::template::tests::fixture_cpu_vs_gpu_speed`
画面为真实截图 `friend_list-0.png`（1920×1080），模板取自 `deps/assets`，
搜索区域为"模板尺寸 + 64px 边距"的有界窗口（贴近应用真实用法）。

- `GPU重建`：crate 自由函数，**每次调用重建 wgpu 设备**（＝改造前生产路径）。
- `GPU复用`：同一次会话内复用一个 `TemplateMatcher`，把"建设备"与"算 SAD"分开。

| 模板 | 尺寸 | 搜索区域 | CPU | GPU 重建设备 | GPU 复用 matcher | 重建/CPU | 复用/CPU |
|---|---|---|---|---|---|---|---|
| chat-marker-blue | 28×19 | 92×83 | **0.11ms** | 753.05ms | 0.18ms | 7044× | 1.7×（CPU 更快） |
| chat-marker-pink | 28×18 | 92×82 | **0.21ms** | 268.98ms | 0.37ms | 1275× | 1.8×（CPU 更快） |
| chat-marker-yellow | 27×18 | 91×82 | **0.14ms** | 215.18ms | 0.35ms | 1593× | 2.6×（CPU 更快） |
| friend-panel | 49×35 | 113×99 | 1.03ms | 249.04ms | 0.51ms | 242× | 0.5× |
| friend-more-settings | 46×44 | 110×108 | 3.14ms | 223.09ms | 0.44ms | 71.0× | 0.1× |
| friend-confirm | 390×82 | 454×146 | 12.02ms | 228.13ms | 1.84ms | 19.0× | 0.2× |
| friend-block-chat | 399×56 | 463×120 | 32.95ms | 208.88ms | 1.10ms | 6.3× | **0.03×（GPU 快约 30×）** |
| friend-blacklist | 404×62 | 468×126 | 39.40ms | 211.57ms | 1.31ms | 5.4× | **0.03×（GPU 快约 30×）** |

（"重建/CPU" 与 "复用/CPU" 是 GPU 耗时除以 CPU 耗时的倍数：**数值越大表示 CPU 越快**，小于 1 表示 GPU 更快。）

**读法：**

1. **GPU 侧有一个约 210–250ms 的固定开销**，与模板大小几乎无关（首次调用 753ms 是冷启动）。
   这就是"每次调用新建 wgpu 设备"的代价。
2. **去掉固定开销后，纯 SAD 计算的结论是分裂的**：
   - 小模板（≤50px）：**CPU 更快**（0.11ms vs 0.18ms）。
   - 大模板（390–404px 宽）：**GPU 快约 30 倍**（1.31ms vs 39.40ms）——GPU 的并行暴力计算在这里确实强。
3. 因此本次统一到 CPU 的收益**来自消除每次重建设备的固定开销，而不是"CPU 算法比 GPU 快"**。
   对本项目是合适的：主循环每周期只做少量模板、搜索区域很小，而每周期预算约 240ms，
   200ms 级的固定开销不可接受。
4. 若将来需要在一帧内匹配**大量大模板**，正确做法是**复用同一个 GPU matcher（或批量提交）**，
   而不是全面改用 CPU。

**CPU 路径另外两点客观优势：**

- **整数精确累加**：CPU 用 u64 累加整数差；GPU 先把像素归一化到 0..1 的 f32 再累加，
  大模板会有可观测的舍入误差（实测 404×62 模板在 798095 量级上偏差约 60，相对 7.5e-5）。
  因此对拍断言用 0.1% 相对容差，而不是精确相等。
- **alpha 掩码**：GPU 路径完全忽略 alpha，透明模板只能退回 CPU；CPU 路径原生支持。

### 4.1 本机适配器实测与 GPU 路径的可用性限制

`cargo test --lib ui::template::tests::adapter_report`（新增，仅测试用）会枚举 wgpu 能看到的所有适配器。

这台开发机（**集显 + 独显 + 2 个虚拟显示适配器**）枚举出 **12 个条目**：

| 适配器 | 后端 | 类型 |
|---|---|---|
| NVIDIA GeForce RTX 4070 Laptop GPU | Vulkan | DiscreteGpu |
| NVIDIA GeForce RTX 4070 Laptop GPU | Dx12 | DiscreteGpu |
| Intel(R) RaptorLake-S Mobile Graphics Controller | Vulkan | IntegratedGpu（多个条目） |
| Intel(R) UHD Graphics | Dx12 | IntegratedGpu（多个条目） |
| Microsoft Direct3D12 (Intel UHD Graphics) | Vulkan | IntegratedGpu（Mesa "Dozen" 转译层） |
| Microsoft Direct3D12 (NVIDIA RTX 4070) | Vulkan | DiscreteGpu（Dozen 转译层） |
| Microsoft Direct3D12 (Microsoft Basic Render Driver) | Vulkan | Cpu（软件） |
| Microsoft Basic Render Driver | Dx12 | Cpu（软件） |

用**与 crate 完全相同的参数**请求后，选中：

```text
NVIDIA GeForce RTX 4070 Laptop GPU | type=DiscreteGpu | backend=Vulkan
```

即 `PowerPreference::HighPerformance` 正确挑到了独显。因此本文 §4 的计时是在
**NVIDIA 独显 + Vulkan 后端**上测得的。软件回退（Basic Render Driver）本身可用，但不会被优先选中。

**由此可以明确 GPU 路径的限制：**

| 限制 | 说明 |
|---|---|
| **集显完全可用，只影响性能** | Intel UHD 在 Vulkan/DX12 下都能枚举到，wgpu 不要求独显；且 crate 只用 `Features::empty()` + `Limits::default()`，是最基础的能力集，兼容性门槛低 |
| **同一块 GPU 会出现多个条目** | `Backends::all()` 同时枚举 Vulkan 与 DX12，RTX 4070 就有两个条目；最终选中哪个由后端排序决定，**代码无法控制** |
| **不使用软件回退** | crate 写死 `force_fallback_adapter: false`，因此"Microsoft Basic Render Driver"（CPU 软件渲染）不会被优先选 |
| **没有可用适配器时直接 panic** | crate 用 `.expect("Adapter request failed")` / `.expect("Device request failed")`，失败即**进程 panic**，没有降级路径 |
| **无法指定后端或指定用哪块 GPU** | 参数里没有后端/设备选择入口，多显卡机器上无法强制绑定某一块 |

**环境风险（旧路径）：** 虚拟机、远程桌面、容器、显卡驱动缺失/被禁用/崩溃，或只剩软件回退时，
`request_adapter` 可能返回 `None`。而旧的 `Template` 断言与 `/tools/template` 都走这条路径，
**一旦如此就是 panic**。

**统一到 CPU 之后，模板匹配不再需要任何 GPU 或显卡驱动**——这类环境依赖被彻底移除，
这是除性能之外的另一个实质收益。

**注意**：210–250ms 的固定开销是 **NVIDIA 独显 + Vulkan** 的数字。换集显、或换到 DX12 后端，
初始化成本会不同（设备初始化与 shader 编译都由厂商驱动决定），因此该数值**不能外推到其它机器**。

### 4.2 集显 vs 独显（同一 workload）

`cargo test --lib ui::template::tests::gpu_adapter_bench`

模板 friend-confirm 390×82，区域 454×146，结果图 65×65，均为**原生 Vulkan 驱动**：

| GPU | 类型 | 建设备 | 建管线 | 每帧中位 | 每帧最快 |
|---|---|---|---|---|---|
| Intel RaptorLake-S | 集显 | 16.2ms | 12.4ms | 8.34ms | 7.91ms |
| NVIDIA RTX 4070 | 独显 | 40.5ms | 6.0ms | **1.19ms** | **1.04ms** |

（该微基准在 debug 下运行；GPU 时间是 GPU 侧负载加提交等待，受 Rust 优化影响很小，可视为上界。）

**结论：**

- **独显比集显快约 7 倍**（1.19ms vs 8.34ms）。两者的最大相对误差都只有 **1.86e-4**（f32 累加所致）。
- 建设备成本反而**集显更低**（16ms vs 40ms，独显要唤醒/初始化驱动），但只要复用，这就是一次性的。
- 与 CPU 对比（release：同一模板 12.02ms）：
  - 独显每帧 1.19ms → 约 **10 倍**于 CPU；
  - 集显每帧 8.34ms → 约 **1.4 倍**于 CPU。
- **回本点**：独显约 5 帧、集显约 10 帧即可摊销掉一次性建管线成本。主循环每个周期都会匹配模板，
  因此长期运行 GPU 明显占优。
- 与**旧 crate 路径**对比：同一块独显，旧路径每次调用 210–250ms，现在是 1.19ms，
  **提升约 180–200 倍**——差别全部来自"不再每次重建 wgpu 设备"。

### 4.3 大模板优化（release，RTX 4070）

**同一模板** friend-confirm 390×82、区域 454×146，直接对比两种着色器：

| 实现 | 归一化 | 上传 | 提交+回读 | 合计 |
|---|---|---|---|---|
| 朴素 f32 着色器（每线程算完整个模板） | — | — | 0.98ms | ~0.98ms |
| 整数 SAD + 模板行分片 | 0.02ms | 0.03ms | 0.45ms | **0.50ms** |

**约 2 倍提升**，并且结果从 f32 近似变成**精确整数**（与 CPU 逐位一致）。

各尺寸的阶段拆分（release）：

| 模板 | 区域 | 归一化 | 上传 | 提交+回读 | 合计 | CPU | GPU/CPU |
|---|---|---|---|---|---|---|---|
| 28×19 | 92×83 | 0.00 | 0.02 | 0.09 | 0.11ms | 0.11ms | 1.0× |
| 49×35 | 113×99 | 0.00 | 0.01 | 0.10 | 0.11ms | 1.03ms | **9.4×** |
| 390×82 | 454×146 | 0.02 | 0.03 | 0.45 | 0.50ms | 12.02ms | **24×** |
| 404×62 | 468×126 | 0.01 | 0.05 | 0.39 | 0.45ms | 39.40ms | **87×** |

**读法**：大模板由"计算"主导（上传只占 ~10%），而**小模板 CPU 与 GPU 打平**，
所以交给工作量阈值自动选路即可，调用方不需要关心。

⚠️ **性能数字必须用 release 读**：debug 构建下"上传"会被逐元素封送放大到 2.63ms，
release 只有 0.04ms——同一份代码相差 60 倍，这是测量口径问题，不是真实开销。

## 5. 正确性对拍

### 5.1 分块（模板行分片）的精确性

`cargo test --release --lib ui::template::tests::gpu_chunked_map_is_exact`

对同一区域用 6 种分片配置（1/3/7/15/62/256，覆盖不分片、恰好整除、尾片残缺、
分片数远大于模板行数）各跑一遍 GPU，把**整张结果图逐点**与 CPU 精确 SAD 比对：

> 分块精确性：6 个模板 × 6 种分片配置，共 900 个位置逐位一致

任何漏行、重行或漏清零都会让某个位置的部分和偏离 CPU 值，逐点相等即排除这些可能。

**为什么分块是安全的**：分块把同一位置的模板行拆给多个线程做 `atomicAdd`，
而**整数加法可结合、可交换**，所以累加顺序不影响结果，SAD 与单线程逐位相同。
反之若沿用 f32 累加，浮点加法不满足结合律、且原子顺序每次运行都不同，
结果会**逐次抖动**，边界阈值判断就可能间歇性翻转——这正是改用整数 SAD 的另一个理由。

### 5.2 分块 + 纯色模板：会不会误匹配？

这是**两个独立的问题**，必须分开回答。

**分块本身不会。** 两条测试都做了逐点比对：

```text
gpu_chunked_map_is_exact : 6 个模板 × 6 种分片配置，共 900 个位置逐位一致
solid_template_audit     : 纯色 16x16 模板、256x256 区域 58081 个位置，
                           chunks=1/64/256 与 CPU 不一致 0 个，偏小 0 个
```

"偏小"是关键指标：**只有 SAD 被算小才会抬高分数造成假阳性**。分块后每个位置由多个线程
`atomicAdd` 累加，只要有一片漏算，该位置就必然小于 CPU 值——实测 0 个。

**但纯色模板本身确实会大面积误匹配**，这与分块无关，是 SAD 归一化方式决定的。
score = 1 - MAD/255，16x16 纯色模板（值 217）在 256x256 区域内的实测分布：

| 阈值 | 通过位置数 | 占比 |
|---|---|---|
| ≥0.80 | 56753 | **97.7%** |
| ≥0.90 | 45025 | 77.5% |
| ≥0.95 | 18197 | 31.3% |
| =1.00 | 16 | 0.03% |

即 `min_score = 0.80` 时，纯色模板几乎"匹配一切均匀区域"。

**为什么纯色场景对分块最危险**：若结果图被读到累加中途，漏算的部分恰好是真实 SAD 的
k/th。对纯色模板而言这是一个"合情合理地小"的数值，看起来就是一次高分命中。
因此"读取前累加必须全部完成"很重要——本实现把 `copy_buffer_to_buffer` 与 compute pass
编码在**同一个 command buffer** 内，wgpu 会自动插入屏障，复制必然发生在所有原子写之后；
不存在跨 submit 或跨帧复用造成的部分累加。

**护栏**：加载模板时计算灰度标准差，低于 `FLAT_TEMPLATE_STDDEV = 4.0` 就告警。
实测现有 20 个生产模板全部通过，最低的 chat-marker-pink stddev = 27.04，离门槛很远——
**当前模板库没有这个风险**。真正的防线仍是多点签名断言（见 §4.5），
因为"纯色块"作为单点判据在信息量上就是不足的。


`cargo test --release --lib ui::template::tests::fixture_cpu_matcher_matches_reference`

在 4 张真实帧 × 全部模板（有界窗口）上，把优化实现与**改造前的逐像素朴素扫描**逐位对照：
**124 组全部一致**（位置与 SAD 均相等）。朴素基准以 `#[cfg(test)]` 形式保留在源码中。

同一测试还会在**每个模板的 65 个位置**上逐点比较 CPU 与 GPU 的整数 SAD，
确认除"被污染的行首"外两边完全吻合（GPU 侧按 0.1% 相对容差，因为它是 f32 累加）。

## 6. 附带发现：`template-matching 0.2.0` 的结果图存在行首污染（真实 bug）

对拍时发现 GPU 与 CPU 在同一位置的值不一致，追查后确认是 **crate 自身的缺陷**。

分发的线程数按 16 上取整，而 shader **没有边界判断**：

```rust
compute_pass.dispatch_workgroups((result_width as f32 / 16.0).ceil() as u32, ...)
```

```wgsl
let result_idx = y * (input_width - template_width + 1u) + x;
result_buf[result_idx] = total_sum;   // x 越界时写入的是“下一行”的条目
```

结果图宽 65 时实际有 80 列线程，`x = 65..79` 的线程会覆盖下一行最左 15 个条目，且它们算的是
`match_width = min(template_width, input_width - x)` 的**部分和（偏小）**。

实测证据（模板 `chat-marker-blue.png` 28×19，区域 92×83）：

| 量 | 值 |
|---|---|
| 结果图 | 65×65（= 4225，与 CPU 位置数一致） |
| CPU 最优点 (28, 64) 的 SAD | 30882 |
| GPU 在**同一位置**的值 | 121.105835 → ×255 = **30882**（完全一致） |
| GPU 结果图 argmin | 69.29 → ×255 = **17669**（在 (14,64)，即被污染的行首） |
| CPU 在 (14,64) 的真实 SAD | 34333 |

结论：同一位置两边完全吻合，而 GPU 的 argmin 落在了**被越界线程污染的行首**（65 宽 → 每行最左 15 个条目，
正好覆盖 x=0..14，与观测到的 (14,64) 吻合）。

**影响**：旧 GPU 路径会拿到偏小的 SAD → 虚高的分数 → **假阳性**。

- `find_template_hits`（`/tools/template`）会报告并不存在的命中。
- 沙盒 `Template` 断言取全图最小值，可能**在应当失败时判定通过**。

统一到 CPU 后这类假阳性被消除，即**错误率下降**（不是不变）。

## 7. 复现方式

```powershell
$env:MILIASTRA_MATCH_FRAMES    = "dist\sandbox-run\probe\frames"
$env:MILIASTRA_MATCH_TEMPLATES = "dist\sandbox-run\deps\assets"
cargo test --release --lib ui::template::tests::fixture -- --nocapture --test-threads=1
```

未设置环境变量时这两项测试自动跳过，不影响 CI。

### 8.1 彩色模板路径（`color_sad_at`）：已优化 9.2×

这条路径此前被记为"仍为朴素扫描"，但**先测量才发现它是真实瓶颈**（我原本以为 PDE 已足够，测量推翻了这个假设）。

调用点：`find_chat_markers`（聊天标记扫描）。搜索区域只有 **60 像素宽**（`CHAT_MARKER_SEARCH_WIDTH`），
但高度是整个聊天图，因此最坏情况是 60×1080 的条带，三个模板各扫一遍。

| 版本 | 60×1080 × 3 模板（阈值 0.9）中位 | 相对首版 |
| --- | --- | --- |
| 逐字节标量 + 逐字节提前退出 | 56.53ms | — |
| 按行无分支累加 + 行边界提前退出 | 28.47ms | **2.0×** |
| 行内 SSE2 `psadbw` + 行边界提前退出 | **6.16ms** | **9.2×** |

（release，RTX 4070 机器，20 轮取中位。）

三步的关键事实：

1. 逐字节提前退出**破坏了向量化**：实测内层只有约 1.8 GB/s（每周期约 2.2 字节），
   是标量分支循环的典型水平，而不是内存带宽上限。
2. 把提前退出从逐字节降到**行边界**即可让编译器向量化内层，白拿 2×。
3. 行内改用 SSE2 的 `psadbw`（一条指令算 16 字节的绝对差求和）再拿 4.6×。
   `psadbw` 输出 8 个 16 位部分和，每半区最多 4×8×255 = 8160，转 `u64` 不会溢出。
   非 x86_64 保留可移植回退。

**正确性**：`color_sad_row_blocking_preserves_accept_reject` 用确定性伪随机图（37×29，模板 9×7）
对 4 个阈值 × 全部候选位置，逐点比较"行块化实现"与"逐字节朴素实现"的接受/拒绝判定，要求两者完全一致，
并断言接受与拒绝两种结果都出现过。等价性成立的原因是：被拒绝的候选两种实现都返回某个大于
`max_allowed_sad` 的部分和（因此都落在阈值之外），通过的候选都算出完整 SAD。

### 8.2 测量纪律：功能用快档，性能才用 release

`[profile.release]` 用了 `lto = "fat"` + `codegen-units = 1`，一次 release 编译约 10 分钟；
而此前 `[profile.dev]` 未配置，默认携带全量调试信息，改一行源码也要重新编译并写 PDB。

现在 `Cargo.toml` 显式声明：

- `[profile.dev]` / `[profile.test]`：`debug = "line-tables-only"` + `incremental = true`，
  保留行号（断言失败仍能定位到源码行），但不再生成全量调试信息。增量编译从约 35–45s 降到 **12.4s**。
- `[profile.release]` 未改动，仍以运行性能为准。

**约定**：正确性/功能用例一律用 `cargo test --lib`（debug）；
只有需要时间数字时才用 `cargo test --release --lib ui::template`。

**注意**：如果带着 `MILIASTRA_MATCH_FRAMES` / `MILIASTRA_MATCH_TEMPLATES` 跑**debug 全量测试**，
模板基准会在 debug 下占用大量 CPU（`ui::template` 单独跑就要 58s），
连带把有 30s 超时的 live UI 用例拖到超时而失败。这是测量环境假象，不是回归：
不带这两个环境变量时全量为 1245 passed / 0 failed。模板基准应在 release 下单独跑。

## 8. 尚未覆盖

- 只在**一张真实帧 + 一套模板**上做了 CPU/GPU 计时，且只在**一台机器（NVIDIA 独显 + Vulkan）**上测过；
  集显、DX12 后端、其它驱动版本上的固定开销未测，数值不可外推。
- ~~`color_sad_at`（彩色模板路径）仍为朴素扫描~~：已用行块化 + SSE2 `psadbw` 优化 9.2×（见 §8.1）。
- 沙盒断言自身的端到端耗时（含截图）未在游戏运行时测量。
