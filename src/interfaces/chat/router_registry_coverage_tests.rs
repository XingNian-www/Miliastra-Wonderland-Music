//! 登记表 ↔ 路由表双向覆盖（W4b 的「漏登记变红」守卫）。
//!
//! 为什么住在 `interfaces`：`AT_HANDLERS` / `HASH_START_HANDLERS` 只在本模块内可见，
//! 而 `MODULE_SPECS` 在 `features`。反向 `use` 会新增 `features → interfaces`
//! 违规边（红线），因此这条守卫只能放在能同时看到两边的地方——路由层自己。
//!
//! `claims`/`parse` 闭包是**刻意手写**的（每模块签名不同，从「模块名」推不出
//! 「类型与方法」），因此「登记表加了模块、路由表忘了加」原本是静默的：命令出现
//! 在面板与帮助里，游戏里却永不认领。本文件把这种漏登记变成红色。

use super::{AT_HANDLERS, HASH_START_HANDLERS};
use crate::features::command::MODULE_SPECS;
use crate::features::command_catalog::SESSION_DISPATCH;

/// 自定义流程没有 `handler(...)` 条目：由 `ChatCommandRouter::route_match` 末尾
/// 的 `self.custom_workflow.is_some_and(...)` 直接判定。
const MODULES_WITHOUT_A_HANDLER: &[&str] = &["CustomWorkflow"];

#[test]
fn every_registered_module_appears_in_the_router_tables() {
    let mut routed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in AT_HANDLERS {
        routed.insert(format!("{:?}", entry.module));
    }
    for entry in HASH_START_HANDLERS {
        routed.insert(format!("{:?}", entry.module));
    }
    for rule in SESSION_DISPATCH {
        routed.insert(format!("{:?}", rule.module));
    }
    for module in MODULES_WITHOUT_A_HANDLER {
        routed.insert((*module).to_string());
    }

    let missing: Vec<String> = MODULE_SPECS
        .iter()
        .map(|spec| format!("{:?}", spec.module))
        .filter(|module| !routed.contains(module))
        .collect();
    assert!(
        missing.is_empty(),
        "这些模块登记在 MODULE_SPECS 里，但 AT_HANDLERS / HASH_START_HANDLERS / \
         SESSION_DISPATCH 都没有它们——命令在面板里可见，游戏里却永不认领：{missing:?}"
    );
}

#[test]
fn every_routed_module_is_registered() {
    let registered: std::collections::BTreeSet<String> = MODULE_SPECS
        .iter()
        .map(|spec| format!("{:?}", spec.module))
        .collect();
    let mut unregistered = Vec::new();
    for module in AT_HANDLERS
        .iter()
        .chain(HASH_START_HANDLERS.iter())
        .map(|entry| entry.module)
        .chain(SESSION_DISPATCH.iter().map(|rule| rule.module))
    {
        let name = format!("{module:?}");
        if !registered.contains(&name) {
            unregistered.push(name);
        }
    }
    assert!(
        unregistered.is_empty(),
        "路由表里这些模块没有登记到 MODULE_SPECS（面板与帮助里看不到）：{unregistered:?}"
    );
}

/// 登记表声明的命令，必须真的能被**某张**路由表认领到。
///
/// 与 `alias_oracle_tests` 的区别：那边驱动的是 `command_catalog` 的命令字面量，
/// 这边驱动的是 `MODULE_SPECS` 的 `commands:` 列——两处独立登记，互相校验。
/// 只在登记表里出现、`command_catalog` 却没有字面量的命令，在这里会红。
#[test]
fn every_registered_command_name_is_claimable() {
    use crate::features::command::{CommandAuthority, CommandObservation};

    let mut problems = Vec::new();
    for spec in MODULE_SPECS {
        for command in spec.commands {
            // 至少一种**可发**写法 + 至少一种来源必须被认领。
            //
            // 有些命令裸名触发不了（`@导入QQ歌单<链接>` 必须有参数），这不是漂移：
            // 登记表里没有参数字面量，因此从 `command_catalog` 的同一批 `CommandSpec`
            // 里取它登记的 `usage` 示例（与 `alias_oracle_tests` 同一口径）。
            let mut claimed = false;
            for name in claimable_texts(command.name, command.prefix) {
                for authority in spec_authorities(command.prefix) {
                    let glyph = match command.prefix {
                        crate::features::command::CommandPrefix::At => "@",
                        crate::features::command::CommandPrefix::Hash => "#",
                        crate::features::command::CommandPrefix::Amp => "&",
                    };
                    let message_type = match authority {
                        CommandAuthority::HallMember => "blue",
                        CommandAuthority::Friend => "pink",
                    };
                    let Some(envelope) = crate::features::command::CommandEnvelope::new(
                        format!("{glyph}{name}"),
                        "测试用户",
                        message_type,
                        format!("{glyph}{name}"),
                        CommandObservation::default(),
                    ) else {
                        continue;
                    };
                    let handlers = match command.prefix {
                        crate::features::command::CommandPrefix::At => AT_HANDLERS,
                        crate::features::command::CommandPrefix::Hash => HASH_START_HANDLERS,
                        crate::features::command::CommandPrefix::Amp => &[],
                    };
                    if handlers.iter().any(|entry| (entry.claims)(&envelope)) {
                        claimed = true;
                        break;
                    }
                    if SESSION_DISPATCH.iter().any(|rule| (rule.claims)(&envelope)) {
                        claimed = true;
                        break;
                    }
                }
                if claimed {
                    break;
                }
            }
            if !claimed {
                problems.push(format!(
                    "{} -> {}（{:?}）",
                    spec.label, command.name, command.prefix
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "这些登记在 MODULE_SPECS 的命令，裸写时没有任何 handler 认领：{problems:#?}"
    );
}

/// 一条命令的**可发**写法：命令名本身，加上 `command_catalog` 里登记的全部 `usage` 示例。
///
/// 与 `alias_oracle_tests::candidate_texts` 同一口径：判定"能不能发"交给真实 handler，
/// 这里只负责给出候选，不猜。
fn claimable_texts(
    name: &'static str,
    prefix: crate::features::command::CommandPrefix,
) -> Vec<&'static str> {
    let mut texts: Vec<&'static str> = vec![name];
    for spec in crate::features::command_catalog::static_commands() {
        if spec.prefix != prefix {
            continue;
        }
        if !spec.all_names().any(|candidate| candidate == name) {
            continue;
        }
        for usage in spec.usage {
            let trimmed = usage.trim_start_matches(['@', '#']);
            if !trimmed.is_empty() {
                texts.push(trimmed);
            }
        }
    }
    texts
}

/// 该前缀下允许的来源。
///
/// 与 HEAD 的两张表一致：`@` 命令大厅/好友都可能有（各模块 `claims_chat` 自己门控），
/// `#` 开局命令只在大厅发起。
fn spec_authorities(
    prefix: crate::features::command::CommandPrefix,
) -> &'static [crate::features::command::CommandAuthority] {
    use crate::features::command::CommandAuthority;
    use crate::features::command::CommandPrefix;
    match prefix {
        CommandPrefix::At => &[CommandAuthority::HallMember, CommandAuthority::Friend],
        CommandPrefix::Hash => &[CommandAuthority::HallMember],
        CommandPrefix::Amp => &[],
    }
}
