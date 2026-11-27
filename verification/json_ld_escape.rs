use vstd::prelude::*;

verus! {

// The JSON-LD script escaper.
//
// The runtime is `escape_script_json` in `autumn/src/json_ld.rs`. It writes
// serialized JSON into a `<script type="application/ld+json">` element. The
// HTML parser ends that element at `</script`, and it reads `<!--` there as
// the start of an escaped text span. Both patterns start with `<`. The
// escaper writes `<` as `\u003c`, `>` as `\u003e` and `&` as `\u0026`.
//
// Here the JSON is a sequence of bytes. The proofs show:
//
// - The output has no `<`, `>` or `&` byte, for any input.
// - So no pattern that starts with `<` appears anywhere in the output:
//   `</script` and `<!--` cannot occur.
// - Every other byte passes through unchanged.
//
// U+2028 and U+2029 are UTF-8 sequences of three bytes, none of them `<`,
// `>` or `&`. The runtime writes them as ` ` and ` `, which is also
// free of those bytes. This model treats them as bytes that pass through, so
// the first property holds with or without that rule. The proptest
// `escaped_output_has_no_markup_bytes_and_round_trips` checks the runtime on
// any string, separators included.

/// `<`, `>` or `&`.
pub open spec fn is_markup(b: u8) -> bool {
    b == 60u8 || b == 62u8 || b == 38u8
}

/// No byte of `s` is `<`, `>` or `&`.
pub open spec fn no_markup(s: Seq<u8>) -> bool {
    forall|i: int| 0 <= i < s.len() ==> !is_markup(#[trigger] s[i])
}

/// The six bytes of a JSON `\u00` escape: a backslash, `u`, `0`, `0`, then
/// two lowercase hex digits.
pub open spec fn unicode_escape(hi: u8, lo: u8) -> Seq<u8> {
    seq![92u8, 117u8, 48u8, 48u8, hi, lo]
}

/// The output for one input byte.
pub open spec fn escape_byte(b: u8) -> Seq<u8> {
    if b == 60u8 {
        // `\u003c`
        unicode_escape(51u8, 99u8)
    } else if b == 62u8 {
        // `\u003e`
        unicode_escape(51u8, 101u8)
    } else if b == 38u8 {
        // `\u0026`
        unicode_escape(50u8, 54u8)
    } else {
        seq![b]
    }
}

/// The escaper: each byte in order, replaced by `escape_byte`.
pub open spec fn escape(s: Seq<u8>) -> Seq<u8>
    decreases s.len(),
{
    if s.len() == 0 {
        Seq::empty()
    } else {
        escape(s.drop_last()) + escape_byte(s.last())
    }
}

/// `p` occurs in `s` at `i`.
pub open spec fn occurs_at(s: Seq<u8>, p: Seq<u8>, i: int) -> bool {
    0 <= i && i + p.len() <= s.len() && s.subrange(i, i + p.len()) == p
}

/// `</script`.
pub open spec fn script_close() -> Seq<u8> {
    seq![60u8, 47u8, 115u8, 99u8, 114u8, 105u8, 112u8, 116u8]
}

/// `<!--`.
pub open spec fn comment_open() -> Seq<u8> {
    seq![60u8, 33u8, 45u8, 45u8]
}

/// An escape has no markup byte.
proof fn unicode_escape_has_no_markup(hi: u8, lo: u8)
    requires
        !is_markup(hi),
        !is_markup(lo),
    ensures
        no_markup(unicode_escape(hi, lo)),
{
    let e = unicode_escape(hi, lo);
    assert(e.len() == 6);
    assert(e[0] == 92u8);
    assert(e[1] == 117u8);
    assert(e[2] == 48u8);
    assert(e[3] == 48u8);
    assert(e[4] == hi);
    assert(e[5] == lo);
}

/// One byte's output has no markup byte.
proof fn escape_byte_has_no_markup(b: u8)
    ensures
        no_markup(escape_byte(b)),
{
    if b == 60u8 {
        unicode_escape_has_no_markup(51u8, 99u8);
    } else if b == 62u8 {
        unicode_escape_has_no_markup(51u8, 101u8);
    } else if b == 38u8 {
        unicode_escape_has_no_markup(50u8, 54u8);
    } else {
        let e = escape_byte(b);
        assert(e.len() == 1);
        assert(e[0] == b);
    }
}

/// A byte that is not markup passes through.
proof fn other_bytes_pass_through(b: u8)
    requires
        !is_markup(b),
    ensures
        escape_byte(b) == seq![b],
{
}

/// Joining two sequences with no markup byte gives one with none.
proof fn concat_keeps_no_markup(a: Seq<u8>, b: Seq<u8>)
    requires
        no_markup(a),
        no_markup(b),
    ensures
        no_markup(a + b),
{
    assert forall|i: int| 0 <= i < (a + b).len() implies !is_markup(#[trigger] (a + b)[i]) by {
        if i < a.len() {
            assert((a + b)[i] == a[i]);
        } else {
            assert((a + b)[i] == b[i - a.len()]);
        }
    }
}

/// The escaper's output has no `<`, `>` or `&` byte.
proof fn escape_has_no_markup(s: Seq<u8>)
    ensures
        no_markup(escape(s)),
    decreases s.len(),
{
    if s.len() > 0 {
        escape_has_no_markup(s.drop_last());
        escape_byte_has_no_markup(s.last());
        concat_keeps_no_markup(escape(s.drop_last()), escape_byte(s.last()));
    }
}

/// A pattern that starts with `<` does not occur in a sequence with no
/// markup byte.
proof fn no_markup_has_no_lt_pattern(s: Seq<u8>, p: Seq<u8>, i: int)
    requires
        no_markup(s),
        p.len() > 0,
        p[0] == 60u8,
    ensures
        !occurs_at(s, p, i),
{
    if occurs_at(s, p, i) {
        assert(s.subrange(i, i + p.len())[0] == s[i]);
        assert(s[i] == 60u8);
        assert(is_markup(s[i]));
    }
}

/// `</script` and `<!--` never occur in the escaper's output.
proof fn escape_cannot_close_the_script(s: Seq<u8>, i: int)
    ensures
        !occurs_at(escape(s), script_close(), i),
        !occurs_at(escape(s), comment_open(), i),
{
    escape_has_no_markup(s);
    assert(script_close()[0] == 60u8);
    assert(comment_open()[0] == 60u8);
    no_markup_has_no_lt_pattern(escape(s), script_close(), i);
    no_markup_has_no_lt_pattern(escape(s), comment_open(), i);
}

fn main() {}

} // verus!
