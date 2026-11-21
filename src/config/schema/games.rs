//! 娱乐玩法段 schema：成语接龙、斗地主、谁是卧底、海龟汤、数字炸弹。
//!
//! 每个玩法一段，字段都只影响该玩法的规则与播报，不牵动识别链路。
//! 字段构造器（`db_live` 等）与段内小节推导都在父模块，这里只声明字段。

use super::{ConfigFieldSchema, FieldKind, MAX_STABLE_COUNT, MAX_TIMEOUT_MS, int};

/// idiom_chain 段：成语接龙。
pub(super) fn idiom_chain_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用接龙",
            FieldKind::Bool,
            "是否启用项目内置成语词库的接龙功能",
        ),
        ConfigFieldSchema::db_idle_reload(
            "lexicon_path",
            "词库路径",
            FieldKind::Path,
            "成语词库路径；相对路径以主程序 EXE 所在目录为基准",
        ),
        ConfigFieldSchema::db_idle_reload(
            "history_limit",
            "历史上限",
            int(1, 10_000),
            "最多保留多少个最近成语用于会话历史；本局完整已用集合始终禁止重复",
        ),
        ConfigFieldSchema::db_idle_reload(
            "idle_timeout_seconds",
            "空闲超时",
            int(1, 86_400),
            "多久无人接词就自动结束本局；启用接龙时须大于 0，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "allow_consecutive_player",
            "允许连续接词（兼容字段）",
            FieldKind::Bool,
            "为兼容旧配置保留；现在始终允许同一玩家连续接词",
        ),
        ConfigFieldSchema::db_idle_reload(
            "allow_anyone_stop",
            "允许任意结束",
            FieldKind::Bool,
            "false 时仅开局玩家可使用 #结束",
        ),
    ]
}

/// landlord 段：斗地主。
pub(super) fn landlord_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用斗地主",
            FieldKind::Bool,
            "三名好友参与的单局斗地主娱乐模块",
        ),
        ConfigFieldSchema::db_idle_reload(
            "lobby_timeout_seconds",
            "组局超时",
            int(1, 86_400),
            "等待三人组局的最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "turn_timeout_seconds",
            "出牌时间",
            int(1, 86_400),
            "每名玩家的出牌时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "trustee_after_timeouts",
            "托管触发次数",
            int(1, MAX_STABLE_COUNT),
            "连续超时多少次后进入自动托管；启用斗地主时须大于 0",
        ),
        ConfigFieldSchema::db_idle_reload(
            "hand_cooldown_seconds",
            "手牌查询冷却",
            int(0, 86_400),
            "好友私聊 #手牌 的单人查询冷却，单位秒",
        ),
    ]
}

/// undercover 段：谁是卧底。
pub(super) fn undercover_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用卧底",
            FieldKind::Bool,
            "谁是卧底娱乐模块；启用前复制 undercover.example.yaml 为 undercover.yaml 并审核词对",
        ),
        ConfigFieldSchema::db_idle_reload(
            "word_bank_path",
            "词库路径",
            FieldKind::Path,
            "词对词库路径（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "used_state_path",
            "已用词记录路径",
            FieldKind::Path,
            "已使用词对持久化路径（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "min_players",
            "最少玩家",
            int(4, 11),
            "开局最少玩家数；启用时需 4..=max_players",
        ),
        ConfigFieldSchema::db_idle_reload(
            "double_min_players",
            "双卧底最少玩家",
            int(6, 11),
            "双卧底模式所需最少玩家数；启用时需 6..=max_players",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_players",
            "最多玩家",
            int(4, 11),
            "开局最多玩家数；4..=11 且 >= min_players",
        ),
        ConfigFieldSchema::db_idle_reload(
            "lobby_timeout_seconds",
            "大厅等待超时",
            int(1, 86_400),
            "等待玩家进入大厅的最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "phase_timeout_seconds",
            "阶段超时",
            int(1, 86_400),
            "每个游戏阶段的最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "progress_interval_seconds",
            "进度公布间隔",
            int(1, 86_400),
            "投票阶段公布未投票位置的间隔，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "description_max_width",
            "描述最大宽度",
            int(1, 1000),
            "单条公屏 #内容 的最大显示宽度",
        ),
    ]
}

/// turtle_soup 段：海龟汤。
pub(super) fn turtle_soup_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用海龟汤",
            FieldKind::Bool,
            "海龟汤娱乐模块；启用前必须准备正式题库并配置独立 AI Provider",
        ),
        ConfigFieldSchema::db_idle_reload(
            "question_bank_path",
            "题库路径",
            FieldKind::Path,
            "正式题库路径，格式参考 turtle_soup.example.yaml（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "used_state_path",
            "已用题目记录路径",
            FieldKind::Path,
            "永久已使用题目记录；程序不提供重置接口，只能手动修改此文件",
        ),
        ConfigFieldSchema::db_idle_reload(
            "idle_timeout_seconds",
            "空闲超时",
            int(1, 86_400),
            "进行中多久没有新问题就自动结束，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_session_seconds",
            "单局最长时长",
            int(1, 86_400),
            "单局最长时间，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_concurrency",
            "并发裁决上限",
            int(1, 1024),
            "AI 同时裁决的最大请求数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_pending",
            "待裁决上限",
            int(1, 1024),
            "除正在裁决外，最多等待多少条问题",
        ),
        ConfigFieldSchema::db_idle_reload(
            "batch_max_parts",
            "批量答案分段上限",
            int(1, 1024),
            "单个昵称的一份批量长答案最多暂存多少段；每段仍受游戏聊天长度限制",
        ),
        ConfigFieldSchema::db_idle_reload(
            "nickname_stable_count",
            "昵称稳定确认次数",
            int(0, MAX_STABLE_COUNT),
            "昵称 OCR 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "content_stable_count",
            "正文稳定确认次数",
            int(0, MAX_STABLE_COUNT),
            "正文 OCR 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
        ),
        ConfigFieldSchema::db_idle_reload(
            "request_timeout_seconds",
            "AI 请求超时",
            int(1, 86_400),
            "单次 AI 请求超时，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_count",
            "重试次数",
            int(0, MAX_STABLE_COUNT),
            "单次裁决请求失败后的重试次数；实际最多请求 retry_count + 1 次",
        ),
        ConfigFieldSchema::db_idle_reload(
            "retry_delay_ms",
            "重试间隔",
            int(0, MAX_TIMEOUT_MS),
            "AI 请求重试间隔，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "custom_prompt",
            "自定义规则",
            FieldKind::Text,
            "附加在裁决提示词后的房间规则；不要要求模型输出自由解释或汤底",
        ),
        ConfigFieldSchema::db_idle_reload(
            "system_prompt",
            "AI 系统提示词",
            FieldKind::Text,
            "海龟汤 AI 的系统提示词；留空使用内置默认值，可使用 {{role}}、{{verification_rules}}",
        ),
        ConfigFieldSchema::db_idle_reload(
            "review_prompt",
            "AI 裁决提示词",
            FieldKind::Text,
            "海龟汤 AI 的裁决模板；可使用 {{verification_instruction}}、{{context}}、{{custom_prompt}}",
        ),
        ConfigFieldSchema::db_idle_reload(
            "model",
            "专用模型",
            FieldKind::ModelName,
            "海龟汤专用模型；留空沿用 ai.model。裁决偏重复杂逻辑，可用更强模型",
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
            "海龟汤裁决的思考强度；留空跟随服务端默认。裁决逻辑复杂，通常用中或高",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_tokens",
            "最大 Token",
            int(1, 1_000_000),
            "单次裁决允许生成的最大 Token",
        ),
    ]
}

/// number_bomb 段：数字炸弹。
pub(super) fn number_bomb_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "enabled",
            "启用数字炸弹",
            FieldKind::Bool,
            "大厅发起、轮流猜数字的娱乐玩法",
        ),
        ConfigFieldSchema::db_idle_reload(
            "min",
            "范围下界",
            int(0, 1_000_000_000),
            "炸弹与猜测范围的固定下界；必须小于上限",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max",
            "默认上限",
            int(1, 1_000_000_000),
            "不超过 5 人时的范围上限；必须大于下界",
        ),
        ConfigFieldSchema::db_idle_reload(
            "turn_timeout_seconds",
            "回合超时",
            int(1, 86_400),
            "当前玩家多久未猜就自动在剩余范围内随机猜一个，单位秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "min_players",
            "最少玩家",
            int(2, 64),
            "开局所需最少玩家数；必须至少为 2",
        ),
        ConfigFieldSchema::db_idle_reload(
            "max_players",
            "最多玩家",
            int(2, 64),
            "房间最多玩家数；必须不小于最少玩家数",
        ),
        ConfigFieldSchema::db_idle_reload(
            "extra_range_per_player",
            "超员上限加成",
            int(0, 1_000_000),
            "超过 5 人后，每多 1 人给默认上限增加多少；0 表示不加",
        ),
        ConfigFieldSchema::db_idle_reload(
            "upper_bound_cap",
            "上限封顶",
            int(1, 1_000_000_000),
            "人数再多，范围上限也不会超过这个值；不能小于默认上限",
        ),
    ]
}
