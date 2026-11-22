//! 记忆存储：预算水位、大厅上下文、压缩与遗忘级联、事实抽取。

use super::support::*;
use super::*;

#[test]
fn compression_keeps_recent_turns_and_never_blanks_the_summary() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    // 预算设得很小，确保一定越过触发水位（82%）。
    for index in 0..12 {
        memory
            .record_turn(
                "甲",
                &format!("第{index}个问题，内容足够长以占用一些 token 空间"),
                &format!("第{index}个回答，也要够长才能触发压缩"),
                200,
                1,
            )
            .unwrap();
    }
    let plan = memory
        .plan_actor_compression("甲", 200)
        .unwrap()
        .expect("越过触发水位时必须给出压缩计划");
    assert!(!plan.records.is_empty(), "应当取到需要压缩的原文");
    // 压缩输入必须带问答双方，不能只剩问题。
    assert!(
        plan.records.iter().any(|r| r.contains("→")),
        "压缩输入应包含问答双方，实际 {:?}",
        plan.records
    );

    // 空摘要不能覆盖已有内容。
    memory
        .apply_compression(&plan, "旧摘要内容", 2_000, 1)
        .unwrap();
    memory.apply_compression(&plan, "   ", 2_000, 2).unwrap();
    let context = memory.context("甲").unwrap();
    assert_eq!(context.summary.as_deref(), Some("旧摘要内容"));
}

/// append-only：没超读取预算时，`context().turns` 只追加，历史前缀逐字节一致。
///
/// 这是 provider 前缀缓存的前提：`messages` 的第一条历史一变，OpenAI /
/// DeepSeek 的自动缓存就从最前面失配，后面整段都要重付。旧实现每 8 轮
/// 滑一个块，正是这个问题的来源；现在改为读全部未压缩原文。
#[test]
fn history_is_append_only_while_under_the_read_budget() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    let mut previous: Vec<(String, String)> = Vec::new();
    for index in 0..40 {
        memory
            .record_turn(
                "甲",
                &format!("问{index}"),
                &format!("答{index}"),
                ACTOR_TOKEN_BUDGET,
                index as i64,
            )
            .unwrap();
        let context = memory.context("甲").unwrap();
        let current: Vec<(String, String)> = context
            .turns
            .iter()
            .map(|turn| (turn.question.clone(), turn.answer.clone()))
            .collect();
        assert_eq!(
            current.len(),
            index + 1,
            "第 {index} 轮后应读到全部未压缩历史（append-only）"
        );
        assert_eq!(
            &current[..previous.len()],
            previous.as_slice(),
            "第 {index} 轮后历史前缀必须逐字节不变"
        );
        assert_eq!(
            current.last(),
            Some(&(format!("问{index}"), format!("答{index}"))),
            "新内容只能追加在尾部"
        );
        previous = current;
    }
    // 历史变长不影响提示词尾部：问题行仍然最后、且只出现一次。
    let context = memory.context("甲").unwrap();
    let config = config();
    let prompt = build_prompt(&PromptInput {
        config: &config,
        question: "再推荐一首",
        context: &context,
        allowed_commands: &[],
        asker: "甲",
        hall: &HallContext::default(),
        now_unix: TEST_NOW_UNIX,
    });
    assert!(
        prompt.ends_with("[甲]:再推荐一首"),
        "问题行必须在最后，实际尾部：{}",
        &prompt[prompt.len().saturating_sub(120)..]
    );
    assert_eq!(
        prompt.matches("再推荐一首").count(),
        1,
        "问题不能在模板中部再展开一次"
    );
    // 历史正文只走独立消息，不重复塞进模板。
    assert!(!prompt.contains("问0"));
}

/// 读取上限只做异常保护：超过 96K 时整段保留最近历史，不跳行。
#[test]
fn history_read_keeps_the_most_recent_whole_segment() {
    // 纯函数：每轮 1K token。
    let tokens = vec![1_000usize; 200];
    let start = actor_turn_read_start(&tokens);
    let expected = 200 - ACTOR_READ_TOKEN_BUDGET / 1_000;
    assert_eq!(start, expected, "应刚好装下 96K 的最近整段");
    assert!(tokens[start..].iter().sum::<usize>() <= ACTOR_READ_TOKEN_BUDGET);
    assert!(
        tokens[start - 1..].iter().sum::<usize>() > ACTOR_READ_TOKEN_BUDGET,
        "再多一轮就超预算，说明保留的是最大整段"
    );
    // 远小于预算时一律读全部。
    assert_eq!(actor_turn_read_start(&vec![1usize; 500]), 0);
    assert_eq!(actor_turn_read_start(&[]), 0);

    // 库里同样按「最近整段」裁：读到的必须是连续的一段尾部，不能跳行。
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    let questions: Vec<String> = (0..120)
        .map(|index| format!("第{index}问{}", "中".repeat(500)))
        .collect();
    let answers: Vec<String> = (0..120)
        .map(|index| format!("第{index}答{}", "文".repeat(500)))
        .collect();
    for (index, (question, answer)) in questions.iter().zip(answers.iter()).enumerate() {
        memory
            .record_turn(
                "甲",
                question,
                answer,
                // 落库预算放到极大，确保这里只测读取侧的保护。
                1_000_000,
                index as i64,
            )
            .unwrap();
    }
    let tokens: Vec<usize> = questions
        .iter()
        .zip(answers.iter())
        .map(|(question, answer)| estimate_tokens(question) + estimate_tokens(answer))
        .collect();
    let expected_start = actor_turn_read_start(&tokens);
    assert!(expected_start > 0, "这组数据必须真的超读取预算");
    assert!(
        tokens[expected_start..].iter().sum::<usize>() <= ACTOR_READ_TOKEN_BUDGET,
        "保留段不能超读取预算"
    );
    let context = memory.context("甲").unwrap();
    assert_eq!(context.turns.len(), questions.len() - expected_start);
    assert_eq!(
        context.turns.first().unwrap().question,
        questions[expected_start],
        "保留的必须是最近整段（连续、从 expected_start 开始）"
    );
    assert_eq!(
        context.turns.last().unwrap().question,
        questions[questions.len() - 1],
        "最后一条必须是最新那条"
    );
}

/// 单条消息异常大时仍至少保留 12 轮，不能把历史读成空。
#[test]
fn history_read_keeps_at_least_twelve_turns_even_with_a_huge_message() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    for index in 0..30 {
        memory
            .record_turn("甲", &format!("问{index}"), "答", 1_000_000, index as i64)
            .unwrap();
    }
    // 一条就超过读取预算的消息。
    let huge = "中".repeat(100_000);
    memory
        .record_turn("甲", &huge, "答", 1_000_000, 99)
        .unwrap();
    let context = memory.context("甲").unwrap();
    assert_eq!(
        context.turns.len(),
        ACTOR_READ_MIN_TURNS,
        "超预算时至少保留最近 12 轮"
    );
    assert_eq!(context.turns.last().unwrap().question, huge);
}

/// 真实预算下的迟滞：96K 的 82% 才触发，一次压到 15%。
#[test]
fn compression_at_the_real_budget_triggers_at_eighty_two_and_drops_to_fifteen() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    // 每轮 2000 token；触发线 78.72K = 39.36 轮，目标 14.4K。
    let question = "中".repeat(1_000);
    let answer = "文".repeat(1_000);
    for _ in 0..39 {
        memory
            .record_turn("甲", &question, &answer, ACTOR_TOKEN_BUDGET, 1)
            .unwrap();
    }
    assert!(
        memory
            .plan_actor_compression("甲", ACTOR_TOKEN_BUDGET)
            .unwrap()
            .is_none(),
        "39 轮共 78K，刚好在 82% 触发线以下，不应触发"
    );
    memory
        .record_turn("甲", &question, &answer, ACTOR_TOKEN_BUDGET, 2)
        .unwrap();
    let plan = memory
        .plan_actor_compression("甲", ACTOR_TOKEN_BUDGET)
        .unwrap()
        .expect("超过 82% 必须触发压缩");
    memory
        .apply_compression(&plan, "压缩后的摘要", ACTOR_SUMMARY_BUDGET, 3)
        .unwrap();
    let remaining: usize = memory
        .context("甲")
        .unwrap()
        .turns
        .iter()
        .map(|turn| estimate_tokens(&turn.question) + estimate_tokens(&turn.answer))
        .sum();
    let target = ACTOR_TOKEN_BUDGET * COMPRESS_TARGET_PERCENT / 100;
    assert!(
        remaining <= target,
        "压到 15% 以内，实际 {remaining} > {target}"
    );
    assert!(remaining > 0, "最新一轮不能被压掉");
    assert!(
        memory
            .plan_actor_compression("甲", ACTOR_TOKEN_BUDGET)
            .unwrap()
            .is_none(),
        "压完必须回到触发线以下，否则下一轮又压"
    );
}

/// 迟滞：只有越过触发水位才压，且一次压到目标水位，压完不会立刻再触发。
///
/// 这是"BOT 每轮都烧一次摘要调用"的根因回归：旧实现把 10% 同时当触发线和
/// 回落目标，压完即贴线，下一轮必定再压。
#[test]
fn compression_uses_hysteresis_watermarks_instead_of_triggering_every_turn() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    // 预算 1000：触发线 750，目标 200。每轮约 60 token，15 轮 ≈ 900。
    let question = "中".repeat(30);
    let answer = "文".repeat(30);
    for _ in 0..15 {
        memory
            .record_turn("甲", &question, &answer, 1_000, 1)
            .unwrap();
    }
    let total_before: usize = memory
        .context("甲")
        .unwrap()
        .turns
        .iter()
        .map(|turn| estimate_tokens(&turn.question) + estimate_tokens(&turn.answer))
        .sum();
    assert!(total_before > 0);

    let plan = memory
        .plan_actor_compression("甲", 1_000)
        .unwrap()
        .expect("越过 82% 触发线必须给计划");
    let consumed = plan.source_ids.len();
    // 真正写回压缩结果后才有「回落」：只拿计划不 apply 时数据库没变，
    // 断言剩余量等于断言读取窗口，与本测试要守的迟滞无关。
    memory
        .apply_compression(&plan, "压缩后的摘要", 1_000, 2)
        .unwrap();
    let remaining: usize = {
        let context = memory.context("甲").unwrap();
        context
            .turns
            .iter()
            .map(|turn| estimate_tokens(&turn.question) + estimate_tokens(&turn.answer))
            .sum()
    };
    assert!(consumed > 0, "计划里应包含待压条目");
    // 压完必须明显低于触发线，否则下一轮又压——这就是迟滞要保证的。
    assert!(
        remaining < 1_000 * COMPRESS_TRIGGER_PERCENT / 100,
        "压缩后应落到目标水位附近，实际 {remaining}"
    );
}

/// prune_actor 的兜底删除阈值必须高于压缩触发线。
///
/// 如果兜底就是落库预算本身（或比触发线低），每轮 `record_turn` 都会先删
/// 最旧一行，历史又退化成滑动窗口，前缀缓存与 append-only 都失效。
#[test]
fn prune_fallback_sits_above_the_compression_trigger() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    // 预算 1000：触发线 820，兜底 1500。每轮 60 token，14 轮 = 840。
    // 已越过触发线，但远未到兜底阈值，record_turn 不应删任何行。
    let question = "中".repeat(30);
    let answer = "文".repeat(30);
    for index in 0..14 {
        memory
            .record_turn("甲", &question, &answer, 1_000, index)
            .unwrap();
    }
    let context = memory.context("甲").unwrap();
    assert_eq!(
        context.turns.len(),
        14,
        "未到 150% 兜底线时不能删行（触发线 82% 只应由压缩消费）"
    );
    assert_eq!(context.turns.first().unwrap().question, question);
    assert_eq!(context.turns.last().unwrap().question, question);
    assert!(
        14 * 60 > 1_000 * COMPRESS_TRIGGER_PERCENT / 100,
        "这组数据必须真的越过触发线"
    );
}

/// 压缩只动原文与摘要，facts 是独立层，不能被压缩删掉。
#[test]
fn compression_never_touches_facts() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    memory.add_fact("甲", "事实一", 0).unwrap();
    memory.add_fact("甲", "事实二", 1).unwrap();
    let question = "中".repeat(100);
    let answer = "文".repeat(100);
    // 小预算触发压缩：触发线 820，目标 150。
    for index in 0..12 {
        memory
            .record_turn("甲", &question, &answer, 1_000, index)
            .unwrap();
    }
    let plan = memory
        .plan_actor_compression("甲", 1_000)
        .unwrap()
        .expect("应触发压缩");
    assert!(!plan.source_ids.is_empty());
    memory
        .apply_compression(&plan, "压缩后的摘要", 1_000, 99)
        .unwrap();
    let context = memory.context("甲").unwrap();
    assert_eq!(
        context.facts,
        vec!["事实一".to_string(), "事实二".to_string()],
        "facts 必须原样保留，压缩只动 turns"
    );
    assert_eq!(context.summary.as_deref(), Some("压缩后的摘要"));
    assert!(
        context.turns.len() < 12,
        "原文应被压缩搬走一部分，实际 {}",
        context.turns.len()
    );
}

/// 事实层独立预算：超限时淘汰最久未更新的记录，且不影响最近问答原文。
#[test]
fn facts_have_their_own_cap_and_do_not_evict_recent_turns() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    memory
        .record_turn("甲", "最近一轮问题", "最近一轮回答", 10_000, 1)
        .unwrap();
    for index in 0..(MAX_FACTS_PER_ACTOR + 5) {
        memory
            .add_fact("甲", &format!("事实{index}"), index as i64)
            .unwrap();
    }
    let context = memory.context("甲").unwrap();
    assert_eq!(
        context.facts.len(),
        MAX_FACTS_PER_ACTOR,
        "事实条数应按上限收敛"
    );
    assert_eq!(context.turns.len(), 1, "事实淘汰不能牵连问答原文");
    assert_eq!(context.turns[0].question, "最近一轮问题");
}

/// 大厅原文的硬上限兜底：压缩没跑起来时不能无限增长。
#[test]
fn hall_lines_are_capped_even_without_compression() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    let budget = 40;
    for index in 0..40 {
        memory
            .record_hall_line(
                "甲",
                &format!("第{index}条大厅发言，内容足够长"),
                HallSpeakerRole::Player,
                "",
                budget,
                index as i64,
            )
            .unwrap();
    }
    let context = memory.hall_context("").unwrap();
    assert!(!context.lines.is_empty(), "至少要保留最新一条");
    assert!(
        context.lines.len() < 40,
        "超出硬上限后应淘汰最旧记录，实际 {}",
        context.lines.len()
    );
    // 保留的必须是较新的内容。
    assert!(context.lines.last().unwrap().1.contains("第39条"));
}

/// 大厅读取上限 4K：落库保留更多，但每轮只注入最近的一段。
#[test]
fn hall_context_reads_only_the_recent_lines_within_the_read_budget() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    // 每条 1000 token；落库预算放到极大，只测读取窗口。
    let line = "中".repeat(1_000);
    for index in 0..40 {
        memory
            .record_hall_line(
                "甲",
                &line,
                HallSpeakerRole::Player,
                "",
                1_000_000,
                index as i64,
            )
            .unwrap();
    }
    let context = memory.hall_context("").unwrap();
    assert_eq!(
        context.lines.len(),
        HALL_READ_BUDGET / 1_000,
        "大厅读取应停在 4K 预算上"
    );
    // 读的是最新一段，最旧的那些不注入。
    assert!(context.lines.iter().all(|(_, text, _)| text == &line));
    assert_eq!(memory.hall_stats().unwrap().0, 40, "读取不能删库里的行");
}

/// Bot 行并入 hall 后仍有独立预算：最近 6 条且 ≤800 token，取先到者；
/// 玩家行不受影响，仍按 HALL_READ_BUDGET 读取。
#[test]
fn hall_context_caps_bot_lines_without_eating_player_budget() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    let bot_line = "中".repeat(150);
    for index in 0..10 {
        memory
            .record_hall_line(
                "橘子",
                &bot_line,
                HallSpeakerRole::Bot,
                "",
                1_000_000,
                index as i64,
            )
            .unwrap();
    }
    let player_line = "啊".repeat(1_000);
    for index in 0..10 {
        memory
            .record_hall_line(
                "甲",
                &player_line,
                HallSpeakerRole::Player,
                "",
                1_000_000,
                100 + index,
            )
            .unwrap();
    }
    let context = memory.hall_context("").unwrap();
    let bots = context
        .lines
        .iter()
        .filter(|(_, _, role)| matches!(role, HallSpeakerRole::Bot))
        .count();
    let players = context.lines.len() - bots;
    // 150 token × 6 条 = 900 > 800，因此按 token 预算只保留 5 条。
    assert_eq!(bots, 5, "Bot 行应按 800 token 上限截断: {context:?}");
    // 玩家行仍按 4K 预算保留最近的 4 条（1000 token/条）。
    assert_eq!(players, 4, "玩家行不应被 Bot 行挤掉: {context:?}");
    assert_eq!(memory.hall_stats().unwrap().0, 20, "读取不能删库里的行");
}

/// 旧库升级：没有 actor_key 列的表要能自动 ALTER 补列，且旧行继续可读。
#[test]
fn legacy_hall_table_is_migrated_and_keeps_old_rows() {
    let connection = Connection::open_in_memory().unwrap();
    // 模拟 75cb8e7 之前的旧表：无 actor_key（role 也先不加，验证两列都能补）。
    connection
        .execute_batch(
            "CREATE TABLE ai_chat_hall_turns (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 speaker TEXT NOT NULL,
                 text TEXT NOT NULL,
                 tokens INTEGER NOT NULL,
                 added_at INTEGER NOT NULL
             );
             INSERT INTO ai_chat_hall_turns(speaker, text, tokens, added_at)
             VALUES ('旧玩家', '升级前的发言', 6, 1);",
        )
        .unwrap();
    let memory = AiChatMemory::from_connection(connection).unwrap();
    let columns = {
        let connection = memory.connection.lock().unwrap();
        connection
            .prepare("PRAGMA table_info(ai_chat_hall_turns)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert!(columns.iter().any(|name| name == "actor_key"));
    assert!(columns.iter().any(|name| name == "role"));
    // 旧行 actor_key 走默认空串：任何玩家的 hall 都能看到，不会升级即丢。
    let context = memory.hall_context("任意-key").unwrap();
    assert_eq!(context.lines.len(), 1);
    assert_eq!(context.lines[0].1, "升级前的发言");
    assert_eq!(context.lines[0].2, HallSpeakerRole::Player);
}

/// 方案 A 的受众互补：A 自己的提问/BOT 对 A 的回复只进个人 messages；
/// 在 B 的 hall 里仍能看到（同样 B 的也不出现在 A 的 hall）。
#[test]
fn hall_context_filters_current_actor_but_keeps_others_complementary() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    memory
        .record_hall_line(
            "甲",
            "甲的问题",
            HallSpeakerRole::Player,
            "key-a",
            1_000_000,
            1,
        )
        .unwrap();
    memory
        .record_hall_line(
            "橘子",
            "对甲的回答",
            HallSpeakerRole::Bot,
            "key-a",
            1_000_000,
            2,
        )
        .unwrap();
    memory
        .record_hall_line(
            "乙",
            "乙的问题",
            HallSpeakerRole::Player,
            "key-b",
            1_000_000,
            3,
        )
        .unwrap();
    memory
        .record_hall_line(
            "橘子",
            "对乙的回答",
            HallSpeakerRole::Bot,
            "key-b",
            1_000_000,
            4,
        )
        .unwrap();
    memory
        .record_hall_line(
            "丙",
            "旧库无归属行",
            HallSpeakerRole::Player,
            "",
            1_000_000,
            5,
        )
        .unwrap();

    let a = memory.hall_context("key-a").unwrap();
    let a_texts: Vec<&str> = a.lines.iter().map(|(_, text, _)| text.as_str()).collect();
    assert!(!a_texts.contains(&"甲的问题"), "A 的提问不应在 A 的 hall");
    assert!(
        !a_texts.contains(&"对甲的回答"),
        "BOT 对 A 的回复不应在 A 的 hall"
    );
    assert!(a_texts.contains(&"乙的问题"), "其他人的提问必须共享");
    assert!(a_texts.contains(&"对乙的回答"), "BOT 对别人的回复必须共享");
    assert!(a_texts.contains(&"旧库无归属行"), "旧库空 key 行必须保留");

    let b = memory.hall_context("key-b").unwrap();
    let b_texts: Vec<&str> = b.lines.iter().map(|(_, text, _)| text.as_str()).collect();
    assert!(!b_texts.contains(&"乙的问题"));
    assert!(!b_texts.contains(&"对乙的回答"));
    assert!(b_texts.contains(&"甲的问题"));
    assert!(b_texts.contains(&"对甲的回答"));

    let all = memory.hall_context("").unwrap();
    assert_eq!(all.lines.len(), 5, "面板空 key 应拿到全量视图");
}

/// actor_key 在写入/过滤/遗忘三处统一 trim，空格不能造成漏过滤/漏删。
#[test]
fn actor_key_is_trimmed_for_filter_and_forget() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    memory
        .record_hall_line(
            "甲",
            "带空格 key 的提问",
            HallSpeakerRole::Player,
            " key-trim ",
            1_000_000,
            1,
        )
        .unwrap();
    for key in ["key-trim", " key-trim ", "key-trim "] {
        let hall = memory.hall_context(key).unwrap();
        let texts: Vec<&str> = hall
            .lines
            .iter()
            .map(|(_, text, _)| text.as_str())
            .collect();
        assert!(
            !texts.contains(&"带空格 key 的提问"),
            "key={key:?} 时必须过滤自己的行"
        );
    }
    assert!(memory.forget(" key-trim ", " key-trim ").unwrap());
    let hall = memory.hall_context("").unwrap();
    let texts: Vec<&str> = hall
        .lines
        .iter()
        .map(|(_, text, _)| text.as_str())
        .collect();
    assert!(
        !texts.contains(&"带空格 key 的提问"),
        "遗忘必须按 trim 后 key 删除"
    );
}

/// 遗忘级联：删除该 actor 的大厅行 + 清空大厅摘要；别人的行保留。
#[test]
fn forgetting_an_actor_cascades_to_hall_rows_and_hall_summary() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    memory.add_fact("key-a", "甲的旧事实", 1).unwrap();
    memory
        .record_turn("key-a", "甲的问题", "甲的回答", 100_000, 2)
        .unwrap();
    memory
        .record_hall_line(
            "甲",
            "甲的问题",
            HallSpeakerRole::Player,
            "key-a",
            1_000_000,
            3,
        )
        .unwrap();
    memory
        .record_hall_line(
            "橘子",
            "对甲的回答",
            HallSpeakerRole::Bot,
            "key-a",
            1_000_000,
            4,
        )
        .unwrap();
    memory
        .record_hall_line(
            "乙",
            "乙的问题",
            HallSpeakerRole::Player,
            "key-b",
            1_000_000,
            5,
        )
        .unwrap();
    {
        let connection = memory.connection.lock().unwrap();
        connection
            .execute(
                "INSERT INTO ai_chat_summaries(scope, summary, tokens, updated_at)
                 VALUES ('hall', '含甲内容的大厅摘要', 20, 6)",
                [],
            )
            .unwrap();
    }
    assert!(memory.forget("key-a", "key-a").unwrap());
    let context = memory.context("key-a").unwrap();
    assert!(context.facts.is_empty());
    assert!(context.turns.is_empty());
    let hall = memory.hall_context("").unwrap();
    let texts: Vec<&str> = hall
        .lines
        .iter()
        .map(|(_, text, _)| text.as_str())
        .collect();
    assert!(!texts.contains(&"甲的问题"), "被遗忘者的大厅提问必须删除");
    assert!(!texts.contains(&"对甲的回答"), "BOT 对 TA 的回复必须删除");
    assert!(texts.contains(&"乙的问题"), "别人的大厅行不能被误删");
    assert!(hall.summary.is_none(), "大厅摘要必须整体清空");
}

/// 旧库遗忘：actor_names 别名、空 actor_key 的旧大厅行都要清；无删除时
/// 不能顺手清掉别人的 hall 摘要。
#[test]
fn forgetting_cascades_to_legacy_aliases_and_keeps_summary_when_nothing_removed() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    memory.record_actor_name("uuid-x", "旧甲", 1).unwrap();
    memory.add_fact("旧甲", "按旧昵称存的事实", 2).unwrap();
    memory
        .record_turn("旧甲", "旧问题", "旧回答", 100_000, 3)
        .unwrap();
    {
        let connection = memory.connection.lock().unwrap();
        connection
            .execute(
                "INSERT INTO ai_chat_hall_turns(speaker, text, tokens, added_at, role, actor_key)
                 VALUES ('旧甲', '升级前的发言', 6, 4, 'player', '')",
                [],
            )
            .unwrap();
        // legacy BOT 行没有 actor 归属：遗忘时也保守清掉。
        connection
            .execute(
                "INSERT INTO ai_chat_hall_turns(speaker, text, tokens, added_at, role, actor_key)
                 VALUES ('橘子', '升级前的BOT回复', 6, 5, 'bot', '')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO ai_chat_hall_turns(speaker, text, tokens, added_at, role, actor_key)
                 VALUES ('橘子', '别人的BOT回复', 6, 6, 'bot', 'key-b')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO ai_chat_summaries(scope, summary, tokens, updated_at)
                 VALUES ('hall', '全局大厅摘要', 20, 5)",
                [],
            )
            .unwrap();
    }
    assert!(memory.forget("uuid-x", "旧甲").unwrap());
    assert!(memory.context("旧甲").unwrap().facts.is_empty());
    let hall = memory.hall_context("").unwrap();
    let texts: Vec<&str> = hall
        .lines
        .iter()
        .map(|(_, text, _)| text.as_str())
        .collect();
    assert!(!texts.contains(&"升级前的BOT回复"), "legacy BOT 行必须清掉");
    assert!(texts.contains(&"别人的BOT回复"), "有归属的 BOT 行不能误删");
    assert!(!texts.contains(&"升级前的发言"), "legacy 玩家行必须清掉");
    assert!(
        !memory.forget("uuid-x", "旧甲").unwrap(),
        "第二次遗忘不应再有删除"
    );
    // 第二次无删除：不能把第一次已删后新写入的全局摘要再清掉。
    {
        let connection = memory.connection.lock().unwrap();
        connection
            .execute(
                "INSERT INTO ai_chat_summaries(scope, summary, tokens, updated_at)
                 VALUES ('hall', '另一批全局摘要', 20, 6)",
                [],
            )
            .unwrap();
    }
    assert!(!memory.forget("不存在的-key", "不存在的-key").unwrap());
    let summary: Option<String> = memory
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT summary FROM ai_chat_summaries WHERE scope = 'hall'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(summary.as_deref(), Some("另一批全局摘要"));
}

/// 迁移幂等：同一文件连续打开两次不应因重复列失败。
#[test]
fn hall_migration_is_idempotent_across_reopens() {
    let path = std::env::temp_dir().join(format!(
        "ai_chat_migration_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let memory = AiChatMemory::open(&path).unwrap();
        memory
            .record_hall_line("甲", "第一行", HallSpeakerRole::Player, "", 1_000, 1)
            .unwrap();
    }
    {
        let memory = AiChatMemory::open(&path).unwrap();
        assert_eq!(memory.hall_context("").unwrap().lines.len(), 1);
    }
    let _ = std::fs::remove_file(&path);
}

/// 摘要硬上限：面板上的 summary_budget 必须真的生效。
#[test]
fn summary_is_clipped_to_its_configured_budget() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    // 预算 200：触发线 150。每轮约 60 token，4 轮后落在 150~200 之间。
    for _ in 0..4 {
        memory
            .record_turn("甲", &"中".repeat(30), &"文".repeat(30), 200, 1)
            .unwrap();
    }
    let plan = memory
        .plan_actor_compression("甲", 200)
        .unwrap()
        .expect("应有压缩计划");
    let long_summary = "很长的摘要句子。".repeat(80);
    memory
        .apply_compression(&plan, &long_summary, 40, 2)
        .unwrap();
    let stored = memory.context("甲").unwrap().summary.unwrap();
    assert!(
        estimate_tokens(&stored) <= 40,
        "摘要应被裁到预算内，实际 {} token",
        estimate_tokens(&stored)
    );
    assert!(!stored.trim().is_empty(), "裁剪后不能是空摘要");
}

#[test]
fn fact_update_parses_additions_and_removals() {
    let (facts, remove) =
        parse_fact_update(r#"{"facts":["喜欢晴天"],"remove":["玩家的名字是安娜"]}"#);
    assert_eq!(facts, vec!["喜欢晴天".to_string()]);
    assert_eq!(remove, vec!["玩家的名字是安娜".to_string()]);

    // 旧格式（只有 facts）仍然可用，remove 为空。
    let (facts, remove) = parse_fact_update(r#"{"facts":["常在晚上上线"]}"#);
    assert_eq!(facts, vec!["常在晚上上线".to_string()]);
    assert!(remove.is_empty());

    // 非法负载一律视为没有变化。
    let (facts, remove) = parse_fact_update("not json");
    assert!(facts.is_empty() && remove.is_empty());
    let (facts, _) = parse_fact_update(r#"{"facts":"single"}"#);
    assert!(facts.is_empty());

    // 超长事实被丢弃而不是截断。
    let long = "字".repeat(MAX_FACT_CHARS + 1);
    let (facts, _) = parse_fact_update(&format!(r#"{{"facts":["{long}"]}}"#));
    assert!(facts.is_empty());
}

/// 回归：改名后旧称呼必须能被淘汰。
///
/// 线上出现过同一个玩家 10 条事实里既有「叫玩家乙」又有「叫安娜」
/// 「叫别名乙」，模型随机挑一条，表现为「没使用最新信息」。
#[test]
fn fact_update_can_retire_a_superseded_nickname() {
    let reply = r#"{"facts":["玩家现在的称呼是别名甲喵"],"remove":["玩家叫别名甲"]}"#;
    let (facts, remove) = parse_fact_update(reply);
    assert_eq!(facts.len(), 1, "必须写入新称呼");
    assert_eq!(remove.len(), 1, "必须淘汰旧称呼");
}

#[test]
fn token_estimate_treats_cjk_as_one_and_ascii_as_cheaper() {
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("橘子"), 2);
    assert!(estimate_tokens("abcdefghij") < estimate_tokens("十个中文字符啊啊"));
}

#[test]
fn memory_keeps_context_prunes_to_budget_and_forgets() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();

    memory
        .record_turn("甲", "问题一", "回答一", 10_000, 1)
        .unwrap();
    memory.add_fact("甲", "喜欢晴天", 2).unwrap();
    let context = memory.context("甲").unwrap();
    assert_eq!(context.turns.len(), 1);
    assert_eq!(context.facts, vec!["喜欢晴天".to_string()]);

    // 事实去重：重复写入仍只有一条。
    memory.add_fact("甲", "喜欢晴天", 3).unwrap();
    assert_eq!(memory.summary("甲").unwrap().0, 1);

    // 预算收紧到只够一轮：最旧的一轮被淘汰，最新一轮保留。
    memory.record_turn("甲", "问题二", "回答二", 2, 4).unwrap();
    let context = memory.context("甲").unwrap();
    assert_eq!(context.turns.len(), 1, "只保留最新一轮");
    assert_eq!(context.turns[0].question, "问题二");

    assert!(memory.forget("甲", "甲").unwrap(), "仍有记忆可清空");
    let context = memory.context("甲").unwrap();
    assert!(context.turns.is_empty());
    assert!(context.facts.is_empty());
    assert!(!memory.forget("甲", "甲").unwrap(), "第二次清空不再有内容");
}

/// 决策 5-b 的生产接线：遗忘级联必须**按人**清 trace，而不是整环清空。
///
/// 这条用例打的是「改回 `clear()` 会不会被发现」：全清之后别人的报文也会消失，
/// `count == 1` 与「乙的报文仍在」两条断言同时变红。只测 `purge_player` 本身
/// （`ai_trace` 里的单测）证明不了这条路真的被 `forget()` 调用。
#[test]
fn forgetting_a_player_purges_only_their_trace_entries() {
    // trace 是进程级单例，必须与 `ai_trace` 的测试串行。
    let _guard = crate::ai_trace::test_lock();
    crate::ai_trace::clear_for_tests();

    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    // 先写一条要遗忘的记忆：`removed == 0` 时级联整段不执行，测不到接线。
    memory.add_fact("甲", "喜欢晴天", 1).unwrap();

    for name in ["甲", "乙"] {
        crate::ai_trace::record(miliastra_kernel::ai::WireExchange {
            wire_api: "ChatCompletions",
            request: serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "稳定人设"},
                    {"role": "user", "content": format!("[{name}]:今天点什么歌")}
                ],
                "tools": []
            }),
            response: None,
            error: None,
            started_at_ms: 1_793_026_800_000,
            elapsed: std::time::Duration::from_millis(100),
        });
    }

    assert!(memory.forget("甲", "甲").unwrap(), "甲仍有记忆可清空");

    let list: serde_json::Value =
        serde_json::from_str(&crate::ai_trace::list_json()).expect("trace 列表是 JSON");
    assert_eq!(
        list["count"], 1,
        "只应清掉甲的报文；整环 clear() 会把乙的一起丢掉：{list}"
    );
    // 列表只有摘要，正文要按 id 取详情才看得到（这正是要断言的东西）。
    let id = list["entries"][0]["id"].as_u64().expect("条目 id");
    let text = crate::ai_trace::entry_json(id).expect("条目详情");
    assert!(text.contains("乙"), "别人的报文必须保留：{text}");
    assert!(!text.contains("甲"), "被遗忘者的原文不得残留：{text}");

    crate::ai_trace::clear_for_tests();
}

#[test]
fn actors_are_isolated_from_each_other() {
    let memory = AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap();
    memory.add_fact("甲", "只属于甲", 1).unwrap();
    memory.add_fact("乙", "只属于乙", 2).unwrap();

    assert_eq!(
        memory.context("甲").unwrap().facts,
        vec!["只属于甲".to_string()]
    );
    assert_eq!(
        memory.context("乙").unwrap().facts,
        vec!["只属于乙".to_string()]
    );
}

/// 抽取提示词必须告诉模型「谁在说话」。
///
/// 原来只传裸正文，模型不知道主语是谁，于是把「玩家自称」写成
/// 「玩家称呼助手为X」，也会把大厅里别人的名字记到本人头上。
#[test]
fn extraction_prompt_names_the_speaker_and_directions() {
    let prompt = fact_extraction_prompt("玩家乙", "（暂无）", "叫我别名乙", "哼，记住了");
    assert!(prompt.contains("玩家乙"), "必须包含提问者昵称");
    assert!(prompt.contains("玩家自称："), "必须给出自称槽位");
    assert!(prompt.contains("玩家称呼助手："), "必须给出助手称呼槽位");
    assert!(
        prompt.contains("绝不能写成「玩家称呼助手为玩家丙」"),
        "必须给出方向反例"
    );
}
