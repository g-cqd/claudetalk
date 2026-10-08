//! Agora domain: roles, card lifecycle, and the transitions that guard it.
//!
//! Card state is derived from an append-only event log; this module only
//! decides whether a transition is legal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Observer,
    Worker,
    Supervisor,
    Controller,
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    Backlog,
    Ready,
    Claimed,
    Doing,
    Review,
    Done,
    Blocked,
    Cancelled,
}

impl CardState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Doing => "doing",
            Self::Review => "review",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "backlog" => Self::Backlog,
            "ready" => Self::Ready,
            "claimed" => Self::Claimed,
            "doing" => Self::Doing,
            "review" => Self::Review,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("illegal transition {from:?} -> {to:?}")]
    Illegal { from: CardState, to: CardState },
    #[error("role {0:?} may not perform this transition")]
    Forbidden(Role),
    #[error("done requires evidence")]
    EvidenceRequired,
    #[error("done requires approval by a reviewer other than the owner")]
    ReviewRequired,
}

/// Context a transition is judged against.
#[derive(Debug, Default, Clone, Copy)]
pub struct Gate {
    pub has_evidence: bool,
    /// An approval exists from someone who is not the card owner.
    pub independent_approval: bool,
    pub require_review: bool,
}

/// Validate `from -> to` for an actor with `role`.
pub fn check_transition(
    from: CardState,
    to: CardState,
    role: Role,
    gate: Gate,
) -> Result<(), TransitionError> {
    use CardState::*;
    let legal = matches!(
        (from, to),
        (Backlog, Ready)
            | (Ready, Claimed)
            | (Claimed, Doing)
            | (Claimed, Ready)
            | (Doing, Review)
            | (Doing, Done)
            | (Review, Doing)
            | (Review, Done)
            | (Blocked, Ready)
            | (Backlog | Ready | Claimed | Doing | Review, Blocked)
            | (Backlog | Ready | Claimed | Doing | Review | Blocked, Cancelled)
    );
    if !legal {
        return Err(TransitionError::Illegal { from, to });
    }
    // Promoting to ready and cancelling are planning decisions.
    if matches!(to, Ready) && from == Backlog || to == Cancelled {
        if role < Role::Supervisor {
            return Err(TransitionError::Forbidden(role));
        }
    } else if role < Role::Worker {
        return Err(TransitionError::Forbidden(role));
    }
    if to == Done {
        if !gate.has_evidence {
            return Err(TransitionError::EvidenceRequired);
        }
        if gate.require_review && !gate.independent_approval {
            return Err(TransitionError::ReviewRequired);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: Gate = Gate { has_evidence: true, independent_approval: true, require_review: true };

    #[test]
    fn happy_path() {
        assert!(check_transition(CardState::Ready, CardState::Claimed, Role::Worker, OK).is_ok());
        assert!(check_transition(CardState::Review, CardState::Done, Role::Worker, OK).is_ok());
    }

    #[test]
    fn observers_cannot_move_cards() {
        assert_eq!(
            check_transition(CardState::Ready, CardState::Claimed, Role::Observer, OK),
            Err(TransitionError::Forbidden(Role::Observer))
        );
    }

    #[test]
    fn promoting_and_cancelling_need_supervisor() {
        assert!(check_transition(CardState::Backlog, CardState::Ready, Role::Worker, OK).is_err());
        assert!(check_transition(CardState::Backlog, CardState::Ready, Role::Supervisor, OK).is_ok());
        assert!(check_transition(CardState::Doing, CardState::Cancelled, Role::Worker, OK).is_err());
    }

    #[test]
    fn done_is_gated() {
        let no_ev = Gate { has_evidence: false, ..OK };
        assert_eq!(
            check_transition(CardState::Review, CardState::Done, Role::Worker, no_ev),
            Err(TransitionError::EvidenceRequired)
        );
        let no_rev = Gate { independent_approval: false, ..OK };
        assert_eq!(
            check_transition(CardState::Review, CardState::Done, Role::Worker, no_rev),
            Err(TransitionError::ReviewRequired)
        );
    }

    #[test]
    fn terminal_states_are_final() {
        assert!(check_transition(CardState::Done, CardState::Doing, Role::Owner, OK).is_err());
        assert!(check_transition(CardState::Cancelled, CardState::Ready, Role::Owner, OK).is_err());
    }

    #[test]
    fn state_names_round_trip() {
        for s in [CardState::Backlog, CardState::Ready, CardState::Claimed, CardState::Doing,
                  CardState::Review, CardState::Done, CardState::Blocked, CardState::Cancelled] {
            assert_eq!(CardState::parse(s.as_str()), Some(s));
        }
    }
}
