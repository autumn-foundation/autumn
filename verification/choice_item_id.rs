use vstd::prelude::*;

verus! {

// The id of one input in a choice group (issue #3405).
//
// The runtime is `push_choice_items` in `autumn/src/form.rs`. The first
// radio, box or segment gets `id="{field}"`, so an error summary link
// `#{field}` finds the group. Item 1 and up gets `id="{field}-{index}"`. The
// index is `usize::to_string`: one or more ASCII digits. The other ids of a
// field are `{field}-hint`, `{field}-error`, `{field}-field` and
// `{field}-label`. A nested row writes `{collection}-{row}-{subfield}`. A Rust
// field name has no `-`, so the first item id is a plain id.
//
// Here an id is a sequence of ASCII bytes. The proofs show:
//
// - Two items never share an id, in one group or in two groups.
// - An item id is never `{g}-{word}` when `word` has no `-` and does not
//   start with a digit. That covers `hint`, `error`, `field`, `label` and a
//   nested subfield.
// - An item id is never an id with no `-`, such as a plain field id or the
//   first item of a group.
//
// The proptest `choice_item_ids_never_collide` checks the same rules on the
// runtime ids.

/// `[0-9]`.
pub open spec fn is_digit(b: u8) -> bool {
    48 <= b && b <= 57
}

/// The byte `-`.
pub open spec fn dash() -> u8 {
    45u8
}

/// Each byte is a digit.
pub open spec fn all_digits(s: Seq<u8>) -> bool {
    forall|i: int| 0 <= i < s.len() ==> is_digit(#[trigger] s[i])
}

/// No byte is `-`.
pub open spec fn no_dash(s: Seq<u8>) -> bool {
    forall|i: int| 0 <= i < s.len() ==> #[trigger] s[i] != dash()
}

/// An index: the decimal digits of a `usize`.
pub open spec fn is_index(s: Seq<u8>) -> bool {
    s.len() > 0 && all_digits(s)
}

/// The last part of another framework id: no `-`, and the first byte is
/// not a digit (`hint`, `error`, `field`, `label`, a Rust field name).
pub open spec fn is_word(s: Seq<u8>) -> bool {
    s.len() > 0 && no_dash(s) && !is_digit(s[0])
}

/// `{field}-{index}`.
pub open spec fn item_id(field: Seq<u8>, index: Seq<u8>) -> Seq<u8> {
    field + seq![dash()] + index
}

/// A digit is not `-`.
proof fn digits_have_no_dash(s: Seq<u8>)
    requires
        all_digits(s),
    ensures
        no_dash(s),
{
    assert forall|i: int| 0 <= i < s.len() implies #[trigger] s[i] != dash() by {
        assert(is_digit(s[i]));
    }
}

/// In `a + "-" + x`, the byte at `a.len()` is `-`.
proof fn the_dash_is_at_the_field_end(a: Seq<u8>, x: Seq<u8>)
    ensures
        (a + seq![dash()] + x).len() == a.len() + 1 + x.len(),
        (a + seq![dash()] + x)[a.len() as int] == dash(),
{
    let ad = a + seq![dash()];
    assert(ad.len() == a.len() + 1);
    assert(ad[a.len() as int] == dash());
    assert((ad + x)[a.len() as int] == ad[a.len() as int]);
}

/// In `a + "-" + x`, a byte after the dash is a byte of `x`.
proof fn a_byte_after_the_dash_is_in_the_tail(a: Seq<u8>, x: Seq<u8>, k: int)
    requires
        a.len() + 1 <= k < a.len() + 1 + x.len(),
    ensures
        (a + seq![dash()] + x)[k] == x[k - a.len() - 1],
{
    let ad = a + seq![dash()];
    assert(ad.len() == a.len() + 1);
    assert((ad + x)[k] == x[k - ad.len()]);
}

/// When the tails have no `-`, a split at the last `-` is unique.
proof fn split_at_the_last_dash(a: Seq<u8>, x: Seq<u8>, b: Seq<u8>, y: Seq<u8>)
    requires
        no_dash(x),
        no_dash(y),
        a + seq![dash()] + x == b + seq![dash()] + y,
    ensures
        a == b,
        x == y,
{
    let s = a + seq![dash()] + x;
    let t = b + seq![dash()] + y;
    the_dash_is_at_the_field_end(a, x);
    the_dash_is_at_the_field_end(b, y);
    assert(s.len() == t.len());
    if a.len() < b.len() {
        // `t` has `-` at `b.len()`. In `s`, that byte is in `x`.
        a_byte_after_the_dash_is_in_the_tail(a, x, b.len() as int);
        assert(s[b.len() as int] == x[b.len() - a.len() - 1]);
        assert(x[b.len() - a.len() - 1] != dash());
        assert(false);
    } else if b.len() < a.len() {
        a_byte_after_the_dash_is_in_the_tail(b, y, a.len() as int);
        assert(t[a.len() as int] == y[a.len() - b.len() - 1]);
        assert(y[a.len() - b.len() - 1] != dash());
        assert(false);
    }
    assert(a.len() == b.len());
    assert(a =~= s.subrange(0, a.len() as int));
    assert(b =~= t.subrange(0, b.len() as int));
    assert(x =~= s.subrange(a.len() + 1, s.len() as int));
    assert(y =~= t.subrange(b.len() + 1, t.len() as int));
}

/// Two items never share an id.
pub proof fn item_ids_are_injective(f1: Seq<u8>, i1: Seq<u8>, f2: Seq<u8>, i2: Seq<u8>)
    requires
        is_index(i1),
        is_index(i2),
        item_id(f1, i1) == item_id(f2, i2),
    ensures
        f1 == f2,
        i1 == i2,
{
    digits_have_no_dash(i1);
    digits_have_no_dash(i2);
    split_at_the_last_dash(f1, i1, f2, i2);
}

/// An item id is never `{g}-{word}`: not a hint, error, wrapper or label id,
/// and not a nested subfield id.
pub proof fn an_item_id_is_not_a_word_id(f: Seq<u8>, i: Seq<u8>, g: Seq<u8>, word: Seq<u8>)
    requires
        is_index(i),
        is_word(word),
    ensures
        item_id(f, i) != g + seq![dash()] + word,
{
    digits_have_no_dash(i);
    if item_id(f, i) == g + seq![dash()] + word {
        split_at_the_last_dash(f, i, g, word);
        assert(i[0] == word[0]);
        assert(is_digit(i[0]));
        assert(false);
    }
}

/// An item id has a `-`, so it is never an id with no `-`.
pub proof fn an_item_id_is_not_a_plain_id(f: Seq<u8>, i: Seq<u8>, id: Seq<u8>)
    requires
        no_dash(id),
    ensures
        item_id(f, i) != id,
{
    the_dash_is_at_the_field_end(f, i);
    if item_id(f, i) == id {
        assert(id[f.len() as int] == dash());
        assert(false);
    }
}

fn main() {}

} // verus!
