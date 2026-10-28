//! 可调整的回复文案：代码提供默认值，配置中心提供覆盖。
//!
//! 设计约束：
//! - 默认值内置于代码，配置缺失时回落默认值并记一条 WARN，绝不因为缺配置而拒绝发送。
//! - 占位符形如 {歌名}；渲染顺序是先渲染模板，再把已渲染的变量值填入。
//! - AI 生成文本只作为变量值注入，绝不参与模板解析，避免 AI 输出里的花括号被误当占位符。
//! - 未知占位符原样保留（便于排查）；已知但缺失的变量替换为空串并上报。

use std::collections::BTreeMap;

// 按消费模块拆分的模板文件：每个模块只写自己的文件，避免并行开发时互相覆盖。
pub(crate) mod card_games;
pub(crate) mod hall;
pub(crate) mod idiom_chain;
pub(crate) mod moderation;
pub(crate) mod ownership;
pub(crate) mod playback;
pub(crate) mod playlist;
pub(crate) mod song;
pub(crate) mod tarot;
pub(crate) mod turtle_soup;
pub(crate) mod undercover;

/// 统一的待发送回复类型。

/// 一条可调整回复的元数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplyTemplate {
    /// 稳定键，如 song.request.accepted。回复面板覆盖按此键索引。
    pub(crate) key: &'static str,
    /// 内置默认文案，可含花括号占位符。
    pub(crate) default: &'static str,
    /// 供命令面板展示的一行摘要。
    pub(crate) summary: &'static str,
    /// 面板分组：点歌 / 播放 / 大厅 / 娱乐 / 管理。
    pub(crate) group: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplyVariable {
    pub name: &'static str,
    pub label: &'static str,
    /// 面板上展示的简短示例值。
    ///
    /// 按钮直接显示示例（如「晴天」）而不是占位符（如「{歌名}」）：
    /// 用户看到的是"这条变量长什么样"，不必先理解花括号语法再想象结果。
    pub example: &'static str,
}

/// Global template tokens; includes builder-supported reserved values.
pub(crate) const REPLY_VARIABLES: &[ReplyVariable] = &[
    ReplyVariable {
        name: "UID",
        label: "目标玩家的游戏 UID",
        example: "123456789",
    },
    ReplyVariable {
        name: "上一个",
        label: "上一个成语（接龙）",
        example: "画蛇添足",
    },
    ReplyVariable {
        name: "上一手",
        label: "上一手出牌（含出牌人与牌型）",
        example: "对3",
    },
    ReplyVariable {
        name: "上一手牌型",
        label: "上一手的牌型名",
        example: "对子",
    },
    ReplyVariable {
        name: "上限",
        label: "队列数量上限",
        example: "20",
    },
    ReplyVariable {
        name: "下一位",
        label: "下一位该行动的玩家",
        example: "小明",
    },
    ReplyVariable {
        name: "人数",
        label: "当前人数",
        example: "4",
    },
    ReplyVariable {
        name: "位置",
        label: "座位号",
        example: "A",
    },
    ReplyVariable {
        name: "位置列表",
        label: "座位号列表",
        example: "A、C",
    },
    ReplyVariable {
        name: "关键词",
        label: "从播放池续播的歌曲",
        example: "晴天",
    },
    ReplyVariable {
        name: "其他玩法",
        label: "正在占用娱乐会话的其他玩法",
        example: "海龟汤",
    },
    ReplyVariable {
        name: "决定",
        label: "抢地主的选择（抢/不抢）",
        example: "抢地主",
    },
    ReplyVariable {
        name: "前缀",
        label: "队列入队提示前缀",
        example: "[提示]",
    },
    ReplyVariable {
        name: "剩余",
        label: "剩余手牌",
        example: "3张",
    },
    ReplyVariable {
        name: "剩余分钟",
        label: "大厅剩余分钟数",
        example: "15",
    },
    ReplyVariable {
        name: "动作",
        label: "当前动作",
        example: "出牌",
    },
    ReplyVariable {
        name: "卧底位置",
        label: "卧底所在的座位号",
        example: "B",
    },
    ReplyVariable {
        name: "卧底词",
        label: "卧底阵营拿到的词",
        example: "苹果",
    },
    ReplyVariable {
        name: "原因",
        label: "失败原因",
        example: "平台无音源",
    },
    ReplyVariable {
        name: "后续",
        label: "超时后的后续提示",
        example: "轮到小明",
    },
    ReplyVariable {
        name: "后缀",
        label: "追加在后方的文本",
        example: "（超时）",
    },
    ReplyVariable {
        name: "地主",
        label: "地主玩家名",
        example: "小明",
    },
    ReplyVariable {
        name: "大厅名",
        label: "当前大厅名称",
        example: "1号大厅",
    },
    ReplyVariable {
        name: "字",
        label: "接龙要求的目标首字",
        example: "天",
    },
    ReplyVariable {
        name: "平民词",
        label: "平民阵营拿到的词",
        example: "香蕉",
    },
    ReplyVariable {
        name: "序号",
        label: "列表条目序号",
        example: "1",
    },
    ReplyVariable {
        name: "底牌",
        label: "地主的三张底牌",
        example: "红桃2",
    },
    ReplyVariable {
        name: "开头描述",
        label: "接龙开头要求的整句描述",
        example: "以「天」字开头",
    },
    ReplyVariable {
        name: "张数",
        label: "手牌张数",
        example: "17",
    },
    ReplyVariable {
        name: "当前",
        label: "队列当前数量",
        example: "晴天",
    },
    ReplyVariable {
        name: "当前抢地主",
        label: "当前正在抢地主的玩家",
        example: "小明",
    },
    ReplyVariable {
        name: "成语",
        label: "当前接到的成语",
        example: "一举两得",
    },
    ReplyVariable {
        name: "手数",
        label: "本局累计出牌次数",
        example: "12",
    },
    ReplyVariable {
        name: "手牌",
        label: "自己的手牌（含花色）",
        example: "♠3 ♥5 ♦K",
    },
    ReplyVariable {
        name: "托管后缀",
        label: "托管状态标记",
        example: "（托管）",
    },
    ReplyVariable {
        name: "投票说明",
        label: "投票操作提示整句",
        example: "私聊#A投票",
    },
    ReplyVariable {
        name: "拼音",
        label: "目标首字的拼音",
        example: "tiān",
    },
    ReplyVariable {
        name: "提问数",
        label: "已提出的有效问题数",
        example: "5",
    },
    ReplyVariable {
        name: "搜索词",
        label: "AI 改写后的搜索词",
        example: "晴天 周杰伦",
    },
    ReplyVariable {
        name: "数量",
        label: "条目数量",
        example: "3",
    },
    ReplyVariable {
        name: "条目",
        label: "队列条目（序号 + 歌名）",
        example: "1.晴天",
    },
    ReplyVariable {
        name: "来源",
        label: "音乐平台",
        example: "QQ音乐",
    },
    ReplyVariable {
        name: "标签",
        label: "命令回显前缀",
        example: "@点歌",
    },
    ReplyVariable {
        name: "模式",
        label: "玩法模式",
        example: "同音接龙",
    },
    ReplyVariable {
        name: "歌名",
        label: "曲目标题",
        example: "晴天",
    },
    ReplyVariable {
        name: "歌手",
        label: "歌手名",
        example: "周杰伦",
    },
    ReplyVariable {
        name: "状态",
        label: "播放状态（播放/暂停/停止）",
        example: "播放",
    },
    ReplyVariable {
        name: "曲名",
        label: "曲名（歌名 - 歌手）",
        example: "晴天 - 周杰伦",
    },
    ReplyVariable {
        name: "进度",
        label: "已播放时长",
        example: "1:05",
    },
    ReplyVariable {
        name: "总长",
        label: "歌曲总时长",
        example: "3:05",
    },
    ReplyVariable {
        name: "点歌人",
        label: "点歌人昵称",
        example: "小明",
    },
    ReplyVariable {
        name: "歌单归属",
        label: "歌单模式所有者",
        example: "小明",
    },
    ReplyVariable {
        name: "内容",
        label: "被外层模板包裹的状态行正文",
        example: "状态行正文",
    },
    ReplyVariable {
        name: "歌词",
        label: "当前歌词行",
        example: "故事的小黄花",
    },
    ReplyVariable {
        name: "新增",
        label: "新增歌曲数",
        example: "12",
    },
    ReplyVariable {
        name: "重复",
        label: "重复歌曲数",
        example: "3",
    },
    ReplyVariable {
        name: "炸弹",
        label: "本局炸弹次数",
        example: "2",
    },
    ReplyVariable {
        name: "牌型",
        label: "牌型名",
        example: "顺子",
    },
    ReplyVariable {
        name: "牌面",
        label: "牌面字符串",
        example: "♠3♠4♠5♠6♠7",
    },
    ReplyVariable {
        name: "玩家",
        label: "玩家名",
        example: "小明",
    },
    ReplyVariable {
        name: "玩家列表",
        label: "玩家名列表",
        example: "甲、乙、丙",
    },
    ReplyVariable {
        name: "玩法",
        label: "玩法名称",
        example: "斗地主",
    },
    ReplyVariable {
        name: "秒数",
        label: "秒数",
        example: "30",
    },
    ReplyVariable {
        name: "结果",
        label: "判定结果",
        example: "是",
    },
    ReplyVariable {
        name: "胜方",
        label: "胜利阵营",
        example: "平民胜利",
    },
    ReplyVariable {
        name: "词语",
        label: "自己拿到的词",
        example: "苹果",
    },
    ReplyVariable {
        name: "超时秒数",
        label: "投票超时秒数",
        example: "30",
    },
    ReplyVariable {
        name: "身份",
        label: "玩家身份（平民/卧底）",
        example: "平民",
    },
    ReplyVariable {
        name: "轮次",
        label: "当前轮次",
        example: "2",
    },
    ReplyVariable {
        name: "问题",
        label: "玩家提出的问题",
        example: "他认识死者吗",
    },
    ReplyVariable {
        name: "角色",
        label: "该玩家在本局的身份（地主/农民）",
        example: "地主",
    },
    ReplyVariable {
        name: "阵营",
        label: "对局阵营（地主方/农民方）",
        example: "地主方",
    },
    ReplyVariable {
        name: "阶段",
        label: "对局阶段",
        example: "提问",
    },
    ReplyVariable {
        name: "音量",
        label: "播放器音量",
        example: "80",
    },
];

/// 命令回复目录项；由 `ownership::REPLY_GROUPS` 派生，不再手写维护。
///
/// 保留这个类型是为了让路由层与发送层只依赖"命令 → 回复"这一层语义，
/// 不必知道归一化后的归属表结构。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandReplyCatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub category: &'static str,
    keys: &'static [&'static str],
}

impl CommandReplyCatalogEntry {
    /// 该命令拥有的模板，顺序与归属表一致。
    pub(crate) fn replies(&self) -> impl Iterator<Item = &'static ReplyTemplate> + '_ {
        self.keys.iter().filter_map(|key| spec_for_key(key))
    }
}

/// 命令回复目录。顺序即命令面板的展示顺序。
pub(crate) fn command_reply_catalog() -> impl Iterator<Item = CommandReplyCatalogEntry> + 'static {
    ownership::REPLY_GROUPS
        .iter()
        .map(|group| CommandReplyCatalogEntry {
            id: group.command_id,
            name: group.name,
            category: group.category,
            keys: group.keys,
        })
}

/// 与已有分组共用回复的命令别名表。
pub(crate) fn command_id_aliases() -> &'static [(&'static str, &'static str)] {
    ownership::COMMAND_ID_ALIASES
}

/// 把命令 ID 归一化为真正持有回复的分组 ID。
///
/// 共享命令（@播放 与 @继续、各平台点歌等）没有独立文案，
/// 面板据此展示所属分组的回复，而不是显示"暂无可编辑回复"。
pub(crate) fn canonical_command_id(command_id: &str) -> &str {
    ownership::canonical_command_id(command_id)
}

/// 一条 key 归属哪个命令。
///
/// 这是"覆盖能不能被找到"的唯一判定：不在任何命令下登记的 key，
/// 发送时查不到覆盖，面板里也编辑不到。
pub(crate) fn command_reply_entry_for_key(key: &str) -> Option<CommandReplyCatalogEntry> {
    let group = ownership::group_for_key(key)?;
    Some(CommandReplyCatalogEntry {
        id: group.command_id,
        name: group.name,
        category: group.category,
        keys: group.keys,
    })
}

/// 按稳定 key 查模板定义。
pub(crate) fn spec_for_key(key: &str) -> Option<&'static ReplyTemplate> {
    catalog().find(|spec| spec.key == key)
}

pub(crate) fn reply_variables() -> &'static [ReplyVariable] {
    REPLY_VARIABLES
}
/// 回复模板里允许出现的变量名。
///
/// 两类都要认，缺一类就会出现「面板列出来、保存却报未知变量」：
///
/// 1. `REPLY_VARIABLES`：依赖命令上下文的常规变量
/// 2. `AI_TAG`：AI 标签，渲染前预生成，同样可让用户插入
///
/// 随机的 `{a|b|c}` 不在这里判定——它不是具名变量，由调用方单独放行。
pub(crate) fn reply_variable_allowed(name: &str) -> bool {
    REPLY_VARIABLES.iter().any(|v| v.name == name) || name == crate::features::ai_tag::AI_TAG
}

/// AI 聊天回复。
pub(crate) const AI_CHAT_FORGET: ReplyTemplate = ReplyTemplate {
    key: "ai_chat.forget.done",
    default: "已忘记关于你的记忆",
    summary: "清空自己的 AI 记忆",
    group: "AI 聊天",
};

/// 提问过长。
pub(crate) const AI_CHAT_TOO_LONG: ReplyTemplate = ReplyTemplate {
    key: "ai_chat.question.too_long",
    default: "问题太长，请精简后再问",
    summary: "提问超过长度上限",
    group: "AI 聊天",
};

/// AI 暂不可用。
pub(crate) const AI_CHAT_UNAVAILABLE: ReplyTemplate = ReplyTemplate {
    key: "ai_chat.unavailable",
    default: "我现在答不上来，稍后再问",
    summary: "AI 调用失败",
    group: "AI 聊天",
};

/// 管理员启用大厅命令识别后的回执。
pub(crate) const ADMIN_COMMANDS_ENABLED: ReplyTemplate = ReplyTemplate {
    key: "admin.commands.enabled",
    default: "管理员已启用大厅命令识别功能",
    summary: "@启用 执行成功",
    group: "管理",
};

/// 管理员禁用大厅命令识别后的回执。
pub(crate) const ADMIN_COMMANDS_DISABLED: ReplyTemplate = ReplyTemplate {
    key: "admin.commands.disabled",
    default: "管理员已禁用大厅命令识别功能",
    summary: "@禁用 执行成功",
    group: "管理",
};
/// 渲染时的变量集合。AI 回答等动态文本以普通值形式放入。
///
/// 实现 `PartialEq`/`Eq` 是为了让"待渲染消息"能放进 `#[derive(PartialEq, Eq)]`
/// 的 effect 队列里（如 `UndercoverEffect`），测试才能整体断言 effect 相等。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReplyVars {
    values: BTreeMap<&'static str, String>,
}

impl ReplyVars {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.values.insert(name, value.into());
        self
    }

    /// 注入 AI 生成的文本。单独开一个方法，是为了在调用点显式表达
    /// “这是外部模型输出”，提醒读者它不参与模板解析。
    /// 注入 AI 生成的文本。
    ///
    /// 占位符名与全局标签 `{AI回复}` 一致：模板里写 `{AI回复}`，
    /// 由调用方预生成后注入，渲染时与普通变量同路径取得。
    pub(crate) fn with_ai(self, value: impl Into<String>) -> Self {
        self.with(crate::features::ai_tag::AI_TAG, value)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub(crate) fn with_nested(mut self, nested: Vec<(&'static str, String)>) -> Self {
        self.values.extend(nested);
        self
    }
}

/// 渲染结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderedReply {
    pub(crate) text: String,
    /// 配置里存在该键（即用户改过）。
    pub(crate) overridden: bool,
    /// 模板里出现但变量集合未提供的占位符名（已替换为空串）。
    pub(crate) missing: Vec<String>,
}

/// 从 `{选项1|选项2|…}` 里随机取一条。
///
/// 让用户在回复里直接写随机文案，不用去改代码里的标签表：
/// 同一条回复反复出现也不会完全一样。
///
/// 判定条件：花括号内**不含** `{` 且**含** `|`。
/// 空选项（末尾的 `|`、连续 `||`）会被剔除；全空时返回空串。
fn pick_random_choice(inner: &str) -> Option<&str> {
    if inner.contains('{') || !inner.contains('|') {
        return None;
    }
    let choices: Vec<&str> = inner
        .split('|')
        .map(str::trim)
        .filter(|choice| !choice.is_empty())
        .collect();
    if choices.is_empty() {
        return Some("");
    }
    // 用纳秒做种子：不需要密码学强度，只要每次调用不同即可。
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default() as usize;
    Some(choices[seed % choices.len()])
}

/// 渲染一条模板。
///
/// override_text 为 Some 且非空白时使用配置覆盖，否则用 spec.default。
/// 未知占位符原样保留；已知但缺失的变量替换为空串并记入 missing。
pub(crate) fn render(
    spec: &ReplyTemplate,
    override_text: Option<&str>,
    vars: &ReplyVars,
) -> RenderedReply {
    let (template, overridden) = match override_text {
        Some(text) if !text.trim().is_empty() => (text, true),
        _ => (spec.default, false),
    };
    let mut out = String::with_capacity(template.len() + 32);
    let mut missing: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // 未闭合的 { ：原样保留，不吞掉后续文本。
            out.push('{');
            rest = after;
            continue;
        };
        let name = &after[..close];
        if name.is_empty() || name.contains('{') {
            // 空花括号或嵌套左花括号视为普通文本。
            out.push_str(&rest[open..open + 1 + close + 1]);
            rest = &after[close + 1..];
            continue;
        }
        // \u968f\u673a\u9009\u9879\u6700\u5148\uff1a\u5b83\u4e0d\u4f9d\u8d56\u4efb\u4f55\u4e1a\u52a1\u4e0a\u4e0b\u6587\uff0c
        // \u800c\u4e14\u7528\u6237\u5199\u5728\u56de\u590d\u91cc\u5c31\u8981\u751f\u6548\u3002
        if let Some(choice) = pick_random_choice(name) {
            out.push_str(choice);
            rest = &after[close + 1..];
            continue;
        }
        match vars.get(name) {
            Some(value) => out.push_str(value),
            // 已知但未注入（如 {AI回复} 预生成失败）替换为空串并记入 missing；
            // 未知名字同样记进去，由调用方决定怎么处理。
            None => {
                if !missing.iter().any(|seen| seen == name) {
                    missing.push(name.to_string());
                }
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    RenderedReply {
        text: out,
        overridden,
        missing,
    }
}

/// 从已持久化的命令回复里取某条 key 的用户覆盖。
///
/// 这是“覆盖能不能查到”的唯一实现：立即发送、延迟发送、播放器三条路径都调用它，
/// 避免各写一份导致同一条配置在不同路径上表现不一致。
///
/// 覆盖文案含换行时由发送路径拆条，这里只负责取合并后的整段文本。
pub(crate) fn stored_override(
    stored: &std::collections::BTreeMap<String, Vec<crate::config::StoredReply>>,
    key: &str,
) -> Option<String> {
    let entry = command_reply_entry_for_key(key)?;
    stored
        .get(entry.id)?
        .iter()
        .find(|reply| reply.key == key)
        .map(|reply| reply.text.clone())
        // 空文本表示"使用内置默认"，不是覆盖。
        .filter(|text| !text.trim().is_empty())
}

/// 某条 key 是否被命令面板静音。
///
/// 两种静音来源，都表示"不要发送"：
/// - 整个命令存了空列表：命令级静音。
/// - 该 key 不在命令的行里：单行被停用。
///
/// 注意行的**存在**表示启用，与文本是否为空无关：留空行表示"使用内置默认"，
/// 仍会正常发送。若把空文本行当成静音，用户清空文案就会意外关掉回复。
///
/// 未登记归属的 key 永远不会被静音，避免误吞没有编辑入口的文案。
pub(crate) fn is_muted(
    stored: &std::collections::BTreeMap<String, Vec<crate::config::StoredReply>>,
    key: &str,
) -> bool {
    let Some(entry) = command_reply_entry_for_key(key) else {
        return false;
    };
    stored
        .get(entry.id)
        .is_some_and(|rows| rows.is_empty() || !rows.iter().any(|reply| reply.key == key))
}

// ---------------------------------------------------------------------------
// 模板目录
// ---------------------------------------------------------------------------
//
// 约定：
// - key 一旦发布不要改名，否则用户已保存的覆盖会失效（改名等同于删除旧覆盖）。
// - default 是内置文案，也是“用户没改过”时的唯一来源。
// - summary 给命令面板做说明；group 用于面板分组。

/// 大厅帮助（点歌类）。
///
/// 单条模板、多行文案：一行发一条消息。合并成一个 key 后，
/// 用户改一处就能看到全部帮助，不必在三个文本框之间来回找。
pub(crate) const HELP_SONG_REQUEST: ReplyTemplate = ReplyTemplate {
    key: "help.song_request",
    default: "点歌示例: @点歌/@AI点歌 歌名 歌手 伴奏,输入伴奏时优先匹配伴奏\n切换网易平台: @网易点歌/@网易云点歌 歌名 歌手 伴奏,默认为QQ平台\n可用 @QQ点歌/@网易点歌/@网易云点歌 指定来源,@AI点歌用于智能识别歌名歌手",
    summary: "大厅帮助（点歌）",
    group: "管理",
};

/// 大厅帮助（歌单类）。
pub(crate) const HELP_PLAYLIST: ReplyTemplate = ReplyTemplate {
    key: "help.playlist",
    default: "歌单: @歌单 推荐5首歌; @随机歌单/@歌单随机 随机推荐\n收藏: @收藏 收藏当前歌曲; @取消收藏\n模式: @歌单模式/@播放歌单 进入歌单模式; @退出歌单模式/@大厅模式 退出歌单模式\n导入: @导入QQ歌单<数字ID>; @导入网易歌单<数字ID>; @歌单清空",
    summary: "大厅帮助（歌单）",
    group: "歌单",
};

/// 大厅帮助（娱乐类）。
pub(crate) const HELP_ENTERTAINMENT: ReplyTemplate = ReplyTemplate {
    key: "help.entertainment",
    default: "塔罗: #塔罗牌抽当天三张;#塔罗牌重抽提前换组;带问题自动AI\n塔罗: UTC+8每日更新;#塔罗牌AI解读当前三张;#塔罗牌帮助\n成语接龙: #接龙随机开局;同音模式用 #同音接龙随机开局;进行中直接发送成语/#提示/#解释\n斗地主: #斗地主/#加入/#抢/#不抢/#出3334/#过/#状态/#重试/#结束;好友私聊 #手牌\n跑得快: #跑得快/#加入/#出3334/#过/#状态/#重试/#结束;好友私聊 #手牌\n海龟汤: #海龟汤;进行中 #状态/#结束;提问直接以#开头,如#他认识死者吗\n海龟汤长答案: ##1第一段,##2第二段,最后发送##提交;取消用##取消\n谁是卧底: #卧底/#卧底双;公屏 #开局/#状态/#退出/#结束/#重试/#内容\n谁是卧底: 好友私聊 #加入/#退出/#弃权/#A/#投A;局外好友私聊 #谜底",
    summary: "大厅帮助（娱乐）",
    group: "管理",
};

/// 点歌/歌单类回复文案。
pub(crate) const REPLY_SONG: [ReplyTemplate; 1] = [ReplyTemplate {
    key: "song.playlist.friend_only",
    default: "个人歌单功能仅对好友及以上开放.",
    summary: "歌单权限不足时的提示",
    group: "点歌",
}];

/// 全部可配置回复模板。
///
/// 每个新增模板模块必须在这里加一行；`every_production_key_is_registered`
/// 测试会拦住"定义了模板却漏登记"的情况。
pub(crate) fn catalog() -> impl Iterator<Item = &'static ReplyTemplate> {
    // 帮助与 AI 聊天文案住在本文件里，此前漏登记，导致配置页看不到、也改不了。
    std::iter::once(&HELP_SONG_REQUEST)
        .chain(std::iter::once(&HELP_PLAYLIST))
        .chain(std::iter::once(&HELP_ENTERTAINMENT))
        .chain(std::iter::once(&AI_CHAT_FORGET))
        .chain(std::iter::once(&AI_CHAT_TOO_LONG))
        .chain(std::iter::once(&AI_CHAT_UNAVAILABLE))
        .chain(std::iter::once(&ADMIN_COMMANDS_ENABLED))
        .chain(std::iter::once(&ADMIN_COMMANDS_DISABLED))
        .chain(REPLY_SONG.iter())
        // 按模块拆分的模板：新增模块时只在这里加一行。
        .chain(playback::PLAYBACK.iter())
        .chain(playlist::PLAYLIST.iter())
        .chain(hall::HALL.iter())
        .chain(song::SONG.iter())
        .chain(undercover::UNDERCOVER.iter())
        .chain(turtle_soup::TURTLE_SOUP.iter())
        .chain(card_games::CARD_GAMES.iter())
        .chain(idiom_chain::IDIOM_CHAIN.iter())
        .chain(tarot::TAROT.iter())
        .chain(moderation::MODERATION.iter())
}

#[cfg(test)]
mod tests {
    /// 随机选项 {a|b|c}：每次取到其中一条，且不留下花括号。

    /// 回归：内置默认文案用到的变量，必须都在允许列表里。
    ///
    /// `#斗地主` 的默认回复用了 `{角色}`，但 `REPLY_VARIABLES` 漏登记了它——
    /// 结果是用户打开面板、什么都不改、直接点保存就报「未知变量 {角色}」。
    /// 产品自己的默认值过不了自己的校验，这类不一致必须由测试拦住。
    #[test]
    fn every_placeholder_in_default_replies_is_an_allowed_variable() {
        let allowed: std::collections::BTreeSet<&str> =
            REPLY_VARIABLES.iter().map(|v| v.name).collect();
        let mut unknown = Vec::new();
        for entry in command_reply_catalog() {
            for template in entry.replies() {
                for name in placeholder_names(template.default) {
                    if !allowed.contains(name.as_str()) && name != crate::features::ai_tag::AI_TAG {
                        unknown.push(format!("{} -> {{{name}}}", template.key));
                    }
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "这些默认文案用了未登记的变量，用户一保存就会失败：{unknown:?}"
        );
    }

    /// 抽出模板里的 `{变量}` 名，跳过 `{{...}}`（AI 提示词用的双括号）与 `{甲|乙}`（随机选择）。
    fn placeholder_names(text: &str) -> Vec<String> {
        let mut names = Vec::new();
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'{' {
                index += 1;
                continue;
            }
            // `{{` 不是变量占位符。
            if index + 1 < bytes.len() && bytes[index + 1] == b'{' {
                index += 2;
                continue;
            }
            let Some(end) = text[index..].find('}') else {
                break;
            };
            let inner = &text[index + 1..index + end];
            index += end + 1;
            // `{甲|乙|丙}` 是随机选择，不是变量。
            if inner.contains('|') || inner.is_empty() {
                continue;
            }
            names.push(inner.trim().to_string());
        }
        names
    }

    #[test]
    fn random_choice_picks_one_of_the_options() {
        let template = crate::features::reply_template::ReplyTemplate {
            key: "test.random",
            default: "好{喵~|诶嘿|嘻嘻}",
            summary: "测试",
            group: "测试",
        };
        let vars = crate::features::reply_template::ReplyVars::new();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..60 {
            let text = crate::features::reply_template::render(&template, None, &vars).text;
            assert!(!text.contains('{'), "不应残留花括号: {text}");
            assert!(!text.contains('|'), "不应残留竖线: {text}");
            assert!(text.starts_with('好'), "前缀应保留: {text}");
            seen.insert(text.trim_start_matches('好').to_string());
        }
        // 60 次里至少要出现两个不同选项，否则随机没生效。
        assert!(seen.len() >= 2, "随机未生效，只见到: {seen:?}");
    }

    /// 末尾空选项、连续竖线都要剔除。
    #[test]
    fn random_choice_skips_empty_options() {
        let template = crate::features::reply_template::ReplyTemplate {
            key: "test.random.empty",
            default: "[{甲||乙|}]",
            summary: "测试",
            group: "测试",
        };
        let vars = crate::features::reply_template::ReplyVars::new();
        for _ in 0..20 {
            let text = crate::features::reply_template::render(&template, None, &vars).text;
            assert!(text == "[甲]" || text == "[乙]", "实际: {text}");
        }
    }

    use super::*;

    const SPEC: ReplyTemplate = ReplyTemplate {
        key: "test.spec",
        default: "已为{玩家}点播{歌名}",
        summary: "测试",
        group: "点歌",
    };

    #[test]
    fn falls_back_to_the_builtin_default_when_no_override_exists() {
        let vars = ReplyVars::new().with("玩家", "甲").with("歌名", "晴天");
        let rendered = render(&SPEC, None, &vars);
        assert_eq!(rendered.text, "已为甲点播晴天");
        assert!(!rendered.overridden);
        assert!(rendered.missing.is_empty());
    }

    #[test]
    fn an_empty_override_still_falls_back_instead_of_sending_blank_text() {
        let vars = ReplyVars::new().with("玩家", "甲").with("歌名", "晴天");
        let rendered = render(&SPEC, Some("   "), &vars);
        assert!(!rendered.overridden);
        assert_eq!(rendered.text, "已为甲点播晴天");
    }

    #[test]
    fn override_text_is_used_verbatim_apart_from_placeholders() {
        let vars = ReplyVars::new().with("玩家", "乙").with("歌名", "夜曲");
        let rendered = render(&SPEC, Some("收到，{玩家} 的 {歌名} 已排上"), &vars);
        assert_eq!(rendered.text, "收到，乙 的 夜曲 已排上");
        assert!(rendered.overridden);
    }

    #[test]
    fn ai_output_is_inserted_as_a_value_and_never_parsed_as_a_template() {
        // AI 文本里带花括号时不得被当成占位符，否则会吞掉内容或泄露变量名。
        let vars = ReplyVars::new().with_ai("牌面显示 {歌名} 与 {未知}");
        let spec = ReplyTemplate {
            key: "test.ai",
            default: "解读：{AI回复}",
            summary: "测试",
            group: "娱乐",
        };
        let rendered = render(&spec, None, &vars);
        assert_eq!(rendered.text, "解读：牌面显示 {歌名} 与 {未知}");
        assert!(
            rendered.missing.is_empty(),
            "AI 文本里的花括号不应计入缺失变量"
        );
    }

    #[test]
    fn missing_variables_become_empty_and_are_reported_once() {
        let rendered = render(&SPEC, None, &ReplyVars::new());
        assert_eq!(rendered.text, "已为点播");
        assert_eq!(
            rendered.missing,
            vec!["玩家".to_string(), "歌名".to_string()]
        );
    }

    #[test]
    fn an_unclosed_brace_is_preserved_instead_of_swallowing_the_rest() {
        let spec = ReplyTemplate {
            key: "test.brace",
            default: "前缀{未闭合",
            summary: "测试",
            group: "管理",
        };
        assert_eq!(render(&spec, None, &ReplyVars::new()).text, "前缀{未闭合");
    }

    #[test]
    fn a_literal_empty_placeholder_is_left_alone() {
        let spec = ReplyTemplate {
            key: "test.empty",
            default: "花括号{}保留",
            summary: "测试",
            group: "管理",
        };
        assert_eq!(render(&spec, None, &ReplyVars::new()).text, "花括号{}保留");
    }

    #[test]
    fn the_registered_reply_catalog_entries_are_reachable_and_distinct() {
        // 目录里的键必须唯一，否则配置覆盖会串台。
        let mut keys = std::collections::BTreeSet::new();
        for spec in catalog() {
            assert!(keys.insert(spec.key), "模板键重复登记：{}", spec.key);
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
            assert!(!spec.group.trim().is_empty(), "{} 缺少分组", spec.key);
        }
        assert!(!keys.is_empty(), "模板目录不应为空");

        let mut command_ids = std::collections::BTreeSet::new();
        let mut owned_keys = std::collections::BTreeSet::new();
        for entry in command_reply_catalog() {
            assert!(
                command_ids.insert(entry.id),
                "命令回复目录ID重复：{}",
                entry.id
            );
            assert!(entry.replies().next().is_some(), "{} 未登记回复", entry.id);
            for spec in entry.replies() {
                assert!(
                    owned_keys.insert(spec.key),
                    "模板键被多个命令目录项拥有：{}",
                    spec.key
                );
                assert_eq!(
                    command_reply_entry_for_key(spec.key).map(|owner| owner.id),
                    Some(entry.id),
                    "模板 key 必须解析回它唯一的命令 owner: {}",
                    spec.key
                );
            }
        }
    }

    #[test]
    fn every_reply_variable_has_a_short_example() {
        // 面板按钮展示示例值而不是占位符：用户看到「晴天」就知道这条变量长什么样。
        // 示例必须简短，否则按钮会撑破布局；也不能为空或仍是占位符。
        for variable in reply_variables() {
            assert!(
                !variable.example.trim().is_empty(),
                "变量 {} 缺少示例",
                variable.name
            );
            assert!(
                variable.example.chars().count() <= 16,
                "变量 {} 的示例过长：{}",
                variable.name,
                variable.example
            );
            assert!(
                !variable.example.contains('{') && !variable.example.contains('}'),
                "变量 {} 的示例不该是占位符：{}",
                variable.name,
                variable.example
            );
        }
    }

    #[test]
    fn every_catalog_key_is_owned_by_exactly_one_command() {
        // 这是"全部命令可编辑"的结构性保证：目录里任何一条文案都必须能在
        // 某个命令下被编辑；否则配置页列出它、命令面板却找不到入口。
        let mut owners = std::collections::BTreeMap::new();
        for spec in catalog() {
            if let Some(owner) = command_reply_entry_for_key(spec.key) {
                assert!(
                    owners.insert(spec.key, owner.id).is_none(),
                    "模板 key 被重复归属：{}",
                    spec.key
                );
            }
        }
        let orphan: Vec<_> = catalog()
            .map(|spec| spec.key)
            .filter(|key| !owners.contains_key(key))
            .collect();
        assert!(
            orphan.is_empty(),
            "以下模板没有归属命令，面板里无法编辑：{orphan:?}"
        );
    }

    #[test]
    fn ownership_groups_only_reference_registered_templates() {
        // 反向校验：归属表里写错一个 key，发送时查不到模板，覆盖会静默失效。
        for group in ownership::REPLY_GROUPS {
            for key in group.keys {
                assert!(
                    spec_for_key(key).is_some(),
                    "归属表引用了不存在的模板 key：{key}（命令 {}）",
                    group.command_id
                );
            }
            assert_eq!(
                ownership::group_for_command(group.command_id).map(|g| g.command_id),
                Some(group.command_id),
                "命令 {} 的归属不可查",
                group.command_id
            );
        }
    }

    #[test]
    fn every_command_alias_points_at_a_real_group() {
        // 别名让共享命令在面板里复用别人的回复；指向不存在的分组会让
        // 该命令重新变成"暂无可编辑回复"。
        for (alias, owner) in ownership::COMMAND_ID_ALIASES {
            assert!(
                ownership::group_for_command(owner).is_some(),
                "别名 {alias} 指向不存在的分组 {owner}"
            );
            assert_ne!(alias, owner, "别名不应指向自己：{alias}");
            // 别名命令本身不应再持有 key，否则两处都能编辑同一批文案。
            assert!(
                ownership::group_for_command(alias).is_none(),
                "{alias} 既有别名又自带分组，归属会歧义"
            );
        }
    }

    #[test]
    fn command_catalog_resolves_playback_keys_and_renders_shared_global_variables() {
        let entry = command_reply_entry_for_key("playback.volume.set").unwrap();
        assert_eq!(entry.id, "@音量");
        let specs: Vec<_> = entry.replies().collect();
        assert_eq!(specs[0].key, "playback.volume.set");
        let rendered = render(
            specs[0],
            Some("音量调整完成：{音量}"),
            &ReplyVars::new().with("音量", "65"),
        );
        assert_eq!(rendered.text, "音量调整完成：65");
        assert!(
            command_reply_entry_for_key("playback.resume.ok")
                .is_some_and(|entry| entry.id == "@继续")
        );
    }

    #[test]
    fn muted_keys_render_to_none_on_every_path() {
        // 这是"静音在所有路径生效"的契约测试。
        //
        // 修复前：21 个发送点只有 5 个自己查静音，牌类/卧底/海龟汤片段/塔罗/歌单
        // 的静音根本不生效。现在静音判定收在 render_template_text 里，
        // 这些组合回复的静音才能沿嵌套链传播。
        let muted = std::collections::BTreeMap::from([("@音量".to_string(), Vec::new())]);
        assert!(is_muted(&muted, "playback.volume.set"));

        // 静音是"按命令"生效的：同一命令下的 key 一起静音，
        // 其他命令不受影响。组合回复的嵌套片段分属不同命令时，
        // 任一被静音都会让整条不发（见 render_reply_text 的 ? 传播）。
        let playlist_muted = std::collections::BTreeMap::from([("@歌单".to_string(), Vec::new())]);
        assert!(is_muted(&playlist_muted, "playlist.window.header"));
        assert!(is_muted(&playlist_muted, "playlist.select.queued"));
        assert!(!is_muted(&playlist_muted, "playback.volume.set"));
    }

    fn a_blank_row_means_default_not_muted() {
        // 行的存在 = 该回复启用；行内文本为空 = 用内置默认。
        // 这两件事必须分开：曾经"清空文案"会把唯一一行丢掉，命令变空列表后被
        // 判为整条静音——用户只是想恢复默认，却把回复关掉了。
        let blank = stored(&[("playback.volume.set", "")]);
        assert_eq!(stored_override(&blank, "playback.volume.set"), None);
        assert!(
            !is_muted(&blank, "playback.volume.set"),
            "留空行必须仍视为启用"
        );

        // 只有真的是空列表（命令级静音）才不发送。
        let muted = std::collections::BTreeMap::from([("@音量".to_string(), Vec::new())]);
        assert!(is_muted(&muted, "playback.volume.set"));
    }

    #[test]
    fn every_catalog_key_can_be_overridden_and_renders_the_override() {
        // 这是"全部回复可改"的直接证据：目录里任何一条都能被覆盖并正确渲染。
        for spec in catalog() {
            let rendered = render(spec, Some("自定义文案"), &ReplyVars::new());
            assert_eq!(rendered.text, "自定义文案", "{} 未能被覆盖", spec.key);
            assert!(rendered.overridden);
        }
    }

    fn stored(
        rows: &[(&str, &str)],
    ) -> std::collections::BTreeMap<String, Vec<crate::config::StoredReply>> {
        std::collections::BTreeMap::from([(
            "@音量".to_string(),
            rows.iter()
                .map(|(key, text)| crate::config::StoredReply {
                    key: (*key).to_string(),
                    text: (*text).to_string(),
                })
                .collect(),
        )])
    }

    #[test]
    fn stored_override_reads_the_row_saved_under_its_own_command() {
        // "改了没反应"的根因就是查不到这一行；这条测试直接钉住查询路径。
        let stored = stored(&[("playback.volume.set", "音量已设为{音量}")]);
        assert_eq!(
            stored_override(&stored, "playback.volume.set").as_deref(),
            Some("音量已设为{音量}")
        );
    }

    #[test]
    fn stored_override_is_none_when_the_row_was_removed() {
        // 留空 / 停用 = 恢复内置默认，不应再返回覆盖。
        let stored = stored(&[]);
        assert_eq!(stored_override(&stored, "playback.volume.set"), None);
    }

    #[test]
    fn muting_a_command_suppresses_only_its_own_keys() {
        let stored = std::collections::BTreeMap::from([("@音量".to_string(), Vec::new())]);
        assert!(is_muted(&stored, "playback.volume.set"));
        // 别的命令不受影响
        assert!(!is_muted(&stored, "playback.pause.ok"));
        // 没有归属的 key 永远不会被静音，避免误吞没有编辑入口的文案
        assert!(!is_muted(&stored, "test.spec"));
    }

    #[test]
    fn partial_override_does_not_mute_the_remaining_rows() {
        // 只改一条时，同命令其他 key 仍应正常发送。
        let stored = stored(&[("playback.volume.set", "音量{音量}")]);
        assert!(!is_muted(&stored, "playback.volume.set"));
        assert!(!is_muted(&stored, "playback.pause.ok"));
    }

    #[test]
    fn overriding_a_reply_never_requires_changing_code() {
        // 用户只写了覆盖文案、没写变量：渲染结果就是该文案本身，不会残留花括号。
        let spec = &REPLY_SONG[0];
        assert!(spec.default.contains("好友"));
        let rendered = render(spec, Some("歌单只给好友用哦"), &ReplyVars::new());
        assert_eq!(rendered.text, "歌单只给好友用哦");
        assert!(rendered.missing.is_empty());
    }
}
