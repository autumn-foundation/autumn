// ── v0.2 Feature: #[scheduled] macro ─────────────────────────────────────
//
// Declares a background task that runs every hour alongside the
// HTTP server. Dependencies (AppState) are injected automatically,
// just like handler extractors.
//
// Errors are logged at WARN level and the task retries on the
// next scheduled interval.

use autumn_web::http::{Client, ClientError};
use autumn_web::prelude::*;
use reqwest::StatusCode;

use crate::repositories::BookmarkRepository;

fn response_is_reachable(status: StatusCode) -> bool {
    status.is_success() || status.is_redirection()
}

fn head_requires_get_fallback(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
    )
}

fn probe_outcome(head: Result<StatusCode, ()>, get: Option<Result<StatusCode, ()>>) -> bool {
    match head {
        Ok(status) if response_is_reachable(status) => true,
        Ok(status) if head_requires_get_fallback(status) => {
            get.is_some_and(|fallback| fallback.is_ok_and(response_is_reachable))
        }
        _ => false,
    }
}

async fn probe_reachable(client: &Client, url: &str) -> bool {
    let head_result = client.head(url).no_retry().send().await;
    let head = match head_result {
        Err(ClientError::CircuitBreakerOpen) => return true, // inconclusive — don't mark dead
        other => other.map(|r| r.status()),
    };
    match head {
        Ok(status) if response_is_reachable(status) => true,
        Ok(status) if head_requires_get_fallback(status) => {
            let get_result = client.get(url).no_retry().send().await;
            let get = match get_result {
                Err(ClientError::CircuitBreakerOpen) => return true, // inconclusive
                other => other.map(|r| r.status()),
            };
            probe_outcome(Ok(status), Some(get.map_err(|_| ())))
        }
        Ok(status) => probe_outcome(Ok(status), None),
        Err(_) => false,
    }
}

async fn process_shard(
    repo: &BookmarkRepository,
    client: &Client,
    shard: u32,
) -> AutumnResult<(u32, u32)> {
    let shard_alive = repo.find_alive_in_shard(shard).await?;

    if shard_alive.is_empty() {
        return Ok((0, 0));
    }
    let shard_checked_count =
        u32::try_from(shard_alive.len()).expect("shard bookmark count must fit in u32");

    tracing::info!(shard, count = shard_alive.len(), "link-checker owns shard");

    let mut dead_count = 0u32;
    for (id, url) in shard_alive {
        let reachable = probe_reachable(client, &url).await;

        if !reachable {
            tracing::warn!("link-checker: dead link id={id} url={url}");
            if repo.mark_dead(id).await? {
                dead_count += 1;
            }
        }
    }

    Ok((shard_checked_count, dead_count))
}

#[scheduled(every = "1h", name = "link-checker")]
pub async fn check_links(state: AppState) -> AutumnResult<()> {
    let repo = BookmarkRepository;
    let client = Client::from_state(&state);

    let mut dead_count = 0u32;
    let mut checked_count = 0u32;
    let mut owned_shards = 0u32;

    for shard in BookmarkRepository::shard_ids() {
        // `try_with` runs the section on exactly one replica: it skips the shard
        // (returns `None`) when another replica holds the lock, and releases the
        // lock automatically when `process_shard` finishes — on normal return,
        // an early `?`, or a panic (the guard closes its session as it unwinds,
        // so no lock leaks). This replaces the hand-rolled `pg_try_advisory_lock`
        // / `pg_advisory_unlock` dance the example used to carry.
        let outcome = BookmarkRepository::shard_lock(shard)?
            .try_with(|| process_shard(&repo, &client, shard))
            .await?;

        let Some(shard_result) = outcome else {
            tracing::debug!(shard, "link-checker shard already owned by another replica");
            continue;
        };

        let (shard_checked_count, shard_dead_count) = shard_result?;
        owned_shards += 1;
        dead_count += shard_dead_count;
        checked_count += shard_checked_count;
    }

    tracing::info!(
        owned_shards,
        dead_count,
        checked = checked_count,
        "link-checker done"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{head_requires_get_fallback, probe_outcome, response_is_reachable};
    use reqwest::StatusCode;

    #[test]
    fn reachable_statuses_match_link_checker_expectation() {
        assert!(response_is_reachable(StatusCode::OK));
        assert!(response_is_reachable(StatusCode::MOVED_PERMANENTLY));
        assert!(!response_is_reachable(StatusCode::NOT_FOUND));
    }

    #[test]
    fn head_fallback_is_limited_to_head_unsupported_statuses() {
        assert!(head_requires_get_fallback(StatusCode::METHOD_NOT_ALLOWED));
        assert!(head_requires_get_fallback(StatusCode::NOT_IMPLEMENTED));
        assert!(!head_requires_get_fallback(StatusCode::NOT_FOUND));
        assert!(!head_requires_get_fallback(StatusCode::FORBIDDEN));
    }

    #[test]
    fn successful_head_probe_marks_link_reachable() {
        assert!(probe_outcome(Ok(StatusCode::OK), None));
    }

    #[test]
    fn head_405_falls_back_to_get_before_marking_dead() {
        assert!(probe_outcome(
            Ok(StatusCode::METHOD_NOT_ALLOWED),
            Some(Ok(StatusCode::OK))
        ));
        assert!(!probe_outcome(
            Ok(StatusCode::METHOD_NOT_ALLOWED),
            Some(Ok(StatusCode::NOT_FOUND))
        ));
    }

    #[test]
    fn hard_head_failures_do_not_trigger_fallback() {
        assert!(!probe_outcome(Ok(StatusCode::NOT_FOUND), None));
        assert!(!probe_outcome(Err(()), None));
    }
}

/// Ledger findings/fix harness for `process_shard`'s dead-link write step
/// (above): `BookmarkRepository::mark_dead(id)` -- a fresh pooled-connection
/// checkout plus a single-row `UPDATE bookmarks ... WHERE id = $1` -- is
/// awaited once per dead link the hourly link-checker finds, sequentially,
/// on top of the (already sequential, HTTP-bound) probe loop. A shard's
/// `UPDATE` statement count scales with how many of its alive bookmarks
/// rotted since the last hourly run, not with anything fixed: for a
/// bookmarking app with tens of thousands of saved links, that's hundreds of
/// single-row round trips, once per shard, every hour, forever.
///
/// This harness drives the REAL production entry point --
/// `bookmarks_distributed::tasks::check_links`, the `#[scheduled(every =
/// "1h")]` job itself, called directly the same way the scheduler calls it
/// (skipping only the timer tick, which is orthogonal to the query this
/// harness measures). The HTTP half is real too: a tiny in-process TCP
/// responder stands in for the public internet, returning 200 for
/// `/alive/*` paths and 404 for `/dead/*`, so `check_links` makes real HTTP
/// round trips and real Postgres round trips end to end -- only DNS/the
/// public internet is swapped for loopback.
///
/// **Requires Docker.** `bookmarks-distributed` ships no library target (it
/// is a `[[bin]]`-only example crate, see `Cargo.toml`), so an external
/// `tests/*.rs` file cannot link against `check_links`/`BookmarkRepository`
/// the way most other Ledger harnesses in this repo do -- this one lives in
/// `src/tasks.rs`'s own `#[cfg(test)]` tree instead, exactly like this
/// crate's existing `shard_lock_is_exclusive_and_reacquirable` Docker test
/// (`repositories.rs`). This crate also has no consolidated Docker sweep
/// (CLAUDE.md), so CI runs it via an explicit
/// `--bin bookmarks-distributed -- --ignored` line in
/// `.github/workflows/ci.yml`'s "Run Docker-dependent tests" step. Run
/// manually with:
///
/// ```text
/// cargo test -p bookmarks-distributed --bin bookmarks-distributed \
///   -- --ignored link_checker_batches_mark_dead --nocapture --test-threads=1
/// ```
///
/// ## Fixture
///
/// 10,000 bookmarks, ids `1..=10,000` (a `BIGSERIAL` primary key, so `id ==
/// gs`), spread across the job's 16 shards by `id % 16` exactly as
/// `find_alive_in_shard` partitions them:
///
/// | category | selector | count | probe outcome |
/// |---|---|---:|---|
/// | historical dead | `gs % 7 == 0` | 1,428 | `alive = false` already; excluded by the scan, never probed |
/// | rotted since last run | `gs % 7 != 0 && gs % 11 == 0` | 780 | `alive = true`, URL routes to `/dead/…`; this run's write target |
/// | still alive | everything else | 7,792 | `alive = true`, URL routes to `/alive/…`; probed, stays alive |
///
/// The rotted-link selector deliberately uses modulus 11, coprime with
/// `LINK_CHECKER_SHARD_COUNT` (16 = 2^4): a selector sharing a factor with
/// 16 (e.g. `% 8`) sends every match to only 2 of the 16 `id % 16` shard
/// buckets, understating how many shards the batched write actually has to
/// touch. 11 cycles through all 16 residues as `gs` grows, so the rotted
/// set spreads realistically across every shard.
///
/// A follow-up `UPDATE` touches every historical-dead row and `ANALYZE` runs
/// with no intervening `VACUUM`, so planner statistics see real dead tuples,
/// matching every other Ledger fixture in this repo.
///
/// ## Baseline (captured against this commit's pre-fix `mark_dead`/
/// `process_shard`, one full 16-shard run)
///
/// `pg_stat_statements`, reset immediately before the run:
///
/// ```text
/// calls=780    buffers=9518       UPDATE "bookmarks" SET "alive" = $1 WHERE ("bookmarks"."id" = $2)
/// calls=16     buffers=2323       SELECT id, url FROM bookmarks WHERE alive = $3 AND (id % $1) = $2 ORDER BY id
/// calls=809    buffers=0          SELECT $1                                    -- shard-lock try/release + probes' own bookkeeping
/// calls=16     buffers=0          SELECT pg_try_advisory_lock($1) AS acquired
/// calls=16     buffers=0          SELECT pg_advisory_unlock($1) AS released
/// ```
///
/// `mark_dead`'s per-row `UPDATE`: **calls=780, buffers=9518 -- 80.4% of
/// the run's 11,841 total workload buffers**, and the single largest
/// statement by both calls and buffers. Comfortably clears the "worth
/// changing" floor (>5% of buffers and calls) by a wide margin. Each
/// individual `UPDATE` is a cheap primary-key point lookup (`EXPLAIN
/// (ANALYZE, BUFFERS, VERBOSE, SETTINGS)`, captured by this same harness:
/// `Index Scan using bookmarks_pkey`, `Buffers: shared hit=9`) -- the
/// defect is exclusively statement *count* (one per dead link instead of
/// one per shard), not plan shape, which is exactly what "elimination of
/// an N+1" targets.
#[cfg(test)]
mod link_checker_batch_profile {
    use crate::db::create_dual_pools;
    use crate::repositories::LINK_CHECKER_SHARD_COUNT;
    use crate::state::DistributedState;
    use autumn_web::test::TestApp;
    use diesel::connection::SimpleConnection;
    use diesel::sql_types::{BigInt, Text};
    use diesel::{Connection, PgConnection, QueryableByName};
    use diesel_migrations::MigrationHarness;
    use std::sync::Arc;
    use testcontainers::ImageExt;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TOTAL_BOOKMARKS: i64 = 10_000;

    /// A tiny in-process HTTP responder standing in for the public internet:
    /// `HEAD`/`GET /dead/*` -> 404, everything else -> 200. Sequential by
    /// construction (`process_shard` awaits one probe at a time), so a plain
    /// accept loop with no request pipelining needs to be handled.
    async fn spawn_probe_mock() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock probe listener");
        let port = listener.local_addr().expect("mock listener addr").port();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(1024);
                    let mut chunk = [0u8; 512];
                    loop {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= 4096 {
                            break;
                        }
                    }
                    let request = String::from_utf8_lossy(&buf);
                    let dead =
                        request.starts_with("HEAD /dead/") || request.starts_with("GET /dead/");
                    let status = if dead { "404 Not Found" } else { "200 OK" };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (port, handle)
    }

    /// Seed `bookmarks`: see the module doc table for the category shape.
    fn seed_fixture(conn: &mut PgConnection, mock_port: u16) {
        conn.batch_execute(&format!(
            "INSERT INTO bookmarks (url, title, tag, alive, created_at) \
             SELECT \
               'http://127.0.0.1:{mock_port}/' || \
                 (CASE WHEN gs % 7 != 0 AND gs % 11 = 0 THEN 'dead' ELSE 'alive' END) \
                 || '/' || gs, \
               'Bookmark ' || gs, \
               (CASE gs % 5 \
                  WHEN 0 THEN 'news' WHEN 1 THEN 'tools' WHEN 2 THEN 'blog' \
                  WHEN 3 THEN 'reference' ELSE 'misc' END), \
               gs % 7 != 0, \
               TIMESTAMP '2025-01-01 00:00:00' + (gs || ' seconds')::interval \
             FROM generate_series(1, {TOTAL_BOOKMARKS}) AS gs"
        ))
        .expect("seed bookmarks");

        // Real dead tuples on the historical-dead rows only -- never touches
        // the alive rows this harness measures.
        conn.batch_execute(
            "UPDATE bookmarks SET title = title || ' (checked)' WHERE alive = false",
        )
        .expect("create dead tuples");
        conn.batch_execute("ANALYZE bookmarks")
            .expect("analyze bookmarks");
    }

    #[derive(QueryableByName, Debug)]
    struct CountRow {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }

    fn count(conn: &mut PgConnection, sql: &str) -> i64 {
        use diesel::RunQueryDsl;
        diesel::sql_query(sql)
            .get_result::<CountRow>(conn)
            .expect("count query")
            .n
    }

    #[derive(QueryableByName, Debug)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }

    fn ids(conn: &mut PgConnection, sql: &str) -> Vec<i64> {
        use diesel::RunQueryDsl;
        diesel::sql_query(sql)
            .load::<IdRow>(conn)
            .expect("id query")
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    fn reset_stats(conn: &mut PgConnection) {
        conn.batch_execute("SELECT pg_stat_statements_reset()")
            .expect("reset pg_stat_statements");
    }

    #[derive(QueryableByName, Debug)]
    struct StatementRow {
        #[diesel(sql_type = Text)]
        query: String,
        #[diesel(sql_type = BigInt)]
        calls: i64,
        #[diesel(sql_type = BigInt)]
        buffers: i64,
    }

    /// Prints every statement this run issued, ranked by buffers and by
    /// calls, and returns `(calls, buffers, workload_total_buffers)` for the
    /// `mark_dead` write this harness targets -- an `UPDATE` naming
    /// `bookmarks` and `alive`.
    fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64) {
        use diesel::RunQueryDsl;
        println!("\n=== pg_stat_statements: {label} (by buffers) ===");
        let by_buffers = diesel::sql_query(
            "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
             FROM pg_stat_statements \
             WHERE query NOT ILIKE '%pg_stat_statements%' \
             ORDER BY buffers DESC LIMIT 10",
        )
        .load::<StatementRow>(conn)
        .expect("query pg_stat_statements by buffers");
        for row in &by_buffers {
            let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
            println!(
                "calls={:<6} buffers={:<10} {normalized}",
                row.calls, row.buffers
            );
        }

        println!("\n=== pg_stat_statements: {label} (by calls) ===");
        let by_calls = diesel::sql_query(
            "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
             FROM pg_stat_statements \
             WHERE query NOT ILIKE '%pg_stat_statements%' \
             ORDER BY calls DESC LIMIT 10",
        )
        .load::<StatementRow>(conn)
        .expect("query pg_stat_statements by calls");
        for row in &by_calls {
            let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
            println!(
                "calls={:<6} buffers={:<10} {normalized}",
                row.calls, row.buffers
            );
        }

        let grand_total_all: i64 = diesel::sql_query(
            "SELECT COALESCE(SUM(shared_blks_hit + shared_blks_read), 0)::bigint AS n \
             FROM pg_stat_statements WHERE query NOT ILIKE '%pg_stat_statements%'",
        )
        .get_result::<CountRow>(conn)
        .expect("grand total buffers")
        .n;

        let target_rows = diesel::sql_query(
            "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
             FROM pg_stat_statements \
             WHERE query ILIKE 'UPDATE%bookmarks%SET%alive%'",
        )
        .load::<StatementRow>(conn)
        .expect("query the target statement");
        let target_calls: i64 = target_rows.iter().map(|r| r.calls).sum();
        let target_buffers: i64 = target_rows.iter().map(|r| r.buffers).sum();
        println!(
            "\n-- mark_dead write: calls={target_calls} buffers={target_buffers} \
             (workload total buffers={grand_total_all}) -- shapes: {target_rows:?}"
        );
        (target_calls, target_buffers, grand_total_all)
    }

    #[derive(QueryableByName, Debug)]
    struct ExplainLine {
        #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
        line: String,
    }

    fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
        use diesel::RunQueryDsl;
        println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} ===");
        println!("{sql}");
        let lines = diesel::sql_query(format!(
            "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) {sql}"
        ))
        .load::<ExplainLine>(conn)
        .expect("explain");
        for line in lines {
            println!("{}", line.line);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn link_checker_batches_mark_dead_into_one_statement_per_shard() {
        let container = Postgres::default()
            .with_tag("16-alpine")
            .with_cmd([
                "-c",
                "fsync=off",
                "-c",
                "shared_preload_libraries=pg_stat_statements",
                "-c",
                "pg_stat_statements.track=all",
                "-c",
                "pg_stat_statements.max=2000",
            ])
            .start()
            .await
            .expect("failed to start postgres container");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let migrate_url = url.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = diesel::PgConnection::establish(&migrate_url).expect("sync connection");
            conn.run_pending_migrations(crate::MIGRATIONS)
                .expect("apply bookmarks-distributed migrations");
        })
        .await
        .expect("migration task");

        let mut conn = PgConnection::establish(&url).expect("sync db connection");
        conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
            .expect("create pg_stat_statements extension");

        let (mock_port, mock_handle) = spawn_probe_mock().await;
        seed_fixture(&mut conn, mock_port);

        let total = count(&mut conn, "SELECT COUNT(*) AS n FROM bookmarks");
        assert_eq!(total, TOTAL_BOOKMARKS, "seeded bookmark count");

        let historical_dead_ids = ids(
            &mut conn,
            "SELECT id AS id FROM bookmarks WHERE alive = false ORDER BY id",
        );
        let historical_dead = i64::try_from(historical_dead_ids.len()).unwrap();
        let expected_dead_ids = ids(
            &mut conn,
            "SELECT id AS id FROM bookmarks WHERE alive = true AND url LIKE '%/dead/%' ORDER BY id",
        );
        let still_alive = count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM bookmarks WHERE alive = true AND url LIKE '%/alive/%'",
        );
        assert_eq!(
            historical_dead + i64::try_from(expected_dead_ids.len()).unwrap() + still_alive,
            TOTAL_BOOKMARKS,
            "every row falls into exactly one fixture category"
        );

        println!(
            "\n-- fixture: {TOTAL_BOOKMARKS} bookmarks, {historical_dead} already dead, \
             {} rotted since the last run (will be probed dead this run), {still_alive} stay alive --",
            expected_dead_ids.len()
        );

        let config = crate::config::DistributedConfig::from_urls(&url, &url).with_pool_sizes(5, 5);
        let pools = create_dual_pools(&config).expect("test pools should build");
        Arc::new(DistributedState::new(config, pools))
            .install_global()
            .expect("distributed state should install");

        let client = TestApp::new().build();
        let state = client.state().clone();

        reset_stats(&mut conn);

        super::check_links(state)
            .await
            .expect("check_links should run to completion");

        mock_handle.abort();

        let (target_calls, target_buffers, workload_buffers) =
            print_profile(&mut conn, "one full 16-shard link-checker run");

        let buffers_pct = 100.0 * target_buffers as f64 / workload_buffers.max(1) as f64;
        println!(
            "\n-- mark_dead write: calls={target_calls} buffers={target_buffers} \
             ({buffers_pct:.1}% of {workload_buffers} total workload buffers) --"
        );

        // Characterizes the CURRENT (pre-fix) defect: one `UPDATE` per dead
        // link found, not one per shard. `LINK_CHECKER_SHARD_COUNT` (16) is
        // the post-fix ceiling this same assertion will drop to.
        assert_eq!(
            target_calls,
            i64::try_from(expected_dead_ids.len()).unwrap(),
            "pre-fix: mark_dead issues exactly one UPDATE per dead link, not one per shard \
             (post-fix ceiling is LINK_CHECKER_SHARD_COUNT = {LINK_CHECKER_SHARD_COUNT})"
        );

        let final_dead_ids = ids(
            &mut conn,
            "SELECT id AS id FROM bookmarks WHERE alive = false ORDER BY id",
        );
        // Built entirely from PRE-run queries (`historical_dead_ids`,
        // `expected_dead_ids` above) rather than re-derived from
        // post-run state: by the time this line runs, `check_links` has
        // already flipped every `/dead/*` row's `alive` flag to `false`,
        // so a post-run `url LIKE '%/dead/%' AND alive = true` query would
        // vacuously return nothing.
        let mut expected_final_dead = historical_dead_ids.clone();
        expected_final_dead.extend(expected_dead_ids.iter().copied());
        expected_final_dead.sort_unstable();
        assert_eq!(
            final_dead_ids, expected_final_dead,
            "result equivalence: exactly the historical-dead rows plus exactly the rows this \
             run's probe found unreachable end up alive=false, no more, no less"
        );

        explain(
            &mut conn,
            "mark_dead: single-row UPDATE (pre-fix shape, one per dead link)",
            &format!(
                "UPDATE bookmarks SET alive = false WHERE id = {}",
                expected_dead_ids.first().copied().unwrap_or(1)
            ),
        );

        // A small, isolated batch inserted AFTER the measured run completes,
        // purely to give the batched EXPLAIN below real `alive = true` rows
        // to match against without perturbing the fixture/counters above.
        conn.batch_execute(
            "INSERT INTO bookmarks (url, title, tag, alive, created_at) \
             SELECT 'http://127.0.0.1/explain-demo/' || gs, 'Explain demo ' || gs, \
               'explain-demo', true, NOW() \
             FROM generate_series(1, 3) AS gs",
        )
        .expect("seed explain-demo rows");
        let demo_ids = ids(
            &mut conn,
            "SELECT id AS id FROM bookmarks WHERE tag = 'explain-demo' ORDER BY id",
        );
        let demo_ids_sql = demo_ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        explain(
            &mut conn,
            "mark_dead_many: batched UPDATE (this PR's shape, one per shard)",
            &format!(
                "UPDATE bookmarks SET alive = false \
                 WHERE (id = ANY(ARRAY[{demo_ids_sql}])) AND alive = true"
            ),
        );
    }
}
