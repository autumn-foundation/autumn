use vstd::prelude::*;

verus! {

// Model of one `autumn_lease_locks` row (issue #3053) and of one resource
// that checks fencing tokens. Each spec fn below names the SQL statement it
// models in `autumn/src/lock/lease.rs`.

/// One lease row. `generation == 0` means "no row yet".
pub struct LeaseRow {
    pub generation: nat,
    pub held: bool,
    pub expires_at: int,
}

/// A resource that keeps the highest fencing token it accepted.
/// `highest == 0` means "no write yet".
pub struct Resource {
    pub highest: nat,
}

pub enum Op {
    Acquire { now: int, ttl: nat },
    Renew { token: nat, now: int, ttl: nat },
    Release { token: nat },
}

/// `ON CONFLICT .. DO UPDATE .. WHERE owner IS NULL OR expires_at <= now()`.
pub open spec fn can_acquire(row: LeaseRow, now: int) -> bool {
    !row.held || row.expires_at <= now
}

/// `UPDATE .. WHERE generation = $token AND owner IS NOT NULL
/// AND expires_at > now()`.
pub open spec fn can_renew(row: LeaseRow, token: nat, now: int) -> bool {
    row.held && row.generation == token && row.expires_at > now
}

/// `UPDATE .. SET owner = NULL WHERE generation = $token
/// AND owner IS NOT NULL`.
pub open spec fn can_release(row: LeaseRow, token: nat) -> bool {
    row.held && row.generation == token
}

/// One statement. A guard that fails leaves the row unchanged.
pub open spec fn apply(row: LeaseRow, op: Op) -> LeaseRow {
    match op {
        Op::Acquire { now, ttl } => if can_acquire(row, now) {
            LeaseRow { generation: row.generation + 1, held: true, expires_at: now + ttl }
        } else {
            row
        },
        Op::Renew { token, now, ttl } => if can_renew(row, token, now) {
            LeaseRow { expires_at: now + ttl, ..row }
        } else {
            row
        },
        Op::Release { token } => if can_release(row, token) {
            LeaseRow { held: false, ..row }
        } else {
            row
        },
    }
}

/// True when `op` grants a new lease (the acquire returns a row).
pub open spec fn grants(row: LeaseRow, op: Op) -> bool {
    match op {
        Op::Acquire { now, ttl } => can_acquire(row, now),
        _ => false,
    }
}

/// The row after a sequence of statements on one lock name.
pub open spec fn run(row: LeaseRow, ops: Seq<Op>) -> LeaseRow
    decreases ops.len()
{
    if ops.len() == 0 {
        row
    } else {
        apply(run(row, ops.drop_last()), ops.last())
    }
}

/// The token the `k`-th statement grants, if it grants one.
pub open spec fn token_at(row: LeaseRow, ops: Seq<Op>, k: int) -> nat {
    run(row, ops.take(k + 1)).generation
}

/// The conditional write `WHERE fencing_token <= $incoming`.
pub open spec fn admits(res: Resource, incoming: nat) -> bool {
    res.highest <= incoming
}

pub open spec fn write(res: Resource, incoming: nat) -> Resource {
    if admits(res, incoming) { Resource { highest: incoming } } else { res }
}

// ---- Proofs on one statement ----

/// An acquire that grants a lease gives a strictly larger token.
pub proof fn acquire_strictly_increases(row: LeaseRow, op: Op)
    requires grants(row, op)
    ensures
        apply(row, op).generation == row.generation + 1,
        apply(row, op).held,
{}

/// No statement makes the generation smaller.
pub proof fn apply_never_decreases(row: LeaseRow, op: Op)
    ensures apply(row, op).generation >= row.generation
{}

/// Only an acquire changes the generation.
pub proof fn only_acquire_changes_generation(row: LeaseRow, op: Op)
    requires !grants(row, op)
    ensures apply(row, op).generation == row.generation
{}

/// A stale token cannot renew or release. So a stale holder cannot extend
/// its lease or free the lease of its successor.
pub proof fn stale_token_cannot_renew_or_release(
    row: LeaseRow, stale: nat, now: int, ttl: nat)
    requires stale < row.generation
    ensures
        apply(row, Op::Renew { token: stale, now, ttl }) == row,
        apply(row, Op::Release { token: stale }) == row,
{}

/// An expired lease cannot be renewed back to life.
pub proof fn expired_lease_cannot_renew(row: LeaseRow, token: nat, now: int, ttl: nat)
    requires row.expires_at <= now
    ensures apply(row, Op::Renew { token, now, ttl }) == row
{}

/// A held, live lease cannot be taken.
pub proof fn live_lease_blocks_acquire(row: LeaseRow, now: int, ttl: nat)
    requires row.held, row.expires_at > now
    ensures apply(row, Op::Acquire { now, ttl }) == row
{}

// ---- Proofs on a sequence of statements ----

proof fn take_step(ops: Seq<Op>, k: int)
    requires 0 <= k < ops.len()
    ensures ops.take(k + 1).drop_last() == ops.take(k), ops.take(k + 1).last() == ops[k]
{
    assert(ops.take(k + 1).drop_last() =~= ops.take(k));
}

/// The generation never goes down along a run.
pub proof fn run_is_monotonic(row: LeaseRow, ops: Seq<Op>, i: int, j: int)
    requires 0 <= i <= j <= ops.len()
    ensures run(row, ops.take(i)).generation <= run(row, ops.take(j)).generation
    decreases j - i
{
    if i < j {
        run_is_monotonic(row, ops, i, j - 1);
        take_step(ops, j - 1);
        apply_never_decreases(run(row, ops.take(j - 1)), ops[j - 1]);
    }
}

/// Tokens are strictly monotonic per lock name: a later grant always gets a
/// larger token than an earlier grant.
pub proof fn tokens_strictly_increase(row: LeaseRow, ops: Seq<Op>, i: int, j: int)
    requires
        0 <= i < j < ops.len(),
        grants(run(row, ops.take(i)), ops[i]),
        grants(run(row, ops.take(j)), ops[j]),
    ensures token_at(row, ops, i) < token_at(row, ops, j)
{
    take_step(ops, i);
    take_step(ops, j);
    acquire_strictly_increases(run(row, ops.take(j)), ops[j]);
    run_is_monotonic(row, ops, i + 1, j);
}

/// Two grants never share a token.
pub proof fn grants_have_unique_tokens(row: LeaseRow, ops: Seq<Op>, i: int, j: int)
    requires
        0 <= i < ops.len(),
        0 <= j < ops.len(),
        i != j,
        grants(run(row, ops.take(i)), ops[i]),
        grants(run(row, ops.take(j)), ops[j]),
    ensures token_at(row, ops, i) != token_at(row, ops, j)
{
    if i < j {
        tokens_strictly_increase(row, ops, i, j);
    } else {
        tokens_strictly_increase(row, ops, j, i);
    }
}

// ---- Proofs on the resource ----

/// The current holder can write more than once with one token.
pub proof fn holder_can_write_again(res: Resource, token: nat)
    requires admits(res, token)
    ensures admits(write(res, token), token)
{}

/// The highest accepted token never goes down.
pub proof fn resource_is_monotonic(res: Resource, incoming: nat)
    ensures write(res, incoming).highest >= res.highest
{}

/// The stale-holder theorem. Holder A got token `a`. Later, holder B got
/// token `b` and wrote with it. A's write with `a` is rejected and does not
/// change the resource.
pub proof fn stale_write_is_rejected(
    row: LeaseRow, ops: Seq<Op>, i: int, j: int, res: Resource)
    requires
        0 <= i < j < ops.len(),
        grants(run(row, ops.take(i)), ops[i]),
        grants(run(row, ops.take(j)), ops[j]),
        admits(res, token_at(row, ops, j)),
    ensures
        !admits(write(res, token_at(row, ops, j)), token_at(row, ops, i)),
        write(write(res, token_at(row, ops, j)), token_at(row, ops, i))
            == write(res, token_at(row, ops, j)),
{
    tokens_strictly_increase(row, ops, i, j);
}

fn main() {}

} // verus!
