use vstd::prelude::*;

verus! {

// Avro integer types of `autumn contracts export --avro`.
//
// The runtime is `integer` in `autumn-cli/src/contracts/avro/mod.rs`. It
// gives the Avro type of a Rust integer of `bits` width and sign.
//
// The proofs show:
//
// - The mapping never narrows: each value of the Rust integer is in the
//   range of the Avro type it gets.
// - The mapping is the smallest: a `long` is given only when an `int` does
//   not hold the range.
// - A refusal is given only when no Avro integer holds the range.
//
// The golden test `each_wire_type_maps_as_the_table_says` checks the same
// table on the runtime.

pub enum AvroInt {
    Int,
    Long,
}

/// Two to the power `n`.
pub open spec fn pow2(n: nat) -> int
    decreases n,
{
    if n == 0 {
        1
    } else {
        2 * pow2((n - 1) as nat)
    }
}

/// The smallest value of a Rust integer.
pub open spec fn rust_min(bits: nat, signed: bool) -> int {
    if signed {
        -pow2((bits - 1) as nat)
    } else {
        0
    }
}

/// The largest value of a Rust integer.
pub open spec fn rust_max(bits: nat, signed: bool) -> int {
    if signed {
        pow2((bits - 1) as nat) - 1
    } else {
        pow2(bits) - 1
    }
}

/// Whether a signed integer of `width` bits holds each value of the Rust
/// integer.
pub open spec fn fits(bits: nat, signed: bool, width: nat) -> bool {
    -pow2((width - 1) as nat) <= rust_min(bits, signed) && rust_max(bits, signed) <= pow2(
        (width - 1) as nat,
    ) - 1
}

proof fn lemma_pow2_pos(n: nat)
    ensures
        pow2(n) >= 1,
    decreases n,
{
    if n > 0 {
        lemma_pow2_pos((n - 1) as nat);
    }
}

proof fn lemma_pow2_mono(a: nat, b: nat)
    requires
        a <= b,
    ensures
        pow2(a) <= pow2(b),
    decreases b,
{
    if a < b {
        lemma_pow2_mono(a, (b - 1) as nat);
        lemma_pow2_pos((b - 1) as nat);
    }
}

proof fn lemma_constants()
    ensures
        pow2(31) == 2147483648,
        pow2(32) == 4294967296,
        pow2(63) == 9223372036854775808,
        pow2(64) == 18446744073709551616,
{
    assert(pow2(31) == 2147483648) by (compute_only);
    assert(pow2(32) == 4294967296) by (compute_only);
    assert(pow2(63) == 9223372036854775808) by (compute_only);
    assert(pow2(64) == 18446744073709551616) by (compute_only);
}

/// `integer`: the Avro type of a Rust integer, or `None` to refuse it.
pub fn integer(bits: u8, signed: bool) -> (r: Option<AvroInt>)
    requires
        bits >= 1,
    ensures
        r matches Some(AvroInt::Int) ==> fits(bits as nat, signed, 32),
        r matches Some(AvroInt::Long) ==> fits(bits as nat, signed, 64) && !fits(
            bits as nat,
            signed,
            32,
        ),
        r is None ==> !fits(bits as nat, signed, 64),
{
    proof {
        lemma_constants();
        lemma_pow2_pos(bits as nat);
        lemma_pow2_pos((bits - 1) as nat);
    }
    if (signed && bits <= 32) || (!signed && bits <= 31) {
        proof {
            if signed {
                lemma_pow2_mono((bits - 1) as nat, 31);
            } else {
                lemma_pow2_mono(bits as nat, 31);
            }
        }
        Some(AvroInt::Int)
    } else if (signed && bits <= 64) || (!signed && bits <= 63) {
        proof {
            if signed {
                lemma_pow2_mono((bits - 1) as nat, 63);
                lemma_pow2_mono(32, (bits - 1) as nat);
            } else {
                lemma_pow2_mono(bits as nat, 63);
                lemma_pow2_mono(32, bits as nat);
            }
        }
        Some(AvroInt::Long)
    } else {
        proof {
            if signed {
                lemma_pow2_mono(64, (bits - 1) as nat);
            } else {
                lemma_pow2_mono(64, bits as nat);
            }
        }
        None
    }
}

fn main() {
}

} // verus!
