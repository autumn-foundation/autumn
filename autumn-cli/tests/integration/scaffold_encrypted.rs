//! The `{encrypted}` field-DSL modifier wires at-rest column encryption into
//! `autumn generate scaffold`/`model`/`admin` end to end (issue #1340).
//!
//! Drives the real `autumn` binary against a real `autumn new` project, so
//! these assertions cover the whole path a developer walks — DSL token ->
//! `#[encrypted(...)]` on the model -> `TEXT` migration column -> the admin
//! generator's auto-detection redacting the column — rather than any one
//! function's output.

// DSL tokens like `"api_token:String{encrypted}"` are literal scaffold inputs,
// not format strings — the `{…}` is the modifier under test.
#![allow(clippy::literal_string_with_formatting_args)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn")
}

fn run_autumn_ok(dir: &Path, args: &[&str]) -> String {
    let output = run_autumn(dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run_autumn_fail(dir: &Path, args: &[&str]) -> String {
    let output = run_autumn(dir, args);
    assert!(
        !output.status.success(),
        "autumn {args:?} unexpectedly succeeded\nstdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `autumn new` + a scaffold carrying one randomized and one deterministic
/// encrypted column alongside an ordinary plaintext one.
fn encrypted_project(name: &str) -> (tempfile::TempDir, PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let output = run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Account",
            "username:String",
            "api_token:String{encrypted}",
            "email:String{encrypted:deterministic}",
        ],
    );
    (tmp, project, output)
}

/// AC3: the generated model carries the attribute for both modes, and a
/// non-encrypted field in the same model is untouched.
#[test]
fn scaffolded_model_carries_the_encrypted_attribute() {
    let (_tmp, project, _) = encrypted_project("encrypted-model-app");
    let model = fs::read_to_string(project.join("src/models/account.rs")).unwrap();

    assert!(
        model.contains("#[encrypted]\n    pub api_token: String,"),
        "randomized column must carry a bare `#[encrypted]`:\n{model}"
    );
    assert!(
        model.contains("#[encrypted(deterministic)]\n    pub email: String,"),
        "deterministic column must carry the mode:\n{model}"
    );
    assert!(
        model.contains("pub username: String,"),
        "the plaintext column must still be emitted:\n{model}"
    );
    // The plaintext column must not pick up encryption by proximity.
    let username_line = model
        .lines()
        .position(|l| l.contains("pub username: String,"))
        .expect("username field");
    assert!(
        !model
            .lines()
            .nth(username_line - 1)
            .unwrap()
            .contains("encrypted"),
        "the plaintext column must not be preceded by an encryption attribute:\n{model}"
    );
}

/// The success metric in one assertion: **no** generated write path binds the
/// plaintext of an encrypted column. The scaffold's HTML `update` handler is a
/// raw diesel `.set((…))` tuple that never sees `New`/`Update{Model}`'s
/// `serialize_as`, so it must name the encrypting wrapper itself — otherwise a
/// scaffolded "encrypted" column silently stores plaintext on every edit.
#[test]
fn no_generated_write_path_binds_plaintext_for_an_encrypted_column() {
    let (_tmp, project, _) = encrypted_project("encrypted-writepath-app");
    let routes = fs::read_to_string(project.join("src/routes/accounts.rs")).unwrap();

    assert!(
        routes.contains("autumn_web::encryption::RandomizedText::from(new.api_token.clone())"),
        "the randomized column must be bound through its encrypting wrapper:\n{routes}"
    );
    assert!(
        routes.contains("autumn_web::encryption::DeterministicText::from(new.email.clone())"),
        "the deterministic column must be bound through its encrypting wrapper:\n{routes}"
    );
    for plaintext_bind in [
        "accounts::api_token.eq(new.api_token.clone())",
        "accounts::email.eq(new.email.clone())",
    ] {
        assert!(
            !routes.contains(plaintext_bind),
            "a plaintext bind for an encrypted column leaked into the routes: \
             `{plaintext_bind}`\n{routes}"
        );
    }
    // The plaintext sibling is untouched.
    assert!(
        routes.contains("accounts::username.eq(new.username.clone())"),
        "routes:\n{routes}"
    );
}

/// AC4: the column is unbounded `TEXT` — sized for the base64 ciphertext
/// envelope rather than the plaintext — and the migration explains why, so a
/// later "tidy this to VARCHAR(n)" edit is warned off.
#[test]
fn scaffolded_migration_column_is_unbounded_text_for_the_envelope() {
    let (_tmp, project, _) = encrypted_project("encrypted-migration-app");
    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains("create_accounts"))
        .expect("create_accounts migration");
    let up = fs::read_to_string(migrations.join("up.sql")).unwrap();

    assert!(up.contains("api_token TEXT NOT NULL"), "up.sql:\n{up}");
    assert!(up.contains("email TEXT NOT NULL"), "up.sql:\n{up}");
    let (comment, ddl) = up.split_once("CREATE TABLE").expect("create table");
    assert!(
        !ddl.contains("VARCHAR("),
        "no bounded column type may reach the DDL for an encrypted column:\n{up}"
    );
    // …but the comment *does* warn against one.
    assert!(
        comment.contains("VARCHAR(n)"),
        "the migration must warn against narrowing the column:\n{up}"
    );
    assert!(
        comment.contains("api_token") && comment.contains("email"),
        "the migration must name its encrypted columns:\n{up}"
    );
    assert!(
        comment.contains("base64") && comment.contains("envelope"),
        "the migration must explain the envelope sizing:\n{up}"
    );
}

/// AC5: `generate admin` over the scaffolded model redacts exactly the
/// encrypted columns, with no extra flags — the generator-emitted annotation
/// feeding the pre-existing `detect_encrypted_fields` auto-detection.
#[test]
fn generated_admin_redacts_the_scaffolded_encrypted_columns() {
    let (_tmp, project, _) = encrypted_project("encrypted-admin-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "admin",
            "Account",
            "username:String",
            "api_token:String{encrypted}",
            "email:String{encrypted:deterministic}",
        ],
    );
    let admin = fs::read_to_string(project.join("src/admin/account.rs")).unwrap();

    assert_eq!(
        admin.matches(".encrypted()").count(),
        2,
        "both encrypted columns must be redacted, and only those:\n{admin}"
    );
    assert!(
        !admin.contains(".encrypted_visible()"),
        "no column opted into admin visibility:\n{admin}"
    );
    // Ciphertext never matches a plaintext ILIKE, so encrypted columns must not
    // feed the admin's search filter — while the plaintext column still does.
    assert!(
        admin.contains("username") && !admin.contains("api_token.ilike"),
        "admin: {admin}"
    );
}

/// AC6: randomized encryption plus anything that performs an equality lookup is
/// refused at generate time, naming the field and pointing at the fix — instead
/// of generating code that fails at runtime with
/// `EncryptionError::RandomizedEqualityLookup`.
#[test]
fn randomized_encrypted_equality_lookups_are_refused_at_generate_time() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "encrypted-guard-app"]);
    let project = tmp.path().join("encrypted-guard-app");

    // Inline `:unique` modifier.
    let stderr = run_autumn_fail(
        &project,
        &[
            "generate",
            "scaffold",
            "Account",
            "api_token:String{encrypted}:unique",
        ],
    );
    assert!(
        stderr.contains("api_token") && stderr.contains("deterministic"),
        "the `:unique` refusal must name the field and the fix:\n{stderr}"
    );

    // `--unique` flag spelling of the same thing.
    let stderr = run_autumn_fail(
        &project,
        &[
            "generate",
            "scaffold",
            "Account",
            "api_token:String{encrypted}",
            "--unique",
            "api_token",
        ],
    );
    assert!(
        stderr.contains("api_token") && stderr.contains("deterministic"),
        "the `--unique` refusal must name the field and the fix:\n{stderr}"
    );

    // A derived `--query` equality lookup.
    let stderr = run_autumn_fail(
        &project,
        &[
            "generate",
            "scaffold",
            "Account",
            "api_token:String{encrypted}",
            "--query",
            "find_by_api_token:api_token",
        ],
    );
    assert!(
        stderr.contains("api_token") && stderr.contains("deterministic"),
        "the `--query` refusal must name the field and the fix:\n{stderr}"
    );

    // Nothing was written by any of the three refusals.
    assert!(
        !project.join("src/models/account.rs").exists(),
        "a refused generate must not leave a model behind"
    );
}

/// The deterministic mode is the sanctioned path: it keeps the derived
/// `find_by_<col>` lookup and the `UNIQUE` index (equal plaintext yields equal
/// ciphertext, so the index really does enforce plaintext uniqueness).
#[test]
fn deterministic_encrypted_column_keeps_unique_index_and_find_by() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "encrypted-deterministic-app"]);
    let project = tmp.path().join("encrypted-deterministic-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Account",
            "email:String{encrypted:deterministic}:unique",
        ],
    );

    let repo = fs::read_to_string(project.join("src/repositories/account.rs")).unwrap();
    assert!(
        repo.contains("fn find_by_email(email: String) -> Vec<Account>;"),
        "repo:\n{repo}"
    );
    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains("create_accounts"))
        .expect("create_accounts migration");
    let up = fs::read_to_string(migrations.join("up.sql")).unwrap();
    assert!(
        up.contains("CREATE UNIQUE INDEX idx_accounts_email_unique ON accounts (email);"),
        "up.sql:\n{up}"
    );
}

/// The generator must tell the developer about the one step it cannot do for
/// them: provisioning key material. Without it the app boots but every read and
/// write of the column fails (and production refuses to boot at all).
#[test]
fn scaffold_prints_the_key_material_next_step() {
    let (_tmp, _project, output) = encrypted_project("encrypted-warning-app");
    assert!(
        output.contains("autumn credentials edit"),
        "the generator must name the command:\n{output}"
    );
    assert!(
        output.contains("active_record_encryption.primary_key"),
        "the generator must name the credential:\n{output}"
    );
    assert!(
        output.contains("active_record_encryption.deterministic_key"),
        "a deterministic column must ask for the deterministic key:\n{output}"
    );
}

/// A scaffold with no `{encrypted}` column is completely unaffected — no
/// attribute, no migration comment, no warning.
#[test]
fn unencrypted_scaffold_is_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "plain-app"]);
    let project = tmp.path().join("plain-app");
    let output = run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "body:Text"],
    );

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(!model.contains("encrypted"), "model:\n{model}");
    assert!(
        !output.contains("credentials edit"),
        "an unencrypted scaffold must not warn about key material:\n{output}"
    );
    let migrations = fs::read_dir(project.join("migrations"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains("create_posts"))
        .expect("create_posts migration");
    let up = fs::read_to_string(migrations.join("up.sql")).unwrap();
    assert!(up.starts_with("CREATE TABLE posts ("), "up.sql:\n{up}");
}

/// `generate migration` emits SQL only, so accepting `{encrypted}` there would
/// hand back a plaintext column while the author believes otherwise — exactly
/// the silent failure this token exists to eliminate.
#[test]
fn generate_migration_refuses_the_encrypted_modifier() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "encrypted-migration-guard-app"]);
    let project = tmp.path().join("encrypted-migration-guard-app");

    let stderr = run_autumn_fail(
        &project,
        &[
            "generate",
            "migration",
            "AddApiTokenToAccounts",
            "api_token:String{encrypted}",
        ],
    );
    assert!(
        stderr.contains("encrypted") && stderr.contains("generate model"),
        "the refusal must point at the generators that wire the attribute:\n{stderr}"
    );

    // The documented conversion path for an existing column still works.
    run_autumn_ok(
        &project,
        &["generate", "migration", "EncryptApiTokenOnAccounts"],
    );
}

/// Slow end-to-end proof: a scaffolded app whose model carries
/// `#[encrypted]`/`#[encrypted(deterministic)]` actually type-checks against
/// this workspace's `autumn-web` — i.e. the emitted attribute is one the
/// `#[model]`/`#[repository]` macros accept, not merely the right text.
///
/// Ignored by default (it compiles a fresh project); run with
/// `cargo test -p autumn-cli --test cli_tests -- --ignored encrypted_scaffold_cargo_checks`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn encrypted_scaffold_cargo_checks() {
    let (_tmp, project, _) = encrypted_project("encrypted-check-app");
    cargo_check_against_workspace(&project);
}

/// The same compile proof for the `--api` variant, which emits no HTML routes
/// at all and reaches the database only through the repository macro.
///
/// Split into one test per variant (rather than a loop) so a CI failure names
/// the shape that broke. Each of these has been a distinct source of "compiles
/// for a plain model, not for an encrypted one" in this slice.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn encrypted_api_scaffold_cargo_checks() {
    let project = encrypted_variant_project("encrypted-api-app", &["--api"]);
    cargo_check_against_workspace(&project.1);
}

/// `--live`: writes go through the repository extractor (`repo.save` /
/// `repo.update` with a `Patch::Set` changeset) rather than the raw diesel
/// statements the standard HTML path uses.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn encrypted_live_scaffold_cargo_checks() {
    let project = encrypted_variant_project("encrypted-live-app", &["--live"]);
    cargo_check_against_workspace(&project.1);
}

/// A nested `--belongs-to` child carrying an encrypted column: its inline "add"
/// form posts through the nested create handler's own raw diesel insert, a
/// third distinct write path.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn encrypted_nested_scaffold_cargo_checks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "encrypted-nested-app"]);
    let project = tmp.path().join("encrypted-nested-app");
    run_autumn_ok(&project, &["generate", "scaffold", "Post", "title:String"]);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Token",
            "post:references",
            "secret:String{encrypted}",
            "--belongs-to",
            "Post",
        ],
    );
    cargo_check_against_workspace(&project);
}

/// `autumn new` + an encrypted scaffold with `extra` flags appended. The
/// `TempDir` is returned alongside the project path so it outlives the check.
fn encrypted_variant_project(name: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec![
        "generate",
        "scaffold",
        "Account",
        "username:String",
        "api_token:String{encrypted}",
        "email:String{encrypted:deterministic}",
    ];
    args.extend_from_slice(extra);
    run_autumn_ok(&project, &args);
    (tmp, project)
}

/// `generate admin` over an encrypted model must compile once the developer
/// wires the plugin in, as `docs/guide/admin.md` instructs.
///
/// This leg exists because the admin adapter reaches the database through code
/// the scaffold never emits — its own `.values(..)` insert, its
/// `Update{Model}` changeset, and (with a `lock_version` column) a raw
/// `col.eq(..)` tuple. `generate admin` also does NOT add `mod admin;` or the
/// plugin dependency, so a bare `cargo check` on a freshly generated project
/// silently skips the whole file: the adapter has to be wired up here or this
/// proves nothing.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn encrypted_admin_scaffold_cargo_checks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "encrypted-admin-check-app"]);
    let project = tmp.path().join("encrypted-admin-check-app");
    // `lock_version` puts the admin on its raw `col.eq(..)` update path, which
    // is a different write path from the ordinary changeset one.
    let fields = [
        "username:String",
        "api_token:String{encrypted}",
        "email:String{encrypted:deterministic}",
        "lock_version:i32",
    ];
    let mut scaffold_args = vec!["generate", "scaffold", "Account"];
    scaffold_args.extend_from_slice(&fields);
    run_autumn_ok(&project, &scaffold_args);
    let mut admin_args = vec!["generate", "admin", "Account"];
    admin_args.extend_from_slice(&fields);
    run_autumn_ok(&project, &admin_args);

    // Wire the generated adapter in, per docs/guide/admin.md — `generate admin`
    // deliberately leaves both steps to the developer.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let plugin = workspace_root
        .join("autumn-admin-plugin")
        .display()
        .to_string()
        .replace('\\', "/");
    let cargo_toml_path = project.join("Cargo.toml");
    let content = fs::read_to_string(&cargo_toml_path).unwrap();
    let content = content.replacen(
        "[dependencies]",
        &format!("[dependencies]\nautumn-admin-plugin = {{ path = \"{plugin}\" }}"),
        1,
    );
    fs::write(&cargo_toml_path, content).unwrap();
    let main_path = project.join("src/main.rs");
    let main = fs::read_to_string(&main_path).unwrap();
    fs::write(&main_path, format!("mod admin;\n{main}")).unwrap();

    cargo_check_against_workspace(&project);
}

/// `cargo check --tests` a generated project against this workspace's
/// `autumn-web` (rather than the published crate), asserting success.
fn cargo_check_against_workspace(project: &Path) {
    use std::fmt::Write as _;

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

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the encrypted scaffold at {} failed:\nstdout:\n{}\nstderr:\n{}",
        project.display(),
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
