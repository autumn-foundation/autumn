//! Ledger findings/fix harness for `autumn generate admin`'s generated
//! `{Model}Admin::execute_action` bulk `"delete"` action
//! (`autumn-cli/src/generate/admin.rs`, `render_admin_file`).
//!
//! Every model an app scaffolds with `autumn generate admin` gets an
//! `AdminModel` impl that (before this fix) did NOT override
//! `execute_action`, so it inherited the trait's default (`traits.rs`):
//! `for id in ids { self.delete(&pool, id).await?; count += 1; }` -- one
//! `pool.get()` + single-row `DELETE ... WHERE id = $1` round trip **per
//! id**. This is the exact shape already closed, one hand-written admin
//! model at a time, by `TokenAdminModel`
//! (`docs/reports/2026-08-31-ledger-admin-bulk-delete-batch/`) and
//! `FeatureFlagAdminModel`
//! (`docs/reports/2026-09-06-ledger-feature-flag-admin-bulk-delete-batch/`)
//! -- but every one of those was a bespoke fix to a single built-in model.
//! This harness targets the CODE GENERATOR itself: `render_admin_file`'s
//! output is what every `autumn generate admin`-scaffolded app in the wild
//! runs, so fixing the template closes the gap for all of them at once,
//! not just the framework's own three bundled models.
//!
//! Drives the REAL generated code, compiled and run in place: `autumn new`
//! -> `generate scaffold` -> `generate admin` -> wire `autumn-admin-plugin`
//! in (exactly as `docs/guide/admin.md` instructs and
//! `scaffold_encrypted.rs::encrypted_admin_scaffold_cargo_checks` already
//! proves compiles) -> a tiny injected test-only probe module that calls
//! the generated `PostAdmin::execute_action(&pool, "delete", ids)` --
//! exactly what `POST /admin/posts/actions`
//! (`autumn-admin-plugin/src/routes.rs`, `model_action`) calls, skipping
//! only the HTTP form-decoding step, same as the three precedent
//! harnesses -- against a production-shaped `posts` table in real
//! Postgres.
//!
//! **Requires Docker AND compiles two fresh crates** (this crate's own
//! `autumn` binary is already built by `cargo test`; the harness then
//! generates and compiles a whole separate scaffolded project against this
//! workspace's local `autumn-web`/`autumn-admin-plugin`). Slower than a
//! typical Docker-gated test in this sweep -- CLAUDE.md's own guidance for
//! a test that is unavoidably both Docker-gated and cold-start-compile is
//! to accept it running in the (slower, but not silently dark) Docker
//! sweep rather than invent new CI wiring for a single test.
//!
//! ```text
//! cargo test -p autumn-cli --test cli_tests -- --ignored \
//!   admin_generator_bulk_delete_batches_the_default_execute_action \
//!   --nocapture --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection as _, PgConnection, QueryableByName, RunQueryDsl as _, sql_query};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn_ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("failed to run autumn");
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Point the generated project's `autumn-web` dependency at this checkout,
/// add `autumn-admin-plugin` as a real (not published) dependency, mirroring
/// `docs/guide/admin.md`'s wiring instructions and
/// `scaffold_encrypted.rs::encrypted_admin_scaffold_cargo_checks`.
fn wire_admin_plugin_and_patch_autumn_web(project: &Path) {
    use std::fmt::Write as _;

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root
        .join("autumn")
        .display()
        .to_string()
        .replace('\\', "/");
    let admin_plugin = workspace_root
        .join("autumn-admin-plugin")
        .display()
        .to_string()
        .replace('\\', "/");

    let cargo_toml_path = project.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&cargo_toml_path).unwrap();
    content = content.replacen(
        "[dependencies]",
        &format!("[dependencies]\nautumn-admin-plugin = {{ path = \"{admin_plugin}\" }}"),
        1,
    );
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{autumn_web}\" }}\n"
    );
    std::fs::write(&cargo_toml_path, content).unwrap();
}

/// The probe module: a test-only unit test, compiled into the generated
/// project's own binary, that calls the generated `PostAdmin::execute_action`
/// directly (bypassing only the HTTP layer, same as the three precedent
/// harnesses). Reads its target DB and id selection from env vars the outer
/// harness sets before invoking `cargo test`, and prints
/// `LEDGER_RESULT=<applied>` so the outer harness can read the return value
/// back out of the subprocess.
const PROBE_SOURCE: &str = "\
//! Ledger profiling probe (test-only, not part of the generated admin
//! adapter) -- added by admin_generator_bulk_delete_profile.rs to exercise
//! PostAdmin::execute_action in place against a live Postgres.

#[cfg(test)]
mod tests {
    use autumn_admin_plugin::AdminModel;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;

    #[tokio::test]
    async fn ledger_execute_action_probe() {
        let url = std::env::var(\"LEDGER_DB_URL\").expect(\"LEDGER_DB_URL must be set\");
        let ids: Vec<i64> = std::env::var(\"LEDGER_IDS\")
            .expect(\"LEDGER_IDS must be set\")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().expect(\"id must be i64\"))
            .collect();

        let manager = AsyncDieselConnectionManager::<diesel_async::AsyncPgConnection>::new(&url);
        let pool = Pool::builder(manager).max_size(5).build().expect(\"pool\");

        let model = crate::admin::post::PostAdmin;
        let applied = model
            .execute_action(&pool, \"delete\", ids)
            .await
            .expect(\"execute_action\");
        println!(\"LEDGER_RESULT={applied}\");
    }
}
";

fn wire_probe(project: &Path) {
    std::fs::write(
        project.join("src/ledger_admin_bulk_delete_probe.rs"),
        PROBE_SOURCE,
    )
    .unwrap();
    let main_path = project.join("src/main.rs");
    let main = std::fs::read_to_string(&main_path).unwrap();
    // `mod admin;` is the first line `generate admin` leaves for the
    // developer to add (per docs/guide/admin.md); prepend both module
    // declarations together so `crate::admin::post::PostAdmin` and the
    // probe's own `#[cfg(test)]` module both compile into the same binary
    // crate root (this project has no `src/lib.rs` -- `autumn new` is
    // bin-only -- so an external `tests/` integration test cannot reach
    // `crate::admin`; the probe has to live inside the binary itself).
    std::fs::write(
        &main_path,
        format!("mod admin;\nmod ledger_admin_bulk_delete_probe;\n{main}"),
    )
    .unwrap();
}

/// Run the probe's `#[tokio::test]` in the generated project, returning its
/// parsed `LEDGER_RESULT=<n>` line and full stdout+stderr (for diagnostics).
fn run_probe(project: &Path, db_url: &str, ids: &[i64]) -> (u64, String) {
    let ids_csv = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let output = Command::new(env!("CARGO"))
        .args([
            "test",
            "ledger_execute_action_probe",
            "--",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(project)
        .env("LEDGER_DB_URL", db_url)
        .env("LEDGER_IDS", ids_csv)
        .output()
        .expect("failed to run cargo test (probe)");
    let combined = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "probe run failed:\n{combined}");
    // `cargo test`'s own "test <name> ... " status prefix has no trailing
    // newline until the outcome is appended, so with `--nocapture` the
    // probe's own `println!` lands mid-line right after those dots (e.g.
    // `test ... ledger_execute_action_probe ... LEDGER_RESULT=2000`) rather
    // than at line-start — search for the marker anywhere in the line, not
    // just as a prefix.
    let result_line = combined
        .lines()
        .find_map(|l| l.split_once("LEDGER_RESULT=").map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("probe did not print LEDGER_RESULT:\n{combined}"));
    (
        result_line
            .trim()
            .parse()
            .expect("LEDGER_RESULT must be a u64"),
        combined,
    )
}

const TOTAL_ROWS: i64 = 50_000;
/// Every 25th id, `1..=50_000` -> 2,000 ids selected -- all guaranteed to
/// exist and unique (a `BIGSERIAL` primary key on a freshly migrated,
/// never-before-deleted-from table), so the baseline (pre-fix) trait-default
/// loop and the after (fixed) batched form are directly comparable: a
/// missing/duplicate id feeding the OLD loop aborts it early with
/// `AdminError::NotFound` (see module docs and the Equivalence section of
/// the accompanying report), so the primary N-vs-1 statement-count claim
/// deliberately uses a selection where every id exists exactly once. The
/// no-op/duplicate contract the fix establishes is exercised separately by
/// `edge_case_missing_and_duplicate_ids_are_a_safe_no_op` below.
const BULK_IDS_STEP: i64 = 25;

/// Skewed `published` distribution (15% true, matching this repo's other
/// Ledger admin-bulk-delete fixtures' "most rows are stale/long-tail"
/// shape), varying title/body content, and real dead tuples from a
/// follow-up `UPDATE` before `ANALYZE` (no `VACUUM`) -- the fixture shape
/// the Ledger process requires. `posts` has no nullable column (the
/// generator emits `title`/`body`/`published` all `NOT NULL`), so there is
/// no NULL-density axis to vary here.
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO posts (title, body, published, created_at) \
         SELECT \
           'Post title ' || gs || ' ' || repeat('x', (gs % 40)), \
           repeat('body paragraph ' || gs || ' ', 3 + (gs % 12)), \
           (gs % 20) < 3, \
           TIMESTAMP '2025-01-01 00:00:00' + (gs || ' minutes')::interval \
         FROM generate_series(1, {TOTAL_ROWS}) AS gs"
    ))
    .expect("seed posts");

    // Real dead tuples: flip `published` on a slice of rows post-insert.
    conn.batch_execute("UPDATE posts SET published = NOT published WHERE id % 13 = 0")
        .expect("create dead tuples");
    conn.batch_execute("ANALYZE posts").expect("analyze");
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

/// Prints every `posts`-touching statement from this run and returns
/// `(calls, buffers)` for the `DELETE FROM posts` statement shape
/// specifically (isolated from the harness's own read-back queries).
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%posts%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut delete_calls, mut delete_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        // Diesel's query builder quotes identifiers (`DELETE FROM "posts"
        // WHERE ("posts"."id" = ANY($1))`), unlike this harness's own raw
        // `sql_query` diagnostics elsewhere -- match either spelling.
        if normalized.starts_with("DELETE FROM posts")
            || normalized.starts_with("DELETE FROM \"posts\"")
        {
            delete_calls += row.calls;
            delete_buffers += row.buffers;
        }
    }
    println!("-- DELETE FROM posts: calls={delete_calls} buffers={delete_buffers} --");
    (delete_calls, delete_buffers)
}

#[derive(QueryableByName, Debug)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

/// `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` wrapped in a rolled-back
/// transaction so the diagnostic DELETE never actually removes rows the rest
/// of the harness still needs.
fn explain_rollback(conn: &mut PgConnection, label: &str, sql: &str) {
    println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} ===");
    println!("{sql}");
    conn.batch_execute("BEGIN").expect("begin");
    let lines = sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) {sql}"
    ))
    .load::<ExplainLine>(conn)
    .expect("explain");
    for line in &lines {
        println!("{}", line.line);
    }
    conn.batch_execute("ROLLBACK").expect("rollback");
}

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn count_posts(conn: &mut PgConnection, where_clause: &str) -> i64 {
    sql_query(format!(
        "SELECT COUNT(*) AS n FROM posts WHERE {where_clause}"
    ))
    .get_result::<CountRow>(conn)
    .expect("count")
    .n
}

/// Deterministic, sorted dump of the ids still present in `1..=window` --
/// the result-equivalence artifact compared byte-for-byte between the
/// baseline (pre-fix) and after (post-fix) commits.
fn dump_surviving_ids(conn: &mut PgConnection, window: i64) -> String {
    #[derive(QueryableByName, Debug)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    let rows = sql_query(format!(
        "SELECT id FROM posts WHERE id BETWEEN 1 AND {window} ORDER BY id"
    ))
    .load::<IdRow>(conn)
    .expect("dump surviving ids");
    rows.into_iter()
        .map(|r| r.id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
    u16,
) {
    use testcontainers::ImageExt as _;
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .with_tag("17-alpine")
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
        .expect("failed to start Postgres testcontainer — is Docker running?");
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (container, host, port)
}

/// `autumn new` + `generate scaffold Post ...` + `generate admin Post ...` +
/// wiring, returning the tempdir guard and project root.
fn scaffold_wired_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name], &[]);
    let project = tmp.path().join(name);
    let fields = ["title:String", "body:Text", "published:bool"];
    let mut scaffold_args = vec!["generate", "scaffold", "Post"];
    scaffold_args.extend_from_slice(&fields);
    run_autumn_ok(&project, &scaffold_args, &[]);
    let mut admin_args = vec!["generate", "admin", "Post"];
    admin_args.extend_from_slice(&fields);
    run_autumn_ok(&project, &admin_args, &[]);

    wire_admin_plugin_and_patch_autumn_web(&project);
    wire_probe(&project);
    (tmp, project)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn admin_generator_bulk_delete_batches_the_default_execute_action() {
    let (container, host, port) = start_postgres().await;
    let db_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let (_tmp, project) = scaffold_wired_project("ledger_admin_bulk_app");

    run_autumn_ok(
        &project,
        &["migrate"],
        &[("AUTUMN_DATABASE__URL", db_url.as_str())],
    );

    let mut conn = PgConnection::establish(&db_url).expect("connect to postgres");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    seed_fixture(&mut conn);

    // ── Primary measurement: 2,000 ids, every one guaranteed to exist and
    // unique (see BULK_IDS_STEP doc comment for why this selection is kept
    // clean of duplicates/misses) — the N-vs-1 statement-count claim.
    let ids: Vec<i64> = (1..=(TOTAL_ROWS / BULK_IDS_STEP))
        .map(|gs| gs * BULK_IDS_STEP)
        .collect();
    println!(
        "\n-- bulk-delete selection: {} ids, every {BULK_IDS_STEP}th of {TOTAL_ROWS} --",
        ids.len()
    );

    reset_stats(&mut conn);
    let (applied, probe_output) = run_probe(&project, &db_url, &ids);
    println!("{probe_output}");

    assert_eq!(
        applied,
        ids.len() as u64,
        "every selected id exists and is unique, so the rows-actually-deleted \
         count must equal the number of ids submitted"
    );

    let (delete_calls, delete_buffers) = print_profile(&mut conn, "bulk delete 2,000 ids");
    println!(
        "\n-- statement-count claim: {} ids submitted, DELETE FROM posts calls={delete_calls} buffers={delete_buffers} --",
        ids.len()
    );

    // The N+1-elimination floor claim, pinned as an assertion: one bulk
    // action now costs exactly one DELETE statement, regardless of how many
    // ids were submitted — not `ids.len()` calls, one per id, the way the
    // trait-default loop this replaces would have produced (see
    // docs/reports/2026-09-10-ledger-admin-generator-bulk-delete-batch/baseline/).
    assert_eq!(
        delete_calls, 1,
        "the batched execute_action must issue exactly one DELETE statement \
         for the whole bulk action, not one per id"
    );

    let remaining = count_posts(&mut conn, "true");
    assert_eq!(
        remaining,
        TOTAL_ROWS - i64::try_from(ids.len()).expect("ids.len() fits in i64"),
        "exactly the selected ids were removed"
    );
    let ids_csv = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let still_present = count_posts(&mut conn, &format!("id IN ({ids_csv})"));
    assert_eq!(
        still_present, 0,
        "every submitted id must be gone after the bulk action"
    );

    let dump = dump_surviving_ids(&mut conn, 500);
    println!("\n=== surviving ids in 1..=500 (sorted) ===\n{dump}");

    // Illustrative samples drawn from ids NOT in the bulk-delete selection
    // (not multiples of 25), so they still exist at this point and the plan
    // reflects a real row removal rather than a zero-match probe.
    explain_rollback(
        &mut conn,
        "batched DELETE ... WHERE id = ANY($1) (the fixed shape, illustrative 5-id sample)",
        "DELETE FROM posts WHERE id = ANY(ARRAY[1,2,3,4,6]::bigint[])",
    );
    explain_rollback(
        &mut conn,
        "point DELETE by id (the pre-fix loop's per-id statement shape)",
        "DELETE FROM posts WHERE id = 1",
    );

    // ── Edge case, same running container/project (no second generate+
    // compile cycle): ids that don't cleanly exist exactly once. This
    // characterizes the FIXED contract only — the code this replaces aborts
    // the whole bulk action with `AdminError::NotFound` on the first
    // missing id (see module docs), so there is no comparable "before"
    // number here; mixing missing/duplicate ids into the primary
    // measurement above would make ITS before/after statement counts
    // incomparable for reasons unrelated to batching. Fresh ids well past
    // the 50,000-row fixture keep this independent of the primary deletion.
    // `id` is bound explicitly here -- a bare `INSERT ... (title, body,
    // published, created_at)` would let the `id` `BIGSERIAL` default
    // continue from wherever the 50,000-row seed left the sequence
    // (50001..50010), not the 60001..60010 this test's `edge_ids` assume.
    conn.batch_execute(
        "INSERT INTO posts (id, title, body, published, created_at) \
         SELECT gs, 'Edge post ' || gs, 'Edge body ' || gs, false, NOW() \
         FROM generate_series(60001, 60010) AS gs",
    )
    .expect("seed 10 edge-case rows");
    let edge_ids: Vec<i64> = vec![
        60001, 60002, 60003, 60004, 60005, 60003, 60003, 70001, 70002, 70003,
    ];
    let (edge_applied, edge_probe_output) = run_probe(&project, &db_url, &edge_ids);
    println!("{edge_probe_output}");
    assert_eq!(
        edge_applied, 5,
        "returned count must be the number of DISTINCT existing rows actually \
         deleted (5) -- not edge_ids.len() (10), and not an error, even though \
         2 ids repeat and 3 don't exist"
    );
    let edge_remaining = count_posts(&mut conn, "id BETWEEN 60001 AND 60010");
    assert_eq!(
        edge_remaining, 5,
        "the 5 non-selected edge-case rows (60006..=60010) must survive untouched"
    );

    drop(container);
}
