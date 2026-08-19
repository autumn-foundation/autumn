//! Ordered-list `position` field scaffolding (issue #1358).
//!
//! Exercises the DSL token end to end: scaffolding a model with a `position`
//! field emits a `#[repository(..., position(...))]` attribute (which is what
//! generates the `move_to`/`move_before`/`move_after`/`move_up`/`move_down`
//! repository methods), a `#[position]`-marked model field, and — on the
//! plain (non-`--api`/`--live`/`--sharded`/owner-scoped) HTML scaffold — an
//! index ordered by the position column with no-JS Move up/down buttons.
//!
//! The `*_generated_project_cargo_checks` tests are the load-bearing ones:
//! they actually compile the generated project against this workspace's
//! `autumn-web`, which is the only way to catch a genuine type/borrow error
//! in the repository macro's hand-written `quote!` codegen (the other tests
//! here only assert on the generated *text*).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn_ok(dir: &Path, args: &[&str]) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
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

fn run_autumn_err(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    assert!(
        !output.status.success(),
        "autumn {args:?} unexpectedly succeeded"
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `autumn new` + `generate scaffold Task <cols> [extra_flags]`, returning the
/// tempdir guard and the generated project root.
fn scaffold_project(
    name: &str,
    fields: &[&str],
    extra_flags: &[&str],
) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec!["generate", "scaffold", "Task"];
    args.extend_from_slice(fields);
    args.extend_from_slice(extra_flags);
    run_autumn_ok(&project, &args);
    (tmp, project)
}

fn read(project: &Path, rel: &str) -> String {
    fs::read_to_string(project.join(rel)).unwrap_or_else(|e| panic!("failed to read {rel}: {e}"))
}

#[test]
fn unscoped_position_field_wires_repository_attribute() {
    let (_tmp, project) = scaffold_project("pos-unscoped", &["title:String", "rank:position"], &[]);
    let repo = read(&project, "src/repositories/task.rs");
    assert!(
        repo.contains("position(column = \"rank\")"),
        "expected position(column = \"rank\") in the #[repository(...)] attribute:\n{repo}"
    );
    assert!(
        !repo.contains("scope ="),
        "an unscoped position field must not emit a scope: {repo}"
    );

    let model = read(&project, "src/models/task.rs");
    assert!(
        model.contains("#[position]\n    pub rank: i64,"),
        "expected #[position]-marked field in the model struct:\n{model}"
    );

    // Migration dir name is timestamped; find it.
    let migrations = project.join("migrations");
    let dir = fs::read_dir(&migrations)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().contains("create_tasks"))
        .expect("create_tasks migration dir");
    let up_sql_rel = format!("migrations/{}/up.sql", dir.file_name().to_string_lossy());
    let up_sql = read(&project, &up_sql_rel);
    assert!(
        up_sql.contains("rank BIGINT NOT NULL DEFAULT 0"),
        "expected rank column with placeholder default:\n{up_sql}"
    );
    assert!(
        up_sql.contains("CREATE TRIGGER") && up_sql.contains("tasks_rank_assign"),
        "expected the insert-assign trigger:\n{up_sql}"
    );
    assert!(
        up_sql.contains("tasks_rank_compact"),
        "expected the delete-compact trigger:\n{up_sql}"
    );
}

// Test inputs like `"rank:position{scope:board_id}"` are literal DSL tokens
// passed to the CLI, not format strings — the `{…}` is the DSL's own
// constraint-modifier syntax under test.
#[allow(clippy::literal_string_with_formatting_args)]
#[test]
fn scoped_position_field_wires_scope_into_repository_attribute() {
    let (_tmp, project) = scaffold_project(
        "pos-scoped",
        &[
            "title:String",
            "board:references",
            "rank:position{scope:board_id}",
        ],
        &[],
    );
    let repo = read(&project, "src/repositories/task.rs");
    assert!(
        repo.contains("position(column = \"rank\", scope = \"board_id\")"),
        "expected scoped position attribute:\n{repo}"
    );
}

#[test]
fn only_one_position_field_allowed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "pos-double"]);
    let project = tmp.path().join("pos-double");
    let stderr = run_autumn_err(
        &project,
        &[
            "generate",
            "scaffold",
            "Task",
            "title:String",
            "rank:position",
            "rank2:position",
        ],
    );
    assert!(
        stderr.contains("position"),
        "expected an error naming the duplicate position field: {stderr}"
    );
}

#[test]
fn index_view_orders_by_position_and_renders_move_buttons() {
    let (_tmp, project) = scaffold_project("pos-index", &["title:String", "rank:position"], &[]);
    let routes = read(&project, "src/routes/tasks.rs");
    assert!(
        routes.contains("list_query.sort().is_none()") && routes.contains("Some(\"rank\")"),
        "expected the index to default sort to the position column:\n{routes}"
    );
    assert!(
        routes.contains("paths::move_up(row.id)") && routes.contains("paths::move_down(row.id)"),
        "expected Move up/down buttons in the index columns:\n{routes}"
    );
    assert!(
        routes.contains("pub async fn move_up(") && routes.contains("pub async fn move_down("),
        "expected move_up/move_down route handlers:\n{routes}"
    );
    assert!(
        routes.contains("repo.move_up(row.id)") && routes.contains("repo.move_down(row.id)"),
        "expected the handlers to call through to the repository:\n{routes}"
    );

    let main_rs = read(&project, "src/main.rs");
    assert!(
        main_rs.contains("routes::tasks::move_up") && main_rs.contains("routes::tasks::move_down"),
        "main.rs must mount the reorder routes:\n{main_rs}"
    );
}

#[test]
fn api_scaffold_omits_reorder_buttons_but_keeps_repository_methods() {
    let (_tmp, project) =
        scaffold_project("pos-api", &["title:String", "rank:position"], &["--api"]);
    assert!(
        !project.join("src/routes/tasks.rs").exists(),
        "an --api scaffold must not emit an HTML routes file"
    );
    let repo = read(&project, "src/repositories/task.rs");
    assert!(
        repo.contains("position(column = \"rank\")"),
        "the repository still gets move_* methods even without the HTML buttons:\n{repo}"
    );
}

#[test]
fn i18n_scaffold_translates_reorder_buttons_and_flash() {
    // The Move up/down buttons and the "moved" flash are generated Rust
    // source, not covered by any autumn-web widget's own i18n — so `--i18n`
    // must route them through `t!(locale, "...")` like every other
    // generated label, or they'd be the one surface still hardcoding
    // English on an otherwise-translated scaffold.
    let (_tmp, project) =
        scaffold_project("pos-i18n", &["title:String", "rank:position"], &["--i18n"]);
    let routes = read(&project, "src/routes/tasks.rs");
    assert!(
        routes.contains("t!(locale, \"common.move_up\")"),
        "expected the Move up button to look up its label via t!: {routes}"
    );
    assert!(
        routes.contains("t!(locale, \"common.move_down\")"),
        "expected the Move down button to look up its label via t!: {routes}"
    );
    assert!(
        routes.contains("t!(locale, \"task.flash.moved\")"),
        "expected the moved flash to look up its message via t!: {routes}"
    );
    assert!(
        !routes.contains("\"Move up\"") && !routes.contains("\"Move down\""),
        "the raw English button labels must not survive --i18n: {routes}"
    );

    let ftl = read(&project, "i18n/en.ftl");
    assert!(
        ftl.contains("common.move_up") && ftl.contains("common.move_down"),
        "the shared Move up/down keys must be backfilled into en.ftl: {ftl}"
    );
    assert!(
        ftl.contains("task.flash.moved"),
        "the per-resource moved-flash key must be backfilled into en.ftl: {ftl}"
    );
}

#[test]
fn sharded_scaffold_rejects_position() {
    // `position(...)` explicitly rejects `sharded` at macro-expansion time
    // (see autumn-macros/src/repository.rs): a move only reaches the
    // pool/shard it happens to be given. An earlier version of this
    // generator omitted `position(...)` from the sharded repository
    // attribute and proceeded with just a warning, still emitting the
    // column and migration triggers — but a Codex review round on issue
    // #1358 caught that this left `delete_many`'s single-row-chunking fix
    // for the batch-compaction race unable to see `config.position` (the
    // sharded attribute no longer carries it), so a sharded model's bulk
    // delete kept its full-size chunks against triggers still vulnerable to
    // the same race. The combination is now rejected outright at generate
    // time instead.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "pos-sharded"]);
    let project = tmp.path().join("pos-sharded");
    let stderr = run_autumn_err(
        &project,
        &[
            "generate",
            "scaffold",
            "Task",
            "title:String",
            "rank:position",
            "--sharded",
            "--shard-key",
            "title",
        ],
    );
    assert!(
        stderr.contains("position") && stderr.contains("sharded"),
        "expected an error naming the position+sharded combination: {stderr}"
    );
    assert!(
        !project.join("src/repositories/task.rs").exists(),
        "a rejected scaffold must not partially generate files: {stderr}"
    );
}

/// Point a generated project at this workspace's `autumn-web` rather than a
/// published version, so a compile of the generated code exercises the code
/// under test.
fn patch_autumn_web_path(project: &Path) {
    let cargo_toml_path = project.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/")
    );
    fs::write(&cargo_toml_path, content).unwrap();
}

fn assert_project_cargo_checks(project: &Path) {
    patch_autumn_web_path(project);

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// The emitted `move_to`/`move_before`/`move_after`/`move_up`/`move_down`
/// repository methods, the model's `#[position]` field, and the scaffold's
/// `index`/`move_up`/`move_down` handlers must actually compile — unscoped.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn unscoped_position_generated_project_cargo_checks() {
    let (_tmp, project) = scaffold_project(
        "pos-check-unscoped",
        &["title:String", "rank:position"],
        &[],
    );
    assert_project_cargo_checks(&project);
}

// Test inputs like `"rank:position{scope:board_id}"` are literal DSL tokens
// passed to the CLI, not format strings — the `{…}` is the DSL's own
// constraint-modifier syntax under test.
#[allow(clippy::literal_string_with_formatting_args)]
/// Same as above, for the scoped (`position{scope:board_id}`) variant — a
/// structurally different codegen path in `position_impl_methods` (the
/// `scope_ident`-present branch).
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn scoped_position_generated_project_cargo_checks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "pos-check-scoped"]);
    let project = tmp.path().join("pos-check-scoped");
    run_autumn_ok(&project, &["generate", "scaffold", "Board", "title:String"]);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Task",
            "title:String",
            "board:references",
            "rank:position{scope:board_id}",
        ],
    );
    assert_project_cargo_checks(&project);
}

/// Soft-delete composes with `position`: the migration's compaction trigger
/// gains its `deleted_at`-transition branch, and the model/repository still
/// compile with both `#[soft_delete]` and `position(...)` on the same
/// `#[repository(...)]` attribute.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn soft_delete_position_generated_project_cargo_checks() {
    let (_tmp, project) = scaffold_project(
        "pos-check-soft",
        &["title:String", "rank:position"],
        &["--soft-delete"],
    );
    let repo = read(&project, "src/repositories/task.rs");
    assert!(
        repo.contains("soft_delete") && repo.contains("position(column = \"rank\")"),
        "expected both soft_delete and position on the repository attribute:\n{repo}"
    );
    assert_project_cargo_checks(&project);
}
