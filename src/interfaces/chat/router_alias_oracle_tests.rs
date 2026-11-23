//! 别名认领的 name-driven oracle（W4c）。
//!
//! 取代 `features::command_catalog` 里原先**手抄一份 `claims_chat` 并或起来**的守卫：
//! 那种写法让守卫自己成了漂移源 —— 新增一个模块时，既要登记命令、又要记得回来
//! 往那串 `||` 里补一行，漏了不会有任何提示。
//!
//! 这里直接驱动**真实路由表**（`AT_HANDLERS` / `HASH_START_HANDLERS`），断言：
//! 1. 每条已登记命令都**至少有一种**真实可发的写法能被路由表认领并解析成功；
//! 2. 认领它的恰好是**一个** handler（0 个=面板有但游戏不认；≥2 个=优先级歧义）；
//! 3. 解析出的模块与该 handler 登记的模块一致（认领用 A、解析产出 B 会在这里红）；
//! 4. 反向：路由表里的每个模块都能被某条已登记命令触发（否则命令能触发但面板看不到）。

use super::*;
use crate::features::command::{ChatCommandModule, CommandAuthority, CommandPrefix};
use crate::features::command_catalog::static_commands;

fn envelope_for(raw: &str, prefix: CommandPrefix, authority: CommandAuthority) -> CommandEnvelope {
    let glyph = match prefix {
        CommandPrefix::At => "@",
        CommandPrefix::Hash => "#",
        CommandPrefix::Amp => "&",
    };
    let message_type = match authority {
        CommandAuthority::HallMember => "blue",
        CommandAuthority::Friend => "pink",
    };
    CommandEnvelope::new(
        format!("{glyph}{raw}"),
        "测试用户",
        message_type,
        format!("{glyph}{raw}"),
        crate::features::command::CommandObservation::default(),
    )
    .expect("构造信封")
}

/// 当前前缀下全部静态 handler（@ 是 AT，\# 是开局表，& 是测试专用前缀、永无 handler）。
fn handlers_for(prefix: CommandPrefix) -> &'static [ChatHandler] {
    match prefix {
        CommandPrefix::At => AT_HANDLERS,
        CommandPrefix::Hash => HASH_START_HANDLERS,
        CommandPrefix::Amp => &[],
    }
}

/// 一条 `CommandSpec` 所有**理论上可发**的文本形态。
///
/// 既包括主名与别名（`@点歌` 这种裸命令），也包括 `usage` 里的示例
/// （`@邀请123456` 这类必须带参数的命令，裸命令名触发不了）。
/// 判定"能不能发"一律交给真实 handler，不在这里猜。
fn candidate_texts(spec: &crate::features::command_catalog::CommandSpec) -> Vec<&'static str> {
    let mut texts: Vec<&'static str> = spec.all_names().collect();
    for usage in spec.usage {
        let trimmed = usage.trim_start_matches(['@', '#']);
        if !trimmed.is_empty() {
            texts.push(trimmed);
        }
    }
    texts
}

#[test]
fn every_registered_command_routes_to_exactly_one_module() {
    let mut problems: Vec<String> = Vec::new();
    for spec in static_commands() {
        // 只要**有一种**可发写法能正确路由，这条登记就算成立：
        // 裸命令名对 `@拉黑<UID>` 这类必须带参数的命令本来就触发不了，
        // 那不是漂移，而是 usage 里给了示例。
        let mut resolved = false;
        let mut rejected: Vec<String> = Vec::new();
        for text in candidate_texts(spec) {
            for authority in spec.authorities {
                let envelope = envelope_for(text, spec.prefix, *authority);
                let claiming: Vec<&ChatHandler> = handlers_for(spec.prefix)
                    .iter()
                    .filter(|entry| (entry.claims)(&envelope))
                    .collect();
                match claiming.len() {
                    0 => {}
                    1 => {
                        let entry = claiming[0];
                        match (entry.parse)(&envelope) {
                            Some(matched) if matched.command.module() == entry.module => {
                                resolved = true;
                            }
                            Some(matched) => rejected.push(format!(
                                "{text}：认领模块 {:?} 与解析模块 {:?} 不一致",
                                entry.module,
                                matched.command.module()
                            )),
                            None => rejected.push(format!(
                                "{text}：handler 认领了却解析不出模块（认领谓词与解析器漂移）"
                            )),
                        }
                    }
                    count => rejected.push(format!(
                        "{text}：被 {count} 个 handler 同时认领（优先级取决于表顺序，属歧义）"
                    )),
                }
            }
        }
        if resolved {
            // 有正解时，仍把"歧义"当作问题：歧义意味着真实消息可能被别的模块先吃掉。
            if rejected.iter().any(|line| line.contains("同时认领")) {
                problems.push(format!("{}：{rejected:?}", spec.name));
            }
        } else {
            problems.push(format!(
                "{}：没有任何可直接发出的写法能正确路由：{rejected:?}",
                spec.name
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "已登记命令项与真实路由表不一致：{problems:#?}"
    );
}

/// 反向守卫：路由表里的每个模块都必须能被**已登记命令**触发。
///
/// 防的是"路由表加了模块、命令表忘了登记"——那种情况下命令能触发，
/// 但面板和帮助里看不到，用户不知道有这条命令。
#[test]
fn every_routed_module_is_reachable_from_a_registered_command() {
    let mut reachable: std::collections::BTreeSet<ChatCommandModule> =
        std::collections::BTreeSet::new();
    for spec in static_commands() {
        for text in candidate_texts(spec) {
            for authority in spec.authorities {
                let envelope = envelope_for(text, spec.prefix, *authority);
                for entry in handlers_for(spec.prefix) {
                    if (entry.claims)(&envelope) {
                        reachable.insert(entry.module);
                    }
                }
            }
        }
    }
    let missing: Vec<String> = AT_HANDLERS
        .iter()
        .chain(HASH_START_HANDLERS.iter())
        .map(|entry| entry.module)
        .filter(|module| !reachable.contains(module))
        .map(|module| format!("{module:?}"))
        .collect();
    assert!(
        missing.is_empty(),
        "这些模块在路由表里，但没有任何已登记命令能触发它们（面板/帮助里看不到）：{missing:?}"
    );
}
