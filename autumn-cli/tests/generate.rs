//! End-to-end integration tests for `autumn generate`.
//!
//! These run the real `autumn` binary against a freshly-`new`-ed project and
//! assert the produced filesystem matches the documented contract — covering
//! the user-facing flow described in [Issue #493].
//!
//! [Issue #493]: https://github.com/madmax983/autumn/issues/493

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
        run_autumn_failing(&project, &["generate", "model", "Post", "price:Decimal"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("unsupported type"));
    assert!(stderr.contains("Supported:"));
    assert!(stderr.contains("String"));
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
        "#[get(\"/posts/new\")]",
        "#[post(\"/posts\")]",
        "#[get(\"/posts/{id}/edit\")]",
        "#[post(\"/posts/{id}/update\")]",
        "pub async fn index",
        "pub async fn show",
        "pub async fn new_form(",
        "pub async fn update",
        "use autumn_web::security::{CsrfFormField, CsrfToken};",
        "input type=\"hidden\" name=(csrf_field_name)",
        "(csrf_input(csrf.as_ref(), csrf_field.as_ref()))",
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

    // Smoke test: real, in-process, DB-backed read test (issue #1023).
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(test.contains("posts_api_list_returns_ok_against_a_real_database"));
    assert!(test.contains("autumn_web::test::{TestApp, TestClient, TestDb}"));
    assert!(!test.contains("TcpStream"));
    assert!(!test.contains("AUTUMN_TEST_BASE_URL"));
    assert!(test.contains("#[ignore = \"requires Docker"));
    assert!(test.contains("/api/posts"));

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
    assert!(routes.contains("name=\"url\""));
    assert!(routes.contains("name=\"title\""));
    assert!(routes.contains("name=\"tag\""));
    assert!(!routes.contains("name=\"alive\""));
    assert!(routes.contains("bookmarks::tag.eq(form.tag.clone())"));
    assert!(!routes.contains("bookmarks::alive.eq(form.alive.clone())"));
    assert!(!routes.contains("form.alive"));

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

/// Slow live-HTTP check: scaffold a fresh project, run migrations against a
/// real Postgres testcontainer, boot the generated server, and assert the
/// generated HTML and JSON routes actually respond.
///
/// Ignored by default; requires Docker and `diesel` CLI on PATH. Run with:
/// `cargo test -p autumn-cli --test generate generated_scaffold_serves_posts_index_and_json_api -- --ignored --exact`
#[tokio::test]
#[ignore = "slow: starts Postgres, runs diesel migrations, builds and boots a generated app"]
async fn generated_scaffold_serves_posts_index_and_json_api() {
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

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

    let postgres = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer");
    let host = postgres.get_host().await.expect("postgres host");
    let pg_port = postgres
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");
    let database_url = format!("postgres://postgres:postgres@{host}:{pg_port}/postgres");

    run_autumn_with_env(
        &project,
        &["migrate"],
        &[("AUTUMN_DATABASE__URL", database_url.as_str())],
    );

    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(&project)
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
        .current_dir(&project)
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .env("AUTUMN_DATABASE__URL", &database_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn generated server");

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let _server = wait_for_server_ready_async(child, &client, &base).await;

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
    assert_eq!(body.trim(), "[]", "empty JSON index body");
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
            "subtitle:Option<String>",
            "views:Option<i64>",
            "published_at:Option<NaiveDateTime>",
            "token:Option<Uuid>",
        ],
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
        ],
    );

    // The generated Cargo.toml must include serde.
    let cargo_toml = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo_toml.contains("serde"),
        "Cargo.toml must include serde after generate job"
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
        3,
        "User auth routes should only write user_id as the generated account id key"
    );
    assert!(
        routes.contains("email.split_once('@')"),
        "signup email validation should use split_once"
    );
    assert!(
        !routes.contains("email.find('@').unwrap()"),
        "signup email validation should not search for @ repeatedly"
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
    assert!(
        routes.contains(".page("),
        "scaffold index must call the repository page() method: {routes}"
    );
    assert!(
        !routes.contains(".load(&mut *db)"),
        "scaffold index must not load every row without pagination: {routes}"
    );
    // The repository trait must be imported so `repo.page()` (a trait method)
    // resolves at compile time — without it the generated code fails with E0599.
    assert!(
        routes.contains("PostRepository"),
        "scaffold routes must import the PostRepository trait (needed to call repo.page()): {routes}"
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
        "email_change_reenters_unconfirmed",
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

    // hx-post attr on the title input in the create form
    assert!(
        routes.contains("hx-post=\"/posts/validate/title\""),
        "create form must have hx-post on validated title input:\n{routes}"
    );
    // hx-trigger, hx-target, hx-swap attrs
    assert!(
        routes.contains("hx-trigger=\"change\""),
        "create form must have hx-trigger=\"change\":\n{routes}"
    );
    assert!(
        routes.contains("hx-target=\"#title-error\""),
        "create form must have hx-target pointing at the error slot:\n{routes}"
    );
    assert!(
        routes.contains("hx-swap=\"outerHTML\""),
        "create form must have hx-swap=\"outerHTML\":\n{routes}"
    );
    // error span emitted after the input
    assert!(
        routes.contains("span id=\"title-error\""),
        "create form must have a companion error span:\n{routes}"
    );
    // body field (not validated) must NOT have hx-post
    assert!(
        !routes.contains("hx-post=\"/posts/validate/body\""),
        "unvalidated body input must not have hx-post:\n{routes}"
    );
}

/// `--live-validation` emits a `validate_{field}` route handler that actually
/// checks the declared rule (length, url, email) — not just the empty check.
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

    // length-validated field checks the length bounds
    assert!(
        routes.contains("validate_title"),
        "routes must contain validate_title handler:\n{routes}"
    );
    assert!(
        routes.contains("value.chars().count() < 1 || value.chars().count() > 200"),
        "validate_title must check length bounds:\n{routes}"
    );

    // url-validated field checks with url::Url::parse
    assert!(
        routes.contains("validate_site"),
        "routes must contain validate_site handler:\n{routes}"
    );
    assert!(
        routes.contains("url::Url::parse(&value).is_err()"),
        "validate_site must check with url::Url::parse:\n{routes}"
    );

    // email-validated field checks for @ and domain dot
    assert!(
        routes.contains("validate_email"),
        "routes must contain validate_email handler:\n{routes}"
    );
    assert!(
        routes.contains("!value.contains('@')"),
        "validate_email must check for @ character:\n{routes}"
    );
}

/// `--validate field=length:min=N` (no max) generates the min-only length check.
#[test]
fn live_validation_length_min_only() {
    let (_tmp, project) = fresh_project("lv-min-only");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--validate",
            "title=length:min=3",
            "--live-validation",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("value.chars().count() < 3"),
        "min-only length check must guard count < min:\n{routes}"
    );
    assert!(
        routes.contains("must be at least 3 characters"),
        "min-only error message must say 'at least':\n{routes}"
    );
    assert!(
        !routes.contains("value.chars().count() >"),
        "min-only rule must not emit an upper-bound check:\n{routes}"
    );
}

/// `--validate field=length:max=N` (no min) generates the max-only length check.
#[test]
fn live_validation_length_max_only() {
    let (_tmp, project) = fresh_project("lv-max-only");
    run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--validate",
            "title=length:max=50",
            "--live-validation",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("value.chars().count() > 50"),
        "max-only length check must guard count > max:\n{routes}"
    );
    assert!(
        routes.contains("must be at most 50 characters"),
        "max-only error message must say 'at most':\n{routes}"
    );
    assert!(
        !routes.contains("value.chars().count() <"),
        "max-only rule must not emit a lower-bound check:\n{routes}"
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
    assert!(
        repo.contains("a href=(format!(\"/posts/"),
        "render_fragment must include a show link href:\n{repo}"
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
        routes.contains("sse-connect=\"/posts/events\""),
        "index list must connect to the SSE stream endpoint:\n{routes}"
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
