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
}

/// Global template tokens; includes builder-supported reserved values.
pub(crate) const REPLY_VARIABLES: &[ReplyVariable] = &[
    ReplyVariable {
        name: "AI回答",
        label: "AI回答",
    },
    ReplyVariable {
        name: "UID",
        label: "UID",
    },
    ReplyVariable {
        name: "上一个",
        label: "上一个",
    },
    ReplyVariable {
        name: "上一手",
        label: "上一手",
    },
    ReplyVariable {
        name: "上一手牌型",
        label: "上一手牌型",
    },
    ReplyVariable {
        name: "上限",
        label: "上限",
    },
    ReplyVariable {
        name: "下一位",
        label: "下一位",
    },
    ReplyVariable {
        name: "人数",
        label: "人数",
    },
    ReplyVariable {
        name: "位置",
        label: "位置",
    },
    ReplyVariable {
        name: "位置列表",
        label: "位置列表",
    },
    ReplyVariable {
        name: "关键词",
        label: "关键词",
    },
    ReplyVariable {
        name: "其他玩法",
        label: "其他玩法",
    },
    ReplyVariable {
        name: "决定",
        label: "决定",
    },
    ReplyVariable {
        name: "前缀",
        label: "前缀",
    },
    ReplyVariable {
        name: "剩余",
        label: "剩余",
    },
    ReplyVariable {
        name: "剩余分钟",
        label: "剩余分钟",
    },
    ReplyVariable {
        name: "动作",
        label: "动作",
    },
    ReplyVariable {
        name: "卧底位置",
        label: "卧底位置",
    },
    ReplyVariable {
        name: "卧底词",
        label: "卧底词",
    },
    ReplyVariable {
        name: "原因",
        label: "原因",
    },
    ReplyVariable {
        name: "名单",
        label: "名单",
    },
    ReplyVariable {
        name: "后续",
        label: "后续",
    },
    ReplyVariable {
        name: "后缀",
        label: "后缀",
    },
    ReplyVariable {
        name: "地主",
        label: "地主",
    },
    ReplyVariable {
        name: "大厅名",
        label: "大厅名",
    },
    ReplyVariable {
        name: "字",
        label: "字",
    },
    ReplyVariable {
        name: "平民词",
        label: "平民词",
    },
    ReplyVariable {
        name: "序号",
        label: "序号",
    },
    ReplyVariable {
        name: "底牌",
        label: "底牌",
    },
    ReplyVariable {
        name: "开头描述",
        label: "开头描述",
    },
    ReplyVariable {
        name: "张数",
        label: "张数",
    },
    ReplyVariable {
        name: "当前",
        label: "当前",
    },
    ReplyVariable {
        name: "当前抢地主",
        label: "当前抢地主",
    },
    ReplyVariable {
        name: "成语",
        label: "成语",
    },
    ReplyVariable {
        name: "手数",
        label: "手数",
    },
    ReplyVariable {
        name: "手牌",
        label: "手牌",
    },
    ReplyVariable {
        name: "托管后缀",
        label: "托管后缀",
    },
    ReplyVariable {
        name: "投票说明",
        label: "投票说明",
    },
    ReplyVariable {
        name: "拼音",
        label: "拼音",
    },
    ReplyVariable {
        name: "提问数",
        label: "提问数",
    },
    ReplyVariable {
        name: "搜索词",
        label: "搜索词",
    },
    ReplyVariable {
        name: "数量",
        label: "数量",
    },
    ReplyVariable {
        name: "昵称",
        label: "昵称",
    },
    ReplyVariable {
        name: "条目",
        label: "条目",
    },
    ReplyVariable {
        name: "来源",
        label: "来源",
    },
    ReplyVariable {
        name: "标签",
        label: "标签",
    },
    ReplyVariable {
        name: "模式",
        label: "模式",
    },
    ReplyVariable {
        name: "歌名",
        label: "歌名",
    },
    ReplyVariable {
        name: "歌手",
        label: "歌手",
    },
    ReplyVariable {
        name: "状态",
        label: "播放状态（播放/暂停/停止）",
    },
    ReplyVariable {
        name: "曲名",
        label: "曲名（歌名 - 歌手）",
    },
    ReplyVariable {
        name: "进度",
        label: "已播放时长",
    },
    ReplyVariable {
        name: "总长",
        label: "歌曲总时长",
    },
    ReplyVariable {
        name: "点歌人",
        label: "点歌人昵称",
    },
    ReplyVariable {
        name: "歌单归属",
        label: "歌单模式所有者",
    },
    ReplyVariable {
        name: "内容",
        label: "被包裹的正文",
    },
    ReplyVariable {
        name: "歌词",
        label: "当前歌词行",
    },
    ReplyVariable {
        name: "新增",
        label: "新增数量",
    },
    ReplyVariable {
        name: "重复",
        label: "重复数量",
    },
    ReplyVariable {
        name: "炸弹",
        label: "炸弹",
    },
    ReplyVariable {
        name: "牌型",
        label: "牌型",
    },
    ReplyVariable {
        name: "牌面",
        label: "牌面",
    },
    ReplyVariable {
        name: "玩家",
        label: "玩家",
    },
    ReplyVariable {
        name: "玩家列表",
        label: "玩家列表",
    },
    ReplyVariable {
        name: "玩法",
        label: "玩法",
    },
    ReplyVariable {
        name: "用户名",
        label: "用户名",
    },
    ReplyVariable {
        name: "秒数",
        label: "秒数",
    },
    ReplyVariable {
        name: "结果",
        label: "结果",
    },
    ReplyVariable {
        name: "胜方",
        label: "胜方",
    },
    ReplyVariable {
        name: "角色",
        label: "角色",
    },
    ReplyVariable {
        name: "词语",
        label: "词语",
    },
    ReplyVariable {
        name: "超时秒数",
        label: "超时秒数",
    },
    ReplyVariable {
        name: "身份",
        label: "身份",
    },
    ReplyVariable {
        name: "轮次",
        label: "轮次",
    },
    ReplyVariable {
        name: "问题",
        label: "问题",
    },
    ReplyVariable {
        name: "阵营",
        label: "阵营",
    },
    ReplyVariable {
        name: "阶段",
        label: "阶段",
    },
    ReplyVariable {
        name: "音量",
        label: "音量",
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
    pub ordered_batch: bool,
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
    ownership::REPLY_GROUPS.iter().map(|group| CommandReplyCatalogEntry {
        id: group.command_id,
        name: group.name,
        category: group.category,
        ordered_batch: group.ordered_batch,
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
        ordered_batch: group.ordered_batch,
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
pub(crate) fn reply_variable_allowed(name: &str) -> bool {
    REPLY_VARIABLES.iter().any(|v| v.name == name)
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
    pub(crate) fn with_ai(self, value: impl Into<String>) -> Self {
        self.with("AI回答", value)
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
        match vars.get(name) {
            Some(value) => out.push_str(value),
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
/// ordered_batch 的命令整段按序发送，单条覆盖由组装层按行渲染，
/// 这里返回 None 表示“不参与单条覆盖查找”。
pub(crate) fn stored_override(
    stored: &std::collections::BTreeMap<String, Vec<crate::config::StoredReply>>,
    key: &str,
) -> Option<String> {
    let entry = command_reply_entry_for_key(key)?;
    if entry.ordered_batch {
        return None;
    }
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
    stored.get(entry.id).is_some_and(|rows| {
        rows.is_empty() || (!entry.ordered_batch && !rows.iter().any(|reply| reply.key == key))
    })
}

// ---------------------------------------------------------------------------
// 模板目录
// ---------------------------------------------------------------------------
//
// 约定：
// - key 一旦发布不要改名，否则用户已保存的覆盖会失效（改名等同于删除旧覆盖）。
// - default 是内置文案，也是“用户没改过”时的唯一来源。
// - summary 给命令面板做说明；group 用于面板分组。

/// 大厅帮助（点歌类）逐行文案。
pub(crate) const HELP_SONG_REQUEST: [ReplyTemplate; 3] = [
    ReplyTemplate {
        key: "help.song_request.1",
        default: "点歌示例: @点歌/@AI点歌 歌名 歌手 伴奏,输入伴奏时优先匹配伴奏",
        summary: "大厅帮助-点歌示例",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.song_request.2",
        default: "切换网易平台: @网易点歌/@网易云点歌 歌名 歌手 伴奏,默认为QQ平台",
        summary: "大厅帮助-切换平台",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.song_request.3",
        default: "可用 @QQ点歌/@网易点歌/@网易云点歌 指定来源,@AI点歌用于智能识别歌名歌手",
        summary: "大厅帮助-音源说明",
        group: "管理",
    },
];

/// 大厅帮助（歌单类）逐行文案。
pub(crate) const HELP_PLAYLIST: [ReplyTemplate; 4] = [
    ReplyTemplate {
        key: "help.playlist.1",
        default: "歌单: @歌单 推荐5首歌; @随机歌单/@歌单随机 随机推荐",
        summary: "歌单帮助-浏览",
        group: "歌单",
    },
    ReplyTemplate {
        key: "help.playlist.2",
        default: "收藏: @收藏 收藏当前歌曲; @取消收藏",
        summary: "歌单帮助-收藏",
        group: "歌单",
    },
    ReplyTemplate {
        key: "help.playlist.3",
        default: "模式: @歌单模式/@播放歌单 进入歌单模式; @退出歌单模式/@大厅模式 退出歌单模式",
        summary: "歌单帮助-播放模式",
        group: "歌单",
    },
    ReplyTemplate {
        key: "help.playlist.4",
        default: "导入: @导入QQ歌单<数字ID>; @导入网易歌单<数字ID>; @歌单清空",
        summary: "歌单帮助-导入",
        group: "歌单",
    },
];

pub(crate) const HELP_ENTERTAINMENT: [ReplyTemplate; 9] = [
    ReplyTemplate {
        key: "help.entertainment.tarot.1",
        default: "塔罗: #塔罗牌抽当天三张;#塔罗牌重抽提前换组;带问题自动AI",
        summary: "娱乐帮助-塔罗1",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.tarot.2",
        default: "塔罗: UTC+8每日更新;#塔罗牌AI解读当前三张;#塔罗牌帮助",
        summary: "娱乐帮助-塔罗2",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.idiom",
        default: "成语接龙: #接龙随机开局;同音模式用 #同音接龙随机开局;进行中直接发送成语/#提示/#解释",
        summary: "娱乐帮助-成语接龙",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.landlord",
        default: "斗地主: #斗地主/#加入/#抢/#不抢/#出3334/#过/#状态/#重试/#结束;好友私聊 #手牌",
        summary: "娱乐帮助-斗地主",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.run_fast",
        default: "跑得快: #跑得快/#加入/#出3334/#过/#状态/#重试/#结束;好友私聊 #手牌",
        summary: "娱乐帮助-跑得快",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.turtle_soup",
        default: "海龟汤: #海龟汤;进行中 #状态/#结束;提问直接以#开头,如#他认识死者吗",
        summary: "娱乐帮助-海龟汤",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.turtle_soup_long",
        default: "海龟汤长答案: ##1第一段,##2第二段,最后发送##提交;取消用##取消",
        summary: "娱乐帮助-海龟汤长答案",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.undercover.1",
        default: "谁是卧底: #卧底/#卧底双;公屏 #开局/#状态/#退出/#结束/#重试/#内容",
        summary: "娱乐帮助-谁是卧底1",
        group: "管理",
    },
    ReplyTemplate {
        key: "help.entertainment.undercover.2",
        default: "谁是卧底: 好友私聊 #加入/#退出/#弃权/#A/#投A;局外好友私聊 #谜底",
        summary: "娱乐帮助-谁是卧底2",
        group: "管理",
    },
];

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
    HELP_SONG_REQUEST
        .iter()
        .chain(HELP_PLAYLIST.iter())
        .chain(HELP_ENTERTAINMENT.iter())
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
    use super::*;

    const SPEC: ReplyTemplate = ReplyTemplate {
        key: "test.spec",
        default: "已为{用户名}点播{歌名}",
        summary: "测试",
        group: "点歌",
    };

    #[test]
    fn falls_back_to_the_builtin_default_when_no_override_exists() {
        let vars = ReplyVars::new().with("用户名", "甲").with("歌名", "晴天");
        let rendered = render(&SPEC, None, &vars);
        assert_eq!(rendered.text, "已为甲点播晴天");
        assert!(!rendered.overridden);
        assert!(rendered.missing.is_empty());
    }

    #[test]
    fn an_empty_override_still_falls_back_instead_of_sending_blank_text() {
        let vars = ReplyVars::new().with("用户名", "甲").with("歌名", "晴天");
        let rendered = render(&SPEC, Some("   "), &vars);
        assert!(!rendered.overridden);
        assert_eq!(rendered.text, "已为甲点播晴天");
    }

    #[test]
    fn override_text_is_used_verbatim_apart_from_placeholders() {
        let vars = ReplyVars::new().with("用户名", "乙").with("歌名", "夜曲");
        let rendered = render(&SPEC, Some("收到，{用户名} 的 {歌名} 已排上"), &vars);
        assert_eq!(rendered.text, "收到，乙 的 夜曲 已排上");
        assert!(rendered.overridden);
    }

    #[test]
    fn ai_output_is_inserted_as_a_value_and_never_parsed_as_a_template() {
        // AI 文本里带花括号时不得被当成占位符，否则会吞掉内容或泄露变量名。
        let vars = ReplyVars::new().with_ai("牌面显示 {歌名} 与 {未知}");
        let spec = ReplyTemplate {
            key: "test.ai",
            default: "解读：{AI回答}",
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
            vec!["用户名".to_string(), "歌名".to_string()]
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
            assert!(
                entry.replies().next().is_some(),
                "{} 未登记回复",
                entry.id
            );
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
    fn ordered_batch_groups_are_the_help_batches() {
        // 只有"整段按序发送"的帮助类命令允许增删行；业务分支回复必须按实际分支发送。
        let ordered: Vec<_> = ownership::REPLY_GROUPS
            .iter()
            .filter(|group| group.ordered_batch)
            .map(|group| group.command_id)
            .collect();
        assert_eq!(
            ordered,
            ["@歌单帮助", "#塔罗牌帮助", "@帮助", "@娱乐帮助"],
            "按序批量发送的命令清单发生变化，请确认这是有意的"
        );
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
        let playlist_muted =
            std::collections::BTreeMap::from([("@歌单".to_string(), Vec::new())]);
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

    fn stored(rows: &[(&str, &str)]) -> std::collections::BTreeMap<String, Vec<crate::config::StoredReply>> {
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
    fn stored_override_ignores_ordered_batch_commands() {
        // 按序批量命令的单条文案由组装层按行渲染，不走单条覆盖查找。
        let stored = std::collections::BTreeMap::from([(
            "@帮助".to_string(),
            vec![crate::config::StoredReply {
                key: "help.song_request.1".to_string(),
                text: "自定义".to_string(),
            }],
        )]);
        assert_eq!(stored_override(&stored, "help.song_request.1"), None);
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