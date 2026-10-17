use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use miliastra_kernel::clock::Clock;
use miliastra_kernel::identity::{BusinessOperationIdAllocator, SessionGeneration};
use miliastra_kernel::timer::TimerRuntimeHandle;

use crate::features::card_games::{CardGameService, CardGameTimedOutcome};
use crate::features::entertainment::{EntertainmentKind, EntertainmentState};
use crate::features::idiom_chain::{IdiomChainCommand, IdiomChainOutcome, IdiomChainService};
use crate::features::invite::{InviteRequest, InviteService, InviteStart};
use crate::features::turtle_soup::{TurtleSoupAiCompletion, TurtleSoupService, TurtleSoupSnapshot};
use crate::features::undercover::{
    UndercoverRuntimeService, UndercoverSnapshot, UndercoverTimedOutcome,
};
use crate::runtime::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
use crate::runtime::task_engine::BusinessTaskPort;

use super::{
    ActiveCardGameDeadline, ActiveIdiomDeadline, ActiveTurtleSoupDeadline,
    ActiveUndercoverDeadline, BusinessRuntimeError, BusinessStateSink, CardGameRuntimeMessage,
    TurtleSoupHandlerContext, TurtleSoupRuntimeMessage, UndercoverRuntimeMessage,
    abort_business_modules, handle_business_timer, handle_card_game_message,
    handle_turtle_soup_message, handle_undercover_message, idiom_chain_operation_failed,
    publish_business_state, sync_idiom_deadline, sync_turtle_soup_deadline,
};

/// 持有全部娱乐领域服务、互斥状态、期限关联与延迟结果。
///
/// 该组件只在 business actor 线程内使用，不引入额外锁或工作线程。
pub(super) struct EntertainmentRuntimeState {
    entertainment: EntertainmentState,
    idiom_chain: IdiomChainService,
    card_games: CardGameService,
    undercover: UndercoverRuntimeService,
    turtle_soup: Option<TurtleSoupService>,
    invite: InviteService,
    active_idiom_deadline: Option<ActiveIdiomDeadline>,
    active_card_game_deadline: Option<ActiveCardGameDeadline>,
    active_undercover_deadline: Option<ActiveUndercoverDeadline>,
    active_turtle_soup_deadline: Option<ActiveTurtleSoupDeadline>,
    pending_card_game_cancellations: Vec<ActiveCardGameDeadline>,
    pending_undercover_cancellations: Vec<ActiveUndercoverDeadline>,
    pending_turtle_soup_cancellations: Vec<ActiveTurtleSoupDeadline>,
    pending_card_game_outcomes: VecDeque<CardGameTimedOutcome>,
    pending_undercover_outcomes: VecDeque<UndercoverTimedOutcome>,
    entertainment_clock_active: bool,
    operation_ids: BusinessOperationIdAllocator,
    session_generation: SessionGeneration,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
}

impl EntertainmentRuntimeState {
    pub(super) fn new(
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        undercover: UndercoverRuntimeService,
        turtle_soup: Option<TurtleSoupService>,
        invite: InviteService,
        state_sink: Option<Arc<dyn BusinessStateSink>>,
        clock: &dyn Clock,
    ) -> Self {
        let state = Self {
            entertainment: EntertainmentState::new(),
            idiom_chain,
            card_games,
            undercover,
            turtle_soup,
            invite,
            active_idiom_deadline: None,
            active_card_game_deadline: None,
            active_undercover_deadline: None,
            active_turtle_soup_deadline: None,
            pending_card_game_cancellations: Vec::new(),
            pending_undercover_cancellations: Vec::new(),
            pending_turtle_soup_cancellations: Vec::new(),
            pending_card_game_outcomes: VecDeque::new(),
            pending_undercover_outcomes: VecDeque::new(),
            entertainment_clock_active: true,
            operation_ids: BusinessOperationIdAllocator::new(),
            session_generation: SessionGeneration::INITIAL,
            state_sink,
        };
        state.publish(clock);
        state
    }

    pub(super) fn active(&self) -> Option<EntertainmentKind> {
        self.entertainment.active()
    }

    pub(super) fn publish(&self, clock: &dyn Clock) {
        publish_business_state(
            &self.state_sink,
            self.turtle_soup.as_ref(),
            &self.undercover,
            clock,
        );
    }

    pub(super) fn undercover_snapshot(&self, now: Instant) -> UndercoverSnapshot {
        self.undercover.snapshot(now)
    }

    pub(super) fn turtle_soup_snapshot(&self) -> Result<TurtleSoupSnapshot, BusinessRuntimeError> {
        self.turtle_soup
            .as_ref()
            .map(TurtleSoupService::snapshot)
            .ok_or(BusinessRuntimeError::RuntimeStopped)
    }

    pub(super) fn invite_should_accept(&self, sequence: Option<u32>) -> bool {
        self.invite.should_accept(sequence)
    }

    pub(super) fn begin_invite(&mut self, request: InviteRequest) -> InviteStart {
        self.invite.begin(request)
    }

    pub(super) fn handle_timer(
        &mut self,
        event: BusinessDeadlineEvent,
        task_port: &mut BusinessTaskPort,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        clock: &dyn Clock,
    ) -> Result<(), BusinessRuntimeError> {
        handle_business_timer(
            event,
            &mut self.entertainment,
            &mut self.idiom_chain,
            &mut self.card_games,
            &mut self.undercover,
            self.turtle_soup.as_mut(),
            task_port,
            timer,
            &mut self.active_idiom_deadline,
            &mut self.active_card_game_deadline,
            &mut self.active_undercover_deadline,
            &mut self.active_turtle_soup_deadline,
            &mut self.pending_card_game_cancellations,
            &mut self.pending_undercover_cancellations,
            &mut self.pending_turtle_soup_cancellations,
            &mut self.pending_card_game_outcomes,
            &mut self.pending_undercover_outcomes,
            &self.operation_ids,
            &mut self.session_generation,
            self.entertainment_clock_active,
            clock,
        )
    }

    pub(super) fn apply_turtle_soup_ai_completion(
        &mut self,
        completion: TurtleSoupAiCompletion,
        task_port: &mut BusinessTaskPort,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        clock: &dyn Clock,
    ) -> Result<(), BusinessRuntimeError> {
        if let Some(service) = self.turtle_soup.as_mut() {
            service.apply_ai_completion(&mut self.entertainment, completion, task_port);
        }
        self.sync_turtle_soup(timer, clock.now(), self.entertainment_clock_active)
    }

    pub(super) fn handle_idiom(
        &mut self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        let outcome = self
            .idiom_chain
            .handle_at(&mut self.entertainment, player, command, observed_at)
            .map_err(idiom_chain_operation_failed)?;
        self.sync_idiom(timer)?;
        Ok(outcome)
    }

    pub(super) fn explain_idiom(
        &mut self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        let outcome = self
            .idiom_chain
            .explain_at(player, command, observed_at)
            .map_err(idiom_chain_operation_failed)?;
        self.sync_idiom(timer)?;
        Ok(outcome)
    }

    pub(super) fn abort_idiom(
        &mut self,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    ) -> Result<bool, BusinessRuntimeError> {
        let aborted = self
            .idiom_chain
            .abort(&mut self.entertainment)
            .map_err(idiom_chain_operation_failed)?;
        self.sync_idiom(timer)?;
        Ok(aborted)
    }

    pub(super) fn expire_idiom(
        &mut self,
        now: Instant,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    ) -> Result<bool, BusinessRuntimeError> {
        let expired = self
            .idiom_chain
            .expire_idle_at(&mut self.entertainment, now)
            .map_err(idiom_chain_operation_failed)?;
        self.sync_idiom(timer)?;
        Ok(expired)
    }

    pub(super) fn handle_card_game(
        &mut self,
        message: CardGameRuntimeMessage,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        clock: &dyn Clock,
    ) -> Result<(), BusinessRuntimeError> {
        handle_card_game_message(
            &mut self.card_games,
            &mut self.entertainment,
            message,
            timer,
            &mut self.active_card_game_deadline,
            &mut self.pending_card_game_cancellations,
            &self.operation_ids,
            &mut self.pending_card_game_outcomes,
            &mut self.entertainment_clock_active,
            clock,
        )
    }

    pub(super) fn handle_undercover(
        &mut self,
        message: UndercoverRuntimeMessage,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        clock: &dyn Clock,
    ) -> Result<(), BusinessRuntimeError> {
        handle_undercover_message(
            &mut self.undercover,
            &mut self.entertainment,
            message,
            timer,
            &mut self.active_undercover_deadline,
            &mut self.pending_undercover_cancellations,
            &self.operation_ids,
            &mut self.pending_undercover_outcomes,
            &mut self.entertainment_clock_active,
            clock,
        )
    }

    pub(super) fn handle_turtle_soup(
        &mut self,
        message: TurtleSoupRuntimeMessage,
        task_port: &mut BusinessTaskPort,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        clock: &dyn Clock,
    ) -> Result<(), BusinessRuntimeError> {
        handle_turtle_soup_message(
            TurtleSoupHandlerContext {
                turtle_soup: self.turtle_soup.as_mut(),
                entertainment: &mut self.entertainment,
                task_port,
                timer,
                active_deadline: &mut self.active_turtle_soup_deadline,
                pending_cancellations: &mut self.pending_turtle_soup_cancellations,
                operation_ids: &self.operation_ids,
                clock_active: self.entertainment_clock_active,
                clock,
            },
            message,
        )
    }

    pub(super) fn refresh_turtle_soup(
        &mut self,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        now: Instant,
        clock_active: bool,
    ) -> Result<(), BusinessRuntimeError> {
        self.sync_turtle_soup(timer, now, clock_active)
    }

    pub(super) fn abort_all(
        &mut self,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        clock: &dyn Clock,
    ) {
        abort_business_modules(
            &mut self.entertainment,
            &mut self.idiom_chain,
            &mut self.card_games,
            &mut self.undercover,
            self.turtle_soup.as_mut(),
            timer,
            &mut self.active_idiom_deadline,
            &mut self.active_card_game_deadline,
            &mut self.active_undercover_deadline,
            &mut self.active_turtle_soup_deadline,
            &mut self.pending_card_game_cancellations,
            &mut self.pending_undercover_cancellations,
            &mut self.pending_turtle_soup_cancellations,
            &self.operation_ids,
            &mut self.session_generation,
            &mut self.pending_card_game_outcomes,
            &mut self.pending_undercover_outcomes,
            self.entertainment_clock_active,
            clock,
        );
    }

    fn sync_idiom(
        &mut self,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    ) -> Result<(), BusinessRuntimeError> {
        sync_idiom_deadline(
            &self.idiom_chain,
            timer,
            &mut self.active_idiom_deadline,
            &self.operation_ids,
            &mut self.session_generation,
        )
    }

    fn sync_turtle_soup(
        &mut self,
        timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
        now: Instant,
        clock_active: bool,
    ) -> Result<(), BusinessRuntimeError> {
        sync_turtle_soup_deadline(
            self.turtle_soup.as_ref(),
            timer,
            &mut self.active_turtle_soup_deadline,
            &mut self.pending_turtle_soup_cancellations,
            &self.operation_ids,
            now,
            clock_active,
        )
    }
}
