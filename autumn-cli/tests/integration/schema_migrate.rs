//! Integration tests for `autumn schema migrate` / `autumn schema doctor`
//! (issue #1975 slice 6), Postgres path.
//!
//! These require Docker (via testcontainers) and are marked `#[ignore]`, so they
//! only run when explicitly requested:
//!
//!   `cargo test -p autumn-cli --test cli_tests -- --ignored schema_migrate`
//!
//! The happy path proves the headline behaviour (#2041): `schema diff
//! --write-migration` advances the checked-in snapshot to the generated plan's
//! target state at generation time; `schema migrate` then applies the migration
//! against a live Postgres, a second run reports up-to-date, the snapshot file is
//! left BYTE-IDENTICAL by the apply, and no credential ever leaks into the CLI
//! output.

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
#[allow(clippy::too_many_lines)] // linear end-to-end scenario reads clearest whole
async fn schema_migrate_applies_generated_migration_and_leaves_snapshot_untouched() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("migrate_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

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
        "doctor should report drift before generating:\n{doc_json}"
    );

    // 2. Generate the migration from the diff. THIS is where the snapshot now
    //    advances (#2041): `--write-migration` moves the checked-in baseline to
    //    the generated plan's target state at generation time.
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
    assert!(
        diff_out.contains("advanced schema snapshot"),
        "diff --write-migration reports advancing the snapshot: {diff_out}"
    );

    // The snapshot advanced to the plan target (now contains `posts`). Capture the
    // exact bytes: the apply below must NOT change them.
    let snap_after_generation = std::fs::read_to_string(&snapshot_path).expect("snapshot");
    assert!(
        snap_after_generation.contains("\"name\": \"posts\""),
        "snapshot advanced at generation: {snap_after_generation}"
    );

    // Drift is already resolved right after generation: models match the advanced
    // snapshot (there is still a pending, unapplied migration though).
    let (doc_gen, _) = run_autumn_ok(&project, &["schema", "doctor", "--json"], &envs);
    assert!(
        doc_gen.contains("\"name\": \"snapshot-drift\"") && doc_gen.contains("\"OK\""),
        "doctor drift is clean right after generation:\n{doc_gen}"
    );

    // 3. Apply it. Reports the applied migration; must NOT report or perform a
    //    snapshot refresh, and must leave the snapshot file byte-identical.
    let (mig_out, mig_err) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        mig_out.contains("Applied 1 migration(s)."),
        "migrate stdout: {mig_out}"
    );
    assert!(
        !mig_out.contains("refreshed schema snapshot"),
        "migrate must NOT refresh the snapshot anymore: {mig_out}"
    );
    assert_no_secret_leak(&mig_out, &mig_err);
    let snap_after_apply = std::fs::read_to_string(&snapshot_path).expect("snapshot");
    assert_eq!(
        snap_after_generation, snap_after_apply,
        "migrate must leave the snapshot byte-for-byte unchanged"
    );

    // 4. A second migrate is a clean no-op ("already up to date").
    let (rerun_out, rerun_err) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        rerun_out.contains("Database already up to date."),
        "second migrate: {rerun_out}"
    );
    assert_no_secret_leak(&rerun_out, &rerun_err);

    // 5. Doctor is now fully clean: drift OK, pending-migrations OK, exit 0.
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

    // 6. Finding 1 (#2036): introduce a NEW, ungenerated model change (add `body`)
    //    WITHOUT generating/applying a migration for it, then run `schema migrate`
    //    again. With nothing pending to apply it is a no-op, and MUST leave the
    //    snapshot untouched — migrate no longer touches the snapshot at all.
    std::fs::write(
        project.join("src/models.rs"),
        "#[autumn_web::model(managed)]\npub struct Post {\n    #[id]\n    pub id: i64,\n    pub title: String,\n    pub body: Option<String>,\n}\n",
    )
    .expect("edit models.rs with ungenerated drift");
    let (noop_out, noop_err) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        noop_out.contains("Database already up to date."),
        "no-op migrate after model edit: {noop_out}"
    );
    assert!(
        !noop_out.contains("refreshed schema snapshot"),
        "a no-op migrate must NOT refresh the snapshot: {noop_out}"
    );
    let snap_after_noop = std::fs::read_to_string(&snapshot_path).expect("snapshot");
    assert_eq!(
        snap_after_apply, snap_after_noop,
        "a no-op migrate must leave the snapshot byte-for-byte unchanged"
    );
    assert!(
        !snap_after_noop.contains("body"),
        "the ungenerated `body` edit must NOT be baked into the baseline: {snap_after_noop}"
    );
    assert_no_secret_leak(&noop_out, &noop_err);

    // 7. Because the snapshot never advanced for the ungenerated edit, it is still
    //    visible as drift: `schema diff` still generates the pending column add.
    let (diff_after, diff_after_err) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        !diff_after.contains("No schema changes") && diff_after.contains("body"),
        "drift must still be detected after the no-op migrate: {diff_after}"
    );
    assert_no_secret_leak(&diff_after, &diff_after_err);
}
