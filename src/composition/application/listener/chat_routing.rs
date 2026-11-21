//! 聊天快照消费、命令路由与权限闸门。
//!
//! 从 `listener.rs` 拆出：从「共享聊天快照」到「命令进入正式队列」的全部业务判定，
//! 加上 actor 映射、命令锁提交与 AI 关键词触发。
//!
//! ## 这个文件必须守住的四条纪律
//!
//! 1. **业务单写者**：命令只经 [`AppServices::enqueue_pending_command`] 入队，
//!    这里从不直接执行业务动作。
//! 2. **命令锁只认真正派发成功**：被禁用/未路由/闲聊只确认（`acknowledge`），
//!    不写锁；写锁用的文本与锚点必须来自被派发消息自身，绝不回头查当前画面。
//! 3. **ChatTimeline 单锁**：所有观察状态都经 `self.ui.chat_timeline` 的既有方法
//!    访问，本文件不另存一份，避免出现第二个真相。
//! 4. **消费门控看范围不看屏幕桶**：`hall_consumes_scope` 只按 [`ChatScope`] 判定，
//!    屏幕分桶键（[`ChatIdentity`]）只用于跨帧去重分桶。

use super::super::secondary_chat::SecondaryObservationOutcome;
use super::super::*;
use super::display::{
    ObservationDedup, actor_role_label, chat_identity_label, compact_for_log, delivery_state_label,
    gap_kind_label, message_source_label,
};
use crate::features::administration::{
    AdministrationCommandContext, AdministrationDispatch, ImmediateAdministrationOutcome,
};
use crate::observation::chat::{ChatIdentity, ChatScope, SourceId};

/// 排查日志用：折叠空白并截断，避免日志里塞入超长文本。
/// 命令锁锚点：OCR 消息用屏幕 y，外部消息用服务端序号。
///
/// 外部消息的 `block` 是哨兵坐标，拿它当屏幕锚点会让「同文本即同一条」
/// 对所有已记录命令成立，从而整批吞掉真实命令。
///
/// **只接受屏幕来源的 id。** 外部消息的原始服务端序号不存在于 id 里
/// （`id.bubble_sequence` 是去重键的 FNV 哈希，不单调，不能当锚点），
/// 所以外部消息必须自己带着 `source_sequence` 走到
/// `ChatTimelineMessage::lock_anchor()`，不能经这里推算。
pub(in crate::composition::application) fn lock_anchor_for(
    id: &crate::observation::chat::ObservedChatMessageId,
    screen_y: i32,
) -> crate::observation::chat::LockAnchor {
    use crate::observation::chat::LockAnchor;
    debug_assert!(
        id.is_from_screen(),
        "lock_anchor_for 只适用于屏幕消息；外部消息请用 ChatTimelineMessage::lock_anchor()"
    );
    LockAnchor::Screen { y: screen_y }
}

/// 命令在**拦截点**被挡下的具体原因。
///
/// 拦截点在入队/派发之前（[\`AppServices::command_gate\`]），
/// 命令走不到 \`execute_command\`，所以文案必须在这里出；
/// 只有把原因结构化，才能对玩家说清「是面板关了」还是「你权限不够」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::composition::application) enum CommandGate {
    /// 放行。
    Allowed,
    /// 面板把这条命令关掉了。
    Disabled,
    /// 命令开着，但发送者档位低于 \`minimum_role\`。
    BelowMinimum(crate::features::command_access::CommandPermission),
    /// 全局命令开关（\`@禁用\`）关着，且发送者是未登记的大厅成员。
    GlobalOff,
}

impl CommandGate {
    /// 这次拦截是否需要给玩家一句提示。
    pub(in crate::composition::application) const fn is_blocked(self) -> bool {
        !matches!(self, Self::Allowed)
    }

    /// 纯判定：给定策略、发送者档位与全局开关，得出拦截原因。
    ///
    /// 抽成纯函数是为了能脱离 \`ApplicationRuntime\` 单测——上一版把文案写在
    /// \`execute_command\` 里，纯函数测得通、玩家却永远收不到，正是漏掉这一层。
    pub(in crate::composition::application) fn from_policy(
        policy: &crate::features::command_access::CommandPolicy,
        actor: crate::features::command_access::CommandPermission,
        authority: CommandAuthority,
        sender_is_mapped: bool,
        commands_enabled: bool,
    ) -> Self {
        if !policy.allows(Some(actor)) {
            return if policy.enabled {
                Self::BelowMinimum(policy.minimum_role)
            } else {
                Self::Disabled
            };
        }
        // 全局开关（@禁用）只挡「未登记的大厅成员」；好友与已映射角色不受影响。
        if !commands_enabled && authority == CommandAuthority::HallMember && !sender_is_mapped {
            return Self::GlobalOff;
        }
        Self::Allowed
    }

    /// 日志里的原因说明：把档位转成中文，避免 `{:?}` 打出代码词。
    pub(in crate::composition::application) const fn log_reason(self) -> &'static str {
        use crate::features::command_access::CommandPermission;
        match self {
            Self::Allowed => "已放行",
            Self::Disabled => "该命令已在面板禁用",
            Self::BelowMinimum(CommandPermission::User) => "该命令当前对所有大厅成员关闭",
            Self::BelowMinimum(CommandPermission::Friend) => "权限不足：该命令仅限好友使用",
            Self::BelowMinimum(CommandPermission::Admin | CommandPermission::Owner) => {
                "权限不足：该命令仅限管理员使用"
            }
            Self::GlobalOff => "全局命令开关已关闭，未登记的大厅成员不派发命令",
        }
    }

    /// 面向玩家的文案；\`None\` 表示不放行但也不提示（目前没有这种情形）。
    ///
    /// 与旧 \`execute_command\` 的三档文案保持一致，避免同一个原因两处说得不一样。
    pub(in crate::composition::application) const fn message(self) -> Option<&'static str> {
        use crate::features::command_access::CommandPermission;
        match self {
            Self::Allowed => None,
            // 命令本身是关的：不要去谈权限档位，那会把排查方向带偏。
            Self::Disabled => Some("该命令已在面板禁用"),
            // minimum_role=User 却仍被拒：说明不是档位问题（User 已是最低档），
            // 而是更外层把命令整体关了。
            Self::BelowMinimum(CommandPermission::User) => Some("该命令当前对所有大厅成员关闭"),
            Self::BelowMinimum(CommandPermission::Friend) => Some("权限不足：该命令仅限好友使用"),
            Self::BelowMinimum(CommandPermission::Admin | CommandPermission::Owner) => {
                Some("权限不足：该命令仅限管理员使用")
            }
            // 沿用旧说法：这条只针对全局开关，不是单条命令的权限。
            Self::GlobalOff => Some("命令识别已禁用"),
        }
    }
}

/// 拦截后应当回给玩家的提示；\`None\` 表示不回复。
///
/// 单独成函数，是为了让「什么时候该回、回什么」能被直接断言：
/// - 只有**看起来像命令**的文本才回，闲聊不打扰（防刷屏第一道）；
/// - 放行时当然不回。
///
/// 传入的必须是**带前缀**的命令文本（`RoutedCommand::user_command`）。
/// `RoutedCommand::raw` 已剥掉 @/#，喂进 `looks_like_command` 只会得到 false。
pub(in crate::composition::application) fn gate_notice(
    gate: CommandGate,
    command_text: &str,
) -> Option<&'static str> {
    if !gate.is_blocked() {
        return None;
    }
    if !crate::interfaces::chat::looks_like_command(command_text) {
        return None;
    }
    gate.message()
}

pub(in crate::composition::application) fn chat_listener_mode_dispatch(
    parsed: &RoutedCommand,
) -> Option<ChatListenerModeCommand> {
    if parsed.permission_required.is_some() {
        return None;
    }
    let ModuleCommand::Administration(command) = &parsed.command else {
        return None;
    };
    let AdministrationDispatch::ChatListenerMode(command) = command.dispatch() else {
        return None;
    };
    Some(command)
}

/// 大厅归属闸门：一级值守只消费主大厅，二级值守只消费二级大厅。
///
/// 消费门控：这条消息该不该由当前值守模式处理。
///
/// **只看消息范围（`ChatScope`），不看屏幕分桶键（`ChatIdentity`）。**
///
/// 这道闸门存在的理由：一级区与二级区可能是同一个大厅，共享观察里的跨帧去重
/// 结构（`command_locks`、气泡键、业务认领）都按 `ChatIdentity` 分桶，
/// 两个桶互不相识，同一条大厅消息会被各派发一次。互斥成立在消费处。
///
/// 但「同屏去重」与「消息属于哪个容器」是两件事：
///
/// - OCR 的一级区/二级区确实是**两个屏幕桶**，需要按模式互斥；
/// - 外部聊天源没有屏幕，它的大厅消息借 `SecondaryHall` 桶只是为了共用去重结构，
///   并不代表「BOT 正看着二级界面」。
///
/// 旧实现按 `chat` 判断，导致用户把值守切到一级后，外部源的大厅命令被整批丢弃
/// （日志：`值守模式=一级值守 不消费 SecondaryHall 观察`）。
pub(in crate::composition::application) fn hall_consumes_scope(
    mode: ChatListenerMode,
    scope: &ChatScope,
    source: &SourceId,
    chat: Option<&ChatIdentity>,
) -> bool {
    if !scope.is_hall() {
        // 私聊与大厅无关，不受值守模式约束。
        return true;
    }
    // 屏幕互斥只对屏幕来源成立。
    //
    // 外部源没有屏幕：同一条消息只会经由一个来源到达，不存在「一级区与二级区
    // 各派发一次」的问题。把它纳入按模式互斥，会让用户切到一级后外部命令被整批丢弃。
    if !source.is_ocr() {
        return true;
    }
    // 走到这里一定是 OCR 帧，屏幕桶必然存在（`publish` 恒填 Some）。
    // `None` 会在下面落进 `_ => true`，那只是给「外部帧误走这里」留的兜底，
    // 不是一条正常路径。
    match chat {
        // OCR 的屏幕桶：按模式互斥，维持原有的同屏去重语义。
        Some(ChatIdentity::PrimaryHall) => mode == ChatListenerMode::Primary,
        Some(ChatIdentity::SecondaryHall) => mode == ChatListenerMode::Secondary,
        _ => true,
    }
}

pub(in crate::composition::application) fn acknowledge_unrecognized_primary_message(
    chat_timeline: &crate::observation::chat::ChatTimeline,
    observed: &PrimaryObservedMessage,
    captured_at: Instant,
    baseline_primed: bool,
) -> Result<()> {
    if baseline_primed || observed.message.message_type != "pink" {
        chat_timeline.acknowledge_ignored(
            &observed.id,
            captured_at,
            &observed.message.text,
            None,
            None,
        )?;
    } else {
        // 仅粉色首屏历史永久消费；蓝色首屏的未识别文本仍可由有效修订交付。
        //
        // 必须带快照时间：这条气泡在后续帧里可能被 OCR 修正成一条真命令，
        // 那次修正会推进修订围栏；用无时间的永久 ACK 会把修正后的命令永久吞掉。
        chat_timeline.acknowledge_business_at(&observed.id, captured_at)?;
    }
    Ok(())
}

/// 一级命令的收尾：`write_lock` 只在真正派发成功点为 true；被禁用/被跳过的路径
/// 只确认不写锁。蓝色首屏允许执行，不再用启动 seed 把它永久跳过。
///
/// 缺陷 6：`message_text` / `message_y` 任一缺失就**不写锁**并 warn，绝不用
/// `unwrap_or_default` 退化成空串/0——空文本会静默不写锁，y=0 则会命中任何
/// `y <= 28` 的同文本气泡，把无关命令错误抑制掉。
///
/// 时间基准取自**命令自身捕获的观察时刻**（`observation.captured_at`），
/// 而不是调用时刻：命令从被观察到成功派发之间会先在待处理队列里等，
/// 期间同一气泡可能被 OCR 修正。围栏比较必须用观察时刻，否则新修订反而会被
/// 迟到的那份伪装成“更新”的快照通过。
pub(in crate::composition::application) fn acknowledge_primary_command(
    chat_timeline: &crate::observation::chat::ChatTimeline,
    command: &RoutedCommand,
    write_lock: bool,
) -> Result<()> {
    let Some(message_id) = command.observation.message_id.as_ref() else {
        return Ok(());
    };
    // 控制台/测试构造的命令没有快照时间：这类命令不存在「同一气泡的更新修订」，
    // 只能走无围栏的兼容入口（见 ChatTimeline 两个 API 的语义边界说明）。
    let captured_at = command.observation.captured_at;
    let acknowledged = if write_lock {
        // 用被派发消息自身捕获的文本与锚点，绝不回头查当前 tracker。
        match (
            command
                .observation
                .message_text
                .as_deref()
                .filter(|text| !text.trim().is_empty()),
            command.observation.lock_anchor,
        ) {
            (Some(text), Some(anchor)) => match captured_at {
                Some(captured_at) => chat_timeline.acknowledge_command_at_revision(
                    message_id,
                    captured_at,
                    text,
                    anchor,
                )?,
                None => chat_timeline.acknowledge_command_at(message_id, text, anchor)?,
            },
            // 文本或锚点任一缺失：不再打印它们本身（缺的就是不可信内容），
            // 只说明缺了什么、后果如何。
            (_text, _anchor) => {
                log::warn!(
                    "命令锁未写入：这条命令的观察缺少可信文本或屏幕锚点（所属聊天区：{}），本轮只确认、不写锁，下次重整画面后会重新判定",
                    chat_identity_label(&message_id.chat),
                );
                acknowledge_business_observation(chat_timeline, message_id, captured_at)?
            }
        }
    } else {
        acknowledge_business_observation(chat_timeline, message_id, captured_at)?
    };
    if !acknowledged {
        log::debug!(
            "一级命令完成处理时，这条修订已不在待处理队列，或已被更新的修订推进围栏取代，本次确认未生效（所属聊天区：{}）",
            chat_identity_label(&message_id.chat)
        );
    }
    Ok(())
}

/// 确认一条业务消息已处理，时间基准是它**被观察到的时刻**。
///
/// `captured_at` 为 `None` 只出现在控制台/测试构造的观察里；真实聊天观察总是
/// 带着快照时间，因此真实路径一律核验修订围栏。
fn acknowledge_business_observation(
    chat_timeline: &crate::observation::chat::ChatTimeline,
    message_id: &crate::observation::chat::ObservedChatMessageId,
    captured_at: Option<Instant>,
) -> Result<bool> {
    match captured_at {
        Some(captured_at) => chat_timeline.acknowledge_business_at(message_id, captured_at),
        None => chat_timeline.acknowledge_business(message_id),
    }
}

/// 二级/好友命令派发的唯一提交点：只有真正作为命令派发成功才写命令锁。
/// 普通闲聊、被禁用/未路由/被邀请去重的命令只确认（不写锁）。
pub(in crate::composition::application) fn complete_secondary_claim(
    claim: crate::observation::chat::SharedBusinessClaim,
    command_dispatched: bool,
) -> Result<()> {
    if command_dispatched {
        claim.complete_as_command()
    } else {
        claim.complete_ignored()
    }
}

impl AppServices {
    pub(in crate::composition::application) fn clear_hall_countdown_cache_for_new_visual_session(
        &self,
        reason: &str,
    ) -> Result<bool> {
        let cleared = self.business.business.clear_hall_countdown_cache()?;
        self.reset_chat_observation_baseline(reason)?;
        if cleared {
            log::debug!(
                "大厅倒计时缓存已清理（原因：{}），等待本次大厅检测重新确认剩余时间",
                reason
            );
        }
        Ok(cleared)
    }

    pub(in crate::composition::application) fn reset_chat_observation_baseline(
        &self,
        reason: &str,
    ) -> Result<()> {
        self.ui
            .chat_baseline_primed
            .store(false, AtomicOrdering::SeqCst);
        self.ui.chat_timeline.note_baseline(reason)?;
        let visual_session = self.ui.chat_observations.begin_visual_session()?;
        log::debug!(
            "聊天观察已进入新的视觉会话（原因：{}，会话序号 {}）：仅粉色首屏建立历史基线，其他颜色照常交付",
            reason,
            visual_session.get()
        );
        Ok(())
    }

    pub(in crate::composition::application) fn consume_shared_chat_snapshots(
        &self,
        subscriber: &crate::observation::chat::ChatTimelineSubscriber,
        backlog: &mut std::collections::VecDeque<crate::observation::chat::ChatTimelineSnapshot>,
        dedup: &mut ObservationDedup,
    ) -> Result<()> {
        // 暂停值守：**必须照常取走快照并丢弃**，不能直接跳过读取。
        //
        // 发布侧不看暂停标志（观察线程仍在按 loop_idle_ms 推帧），所以只要不取，
        // 订阅者队列就会一路涨到 4096 上限。撞上限时发布侧会把订阅者标成失败并清空
        // 队列，下一次 read 直接报错——错误沿消费循环→扫描循环→主循环冒到 main，
        // 看门狗于是重启整个进程。表现就是「暂停两三个小时后程序自己退出」。
        //
        // 取走并丢弃等于「暂停期间的消息确实不处理」，符合暂停语义；恢复后
        // 靠 OCR 冷基线重建观察状态。积压一并清掉，避免恢复瞬间暴发式补跑历史命令。
        if self.shared.paused.load(AtomicOrdering::SeqCst) {
            let discarded = subscriber.read()?.len().saturating_add(backlog.len());
            backlog.clear();
            if discarded > 0 {
                log::debug!("值守已暂停，本次丢弃 {discarded} 帧共享聊天观察，恢复后按冷基线重建");
            }
            return Ok(());
        }
        let snapshots = subscriber.read()?;
        if backlog.len().saturating_add(snapshots.len()) > 4096 {
            return Err(anyhow!(
                "共享聊天业务积压超过 4096 帧，停止值守以避免静默丢失"
            ));
        }
        backlog.extend(snapshots);
        if backlog.is_empty() {
            return Ok(());
        }
        // 大厅归属闸门按逻辑值守模式判定。本批次取一次模式：批次中途发生模式切换时，
        // 沿用既有顺序语义，不在这里新增一致性机制。
        let listener_mode = self.business.business.chat_listener_snapshot()?.mode;
        while !backlog.is_empty() {
            // 普通派发与独占起点原子协调；期间仍读取共享流并保留业务积压。
            let Some(_business_dispatch) =
                self.ui.chat_observations.try_begin_business_dispatch()?
            else {
                // 临时暂缓（backlog 保留，窗口结束后继续），走 debug 防刷屏。
                log::debug!(
                    "命令暂缓：独占聊天观察正在进行，当前积压 {} 帧，该窗口结束后继续派发",
                    backlog.len()
                );
                break;
            };
            let snapshot = backlog.pop_front().expect("nonempty business backlog");
            self.consume_shared_chat_snapshot(snapshot, listener_mode, dedup)?;
        }
        Ok(())
    }

    pub(in crate::composition::application) fn consume_shared_chat_snapshot(
        &self,
        snapshot: crate::observation::chat::ChatTimelineSnapshot,
        listener_mode: ChatListenerMode,
        dedup: &mut ObservationDedup,
    ) -> Result<()> {
        let frame = self
            .ui
            .chat_observations
            .begin_frame(snapshot.captured_at)?;
        let result = (|| {
            // 二级大厅与主观察可能是同一个大厅，跨身份去重互不相识，因此按模式互斥消费：
            // 一级值守不消费二级大厅观察，二级值守不消费主大厅观察。
            // 这里只拦命令消费（不认领、不派发）；共享观察的发布、帧缓存与只读事件保持原样。
            if !hall_consumes_scope(
                listener_mode,
                &snapshot.scope,
                &snapshot.source,
                snapshot.chat.as_ref(),
            ) {
                // 按消息 id 去重：同一个快照批次被反复消费时（同一条卡在屏上的
                // 命令样式消息会随帧重复投递），不重复记同一句。
                // 该分支在值守模式互斥时每轮都会进，不去重会逐帧刷 info。
                for message in &snapshot.messages {
                    if command::looks_like_command(&message.text)
                        && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                    {
                        log::info!(
                            "命令未执行：当前为{}，不消费{}里的命令（该聊天区归另一种值守模式处理）「{}」",
                            listener_mode.label(),
                            snapshot
                                .chat
                                .as_ref()
                                .map(chat_identity_label)
                                .unwrap_or("未识别聊天区"),
                            compact_for_log(&message.text)
                        );
                    }
                }
                log::debug!(
                    "{}不消费{}的观察，仅保留只读发布",
                    listener_mode.label(),
                    snapshot
                        .chat
                        .as_ref()
                        .map(chat_identity_label)
                        .unwrap_or("未识别聊天区")
                );
                return Ok(());
            }
            // 分派桶：OCR 直接用屏幕桶，外部源按业务容器归到二级分派。
            //
            // 外部消息的 `chat` 恒为 None——它没有屏幕位置。二级分派分支内部
            // 一律用 `scope` 判 is_hall 与 friend_name，所以把外部帧交给它是对的。
            // 若哪天有人在那条分支里新增按 `chat` 判断的逻辑，外部私聊会被误伤；
            // 新增逻辑时请继续用 scope。
            let screen_bucket = match snapshot.chat.clone() {
                Some(bucket) => bucket,
                None => ChatIdentity::SecondaryHall,
            };
            match screen_bucket {
                ChatIdentity::PrimaryHall => {
                    let messages = snapshot
                        .messages
                        .into_iter()
                        .map(|message| {
                            let is_new = self
                                .ui
                                .chat_timeline
                                .is_business_pending_at(&message.id, snapshot.captured_at)?;
                            Ok(PrimaryObservedMessage {
                                id: message.id,
                                is_new,
                                message: ChatMessage {
                                    sender: message.sender,
                                    text: message.text,
                                    block: message.block,
                                    message_type: message.message_type,
                                },
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    self.dispatch_chat_observations(
                        vec![ChatObservationDispatch::Primary { frame, messages }],
                        dedup,
                    )?;
                }
                ref chat @ (ChatIdentity::SecondaryHall | ChatIdentity::Friend(_)) => {
                    // 业务容器看 scope，屏幕桶只看 chat。
                    //
                    // 外部聊天源的大厅消息被映射成 SecondaryHall 只是为了复用它去重结构，
                    // 真正的「是不是大厅 / 是哪个好友」由 scope 回答。这里以前用 chat
                    // 推导，等于让业务判定依赖屏幕位置——正是本轮要拆掉的耦合。
                    let is_hall = snapshot.scope.is_hall();
                    let accepts_turtle_questions = is_hall
                        && self.commands_enabled()?
                        && self.business.business.turtle_soup_accepts_questions()?;
                    // 好友名以业务容器为准：`Private.peer` 是身份判定的权威来源
                    // （外部源用 UID，OCR 用昵称，都由 `ChatScope::from_identity`
                    // 与 `external_chat_scope` 各自保证）。
                    //
                    // 第二个分支理论上不可达（`Friend` 的 scope 恒为 `Private`），
                    // 保留它作为映射漂移时的兜底：宁可退回屏幕桶里的名字，
                    // 也不要静默变成空串让下游以为「没有好友」。
                    let friend_name = match (&snapshot.scope, chat) {
                        (ChatScope::Private { peer }, _) => peer.to_string(),
                        (_, ChatIdentity::Friend(name)) => name.to_string(),
                        _ => String::new(),
                    };
                    for message in snapshot.messages {
                        if !self
                            .ui
                            .chat_timeline
                            .is_business_pending_at(&message.id, snapshot.captured_at)?
                        {
                            if command::looks_like_command(&message.text)
                                && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                            {
                                log::info!(
                                    "命令未执行：二级消息未判为新增追加（交付状态：{}，聊天区：{}）「{}」",
                                    delivery_state_label(
                                        self.ui.chat_timeline.business_delivery_state_at(
                                            &message.id,
                                            snapshot.captured_at,
                                        )?
                                    ),
                                    chat_identity_label(chat),
                                    compact_for_log(&message.text)
                                );
                            }
                            continue;
                        }
                        let revision_sender = message.sender.clone();
                        let revision_raw_sender = message.raw_sender.clone();
                        let sender = message
                            .raw_sender
                            .as_deref()
                            .map(parse_secondary_sender_identity)
                            .filter(|identity| !identity.effective_name().trim().is_empty())
                            .map(|identity| identity.effective_name().to_string())
                            // clone 而不是 move：下面还要从 message 取 lock_anchor()
                            // 与 text，部分移动会让它们不可用。
                            .or_else(|| message.sender.clone())
                            .filter(|sender| !sender.trim().is_empty());
                        if is_hall {
                            let classification = self.classify_secondary_business_message(
                                &message.text,
                                accepts_turtle_questions,
                            )?;
                            // 身份门禁看「有没有可授权身份」，不是「有没有昵称」。
                            //
                            // ChatBridge 协议保证每条用户消息都带 UID；没有 UID 的
                            // 不是用户消息（系统/观测帧），本就不该进这里。
                            // 旧实现只判 sender.is_none()，会把「有 UID、昵称未到」的
                            // 外部消息当作无身份拒掉——这是「接管后命令完全无效」的
                            // 第一道硬闸。
                            //
                            // MUST_READ §2 的「Unknown 不授权」仍然成立：
                            // 昵称与 UID 都没有才是 Unknown。
                            // 身份判定只有一处实现（features::command），避免两处漂移。
                            if classification.requires_identity
                                && !crate::features::command::has_known_identity(
                                    sender.as_deref(),
                                    message.account.as_ref(),
                                )
                            {
                                if command::looks_like_command(&message.text)
                                    && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                                {
                                    log::info!(
                                        "命令未执行：这条二级消息既没有 UID 也没有昵称，无法确认发送者，因此不授权「{}」",
                                        compact_for_log(&message.text)
                                    );
                                }
                                continue;
                            }
                        }
                        let id = message.id.clone();
                        // 锚点跟着来源走：OCR 用屏幕 y，外部源用服务端序号。
                        // 外部消息的 block 是哨兵坐标，拿它当屏幕锚点会让任何
                        // 已记录条目都命中，同文本命令被整批吞掉。
                        // 必须在 message 被部分移动前取出来。
                        let lock_anchor = message.lock_anchor();
                        // Claim atomically against discard suppression and other
                        // queued revisions before dispatching business effects.
                        let Some(claim) = self.ui.chat_timeline.claim_business_at(
                            &id,
                            snapshot.captured_at,
                            &message.text,
                            lock_anchor,
                        )?
                        else {
                            if command::looks_like_command(&message.text)
                                && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                            {
                                log::info!(
                                    "命令未执行：二级命令认领失败（已被基线或先前轮次消费，或本条修订被否决；交付状态：{}）「{}」",
                                    delivery_state_label(
                                        self.ui.chat_timeline.business_delivery_state_at(
                                            &message.id,
                                            snapshot.captured_at,
                                        )?
                                    ),
                                    compact_for_log(&message.text)
                                );
                            }
                            continue;
                        };
                        // AI 触发需要正文与类型：dispatch 会移动 message，先留存。
                        let ai_candidate = crate::observation::chat::scan::ChatMessage {
                            message_type: message.message_type.clone(),
                            block: message.block,
                            text: message.text.clone(),
                            sender: message.sender.clone(),
                        };
                        // 外部源昵称可能缺失但 UID 必有：AI 触发要用它兜底。
                        let ai_account = message.account.clone();
                        // 正文现在是纯正文，昵称必须从 sender 字段带过去；
                        // sender 稍后会被移动进 observation，先克隆一份。
                        let ai_sender = sender.clone();
                        let outcome = self.dispatch_chat_observations(
                            vec![ChatObservationDispatch::Secondary {
                                frame,
                                observation: SecondaryChatObservation {
                                    message_type: message.message_type,
                                    friend_name: friend_name.clone(),
                                    accepts_turtle_questions,
                                    messages: vec![SecondaryObservedMessage {
                                        id: id.clone(),
                                        text: message.text,
                                        sender,
                                        account: message.account,
                                    }],
                                },
                            }],
                            dedup,
                        )?;
                        // 关键词 AI 聊天：判据与一级路径完全一致——**路由失败之后**才
                        // 判定，而不是提前看「像不像命令」。`looks_like_command` 只是
                        // 样式启发，一条形似命令但没被任何模块认领的消息，在一级会去
                        // 尝试 AI 触发，在二级不该被判死。
                        if is_hall && outcome.unrecognized {
                            self.enqueue_ai_chat_keyword(
                                &id,
                                &ai_candidate,
                                ai_sender.as_deref(),
                                ai_account.as_ref(),
                                snapshot.captured_at,
                                lock_anchor,
                            )?;
                        }
                        // 缺陷 1：只有真正作为命令派发成功才写锁；
                        // 普通闲聊/被禁用/未路由只 ack，绝不能挤掉真命令条目。
                        if outcome.unrecognized {
                            claim.complete_unrecognized(
                                revision_sender.as_deref(),
                                revision_raw_sender.as_deref(),
                            )?;
                        } else {
                            complete_secondary_claim(claim, outcome.command_dispatched)?;
                        }
                    }
                }
                ChatIdentity::Unknown
                | ChatIdentity::PublicChannel
                | ChatIdentity::StrangerMessages => {
                    // 这些身份从不派发业务：Unknown 是身份没认出来，PublicChannel/
                    // StrangerMessages 不是玩家大厅。大厅里「等待身份修订」的消息
                    // 保持未确认，等 OCR 修订到达后再走大厅分支。
                    //
                    // 但必须留痕：这里是**永久 ACK**，一旦确认同一 id 不再交付。
                    // 二级面板昵称 OCR 失败时（高亮行读不出名字 → Unknown），屏上的
                    // `@点歌晴天` 会被这样静默吞掉，用户和日志都看不到任何证据。
                    //
                    // 确认必须带本帧 `captured_at`：这些身份虽然「从不派发业务」，
                    // 但同一气泡在后续帧里可能被识别出真实身份而改走大厅分支，
                    // 那次认出会推进修订围栏。无时间的永久 ACK 会把它永久吞掉。
                    for message in snapshot.messages {
                        if command::looks_like_command(&message.text)
                            && self.ui.chat_timeline.note_not_executed_log(&message.id)?
                        {
                            log::info!(
                                "命令未执行：{}不派发业务，本轮永久丢弃「{}」",
                                snapshot
                                    .chat
                                    .as_ref()
                                    .map(chat_identity_label)
                                    .unwrap_or("未识别聊天区"),
                                compact_for_log(&message.text)
                            );
                        }
                        self.ui
                            .chat_timeline
                            .acknowledge_business_at(&message.id, snapshot.captured_at)?;
                    }
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.ui.chat_observations.complete_without_messages(frame),
            Err(error) => {
                self.ui
                    .chat_observations
                    .record_terminal_failure(frame, format!("{error:#}"))?;
                Err(error)
            }
        }
    }

    pub(in crate::composition::application) fn forward_completion_advances(
        &self,
        subscriber: &mut CompletionAdvanceSubscriber,
    ) -> Result<()> {
        loop {
            match self
                .ui
                .chat_observations
                .read_completion_advance(subscriber)?
            {
                Some(ObservationRead::Item { value, .. }) => self
                    .business
                    .business_events
                    .submit(BusinessEvent::CompletionAdvance(Arc::unwrap_or_clone(
                        value,
                    )))
                    .context("向业务运行时提交观察完成推进")?,
                Some(ObservationRead::Gap(gap)) => self
                    .business
                    .business_events
                    .submit(BusinessEvent::CompletionGap(gap))
                    .context("向业务运行时提交观察完成流缺口")?,
                None => return Ok(()),
            }
        }
    }

    /// 把一批观察派发到业务侧。
    ///
    /// `dedup` 是主循环持有的展示去重状态：调试模式把观测推给外部 AI 时，
    /// 「这条气泡推过没有」必须跟着**这一轮循环**走，不能藏进进程级 `static`。
    pub(in crate::composition::application) fn dispatch_chat_observations(
        &self,
        dispatches: Vec<ChatObservationDispatch>,
        dedup: &mut ObservationDedup,
    ) -> Result<SecondaryObservationOutcome> {
        if crate::debug_mode::enabled() {
            // 调试模式下本进程不执行自动化——但"不执行"不等于"看不见"。
            // 外部 AI 恰恰是在这个模式下驱动游戏的，它必须知道游戏里说了什么、
            // 观测有没有漏。所以这里把原本要丢弃的观测**原样推送**出去：
            // 本进程当观察者，AI 当执行者。行为边界不变，仍然不执行任何自动化。
            dedup.publish_debug_chat_observations(&dispatches);
            let command_like: Vec<String> = dispatches
                .iter()
                .flat_map(|dispatch| match dispatch {
                    ChatObservationDispatch::Primary { messages, .. } => messages
                        .iter()
                        .map(|message| message.message.text.clone())
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .filter(|text| command::looks_like_command(text))
                .collect();
            if command_like.is_empty() {
                log::debug!("调试模式已启用，丢弃游戏内聊天命令与问题，不执行自动化");
            } else {
                log::info!(
                    "命令未执行：调试或完全控制模式已启用，本进程不执行游戏内命令「{}」",
                    compact_for_log(&command_like.join(" | "))
                );
            }
            return Ok(SecondaryObservationOutcome::default());
        }
        let mut secondary = SecondaryObservationOutcome::default();
        for dispatch in dispatches {
            match dispatch {
                ChatObservationDispatch::Primary { frame, messages } => {
                    let messages = messages.into_iter().collect::<Vec<_>>();
                    self.handle_scan_messages(frame.captured_at(), messages)?;
                }
                ChatObservationDispatch::Secondary { frame, observation } => {
                    let outcome = self.process_secondary_chat_observation(frame, observation)?;
                    secondary.command_dispatched |= outcome.command_dispatched;
                    secondary.unrecognized |= outcome.unrecognized;
                }
                ChatObservationDispatch::Gap(gap) => {
                    self.ui
                        .chat_baseline_primed
                        .store(false, AtomicOrdering::SeqCst);
                    // 缺口序号是内部枚举且不对外暴露 Display，这里只写原因与后果；
                    // 需要精确序号时看 observation 层的 trace。
                    log::warn!(
                        "聊天观察流出现缺口（{}），这一屏之后的观察不连续；下一屏会重建观察状态，只有粉色首屏才建立历史基线，缺口期间的消息可能不会派发",
                        gap_kind_label(gap.kind)
                    );
                }
            }
        }
        Ok(secondary)
    }

    /// 展示名（给玩家看）：显示名 → 真实昵称 → 占位「玩家」。
    ///
    /// 外部聊天源的 `from.name` 是资料包昵称、与屏幕名无关，必须用账号 UID 先解析身份。
    pub(in crate::composition::application) fn mapped_actor_name(
        &self,
        ocr_name: &str,
        account: Option<&crate::features::command::CommandAccount>,
    ) -> String {
        self.shared.live_configs.identity.player_name(
            ocr_name,
            account.map(|account| (account.game.as_str(), account.uid.as_str())),
        )
    }

    /// 记忆归属键：已登记玩家用 UUID，未登记退回显示昵称。
    pub(in crate::composition::application) fn canonical_actor_name(
        &self,
        ocr_name: &str,
    ) -> String {
        self.shared.live_configs.identity.memory_key(ocr_name)
    }

    /// 记忆键对应的展示名（BOT 称呼、面板、日志用）。
    pub(in crate::composition::application) fn actor_display_name(
        &self,
        actor_key: &str,
    ) -> String {
        self.shared
            .live_configs
            .identity
            .player_name_for_key(actor_key)
    }

    /// 歌单归属键的候选列表，按优先级：UID > 身份映射 UUID > 昵称。
    ///
    /// UID 最高是因为它能跨改名稳定定位；身份映射次之（用户显式声明同一人）；
    /// 昵称兜底。读取时依次尝试，写入时用第一个可用的。
    /// 歌单归属只认身份 UUID：没有登记身份映射的玩家没有歌单。
    ///
    /// 返回空表示"无歌单"，调用方应提示用户先配置身份映射，
    /// 而不是退化成按昵称归属（昵称会改、会重名、OCR 会读错）。
    pub(in crate::composition::application) fn playlist_owner_candidates(
        &self,
        subject: &str,
        account: Option<&miliastra_kernel::chat_source::CommandAccount>,
    ) -> Vec<String> {
        // 与权限判定同源：有 UID 就只认 UID，没有才按游戏内昵称。
        //
        // 原来这里直接调 `resolve(ocr_name)`（只按 nickname 精确匹配）。
        // 外部聊天源的私聊 peer 是**数字 UID**，传进去必然落空，
        // 已登记的玩家被判成未登记——线上表现为大厅能导入歌单、
        // 好友私聊提示「该玩家未在身份映射登记」。
        let hint = match account {
            Some(account) => crate::features::identity::IdentityHint::Account {
                game: &account.game,
                uid: &account.uid,
                nickname: Some(subject),
            },
            None => crate::features::identity::IdentityHint::Nickname(subject),
        };
        let identity = &self.shared.live_configs.identity;
        let uuid = identity.resolve_hint(hint).map(|resolved| resolved.id);
        // 兜底：`resolve_hint` 只认 nickname（那是**授权**口径，不能放宽），
        // 而外部聊天源给的发送者名字常常是 **真实昵称（玩家自设）**——
        // 已登记玩家的真实昵称与备注串是两条不同字符串，前者解析不出来，
        // 表现为「歌单窗口决策的发送者不是当前窗口属主」，`@1`~`@5` 全部失效。
        //
        // **这里只服务归属**（决定这份歌单属于谁），不是授权。
        // 归属允许匹配显示名/真实昵称（见 identity.rs 的「它们可用于归属、不可用于授权」）。
        // 绝不能把这段兜底上移到 `resolve_hint`：那会让「改昵称冒充管理员」复活。
        let uuid = uuid.or_else(|| identity.ownership_uuid_by_name(subject));
        let identity = uuid.map(|uuid| uuid.to_string());
        crate::features::playlist::owner_key::candidates(identity.as_deref())
    }

    /// 写入用的归属键：候选中优先级最高的那个。
    /// 写入用的归属键；没有身份映射时为 `None`（调用方应拒绝操作）。
    pub(in crate::composition::application) fn playlist_owner_key(
        &self,
        subject: &str,
        account: Option<&miliastra_kernel::chat_source::CommandAccount>,
    ) -> Option<String> {
        self.playlist_owner_candidates(subject, account)
            .into_iter()
            .next()
    }

    pub(in crate::composition::application) fn map_command_actor(
        &self,
        mut command: RoutedCommand,
    ) -> RoutedCommand {
        // 有账号时按 UID 反查真实名字。
        //
        // `username` 在外部聊天源下可能是 UID（私聊容器的 peer 取的是 to_uid，
        // 也就是收件人＝BOT 自己），拿去当昵称查必然落空、原样返回一串数字，
        // 于是大厅通告会念出「900000001邀请BOT前往大厅」。
        // 帐号里的 uid 才是真正的发送者。
        // 展示名走「软件显示昵称 → 真实昵称」；有账号时先按 UID 解析身份，
        // 因为外部源的 username 可能是 UID 或资料包昵称，按名字查会落空。
        let actor = self.mapped_actor_name(&command.username, command.account.as_ref());
        match &mut command.command {
            ModuleCommand::Invite(invite) => invite.display_name = actor,
            ModuleCommand::Moderation(moderation) => moderation.requester = actor,
            _ => {}
        }
        command
    }

    pub(in crate::composition::application) fn handle_scan_messages(
        &self,
        captured_at: Instant,
        observed_messages: Vec<PrimaryObservedMessage>,
    ) -> Result<()> {
        let active_entertainment = self.business.business.active_entertainment()?;
        let accepts_turtle_questions = self.business.business.turtle_soup_accepts_questions()?;
        let access_config = self.shared.live_configs.snapshot();
        let command_router = ChatCommandRouter::with_access(
            &self.business.custom_workflow,
            &self.shared.live_configs.identity,
            &access_config.command_access,
        );
        if observed_messages.is_empty() {
            log::debug!("没有找到聊天标志，本轮不更新命令锁");
            return Ok(());
        }

        let baseline_primed = self.ui.chat_baseline_primed.load(AtomicOrdering::SeqCst);
        let mut visible_turtle_questions = Vec::new();
        let mut parsed = Vec::new();
        for observed in &observed_messages {
            let message = &observed.message;
            let sample_as_question = accepts_turtle_questions
                && message.message_type == "blue"
                && !message.text.is_empty();
            let command_like = command::looks_like_command(&message.text);
            if !observed.is_new && !sample_as_question {
                if command_like && self.ui.chat_timeline.note_not_executed_log(&observed.id)? {
                    let state = self
                        .ui
                        .chat_timeline
                        .business_delivery_state_at(&observed.id, captured_at)?;
                    log::info!(
                        "命令未执行：这条消息未判为新增追加（交付状态：{}，来源：{}），通常因为它是基线历史或画面不连续「{}」",
                        delivery_state_label(state),
                        message_source_label(&message.message_type),
                        compact_for_log(&message.text)
                    );
                }
                continue;
            }
            // 这两个数在函数入口就能算，成本为零；只写主语等于没有信息。
            log::debug!(
                "处理一级观察消息：本批 {} 条，其中新增 {} 条",
                observed_messages.len(),
                observed_messages.iter().filter(|item| item.is_new).count()
            );
            if message.text.is_empty() {
                if observed.is_new {
                    acknowledge_unrecognized_primary_message(
                        &self.ui.chat_timeline,
                        observed,
                        captured_at,
                        baseline_primed,
                    )?;
                }
                continue;
            }

            let observation = CommandObservation {
                frame_id: None,
                captured_at: Some(captured_at),
                message_id: Some(observed.id.clone()),
                // 命令锁只认被派发消息自身的数据：这里连同快照一起捕获文本与锚点。
                // 锚点跟着来源走，外部消息用序号而不是哨兵坐标。
                message_text: Some(message.text.clone()),
                lock_anchor: Some(lock_anchor_for(&observed.id, message.block.y)),
            };
            // 正文已由扫描层切成纯正文，昵称在 message.sender 里。
            let envelope = command::parse_command_envelope(
                &message.text,
                &message.message_type,
                message.sender.as_deref(),
                observation,
            );
            let routed = envelope
                .as_ref()
                .and_then(|envelope| command_router.route(envelope, active_entertainment));

            // 关键词 AI 聊天：不是 @ 命令，因此在路由失败后单独判定。
            // 与命令互斥：一条消息要么是命令，要么是闲聊触发，不会被处理两次。
            if routed.is_none() && observed.is_new {
                self.enqueue_ai_chat_keyword(
                    &observed.id,
                    &message,
                    // 昵称已由扫描层从正文剥出。
                    message.sender.as_deref(),
                    None,
                    captured_at,
                    lock_anchor_for(&observed.id, message.block.y),
                )?;
            }

            if sample_as_question && routed.is_none() {
                if let Some(mut question) = turtle_soup::parse_question_message(&message.text, None)
                {
                    // 与二级大厅同一条展示链：正文里读到的是屏幕名（备注串或真实昵称），
                    // 直接渲染会把随机备注念到大厅。player_key 保留原文，匹配不受影响。
                    question.player = self.mapped_actor_name(&question.player, None);
                    visible_turtle_questions.push(question);
                }
            }
            if !observed.is_new {
                // 海龟汤提问采样开启时 sample_as_question 对所有非空蓝消息为真，会走到此处；
                // 若并非新增追加，则命令不会执行，需留痕。
                if command_like && self.ui.chat_timeline.note_not_executed_log(&observed.id)? {
                    log::info!(
                        "命令未执行：本轮正在进行海龟汤提问采样，这条消息未判为新增追加（来源：{}）「{}」",
                        message_source_label(&message.message_type),
                        compact_for_log(&message.text)
                    );
                }
                continue;
            }
            let Some(parsed_command) = routed else {
                if command_like && self.ui.chat_timeline.note_not_executed_log(&observed.id)? {
                    // 加**固定类别标签**：正文细节各不相同，类别才可被 grep 统计。
                    // 这里只有两个类别——本分支只覆盖「没解析出来」与「没模块认领」；
                    // 权限不足/去重跳过/界面不符/参数无效 各有自己的日志点，不在此处编造。
                    let (category, reason) = if envelope.is_none() {
                        (
                            "解析失败",
                            "无法解析为命令信封（发送者/前缀或结构化点歌不成立）",
                        )
                    } else {
                        (
                            "无模块认领",
                            "信封可解析但无模块认领（或保留的决策语法被正常分发排除）",
                        )
                    };
                    log::info!(
                        "命令未执行（{}）：{}（来源：{}）「{}」",
                        category,
                        reason,
                        message_source_label(&message.message_type),
                        compact_for_log(&message.text)
                    );
                }
                acknowledge_unrecognized_primary_message(
                    &self.ui.chat_timeline,
                    observed,
                    captured_at,
                    baseline_primed,
                )?;
                continue;
            };
            let parsed_command = self.map_command_actor(parsed_command);
            if let ModuleCommand::Playlist(command) = &parsed_command.command {
                let owner = self.canonical_actor_name(&parsed_command.username);
                let application = &self.business.playlist_application;
                if !application.has_command_context(&owner, command, Instant::now())? {
                    log::info!(
                        "命令未执行：歌单命令没有对应的当前窗口上下文，先打开歌单窗口再操作「{}」",
                        compact_for_log(&message.text)
                    );
                    acknowledge_unrecognized_primary_message(
                        &self.ui.chat_timeline,
                        observed,
                        captured_at,
                        baseline_primed,
                    )?;
                    continue;
                }
            }
            if let ModuleCommand::Invite(invite) = &parsed_command.command
                && !self.business.business.invite_should_accept(invite.seq)?
            {
                let seq = invite.seq.expect("unsequenced invites are always accepted");
                log::info!(
                    "邀请参数 {} 已执行过，本次跳过，不会重复发送邀请（命令：{}）",
                    seq,
                    parsed_command.raw
                );
                // 邀请去重是「这条观察已被处理」的永久结论，仍要带本帧时间：
                // 同一气泡的后续修订（例如 OCR 把它修正成另一条邀请命令）会推进
                // 修订围栏，旧确认不能让那份修订失效。
                self.ui
                    .chat_timeline
                    .acknowledge_business_at(&observed.id, captured_at)?;
                continue;
            }
            if matches!(
                parsed_command.command,
                ModuleCommand::Playlist(crate::features::playlist::PlaylistCommand::Import { .. })
            ) {
                log::debug!("解析到歌单导入命令，分享链接已隐藏不记入日志");
            } else {
                // 只打命令原文无法回答「谁发的、从哪个聊天区来的」，
                // 而这两点正是排查「为什么这条命令被执行/没被执行」的起点。
                log::debug!(
                    "解析出命令「{}」，发送者「{}」，来源：{}",
                    parsed_command.raw,
                    command_username(&parsed_command),
                    message_source_label(&message.message_type)
                );
            }
            parsed.push(parsed_command);
        }

        // 问题只按大厅画面的可见顺序做跨帧稳定采样；不与命令互相排序或回映射。
        let questions = self
            .business
            .business
            .filter_turtle_soup_primary_questions(visible_turtle_questions, false)?;
        self.ui
            .chat_baseline_primed
            .store(true, AtomicOrdering::SeqCst);

        for routed in parsed {
            let gate = self.command_gate(&routed)?;
            if gate.is_blocked() {
                // 文案必须反映真实原因：这里既可能是全局命令开关关闭，
                // 也可能是该命令的最低角色/单条启用策略拒绝。写成「识别已禁用」
                // 会把人引去查全局开关，而实际原因是权限。
                log::info!("命令未执行：{}（命令：{}）", gate.log_reason(), routed.raw);
                // 命令在这里就被丢掉，走不到 execute_command，所以提示只能在这里出。
                self.notify_gate_denied(&routed, gate)?;
                // 被禁用不是"执行成功"，只确认不写锁。
                acknowledge_primary_command(&self.ui.chat_timeline, &routed, false)?;
                continue;
            }
            if self.enqueue_chat_listener_command(&routed)? {
                // 真正派发成功：写锁作为持久墓碑，重建后同一屏命令不再派发。
                acknowledge_primary_command(&self.ui.chat_timeline, &routed, true)?;
                continue;
            }
            if self.apply_immediate_administration(&routed, false)? {
                acknowledge_primary_command(&self.ui.chat_timeline, &routed, true)?;
                continue;
            }
            let pending = PendingCommand {
                lock_key: command::lock_key(&routed),
                routed: routed.clone(),
            };
            self.enqueue_pending_command(pending)?;
            acknowledge_primary_command(&self.ui.chat_timeline, &routed, true)?;
        }
        if self.commands_enabled()? {
            for question in questions {
                self.enqueue_turtle_soup_question(question, captured_at)?;
            }
        }
        Ok(())
    }

    /// 关键词触发的 AI 聊天：把命中的闲聊转成一次 `AiChatCommand::Ask`。
    ///
    /// 不是命令：不写成功命令锁、不参与命令路由；只受冷却与开关约束。
    pub(in crate::composition::application) fn enqueue_ai_chat_keyword(
        &self,
        message_id: &crate::observation::chat::ObservedChatMessageId,
        message: &crate::observation::chat::scan::ChatMessage,
        sender: Option<&str>,
        account: Option<&miliastra_kernel::chat_source::CommandAccount>,
        captured_at: Instant,
        lock_anchor: crate::observation::chat::LockAnchor,
    ) -> Result<()> {
        let config = self.shared.live_configs.snapshot().ai_chat.clone();
        // 大厅消息没有独立 sender 字段，昵称在文本行首，与命令信封共用同一套切分。
        //
        // 拆不出昵称时退回账号 UID：外部聊天源的昵称可能为 null（API 文档 §5），
        // 但 UID 必填。只按昵称判断会让这类消息整条静默失效——它不是 Unknown，
        // 只是缺一个展示名。正文此时本来就是纯正文（投递层没补前缀）。
        // 发送者来源按可信度排序：显式 sender（外部源/二级面板）> 正文行首（一级 OCR）
        // > 账号 UID（昵称缺失时的兜底）。
        let split = sender
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| (name.to_string(), message.text.as_str()))
            .or_else(|| {
                crate::interfaces::chat::split_sender_line(&message.text, &message.message_type)
            })
            .or_else(|| account.map(|account| (account.uid.clone(), message.text.as_str())));
        // 全部门槛收敛在 features::ai_chat::keyword_trigger：原来每个不满足的分支都
        // 直接 return Ok(()) 且不打日志，用户只看到「完全静默」。收敛成一条不可
        // 静默的日志，也避免以后新增门槛时又悄悄吞掉触发。
        let (sender, question) = match crate::features::ai_chat::keyword_trigger(
            &config,
            self.business.ai.enabled(),
            &message.message_type,
            split,
        ) {
            crate::features::ai_chat::KeywordTrigger::Ask { sender, question } => {
                // 权限判定放在触发之后：先确认这句话确实是在叫机器人，
                // 再判断叫的人有没有资格，避免把普通闲聊记成「越权」。
                let role = self
                    .shared
                    .live_configs
                    .identity
                    .role_of_sender(&sender, account);
                if !config.min_access.allows(role) {
                    // role=None 有两种成因：身份没登记，或这条消息根本没带账号
                    // （于是只按昵称查，随机 nickname 查不到）。日志必须能区分这两者，
                    // 但**不能打印 UID 本身**——那是真实玩家标识（MUST_READ §9）。
                    log::info!(
                        "AI 聊天未触发：需要{}权限才能触发，发送者「{}」当前身份为{}，本条消息{}",
                        config.min_access.label(),
                        sender,
                        role.map(actor_role_label).unwrap_or("未登记"),
                        if account.is_some() {
                            "带账号（按账号查身份）"
                        } else {
                            "不带账号（只能按昵称查身份）"
                        }
                    );
                    return Ok(());
                }
                (sender, question)
            }
            crate::features::ai_chat::KeywordTrigger::Skip(skip) => {
                // 用 info 而不是 debug：logging.level 默认 info（LoggingConfig::default），
                // debug 级日志在默认配置下根本不落盘，排查时还是「什么都看不到」。
                // 只在真正收到聊天消息时打印，不会淹没日志。
                log::info!(
                    "AI 聊天未触发：{}（消息来源：{}）「{}」",
                    skip.reason(),
                    message_source_label(&message.message_type),
                    compact_for_log(&message.text)
                );
                return Ok(());
            }
        };
        log::info!(
            "收到 AI 聊天提问，提问者「{}」，问题「{}」，开始生成回答",
            sender,
            compact_for_log(&question)
        );
        let mut routed = RoutedCommand::console(
            &config.keyword,
            &message.text,
            ModuleCommand::AiChat(crate::features::ai_chat::AiChatCommand::Ask { question }),
        );
        routed.username = sender.to_string();
        routed.message_type = message.message_type.clone();
        routed.authority = CommandAuthority::HallMember;
        // 身份必须与触发期判定**同源**。
        //
        // 触发期用的是 `role_of_sender(sender, account)`——有 UID 就按 UID 判；
        // 而 `RoutedCommand::console` 构造出的 `account` 恒为 None，
        // 如果这里只回填 `role_of(&sender)`（仅按昵称），
        // 那么「UID 认出 Owner、昵称却是随机 OCR 备注名」的发送者会
        // 触发期放行、执行期 role=None，命令被静默丢弃。
        routed.account = account.cloned();
        routed.role = self
            .shared
            .live_configs
            .identity
            .role_of_sender(&sender, routed.account.as_ref());
        routed.observation = crate::features::command::CommandObservation {
            frame_id: None,
            captured_at: Some(captured_at),
            message_id: Some(message_id.clone()),
            message_text: Some(message.text.clone()),
            lock_anchor: Some(lock_anchor),
        };
        let pending = PendingCommand {
            lock_key: crate::interfaces::chat::lock_key(&routed),
            routed,
        };
        self.enqueue_pending_command(pending)?;
        Ok(())
    }

    /// 全局命令开关只约束「大厅普通成员」；好友与已映射角色不受影响。
    ///
    /// AI 转述的命令也必须过这一关，否则管理员关掉命令后，
    /// 真人 `@点歌` 不执行、AI 转述的同一条却照跑。
    ///
    /// 返回 `bool` 只回答「挡不挡」，调用方无法据此给出正确文案。
    /// 需要文案的拦截点请用 [`Self::command_gate`]。
    pub(in crate::composition::application) fn command_is_disabled(
        &self,
        command: &RoutedCommand,
    ) -> Result<bool> {
        Ok(!matches!(self.command_gate(command)?, CommandGate::Allowed))
    }

    /// [`Self::command_is_disabled`] 的结构化版本：**为什么**被挡。
    ///
    /// 这里才是命令真正被丢弃的地方（入队/派发之前）。旧实现只返回 bool，
    /// 于是「面板关掉了命令」和「档位不够」在拦截点无法区分——玩家什么提示
    /// 都收不到（命令在进 `execute_command` 之前就被丢掉），而曾写在
    /// `execute_command` 里的文案分支因此**永远不可达**。
    pub(in crate::composition::application) fn command_gate(
        &self,
        command: &RoutedCommand,
    ) -> Result<CommandGate> {
        // 控制台来源仅由已鉴权HTTP/内部任务构造，游戏颜色解析不能创建此来源。
        if command.message_type == "控制台" {
            return Ok(CommandGate::Allowed);
        }
        let snapshot = self.shared.live_configs.snapshot();
        let (_, policy) = crate::features::command_access::resolve_with_active(
            &snapshot.command_access,
            command,
            self.business.business.active_entertainment()?,
        );
        // 与路由期同一判定：有账号按 UID，否则按昵称。只按昵称会让外部源的
        // 管理员命令「路由放行、执行拒绝」——昵称是随机 OCR 匹配键，查不到 UID 绑定。
        let role = self
            .shared
            .live_configs
            .identity
            .role_of_sender(&command.username, command.account.as_ref());
        // 与路由期同一口径：身份命中**覆盖**，未命中退回来源下界
        // （粉色好友消息 / 大厅好友标记 → Friend，蓝色大厅 → User）。
        let actor =
            crate::features::command_access::actor_permission_with_floor(role, command.authority);
        Ok(CommandGate::from_policy(
            &policy,
            actor,
            command.authority,
            role.is_some(),
            self.commands_enabled()?,
        ))
    }

    /// 命令在拦截点被挡下时，给玩家一句**说明原因**的提示。
    ///
    /// 纪律：
    /// - 只对像命令的消息回（\`looks_like_command\`），闲聊不回复，避免刷屏；
    /// - 文案按 [\`CommandGate\`] 结构化产出，不靠字符串猜原因；
    /// - 回复失败不能影响「命令已被丢弃」这个事实，因此只记日志不上抛。
    pub(in crate::composition::application) fn notify_gate_denied(
        &self,
        routed: &RoutedCommand,
        gate: CommandGate,
    ) -> Result<()> {
        // 用 user_command（带 @/# 前缀）而不是 raw：raw 已经剥掉前缀
        // （例如 @删除 的 raw 是「删除」），拿它喂 looks_like_command 会恒为 false，
        // 于是提示永远发不出去——正是「代码正确但玩家看不到」的复发。
        let Some(message) = gate_notice(gate, &routed.user_command) else {
            return Ok(());
        };
        if let Err(error) = self.reply(message) {
            log::warn!(
                "向玩家说明命令被拒原因失败（原因：{}），命令仍不执行，玩家这次看不到提示：{error:#}",
                gate.log_reason()
            );
        }
        Ok(())
    }

    pub(in crate::composition::application) fn enqueue_chat_listener_command(
        &self,
        parsed: &RoutedCommand,
    ) -> Result<bool> {
        let Some(command) = chat_listener_mode_dispatch(parsed) else {
            return Ok(false);
        };
        match command {
            ChatListenerModeCommand::Status => {
                let snapshot = self.business.business.chat_listener_snapshot()?;
                let pending = snapshot
                    .pending_mode
                    .map(|mode| format!("，等待切换{}", mode.label()))
                    .unwrap_or_default();
                // 监控事件流与日志共用同一句。已确认消费方只做原样显示：
                // TUI 的 draw_commands 逐行 Line::from(command.as_str())，
                // HTTP 的 commands 只走 map_text_collection 做身份名替换，
                // 两者都不解析这段文本，因此统一措辞是安全的。
                let message = format!("值守模式查询：当前为{}{}", snapshot.mode.label(), pending);
                log::info!("「{}」{}", parsed.username, message);
                self.shared.monitor.publish(MonitorEvent::Command(format!(
                    "{} -> {}",
                    parsed.user_command, message
                )));
                crate::debug_read_only::record_command_immediate(
                    parsed,
                    "chatListenerStatus",
                    message,
                );
            }
            ChatListenerModeCommand::Primary | ChatListenerModeCommand::Secondary => {
                let target = match command {
                    ChatListenerModeCommand::Primary => ChatListenerMode::Primary,
                    ChatListenerModeCommand::Secondary => ChatListenerMode::Secondary,
                    ChatListenerModeCommand::Status => unreachable!(),
                };
                let (queued, snapshot) =
                    self.business.business.request_chat_listener_mode(target)?;
                if !queued {
                    log::debug!(
                        "值守模式切换请求被跳过：当前已是{}，或已有待处理的切换请求「{}」",
                        snapshot.mode.label(),
                        snapshot
                            .pending_mode
                            .map(|mode| mode.label())
                            .unwrap_or("无")
                    );
                    return Ok(true);
                }
                self.record_command_activity(command_observed_at(parsed))?;
                if let Err(error) = self.push_pending_task_with_command_source(
                    PendingTask::SetChatListenerMode { target },
                    parsed,
                ) {
                    self.business
                        .business
                        .cancel_chat_listener_mode_request(target)?;
                    return Err(error);
                }
                // 补当前模式与触发者：只看目标模式无法判断「这次切换到底改了什么、谁要求的」。
                // snapshot 是 request_chat_listener_mode 返回的**切换前**快照。
                log::info!(
                    "值守模式切换已加入待处理队列：{} → {}（触发者「{}」）",
                    snapshot.mode.label(),
                    target.label(),
                    command_username(parsed)
                );
            }
        }
        Ok(true)
    }

    pub(in crate::composition::application) fn enqueue_turtle_soup_question(
        &self,
        question: turtle_soup::TurtleSoupQuestion,
        observed_at: Instant,
    ) -> Result<()> {
        self.record_command_activity(observed_at)?;
        log::info!(
            "海龟汤提问已加入正式输入队列，提问者「{}」",
            question.player
        );
        self.push_pending_task(PendingTask::TurtleSoupQuestion {
            question: Box::new(question),
            observed_at,
        })
    }

    pub(in crate::composition::application) fn abort_entertainment_for_context_loss(
        &self,
        reason: &str,
    ) {
        if let Err(error) = self.business.business.abort_turtle_soup(reason) {
            log::error!("无法中止海龟汤会话：{error:#}");
        }
        match self.business.undercover_game.abort() {
            Ok(true) => log::warn!("谁是卧底已因聊天上下文变化中止：{}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧谁是卧底牌局：{error:#}"),
        }
        match self.business.card_games.abort() {
            Ok(true) => log::warn!("牌局已因聊天上下文变化中止：{}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧牌局：{error:#}"),
        }
        match self.business.business.abort_idiom_chain() {
            Ok(true) => log::warn!("成语接龙已因聊天上下文变化中止：{}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧成语接龙会话：{error:#}"),
        }
        match self.business.business.abort_number_bomb() {
            Ok(true) => log::warn!("数字炸弹已因聊天上下文变化中止：{}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧数字炸弹会话：{error:#}"),
        }
    }

    /// 二级/好友命令的派发入口。返回值是**是否真正作为命令派发成功**——只有 true 才允许
    /// 写命令锁；被禁用、邀请去重、未路由都返回 false（只 ack）。
    pub(in crate::composition::application) fn submit_secondary_command(
        &self,
        parsed: RoutedCommand,
    ) -> Result<bool> {
        let parsed = self.map_command_actor(parsed);
        if self.enqueue_chat_listener_command(&parsed)? {
            return Ok(true);
        }
        let gate = self.command_gate(&parsed)?;
        if gate.is_blocked() {
            // 同上一级：区分「全局开关」「面板关闭」与「该命令权限不够」，
            // 否则会误导排查方向；文案也只能在这里出（命令不会进 execute_command）。
            log::info!(
                "命令未执行：{}（二级命令：{}）",
                gate.log_reason(),
                parsed.raw
            );
            self.notify_gate_denied(&parsed, gate)?;
            return Ok(false);
        }
        if let ModuleCommand::Invite(invite) = &parsed.command
            && !self.business.business.invite_should_accept(invite.seq)?
        {
            let seq = invite.seq.expect("unsequenced invites are always accepted");
            log::info!(
                "邀请参数 {} 已执行过，本次跳过，不会重复发送邀请（命令：{}）",
                seq,
                parsed.raw
            );
            return Ok(false);
        }
        if self.apply_immediate_administration(&parsed, true)? {
            return Ok(true);
        }
        self.enqueue_pending_command(PendingCommand {
            lock_key: command::lock_key(&parsed),
            routed: parsed,
        })?;
        Ok(true)
    }

    pub(in crate::composition::application) fn enqueue_pending_command(
        &self,
        pending: PendingCommand,
    ) -> Result<()> {
        // 可信管理员拉黑先接管待决租约，再进入正式队列；旧结果不能通过去重阻塞它。
        if self.enqueue_direct_blacklist(&pending.routed)? {
            self.record_command_activity(command_observed_at(&pending.routed))?;
            return Ok(());
        }
        if self.pending_contains_command(&pending.routed)? {
            log::info!(
                "命令已在待处理队列，本轮跳过避免重复入队（命令：{}）",
                pending.routed.raw
            );
            return Ok(());
        }
        self.record_command_activity(command_observed_at(&pending.routed))?;
        log::info!("命令已加入待处理队列，等待执行：{}", pending.routed.raw);
        self.push_pending_task(PendingTask::Command(Box::new(pending)))
    }

    pub(in crate::composition::application) fn apply_immediate_administration(
        &self,
        parsed: &RoutedCommand,
        propagate_log_error: bool,
    ) -> Result<bool> {
        if parsed.permission_required.is_some() {
            return Ok(false);
        }
        let ModuleCommand::Administration(command) = &parsed.command else {
            return Ok(false);
        };
        let context = AdministrationCommandContext {
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
            user_command: parsed.user_command.clone(),
        };
        let mut port = self.immediate_administration_port();
        let handled = matches!(
            self.business.administration_application.apply_immediate(
                &context,
                command,
                propagate_log_error,
                &mut port,
            )?,
            ImmediateAdministrationOutcome::Handled
        );
        if handled {
            crate::debug_read_only::record_command_immediate(
                parsed,
                "immediateAdministration",
                "immediate administration completed".to_string(),
            );
        }
        Ok(handled)
    }

    pub(in crate::composition::application) fn execute_console_chat_task(
        &self,
        text: String,
        prefix: String,
    ) -> Result<()> {
        let message = format!("{}{}", prefix, text);
        self.reply(&message)
    }
}
