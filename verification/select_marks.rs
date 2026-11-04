use vstd::prelude::*;

verus! {

// Which `<option>` of a select gets `selected` (issue #3409).
//
// The runtime is `push_select_options` in `autumn/src/form.rs`. It walks the
// top-level options, then each `<optgroup>`, in document order. Here an
// option is its value, and the select is the sequence of all its options in
// that order. `current` is the set of values to show as chosen: one value
// for a single select, `Changeset::field_values` for a multiple select.
//
// The proofs show:
//
// - A single select marks at most one option.
// - A single select marks an option when one or more options match.
// - A match in an earlier part (the top-level options or an earlier group)
//   stops a match in a later part. So groups do not add a second mark.
// - A multiple select marks each option that matches.
// - A marked option always matches.
//
// The proptest `select_marks_follow_the_spec` checks the same rules on the
// rendered markup.

/// The value of option `i` is in `current`.
pub open spec fn is_match(values: Seq<int>, current: Set<int>, i: int) -> bool {
    current.contains(values[i])
}

/// Option `i` matches and no option before it matches.
pub open spec fn first_match(values: Seq<int>, current: Set<int>, i: int) -> bool {
    current.contains(values[i]) && forall|j: int|
        0 <= j < i ==> !current.contains(#[trigger] values[j])
}

/// The runtime rule: a multiple select marks each match. A single select
/// marks the first match only.
pub open spec fn marked(values: Seq<int>, current: Set<int>, multiple: bool, i: int) -> bool {
    if multiple {
        is_match(values, current, i)
    } else {
        first_match(values, current, i)
    }
}

/// A marked option matches.
pub proof fn a_mark_is_a_match(values: Seq<int>, current: Set<int>, multiple: bool, i: int)
    requires
        marked(values, current, multiple, i),
    ensures
        is_match(values, current, i),
{
}

/// A multiple select marks each option that matches.
pub proof fn multiple_marks_each_match(values: Seq<int>, current: Set<int>, i: int)
    requires
        is_match(values, current, i),
    ensures
        marked(values, current, true, i),
{
}

/// A single select never marks two options.
pub proof fn single_marks_at_most_one(values: Seq<int>, current: Set<int>, a: int, b: int)
    requires
        0 <= a < b < values.len(),
        marked(values, current, false, a),
    ensures
        !marked(values, current, false, b),
{
    assert(current.contains(values[a]));
}

/// When one of the first `n` options matches, a single select marks one of
/// them.
pub proof fn single_marks_a_match(values: Seq<int>, current: Set<int>, n: int)
    requires
        0 <= n <= values.len(),
        exists|k: int| 0 <= k < n && #[trigger] current.contains(values[k]),
    ensures
        exists|k: int| 0 <= k < n && #[trigger] first_match(values, current, k),
    decreases n,
{
    if exists|k: int| 0 <= k < n - 1 && #[trigger] current.contains(values[k]) {
        single_marks_a_match(values, current, n - 1);
        let k = choose|k: int| 0 <= k < n - 1 && #[trigger] first_match(values, current, k);
        assert(first_match(values, current, k));
    } else {
        let k = choose|k: int| 0 <= k < n && #[trigger] current.contains(values[k]);
        assert(k == n - 1);
        assert forall|j: int| 0 <= j < n - 1 implies !current.contains(#[trigger] values[j]) by {
        }
        assert(first_match(values, current, n - 1));
    }
}

/// A match in the earlier part `a` stops each mark in the later part `b` of
/// a single select.
pub proof fn an_earlier_match_wins(a: Seq<int>, b: Seq<int>, current: Set<int>, i: int)
    requires
        0 <= i < b.len(),
        exists|k: int| 0 <= k < a.len() && #[trigger] current.contains(a[k]),
    ensures
        !first_match(a + b, current, a.len() as int + i),
{
    let k = choose|k: int| 0 <= k < a.len() && #[trigger] current.contains(a[k]);
    assert((a + b)[k] == a[k]);
    assert(current.contains((a + b)[k]));
}

fn main() {
}

} // verus!
