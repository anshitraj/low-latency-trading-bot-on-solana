//! Order lifecycle state machine (names per master prompt §4).
//!
//! Invariants enforced here and tested exhaustively:
//! - A signed transaction is persisted (`SIGNED`) before any send. Anything
//!   that may already be on the network can never move to `REJECTED`, so a
//!   user can never be told "failed, retry" while the first signature may land.
//! - `SUBMITTED` means "handed to at least one sender"; it says nothing about
//!   landing. Success requires `CONFIRMED`/`FINALIZED` observed on-chain.
//! - `FAILED_ONCHAIN` = landed with a program error (fee paid, no swap).
//! - `EXPIRED` = finalized block height passed `lastValidBlockHeight` and the
//!   signature was never observed; it can no longer land.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderState {
    Created,
    Quoted,
    /// Waiting for the user to confirm (Telegram button / Mini App sheet).
    AwaitingAuth,
    /// Transient; never has a persisted signature. On restart -> REJECTED.
    Signing,
    /// Signature + wire bytes persisted. May or may not have been sent.
    Signed,
    Submitted,
    /// First observed on-chain at `processed`.
    Observed,
    Confirmed,
    Finalized,
    FailedOnchain,
    Expired,
    /// Rejected before reaching the chain (validation, policy, quote, signing,
    /// cancellation, or every sender definitively refused). No fee paid.
    Rejected,
    /// Status could not be determined (restart while in flight, dropped
    /// observation). Must be resolved by reconciliation; never a failure.
    UnknownReconcile,
}

use OrderState::*;

impl OrderState {
    pub const ALL: [OrderState; 13] = [
        Created,
        Quoted,
        AwaitingAuth,
        Signing,
        Signed,
        Submitted,
        Observed,
        Confirmed,
        Finalized,
        FailedOnchain,
        Expired,
        Rejected,
        UnknownReconcile,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Created => "CREATED",
            Quoted => "QUOTED",
            AwaitingAuth => "AWAITING_AUTH",
            Signing => "SIGNING",
            Signed => "SIGNED",
            Submitted => "SUBMITTED",
            Observed => "OBSERVED",
            Confirmed => "CONFIRMED",
            Finalized => "FINALIZED",
            FailedOnchain => "FAILED_ONCHAIN",
            Expired => "EXPIRED",
            Rejected => "REJECTED",
            UnknownReconcile => "UNKNOWN_RECONCILE",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Finalized | FailedOnchain | Expired | Rejected)
    }

    /// A signed transaction for this order may exist on the network.
    pub fn may_be_in_flight(self) -> bool {
        matches!(
            self,
            Signed | Submitted | Observed | Confirmed | UnknownReconcile
        )
    }

    pub fn has_landed(self) -> bool {
        matches!(self, Observed | Confirmed | Finalized | FailedOnchain)
    }

    /// User may cancel only before a signature exists.
    pub fn is_cancellable(self) -> bool {
        matches!(self, Created | Quoted | AwaitingAuth)
    }

    pub fn can_transition_to(self, to: OrderState) -> bool {
        if self == to {
            return false;
        }
        match self {
            Created => matches!(to, Quoted | Rejected),
            Quoted => matches!(to, AwaitingAuth | Signing | Rejected),
            // Approval of an expired quote triggers a re-quote.
            AwaitingAuth => matches!(to, Quoted | Signing | Rejected),
            Signing => matches!(to, Signed | Rejected),
            // Rejected only when no sender accepted it ambiguously or otherwise
            // (all definitive refusals / never handed off).
            Signed => matches!(
                to,
                Submitted
                    | Rejected
                    | Expired
                    | UnknownReconcile
                    | Observed
                    | Confirmed
                    | Finalized
                    | FailedOnchain
            ),
            Submitted => matches!(
                to,
                Observed | Confirmed | Finalized | FailedOnchain | Expired | UnknownReconcile
            ),
            // Observed on an abandoned fork -> back to Submitted.
            Observed => matches!(
                to,
                Confirmed | Finalized | FailedOnchain | Submitted | Expired | UnknownReconcile
            ),
            Confirmed => matches!(to, Finalized | UnknownReconcile),
            UnknownReconcile => matches!(
                to,
                Submitted | Observed | Confirmed | Finalized | FailedOnchain | Expired
            ),
            Finalized | FailedOnchain | Expired | Rejected => false,
        }
    }

    pub fn transition(self, to: OrderState) -> Result<OrderState, TransitionError> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(TransitionError { from: self, to })
        }
    }

    /// State a restarted process must assume for a persisted state.
    pub fn on_restart(self) -> OrderState {
        match self {
            // Signing never persisted a signature; nothing could have been sent.
            Signing => Rejected,
            Signed | Submitted | Observed => UnknownReconcile,
            s => s,
        }
    }
}

impl fmt::Display for OrderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OrderState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        OrderState::ALL
            .iter()
            .copied()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| format!("unknown order state {s}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("illegal order state transition {from} -> {to}")]
pub struct TransitionError {
    pub from: OrderState,
    pub to: OrderState,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

impl Commitment {
    pub fn as_str(self) -> &'static str {
        match self {
            Commitment::Processed => "processed",
            Commitment::Confirmed => "confirmed",
            Commitment::Finalized => "finalized",
        }
    }
    pub fn landed_state(self, failed: bool) -> OrderState {
        match (self, failed) {
            (Commitment::Processed, _) => Observed,
            (_, true) => FailedOnchain,
            (Commitment::Confirmed, false) => Confirmed,
            (Commitment::Finalized, false) => Finalized,
        }
    }
}

/// User-facing text. Never says "filled" unless confirmed on-chain success.
pub fn user_status(state: OrderState) -> &'static str {
    match state {
        Created | Quoted => "preparing",
        AwaitingAuth => "awaiting your confirmation",
        Signing | Signed => "signed, sending",
        Submitted => "submitted — not yet seen on-chain",
        Observed => "landed (processed) — awaiting confirmation",
        Confirmed => "filled (confirmed)",
        Finalized => "filled (finalized)",
        FailedOnchain => "failed on-chain — network fee paid, no swap",
        Expired => "expired — never landed, no fee paid",
        Rejected => "rejected before sending — no fee paid",
        UnknownReconcile => "pending verification — do NOT retry, reconciling",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_paths() {
        for path in [
            vec![
                Created,
                Quoted,
                AwaitingAuth,
                Signing,
                Signed,
                Submitted,
                Observed,
                Confirmed,
                Finalized,
            ],
            vec![Created, Quoted, Signing, Signed, Submitted, Finalized],
            vec![
                Created,
                Quoted,
                AwaitingAuth,
                Quoted,
                AwaitingAuth,
                Signing,
                Signed,
                Submitted,
                FailedOnchain,
            ],
            vec![Signed, Submitted, Observed, Submitted, Expired],
            vec![Signed, UnknownReconcile, Confirmed, Finalized],
        ] {
            for w in path.windows(2) {
                assert!(w[0].can_transition_to(w[1]), "{} -> {}", w[0], w[1]);
            }
        }
    }

    #[test]
    fn terminal_states_are_final() {
        for t in OrderState::ALL.into_iter().filter(|s| s.is_terminal()) {
            for to in OrderState::ALL {
                assert!(!t.can_transition_to(to), "{t} -> {to} must be illegal");
            }
        }
    }

    #[test]
    fn in_flight_can_never_be_rejected_or_resigned() {
        for from in [Submitted, Observed, Confirmed, UnknownReconcile] {
            for bad in [Rejected, Signing, Signed, Quoted, AwaitingAuth, Created] {
                assert!(!from.can_transition_to(bad), "{from} -> {bad}");
            }
        }
        assert!(!Confirmed.can_transition_to(Observed));
        assert!(!Confirmed.can_transition_to(Expired));
        assert!(!Confirmed.can_transition_to(FailedOnchain));
        for s in OrderState::ALL {
            assert!(!s.can_transition_to(s));
            if s.may_be_in_flight() {
                assert!(!s.is_cancellable());
            }
        }
    }

    #[test]
    fn transition_table_size_is_pinned() {
        let n: usize = OrderState::ALL
            .iter()
            .map(|f| {
                OrderState::ALL
                    .iter()
                    .filter(|t| f.can_transition_to(**t))
                    .count()
            })
            .sum();
        assert_eq!(
            n, 38,
            "transition table changed; update tests + docs/EXECUTION_ENGINE.md"
        );
    }

    #[test]
    fn restart_mapping() {
        assert_eq!(Signing.on_restart(), Rejected);
        assert_eq!(Signed.on_restart(), UnknownReconcile);
        assert_eq!(Submitted.on_restart(), UnknownReconcile);
        assert_eq!(Confirmed.on_restart(), Confirmed);
        assert_eq!(AwaitingAuth.on_restart(), AwaitingAuth);
        for s in OrderState::ALL {
            let r = s.on_restart();
            assert!(r == s || s.can_transition_to(r), "{s} -> {r}");
        }
    }

    #[test]
    fn strings_and_status() {
        for s in OrderState::ALL {
            assert_eq!(s.as_str().parse::<OrderState>().unwrap(), s);
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
            let msg = user_status(s);
            if !matches!(s, Confirmed | Finalized) {
                assert!(!msg.contains("filled"), "{s}: {msg}");
            }
        }
        assert_eq!(Commitment::Processed.landed_state(true), Observed);
        assert_eq!(Commitment::Confirmed.landed_state(true), FailedOnchain);
        assert_eq!(Commitment::Finalized.landed_state(false), Finalized);
    }
}
