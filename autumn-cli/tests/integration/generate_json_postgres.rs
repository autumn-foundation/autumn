//! Postgres-backed proof of issue #1341's headline acceptance criterion (AC6)
//! and Success Metric: `autumn generate scaffold Setting data:json` produces
//! an app that **generates, compiles, inserts, and selects** a
//! `serde_json::Value` equal to what was written — zero hand-edits to the
//! migration, `schema.rs`, or model.
//!
//! The generator-level tests in `generate::scaffold::tests` (and the
//! `generate::dsl::tests`/`schema_core_parity` unit tests) assert the *shape*
//! of the parsed field and the emitted code as text. This test runs the real
//! `autumn` binary end to end — `new` → `generate scaffold` → `cargo check` →
//! `migrate` — against real Postgres, then inserts/selects a JSON object and a
//! JSON array through the exact migration + `schema.rs` `Jsonb` column type
//! the generator emitted, proving the round-trip the AC describes rather than
//! just the generated text.
//!
//! Requires Docker (via testcontainers) and is marked `#[ignore]`:
//!
//! ```text
//! cargo test -p autumn-cli --test cli_tests -- --ignored generate_json_postgres
//! ```

use std::path::{Path, PathBuf};
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

/// `autumn new <name>` in a fresh tempdir, returning that tempdir + project root.
fn fresh_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name], &[]);
    let project = tmp.path().join(name);
    (tmp, project)
}

/// Point the generated project's `autumn-web` dependency at this checkout so
/// `cargo check` builds against local source rather than crates.io.
fn patch_generated_cargo_toml(project_dir: &Path) {
    use std::fmt::Write as _;
    let cargo_toml_path = project_dir.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/")
    )
    .unwrap();
    std::fs::write(&cargo_toml_path, content).unwrap();
}

/// Run `cargo check` in the generated project, returning success + combined output.
fn cargo_check(project: &Path) -> (bool, String) {
    let output = Command::new(env!("CARGO"))
        .args(["check", "--quiet"])
        .current_dir(project)
        .output()
        .expect("failed to run cargo check");
    let combined = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (output.status.success(), combined)
}

#[derive(QueryableByName)]
struct ConfigRow {
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    config: serde_json::Value,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Jsonb>)]
    extra: Option<serde_json::Value>,
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) + cargo check; run with -- --ignored"]
async fn scaffolded_json_field_generates_compiles_inserts_and_selects() {
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

    let (_tmp, project) = fresh_project("setting_app");
    patch_generated_cargo_toml(&project);

    // Mirrors the issue's own Success Metric literally: `Setting ... :json`,
    // plus a nullable `jsonb` alias field, with zero hand-edits afterward.
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Setting",
            "name:String",
            "config:json",
            "extra:Option<jsonb>",
        ],
        &[],
    );

    // The generated model uses the bare `serde_json::Value` type the AC
    // requires — no wrapper struct — and `schema.rs` uses diesel's `Jsonb`.
    let model = std::fs::read_to_string(project.join("src/models/setting.rs")).unwrap();
    assert!(
        model.contains("pub config: serde_json::Value,"),
        "generated model must carry a bare serde_json::Value field:\n{model}"
    );
    assert!(
        model.contains("pub extra: Option<serde_json::Value>,"),
        "generated model must carry the nullable field:\n{model}"
    );
    let schema = std::fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(
        schema.contains("config -> Jsonb,"),
        "schema.rs must use diesel's Jsonb sql-type:\n{schema}"
    );
    assert!(
        schema.contains("extra -> Nullable<Jsonb>,"),
        "schema.rs must use a nullable Jsonb sql-type:\n{schema}"
    );

    // AC2/AC6: `#[model]`/`#[repository]` (and the whole generated app)
    // compile as-is, with zero hand-edits to the migration/schema/model.
    let (ok, output) = cargo_check(&project);
    assert!(ok, "generated project must compile:\n{output}");

    // Apply the generated migration against real Postgres.
    run_autumn_ok(&project, &["migrate"], &envs);
    let mut conn = PgConnection::establish(&db_url).expect("connect to postgres");

    // Insert a JSON object (config) and a JSON array (extra) through the
    // exact JSONB column the generator's migration created, then select them
    // back and assert byte-for-byte equality — AC6's literal requirement.
    let config = serde_json::json!({"theme": "dark", "retries": 3, "nested": {"a": [1, 2, 3]}});
    let extra = serde_json::json!(["x", "y", "z"]);

    sql_query("INSERT INTO settings (name, config, extra) VALUES ($1, $2, $3)")
        .bind::<diesel::sql_types::Text, _>("prefs")
        .bind::<diesel::sql_types::Jsonb, _>(config.clone())
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Jsonb>, _>(Some(extra.clone()))
        .execute(&mut conn)
        .expect("insert a row with real JSON object/array values");

    let selected = sql_query("SELECT config, extra FROM settings WHERE name = 'prefs'")
        .get_result::<ConfigRow>(&mut conn)
        .expect("select the row back");
    assert_eq!(
        selected.config, config,
        "the JSON object must round-trip byte-for-byte through the generated JSONB column"
    );
    assert_eq!(
        selected.extra,
        Some(extra),
        "the JSON array must round-trip byte-for-byte through the nullable JSONB column"
    );

    // A NULL `extra` also round-trips as `None` (the AC's nullable-awareness).
    sql_query("INSERT INTO settings (name, config) VALUES ($1, $2)")
        .bind::<diesel::sql_types::Text, _>("no-extra")
        .bind::<diesel::sql_types::Jsonb, _>(config)
        .execute(&mut conn)
        .expect("insert a row leaving the nullable column unset");
    let selected_null = sql_query("SELECT config, extra FROM settings WHERE name = 'no-extra'")
        .get_result::<ConfigRow>(&mut conn)
        .expect("select the NULL-extra row back");
    assert_eq!(selected_null.extra, None);
}
