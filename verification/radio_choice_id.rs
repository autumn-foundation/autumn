use vstd::prelude::*;

verus! {

// The lead part of a radio choice id (issue #2666).
//
// The runtime is `radio_option_ids`, `id_part` and `selector_safe` in
// `autumn/src/a11y.rs`. An id is parts joined by `-`. `id_part` reduces each
// part to the bytes `[A-Za-z0-9_-]`. `selector_safe` then makes the lead part
// start a CSS identifier: it adds one `_` when the part, without its leading
// `_` bytes, does not start with a letter.
//
// Here a part is a sequence of ASCII bytes. The proofs show:
//
// - The result starts with a letter or `_`, so `#id` is a valid selector.
// - Two different parts never give one result, so the rule adds no
//   collision of its own.
// - The rule keeps the alphabet.
// - A part that starts with a letter does not change.

/// `[A-Za-z]`.
pub open spec fn is_letter(b: u8) -> bool {
    (65 <= b && b <= 90) || (97 <= b && b <= 122)
}

/// `[A-Za-z0-9_-]`, the bytes `id_part` keeps.
pub open spec fn in_alphabet(b: u8) -> bool {
    is_letter(b) || (48 <= b && b <= 57) || b == 95 || b == 45
}

/// The part, without its leading `_` bytes, does not start with a letter.
pub open spec fn needs_prefix(s: Seq<u8>) -> bool
    decreases s.len(),
{
    if s.len() == 0 {
        true
    } else if s[0] == 95u8 {
        needs_prefix(s.drop_first())
    } else {
        !is_letter(s[0])
    }
}

/// `selector_safe` in `autumn/src/a11y.rs`.
pub open spec fn selector_safe(s: Seq<u8>) -> Seq<u8> {
    if needs_prefix(s) {
        seq![95u8] + s
    } else {
        s
    }
}

/// A leading `_` does not change the decision.
proof fn an_underscore_keeps_the_decision(s: Seq<u8>)
    ensures
        needs_prefix(seq![95u8] + s) == needs_prefix(s),
{
    let t = seq![95u8] + s;
    assert(t.len() > 0);
    assert(t[0] == 95u8);
    assert(t.drop_first() =~= s);
}

/// The result starts a CSS identifier.
pub proof fn the_result_starts_an_identifier(s: Seq<u8>)
    ensures
        selector_safe(s).len() > 0,
        is_letter(selector_safe(s)[0]) || selector_safe(s)[0] == 95u8,
{
    if needs_prefix(s) {
        assert((seq![95u8] + s)[0] == 95u8);
    }
}

/// Two different parts never give one result.
pub proof fn distinct_parts_give_distinct_results(a: Seq<u8>, b: Seq<u8>)
    requires
        a != b,
    ensures
        selector_safe(a) != selector_safe(b),
{
    an_underscore_keeps_the_decision(a);
    an_underscore_keeps_the_decision(b);
    if needs_prefix(a) && needs_prefix(b) {
        assert((seq![95u8] + a).drop_first() =~= a);
        assert((seq![95u8] + b).drop_first() =~= b);
    }
}

/// The rule adds only `_`, so the result stays in the alphabet.
pub proof fn the_rule_keeps_the_alphabet(s: Seq<u8>)
    requires
        forall|i: int| 0 <= i < s.len() ==> in_alphabet(#[trigger] s[i]),
    ensures
        forall|i: int|
            0 <= i < selector_safe(s).len() ==> in_alphabet(#[trigger] selector_safe(s)[i]),
{
    if needs_prefix(s) {
        let t = seq![95u8] + s;
        assert(selector_safe(s) == t);
        assert forall|i: int| 0 <= i < t.len() implies in_alphabet(#[trigger] t[i]) by {
            if i > 0 {
                assert(t[i] == s[i - 1]);
            }
        }
    }
}

/// A part that starts with a letter is already safe and does not change.
pub proof fn a_letter_led_part_is_unchanged(s: Seq<u8>)
    requires
        s.len() > 0,
        is_letter(s[0]),
    ensures
        selector_safe(s) == s,
{
}

fn main() {}

} // verus!
