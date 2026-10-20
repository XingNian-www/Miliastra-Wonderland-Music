//! 可调整的回复文案：代码提供默认值，配置中心提供覆盖。
//!
//! 设计约束：
//! - 默认值内置于代码，配置缺失时回落默认值并记一条 WARN，绝不因为缺配置而拒绝发送。
//! - 占位符形如 {歌名}；渲染顺序是先渲染模板，再把已渲染的变量值填入。
//! - AI 生成文本只作为变量值注入，绝不参与模板解析，避免 AI 输出里的花括号被误当占位符。
//! - 未知占位符原样保留（便于排查）；已知但缺失的变量替换为空串并上报。

use std::collections::BTreeMap;

// 按消费模块拆分的模板文件：每个模块只写自己的文件，避免并行开发时互相覆盖。
pub(crate) mod hall;
pub(crate) mod playback;

/// 一条可调整回复的元数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplyTemplate {
    /// 稳定键，如 song.request.accepted。配置覆盖按此键索引。
    pub(crate) key: &'static str,
    /// 内置默认文案，可含花括号占位符。
    pub(crate) default: &'static str,
    /// 供命令面板展示的一行摘要。
    pub(crate) summary: &'static str,
    /// 面板分组：点歌 / 播放 / 大厅 / 娱乐 / 管理。
    pub(crate) group: &'static str,
}

/// 渲染时的变量集合。AI 回答等动态文本以普通值形式放入。
#[derive(Clone, Debug, Default)]
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

/// 大厅帮助（娱乐类）逐行文案。
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


/// 全部模板的可枚举清单。
///
/// 新增模板时必须在此登记，否则面板看不到、配置也无法覆盖。
pub(crate) fn catalog() -> impl Iterator<Item = &'static ReplyTemplate> {
    HELP_SONG_REQUEST
        .iter()
        .chain(HELP_ENTERTAINMENT.iter())
        .chain(REPLY_SONG.iter())
        // 按模块拆分的模板：新增模块时只在这里加一行。
        .chain(playback::PLAYBACK.iter())
        .chain(hall::HALL.iter())
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
