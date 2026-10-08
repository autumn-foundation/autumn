//! `LeaseLock` acquire, renew, release and expiry with fencing (ADR 0015,
//! #3053).
//!
//! Two holders race for one lock name. A resource stores the last admitted
//! token. Statements (`autumn/src/lock/lease.rs`):
//!
//! - acquire: upsert that sets `generation = generation + 1` only `WHERE
//!   owner IS NULL OR expires_at <= now()`.
//! - renew: `WHERE generation = $2 AND owner IS NOT NULL AND expires_at >
//!   now()`.
//! - release: `SET owner = NULL WHERE generation = $2 AND owner IS NOT NULL`.
//! - resource write: `WHERE fencing_token <= $token` (`FencingToken::admits`).
//!
//! A holder trusts its lease until `ttl - ttl / 3` after the last send. It
//! can write at any time, also after its lease is gone (a paused process).
//! That stale write is what the fence must reject.

use stateright::{Model, Property};

/// The resource rejects a write with a token older than one it admitted.
pub const STALE_WRITE_REJECTED: &str = "a stale holder's write is rejected";
/// Each grant gets a new token, larger than all earlier tokens.
pub const TOKENS_ARE_UNIQUE: &str = "fencing tokens are unique per grant";
/// At most one holder trusts a live lease.
pub const LIVE_HOLDERS_EXCLUSIVE: &str = "at most one holder trusts its lease";
/// Non-vacuity: the resource rejects a write.
pub const STALE_WRITE_IS_TRIED: &str = "a stale write is tried";
/// Non-vacuity: a lease expires and another holder acquires it.
pub const EXPIRED_LEASE_IS_TAKEN: &str = "an expired lease is taken over";

const HOLDERS: u8 = 2;
const TTL: u8 = 3;
/// `ttl - ttl / 3`.
const TRUST: u8 = 2;
const MAX_GRANTS: u8 = 3;
const MAX_TIME: u8 = 6;

/// The protocol, or one seeded bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// The protocol as shipped.
    Correct,
    /// The resource admits every write.
    ResourceIgnoresToken,
    /// Acquire does not increment `generation`.
    AcquireKeepsGeneration,
    /// Acquire takes a live lease. Fencing must still reject stale writes.
    AcquireWhileHeld,
}

/// The `autumn_lease_locks` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Row {
    /// The `owner` column.
    pub owner: Option<u8>,
    /// The `generation` column. `0` means no row.
    pub generation: u8,
    /// The `expires_at` column.
    pub expires_at: u8,
}

/// A token that a holder kept from a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Held {
    /// The fencing token.
    pub token: u8,
    /// Ghost: the grant number, unique for each acquire.
    pub grant: u8,
    /// The holder trusts the lease while `now < trusted_until`.
    pub trusted_until: u8,
}

/// The global state.
#[allow(clippy::struct_excessive_bools, reason = "the ghost flags are separate facts")]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct State {
    /// The database clock.
    pub now: u8,
    /// The lock row.
    pub row: Row,
    /// The token of each holder, if any.
    pub holders: Vec<Option<Held>>,
    /// The resource's stored token, and the grant that wrote it (ghost).
    pub stored: Option<(u8, u8)>,
    /// Ghost: the number of grants.
    pub grants: u8,
    /// Ghost: the largest token given so far.
    pub max_token: u8,
    /// Ghost: the resource admitted a stale write.
    pub stale_admitted: bool,
    /// Ghost: an acquire gave a token that is not new.
    pub token_reused: bool,
    /// Ghost: the resource rejected a write.
    pub rejected: bool,
    /// Ghost: an acquire took over an expired lease.
    pub took_over: bool,
}

/// An action.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// The holder runs the acquire statement.
    Acquire(u8),
    /// The holder runs the renew statement.
    Renew(u8),
    /// The holder runs the release statement.
    Release(u8),
    /// The holder writes to the resource with its token.
    Write(u8),
    /// The clock moves forward.
    Tick,
}

/// The model.
#[derive(Debug, Clone)]
pub struct LeaseLockModel {
    variant: Variant,
}

impl LeaseLockModel {
    /// A model of `variant`.
    #[must_use]
    pub const fn new(variant: Variant) -> Self {
        Self { variant }
    }

    fn can_acquire(&self, s: &State) -> bool {
        self.variant == Variant::AcquireWhileHeld
            || s.row.owner.is_none()
            || s.row.expires_at <= s.now
    }
}

impl Model for LeaseLockModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<State> {
        vec![State {
            now: 0,
            row: Row {
                owner: None,
                generation: 0,
                expires_at: 0,
            },
            holders: vec![None; usize::from(HOLDERS)],
            stored: None,
            grants: 0,
            max_token: 0,
            stale_admitted: false,
            token_reused: false,
            rejected: false,
            took_over: false,
        }]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Action>) {
        for (h, held) in (0..HOLDERS).zip(&state.holders) {
            if state.grants < MAX_GRANTS {
                actions.push(Action::Acquire(h));
            }
            if held.is_some() {
                actions.push(Action::Renew(h));
                actions.push(Action::Release(h));
                actions.push(Action::Write(h));
            }
        }
        if state.now < MAX_TIME {
            actions.push(Action::Tick);
        }
    }

    fn next_state(&self, last: &State, action: Action) -> Option<State> {
        let mut s = last.clone();
        match action {
            Action::Acquire(h) => {
                if !self.can_acquire(&s) {
                    return None;
                }
                s.took_over |= s.row.owner.is_some_and(|owner| owner != h);
                if self.variant != Variant::AcquireKeepsGeneration || s.row.generation == 0 {
                    s.row.generation = s.row.generation.saturating_add(1);
                }
                s.row.owner = Some(h);
                s.row.expires_at = s.now.saturating_add(TTL);
                s.grants = s.grants.saturating_add(1);
                s.token_reused |= s.row.generation <= s.max_token;
                s.max_token = s.max_token.max(s.row.generation);
                s.holders[usize::from(h)] = Some(Held {
                    token: s.row.generation,
                    grant: s.grants,
                    trusted_until: s.now.saturating_add(TRUST),
                });
            }
            Action::Renew(h) => {
                let held = s.holders[usize::from(h)].as_mut()?;
                if s.row.generation == held.token
                    && s.row.owner.is_some()
                    && s.row.expires_at > s.now
                {
                    s.row.expires_at = s.now.saturating_add(TTL);
                    held.trusted_until = s.now.saturating_add(TRUST);
                } else {
                    // A renewal that matches no row marks the lease lost.
                    held.trusted_until = 0;
                }
            }
            Action::Release(h) => {
                let held = s.holders[usize::from(h)].take()?;
                if s.row.generation == held.token && s.row.owner.is_some() {
                    s.row.owner = None;
                }
            }
            Action::Write(h) => {
                let held = s.holders[usize::from(h)]?;
                let admits = self.variant == Variant::ResourceIgnoresToken
                    || s.stored.is_none_or(|(token, _)| token <= held.token);
                if admits {
                    if let Some((token, grant)) = s.stored {
                        s.stale_admitted |=
                            held.token < token || (held.token == token && held.grant != grant);
                    }
                    s.stored = Some((held.token, held.grant));
                } else {
                    s.rejected = true;
                }
                // A holder writes once per grant, which bounds the model.
                s.holders[usize::from(h)] = None;
            }
            Action::Tick => s.now = s.now.saturating_add(1),
        }
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always(STALE_WRITE_REJECTED, |_, s: &State| !s.stale_admitted),
            Property::always(TOKENS_ARE_UNIQUE, |_, s: &State| !s.token_reused),
            Property::always(LIVE_HOLDERS_EXCLUSIVE, |_, s: &State| {
                s.holders
                    .iter()
                    .flatten()
                    .filter(|held| s.now < held.trusted_until)
                    .count()
                    <= 1
            }),
            Property::sometimes(STALE_WRITE_IS_TRIED, |_, s: &State| s.rejected),
            Property::sometimes(EXPIRED_LEASE_IS_TAKEN, |_, s: &State| s.took_over),
        ]
    }
}
