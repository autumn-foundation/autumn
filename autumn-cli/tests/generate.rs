//! End-to-end integration tests for `autumn generate`.
//!
//! These run the real `autumn` binary against a freshly-`new`-ed project and
//! assert the produced filesystem matches the documented contract — covering
//! the user-facing flow described in [Issue #493].
//!
//! [Issue #493]: https://github.com/autumn-foundation/autumn/issues/493

use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Spawn the production `autumn` binary in `dir` with the given args and
/// assert it exits successfully, returning the captured stdout + stderr.
fn run_autumn(dir: &Path, args: &[&str]) -> (String, String) {
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code(),
    );
    (stdout, stderr)
}

/// Spawn the production `autumn` binary with environment overrides.
fn run_autumn_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("failed to run autumn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code(),
    );
    (stdout, stderr)
}

/// Same as [`run_autumn`] but expects a non-zero exit code.
fn run_autumn_failing(dir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (stdout, stderr, output.status.code())
}

/// `autumn new <name>` in a fresh tempdir, returning that tempdir + the
/// project root inside it.
fn fresh_project(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    (tmp, project)
}

fn patch_generated_cargo_toml(project_dir: &Path) {
    let cargo_toml_path = project_dir.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
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
    fs::write(&cargo_toml_path, content).unwrap();
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child_output(child: &mut Child) -> (String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    (stdout, stderr)
}

// Async HTTP poll used by tests that already run inside a Tokio runtime
// (e.g. #[tokio::test] tests that also drive async testcontainers).
// Using reqwest::blocking inside an existing Tokio runtime panics when the
// blocking client's internal runtime is dropped, so these tests use the
// native async reqwest::Client instead.
async fn wait_for_server_ready_async(
    mut child: Child,
    client: &reqwest::Client,
    base: &str,
) -> ServerGuard {
    for _ in 0..60 {
        if let Some(status) = child.try_wait().expect("server status") {
            let (stdout, stderr) = child_output(&mut child);
            panic!(
                "server exited before becoming ready: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }

        if client.get(format!("{base}/health")).send().await.is_ok() {
            return ServerGuard(child);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let _ = child.kill();
    let _ = child.wait();
    let (stdout, stderr) = child_output(&mut child);
    panic!("server failed to become ready within 30 seconds\nstdout:\n{stdout}\nstderr:\n{stderr}");
}

#[test]
fn generate_model_in_fresh_project() {
    let (_tmp, project) = fresh_project("model-app");
    run_autumn(
        &project,
        &[
            "generate",
            "model",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(model.contains("#[autumn_web::model]"));
    assert!(model.contains("pub struct Post"));
    assert!(model.contains("pub title: String,"));
    assert!(model.contains("pub body: String,"));
    assert!(model.contains("pub published: bool,"));

    let mod_rs = fs::read_to_string(project.join("src/models/mod.rs")).unwrap();
    assert!(mod_rs.contains("pub mod post;"));

    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(schema.contains("posts (id)"));
    assert!(schema.contains("title -> Text,"));

    // The migration directory exists with both up.sql and down.sql.
    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_posts"))
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let dir = migrations[0].path();
    let up = fs::read_to_string(dir.join("up.sql")).unwrap();
    assert!(up.contains("CREATE TABLE posts"));
    assert!(up.contains("title TEXT NOT NULL"));
    assert!(up.contains("published BOOLEAN NOT NULL"));
    assert!(up.contains("id BIGSERIAL PRIMARY KEY"));
    let down = fs::read_to_string(dir.join("down.sql")).unwrap();
    assert!(down.contains("DROP TABLE posts"));
}

#[test]
fn generate_model_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("dryrun-app");
    let (stdout, _stderr) = run_autumn(
        &project,
        &["generate", "model", "Post", "title:String", "--dry-run"],
    );
    assert!(stdout.contains("Dry run"));
    assert!(stdout.contains("src/models/post.rs"));
    assert!(!project.join("src/models/post.rs").exists());
    assert!(!project.join("src/schema.rs").exists());
}

#[test]
fn generate_scaffold_dry_run_api() {
    let (_tmp, project) = fresh_project("dryrun-scaffold-api-app");
    let (stdout, _stderr) = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--api",
            "--dry-run",
        ],
    );
    assert!(stdout.contains("Dry run"));
    assert!(stdout.contains("src/models/post.rs"));
    assert!(!stdout.contains("src/routes/posts.rs"));
    assert!(!stdout.contains("templates"));
}

#[test]
fn generate_model_collision_without_force() {
    let (_tmp, project) = fresh_project("collide-app");
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);
    // Re-run without --force. Should fail with collision message.
    let (_, stderr, code) =
        run_autumn_failing(&project, &["generate", "model", "Post", "title:String"]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("would overwrite") && stderr.contains("post.rs"),
        "expected collision message; got stderr: {stderr}"
    );
}

#[test]
fn generate_model_force_overwrites() {
    let (_tmp, project) = fresh_project("force-app");
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);
    // Modify the model file so we can detect the overwrite.
    let model_path = project.join("src/models/post.rs");
    let original = fs::read_to_string(&model_path).unwrap();
    fs::write(&model_path, "// touched").unwrap();
    run_autumn(
        &project,
        &["generate", "model", "Post", "title:String", "--force"],
    );
    let regenerated = fs::read_to_string(&model_path).unwrap();
    assert_eq!(regenerated, original);
}

#[test]
fn generate_model_invalid_field_lists_supported_set() {
    let (_tmp, project) = fresh_project("badtype-app");
    let (_, stderr, code) =
        run_autumn_failing(&project, &["generate", "model", "Post", "price:Money"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("unsupported type"));
    assert!(stderr.contains("Supported:"));
    assert!(stderr.contains("String"));
}

// ── `references` field type (issue #1026) ───────────────────────────────────

#[test]
fn generate_model_references_field_emits_fk_column_constraint_and_index() {
    let (_tmp, project) = fresh_project("references-app");
    run_autumn(
        &project,
        &[
            "generate",
            "model",
            "Comment",
            "body:Text",
            "post:references",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/comment.rs")).unwrap();
    assert!(model.contains("pub post_id: i64,"));

    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(schema.contains("post_id -> Int8,"));

    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_create_comments")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(up.contains("post_id BIGINT NOT NULL REFERENCES posts(id)"));
    assert!(up.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"));
}

#[test]
fn generate_model_references_warns_when_target_model_missing() {
    let (_tmp, project) = fresh_project("references-warn-app");
    let (_stdout, stderr) = run_autumn(
        &project,
        &["generate", "model", "Comment", "post:references"],
    );
    assert!(
        stderr.contains("Warning") && stderr.contains("posts"),
        "expected a warning about the missing 'post' model; got stderr: {stderr}"
    );
}

#[test]
fn generate_model_references_warning_not_printed_on_a_failed_collision_run() {
    // A run that fails before writing anything (a file collision without
    // --force) must not print advisory warnings for a plan that was never
    // applied — warnings are informational about what *did* happen.
    let (_tmp, project) = fresh_project("references-warn-collision-app");
    run_autumn(
        &project,
        &["generate", "model", "Comment", "post:references"],
    );
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "model", "Comment", "post:references"],
    );
    assert_eq!(code, Some(1));
    assert!(stderr.contains("would overwrite"));
    assert!(
        !stderr.contains("Warning"),
        "no warning should print on a failed, no-op run: {stderr}"
    );
}

#[test]
fn generate_model_references_no_warning_when_target_model_exists() {
    let (_tmp, project) = fresh_project("references-nowarn-app");
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);
    let (_stdout, stderr) = run_autumn(
        &project,
        &["generate", "model", "Comment", "post:references"],
    );
    assert!(
        !stderr.contains("Warning"),
        "no warning expected once the Post model exists; got stderr: {stderr}"
    );
}

#[test]
fn generate_model_references_nullable_form() {
    let (_tmp, project) = fresh_project("references-nullable-app");
    run_autumn(
        &project,
        &["generate", "model", "Comment", "post:references?"],
    );
    let model = fs::read_to_string(project.join("src/models/comment.rs")).unwrap();
    assert!(model.contains("pub post_id: Option<i64>,"));
}

#[test]
fn generate_model_help_documents_references_field() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "model", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("references"));
}

#[test]
fn generate_scaffold_help_documents_references_field() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "scaffold", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("references"));
}

#[test]
fn generate_model_help_documents_enum_field() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "model", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("enum{"));
}

#[test]
fn generate_scaffold_help_documents_enum_field() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "scaffold", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("enum{"));
}

// ── enum field type (issue #1030) ───────────────────────────────────────────

#[test]
fn generate_model_with_enum_writes_check_and_enum_type() {
    let (_tmp, project) = fresh_project("enum-model-app");
    run_autumn(
        &project,
        &[
            "generate",
            "model",
            "Post",
            "title:String",
            "status:enum{draft,published,archived}",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(model.contains("pub enum Status"), "got:\n{model}");
    assert!(model.contains("pub status: Status,"), "got:\n{model}");

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with("_create_posts"))
        .expect("create_posts migration should exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(
        up.contains("status TEXT NOT NULL CHECK (status IN ('draft', 'published', 'archived'))"),
        "got:\n{up}"
    );
}

#[test]
fn generate_migration_add_enum_column_emits_check() {
    let (_tmp, project) = fresh_project("enum-migration-app");
    run_autumn(
        &project,
        &[
            "generate",
            "migration",
            "AddStatusToPosts",
            "status:enum{draft,published}",
        ],
    );

    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_add_status_to_posts")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains(
            "ALTER TABLE posts ADD COLUMN status TEXT NOT NULL CHECK (status IN ('draft', 'published'));"
        ),
        "got:\n{up}"
    );
}

#[test]
fn generate_scaffold_rejects_bad_enum_default() {
    let (_tmp, project) = fresh_project("enum-bad-default-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "status:enum{draft,published,archived}",
            "--default",
            "status=bogus",
        ],
    );

    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("bogus") && stderr.contains("draft") && stderr.contains("archived"),
        "expected an enum-default membership error listing the variants; got stderr: {stderr}"
    );
    assert!(!project.join("src/models/post.rs").exists());
}

#[test]
fn generate_scaffold_rejects_unique_field_with_default() {
    // Regression guard (issue #1032 review follow-up): a `--default` field
    // is excluded from the generated HTML form, so a `unique` column that
    // also has a `--default` would have no `UNIQUE_CONSTRAINTS` entry (and,
    // even if it did, no form input to show a duplicate-value error
    // against). Reject the combination outright at generation time.
    let (_tmp, project) = fresh_project("unique-with-default-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "--default",
            "email='a@b.com'",
        ],
    );

    assert_eq!(code, Some(1));
    assert!(stderr.contains("email"), "got stderr: {stderr}");
    assert!(stderr.contains("unique"), "got stderr: {stderr}");
    assert!(stderr.contains("default"), "got stderr: {stderr}");
    assert!(!project.join("src/models/user.rs").exists());
}

#[test]
fn generate_scaffold_rejects_unique_flag_field_with_default() {
    // Same rejection via the `--unique FIELD` flag path instead of the
    // inline `:unique` DSL marker. `--default` is only a `generate scaffold`
    // flag (`generate model` has no `--default`), so this exercises the
    // combination through the scaffold generator.
    let (_tmp, project) = fresh_project("unique-flag-with-default-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String",
            "--unique",
            "email",
            "--default",
            "email='a@b.com'",
        ],
    );

    assert_eq!(code, Some(1));
    assert!(stderr.contains("email"), "got stderr: {stderr}");
    assert!(stderr.contains("unique"), "got stderr: {stderr}");
    assert!(stderr.contains("default"), "got stderr: {stderr}");
    assert!(!project.join("src/models/user.rs").exists());
}

#[test]
fn generate_scaffold_with_enum_dry_run_lists_files_and_then_collides_without_force() {
    let (_tmp, project) = fresh_project("enum-dryrun-app");
    let (stdout, _stderr) = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "status:enum{draft,published,archived}",
            "--dry-run",
        ],
    );
    assert!(stdout.contains("Dry run"));
    assert!(stdout.contains("src/models/post.rs"));
    assert!(stdout.contains("src/repositories/post.rs"));
    assert!(stdout.contains("src/routes/posts.rs"));
    assert!(stdout.contains("tests/post.rs"));
    assert!(!project.join("src/models/post.rs").exists());

    // A real (non-dry-run) run actually writes the files...
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "status:enum{draft,published,archived}",
        ],
    );
    assert!(project.join("src/models/post.rs").exists());

    // ...and running it again without --force must error on collision rather
    // than silently overwriting.
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "status:enum{draft,published,archived}",
        ],
    );
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("overwrite"),
        "expected a collision error; got stderr: {stderr}"
    );
}

#[test]
fn generate_model_dry_run_lists_migration_file_for_references_field() {
    let (_tmp, project) = fresh_project("references-dryrun-app");
    let (stdout, _stderr) = run_autumn(
        &project,
        &[
            "generate",
            "model",
            "Comment",
            "post:references",
            "--dry-run",
        ],
    );
    assert!(stdout.contains("Dry run"));
    assert!(stdout.contains("migrations/"));
    assert!(stdout.contains("create_comments/up.sql"));
    assert!(!project.join("src/models/comment.rs").exists());
}

#[test]
fn generate_model_references_errors_when_target_has_uuid_pk() {
    let (_tmp, project) = fresh_project("references-uuid-mismatch-app");
    run_autumn(
        &project,
        &["generate", "model", "Post", "--id", "uuid", "title:String"],
    );
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "model", "Comment", "post:references"],
    );
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("UUID"),
        "expected a clear UUID-mismatch error; got stderr: {stderr}"
    );
    assert!(!project.join("src/models/comment.rs").exists());
}

#[test]
fn generate_migration_add_columns_emits_alter() {
    let (_tmp, project) = fresh_project("migrate-app");
    run_autumn(
        &project,
        &["generate", "migration", "AddTitleToPosts", "title:String"],
    );
    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_add_title_to_posts")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(up.contains("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL"));
}

#[test]
fn generate_migration_add_unique_reference_column_skips_redundant_plain_index() {
    // Regression guard (issue #1032 review follow-up): `AddXToY` emitted
    // both a `references` field's auto-index and its `CREATE UNIQUE INDEX`
    // unconditionally, so `author:references:unique` built two overlapping
    // btree indexes on the same column — the plain one is fully redundant
    // since the unique index already covers the same lookup.
    let (_tmp, project) = fresh_project("migrate-unique-reference-app");
    run_autumn(
        &project,
        &[
            "generate",
            "migration",
            "AddAuthorToPosts",
            "author:references:unique",
        ],
    );
    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_add_author_to_posts")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE UNIQUE INDEX idx_posts_author_id_unique ON posts (author_id);"),
        "got:\n{up}"
    );
    assert!(
        !up.contains("CREATE INDEX idx_posts_author_id ON posts (author_id);"),
        "a references field that is also unique must not get a redundant \
         plain index alongside its unique index; got:\n{up}"
    );
}

#[test]
fn generate_migration_unknown_pattern_is_empty() {
    let (_tmp, project) = fresh_project("empty-mig-app");
    run_autumn(&project, &["generate", "migration", "BackfillSomething"]);
    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_backfill_something")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(up.is_empty());
}

#[test]
fn generate_task_emits_task_module() {
    let (_tmp, project) = fresh_project("task-app");
    run_autumn(&project, &["generate", "task", "cleanup_users"]);

    let task = fs::read_to_string(project.join("tasks/cleanup_users.rs")).unwrap();
    assert!(task.contains("#[autumn_web::task]"));
    assert!(task.contains("pub async fn cleanup_users"));
    assert!(task.contains("TaskArgs<CleanupUsersArgs>"));
    assert!(task.contains("AutumnResult<()>"));
}

#[test]
fn generate_scaffold_full_e2e_post() {
    let (_tmp, project) = fresh_project("scaffold-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
            "subtitle:Option<String>",
            "views:Option<i64>",
            "published_at:Option<NaiveDateTime>",
            "token:Option<Uuid>",
        ],
    );

    // Model + migration + schema entry.
    assert!(project.join("src/models/post.rs").is_file());
    assert!(project.join("src/models/mod.rs").is_file());
    assert!(project.join("src/schema.rs").is_file());

    // Repository file.
    let repo = fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();
    assert!(repo.contains("#[autumn_web::repository(Post, api = \"/api/posts\")]"));
    assert!(repo.contains("pub trait PostRepository"));

    // HTML routes — index/show/new/create/edit_form/update; delete goes
    // through the repository's auto-generated JSON REST API.
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    for needle in [
        "#[get(\"/posts\")]",
        "#[get(\"/posts/{id}\")]",
        "#[get(\"/posts/new\", name = \"new\")]",
        "#[post(\"/posts\")]",
        "#[get(\"/posts/{id}/edit\", name = \"edit\")]",
        "#[post(\"/posts/{id}/update\")]",
        "pub async fn index",
        "pub async fn show",
        "pub async fn new_form(",
        "pub async fn update",
        "use autumn_web::security::{CsrfFormField, CsrfToken, SubmitFormField, SubmitToken};",
        "input type=\"hidden\" name=(csrf_field_name)",
        "(csrf_input(csrf.as_ref(), csrf_field.as_ref()))",
        // Issue #1360: one-time submit token wired into create/update forms.
        // The hidden field name follows the configured
        // `security.submit_token.field_name` (via the `SubmitFormField`
        // extractor), not a hardcoded `_submit_token`.
        "fn submit_token_input(",
        "input type=\"hidden\" name=(submit_field_name) value=(submit_token.token());",
        "submit_token: Option<SubmitToken>",
    ] {
        assert!(routes.contains(needle), "routes file missing: {needle}");
    }

    // Smoke test: real, in-process, DB-backed index/read test (issue #1023) --
    // no raw TcpStream, no AUTUMN_TEST_BASE_URL, no silent env-gated skip.
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(test.contains("posts_index_renders_scaffolded_rows"));
    assert!(test.contains("autumn_web::test::{TestApp, TestClient, TestDb}"));
    assert!(!test.contains("TcpStream"));
    assert!(!test.contains("AUTUMN_TEST_BASE_URL"));
    assert!(!test.contains("AUTUMN_TEST_SESSION_COOKIE"));
    assert!(!test.contains("Cookie: {session_cookie}"));
    assert!(test.contains("#[ignore = \"requires Docker"));

    // `routes![]` registration.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("mod models;"));
    assert!(main.contains("mod routes;"));
    assert!(main.contains("mod schema;"));
    assert!(main.contains("mod repositories;"));
    for entry in [
        "routes::posts::index",
        "routes::posts::show",
        "routes::posts::new_form",
        "routes::posts::create",
        "routes::posts::edit_form",
        "routes::posts::update",
        // Issue #1312: the bulk delete-selected route is mounted alongside the
        // per-row destroy for every non-live, non-sharded HTML scaffold.
        "routes::posts::bulk_delete",
        // Issue #1315: the CSV export route is mounted for every scaffold whose
        // index row set is a repository call the export can reuse verbatim.
        "routes::posts::export_csv",
        "repositories::post::post_api_list",
        "repositories::post::post_api_get",
    ] {
        assert!(
            main.contains(entry),
            "main.rs missing routes![] entry: {entry}\n{main}"
        );
    }
    for entry in [
        "repositories::post::post_api_create",
        "repositories::post::post_api_update",
        "repositories::post::post_api_delete",
    ] {
        assert!(
            !main.contains(entry),
            "main.rs should not mount public scaffold write API route: {entry}\n{main}"
        );
    }
}

// ── `autumn destroy` (issue #1048) ─────────────────────────────────────────

/// Spawn the system `git` binary in `dir` (NOT the `autumn` binary — see
/// [`run_autumn`]), asserting success, and return its stdout.
fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run git");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "git {args:?} failed (exit={:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code(),
    );
    stdout
}

/// Recursively collect every file under `root` as `(relative_path, contents)`,
/// sorted for deterministic comparison. Used to assert a project's working
/// tree is byte-for-byte identical before/after a `generate`+`destroy`
/// round-trip — a filesystem-level equivalent of `git status` being clean
/// that doesn't require a `git` binary in the test sandbox.
fn snapshot_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).expect("read_dir").filter_map(Result::ok) {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                // Never compare git's own internals — its background
                // maintenance can create/remove transient files (e.g.
                // `.git/objects/maintenance.lock`) between snapshots,
                // producing spurious diffs unrelated to anything
                // generate/destroy touched.
                continue;
            }
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/");
                let contents = fs::read(&path).expect("read file");
                out.push((rel, contents));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The headline acceptance test (issue #1048's success metric): `autumn
/// generate scaffold Post title:String` immediately followed by `autumn
/// destroy scaffold Post title:String` returns the project's working tree
/// byte-for-byte identical to its pre-generate state — both via a
/// filesystem snapshot comparison and via `git status --porcelain` being
/// empty against a real commit, exactly the round-trip the issue describes.
#[test]
fn generate_then_destroy_scaffold_round_trips_git_clean() {
    let (_tmp, project) = fresh_project("destroy-scaffold-app");

    run_git(&project, &["init"]);
    run_git(&project, &["add", "-A"]);
    run_git(
        &project,
        &[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "base",
        ],
    );

    let before = snapshot_tree(&project);

    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);
    assert!(project.join("src/models/post.rs").is_file());
    assert!(project.join("src/routes/posts.rs").is_file());
    assert!(project.join("src/repositories/post.rs").is_file());

    run_autumn(&project, &["destroy", "scaffold", "Post", "title:String"]);

    let after = snapshot_tree(&project);
    assert_eq!(
        before, after,
        "working tree must be byte-identical after generate+destroy"
    );

    let status_stdout = run_git(&project, &["status", "--porcelain"]);
    assert!(
        status_stdout.trim().is_empty(),
        "git status must be clean after generate+destroy, got:\n{status_stdout}"
    );
}

#[test]
fn generate_then_destroy_model_round_trips_git_clean() {
    let (_tmp, project) = fresh_project("destroy-model-app");
    run_git(&project, &["init"]);
    run_git(&project, &["add", "-A"]);
    run_git(
        &project,
        &[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "base",
        ],
    );

    let before = snapshot_tree(&project);
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);
    run_autumn(&project, &["destroy", "model", "Post", "title:String"]);
    let after = snapshot_tree(&project);
    assert_eq!(before, after);

    let status_stdout = run_git(&project, &["status", "--porcelain"]);
    assert!(status_stdout.trim().is_empty());
}

#[test]
fn generate_then_destroy_migration_round_trips_git_clean() {
    let (_tmp, project) = fresh_project("destroy-migration-app");
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);
    run_git(&project, &["init"]);
    run_git(&project, &["add", "-A"]);
    run_git(
        &project,
        &[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "base",
        ],
    );

    let before = snapshot_tree(&project);
    run_autumn(
        &project,
        &[
            "generate",
            "migration",
            "AddSubtitleToPosts",
            "subtitle:String",
        ],
    );
    run_autumn(
        &project,
        &[
            "destroy",
            "migration",
            "AddSubtitleToPosts",
            "subtitle:String",
        ],
    );
    let after = snapshot_tree(&project);
    assert_eq!(before, after);

    let status_stdout = run_git(&project, &["status", "--porcelain"]);
    assert!(status_stdout.trim().is_empty());
}

#[test]
fn generate_then_destroy_plugin_round_trips_git_clean() {
    // Regression test (issue #1048 PR review): `plan_plugin` refuses a
    // non-empty target directory unless `--force` — a generate-time
    // collision guard. Without special-casing destroy mode, `autumn destroy
    // plugin Foo` would always hit that same guard (the plugin directory
    // legitimately exists, holding the files this destroy is about to
    // remove) and fail before ever reaching `Plan::revert`, even with no
    // `--force` flag and no actual divergence.
    let (_tmp, project) = fresh_project("destroy-plugin-app");

    run_git(&project, &["init"]);
    run_git(&project, &["add", "-A"]);
    run_git(
        &project,
        &[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "base",
        ],
    );

    let before = snapshot_tree(&project);

    run_autumn(&project, &["generate", "plugin", "Foo"]);
    assert!(project.join("autumn-foo-plugin/Cargo.toml").is_file());

    run_autumn(&project, &["destroy", "plugin", "Foo"]);
    assert!(!project.join("autumn-foo-plugin").exists());

    let after = snapshot_tree(&project);
    assert_eq!(
        before, after,
        "working tree must be byte-identical after generate+destroy"
    );

    let status_stdout = run_git(&project, &["status", "--porcelain"]);
    assert!(
        status_stdout.trim().is_empty(),
        "git status must be clean after generate+destroy, got:\n{status_stdout}"
    );
}

/// AC5: `destroy --dry-run` prints a plan and exits 0 without touching disk.
#[test]
fn destroy_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("destroy-dry-run-app");
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);
    let before = snapshot_tree(&project);

    let (stdout, _) = run_autumn(
        &project,
        &["destroy", "scaffold", "Post", "title:String", "--dry-run"],
    );
    assert!(stdout.contains("Dry run"));
    assert!(stdout.contains("Would remove") || stdout.contains("Would revert"));

    let after = snapshot_tree(&project);
    assert_eq!(before, after, "--dry-run must not touch disk");
}

/// AC7: destroy refuses on diverged content unless `--force`, and never
/// deletes the hand-edited file in that case.
#[test]
fn destroy_refuses_on_diverged_file_without_force() {
    let (_tmp, project) = fresh_project("destroy-diverged-app");
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);

    let model_path = project.join("src/models/post.rs");
    let mut content = fs::read_to_string(&model_path).unwrap();
    content.push_str("\n// hand-edited by the user\n");
    fs::write(&model_path, &content).unwrap();

    let (_, stderr, code) =
        run_autumn_failing(&project, &["destroy", "scaffold", "Post", "title:String"]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("diverged") || stderr.contains("Diverged"),
        "expected a divergence error, got stderr: {stderr}"
    );
    assert!(model_path.is_file(), "diverged file must not be deleted");
    assert!(
        fs::read_to_string(&model_path)
            .unwrap()
            .contains("hand-edited")
    );

    // --force overrides the guard and proceeds with the destroy.
    run_autumn(
        &project,
        &["destroy", "scaffold", "Post", "title:String", "--force"],
    );
    assert!(!model_path.exists());
}

#[test]
fn generate_scaffold_api_only() {
    let (_tmp, project) = fresh_project("scaffold-api-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
            "--api",
        ],
    );

    // Model + migration + schema entry.
    assert!(project.join("src/models/post.rs").is_file());
    assert!(project.join("src/models/mod.rs").is_file());
    assert!(project.join("src/schema.rs").is_file());

    // Repository file.
    let repo = fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();
    assert!(repo.contains("#[autumn_web::repository(Post, api = \"/api/posts\")]"));
    assert!(repo.contains("pub trait PostRepository"));
    assert!(repo.contains("Generated by `autumn generate scaffold --api`"));
    assert!(repo.contains("allow_unauthorized_repository_api = true"));

    // No HTML routes file
    assert!(!project.join("src/routes/posts.rs").is_file());

    // Smoke test: real, in-process, DB-backed read test (issue #1023),
    // paginated per issue #1237 — asserts the `Page` envelope + page 2 advance.
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(test.contains("posts_api_list_paginates_against_a_real_database"));
    assert!(test.contains("autumn_web::test::{TestApp, TestClient, TestDb}"));
    assert!(!test.contains("TcpStream"));
    assert!(!test.contains("AUTUMN_TEST_BASE_URL"));
    assert!(test.contains("#[ignore = \"requires Docker"));
    assert!(test.contains("/api/posts"));
    assert!(
        test.contains("PageRequest") && test.contains("Page::new"),
        "the --api smoke test must paginate via the Page envelope: {test}"
    );
    assert!(
        test.contains("page 2 must differ from page 1"),
        "the --api smoke test must assert pages advance: {test}"
    );
    // Required-column seed drives 25 rows off a single `generate_series` and
    // must not fall back to `INSERT ... DEFAULT VALUES` (which cannot satisfy
    // the NOT NULL `title`/`published` columns).
    assert!(
        test.contains("FROM generate_series(1, 25) AS g"),
        "required-column --api seed must use generate_series: {test}"
    );
    assert!(
        !test.contains("INSERT INTO posts DEFAULT VALUES"),
        "required-column --api seed must not fall back to DEFAULT VALUES: {test}"
    );

    // `routes![]` registration.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("mod models;"));
    assert!(
        !main.contains("mod routes;"),
        "main.rs should not declare routes mod if api is set: {main}"
    );
    assert!(main.contains("mod schema;"));
    assert!(main.contains("mod repositories;"));
    for entry in [
        "repositories::post::post_api_create",
        "repositories::post::post_api_update",
        "repositories::post::post_api_delete",
        "repositories::post::post_api_list",
        "repositories::post::post_api_get",
    ] {
        assert!(
            main.contains(entry),
            "main.rs missing routes![] entry: {entry}\n{main}"
        );
    }
    for entry in [
        "routes::posts::index",
        "routes::posts::show",
        "routes::posts::new_form",
        "routes::posts::create",
        "routes::posts::edit_form",
        "routes::posts::update",
    ] {
        assert!(
            !main.contains(entry),
            "main.rs should not mount HTML route: {entry}\n{main}"
        );
    }
}

#[test]
fn generate_scaffold_accepts_metadata_flags() {
    let (_tmp, project) = fresh_project("scaffold-metadata-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Bookmark",
            "url:String",
            "title:String",
            "tag:String",
            "alive:bool",
            "--index",
            "url",
            "--index",
            "tag",
            "--validate",
            "url=url",
            "--validate",
            "title=length:min=1,max=200",
            "--default",
            "alive=true",
            "--query",
            "find_by_tag:tag",
            "--query",
            "find_by_alive:alive",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/bookmark.rs")).unwrap();
    assert!(model.contains("#[indexed]\n    #[validate(url)]\n    pub url: String,"));
    assert!(model.contains("#[validate(length(min = 1, max = 200))]\n    pub title: String,"));
    assert!(model.contains("#[indexed]\n    pub tag: String,"));
    assert!(model.contains("#[default]\n    pub alive: bool,"));

    let repo = fs::read_to_string(project.join("src/repositories/bookmark.rs")).unwrap();
    assert!(repo.contains("fn find_by_tag(tag: String) -> Vec<Bookmark>;"));
    assert!(repo.contains("fn find_by_alive(alive: bool) -> Vec<Bookmark>;"));

    let routes = fs::read_to_string(project.join("src/routes/bookmarks.rs")).unwrap();
    // The views render through one `form_for` call (issue #1135): the
    // per-field controls (including the required signal for the three
    // non-nullable strings) come from the `#[model]`-derived `FormModel`
    // descriptors, delegated to from the generated form struct.
    assert!(routes.contains("impl autumn_web::form::FormModel for BookmarkForm"));
    assert!(routes.contains("<Bookmark as autumn_web::form::FormModel>::form_fields()"));
    assert!(routes.contains("autumn_web::form::form_for(changeset, action, \"post\")"));
    assert!(!routes.contains("autumn_web::form::required_text_input(&changeset"));
    // `alive` is defaulted → excluded from the FORM entirely. Scoped to the
    // generated form struct + its `form_fields` descriptors rather than the whole
    // file: since issue #1315 the module also carries a `CsvSchema` impl, whose
    // column list is the MODEL's columns (what `show` renders), not the form's —
    // a defaulted column is still data an author downloading a spreadsheet wants.
    // The ONE remaining `"alive"` string in the module is that CSV header; the
    // form struct, its descriptors and every rendered control are free of it.
    assert_eq!(
        routes.matches("\"alive\"").count(),
        1,
        "`alive` is defaulted: its only quoted mention may be the CSV header:\n{routes}"
    );
    assert!(routes.contains(r#"&["id", "url", "title", "tag", "alive", "created_at"]"#));
    assert!(routes.contains("self.alive.to_string(),"));
    assert!(routes.contains("bookmarks::tag.eq(new.tag.clone())"));
    assert!(!routes.contains("bookmarks::alive.eq("));
    assert!(!routes.contains("new.alive"));

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_create_bookmarks")
        })
        .expect("create_bookmarks migration should exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(up.contains("alive BOOLEAN NOT NULL DEFAULT TRUE"));
    assert!(up.contains("CREATE INDEX idx_bookmarks_url ON bookmarks (url);"));
    assert!(up.contains("CREATE INDEX idx_bookmarks_tag ON bookmarks (tag);"));

    let cargo_toml = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo_toml.contains("validator ="),
        "validation attributes need validator in Cargo.toml:\n{cargo_toml}"
    );
}

#[test]
fn generate_scaffold_rejects_query_name_field_mismatch() {
    let (_tmp, project) = fresh_project("scaffold-bad-query-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Bookmark",
            "tag:String",
            "alive:bool",
            "--query",
            "find_by_alive:tag",
        ],
    );

    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("find_by_alive:tag") && stderr.contains("must match field 'tag'"),
        "expected query mismatch validation error; got stderr: {stderr}"
    );
}

#[test]
fn generate_scaffold_rejects_validator_field_type_mismatch() {
    let (_tmp, project) = fresh_project("scaffold-bad-validator-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Bookmark",
            "alive:bool",
            "--validate",
            "alive=url",
        ],
    );

    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("alive=url") && stderr.contains("url validation requires String or Text"),
        "expected validator type validation error; got stderr: {stderr}"
    );
}

#[test]
fn generate_scaffold_rejects_i32_default_outside_sql_integer_range() {
    let (_tmp, project) = fresh_project("scaffold-bad-default-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Counter",
            "count:i32",
            "--default",
            "count=9223372036854775807",
        ],
    );

    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("count=9223372036854775807")
            && stderr.contains("i32 defaults must fit the SQL INTEGER range"),
        "expected i32 default range validation error; got stderr: {stderr}"
    );
}

/// Issue #1048's success metric, verified end-to-end: `autumn generate
/// scaffold Post title:String` immediately followed by `autumn destroy
/// scaffold Post title:String` leaves `cargo check` green on the
/// round-tripped project — not just a clean working tree (already covered by
/// [`generate_then_destroy_scaffold_round_trips_git_clean`]), but a project
/// that still *compiles*, proving destroy never leaves a dangling `mod`
/// declaration, `routes![]` entry, or Cargo.toml dependency behind.
///
/// Ignored by default; slow (compiles the full `autumn-web` dependency
/// tree) and requires network access to fetch crates. Run with:
/// `cargo test -p autumn-cli --test generate destroy_scaffold_round_trip_leaves_cargo_check_green -- --ignored --exact`
#[test]
#[ignore = "slow: compiles the generated project with cargo check"]
fn destroy_scaffold_round_trip_leaves_cargo_check_green() {
    let (_tmp, project) = fresh_project("destroy-scaffold-check-app");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);
    run_autumn(&project, &["destroy", "scaffold", "Post", "title:String"]);

    let check = Command::new("cargo")
        .args(["check", "--all-targets"])
        .current_dir(&project)
        .output()
        .expect("failed to run cargo check");
    assert!(
        check.status.success(),
        "cargo check failed on the round-tripped project:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Start a Postgres testcontainer and return it (alive for as long as the
/// binding lives) alongside its connection URL.
async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
) {
    use testcontainers::runners::AsyncRunner as _;

    let postgres = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer");
    let host = postgres.get_host().await.expect("postgres host");
    let pg_port = postgres
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");
    let url = format!("postgres://postgres:postgres@{host}:{pg_port}/postgres");
    (postgres, url)
}

/// Migrate, `cargo build`, and boot a freshly generated project against
/// `database_url`, returning the running server (kept alive by the returned
/// guard) and its base URL.
///
/// Shared by the live-HTTP gates so they cannot drift on how the app under test
/// is brought up; a build failure surfaces the full compiler output, since these
/// gates are the only place the generated app is ever compiled AND run.
async fn migrate_build_and_boot(
    project: &Path,
    database_url: &str,
    client: &reqwest::Client,
) -> (ServerGuard, String) {
    run_autumn_with_env(
        project,
        &["migrate"],
        &[("AUTUMN_DATABASE__URL", database_url)],
    );

    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(project)
        .output()
        .expect("failed to run cargo build");
    assert!(
        build.status.success(),
        "cargo build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let port = free_port();
    let child = Command::new("cargo")
        .args(["run", "--quiet"])
        .current_dir(project)
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .env("AUTUMN_DATABASE__URL", database_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn generated server");

    let base = format!("http://127.0.0.1:{port}");
    let server = wait_for_server_ready_async(child, client, &base).await;
    (server, base)
}

/// Slow live-HTTP check: scaffold a fresh project, run migrations against a
/// real Postgres testcontainer, boot the generated server, and assert the
/// generated HTML and JSON routes actually respond.
///
/// Ignored by default; requires Docker and `diesel` CLI on PATH. Run with:
/// `cargo test -p autumn-cli --test generate generated_scaffold_serves_posts_index_and_json_api -- --ignored --exact`
#[tokio::test]
#[ignore = "slow: starts Postgres, runs diesel migrations, builds and boots a generated app"]
async fn generated_scaffold_serves_posts_index_and_json_api() {
    let (_tmp, project) = fresh_project("scaffold-live");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
        ],
    );

    let (_postgres, database_url) = start_postgres().await;
    let client = reqwest::Client::new();
    let (_server, base) = migrate_build_and_boot(&project, &database_url, &client).await;

    let response = client
        .get(format!("{base}/posts"))
        .send()
        .await
        .expect("GET /posts failed");
    assert_eq!(response.status(), 200, "GET /posts status");
    let html = response.text().await.expect("GET /posts body");
    assert!(
        html.contains("<h1>Posts</h1>") && html.contains("New Post"),
        "GET /posts did not render the generated index template:\n{html}",
    );

    let response = client
        .get(format!("{base}/api/posts"))
        .send()
        .await
        .expect("GET /api/posts failed");
    assert_eq!(response.status(), 200, "GET /api/posts status");
    let body = response.text().await.expect("GET /api/posts body");
    let envelope: serde_json::Value =
        serde_json::from_str(body.trim()).expect("GET /api/posts must return a JSON Page envelope");
    let content = envelope["content"]
        .as_array()
        .expect("Page envelope must carry a content array");
    assert!(content.is_empty(), "empty JSON index body: {body}");
    assert_eq!(
        envelope["total_elements"], 0,
        "empty JSON index total_elements"
    );
}

/// Give a freshly generated app a test-only session sign-in route.
///
/// A scaffold's `new`/`edit` forms and every mutating route are `#[secured]`,
/// so an anonymous client is answered 401 and never reaches the validation path
/// under test. `#[secured]` is satisfied by the configured auth session key
/// (`auth.session_key`, default `user_id`) being present in the session, so
/// this splices exactly one `#[public]` route into the generated `src/main.rs`
/// that sets it — the smallest stand-in for `autumn generate auth`'s real
/// sign-in flow, which is a separate generator with its own coverage.
fn add_session_signin_stub(project: &Path) {
    const HANDLER: &str = "\n// Test-only sign-in stub (see `add_session_signin_stub`).\n\
                           #[get(\"/__signin\")]\n\
                           #[public]\n\
                           async fn signin_stub(session: autumn_web::session::Session) -> &'static str {\n    \
                           session.insert(\"user_id\", \"1\").await;\n    \
                           \"signed in\"\n\
                           }\n\n\
                           #[autumn_web::main]\n";

    let main_rs = project.join("src/main.rs");
    let source = fs::read_to_string(&main_rs).expect("read generated src/main.rs");

    assert_eq!(
        source.matches("\n#[autumn_web::main]\n").count(),
        1,
        "expected exactly one `#[autumn_web::main]` in the generated src/main.rs"
    );
    let patched = source.replacen("\n#[autumn_web::main]\n", HANDLER, 1);

    assert_eq!(
        patched.matches("routes![index,").count(),
        1,
        "expected the generated `routes![index, …]` list in src/main.rs"
    );
    let patched = patched.replacen("routes![index,", "routes![signin_stub, index,", 1);

    fs::write(&main_rs, patched).expect("write patched src/main.rs");
}

/// Slice the rendered `<input …>` tag whose `name="…"` attribute matches
/// `name`, so a field-scoped attribute assertion can never accidentally match a
/// sibling control. `name="…"` appears only on the control itself — the
/// `<label>` uses `for=`, and the inline-error `<div>` uses `id="…-error"` — so
/// the match is unambiguous.
fn input_tag<'a>(html: &'a str, name: &str) -> &'a str {
    let needle = format!("name=\"{name}\"");
    let at = html
        .find(&needle)
        .unwrap_or_else(|| panic!("no control named `{name}` in the rendered form:\n{html}"));
    let start = html[..at]
        .rfind('<')
        .unwrap_or_else(|| panic!("control `{name}` has no opening tag:\n{html}"));
    let end = html[start..]
        .find('>')
        .map_or(html.len(), |rel| start + rel + 1);
    &html[start..end]
}

/// Every `<input type="hidden" name=… value=…>` inside the `<form>` whose
/// `action` matches, as name/value pairs ready to re-submit.
///
/// Collected wholesale rather than named one at a time so the POST legs below
/// carry whatever the framework's own form rendering decided to inject — today
/// the one-time `_submit_token`, and the `_csrf` field whenever the CSRF layer
/// is active — instead of hard-coding a list that silently rots. Scoped to the
/// resource's own form so the layout's consent-banner form can't leak in.
fn hidden_form_fields(html: &str, action: &str) -> Vec<(String, String)> {
    let marker = format!("action=\"{action}\"");
    let at = html
        .find(&marker)
        .unwrap_or_else(|| panic!("no <form {marker}> in:\n{html}"));
    let start = html[..at].rfind('<').expect("form opening tag");
    let end = html[start..]
        .find("</form>")
        .map_or(html.len(), |rel| start + rel);
    let form = &html[start..end];

    // Matched as "an `<input>` tag that carries `type=\"hidden\"`" rather than
    // the literal prefix `<input type="hidden"`: attribute ORDER is a maud
    // rendering detail, and a helper that ever emitted `name` first would
    // otherwise drop its field here silently — the POST would then fail CSRF /
    // submit-token checks and surface as a baffling 403 instead of the 422 the
    // leg is actually asserting.
    let mut fields = Vec::new();
    let mut rest = form;
    while let Some(at) = rest.find("<input") {
        let tag_end = rest[at..].find('>').map_or(rest.len(), |rel| at + rel + 1);
        let tag = &rest[at..tag_end];
        if tag.contains("type=\"hidden\"")
            && let (Some(name), Some(value)) = (attr_value(tag, "name"), attr_value(tag, "value"))
        {
            fields.push((name, value));
        }
        rest = &rest[tag_end..];
    }
    assert!(
        !fields.is_empty(),
        "the create form must carry at least the one-time submit token:\n{form}"
    );
    fields
}

/// The value of `attr` on a single rendered tag, un-escaping the entities maud
/// emits inside an attribute value.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let at = tag.find(&needle)? + needle.len();
    let end = at + tag[at..].find('"')?;
    Some(
        tag[at..end]
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

/// Fetch `GET {base}/posts/new` and return the rendered create form plus the
/// hidden fields a subsequent `POST /posts` must echo back.
///
/// Re-fetched before every POST leg: the submit token is one-time, so a stale
/// pair would be rejected before the validation path under test is reached.
async fn fetch_new_post_form(
    client: &reqwest::Client,
    base: &str,
) -> (String, Vec<(String, String)>) {
    let response = client
        .get(format!("{base}/posts/new"))
        .send()
        .await
        .expect("GET /posts/new failed");
    assert_eq!(response.status(), 200, "GET /posts/new status");
    let html = response.text().await.expect("GET /posts/new body");
    let hidden = hidden_form_fields(&html, "/posts");
    (html, hidden)
}

/// `POST /posts` carrying the form's own hidden fields plus the submitted
/// columns, returning `(status, body)`.
async fn submit_post(
    client: &reqwest::Client,
    base: &str,
    hidden: &[(String, String)],
    columns: &[(&str, &str)],
) -> (u16, String) {
    let mut form: Vec<(String, String)> = hidden.to_vec();
    form.extend(
        columns
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    let response = client
        .post(format!("{base}/posts"))
        .form(&form)
        .send()
        .await
        .expect("POST /posts failed");
    let status = response.status().as_u16();
    let body = response.text().await.expect("POST /posts body");
    (status, body)
}

/// `GET /api/posts` -> the `total_elements` count of the generated JSON index.
async fn stored_post_count(client: &reqwest::Client, base: &str) -> u64 {
    let response = client
        .get(format!("{base}/api/posts"))
        .send()
        .await
        .expect("GET /api/posts failed");
    assert_eq!(response.status(), 200, "GET /api/posts status");
    let body = response.text().await.expect("GET /api/posts body");
    let envelope: serde_json::Value =
        serde_json::from_str(body.trim()).expect("GET /api/posts must return a JSON Page envelope");
    envelope["total_elements"]
        .as_u64()
        .unwrap_or_else(|| panic!("Page envelope must carry total_elements:\n{body}"))
}

/// Assert the rendered `Post` form carries the HTML5 constraints its DSL
/// modifiers declared (issue #1388 AC3/AC4/AC6), and that the pre-existing
/// `required` (derived from non-nullability) survives alongside them rather
/// than being displaced by them.
///
/// Shared by the initial `GET /posts/new` render and the 422 re-render, so a
/// field that sheds its client-side guards on the way back from a rejection is
/// caught as readily as one that never had them.
fn assert_constrained_controls_render_html5(html: &str) {
    // The BOOLEAN `required` attribute, not the `aria-required="true"` that sits
    // beside it: `contains("required")` would be satisfied by the ARIA hint
    // alone, so a regression that dropped the browser-enforced attribute while
    // keeping the screen-reader one would sail through. `required` is rendered
    // last on the tag (`autumn_web::a11y::TextField`), so the closing angle
    // bracket pins it.
    const REQUIRED: &str = " required>";

    let title = input_tag(html, "title");
    for attr in ["minlength=\"3\"", "maxlength=\"120\"", REQUIRED] {
        assert!(
            title.contains(attr),
            "`title` input must carry `{attr}` (issue #1388 AC4): {title}"
        );
    }
    let contact = input_tag(html, "contact");
    for attr in ["type=\"email\"", REQUIRED] {
        assert!(
            contact.contains(attr),
            "`contact` input must carry `{attr}` (issue #1388 AC4): {contact}"
        );
    }
    let homepage = input_tag(html, "homepage");
    for attr in ["type=\"url\"", REQUIRED] {
        assert!(
            homepage.contains(attr),
            "`homepage` input must carry `{attr}` (issue #1388 AC3): {homepage}"
        );
    }
    let age = input_tag(html, "age");
    for attr in ["type=\"number\"", "min=\"0\"", "max=\"130\"", REQUIRED] {
        assert!(
            age.contains(attr),
            "`age` input must carry `{attr}` (issue #1388 AC6): {age}"
        );
    }
}

/// Assert the 422 re-render carries an inline, `role="alert"` error block for
/// `field` specifically.
///
/// Scoped to the field's own `id="{field}-error"` container rather than checking
/// `role="alert"` anywhere on the page: a sibling field's error block (or a
/// flash region) would otherwise satisfy a document-wide search, so the
/// assertion would keep passing after the alert role was dropped from the very
/// element a screen reader needs it on.
fn assert_inline_field_error(html: &str, field: &str) {
    let marker = format!("id=\"{field}-error\"");
    let at = html
        .find(&marker)
        .unwrap_or_else(|| panic!("the 422 must re-render an inline error for `{field}`:\n{html}"));
    let start = html[..at].rfind('<').expect("error element opening tag");
    let end = html[start..]
        .find('>')
        .map_or(html.len(), |rel| start + rel + 1);
    let tag = &html[start..end];
    assert!(
        tag.contains("role=\"alert\""),
        "`{field}`'s inline error must be announced with role=\"alert\": {tag}"
    );
}

/// A booted, signed-in generated app under test, with everything that must
/// outlive the assertions held alive.
///
/// Field order IS the teardown order (struct fields drop in declaration order):
/// the server dies before the Postgres container it talks to, which dies before
/// the tempdir holding the project it was built from.
struct LiveApp {
    _server: ServerGuard,
    _postgres: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    _tmp: tempfile::TempDir,
    client: reqwest::Client,
    base: String,
}

/// Scaffold a `Post` whose columns carry the full issue #1388 constraint mix,
/// boot it against a real Postgres, and sign in — the shared setup for the
/// runtime round-trip below.
async fn boot_constrained_post_app() -> LiveApp {
    let (tmp, project) = fresh_project("scaffold-constraints-live");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String{min=3,max=120}",
            "contact:String{email}",
            "homepage:String{url}",
            "age:i32{min=0,max=130}",
        ],
    );
    add_session_signin_stub(&project);

    let (postgres, database_url) = start_postgres().await;

    // A cookie jar carries the session the `#[secured]` form routes need (and
    // the CSRF cookie whenever that layer is active); redirects are NOT
    // followed, so the create handler's 303-vs-422 answer is observable
    // directly rather than through whatever it redirects to. The per-request
    // timeout keeps a wedged handler failing the test instead of hanging the CI
    // job until the runner's own deadline.
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build reqwest client");
    let (server, base) = migrate_build_and_boot(&project, &database_url, &client).await;

    // The sign-in stub must be load-bearing: assert the form route really is
    // `#[secured]` FIRST. Without this, a regression that made the scaffold's
    // whole write surface public would leave every assertion below green, and
    // the stub would quietly become dead weight.
    let anonymous = client
        .get(format!("{base}/posts/new"))
        .send()
        .await
        .expect("anonymous GET /posts/new failed");
    assert_eq!(
        anonymous.status(),
        401,
        "the scaffold's create form must be `#[secured]`"
    );

    let signin = client
        .get(format!("{base}/__signin"))
        .send()
        .await
        .expect("GET /__signin failed");
    assert_eq!(signin.status(), 200, "test sign-in stub must succeed");

    LiveApp {
        _server: server,
        _postgres: postgres,
        _tmp: tmp,
        client,
        base,
    }
}

/// Issue #1388 AC4/AC6, proven at RUNTIME rather than by string-matching the
/// generated source: scaffold a resource whose fields carry `{…}` constraint
/// modifiers, migrate it against a real Postgres, boot the generated server,
/// and drive the actual HTTP surface.
///
/// Every other test for the `{…}` block asserts on generated *text* — that the
/// model carries `#[validate(length(min = 3, max = 120))]`, that the routes
/// module builds an `a11y::TextField` with `.minlength(3u32)`. None of them
/// proves the fan-out actually *works*: that the emitted `#[validate]` rules
/// reach the `Validated`/changeset path and answer **422** (never a 500, never
/// a silent store), that the emitted builder calls render the promised HTML5
/// attributes into the browser's markup, or that a valid submission still gets
/// through. This is the acceptance criterion's "scaffold-to-runtime round-trip
/// test", end to end:
///
/// * `GET /posts/new` renders `title` with `minlength="3" maxlength="120"
///   required`, `contact` as `type="email"`, and `age` as `type="number"
///   min="0" max="130"` — AC3, AC4's client half, AC6's typed-input half;
/// * an empty `title` and a malformed `contact` are rejected **server-side**
///   with a 422 whose body re-renders the form with inline `role="alert"`
///   errors and the submitted input preserved — AC2, AC4's server half, and
///   composition with the #1124 error re-render;
/// * an out-of-range `age` surfaces its `range` rejection inline the same way
///   — AC6's server half;
/// * neither rejected submission stores a row — the Success Metric's "zero
///   successful inserts of an empty title or a malformed email";
/// * a valid submission still redirects (303) and persists, so the constraints
///   reject bad input without blocking good input.
///
/// `age:i32{min=0,max=130}` is scaffolded alongside the acceptance criterion's
/// own `title`/`contact` pair (rather than in a second test) deliberately: it
/// is the issue's own AC1/AC6 numeric example, and booting a freshly compiled
/// app is the expensive part — one boot covers both halves. The extra column
/// changes nothing about the `title`/`contact` assertions.
///
/// Ignored by default; requires Docker and `diesel` CLI on PATH. Run with:
/// `cargo test -p autumn-cli --test generate generated_constrained_scaffold_enforces_validation_end_to_end -- --ignored --exact`
#[tokio::test]
#[ignore = "slow: starts Postgres, runs diesel migrations, builds and boots a constrained scaffold"]
async fn generated_constrained_scaffold_enforces_validation_end_to_end() {
    let app = boot_constrained_post_app().await;
    let (client, base) = (&app.client, app.base.as_str());

    // ── AC3/AC4 (client half) + AC6: the rendered form carries the HTML5
    // attributes the DSL declared, and the pre-existing `required` (from
    // non-null) survives alongside them. ────────────────────────────────────
    let (form_html, hidden) = fetch_new_post_form(client, base).await;
    assert_constrained_controls_render_html5(&form_html);

    // ── AC2/AC4 (server half): an empty title and a malformed email are
    // rejected with a 422 that re-renders inline errors and preserves input —
    // not a 500, not a redirect, not a silent store. ────────────────────────
    let (status, body) = submit_post(
        client,
        base,
        &hidden,
        &[
            ("title", ""),
            ("contact", "not-an-email"),
            ("homepage", "not-a-url"),
            ("age", "42"),
        ],
    )
    .await;
    assert_eq!(
        status, 422,
        "an empty title + malformed email must be a 422, never a 500 or a redirect:\n{body}"
    );
    for field in ["title", "contact", "homepage"] {
        assert_inline_field_error(&body, field);
    }
    assert!(
        input_tag(&body, "contact").contains("value=\"not-an-email\""),
        "the 422 must preserve the submitted input (issue #1124):\n{body}"
    );
    // The constraints still render on the re-rendered form, so a corrected
    // resubmit keeps its client-side guards.
    assert_constrained_controls_render_html5(&body);

    // ── AC6 (server half): a numeric `range` rejection surfaces inline the
    // same way. ─────────────────────────────────────────────────────────────
    let (_, hidden) = fetch_new_post_form(client, base).await;
    let (status, body) = submit_post(
        client,
        base,
        &hidden,
        &[
            ("title", "A valid title"),
            ("contact", "author@example.com"),
            ("homepage", "https://example.com"),
            ("age", "999"),
        ],
    )
    .await;
    assert_eq!(
        status, 422,
        "an out-of-range `age` must be a 422 (issue #1388 AC6):\n{body}"
    );
    assert_inline_field_error(&body, "age");

    // ── Success Metric: neither rejected submission stored a row. ───────────
    assert_eq!(
        stored_post_count(client, base).await,
        0,
        "a rejected submission must never be stored"
    );

    // ── And the constraints reject bad input without blocking good input: a
    // valid submission still redirects (303) and persists. ──────────────────
    let (_, hidden) = fetch_new_post_form(client, base).await;
    let (status, body) = submit_post(
        client,
        base,
        &hidden,
        &[
            ("title", "A valid title"),
            ("contact", "author@example.com"),
            ("homepage", "https://example.com"),
            ("age", "42"),
        ],
    )
    .await;
    assert_eq!(
        status, 303,
        "a valid submission must redirect, not re-render:\n{body}"
    );
    assert_eq!(
        stored_post_count(client, base).await,
        1,
        "the valid submission must persist"
    );
}

#[test]
fn generate_outside_project_root_fails_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, stderr, code) =
        run_autumn_failing(tmp.path(), &["generate", "model", "Post", "title:String"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("not inside an Autumn project"));
}

#[test]
fn generate_help_documents_field_dsl() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("model"));
    assert!(stdout.contains("migration"));
    assert!(stdout.contains("scaffold"));
}

#[test]
fn generate_help_documents_decimal_field_type() {
    // AC4 (issue #1038): `decimal` must be discoverable from `--help`, not
    // just accepted silently by the parser.
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("decimal"), "got: {stdout}");
    assert!(stdout.contains("NUMERIC"), "got: {stdout}");
}

#[test]
fn generate_model_help_shows_example() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "model", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("autumn generate model Post"));
    assert!(stdout.contains("--dry-run"));
    assert!(stdout.contains("--force"));
}

/// The `SQLite` counterpart of [`generated_scaffold_cargo_checks`], and the only
/// machine proof of issue #1924: a `SQLite`-configured app scaffolded with every
/// field kind that needed a `SQLite` conversion — `Uuid`, `Option<Uuid>`,
/// `decimal{p,s}`, `enum{…}`, `DateTime<Utc>`, `Attachment`, `json` — actually
/// compiles.
///
/// Two halves have to be right for this to pass, and neither is visible to a
/// unit test over the emitted strings:
///
/// 1. The dependency set. A `SQLite` app's `Cargo.toml` must carry diesel on its
///    `sqlite` feature, the bundled `libsqlite3-sys`, and `autumn-web/sqlite` —
///    and must NOT carry `pq-sys`.
/// 2. The Rust types. `Uuid` and `decimal` render
///    `autumn_web::db::sqlite_types::{SqliteUuid, SqliteDecimal}`, and the
///    generated `enum` carries `Text`/`Sqlite` (not `Pg`) conversions.
///
/// `cargo check`, not `--all-targets`: the scaffold's `tests/<model>.rs` smoke
/// test still uses `autumn_web::test::TestDb`, a Postgres-only testcontainer.
/// A `SQLite` `TestDb` lands with the runtime slice (#1905) — see
/// `docs/guide/sqlite-in-production.md`.
///
/// Ignored by default; run with:
/// `cargo test -p autumn-cli --test generate generated_sqlite_scaffold_cargo_checks -- --ignored --exact`
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_sqlite_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("sqlite-scaffold-build");
    patch_generated_cargo_toml(&project);

    // Point the app at SQLite BEFORE generating: the generator resolves the
    // backend from this file.
    fs::write(
        project.join("autumn.toml"),
        "[database]\nprimary_url = \"sqlite://./app.db\"\n",
    )
    .unwrap();

    // `run_autumn_with_env`, not `run_autumn`: backend detection gives the
    // environment precedence over `autumn.toml`, so a developer running this
    // with `DATABASE_URL=postgres://…` exported would silently get Postgres
    // output and an opaque assertion failure below. Pinning both spellings to
    // the same SQLite URL makes the run independent of the ambient shell.
    run_autumn_with_env(
        &project,
        &[
            "generate",
            "scaffold",
            "Widget",
            "name:String",
            "token:Uuid",
            "owner:Option<Uuid>",
            "price:decimal{10,2}",
            "balance:Option<decimal>",
            "status:enum{draft,published}",
            "mood:Option<enum{happy,sad}>",
            "at:DateTime",
            "seen_at:Option<NaiveDateTime>",
            "payload:json",
            "cover:Attachment",
        ],
        &[
            ("DATABASE_URL", "sqlite://./app.db"),
            ("AUTUMN_DATABASE__URL", "sqlite://./app.db"),
        ],
    );

    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("libsqlite3-sys"),
        "SQLite app must link the bundled SQLite amalgamation:\n{cargo}"
    );
    assert!(
        !cargo.contains("pq-sys"),
        "SQLite app must not link libpq:\n{cargo}"
    );

    let model = fs::read_to_string(project.join("src/models/widget.rs")).unwrap();
    assert!(
        model.contains("autumn_web::db::sqlite_types::SqliteUuid"),
        "Uuid must render the SQLite newtype:\n{model}"
    );
    assert!(
        model.contains("autumn_web::db::sqlite_types::SqliteDecimal"),
        "decimal must render the SQLite newtype:\n{model}"
    );
    assert!(
        !model.contains("diesel::pg::Pg"),
        "the generated enum must carry Sqlite, not Pg, conversions:\n{model}"
    );

    let check = Command::new("cargo")
        .args(["check"])
        .current_dir(&project)
        .output()
        .expect("failed to run cargo check");
    assert!(
        check.status.success(),
        "cargo check failed on the generated SQLite scaffold:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check: scaffold a fresh project, run `autumn generate
/// scaffold`, and `cargo check --tests` the result against the local `autumn-web`
/// crate. Verifies the generator adds every dep its emitted code needs and
/// that the generated application and smoke test actually type-check.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("scaffold-build");

    // Patch Cargo.toml to point at the *local* autumn-web crate (so we don't
    // depend on crates.io having this exact version published). We do NOT
    // pre-add the diesel/maud/etc deps here — that's what the generator is
    // supposed to do automatically.
    let cargo_toml_path = project.join("Cargo.toml");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
            "archived:Option<bool>",
            "subtitle:Option<String>",
            "views:Option<i64>",
            "rank:i32",
            "rating:f64",
            "weight:Option<f32>",
            "published_at:Option<NaiveDateTime>",
            "scheduled_at:DateTime",
            "token:Option<Uuid>",
            "status:enum{draft,published,archived}",
            "mood:Option<enum{happy,sad}>",
            "price:decimal{10,2}",
            "balance:Option<decimal>",
            "payload:Bytea",
            "nickname:Option<Bytea>",
            "--validate",
            "title=length:min=1,max=200",
            "--live-validation",
        ],
    );

    // A `Bytea` field's `{Pascal}Form` representation must actually round-trip:
    // `Vec<u8>` cannot deserialize from a single url-encoded value at all
    // (issue #1124 review), so both nullable and non-nullable Bytea fields
    // are represented as `String`/`Option<String>` on the form.
    let routes_bytea = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes_bytea.contains("pub payload: String,"),
        "{routes_bytea}"
    );
    assert!(
        routes_bytea.contains("pub nickname: Option<String>,"),
        "{routes_bytea}"
    );

    // The `--live-validation` inline-validation handler must compile against
    // the real framework too (issue #1124 follow-up: it now decodes the full
    // form via `decode_form` and renders through `text_input_htmx`, rather
    // than a hand-rolled per-rule check returning a bare error span). `title`
    // is non-nullable, so it keeps the `required` htmx variant.
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("pub async fn validate_title("),
        "expected a validate_title handler:\n{routes}"
    );
    assert!(
        routes.contains("autumn_web::form::required_text_input_htmx(&changeset, \"title\""),
        "validate_title must return the full required_text_input_htmx wrapper:\n{routes}"
    );

    // The generator must have added every dep its emitted code needs.
    let cargo_toml_after = fs::read_to_string(&cargo_toml_path).unwrap();
    for dep in [
        "chrono",
        "diesel",
        "diesel-async",
        "maud",
        "serde",
        "serde_json",
        "serde_urlencoded",
        "url",
        "rust_decimal",
    ] {
        assert!(
            cargo_toml_after.contains(&format!("{dep} =")),
            "Cargo.toml is missing '{dep}' after `generate scaffold`"
        );
    }

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Issue #1323: a `--belongs-to` scaffold compiles against the real framework
/// AND its generated nested write-path test passes.
///
/// This is the only machine proof that the nested surface type-checks: the
/// nested handlers, the shared `children_section` helper, the `exclude_parent_fk`
/// form flag, the `paths::nested_index`/`nested_create` helpers, AND — critically
/// — the *injected* edit to the parent's already-generated `show` handler, which
/// is a textual patch to a file this invocation does not own. A `cargo check`
/// here is what catches that patch going stale if the flat `show` template ever
/// changes shape.
///
/// The generated nested test needs no database (its rows are in-process), so it
/// is run for real rather than just compiled.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_nested_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("nested-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--belongs-to",
            "Post",
        ],
    );

    let child = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();
    assert!(
        child.contains("#[get(\"/posts/{post_id}/comments\", name = \"nested_index\")]"),
        "missing the nested read route:\n{child}"
    );
    assert!(
        child.contains("#[post(\"/posts/{post_id}/comments\", name = \"nested_create\")]"),
        "missing the nested create route:\n{child}"
    );
    let parent = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        parent.contains("crate::routes::comments::children_section("),
        "the parent show must render its children:\n{parent}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the nested scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );

    // AC7: create child under parent -> appears in that parent's list -> does
    // NOT appear under a different parent. DB-free, so run it here.
    let output = Command::new("cargo")
        .args(["test", "--test", "comment", "comments_nested_under_parent"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the generated nested write-path test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("test result: ok"),
        "expected the generated nested test to pass:\n{stdout}"
    );
}

/// Issue #2431: `--belongs-to <Parent> --counter-cache` must produce a child
/// model that actually compiles. Before this fix, `add_counter_cache_to_model_source`
/// inserted the generated `#[belongs_to(Post, counter_cache)]` attribute ABOVE
/// `#[autumn_web::model]` instead of below it — `#[belongs_to]` is a helper
/// attribute only `#[model]`'s own expansion understands, so rustc rejected it
/// outright with `cannot find attribute belongs_to in this scope` on every
/// first-time use of the documented, only invocation of this flag. No prior
/// test ever ran `cargo check` on a `--counter-cache` scaffold (the unit test
/// covering the rewrite only asserted the attribute's text was present, not
/// its position), so this compile break shipped invisibly.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_counter_cache_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("counter-cache-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &["generate", "scaffold", "Post", "title:String", "body:Text"],
    );
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "post:references",
            "body:Text",
            "--belongs-to",
            "Post",
            "--counter-cache",
        ],
    );

    // The generated attribute must sit below `#[autumn_web::model]`, not
    // above it — this is the assertion the pre-fix unit test was missing.
    let model = fs::read_to_string(project.join("src/models/comment.rs")).unwrap();
    let model_pos = model
        .find("#[autumn_web::model]")
        .expect("model attribute present");
    let belongs_to_pos = model
        .find("#[belongs_to(Post, counter_cache)]")
        .expect("belongs_to attribute present");
    assert!(
        model_pos < belongs_to_pos,
        "#[belongs_to] must be emitted below #[autumn_web::model]:\n{model}"
    );

    // The parent-side warning names the two lines the scaffold cannot own
    // (schema.rs + the model struct) — paste them in by hand before `cargo
    // check`, matching what a real user following the warning would do.
    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    let patched_schema = schema.replacen(
        "posts (id) {",
        "posts (id) {\n        comment_count -> Int8,",
        1,
    );
    assert_ne!(schema, patched_schema, "expected to find the posts table");
    fs::write(project.join("src/schema.rs"), patched_schema).unwrap();

    let post_model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    let patched_post_model = post_model.replacen(
        "pub struct Post {",
        "pub struct Post {\n    #[default]\n    pub comment_count: i64,",
        1,
    );
    assert_ne!(post_model, patched_post_model, "expected the Post struct");
    fs::write(project.join("src/models/post.rs"), patched_post_model).unwrap();

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the --counter-cache scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Issue #1125: a scaffold WITH an owner column generates a record-level
/// `Policy`/`Scope`, authorizes the mutating HTML handlers, scopes the index,
/// and emits a cross-user 403 smoke test. `cargo check --tests` proves the
/// whole thing (policy file, authorize wiring, scoped index, cross-user test)
/// type-checks against the real framework; the generated cross-user test needs
/// no database, so we also run it directly to prove the 403 semantics hold end
/// to end.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
#[allow(
    clippy::too_many_lines,
    reason = "one compile gate covering the policy scaffold plus the two CSV \
              surfaces that ride on it; each block is a separate assertion set"
)]
fn generated_policy_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("policy-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "author_id:i64",
            // Issue #1393: the CSV import surface rides along on this scaffold
            // rather than a fourth compile gate of its own. This is the
            // owner-scoped shape, so it also puts the import's `authorize_create`
            // call, its `Multipart` extractor, and the `save_many_skip_invalid`
            // write through a real `cargo check --tests` — and the generated
            // import test is run below.
            "--import",
        ],
    );

    // The generated policy, its registration, and the authorize wiring exist.
    let policy = fs::read_to_string(project.join("src/policies/post.rs")).unwrap();
    assert!(
        policy.contains("impl Policy<Post> for PostPolicy"),
        "policy file must define PostPolicy:\n{policy}"
    );
    assert!(
        policy.contains("Some(post.author_id)"),
        "owner check must use author_id:\n{policy}"
    );
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains(".policy::<crate::models::post::Post, _>"),
        "main.rs must register the policy:\n{main_rs}"
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("autumn_web::authorization::authorize::<Post>"),
        "routes must authorize mutating handlers:\n{routes}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated policy scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );

    // The cross-user 403 test needs no database — run it and prove it passes.
    let cross_user = Command::new("cargo")
        .args([
            "test",
            "--test",
            "post",
            "posts_cross_user_mutations_are_forbidden",
        ])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        cross_user.status.success(),
        "generated cross-user 403 test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&cross_user.stdout),
        String::from_utf8_lossy(&cross_user.stderr),
    );
    assert!(
        String::from_utf8_lossy(&cross_user.stdout).contains("test result: ok"),
        "expected the cross-user test to pass:\n{}",
        String::from_utf8_lossy(&cross_user.stdout)
    );

    // Issue #1315: the generated CSV download test needs no database either.
    // Running it here is the only place the repo proves the emitted test
    // actually passes against the real `export_csv` + `Download` pair, rather
    // than merely type-checking.
    let export_csv = Command::new("cargo")
        .args([
            "test",
            "--test",
            "post",
            "posts_export_csv_downloads_a_spreadsheet",
        ])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        export_csv.status.success(),
        "generated CSV export test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&export_csv.stdout),
        String::from_utf8_lossy(&export_csv.stderr),
    );
    assert!(
        String::from_utf8_lossy(&export_csv.stdout).contains("1 passed"),
        "expected the CSV export test to run and pass:\n{}",
        String::from_utf8_lossy(&export_csv.stdout)
    );

    // Issue #1393: the same for the import. This is the only place the repo
    // proves the emitted import test really passes against the real `Multipart`
    // extractor + `import_csv` + `ImportReport` — that a dry run writes nothing
    // and a confirmed commit writes exactly the valid row — rather than merely
    // type-checking.
    let import_csv = Command::new("cargo")
        .args([
            "test",
            "--test",
            "post",
            "posts_csv_import_previews_then_commits",
        ])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        import_csv.status.success(),
        "generated CSV import test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&import_csv.stdout),
        String::from_utf8_lossy(&import_csv.stderr),
    );
    assert!(
        String::from_utf8_lossy(&import_csv.stdout).contains("1 passed"),
        "expected the CSV import test to run and pass:\n{}",
        String::from_utf8_lossy(&import_csv.stdout)
    );
}

/// Companion to [`generated_policy_scaffold_cargo_checks`] proving the
/// *nullable* owner-column path compiles (PR #1831 review): a `user:references?`
/// owner is `Option<i64>`, so the generated `can_update`/`can_delete` must
/// compare it option-to-option (`ctx.user_id_i64() == post.user_id`) rather
/// than wrapping it in `Some(...)` (which would compare `Option<i64>` to
/// `Option<Option<i64>>` and fail to build), and the scope must still `.eq()`
/// the nullable diesel column.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_nullable_owner_policy_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("policy-nullable-owner-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Note",
            "title:String",
            "body:Text",
            "user:references?",
        ],
    );

    let policy = fs::read_to_string(project.join("src/policies/note.rs")).unwrap();
    assert!(
        policy.contains("ctx.user_id_i64() == note.user_id"),
        "nullable owner must compare option-to-option:\n{policy}"
    );
    assert!(
        !policy.contains("Some(note.user_id)"),
        "nullable owner must not wrap in Some(...):\n{policy}"
    );
    assert!(
        policy.contains("notes::user_id.eq(owner_id)"),
        "scope must filter on the nullable owner column:\n{policy}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated nullable-owner policy scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Issue #1830 companion to [`generated_policy_scaffold_cargo_checks`] for the
/// `--live` variant: an owner column on a `--live` scaffold now record-authorizes
/// the mutating handlers (loading the current row through the *repository*, not
/// raw diesel) and owner-scopes the index while keeping the SSE `<ul>` island.
/// `cargo check --tests` proves the repository-based authorize wiring compiles
/// against the real framework.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_live_owner_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("live-owner-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "author_id:i64",
            "--live",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("autumn_web::authorization::authorize::<Post>"),
        "live routes must authorize mutating handlers:\n{routes}"
    );
    assert!(
        routes.contains("let current: Post = repo.find_by_id(*id).await?"),
        "live variant must load the current row via the repository:\n{routes}"
    );
    assert!(
        routes.contains("posts::author_id.eq(owner_id)"),
        "live index must be owner-scoped:\n{routes}"
    );
    assert!(
        routes.contains("sse-connect=(paths::events())"),
        "live owner index must keep the SSE list contract:\n{routes}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated live-owner scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Issue #1830 companion for the `--sharded` variant: an owner column on a
/// `--sharded` scaffold record-authorizes the mutating handlers (loading the
/// current row through a `from_shard` repository) and owner-scopes the index on
/// the `ShardedDb` connection. `cargo check --tests` proves the sharded
/// authorize wiring compiles against the real framework.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_sharded_owner_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("sharded-owner-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "author_id:i64",
            "--sharded",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("autumn_web::authorization::authorize::<Post>"),
        "sharded routes must authorize mutating handlers:\n{routes}"
    );
    assert!(
        routes.contains(
            "let current: Post = PgPostRepository::from_shard(&db).find_by_id(*id).await?"
        ),
        "sharded variant must load the current row via from_shard:\n{routes}"
    );
    assert!(
        routes.contains("posts::author_id.eq(owner_id)"),
        "sharded index must be owner-scoped:\n{routes}"
    );
    assert!(
        !routes.contains("mut db: Db"),
        "sharded handlers must use ShardedDb, not a bare Db extractor:\n{routes}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated sharded-owner scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Issue #1830 companion for the attachment variant: an owner column on an
/// attachment scaffold record-authorizes the mutating handlers by REUSING the
/// `state: State<AppState>` wrapper the multipart upload already threads
/// (`&*state`) rather than injecting a second, conflicting `State` extractor —
/// the exact case #1125 excluded. `cargo check --tests` proves the deref-based
/// authorize wiring compiles against the real framework.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_attachment_owner_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("attachment-owner-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "author_id:i64",
            "avatar:Attachment",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("autumn_web::authorization::authorize_create::<Post>(&*state, &session)"),
        "attachment create must authorize_create via the reused &*state wrapper:\n{routes}"
    );
    assert!(
        routes.contains(
            "autumn_web::authorization::authorize::<Post>(&*state, &session, \"update\", &current)"
        ),
        "attachment update must authorize via &*state:\n{routes}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated attachment-owner scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check (issue #1124): scaffold a model with a `--validate`
/// rule and prove the generated changeset round-trip actually compiles *and*
/// runs — a rejected submission gets 422 with the other field preserved and
/// an inline error, a valid one still succeeds (AC1-AC5, AC7). Runs the
/// generated `tests/<snake>.rs` changeset smoke test directly (it needs no
/// Docker/Postgres — see `render_validation_rejection_smoke_test`), not just
/// `cargo check`.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-builds and runs a fresh project's test suite — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_validated_scaffold_round_trip_test_passes() {
    let (_tmp, project) = fresh_project("scaffold-validation-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:String",
            "--validate",
            "title=length:min=1,max=200",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(routes.contains("pub struct PostForm"));
    assert!(routes.contains("into_changeset()"));
    assert!(routes.contains("StatusCode::UNPROCESSABLE_ENTITY"));

    let test_file = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(test_file.contains("posts_rejects_invalid_title_and_preserves_input"));

    let output = Command::new("cargo")
        .args([
            "test",
            "--test",
            "post",
            "posts_rejects_invalid_title_and_preserves_input",
        ])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the generated changeset round-trip test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("test result: ok"),
        "expected the generated test to pass:\n{stdout}"
    );

    // Issue #1127: the same generated binary also carries the in-process
    // write-path suite (create/update/delete + the validation-failure
    // re-render). Run it by name and prove it compiles and passes — no Docker,
    // no external services.
    let write_path = Command::new("cargo")
        .args(["test", "--test", "post", "posts_write_path_crud"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        write_path.status.success(),
        "the generated write-path CRUD test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&write_path.stdout),
        String::from_utf8_lossy(&write_path.stderr),
    );
    assert!(
        String::from_utf8_lossy(&write_path.stdout).contains("test result: ok"),
        "expected the generated write-path test to pass:\n{}",
        String::from_utf8_lossy(&write_path.stdout)
    );
}

/// Slow end-to-end check: scaffold a fresh project, run `autumn generate job`,
/// and `cargo check` the result against the local `autumn-web` crate. Verifies
/// the generator produces code that compiles without hand-editing.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_job_cargo_checks() {
    let (_tmp, project) = fresh_project("job-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "job",
            "SendWelcomeEmail",
            "user_id:i64",
            "email:String",
            "amount:decimal",
        ],
    );

    // The generated Cargo.toml must include serde and — since a `decimal`
    // field is present — rust_decimal (issue #1038 PR review: job_deps
    // previously omitted it, so the generated args struct referenced
    // rust_decimal::Decimal without the crate ever being declared).
    let cargo_toml = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo_toml.contains("serde"),
        "Cargo.toml must include serde after generate job"
    );
    assert!(
        cargo_toml.contains("rust_decimal"),
        "Cargo.toml must include rust_decimal after generate job with a decimal field"
    );

    // The generator must have created the expected files.
    assert!(
        project.join("src/jobs/send_welcome_email.rs").exists(),
        "src/jobs/send_welcome_email.rs must exist"
    );
    assert!(
        project.join("src/jobs/mod.rs").exists(),
        "src/jobs/mod.rs must exist"
    );

    // main.rs must be wired up.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("mod jobs;"), "main.rs must declare mod jobs");
    assert!(
        main.contains(".jobs(jobs::registered_jobs())"),
        "main.rs must include .jobs() call"
    );

    // The whole project must cargo-check cleanly (inline tests included).
    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated job failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

// ── `unique` field marker (issue #1032) ─────────────────────────────────────

#[test]
fn generate_model_unique_field_emits_unique_index() {
    let (_tmp, project) = fresh_project("unique-app");
    run_autumn(
        &project,
        &["generate", "model", "User", "email:String:unique"],
    );

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
        .expect("create_users migration should exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
        "got:\n{up}"
    );
    assert!(
        !up.contains("CREATE INDEX idx_users_email ON"),
        "a unique field must not also emit a plain, non-unique index; got:\n{up}"
    );
}

#[test]
fn generate_model_unique_flag_marks_field_unique() {
    let (_tmp, project) = fresh_project("unique-flag-app");
    run_autumn(
        &project,
        &[
            "generate",
            "model",
            "User",
            "email:String",
            "--unique",
            "email",
        ],
    );

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
        .expect("create_users migration should exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
        "got:\n{up}"
    );
}

#[test]
fn generate_model_unique_flag_rejects_unknown_field() {
    let (_tmp, project) = fresh_project("unique-flag-unknown-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "model",
            "User",
            "email:String",
            "--unique",
            "bogus",
        ],
    );
    assert_eq!(code, Some(1));
    assert!(stderr.contains("bogus"), "got: {stderr}");
}

#[test]
fn generate_migration_add_unique_column_emits_unique_index() {
    let (_tmp, project) = fresh_project("unique-migration-app");
    run_autumn(
        &project,
        &[
            "generate",
            "migration",
            "AddEmailToUsers",
            "email:String:unique",
        ],
    );

    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_add_email_to_users")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("ALTER TABLE users ADD COLUMN email TEXT NOT NULL;"),
        "got:\n{up}"
    );
    assert!(
        up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
        "got:\n{up}"
    );
}

#[test]
fn generate_migration_unique_flag_emits_unique_index() {
    let (_tmp, project) = fresh_project("unique-migration-flag-app");
    run_autumn(
        &project,
        &[
            "generate",
            "migration",
            "AddEmailToUsers",
            "email:String",
            "--unique",
            "email",
        ],
    );

    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_add_email_to_users")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
        "got:\n{up}"
    );
}

#[test]
fn generate_migration_remove_unique_column_rollback_restores_unique_index() {
    let (_tmp, project) = fresh_project("unique-migration-remove-app");
    run_autumn(
        &project,
        &[
            "generate",
            "migration",
            "RemoveEmailFromUsers",
            "email:String:unique",
        ],
    );

    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_remove_email_from_users")
        })
        .collect::<Vec<_>>();
    assert_eq!(migrations.len(), 1);
    let down = fs::read_to_string(migrations[0].path().join("down.sql")).unwrap();
    assert!(
        down.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
        "rollback must restore the UNIQUE index, not just the bare column; got:\n{down}"
    );
}

#[test]
fn generate_scaffold_unique_field_adds_find_by_query_to_repository() {
    let (_tmp, project) = fresh_project("unique-scaffold-repo-app");
    run_autumn(
        &project,
        &["generate", "scaffold", "User", "email:String:unique"],
    );

    let repo = fs::read_to_string(project.join("src/repositories/user.rs")).unwrap();
    assert!(
        repo.contains("fn find_by_email(email: String) -> Vec<User>;"),
        "a unique field must get a derived find_by_ repository lookup for free; got:\n{repo}"
    );
}

#[test]
fn generate_scaffold_unique_flag_adds_find_by_query_to_repository() {
    let (_tmp, project) = fresh_project("unique-scaffold-flag-repo-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String",
            "--unique",
            "email",
        ],
    );

    let repo = fs::read_to_string(project.join("src/repositories/user.rs")).unwrap();
    assert!(
        repo.contains("fn find_by_email(email: String) -> Vec<User>;"),
        "got:\n{repo}"
    );
}

#[test]
fn generate_scaffold_unique_field_does_not_duplicate_explicit_query() {
    let (_tmp, project) = fresh_project("unique-scaffold-explicit-query-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "--query",
            "find_by_email:email",
        ],
    );

    let repo = fs::read_to_string(project.join("src/repositories/user.rs")).unwrap();
    assert_eq!(
        repo.matches("fn find_by_email(").count(),
        1,
        "an explicit --query and the automatic unique-field derivation for the \
         same field must not produce two trait methods:\n{repo}"
    );
}

#[test]
fn generate_scaffold_unique_field_routes_handle_duplicate_with_inline_error() {
    let (_tmp, project) = fresh_project("unique-scaffold-routes-app");
    run_autumn(
        &project,
        &["generate", "scaffold", "User", "email:String:unique"],
    );

    let routes = fs::read_to_string(project.join("src/routes/users.rs")).unwrap();
    assert!(routes.contains("UNIQUE_CONSTRAINTS"), "got:\n{routes}");
    assert!(routes.contains("idx_users_email_unique"), "got:\n{routes}");
    assert!(
        routes.contains("autumn_web::error::unique_violation_field"),
        "got:\n{routes}"
    );
    assert!(
        routes.contains("StatusCode::UNPROCESSABLE_ENTITY"),
        "got:\n{routes}"
    );
    assert!(
        !routes.contains(
            "pub async fn create(flash: Flash, mut db: Db, body: Bytes) -> AutumnResult<Markup>"
        ),
        "a scaffold with a unique field must not use the plain create signature \
         that can only ever 500 on a duplicate; got:\n{routes}"
    );
}

#[test]
fn generate_scaffold_unique_field_create_violation_form_preserves_submitted_values() {
    // Regression guard (issue #1032 review follow-up): a duplicate
    // submission must not wipe every field the user entered — only the
    // colliding unique field needs an inline error; every other field's
    // submitted value should still be pre-filled when the form re-renders
    // with a 422.
    let (_tmp, project) = fresh_project("unique-create-preserve-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "age:i32",
            "active:bool",
            "status:enum{draft,published}",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/users.rs")).unwrap();
    // The plain `new_form` (blank form, no prior submission) must stay
    // untouched — no reference to `new` at all.
    let new_form_start = routes
        .find("pub async fn new_form")
        .expect("new_form handler");
    let create_start = routes.find("pub async fn create(").expect("create handler");
    let new_form_body = &routes[new_form_start..create_start];
    assert!(
        !new_form_body.contains("new."),
        "the blank new_form must not reference `new`; got:\n{new_form_body}"
    );

    // The violation-branch re-render (spliced into `create`) rebuilds the
    // changeset with the duplicate error via `Changeset::from_errors` and
    // re-renders through the same changeset-aware helpers, so every submitted
    // value is preserved (issue #1124 unifies the unique path with validation).
    let create_body = &routes[create_start..];
    assert!(
        create_body.contains("Changeset::from_errors(changeset.into_inner(), errors)"),
        "got:\n{create_body}"
    );
    // The re-render goes through the same shared `form_for` helper as the GET
    // views (issue #1135) — every submitted value is preserved because the
    // controls all read from the rebuilt changeset.
    assert!(
        create_body.contains("user_form_for(&changeset"),
        "got:\n{create_body}"
    );
    assert!(
        routes.contains(".override_field(\"status\", autumn_web::form::FieldControl::Select"),
        "got:\n{routes}"
    );
}

#[test]
fn generate_scaffold_unique_field_update_violation_form_preserves_submitted_values() {
    // Regression guard (issue #1032 review follow-up): the update path's
    // violation re-render used to always source field values from the
    // stale, re-fetched `row` (its pre-update values), silently reverting
    // every other field the user had changed in the same submission back to
    // what was stored before — even though only the unique column actually
    // collided. It must instead source from `form`, the just-decoded
    // (otherwise valid) rejected submission, the same way the create path
    // already does from `new`.
    let (_tmp, project) = fresh_project("unique-update-preserve-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "age:i32",
            "active:bool",
            "status:enum{draft,published}",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/users.rs")).unwrap();
    // The plain `edit_form` seeds its changeset from the persisted row.
    let edit_form_start = routes
        .find("pub async fn edit_form")
        .expect("edit_form handler");
    let update_start = routes.find("pub async fn update(").expect("update handler");
    let edit_form_body = &routes[edit_form_start..update_start];
    assert!(
        edit_form_body.contains("Changeset::new(UserForm::from(&row))"),
        "the edit_form seeds a changeset from the loaded row; got:\n{edit_form_body}"
    );

    // The violation-branch re-render (spliced into `update`) preserves the
    // *submitted* edits by rebuilding the changeset from the decoded form via
    // `Changeset::from_errors` — no stale `row` refetch (issue #1124). Scope to
    // the `update` handler only (the trailing `destroy` handler also loads its
    // row for the authorize preamble).
    let update_only = &routes[update_start..];
    let destroy_off = update_only
        .find("pub async fn destroy")
        .expect("destroy handler after update");
    let update_body = &update_only[..destroy_off];
    assert!(
        update_body.contains("Changeset::from_errors(changeset.into_inner(), errors)"),
        "got:\n{update_body}"
    );
    // Same shared `form_for` helper as the GET views (issue #1135): the
    // rebuilt changeset carries the submitted values into every control.
    assert!(
        update_body.contains("user_form_for(&changeset"),
        "got:\n{update_body}"
    );
    // Issue #1830: the mutating handlers now record-authorize the actor, which
    // loads the target row ONCE up front (`.first(&mut *db)` → `authorize`). That
    // load feeds the policy check, NOT the 422 re-render — the violation branch
    // still rebuilds the changeset from the submitted `form`, never a refetched
    // row. So the only row load in `update` is the authorize preamble.
    assert_eq!(
        update_body.matches(".first(&mut *db)").count(),
        1,
        "the update violation path must not re-fetch the row for its re-render; \
         the only load is the authorize preamble; got:\n{update_body}"
    );
    assert!(
        update_body.contains("authorize::<User>(&state, &session, \"update\", &current)"),
        "the single row load must be the authorize preamble; got:\n{update_body}"
    );
}

#[test]
fn generate_scaffold_unique_field_emits_duplicate_rejection_smoke_test() {
    let (_tmp, project) = fresh_project("unique-scaffold-smoke-app");
    run_autumn(
        &project,
        &["generate", "scaffold", "User", "email:String:unique"],
    );

    let test_file = fs::read_to_string(project.join("tests/user.rs")).unwrap();
    assert!(
        test_file.contains("async fn users_rejects_duplicate_email()"),
        "got:\n{test_file}"
    );
    assert!(
        test_file.contains("idx_users_email_unique"),
        "got:\n{test_file}"
    );
    assert!(
        test_file.contains("autumn_web::error::unique_violation_field"),
        "got:\n{test_file}"
    );
    // The DB-level check: a real duplicate INSERT must violate the UNIQUE
    // index and be classified by field.
    assert!(
        test_file.contains("must violate the UNIQUE index"),
        "got:\n{test_file}"
    );
    // The request-boundary check: a stand-in POST handler proves the full
    // 422-with-inline-error path (issue #1032's success metric).
    assert!(
        test_file.contains(".assert_status(422)") && test_file.contains(".assert_status(200)"),
        "got:\n{test_file}"
    );
    // Regression guard: the DB-level check above already inserts the target
    // value once, so the table must be truncated again before the first
    // client POST — otherwise that "must succeed with 200" request collides
    // with the row the DB-level check left behind and the smoke test fails
    // under `cargo test -- --ignored`.
    let before_200 = test_file
        .split(".assert_status(200)")
        .next()
        .expect("assert_status(200) must appear in the generated test");
    assert!(
        before_200
            .matches("TRUNCATE users RESTART IDENTITY")
            .count()
            >= 2,
        "the table must be truncated again before the request-boundary POSTs; got:\n{test_file}"
    );
}

#[test]
fn generate_scaffold_unique_enum_field_smoke_test_inserts_a_valid_variant() {
    // Regression guard: the duplicate-insert smoke test's first INSERT must
    // actually succeed so the *second* insert is the one that trips the
    // UNIQUE index. A generic `'dup_value'` literal is not a valid enum
    // variant and would fail the first insert on the enum's CHECK
    // constraint instead, so the target field's sample value must be one of
    // its declared variants.
    let (_tmp, project) = fresh_project("unique-enum-smoke-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "status:enum{draft,published}:unique",
        ],
    );

    let test_file = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(
        test_file.contains("async fn posts_rejects_duplicate_status()"),
        "got:\n{test_file}"
    );
    assert!(
        !test_file.contains("'dup_value'"),
        "a unique enum field's smoke test must not insert an invalid variant \
         literal; got:\n{test_file}"
    );
    assert!(
        test_file.contains("(status) VALUES ('draft')"),
        "the target enum field must be seeded with its first declared \
         variant; got:\n{test_file}"
    );
}

#[test]
fn generate_scaffold_unique_reference_field_smoke_test_uses_seeded_fk_value() {
    // Regression guard (issue #1032 review follow-up): `unique_sample_literal`
    // used to lump `references` in with plain integers and emit an arbitrary
    // `424242` FK value. `render_reference_stub_tables_sql` seeds real rows
    // (ids 1 and 2) into the stub target table, so that arbitrary id fails
    // the FK constraint before the insert ever reaches the UNIQUE index this
    // test exists to exercise.
    let (_tmp, project) = fresh_project("unique-reference-smoke-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Membership",
            "profile:references:unique",
        ],
    );

    let test_file = fs::read_to_string(project.join("tests/membership.rs")).unwrap();
    assert!(
        test_file.contains("async fn memberships_rejects_duplicate_profile_id()"),
        "got:\n{test_file}"
    );
    assert!(
        !test_file.contains("424242"),
        "a unique `references` field's smoke test must not insert an \
         arbitrary FK id that doesn't exist in the seeded stub table; \
         got:\n{test_file}"
    );
    assert!(
        test_file.contains("(profile_id) VALUES (1)"),
        "the target references field must be seeded with the stub table's \
         real row id; got:\n{test_file}"
    );
}

#[test]
fn generate_scaffold_multiple_unique_fields_smoke_test_isolates_the_target_field() {
    // Regression guard (issue #1032 review follow-up): with more than one
    // `unique` field, the duplicate-insert SQL used to fill every *other*
    // required column (including other unique ones) with the same fixed
    // literal on both inserts. Testing `username`'s duplicate would then
    // *also* duplicate `email`, so Postgres could report `email`'s
    // constraint first and the `assert_eq!(field, "username")` assertion
    // would fail even though `username`'s constraint genuinely exists. The
    // second (duplicate) insert must vary every *other* unique column's
    // value so only the field under test actually collides.
    let (_tmp, project) = fresh_project("unique-multi-field-smoke-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "username:String:unique",
        ],
    );

    let test_file = fs::read_to_string(project.join("tests/user.rs")).unwrap();
    let username_test_start = test_file
        .find("async fn users_rejects_duplicate_username()")
        .expect("users_rejects_duplicate_username test should exist");
    let email_test_start = test_file
        .find("async fn users_rejects_duplicate_email()")
        .expect("users_rejects_duplicate_email test should exist");
    // The two generated tests can appear in either order; slice from
    // whichever comes first to the end so this doesn't depend on it.
    let (test_body, other_field_name) = if username_test_start < email_test_start {
        (&test_file[username_test_start..], "email")
    } else {
        (&test_file[email_test_start..], "username")
    };
    // Within a single duplicate-rejection test, the two `diesel::sql_query`
    // calls (first insert, then the duplicate) must use different literals
    // for the *other* unique field so it doesn't also collide.
    let insert_calls: Vec<&str> = test_body
        .lines()
        .filter(|l| l.contains("diesel::sql_query(") && l.contains(other_field_name))
        .take(2)
        .collect();
    assert_eq!(
        insert_calls.len(),
        2,
        "expected two inserts referencing the non-target unique field \
         {other_field_name}; got:\n{test_body}"
    );
    assert_ne!(
        insert_calls[0], insert_calls[1],
        "the first and duplicate inserts must give the non-target unique \
         field {other_field_name} different values, or it would collide \
         with itself the same way the target field is meant to; got:\n{test_body}"
    );
}

#[test]
fn generate_scaffold_multiple_unique_fields_request_boundary_check_isolates_the_target_field() {
    // Regression guard (issue #1032 review follow-up): the DB-level check's
    // duplicate insert was fixed to vary non-target unique columns (see
    // generate_scaffold_multiple_unique_fields_smoke_test_isolates_the_target_field),
    // but the request-boundary stand-in handler is a single compiled
    // function invoked via two separate HTTP calls with no way to tell
    // "first call" from "second call" apart -- it kept reusing one literal
    // insert for both, so a non-target unique column would still collide
    // with itself. It must instead pick between the two insert variants at
    // runtime based on whether the table is still empty.
    let (_tmp, project) = fresh_project("unique-multi-field-boundary-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "username:String:unique",
        ],
    );

    let test_file = fs::read_to_string(project.join("tests/user.rs")).unwrap();
    for (field, other_field) in [("email", "username"), ("username", "email")] {
        let test_start = test_file
            .find(&format!("async fn users_rejects_duplicate_{field}()"))
            .unwrap_or_else(|| panic!("users_rejects_duplicate_{field} test should exist"));
        let next_test_start = test_file[test_start + 1..]
            .find("async fn users_rejects_duplicate_")
            .map_or(test_file.len(), |i| test_start + 1 + i);
        let test_body = &test_file[test_start..next_test_start];

        assert!(
            test_body
                .contains("let existing: i64 = users::table.count().get_result(&mut *db).await?;"),
            "the request-boundary handler must branch on the table's row \
             count to tell the two calls apart; got:\n{test_body}"
        );
        let branch_line = test_body
            .lines()
            .find(|l| l.contains("let insert_sql = if existing == 0"))
            .unwrap_or_else(|| panic!("expected an insert_sql branch; got:\n{test_body}"));
        // Pull out just the quoted SQL literal from each branch (the second
        // `"..."`-delimited token — the first is the `if existing == 0`
        // condition itself, which has no quotes) so the comparison is
        // against the actual INSERT statement, not incidental surrounding
        // syntax that always differs between an if- and else-arm.
        let quoted: Vec<&str> = branch_line.split('"').collect();
        assert!(
            quoted.len() >= 4,
            "expected two quoted SQL literals in the branch; got: {branch_line}"
        );
        let (if_sql, else_sql) = (quoted[1], quoted[3]);
        assert_ne!(
            if_sql, else_sql,
            "the two insert_sql branches must give the non-target unique \
             field {other_field} different values, or the second HTTP call \
             would collide on it too; got: {branch_line}"
        );
    }
}

#[test]
fn generate_scaffold_long_unique_field_name_agrees_across_migration_and_routes() {
    // Regression guard (issue #1032 review follow-up): `idx_<table>_<field>_
    // unique` is unbounded, and PostgreSQL silently truncates identifiers
    // past its 63-byte limit. If the migration's `CREATE UNIQUE INDEX` and
    // the generated routes' `UNIQUE_CONSTRAINTS` computed that name
    // independently, a long enough table/field combination would make them
    // disagree with what Postgres actually names the index, and a real
    // duplicate submission would fall through to a 500 instead of the
    // intended 422. Both must resolve to the identical (possibly
    // truncated+hashed) name.
    let (_tmp, project) = fresh_project("unique-longname-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "AVeryLongModelNameForTruncationTesting",
            "an_equally_long_field_name_for_good_measure:String:unique",
        ],
    );

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_create_a_very_long_model_name_for_truncation_testings")
        })
        .expect("create migration for the long-named model should exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    let index_line = up
        .lines()
        .find(|l| l.starts_with("CREATE UNIQUE INDEX"))
        .unwrap_or_else(|| panic!("expected a CREATE UNIQUE INDEX line; got:\n{up}"));
    let index_name = index_line
        .strip_prefix("CREATE UNIQUE INDEX ")
        .and_then(|rest| rest.split(' ').next())
        .expect("index name token");
    assert!(
        index_name.len() <= 63,
        "index name must fit Postgres's identifier limit, got {} bytes: {index_name}",
        index_name.len()
    );

    let routes = fs::read_to_string(
        project.join("src/routes/a_very_long_model_name_for_truncation_testings.rs"),
    )
    .unwrap();
    assert!(
        routes.contains(&format!("\"{index_name}\"")),
        "the generated routes' UNIQUE_CONSTRAINTS must reference the exact \
         same (possibly truncated) index name as the migration; index_name=\
         {index_name}, got:\n{routes}"
    );
}

#[test]
fn generate_scaffold_unique_field_avoids_name_collision_with_coincidentally_named_index() {
    // Regression guard (issue #1032 review follow-up): a plain index always
    // names itself after its own column (`idx_<table>_<name>`, no `_unique`
    // suffix). A field literally named `<other_field>_unique` that also
    // gets a plain index therefore claims the exact name
    // `<other_field>:unique` would otherwise compute for itself
    // (`idx_<table>_<other_field>_unique`), even though the two fields are
    // otherwise unrelated. Without disambiguation the generated migration
    // fails with "relation already exists" before the table is usable.
    let (_tmp, project) = fresh_project("unique-collision-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "email_unique:String",
            "--index",
            "email_unique",
        ],
    );

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
        .expect("create_users migration should exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE INDEX idx_users_email_unique ON users (email_unique);"),
        "got:\n{up}"
    );
    assert!(
        !up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
        "the unique index must not collide with the plain index's exact \
         name; got:\n{up}"
    );

    let disambiguated_line = up
        .lines()
        .find(|l| l.starts_with("CREATE UNIQUE INDEX") && l.contains(" (email);"))
        .unwrap_or_else(|| panic!("expected a disambiguated unique index for email; got:\n{up}"));
    let index_name = disambiguated_line
        .strip_prefix("CREATE UNIQUE INDEX ")
        .and_then(|rest| rest.split(' ').next())
        .expect("index name token");
    assert_ne!(index_name, "idx_users_email_unique");

    // The generated routes' UNIQUE_CONSTRAINTS must reference the exact
    // same disambiguated name, or a real duplicate submission would fall
    // through to a 500 instead of the intended 422.
    let routes = fs::read_to_string(project.join("src/routes/users.rs")).unwrap();
    assert!(
        routes.contains(&format!("\"{index_name}\"")),
        "got:\n{routes}"
    );
}

#[test]
fn generate_migration_add_unique_field_avoids_collision_with_earlier_migrations_column() {
    // Regression guard (issue #1032 review follow-up): the create-table
    // collision fix above only sees the fields in a single `generate`
    // invocation. This reproduces the cross-migration case Codex flagged: a
    // table already has a plain-indexed `email_unique` column from an
    // earlier `generate scaffold`, and a *later*, separate `generate
    // migration AddEmailToUsers email:String:unique` call must still avoid
    // colliding with it -- `src/schema.rs` (kept in sync by the scaffold
    // generator) is what lets the migration generator see across that gap.
    let (_tmp, project) = fresh_project("unique-cross-migration-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email_unique:String",
            "--index",
            "email_unique",
        ],
    );
    run_autumn(
        &project,
        &[
            "generate",
            "migration",
            "AddEmailToUsers",
            "email:String:unique",
        ],
    );

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_add_email_to_users")
        })
        .expect("add_email_to_users migration should exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(
        !up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
        "must not collide with the pre-existing email_unique column's plain \
         index (created by the earlier `generate scaffold` call); got:\n{up}"
    );
    assert!(
        up.contains("CREATE UNIQUE INDEX idx_users_email_unique_"),
        "the unique index must still exist, under a disambiguated name; \
         got:\n{up}"
    );
}

#[test]
fn generate_scaffold_unique_attachment_field_is_skipped_in_smoke_test() {
    // A unique constraint on an always-nullable attachment blob is a
    // degenerate case (Postgres allows unlimited NULLs under a unique
    // index) — no meaningful violation to provoke, so no smoke test should
    // be emitted for it.
    let (_tmp, project) = fresh_project("unique-attachment-smoke-app");
    run_autumn(
        &project,
        &["generate", "scaffold", "Document", "file:Attachment:unique"],
    );

    let test_file = fs::read_to_string(project.join("tests/document.rs")).unwrap();
    assert!(
        !test_file.contains("rejects_duplicate_file"),
        "got:\n{test_file}"
    );
}

/// PR #1867 review (Finding 2): destroying the last attachment *model* must
/// not strip `autumn-web/multipart` or `autumn-web/storage` from Cargo.toml
/// when a hand-written route still uses those APIs — otherwise the project
/// stops compiling. The `Revert::CargoAutumnWebFeature` bookkeeping alone
/// would strip them (its `owner_dir` sibling check only sees the scaffold's
/// own `src/models`), so `autumn_web_feature_markers` must retain them via a
/// whole-project marker scan.
#[test]
fn destroy_attachment_scaffold_keeps_features_used_by_handwritten_route() {
    let (_tmp, project) = fresh_project("destroy-attachment-features-app");

    run_autumn(
        &project,
        &["generate", "scaffold", "Document", "file:Attachment"],
    );

    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo_before.contains("\"multipart\"") && cargo_before.contains("\"storage\""),
        "attachment scaffold must enable multipart+storage, got:\n{cargo_before}"
    );

    // A hand-written route that uses both the multipart extractor and the
    // blob store directly — unrelated to the scaffold's generated files. It
    // imports through `use autumn_web::prelude::*;` and names `Multipart`
    // UNQUALIFIED (the prelude re-exports it), the shape the earlier
    // `extract::Multipart` marker would have missed — so destroy would have
    // wrongly stripped `multipart`. The blob store stays a
    // `autumn_web::storage::` path since the prelude re-exports no storage type.
    fs::write(
        project.join("src/routes/manual.rs"),
        "use autumn_web::prelude::*;\n\n\
         pub async fn manual_upload(\n    \
         mut multipart: Multipart,\n    \
         store: autumn_web::storage::BlobStoreState,\n\
         ) {\n    let _ = (&mut multipart, &store);\n}\n",
    )
    .unwrap();

    run_autumn(
        &project,
        &["destroy", "scaffold", "Document", "file:Attachment"],
    );

    let cargo_after = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo_after.contains("\"multipart\""),
        "multipart must be retained while a hand-written route uses the Multipart extractor, got:\n{cargo_after}"
    );
    assert!(
        cargo_after.contains("\"storage\""),
        "storage must be retained while a hand-written route references autumn_web::storage::, got:\n{cargo_after}"
    );
}

#[test]
fn generate_scaffold_without_unique_field_omits_unique_constraints() {
    // A scaffold with NO unique fields emits no UNIQUE_CONSTRAINTS const, but
    // (issue #1124) every scaffold now uses the changeset round-trip: create
    // returns a Response and carries the CSRF params for the 422 re-render.
    let (_tmp, project) = fresh_project("no-unique-scaffold-routes-app");
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(!routes.contains("UNIQUE_CONSTRAINTS"), "got:\n{routes}");
    // Issue #1830: no-owner scaffolds record-authorize their handlers too, so
    // `State`/`Session` are threaded in before the last `body: Bytes` extractor.
    assert!(
        routes.contains(
            "pub async fn create(flash: Flash, csrf: Option<CsrfToken>, csrf_field: Option<CsrfFormField>, submit_token: Option<SubmitToken>, submit_field: Option<SubmitFormField>, mut db: Db, \n    autumn_web::extract::State(state): autumn_web::extract::State<autumn_web::AppState>,\n    session: autumn_web::session::Session,\n    body: Bytes) -> AutumnResult<autumn_web::reexports::axum::response::Response>"
        ),
        "got:\n{routes}"
    );
    assert!(routes.contains("form.into_changeset()"), "got:\n{routes}");
}

#[test]
fn generate_model_help_documents_unique_field() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "model", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("unique"), "got:\n{stdout}");
    assert!(stdout.contains("--unique"), "got:\n{stdout}");
}

#[test]
fn generate_scaffold_help_documents_unique_field() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "scaffold", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("unique"), "got:\n{stdout}");
    assert!(stdout.contains("--unique"), "got:\n{stdout}");
}

/// Issue #1340: the `{encrypted}` modifier must be discoverable from the help
/// of the two subcommands that actually accept it. Documenting it only on the
/// parent `autumn generate --help` is not enough — clap builds each
/// subcommand's long help from its own doc block, and `model`/`scaffold` are
/// what a user checking "how do I declare this field?" actually runs.
#[test]
fn generate_model_help_documents_the_encrypted_modifier() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "model", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("{encrypted}"), "got:\n{stdout}");
    assert!(
        stdout.contains("{encrypted:deterministic}"),
        "got:\n{stdout}"
    );
    // The one manual step the generator cannot do for the user.
    assert!(stdout.contains("autumn credentials edit"), "got:\n{stdout}");
}

#[test]
fn generate_scaffold_help_documents_the_encrypted_modifier() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "scaffold", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("{encrypted}"), "got:\n{stdout}");
    assert!(
        stdout.contains("{encrypted:deterministic}"),
        "got:\n{stdout}"
    );
    assert!(stdout.contains("autumn credentials edit"), "got:\n{stdout}");
}

/// The advertised examples must survive a copy-paste into bash/zsh: an
/// unquoted `{…}` is brace-expanded by the shell before `autumn` ever sees it,
/// so every `String{encrypted…}` token shown in help must be single-quoted.
///
/// Checked positionally rather than per-line, so it holds whichever way clap
/// wraps the block (`verbatim_doc_comment` or reflowed) and for examples given
/// inline in a paragraph as well as in an `Examples:` list.
#[test]
fn generate_help_encrypted_examples_are_shell_quoted() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    for args in [
        vec!["generate", "--help"],
        vec!["generate", "model", "--help"],
        vec!["generate", "scaffold", "--help"],
    ] {
        let label = args.join(" ");
        let output = Command::new(autumn_bin)
            .args(&args)
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut checked = 0usize;
        for (idx, _) in stdout.match_indices("String{encrypted") {
            // Walk back to the start of the `name:String{encrypted…}` token.
            let token_start = stdout[..idx]
                .rfind(|c: char| c.is_whitespace())
                .map_or(0, |i| i + 1);
            // Only the runnable examples need quoting — a bare mention of the
            // syntax in prose is wrapped in markdown backticks instead.
            let is_example = stdout[..token_start].ends_with("autumn generate ")
                || stdout[..token_start]
                    .rsplit('\n')
                    .next()
                    .is_some_and(|line| line.contains("autumn generate "));
            if !is_example {
                continue;
            }
            checked += 1;
            assert!(
                stdout[token_start..].starts_with('\''),
                "`{label}` shows an unquoted example — bash/zsh would \
                 brace-expand it before `autumn` sees it: {}",
                &stdout[token_start
                    ..stdout[token_start..]
                        .find(char::is_whitespace)
                        .map_or(stdout.len(), |i| token_start + i)]
            );
        }
        // `generate --help` and both subcommands each advertise at least one.
        assert!(checked > 0, "`{label}` shows no runnable encrypted example");
    }
}

#[test]
fn generate_scaffold_help_documents_unique_is_html_only() {
    // Regression guard (issue #1032 review follow-up): `unique`'s 422
    // inline-error handling is only wired into the HTML routes generator —
    // an `--api` scaffold's JSON CRUD routes are auto-generated by
    // `#[repository]` and a duplicate create/update there still falls
    // through to a 500. This is a deliberately deferred slice (also
    // recorded in CHANGELOG.md), but it must say so in `--help` too, or a
    // `--api` user has no way to discover the gap before hitting it.
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "scaffold", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("HTML-only"), "got:\n{stdout}");
    assert!(stdout.contains("--api"), "got:\n{stdout}");
    assert!(stdout.contains("#[repository]"), "got:\n{stdout}");
}

#[test]
fn generate_migration_help_documents_unique_field() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "migration", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("unique"), "got:\n{stdout}");
    assert!(stdout.contains("--unique"), "got:\n{stdout}");
}

#[test]
fn generate_scaffold_with_unique_dry_run_lists_migration_file() {
    let (_tmp, project) = fresh_project("unique-dryrun-app");
    let (stdout, _stderr) = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "--dry-run",
        ],
    );
    assert!(stdout.contains("Dry run"));
    assert!(stdout.contains("src/models/user.rs"));
    // The migration file (which will contain the UNIQUE index — see
    // generate_model_unique_field_emits_unique_index for the SQL-level
    // assertion) shows up in the dry-run's file plan like every other
    // generated file.
    assert!(
        stdout.contains("_create_users") && stdout.contains("up.sql"),
        "the migration file plan must be listed under --dry-run; got:\n{stdout}"
    );
    assert!(!project.join("src/models/user.rs").exists());
}

/// Slow end-to-end check: scaffold a `unique` field and `cargo check` the
/// result. The generated `create`/`update` handlers (issue #1032) use a
/// hand-templated `maud::html!` re-render on a constraint violation — this
/// is the one test that actually compiles that generated code, catching any
/// template/escaping mistake a string-content assertion alone would miss.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_unique_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("unique-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &["generate", "scaffold", "User", "email:String:unique"],
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated unique scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check (issue #1260): scaffold a `slug` field and `cargo
/// check` the result. The rekeyed `show`/`edit`/`update`/`delete` handlers
/// (`Path<String>` instead of `Path<i64>`, `.filter(...)` instead of
/// `.find(*id)`, the create-time collision-suffix loop) are hand-templated
/// string codegen with no compiler feedback at generation time — this is the
/// one test that actually compiles that generated code, catching any
/// template/escaping/borrow mistake a string-content assertion alone would
/// miss.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
// `"slug:slug{from:title}"` is a literal DSL token passed to the CLI, not a
// format string — the `{…}` is the scaffold's own constraint-modifier syntax.
#[allow(clippy::literal_string_with_formatting_args)]
fn generated_slug_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("slug-scaffold-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            r"slug:slug{from:title}",
        ],
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated slug scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// `--live` (SSE-backed repository writes) routes the insert/update through
/// `repo.save`/`repo.update` instead of a bare `diesel::insert_into` — a
/// different error-propagation path into `unique_violation_field` (see its
/// doc comment on recovering the diesel error either way). This can't be a
/// `cargo check` test like [`generated_unique_scaffold_cargo_checks`]:
/// `--live` scaffolds fail to compile independent of `unique` (a pre-existing
/// `<Model>DraftExt` import gap in `broadcasts = true` repositories,
/// reproducible with zero unique fields — out of scope for issue #1032), so
/// this only asserts the unique-aware codegen paths are wired correctly.
#[test]
fn generate_scaffold_unique_live_field_wires_repository_save_and_db_refetch() {
    let (_tmp, project) = fresh_project("unique-live-scaffold-app");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "User",
            "email:String:unique",
            "--live",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/users.rs")).unwrap();
    assert!(
        routes.contains("repo.save(&new).await?;"),
        "the create failure branch must still route inserts through the \
         repository on --live, not a bare diesel call; got:\n{routes}"
    );
    assert!(
        routes.contains("repo.update(*id, &update_changes).await?;"),
        "got:\n{routes}"
    );
    // Regression guard (issue #1032 review follow-up): `update`'s signature
    // must NOT carry a second `Db` extractor alongside `repo` — holding both a
    // `Db` and `repo`'s own checkout at once self-deadlocks a pool sized for
    // one connection per request. Issue #1124 drops the unique-violation
    // row-refetch entirely (the 422 re-renders the submitted changeset), so the
    // live update path no longer needs any refetch connection at all.
    let update_start = routes.find("pub async fn update(").expect("update handler");
    let update_body = &routes[update_start..];
    assert!(
        !update_body.contains("mut db:"),
        "the live update handler must not also take a `Db` extractor; \
         got:\n{update_body}"
    );
    assert!(
        update_body.contains("Changeset::from_errors(changeset.into_inner(), errors)"),
        "the update violation path re-renders the submitted changeset; got:\n{update_body}"
    );
    assert!(routes.contains("repo: PgUserRepository"), "got:\n{routes}");
    assert!(
        routes.contains("autumn_web::error::unique_violation_field"),
        "got:\n{routes}"
    );
}

// ── autumn generate channel integration tests ─────────────────────────────────

#[test]
fn generate_channel_creates_all_expected_files() {
    let (_tmp, project) = fresh_project("channel-app");
    let (stdout, _stderr) = run_autumn(&project, &["generate", "channel", "Chat"]);
    assert!(
        stdout.contains("chat.rs") || stdout.contains("Created"),
        "output should mention created files: {stdout}"
    );

    assert!(project.join("src/channels/chat.rs").is_file());
    let channel = fs::read_to_string(project.join("src/channels/chat.rs")).unwrap();
    assert!(channel.contains(r#"#[get("/chat")]"#));
    assert!(channel.contains(r#"#[get("/chat/events")]"#));
    assert!(channel.contains(r#"#[post("/chat/messages")]"#));
    assert!(channel.contains(r#"sse-connect="/chat/events""#));
    assert!(channel.contains(r#"sse-swap="message""#));
    assert!(channel.contains("autumn_web::sse::stream"));

    assert!(project.join("src/channels/mod.rs").is_file());
    let mod_rs = fs::read_to_string(project.join("src/channels/mod.rs")).unwrap();
    assert!(mod_rs.contains("pub mod chat;"));

    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("mod channels;"));
    assert!(main.contains("channels::chat::chat_page"));
    assert!(main.contains("channels::chat::chat_events"));
    assert!(main.contains("channels::chat::chat_publish"));

    assert!(project.join("tests/chat_channel.rs").is_file());
    let test_src = fs::read_to_string(project.join("tests/chat_channel.rs")).unwrap();
    assert!(test_src.contains("TestApp"));
    assert!(test_src.contains(".subscribe("));
    assert!(!test_src.contains("#[ignore"));

    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"ws\""),
        "Cargo.toml must enable the ws feature: {cargo}"
    );
    assert!(cargo.contains("maud"));
    assert!(cargo.contains("serde"));
}

#[test]
fn generate_channel_ws_emits_ws_handler() {
    let (_tmp, project) = fresh_project("channel-ws-app");
    run_autumn(&project, &["generate", "channel", "Chat", "--ws"]);

    let channel = fs::read_to_string(project.join("src/channels/chat.rs")).unwrap();
    assert!(channel.contains(r#"#[ws("/chat/ws")]"#));
    assert!(channel.contains("WithShutdown"));
    assert!(!channel.contains("sse-connect"));

    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("channels::chat::chat_ws"));
    assert!(main.contains("channels::chat::chat_publish"));

    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("\"ws\""));
}

/// Regression test: `autumn generate channel Chat --ws --force` after a
/// plain SSE run must not leave `main.rs` referencing `chat_page`/
/// `chat_events`, which `src/channels/chat.rs` no longer defines once
/// `--ws` overwrites it.
#[test]
fn generate_channel_force_transport_switch_does_not_strand_stale_routes() {
    let (_tmp, project) = fresh_project("channel-switch-app");
    run_autumn(&project, &["generate", "channel", "Chat"]);
    run_autumn(
        &project,
        &["generate", "channel", "Chat", "--ws", "--force"],
    );

    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !main.contains("chat_page"),
        "stale SSE page route must be removed: {main}"
    );
    assert!(
        !main.contains("chat_events"),
        "stale SSE events route must be removed: {main}"
    );
    assert!(main.contains("channels::chat::chat_ws"));
    assert!(main.contains("channels::chat::chat_publish"));

    let channel = fs::read_to_string(project.join("src/channels/chat.rs")).unwrap();
    assert!(channel.contains(r#"#[ws("/chat/ws")]"#));
}

#[test]
fn generate_channel_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("channel-dry-app");
    let (stdout, _) = run_autumn(&project, &["generate", "channel", "Chat", "--dry-run"]);
    assert!(
        stdout.contains("Dry run"),
        "dry run must print Dry run header: {stdout}"
    );
    assert!(!project.join("src/channels/chat.rs").exists());
    assert!(!project.join("tests/chat_channel.rs").exists());
}

#[test]
fn generate_channel_collision_without_force_fails() {
    let (_tmp, project) = fresh_project("channel-collide-app");
    run_autumn(&project, &["generate", "channel", "Chat"]);
    let (_, stderr, code) = run_autumn_failing(&project, &["generate", "channel", "Chat"]);
    assert_eq!(code, Some(1), "second run without --force must exit 1");
    assert!(
        stderr.contains("would overwrite") || stderr.contains("chat.rs"),
        "must report collision: {stderr}"
    );
}

#[test]
fn generate_channel_force_overwrites_existing() {
    let (_tmp, project) = fresh_project("channel-force-app");
    run_autumn(&project, &["generate", "channel", "Chat"]);
    let path = project.join("src/channels/chat.rs");
    fs::write(&path, "// corrupted").unwrap();
    run_autumn(&project, &["generate", "channel", "Chat", "--force"]);
    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("TOPIC"),
        "--force must regenerate the channel file"
    );
}

#[test]
fn generate_channel_sse_ws_conflict_fails() {
    let (_tmp, project) = fresh_project("channel-conflict-app");
    let (_, stderr, code) =
        run_autumn_failing(&project, &["generate", "channel", "Chat", "--sse", "--ws"]);
    assert_ne!(code, Some(0), "--sse and --ws together must fail");
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "clap should report the conflicting flags: {stderr}"
    );
}

/// Slow end-to-end check: scaffold a fresh project, run `autumn generate
/// channel` (default SSE transport), and `cargo check --tests` the result
/// against the local `autumn-web` crate. Verifies the generator adds every
/// dep its emitted code needs and that the generated application and smoke
/// test actually type-check.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_channel_cargo_checks() {
    let (_tmp, project) = fresh_project("channel-build");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "channel", "Chat"]);

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated channel (SSE) failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Same as [`generated_channel_cargo_checks`] but for the `--ws` transport.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_channel_ws_cargo_checks() {
    let (_tmp, project) = fresh_project("channel-ws-build");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "channel", "Chat", "--ws"]);

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated channel (--ws) failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check: scaffold a fresh project, run `autumn generate
/// channel`, and actually run the generated smoke test with `cargo test`.
/// This is the acceptance-criterion proof that the smoke test does not just
/// compile but passes on first run with no manual edits — it publishes a
/// message and asserts a live subscriber receives it.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: builds and runs a fresh project's test suite — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_channel_smoke_test_passes() {
    let (_tmp, project) = fresh_project("channel-smoke-build");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "channel", "Chat"]);

    let test_run = Command::new("cargo")
        .args(["test", "--test", "chat_channel"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        test_run.status.success(),
        "generated chat_channel smoke test failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&test_run.stdout),
        String::from_utf8_lossy(&test_run.stderr),
    );
    assert!(
        String::from_utf8_lossy(&test_run.stdout).contains("test result: ok"),
        "expected the smoke test to report success"
    );
}

// ── autumn generate webhook integration tests (issue #1366) ───────────────────

#[test]
fn generate_webhook_creates_all_expected_files() {
    let (_tmp, project) = fresh_project("webhook-app");
    let (stdout, stderr) = run_autumn(&project, &["generate", "webhook", "stripe", "Payments"]);
    assert!(
        stdout.contains("Created") && stdout.contains("payments.rs"),
        "output should list the created handler: {stdout}"
    );

    assert!(project.join("src/webhooks/payments.rs").is_file());
    assert!(project.join("src/webhooks/mod.rs").is_file());

    let handler = fs::read_to_string(project.join("src/webhooks/payments.rs")).unwrap();
    assert!(
        handler.contains("#[post(\"/webhooks/stripe\")]"),
        "handler must own the provider route path:\n{handler}"
    );
    assert!(
        handler.contains("webhook: SignedWebhook"),
        "handler must take the shipped extractor:\n{handler}"
    );
    assert!(
        handler.contains("webhook.event_type()")
            && handler.contains("\"payment_intent.succeeded\""),
        "handler must dispatch on the event type:\n{handler}"
    );

    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main_rs.contains("mod webhooks;"), "got:\n{main_rs}");
    assert!(
        main_rs.contains("webhooks::payments::payments_webhook"),
        "the route must be registered in routes![...]:\n{main_rs}"
    );

    let autumn_toml = fs::read_to_string(project.join("autumn.toml")).unwrap();
    assert!(
        autumn_toml.contains("[[security.webhooks.endpoints]]"),
        "got:\n{autumn_toml}"
    );
    assert!(
        autumn_toml.contains("secret_env = \"STRIPE_WEBHOOK_SECRET\""),
        "the endpoint must reference a secret env var, never an inline secret:\n{autumn_toml}"
    );
    assert!(
        autumn_toml.contains("replay_protection = true"),
        "replay protection must be on by default:\n{autumn_toml}"
    );
    // No CSRF/CAPTCHA exemption copies: the framework derives those from the
    // endpoint block on every boot, so a literal copy would only go stale.
    assert!(
        !autumn_toml.contains("exempt_paths"),
        "path exemptions are derived from the endpoint block, not copied:\n{autumn_toml}"
    );

    // The printed next steps name the secret env var, the dashboard target, and
    // how to fire a test delivery — on stdout, not as warnings.
    assert!(
        stdout.contains("Next steps:") && stdout.contains("STRIPE_WEBHOOK_SECRET"),
        "the secret env var must be part of the printed next steps:\n{stdout}"
    );
    assert!(
        stdout.contains("autumn webhook sim stripe"),
        "the next steps should show how to fire a signed test delivery:\n{stdout}"
    );
    assert!(
        !stderr.contains("Warning:"),
        "a clean run must not print warnings:\n{stderr}"
    );
}

#[test]
fn generate_webhook_supports_every_provider_preset() {
    let (_tmp, project) = fresh_project("webhook-presets");
    for (provider, name, snake) in [
        ("stripe", "Payments", "payments"),
        ("github", "Repo", "repo"),
        ("slack", "Events", "events"),
        ("generic", "Partner", "partner"),
    ] {
        run_autumn(&project, &["generate", "webhook", provider, name]);
        let handler = fs::read_to_string(project.join(format!("src/webhooks/{snake}.rs"))).unwrap();
        assert!(
            handler.contains(&format!("#[post(\"/webhooks/{provider}\")]")),
            "{provider}: wrong route path:\n{handler}"
        );
    }
    let autumn_toml = fs::read_to_string(project.join("autumn.toml")).unwrap();
    assert_eq!(
        autumn_toml
            .matches("[[security.webhooks.endpoints]]")
            .count(),
        4,
        "each preset must add its own endpoint:\n{autumn_toml}"
    );
}

#[test]
fn generate_webhook_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("webhook-dry-run");
    let toml_before = fs::read_to_string(project.join("autumn.toml")).unwrap();
    let (stdout, _stderr) = run_autumn(
        &project,
        &["generate", "webhook", "stripe", "Payments", "--dry-run"],
    );

    assert!(stdout.contains("Dry run"), "got:\n{stdout}");
    assert!(stdout.contains("Would create"), "got:\n{stdout}");
    assert!(
        !project.join("src/webhooks").exists(),
        "--dry-run must not write any file"
    );
    assert_eq!(
        fs::read_to_string(project.join("autumn.toml")).unwrap(),
        toml_before,
        "--dry-run must not touch autumn.toml"
    );
}

#[test]
fn generate_webhook_rejects_an_unknown_provider() {
    let (_tmp, project) = fresh_project("webhook-bad-provider");
    let (_stdout, stderr, code) =
        run_autumn_failing(&project, &["generate", "webhook", "twilio", "Sms"]);
    assert_eq!(code, Some(1), "got:\n{stderr}");
    assert!(
        stderr.contains("twilio") && stderr.contains("generic"),
        "got:\n{stderr}"
    );
    assert!(!project.join("src/webhooks").exists());
}

#[test]
fn generate_webhook_rejects_hostile_path_and_secret_env_overrides() {
    let (_tmp, project) = fresh_project("webhook-hostile-input");
    let toml_before = fs::read_to_string(project.join("autumn.toml")).unwrap();

    // A quote would break out of the generated `#[post("…")]` attribute.
    let (_stdout, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "webhook",
            "stripe",
            "Payments",
            "--path",
            "/a\")]pub fn evil(){}//",
        ],
    );
    assert_eq!(code, Some(1), "got:\n{stderr}");

    // A newline in --secret-env used to smuggle a whole endpoint block, with a
    // plaintext secret and replay protection off, into autumn.toml.
    let (_stdout, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "webhook",
            "stripe",
            "Payments",
            "--secret-env",
            "X\n\n[[security.webhooks.endpoints]]\nname = \"evil\"\npath = \"/evil\"\nprovider = \"generic\"\nsecret = \"attacker-known\"\nreplay_protection = false\n# ",
        ],
    );
    assert_eq!(code, Some(1), "got:\n{stderr}");
    assert!(
        stderr.contains("secret environment variable"),
        "got:\n{stderr}"
    );

    assert!(!project.join("src/webhooks").exists());
    assert_eq!(
        fs::read_to_string(project.join("autumn.toml")).unwrap(),
        toml_before,
        "a rejected invocation must not touch autumn.toml"
    );
}

#[test]
fn generate_webhook_rejects_a_second_endpoint_on_the_same_path() {
    let (_tmp, project) = fresh_project("webhook-dup-path");
    run_autumn(&project, &["generate", "webhook", "stripe", "Payments"]);
    let (_stdout, stderr, code) =
        run_autumn_failing(&project, &["generate", "webhook", "stripe", "Billing"]);
    assert_eq!(code, Some(1), "got:\n{stderr}");
    assert!(
        stderr.contains("/webhooks/stripe") && stderr.contains("--path"),
        "the duplicate-path error must suggest --path:\n{stderr}"
    );

    // …and the override succeeds.
    run_autumn(
        &project,
        &[
            "generate",
            "webhook",
            "stripe",
            "Billing",
            "--path",
            "/webhooks/stripe-billing",
        ],
    );
    let handler = fs::read_to_string(project.join("src/webhooks/billing.rs")).unwrap();
    assert!(
        handler.contains("#[post(\"/webhooks/stripe-billing\")]"),
        "got:\n{handler}"
    );
}

#[test]
fn destroy_webhook_removes_the_generated_files_and_config() {
    let (_tmp, project) = fresh_project("webhook-destroy");
    let toml_before = fs::read_to_string(project.join("autumn.toml")).unwrap();
    let main_before = fs::read_to_string(project.join("src/main.rs")).unwrap();

    run_autumn(&project, &["generate", "webhook", "stripe", "Payments"]);
    run_autumn(&project, &["destroy", "webhook", "stripe", "Payments"]);

    assert!(
        !project.join("src/webhooks").exists(),
        "the handler module must be gone"
    );
    assert_eq!(
        fs::read_to_string(project.join("autumn.toml")).unwrap(),
        toml_before,
        "autumn.toml must be restored exactly"
    );
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before,
        "src/main.rs must be restored exactly"
    );
}

/// Slow end-to-end check: scaffold a fresh project, generate every provider
/// preset, and `cargo check --tests` it — the acceptance-criterion proof that
/// generated webhook code compiles with no hand-editing.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_webhook_cargo_checks() {
    let (_tmp, project) = fresh_project("webhook-build");
    patch_generated_cargo_toml(&project);

    for (provider, name) in [
        ("stripe", "Payments"),
        ("github", "Repo"),
        ("slack", "Events"),
        ("generic", "Partner"),
    ] {
        run_autumn(&project, &["generate", "webhook", provider, name]);
    }

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated webhooks failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check: actually RUN the generated webhook tests. This is the
/// acceptance-criterion proof that a valid signature is accepted, a
/// missing/invalid signature is rejected, and a replayed delivery is rejected —
/// on first run, with no manual edits beyond the ones the issue allows.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: builds and runs a fresh project's test suite — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_webhook_tests_pass() {
    let (_tmp, project) = fresh_project("webhook-smoke");
    patch_generated_cargo_toml(&project);

    for (provider, name) in [
        ("stripe", "Payments"),
        ("github", "Repo"),
        ("slack", "Events"),
        ("generic", "Partner"),
    ] {
        run_autumn(&project, &["generate", "webhook", provider, name]);
    }

    let test_run = Command::new("cargo")
        .args(["test", "webhooks::"])
        .current_dir(&project)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&test_run.stdout);
    assert!(
        test_run.status.success(),
        "generated webhook tests failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&test_run.stderr),
    );
    assert!(
        stdout.contains("16 passed"),
        "expected all four presets' four cases to pass; got:\n{stdout}"
    );
}

// ── autumn generate auth integration tests ────────────────────────────────────

#[allow(clippy::too_many_lines)]
#[test]
fn generate_auth_in_fresh_project_creates_expected_files() {
    let (_tmp, project) = fresh_project("auth-app");
    run_autumn(&project, &["generate", "auth", "User"]);

    // Migration directory exists with up.sql and down.sql.
    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
        .collect();
    assert_eq!(migrations.len(), 1, "expected one create_users migration");
    let mig_dir = migrations[0].path();
    let up = fs::read_to_string(mig_dir.join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE TABLE users"),
        "up.sql missing CREATE TABLE"
    );
    assert!(up.contains("email"), "up.sql missing email column");
    assert!(
        up.contains("time_zone TEXT NULL"),
        "up.sql missing time_zone column"
    );
    assert!(
        up.contains("password_digest"),
        "up.sql missing password_digest"
    );
    assert!(
        up.contains("reset_token_digest"),
        "up.sql missing reset_token_digest"
    );
    assert!(up.contains("UNIQUE"), "email must be UNIQUE");
    let down = fs::read_to_string(mig_dir.join("down.sql")).unwrap();
    assert!(
        down.contains("DROP TABLE users"),
        "down.sql missing DROP TABLE"
    );

    // Model file
    let model = fs::read_to_string(project.join("src/models/user.rs")).unwrap();
    assert!(model.contains("pub struct User"), "model missing struct");
    assert!(
        model.contains("pub email: String"),
        "model missing email field"
    );
    assert!(
        model.contains("pub time_zone: Option<String>"),
        "model missing time_zone field"
    );
    assert!(
        model.contains("pub password_digest: String"),
        "model missing password_digest"
    );
    assert!(
        !model.contains("pub password:"),
        "raw password must not be stored"
    );

    // mod.rs declares user module
    let mod_rs = fs::read_to_string(project.join("src/models/mod.rs")).unwrap();
    assert!(
        mod_rs.contains("pub mod user;"),
        "models/mod.rs missing pub mod user"
    );

    // schema.rs entry
    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(
        schema.contains("users (id)"),
        "schema.rs missing users table block"
    );
    assert!(
        schema.contains("email -> Text"),
        "schema.rs missing email column"
    );
    assert!(
        schema.contains("time_zone -> Nullable<Text>"),
        "schema.rs missing time_zone column"
    );
    assert!(
        schema.contains("reset_token_digest -> Nullable<Text>"),
        "schema.rs missing nullable reset_token_digest"
    );

    // Routes file
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    for handler in [
        "pub async fn signup_form",
        "pub async fn signup",
        "pub async fn login_form",
        "pub async fn login",
        "pub async fn logout",
        "pub async fn account",
        "pub async fn forgot_password_form",
        "pub async fn forgot_password",
        "pub async fn reset_password_form",
        "pub async fn reset_password",
    ] {
        assert!(
            routes.contains(handler),
            "routes/auth.rs missing: {handler}"
        );
    }
    assert!(
        routes.contains("#[secured]"),
        "account route must be protected"
    );
    assert!(
        routes.contains("session.destroy"),
        "logout must destroy session"
    );
    assert!(
        routes.contains("session.rotate_id"),
        "login must rotate session id"
    );
    assert!(
        routes.contains("State(state): State<AppState>"),
        "auth routes must receive AppState so sessions use the configured auth key"
    );
    assert!(
        routes.contains("session.insert(state.auth_session_key()"),
        "auth routes must populate the configured auth session key"
    );
    assert_eq!(
        routes.matches("session.insert(\"user_id\"").count(),
        4,
        "User auth routes should only write user_id as the generated account id key \
         (login, reset_password, confirm_email, and the remember-me restore path \
         establish_remember_login, which must set the same identity keys as a fresh login)"
    );
    assert!(
        routes.contains("email.split_once('@')"),
        "signup email validation should use split_once"
    );
    assert!(
        !routes.contains("email.find('@').unwrap()"),
        "signup email validation should not search for @ repeatedly"
    );
    // Issue #1353: auth views render through the app's shared `crate::layout`
    // (nav/header/footer shell) rather than a private bare-DOCTYPE `fn layout`
    // stub, and pending flashes are threaded to the layout's 3rd argument via
    // the accessible `flash_messages()` helper (not the old `flash.render()`).
    assert!(
        !routes.contains("fn layout(title: &str, content: Markup)"),
        "the private 2-arg layout stub must be removed"
    );
    assert!(
        routes.contains("crate::layout("),
        "auth views must render through crate::layout"
    );
    assert!(
        routes.contains(
            "crate::layout(\"Log In\", \"/login\", flash_messages(&flash.consume().await),"
        ),
        "login must render through crate::layout with /login + flash"
    );
    assert!(
        !routes.contains("flash.render().await"),
        "auth views must not use the old in-content flash.render() path"
    );

    // routes/mod.rs
    let route_mod = fs::read_to_string(project.join("src/routes/mod.rs")).unwrap();
    assert!(
        route_mod.contains("pub mod auth;"),
        "routes/mod.rs missing pub mod auth"
    );

    // Generated tests file
    let tests = fs::read_to_string(project.join("tests/auth.rs")).unwrap();
    for flow in [
        "auth_signup_returns_200",
        "auth_login_returns_200",
        "auth_logout_redirects",
        "auth_forgot_password_returns_200",
        "auth_reset_password_returns_200",
        "auth_account_rejects_anonymous",
    ] {
        assert!(tests.contains(flow), "tests/auth.rs missing: {flow}");
    }

    // Documentation
    assert!(
        project.join("docs/guide/authentication.md").exists(),
        "docs/guide/authentication.md must be created"
    );

    // main.rs registers auth routes
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    for entry in [
        "routes::auth::signup_form",
        "routes::auth::login_form",
        "routes::auth::logout",
        "routes::auth::account",
        "routes::auth::forgot_password_form",
        "routes::auth::reset_password_form",
    ] {
        assert!(main.contains(entry), "main.rs missing route: {entry}");
    }
}

#[test]
fn generate_auth_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("auth-dry-app");
    let (stdout, _) = run_autumn(&project, &["generate", "auth", "User", "--dry-run"]);
    assert!(
        stdout.contains("Dry run"),
        "expected dry-run output; got: {stdout}"
    );
    assert!(
        !project.join("src/models/user.rs").exists(),
        "dry run must not create model file"
    );
    assert!(
        !project.join("src/routes/auth.rs").exists(),
        "dry run must not create routes file"
    );
    assert!(
        !project.join("tests/auth.rs").exists(),
        "dry run must not create tests file"
    );
}

#[test]
fn generate_auth_collision_without_force_fails() {
    let (_tmp, project) = fresh_project("auth-collide-app");
    run_autumn(&project, &["generate", "auth", "User"]);
    // Re-run without --force should fail with collision error.
    let (_, stderr, code) = run_autumn_failing(&project, &["generate", "auth", "User"]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("would overwrite"),
        "expected collision message; got stderr: {stderr}"
    );
}

#[test]
fn generate_auth_force_overwrites_existing_files() {
    let (_tmp, project) = fresh_project("auth-force-app");
    run_autumn(&project, &["generate", "auth", "User"]);
    let model_path = project.join("src/models/user.rs");
    let original = fs::read_to_string(&model_path).unwrap();
    fs::write(&model_path, "// touched").unwrap();
    run_autumn(&project, &["generate", "auth", "User", "--force"]);
    let regenerated = fs::read_to_string(&model_path).unwrap();
    assert_eq!(
        regenerated, original,
        "--force must restore original content"
    );
}

#[test]
fn generate_auth_help_documents_command() {
    let tmp = tempfile::tempdir().unwrap();
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["generate", "auth", "--help"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--dry-run"),
        "help should mention --dry-run"
    );
    assert!(stdout.contains("--force"), "help should mention --force");
    assert!(stdout.contains("--totp"), "help should mention --totp");
}

// ── autumn generate auth --totp (issue #799) ──────────────────────────────────

#[allow(clippy::too_many_lines)]
#[test]
fn generate_auth_totp_creates_expected_files() {
    let (_tmp, project) = fresh_project("auth-totp-app");
    run_autumn(&project, &["generate", "auth", "User", "--totp"]);

    // Migration: TOTP columns on users + recovery_codes table.
    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
        .collect();
    assert_eq!(migrations.len(), 1, "expected one create_users migration");
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("totp_secret_encrypted"),
        "up.sql missing totp_secret_encrypted"
    );
    assert!(up.contains("totp_enabled"), "up.sql missing totp_enabled");
    assert!(
        up.contains("CREATE TABLE recovery_codes"),
        "up.sql missing recovery_codes table"
    );
    assert!(up.contains("code_digest"), "up.sql missing code_digest");
    assert!(up.contains("used_at"), "up.sql missing used_at");
    let down = fs::read_to_string(migrations[0].path().join("down.sql")).unwrap();
    assert!(
        down.contains("DROP TABLE recovery_codes"),
        "down.sql must drop recovery_codes"
    );

    // Model gains TOTP fields; recovery_code model exists.
    let model = fs::read_to_string(project.join("src/models/user.rs")).unwrap();
    assert!(model.contains("pub totp_secret_encrypted: Option<String>"));
    assert!(model.contains("pub totp_enabled: bool"));
    assert!(
        project.join("src/models/recovery_code.rs").exists(),
        "recovery_code model missing"
    );
    let mod_rs = fs::read_to_string(project.join("src/models/mod.rs")).unwrap();
    assert!(
        mod_rs.contains("pub mod recovery_code;"),
        "models/mod.rs missing recovery_code"
    );

    // schema.rs: totp columns + recovery_codes table.
    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(schema.contains("totp_secret_encrypted -> Nullable<Text>"));
    assert!(schema.contains("totp_enabled -> Bool"));
    assert!(schema.contains("recovery_codes (id)"));

    // Routes: 2FA handlers + paths.
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    for needle in [
        "pub async fn two_factor_status",
        "pub async fn two_factor_enable",
        "pub async fn two_factor_confirm",
        "pub async fn two_factor_disable",
        "pub async fn login_verify",
        "otpauth://",
        "Aes256Gcm",
        "totp_pending",
    ] {
        assert!(routes.contains(needle), "routes/auth.rs missing: {needle}");
    }

    // Generated 2FA integration tests cover the full round trip.
    let tests = fs::read_to_string(project.join("tests/auth_2fa.rs")).unwrap();
    for flow in [
        "two_factor_enroll_and_confirm",
        "login_with_totp_code",
        "login_with_recovery_code",
        "recovery_code_reuse_rejected",
        "two_factor_disable",
    ] {
        assert!(tests.contains(flow), "tests/auth_2fa.rs missing: {flow}");
    }

    // Cargo deps + docs.
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("totp-rs ="), "Cargo.toml missing totp-rs");
    assert!(cargo.contains("aes-gcm ="), "Cargo.toml missing aes-gcm");
    let docs = fs::read_to_string(project.join("docs/guide/authentication.md")).unwrap();
    assert!(
        docs.contains("Two-Factor Authentication"),
        "docs missing 2FA section"
    );

    // main.rs registers the new routes.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    for entry in [
        "routes::auth::two_factor_enable",
        "routes::auth::login_verify",
    ] {
        assert!(main.contains(entry), "main.rs missing route: {entry}");
    }
}

#[test]
fn generate_auth_without_totp_has_no_totp_artifacts() {
    let (_tmp, project) = fresh_project("auth-no-totp-app");
    run_autumn(&project, &["generate", "auth", "User"]);
    let model = fs::read_to_string(project.join("src/models/user.rs")).unwrap();
    assert!(
        !model.contains("totp_enabled"),
        "default auth must not include totp fields"
    );
    assert!(!project.join("src/models/recovery_code.rs").exists());
    assert!(!project.join("tests/auth_2fa.rs").exists());
}

#[test]
fn generate_auth_passkeys_creates_expected_files() {
    let (_tmp, project) = fresh_project("auth-passkeys-app");
    run_autumn(&project, &["generate", "auth", "User", "--passkeys"]);

    // Migration: Webauthn credentials table exists.
    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_create_webauthn_credentials")
        })
        .collect();
    assert_eq!(
        migrations.len(),
        1,
        "expected one create_webauthn_credentials migration"
    );
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE TABLE webauthn_credentials"),
        "up.sql missing webauthn_credentials table"
    );

    // Model: webauthn_credential model exists.
    assert!(
        project.join("src/models/webauthn_credential.rs").exists(),
        "webauthn_credential model missing"
    );

    // Routes: passkey routes.
    let routes = fs::read_to_string(project.join("src/routes/passkeys.rs")).unwrap();
    for needle in [
        "pub async fn passkey_register_page",
        "pub async fn passkey_login_page",
        "let script_nonce = nonce.map(|n| n.value().to_owned());",
    ] {
        assert!(
            routes.contains(needle),
            "routes/passkeys.rs missing or incorrect: {needle}"
        );
    }

    // Ensure it does not contain the old private field access.
    assert!(
        !routes.contains("nonce.map(|n| n.0.clone())"),
        "routes/passkeys.rs must not access private field n.0"
    );
}

#[test]
fn generate_auth_without_passkeys_has_no_passkeys_artifacts() {
    let (_tmp, project) = fresh_project("auth-no-passkeys-app");
    run_autumn(&project, &["generate", "auth", "User"]);
    assert!(!project.join("src/models/webauthn_credential.rs").exists());
    assert!(!project.join("src/routes/passkeys.rs").exists());
}

#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_auth_passkeys_cargo_checks() {
    let (_tmp, project) = fresh_project("auth-passkeys-build");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "auth", "User", "--passkeys"]);

    let cargo_after = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    for dep in ["webauthn-rs", "uuid", "serde", "diesel", "maud", "chrono"] {
        assert!(
            cargo_after.contains(&format!("{dep} =")),
            "Cargo.toml missing '{dep}' after `generate auth --passkeys`"
        );
    }

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated --passkeys auth failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

// ── autumn generate auth --magic-link (issue #1328) ───────────────────────────

#[test]
fn generate_auth_magic_link_creates_expected_files() {
    let (_tmp, project) = fresh_project("auth-magic-link-app");
    run_autumn(&project, &["generate", "auth", "User", "--magic-link"]);

    // Migration: dedicated magic_link_tokens table with single-use marker.
    let up = fs::read_to_string(
        fs::read_dir(project.join("migrations"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
            .unwrap()
            .path()
            .join("up.sql"),
    )
    .unwrap();
    assert!(up.contains("CREATE TABLE magic_link_tokens"));
    assert!(up.contains("token_digest TEXT NOT NULL UNIQUE"));
    assert!(up.contains("consumed_at TIMESTAMP NULL"));

    // Model file + module declaration.
    assert!(project.join("src/models/magic_link_token.rs").exists());
    let mod_rs = fs::read_to_string(project.join("src/models/mod.rs")).unwrap();
    assert!(mod_rs.contains("pub mod magic_link_token;"));

    // Routes: request/email/verify handlers + throttle.
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    for needle in [
        "#[get(\"/login/magic\")]",
        "#[post(\"/login/magic\")]",
        "#[get(\"/login/magic/verify\")]",
        "#[throttle(",
        "pub async fn magic_link_verify",
        "async fn send_magic_link_email",
    ] {
        assert!(routes.contains(needle), "routes/auth.rs missing: {needle}");
    }

    // Routes registered in main.rs.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("routes::auth::magic_link_verify"));

    // Docs document the flow.
    let docs = fs::read_to_string(project.join("docs/guide/authentication.md")).unwrap();
    assert!(docs.contains("Passwordless Magic-Link Login"));
}

#[test]
fn generate_auth_without_magic_link_emits_no_magic_link_artifacts() {
    let (_tmp, project) = fresh_project("auth-no-magic-link-app");
    run_autumn(&project, &["generate", "auth", "User"]);
    assert!(!project.join("src/models/magic_link_token.rs").exists());
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(!routes.contains("/login/magic"));
}

/// Slow: scaffold `generate auth --magic-link` and `cargo check --tests` the
/// result against the local `autumn-web` crate, proving the generated
/// magic-link app and its test suite type-check with zero edits (issue #1328).
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_auth_magic_link_cargo_checks() {
    let (_tmp, project) = fresh_project("auth-magic-link-build");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "auth", "User", "--magic-link"]);

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated --magic-link auth failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow: prove `--magic-link` composes with `--totp` in a single generated app
/// that still type-checks (issue #1328 composability AC1).
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_auth_magic_link_with_totp_cargo_checks() {
    let (_tmp, project) = fresh_project("auth-magic-totp-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &["generate", "auth", "User", "--totp", "--magic-link"],
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated --totp --magic-link auth failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow: scaffold `generate auth --totp` and `cargo check --tests` the result
/// against the local `autumn-web` crate, proving the generated 2FA app and its
/// test suite type-check with zero edits (issue #799 success metric).
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_auth_totp_cargo_checks() {
    let (_tmp, project) = fresh_project("auth-totp-build");
    patch_generated_cargo_toml(&project);

    run_autumn(&project, &["generate", "auth", "User", "--totp"]);

    let cargo_after = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    for dep in ["totp-rs", "aes-gcm", "base64", "diesel", "maud", "chrono"] {
        assert!(
            cargo_after.contains(&format!("{dep} =")),
            "Cargo.toml missing '{dep}' after `generate auth --totp`"
        );
    }

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated --totp auth failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

// ── TOML config (issue #669) ──────────────────────────────────────────────────

/// Scaffold a resource using only a `--config` file, no inline CLI metadata.
#[test]
fn generate_scaffold_from_config_file() {
    let (_tmp, project) = fresh_project("scaffold-config-app");

    fs::write(
        project.join("autumn.generate.toml"),
        "[scaffold.Bookmark]\n\
         fields      = [\"url:String\", \"title:String\", \"tag:String\", \"alive:bool\"]\n\
         indexes     = [\"url\", \"tag\"]\n\
         validations = [\"url=url\", \"title=length:min=1,max=200\"]\n\
         defaults    = [\"alive=true\"]\n\
         queries     = [\"find_by_tag:tag\", \"find_by_alive:alive\"]\n",
    )
    .unwrap();

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Bookmark",
            "--config",
            "autumn.generate.toml",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/bookmark.rs")).unwrap();
    assert!(
        model.contains("#[indexed]\n    #[validate(url)]\n    pub url: String,"),
        "model missing indexed+validated url field:\n{model}"
    );
    assert!(
        model.contains("#[validate(length(min = 1, max = 200))]\n    pub title: String,"),
        "model missing length-validated title field:\n{model}"
    );
    assert!(
        model.contains("#[indexed]\n    pub tag: String,"),
        "model missing indexed tag field:\n{model}"
    );
    assert!(
        model.contains("#[default]\n    pub alive: bool,"),
        "model missing defaulted alive field:\n{model}"
    );

    let repo = fs::read_to_string(project.join("src/repositories/bookmark.rs")).unwrap();
    assert!(
        repo.contains("fn find_by_tag(tag: String) -> Vec<Bookmark>;"),
        "repo missing find_by_tag query:\n{repo}"
    );
    assert!(
        repo.contains("fn find_by_alive(alive: bool) -> Vec<Bookmark>;"),
        "repo missing find_by_alive query:\n{repo}"
    );

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_create_bookmarks")
        })
        .expect("create_bookmarks migration must exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(
        up.contains("alive BOOLEAN NOT NULL DEFAULT TRUE"),
        "SQL missing default: {up}"
    );
    assert!(
        up.contains("CREATE INDEX idx_bookmarks_url ON bookmarks (url);"),
        "SQL missing url index: {up}"
    );
    assert!(
        up.contains("CREATE INDEX idx_bookmarks_tag ON bookmarks (tag);"),
        "SQL missing tag index: {up}"
    );

    let cargo_toml = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo_toml.contains("validator ="),
        "Cargo.toml missing validator dep:\n{cargo_toml}"
    );
}

/// CLI flags override the corresponding TOML values when both are present.
#[test]
fn generate_scaffold_cli_overrides_toml_config() {
    let (_tmp, project) = fresh_project("scaffold-config-override-app");

    fs::write(
        project.join("autumn.generate.toml"),
        "[scaffold.Post]\nfields  = [\"title:String\", \"body:Text\"]\nindexes = [\"title\"]\n",
    )
    .unwrap();

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "content:String",
            "--index",
            "content",
            "--config",
            "autumn.generate.toml",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("pub content: String"),
        "model must have CLI field 'content': {model}"
    );
    assert!(
        !model.contains("pub title: String"),
        "model must not have TOML field 'title': {model}"
    );
    assert!(
        !model.contains("pub body:"),
        "model must not have TOML field 'body': {model}"
    );

    let migration = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with("_create_posts"))
        .expect("create_posts migration must exist");
    let up = fs::read_to_string(migration.path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE INDEX idx_posts_content ON posts (content);"),
        "SQL must have CLI index on 'content': {up}"
    );
    assert!(
        !up.contains("idx_posts_title"),
        "SQL must not have TOML index on 'title': {up}"
    );
}

/// A non-existent config file must cause a non-zero exit with the filename
/// mentioned in the error output.
#[test]
fn generate_scaffold_rejects_missing_config_file() {
    let (_tmp, project) = fresh_project("scaffold-missing-config-app");

    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--config",
            "nonexistent.toml",
        ],
    );

    assert_eq!(code, Some(1), "expected exit code 1; got {code:?}");
    assert!(
        stderr.contains("nonexistent.toml"),
        "error must mention the missing file name; got:\n{stderr}"
    );
}

/// Explicit `--config` with no matching `[scaffold.X]` section but WITH CLI
/// fields succeeds: the field list comes from the CLI and the config is only
/// consulted for project defaults.
#[test]
fn generate_scaffold_missing_resource_section_uses_defaults() {
    let (_tmp, project) = fresh_project("scaffold-missing-section-app");

    fs::write(
        project.join("autumn.generate.toml"),
        "[scaffold.OtherResource]\nfields = [\"name:String\"]\n",
    )
    .unwrap();

    // No [scaffold.Post] section, but fields supplied on the CLI → succeed.
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--config",
            "autumn.generate.toml",
        ],
    );

    // Default id type is BigSerial when no [generate] id is set.
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("pub id: i64,"),
        "missing-section scaffold should default to i64 PK; got:\n{model}"
    );
}

/// Typo protection (Codex P2): explicit `--config` with no matching
/// `[scaffold.X]` section, the file DOES define other scaffold resources, and
/// NO CLI fields were given → the command must error rather than silently
/// generate an empty resource.
#[test]
fn generate_scaffold_explicit_config_missing_section_errors() {
    let (_tmp, project) = fresh_project("scaffold-typo-section-app");

    fs::write(
        project.join("autumn.generate.toml"),
        "[scaffold.OtherResource]\nfields = [\"name:String\"]\n",
    )
    .unwrap();

    // Misspelled/missing [scaffold.Post] + no CLI fields → likely a typo.
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "--config",
            "autumn.generate.toml",
        ],
    );
    assert_eq!(
        code,
        Some(1),
        "missing section with no CLI fields must fail"
    );
    assert!(
        stderr.contains("no [scaffold.Post] section found"),
        "error must name the missing section; got:\n{stderr}"
    );
    assert!(
        !project.join("src/models/post.rs").exists(),
        "errored scaffold must not write a model file"
    );
}

/// Slow compile-check: scaffold a fresh project from a TOML config file and
/// verify that `cargo check --tests` succeeds against the local `autumn-web`
/// crate.  Ensures that the config-driven generator adds every dependency its
/// emitted code needs (validator, maud, etc.) and that all generated files
/// type-check without manual edits.
///
/// Ignored by default; run with:
/// `cargo test -p autumn-cli --test generate generated_scaffold_config_cargo_checks -- --ignored --exact`
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_scaffold_config_cargo_checks() {
    let (_tmp, project) = fresh_project("scaffold-config-build");

    patch_generated_cargo_toml(&project);

    fs::write(
        project.join("autumn.generate.toml"),
        "[scaffold.Bookmark]\n\
         fields      = [\"url:String\", \"title:String\", \"tag:String\", \"alive:bool\"]\n\
         indexes     = [\"url\", \"tag\"]\n\
         validations = [\"url=url\", \"title=length:min=1,max=200\"]\n\
         defaults    = [\"alive=true\"]\n\
         queries     = [\"find_by_tag:tag\", \"find_by_alive:alive\"]\n",
    )
    .unwrap();

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Bookmark",
            "--config",
            "autumn.generate.toml",
        ],
    );

    let cargo_toml = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    for dep in [
        "chrono",
        "diesel",
        "diesel-async",
        "maud",
        "serde",
        "serde_urlencoded",
        "url",
        "validator",
    ] {
        assert!(
            cargo_toml.contains(&format!("{dep} =")),
            "Cargo.toml missing '{dep}' after config-driven scaffold"
        );
    }

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on config-driven scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

// ── Pagination scaffold tests (issue #681) ──────────────────────────────────

#[test]
fn generate_scaffold_index_uses_paginated_repo_method() {
    let (_tmp, project) = fresh_project("scaffold-paginated-app");
    run_autumn(
        &project,
        &["generate", "scaffold", "Post", "title:String", "body:Text"],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    assert!(
        routes.contains("PageRequest") || routes.contains("page_req"),
        "scaffold index must use PageRequest for pagination: {routes}"
    );
    assert!(
        routes.contains("pagination_nav") || routes.contains("pagination"),
        "scaffold index must render a pagination nav partial: {routes}"
    );
    // Since #1126 the default (non-live) index calls the paginated, sort/filter
    // `list(&ListQuery, &PageRequest)` method instead of the bare `page()`;
    // either paginated repository method satisfies issue #681 (both take a
    // `PageRequest` and return a `Page`, never loading every row).
    assert!(
        routes.contains(".page(") || routes.contains(".list("),
        "scaffold index must call a paginated repository method (page()/list()): {routes}"
    );
    // Scoped to the `index` handler body: since issue #1312 the module also
    // emits a `bulk_delete` handler whose SELECT is bounded by the submitted id
    // list (`WHERE id = ANY($1)`), which is not an unpaginated index load.
    let index = routes
        .split_once("pub async fn index(")
        .expect("scaffold must emit an index handler")
        .1;
    let index = index.split("pub async fn ").next().unwrap_or(index);
    assert!(
        !index.contains(".load(&mut *db)"),
        "scaffold index must not load every row without pagination: {index}"
    );
    // The repository trait must be imported so `repo.list()`/`repo.page()` (trait
    // methods) resolve at compile time — without it the generated code fails with E0599.
    assert!(
        routes.contains("PostRepository"),
        "scaffold routes must import the PostRepository trait (needed to call repo.list()): {routes}"
    );
}

#[test]
fn generate_scaffold_index_uses_page_request_extractor() {
    let (_tmp, project) = fresh_project("scaffold-paginated-extractor-app");
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    // PageRequest extractor handles all clamping — no manual HashMap parsing.
    assert!(
        routes.contains("page_req: PageRequest") || routes.contains("PageRequest,"),
        "scaffold index must use the PageRequest extractor: {routes}"
    );
    assert!(
        !routes.contains("HashMap"),
        "scaffold index must not manually parse query params via HashMap: {routes}"
    );
}

#[test]
fn generate_scaffold_repository_exposes_page_method() {
    let (_tmp, project) = fresh_project("scaffold-repo-page-app");
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);

    let repo = fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();
    // The page() and cursor_page() methods are code-generated by the
    // #[autumn_web::repository] macro — they are not written out in the source
    // file.  Asserting the macro attribute is present is the correct contract:
    // the macro tests already verify it generates page() + cursor_page().
    assert!(
        repo.contains("#[autumn_web::repository("),
        "scaffold repository must use #[autumn_web::repository] (which generates page()): {repo}"
    );
    // Sanity-check that the trait is declared (confirms the scaffold structure).
    assert!(
        repo.contains("pub trait PostRepository"),
        "scaffold repository must declare a public PostRepository trait: {repo}"
    );
}

// ── autumn generate mailer ────────────────────────────────────────────────────

#[test]
fn generate_mailer_creates_all_expected_files() {
    let (_tmp, project) = fresh_project("mailer-app");
    let (stdout, _stderr) = run_autumn(&project, &["generate", "mailer", "Welcome"]);

    assert!(
        stdout.contains("welcome.rs") || stdout.contains("Created"),
        "output should mention created files: {stdout}"
    );

    // Mailer source file — production code only, no preview.
    assert!(project.join("src/mailers/welcome.rs").is_file());
    let mailer = fs::read_to_string(project.join("src/mailers/welcome.rs")).unwrap();
    assert!(mailer.contains("pub struct WelcomeMailer"));
    assert!(mailer.contains("#[mailer]"));
    assert!(
        !mailer.contains("#[mailer_preview]"),
        "#[mailer_preview] must live in previews/, not the mailer file"
    );
    assert!(mailer.contains("pub fn welcome("));
    assert!(mailer.contains("deliver_later"));

    // Shared layout files (created on first generate mailer).
    assert!(project.join("templates/mailers/_layout.html").is_file());
    let layout_html = fs::read_to_string(project.join("templates/mailers/_layout.html")).unwrap();
    assert!(
        layout_html.contains("<!DOCTYPE html>"),
        "_layout.html must be a full document shell"
    );
    assert!(
        layout_html.contains("<table"),
        "_layout.html must contain a table-based wrapper"
    );
    assert!(
        layout_html.contains("style="),
        "_layout.html must use inline styles"
    );
    assert!(
        layout_html.contains("{{ content }}"),
        "_layout.html must contain the content slot"
    );
    assert!(project.join("templates/mailers/_layout.txt").is_file());
    let layout_txt = fs::read_to_string(project.join("templates/mailers/_layout.txt")).unwrap();
    assert!(
        layout_txt.contains("{{ content }}"),
        "_layout.txt must contain the content slot"
    );

    // Per-mailer HTML + text templates — body fragment only, no document shell.
    assert!(project.join("templates/mailers/welcome.html").is_file());
    let html = fs::read_to_string(project.join("templates/mailers/welcome.html")).unwrap();
    assert!(html.contains("WelcomeMailer"));
    assert!(
        !html.contains("<!DOCTYPE"),
        "per-mailer template must be a body fragment, not a full document"
    );
    assert!(project.join("templates/mailers/welcome.txt").is_file());
    let txt = fs::read_to_string(project.join("templates/mailers/welcome.txt")).unwrap();
    assert!(txt.contains("WelcomeMailer"));

    // Module index declares both the mailer and the previews sub-module.
    assert!(project.join("src/mailers/mod.rs").is_file());
    let mod_rs = fs::read_to_string(project.join("src/mailers/mod.rs")).unwrap();
    assert!(mod_rs.contains("pub mod welcome;"));
    assert!(mod_rs.contains("pub mod previews;"));

    // Smoke test is inline in the mailer file.
    assert!(!project.join("tests/welcome_mailer.rs").exists());
    assert!(mailer.contains("mod welcome_mailer_tests"));
    assert!(mailer.contains("welcome_mailer_renders_both_bodies"));
}

#[test]
fn generate_mailer_creates_preview_files_and_wires_main() {
    let (_tmp, project) = fresh_project("mailer-preview-files-app");
    run_autumn(&project, &["generate", "mailer", "Welcome"]);

    // Separate preview file with #[mailer_preview].
    assert!(project.join("src/mailers/previews/welcome.rs").is_file());
    let preview = fs::read_to_string(project.join("src/mailers/previews/welcome.rs")).unwrap();
    assert!(preview.contains("#[mailer_preview]"));
    assert!(preview.contains("welcome_preview"));

    // Previews mod.rs.
    assert!(project.join("src/mailers/previews/mod.rs").is_file());
    let previews_mod = fs::read_to_string(project.join("src/mailers/previews/mod.rs")).unwrap();
    assert!(previews_mod.contains("pub mod welcome;"));

    // main.rs wiring.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("mod mailers;"));
    assert!(main.contains("mail_previews!["));
    assert!(main.contains("mailers::welcome::WelcomeMailer"));

    // Cargo.toml: mail feature enabled.
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"mail\""),
        "Cargo.toml must include the mail feature: {cargo}"
    );
}

#[test]
fn generate_mailer_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("mailer-dry-app");
    let (stdout, _) = run_autumn(&project, &["generate", "mailer", "Welcome", "--dry-run"]);
    assert!(
        stdout.contains("Dry run"),
        "dry run must print Dry run header: {stdout}"
    );
    assert!(
        !project.join("src/mailers/welcome.rs").exists(),
        "dry run must not create the mailer file"
    );
    assert!(
        !project.join("src/mailers/previews/welcome.rs").exists(),
        "dry run must not create the preview file"
    );
    assert!(
        !project.join("templates/mailers/welcome.html").exists(),
        "dry run must not create html template"
    );
    assert!(
        !project.join("templates/mailers/welcome.txt").exists(),
        "dry run must not create txt template"
    );
}

#[test]
fn generate_mailer_collision_without_force_fails() {
    let (_tmp, project) = fresh_project("mailer-collide-app");
    run_autumn(&project, &["generate", "mailer", "Welcome"]);
    let (_, stderr, code) = run_autumn_failing(&project, &["generate", "mailer", "Welcome"]);
    assert_eq!(code, Some(1), "second run without --force must exit 1");
    assert!(
        stderr.contains("would overwrite") || stderr.contains("welcome.rs"),
        "must report collision: {stderr}"
    );
}

#[test]
fn generate_mailer_force_overwrites_existing() {
    let (_tmp, project) = fresh_project("mailer-force-app");
    run_autumn(&project, &["generate", "mailer", "Welcome"]);
    // Corrupt the mailer file so we can detect the overwrite.
    let path = project.join("src/mailers/welcome.rs");
    fs::write(&path, "// corrupted").unwrap();
    run_autumn(&project, &["generate", "mailer", "Welcome", "--force"]);
    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("WelcomeMailer"),
        "--force must regenerate the mailer file"
    );
    assert!(
        project.join("src/mailers/previews/welcome.rs").exists(),
        "--force must also create the preview file"
    );
}

#[test]
fn generate_mailer_preview_registry_wired_into_main() {
    let (_tmp, project) = fresh_project("mailer-preview-app");
    run_autumn(&project, &["generate", "mailer", "Welcome"]);

    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();

    // The preview registry wiring must appear before `.run()`.
    let preview_pos = main
        .find("mail_previews![")
        .expect("mail_previews![] must be present in main.rs");
    let run_pos = main.find(".run()").expect(".run() must still be present");
    assert!(
        preview_pos < run_pos,
        "mail_previews![] must be wired before .run() in the builder chain"
    );
    assert!(
        main.contains("mailers::welcome::WelcomeMailer"),
        "preview registry must reference the generated mailer type"
    );
}

// ── autumn generate teams (issue #1261) ────────────────────────────────────

#[test]
fn generate_teams_emits_organization_membership_invitation_models() {
    let (_tmp, project) = fresh_project("teams-app");
    let (stdout, _stderr) = run_autumn(&project, &["generate", "teams"]);
    assert!(
        stdout.contains("Created") || stdout.contains("teams"),
        "output should mention created files: {stdout}"
    );

    // Models: Organization, Membership, Invitation.
    assert!(project.join("src/teams/models.rs").is_file());
    let models = fs::read_to_string(project.join("src/teams/models.rs")).unwrap();
    assert!(models.contains("pub struct Organization"), "{models}");
    assert!(models.contains("pub struct Membership"), "{models}");
    assert!(models.contains("pub struct Invitation"), "{models}");

    // Role enum + require_role guard.
    assert!(project.join("src/teams/role.rs").is_file());
    let role = fs::read_to_string(project.join("src/teams/role.rs")).unwrap();
    assert!(role.contains("pub enum Role"), "{role}");
    assert!(role.contains("Owner"), "{role}");
    assert!(role.contains("Admin"), "{role}");
    assert!(role.contains("Member"), "{role}");
    assert!(role.contains("pub async fn require_role"), "{role}");
    assert!(
        role.contains("pub async fn establish_org_session"),
        "{role}"
    );

    // Repositories, tenant_scoped.
    assert!(project.join("src/teams/repositories.rs").is_file());
    let repos = fs::read_to_string(project.join("src/teams/repositories.rs")).unwrap();
    assert!(repos.contains("tenant_scoped"), "{repos}");

    // InvitationMailer.
    assert!(
        project
            .join("src/teams/mailers/invitation_mailer.rs")
            .is_file()
    );
    let mailer =
        fs::read_to_string(project.join("src/teams/mailers/invitation_mailer.rs")).unwrap();
    assert!(mailer.contains("pub struct InvitationMailer"), "{mailer}");
    assert!(mailer.contains("#[mailer]"), "{mailer}");

    // Route handlers.
    assert!(project.join("src/teams/routes/organizations.rs").is_file());
    assert!(project.join("src/teams/routes/invitations.rs").is_file());
    assert!(project.join("src/teams/routes/members.rs").is_file());

    // Migration: organizations, memberships, invitations tables.
    let migrations_root = project.join("migrations");
    let teams_migration_dir = fs::read_dir(&migrations_root)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with("_create_teams"))
        .expect("a *_create_teams migration must be generated")
        .path();
    let up = fs::read_to_string(teams_migration_dir.join("up.sql")).unwrap();
    assert!(up.contains("CREATE TABLE organizations"), "{up}");
    assert!(up.contains("CREATE TABLE memberships"), "{up}");
    assert!(up.contains("CREATE TABLE invitations"), "{up}");

    // main.rs wiring.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("mod teams;"), "{main}");
    assert!(
        main.contains("teams::routes::organizations::create_organization"),
        "{main}"
    );
    assert!(
        main.contains("teams::routes::invitations::accept_invitation"),
        "{main}"
    );
    assert!(
        main.contains("teams::routes::members::list_members"),
        "{main}"
    );

    // Cargo.toml: mail feature enabled.
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"mail\""),
        "Cargo.toml must include the mail feature: {cargo}"
    );
}

#[test]
fn generate_teams_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("teams-dry-app");
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let (stdout, _) = run_autumn(&project, &["generate", "teams", "--dry-run"]);
    assert!(
        stdout.contains("Dry run"),
        "dry run must print Dry run header: {stdout}"
    );
    assert!(
        !project.join("src/teams").exists(),
        "dry run must not create the src/teams directory"
    );
    let has_teams_migration = fs::read_dir(project.join("migrations")).is_ok_and(|rd| {
        rd.filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with("_create_teams"))
    });
    assert!(
        !has_teams_migration,
        "dry run must not create a *_create_teams migration"
    );
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !main.contains("mod teams;"),
        "dry run must not touch main.rs: {main}"
    );
    let cargo_after = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert_eq!(
        cargo_after, cargo_before,
        "dry run must not touch Cargo.toml"
    );
}

#[test]
fn generate_teams_invite_accept_routes_use_invite_prefix_not_invitations() {
    let (_tmp, project) = fresh_project("teams-invite-prefix-app");
    run_autumn(&project, &["generate", "teams"]);

    let invitations = fs::read_to_string(project.join("src/teams/routes/invitations.rs")).unwrap();

    // Invitee-facing accept flow lives under its own `/invite` prefix so
    // `[tenancy] public_paths = ["/invite"]` doesn't also exempt the
    // Admin-only routes below from tenant resolution.
    assert!(
        invitations.contains(r#"#[get("/invite/{token}")]"#),
        "{invitations}"
    );
    assert!(
        invitations.contains(r#"#[post("/invite/{token}/accept")]"#),
        "{invitations}"
    );
    assert!(
        !invitations.contains(r#"#[get("/invitations/{token}")]"#),
        "{invitations}"
    );
    assert!(
        !invitations.contains(r#"#[post("/invitations/{token}/accept")]"#),
        "{invitations}"
    );

    // Admin-only create/revoke/resend stay under `/invitations`.
    assert!(
        invitations.contains(r#"#[post("/invitations")]"#),
        "{invitations}"
    );
    assert!(
        invitations.contains(r#"#[post("/invitations/{id}/revoke")]"#),
        "{invitations}"
    );
    assert!(
        invitations.contains(r#"#[post("/invitations/{id}/resend")]"#),
        "{invitations}"
    );
}

#[test]
fn generate_teams_sends_invite_mail_synchronously_not_deliver_later() {
    let (_tmp, project) = fresh_project("teams-sync-mail-app");
    run_autumn(&project, &["generate", "teams"]);

    let invitations = fs::read_to_string(project.join("src/teams/routes/invitations.rs")).unwrap();
    assert!(
        invitations.contains(".send_invite("),
        "invite mail must be sent synchronously: {invitations}"
    );
    assert!(
        !invitations.contains(".deliver_later_invite("),
        "invite mail must not be a fire-and-forget background send: {invitations}"
    );
}

#[test]
fn generate_teams_guards_against_admin_self_promotion_to_owner() {
    let (_tmp, project) = fresh_project("teams-owner-guard-app");
    run_autumn(&project, &["generate", "teams"]);

    // create_invitation: an Admin cannot mint a fresh Owner invite.
    let invitations = fs::read_to_string(project.join("src/teams/routes/invitations.rs")).unwrap();
    assert!(
        invitations.contains("role == Role::Owner && caller_role != Role::Owner"),
        "{invitations}"
    );
    assert!(
        invitations.contains("Only an owner can invite someone as owner"),
        "{invitations}"
    );

    // change_role: an Admin cannot promote an existing member to Owner.
    let members = fs::read_to_string(project.join("src/teams/routes/members.rs")).unwrap();
    assert!(
        members.contains("new_role == Role::Owner && caller_role != Role::Owner"),
        "{members}"
    );
    assert!(
        members.contains("Only an owner can grant the owner role"),
        "{members}"
    );
}

// ── autumn generate auth email confirmation (issue #823) ──────────────────────
//
// RED phase: these tests capture the full acceptance criteria from #823.
// They fail until the email-confirmation feature is implemented in auth.rs.

/// AC1, AC3, AC4: Migration includes `email_confirmed_at`, `confirm_token_digest`,
/// and `confirm_token_expires_at` columns.
#[test]
fn generate_auth_confirmation_migration_has_new_columns() {
    let (_tmp, project) = fresh_project("auth-confirm-migration");
    run_autumn(&project, &["generate", "auth", "User"]);

    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
        .collect();
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();

    assert!(
        up.contains("email_confirmed_at"),
        "up.sql missing email_confirmed_at column"
    );
    assert!(
        up.contains("confirm_token_digest"),
        "up.sql missing confirm_token_digest column"
    );
    assert!(
        up.contains("confirm_token_expires_at"),
        "up.sql missing confirm_token_expires_at column"
    );
}

/// AC1, AC3: User model has confirmation fields; signup redirects to a
/// confirmation-pending page instead of logging the user in.
#[test]
fn generate_auth_confirmation_model_fields_and_signup_not_logged_in() {
    let (_tmp, project) = fresh_project("auth-confirm-signup");
    run_autumn(&project, &["generate", "auth", "User"]);

    let model = fs::read_to_string(project.join("src/models/user.rs")).unwrap();
    assert!(
        model.contains("pub email_confirmed_at: Option<chrono::NaiveDateTime>"),
        "model missing email_confirmed_at field"
    );
    assert!(
        model.contains("pub confirm_token_digest: Option<String>"),
        "model missing confirm_token_digest field"
    );
    assert!(
        model.contains("pub confirm_token_expires_at: Option<chrono::NaiveDateTime>"),
        "model missing confirm_token_expires_at field"
    );

    // Signup must NOT log the user in — redirect to a confirmation-pending page.
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(
        routes.contains("confirm-email") || routes.contains("check-your-email"),
        "signup handler must redirect to a confirmation-pending page, not /account"
    );
}

/// AC2: Confirmation route `GET /auth/confirm/{token}` exists, stamps
/// `email_confirmed_at`, and invalidates the token.
#[test]
fn generate_auth_confirmation_route_marks_confirmed_and_invalidates_token() {
    let (_tmp, project) = fresh_project("auth-confirm-route");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(
        routes.contains("pub async fn confirm_email"),
        "routes/auth.rs missing confirm_email handler"
    );
    assert!(
        routes.contains("/auth/confirm/"),
        "routes/auth.rs missing /auth/confirm/:token path"
    );
    assert!(
        routes.contains("email_confirmed_at"),
        "confirm_email handler must stamp email_confirmed_at"
    );
    // Token must be cleared after use.
    assert!(
        routes.contains("confirm_token_digest.eq(None::<String>)"),
        "confirm_email handler must invalidate token after use (set digest to NULL)"
    );
}

/// AC3: Only the SHA-256 digest of the confirmation token is stored in the DB.
#[test]
fn generate_auth_confirmation_only_digest_stored_in_db() {
    let (_tmp, project) = fresh_project("auth-confirm-digest");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    // Digest must be stored (not raw token).
    let stores_digest = routes.contains("confirm_token_digest.eq(Some(&confirm_digest")
        || routes.contains("confirm_token_digest.eq(Some(&token_digest");
    assert!(
        stores_digest,
        "confirmation token digest (not raw token) must be stored"
    );
}

/// AC4: Confirmation tokens expire after 24 hours (default).
#[test]
fn generate_auth_confirmation_token_expires_24h() {
    let (_tmp, project) = fresh_project("auth-confirm-expiry");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(
        routes.contains("hours(24)"),
        "confirmation token must default to 24-hour expiry"
    );
    assert!(
        routes.contains("confirm_token_expires_at.gt(now)"),
        "confirm handler must reject tokens past confirm_token_expires_at"
    );
}

/// AC5: Unconfirmed login is rejected; login page offers a "resend confirmation"
/// affordance.
#[test]
fn generate_auth_unconfirmed_login_rejected_with_resend_affordance() {
    let (_tmp, project) = fresh_project("auth-confirm-login-gate");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(
        routes.contains("email_confirmed_at"),
        "login handler must check email_confirmed_at before granting session"
    );
    assert!(
        routes.contains("resend") || routes.contains("Resend"),
        "login form must offer a resend confirmation email affordance"
    );
}

/// AC6: Resend-confirmation handler exists and overwrites the old token.
#[test]
fn generate_auth_resend_confirmation_invalidates_old_token() {
    let (_tmp, project) = fresh_project("auth-confirm-resend");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(
        routes.contains("pub async fn resend_confirmation"),
        "routes/auth.rs missing resend_confirmation handler"
    );
    assert!(
        routes.contains("confirm_token_digest"),
        "resend_confirmation must update confirm_token_digest"
    );
}

/// AC7: The generated account route or a helper function demonstrates a
/// confirmed-only gate (`email_confirmed_at` check).
#[test]
fn generate_auth_confirmed_gate_present() {
    let (_tmp, project) = fresh_project("auth-confirm-gate");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(
        routes.contains("email_confirmed_at.is_some()")
            || routes.contains("email_confirmed_at.is_none()")
            || routes.contains("require_confirmed"),
        "routes must demonstrate an email-confirmed gate"
    );
}

/// AC8: Password-reset completion does NOT stamp `email_confirmed_at`.
#[test]
fn generate_auth_password_reset_does_not_confirm_email() {
    let (_tmp, project) = fresh_project("auth-confirm-reset-independence");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();

    // Locate the reset_password handler body (between its `pub async fn` and the next one).
    let reset_start = routes
        .find("pub async fn reset_password(")
        .expect("reset_password handler must exist");
    let rest = &routes[reset_start..];
    // Everything up to the next `pub async fn` is the handler body.
    let reset_body_end = rest[1..]
        .find("pub async fn ")
        .map_or(rest.len(), |p| p + 1);
    let reset_body = &rest[..reset_body_end];

    assert!(
        !reset_body.contains("email_confirmed_at.eq("),
        "reset_password must NOT set email_confirmed_at (confirmation and credential recovery are independent)"
    );
}

/// AC10: The signup handler checks `mailer.is_disabled()` and returns a clear
/// error when mail is not configured — matching the forgot-password precedent.
#[test]
fn generate_auth_confirmation_signup_fails_clearly_when_mail_disabled() {
    let (_tmp, project) = fresh_project("auth-confirm-mail-check");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    assert!(
        routes.contains("mailer.is_disabled()"),
        "signup must check mailer.is_disabled() and return a clear error message"
    );
}

/// AC11: Generated tests/auth.rs covers all confirmation-related flows.
#[test]
fn generate_auth_confirmation_tests_cover_required_flows() {
    let (_tmp, project) = fresh_project("auth-confirm-tests");
    run_autumn(&project, &["generate", "auth", "User"]);

    let tests = fs::read_to_string(project.join("tests/auth.rs")).unwrap();
    for flow in [
        "signup_without_confirm_cannot_login",
        "confirm_with_valid_token_can_login",
        "confirm_with_expired_token_fails",
        "confirm_with_replayed_token_fails",
        "resend_confirmation_rate_limit",
        // The old `email_change_reenters_unconfirmed` stub was replaced by the
        // #1396 account-flow tests: `email_change_confirm_invalid_token_fails`
        // guards the confirm endpoint, and `password_and_email_change_end_to_end`
        // exercises the full pending-email change (old address stays usable until
        // the new-address token is confirmed, then the new address signs in).
        "email_change_confirm_invalid_token_fails",
        "password_and_email_change_end_to_end",
    ] {
        assert!(tests.contains(flow), "tests/auth.rs missing test: {flow}");
    }
}

/// AC12: docs/guide/authentication.md gains a confirmation section covering
/// the threat model, digest storage, gate usage, and email-change behavior.
#[test]
fn generate_auth_confirmation_docs_section_present() {
    let (_tmp, project) = fresh_project("auth-confirm-docs");
    run_autumn(&project, &["generate", "auth", "User"]);

    let docs = fs::read_to_string(project.join("docs/guide/authentication.md")).unwrap();
    assert!(
        docs.contains("Email Confirmation") || docs.contains("email confirmation"),
        "docs missing Email Confirmation section"
    );
    assert!(
        docs.contains("digest") || docs.contains("SHA-256"),
        "docs must describe digest-only token storage rule"
    );
    assert!(
        docs.contains("email_confirmed_at"),
        "docs must reference the email_confirmed_at field"
    );
}

/// AC13: Docs include an opt-in migration path (ALTER TABLE SQL) for existing apps.
#[test]
fn generate_auth_confirmation_docs_migration_path_for_existing_apps() {
    let (_tmp, project) = fresh_project("auth-confirm-migration-path");
    run_autumn(&project, &["generate", "auth", "User"]);

    let docs = fs::read_to_string(project.join("docs/guide/authentication.md")).unwrap();
    assert!(
        docs.contains("email_confirmed_at") && docs.contains("confirm_token_digest"),
        "docs migration path must name both new columns"
    );
    assert!(
        docs.contains("ADD COLUMN") || docs.contains("ALTER TABLE"),
        "docs must include ALTER TABLE migration SQL for existing apps"
    );
}

/// main.rs registers the new confirmation routes.
#[test]
fn generate_auth_confirmation_routes_registered_in_main() {
    let (_tmp, project) = fresh_project("auth-confirm-main-rs");
    run_autumn(&project, &["generate", "auth", "User"]);

    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main.contains("routes::auth::confirm_email"),
        "main.rs must register confirm_email route"
    );
    assert!(
        main.contains("routes::auth::resend_confirmation"),
        "main.rs must register resend_confirmation route"
    );
}

// ── Active session management (issue #819) ────────────────────────────────────
//
// `autumn generate auth` must emit first-class login-session tracking: a
// per-login session row (token digest, IP, parsed User-Agent, label), per-request
// validation with throttled `last_seen_at` updates, revocation APIs on the user
// model, an `/account/sessions` Maud+htmx page, auto-revocation on
// credential-changing events, integration tests, and privacy documentation.

/// AC1 — a session row is persisted per login with token digest, user id,
/// timestamps, IP, parsed User-Agent fields, and an optional device label.
#[test]
fn generate_auth_sessions_migration_schema_and_model() {
    let (_tmp, project) = fresh_project("auth-sess-app");
    run_autumn(&project, &["generate", "auth", "User"]);

    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_users"))
        .collect();
    assert_eq!(migrations.len(), 1, "expected one create_users migration");
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE TABLE user_sessions"),
        "up.sql missing CREATE TABLE user_sessions:\n{up}"
    );
    for column in [
        "user_id BIGINT NOT NULL REFERENCES users",
        "token_digest TEXT NOT NULL UNIQUE",
        "ip TEXT NOT NULL",
        "user_agent TEXT NOT NULL",
        "ua_family TEXT NOT NULL",
        "ua_os TEXT NOT NULL",
        "ua_device TEXT NOT NULL",
        "label TEXT NULL",
        "last_seen_at TIMESTAMP NOT NULL",
    ] {
        assert!(up.contains(column), "up.sql missing column: {column}\n{up}");
    }
    let down = fs::read_to_string(migrations[0].path().join("down.sql")).unwrap();
    assert!(
        down.contains("DROP TABLE user_sessions"),
        "down.sql must drop user_sessions"
    );
    // Dependent table must drop before the referenced users table.
    assert!(
        down.find("DROP TABLE user_sessions").unwrap() < down.find("DROP TABLE users").unwrap(),
        "user_sessions must be dropped before users"
    );

    // schema.rs gains the table block.
    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(
        schema.contains("user_sessions (id)"),
        "schema.rs missing user_sessions block"
    );
    assert!(
        schema.contains("token_digest -> Text"),
        "schema.rs missing token_digest column"
    );

    // Model file: session row + revocation APIs on the user model (AC3).
    let model = fs::read_to_string(project.join("src/models/user_session.rs")).unwrap();
    assert!(
        model.contains("pub struct UserSession"),
        "model missing UserSession struct"
    );
    for needle in [
        "pub token_digest: String",
        "pub last_seen_at: chrono::NaiveDateTime",
        "pub label: Option<String>",
        "pub async fn sessions(",
        "pub async fn revoke_session(",
        "pub async fn revoke_other_sessions(",
        "pub async fn revoke_all_sessions(",
    ] {
        assert!(model.contains(needle), "user_session.rs missing: {needle}");
    }
    // The raw session id must never be stored — only its digest.
    assert!(
        !model.contains("pub token: String"),
        "raw session token must not be stored"
    );

    let mod_rs = fs::read_to_string(project.join("src/models/mod.rs")).unwrap();
    assert!(
        mod_rs.contains("pub mod user_session;"),
        "models/mod.rs missing pub mod user_session"
    );
}

/// AC2 + AC4 + AC6 — routes record the session on login, validate it on
/// authenticated requests (with bounded `last_seen_at` writes), destroy
/// revoked sessions immediately, and serve the /account/sessions page.
#[test]
fn generate_auth_sessions_routes_and_page() {
    let (_tmp, project) = fresh_project("auth-sess-routes");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();

    // Login + logout lifecycle.
    assert!(
        routes.contains("pub async fn record_login_session"),
        "routes missing record_login_session helper"
    );
    assert!(
        routes.contains("pub async fn session_token_digest"),
        "routes missing session_token_digest helper"
    );
    assert!(
        routes.contains("autumn_web::user_agent::parse_user_agent"),
        "login must parse the User-Agent via autumn_web::user_agent"
    );
    // The tracked-session gate: row lookup + throttled last_seen_at update.
    assert!(
        routes.contains("pub async fn require_tracked_session"),
        "routes missing require_tracked_session gate"
    );
    assert!(
        routes.contains("last_seen_update_secs"),
        "last_seen_at updates must be throttled via config"
    );
    // Revoked sessions are destroyed so a replayed cookie cannot resurrect them.
    assert!(
        routes.contains("session.destroy()"),
        "require_tracked_session must destroy revoked sessions"
    );

    // The sessions page + revocation handlers (AC6).
    for needle in [
        "pub async fn sessions_page",
        "pub async fn sessions_revoke",
        "pub async fn sessions_revoke_others",
        "pub async fn sessions_label",
        "#[get(\"/account/sessions\")]",
        "#[post(\"/account/sessions/{id}/revoke\")]",
        "#[post(\"/account/sessions/revoke-others\")]",
        "#[post(\"/account/sessions/{id}/label\")]",
    ] {
        assert!(routes.contains(needle), "routes/auth.rs missing: {needle}");
    }
    // htmx-powered page with a one-click "sign out everywhere else".
    assert!(
        routes.contains("hx-post"),
        "sessions page must use htmx for revocation"
    );
    assert!(
        routes.to_lowercase().contains("sign out everywhere else"),
        "sessions page must offer one-click bulk revocation"
    );

    // main.rs registers the new handlers.
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    for entry in [
        "routes::auth::sessions_page",
        "routes::auth::sessions_revoke",
        "routes::auth::sessions_revoke_others",
        "routes::auth::sessions_label",
    ] {
        assert!(main.contains(entry), "main.rs missing route: {entry}");
    }
}

/// AC5 (password change) — resetting the password revokes existing sessions
/// by default, gated on the `[auth.sessions]` config flag.
#[test]
fn generate_auth_reset_password_revokes_sessions() {
    let (_tmp, project) = fresh_project("auth-sess-reset");
    run_autumn(&project, &["generate", "auth", "User"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    let reset_body = &routes[routes.find("pub async fn reset_password(").unwrap()..];
    let reset_body = &reset_body[..reset_body.find("\n// ──").unwrap_or(reset_body.len())];
    assert!(
        reset_body.contains("revoke_existing_sessions") || reset_body.contains("user_sessions"),
        "reset_password must revoke existing sessions"
    );
    assert!(
        reset_body.contains("revoke_on_credential_change"),
        "session revocation on password change must be configurable"
    );
    assert!(
        reset_body.contains("insert_into(user_sessions::table)"),
        "reset_password logs the user in and must record the new session in its transaction"
    );
}

/// AC5 (TOTP) — enrollment and disable revoke all *other* sessions by default.
#[test]
fn generate_auth_totp_changes_revoke_other_sessions() {
    let (_tmp, project) = fresh_project("auth-sess-totp");
    run_autumn(&project, &["generate", "auth", "User", "--totp"]);

    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();
    for handler in [
        "pub async fn two_factor_confirm",
        "pub async fn two_factor_disable",
    ] {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        let body = &body[..body.find("\n/// `").unwrap_or(body.len())];
        assert!(
            body.contains("token_digest.ne(") || body.contains("revoke_other_sessions"),
            "{handler} must revoke other sessions"
        );
        assert!(
            body.contains("revoke_on_credential_change"),
            "{handler} revocation must be configurable"
        );
    }
    // Completing a TOTP login also records the session row.
    assert!(
        routes.contains("pub async fn login_verify"),
        "missing login_verify"
    );
    let verify = &routes[routes.find("pub async fn login_verify(").unwrap()..];
    assert!(
        verify.contains("record_login_session"),
        "login_verify completes a login and must record the session row"
    );
}

/// AC5 (`WebAuthn`) — passkey add/remove revoke all *other* sessions by default,
/// and passkey login records a session row.
#[test]
fn generate_auth_passkeys_changes_revoke_other_sessions() {
    let (_tmp, project) = fresh_project("auth-sess-passkeys");
    run_autumn(&project, &["generate", "auth", "User", "--passkeys"]);

    let routes = fs::read_to_string(project.join("src/routes/passkeys.rs")).unwrap();
    for handler in [
        "pub async fn passkey_register_finish",
        "pub async fn passkey_revoke",
    ] {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        let body = &body[..body.find("\n/// `").unwrap_or(body.len())];
        assert!(
            body.contains("token_digest.ne(") || body.contains("revoke_other_sessions"),
            "{handler} must revoke other sessions"
        );
        assert!(
            body.contains("revoke_on_credential_change"),
            "{handler} revocation must be configurable"
        );
    }
    let login_finish = &routes[routes.find("pub async fn passkey_login_finish(").unwrap()..];
    assert!(
        login_finish.contains("record_login_session"),
        "passkey_login_finish completes a login and must record the session row"
    );
}

/// AC7 — the generated integration tests cover the two-client revocation flow.
#[test]
fn generate_auth_sessions_tests_emitted() {
    let (_tmp, project) = fresh_project("auth-sess-tests");
    run_autumn(&project, &["generate", "auth", "User"]);

    let tests = fs::read_to_string(project.join("tests/auth_sessions.rs")).unwrap();
    for needle in [
        "fn sessions_page_rejects_anonymous",
        "fn revoked_session_next_request_401s",
        "fn revoke_other_sessions_keeps_current_session_alive",
        "/account/sessions",
    ] {
        assert!(
            tests.contains(needle),
            "tests/auth_sessions.rs missing: {needle}"
        );
    }
}

/// AC8 — documentation covers privacy posture for stored IP/UA and how to
/// plug in a custom User-Agent parser.
#[test]
fn generate_auth_sessions_docs_emitted() {
    let (_tmp, project) = fresh_project("auth-sess-docs");
    run_autumn(&project, &["generate", "auth", "User"]);

    let docs = fs::read_to_string(project.join("docs/guide/session-management.md")).unwrap();
    for needle in [
        "## Privacy",
        "retention",
        "parse_user_agent",
        "revoke_on_credential_change",
        "/account/sessions",
        "CREATE TABLE user_sessions",
    ] {
        assert!(
            docs.contains(needle),
            "session-management.md missing: {needle}"
        );
    }
}

/// PR #1176 review hardening: reauth must re-point the tracked session row
/// after rotating the session id (otherwise step-up locks the user out),
/// every protected handler — including GET form pages — must go through the
/// tracked-session gate, and the revocation controls must work without
/// JavaScript (real forms, htmx as progressive enhancement).
#[test]
fn generate_auth_sessions_review_hardening() {
    let (_tmp, project) = fresh_project("auth-sess-hardening");
    run_autumn(&project, &["generate", "auth", "User"]);
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();

    // P1: reauth rotates the session id and must rebind the tracked row to
    // the new digest, after the rotation.
    let reauth_start = routes
        .find("pub async fn reauth(")
        .expect("missing reauth handler");
    let reauth = &routes[reauth_start..];
    let reauth = &reauth[..reauth.find("\n// ──").unwrap_or(reauth.len())];
    let rotate_at = reauth
        .find("session.rotate_id()")
        .expect("reauth must rotate");
    let rebind_at = reauth
        .find("rebind_tracked_session")
        .expect("reauth must rebind the tracked session row after rotation");
    assert!(
        rotate_at < rebind_at,
        "reauth must rebind AFTER rotating the session id"
    );

    // P2: protected GET form pages validate the tracked row too, so a revoked
    // device's next request 401s no matter which authenticated route it hits.
    for handler in [
        "pub async fn data_export_form",
        "pub async fn delete_account_form",
        "pub async fn reauth_form",
    ] {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        let body = &body[..body.find("\n/// `").unwrap_or(body.len())];
        assert!(
            body.contains("require_tracked_session"),
            "{handler} must validate the tracked session row"
        );
    }

    // P2: revocation controls are real POST forms (usable without JS), with
    // htmx attributes for in-place swaps when JS is available.
    assert!(
        routes.contains("form method=\"post\" action=\"/account/sessions/revoke-others\""),
        "bulk revoke must be a real form for non-JS fallback"
    );
    assert!(
        routes.contains("action={ \"/account/sessions/\" (s.id) \"/revoke\" }"),
        "per-session revoke must be a real form for non-JS fallback"
    );
    assert!(
        routes.contains("hx-post=\"/account/sessions/revoke-others\""),
        "bulk revoke keeps htmx enhancement"
    );
}

/// PR #1176 review hardening for `--passkeys`: the registration page and
/// challenge endpoint must also validate the tracked session row.
#[test]
fn generate_auth_passkeys_pages_gated_on_tracked_session() {
    let (_tmp, project) = fresh_project("auth-sess-pk-gate");
    run_autumn(&project, &["generate", "auth", "User", "--passkeys"]);
    let routes = fs::read_to_string(project.join("src/routes/passkeys.rs")).unwrap();

    for handler in [
        "pub async fn passkey_register_page",
        "pub async fn passkey_register_begin",
    ] {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        let body = &body[..body.find("\n/// `").unwrap_or(body.len())];
        assert!(
            body.contains("require_tracked_session"),
            "{handler} must validate the tracked session row"
        );
    }
}

// ── AC#2: --live and --live-validation scaffold tests (Issue #1445) ─────────

/// `--live-validation` emits `hx-post`, `hx-trigger`, `hx-target`, `hx-swap`
/// attributes on validated form inputs plus a companion `<span id="…-error">`.
#[test]
fn live_validation_emits_hx_post_and_error_slot() {
    let (_tmp, project) = fresh_project("lv-hx-post");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:String",
            "--validate",
            "title=length:min=1,max=200",
            "--live-validation",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // A validated text field on `--live-validation` renders via the
    // changeset-aware `text_input_htmx` helper (issue #1124), which wires up
    // its own hx-post/hx-trigger/hx-target/hx-swap and inline error block —
    // the generator no longer hand-rolls these attributes. `title` is
    // non-nullable, so it keeps `required`/`aria-required` via the
    // `required_text_input_htmx` variant.
    assert!(
        routes.contains(
            "autumn_web::form::required_text_input_htmx(&changeset, \"title\", \"Title\", &paths::validate_title())"
        ),
        "create form must render the validated title input via required_text_input_htmx with a typed path helper:\n{routes}"
    );
    // body field (not validated, but required) must use required_text_input.
    assert!(
        routes.contains("autumn_web::form::required_text_input(&changeset, \"body\", \"Body\")"),
        "unvalidated required body input must use required_text_input:\n{routes}"
    );
    assert!(
        !routes.contains("autumn_web::form::text_input_htmx(&changeset, \"body\""),
        "unvalidated body input must not use text_input_htmx:\n{routes}"
    );
}

/// `--live-validation` emits a `validate_{field}` route handler that decodes
/// the full form, validates through the same `{Pascal}Form`/`Changeset`
/// machinery as `create`/`update` (issue #1124 follow-up), and returns
/// `text_input_htmx`'s full field wrapper — never a bare error span, which
/// would delete the input on htmx's `hx-swap="outerHTML"`.
#[test]
fn live_validation_emits_validate_handler_with_real_rules() {
    let (_tmp, project) = fresh_project("lv-validate-handler");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "site:String",
            "email:String",
            "--validate",
            "title=length:min=1,max=200",
            "--validate",
            "site=url",
            "--validate",
            "email=email",
            "--live-validation",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    for field in ["title", "site", "email"] {
        assert!(
            routes.contains(&format!("pub async fn validate_{field}(")),
            "routes must contain a validate_{field} handler:\n{routes}"
        );
        // All three fields are non-nullable, so the required htmx variant
        // keeps the `required`/`aria-required` signal through the swap too.
        assert!(
            routes.contains(&format!(
                "autumn_web::form::required_text_input_htmx(&changeset, \"{field}\""
            )),
            "validate_{field} must return the full required_text_input_htmx wrapper, \
             not a bare error span:\n{routes}"
        );
    }
    // Each handler decodes the whole form (htmx posts the entire form via
    // `hx-include="closest form"`) and validates through the derived
    // `#[validate(...)]` rules on `PostForm` — one rule implementation, not a
    // hand-rolled duplicate per field.
    assert_eq!(
        routes
            .matches("let Ok(form) = decode_form(body) else")
            .count(),
        3,
        "every validate_{{field}} handler must decode via the shared decode_form:\n{routes}"
    );
    assert!(
        routes.contains("#[validate(length(min = 1, max = 200))]\n    pub title: String"),
        "PostForm must carry the length rule for validator::Validate to enforce:\n{routes}"
    );
}

/// Without `--live-validation` the routes file contains no validate handlers
/// and no hx-post attributes on form inputs.
#[test]
fn without_live_validation_no_hx_attrs() {
    let (_tmp, project) = fresh_project("no-lv");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--validate",
            "title=length:min=1,max=200",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    assert!(
        !routes.contains("hx-post"),
        "routes without --live-validation must not have hx-post:\n{routes}"
    );
    assert!(
        !routes.contains("validate_title"),
        "routes without --live-validation must not have validate_title handler:\n{routes}"
    );
}

/// `--live-validation` without `--live` still loads the htmx script tag so
/// that validation requests can fire.
#[test]
fn live_validation_without_live_loads_htmx_script() {
    let (_tmp, project) = fresh_project("lv-no-live-htmx");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--validate",
            "title=length:min=1,max=200",
            "--live-validation",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    assert!(
        routes.contains("HTMX_JS_PATH"),
        "layout must include htmx script when --live-validation is set:\n{routes}"
    );

    // Cargo.toml must have htmx + maud features even when --live is not set,
    // because the generated validate handlers return Markup and the layout
    // references HTMX_JS_PATH.
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"htmx\"") || cargo.contains("htmx"),
        "Cargo.toml must include autumn-web htmx feature for --live-validation:\n{cargo}"
    );
    assert!(
        cargo.contains("\"maud\"") || cargo.contains("maud"),
        "Cargo.toml must include autumn-web maud feature for --live-validation:\n{cargo}"
    );
}

/// `--live` emits a `LiveFragment` impl for the model and a `broadcasts`
/// attribute on the repository.
#[test]
fn live_scaffold_emits_live_fragment_and_broadcasts() {
    let (_tmp, project) = fresh_project("live-frag");
    run_autumn(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--live"],
    );

    let repo = fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();
    assert!(
        repo.contains("broadcasts = true"),
        "repository must have broadcasts attribute under --live:\n{repo}"
    );
    // LiveFragment impl is co-located in the repository file next to the
    // `#[repository]` annotation that uses it.
    assert!(
        repo.contains("impl autumn_web::live::LiveFragment for Post"),
        "repository must contain LiveFragment impl under --live:\n{repo}"
    );
    // insert_swap must target the list container so new rows are appended
    // rather than replacing a non-existent element on remote clients.
    assert!(
        repo.contains("fn insert_swap()") && repo.contains("OobMethod::BeforeEnd"),
        "LiveFragment impl must override insert_swap() with BeforeEnd targeting the list container:\n{repo}"
    );
    // render_fragment must include a show-page link so live rows are navigable.
    // The URL goes through the routes module's typed path helper (issue #1133).
    assert!(
        repo.contains("a href=(crate::routes::posts::paths::show(self.id))"),
        "render_fragment must include a show link href via the typed path helper:\n{repo}"
    );
}

/// `--live` wires the index list container to an SSE stream so the list
/// updates via push events.
#[test]
fn live_scaffold_index_uses_sse_list_and_stream_route() {
    let (_tmp, project) = fresh_project("live-sse");
    run_autumn(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--live"],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    assert!(
        routes.contains("hx-ext=\"sse\""),
        "index list must have hx-ext=\"sse\" under --live:\n{routes}"
    );
    assert!(
        routes.contains("sse-connect=(paths::events())"),
        "index list must connect to the SSE stream endpoint via the typed path helper:\n{routes}"
    );
    assert!(
        routes.contains("pub async fn events"),
        "routes must contain the stream handler:\n{routes}"
    );
}

/// `--live` layout must include the idiomorph script, enable morph on the
/// body, and wire the SSE container with hx-swap="none" so that OOB fragments
/// are processed without the in-band innerHTML swap clearing the list.
#[test]
fn live_layout_references_idiomorph_and_morph() {
    let (_tmp, project) = fresh_project("live-morph");
    run_autumn(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--live"],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    assert!(
        routes.contains("IDIOMORPH_JS_PATH"),
        "layout <head> must include the idiomorph script under --live:\n{routes}"
    );
    assert!(
        routes.contains(r#"body hx-ext="morph""#),
        "layout <body> must carry hx-ext=\"morph\" under --live:\n{routes}"
    );
    assert!(
        routes.contains(r#"hx-swap="none""#),
        "SSE list container must use hx-swap=\"none\" under --live:\n{routes}"
    );
}

/// `--api --live` emits the SSE stream handler inside the repository file
/// (there is no routes file for API scaffolds) and renders fragment items as
/// plain ids rather than links (no HTML show page exists).
#[test]
fn live_api_scaffold_emits_stream_handler_in_repository() {
    let (_tmp, project) = fresh_project("live-api");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--api",
            "--live",
        ],
    );

    // API scaffolds produce no routes file — the stream handler lives in the
    // repository instead.
    assert!(
        !project.join("src/routes/posts.rs").is_file(),
        "--api scaffold must not create a routes file"
    );

    let repo = fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();

    // The stream handler must be appended to the repository file.
    assert!(
        repo.contains("pub async fn stream("),
        "repository must contain stream handler under --api --live:\n{repo}"
    );
    assert!(
        repo.contains("GET /posts/stream"),
        "repository must document the stream route path:\n{repo}"
    );
    assert!(
        repo.contains("autumn_web::sse::stream(&state, \"posts\")"),
        "stream handler must delegate to sse::stream:\n{repo}"
    );

    // LiveFragment impl is still present but renders plain ids (no show link).
    assert!(
        repo.contains("impl autumn_web::live::LiveFragment for Post"),
        "repository must contain LiveFragment impl under --api --live:\n{repo}"
    );
    assert!(
        !repo.contains("a href=(format!(\"/posts/"),
        "API LiveFragment must NOT emit a show-page link (no HTML routes):\n{repo}"
    );
    assert!(
        repo.contains("(self.id)"),
        "API LiveFragment render_fragment must emit plain id:\n{repo}"
    );
}

/// PR #1176 Codex round 2: login flows must delete the consumed session's
/// tracked row before rotating (no phantom devices), the password-reset
/// commit must be atomic with its session revocation (no consumed-token 500
/// limbo), and the TOTP reset-commit path gets the same treatment.
#[test]
fn generate_auth_sessions_codex_round2() {
    let (_tmp, project) = fresh_project("auth-sess-codex2");
    run_autumn(&project, &["generate", "auth", "User"]);
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();

    let body_of = |handler: &str| {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        &body[..body.find("\n/// `").unwrap_or(body.len())]
    };

    // Re-login from an already-tracked browser: the old row must be removed
    // BEFORE the rotation that destroys its session id.
    let login = body_of("pub async fn login(");
    let untrack_at = login
        .find("untrack_current_session")
        .expect("login must untrack the consumed session row");
    let rotate_at = login
        .find("session.rotate_id()")
        .expect("login must rotate");
    assert!(
        untrack_at < rotate_at,
        "login must untrack BEFORE rotating the session id"
    );
    for handler in [
        "pub async fn confirm_email(",
        "pub async fn reset_password(",
    ] {
        assert!(
            body_of(handler).contains("untrack_current_session"),
            "{handler} rotates while logging in and must untrack the old row"
        );
    }
    assert!(
        body_of("pub async fn logout(").contains("untrack_current_session"),
        "logout shares the untrack helper"
    );

    // Password change + session revocation + token consumption are one
    // transaction: a failure rolls everything back so the reset link
    // remains usable, and no path leaves sessions unrevoked after the
    // password actually changed.
    let reset = body_of("pub async fn reset_password(");
    assert!(
        reset.contains(".transaction"),
        "reset_password must commit password + revocation atomically"
    );
    assert!(
        reset.contains("revoke_on_credential_change") && reset.contains("user_sessions"),
        "reset_password revocation must stay config-gated and target user_sessions"
    );
}

/// PR #1176 Codex round 2 (`--totp`): the post-enrollment/disable revocation
/// happens inside the existing transactions so a revocation failure can
/// never 500 after the credential change committed (which would hide the
/// one-time recovery codes), and the deferred-reset commit in `login_verify`
/// is atomic with its revocation.
#[test]
fn generate_auth_totp_revocation_is_atomic() {
    let (_tmp, project) = fresh_project("auth-sess-totp-atomic");
    run_autumn(&project, &["generate", "auth", "User", "--totp"]);
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();

    let body_of = |handler: &str| {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        &body[..body.find("\n/// `").unwrap_or(body.len())]
    };

    for handler in [
        "pub async fn two_factor_confirm(",
        "pub async fn two_factor_disable(",
    ] {
        let body = body_of(handler);
        assert!(
            body.contains("revoke_on_credential_change"),
            "{handler} revocation must stay config-gated"
        );
        // The other-sessions delete lives inside the credential txn.
        assert!(
            body.contains("token_digest.ne("),
            "{handler} must revoke other sessions inside its transaction"
        );
        assert!(
            !body.contains("revoke_other_sessions(&mut *db"),
            "{handler} must not revoke outside the transaction (a post-commit \
             failure would 500 after the credential change)"
        );
    }

    let verify = body_of("pub async fn login_verify(");
    assert!(
        verify.contains(".transaction"),
        "login_verify's deferred password-reset commit must be atomic with revocation"
    );
}

/// PR #1176 Codex round 3: signup must survive an already-authenticated
/// browser (rebind the tracked row across its rotation), the password-reset
/// transaction must include the new session-row insert (no 500 after the
/// reset link is consumed), and passkey changes must revoke other sessions
/// in the same transaction as the credential change.
#[test]
fn generate_auth_sessions_codex_round3() {
    let (_tmp, project) = fresh_project("auth-sess-codex3");
    run_autumn(&project, &["generate", "auth", "User"]);
    let routes = fs::read_to_string(project.join("src/routes/auth.rs")).unwrap();

    let body_of = |handler: &str| {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        &body[..body.find("\n/// `").unwrap_or(body.len())]
    };

    // signup: rotation preserves a previous login's keys, so the tracked row
    // must be re-pointed at the new session id.
    let signup = body_of("pub async fn signup(");
    let rotate_at = signup
        .find("session.rotate_id()")
        .expect("signup must rotate");
    let rebind_at = signup
        .find("rebind_tracked_session")
        .expect("signup must rebind the tracked row across its rotation");
    assert!(
        rotate_at < rebind_at,
        "signup must rebind AFTER rotating the session id"
    );

    // reset_password: the new session row is inserted inside the same
    // transaction as the password change + token consumption, so a failure
    // rolls everything back and the link stays usable.
    let reset = body_of("pub async fn reset_password(");
    assert!(
        reset.contains("insert_into(user_sessions::table)"),
        "reset_password must insert the new session row inside its transaction"
    );
    assert!(
        !reset.contains("record_login_session"),
        "reset_password must not record the session outside the transaction"
    );

    // The UA parse stays funneled through one documented helper.
    assert!(
        routes.contains("pub async fn build_session_row"),
        "session-row construction must be a shared helper"
    );
}

/// PR #1176 Codex round 3 (`--passkeys`): the credential change and the
/// other-sessions revocation commit atomically — the documented
/// revoke-on-credential-change guarantee can never be silently skipped, and
/// a failure rolls back the credential change instead of 500ing after it.
#[test]
fn generate_auth_passkeys_revocation_is_atomic() {
    let (_tmp, project) = fresh_project("auth-sess-pk-atomic");
    run_autumn(&project, &["generate", "auth", "User", "--passkeys"]);
    let routes = fs::read_to_string(project.join("src/routes/passkeys.rs")).unwrap();

    let body_of = |handler: &str| {
        let start = routes
            .find(handler)
            .unwrap_or_else(|| panic!("missing {handler}"));
        let body = &routes[start..];
        &body[..body.find("\n/// `").unwrap_or(body.len())]
    };

    for handler in [
        "pub async fn passkey_register_finish(",
        "pub async fn passkey_revoke(",
    ] {
        let body = body_of(handler);
        assert!(
            body.contains(".transaction"),
            "{handler} must commit the credential change and revocation atomically"
        );
        assert!(
            body.contains("token_digest.ne("),
            "{handler} must revoke other sessions inside the transaction"
        );
        assert!(
            body.contains("revoke_on_credential_change"),
            "{handler} revocation must stay config-gated"
        );
    }
}

// ── autumn generate wizard (issue #832) ──────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn generate_wizard_creates_expected_files() {
    let (_tmp, project) = fresh_project("wizard-app");
    run_autumn(
        &project,
        &[
            "generate", "wizard", "checkout", "shipping", "payment", "review",
        ],
    );

    // ── main wizard file ──────────────────────────────────────────────
    let wizard = fs::read_to_string(project.join("src/wizards/checkout.rs")).unwrap();

    // Wizard configuration constants
    assert!(
        wizard.contains("pub const WIZARD_NAME: &str = \"checkout\";"),
        "wizard file missing WIZARD_NAME constant"
    );
    assert!(
        wizard.contains("pub const STEPS: &[&str] = &[\"shipping\", \"payment\", \"review\"];"),
        "wizard file missing STEPS constant with all step names"
    );
    assert!(
        wizard.contains("pub fn wizard_context(session: Session) -> WizardContext"),
        "wizard file missing wizard_context helper"
    );

    // Step structs
    for (pascal_struct, snake_step) in [
        ("ShippingForm", "shipping"),
        ("PaymentForm", "payment"),
        ("ReviewForm", "review"),
    ] {
        assert!(
            wizard.contains(&format!("pub struct {pascal_struct}")),
            "wizard file missing step struct: {pascal_struct}"
        );
        assert!(
            wizard.contains("Serialize, Deserialize"),
            "step struct for {snake_step} must derive Serialize and Deserialize"
        );
    }

    // GET + POST handlers for every step
    for step in ["shipping", "payment", "review"] {
        assert!(
            wizard.contains(&format!("#[get(\"/checkout/{step}\")]")),
            "wizard file missing GET route attribute for step: {step}"
        );
        assert!(
            wizard.contains(&format!("pub async fn show_{step}(")),
            "wizard file missing show_{step} handler"
        );
        assert!(
            wizard.contains(&format!("#[post(\"/checkout/{step}\")]")),
            "wizard file missing POST route attribute for step: {step}"
        );
        assert!(
            wizard.contains(&format!("pub async fn submit_{step}(")),
            "wizard file missing submit_{step} handler"
        );
    }

    // Confirm is a GET (summary before final commit)
    assert!(
        wizard.contains("#[get(\"/checkout/confirm\")]"),
        "wizard file missing GET /checkout/confirm route"
    );
    assert!(
        wizard.contains("pub async fn show_confirm("),
        "wizard file missing show_confirm handler"
    );

    // Commit and cancel are POST-only
    assert!(
        wizard.contains("#[post(\"/checkout/commit\")]"),
        "commit must be POST, not GET"
    );
    assert!(
        wizard.contains("pub async fn commit("),
        "wizard file missing commit handler"
    );
    assert!(
        wizard.contains("#[post(\"/checkout/cancel\")]"),
        "cancel must be POST, not GET"
    );
    assert!(
        wizard.contains("pub async fn cancel("),
        "wizard file missing cancel handler"
    );

    // Guard and progress rendering
    assert!(
        wizard.contains("wizard.guard_step("),
        "step handlers must call guard_step"
    );
    assert!(
        wizard.contains("wizard_progress("),
        "step handlers must render wizard_progress"
    );

    // CSRF uses optional extractors
    assert!(
        wizard.contains("csrf: Option<CsrfToken>"),
        "GET step handlers must use optional CsrfToken"
    );
    assert!(
        wizard.contains("csrf_field: Option<CsrfFormField>"),
        "GET step handlers must use optional CsrfFormField"
    );

    // ChangesetForm used for step submission
    assert!(
        wizard.contains("use autumn_web::form::ChangesetForm;"),
        "wizard file must import ChangesetForm"
    );

    // 422 on invalid data
    assert!(
        wizard.contains("StatusCode::UNPROCESSABLE_ENTITY"),
        "submit handlers must return 422 on validation failure"
    );

    // wizard.clear() called on both commit and cancel
    assert_eq!(
        wizard.matches("wizard.clear()").count(),
        2,
        "wizard.clear() must be called in both commit and cancel handlers"
    );

    // ── mod.rs ────────────────────────────────────────────────────────
    let mod_rs = fs::read_to_string(project.join("src/wizards/mod.rs")).unwrap();
    assert!(
        mod_rs.contains("pub mod checkout;"),
        "src/wizards/mod.rs missing pub mod checkout"
    );

    // ── integration test skeleton ─────────────────────────────────────
    let test = fs::read_to_string(project.join("tests/checkout_wizard.rs")).unwrap();
    assert!(
        test.contains("checkout_wizard_happy_path"),
        "test file missing checkout_wizard_happy_path test"
    );
    assert!(
        test.contains("checkout_step2_invalid_rerender_with_errors"),
        "test file missing step2 invalid-data test"
    );
    assert!(
        test.contains("checkout_cancel_clears_session_state"),
        "test file missing cancel test"
    );
    assert!(
        test.contains(".wizard-progress"),
        "test file must reference the .wizard-progress CSS selector"
    );
    assert!(
        test.contains("#[ignore"),
        "generated tests must be #[ignore] until the user fills them in"
    );
}

#[test]
fn generate_wizard_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("wizard-dry-app");
    let (stdout, _) = run_autumn(
        &project,
        &[
            "generate",
            "wizard",
            "checkout",
            "shipping",
            "payment",
            "--dry-run",
        ],
    );
    assert!(
        stdout.contains("Dry run"),
        "expected Dry run header; got: {stdout}"
    );
    assert!(
        !project.join("src/wizards/checkout.rs").exists(),
        "dry run must not create the wizard file"
    );
    assert!(
        !project.join("src/wizards/mod.rs").exists(),
        "dry run must not create mod.rs"
    );
    assert!(
        !project.join("tests/checkout_wizard.rs").exists(),
        "dry run must not create the test file"
    );
}

#[test]
fn generate_wizard_collision_without_force_fails() {
    let (_tmp, project) = fresh_project("wizard-collide-app");
    run_autumn(
        &project,
        &["generate", "wizard", "checkout", "shipping", "payment"],
    );
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "wizard", "checkout", "shipping", "payment"],
    );
    assert_eq!(code, Some(1), "re-run without --force must exit 1");
    assert!(
        stderr.contains("would overwrite") || stderr.contains("checkout.rs"),
        "must report collision; got stderr: {stderr}"
    );
}

#[test]
fn generate_wizard_force_overwrites() {
    let (_tmp, project) = fresh_project("wizard-force-app");
    run_autumn(
        &project,
        &["generate", "wizard", "checkout", "shipping", "payment"],
    );
    let wizard_path = project.join("src/wizards/checkout.rs");
    let original = fs::read_to_string(&wizard_path).unwrap();
    fs::write(&wizard_path, "// corrupted").unwrap();
    run_autumn(
        &project,
        &[
            "generate", "wizard", "checkout", "shipping", "payment", "--force",
        ],
    );
    let regenerated = fs::read_to_string(&wizard_path).unwrap();
    assert_eq!(
        regenerated, original,
        "--force must restore original content"
    );
}

#[test]
fn generate_wizard_mod_rs_is_idempotent() {
    let (_tmp, project) = fresh_project("wizard-idempotent-app");
    run_autumn(
        &project,
        &[
            "generate", "wizard", "checkout", "shipping", "payment", "--force",
        ],
    );
    run_autumn(
        &project,
        &[
            "generate", "wizard", "checkout", "shipping", "payment", "--force",
        ],
    );
    let mod_rs = fs::read_to_string(project.join("src/wizards/mod.rs")).unwrap();
    assert_eq!(
        mod_rs.matches("pub mod checkout;").count(),
        1,
        "mod.rs must not gain duplicate pub mod declarations on re-run"
    );
}

#[test]
fn generate_wizard_rejects_fewer_than_two_steps() {
    let (_tmp, project) = fresh_project("wizard-toofew-app");
    let (_, stderr, code) =
        run_autumn_failing(&project, &["generate", "wizard", "checkout", "shipping"]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("at least") || stderr.contains('2'),
        "error must mention the minimum step requirement; got: {stderr}"
    );
}

#[test]
fn generate_wizard_rejects_reserved_step_names() {
    let (_tmp, project) = fresh_project("wizard-reserved-app");
    for reserved in ["confirm", "commit", "cancel"] {
        let (_, stderr, code) = run_autumn_failing(
            &project,
            &["generate", "wizard", "checkout", reserved, "payment"],
        );
        assert_eq!(
            code,
            Some(1),
            "reserved step name '{reserved}' must be rejected"
        );
        assert!(
            stderr.contains(reserved) || stderr.contains("reserved"),
            "error must mention the reserved name '{reserved}'; got: {stderr}"
        );
    }
}

#[test]
fn generate_wizard_rejects_step_name_with_hyphen() {
    let (_tmp, project) = fresh_project("wizard-hyphen-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "wizard", "checkout", "ship-ping", "payment"],
    );
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("ship-ping") || stderr.contains("only ASCII"),
        "error must mention the invalid step name; got: {stderr}"
    );
}

#[test]
fn generate_wizard_rejects_duplicate_step_names() {
    let (_tmp, project) = fresh_project("wizard-dupe-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "wizard", "checkout", "shipping", "shipping"],
    );
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("shipping") || stderr.contains("duplicate"),
        "error must mention the duplicate step name; got: {stderr}"
    );
}

#[test]
fn generate_wizard_rejects_rust_keyword_as_name() {
    let (_tmp, project) = fresh_project("wizard-keyword-app");
    // "type" normalizes to the Rust keyword `type`; `pub mod type;` is invalid Rust.
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "wizard", "type", "shipping", "payment"],
    );
    assert_eq!(code, Some(1), "Rust keyword wizard name must be rejected");
    assert!(
        stderr.contains("keyword") || stderr.contains("type"),
        "error must mention the keyword issue; got: {stderr}"
    );
}

#[test]
fn generate_wizard_rejects_rust_keyword_as_step_name() {
    let (_tmp, project) = fresh_project("wizard-keyword-step-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "wizard", "checkout", "mod", "payment"],
    );
    assert_eq!(code, Some(1), "Rust keyword step name must be rejected");
    assert!(
        stderr.contains("keyword") || stderr.contains("mod"),
        "error must mention the keyword issue; got: {stderr}"
    );
}

#[test]
fn generate_wizard_rejects_underscore_only_name() {
    let (_tmp, project) = fresh_project("wizard-underscore-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "wizard", "_", "shipping", "payment"],
    );
    assert_eq!(
        code,
        Some(1),
        "underscore-only wizard name must be rejected"
    );
    assert!(
        stderr.contains('_') || stderr.contains("letter") || stderr.contains("digit"),
        "error must mention the invalid name; got: {stderr}"
    );
}

#[test]
fn generate_wizard_rejects_gen_keyword() {
    let (_tmp, project) = fresh_project("wizard-gen-app");
    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "wizard", "gen", "shipping", "payment"],
    );
    assert_eq!(
        code,
        Some(1),
        "'gen' (Rust 2024 reserved keyword) must be rejected"
    );
    assert!(
        stderr.contains("keyword") || stderr.contains("gen"),
        "error must mention the keyword issue; got: {stderr}"
    );
}

// ── autumn generate scaffold --sharded integration tests ─────────────────────

#[test]
fn generate_sharded_scaffold_in_fresh_project() {
    let (_tmp, project) = fresh_project("sharded-scaffold-app");

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Account",
            "shard_id:i64",
            "name:String",
            "--sharded",
        ],
    );

    // Model file: must have #[shard_key = "tenant_id"] or #[shard_key = "shard_id"]
    // (shard_id present, tenant_id absent → fallback to id; but shard_id is not tenant_id,
    //  so no tenant_id field → default key is "id")
    // With shard_id but no tenant_id field, default key is "id"
    let model = fs::read_to_string(project.join("src/models/account.rs")).unwrap();
    assert!(
        model.contains("#[shard_key = \"id\"]"),
        "model must have #[shard_key = \"id\"] (default when no tenant_id field):\n{model}"
    );
    assert!(
        model.contains("#[autumn_web::model]"),
        "model must have #[autumn_web::model] attr:\n{model}"
    );

    // Routes file: must use ShardedDb not Db
    let routes = fs::read_to_string(project.join("src/routes/accounts.rs")).unwrap();
    assert!(
        routes.contains("use autumn_web::sharding::ShardedDb"),
        "routes must import ShardedDb:\n{routes}"
    );
    assert!(
        routes.contains("mut db: ShardedDb"),
        "routes must use ShardedDb in handler signatures:\n{routes}"
    );
    assert!(
        routes.contains("from_shard(&db)"),
        "routes index must use from_shard(&db):\n{routes}"
    );
    assert!(
        !routes.contains("mut db: Db"),
        "routes must not use bare Db extractor when sharded:\n{routes}"
    );

    // Repository file: must have shard-aware doc note
    let repo = fs::read_to_string(project.join("src/repositories/account.rs")).unwrap();
    assert!(
        repo.contains("from_shard"),
        "repository must mention from_shard in doc comment:\n{repo}"
    );

    // Migration: must have shard target comment
    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("_create_accounts")
        })
        .collect();
    assert_eq!(
        migrations.len(),
        1,
        "expected one create_accounts migration"
    );
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("autumn migrate --shard"),
        "up.sql must mention `autumn migrate --shard`:\n{up}"
    );
}

#[test]
fn generate_sharded_scaffold_with_tenant_id_field_uses_tenant_id_as_default_key() {
    let (_tmp, project) = fresh_project("sharded-tenant-app");

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Booking",
            "tenant_id:i64",
            "title:String",
            "--sharded",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/booking.rs")).unwrap();
    assert!(
        model.contains("#[shard_key = \"tenant_id\"]"),
        "model must default shard_key to tenant_id when that field is present:\n{model}"
    );
}

#[test]
fn generate_sharded_scaffold_with_explicit_shard_key() {
    let (_tmp, project) = fresh_project("sharded-explicit-app");

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Widget",
            "org_id:i64",
            "label:String",
            "--sharded",
            "--shard-key",
            "org_id",
        ],
    );

    let model = fs::read_to_string(project.join("src/models/widget.rs")).unwrap();
    assert!(
        model.contains("#[shard_key = \"org_id\"]"),
        "model must use explicitly supplied --shard-key:\n{model}"
    );
}

#[test]
fn generate_sharded_scaffold_rejects_bogus_shard_key() {
    let (_tmp, project) = fresh_project("sharded-bogus-app");

    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Widget",
            "label:String",
            "--sharded",
            "--shard-key",
            "bogus",
        ],
    );
    assert_eq!(
        code,
        Some(1),
        "--shard-key with non-existent field must fail"
    );
    assert!(
        stderr.contains("bogus"),
        "error must mention the invalid field name; got: {stderr}"
    );
}

/// Slow end-to-end check: scaffold a sharded project, patch Cargo.toml to the
/// local autumn-web, and `cargo check --tests` the result. Verifies that
/// `use autumn_web::sharding::ShardedDb` resolves, `#[shard_key]` compiles
/// (requires Track A), and `from_shard` typechecks.
///
/// Requires Track A (`#[shard_key]` in `#[model]` macro) to be merged first.
/// Run with: `cargo test -p autumn-cli -- --ignored generated_sharded_scaffold_cargo_checks`
#[test]
#[ignore = "slow: cargo-checks a fresh sharded project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_sharded_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("sharded-build");

    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Account",
            "shard_id:i64",
            "name:String",
            "--sharded",
        ],
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated sharded scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check: scaffold a `--searchable` (full-text search) project,
/// patch Cargo.toml to the local autumn-web, and `cargo check --tests` the
/// result. Verifies that the merged FTS codegen (issue #1319/#1825) compiles:
/// the `#[searchable]` model attributes, the `searchable` repository option,
/// the generated `{Model}SearchQuery` extractor type, and the `search_vector`
/// migration all typecheck against this workspace's `autumn-web`.
///
/// Uses a plain searchable model (no owner scoping). The owner-scoped +
/// searchable combination is covered separately by
/// [`generated_owner_searchable_scaffold_cargo_checks`] (issue #1841).
///
/// Run with: `cargo test -p autumn-cli -- --ignored generated_searchable_scaffold_cargo_checks`
#[test]
#[ignore = "slow: cargo-checks a fresh searchable project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_searchable_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("searchable-build");

    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--searchable",
            "title,body",
        ],
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated searchable scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Issue #1841: the critical proof that lifting the owner-scoped `--searchable`
/// rejection produces a COMPILING app whose search/index are safely owner-scoped.
///
/// Scaffolds `Post title:String body:Text author_id:i64 --searchable` — an owner
/// column (`author_id`) AND full-text search, the exact combination the
/// generator REJECTED before #1841. The repository macro now emits owner-filtered
/// `list_scoped` / `search_page_scoped`, and the generated owner-scoped index +
/// `/search` handlers call ONLY those scoped methods. `cargo check --tests`
/// proves the whole owner-scoped FTS codegen (the macro's three-phase
/// owner-filtered `search_page_scoped`, `list_scoped`, and the scoped handlers)
/// type-checks against this workspace's `autumn-web`.
///
/// The security invariant — the owner branch never calls the unscoped
/// `repo.search_page`/`repo.page` — is asserted directly on the generated source
/// here and in the `scaffold.rs` unit tests; this test proves it also COMPILES.
///
/// Run with:
/// `cargo test -p autumn-cli --test generate generated_owner_searchable_scaffold_cargo_checks -- --ignored --exact`
#[test]
#[ignore = "slow: cargo-checks a fresh owner-scoped searchable project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_owner_searchable_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("owner-searchable-build");

    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "author_id:i64",
            "--searchable",
            "title,body",
        ],
    );

    // The repository carries `owner = author_id` (→ the macro's scoped codegen).
    let repo = fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();
    assert!(
        repo.contains(", owner = author_id)"),
        "owner-scoped searchable repository must carry `owner = author_id`:\n{repo}"
    );

    // The owner-scoped /search + index call ONLY the scoped methods — never the
    // unscoped `repo.search_page(`/`repo.page(` (the cross-user leak guard).
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("repo.search_page_scoped(owner_id, q, &page_req)")
            && routes.contains("repo.list_scoped(owner_id, &list_query, &page_req)"),
        "owner-scoped search/index must call the scoped repository methods:\n{routes}"
    );
    assert!(
        !routes.contains("repo.search_page(") && !routes.contains("repo.page("),
        "SECURITY: owner-scoped searchable routes must NEVER call the unscoped \
         repo.search_page(/repo.page( (cross-user leak):\n{routes}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated owner-scoped searchable scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Issue #1841 companion proving the *nullable* owner-column + searchable path
/// compiles: a `user:references?` owner is `Option<i64>`, so the macro's scoped
/// `.eq(owner_id)` filter (typed hydration) and the raw-SQL owner bind must
/// compile against a `Nullable<BigInt>` column. A nullable owner never matches
/// NULL (unowned) rows — semantically correct (unowned rows stay hidden).
///
/// Run with:
/// `cargo test -p autumn-cli --test generate generated_nullable_owner_searchable_scaffold_cargo_checks -- --ignored --exact`
#[test]
#[ignore = "slow: cargo-checks a fresh nullable-owner searchable project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_nullable_owner_searchable_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("nullable-owner-searchable-build");

    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Note",
            "title:String",
            "body:Text",
            "user:references?",
            "--searchable",
            "title,body",
        ],
    );

    let repo = fs::read_to_string(project.join("src/repositories/note.rs")).unwrap();
    assert!(
        repo.contains(", owner = user_id)"),
        "nullable-owner searchable repository must carry `owner = user_id`:\n{repo}"
    );
    let routes = fs::read_to_string(project.join("src/routes/notes.rs")).unwrap();
    assert!(
        !routes.contains("repo.search_page(") && !routes.contains("repo.page("),
        "SECURITY: nullable-owner searchable routes must NEVER call the unscoped \
         repo.search_page(/repo.page(:\n{routes}"
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated nullable-owner searchable scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check: scaffold a `--live` project, patch Cargo.toml to the
/// local autumn-web, and `cargo check --tests` the result. Regression coverage
/// for issue #1853: `--live` sets `broadcasts = true` on the `#[repository]`,
/// which makes the macro synthesize internal hooks whose generated `update`
/// body expands an unqualified `{Pascal}DraftExt::from_patch(...)`. The
/// generated repository file must import `{Pascal}DraftExt` alongside the other
/// model types, or the scaffold fails to compile with
/// `error[E0405]: cannot find trait PostDraftExt in this scope`.
///
/// Run with: `cargo test -p autumn-cli -- --ignored generated_live_scaffold_cargo_checks`
#[test]
#[ignore = "slow: cargo-checks a fresh live scaffold — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_live_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("live-build");

    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--live",
        ],
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated live scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Slow end-to-end check: scaffold a `--soft-delete` project, patch Cargo.toml
/// to the local autumn-web, and `cargo check --tests` the result. Regression
/// coverage for a bug where the generated model's `deleted_at` field lacked
/// `#[default]` (so `NewX`/`UpdateX` required it, but no handler populated
/// it) and was declared in the wrong position relative to `created_at`
/// (mismatching the migration/schema.rs column order the `#[repository]`
/// macro's positional insert-`RETURNING` query relies on).
///
/// Run with: `cargo test -p autumn-cli -- --ignored generated_soft_delete_scaffold_cargo_checks`
#[test]
#[ignore = "slow: cargo-checks a fresh soft-delete scaffold — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_soft_delete_scaffold_cargo_checks() {
    let (_tmp, project) = fresh_project("soft-delete-build");

    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--soft-delete",
        ],
    );

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated soft-delete scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

// ── UUID primary-key option (issue #1400) ──────────────────────────────────

/// AC1 + AC4: `--id uuid` generates UUID PK in model and migration;
/// default (no flag) still generates BIGSERIAL/i64 byte-for-byte.
#[test]
fn generate_model_uuid_id() {
    let (_tmp, project) = fresh_project("uuid-model-app");

    run_autumn(
        &project,
        &["generate", "model", "Post", "title:String", "--id", "uuid"],
    );

    // AC1: model struct uses uuid::Uuid
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("pub id: uuid::Uuid,"),
        "model should have `pub id: uuid::Uuid`; got:\n{model}"
    );
    assert!(
        !model.contains("pub id: i64,"),
        "model must not have i64 id with --id uuid; got:\n{model}"
    );

    // AC1: schema.rs uses Uuid type token
    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(
        schema.contains("id -> Uuid,"),
        "schema should have `id -> Uuid`; got:\n{schema}"
    );
    assert!(
        !schema.contains("id -> Int8,"),
        "schema must not have Int8 with --id uuid; got:\n{schema}"
    );

    // AC1: migration uses UUID PRIMARY KEY DEFAULT gen_random_uuid()
    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_posts"))
        .collect();
    assert_eq!(migrations.len(), 1, "expected 1 migration directory");
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("id UUID PRIMARY KEY DEFAULT gen_random_uuid()"),
        "migration should have UUID PK; got:\n{up}"
    );
    assert!(
        !up.contains("BIGSERIAL"),
        "migration must not have BIGSERIAL with --id uuid; got:\n{up}"
    );
    // migration comment about UUIDv7 trade-off should be present
    assert!(
        up.contains("gen_random_uuid()"),
        "migration should include UUID default; got:\n{up}"
    );
}

/// AC4: default (no `--id`) still generates BIGSERIAL and i64.
#[test]
fn generate_model_default_id_is_bigserial() {
    let (_tmp, project) = fresh_project("default-id-app");

    run_autumn(&project, &["generate", "model", "Post", "title:String"]);

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("pub id: i64,"),
        "default model should have `pub id: i64`; got:\n{model}"
    );

    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(
        schema.contains("id -> Int8,"),
        "default schema should have `id -> Int8`; got:\n{schema}"
    );

    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_posts"))
        .collect();
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("id BIGSERIAL PRIMARY KEY"),
        "default migration should have BIGSERIAL; got:\n{up}"
    );
    assert!(
        !up.contains("UUID"),
        "default migration must not have UUID; got:\n{up}"
    );
}

/// `--id uuid` is gated for `generate scaffold`: the generated `#[repository]`
/// REST API is hard-coded to i64 primary keys, so a UUID-keyed scaffold would
/// not compile. The command must reject it up-front with a clear, actionable
/// error and write nothing — pointing users to `generate model --id uuid`.
#[test]
fn generate_scaffold_uuid_id_is_rejected() {
    let (_tmp, project) = fresh_project("uuid-scaffold-app");

    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--id",
            "uuid",
        ],
    );
    assert_eq!(code, Some(1), "scaffold --id uuid must fail with exit 1");
    assert!(
        stderr.contains("not yet supported") && stderr.contains("generate model"),
        "error must explain the limitation and point to `generate model`; got:\n{stderr}"
    );

    // Nothing should have been written.
    assert!(
        !project.join("src/models/post.rs").exists(),
        "rejected scaffold must not write a model file"
    );
    assert!(
        !project.join("src/repositories/post.rs").exists(),
        "rejected scaffold must not write a repository file"
    );
}

/// AC7: `--id` with an unknown value exits non-zero and lists accepted values.
#[test]
fn generate_model_bad_id_type_errors() {
    let (_tmp, project) = fresh_project("bad-id-app");

    let (_, stderr, code) =
        run_autumn_failing(&project, &["generate", "model", "Post", "--id", "guid"]);
    assert_eq!(code, Some(1), "--id guid must fail with exit 1");
    assert!(
        stderr.contains("guid") && stderr.contains("uuid") && stderr.contains("bigint"),
        "error must list the bad value and accepted values; got: {stderr}"
    );
}

/// AC7: scaffold `--id` with an unknown value exits non-zero.
#[test]
fn generate_scaffold_bad_id_type_errors() {
    let (_tmp, project) = fresh_project("bad-id-scaffold-app");

    let (_, stderr, code) = run_autumn_failing(
        &project,
        &["generate", "scaffold", "Post", "--id", "serial4"],
    );
    assert_eq!(code, Some(1), "--id serial4 must fail with exit 1");
    assert!(
        stderr.contains("serial4") && stderr.contains("uuid") && stderr.contains("bigint"),
        "error must list the bad value and accepted values; got: {stderr}"
    );
}

/// AC6 (Finding #3 fix): `[generate] id` propagates via `--config` even when
/// there is no `[scaffold.Post]` section. Since the resolved type is `uuid`,
/// the scaffold gate then rejects it — proving the project default reaches the
/// scaffold path (rather than being silently dropped or erroring on the missing
/// section).
#[test]
fn generate_scaffold_project_default_uuid_config_only_is_rejected() {
    let (_tmp, project) = fresh_project("project-default-uuid-config-app");

    // No [scaffold.Post] section — only the project-level [generate] default.
    fs::write(
        project.join("autumn.generate.toml"),
        "[generate]\nid = \"uuid\"\n",
    )
    .unwrap();

    let (_, stderr, code) = run_autumn_failing(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--config",
            "autumn.generate.toml",
        ],
    );
    assert_eq!(
        code,
        Some(1),
        "scaffold resolving to uuid must fail with exit 1"
    );
    assert!(
        stderr.contains("not yet supported") && stderr.contains("generate model"),
        "error must come from the UUID scaffold gate (proving the [generate] \
         default reached the scaffold path); got:\n{stderr}"
    );
}

/// AC6: `[generate] id = "uuid"` in autumn.generate.toml is auto-discovered
/// (no --config flag needed) and applies to both scaffold and model generation.
#[test]
fn generate_model_project_default_uuid_auto_discovered() {
    let (_tmp, project) = fresh_project("project-default-uuid-auto-app");

    // Write the project-level default without --config; the generator should
    // discover autumn.generate.toml automatically.
    fs::write(
        project.join("autumn.generate.toml"),
        "[generate]\nid = \"uuid\"\n",
    )
    .unwrap();

    // generate model (no --id flag, no --config)
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("pub id: uuid::Uuid,"),
        "auto-discovered [generate] id=uuid should emit uuid::Uuid; got:\n{model}"
    );

    let migrations: Vec<_> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with("_create_posts"))
        .collect();
    let up = fs::read_to_string(migrations[0].path().join("up.sql")).unwrap();
    assert!(
        up.contains("id UUID PRIMARY KEY DEFAULT gen_random_uuid()"),
        "auto-discovered project default migration should have UUID PK; got:\n{up}"
    );
}

/// Codex P2: a defaults-only read (`generate model`, no --config) must parse
/// only `[generate]` and ignore `[scaffold.*]` recipes — so an unrelated
/// checked-in recipe with a typo'd/unsupported key does not break it.
#[test]
fn generate_model_tolerates_malformed_scaffold_recipe_in_config() {
    let (_tmp, project) = fresh_project("malformed-recipe-model-app");

    // [scaffold.Other] uses `index` (unsupported; the key is `indexes`). A full
    // parse would reject it, but `generate model` only reads [generate].
    fs::write(
        project.join("autumn.generate.toml"),
        "[generate]\nid = \"bigint\"\n\n[scaffold.Other]\nfields = [\"x:String\"]\nindex = [\"x\"]\n",
    )
    .unwrap();

    // Must succeed despite the malformed [scaffold.Other] recipe.
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("pub id: i64,"),
        "[generate] id=bigint should produce an i64 PK; got:\n{model}"
    );
}

/// AC6 + scaffold UUID gate: an auto-discovered `[generate] id = "uuid"` flows
/// into `generate scaffold`, where it is rejected (UUID scaffolds are gated).
/// The project-wide default must not silently produce a broken scaffold.
#[test]
fn generate_scaffold_project_default_uuid_is_rejected() {
    let (_tmp, project) = fresh_project("project-default-uuid-scaffold-auto-app");

    fs::write(
        project.join("autumn.generate.toml"),
        "[generate]\nid = \"uuid\"\n",
    )
    .unwrap();

    // generate scaffold (no --id flag, no --config) — auto-discovers the default.
    let (_, stderr, code) =
        run_autumn_failing(&project, &["generate", "scaffold", "Post", "title:String"]);
    assert_eq!(
        code,
        Some(1),
        "scaffold resolving to uuid must fail with exit 1"
    );
    assert!(
        stderr.contains("not yet supported") && stderr.contains("generate model"),
        "error must explain the limitation and point to `generate model`; got:\n{stderr}"
    );
    assert!(
        !project.join("src/models/post.rs").exists(),
        "rejected scaffold must not write a model file"
    );
}

/// Codex P2: an auto-discovered `autumn.generate.toml` must contribute ONLY the
/// project-level `[generate]` defaults — a checked-in `[scaffold.Post]` recipe
/// must NOT silently apply to an ordinary `generate scaffold` run without
/// `--config`. Here the recipe sets `api = true`; without --config the scaffold
/// must remain a full HTML scaffold (i.e. the recipe is ignored).
#[test]
fn generate_scaffold_auto_discovery_ignores_per_resource_recipe() {
    let (_tmp, project) = fresh_project("auto-discovery-recipe-app");

    // A checked-in per-resource recipe with api = true (TOML-only option).
    fs::write(
        project.join("autumn.generate.toml"),
        "[scaffold.Post]\nfields = [\"name:String\"]\napi = true\n",
    )
    .unwrap();

    // No --config: the [scaffold.Post] recipe must be ignored.
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);

    // A full (non-api) scaffold generates the HTML routes file; an api scaffold
    // does not. Its presence proves api = true was NOT inherited.
    assert!(
        project.join("src/routes/posts.rs").is_file(),
        "auto-discovery must not apply the recipe's api=true (HTML routes expected)"
    );
    // CLI fields win; the recipe's `name` field must not appear.
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("pub title: String,") && !model.contains("pub name: String,"),
        "CLI fields must be used, not the recipe's fields; got:\n{model}"
    );
}

// ── autumn generate tauri ─────────────────────────────────────────────────────

#[test]
fn generate_tauri_scaffolds_expected_files() {
    let (_tmp, project) = fresh_project("tauri-scaffold-app");
    run_autumn(&project, &["generate", "tauri"]);

    // Core Tauri project files must exist
    assert!(
        project.join("src-tauri/tauri.conf.json").is_file(),
        "src-tauri/tauri.conf.json must be created"
    );
    assert!(
        project.join("src-tauri/Cargo.toml").is_file(),
        "src-tauri/Cargo.toml must be created"
    );
    assert!(
        project.join("src-tauri/build.rs").is_file(),
        "src-tauri/build.rs must be created"
    );
    assert!(
        project.join("src-tauri/src/main.rs").is_file(),
        "src-tauri/src/main.rs must be created"
    );
    assert!(
        project.join("src-tauri/src/lib.rs").is_file(),
        "src-tauri/src/lib.rs must be created"
    );

    // Platform-specific Tauri config overlays (beforeBuildCommand/beforeDevCommand)
    assert!(
        project.join("src-tauri/tauri.linux.conf.json").is_file(),
        "tauri.linux.conf.json must be created"
    );
    assert!(
        project.join("src-tauri/tauri.macos.conf.json").is_file(),
        "tauri.macos.conf.json must be created"
    );
    assert!(
        project.join("src-tauri/tauri.windows.conf.json").is_file(),
        "tauri.windows.conf.json must be created"
    );

    // Staging scripts
    assert!(
        project.join("src-tauri/stage-sidecar.sh").is_file(),
        "stage-sidecar.sh must be created"
    );
    assert!(
        project.join("src-tauri/stage-sidecar.ps1").is_file(),
        "stage-sidecar.ps1 must be created"
    );

    // Icons
    assert!(
        project.join("src-tauri/icons/icon.svg").is_file(),
        "icons/icon.svg must be created"
    );
    for name in &["32x32.png", "128x128.png", "128x128@2x.png", "icon.png"] {
        assert!(
            project.join("src-tauri/icons").join(name).is_file(),
            "icons/{name} must be created"
        );
    }
    assert!(
        project.join("src-tauri/icons/icon.ico").is_file(),
        "icons/icon.ico must be created"
    );
    assert!(
        project.join("src-tauri/icons/icon.icns").is_file(),
        "icons/icon.icns must be created"
    );

    // .gitignore
    assert!(
        project.join("src-tauri/.gitignore").is_file(),
        "src-tauri/.gitignore must be created"
    );
}

#[test]
fn generate_tauri_conf_is_valid_json_with_required_fields() {
    let (_tmp, project) = fresh_project("tauri-conf-app");
    run_autumn(&project, &["generate", "tauri"]);

    let conf = fs::read_to_string(project.join("src-tauri/tauri.conf.json")).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&conf).expect("tauri.conf.json must be valid JSON");

    assert!(parsed["identifier"].is_string(), "must have identifier");
    assert!(parsed["productName"].is_string(), "must have productName");
    assert!(
        parsed["bundle"]["externalBin"].is_array(),
        "must have bundle.externalBin"
    );
    assert!(
        !parsed["bundle"]["externalBin"]
            .as_array()
            .unwrap()
            .is_empty(),
        "externalBin must not be empty"
    );
    assert!(parsed["bundle"]["icon"].is_array(), "must have bundle.icon");
    // beforeBuildCommand lives in platform-specific overlay files, not the main conf.
    assert!(
        parsed["build"]["beforeBuildCommand"].is_null(),
        "beforeBuildCommand must be absent from tauri.conf.json (lives in platform overlays)"
    );
    // The externalBin must reference the app name
    let bins = parsed["bundle"]["externalBin"].as_array().unwrap();
    assert!(
        bins.iter()
            .any(|b| b.as_str().unwrap_or("").contains("tauri-conf-app")),
        "externalBin must reference the app package name"
    );
}

#[test]
fn generate_tauri_shell_cargo_toml_has_own_workspace() {
    let (_tmp, project) = fresh_project("tauri-ws-app");
    run_autumn(&project, &["generate", "tauri"]);

    let cargo = fs::read_to_string(project.join("src-tauri/Cargo.toml")).unwrap();
    assert!(
        cargo.contains("[workspace]"),
        "src-tauri/Cargo.toml must have its own [workspace] so it is independent"
    );
    assert!(cargo.contains("tauri"), "must depend on tauri");
    assert!(
        cargo.contains("tauri-plugin-shell"),
        "must depend on tauri-plugin-shell"
    );
}

#[test]
fn generate_tauri_lib_rs_has_sidecar_lifecycle() {
    let (_tmp, project) = fresh_project("tauri-lifecycle-app");
    run_autumn(&project, &["generate", "tauri"]);

    let lib = fs::read_to_string(project.join("src-tauri/src/lib.rs")).unwrap();
    assert!(
        lib.contains("127.0.0.1:0"),
        "must bind ephemeral loopback port"
    );
    assert!(
        lib.contains("AUTUMN_SERVER__PORT"),
        "must pass port env to sidecar"
    );
    assert!(
        lib.contains("AUTUMN_MANAGED_PG_DATA_DIR"),
        "must pass DB dir env (#1119)"
    );
    assert!(lib.contains(".sidecar("), "must spawn sidecar");
    assert!(lib.contains("/health"), "must poll /health for readiness");
    assert!(lib.contains(".kill()"), "must kill sidecar on window close");
    assert!(
        lib.contains("Destroyed"),
        "must handle WindowEvent::Destroyed"
    );
}

#[test]
fn generate_tauri_prints_prerequisites() {
    let (_tmp, project) = fresh_project("tauri-prereq-app");
    let (stdout, _stderr) = run_autumn(&project, &["generate", "tauri"]);
    assert!(
        stdout.contains("tauri-cli") || stdout.contains("cargo tauri"),
        "must print Tauri CLI prerequisite; got:\n{stdout}"
    );
    assert!(
        stdout.contains("embed-assets"),
        "must mention embed-assets (#1004); got:\n{stdout}"
    );
    assert!(
        stdout.contains("managed-pg"),
        "must mention managed-pg (#1119); got:\n{stdout}"
    );
}

#[test]
fn generate_tauri_is_additive_no_app_files_modified() {
    let (_tmp, project) = fresh_project("tauri-additive-app");
    let original_main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    let original_cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();

    run_autumn(&project, &["generate", "tauri"]);

    assert_eq!(
        original_main,
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        "src/main.rs must be unchanged after generate tauri"
    );
    assert_eq!(
        original_cargo,
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        "root Cargo.toml must be unchanged after generate tauri"
    );
}

#[test]
fn generate_tauri_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("tauri-dry-run-app");
    let (stdout, _stderr) = run_autumn(&project, &["generate", "tauri", "--dry-run"]);
    assert!(
        !project.join("src-tauri").exists(),
        "dry-run must not create src-tauri/; got stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("tauri.conf.json") || stdout.contains("Would create"),
        "dry-run must print the file plan; got:\n{stdout}"
    );
}

#[test]
fn generate_tauri_collision_without_force_exits_nonzero() {
    let (_tmp, project) = fresh_project("tauri-collision-app");
    run_autumn(&project, &["generate", "tauri"]);

    let (_, stderr, code) = run_autumn_failing(&project, &["generate", "tauri"]);
    assert_eq!(code, Some(1), "re-running without --force must exit 1");
    assert!(
        stderr.contains("would overwrite") || stderr.contains("already exists"),
        "must explain collision; got stderr:\n{stderr}"
    );
}

#[test]
fn generate_tauri_force_is_idempotent() {
    let (_tmp, project) = fresh_project("tauri-force-app");
    run_autumn(&project, &["generate", "tauri"]);
    run_autumn(&project, &["generate", "tauri", "--force"]);

    // After --force, the JSON must still be valid
    let conf = fs::read_to_string(project.join("src-tauri/tauri.conf.json")).unwrap();
    let _: serde_json::Value =
        serde_json::from_str(&conf).expect("tauri.conf.json must still be valid after --force");
}

#[test]
fn generate_tauri_reuses_pwa_icon() {
    let (_tmp, project) = fresh_project("tauri-pwa-icon-app");
    // Simulate PWA generator having run
    run_autumn(&project, &["generate", "pwa"]);

    // Read the PWA icon before running the Tauri generator
    let pwa_icon =
        fs::read_to_string(project.join("static/icons/icon.svg")).expect("PWA icon must exist");

    run_autumn(&project, &["generate", "tauri"]);

    // The Tauri icon.svg must contain the same content as the PWA icon
    let tauri_icon = fs::read_to_string(project.join("src-tauri/icons/icon.svg"))
        .expect("src-tauri/icons/icon.svg must be created");
    assert_eq!(
        pwa_icon, tauri_icon,
        "src-tauri/icons/icon.svg must contain the same content as the PWA icon"
    );
    // Original PWA icon must be untouched
    assert!(
        project.join("static/icons/icon.svg").is_file(),
        "PWA icon must still exist"
    );
}

// ── generate controller (issue #1050) ──────────────────────────────────────

/// Re-running `generate controller` against an existing controller must fail
/// (non-zero exit) and leave the first file byte-for-byte intact. Fast — no
/// compile needed.
#[test]
fn controller_rerun_without_force_fails() {
    let (_tmp, project) = fresh_project("controller-rerun-app");
    run_autumn(
        &project,
        &["generate", "controller", "pages", "home", "about"],
    );
    let original = fs::read_to_string(project.join("src/routes/pages.rs")).unwrap();

    let (_out, _err, code) = run_autumn_failing(
        &project,
        &["generate", "controller", "pages", "home", "contact"],
    );
    assert_eq!(code, Some(1), "second run must exit non-zero");
    let after = fs::read_to_string(project.join("src/routes/pages.rs")).unwrap();
    assert_eq!(
        original, after,
        "existing controller file must be untouched"
    );
}

/// Slow end-to-end check: a fresh project + `generate controller` (HTML) must
/// compile with zero edits, and `autumn routes` must list every generated
/// route.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-builds a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn controller_generates_compiles_and_lists_routes() {
    let (_tmp, project) = fresh_project("controller-html-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &[
            "generate",
            "controller",
            "pages",
            "home",
            "about",
            "contact",
        ],
    );

    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(&project)
        .output()
        .expect("failed to run cargo build");
    assert!(
        build.status.success(),
        "cargo build failed on generated controller:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let (stdout, _stderr) = run_autumn(&project, &["routes"]);
    for path in ["/pages/home", "/pages/about", "/pages/contact"] {
        assert!(
            stdout.contains(path),
            "autumn routes must list {path}:\n{stdout}"
        );
    }

    // --force regeneration with a CHANGED action set must prune the stale
    // route entries (`about`) from main.rs, not just append the new one
    // (`services`) — otherwise `routes::pages::about` would reference a
    // handler the overwritten file no longer defines and break the build.
    run_autumn(
        &project,
        &[
            "generate",
            "controller",
            "pages",
            "home",
            "contact",
            "services",
            "--force",
        ],
    );

    let rebuild = Command::new("cargo")
        .args(["build"])
        .current_dir(&project)
        .output()
        .expect("failed to run cargo build after --force regen");
    assert!(
        rebuild.status.success(),
        "cargo build failed after --force regen (stale route entry not pruned?):\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rebuild.stdout),
        String::from_utf8_lossy(&rebuild.stderr),
    );

    let (stdout2, _stderr2) = run_autumn(&project, &["routes"]);
    for path in ["/pages/home", "/pages/contact", "/pages/services"] {
        assert!(
            stdout2.contains(path),
            "autumn routes must list {path} after regen:\n{stdout2}"
        );
    }
    assert!(
        !stdout2.contains("/pages/about"),
        "the dropped action /pages/about must no longer be listed:\n{stdout2}"
    );
}

/// Slow end-to-end check: `generate controller --api` must compile and list its
/// JSON routes under `/api/<controller>`.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-builds a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn controller_api_generates_json() {
    let (_tmp, project) = fresh_project("controller-api-build");
    patch_generated_cargo_toml(&project);

    run_autumn(
        &project,
        &["generate", "controller", "pages", "index", "stats", "--api"],
    );

    let file = fs::read_to_string(project.join("src/routes/pages.rs")).unwrap();
    assert!(
        file.contains("AutumnResult<Json<serde_json::Value>>"),
        "--api controller must return JSON:\n{file}"
    );

    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(&project)
        .output()
        .expect("failed to run cargo build");
    assert!(
        build.status.success(),
        "cargo build failed on generated --api controller:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let (stdout, _stderr) = run_autumn(&project, &["routes"]);
    for path in ["/api/pages", "/api/pages/stats"] {
        assert!(
            stdout.contains(path),
            "autumn routes must list {path}:\n{stdout}"
        );
    }
}

// ── `autumn plugin add` / `autumn plugin list` (issue #1606) ────────────────

/// Every first-party plugin the install catalog ships, in `plugin list` order.
const FIRST_PARTY_PLUGINS: [&str; 5] = [
    "autumn-admin-plugin",
    "autumn-cache-redis",
    "autumn-media-plugin",
    "autumn-search",
    "autumn-storage-s3",
];

/// Repoint every first-party plugin crate — and `autumn-web` itself — at this
/// workspace, so a generated project resolves the versions under test rather
/// than whatever is published on crates.io.
fn patch_generated_cargo_toml_for_plugins(project_dir: &Path) {
    let cargo_toml_path = project_dir.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    content.push_str("\n[patch.crates-io]\n");
    for crate_name in std::iter::once("autumn-web").chain(FIRST_PARTY_PLUGINS) {
        // `autumn-web` lives in `autumn/`; the plugin crates are named after
        // their own directories.
        let dir = if crate_name == "autumn-web" {
            "autumn"
        } else {
            crate_name
        };
        writeln!(
            content,
            "{crate_name} = {{ path = \"{}\" }}",
            workspace_root
                .join(dir)
                .display()
                .to_string()
                .replace('\\', "/")
        )
        .unwrap();
    }
    fs::write(&cargo_toml_path, content).unwrap();
}

/// AC #1: the listing names every first-party plugin, with a description and
/// the version compatible with the app's `autumn-web`.
#[test]
fn plugin_list_shows_every_first_party_plugin() {
    let (_tmp, project) = fresh_project("plugin-list");
    let (stdout, _) = run_autumn(&project, &["plugin", "list", "--offline"]);
    for plugin in FIRST_PARTY_PLUGINS {
        assert!(stdout.contains(plugin), "{plugin} missing from:\n{stdout}");
    }
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
    assert!(stdout.contains("autumn plugin add"), "{stdout}");
}

/// The same listing, machine-readable.
#[test]
fn plugin_list_json_is_parseable() {
    let (_tmp, project) = fresh_project("plugin-list-json");
    let (stdout, _) = run_autumn(&project, &["plugin", "list", "--json", "--offline"]);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let plugins = value["plugins"].as_array().expect("plugins array");
    for plugin in FIRST_PARTY_PLUGINS {
        assert!(
            plugins.iter().any(|p| p["name"] == plugin),
            "{plugin} missing from {stdout}"
        );
    }
}

/// AC #2 (edits) + AC #4 (idempotency), without paying for a compile.
#[test]
fn plugin_add_writes_the_dependency_and_the_mount_then_is_idempotent() {
    let (_tmp, project) = fresh_project("plugin-add");
    let (stdout, _) = run_autumn(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    assert!(stdout.contains("Installed autumn-admin-plugin"), "{stdout}");
    assert!(stdout.contains("autumn generate admin"), "{stdout}");

    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("autumn-admin-plugin ="), "{cargo}");
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains(".plugin(autumn_admin_plugin::AdminPlugin::new())"),
        "{main_rs}"
    );

    let (stdout, _) = run_autumn(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    assert!(stdout.contains("already installed"), "{stdout}");
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo
    );
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_rs
    );
}

/// AC #2: `--dry-run` reports the same edits without applying any of them.
#[test]
fn plugin_add_dry_run_changes_nothing() {
    let (_tmp, project) = fresh_project("plugin-add-dry");
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let main_before = fs::read_to_string(project.join("src/main.rs")).unwrap();
    run_autumn(
        &project,
        &[
            "plugin",
            "add",
            "autumn-cache-redis",
            "--dry-run",
            "--offline",
        ],
    );
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before
    );
}

/// AC #3: an incompatible `autumn-web` fails before any file is modified, and
/// the diagnostic names both versions.
#[test]
fn plugin_add_refuses_an_incompatible_autumn_web_without_editing() {
    let (_tmp, project) = fresh_project("plugin-add-incompat");
    let cargo_path = project.join("Cargo.toml");
    let cargo = fs::read_to_string(&cargo_path).unwrap().replace(
        &format!("autumn-web = \"{}\"", env!("CARGO_PKG_VERSION")),
        "autumn-web = \"0.1.0\"",
    );
    fs::write(&cargo_path, &cargo).unwrap();
    let main_before = fs::read_to_string(project.join("src/main.rs")).unwrap();

    let (_stdout, stderr, code) = run_autumn_failing(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("0.1.0"), "{stderr}");
    assert!(stderr.contains(env!("CARGO_PKG_VERSION")), "{stderr}");

    assert_eq!(fs::read_to_string(&cargo_path).unwrap(), cargo);
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before
    );
}

/// AC #5: a `main.rs` whose builder chain cannot be found is left completely
/// alone, and the command prints what to apply by hand.
#[test]
fn plugin_add_degrades_on_a_customized_main() {
    let (_tmp, project) = fresh_project("plugin-add-custom");
    let main_path = project.join("src/main.rs");
    let custom = "#[autumn_web::main]\nasync fn main() {\n    my_bootstrap().await;\n}\n";
    fs::write(&main_path, custom).unwrap();
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();

    // A refusal, not a result: it goes to stderr and exits 2 so a script
    // cannot read "I changed nothing" as a successful install.
    let (_stdout, stderr, code) = run_autumn_failing(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("No files were changed"), "{stderr}");
    assert!(stderr.contains("autumn-admin-plugin = \""), "{stderr}");
    assert!(stderr.contains("AdminPlugin::new()"), "{stderr}");

    assert_eq!(fs::read_to_string(&main_path).unwrap(), custom);
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
}

/// AC #5, the shape that used to slip through: a `main.rs` that factors its
/// builder into a helper. Splicing there mounts the plugin into a function the
/// binary never calls — and for `autumn-storage-s3`, whose mount awaits, into a
/// synchronous fn, which does not compile.
#[test]
fn plugin_add_degrades_when_the_builder_lives_in_a_helper() {
    let (_tmp, project) = fresh_project("plugin-add-helper");
    let main_path = project.join("src/main.rs");
    let custom = "#[autumn_web::main]\nasync fn main() {\n    build_app().run().await;\n}\n\n\
                  fn build_app() -> autumn_web::app::AppBuilder {\n    autumn_web::app()\n        \
                  .routes(routes![index])\n}\n";
    fs::write(&main_path, custom).unwrap();

    let (_stdout, stderr, code) = run_autumn_failing(
        &project,
        &["plugin", "add", "autumn-storage-s3", "--offline"],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert_eq!(fs::read_to_string(&main_path).unwrap(), custom);
}

/// An unknown name is refused with a pointer at `plugin list`.
#[test]
fn plugin_add_rejects_an_unknown_crate() {
    let (_tmp, project) = fresh_project("plugin-add-unknown");
    let (_stdout, stderr, code) =
        run_autumn_failing(&project, &["plugin", "add", "tokio", "--offline"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("autumn plugin list"), "{stderr}");
}

/// The Success Metric for issue #1606: `autumn plugin add` for **every**
/// first-party plugin against a fresh `autumn new` scaffold, each of which
/// must then `cargo check` green — the machine proof that the generated mount
/// compiles on the first try.
///
/// Ignored by default (it compiles five generated projects); run in CI by the
/// `plugin-install` job in `.github/workflows/generator-conformance.yml`. The
/// five projects share one `CARGO_TARGET_DIR` so the framework is built once
/// rather than five times.
#[test]
#[ignore = "compiles generated projects against the local workspace (slow)"]
fn plugin_add_first_party_scaffolds_cargo_check() {
    let shared_target = tempfile::tempdir().expect("shared target dir");
    for plugin in FIRST_PARTY_PLUGINS {
        let (_tmp, project) = fresh_project(&plugin.replace('-', "_"));
        patch_generated_cargo_toml_for_plugins(&project);

        let (stdout, _) = run_autumn(&project, &["plugin", "add", plugin, "--offline"]);
        assert!(stdout.contains(&format!("Installed {plugin}")), "{stdout}");

        // Re-running must be a no-op, so the compile below is of a
        // singly-installed project (AC #4).
        let (stdout, _) = run_autumn(&project, &["plugin", "add", plugin, "--offline"]);
        assert!(stdout.contains("already installed"), "{stdout}");

        let check = Command::new("cargo")
            .args(["check", "--all-targets"])
            .current_dir(&project)
            .env("CARGO_TARGET_DIR", shared_target.path())
            .output()
            .expect("failed to run cargo check");
        assert!(
            check.status.success(),
            "`autumn plugin add {plugin}` produced a project that does not compile:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr),
        );
    }
}

// ── `autumn plugin remove` / `autumn new --with` (issue #1631) ──────────────

/// AC #1 + AC #5: `add` then `remove` returns the app to exactly what it was,
/// and a second `remove` is an idempotent no-op.
#[test]
fn plugin_remove_reverses_both_wires_and_is_idempotent() {
    let (_tmp, project) = fresh_project("plugin-remove");
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let main_before = fs::read_to_string(project.join("src/main.rs")).unwrap();

    run_autumn(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    assert_ne!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before
    );

    let (stdout, _) = run_autumn(&project, &["plugin", "remove", "autumn-admin-plugin"]);
    assert!(stdout.contains("Removed autumn-admin-plugin"), "{stdout}");

    // Byte-identical: the marker comment, the mount, and the dependency line
    // all came back out, and nothing else moved.
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before
    );

    let (stdout, _) = run_autumn(&project, &["plugin", "remove", "autumn-admin-plugin"]);
    assert!(stdout.contains("not installed"), "{stdout}");
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
}

/// AC #3: `--dry-run` writes nothing, and its exit code distinguishes "would
/// change something" (3) from "nothing to do" (0).
#[test]
fn plugin_remove_dry_run_reports_without_writing_and_signals_pending_changes() {
    let (_tmp, project) = fresh_project("plugin-remove-dry");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let main_before = fs::read_to_string(project.join("src/main.rs")).unwrap();

    let (stdout, stderr, code) = run_autumn_failing(
        &project,
        &["plugin", "remove", "autumn-admin-plugin", "--dry-run"],
    );
    assert_eq!(code, Some(3), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Dry run"), "{stdout}");
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before
    );

    // Nothing to do is a plain success, so a script can branch on the code.
    let (stdout, stderr, code) = run_autumn_failing(
        &project,
        &["plugin", "remove", "autumn-search", "--dry-run"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
}

/// AC #2: removal never touches the database, and says exactly what it left
/// behind plus the flag that would remove it.
#[test]
fn plugin_remove_lists_the_data_it_leaves_in_place() {
    let (_tmp, project) = fresh_project("plugin-remove-data");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-media-plugin", "--offline"],
    );
    let (stdout, _) = run_autumn(&project, &["plugin", "remove", "autumn-media-plugin"]);
    assert!(stdout.contains("media_rooms"), "{stdout}");
    assert!(stdout.contains("20260720000000_media_rooms"), "{stdout}");
    assert!(stdout.contains("--drop-data"), "{stdout}");
    assert!(stdout.contains("database was not touched"), "{stdout}");
}

/// AC #4: a dependency added by hand with no mount (the `README` install path)
/// is unwired as far as it goes, and the missing half is reported.
#[test]
fn plugin_remove_handles_a_dependency_with_no_mount() {
    let (_tmp, project) = fresh_project("plugin-remove-partial");
    let cargo_path = project.join("Cargo.toml");
    let cargo = fs::read_to_string(&cargo_path).unwrap();
    fs::write(
        &cargo_path,
        cargo.replace(
            "[dependencies]",
            "[dependencies]\nautumn-admin-plugin = \"0.7.0\"",
        ),
    )
    .unwrap();

    let (stdout, _) = run_autumn(&project, &["plugin", "remove", "autumn-admin-plugin"]);
    assert!(stdout.to_lowercase().contains("could not find"), "{stdout}");
    let cargo_after = fs::read_to_string(&cargo_path).unwrap();
    assert!(
        !cargo_after.contains("autumn-admin-plugin"),
        "{cargo_after}"
    );
}

/// AC #4: a builder chain this command cannot read is left completely alone,
/// and the exact lines to delete are printed. The app never stops compiling.
#[test]
fn plugin_remove_degrades_on_an_unexcisable_mount() {
    let (_tmp, project) = fresh_project("plugin-remove-custom");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    let main_path = project.join("src/main.rs");
    // A mount built into a variable: a real mount whose type this command
    // cannot see inside the `.plugin(...)` call.
    let custom = "#[autumn_web::main]\nasync fn main() {\n    let configured = autumn_admin_plugin::AdminPlugin::new();\n    autumn_web::app()\n        .plugin(configured)\n        .run()\n        .await;\n}\n";
    fs::write(&main_path, custom).unwrap();
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();

    let (_stdout, stderr, code) =
        run_autumn_failing(&project, &["plugin", "remove", "autumn-admin-plugin"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("No files were changed"), "{stderr}");
    assert!(stderr.contains("AdminPlugin"), "{stderr}");

    assert_eq!(fs::read_to_string(&main_path).unwrap(), custom);
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
}

/// A dependency the app still names elsewhere survives the removal, and the
/// report says which file kept it alive.
#[test]
fn plugin_remove_keeps_a_dependency_the_app_still_uses() {
    let (_tmp, project) = fresh_project("plugin-remove-inuse");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-admin-plugin", "--offline"],
    );
    fs::write(
        project.join("src/support.rs"),
        "pub fn panel() -> autumn_admin_plugin::AdminPlugin { todo!() }\n",
    )
    .unwrap();

    let (stdout, _) = run_autumn(&project, &["plugin", "remove", "autumn-admin-plugin"]);
    assert!(stdout.contains("support.rs"), "{stdout}");
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("autumn-admin-plugin"), "{cargo}");
    // The mount still came out — only the dependency was held back.
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(!main_rs.contains("AdminPlugin::new()"), "{main_rs}");
}

/// AC #6: `autumn new --with` scaffolds an app with the plugin already wired.
#[test]
fn new_with_scaffolds_an_app_with_the_plugin_wired() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (stdout, _) = run_autumn(
        tmp.path(),
        &[
            "new",
            "with-app",
            "--with",
            "autumn-admin-plugin",
            "--with",
            "autumn-search",
        ],
    );
    assert!(stdout.contains("Installed autumn-admin-plugin"), "{stdout}");
    assert!(stdout.contains("Installed autumn-search"), "{stdout}");

    let project = tmp.path().join("with-app");
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("autumn-admin-plugin ="), "{cargo}");
    assert!(cargo.contains("autumn-search ="), "{cargo}");
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains(".plugin(autumn_admin_plugin::AdminPlugin::new())"),
        "{main_rs}"
    );
    assert!(
        main_rs.contains(".plugin(autumn_search::SearchPlugin::new())"),
        "{main_rs}"
    );
}

/// AC #6: name resolution and version compatibility are checked BEFORE any
/// file is written — an unknown plugin leaves no project behind at all.
#[test]
fn new_with_rejects_an_unknown_plugin_before_scaffolding() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_stdout, stderr, code) =
        run_autumn_failing(tmp.path(), &["new", "doomed-app", "--with", "tokio"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("autumn plugin list"), "{stderr}");
    assert!(
        !tmp.path().join("doomed-app").exists(),
        "a rejected --with must not leave a half-scaffolded project"
    );
}

/// AC #7: `autumn doctor` reports orphaned plugin residue under the existing
/// `--json` contract.
#[test]
fn doctor_reports_a_dependency_with_no_mount_as_residue() {
    let (_tmp, project) = fresh_project("doctor-residue");
    let cargo_path = project.join("Cargo.toml");
    let cargo = fs::read_to_string(&cargo_path).unwrap();
    fs::write(
        &cargo_path,
        cargo.replace(
            "[dependencies]",
            "[dependencies]\nautumn-admin-plugin = \"0.7.0\"",
        ),
    )
    .unwrap();

    let (stdout, _stderr, _code) = run_autumn_failing(&project, &["doctor", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let check = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "plugin_residue")
        .unwrap_or_else(|| panic!("plugin_residue missing from {stdout}"));
    assert_eq!(check["status"], "warn", "{stdout}");
    assert!(
        check["detail"]
            .as_str()
            .is_some_and(|d| d.contains("autumn-admin-plugin")),
        "{stdout}"
    );
}

/// A clean scaffold has no residue at all — the check must not warn on every
/// project that simply has no plugins.
#[test]
fn doctor_reports_no_residue_for_a_plain_scaffold() {
    let (_tmp, project) = fresh_project("doctor-no-residue");
    let (stdout, _stderr, _code) = run_autumn_failing(&project, &["doctor", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let check = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "plugin_residue")
        .unwrap_or_else(|| panic!("plugin_residue missing from {stdout}"));
    assert_eq!(check["status"], "pass", "{stdout}");
}

/// The Success Metric for issue #1631, for every first-party plugin:
/// `autumn new --with <plugin>` compiles, `autumn plugin remove <plugin>`
/// returns the app to a state that compiles, and `autumn doctor` finds no
/// residue afterwards.
///
/// Ignored by default (it compiles a generated project per plugin); run in CI
/// by the `plugin-install` job in `.github/workflows/generator-conformance.yml`.
/// The projects share one `CARGO_TARGET_DIR` so the framework is built once.
#[test]
#[ignore = "compiles generated projects against the local workspace (slow)"]
fn plugin_new_with_then_remove_round_trips_cargo_check() {
    let shared_target = tempfile::tempdir().expect("shared target dir");
    for plugin in FIRST_PARTY_PLUGINS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = plugin.replace('-', "_");
        run_autumn(tmp.path(), &["new", &name, "--with", plugin]);
        let project = tmp.path().join(&name);
        patch_generated_cargo_toml_for_plugins(&project);

        let cargo_check = |stage: &str| {
            let check = Command::new("cargo")
                .args(["check", "--all-targets"])
                .current_dir(&project)
                .env("CARGO_TARGET_DIR", shared_target.path())
                .output()
                .expect("failed to run cargo check");
            assert!(
                check.status.success(),
                "{plugin} does not compile {stage}:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&check.stdout),
                String::from_utf8_lossy(&check.stderr),
            );
        };
        cargo_check("after `autumn new --with`");

        let (stdout, _) = run_autumn(&project, &["plugin", "remove", plugin]);
        assert!(stdout.contains(&format!("Removed {plugin}")), "{stdout}");
        cargo_check("after `autumn plugin remove`");

        let (stdout, _stderr, _code) = run_autumn_failing(&project, &["doctor", "--json"]);
        let value: serde_json::Value =
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
        let check = value["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|c| c["name"] == "plugin_residue")
            .unwrap_or_else(|| panic!("plugin_residue missing from {stdout}"));
        assert_eq!(check["status"], "pass", "{plugin}: {stdout}");
    }
}

/// AC #2: `--drop-data` never drops without a confirmation, and a
/// non-interactive stdin is a refusal — never an assumed yes. Nothing is
/// changed: not the code, not the database.
#[test]
fn plugin_remove_drop_data_refuses_without_a_confirmation() {
    let (_tmp, project) = fresh_project("plugin-remove-drop-noconfirm");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-media-plugin", "--offline"],
    );
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let main_before = fs::read_to_string(project.join("src/main.rs")).unwrap();

    // A reachable-looking Postgres URL so the command gets as far as the
    // confirmation instead of stopping at "no database configured".
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args(["plugin", "remove", "autumn-media-plugin", "--drop-data"])
        .current_dir(&project)
        .env("DATABASE_URL", "postgres://localhost/definitely-not-here")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to run autumn");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("needs a confirmation"), "{stderr}");
    assert!(stderr.contains("Aborted"), "{stderr}");

    // The confirmation comes BEFORE the edits, so a refusal leaves the app
    // exactly as it was.
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before
    );
}

/// AC #2/#3: `--drop-data --dry-run` prints the exact statements and touches
/// neither the files nor the database.
#[test]
fn plugin_remove_drop_data_dry_run_prints_the_statements_and_writes_nothing() {
    let (_tmp, project) = fresh_project("plugin-remove-drop-dry");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-media-plugin", "--offline"],
    );
    let main_before = fs::read_to_string(project.join("src/main.rs")).unwrap();

    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args([
            "plugin",
            "remove",
            "autumn-media-plugin",
            "--drop-data",
            "--dry-run",
        ])
        .current_dir(&project)
        .env("DATABASE_URL", "postgres://localhost/definitely-not-here")
        .output()
        .expect("failed to run autumn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("DROP TABLE IF EXISTS media_rooms"),
        "{stdout}"
    );
    assert!(stdout.contains("__diesel_schema_migrations"), "{stdout}");
    assert!(stdout.contains("database was not touched"), "{stdout}");
    assert_eq!(
        fs::read_to_string(project.join("src/main.rs")).unwrap(),
        main_before
    );
}

/// AC #2: with no database configured there is nothing to connect to, so the
/// statements are printed instead — and the exit code says so, since a script
/// must not read "printed for you" as "dropped".
#[test]
fn plugin_remove_drop_data_without_a_database_prints_the_statements() {
    let (_tmp, project) = fresh_project("plugin-remove-drop-nodb");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-media-plugin", "--offline"],
    );
    let (_stdout, stderr, code) = run_autumn_failing(
        &project,
        &[
            "plugin",
            "remove",
            "autumn-media-plugin",
            "--drop-data",
            "--yes",
        ],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("DROP TABLE IF EXISTS media_rooms"),
        "{stderr}"
    );
    // Nothing at all was changed — not the database, and not the code either.
    assert!(stderr.contains("database is untouched"), "{stderr}");
    assert!(stderr.contains("still\nwired"), "{stderr}");
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main_rs.contains("MediaPlugin::new()"), "{main_rs}");
}

/// `--drop-data` needs a declared migration/table list, which only first-party
/// plugins carry. A community crate is refused before anything is planned.
#[test]
fn plugin_remove_drop_data_refuses_a_community_crate_before_editing() {
    let (_tmp, project) = fresh_project("plugin-remove-drop-community");
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let (_stdout, stderr, code) = run_autumn_failing(
        &project,
        &[
            "plugin",
            "remove",
            "autumn-plugin-live-feed",
            "--drop-data",
            "--yes",
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("community crate"), "{stderr}");
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
}

/// AC #4: a dependency declared in a shape the manifest rewriter will not
/// touch is left alone, said so, and exits with the "there is still work for
/// you" code rather than a bare success.
#[test]
fn plugin_remove_leaves_an_uneditable_dependency_and_says_so() {
    let (_tmp, project) = fresh_project("plugin-remove-subtable");
    let cargo_path = project.join("Cargo.toml");
    let cargo = fs::read_to_string(&cargo_path).unwrap();
    fs::write(
        &cargo_path,
        format!("{cargo}\n[dependencies.autumn-admin-plugin]\nversion = \"0.7.0\"\n"),
    )
    .unwrap();
    let cargo_before = fs::read_to_string(&cargo_path).unwrap();

    let (stdout, stderr, code) =
        run_autumn_failing(&project, &["plugin", "remove", "autumn-admin-plugin"]);
    assert_eq!(code, Some(2), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Delete it by hand"), "{stdout}");
    // Left byte-identical rather than half-rewritten into something Cargo
    // cannot parse.
    assert_eq!(fs::read_to_string(&cargo_path).unwrap(), cargo_before);
}

/// AC #7: a community `autumn-plugin-*` dependency with no mount is residue
/// too, and gets advice that matches how community plugins actually install.
#[test]
fn doctor_reports_an_unmounted_community_dependency_as_residue() {
    let (_tmp, project) = fresh_project("doctor-residue-community");
    let cargo_path = project.join("Cargo.toml");
    let cargo = fs::read_to_string(&cargo_path).unwrap();
    fs::write(
        &cargo_path,
        cargo.replace(
            "[dependencies]",
            "[dependencies]\nautumn-plugin-live-feed = \"0.3.1\"",
        ),
    )
    .unwrap();

    let (stdout, _stderr, _code) = run_autumn_failing(&project, &["doctor", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let check = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "plugin_residue")
        .unwrap_or_else(|| panic!("plugin_residue missing from {stdout}"));
    assert_eq!(check["status"], "warn", "{stdout}");
    assert!(
        check["detail"]
            .as_str()
            .is_some_and(|d| d.contains("autumn-plugin-live-feed") && d.contains("README")),
        "{stdout}"
    );
}

/// A builder that lives outside `src/main.rs` — the shape `plugin add`'s own
/// manual fallback tells users to write — is a correctly wired app, and must
/// not be warned at (which under `--strict` would fail their CI).
#[test]
fn doctor_finds_no_residue_when_the_builder_lives_outside_main_rs() {
    let (_tmp, project) = fresh_project("doctor-residue-elsewhere");
    let cargo_path = project.join("Cargo.toml");
    let cargo = fs::read_to_string(&cargo_path).unwrap();
    fs::write(
        &cargo_path,
        cargo.replace(
            "[dependencies]",
            "[dependencies]\nautumn-admin-plugin = \"0.7.0\"",
        ),
    )
    .unwrap();
    fs::write(
        project.join("src/app_builder.rs"),
        "pub fn build() {\n    autumn_web::app().plugin(autumn_admin_plugin::AdminPlugin::new());\n}\n",
    )
    .unwrap();

    let (stdout, _stderr, _code) = run_autumn_failing(&project, &["doctor", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let check = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "plugin_residue")
        .unwrap_or_else(|| panic!("plugin_residue missing from {stdout}"));
    assert_eq!(check["status"], "pass", "{stdout}");
}

/// Codex review (AC #6): a `--starter` brings its own `Cargo.toml`, which may
/// pin a different `autumn-web` series than this CLI. That pin is not knowable
/// until the starter is fetched, so the version answer arrives after the app
/// exists — and it must read as "the app was created, the plugin was not
/// wired", not as a bare failure. The starter itself is left complete and
/// untouched.
#[test]
fn new_with_on_an_incompatible_starter_reports_an_unwired_plugin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let starter = tmp.path().join("old-starter");
    fs::create_dir_all(starter.join("src")).unwrap();
    fs::write(
        starter.join("autumn-starter.toml"),
        "[starter]\nname = \"old\"\ndescription = \"pins an older autumn-web\"\n",
    )
    .unwrap();
    fs::write(
        starter.join("Cargo.toml.tmpl"),
        "[package]\nname = \"{{project_name}}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nautumn-web = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        starter.join("src/main.rs"),
        "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .routes(routes![index]);\n    app.run().await;\n}\n",
    )
    .unwrap();

    let (stdout, stderr, code) = run_autumn_failing(
        tmp.path(),
        &[
            "new",
            "starter-app",
            "--starter",
            starter.to_str().unwrap(),
            "--yes",
            "--with",
            "autumn-admin-plugin",
        ],
    );
    assert_eq!(code, Some(2), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("was not wired"), "{stderr}");
    assert!(stderr.contains("autumn plugin add"), "{stderr}");

    // The starter scaffolded completely; only the plugin is absent.
    let project = tmp.path().join("starter-app");
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("autumn-web = \"0.1.0\""), "{cargo}");
    assert!(!cargo.contains("autumn-admin-plugin"), "{cargo}");
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(!main_rs.contains("AdminPlugin"), "{main_rs}");
}

/// An unknown `--with` name is still refused before the starter is fetched:
/// that half of the preflight does not need the starter's manifest.
#[test]
fn new_with_rejects_an_unknown_plugin_before_fetching_a_starter() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_stdout, stderr, code) = run_autumn_failing(
        tmp.path(),
        &[
            "new",
            "doomed-starter-app",
            "--starter",
            "saas",
            "--yes",
            "--with",
            "tokio",
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("autumn plugin list"), "{stderr}");
    assert!(
        !tmp.path().join("doomed-starter-app").exists(),
        "a rejected --with must not leave a scaffolded project"
    );
}

/// Codex review: a mount that cannot be excised means the plugin is still
/// wired. Dropping the tables it is about to read would break a running app —
/// and confirming a destructive step that then silently does nothing is worse.
/// Nothing is asked, and nothing is changed.
#[test]
fn plugin_remove_drop_data_is_not_confirmed_when_the_mount_cannot_be_excised() {
    let (_tmp, project) = fresh_project("plugin-remove-drop-manual");
    run_autumn(
        &project,
        &["plugin", "add", "autumn-media-plugin", "--offline"],
    );
    let main_path = project.join("src/main.rs");
    // A mount built into a variable: real, and not excisable by this command.
    let custom = "#[autumn_web::main]\nasync fn main() {\n    let configured = autumn_media_plugin::MediaPlugin::new();\n    autumn_web::app()\n        .plugin(configured)\n        .run()\n        .await;\n}\n";
    fs::write(&main_path, custom).unwrap();
    let cargo_before = fs::read_to_string(project.join("Cargo.toml")).unwrap();

    let (stdout, stderr, code) = run_autumn_failing(
        &project,
        &[
            "plugin",
            "remove",
            "autumn-media-plugin",
            "--drop-data",
            "--yes",
        ],
    );
    assert_eq!(code, Some(2), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("No files were changed"), "{stderr}");
    assert!(stderr.contains("--drop-data was not applied"), "{stderr}");
    // The SQL must never have been presented as something about to run.
    assert!(
        !stdout.contains("will run these"),
        "a drop must not be announced on this path:\n{stdout}"
    );
    assert_eq!(fs::read_to_string(&main_path).unwrap(), custom);
    assert_eq!(
        fs::read_to_string(project.join("Cargo.toml")).unwrap(),
        cargo_before
    );
}

/// Codex review (AC #3): the plugin is already unwired but its tables remain.
/// No file would move, so the file-level check alone would exit 0 — telling a
/// script the cleanup is finished while a real run still drops data.
#[test]
fn plugin_remove_drop_data_dry_run_exits_three_for_database_only_work() {
    let (_tmp, project) = fresh_project("plugin-remove-drop-dbonly");
    // Never installed: nothing to unwire, but the plugin owns tables.
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args([
            "plugin",
            "remove",
            "autumn-media-plugin",
            "--drop-data",
            "--dry-run",
        ])
        .current_dir(&project)
        .env("DATABASE_URL", "postgres://localhost/definitely-not-here")
        .output()
        .expect("failed to run autumn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("not installed"), "{stdout}");
    assert!(
        stdout.contains("DROP TABLE IF EXISTS media_rooms"),
        "{stdout}"
    );
}

/// Codex review (AC #7): an app whose builder lives in an explicitly-pathed
/// Cargo target is correctly wired. Reporting it as "declared but never
/// mounted" would fail `autumn doctor --strict` on a valid project.
#[test]
fn doctor_finds_no_residue_when_the_builder_lives_in_a_custom_target() {
    let (_tmp, project) = fresh_project("doctor-residue-custom-target");
    let cargo_path = project.join("Cargo.toml");
    let cargo = fs::read_to_string(&cargo_path).unwrap();
    fs::write(
        &cargo_path,
        format!(
            "{}\n[[bin]]\nname = \"server\"\npath = \"cmd/server.rs\"\n",
            cargo.replace(
                "[dependencies]",
                "[dependencies]\nautumn-admin-plugin = \"0.7.0\"",
            )
        ),
    )
    .unwrap();
    fs::create_dir_all(project.join("cmd")).unwrap();
    fs::write(
        project.join("cmd/server.rs"),
        "fn main() {\n    autumn_web::app().plugin(autumn_admin_plugin::AdminPlugin::new());\n}\n",
    )
    .unwrap();

    let (stdout, _stderr, _code) = run_autumn_failing(&project, &["doctor", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let check = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "plugin_residue")
        .unwrap_or_else(|| panic!("plugin_residue missing from {stdout}"));
    assert_eq!(check["status"], "pass", "{stdout}");
}
