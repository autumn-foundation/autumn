//! Fencing tokens (issue #3053).
//!
//! A lease can expire while its holder still runs. The holder cannot always
//! know this in time. A fencing token lets the *resource* reject the late
//! write: each grant of a lease gets a larger token, and the resource keeps
//! the highest token it accepted.
//!
//! The model and its proofs are in `verification/lease_fencing.rs`.

/// A strictly increasing number that identifies one grant of a lease.
///
/// Each grant of a `LeaseLock` with one name gets a larger token than every
/// earlier grant with that name. The value is never
/// zero. Send it with each write, and let the resource reject a token that is
/// lower than the highest token it accepted (see [`FencingToken::admits`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FencingToken(u64);

/// The error for a value that is not a fencing token (zero or negative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidFencingToken(i64);

impl InvalidFencingToken {
    /// The value that is not a token.
    #[must_use]
    pub const fn value(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for InvalidFencingToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not a fencing token (a token is 1 or more)",
            self.0
        )
    }
}

impl std::error::Error for InvalidFencingToken {}

impl FencingToken {
    /// The token as an unsigned number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The token as a signed number, for a `BIGINT` column.
    ///
    /// A token comes from a `BIGINT` column, so it always fits.
    #[must_use]
    #[allow(
        clippy::cast_possible_wrap,
        reason = "construction bounds the value to 1..=i64::MAX"
    )]
    pub const fn as_i64(self) -> i64 {
        self.0 as i64
    }

    /// Returns `true` when a resource that stores `self` as its highest token
    /// must accept a write with `incoming`.
    ///
    /// Equal is accepted, so one holder can write more than once. Lower is a
    /// stale holder and is rejected. This is the check that
    /// `WHERE fencing_token <= $incoming` does in SQL.
    #[must_use]
    pub const fn admits(self, incoming: Self) -> bool {
        incoming.0 >= self.0
    }
}

impl TryFrom<i64> for FencingToken {
    type Error = InvalidFencingToken;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u64::try_from(value)
            .ok()
            .filter(|&v| v > 0)
            .map(Self)
            .ok_or(InvalidFencingToken(value))
    }
}

impl From<FencingToken> for i64 {
    fn from(token: FencingToken) -> Self {
        token.as_i64()
    }
}

impl std::fmt::Display for FencingToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn token_round_trips_through_i64() {
        let token = FencingToken::try_from(7_i64).expect("positive");
        assert_eq!(token.get(), 7);
        assert_eq!(token.as_i64(), 7);
        assert_eq!(i64::from(token), 7);
    }

    #[test]
    fn zero_and_negative_are_not_tokens() {
        assert!(FencingToken::try_from(0_i64).is_err());
        assert_eq!(
            FencingToken::try_from(-1_i64).map_err(InvalidFencingToken::value),
            Err(-1)
        );
    }

    #[test]
    fn tokens_order_by_generation() {
        let older = FencingToken::try_from(3_i64).expect("token");
        let newer = FencingToken::try_from(4_i64).expect("token");
        assert!(older < newer);
        assert_eq!(newer.to_string(), "4");
    }

    #[test]
    fn stored_token_admits_equal_and_newer_only() {
        let stored = FencingToken::try_from(5_i64).expect("token");
        assert!(stored.admits(stored), "the holder can write again");
        assert!(stored.admits(FencingToken::try_from(6_i64).expect("token")));
        assert!(!stored.admits(FencingToken::try_from(4_i64).expect("token")));
    }

    proptest! {
        /// A resource that applies `admits` keeps a non-decreasing highest
        /// token and never accepts a token below it.
        #[test]
        fn resource_rejects_every_stale_write(writes in prop::collection::vec(1_i64..50, 1..64)) {
            let mut highest: Option<FencingToken> = None;
            for raw in writes {
                let incoming = FencingToken::try_from(raw).expect("positive");
                let accepted = highest.is_none_or(|stored| stored.admits(incoming));
                prop_assert_eq!(accepted, highest.is_none_or(|stored| incoming >= stored));
                if accepted {
                    prop_assert!(highest.is_none_or(|stored| incoming >= stored));
                    highest = Some(incoming);
                }
            }
        }
    }
}
