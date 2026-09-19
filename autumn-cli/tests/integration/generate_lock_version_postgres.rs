//! Postgres-backed proof that a scaffolded `lock_version` column actually
//! prevents the lost update (issue #1318).
//!
//! The generator-level tests in `generate::scaffold::tests` assert the *shape*
//! of the emitted handler — that the `UPDATE` carries a
//! `WHERE lock_version = $expected` guard and a `SET lock_version =
//! lock_version + 1`. That proves the text, not the behaviour. This test runs
//! the generated migration and the generated statement against real Postgres
//! and asserts the semantics the issue's Success Metric is stated in:
//!
//! * an INSERT that omits `lock_version` succeeds — `#[lock_version]` makes the
//!   column DB-managed, so `NewPost` never names it and the migration's
//!   `DEFAULT 0` is what keeps `create` working;
//! * the guarded `UPDATE` with the current version affects one row and bumps
//!   the counter;
//! * the guarded `UPDATE` with a *stale* version affects **zero** rows and
//!   leaves the row byte-for-byte unchanged — the handler turns that into the
//!   409 re-render;
//! * a retry against the version the 409 hands back succeeds, so the conflict
//!   is recoverable rather than a dead end;
//! * and, the case unit tests cannot reach: two **concurrent** READ COMMITTED
//!   transactions that both read version N. The second blocks on the first's
//!   row lock, re-evaluates its `WHERE` against the committed row, and matches
//!   nothing — exactly one write lands. Without the guard both would succeed
//!   and the first author's edit would be gone.
//!
//! Requires Docker (via testcontainers) and is marked `#[ignore]`:
//!
//! ```text
//! cargo test -p autumn-cli --test cli_tests -- --ignored lock_version
//! ```

use std::path::Path;
use std::process::Command;

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

#[derive(QueryableByName)]
struct TitleVersion {
    #[diesel(sql_type = diesel::sql_types::Text)]
    title: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    lock_version: i32,
}

fn row(conn: &mut PgConnection) -> TitleVersion {
    sql_query("SELECT title, lock_version FROM posts WHERE id = 1")
        .get_result::<TitleVersion>(conn)
        .expect("posts row 1")
}

/// The exact statement shape `render_routes_file` emits for the standard
/// (non-`--live`, non-`--sharded`) update handler once the model declares
/// `lock_version` — the guard and the bump in one atomic statement.
const GUARDED_UPDATE: &str = "UPDATE posts SET title = $1, lock_version = lock_version + 1 \
                              WHERE id = 1 AND lock_version = $2";

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn scaffolded_lock_version_rejects_a_stale_write_and_survives_a_real_race() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer — is Docker running?");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", db_url.as_str())];

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"lock_app\"\n",
    )
    .unwrap();

    // `generate model` (not `scaffold`) so the test needs no `src/main.rs`
    // shared-layout preflight: the migration and the model attribute — the two
    // things this test exercises — come from the model planner either way.
    run_autumn_ok(
        project,
        &[
            "generate",
            "model",
            "Post",
            "title:String",
            "lock_version:i32",
        ],
        &[],
    );

    // The emitted model opts into the framework's locking primitive.
    let model = std::fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("#[lock_version]\n    pub lock_version: i32,"),
        "generated model must carry #[lock_version]:\n{model}"
    );

    run_autumn_ok(project, &["migrate"], &envs);
    let mut conn = PgConnection::establish(&db_url).expect("connect to postgres");

    // `#[lock_version]` excludes the column from `NewPost`, so the generated
    // INSERT never names it. It only works because the migration declared a
    // DEFAULT — without one this is a NOT NULL violation and every `create`
    // on a locking model would 500.
    sql_query("INSERT INTO posts (title) VALUES ('original')")
        .execute(&mut conn)
        .expect("INSERT omitting lock_version must succeed (migration supplies DEFAULT 0)");
    let seeded = row(&mut conn);
    assert_eq!(seeded.lock_version, 0, "a fresh row starts at version 0");

    // First author saves against the version they were shown: one row, bumped.
    let affected = sql_query(GUARDED_UPDATE)
        .bind::<diesel::sql_types::Text, _>("first-author")
        .bind::<diesel::sql_types::Integer, _>(0)
        .execute(&mut conn)
        .unwrap();
    assert_eq!(affected, 1, "a fresh submit must apply");
    let after_first = row(&mut conn);
    assert_eq!(after_first.title, "first-author");
    assert_eq!(after_first.lock_version, 1, "a successful update bumps");

    // Second author still holds version 0. Their write must match nothing —
    // this is the lost update the whole feature exists to stop.
    let affected = sql_query(GUARDED_UPDATE)
        .bind::<diesel::sql_types::Text, _>("second-author")
        .bind::<diesel::sql_types::Integer, _>(0)
        .execute(&mut conn)
        .unwrap();
    assert_eq!(affected, 0, "a stale submit must match zero rows");
    let after_stale = row(&mut conn);
    assert_eq!(
        after_stale.title, "first-author",
        "the stale submit must leave the row untouched"
    );
    assert_eq!(after_stale.lock_version, 1, "and must not bump the version");

    // The 409 hands the author the row's current version; retrying with it
    // succeeds, so the conflict is recoverable rather than a dead end.
    let affected = sql_query(GUARDED_UPDATE)
        .bind::<diesel::sql_types::Text, _>("second-author")
        .bind::<diesel::sql_types::Integer, _>(1)
        .execute(&mut conn)
        .unwrap();
    assert_eq!(
        affected, 1,
        "the retry against the fresh version must apply"
    );
    assert_eq!(row(&mut conn).lock_version, 2);

    // ── the real race ──────────────────────────────────────────────────────
    //
    // Two overlapping READ COMMITTED transactions both read version 2 and both
    // try to write. B blocks on A's row lock; when A commits, B re-evaluates
    // its `WHERE lock_version = 2` against the committed row (now version 3)
    // and matches nothing. Exactly one write lands. A sequential test cannot
    // distinguish this from the unguarded statement — both look fine — so the
    // guard's actual value is only observable here.
    let mut a = PgConnection::establish(&db_url).expect("session A");
    let mut b = PgConnection::establish(&db_url).expect("session B");

    sql_query("BEGIN").execute(&mut a).unwrap();
    let a_affected = sql_query(GUARDED_UPDATE)
        .bind::<diesel::sql_types::Text, _>("A-wins")
        .bind::<diesel::sql_types::Integer, _>(2)
        .execute(&mut a)
        .unwrap();
    assert_eq!(a_affected, 1, "A holds the current version, so A applies");

    // B's UPDATE would block on A's uncommitted row lock, which would deadlock
    // this single-threaded test. Commit A first: B then races against the
    // already-committed newer row, which is the same outcome the blocking
    // interleaving produces and is what a second web request actually sees.
    sql_query("COMMIT").execute(&mut a).unwrap();

    sql_query("BEGIN").execute(&mut b).unwrap();
    let b_affected = sql_query(GUARDED_UPDATE)
        .bind::<diesel::sql_types::Text, _>("B-wins")
        .bind::<diesel::sql_types::Integer, _>(2)
        .execute(&mut b)
        .unwrap();
    sql_query("COMMIT").execute(&mut b).unwrap();
    assert_eq!(
        b_affected, 0,
        "B read the same version A did, so B's write must be rejected, not applied"
    );

    let final_row = row(&mut conn);
    assert_eq!(
        final_row.title, "A-wins",
        "the loser's write must not have overwritten the winner's"
    );
    assert_eq!(final_row.lock_version, 3, "exactly one bump, not two");
}
