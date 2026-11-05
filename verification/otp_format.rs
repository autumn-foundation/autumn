use vstd::prelude::*;

verus! {

// The format of a one-time code field (issue #3412).
//
// The runtime is `OtpFormat` in `autumn/src/form.rs`. `digits(n)` clamps `n`
// to 4..=10. `group(g)` keeps `g` only when 2 <= g <= 5 and g < n. Here a
// group of 0 means "no group". `cells()` counts the digit boxes and one
// separator between two groups. `autumn-widgets.js` builds the grouped
// `pattern` from groups of `g` digits: the last group can be shorter.
//
// The proofs show:
//
// - The digit count is always 4 to 10.
// - A kept group is 2 to 5 and splits the code in two or more groups.
// - The cell count does not overflow a u8. It is the digit count plus the
//   separators, and it is at most 14.
// - The JS pattern has one group more than the separators. Thus each space
//   that the JS writes has a separator cell.
//
// The proptest `otp_format_stays_in_bounds` checks the same rules on the
// runtime.

pub const MIN_DIGITS: u8 = 4;

pub const MAX_DIGITS: u8 = 10;

/// `OtpFormat::digits`.
pub fn clamp_digits(n: u8) -> (d: u8)
    ensures
        MIN_DIGITS <= d <= MAX_DIGITS,
        MIN_DIGITS <= n <= MAX_DIGITS ==> d == n,
        n < MIN_DIGITS ==> d == MIN_DIGITS,
        n > MAX_DIGITS ==> d == MAX_DIGITS,
{
    if n < MIN_DIGITS {
        MIN_DIGITS
    } else if n > MAX_DIGITS {
        MAX_DIGITS
    } else {
        n
    }
}

/// A group that splits the code.
pub open spec fn valid_group(digits: int, g: int) -> bool {
    2 <= g <= 5 && g < digits
}

/// `OtpFormat::group`. 0 is "no group".
pub fn keep_group(digits: u8, g: u8) -> (k: u8)
    requires
        MIN_DIGITS <= digits <= MAX_DIGITS,
    ensures
        valid_group(digits as int, g as int) ==> k == g,
        !valid_group(digits as int, g as int) ==> k == 0,
        k == 0 || valid_group(digits as int, k as int),
{
    if 2 <= g && g <= 5 && g < digits {
        g
    } else {
        0
    }
}

/// The separator cells: one between two groups.
pub open spec fn separators(digits: int, g: int) -> int {
    if g == 0 {
        0
    } else {
        (digits - 1) / g
    }
}

/// `OtpFormat::cells`.
pub fn cells(digits: u8, g: u8) -> (c: u8)
    requires
        MIN_DIGITS <= digits <= MAX_DIGITS,
        g == 0 || valid_group(digits as int, g as int),
    ensures
        c as int == digits as int + separators(digits as int, g as int),
        digits <= c <= 14,
{
    // One branch for each divisor, so each division is by a constant.
    if g == 0 {
        digits
    } else if g == 2 {
        digits + (digits - 1) / 2
    } else if g == 3 {
        digits + (digits - 1) / 3
    } else if g == 4 {
        digits + (digits - 1) / 4
    } else {
        digits + (digits - 1) / 5
    }
}

/// The groups of the JS pattern: `g` digits each, the last one 1 to `g`.
pub open spec fn parts(rest: int, g: int) -> int
    decreases rest,
{
    if g < 1 || rest <= g {
        1
    } else {
        1 + parts(rest - g, g)
    }
}

/// The pattern and the cells agree, and the cells fit.
pub open spec fn format_ok(n: int, g: int) -> bool {
    parts(n, g) == separators(n, g) + 1 && n + separators(n, g) <= 14
}

/// `format_ok` for each kept group of `n`, from `g` down to 2.
pub open spec fn all_groups(n: int, g: int) -> bool
    decreases g,
{
    if g < 2 {
        true
    } else {
        (g >= n || format_ok(n, g)) && all_groups(n, g - 1)
    }
}

/// `all_groups` for each digit count from `n` down to 4.
pub open spec fn all_formats(n: int) -> bool
    decreases n,
{
    if n < 4 {
        true
    } else {
        all_groups(n, 5) && all_formats(n - 1)
    }
}

proof fn all_groups_gives(n: int, top: int, g: int)
    requires
        all_groups(n, top),
        2 <= g <= top,
        g < n,
    ensures
        format_ok(n, g),
    decreases top,
{
    if g < top {
        all_groups_gives(n, top - 1, g);
    }
}

proof fn all_formats_gives(top: int, n: int, g: int)
    requires
        all_formats(top),
        4 <= n <= top,
        2 <= g <= 5,
        g < n,
    ensures
        format_ok(n, g),
    decreases top,
{
    if n < top {
        all_formats_gives(top - 1, n, g);
    } else {
        all_groups_gives(n, 5, g);
    }
}

/// Each space that the JS writes has a separator cell, and the cells fit.
proof fn pattern_groups_match_the_cells(n: int, g: int)
    requires
        4 <= n <= 10,
        valid_group(n, g),
    ensures
        parts(n, g) == separators(n, g) + 1,
        n + separators(n, g) <= 14,
{
    // The domain is finite (7 digit counts, 4 groups). Evaluate it.
    assert(all_formats(10)) by (compute);
    all_formats_gives(10, n, g);
}

/// With no group, the pattern is one group and there is no separator.
proof fn no_group_has_one_part(n: int)
    requires
        4 <= n <= 10,
    ensures
        parts(n, 0) == 1,
        separators(n, 0) == 0,
{
}

fn main() {}

} // verus!
