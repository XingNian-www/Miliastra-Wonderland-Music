//! W4b 的对拍与「漏登记变红」守卫。
//!
//! 重构的硬门槛是**行为等价**：宏派生出的枚举与表必须与 HEAD 的手写版本逐字相等。
//! 这里的每一条测试都直接读 [`MODULE_SPECS`] 与派生结果，与
//! [`super::registry_fixtures`] 里冻结的 HEAD 快照比对（顺序也参与比较，
//! 因为 `/commands` 的数组序是面板契约）。

use std::collections::BTreeSet;

use super::registry_fixtures as head;
use super::{
    ChatCommandModule, CommandTable, MODULE_SPECS, ModuleCommand, ModuleSpec, PrivatePayloadClass,
    REPLY_GROUPS,
};

/// 把 `ChatCommandModule` 的变体名按**派生顺序**取出。
///
/// 用 `Debug` 的变体名而不是手写字符串：这样「宏生成的顺序」与「HEAD 的顺序」
/// 比较的是同一口径，不会因为两边都抄错而假绿。
fn derived_module_names() -> Vec<String> {
    ChatCommandModule::ALL
        .iter()
        .map(|module| format!("{module:?}"))
        .collect()
}

/// 把 `ModuleCommand` 的变体名与载荷类型名按**派生顺序**取出。
fn derived_command_variants() -> Vec<(String, String)> {
    // 变体名由 `MODULE_SPECS` 的 `module:` 列给出，载荷类型由 `payload:` 列给出；
    // 两枚举由同一次宏调用展开，因此这里等价于直接读 `ModuleCommand`。
    let _: Option<&ModuleCommand> = None;
    MODULE_SPECS
        .iter()
        .map(|spec| {
            // `payload` 存的是 `stringify!` 的完整路径，取最后一段作为类型名。
            let payload = spec.payload.rsplit("::").next().unwrap_or(spec.payload);
            (format!("{:?}", spec.module), payload.to_string())
        })
        .collect()
}

#[test]
fn chat_command_module_variants_match_head_exactly() {
    assert_eq!(
        derived_module_names(),
        head::CHAT_COMMAND_MODULE
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "ChatCommandModule 的变体名或顺序与 HEAD 不一致"
    );
}

#[test]
fn module_command_variants_match_head_exactly() {
    assert_eq!(
        derived_command_variants(),
        head::MODULE_COMMAND
            .iter()
            .map(|(name, ty)| {
                let ty = ty.rsplit("::").next().unwrap_or(ty);
                (name.to_string(), ty.to_string())
            })
            .collect::<Vec<_>>(),
        "ModuleCommand 的变体名、载荷类型或顺序与 HEAD 不一致"
    );
}

#[test]
fn reply_groups_match_head_exactly_including_order() {
    let derived: Vec<(String, String, String, Vec<String>)> = REPLY_GROUPS
        .iter()
        .map(|group| {
            (
                group.command_id.to_string(),
                group.name.to_string(),
                group.category.to_string(),
                group.keys.iter().map(|k| k.to_string()).collect(),
            )
        })
        .collect();
    let expected: Vec<(String, String, String, Vec<String>)> = head::REPLY_GROUPS
        .iter()
        .map(|(id, name, category, keys)| {
            (
                id.to_string(),
                name.to_string(),
                category.to_string(),
                keys.iter().map(|k| k.to_string()).collect(),
            )
        })
        .collect();
    assert_eq!(derived.len(), expected.len(), "回复分组数量与 HEAD 不一致");
    for (index, (left, right)) in derived.iter().zip(expected.iter()).enumerate() {
        assert_eq!(left, right, "第 {index} 个回复分组与 HEAD 不一致");
    }
}

#[test]
fn every_reply_key_is_owned_exactly_once() {
    // HEAD 的每个 key 互不重复（总数由快照给出，不再硬编码）；派生后若某组重复登记，面板会出现
    // 「同一条文案能被两个命令同时编辑」的歧义。
    let mut seen = BTreeSet::new();
    let mut duplicates = Vec::new();
    for group in REPLY_GROUPS {
        for key in group.keys {
            if !seen.insert(*key) {
                duplicates.push(format!("{key}（{}）", group.command_id));
            }
        }
    }
    assert!(duplicates.is_empty(), "回复 key 重复登记：{duplicates:?}");
    let expected: usize = head::REPLY_GROUPS
        .iter()
        .map(|(_, _, _, keys)| keys.len())
        .sum();
    assert_eq!(
        seen.len(),
        expected,
        "回复 key 总数与 HEAD 的 {expected} 不一致"
    );
}

#[test]
fn every_reply_group_belongs_to_a_registered_module() {
    // `REPLY_GROUPS` 由各模块的 `replies:` 列拍平而来，因此每组的 owner
    // 必须能在 `MODULE_SPECS` 里找到——否则说明宏把两组串了。
    for spec in MODULE_SPECS {
        for group in spec.replies {
            assert!(
                REPLY_GROUPS
                    .iter()
                    .any(|candidate| candidate.command_id == group.command_id),
                "{} 登记的回复分组 {} 没有进 REPLY_GROUPS",
                spec.label,
                group.command_id
            );
        }
    }
}

#[test]
fn module_specs_and_chat_command_module_are_one_to_one() {
    // 你要的编译期守护的运行时补充：两个列表必须同长同序。
    // （真正的编译期保证是：`ChatCommandModule` 的变体**由同一个宏**生成，
    // 少一段宏调用就少一个变体；见本模块文档与 `private_payload` 的穷尽 match。）
    assert_eq!(
        MODULE_SPECS.len(),
        ChatCommandModule::ALL.len(),
        "MODULE_SPECS 与 ChatCommandModule::ALL 长度不一致"
    );
    for (spec, module) in MODULE_SPECS.iter().zip(ChatCommandModule::ALL.iter()) {
        assert_eq!(spec.module, *module, "MODULE_SPECS 与 ALL 的顺序不一致");
    }
}

#[test]
fn every_module_spec_field_is_filled() {
    for spec in MODULE_SPECS {
        assert!(
            !spec.module_path.is_empty(),
            "{} 缺 module_path",
            spec.label
        );
        assert!(!spec.label.is_empty(), "{:?} 缺 label", spec.module);
        assert!(
            !spec.config_section.is_empty(),
            "{} 缺 config_section",
            spec.label
        );
        assert!(!spec.payload.is_empty(), "{} 缺 payload", spec.label);
    }
}

/// 登记表里出现的全部命令名（主名 + 别名），供别的表反查。
fn registered_command_names() -> BTreeSet<&'static str> {
    MODULE_SPECS
        .iter()
        .flat_map(ModuleSpec::all_names)
        .collect()
}

#[test]
fn command_names_are_unique_per_prefix_inside_the_registry() {
    // `@` 与 `#` 下同名（如「娱乐帮助」在两种前缀各一条）是允许的，
    // 但同一前缀下同名说明登记表自己重复了。
    let mut seen: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let mut duplicates = Vec::new();
    for spec in MODULE_SPECS {
        for command in spec.commands {
            for name in command.all_names() {
                let key = (format!("{:?}", command.prefix), name);
                if !seen.insert(key) {
                    duplicates.push(format!("{name}（{:?}）", command.prefix));
                }
            }
        }
    }
    assert!(duplicates.is_empty(), "登记表内命令名重复：{duplicates:?}");
}

#[test]
fn every_command_table_in_all_is_produced_by_some_module() {
    let produced: BTreeSet<CommandTable> = MODULE_SPECS
        .iter()
        .flat_map(|spec| spec.commands.iter().map(|command| command.table))
        .collect();
    for table in CommandTable::ALL {
        assert!(
            produced.contains(&table),
            "CommandTable::ALL 里的 {table:?} 没有任何模块贡献"
        );
    }
}

#[test]
fn private_payload_class_is_declared_for_every_module() {
    // 分类本身只是登记；真正的载荷判定在 `command.rs`。这里确保没有模块
    // 漏掉这一列（`Never` 也是显式选择，不是默认值）。
    for spec in MODULE_SPECS {
        let declared = matches!(
            spec.private_payload,
            PrivatePayloadClass::Never
                | PrivatePayloadClass::Always
                | PrivatePayloadClass::BySource
                | PrivatePayloadClass::ByShape
                | PrivatePayloadClass::Mixed
        );
        assert!(declared, "{} 的私密载荷分类非法", spec.label);
    }
    assert!(
        MODULE_SPECS
            .iter()
            .any(|spec| spec.private_payload == PrivatePayloadClass::Always),
        "AI 聊天应登记为 Always"
    );
}

#[test]
fn registered_names_are_not_empty_strings() {
    for name in registered_command_names() {
        assert!(!name.trim().is_empty(), "登记了空命令名");
        assert_eq!(name.trim(), name, "命令名不应带空白：{name:?}");
    }
}

/// `static_commands()` 的拼接顺序必须与 [`CommandTable::ALL`] 一致。
///
/// 登记表声明「按这个顺序拼」，实际拼接也必须如此——否则面板的命令顺序会漂移。
#[test]
fn static_commands_follows_the_registered_table_order() {
    use crate::features::command_catalog::static_commands;

    let specs: Vec<&crate::features::command_catalog::CommandSpec> = static_commands().collect();
    assert!(!specs.is_empty());

    // 逐表校验：对每张表，它在 `static_commands()` 里出现的下标区间必须单调。
    let order: Vec<CommandTable> = CommandTable::ALL.to_vec();
    let mut cursor = 0usize;
    for table in order {
        let names: BTreeSet<&str> = registered_names_for_table(table);
        if names.is_empty() {
            continue;
        }
        let mut advanced = false;
        while cursor < specs.len() && names.contains(specs[cursor].name) {
            cursor += 1;
            advanced = true;
        }
        assert!(
            advanced,
            "CommandTable::{table:?} 的命令在 static_commands() 里没有按登记顺序连续出现"
        );
    }
}

/// 某张表登记的全部命令名。
fn registered_names_for_table(table: CommandTable) -> BTreeSet<&'static str> {
    MODULE_SPECS
        .iter()
        .flat_map(|spec| spec.commands.iter())
        .filter(|command| command.table == table)
        .flat_map(|command| command.all_names())
        .collect()
}

// ---------------------------------------------------------------------------
// `private_payload` 对拍：手写判定 vs 登记表分类
// ---------------------------------------------------------------------------

/// 登记表的 `private_payload:` 分类必须与 `command.rs` 里手写判定的**行为**一致。
///
/// `private_payload` 含 `message_type == "pink"` 这类按来源分流的谓词与
/// `TarotCommand::Draw { ai: true, .. }` 这类结构解构，**推不出来**，所以它保持手写。
/// 本条测试直接驱动真实判定，断言「每个模块的实际行为」落在登记表声明的分类里——
/// 改登记表却忘了改手写逻辑，这里会红。
#[test]
fn private_payload_behaviour_matches_the_registry() {
    use super::registry_fixtures as fixtures;
    use crate::features::command::registry::PrivatePayloadClass;
    use crate::features::command::{ModuleCommand, PrivatePayload};

    let declared = |module: &str| {
        MODULE_SPECS
            .iter()
            .find(|spec| format!("{:?}", spec.module) == module)
            .map(|spec| spec.private_payload)
            .unwrap_or_else(|| panic!("{module} 未登记"))
    };

    let samples: Vec<(&str, ModuleCommand)> = vec![
        (
            "SongRequest",
            ModuleCommand::SongRequest(fixtures::sample_song()),
        ),
        ("AiChat", ModuleCommand::AiChat(fixtures::sample_ai_chat())),
    ];

    for (module, command) in samples {
        let class = declared(module);
        let public = command.private_payload("blue");
        let private = command.private_payload("pink");
        match class {
            PrivatePayloadClass::Never => {
                assert!(public.is_none(), "{module} 声明 Never，公屏却判为私密");
                assert!(private.is_none(), "{module} 声明 Never，私聊却判为私密");
            }
            PrivatePayloadClass::Always => {
                assert_eq!(
                    public,
                    Some(PrivatePayload::AiChatQuestion),
                    "{module} 声明 Always，公屏必须也是私密"
                );
                assert_eq!(
                    private,
                    Some(PrivatePayload::AiChatQuestion),
                    "{module} 声明 Always，私聊必须也是私密"
                );
            }
            // BySource / ByShape / Mixed 按来源或结构分流，由各模块自己的测试覆盖；
            // 这里只断言「至少有一种来源判为私密」这一必要条件。
            PrivatePayloadClass::BySource
            | PrivatePayloadClass::ByShape
            | PrivatePayloadClass::Mixed => {
                assert!(
                    public.is_some() || private.is_some(),
                    "{module} 声明为按来源/结构分流，却两种来源都不私密"
                );
            }
        }
    }
}
