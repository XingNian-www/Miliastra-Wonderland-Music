//! Shared song-entry registry for parsing, command enumeration and platform policy.
use super::SongSource;
use crate::features::chat_text::parse_prefixed_command;
use crate::features::command::CommandAuthority::{self, Friend, HallMember};
use crate::features::command::CommandPrefix;
use crate::features::command_catalog::{CommandGroup, CommandMatch, CommandSpec};

struct SongEntry {
    spec: CommandSpec,
    policy_id: &'static str,
    source: SongSource,
    ai_assisted: bool,
}

const BOTH: &[CommandAuthority] = &[HallMember, Friend];
const ENTRIES: &[SongEntry] = &[
    SongEntry {
        spec: CommandSpec {
            name: "QQ点歌",
            aliases: &["点歌"],
            prefix: CommandPrefix::At,
            match_rule: CommandMatch::Prefix {
                allows_argument: true,
            },
            group: CommandGroup::Song,
            authorities: BOTH,
            summary: "QQ 音乐点歌；@点歌使用命令面板设置的默认平台，好友 BV 号自动使用 B站",
            usage: &["@QQ点歌晴天", "@点歌晴天"],
            hidden: false,
        },
        policy_id: "@QQ点歌",
        source: SongSource::QqMusic,
        ai_assisted: false,
    },
    SongEntry {
        spec: CommandSpec {
            name: "网易点歌",
            aliases: &["网易云点歌"],
            prefix: CommandPrefix::At,
            match_rule: CommandMatch::Prefix {
                allows_argument: true,
            },
            group: CommandGroup::Song,
            authorities: BOTH,
            summary: "网易云音乐点歌",
            usage: &["@网易点歌晴天"],
            hidden: false,
        },
        policy_id: "@网易点歌",
        source: SongSource::Netease,
        ai_assisted: false,
    },
    SongEntry {
        spec: CommandSpec {
            name: "酷狗点歌",
            aliases: &[],
            prefix: CommandPrefix::At,
            match_rule: CommandMatch::Prefix {
                allows_argument: true,
            },
            group: CommandGroup::Song,
            authorities: BOTH,
            summary: "酷狗音乐点歌",
            usage: &["@酷狗点歌晴天"],
            hidden: false,
        },
        policy_id: "@酷狗点歌",
        source: SongSource::Kugou,
        ai_assisted: false,
    },
    SongEntry {
        spec: CommandSpec {
            name: "B站点歌",
            aliases: &[],
            prefix: CommandPrefix::At,
            match_rule: CommandMatch::Prefix {
                allows_argument: true,
            },
            group: CommandGroup::Song,
            // 渠道只决定「在哪些界面能被识别」，不再代表「仅好友可用」。
            // 原来只声明 Friend，结果大厅里连命令都匹配不上——管理员也触发不了。
            // 真正的门是权限档位（>= minimum_role），由 command_access 统一判定。
            authorities: BOTH,
            summary: "B站音源点歌（默认好友及以上）",
            usage: &["@B站点歌BV1xx411c7mD"],
            hidden: false,
        },
        policy_id: "@B站点歌",
        source: SongSource::Bilibili,
        ai_assisted: false,
    },
    SongEntry {
        spec: CommandSpec {
            name: "AI点歌",
            aliases: &[],
            prefix: CommandPrefix::At,
            match_rule: CommandMatch::Prefix {
                allows_argument: true,
            },
            group: CommandGroup::Song,
            authorities: BOTH,
            summary: "AI 辅助点歌；大厅使用 QQ，好友可选全部音源",
            usage: &["@AI点歌晴天周杰伦"],
            hidden: false,
        },
        policy_id: "@AI点歌",
        source: SongSource::QqMusic,
        ai_assisted: true,
    },
];

pub(crate) fn permission_id_for_command_name(name: &str) -> Option<&'static str> {
    ENTRIES
        .iter()
        .find(|entry| entry.spec.name == name)
        .map(|entry| entry.policy_id)
}

pub(super) fn permission_id(source: SongSource) -> Option<&'static str> {
    ENTRIES
        .iter()
        .find(|entry| !entry.ai_assisted && entry.source == source)
        .map(|entry| entry.policy_id)
}

pub(super) fn ai_permission_id() -> &'static str {
    ENTRIES
        .iter()
        .find(|entry| entry.ai_assisted)
        .expect("AI song entry is registered")
        .policy_id
}

pub(crate) fn platform_sources() -> impl Iterator<Item = (SongSource, &'static str)> {
    ENTRIES
        .iter()
        .filter(|entry| !entry.ai_assisted)
        .map(|entry| (entry.source, entry.spec.name))
}

pub(crate) fn command_specs() -> &'static [CommandSpec] {
    static SPECS: std::sync::LazyLock<Vec<CommandSpec>> =
        std::sync::LazyLock::new(|| ENTRIES.iter().map(|entry| entry.spec).collect());
    &SPECS
}

pub(super) struct SongEntryMatch<'a> {
    pub(super) matched: &'static str,
    pub(super) argument: &'a str,
    pub(super) source: SongSource,
    pub(super) ai_assisted: bool,
}

pub(super) fn match_entry(
    command: &str,
    authority: CommandAuthority,
) -> Option<SongEntryMatch<'_>> {
    for entry in ENTRIES {
        if !entry.spec.authorities.contains(&authority) {
            continue;
        }
        for name in entry.spec.all_names() {
            if let Some(argument) = parse_prefixed_command(command, name, true) {
                return Some(SongEntryMatch {
                    matched: name,
                    argument,
                    source: if entry.ai_assisted && authority == Friend {
                        SongSource::All
                    } else {
                        entry.source
                    },
                    ai_assisted: entry.ai_assisted,
                });
            }
        }
    }
    None
}
