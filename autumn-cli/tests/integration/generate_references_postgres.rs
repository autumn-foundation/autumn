//! Postgres-backed generator test for the `references` field type (issue #1026).
//!
//! Exercises the issue's Success Metric end to end: scaffold a two-table
//! related schema (`Post` + `Comment`, where `Comment` has `post:references`),
//! apply the generated migrations with `autumn migrate`, confirm the foreign
//! key constraint and its index exist in Postgres's catalogs, confirm
//! referential integrity is actually enforced, then roll the migrations back
//! and confirm both tables are gone.
//!
//! Requires Docker (via testcontainers) and is marked `#[ignore]` so it only
//! runs when explicitly requested:
//!
//!   cargo test -p autumn-cli --test `generate_references_postgres` -- --ignored --nocapture

use std::path::Path;
use std::process::Command;

use diesel::{Connection as _, PgConnection, QueryableByName, RunQueryDsl as _, sql_query};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String, Option<i32>) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

fn run_autumn_ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_eq!(
        code,
        Some(0),
        "autumn {args:?} failed (exit={code:?})\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

fn count(conn: &mut PgConnection, sql: &str) -> i64 {
    sql_query(sql)
        .get_result::<CountRow>(conn)
        .unwrap_or_else(|e| panic!("query failed: {sql}\n{e}"))
        .count
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn references_field_creates_fk_and_index_enforces_integrity_and_reverts_cleanly() {
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
        "[package]\nname = \"refs_app\"\n",
    )
    .unwrap();

    // Two related tables: Post (the referenced side) and Comment (the FK side).
    // Migration directories are timestamped to whole-second resolution and
    // applied in filename order, so `Post`'s migration must sort before
    // `Comment`'s — sleep past the second boundary to make that deterministic
    // rather than relying on two process spawns landing in different seconds.
    run_autumn_ok(project, &["generate", "model", "Post", "title:String"], &[]);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    run_autumn_ok(
        project,
        &[
            "generate",
            "model",
            "Comment",
            "body:Text",
            "post:references",
        ],
        &[],
    );

    // Apply both migrations against real Postgres — no manual SQL edits.
    run_autumn_ok(project, &["migrate"], &envs);

    let mut conn = PgConnection::establish(&db_url).expect("connect to postgres");

    // The FK constraint exists on `comments`.
    let fk_count = count(
        &mut conn,
        "SELECT COUNT(*)::bigint AS count FROM information_schema.table_constraints \
         WHERE table_name = 'comments' AND constraint_type = 'FOREIGN KEY'",
    );
    assert_eq!(
        fk_count, 1,
        "expected exactly one FK constraint on comments"
    );

    // The FK index exists on `comments`.
    let index_count = count(
        &mut conn,
        "SELECT COUNT(*)::bigint AS count FROM pg_indexes \
         WHERE tablename = 'comments' AND indexname = 'idx_comments_post_id'",
    );
    assert_eq!(index_count, 1, "expected idx_comments_post_id to exist");

    // Referential integrity is enforced: a comment referencing a non-existent
    // post is rejected...
    let bad_insert = sql_query("INSERT INTO comments (body, post_id) VALUES ('orphan', 999999)")
        .execute(&mut conn);
    assert!(
        bad_insert.is_err(),
        "FK constraint should reject a comment referencing a non-existent post"
    );

    // ...but a comment referencing a real post succeeds.
    sql_query("INSERT INTO posts (title) VALUES ('hello')")
        .execute(&mut conn)
        .expect("insert post");
    sql_query("INSERT INTO comments (body, post_id) VALUES ('hi', 1)")
        .execute(&mut conn)
        .expect("insert comment referencing a real post");

    // Reverting both migrations (down.sql) applies and errors cleanly.
    run_autumn_ok(project, &["migrate", "down", "--steps", "2"], &envs);

    let remaining_tables = count(
        &mut conn,
        "SELECT COUNT(*)::bigint AS count FROM information_schema.tables \
         WHERE table_name IN ('posts', 'comments')",
    );
    assert_eq!(
        remaining_tables, 0,
        "both tables (and their FK/index/columns) must be gone after migrate down"
    );
}
