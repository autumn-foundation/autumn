//! `autumn seed` -- run the project's seed binary to populate the database.
//!
//! Delegates to `cargo run --bin seed` after:
//!   1. Verifying `src/bin/seed.rs` exists.
//!   2. Checking for pending migrations via the diesel CLI.
//!
//! The seed binary receives the active profile through the `AUTUMN_ENV`
//! environment variable, matching how the rest of the framework resolves
//! configuration profiles.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Errors surfaced by the seed runner.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SeedError {
    #[error(
        "no seed binary found; create `src/bin/seed.rs`\n\
         See: https://autumn.rs/guide/seeding"
    )]
    MissingSeedBinary,

    #[error("pending migrations detected; run `autumn migrate` before `autumn seed`")]
    PendingMigrations,

    #[error(
        "`--count` and `--model` must be used together: pass both to generate \
         faked rows (e.g. `autumn seed --count 200 --model Post`), or neither \
         to run the project seed binary"
    )]
    IncompleteFakeFlags,
}

/// Resolve the `--count`/`--model` pair into the fake-seed request, if any.
///
/// - Both absent → `Ok(None)`: preserve today's behavior (run the seed binary).
/// - Both present → `Ok(Some((count, model)))`: forward to the seed binary so
///   it generates faked rows for that model.
/// - Exactly one present → [`SeedError::IncompleteFakeFlags`].
fn resolve_fake_request(
    count: Option<usize>,
    model: Option<&str>,
) -> Result<Option<(usize, String)>, SeedError> {
    match (count, model) {
        (None, None) => Ok(None),
        (Some(count), Some(model)) => Ok(Some((count, model.to_owned()))),
        _ => Err(SeedError::IncompleteFakeFlags),
    }
}

/// Returns `true` if the seed binary source file exists at `path`.
fn seed_binary_exists_at(path: &Path) -> bool {
    path.is_file()
}

/// Locate the directory of a Cargo package by name using `cargo metadata`.
///
/// Returns the directory containing the package's `Cargo.toml`, or `None` if
/// the package cannot be found or `cargo metadata` fails.
fn find_package_dir(package: &str) -> Option<PathBuf> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let manifest_path = metadata["packages"]
        .as_array()?
        .iter()
        .find(|p| p["name"].as_str() == Some(package))?["manifest_path"]
        .as_str()?
        .to_owned();
    Path::new(&manifest_path).parent().map(Path::to_path_buf)
}

/// Resolve the database URL from environment variables and `autumn.toml`.
///
/// Reads `autumn.toml` from `base_dir` (the project root) rather than from
/// the process working directory so that workspace invocations are correct.
///
/// Precedence (highest to lowest):
/// 1. `AUTUMN_DATABASE__PRIMARY_URL` env var
/// 2. `AUTUMN_DATABASE__URL` env var
/// 3. `DATABASE_URL` env var
/// 4. `[profile.<profile>.database.primary_url]` in `<base_dir>/autumn.toml`
/// 5. `[profile.<profile>.database.url]` in `<base_dir>/autumn.toml`
/// 6. `[database.primary_url]` in `<base_dir>/autumn.toml`
/// 7. `[database.url]` in `<base_dir>/autumn.toml`
fn resolve_database_url_with_env<F>(
    env_var: F,
    base_dir: &Path,
    profile: &str,
) -> Result<String, SeedError>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    for var in [
        "AUTUMN_DATABASE__PRIMARY_URL",
        "AUTUMN_DATABASE__URL",
        "DATABASE_URL",
    ] {
        if let Ok(url) = env_var(var)
            && !url.is_empty()
        {
            return Ok(url);
        }
    }

    let config_path = base_dir.join("autumn.toml");
    if config_path.exists()
        && let Ok(contents) = std::fs::read_to_string(&config_path)
        && let Ok(table) = toml::from_str::<toml::Table>(&contents)
    {
        let value = toml::Value::Table(table);

        // Profile-specific override: [profile.<name>.database]
        let profile_database = value
            .get("profile")
            .and_then(|p| p.get(profile))
            .and_then(|p| p.get("database"))
            .and_then(toml::Value::as_table);
        if let Some(url) = first_database_url(profile_database) {
            return Ok(url);
        }

        // Top-level fallback: [database]
        let database = value.get("database").and_then(toml::Value::as_table);
        if let Some(url) = first_database_url(database) {
            return Ok(url);
        }
    }

    Err(SeedError::MissingSeedBinary)
}

fn first_database_url(database: Option<&toml::Table>) -> Option<String> {
    let database = database?;
    for key in ["primary_url", "url"] {
        if let Some(url) = database
            .get(key)
            .and_then(toml::Value::as_str)
            .filter(|url| !url.is_empty())
        {
            return Some(url.to_owned());
        }
    }

    None
}

/// Resolve the database URL using the real environment, a given base dir, and profile.
fn resolve_database_url(base_dir: &Path, profile: &str) -> Result<String, SeedError> {
    resolve_database_url_with_env(|key| std::env::var(key), base_dir, profile)
}

/// Check whether there are pending (unapplied) migrations.
///
/// Runs `diesel migration list` and scans for `[ ]` markers that indicate
/// unapplied migrations. Returns `Ok(())` if all migrations are applied or
/// if diesel CLI is unavailable (in which case a warning is printed).
fn check_pending_migrations(database_url: &str, migrations_dir: &str) -> Result<(), SeedError> {
    let output = Command::new("diesel")
        .args(["migration", "list", "--migration-dir", migrations_dir])
        .env("DATABASE_URL", database_url)
        .output();

    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let combined = format!("{stdout}{stderr}");
        if combined.lines().any(|line| line.contains("[ ]")) {
            return Err(SeedError::PendingMigrations);
        }
    } else {
        eprintln!(
            "  \u{26a0} diesel CLI not found; skipping pending-migration check.\n  \
             Install with: cargo install diesel_cli --no-default-features --features postgres"
        );
    }
    Ok(())
}

/// Entry point for `autumn seed`.
///
/// When both `count` and `model` are supplied, the seed binary is asked (via the
/// `AUTUMN_SEED_COUNT`/`AUTUMN_SEED_MODEL` environment variables) to generate
/// that many faked rows for the named model instead of running its hand-written
/// seed body. Supplying only one of the two is an error; supplying neither
/// preserves the original behavior (run the project seed binary).
pub fn run(profile: &str, package: Option<&str>, count: Option<usize>, model: Option<&str>) {
    eprintln!("\u{1F342} autumn seed\n");
    eprintln!("  Profile: {profile}");

    let fake_request = match resolve_fake_request(count, model) {
        Ok(req) => req,
        Err(e) => {
            eprintln!("\u{2717} {e}");
            std::process::exit(1);
        }
    };
    if let Some((count, model)) = &fake_request {
        eprintln!("  Faking: {count} row(s) of model `{model}`");
    }

    // Determine the project directory: either the workspace member's root (when
    // --package is given) or the current working directory.
    let project_dir: PathBuf = package
        .and_then(find_package_dir)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let seed_path = project_dir.join("src/bin/seed.rs");
    if !seed_binary_exists_at(&seed_path) {
        eprintln!("\u{2717} {}", SeedError::MissingSeedBinary);
        std::process::exit(1);
    }

    // Best-effort pending migration check when diesel CLI and migrations dir are available.
    let migrations_dir = project_dir.join("migrations");
    if let Ok(db_url) = resolve_database_url(&project_dir, profile)
        && migrations_dir.is_dir()
        && let Err(e) = check_pending_migrations(&db_url, &migrations_dir.to_string_lossy())
    {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }

    eprintln!("  Running seed binary...\n");

    let mut cmd = Command::new("cargo");
    cmd.args(["run", "--bin", "seed"]);
    if let Some(pkg) = package {
        cmd.args(["--package", pkg]);
    }
    cmd.env("AUTUMN_ENV", profile);
    cmd.env("AUTUMN_PROFILE", profile);
    // Forward the fake-seed request (if any) to the seed binary, which
    // dispatches on these vars to `autumn_web::seed::fake_seed_model`.
    if let Some((count, model)) = &fake_request {
        cmd.env("AUTUMN_SEED_COUNT", count.to_string());
        cmd.env("AUTUMN_SEED_MODEL", model);
    }
    // Run from the project directory so the seed binary's SeedContext reads
    // autumn.toml from the correct location (the package root, not the
    // workspace root).
    cmd.current_dir(&project_dir);

    let status = cmd.status();
    match status {
        Ok(s) if s.success() => {
            eprintln!("\n\u{2713} Seed completed successfully.");
        }
        Ok(_) => {
            eprintln!("\n\u{2717} Seed binary exited with a non-zero status.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("\u{2717} Failed to run cargo: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    // ── SeedError messages ─────────────────────────────────────────────────

    #[test]
    fn missing_seed_binary_error_mentions_src_bin_seed() {
        let msg = SeedError::MissingSeedBinary.to_string();
        assert!(
            msg.contains("src/bin/seed.rs"),
            "error should mention src/bin/seed.rs, got: {msg}"
        );
    }

    #[test]
    fn missing_seed_binary_error_mentions_seeding_guide_url() {
        let msg = SeedError::MissingSeedBinary.to_string();
        assert!(
            msg.contains("autumn.rs/guide/seeding"),
            "error should link to the seeding guide, got: {msg}"
        );
    }

    #[test]
    fn pending_migrations_error_mentions_autumn_migrate() {
        let msg = SeedError::PendingMigrations.to_string();
        assert!(
            msg.contains("autumn migrate"),
            "error should mention `autumn migrate`, got: {msg}"
        );
    }

    #[test]
    fn pending_migrations_error_mentions_pending() {
        let msg = SeedError::PendingMigrations.to_string();
        assert!(
            msg.contains("pending"),
            "error should mention pending migrations, got: {msg}"
        );
    }

    // ── resolve_fake_request (--count / --model, #1343 AC4) ─────────────────

    #[test]
    fn resolve_fake_request_none_when_both_absent() {
        assert_eq!(resolve_fake_request(None, None), Ok(None));
    }

    #[test]
    fn resolve_fake_request_some_when_both_present() {
        assert_eq!(
            resolve_fake_request(Some(200), Some("Post")),
            Ok(Some((200, "Post".to_string())))
        );
    }

    #[test]
    fn resolve_fake_request_errors_on_count_without_model() {
        assert_eq!(
            resolve_fake_request(Some(200), None),
            Err(SeedError::IncompleteFakeFlags)
        );
    }

    #[test]
    fn resolve_fake_request_errors_on_model_without_count() {
        assert_eq!(
            resolve_fake_request(None, Some("Post")),
            Err(SeedError::IncompleteFakeFlags)
        );
    }

    #[test]
    fn incomplete_fake_flags_error_is_actionable() {
        let msg = SeedError::IncompleteFakeFlags.to_string();
        assert!(
            msg.contains("--count") && msg.contains("--model"),
            "error should mention both flags, got: {msg}"
        );
    }

    // ── seed_binary_exists_at ──────────────────────────────────────────────

    #[test]
    fn seed_binary_exists_at_returns_false_for_missing_path() {
        assert!(!seed_binary_exists_at(Path::new(
            "/nonexistent/src/bin/seed.rs"
        )));
    }

    #[test]
    fn seed_binary_exists_at_returns_true_when_file_present() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("src/bin/seed.rs");
        std::fs::create_dir_all(seed_path.parent().unwrap()).unwrap();
        std::fs::write(&seed_path, "fn main() {}").unwrap();
        assert!(seed_binary_exists_at(&seed_path));
    }

    #[test]
    fn seed_binary_exists_at_returns_false_for_directory() {
        let tmp = TempDir::new().unwrap();
        let seed_dir = tmp.path().join("src/bin/seed.rs");
        std::fs::create_dir_all(&seed_dir).unwrap();
        // seed_dir is a directory, not a file
        assert!(!seed_binary_exists_at(&seed_dir));
    }

    // ── resolve_database_url_with_env ──────────────────────────────────────

    #[test]
    fn resolve_db_url_prefers_autumn_database_url() {
        let env = |key: &str| match key {
            "AUTUMN_DATABASE__URL" => Ok("postgres://autumn:5432/db".to_string()),
            "DATABASE_URL" => Ok("postgres://plain:5432/db".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let url = resolve_database_url_with_env(env, Path::new("."), "dev").unwrap();
        assert_eq!(url, "postgres://autumn:5432/db");
    }

    #[test]
    fn resolve_db_url_prefers_primary_database_url_env() {
        let env = |key: &str| match key {
            "AUTUMN_DATABASE__PRIMARY_URL" => Ok("postgres://primary:5432/db".to_string()),
            "AUTUMN_DATABASE__URL" => Ok("postgres://legacy:5432/db".to_string()),
            "DATABASE_URL" => Ok("postgres://plain:5432/db".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let url = resolve_database_url_with_env(env, Path::new("."), "dev").unwrap();
        assert_eq!(url, "postgres://primary:5432/db");
    }

    #[test]
    fn resolve_db_url_falls_back_to_database_url() {
        let env = |key: &str| match key {
            "DATABASE_URL" => Ok("postgres://fallback:5432/db".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let url = resolve_database_url_with_env(env, Path::new("."), "dev").unwrap();
        assert_eq!(url, "postgres://fallback:5432/db");
    }

    #[test]
    fn resolve_db_url_returns_err_when_no_env_no_toml() {
        let tmp = TempDir::new().unwrap();
        let env =
            |_: &str| -> Result<String, std::env::VarError> { Err(std::env::VarError::NotPresent) };
        assert!(resolve_database_url_with_env(env, tmp.path(), "dev").is_err());
    }

    #[test]
    fn resolve_db_url_ignores_empty_env_var() {
        let env = |key: &str| match key {
            "AUTUMN_DATABASE__URL" => Ok(String::new()),
            "DATABASE_URL" => Ok("postgres://real:5432/db".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let url = resolve_database_url_with_env(env, Path::new("."), "dev").unwrap();
        assert_eq!(url, "postgres://real:5432/db");
    }

    #[test]
    fn resolve_db_url_reads_database_url_from_base_dir_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nurl = \"postgres://toml:5432/db\"\n",
        )
        .unwrap();
        let env =
            |_: &str| -> Result<String, std::env::VarError> { Err(std::env::VarError::NotPresent) };
        let url = resolve_database_url_with_env(env, tmp.path(), "dev").unwrap();
        assert_eq!(url, "postgres://toml:5432/db");
    }

    #[test]
    fn resolve_db_url_reads_primary_url_from_base_dir_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://primary-toml:5432/db\"\n\
             url = \"postgres://legacy-toml:5432/db\"\n",
        )
        .unwrap();
        let env =
            |_: &str| -> Result<String, std::env::VarError> { Err(std::env::VarError::NotPresent) };
        let url = resolve_database_url_with_env(env, tmp.path(), "dev").unwrap();
        assert_eq!(url, "postgres://primary-toml:5432/db");
    }

    #[test]
    fn resolve_db_url_ignores_toml_in_wrong_base_dir() {
        // autumn.toml exists only in a different directory; base_dir has none.
        let tmp_with_toml = TempDir::new().unwrap();
        std::fs::write(
            tmp_with_toml.path().join("autumn.toml"),
            "[database]\nurl = \"postgres://wrong:5432/db\"\n",
        )
        .unwrap();
        let tmp_empty = TempDir::new().unwrap();
        let env =
            |_: &str| -> Result<String, std::env::VarError> { Err(std::env::VarError::NotPresent) };
        // Using tmp_empty as base_dir — no autumn.toml there, so must fail.
        assert!(resolve_database_url_with_env(env, tmp_empty.path(), "dev").is_err());
    }

    #[test]
    fn resolve_db_url_uses_profile_specific_section_from_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nurl = \"postgres://default:5432/db\"\n\
             [profile.demo.database]\nurl = \"postgres://demo:5432/demo_db\"\n",
        )
        .unwrap();
        let env =
            |_: &str| -> Result<String, std::env::VarError> { Err(std::env::VarError::NotPresent) };
        let url = resolve_database_url_with_env(env, tmp.path(), "demo").unwrap();
        assert_eq!(url, "postgres://demo:5432/demo_db");
    }

    #[test]
    fn resolve_db_url_uses_profile_specific_primary_url_from_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://default:5432/db\"\n\
             [profile.demo.database]\nprimary_url = \"postgres://demo:5432/demo_db\"\n",
        )
        .unwrap();
        let env =
            |_: &str| -> Result<String, std::env::VarError> { Err(std::env::VarError::NotPresent) };
        let url = resolve_database_url_with_env(env, tmp.path(), "demo").unwrap();
        assert_eq!(url, "postgres://demo:5432/demo_db");
    }

    #[test]
    fn resolve_db_url_falls_back_to_top_level_when_profile_section_absent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nurl = \"postgres://default:5432/db\"\n",
        )
        .unwrap();
        let env =
            |_: &str| -> Result<String, std::env::VarError> { Err(std::env::VarError::NotPresent) };
        let url = resolve_database_url_with_env(env, tmp.path(), "demo").unwrap();
        assert_eq!(url, "postgres://default:5432/db");
    }
}
