//! AI 相关段 schema：AI 连接、点歌 AI、歌曲审核、AI 聊天、语音 TTS。
//!
//! AI 连接是共用底座（点歌、歌曲审核、海龟汤共用一份协议/密钥/模型），
//! 其余段只在各自业务里覆盖提示词与模型；TTS 独立于聊天模型，走 fish.audio。
//! 字段构造器（`db_live` 等）与段内小节推导都在父模块，这里只声明字段。

use super::{ConfigFieldSchema, FieldKind, MAX_STABLE_COUNT, MAX_TIMEOUT_MS, enum_of, float, int};

/// ai 段：统一 AI 连接配置（点歌、歌曲审核、海龟汤共用）。
pub(super) fn ai_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "provider",
            "服务商",
            FieldKind::Enum(vec![
                ("mimo".to_string(), "小米 MiMo".to_string()),
                ("openai".to_string(), "OpenAI".to_string()),
                ("deepseek".to_string(), "DeepSeek".to_string()),
                ("custom".to_string(), "自定义网关".to_string()),
            ]),
            "所有 AI 功能共用的服务商。二段式配置：先选服务商，再按服务商填写参数；custom 必须填写 endpoint 与 model",
        ),
        ConfigFieldSchema::db_idle_reload(
            "api_key",
            "API Key",
            FieldKind::Secret,
            "所有 AI 功能共用的 API Key。留空表示全部 AI 功能关闭；不要提交到 Git，也不要放进 URL 查询参数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "endpoint",
            "接口地址",
            FieldKind::String,
            "Chat Completions 地址，通常以 /chat/completions 结尾；Responses 地址默认由它推导。非 custom 留空时使用默认地址",
        ),
        ConfigFieldSchema::db_idle_reload(
            "responses_endpoint",
            "Responses 地址",
            FieldKind::String,
            "可选的 Responses 地址；留空时把接口地址的 /chat/completions 换成 /responses",
        ),
        ConfigFieldSchema::db_idle_reload(
            "model",
            "默认模型",
            FieldKind::ModelName,
            "所有 AI 功能的默认模型；各功能可单独覆盖，custom 必须填写",
        ),
        ConfigFieldSchema::db_idle_reload(
            "http_proxy",
            "代理",
            FieldKind::String,
            "可选的独立 HTTP(S) 代理；作用于全部 AI 功能，留空时沿用环境代理设置",
        ),
        ConfigFieldSchema::db_idle_reload(
            "wire_api",
            "线上协议",
            FieldKind::Enum(vec![
                ("responses".to_string(), "Responses（推荐）".to_string()),
                (
                    "chat_completions".to_string(),
                    "Chat Completions".to_string(),
                ),
            ]),
            "默认走 Responses。仅当网关（Kimi、GLM、部分自建网关）只支持 Chat Completions 时切换",
        ),
        ConfigFieldSchema::db_idle_reload(
            "extra_body",
            "第三方兼容字段",
            FieldKind::Object,
            "服务商专属请求参数。Web 对 MiMo/OpenAI/DeepSeek 提供官方字段结构化表单；只有 custom 使用自由 JSON。与程序固定字段同名时程序值优先",
        ),
        ConfigFieldSchema::db_idle_reload(
            "reasoning_rules",
            "思考强度适配规则",
            FieldKind::Object,
            "按模型改写或拒绝某个思考强度：不同模型接受的 effort 词不同，写错会 400。规则按 from + 模型匹配（exact/prefix/suffix），to 写 deny 表示不支持。匹配不到的模型原样发送；若上游仍以 400 拒绝，程序会自动去掉该字段重试一次",
        ),
    ]
}

/// song_ai 段：点歌 AI 的提示词与本地推荐策略。
pub(super) fn song_ai_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "model",
            "专用模型",
            FieldKind::ModelName,
            "点歌专用模型；留空沿用 ai.model。点歌偏重低延迟",
        ),
        ConfigFieldSchema::db_idle_reload(
            "reasoning",
            "思考强度",
            FieldKind::Enum(vec![
                ("unset".to_string(), "不指定（服务端默认）".to_string()),
                ("minimal".to_string(), "最低".to_string()),
                ("low".to_string(), "低".to_string()),
                ("medium".to_string(), "中".to_string()),
                ("high".to_string(), "高".to_string()),
            ]),
            "点歌匹配的思考强度；留空跟随服务端默认。点歌偏重低延迟，通常用最低",
        ),
        ConfigFieldSchema::db_live(
            "local_recommend",
            "本地曲库推荐",
            FieldKind::Bool,
            "在线匹配偏低时额外给出本地曲库推荐；默认开启，保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "local_recommend_min_score",
            "本地推荐触发分数",
            float(0.0, 1.0),
            "在线候选分数低于该值才比较本地曲库；只有本地分数更高才会推荐",
        ),
        ConfigFieldSchema::db_idle_reload(
            "system_prompt",
            "系统提示词",
            FieldKind::Text,
            "点歌 AI 的系统提示词；留空使用内置默认值",
        ),
        ConfigFieldSchema::db_idle_reload(
            "recognize_prompt",
            "识别提示词",
            FieldKind::Text,
            "识别点歌文本的模板；可使用 {{text}}",
        ),
        ConfigFieldSchema::db_idle_reload(
            "match_prompt",
            "匹配提示词",
            FieldKind::Text,
            "判断歌曲匹配的模板；可使用 {{request}}、{{songName}}、{{songSinger}}",
        ),
        ConfigFieldSchema::db_idle_reload(
            "candidate_pick_prompt",
            "选歌提示词",
            FieldKind::Text,
            "从候选中选歌的模板；可使用 {{request}}、{{preferAccompaniment}}、{{preferAccompanimentInstruction}}、{{candidates}}",
        ),
    ]
}

/// song_review 段：候选歌曲 AI 审核。
pub(super) fn song_review_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用审核",
            FieldKind::Bool,
            "候选歌曲 AI 审核；只审核最终候选歌曲/URI，控制台来源最高权限免审",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_allowed_level",
            "最高允许等级",
            int(1, 10),
            "审核返回 level 为 1-10，超过该阈值会拒绝点歌",
        ),
        ConfigFieldSchema::db_idle_reload(
            "failure_policy",
            "失败策略",
            enum_of(&[("reject", "拒绝"), ("allow", "放行")]),
            "审核服务多次失败后的策略：reject 拒绝，allow 放行并写警告日志",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_count",
            "重试次数",
            int(0, MAX_STABLE_COUNT),
            "审核请求失败后的重试次数；实际最多请求 retry_count + 1 次",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_delay_ms",
            "重试间隔",
            int(0, MAX_TIMEOUT_MS),
            "审核重试间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "reply_reason_max_chars",
            "拒绝原因字符上限",
            int(1, 10_000),
            "游戏内拒绝原因最多显示的字符数；完整原因写日志",
        ),
        ConfigFieldSchema::db_idle_reload(
            "system_prompt",
            "系统提示词",
            FieldKind::Text,
            "歌曲审核 AI 的系统提示词；留空使用内置默认值",
        ),
        ConfigFieldSchema::db_idle_reload(
            "review_prompt",
            "审核提示词",
            FieldKind::Text,
            "歌曲审核提示词模板；可使用 {{policy_prompt}}、{{custom_prompt}}、{{candidate}}",
        ),
        ConfigFieldSchema::db_idle_reload(
            "policy_prompt",
            "审核条件",
            FieldKind::Text,
            "审核条件；可以按房间氛围修改，但不要要求模型改变 JSON 输出格式",
        ),
        ConfigFieldSchema::db_idle_reload(
            "custom_prompt",
            "追加规则",
            FieldKind::Text,
            "追加审核规则；会附加在 policy_prompt 后面，用于临时补充口径",
        ),
        ConfigFieldSchema::db_idle_reload(
            "model",
            "审核模型",
            FieldKind::ModelName,
            "审核专用模型；留空沿用 ai.model。审核偏重判断质量，建议用更强模型",
        ),
        ConfigFieldSchema::db_idle_reload(
            "reasoning",
            "思考强度",
            FieldKind::Enum(vec![
                ("unset".to_string(), "不指定（服务端默认）".to_string()),
                ("minimal".to_string(), "最低".to_string()),
                ("low".to_string(), "低".to_string()),
                ("medium".to_string(), "中".to_string()),
                ("high".to_string(), "高".to_string()),
            ]),
            "歌曲审核的思考强度；审核偏重判断质量，通常用中或高",
        ),
        ConfigFieldSchema::db_idle_reload(
            "web_search",
            "联网审核",
            FieldKind::Bool,
            "是否要求模型联网搜索后再判断；需要 ai.wire_api=responses，网关不支持时关闭",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_tokens",
            "最大 Token",
            int(1, 1_000_000),
            "单次审核允许生成的最大 Token",
        ),
    ]
}

/// ai_chat 段：AI 聊天与轻量记忆。
pub(super) fn ai_chat_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "enabled",
            "启用 AI 聊天",
            FieldKind::Bool,
            "关键词触发的大厅聊天机器人；未配置 ai.api_key 时不会回复",
        ),
        ConfigFieldSchema::db_live(
            "keyword",
            "触发关键词",
            FieldKind::String,
            "消息包含该关键词时触发；默认 橘子",
        ),
        ConfigFieldSchema::db_live(
            "trigger_mode",
            "触发方式",
            FieldKind::Enum(vec![
                ("prefix".to_string(), "以关键词开头".to_string()),
                ("contains".to_string(), "包含关键词".to_string()),
            ]),
            "默认只在消息以关键词开头时触发",
        ),
        ConfigFieldSchema::db_live(
            "min_access",
            "触发权限",
            FieldKind::Enum(vec![
                ("everyone".to_string(), "所有人".to_string()),
                ("friend".to_string(), "好友及以上".to_string()),
                ("admin".to_string(), "管理员及以上".to_string()),
                ("owner".to_string(), "仅所有者".to_string()),
            ]),
            "谁能让 AI 回应；身份映射里查不到的人，除「所有人」外一律拒绝",
        ),
        ConfigFieldSchema::db_live(
            "actor_token_budget",
            "单人近期原文预算",
            int(1_000, 200_000),
            "每个玩家的近期问答原文 token 上限；超过 82% 时把较早原文压进中期摘要（压到 15%），历史只追加不滑动",
        ),
        ConfigFieldSchema::db_live(
            "actor_summary_budget",
            "单人中期摘要预算",
            int(200, 50_000),
            "每个玩家的中期摘要 token 上限；摘要滚动覆盖，不是硬截断",
        ),
        ConfigFieldSchema::db_live(
            "actor_history_token_budget",
            "个人历史注入预算",
            int(1_000, 200_000),
            "个人历史作为 messages 注入的 token 上限；超出从最旧裁剪、保最近；默认 12000",
        ),
        ConfigFieldSchema::db_live(
            "inject_actor_history",
            "注入个人问答历史",
            FieldKind::Bool,
            "默认注入个人问答历史，hall 只含其他玩家/对别人回复；关闭后个人历史不注入，仅保留 facts/摘要/大厅上下文。",
        ),
        ConfigFieldSchema::db_live(
            "hall_token_budget",
            "大厅近期原文预算",
            int(1_000, 400_000),
            "大厅共享记忆的原文 token 上限；记录所有人在大厅的发言",
        ),
        ConfigFieldSchema::db_live(
            "hall_summary_budget",
            "大厅中期摘要预算",
            int(200, 100_000),
            "大厅共享记忆的中期摘要 token 上限",
        ),
        ConfigFieldSchema::db_live(
            "bot_token_budget",
            "机器人记忆预算",
            int(1_000, 400_000),
            "机器人自身发言的全局 token 上限",
        ),
        ConfigFieldSchema::db_live(
            "cooldown_seconds",
            "冷却秒数",
            int(0, 86_400),
            "同一玩家两次触发之间的最小间隔",
        ),
        ConfigFieldSchema::db_live(
            "allow_commands",
            "允许执行命令",
            FieldKind::Bool,
            "允许 AI 通过原生工具调用执行命令；程序按提问者身份与命令权限策略复核后执行",
        ),
        ConfigFieldSchema::db_live(
            "model",
            "专用模型",
            FieldKind::ModelName,
            "留空沿用 ai.model",
        ),
        ConfigFieldSchema::db_live(
            "reasoning",
            "思考强度",
            FieldKind::Enum(vec![
                ("unset".to_string(), "不指定（服务端默认）".to_string()),
                ("minimal".to_string(), "最低".to_string()),
                ("low".to_string(), "低".to_string()),
                ("medium".to_string(), "中".to_string()),
                ("high".to_string(), "高".to_string()),
            ]),
            "AI 聊天的思考强度；留空跟随服务端默认",
        ),
        ConfigFieldSchema::db_live(
            "max_tokens",
            "最大 Token",
            int(1, 100_000),
            "单次回答允许生成的最大 token",
        ),
        ConfigFieldSchema::db_live(
            "system_prompt",
            "系统提示词",
            FieldKind::Text,
            "留空使用内置默认值",
        ),
        ConfigFieldSchema::db_live(
            "answer_prompt",
            "回答模板",
            FieldKind::Text,
            "可使用 {{keyword}}、{{facts}}、{{summary}}、{{hall}}、{{asker}}；{{hall}} 只含其他玩家的发言与 BOT 对他人的回复（显示为「你（角色名）：内容」），当前玩家自己的问答默认作为独立角色消息传输（{{history}} 仅作兼容占位）；{{question}}、{{recent_actions}}、{{commands}} 由程序统一追加到末尾，模板里写不写都不会重复",
        ),
        ConfigFieldSchema::db_live(
            "extract_prompt",
            "事实抽取提示词",
            FieldKind::Text,
            "留空使用内置默认值；从每轮对话中抽取值得长期记住的信息",
        ),
        ConfigFieldSchema::db_live(
            "tool_prompts.look",
            "look 工具提示词",
            FieldKind::Text,
            "只在实际暴露 look 工具时注入；留空使用内置默认文案",
        ),
        ConfigFieldSchema::db_live(
            "tool_prompts.speak",
            "speak 工具提示词",
            FieldKind::Text,
            "只在实际暴露 speak 工具时注入；留空使用内置默认文案",
        ),
        ConfigFieldSchema::db_live(
            "tool_prompts.command",
            "命令工具提示词",
            FieldKind::Text,
            "只在 execute_game_command 暴露时注入；留空使用内置默认文案",
        ),
        ConfigFieldSchema::db_live(
            "vision.enabled",
            "识别截图",
            FieldKind::Bool,
            "开启 AI 识图；模型不支持图片时自动降级为纯文本",
        ),
        ConfigFieldSchema::db_live(
            "vision.mode",
            "识图调用方式",
            FieldKind::Enum(vec![
                ("auto".to_string(), "每轮自动附图".to_string()),
                ("tool".to_string(), "仅 look 工具".to_string()),
                ("both".to_string(), "自动附图 + look 工具".to_string()),
            ]),
            "auto 每轮自动附一级大厅截图；tool 不自动附图，由模型按需调用 look；both 两者都启用；enabled=false 时全部关闭",
        ),
        ConfigFieldSchema::db_live(
            "vision.direct_to_main",
            "直接给主模型看图",
            FieldKind::Bool,
            "仅当 AI 调用 look 时把当前截图直接给主聊天模型，不每轮自动截图；需主模型支持图片；auto/both 仍按原模式自动附图；截图失败或不支持图片时回退 look 视觉子模型",
        ),
        ConfigFieldSchema::db_live(
            "vision.capture_scope",
            "截图范围",
            FieldKind::Enum(vec![(
                "primary_screen".to_string(),
                "整个一级大厅".to_string(),
            )]),
            "当前仅支持整个一级大厅；整屏可能包含 UID/HUD，日志不会保存截图内容",
        ),
        ConfigFieldSchema::db_live(
            "vision.max_edge",
            "截图长边上限",
            int(512, 2_048),
            "截图长边上限（默认 1600）；超过时等比缩小，服务端仍可自行降采样",
        ),
        ConfigFieldSchema::db_live(
            "vision.jpeg_quality",
            "截图 JPEG 质量",
            int(40, 100),
            "JPEG 编码质量（40..=100）；越大越清晰、字节越多，默认 100",
        ),
        ConfigFieldSchema::db_live(
            "vision.max_bytes",
            "截图字节上限",
            int(100_000, 20_000_000),
            "编码后超过该字节数会降采样/降质量，仍超限则本轮不附带图片",
        ),
        ConfigFieldSchema::db_live(
            "vision.timeout_ms",
            "截图超时",
            int(100, 5_000),
            "截图与编码的截止时间，单位毫秒；超时降级纯文本",
        ),
        ConfigFieldSchema::db_live(
            "vision.model",
            "look 子请求模型",
            FieldKind::ModelName,
            "只用于 look 的视觉子请求；主聊天始终使用 ai_chat.model；留空沿用 ai_chat.model",
        ),
        ConfigFieldSchema::db_live(
            "vision.allow_full_screen",
            "允许整屏截图",
            FieldKind::Bool,
            "用户明确选择整屏时开启；整屏可能包含昵称/UID/HUD",
        ),
        ConfigFieldSchema::db_live(
            "ai_reply.mode",
            "AI回复 生成方式",
            FieldKind::Enum(vec![
                ("append".to_string(), "补充一句（模板为主）".to_string()),
                ("rewrite".to_string(), "改写整条（AI 为主）".to_string()),
            ]),
            "回复文案里写入 {AI回复} 时的生成方式；改写模式会让 AI 重写整条，事实可能被改动，默认补充一句",
        ),
        ConfigFieldSchema::db_live(
            "ai_reply.include_template",
            "把回复内容给 AI 看",
            FieldKind::Bool,
            "开启后 AI 能看到已填充变量的真实回复（如「小明点播的《晴天》已排上」）；关闭只能看到占位符骨架，不建议关闭",
        ),
        ConfigFieldSchema::db_live(
            "ai_reply.include_command",
            "把命令名给 AI 看",
            FieldKind::Bool,
            "开启后 AI 知道自己在响应哪条命令，如 @QQ点歌",
        ),
        ConfigFieldSchema::db_live(
            "ai_reply.system_prompt",
            "系统提示词",
            FieldKind::Text,
            "留空使用内置默认值；用于约束 AI 的语气与输出格式",
        ),
        ConfigFieldSchema::db_live(
            "ai_reply.user_prompt",
            "用户提示词",
            FieldKind::Text,
            "可使用 {{command}}、{{template}}；留空使用内置默认值",
        ),
        ConfigFieldSchema::db_live(
            "ai_reply.max_tokens",
            "生成上限",
            FieldKind::Int { min: 16, max: 4096 },
            "单次生成的最大 token 数；回复本来就很短，默认 120 足够",
        ),
        ConfigFieldSchema::db_live(
            "ai_reply.timeout_seconds",
            "超时秒数",
            FieldKind::Int { min: 1, max: 120 },
            "超过该时长按失败处理，AI回复 留空继续发送",
        ),
        ConfigFieldSchema::db_live_grouped(
            "protocols.memory",
            "记忆与回复规范",
            FieldKind::Text,
            "留空用内置；讲「记忆只是参考资料、回答要简短」",
            "正文协议",
        ),
        ConfigFieldSchema::db_live_grouped(
            "protocols.identity",
            "身份与归属规范",
            FieldKind::Text,
            "留空用内置；讲「本轮只有一个人在说话、别人的名字不算你的」",
            "正文协议",
        ),
        ConfigFieldSchema::db_live_grouped(
            "protocols.time",
            "时间规范",
            FieldKind::Text,
            "留空用内置",
            "正文协议",
        ),
        ConfigFieldSchema::db_live_grouped(
            "protocols.action",
            "动作规范",
            FieldKind::Text,
            "留空用内置",
            "正文协议",
        ),
        ConfigFieldSchema::db_live_grouped(
            "protocols.reply",
            "回复格式（JSON 契约）",
            FieldKind::Text,
            "必须保留 \"action\" 与 \"text\" 两个键名，否则回复会被丢弃",
            "正文协议",
        ),
        ConfigFieldSchema::db_live_grouped(
            "protocols.tool",
            "工具调用协议",
            FieldKind::Text,
            "留空用内置；不要写「禁止调用工具」这类否定",
            "正文协议",
        ),
        ConfigFieldSchema::db_live_grouped(
            "tool_descriptions.look",
            "look 工具描述",
            FieldKind::Text,
            "进 tools 数组的短描述；留空用内置",
            "工具描述",
        ),
        ConfigFieldSchema::db_live_grouped(
            "tool_descriptions.speak",
            "speak 工具描述",
            FieldKind::Text,
            "进 tools 数组的短描述；留空用内置；模型相关的示例后缀由代码追加",
            "工具描述",
        ),
        ConfigFieldSchema::db_live_grouped(
            "tool_descriptions.command",
            "命令工具描述",
            FieldKind::Text,
            "进 tools 数组的短描述；留空用内置",
            "工具描述",
        ),
        ConfigFieldSchema::db_live_grouped(
            "summary_prompt",
            "记忆压缩提示词",
            FieldKind::Text,
            "可用 {{previous}}、{{records}}；留空用内置",
            "长文本提示词",
        ),
        ConfigFieldSchema::db_live_grouped(
            "vision_prompt",
            "视觉子模型提示词",
            FieldKind::Text,
            "可用 {{question}}、{{instruction}}；留空用内置",
            "长文本提示词",
        ),
    ]
}

pub(super) fn tts_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "enabled",
            "启用 TTS",
            FieldKind::Bool,
            "允许 AI 聊天工具 speak 请求 fish.audio；未配置 api_key 时不发请求",
        ),
        ConfigFieldSchema::db_idle_reload(
            "endpoint",
            "fish 接口地址",
            FieldKind::String,
            "fish.audio 基础地址；默认 https://api.fish.audio，实际请求 {endpoint}/v1/tts",
        ),
        ConfigFieldSchema::db_idle_reload(
            "http_proxy",
            "TTS 代理",
            FieldKind::String,
            "可选独立 HTTP(S)/SOCKS 代理，作用于 fish 音色目录与 /v1/tts 合成；留空沿用环境代理",
        ),
        ConfigFieldSchema::db_idle_reload(
            "api_key",
            "fish API Key",
            FieldKind::Secret,
            "fish.audio API key，只以 Authorization Bearer 发送；空闲后整程序重启时生效",
        ),
        ConfigFieldSchema::db_live(
            "model",
            "TTS 模型",
            FieldKind::ModelName,
            "作为 HTTP model 头发送，默认 s2.1-pro",
        ),
        ConfigFieldSchema::db_live(
            "reference_id",
            "参考音色 ID",
            FieldKind::String,
            "fish reference_id；留空时可使用 voice",
        ),
        ConfigFieldSchema::db_live(
            "voice",
            "音色别名",
            FieldKind::String,
            "reference_id 为空时作为 fish reference_id 发送",
        ),
        ConfigFieldSchema::db_live(
            "format",
            "音频格式",
            FieldKind::Enum(vec![("wav".to_string(), "WAV".to_string())]),
            "当前仅支持 WAV；SpeechPlayer 直接解码 WAV",
        ),
        ConfigFieldSchema::db_live(
            "duck_volume",
            "说话时音乐音量",
            int(0, 100),
            "说话期间实际输出增益百分比；100=不压低，默认 30",
        ),
        ConfigFieldSchema::db_live(
            "duck_fade_ms",
            "duck 淡入淡出",
            int(0, 2_000),
            "压低/恢复增益的渐变时长，默认 200ms",
        ),
        ConfigFieldSchema::db_live(
            "max_chars",
            "单条文本上限",
            int(1, 2_000),
            "超过该字符数截断，控制费用与延迟，默认 200",
        ),
        ConfigFieldSchema::db_live(
            "timeout_ms",
            "请求超时",
            int(1_000, 60_000),
            "fish 请求超时，默认 8000ms；超时静默跳过语音",
        ),
        ConfigFieldSchema::db_live(
            "queue_max",
            "语音队列上限",
            int(1, 64),
            "排队等待播放的语音条数上限，默认 8",
        ),
        ConfigFieldSchema::db_live(
            "trigger_scope",
            "触发范围",
            FieldKind::Enum(vec![("ai_chat".to_string(), "仅 AI 聊天".to_string())]),
            "当前仅 AI 聊天工具 speak 可触发，普通回复不自动朗读",
        ),
    ]
}
