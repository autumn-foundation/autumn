//! The shared demo scenario (issue #2967).
//!
//! One [`Op`] vocabulary for two searchers:
//!
//! - the proptest seed sweep (the `sim-sweep` bin), through [`ops_strategy`];
//! - the cargo-fuzz target (`fuzz/fuzz_targets/sim_ops.rs`), through
//!   [`run_fuzz_input`], which decodes bytes with [`ops_from_bytes`].
//!
//! Coverage-guided fuzzing can then find op sequences that random seeds miss,
//! and a sequence either searcher finds replays in the other:
//! [`ops_to_bytes`] encodes a shrunk sweep failure as a fuzz input.
//!
//! The scenario is a toy account: the invariant "the balance never goes
//! negative" is checked with [`always!`](crate::always) after every op.
//!
//! Unstable harness plumbing, hidden from the stable surface.

use crate::{always, sometimes};

/// The most ops one sequence holds, in both searchers.
pub const MAX_OPS: usize = 31;

/// The largest amount one op moves. The smallest is 1.
pub const MAX_AMOUNT: u32 = 99;

/// Bytes one op takes in a fuzz input: a tag and a little-endian `u16`.
const OP_BYTES: usize = 3;

/// One account operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Add the amount to the balance.
    Deposit(u32),
    /// Take the amount from the balance, but never below zero.
    Withdraw(u32),
}

/// Apply `ops` to a zero balance and check the invariant after each one.
///
/// # Panics
///
/// Panics through [`always!`](crate::always) if the balance goes negative.
pub fn apply_ops(ops: &[Op]) {
    let mut balance: i64 = 0;
    for op in ops {
        match *op {
            Op::Deposit(amount) => balance += i64::from(amount),
            Op::Withdraw(amount) => balance -= i64::from(amount).min(balance),
        }
        always!(balance >= 0, "balance went negative: {balance}");
        sometimes!(balance == 0, "balance-returned-to-zero");
    }
}

/// Decode a fuzz input into ops: 3 bytes per op, at most [`MAX_OPS`].
///
/// An even tag is a deposit and an odd tag a withdrawal. The amount is
/// `1 + (u16 % MAX_AMOUNT)`, so every input decodes to the same space the
/// proptest strategy draws from. A trailing partial op is ignored.
#[must_use]
pub fn ops_from_bytes(bytes: &[u8]) -> Vec<Op> {
    bytes
        .chunks_exact(OP_BYTES)
        .take(MAX_OPS)
        .map(|chunk| {
            let amount = 1 + u32::from(u16::from_le_bytes([chunk[1], chunk[2]])) % MAX_AMOUNT;
            if chunk[0] % 2 == 0 {
                Op::Deposit(amount)
            } else {
                Op::Withdraw(amount)
            }
        })
        .collect()
}

/// Encode ops as a fuzz input that [`ops_from_bytes`] decodes back to them.
///
/// Amounts outside `1..=MAX_AMOUNT` are not in the shared space, and do not
/// round-trip.
#[must_use]
pub fn ops_to_bytes(ops: &[Op]) -> Vec<u8> {
    ops.iter()
        .flat_map(|op| {
            let (tag, amount) = match *op {
                Op::Deposit(amount) => (0_u8, amount),
                Op::Withdraw(amount) => (1_u8, amount),
            };
            let raw = u16::try_from(amount.saturating_sub(1)).unwrap_or(u16::MAX);
            let [low, high] = raw.to_le_bytes();
            [tag, low, high]
        })
        .collect()
}

/// Run one fuzz input: the first 8 bytes seed a [`Sim`](crate::sim::Sim), as
/// one sweep seed does, and the rest are the ops.
///
/// # Panics
///
/// Panics when an op sequence breaks the invariant, which is the crash the
/// fuzzer reports.
pub fn run_fuzz_input(bytes: &[u8]) {
    let Some((seed, ops)) = bytes.split_first_chunk::<8>() else {
        return;
    };
    let _sim = crate::sim::Sim::from_seed(u64::from_le_bytes(*seed));
    apply_ops(&ops_from_bytes(ops));
}

/// The proptest strategy the sweep draws op sequences from.
#[cfg(feature = "sim-testing")]
pub fn ops_strategy() -> impl proptest::strategy::Strategy<Value = Vec<Op>> {
    proptest::collection::vec(proptest::arbitrary::any::<Op>(), 1..=MAX_OPS)
}

#[cfg(feature = "sim-testing")]
impl proptest::arbitrary::Arbitrary for Op {
    type Parameters = ();
    type Strategy = proptest::strategy::BoxedStrategy<Self>;

    fn arbitrary_with((): ()) -> Self::Strategy {
        use proptest::strategy::Strategy;
        proptest::prop_oneof![
            (1..=MAX_AMOUNT).prop_map(Op::Deposit),
            (1..=MAX_AMOUNT).prop_map(Op::Withdraw),
        ]
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_AMOUNT, MAX_OPS, Op, apply_ops, ops_from_bytes, ops_to_bytes, run_fuzz_input};

    #[test]
    fn bytes_decode_to_both_ops_in_range() {
        let ops = ops_from_bytes(&[0, 0, 0, 1, 0xff, 0xff, 2, 98, 0]);
        assert_eq!(
            ops,
            vec![
                Op::Deposit(1),
                Op::Withdraw(1 + u32::from(u16::MAX) % MAX_AMOUNT),
                Op::Deposit(MAX_AMOUNT),
            ]
        );
    }

    #[test]
    fn decoding_caps_the_length_and_drops_a_partial_op() {
        assert_eq!(ops_from_bytes(&[0; 3 * 40]).len(), MAX_OPS);
        assert_eq!(ops_from_bytes(&[0, 1]).len(), 0);
    }

    #[test]
    fn encoding_round_trips() {
        let ops = vec![Op::Deposit(1), Op::Withdraw(MAX_AMOUNT), Op::Deposit(42)];
        assert_eq!(ops_from_bytes(&ops_to_bytes(&ops)), ops);
    }

    #[test]
    fn the_invariant_holds_for_any_input() {
        apply_ops(&[Op::Withdraw(5), Op::Deposit(3), Op::Withdraw(99)]);
        for len in 0..64_u8 {
            let bytes: Vec<u8> = (0..len).map(|i| i.wrapping_mul(37)).collect();
            run_fuzz_input(&bytes);
        }
    }

    #[cfg(feature = "sim-testing")]
    #[test]
    fn every_strategy_draw_round_trips_through_bytes() {
        use proptest::strategy::{Strategy, ValueTree};
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        for _ in 0..64 {
            let ops = super::ops_strategy()
                .new_tree(&mut runner)
                .expect("the strategy draws")
                .current();
            assert!(!ops.is_empty() && ops.len() <= MAX_OPS);
            assert_eq!(ops_from_bytes(&ops_to_bytes(&ops)), ops);
        }
    }
}
