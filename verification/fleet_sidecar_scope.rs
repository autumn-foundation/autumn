use vstd::prelude::*;

verus! {

// The replica scope of a fleet sidecar engine (ADR 0040 §3).
//
// The runtime is `ScopedDestination::admit` in
// `autumn/src/replication/destination.rs` and `sidecar_root` in
// `autumn/src/db/fleet.rs`. A sidecar of the database with the replica
// prefix `p` gets the scope `p/<engine>`. The scope admits a key only when
// the key starts with `p/<engine>/` and has more after it. An engine name and
// a tenant id hold no `/`. The reserved names `generations`, `epochs` and
// `tombstone` are the primary's own names under `p/`.
//
// Here a key is a sequence of bytes. The model leaves out `validate_key`:
// the runtime refuses a key with a `.` or `..` segment before `admit`, so a
// key such as `p/e/../generations/x` never reaches a destination. The unit
// tests check that part. The proofs show:
//
// - An admitted key is under `p/`, so the purge of a deleted database (a
//   listing of `p/`) also takes the sidecar's objects.
// - An admitted key of engine `e` is not under `p/<r>/` and is not `p/<r>`
//   for any other name `r`. So a sidecar cannot write the primary's
//   generations, epochs or tombstone, or the scope of another sidecar.
// - The same rule, one level up, keeps the scope of one tenant out of the
//   prefix of another tenant.
//
// The unit tests `a_scoped_destination_refuses_keys_outside_its_root` and
// `sidecar_names_follow_the_rule` check the same rules on the runtime.

/// The byte `/`.
pub open spec fn slash() -> u8 {
    47u8
}

/// No byte is `/`.
pub open spec fn no_slash(s: Seq<u8>) -> bool {
    forall|i: int| 0 <= i < s.len() ==> #[trigger] s[i] != slash()
}

/// `s` starts with `p`.
pub open spec fn starts_with(s: Seq<u8>, p: Seq<u8>) -> bool {
    p.len() <= s.len() && s.subrange(0, p.len() as int) == p
}

/// `p/`.
pub open spec fn dir(p: Seq<u8>) -> Seq<u8> {
    p.push(slash())
}

/// `p/<name>`.
pub open spec fn child(p: Seq<u8>, name: Seq<u8>) -> Seq<u8> {
    dir(p) + name
}

/// The runtime `admit`: the key starts with `root/` and has more after it.
pub open spec fn admits(root: Seq<u8>, key: Seq<u8>) -> bool {
    starts_with(key, dir(root)) && key.len() > root.len() + 1
}

/// The part of `key` after `p/`.
pub open spec fn rest(key: Seq<u8>, p: Seq<u8>) -> Seq<u8> {
    key.subrange(p.len() + 1, key.len() as int)
}

/// When `s` starts with `p`, a byte of `p` is the byte of `s`.
proof fn starts_with_index(s: Seq<u8>, p: Seq<u8>, i: int)
    requires
        starts_with(s, p),
        0 <= i < p.len(),
    ensures
        s[i] == p[i],
{
    assert(s.subrange(0, p.len() as int)[i] == s[i]);
}

/// The bytes of `p/<x>/`: `p`, then `/`, then `x`, then `/`.
proof fn bytes_of_a_child_dir(p: Seq<u8>, x: Seq<u8>, i: int)
    requires
        0 <= i < x.len() + 1,
    ensures
        dir(child(p, x)).len() == p.len() + 1 + x.len() + 1,
        dir(child(p, x))[p.len() + 1 + i] == dir(x)[i],
        dir(child(p, x))[p.len() as int] == slash(),
{
    let dp = dir(p);
    let c = dp + x;
    assert(dp.len() == p.len() + 1);
    assert(dp[p.len() as int] == slash());
    assert(c.len() == p.len() + 1 + x.len());
    assert(c[p.len() as int] == dp[p.len() as int]);
    assert(dir(c)[p.len() as int] == c[p.len() as int]);
    if i < x.len() {
        assert(c[p.len() + 1 + i] == x[i]);
        assert(dir(c)[p.len() + 1 + i] == c[p.len() + 1 + i]);
        assert(dir(x)[i] == x[i]);
    } else {
        assert(dir(c)[p.len() + 1 + i] == slash());
        assert(dir(x)[i] == slash());
    }
}

/// A key under `p/<x>/` is under `p/`, and its rest starts with `x/`.
proof fn the_rest_starts_with_the_name(p: Seq<u8>, x: Seq<u8>, key: Seq<u8>)
    requires
        starts_with(key, dir(child(p, x))),
    ensures
        starts_with(key, dir(p)),
        starts_with(rest(key, p), dir(x)),
{
    let d = dir(child(p, x));
    let n = p.len() + 1;
    bytes_of_a_child_dir(p, x, 0);
    assert(d.len() == n + x.len() + 1);
    assert(dir(p).len() == p.len() + 1);
    assert(key.len() >= d.len());
    assert forall|i: int| 0 <= i < dir(p).len() implies key.subrange(
        0,
        dir(p).len() as int,
    )[i] == dir(p)[i] by {
        starts_with_index(key, d, i);
        if i < p.len() {
            assert(d[i] == child(p, x)[i]);
            assert(child(p, x)[i] == dir(p)[i]);
        } else {
            assert(d[i] == slash());
        }
    }
    assert(key.subrange(0, dir(p).len() as int) =~= dir(p));
    let r = rest(key, p);
    assert(r.len() == key.len() - n);
    assert(dir(x).len() == x.len() + 1);
    assert forall|i: int| 0 <= i < dir(x).len() implies r.subrange(
        0,
        dir(x).len() as int,
    )[i] == dir(x)[i] by {
        bytes_of_a_child_dir(p, x, i);
        starts_with_index(key, d, n + i);
        assert(r[i] == key[n + i]);
    }
    assert(r.subrange(0, dir(x).len() as int) =~= dir(x));
}

/// When `s` starts with `a/` and with `b/`, and neither name holds `/`, the
/// names are equal.
proof fn one_first_segment(s: Seq<u8>, a: Seq<u8>, b: Seq<u8>)
    requires
        no_slash(a),
        no_slash(b),
        starts_with(s, dir(a)),
        starts_with(s, dir(b)),
    ensures
        a == b,
{
    if a.len() < b.len() {
        starts_with_index(s, dir(a), a.len() as int);
        starts_with_index(s, dir(b), a.len() as int);
        assert(dir(a)[a.len() as int] == slash());
        assert(dir(b)[a.len() as int] == b[a.len() as int]);
        assert(b[a.len() as int] != slash());
        assert(false);
    } else if b.len() < a.len() {
        starts_with_index(s, dir(a), b.len() as int);
        starts_with_index(s, dir(b), b.len() as int);
        assert(dir(b)[b.len() as int] == slash());
        assert(dir(a)[b.len() as int] == a[b.len() as int]);
        assert(a[b.len() as int] != slash());
        assert(false);
    }
    assert(a.len() == b.len());
    assert forall|i: int| 0 <= i < a.len() implies a[i] == b[i] by {
        starts_with_index(s, dir(a), i);
        starts_with_index(s, dir(b), i);
        assert(dir(a)[i] == a[i]);
        assert(dir(b)[i] == b[i]);
    }
    assert(a =~= b);
}

/// An admitted key is under the database prefix `p/`.
pub proof fn an_admitted_key_is_under_the_database(p: Seq<u8>, e: Seq<u8>, key: Seq<u8>)
    requires
        admits(child(p, e), key),
    ensures
        starts_with(key, dir(p)),
{
    the_rest_starts_with_the_name(p, e, key);
}

/// An admitted key of engine `e` is not under `p/<r>/` for another name `r`.
pub proof fn an_admitted_key_is_not_under_another_name(
    p: Seq<u8>,
    e: Seq<u8>,
    r: Seq<u8>,
    key: Seq<u8>,
)
    requires
        no_slash(e),
        no_slash(r),
        e != r,
        admits(child(p, e), key),
    ensures
        !starts_with(key, dir(child(p, r))),
{
    the_rest_starts_with_the_name(p, e, key);
    if starts_with(key, dir(child(p, r))) {
        the_rest_starts_with_the_name(p, r, key);
        one_first_segment(rest(key, p), e, r);
    }
}

/// An admitted key of engine `e` is not the object `p/<r>`, for example the
/// tombstone marker.
pub proof fn an_admitted_key_is_not_another_object(
    p: Seq<u8>,
    e: Seq<u8>,
    r: Seq<u8>,
    key: Seq<u8>,
)
    requires
        no_slash(r),
        admits(child(p, e), key),
    ensures
        key != child(p, r),
{
    the_rest_starts_with_the_name(p, e, key);
    if key == child(p, r) {
        let rs = rest(key, p);
        assert(dir(p).len() == p.len() + 1);
        assert(key.len() == p.len() + 1 + r.len());
        assert(rs.len() == r.len());
        assert(rs =~= r) by {
            assert forall|i: int| 0 <= i < r.len() implies rs[i] == r[i] by {
                assert(rs[i] == key[p.len() + 1 + i]);
                assert(child(p, r)[p.len() + 1 + i] == r[i]);
            }
        }
        assert(dir(e).len() == e.len() + 1);
        assert(rs.len() >= dir(e).len());
        starts_with_index(rs, dir(e), e.len() as int);
        assert(dir(e)[e.len() as int] == slash());
        assert(r[e.len() as int] != slash());
        assert(false);
    }
}

/// The scope of a sidecar of tenant `t1` is not under the prefix of another
/// tenant `t2` with the same parent `p`.
pub proof fn a_sidecar_key_is_not_under_another_tenant(
    p: Seq<u8>,
    t1: Seq<u8>,
    t2: Seq<u8>,
    e: Seq<u8>,
    key: Seq<u8>,
)
    requires
        no_slash(t1),
        no_slash(t2),
        t1 != t2,
        admits(child(child(p, t1), e), key),
    ensures
        !starts_with(key, dir(child(p, t2))),
{
    an_admitted_key_is_under_the_database(child(p, t1), e, key);
    the_rest_starts_with_the_name(p, t1, key);
    if starts_with(key, dir(child(p, t2))) {
        the_rest_starts_with_the_name(p, t2, key);
        one_first_segment(rest(key, p), t1, t2);
    }
}

fn main() {}

} // verus!
