use vstd::prelude::*;

verus! {

// Protobuf field numbers of `autumn contracts export --proto`.
//
// The runtime is `autumn-cli/src/contracts/proto/lock.rs`. `next_number` is
// the function of the same name. `assign_new` is the loop in `Lock::assign`
// that gives each new field the next free number above `top`, the largest
// number the message has used (live, reserved, or set by a tag).
//
// The proofs show:
//
// - A new number is a valid protobuf field number: 1 to 536870911, and not
//   19000 to 19999.
// - `next_number` skips no valid number, so numbering is dense.
// - New numbers are strictly increasing and above `top`. Thus they are
//   distinct, and no new number is a number the message used before.
// - Giving a number to a new name does not change the number of a name that
//   already has one.
//
// The proptest `numbers_never_move_and_are_never_reused` checks the same
// rules on the runtime over random field sets.

pub const MAX_NUMBER: u32 = 536_870_911;

pub const KEPT_LOW: u32 = 19_000;

pub const KEPT_HIGH: u32 = 19_999;

/// A number protobuf allows for a field.
pub open spec fn valid(n: int) -> bool {
    1 <= n <= MAX_NUMBER as int && !(KEPT_LOW as int <= n <= KEPT_HIGH as int)
}

/// `next_number`: the first valid number above `after`.
pub fn next_number(after: u32) -> (r: Option<u32>)
    ensures
        match r {
            Some(n) => valid(n as int) && n > after && forall|m: int|
                after < m < n ==> !valid(m),
            None => forall|m: int| m > after ==> !valid(m),
        },
{
    if after >= MAX_NUMBER {
        return None;
    }
    let next = after + 1;
    if KEPT_LOW <= next && next <= KEPT_HIGH {
        Some(KEPT_HIGH + 1)
    } else {
        Some(next)
    }
}

/// The loop that numbers `count` new fields above `top`.
pub fn assign_new(top: u32, count: usize) -> (r: Option<Vec<u32>>)
    ensures
        match r {
            Some(v) => {
                &&& v@.len() == count
                &&& forall|i: int| 0 <= i < v@.len() ==> valid(v@[i] as int) && v@[i] > top
                &&& forall|i: int, j: int| 0 <= i < j < v@.len() ==> v@[i] < v@[j]
            },
            None => true,
        },
{
    let mut out: Vec<u32> = Vec::new();
    let mut last = top;
    while out.len() < count
        invariant
            out@.len() <= count,
            last >= top,
            forall|i: int| 0 <= i < out@.len() ==> valid(out@[i] as int) && top < out@[i] <= last,
            forall|i: int, j: int| 0 <= i < j < out@.len() ==> out@[i] < out@[j],
            out@.len() > 0 ==> out@[out@.len() - 1] == last,
        decreases count - out@.len(),
    {
        match next_number(last) {
            Some(n) => {
                let ghost before = out@;
                out.push(n);
                proof {
                    assert(out@ == before.push(n));
                    assert forall|i: int| 0 <= i < out@.len() implies valid(out@[i] as int) && top
                        < out@[i] <= n by {
                        if i < before.len() {
                            assert(out@[i] == before[i]);
                        }
                    }
                    assert forall|i: int, j: int| 0 <= i < j < out@.len() implies out@[i]
                        < out@[j] by {
                        assert(out@[i] == before[i]);
                        if j < before.len() {
                            assert(out@[j] == before[j]);
                        } else {
                            assert(out@[j] == n);
                        }
                    }
                }
                last = n;
            },
            None => {
                return None;
            },
        }
    }
    Some(out)
}

/// A number above every used number is not a used number.
pub proof fn lemma_new_is_fresh(used: Set<int>, top: int, n: int)
    requires
        forall|u: int| used.contains(u) ==> u <= top,
        n > top,
    ensures
        !used.contains(n),
{
}

/// Giving a number to a name with none keeps every other name's number.
pub proof fn lemma_insert_keeps_numbers(fields: Map<int, int>, name: int, n: int, other: int)
    requires
        !fields.contains_key(name),
        fields.contains_key(other),
    ensures
        fields.insert(name, n).contains_key(other),
        fields.insert(name, n)[other] == fields[other],
{
}

fn main() {}

} // verus!
