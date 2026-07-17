use crate::features::card_games::CardGameDeadlineToken;
use crate::features::idiom_chain::IdiomChainDeadlineToken;
use crate::features::turtle_soup::TurtleSoupDeadlineToken;
use crate::features::undercover::UndercoverDeadlineToken;

use super::timer::{DeadlineIdentity, TimerRuntimeEvent};

/// The sole timer-runtime identity. It routes only by vertical module; deadline meaning stays in
/// each module's typed token.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BusinessDeadlineToken {
    CardGame(CardGameDeadlineToken),
    Undercover(UndercoverDeadlineToken),
    TurtleSoup(TurtleSoupDeadlineToken),
    IdiomChain(IdiomChainDeadlineToken),
}

impl DeadlineIdentity for BusinessDeadlineToken {
    fn module_name(&self) -> &'static str {
        match self {
            Self::CardGame(token) => token.module_name(),
            Self::Undercover(token) => token.module_name(),
            Self::TurtleSoup(token) => token.module_name(),
            Self::IdiomChain(token) => token.module_name(),
        }
    }
}

impl From<CardGameDeadlineToken> for BusinessDeadlineToken {
    fn from(token: CardGameDeadlineToken) -> Self {
        Self::CardGame(token)
    }
}

impl From<UndercoverDeadlineToken> for BusinessDeadlineToken {
    fn from(token: UndercoverDeadlineToken) -> Self {
        Self::Undercover(token)
    }
}

impl From<TurtleSoupDeadlineToken> for BusinessDeadlineToken {
    fn from(token: TurtleSoupDeadlineToken) -> Self {
        Self::TurtleSoup(token)
    }
}

impl From<IdiomChainDeadlineToken> for BusinessDeadlineToken {
    fn from(token: IdiomChainDeadlineToken) -> Self {
        Self::IdiomChain(token)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessDeadlineEvent {
    CardGame(TimerRuntimeEvent<CardGameDeadlineToken>),
    Undercover(TimerRuntimeEvent<UndercoverDeadlineToken>),
    TurtleSoup(TimerRuntimeEvent<TurtleSoupDeadlineToken>),
    IdiomChain(TimerRuntimeEvent<IdiomChainDeadlineToken>),
}

impl From<TimerRuntimeEvent<BusinessDeadlineToken>> for BusinessDeadlineEvent {
    fn from(event: TimerRuntimeEvent<BusinessDeadlineToken>) -> Self {
        match event.token() {
            BusinessDeadlineToken::CardGame(_) => Self::CardGame(event.map_token(|token| {
                let BusinessDeadlineToken::CardGame(token) = token else {
                    unreachable!("deadline route was checked before mapping")
                };
                token
            })),
            BusinessDeadlineToken::Undercover(_) => Self::Undercover(event.map_token(|token| {
                let BusinessDeadlineToken::Undercover(token) = token else {
                    unreachable!("deadline route was checked before mapping")
                };
                token
            })),
            BusinessDeadlineToken::TurtleSoup(_) => Self::TurtleSoup(event.map_token(|token| {
                let BusinessDeadlineToken::TurtleSoup(token) = token else {
                    unreachable!("deadline route was checked before mapping")
                };
                token
            })),
            BusinessDeadlineToken::IdiomChain(_) => Self::IdiomChain(event.map_token(|token| {
                let BusinessDeadlineToken::IdiomChain(token) = token else {
                    unreachable!("deadline route was checked before mapping")
                };
                token
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::features::card_games::{CardGameDeadlineKind, CardGameDeadlineToken};
    use crate::features::idiom_chain::{IdiomChainDeadlineKind, IdiomChainDeadlineToken};
    use crate::features::turtle_soup::{TurtleSoupDeadlineKind, TurtleSoupDeadlineToken};
    use crate::features::undercover::{UndercoverDeadlineKind, UndercoverDeadlineToken};
    use crate::runtime::identity::{BusinessOperationId, SessionGeneration};
    use crate::runtime::timer::{DeadlineSchedule, TimerCore, TimerRuntimeEvent};

    #[test]
    fn top_level_token_routes_each_module_without_losing_correlation() {
        let deadline = Instant::now();
        let schedules = [
            DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    1,
                    CardGameDeadlineKind::LobbyExpiry,
                )),
                BusinessOperationId::new(11),
                SessionGeneration::new(21),
                deadline,
            ),
            DeadlineSchedule::new(
                BusinessDeadlineToken::from(UndercoverDeadlineToken::new(
                    2,
                    UndercoverDeadlineKind::PhaseIdle,
                )),
                BusinessOperationId::new(12),
                SessionGeneration::new(22),
                deadline,
            ),
            DeadlineSchedule::new(
                BusinessDeadlineToken::from(TurtleSoupDeadlineToken::new(
                    3,
                    TurtleSoupDeadlineKind::SessionMaximum,
                )),
                BusinessOperationId::new(13),
                SessionGeneration::new(23),
                deadline,
            ),
            DeadlineSchedule::new(
                BusinessDeadlineToken::from(IdiomChainDeadlineToken::new(
                    4,
                    IdiomChainDeadlineKind::SessionIdle,
                )),
                BusinessOperationId::new(14),
                SessionGeneration::new(24),
                deadline,
            ),
        ];
        let mut timer = TimerCore::new();
        for schedule in schedules {
            timer.schedule(schedule).unwrap();
        }

        let routed = timer
            .drain_expired(deadline)
            .unwrap()
            .into_iter()
            .map(TimerRuntimeEvent::DeadlineExpired)
            .map(BusinessDeadlineEvent::from)
            .collect::<Vec<_>>();

        assert!(matches!(
            &routed[0],
            BusinessDeadlineEvent::CardGame(TimerRuntimeEvent::DeadlineExpired(event))
                if event.token().id() == 1
                    && event.operation_id() == BusinessOperationId::new(11)
                    && event.session_generation() == SessionGeneration::new(21)
                    && event.deadline() == deadline
        ));
        assert!(matches!(
            &routed[1],
            BusinessDeadlineEvent::Undercover(TimerRuntimeEvent::DeadlineExpired(event))
                if event.token().id() == 2
        ));
        assert!(matches!(
            &routed[2],
            BusinessDeadlineEvent::TurtleSoup(TimerRuntimeEvent::DeadlineExpired(event))
                if event.token().id() == 3
        ));
        assert!(matches!(
            &routed[3],
            BusinessDeadlineEvent::IdiomChain(TimerRuntimeEvent::DeadlineExpired(event))
                if event.token().id() == 4
        ));
    }
}
