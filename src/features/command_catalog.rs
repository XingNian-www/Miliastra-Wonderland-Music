//! 命令注册表：命令的单一事实来源。
//!
//! 背景：命令过去分散在 9 个模块的 `claims_chat` / `parse_chat` 里，
//! 字面量以 const 数组或内联 match 存在，没有任何地方能枚举出"这个程序支持哪些命令"。
//! 帮助文本因此只能硬编码（见 `administration.rs` 的历史常量），命令面板也无从取数。
//!
//! 本模块登记命令的**事实与元数据**。匹配谓词仍由各模块提供（`match_rule` 是
//! 声明式的匹配方式，谓词函数指针由路由层装配），因此不重复实现解析逻辑。
//!
//! 三层命令模型（见 technical-docs/architecture-map.md §4.1）：
//! 1. **静态前缀**：`CommandSpec.match_rule` 描述，注册表完全掌握。
//! 2. **会话态**：玩法进行中才能用的命令，由 `SessionDispatch` 描述状态与模块的绑定。
//! 3. **决策语法**：`@选择`/`@1` 等由独占决策窗口消费，**刻意不进注册表**。

use crate::features::command::{CommandAuthority, CommandPrefix};

/// 命令所属的功能分组，决定面板里的归类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CommandGroup {
    Song,
    Playback,
    Hall,
    Entertainment,
    Administration,
}

impl CommandGroup {
    /// 全部分组，按面板展示顺序排列。
    pub(crate) const ALL: [Self; 5] = [
        Self::Song,
        Self::Playback,
        Self::Hall,
        Self::Entertainment,
        Self::Administration,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Song => "点歌",
            Self::Playback => "播放",
            Self::Hall => "大厅",
            Self::Entertainment => "娱乐",
            Self::Administration => "管理",
        }
    }

    /// 面板展示顺序。
    pub(crate) const fn order(self) -> u8 {
        match self {
            Self::Song => 1,
            Self::Playback => 2,
            Self::Hall => 3,
            Self::Entertainment => 4,
            Self::Administration => 5,
        }
    }
}

/// 命令文本的匹配方式。
///
/// 刻意只保留两种：它们能覆盖现有全部静态命令，且语义直白。
/// 复杂匹配（如成语接龙的"任意文本都当成语提交"）属于会话态，见 `SessionDispatch`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandMatch {
    /// 前缀匹配 + 命令边界判定，可带参数。
    ///
    /// 例：`点歌` 匹配 `@点歌晴天`，也匹配 `@点歌 晴天`。
    Prefix {
        /// 是否允许跟在后面的参数；`false` 时尾部多出内容就不算命中。
        allows_argument: bool,
    },
    /// 去掉全部空白后精确相等。
    ///
    /// 歌单命令用这种：OCR 会吞掉空格，因此必须以压缩文本比对。
    Compact,
}

/// 一条命令的完整登记项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    /// 主名称，也是帮助与面板里显示的名字。
    ///
    /// **一旦发布不要改名**：它同时是面板上的稳定标识。
    pub(crate) name: &'static str,
    /// 其它可触发写法（同义别名），全部等价。
    ///
    /// 同义命令（如 `点歌`/`QQ点歌`、`网易点歌`/`网易云点歌`）在所属功能注册表统一登记。
    pub(crate) aliases: &'static [&'static str],
    pub(crate) prefix: CommandPrefix,
    pub(crate) match_rule: CommandMatch,
    pub(crate) group: CommandGroup,
    /// 允许触发的来源。空表示仅由会话态或自定义流程处理。
    pub(crate) authorities: &'static [CommandAuthority],
    /// 一行说明，面板上显示。
    pub(crate) summary: &'static str,
    /// 用法示例（无空格语法：写 `@点歌晴天`，不写 `@点歌 晴天`）。
    pub(crate) usage: &'static [&'static str],
    /// 隐藏命令：功能存在但不出现在帮助里。
    pub(crate) hidden: bool,
}

impl CommandSpec {
    /// 该命令的全部可触发写法（主名在前）。
    pub(crate) fn all_names(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }

    /// 文本是否命中这条命令。
    ///
    /// 与路由层共用同一份判定，避免"面板显示了但实际不认"的漂移。
    pub(crate) fn matches_text(&self, text: &str) -> bool {
        self.all_names().any(|name| self.matches_name(text, name))
    }

    fn matches_name(&self, text: &str, name: &'static str) -> bool {
        match self.match_rule {
            CommandMatch::Prefix { allows_argument } => {
                crate::features::chat_text::parse_prefixed_command(text, name, allows_argument)
                    .is_some()
            }
            CommandMatch::Compact => crate::features::chat_text::compact_command(text) == name,
        }
    }
}

/// 全部已登记命令（按分组顺序拼接）。
///
/// AI 聊天用它构造"提问者可用命令"提示，避免模型给出越权或不存在命令。
pub(crate) fn all_specs() -> Vec<&'static CommandSpec> {
    // 与 static_commands() 保持同一组表。此前遗漏 FRIEND_MISC，
    // 导致 @邀请/@拉黑/@屏蔽 不进 AI 提示词的可用命令列表。
    static_commands().collect()
}

/// 会话态命令：某个玩法进行中才生效的派发规则。
///
/// 对应 python-telegram-bot `ConversationHandler` 的 `states`：
/// "状态 → 该状态下可用的 handler 列表"。本项目状态是单值的
/// `EntertainmentKind`（同一时刻只有一个玩法），因此这里是状态到规则的映射。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SessionDispatch {
    /// 该规则适用的玩法状态。
    pub(crate) kind: crate::features::entertainment::EntertainmentKind,
    /// 认领后归属的路由模块。
    ///
    /// 有了这一列，路由才能直接从这张表派发，而不必在 `router.rs` 里再写一份
    /// "玩法状态 → 模块" 的 match（那是一份会漂移的重复数据）。
    pub(crate) module: crate::features::command::ChatCommandModule,
    /// 认领谓词：由所属模块提供（各模块现成的 `claims_active_chat`）。
    pub(crate) claims: fn(&crate::features::command::CommandEnvelope) -> bool,
    /// 解析谓词：由所属模块提供（各模块现成的 `parse_active_chat`）。
    ///
    /// 返回完整的匹配结果而非裸命令：路由需要 `matched`/`raw` 参与命令锁与日志。
    pub(crate) parse: fn(
        &crate::features::command::CommandEnvelope,
    ) -> Option<
        crate::features::command::FeatureCommandMatch<crate::features::command::ModuleCommand>,
    >,

    /// 会话内是否接受**任意**文本（由玩法自己兜底解释）。
    ///
    /// `true`：成语接龙（任意文本当成语提交）、斗地主（任意文本当出牌）。
    /// `false`：海龟汤、谁是卧底（只认固定关键词）。
    ///
    /// 面板据此显示"会话中可自由输入"，不会让用户误以为只支持保留关键词。
    pub(crate) open_ended: bool,
    /// 面板上展示的会话内保留关键词（open_ended 为 true 时的补充说明）。
    pub(crate) keywords: &'static [&'static str],
    /// 玩法名（面板分组用）。
    pub(crate) label: &'static str,
}

// ---------------------------------------------------------------------------
// 命令登记数据
// ---------------------------------------------------------------------------

use crate::features::command::CommandAuthority::{Friend, HallMember};
use crate::features::command::CommandPrefix::{At, Hash};

const BOTH: &[CommandAuthority] = &[HallMember, Friend];
const HALL_ONLY: &[CommandAuthority] = &[HallMember];
const FRIEND_ONLY: &[CommandAuthority] = &[Friend];

/// 播放控制命令（@ 前缀，大厅为主）。
pub(crate) const PLAYBACK: &[CommandSpec] = &[
    CommandSpec {
        name: "暂停",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "暂停当前播放",
        usage: &["@暂停"],
        hidden: false,
    },
    CommandSpec {
        name: "继续",
        aliases: &["恢复"],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "继续播放",
        usage: &["@继续"],
        hidden: false,
    },
    CommandSpec {
        name: "播放",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "开始播放",
        usage: &["@播放"],
        hidden: false,
    },
    CommandSpec {
        name: "下一首",
        aliases: &["下一曲"],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "切到下一首",
        usage: &["@下一首"],
        hidden: false,
    },
    CommandSpec {
        name: "上一首",
        aliases: &["上一曲"],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "回到上一首",
        usage: &["@上一首"],
        hidden: false,
    },
    CommandSpec {
        name: "删除",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: FRIEND_ONLY,
        summary: "好友删除当前曲目",
        usage: &["@删除"],
        hidden: false,
    },
    CommandSpec {
        name: "音量",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "设置音量",
        usage: &["@音量50"],
        hidden: false,
    },
    CommandSpec {
        name: "状态",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "报告当前播放状态",
        usage: &["@状态"],
        hidden: false,
    },
    CommandSpec {
        name: "队列",
        aliases: &["列表"],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "查看点歌队列",
        usage: &["@队列"],
        hidden: false,
    },
    CommandSpec {
        name: "队列删除",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "从队列删除指定序号",
        usage: &["@队列删除2"],
        hidden: false,
    },
    CommandSpec {
        name: "队列清空",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "清空点歌队列",
        usage: &["@队列清空"],
        hidden: false,
    },
    CommandSpec {
        name: "歌词",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "发送歌词",
        usage: &["@歌词"],
        hidden: false,
    },
    CommandSpec {
        name: "切换",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "切换歌词模式",
        usage: &["@切换"],
        hidden: false,
    },
    CommandSpec {
        name: "停止歌词",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "停止后台歌词",
        usage: &["@停止歌词"],
        hidden: false,
    },
    CommandSpec {
        name: "后台歌词",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "切到后台歌词",
        usage: &["@后台歌词"],
        hidden: false,
    },
    CommandSpec {
        name: "单曲歌词",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "单曲歌词模式",
        usage: &["@单曲歌词"],
        hidden: false,
    },
    CommandSpec {
        name: "持续歌词",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Playback,
        authorities: HALL_ONLY,
        summary: "持续歌词模式",
        usage: &["@持续歌词"],
        hidden: false,
    },
];

/// 大厅类命令（@ 前缀）。
pub(crate) const HALL: &[CommandSpec] = &[
    CommandSpec {
        name: "大厅检测",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Hall,
        authorities: HALL_ONLY,
        summary: "检测当前所在大厅",
        usage: &["@大厅检测"],
        hidden: false,
    },
    CommandSpec {
        name: "大厅时间",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Hall,
        authorities: HALL_ONLY,
        summary: "报告大厅剩余时间",
        usage: &["@大厅时间"],
        hidden: false,
    },
    CommandSpec {
        name: "麦克风",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Hall,
        authorities: FRIEND_ONLY,
        summary: "切换麦克风状态",
        usage: &["@麦克风"],
        hidden: false,
    },
    CommandSpec {
        name: "忘记我",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Hall,
        authorities: HALL_ONLY,
        summary: "清空 AI 对你说过的记忆",
        usage: &["@忘记我"],
        hidden: false,
    },
];

/// 管理类命令（@ 与 # 两种前缀）。
pub(crate) const ADMINISTRATION: &[CommandSpec] = &[
    CommandSpec {
        name: "帮助",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Administration,
        authorities: HALL_ONLY,
        summary: "发送点歌帮助",
        usage: &["@帮助"],
        hidden: false,
    },
    CommandSpec {
        name: "娱乐帮助",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Administration,
        authorities: HALL_ONLY,
        summary: "发送娱乐玩法帮助",
        usage: &["@娱乐帮助"],
        hidden: false,
    },
    CommandSpec {
        name: "娱乐帮助",
        aliases: &["娱乐", "帮助"],
        prefix: Hash,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Administration,
        authorities: HALL_ONLY,
        summary: "井号别名发送娱乐玩法帮助",
        usage: &["#娱乐", "#帮助", "#娱乐帮助"],
        hidden: false,
    },
    CommandSpec {
        name: "值守模式",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Administration,
        authorities: FRIEND_ONLY,
        summary: "切换聊天值守模式",
        usage: &["@值守模式一级", "@值守模式状态"],
        hidden: false,
    },
    CommandSpec {
        name: "启用",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Administration,
        authorities: FRIEND_ONLY,
        summary: "启用大厅命令识别",
        usage: &["@启用"],
        hidden: false,
    },
    CommandSpec {
        name: "禁用",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Administration,
        authorities: FRIEND_ONLY,
        summary: "禁用大厅命令识别",
        usage: &["@禁用"],
        hidden: false,
    },
    CommandSpec {
        name: "闲置退出",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Administration,
        authorities: FRIEND_ONLY,
        summary: "设置闲置退出分钟数",
        usage: &["@闲置退出30"],
        hidden: false,
    },
];

/// 娱乐玩法开局命令（# 前缀）。会话内命令见 `session_dispatch`。
pub(crate) const ENTERTAINMENT_START: &[CommandSpec] = &[
    CommandSpec {
        name: "塔罗牌",
        aliases: &["塔罗牌AI", "塔罗牌ai"],
        prefix: Hash,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "抽当天塔罗三张，带问题自动 AI 解读",
        usage: &["#塔罗牌", "#塔罗牌今天运势如何", "#塔罗牌AI"],
        hidden: false,
    },
    CommandSpec {
        name: "塔罗牌重抽",
        aliases: &[],
        prefix: Hash,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "提前换一组塔罗牌",
        usage: &["#塔罗牌重抽"],
        hidden: false,
    },
    CommandSpec {
        name: "塔罗牌帮助",
        aliases: &[],
        prefix: Hash,
        match_rule: CommandMatch::Prefix {
            allows_argument: false,
        },
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "塔罗玩法说明",
        usage: &["#塔罗牌帮助"],
        hidden: false,
    },
    CommandSpec {
        name: "接龙",
        aliases: &[],
        prefix: Hash,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "随机开局成语接龙",
        usage: &["#接龙", "#接龙一心一意"],
        hidden: false,
    },
    CommandSpec {
        name: "同音接龙",
        aliases: &[],
        prefix: Hash,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "同音模式开局成语接龙",
        usage: &["#同音接龙"],
        hidden: false,
    },
    CommandSpec {
        name: "斗地主",
        aliases: &[],
        prefix: Hash,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "开局斗地主",
        usage: &["#斗地主"],
        hidden: false,
    },
    CommandSpec {
        name: "跑得快",
        aliases: &[],
        prefix: Hash,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "开局跑得快",
        usage: &["#跑得快"],
        hidden: false,
    },
    CommandSpec {
        name: "海龟汤",
        aliases: &[],
        prefix: Hash,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "开局海龟汤",
        usage: &["#海龟汤"],
        hidden: false,
    },
    CommandSpec {
        name: "卧底",
        aliases: &["卧底双"],
        prefix: Hash,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Entertainment,
        authorities: HALL_ONLY,
        summary: "开局谁是卧底",
        usage: &["#卧底", "#卧底双"],
        hidden: false,
    },
];

/// 歌单命令（@ 前缀，压缩文本精确匹配）。
pub(crate) const PLAYLIST: &[CommandSpec] = &[
    CommandSpec {
        name: "歌单",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "打开歌单窗口",
        usage: &["@歌单"],
        hidden: false,
    },
    CommandSpec {
        name: "歌单帮助",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "歌单用法说明",
        usage: &["@歌单帮助"],
        hidden: false,
    },
    CommandSpec {
        name: "换一批",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "换一批候选",
        usage: &["@换一批"],
        hidden: false,
    },
    CommandSpec {
        name: "收藏",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "收藏当前曲目",
        usage: &["@收藏"],
        hidden: false,
    },
    CommandSpec {
        name: "取消收藏",
        aliases: &["歌单删除"],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "取消收藏",
        usage: &["@取消收藏"],
        hidden: false,
    },
    CommandSpec {
        name: "歌单随机",
        aliases: &["随机歌单"],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "随机播放歌单",
        usage: &["@歌单随机"],
        hidden: false,
    },
    CommandSpec {
        name: "播放歌单",
        aliases: &["歌单模式"],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "进入歌单播放模式",
        usage: &["@播放歌单"],
        hidden: false,
    },
    CommandSpec {
        name: "大厅模式",
        aliases: &["退出歌单模式"],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "退出歌单模式回到大厅模式",
        usage: &["@大厅模式"],
        hidden: false,
    },
    CommandSpec {
        name: "关闭",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "关闭歌单窗口",
        usage: &["@关闭"],
        hidden: false,
    },
    // 以下 7 条此前只在 playlist.rs 的 parse_chat 里存在，catalog 未登记，
    // 导致游戏里能用、面板与帮助里看不到（漂移）。
    CommandSpec {
        name: "停止播放歌单",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "停止个人歌单播放",
        usage: &["@停止播放歌单"],
        hidden: false,
    },
    CommandSpec {
        name: "歌单清空",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "清空自己的歌单（需二次确认）",
        usage: &["@歌单清空"],
        hidden: false,
    },
    CommandSpec {
        name: "确认清空",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "确认清空歌单",
        usage: &["@确认清空"],
        hidden: false,
    },
    CommandSpec {
        name: "放弃清空",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "放弃清空歌单",
        usage: &["@放弃清空"],
        hidden: false,
    },
    CommandSpec {
        name: "确认导入",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "确认导入歌单",
        usage: &["@确认导入"],
        hidden: false,
    },
    CommandSpec {
        name: "取消导入",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Compact,
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "取消导入歌单",
        usage: &["@取消导入"],
        hidden: false,
    },
    CommandSpec {
        name: "导入QQ歌单",
        aliases: &["导入网易歌单", "导入网易云歌单"],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Song,
        authorities: FRIEND_ONLY,
        summary: "按分享链接导入歌单",
        usage: &["@导入QQ歌单<链接>"],
        hidden: false,
    },
];

/// 好友专属命令（邀请、管理）。
///
/// 刻意不登记 `同意`/`不同意`：它们是 `is_reserved_decision_command` 里的保留决策语法，
/// 由独占决策窗口消费，不属于静态命令层（见 router.rs 的该函数与三层命令模型）。
pub(crate) const FRIEND_MISC: &[CommandSpec] = &[
    CommandSpec {
        name: "邀请",
        aliases: &[],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Administration,
        authorities: FRIEND_ONLY,
        summary: "邀请好友进入大厅",
        usage: &["@邀请昵称"],
        hidden: false,
    },
    CommandSpec {
        name: "拉黑",
        aliases: &["拉黑UID"],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Administration,
        authorities: FRIEND_ONLY,
        summary: "拉黑指定 UID",
        usage: &["@拉黑UID123456789"],
        hidden: false,
    },
    CommandSpec {
        name: "屏蔽",
        aliases: &["屏蔽UID"],
        prefix: At,
        match_rule: CommandMatch::Prefix {
            allows_argument: true,
        },
        group: CommandGroup::Administration,
        authorities: FRIEND_ONLY,
        summary: "屏蔽指定 UID",
        usage: &["@屏蔽UID123456789"],
        hidden: false,
    },
];

/// 全部静态命令（不含会话态命令）。
pub(crate) fn static_commands() -> impl Iterator<Item = &'static CommandSpec> {
    crate::features::song_request::command_specs()
        .iter()
        .chain(PLAYBACK.iter())
        .chain(HALL.iter())
        .chain(ADMINISTRATION.iter())
        .chain(ENTERTAINMENT_START.iter())
        .chain(PLAYLIST.iter())
        .chain(FRIEND_MISC.iter())
}

/// 会话态派发规则：玩法进行中才生效的命令。
///
/// 对应 PTB `ConversationHandler` 的 `states`（状态 → 可用 handler 列表）。
/// 本项目状态是单值 `EntertainmentKind`，因此这里是"状态 → 规则"的映射。
///
/// 认领与解析谓词直接复用各模块现成的函数（`claims_active_chat` / `parse_active_chat`），
/// 因此本表只把"状态与模块的绑定"从代码分支变成可枚举数据，不改变任何匹配行为。
pub(crate) const SESSION_DISPATCH: &[SessionDispatch] = &[
    SessionDispatch {
        kind: crate::features::entertainment::EntertainmentKind::IdiomChain,
        module: crate::features::command::ChatCommandModule::IdiomChain,
        claims: crate::features::idiom_chain::IdiomChainCommand::claims_active_chat,
        parse: |envelope| {
            crate::features::idiom_chain::IdiomChainCommand::parse_active_chat(envelope)
                .map(|matched| matched.map(crate::features::command::ModuleCommand::IdiomChain))
        },
        // 成语接龙进行中，任何没有被保留关键词接住的 # 文本都会被当成"提交的成语"。
        open_ended: true,
        keywords: &["提示", "状态", "结束", "解释"],
        label: "成语接龙",
    },
    SessionDispatch {
        kind: crate::features::entertainment::EntertainmentKind::Landlord,
        module: crate::features::command::ChatCommandModule::CardGame,
        claims: crate::features::card_games::LandlordCommand::claims_active_chat,
        parse: |envelope| {
            crate::features::card_games::LandlordCommand::parse_active_chat(envelope)
                .map(|matched| matched.map(crate::features::command::ModuleCommand::CardGame))
        },
        // 斗地主进行中，任何没有被保留关键词接住的文本都会被当成"出的牌"。
        open_ended: true,
        keywords: &["加入", "抢", "不抢", "过", "状态", "重试", "结束", "手牌"],
        label: "斗地主",
    },
    SessionDispatch {
        kind: crate::features::entertainment::EntertainmentKind::RunFast,
        module: crate::features::command::ChatCommandModule::CardGame,
        claims: crate::features::card_games::LandlordCommand::claims_active_chat,
        parse: |envelope| {
            crate::features::card_games::LandlordCommand::parse_active_chat(envelope)
                .map(|matched| matched.map(crate::features::command::ModuleCommand::CardGame))
        },
        open_ended: true,
        keywords: &["加入", "过", "状态", "重试", "结束", "手牌"],
        label: "跑得快",
    },
    SessionDispatch {
        kind: crate::features::entertainment::EntertainmentKind::TurtleSoup,
        module: crate::features::command::ChatCommandModule::TurtleSoup,
        claims: crate::features::turtle_soup::TurtleSoupCommand::claims_active_chat,
        parse: |envelope| {
            crate::features::turtle_soup::TurtleSoupCommand::parse_active_chat(envelope)
                .map(|matched| matched.map(crate::features::command::ModuleCommand::TurtleSoup))
        },
        // 海龟汤只认固定关键词，不兜底解释任意文本。
        open_ended: false,
        keywords: &["状态", "结束"],
        label: "海龟汤",
    },
    SessionDispatch {
        kind: crate::features::entertainment::EntertainmentKind::Undercover,
        module: crate::features::command::ChatCommandModule::Undercover,
        claims: crate::features::undercover::UndercoverCommand::claims_active_chat,
        parse: |envelope| {
            crate::features::undercover::UndercoverCommand::parse_active_chat(envelope)
                .map(|matched| matched.map(crate::features::command::ModuleCommand::Undercover))
        },
        // 大厅成员发任何 # 文本都会进入卧底的 parse_active 分支，但解析器只接受固定动作，
        // 因此这里如实标为 false（不兜底）。
        open_ended: false,
        keywords: &[
            "开局", "状态", "退出", "结束", "重试", "内容", "加入", "弃权", "谜底",
        ],
        label: "谁是卧底",
    },
];

/// 按当前玩法状态查可用规则。
///
/// `None` 表示没有玩法在进行，此时会话态规则一条都不适用（只有开局命令能触发）。
pub(crate) fn session_rules(
    active: Option<crate::features::entertainment::EntertainmentKind>,
) -> &'static [SessionDispatch] {
    let Some(kind) = active else {
        return &[];
    };
    // 用子切片定位：同一 kind 的规则在表里连续出现（RunFast 沿用 Landlord 的谓词）。
    let start = SESSION_DISPATCH
        .iter()
        .position(|rule| rule.kind == kind)
        .unwrap_or(SESSION_DISPATCH.len());
    let len = SESSION_DISPATCH[start..]
        .iter()
        .take_while(|rule| rule.kind == kind)
        .count();
    &SESSION_DISPATCH[start..start + len]
}

// ---------------------------------------------------------------------------
// 面板数据：把注册表序列化成 /commands/catalog 的响应
// ---------------------------------------------------------------------------

/// 面板上一条命令的视图。
///
/// 字段名按 camelCase 序列化：这是与 page.html 的冻结契约，改动会直接让面板读不到字段。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandView {
    pub(crate) name: &'static str,
    pub(crate) aliases: Vec<&'static str>,
    /// "@" 或 "#"
    pub(crate) prefix: &'static str,
    /// "hall" / "friend"
    pub(crate) authorities: Vec<&'static str>,
    pub(crate) summary: &'static str,
    pub(crate) usage: Vec<&'static str>,
    /// 示例：与 usage 分开，面板会把它们放在"示例"一行。
    /// 目前与 usage 同源（用法即示例），保留独立字段是为了将来能只给示例不给用法。
    pub(crate) examples: Vec<&'static str>,
    /// 会话内是否接受任意输入。
    #[serde(rename = "openEnded")]
    pub(crate) open_end: bool,
    pub(crate) hidden: bool,
    /// 该命令当前是否可用（全局开关关闭时，大厅命令对普通成员不可用）。
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandGroupView {
    pub(crate) name: &'static str,
    pub(crate) commands: Vec<CommandView>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandCatalogView {
    pub(crate) commands_enabled: bool,
    pub(crate) groups: Vec<CommandGroupView>,
}

fn prefix_str(prefix: CommandPrefix) -> &'static str {
    match prefix {
        CommandPrefix::At => "@",
        CommandPrefix::Hash => "#",
        CommandPrefix::Amp => "&",
    }
}

fn authority_str(authority: CommandAuthority) -> &'static str {
    match authority {
        CommandAuthority::HallMember => "hall",
        CommandAuthority::Friend => "friend",
    }
}

/// 构造面板数据。
///
/// `commands_enabled` 是全局开关：关闭时大厅普通成员的命令全部不可用，
/// 因此这里据实把大厅命令标为未启用（好友命令不受全局开关影响，见
/// `listener.rs` 的 `command_is_disabled`）。
///
/// 会话态规则不进 `groups`：它们没有稳定的命令名，而是玩法进行中的开放输入。
/// 面板通过 `open_end` 标志与 `session_rules()` 另行说明。
pub(crate) fn catalog_view(commands_enabled: bool) -> CommandCatalogView {
    let mut groups: Vec<CommandGroupView> = Vec::new();
    for spec in static_commands() {
        let hall_only = spec
            .authorities
            .iter()
            .all(|authority| *authority == CommandAuthority::HallMember);
        let enabled = commands_enabled || !hall_only;
        let view = CommandView {
            name: spec.name,
            aliases: spec.aliases.to_vec(),
            prefix: prefix_str(spec.prefix),
            authorities: spec.authorities.iter().map(|a| authority_str(*a)).collect(),
            summary: spec.summary,
            usage: spec.usage.to_vec(),
            examples: spec.usage.to_vec(),
            open_end: false,
            hidden: spec.hidden,
            enabled,
        };
        match groups
            .iter_mut()
            .find(|group| group.name == spec.group.label())
        {
            Some(group) => group.commands.push(view),
            None => groups.push(CommandGroupView {
                name: spec.group.label(),
                commands: vec![view],
            }),
        }
    }
    // 会话态规则作为"进行中可用的玩法"并入娱乐组，让面板能如实展示
    // "哪些玩法进行中会接管 # 命令"，而不是把它们伪装成普通命令。
    if let Some(entertainment) = groups
        .iter_mut()
        .find(|group| group.name == CommandGroup::Entertainment.label())
    {
        for rule in SESSION_DISPATCH {
            entertainment.commands.push(CommandView {
                name: rule.label,
                aliases: rule.keywords.to_vec(),
                prefix: "#",
                authorities: vec!["hall"],
                summary: if rule.open_ended {
                    "玩法进行中接受任意输入"
                } else {
                    "玩法进行中可用的保留指令"
                },
                usage: rule.keywords.to_vec(),
                examples: rule.keywords.to_vec(),
                open_end: rule.open_ended,
                hidden: false,
                enabled: commands_enabled,
            });
        }
    }
    // 按功能模块的既定顺序排序（点歌/播放/大厅/娱乐/管理），
    // 不能用中文字典序——那样"点歌"不会排在首位。
    let order_of = |name: &str| {
        CommandGroup::ALL
            .iter()
            .find(|group| group.label() == name)
            .map(|group| group.order())
            .unwrap_or(u8::MAX)
    };
    groups.sort_by_key(|group| order_of(group.name));
    CommandCatalogView {
        commands_enabled,
        groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_catalog_reuses_platform_registry_without_duplicate_policy_ids() {
        let registered = crate::features::song_request::command_specs();
        assert_eq!(registered.len(), 5);
        let catalog = all_specs();
        for spec in registered {
            assert_eq!(
                catalog
                    .iter()
                    .filter(|entry| entry.name == spec.name && entry.prefix == spec.prefix)
                    .count(),
                1
            );
            assert!(catalog.iter().any(|entry| std::ptr::eq(*entry, spec)));
        }
        let names: std::collections::BTreeSet<_> =
            registered.iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            ["QQ点歌", "网易点歌", "酷狗点歌", "B站点歌", "AI点歌"]
                .into_iter()
                .collect()
        );
        for spec in catalog {
            assert!(
                spec.all_names().all(|name| !name.contains("搜索")),
                "removed search entry: {}",
                spec.name
            );
        }
    }

    #[test]
    fn catalog_entries_have_unique_prefix_name_and_authority_combinations() {
        // 同一命令在不同来源下可以分别登记（大厅与好友的点歌集不同），
        // 但"前缀 + 写法 + 来源"三者完全相同就是重复登记。
        let mut seen = std::collections::BTreeSet::new();
        for spec in static_commands() {
            for name in spec.all_names() {
                for authority in spec.authorities {
                    let key = (format!("{:?}", spec.prefix), name, format!("{authority:?}"));
                    assert!(
                        seen.insert(key.clone()),
                        "命令重复登记：{:?} {} @{}",
                        key.0,
                        key.1,
                        key.2
                    );
                }
            }
        }
    }

    #[test]
    fn every_entry_declares_at_least_one_authority_and_a_summary() {
        for spec in static_commands() {
            assert!(
                !spec.authorities.is_empty(),
                "{} 没有声明任何来源",
                spec.name
            );
            assert!(!spec.summary.is_empty(), "{} 缺少摘要", spec.name);
            assert!(!spec.usage.is_empty(), "{} 缺少用法示例", spec.name);
        }
    }

    #[test]
    fn usage_examples_never_rely_on_a_space() {
        // MUST_READ §1：命令不依赖空格分隔。用法示例里出现 "命令 参数" 会误导用户。
        for spec in static_commands() {
            for usage in spec.usage {
                let body = usage.trim_start_matches('@').trim_start_matches('#');
                let name = spec
                    .all_names()
                    .find(|name| body.starts_with(*name))
                    .unwrap_or_else(|| panic!("{} 的用法示例不以命令名开头：{usage}", spec.name));
                let rest = &body[name.len()..];
                assert!(
                    !rest.starts_with(' '),
                    "{} 的用法示例依赖空格：{usage}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn prefix_matching_follows_the_shared_command_boundary_rules() {
        let pause = PLAYBACK
            .iter()
            .find(|spec| spec.name == "暂停")
            .expect("暂停 已登记");
        assert!(pause.matches_text("暂停"));
        assert!(pause.matches_text("暂停】"));
        // 无参数命令后面多出内容就不该命中。
        assert!(!pause.matches_text("暂停现在"));

        let song = crate::features::song_request::command_specs()
            .iter()
            .find(|spec| spec.name == "QQ点歌")
            .expect("QQ点歌 已登记");
        assert!(song.matches_text("点歌晴天"));
        assert!(song.matches_text("点歌 晴天"));
        assert!(song.matches_text("QQ点歌晴天"), "默认点歌别名必须可匹配");
        assert!(!song.matches_text("AI点歌晴天"), "AI 点歌是独立权限入口");
    }

    #[test]
    fn compact_matching_ignores_whitespace_like_the_real_parser() {
        let soup = ENTERTAINMENT_START
            .iter()
            .find(|spec| spec.name == "海龟汤")
            .expect("海龟汤 已登记");
        assert!(soup.matches_text("海龟汤"));
        assert!(soup.matches_text("海 龟 汤"), "OCR 会吞空格，压缩后应命中");
    }

    #[test]
    fn session_rules_are_looked_up_by_the_active_entertainment() {
        use crate::features::entertainment::EntertainmentKind;
        assert!(session_rules(None).is_empty(), "无玩法时不应有会话态规则");
        for kind in [
            EntertainmentKind::IdiomChain,
            EntertainmentKind::Landlord,
            EntertainmentKind::RunFast,
            EntertainmentKind::TurtleSoup,
            EntertainmentKind::Undercover,
        ] {
            let rules = session_rules(Some(kind));
            assert!(!rules.is_empty(), "{kind:?} 没有会话态规则");
            assert!(
                rules.iter().all(|rule| rule.kind == kind),
                "{kind:?} 查到了别的玩法的规则"
            );
        }
    }

    #[test]
    fn open_ended_flags_match_the_real_parsers() {
        use crate::features::entertainment::EntertainmentKind;
        let open_ended = |kind| {
            session_rules(Some(kind))
                .first()
                .map(|rule| rule.open_ended)
                .unwrap_or(false)
        };
        // 成语接龙与斗地主/跑得快有 "_ => Submit/Play" 兜底。
        assert!(open_ended(EntertainmentKind::IdiomChain));
        assert!(open_ended(EntertainmentKind::Landlord));
        assert!(open_ended(EntertainmentKind::RunFast));
        // 海龟汤与谁是卧底只认固定关键词。
        assert!(!open_ended(EntertainmentKind::TurtleSoup));
        assert!(!open_ended(EntertainmentKind::Undercover));
    }

    #[test]
    fn catalog_view_groups_commands_under_their_functional_module() {
        let view = catalog_view(true);
        let names: Vec<&str> = view.groups.iter().map(|group| group.name).collect();
        for expected in ["点歌", "播放", "大厅", "娱乐", "管理"] {
            assert!(names.contains(&expected), "缺少分组 {expected}：{names:?}");
        }
        // 分组顺序必须稳定（面板按此渲染）。
        assert_eq!(names[0], "点歌");
        assert!(view.commands_enabled);
    }

    #[test]
    fn catalog_view_marks_hall_commands_disabled_when_the_global_switch_is_off() {
        // 全局开关只挡大厅普通成员；好友命令不受影响（见 listener.rs 的 command_is_disabled）。
        let view = catalog_view(false);
        assert!(!view.commands_enabled);
        for group in &view.groups {
            for command in &group.commands {
                let hall_only = command.authorities.iter().all(|a| *a == "hall");
                if hall_only {
                    assert!(
                        !command.enabled,
                        "{} 仅限大厅，开关关闭时应标记为未启用",
                        command.name
                    );
                } else {
                    assert!(
                        command.enabled,
                        "{} 对好友可用，不应随大厅开关关闭",
                        command.name
                    );
                }
            }
        }
    }

    #[test]
    fn catalog_view_reports_open_ended_session_play_as_such() {
        let view = catalog_view(true);
        let entertainment = view
            .groups
            .iter()
            .find(|group| group.name == "娱乐")
            .expect("娱乐分组");
        let open_ended: Vec<&str> = entertainment
            .commands
            .iter()
            .filter(|command| command.open_end)
            .map(|command| command.name)
            .collect();
        // 这三个玩法有 "_ =>" 兜底，会话中接受任意输入。
        for expected in ["成语接龙", "斗地主", "跑得快"] {
            assert!(
                open_ended.contains(&expected),
                "{expected} 应标记为会话中可自由输入，实际：{open_ended:?}"
            );
        }
        // 这两个只认固定关键词，不能标成 open_end。
        for unexpected in ["海龟汤", "谁是卧底"] {
            assert!(
                !open_ended.contains(&unexpected),
                "{unexpected} 只认固定关键词，不应标记为可自由输入"
            );
        }
    }

    #[test]
    fn catalog_view_is_serializable_in_the_shape_the_panel_expects() {
        let view = catalog_view(true);
        let json = serde_json::to_value(&view).expect("序列化");
        assert!(json["commandsEnabled"].is_boolean());
        assert!(json["groups"].is_array());
        let first = &json["groups"][0];
        assert!(first["name"].is_string());
        assert!(first["commands"].is_array());
        let command = &first["commands"][0];
        for field in ["name", "aliases", "prefix", "authorities", "summary"] {
            assert!(!command[field].is_null(), "面板契约要求字段 {field} 存在");
        }
        assert!(command["usage"].is_array());
        assert!(command["openEnded"].is_boolean());
        assert!(command["examples"].is_array());
        assert!(command["hidden"].is_boolean());
        assert!(command["enabled"].is_boolean());
    }

    #[test]
    fn usage_and_examples_are_identical_so_the_panel_shows_one_row() {
        // 面板在两者渲染结果相同时只展示“示例”一行。这条测试钉住前提：
        // 现状 examples 就是 usage 的副本；若将来真给命令单独写示例，
        // 面板会自动恢复成“用法 + 示例”两行，这里也需要同步更新。
        for command in catalog_view(true)
            .groups
            .iter()
            .flat_map(|group| group.commands.iter())
        {
            assert_eq!(
                command.usage, command.examples,
                "命令 {} 的 usage 与 examples 应同源",
                command.name
            );
        }
    }

    /// 构造与真实链路一致的信封：走 CommandEnvelope::new 的同一套解析。
    fn envelope_for(
        raw: &str,
        prefix: CommandPrefix,
        authority: CommandAuthority,
    ) -> crate::features::command::CommandEnvelope {
        let glyph = match prefix {
            CommandPrefix::At => "@",
            CommandPrefix::Hash => "#",
            CommandPrefix::Amp => "&",
        };
        let message_type = match authority {
            CommandAuthority::HallMember => "blue",
            CommandAuthority::Friend => "pink",
        };
        crate::features::command::CommandEnvelope::new(
            format!("{glyph}{raw}"),
            "测试用户",
            message_type,
            format!("{glyph}{raw}"),
            crate::features::command::CommandObservation::default(),
        )
        .expect("构造信封")
    }

    /// 注册表登记的每条别名，都必须被真实模块的 claims_chat 认领。
    ///
    /// 这是"面板显示得出来、游戏里却不认"这类漂移的探针。
    #[test]
    fn every_registered_alias_is_claimed_by_a_real_module() {
        // 带参数的命令（如 @导入QQ歌单<链接>）用裸命令名认领不到，
        // 因此优先用 spec.usage 里的第一个示例构造信封；示例本身也是登记的一部分。
        let claimed = |spec: &CommandSpec, name: &str, authority: CommandAuthority| -> bool {
            let sample = spec
                .usage
                .first()
                .map(|usage| usage.trim_start_matches(['@', '#']))
                .filter(|usage| !usage.is_empty())
                .unwrap_or(name);
            let envelope = envelope_for(sample, spec.prefix, authority);
            let _ = name;
            match spec.prefix {
                CommandPrefix::At => {
                    crate::features::song_request::SongCommand::claims_chat(&envelope)
                        || crate::features::playback::PlaybackCommand::claims_chat(&envelope)
                        || crate::features::hall::HallCommand::claims_chat(&envelope)
                        || crate::features::administration::AdministrationCommand::claims_chat(
                            &envelope,
                        )
                        || crate::features::playlist::PlaylistCommand::claims_chat(&envelope)
                        || crate::features::invite::InviteCommand::claims_chat(&envelope)
                        || crate::features::moderation::ModerationCommand::claims_chat(&envelope)
                        || crate::features::ai_chat::AiChatCommand::claims_chat(&envelope)
                }
                CommandPrefix::Hash => {
                    // # 开局命令的认领入口是 claims_start_chat（不是 claims_chat），
                    // 与 router.rs 的 select_hash_module 候选表保持一致。
                    crate::features::tarot::TarotCommand::claims_chat(&envelope)
                        || crate::features::idiom_chain::IdiomChainCommand::claims_start_chat(
                            &envelope,
                        )
                        || crate::features::card_games::LandlordCommand::claims_start_chat(
                            &envelope,
                        )
                        || crate::features::turtle_soup::TurtleSoupCommand::claims_start_chat(
                            &envelope,
                        )
                        || crate::features::undercover::UndercoverCommand::claims_start_chat(
                            &envelope,
                        )
                        || crate::features::administration::AdministrationCommand::claims_chat(
                            &envelope,
                        )
                }
                CommandPrefix::Amp => false,
            }
        };

        let mut unclaimed: Vec<String> = Vec::new();
        for spec in static_commands() {
            for authority in spec.authorities {
                for name in spec.all_names() {
                    if !claimed(spec, name, *authority) {
                        unclaimed.push(format!("{} ({name}, {authority:?})", spec.name));
                    }
                }
            }
        }
        assert!(
            unclaimed.is_empty(),
            "以下登记项在真实路由中无人认领：{unclaimed:#?}"
        );
    }
}
