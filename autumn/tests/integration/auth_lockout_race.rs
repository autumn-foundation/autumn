//! #2500 regression: a concurrent successful login must never be silently
//! re-locked by a racing failed-password attempt's lock-stamp write.
//!
//! `autumn generate auth`'s generated `login` (and duplicated `reauth`)
//! handler counts a wrong-password attempt with an atomic
//! `UPDATE ... SET failed_attempts = failed_attempts + 1` and then, if the
//! new count crosses the configured threshold, stamps `locked_at` with a
//! *second*, separate `UPDATE`. Before the fix landed in this commit, that
//! second `UPDATE` was unconditional (`WHERE id = ?` only), gated only by an
//! in-memory `current_locked_at` value read at the top of the request. If a
//! concurrent request with the *correct* password committed its own
//! "successful login resets the counter" `UPDATE` (`failed_attempts = 0,
//! locked_at = NULL`) in the gap between the failed request's two
//! statements, the unconditional lock stamp reapplied on top of the reset —
//! silently re-locking an account that had *already* logged in successfully.
//!
//! These tests reproduce that exact interleaving directly against a real
//! Postgres instance (testcontainers) using two genuinely separate
//! connections (distinct Postgres backends, exactly like two separate HTTP
//! requests would use), mirroring the two SQL statements the generator
//! emits. The statements are issued in an explicitly chosen order rather
//! than fired concurrently and hoping the race lands (the issue's own repro
//! script shows the raw race only hits 3-12% of the time), so the exact bad
//! interleaving is exercised on every run and the test never flakes.

use diesel::sql_types::{BigInt, Int4, Nullable, Timestamp};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Mirrors the columns the generator adds to the app's user table.
const DDL: &str = "CREATE TABLE IF NOT EXISTS lockout_accounts ( \
    id BIGSERIAL PRIMARY KEY, \
    failed_attempts INT4 NOT NULL DEFAULT 0, \
    locked_at TIMESTAMP NULL)";

const THRESHOLD: i32 = 3;

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(4).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(DDL)
        .execute(&mut conn)
        .await
        .expect("create lockout_accounts table");
    drop(conn);

    (pool, container)
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct AttemptsRow {
    #[diesel(sql_type = Int4)]
    failed_attempts: i32,
}

#[derive(diesel::QueryableByName)]
struct StateRow {
    #[diesel(sql_type = Int4)]
    failed_attempts: i32,
    #[diesel(sql_type = Nullable<Timestamp>)]
    locked_at: Option<chrono::NaiveDateTime>,
}

/// Seeds one account row one attempt below the lockout threshold, unlocked —
/// the exact precondition the issue's repro script sets up before firing the
/// concurrent wrong/correct password pair.
async fn seed_one_below_threshold(conn: &mut AsyncPgConnection) -> i64 {
    diesel::sql_query(
        "INSERT INTO lockout_accounts (failed_attempts, locked_at) \
         VALUES ($1, NULL) RETURNING id",
    )
    .bind::<Int4, _>(THRESHOLD - 1)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed account")
    .id
}

async fn read_state(conn: &mut AsyncPgConnection, id: i64) -> StateRow {
    diesel::sql_query("SELECT failed_attempts, locked_at FROM lockout_accounts WHERE id = $1")
        .bind::<BigInt, _>(id)
        .get_result::<StateRow>(conn)
        .await
        .expect("read state")
}

/// The wrong-password branch's first statement: atomically increment the
/// failure counter, exactly as `{table}::failed_attempts.eq({table}::failed_attempts + 1)`
/// does in the generated handler.
async fn increment_failed_attempts(conn: &mut AsyncPgConnection, id: i64) -> i32 {
    diesel::sql_query(
        "UPDATE lockout_accounts SET failed_attempts = failed_attempts + 1 \
         WHERE id = $1 RETURNING failed_attempts",
    )
    .bind::<BigInt, _>(id)
    .get_result::<AttemptsRow>(conn)
    .await
    .expect("increment failed_attempts")
    .failed_attempts
}

/// The successful-login branch: reset the counter and clear the lock, unless
/// an *active* lock is already present — exactly the generated handler's
/// `WHERE locked_at IS NULL OR locked_at <= lock_expired_before` guard.
/// Returns the number of rows affected: 0 means "reject this login, the
/// account was concurrently locked."
async fn reset_on_successful_login(conn: &mut AsyncPgConnection, id: i64) -> u64 {
    diesel::sql_query(
        "UPDATE lockout_accounts SET failed_attempts = 0, locked_at = NULL \
         WHERE id = $1 AND (locked_at IS NULL OR locked_at <= now() - interval '900 seconds')",
    )
    .bind::<BigInt, _>(id)
    .execute(conn)
    .await
    .expect("reset on successful login") as u64
}

/// The **pre-fix** lock-stamp write: unconditional except for the row id —
/// gated only by an in-memory `current_locked_at.is_none()` check the caller
/// already evaluated before this statement runs. This is what issue #2500
/// reports as the root cause.
async fn stamp_lock_unconditional(conn: &mut AsyncPgConnection, id: i64) -> u64 {
    diesel::sql_query("UPDATE lockout_accounts SET locked_at = now() WHERE id = $1")
        .bind::<BigInt, _>(id)
        .execute(conn)
        .await
        .expect("stamp lock (unconditional)") as u64
}

/// The **fixed** lock-stamp write: re-checks `failed_attempts` and
/// `locked_at` at the database, at write time, instead of trusting the
/// in-memory value read at the top of the request. Mirrors the
/// `.filter({table}::failed_attempts.ge(lockout_cfg.threshold)).filter({table}::locked_at.is_null())`
/// guard added to `autumn-cli/src/generate/auth.rs`.
async fn stamp_lock_guarded(conn: &mut AsyncPgConnection, id: i64, threshold: i32) -> u64 {
    diesel::sql_query(
        "UPDATE lockout_accounts SET locked_at = now() \
         WHERE id = $1 AND failed_attempts >= $2 AND locked_at IS NULL",
    )
    .bind::<BigInt, _>(id)
    .bind::<Int4, _>(threshold)
    .execute(conn)
    .await
    .expect("stamp lock (guarded)") as u64
}

/// Characterizes the bug: with the **pre-fix** unconditional stamp, the
/// concurrent successful login's reset gets clobbered. The final row shows
/// `failed_attempts = 0` (the reset ran) yet `locked_at` is set anyway —
/// exactly the "BUG" signature the issue's repro script detects, even though
/// the correct-password login had already been granted (`rows_cleared > 0`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unconditional_lock_stamp_reproduces_2500_relock_after_concurrent_success() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let id = seed_one_below_threshold(&mut conn).await;

    let mut wrong_conn = pool.get().await.expect("wrong-password conn");
    let mut correct_conn = pool.get().await.expect("correct-password conn");

    // 1. Wrong-password request: atomic increment (its own separate
    //    statement, exactly as the generated handler does it).
    let new_attempts = increment_failed_attempts(&mut wrong_conn, id).await;
    assert!(new_attempts >= THRESHOLD);

    // 2. Concurrent correct-password request: its reset commits first,
    //    winning the race and completing the successful login (303 issued,
    //    session cookie set, in the real handler) before the wrong-password
    //    request's second statement ever runs.
    let rows_cleared = reset_on_successful_login(&mut correct_conn, id).await;

    // 3. Only now does the wrong-password request run its lock-stamp write —
    //    the exact gap the issue describes.
    let rows_locked = stamp_lock_unconditional(&mut wrong_conn, id).await;

    assert_eq!(
        rows_cleared, 1,
        "the concurrent correct-password login must have succeeded (session issued)"
    );
    assert_eq!(
        rows_locked, 1,
        "the pre-fix unconditional stamp always writes locked_at regardless of \
         concurrent state — that is the bug"
    );

    let state = read_state(&mut conn, id).await;
    assert_eq!(
        state.failed_attempts, 0,
        "the successful login's reset did apply"
    );
    assert!(
        state.locked_at.is_some(),
        "BUG #2500 reproduced: locked_at is set even though failed_attempts \
         was just reset to 0 by a login that already succeeded"
    );
}

/// The fix: guarding the stamp on the row's *current* `failed_attempts` and
/// `locked_at` makes the exact same interleaving a no-op. The successful
/// login's reset stands, and the account is never silently re-locked out
/// from under it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn guarded_lock_stamp_never_relocks_after_concurrent_success() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let id = seed_one_below_threshold(&mut conn).await;

    let mut wrong_conn = pool.get().await.expect("wrong-password conn");
    let mut correct_conn = pool.get().await.expect("correct-password conn");

    // Same forced interleaving as the characterization test above, only the
    // stamp write itself differs.
    let new_attempts = increment_failed_attempts(&mut wrong_conn, id).await;
    assert!(new_attempts >= THRESHOLD);

    let rows_cleared = reset_on_successful_login(&mut correct_conn, id).await;

    let rows_locked = stamp_lock_guarded(&mut wrong_conn, id, THRESHOLD).await;

    assert_eq!(
        rows_cleared, 1,
        "the concurrent correct-password login must still succeed"
    );
    assert_eq!(
        rows_locked, 0,
        "the guarded stamp must be a no-op once a concurrent successful login \
         has already reset failed_attempts below the threshold"
    );

    let state = read_state(&mut conn, id).await;
    assert_eq!(state.failed_attempts, 0, "reset must stand");
    assert!(
        state.locked_at.is_none(),
        "fixed: an account whose login already succeeded must never end up \
         locked because of a losing concurrent wrong-password attempt"
    );
}

/// The mirror-image ordering: if the wrong-password request's guarded stamp
/// commits *before* the concurrent correct-password request's reset runs,
/// the account is genuinely locked at that point, and the reset must
/// correctly reject the login (0 rows affected) rather than let a stale read
/// slip a session through for an account that is, at that instant, locked.
/// This pins the other half of the invariant: the fix does not "always
/// prefer success" — it makes whichever write actually lands first the one
/// that determines a *consistent* final state.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn guarded_lock_stamp_winning_the_race_correctly_rejects_the_concurrent_login() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let id = seed_one_below_threshold(&mut conn).await;

    let mut wrong_conn = pool.get().await.expect("wrong-password conn");
    let mut correct_conn = pool.get().await.expect("correct-password conn");

    let new_attempts = increment_failed_attempts(&mut wrong_conn, id).await;
    assert!(new_attempts >= THRESHOLD);

    // This time the wrong-password request's stamp lands first...
    let rows_locked = stamp_lock_guarded(&mut wrong_conn, id, THRESHOLD).await;
    assert_eq!(rows_locked, 1, "the account must actually be locked here");

    // ...so the concurrent correct-password request's reset must see the
    // active lock and refuse to clear it.
    let rows_cleared = reset_on_successful_login(&mut correct_conn, id).await;
    assert_eq!(
        rows_cleared, 0,
        "a login racing a lock that already committed must be rejected, not \
         silently granted a session against a row it never actually reset"
    );

    let state = read_state(&mut conn, id).await;
    assert!(
        state.locked_at.is_some(),
        "the lock that won the race must remain in effect"
    );
    assert_eq!(
        state.failed_attempts, new_attempts,
        "a rejected login must not have touched the counter"
    );
}

/// The guard must not weaken lockout against genuine credential stuffing: two
/// concurrent wrong-password requests, both crossing the threshold, with no
/// successful login anywhere in the picture, must still end with the account
/// locked. This rules out the guard being exploitable the other way — an
/// attacker racing only wrong-password attempts against each other cannot
/// use the DB-level re-check to dodge lockout, because whichever of them
/// runs its stamp while `locked_at` is still `NULL` succeeds.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn guarded_lock_stamp_still_locks_under_concurrent_attacker_only_race() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let id = seed_one_below_threshold(&mut conn).await;

    let mut attacker_a = pool.get().await.expect("attacker A conn");
    let mut attacker_b = pool.get().await.expect("attacker B conn");

    // Both concurrent wrong-password requests increment first, exactly as
    // two real overlapping requests would (each sees the other's committed
    // increment on its own atomic `+ 1`).
    let attempts_a = increment_failed_attempts(&mut attacker_a, id).await;
    let attempts_b = increment_failed_attempts(&mut attacker_b, id).await;
    assert!(attempts_a >= THRESHOLD && attempts_b >= THRESHOLD);

    // A's stamp lands first and succeeds...
    let a_locked = stamp_lock_guarded(&mut attacker_a, id, THRESHOLD).await;
    assert_eq!(a_locked, 1, "the first over-threshold stamp must apply");

    // ...B's is now a no-op (already locked), but the account stays locked —
    // it is not a no-op that leaves the account unlocked.
    let b_locked = stamp_lock_guarded(&mut attacker_b, id, THRESHOLD).await;
    assert_eq!(
        b_locked, 0,
        "a second stamp attempt on an already-locked row must not re-write it"
    );

    let state = read_state(&mut conn, id).await;
    assert!(
        state.locked_at.is_some(),
        "two concurrent wrong-password requests crossing the threshold must \
         still result in a locked account — the guard closes a false-positive \
         re-lock, it must not open a way to dodge a genuine one"
    );
    assert_eq!(state.failed_attempts, attempts_b.max(attempts_a));
}
