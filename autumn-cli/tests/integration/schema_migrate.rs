//! Integration tests for `autumn schema migrate` / `autumn schema doctor`
//! (issue #1975 slice 6), Postgres path.
//!
//! These require Docker (via testcontainers) and are marked `#[ignore]`, so they
//! only run when explicitly requested:
//!
//!   `cargo test -p autumn-cli --test cli_tests -- --ignored schema_migrate`
//!
//! The happy path proves the headline behaviour: a generated migration is
//! applied against a live Postgres, a second run reports up-to-date, the schema
//! snapshot is refreshed to the applied state, and no credential ever leaks into
//! the CLI output.

use std::path::{Path, PathBuf};
use std::process::Command;

const SECRET_PW: &str = "s3cr3t_pw_do_not_leak";

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

/// `autumn new <name>` in a fresh tempdir, returning that tempdir + project root.
fn fresh_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name], &[]);
    let project = tmp.path().join(name);
    (tmp, project)
}

async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
    u16,
) {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .with_password(SECRET_PW)
        .start()
        .await
        .expect("failed to start Postgres testcontainer — is Docker running?");
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (container, host, port)
}

/// Overwrite the project's single-file models with a `Post` model and remove any
/// scaffolded `src/models/` directory so the single-file layout resolves.
fn write_post_model(project: &Path) {
    let src = project.join("src");
    let _ = std::fs::remove_dir_all(src.join("models"));
    std::fs::write(
        src.join("models.rs"),
        "#[autumn_web::model(managed)]\npub struct Post {\n    #[id]\n    pub id: i64,\n    pub title: String,\n}\n",
    )
    .expect("write models.rs");
}

fn assert_no_secret_leak(stdout: &str, stderr: &str) {
    assert!(
        !stdout.contains(SECRET_PW) && !stderr.contains(SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_migrate_applies_generated_migration_and_refreshes_snapshot() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("migrate_app");

    // 1. A `Post` model plus an EMPTY baseline snapshot, so the diff produces a
    //    CREATE TABLE migration. The empty baseline comes from snapshotting an
    //    empty models file (portable, no `/dev/null` dependency).
    write_post_model(&project);
    std::fs::write(project.join("empty_models.rs"), "").expect("empty models");
    run_autumn_ok(
        &project,
        &[
            "schema",
            "snapshot",
            "--from",
            "empty_models.rs",
            "--backend",
            "pg",
        ],
        &envs,
    );
    // Sanity: doctor sees drift now (models diverge from the empty snapshot).
    let (doc_json, _) = run_autumn_ok(&project, &["schema", "doctor", "--json"], &envs);
    assert!(
        doc_json.contains("\"name\": \"snapshot-drift\"") && doc_json.contains("\"WARN\""),
        "doctor should report drift before migrating:\n{doc_json}"
    );

    // 2. Generate the migration from the diff.
    let (diff_out, _) = run_autumn_ok(
        &project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "create_posts",
        ],
        &envs,
    );
    assert!(diff_out.contains("wrote migration"), "diff: {diff_out}");

    // 3. Apply it. Reports the applied migration, refreshes the snapshot, exit 0.
    let (mig_out, mig_err) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        mig_out.contains("Applied 1 migration(s)."),
        "migrate stdout: {mig_out}"
    );
    assert!(
        mig_out.contains("refreshed schema snapshot"),
        "snapshot refresh reported: {mig_out}"
    );
    assert_no_secret_leak(&mig_out, &mig_err);

    // 4. The snapshot advanced to the applied state (now contains `posts`).
    let snap =
        std::fs::read_to_string(project.join(".autumn/schema-snapshot.json")).expect("snapshot");
    assert!(snap.contains("\"name\": \"posts\""), "snapshot: {snap}");

    // 5. A second migrate is a clean no-op ("already up to date").
    let (rerun_out, rerun_err) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        rerun_out.contains("Database already up to date."),
        "second migrate: {rerun_out}"
    );
    assert_no_secret_leak(&rerun_out, &rerun_err);

    // 6. Doctor is now clean: drift OK, pending-migrations OK, exit 0.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("[OK]") && doc_out.contains("models match the snapshot baseline"),
        "doctor after migrate:\n{doc_out}"
    );
    assert!(
        doc_out.contains("database is up to date"),
        "doctor pending check clean:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}
