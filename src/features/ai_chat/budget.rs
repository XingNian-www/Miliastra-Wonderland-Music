//! 记忆与请求的容量预算。
//!
//! 集中放这里的原因：这些数字互相牵制（压缩触发线必须低于兜底删除线、
//! 读取窗口必须小于落库预算），分散到各子模块后极易单独改坏其中一条。

/// 单人**近期原文**预算：96K token（问答原文，超出的进入中期摘要）。
///
/// 这是**落库/保留**预算，不是读取窗口：压缩在 82% 触发、降到 15%，
/// 正常运行时库里长期在 78.7K 以下，读取侧因此可以「读全部」而不用滑窗。
pub(crate) const ACTOR_TOKEN_BUDGET: usize = 96_000;
/// 单人**中期摘要**预算：4K token。
///
/// 分层保留：近期原文 → 中期摘要 → 远期只留事实。
/// 摘要本身也会滚动覆盖（旧摘要 + 新溢出再压一次），所以它是压缩后的
/// 有界归档，不是无限追加的第三份历史。摘要只做溢出阀门，不是第二份历史：
/// 4K 足以放压缩后的结论，也不至于反过来挤占原文。
pub(crate) const ACTOR_SUMMARY_BUDGET: usize = 4_000;
/// 个人历史作为 messages 注入的 token 上限：12K。
///
/// turns 仍按 96K 落库预算完整保留；但默认注入个人历史后，若把全部原文都
/// 塞进 messages，小上下文模型会 400、大模型也会显著增加费用与首 token 延迟。
/// 12K 覆盖线上当前最大约 10.7K，超出时从最旧裁剪、至少保留最近 1 轮。
///
/// 发布说明素材（不改协议）：
/// - hall 摘要是全局单份，无法按受众拆分，可能含当前玩家早期问答；
/// - <32K context 的模型不建议把本预算开大（默认 12K 已接近其安全上限）；
/// - 本预算 schema 允许 1K..200K，但上限不等于建议值，过大仍可能触发
///   provider context length / 费用与首 token 延迟上升。
pub(crate) const ACTOR_HISTORY_INJECT_TOKEN_BUDGET: usize = 12_000;
/// 大厅**近期原文**预算：32K token，全局共用。
///
/// 大厅记忆记录**所有人在大厅说的话**（不只 BOT 自己的），是所有玩家共享的上下文。
/// 保留 32K 是为了让压缩（82% = 26.24K 触发）有足够长的周期；
/// 每轮注入量由 `HALL_READ_BUDGET`（4K）单独约束。
pub(crate) const HALL_TOKEN_BUDGET: usize = 32_000;
/// 大厅**中期摘要**预算：4K token。
///
/// 与读取预算同量级：摘要本身就是压缩后的整段大厅历史，太小会让大厅
/// 记忆实际只剩最近几句；再大则会挤占原文，摘要只做溢出阀门。
pub(crate) const HALL_SUMMARY_BUDGET: usize = 4_000;
/// 机器人自身输出预算：32K token，全局共用。
pub(crate) const BOT_TOKEN_BUDGET: usize = 32_000;

/// 组装提示词时读取大厅原文（`ai_chat_hall_turns`）的**读取**预算。
///
/// 与 `HALL_TOKEN_BUDGET`（32000，那是**保留**预算）区分开：
/// 原来 `hall_context` 的 SQL 没有 LIMIT，会全表读取，
/// 大厅每轮写 2 行，几百轮后单次请求塞进上万 token，而目标输出只有 160 字符。
///
/// 4K 与个人原文的 96K、摘要的 4K 处于同一量级：大厅是共享上下文，
/// 需要足够长才能接住「刚才谁说了什么」，但不必把整张表塞进提示词。
/// 摘要（4K）已承担更早的大厅信息，读取侧只补最近原文。
pub(crate) const HALL_READ_BUDGET: usize = 4_000;
/// 大厅上下文里 Bot 行最多保留的条数（近期）。
///
/// Bot 自述已经并入 hall 一条通道，但线上有约 130 条历史回答（≈3.5K token）。
/// 全量注入既贵、又让模型按旧模板复读；这里与 token 预算取先到者。
pub(crate) const HALL_BOT_READ_MAX_LINES: usize = 6;
/// 大厅上下文里 Bot 行最多使用的 token 数（与条数取先到者）。
pub(crate) const HALL_BOT_READ_TOKEN_BUDGET: usize = 800;
/// 压缩**触发**水位：原文占用超过预算的这个百分比才压缩。
///
/// 迟滞（hysteresis）是这套分层的关键：触发与回落必须是两个不同的水位，
/// 否则"压到 X 之后立刻又超 X"会让每一轮都压一次（实测就是每轮烧一次摘要调用）。
pub(crate) const COMPRESS_TRIGGER_PERCENT: usize = 82;

/// 压缩**目标**水位：一次压缩把原文降到预算的这个百分比。
///
/// 从 82% 压到 15%：96K 预算下触发线 78.72K、目标 14.4K，按单轮约 200 token
/// 计算约 320 轮才再压一次，压缩因此是低频事件，不是每轮开销。
/// 个人与大厅共用这两个水位，避免两套迟滞参数各自漂移。
pub(crate) const COMPRESS_TARGET_PERCENT: usize = 15;

/// 组装提示词时读取单人历史原文的 token 上限。
///
/// 历史窗口不再按轮数裁剪，而是按 token 预算整段读取。
///
/// 成熟聊天 BOT（ChatGPT / Claude / SillyTavern / MemGPT / LangChain
/// SummaryBuffer）的做法一致：**保留预算 + append-only + 涨满折叠**，
/// 而不是每轮丢最旧一条的滑动窗口。
///
/// 为什么不用滑窗：`messages` 第一条历史一变，按前缀匹配的 provider 缓存
/// （OpenAI / DeepSeek）就从最前面失配，后面整段历史都变成未命中。
/// 旧实现窗口 12 轮、每 4 轮对齐一次：每 4 轮就要重付一次整段历史前缀，
/// 摊到每轮比尾部那 400 token 贵一个量级。
///
/// 现在：读取全部**未被压缩**的原文；只受 `ACTOR_READ_TOKEN_BUDGET` 约束，
/// 超预算的部分由压缩（82% 触发、降到 15%）搬进摘要，而不是读取侧丢弃。
/// 压缩已经把落库量控制在预算 82% 以下，正常路径这个上限永远不会生效。
pub(crate) const ACTOR_READ_TOKEN_BUDGET: usize = 96 * 1024;

/// 读取侧异常保护的**下限**：即使碰到异常巨大的单条消息，也至少保留最近 12 轮。
///
/// 只防「一条消息自己就超过读取预算」时把历史读成空——那种情况下宁可超预算
/// 也要给模型留几轮上下文，否则它会完全失忆。
pub(crate) const ACTOR_READ_MIN_TURNS: usize = 12;

/// 事实层（远期结论）的独立预算。
///
/// facts 是"这个人是谁"的权威结论，**不参与原文淘汰**：
/// 以前 facts 和 turns 共用一个预算，超了会先把最旧的 facts 删掉，
/// 等于把最需要长期保留的一层最先丢掉。
pub(crate) const ACTOR_FACTS_BUDGET: usize = 800;

/// 单人事实条数上限，防止模型把零碎观察无限追加成一堵墙。
pub(crate) const MAX_FACTS_PER_ACTOR: usize = 30;

/// 原文硬上限：正常靠压缩回落，压缩失败时用它兜底，避免无限增长。
///
/// 取预算的 150%：正常触发线是 82%，真到 150% 说明压缩已经连续失败，
/// 此时宁可丢最旧的几句，也不能让表无限涨。个人与大厅共用。
pub(crate) const HARD_CAP_PERCENT: usize = 150;
/// 摘要单次生成允许的最大 token。
/// 摘要的输出预算。
///
/// 原来只有 800：模型一开思考就把预算吃光（线上实测 reasoning_tokens 恰好 800），
/// 正文一个字都出不来，status 变成 incomplete(reason=max_output_tokens)，
/// 压缩因此永远失败、每轮白烧一次调用。
/// 128k 是模型实测支持的上限（16k/32k/64k 也都能过），留足思考空间。
pub(crate) const SUMMARY_MAX_TOKENS: u32 = 131_072;

/// 事实抽取的输出预算。
///
/// 抽取本身只要几十 token 的 JSON，但服务端开启 thinking 后 reasoning 会先花预算：
/// 线上 256 时实测 reasoning_tokens=256、output 为空，整轮记忆被静默丢弃。
/// 给到 2048 后即使思考很久也留得住正文；深寻这类模型单价低，多给的代价可以忽略。
pub(crate) const FACT_EXTRACT_MAX_TOKENS: u32 = 2_048;

/// 事实抽取与摘要的超时。
///
/// 全局的 `timing.external.ai_request_timeout_ms` 是 35 秒，那是给主聊天的；
/// 抽取只是 256 token 的 JSON，用同一个预算会让一次卡住的请求长期占着队列。
/// 抽不到事实只影响「记住多少」，不影响回复，本来就允许失败。
pub(crate) const FACT_EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// 一次提问的最大字符数；超出直接拒绝，避免模型对着半句话编造。
pub(crate) const MAX_QUESTION_CHARS: usize = 200;
/// 同一个人两次触发之间的冷却。
pub(crate) const DEFAULT_COOLDOWN_SECONDS: u64 = 8;
/// 同时记录冷却状态的玩家上限。
pub(crate) const MAX_TRACKED_ACTORS: usize = 512;
/// 单条事实的最大字符数，防止模型把整段对话塞成一条。
pub(crate) const MAX_FACT_CHARS: usize = 120;
