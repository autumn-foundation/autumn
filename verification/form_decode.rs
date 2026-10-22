use vstd::prelude::*;

verus! {

// One step of the form-decode retry loop (issue #3394).
//
// The runtime is `decode_pairs_dropping_blank_optional_fields` and
// `next_retry` in `autumn/src/form.rs`. It decodes the pairs. When serde
// fails at a field, it does one of these things:
//
// - The field takes one value and the body sent it more than once: it stops
//   with "repeated field" (a 422).
// - The field takes one value and its pair is blank: it removes that pair and
//   decodes again.
// - An element of a sequence field failed and that element is blank: it
//   removes every blank pair of that key and decodes again.
// - Else: it stops with the decode error (a 400).
//
// Here a pair is a key id and a blank flag. The value text is not modeled.
// `i` is the index of the failed pair. The unit tests in `form.rs` check that
// the runtime finds the correct index.

pub struct Pair {
    pub key: int,
    pub blank: bool,
}

/// Where serde reports the error.
pub enum At {
    /// A field that takes one value.
    Field(int),
    /// One element of a sequence field (`Vec`, `HashSet`).
    Element(int),
}

pub enum Step {
    /// Stop: a single-value field got more than one value.
    Repeated,
    /// Decode again with these pairs.
    Retry(Seq<Pair>),
    /// Stop: the body does not decode.
    Fail,
}

/// How many pairs have this key.
pub open spec fn count(p: Seq<Pair>, key: int) -> nat
    decreases p.len(),
{
    if p.len() == 0 {
        0
    } else {
        (if p.last().key == key { 1nat } else { 0nat }) + count(p.drop_last(), key)
    }
}

/// How many times this exact pair occurs.
pub open spec fn occurrences(p: Seq<Pair>, x: Pair) -> nat
    decreases p.len(),
{
    if p.len() == 0 {
        0
    } else {
        (if p.last() == x { 1nat } else { 0nat }) + occurrences(p.drop_last(), x)
    }
}

/// `pairs.retain(|(k, v)| !(k == key && v.is_empty()))`.
pub open spec fn drop_blanks(p: Seq<Pair>, key: int) -> Seq<Pair>
    decreases p.len(),
{
    if p.len() == 0 {
        p
    } else {
        let rest = drop_blanks(p.drop_last(), key);
        if p.last().key == key && p.last().blank {
            rest
        } else {
            rest.push(p.last())
        }
    }
}

/// Pair `i` is a blank pair of this key.
pub open spec fn blank_at(p: Seq<Pair>, key: int, i: int) -> bool {
    0 <= i < p.len() && p[i].key == key && p[i].blank
}

/// One step of the loop.
pub open spec fn step(p: Seq<Pair>, at: At, i: int) -> Step {
    match at {
        At::Field(key) => if count(p, key) > 1 {
            Step::Repeated
        } else if blank_at(p, key, i) {
            Step::Retry(p.remove(i))
        } else {
            Step::Fail
        },
        At::Element(key) => if blank_at(p, key, i) {
            Step::Retry(drop_blanks(p, key))
        } else {
            Step::Fail
        },
    }
}

/// A repeated key on a single-value field always stops the loop. The loop
/// never removes one copy and accepts the other.
pub proof fn a_repeated_scalar_is_never_resolved(p: Seq<Pair>, key: int, i: int)
    requires
        count(p, key) > 1,
    ensures
        step(p, At::Field(key), i) == Step::Repeated,
{
}

/// A field retry removes exactly one pair, and that pair is blank. All other
/// pairs stay, in their order.
pub proof fn a_field_retry_drops_one_blank_pair(p: Seq<Pair>, key: int, i: int)
    requires
        step(p, At::Field(key), i) is Retry,
    ensures
        blank_at(p, key, i),
        step(p, At::Field(key), i) == Step::Retry(p.remove(i)),
        p.remove(i).len() + 1 == p.len(),
        forall|j: int| 0 <= j < i ==> #[trigger] p.remove(i)[j] == p[j],
        forall|j: int| i <= j < p.len() - 1 ==> #[trigger] p.remove(i)[j] == p[j + 1],
{
    let q = p.remove(i);
    assert(q =~= p.subrange(0, i) + p.subrange(i + 1, p.len() as int));
    assert forall|j: int| 0 <= j < i implies #[trigger] q[j] == p[j] by {}
    assert forall|j: int| i <= j < p.len() - 1 implies #[trigger] q[j] == p[j + 1] by {}
}

/// Dropping blanks never makes the input longer.
pub proof fn drop_blanks_never_grows(p: Seq<Pair>, key: int)
    ensures
        drop_blanks(p, key).len() <= p.len(),
    decreases p.len(),
{
    if p.len() > 0 {
        drop_blanks_never_grows(p.drop_last(), key);
    }
}

/// With at least one blank pair of the key, the input gets shorter.
pub proof fn drop_blanks_shrinks(p: Seq<Pair>, key: int, i: int)
    requires
        blank_at(p, key, i),
    ensures
        drop_blanks(p, key).len() < p.len(),
    decreases p.len(),
{
    drop_blanks_never_grows(p.drop_last(), key);
    if i < p.len() - 1 {
        assert(p.drop_last()[i] == p[i]);
        drop_blanks_shrinks(p.drop_last(), key, i);
    }
}

/// Every pair that is not a blank pair of the key stays, as many times as it
/// was there. So a filled pair is never dropped.
pub proof fn drop_blanks_keeps_other_pairs(p: Seq<Pair>, key: int, x: Pair)
    requires
        !(x.key == key && x.blank),
    ensures
        occurrences(drop_blanks(p, key), x) == occurrences(p, x),
    decreases p.len(),
{
    if p.len() > 0 {
        drop_blanks_keeps_other_pairs(p.drop_last(), key, x);
        let rest = drop_blanks(p.drop_last(), key);
        if !(p.last().key == key && p.last().blank) {
            assert(rest.push(p.last()).drop_last() =~= rest);
            assert(rest.push(p.last()).last() == p.last());
        }
    }
}

/// Each retry makes the input shorter, so the loop stops.
pub proof fn every_retry_shrinks_the_input(p: Seq<Pair>, at: At, i: int)
    requires
        step(p, at, i) is Retry,
    ensures
        step(p, at, i)->Retry_0.len() < p.len(),
{
    match at {
        At::Field(key) => {
            a_field_retry_drops_one_blank_pair(p, key, i);
        },
        At::Element(key) => {
            drop_blanks_shrinks(p, key, i);
        },
    }
}

fn main() {}

} // verus!
