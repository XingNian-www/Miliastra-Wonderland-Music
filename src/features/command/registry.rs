//! 玩法模块的单一登记表（W4b）。
//!
//! # 为什么需要它
//!
//! 「新增一个玩法」过去要同时动五处：`interfaces/chat/router.rs` 的两张手写表、
//! `features/command.rs` 的两个 15 变体枚举、`features/command_catalog.rs` 的命令
//! 字面量、`features/reply_template/ownership.rs` 的 578 行回复归属表。这五处没有
//! 一处是「事实」——全是同一个事实的五份手抄。漏抄一处不会编译失败、不会测试失败，
//! 只会静默少一条命令或少一组可编辑文案。
//!
//! 本模块把那个事实收敛成**一次宏调用**：每个玩法一段，登记模块名 / 载荷类型 /
//! 中文名 / 配置段 / 命令名与前缀 / 私密载荷分类 / 拥有的回复分组。
//! `ChatCommandModule`、`ModuleCommand`、`MODULE_SPECS`、`REPLY_GROUPS` 全部由
//! [`define_module_registry!`] 派生。
//!
//! # 派生边界（刻意不派生的部分）
//!
//! `AT_HANDLERS` / `SESSION_DISPATCH` 的 `claims`/`parse` **不派生**：每个模块的签名
//! 不同（`AdministrationCommand::parse_chat(envelope).map(|m| m.map(
//! ModuleCommand::Administration))`），从「模块名」推不出「类型与方法」。那两张表保持
//! 手写，由测试反查覆盖——把「漏登记」从静默变成红色。
//!
//! `private_payload` 只派生**分类**，真正的载荷判定仍留在 `command.rs`：它含
//! `message_type == "pink"` 这类按来源分流的谓词与 `TarotCommand::Draw { ai: true, .. }`
//! 这类结构解构，「模块名 => 载荷」推不出来。手写部分与本表的分类由测试对拍。
//!
//! # 新增一个玩法要改几处
//!
//! 1. 本文件的宏调用里加一段（**唯一**的事实）；
//! 2. `private_payload` / `requires_hall_sender` / `lock_key` 的 match 加一行
//!    （那里不写 `_ =>`，编译器会逼你加）；
//! 3. `AT_HANDLERS` 或 `SESSION_DISPATCH` 加一条 `handler(...)`（测试会逼你加）。
//!
//! 从「3 个文件 5 处手抄」降到「3 个文件 3 处，且后两处漏了会红」。

use serde::{Deserialize, Serialize};

use crate::features::command::CommandPrefix;

/// 一条命令下的可编辑回复集合。
///
/// 定义在这里而不是 `reply_template::ownership`，是因为它**是**登记表的一部分：
/// 谁拥有哪些 key 由 [`MODULE_SPECS`] 的 `replies:` 列决定。`ownership` 只做转发，
/// 这样 `reply_template` 知道「命令 → 回复」，`features::command` 不必反向依赖它。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandReplyGroup {
    /// 稳定命令 ID，与命令面板的 前缀+命令名 一致。
    pub(crate) command_id: &'static str,
    /// 面板显示名。
    pub(crate) name: &'static str,
    /// 面板分组，取 `CommandGroup::label()` 的取值。
    pub(crate) category: &'static str,
    /// 该命令拥有的模板 key，顺序即面板展示顺序。
    pub(crate) keys: &'static [&'static str],
}

/// 命令字面量表：`command_catalog` 里按「表」组织的同一批 `CommandSpec`。
///
/// 这些表**仍然手写**，本枚举只提供「按什么顺序拼接」这层事实，好让
/// `static_commands()` 不再手抄一遍 `.chain(...)`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CommandTable {
    Song,
    Playback,
    Hall,
    Administration,
    EntertainmentStart,
    Playlist,
    FriendMisc,
}

impl CommandTable {
    /// 全部命令表，顺序即 `static_commands()` 的拼接顺序。
    ///
    /// 与 HEAD 的 `.chain(...)` 顺序逐字一致（见本模块的 `registry_fixtures`）。
    pub(crate) const ALL: [Self; 7] = [
        Self::Song,
        Self::Playback,
        Self::Hall,
        Self::Administration,
        Self::EntertainmentStart,
        Self::Playlist,
        Self::FriendMisc,
    ];
}

/// 该模块命令的私密载荷分类。
///
/// 这是**事实**的一部分：审核只有私聊输入私密、塔罗只有「带 AI 的抽牌」私密、
/// AI 聊天恒私密、卧底是结构 + 来源混合。登记进表后，`private_payload` 的手写
/// match 才能被测试逐模块对拍——而不是只靠读代码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrivatePayloadClass {
    /// 任何命令都不含私密内容。
    Never,
    /// 恒为私密（AI 聊天的提问）。
    Always,
    /// 仅好友私聊来源的命令私密（审核/拉黑的私聊输入）。
    BySource,
    /// 由命令自身的结构决定（塔罗：`Draw { ai: true, .. }`）。
    ByShape,
    /// 结构与来源混合（谁是卧底：投票/弃权/揭示恒私密，其余公开）。
    Mixed,
}

/// 单个命令的登记项：名字、别名、前缀、字面量表。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandName {
    pub(crate) name: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) prefix: CommandPrefix,
    /// 该命令的字面量住在 `command_catalog` 的哪张表里。
    pub(crate) table: CommandTable,
}

impl CommandName {
    pub(crate) fn all_names(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }
}

/// 一个回复分组在登记表里的形态。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplyGroupSpec {
    /// 拥有该分组的模块。指向 [`MODULE_SPECS`]，是「分组归谁」的唯一事实。
    pub(crate) owner: ChatCommandModule,
    pub(crate) command_id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) category: &'static str,
    pub(crate) keys: &'static [&'static str],
}

/// 一个玩法模块的全部登记事实。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ModuleSpec {
    /// 枚举变体名。`ChatCommandModule::$module` 与 `ModuleCommand::$module`
    /// 由宏按此名生成，两枚举因此**必然**同名、同序、同长。
    pub(crate) module: ChatCommandModule,
    /// 载荷类型的路径文本（`stringify!` 的结果），供测试断言与排查。
    /// 真正的类型由宏写进 `ModuleCommand` 的变体，写错即编译失败。
    pub(crate) payload: &'static str,
    /// 目录名（`src/features/<module_path>/`）。
    pub(crate) module_path: &'static str,
    /// 中文名（帮助与面板文案）。**必须在这里**：与命令分组名不是一回事
    /// （模块 `card_games` 叫「卡牌」，命令分组却是「娱乐」）。
    pub(crate) label: &'static str,
    /// 该玩法在配置库里的段名，回答「这个模块的开关在面板哪一页」。
    pub(crate) config_section: &'static str,
    /// 私密载荷分类（见 [`PrivatePayloadClass`]）。
    pub(crate) private_payload: PrivatePayloadClass,
    /// 该模块认领的全部命令（主名 + 别名 + 前缀）。
    pub(crate) commands: &'static [CommandName],
    /// 该模块拥有的回复分组（顺序即面板展示顺序）。
    pub(crate) replies: &'static [ReplyGroupSpec],
}

impl ModuleSpec {
    /// 该模块的全部命令写法（主名在前，全部别名随后）。
    pub(crate) fn all_names(&self) -> impl Iterator<Item = &'static str> {
        self.commands.iter().flat_map(CommandName::all_names)
    }

    /// 该模块拥有的回复分组，**按面板展示顺序**。
    ///
    /// 反查 [`REPLY_GROUPS`] 的 `owner:` 列：分组归谁只有那一处事实，
    /// 而展示顺序由 `REPLY_GROUPS` 自己的顺序决定（两者顺序不同且都不可变）。
    pub(crate) fn replies(&self) -> impl Iterator<Item = &'static ReplyGroupSpec> {
        REPLY_GROUPS
            .iter()
            .filter(|group| group.owner == self.module)
    }
}

/// 从单一登记表派生两个枚举与两张纯表。
///
/// 由它生成：`ChatCommandModule`（模块变体）、`ModuleCommand`（命令变体，载荷来自
/// `payload:` 列）、`MODULE_SPECS`（表本身）、`REPLY_GROUPS`（回复归属）。
///
/// **穷尽性**：`ModuleCommand::module/private_payload/lock_key/requires_hall_sender`
/// 都是对 `Self` 的穷尽 `match`（**无** `_ =>`），变体由本宏生成而分支在
/// `command.rs` 手写。于是「新增模块却漏写哪条分支」是**编译错误**，而不是静默漏判。
macro_rules! define_module_registry {
    ($(
        {
            module: $module:ident,
            payload: $payload:path,
            module_path: $module_path:literal,
            label: $label:literal,
            config_section: $config_section:literal,
            private_payload: $private_payload:ident,
            commands: [ $( ($name:literal, $aliases:expr, $prefix:ident, $table:ident) ),* $(,)? ],
        },
    )*) => {
        /// 认领该命令的业务模块。
        ///
        /// 由 [`define_module_registry!`] 生成，因此与 `ModuleCommand` **必然**
        /// 同名、同序、同长（两枚举都从同一次宏调用展开）。
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub(crate) enum ChatCommandModule {
            $( $module, )*
        }

        impl ChatCommandModule {
            /// 全部模块，顺序即登记顺序。
            pub(crate) const ALL: &'static [Self] = &[ $( Self::$module, )* ];
        }

        /// ADR 0059 定义的顶层小型路由枚举。
        ///
        /// 每种载荷类型由对应业务模块拥有；载荷由登记表的 `payload:` 列决定。
        #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
        pub(crate) enum ModuleCommand {
            $( $module($payload), )*
        }

        /// 全部玩法模块的登记表——新增玩法的**唯一**登记点。
        pub(crate) const MODULE_SPECS: &'static [ModuleSpec] = &[
            $(
                ModuleSpec {
                    module: ChatCommandModule::$module,
                    payload: stringify!($payload),
                    module_path: $module_path,
                    label: $label,
                    config_section: $config_section,
                    private_payload: PrivatePayloadClass::$private_payload,
                    commands: &[
                        $(
                            CommandName {
                                name: $name,
                                aliases: $aliases,
                                prefix: CommandPrefix::$prefix,
                                table: CommandTable::$table,
                            },
                        )*
                    ],
                    // 由 `replies_for` 在运行时按 owner 反查，见下方 impl。
                    replies: &[],
                },
            )*
        ];
    };
}

// ---------------------------------------------------------------------------
// 单一登记点：新增玩法只改这里
// ---------------------------------------------------------------------------

define_module_registry![
    {
        module: SongRequest,
        payload: crate::features::song_request::SongCommand,
        module_path: "song_request",
        label: "点歌",
        config_section: "command_access",
        private_payload: Never,
        commands: [
            ("QQ点歌", &["点歌"], At, Song),
            ("网易点歌", &["网易云点歌"], At, Song),
            ("酷狗点歌", &[], At, Song),
            ("B站点歌", &[], At, Song),
            ("AI点歌", &[], At, Song),
        ],
    },
    {
        module: Playback,
        payload: crate::features::playback::PlaybackCommand,
        module_path: "playback",
        label: "播放",
        config_section: "playback",
        private_payload: Never,
        commands: [
            ("暂停", &[], At, Playback),
            ("继续", &["恢复"], At, Playback),
            ("播放", &[], At, Playback),
            ("下一首", &["下一曲"], At, Playback),
            ("上一首", &["上一曲"], At, Playback),
            ("删除", &[], At, Playback),
            ("音量", &[], At, Playback),
            ("状态", &[], At, Playback),
            ("队列", &["列表"], At, Playback),
            ("队列删除", &[], At, Playback),
            ("队列清空", &[], At, Playback),
            ("歌词", &[], At, Playback),
            ("切换", &[], At, Playback),
            ("停止歌词", &[], At, Playback),
            ("后台歌词", &[], At, Playback),
            ("单曲歌词", &[], At, Playback),
            ("持续歌词", &[], At, Playback),
        ],
    },
    {
        module: Hall,
        payload: crate::features::hall::HallCommand,
        module_path: "hall",
        label: "大厅",
        config_section: "chat_source",
        private_payload: Never,
        commands: [
            ("大厅检测", &[], At, Hall),
            ("大厅时间", &[], At, Hall),
            ("麦克风", &[], At, Hall),
            ("忘记我", &[], At, Hall),
        ],
    },
    {
        module: Administration,
        payload: crate::features::administration::AdministrationCommand,
        module_path: "administration",
        label: "管理",
        config_section: "stability",
        private_payload: Never,
        commands: [
            ("帮助", &[], At, Administration),
            ("娱乐帮助", &[], At, Administration),
            ("娱乐帮助", &["娱乐", "帮助"], Hash, Administration),
            ("值守模式", &[], At, Administration),
            ("启用", &[], At, Administration),
            ("禁用", &[], At, Administration),
            ("闲置退出", &[], At, Administration),
        ],
    },
    {
        module: IdiomChain,
        payload: crate::features::idiom_chain::IdiomChainCommand,
        module_path: "idiom_chain",
        label: "成语接龙",
        config_section: "idiom_chain",
        private_payload: Never,
        commands: [
            ("接龙", &[], Hash, EntertainmentStart),
            ("同音接龙", &[], Hash, EntertainmentStart),
        ],
    },
    {
        module: CardGame,
        payload: crate::features::card_games::LandlordCommand,
        module_path: "card_games",
        label: "卡牌",
        config_section: "landlord",
        private_payload: Never,
        commands: [
            ("斗地主", &[], Hash, EntertainmentStart),
            ("跑得快", &[], Hash, EntertainmentStart),
        ],
    },
    {
        module: TurtleSoup,
        payload: crate::features::turtle_soup::TurtleSoupCommand,
        module_path: "turtle_soup",
        label: "海龟汤",
        config_section: "turtle_soup",
        private_payload: Never,
        commands: [
            ("海龟汤", &[], Hash, EntertainmentStart),
        ],
    },
    {
        module: Tarot,
        payload: crate::features::tarot::TarotCommand,
        module_path: "tarot",
        label: "塔罗牌",
        config_section: "ai",
        private_payload: ByShape,
        commands: [
            ("塔罗牌", &["塔罗牌AI", "塔罗牌ai"], Hash, EntertainmentStart),
            ("塔罗牌重抽", &[], Hash, EntertainmentStart),
            ("塔罗牌帮助", &[], Hash, EntertainmentStart),
        ],
    },
    {
        module: Playlist,
        payload: crate::features::playlist::PlaylistCommand,
        module_path: "playlist",
        label: "歌单",
        config_section: "queue",
        private_payload: Never,
        commands: [
            ("歌单", &[], At, Playlist),
            ("歌单帮助", &[], At, Playlist),
            ("换一批", &[], At, Playlist),
            ("收藏", &[], At, Playlist),
            ("取消收藏", &["歌单删除"], At, Playlist),
            ("歌单随机", &["随机歌单"], At, Playlist),
            ("播放歌单", &["歌单模式"], At, Playlist),
            ("大厅模式", &["退出歌单模式"], At, Playlist),
            ("关闭", &[], At, Playlist),
            ("停止播放歌单", &[], At, Playlist),
            ("歌单清空", &[], At, Playlist),
            ("确认清空", &[], At, Playlist),
            ("放弃清空", &[], At, Playlist),
            ("确认导入", &[], At, Playlist),
            ("取消导入", &[], At, Playlist),
            ("导入QQ歌单", &["导入网易歌单", "导入网易云歌单"], At, Playlist),
        ],
    },
    {
        module: Undercover,
        payload: crate::features::undercover::UndercoverCommand,
        module_path: "undercover",
        label: "谁是卧底",
        config_section: "undercover",
        private_payload: Mixed,
        commands: [
            ("卧底", &["卧底双"], Hash, EntertainmentStart),
        ],
    },
    {
        module: NumberBomb,
        payload: crate::features::number_bomb::NumberBombCommand,
        module_path: "number_bomb",
        label: "数字炸弹",
        config_section: "number_bomb",
        private_payload: Never,
        commands: [
            ("数字炸弹", &[], Hash, EntertainmentStart),
        ],
    },
    {
        module: Invite,
        payload: crate::features::invite::InviteCommand,
        module_path: "invite",
        label: "邀请",
        config_section: "invite",
        private_payload: Never,
        commands: [
            ("邀请", &[], At, FriendMisc),
        ],
    },
    {
        module: Moderation,
        payload: crate::features::moderation::ModerationCommand,
        module_path: "moderation",
        label: "审核",
        config_section: "moderation",
        private_payload: BySource,
        commands: [
            ("拉黑", &["拉黑UID"], At, FriendMisc),
            ("屏蔽", &["屏蔽UID"], At, FriendMisc),
        ],
    },
    {
        module: CustomWorkflow,
        payload: crate::features::custom_workflow::CustomWorkflowCommand,
        module_path: "custom_workflow",
        label: "自定义流程",
        config_section: "custom_workflows",
        private_payload: Never,
        commands: [],
    },
    {
        module: AiChat,
        payload: crate::features::ai_chat::AiChatCommand,
        module_path: "ai_chat",
        label: "AI 聊天",
        config_section: "ai_chat",
        private_payload: Always,
        commands: [],
    },
];

#[cfg(test)]
#[path = "registry_fixtures.rs"]
mod registry_fixtures;

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;

// ---------------------------------------------------------------------------
// 回复归属分组（面板契约：数组顺序 = 展示顺序）
// ---------------------------------------------------------------------------

/// 全部回复归属分组。**顺序即面板展示顺序**，与 HEAD 逐字一致。
///
/// 与 [`MODULE_SPECS`] 分开登记，是因为两者顺序**不同且都不可变**：
/// - `MODULE_SPECS` / 两个枚举的顺序是 HEAD 的**声明序**（`ModuleCommand` 有
///   serde，声明序就是外部可见的序列化序）；
/// - 本表顺序是 HEAD `REPLY_GROUPS` 的**面板展示序**（`/commands` 的数组序）。
///
/// 谁拥有哪些 key 仍然由 `owner:` 列指向 [`MODULE_SPECS`] 里的模块，
/// 因此「分组归哪个模块」只有一处事实，两份顺序各自可对拍。
pub(crate) const REPLY_GROUPS: &[ReplyGroupSpec] = &[
    ReplyGroupSpec {
        owner: ChatCommandModule::SongRequest,
        command_id: "@QQ点歌",
        name: "点歌",
        category: "点歌",
        keys: &[
            "playback.dedup.recently_played",
            "playback.dedup.skipped",
            "playback.no_source",
            "playback.search.failed",
            "playback.item_failure",
            "playback.failure.reason",
            "playback.source_switched",
            "playback.queue.skipped",
            "song.platform.denied",
            "song.queue.duplicate",
            "song.playing.already",
            "song.ai.disabled",
            "song.ai.disabled_with_hint",
            "song.ai.matching",
            "song.ai.rewritten",
            "song.ai.second_search_failed",
            "song.no_source",
            "song.ai.recognize_failed",
            "song.search_failed",
            "song.queue.added",
            "song.queue.full",
            "song.queue.full_unknown_state",
            "song.dedup.recently_played",
            "song.review.rejected",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playlist,
        command_id: "@歌单",
        name: "歌单",
        category: "点歌",
        keys: &[
            "song.playlist.friend_only",
            "playlist.window.header",
            "playlist.window.item",
            "playlist.window.footer",
            "playlist.select.queued",
            "playlist.select.queue_full",
            "playlist.select.out_of_range",
            "playlist.reroll.exhausted",
            "playlist.window.closed",
            "playlist.favorite.no_track",
            "playlist.favorite.added",
            "playlist.favorite.exists",
            "playlist.unfavorite.no_track",
            "playlist.unfavorite.removed",
            "playlist.unfavorite.missing",
            "playlist.random.queued",
            "playlist.empty.hint",
            "playlist.empty.plain",
            "playlist.play.empty",
            "playlist.play.started",
            "playlist.play.occupied",
            "playlist.mode.exited",
            "playlist.mode.already_hall",
            "playlist.import.pending",
            "playlist.import.busy",
            "playlist.import.loading",
            "playlist.import.still_loading",
            "playlist.import.done",
            "playlist.import.timeout",
            "playlist.import.failed",
            "playlist.import.too_large",
            "playlist.import.cancelled",
            "playlist.clear.already_empty",
            "playlist.clear.confirm",
            "playlist.clear.done",
            "playlist.clear.done_with_exit",
            "playlist.clear.aborted",
            "playlist.actor.missing",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playlist,
        command_id: "@歌单帮助",
        name: "歌单帮助",
        category: "点歌",
        keys: &["help.playlist"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@暂停",
        name: "暂停",
        category: "播放",
        keys: &["playback.pause.ok"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@继续",
        name: "继续 / 播放",
        category: "播放",
        keys: &["playback.resume.ok"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@下一首",
        name: "下一首",
        category: "播放",
        keys: &["playback.next.pool_fallback", "playback.next.empty"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@上一首",
        name: "上一首",
        category: "播放",
        keys: &["playback.previous.missing"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@删除",
        name: "删除当前曲目",
        category: "播放",
        keys: &[
            "playback.delete.track_unavailable",
            "playback.delete.removed.pool_fallback",
            "playback.delete.not_in_pool.pool_fallback",
            "playback.delete.removed.queue_empty",
            "playback.delete.not_in_pool.queue_empty",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@音量",
        name: "音量",
        category: "播放",
        keys: &["playback.volume.set"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@队列",
        name: "队列",
        category: "播放",
        keys: &[
            "playback.queue.empty",
            "playback.queue.header",
            "playback.queue.entry",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@队列删除",
        name: "队列删除",
        category: "播放",
        keys: &[
            "playback.queue.delete.invalid",
            "playback.queue.delete.missing",
            "playback.queue.delete.done",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@队列清空",
        name: "队列清空",
        category: "播放",
        keys: &["playback.queue.cleared"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@歌词",
        name: "歌词",
        category: "播放",
        keys: &[
            "playback.lyrics.toggle.translation",
            "playback.lyrics.toggle.original",
            "playback.lyrics.timed_started",
            "playback.lyrics.already_running",
            "playback.lyrics.line",
            "playback.lyrics.empty",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@状态",
        name: "播放状态",
        category: "播放",
        keys: &[
            "playback.status.with_song",
            "playback.status.without_song",
            "playback.status.requester",
            "playback.status.playlist_prefix",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@后台歌词",
        name: "后台歌词",
        category: "播放",
        keys: &["playback.lyrics.background.started"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@单曲歌词",
        name: "单曲歌词",
        category: "播放",
        keys: &["playback.lyrics.single_song.started"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Playback,
        command_id: "@停止歌词",
        name: "停止歌词",
        category: "播放",
        keys: &[
            "playback.lyrics.background.stopped",
            "playback.lyrics.background.not_running",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Hall,
        command_id: "@大厅检测",
        name: "大厅检测",
        category: "大厅",
        keys: &["hall.detect.failed", "hall.public.current", "hall.current"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Hall,
        command_id: "@大厅时间",
        name: "大厅时间",
        category: "大厅",
        keys: &[
            "hall.time.remaining",
            "hall.time.unknown",
            "hall.public.no_limit",
            "hall.expiring.warning",
            "hall.expiring.warning_minutes",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Hall,
        command_id: "@麦克风",
        name: "麦克风",
        category: "大厅",
        keys: &["hall.microphone.toggled"],
    },
    // 程序自动播报（没有对应的玩家命令）：分组是为了让面板能改这些文案。
    // 这些文案由运行时事件触发（例如自动重启游戏前后），玩家不会主动调用，
    // 因此 command_id 用描述性标识而不是 @/# 命令。
    ReplyGroupSpec {
        owner: ChatCommandModule::Hall,
        command_id: "自动重启游戏",
        name: "自动重启游戏",
        category: "大厅",
        keys: &["hall.game_restart.notice", "hall.game_restart.restored"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Tarot,
        command_id: "#塔罗牌",
        name: "塔罗牌",
        category: "娱乐",
        keys: &[
            "tarot.question.too_long",
            "tarot.actor.missing",
            "tarot.cooldown",
            "tarot.busy",
            "tarot.spread.title",
            "tarot.ai.disabled",
            "tarot.ai.unavailable",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Tarot,
        command_id: "#塔罗牌帮助",
        name: "塔罗牌帮助",
        category: "娱乐",
        keys: &["tarot.help"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::IdiomChain,
        command_id: "#接龙",
        name: "成语接龙",
        category: "娱乐",
        keys: &[
            "idiom_chain.occupied.turtle_soup",
            "idiom_chain.occupied.other",
            "idiom_chain.disabled",
            "idiom_chain.expired",
            "idiom_chain.already_active",
            "idiom_chain.no_start_candidate",
            "idiom_chain.dead_end_start",
            "idiom_chain.expected.homophone",
            "idiom_chain.expected.exact",
            "idiom_chain.started",
            "idiom_chain.no_session.start",
            "idiom_chain.wrong_first_char",
            "idiom_chain.duplicate",
            "idiom_chain.completed",
            "idiom_chain.accepted",
            "idiom_chain.no_session",
            "idiom_chain.status",
            "idiom_chain.no_explanation_target",
            "idiom_chain.missing_explanation",
            "idiom_chain.explanation.title",
            "idiom_chain.no_hint",
            "idiom_chain.hint",
            "idiom_chain.stop_denied",
            "idiom_chain.stopped",
            "idiom_chain.invalid_idiom",
            "idiom_chain.not_in_lexicon",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::CardGame,
        command_id: "#斗地主",
        name: "斗地主",
        category: "娱乐",
        keys: &[
            "card_games.disabled",
            "card_games.already_active",
            "card_games.lobby.created",
            "card_games.lobby.none",
            "card_games.lobby.already_joined",
            "card_games.lobby.joined",
            "card_games.bidding.started",
            "card_games.retry.none",
            "card_games.bidding.not_bidding",
            "card_games.game.not_in_landlord",
            "card_games.bidding.turn",
            "card_games.bidding.decided",
            "card_games.bidding.called",
            "card_games.bidding.call_declined",
            "card_games.bidding.robbed",
            "card_games.bidding.rob_declined",
            "card_games.bidding.counter_prompt",
            "card_games.bidding.caller_landlord",
            "card_games.bidding.counter_robbed",
            "card_games.bidding.wrong_phase",
            "card_games.bidding.redealt",
            "card_games.hand.initial",
            "card_games.status.player_count",
            "card_games.hand.landlord_initial",
            "card_games.landlord.selected",
            "card_games.lobby.timeout",
            "card_games.bidding.warning",
            "card_games.bidding.timeout",
            "card_games.turn.warning",
            "card_games.pass.timeout_no_beating",
            "card_games.pass.timeout",
            "card_games.pass.timeout_with_suffix",
            "card_games.game.no_active",
            "card_games.game.not_in",
            "card_games.play.wrong_turn",
            "card_games.play.empty",
            "card_games.play.invalid_pattern",
            "card_games.play.cannot_beat",
            "card_games.pass.leader_cannot_pass",
            "card_games.hand.no_dealt_game",
            "card_games.hand.current",
            "card_games.status.idle",
            "card_games.status.lobby",
            "card_games.status.bidding",
            "card_games.status.playing_landlord",
            "card_games.exit.not_in_lobby",
            "card_games.lobby.canceled",
            "card_games.lobby.left",
            "card_games.exit.landlord_ended",
            "card_games.exit.game_ended",
            "card_games.won.landlord",
            "card_games.action.play",
            "card_games.action.timeout",
            "card_games.play.landlord",
            "card_games.pass.manual_next",
            "card_games.pass.manual_lead",
            "card_games.pass.timeout_next",
            "card_games.pass.timeout_lead",
            "card_games.pass.forced_next",
            "card_games.pass.forced_lead",
            "card_games.pass.forced_merged_next",
            "card_games.pass.forced_merged_lead",
            "card_games.pass.new_trick",
            "card_games.pass.next",
            "card_games.parse.unknown_card",
            "card_games.parse.missing_card",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::CardGame,
        command_id: "#跑得快",
        name: "跑得快",
        category: "娱乐",
        keys: &[
            "card_games.run_fast.started",
            "card_games.play.spade_three_required",
            "card_games.play.must_play_largest_single",
            "card_games.pass.must_play",
            "card_games.status.playing_run_fast",
            "card_games.won.run_fast",
            "card_games.play.run_fast",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::TurtleSoup,
        command_id: "#海龟汤",
        name: "海龟汤",
        category: "娱乐",
        keys: &[
            "turtle_soup.judgment.suffix",
            "turtle_soup.judgment.reply",
            "turtle_soup.result.yes",
            "turtle_soup.result.no",
            "turtle_soup.result.irrelevant",
            "turtle_soup.result.partial",
            "turtle_soup.result.complete",
            "turtle_soup.result.refused",
            "turtle_soup.result.review_failed",
            "turtle_soup.batch.notice",
            "turtle_soup.batch.saved",
            "turtle_soup.batch.missing",
            "turtle_soup.batch.limit",
            "turtle_soup.batch.empty_part",
            "turtle_soup.batch.no_draft",
            "turtle_soup.batch.format",
            "turtle_soup.batch.cleared",
            "turtle_soup.batch.done",
            "turtle_soup.batch.submit",
            "turtle_soup.batch.format_hint",
            "turtle_soup.question.pending",
            "turtle_soup.question.too_many",
            "turtle_soup.question.worker_unavailable",
            "turtle_soup.disabled",
            "turtle_soup.no_session",
            "turtle_soup.status.no_session",
            "turtle_soup.status",
            "turtle_soup.end.hall_denied",
            "turtle_soup.end.no_session",
            "turtle_soup.end.failed",
            "turtle_soup.start.failed",
            "turtle_soup.announcing",
            "turtle_soup.settling",
            "turtle_soup.surface.repeat_failed",
            "turtle_soup.label.surface",
            "turtle_soup.label.settlement",
            "turtle_soup.label.bottom",
            "turtle_soup.reason.winner",
            "turtle_soup.reason.friend",
            "turtle_soup.reason.web",
            "turtle_soup.reason.idle_timeout",
            "turtle_soup.reason.max_duration",
            "turtle_soup.settlement.summary",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Undercover,
        command_id: "#卧底",
        name: "谁是卧底",
        category: "娱乐",
        keys: &[
            "undercover.vote.instruction",
            "undercover.start.secret",
            "undercover.start.header",
            "undercover.start.prompt",
            "undercover.delivery.canceled",
            "undercover.runoff.no_voters",
            "undercover.runoff.described",
            "undercover.describe.completed",
            "undercover.status.idle",
            "undercover.status.lobby",
            "undercover.status.playing",
            "undercover.phase.describing",
            "undercover.phase.voting",
            "undercover.reveal.answer",
            "undercover.exit.creator",
            "undercover.exit.lobby",
            "undercover.exit.playing",
            "undercover.round.restart",
            "undercover.lobby.canceled",
            "undercover.lobby.timeout",
            "undercover.delivery.timeout",
            "undercover.vote.missing",
            "undercover.describe.timeout",
            "undercover.runoff.describe_timeout",
            "undercover.runoff.describe_cutoff",
            "undercover.describe.cutoff",
            "undercover.runoff.tied",
            "undercover.runoff.enter",
            "undercover.vote.none",
            "undercover.vote.eliminated",
            "undercover.round.start",
            "undercover.settlement.header",
            "undercover.settlement.player",
            "undercover.settlement.rounds",
            "undercover.start.position_mapping",
            "undercover.winner.civilian",
            "undercover.winner.undercover",
            "undercover.winner.none",
            "undercover.role.civilian",
            "undercover.role.undercover",
            "undercover.delivery.waiting",
            "undercover.verify.create_success",
            "undercover.verify.join_success",
            "undercover.verify.failed",
            "undercover.error.no_lobby",
            "undercover.error.already_started",
            "undercover.error.already_joined",
            "undercover.error.disabled",
            "undercover.error.already_active",
            "undercover.error.occupied",
            "undercover.reveal.friend_only",
            "undercover.retry.none",
            "undercover.retry.expired",
            "undercover.retry.unsafe",
            "undercover.delivery.result_unknown",
            "undercover.delivery.no_safe_remainder",
            "undercover.delivery.incomplete",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::NumberBomb,
        command_id: "#数字炸弹",
        name: "数字炸弹",
        category: "娱乐",
        keys: &[
            "number_bomb.disabled",
            "number_bomb.occupied",
            "number_bomb.already_active",
            "number_bomb.no_session",
            "number_bomb.only_creator",
            "number_bomb.lobby.created",
            "number_bomb.lobby.already_joined",
            "number_bomb.lobby.joined",
            "number_bomb.lobby.full",
            "number_bomb.lobby.already_started",
            "number_bomb.start.too_few",
            "number_bomb.start.too_many",
            "number_bomb.start.already_playing",
            "number_bomb.start.started",
            "number_bomb.guess.not_playing",
            "number_bomb.guess.not_in_game",
            "number_bomb.guess.not_your_turn",
            "number_bomb.guess.out_of_range",
            "number_bomb.guess.duplicate",
            "number_bomb.guess.accepted",
            "number_bomb.guess.exploded",
            "number_bomb.timeout.auto",
            "number_bomb.status.no_session",
            "number_bomb.status.lobby",
            "number_bomb.status.playing",
            "number_bomb.status.settled",
            "number_bomb.end.no_session",
            "number_bomb.end.done",
            "number_bomb.guess.not_number",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Administration,
        command_id: "@帮助",
        name: "帮助",
        category: "管理",
        keys: &["help.song_request"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Administration,
        command_id: "@娱乐帮助",
        name: "娱乐帮助",
        category: "管理",
        keys: &["help.entertainment"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Administration,
        command_id: "@启用",
        name: "启用命令识别",
        category: "管理",
        keys: &["admin.commands.enabled"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Administration,
        command_id: "@禁用",
        name: "禁用命令识别",
        category: "管理",
        keys: &["admin.commands.disabled"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::AiChat,
        command_id: "@忘记我",
        name: "AI 聊天",
        category: "管理",
        keys: &[
            "ai_chat.forget.done",
            "ai_chat.question.too_long",
            "ai_chat.unavailable",
        ],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Moderation,
        command_id: "@拉黑",
        name: "拉黑 / 屏蔽",
        category: "管理",
        keys: &["moderation.request.pending", "moderation.vote.opened"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::Invite,
        command_id: "@邀请",
        name: "邀请",
        category: "管理",
        keys: &["invite.accepted", "invite.sent"],
    },
    ReplyGroupSpec {
        owner: ChatCommandModule::CustomWorkflow,
        command_id: "custom_workflow",
        name: "自定义流程",
        category: "管理",
        keys: &["workflow.confirm.timeout"],
    },
];
