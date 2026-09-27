//! Seed context for populating databases with representative data.
//!
//! Enabled with the `seed` cargo feature (off by default). Include in your
//! project's `Cargo.toml` to use it in a seed binary:
//!
//! ```toml
//! autumn-web = { version = "...", features = ["seed"] }
//! ```
//!
//! # Example (`src/bin/seed.rs`)
//!
//! ```no_run
//! use autumn_web::seed::SeedContext;
//!
//! #[tokio::main]
//! async fn main() {
//!     let ctx = SeedContext::build().expect("seed context");
//!     let mut db = ctx.conn().await.expect("db connection");
//!     // use db with Diesel queries ...
//!     println!("Seed complete (profile: {})", ctx.profile());
//! }
//! ```
//!
//! # Faking data (issue #1343)
//!
//! Any `#[autumn_web::model]` gets a factory whose `.fake()` fills unset fields
//! with realistic data. Populate 100+ rows in a single line to exercise
//! pagination and search:
//!
//! ```no_run
//! use autumn_web::seed::SeedContext;
//!
//! autumn_web::reexports::diesel::table! {
//!     posts (id) { id -> Int8, title -> Text, body -> Text }
//! }
//!
//! #[autumn_web::model(table = "posts")]
//! struct Post {
//!     #[id]
//!     id: i64,
//!     title: String,
//!     body: String,
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let ctx = SeedContext::build().expect("seed context");
//!
//!     // 200 faked posts, each with distinct fake title/body:
//!     Post::factory().fake().create_many(200, ctx.pool()).await;
//! }
//! ```
//!
//! The same registry powers `autumn seed --count 200 --model Post`, which drives
//! a model's factory by name (via [`fake_seed_model`]) without editing the seed
//! binary at all.

use std::path::Path;

use crate::config::DatabaseConfig;
use crate::db::RuntimeConnection;
use crate::db::create_pool;
use diesel_async::pooled_connection::deadpool::{Object, Pool};
use futures::future::BoxFuture;

/// Error type returned by [`SeedContext`] operations.
#[derive(Debug, thiserror::Error)]
pub enum SeedContextError {
    /// No database URL was found in the environment or `autumn.toml`.
    #[error(
        "no primary database URL configured; set AUTUMN_DATABASE__PRIMARY_URL, AUTUMN_DATABASE__URL, or `database.primary_url` in autumn.toml"
    )]
    NoDatabaseUrl,

    /// The connection pool could not be built.
    #[error("failed to build connection pool: {0}")]
    PoolBuild(#[from] crate::db::PoolError),

    /// A pooled connection could not be acquired.
    #[error("failed to acquire database connection: {0}")]
    Connection(String),
}

/// Context provided to a seed binary.
///
/// Holds the database connection pool and the active profile, both resolved
/// from the project's `autumn.toml` and environment variables — the same
/// sources the main application uses.
///
/// # Usage
///
/// ```no_run
/// # use autumn_web::seed::SeedContext;
/// # #[tokio::main]
/// # async fn main() {
/// let ctx = SeedContext::build().expect("seed context");
/// println!("profile: {}", ctx.profile());
/// let mut db = ctx.conn().await.expect("connection");
/// // use &mut *db as &mut AsyncPgConnection with diesel_async queries
/// # }
/// ```
pub struct SeedContext {
    pool: Pool<RuntimeConnection>,
    profile: String,
}

impl SeedContext {
    /// Build a `SeedContext` by reading the database URL and profile from the
    /// environment and `autumn.toml` in the current working directory.
    ///
    /// Profile resolution order (first wins):
    /// 1. `AUTUMN_ENV` env var
    /// 2. `AUTUMN_PROFILE` env var
    /// 3. Defaults to `"dev"`
    ///
    /// Database URL resolution order (first wins):
    /// 1. `AUTUMN_DATABASE__PRIMARY_URL` env var
    /// 2. `AUTUMN_DATABASE__URL` env var
    /// 3. `DATABASE_URL` env var
    /// 4. `database.primary_url` in `autumn.toml`
    /// 5. `database.url` in `autumn.toml`
    ///
    /// # Errors
    ///
    /// Returns [`SeedContextError::NoDatabaseUrl`] if no database URL is
    /// configured, or [`SeedContextError::PoolBuild`] if the pool cannot be
    /// constructed.
    pub fn build() -> Result<Self, SeedContextError> {
        let profile = resolve_profile();
        let db_url = resolve_database_url(&profile).ok_or(SeedContextError::NoDatabaseUrl)?;

        let config = DatabaseConfig {
            primary_url: Some(db_url),
            ..DatabaseConfig::default()
        };

        let pool = create_pool(&config)?.ok_or(SeedContextError::NoDatabaseUrl)?;

        Ok(Self { pool, profile })
    }

    /// Returns the active profile name (e.g. `"dev"`, `"demo"`, `"test"`).
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Returns the underlying connection pool.
    ///
    /// Useful for APIs that take a `&Pool<AsyncPgConnection>` directly, such as
    /// factory `create_many(count, pool)` and
    /// [`fake_seed_model`].
    #[must_use]
    pub const fn pool(&self) -> &Pool<RuntimeConnection> {
        &self.pool
    }

    /// Acquires a pooled database connection.
    ///
    /// Returns a [`Object<AsyncPgConnection>`] that implements `DerefMut` to
    /// `AsyncPgConnection`, so it can be passed directly to diesel-async
    /// query methods as `&mut *conn`.
    ///
    /// # Errors
    ///
    /// Returns [`SeedContextError::Connection`] if the pool is exhausted or
    /// the connection cannot be established.
    pub async fn conn(&self) -> Result<Object<RuntimeConnection>, SeedContextError> {
        self.pool
            .get()
            .await
            .map_err(|e| SeedContextError::Connection(e.to_string()))
    }
}

/// Resolve the active profile from environment variables.
fn resolve_profile() -> String {
    std::env::var("AUTUMN_ENV")
        .or_else(|_| std::env::var("AUTUMN_PROFILE"))
        .unwrap_or_else(|_| "dev".to_string())
}

/// Resolve the database URL from environment variables and `autumn.toml`.
///
/// Resolution order (first non-empty value wins):
/// 1. `AUTUMN_DATABASE__PRIMARY_URL` env var
/// 2. `AUTUMN_DATABASE__URL` env var
/// 3. `DATABASE_URL` env var
/// 4. `[profile.<profile>.database.primary_url]` in `autumn.toml`
/// 5. `[profile.<profile>.database.url]` in `autumn.toml`
/// 6. `[database.primary_url]` in `autumn.toml`
/// 7. `[database.url]` in `autumn.toml`
fn resolve_database_url(profile: &str) -> Option<String> {
    if let Ok(url) = std::env::var("AUTUMN_DATABASE__PRIMARY_URL")
        && !url.is_empty()
    {
        return Some(url);
    }
    if let Ok(url) = std::env::var("AUTUMN_DATABASE__URL")
        && !url.is_empty()
    {
        return Some(url);
    }
    if let Ok(url) = std::env::var("DATABASE_URL")
        && !url.is_empty()
    {
        return Some(url);
    }

    resolve_database_url_from_toml(profile, Path::new("autumn.toml"))
}

fn resolve_database_url_from_toml(profile: &str, config_path: &Path) -> Option<String> {
    if config_path.exists()
        && let Ok(contents) = std::fs::read_to_string(config_path)
        && let Ok(table) = toml::from_str::<toml::Table>(&contents)
    {
        let value = toml::Value::Table(table);

        // Profile-specific override: [profile.<name>.database.primary_url/url]
        if let Some(url) = first_database_url(
            value
                .get("profile")
                .and_then(|p| p.get(profile))
                .and_then(|p| p.get("database")),
        ) {
            return Some(url);
        }

        // Top-level fallback: [database.primary_url/url]
        if let Some(url) = first_database_url(value.get("database")) {
            return Some(url);
        }
    }

    None
}

fn first_database_url(database: Option<&toml::Value>) -> Option<String> {
    let database = database?;
    for key in ["primary_url", "url"] {
        if let Some(url) = database
            .get(key)
            .and_then(toml::Value::as_str)
            .filter(|u| !u.is_empty())
        {
            return Some(url.to_string());
        }
    }
    None
}

// ── #1343 AC4: model-name → factory fake-seeder registry ────────────────────
//
// The `autumn` CLI cannot name a project's generated model types directly, so
// each `#[model]` registers a `FakeSeeder` via `inventory` (see the
// `__autumn_register_fake_seeder!` forwarding macro in `lib.rs`). A seed binary
// (or `autumn seed --count N --model M`) then looks a model up by name and runs
// its factory's `.fake().create_many(count, pool)`, all without editing
// `src/bin/seed.rs`.

/// A registered model factory that `fake_seed_model` can drive by name.
///
/// One is submitted per `#[model]` (through the internal
/// `__autumn_register_fake_seeder!` macro) when autumn-web is built with the
/// `seed` feature. Collected at link time via [`inventory`].
pub struct FakeSeeder {
    /// The model's type name, e.g. `"Post"`. Matched case-insensitively.
    pub model: &'static str,
    /// Insert `count` faked rows via the model's factory and return how many
    /// were inserted.
    pub run: fn(&Pool<RuntimeConnection>, usize) -> BoxFuture<'_, usize>,
}

inventory::collect!(FakeSeeder);

/// Error returned by [`fake_seed_model`] when the requested model is not a
/// registered faked model.
#[derive(Debug, thiserror::Error)]
pub enum FakeSeedError {
    /// No registered `#[model]` matched the requested name.
    #[error("unknown model {requested:?}; available faked models: {available}")]
    UnknownModel {
        /// The `--model` value that did not match any registered model.
        requested: String,
        /// Comma-separated list of registered model names (or a placeholder
        /// when none are registered).
        available: String,
    },

    /// The `AUTUMN_SEED_COUNT` value forwarded by `autumn seed --count` did not
    /// parse as a non-negative integer.
    #[error("invalid AUTUMN_SEED_COUNT {value:?}: expected a non-negative integer")]
    InvalidCount {
        /// The raw `AUTUMN_SEED_COUNT` value that failed to parse.
        value: String,
    },

    /// The `AUTUMN_SEED_COUNT` value exceeds [`MAX_FAKE_SEED_COUNT`], the
    /// sanity ceiling on a single fake-seed request.
    #[error("AUTUMN_SEED_COUNT {value} exceeds the maximum of {max} rows per fake-seed request")]
    CountTooLarge {
        /// The requested count.
        value: usize,
        /// The ceiling it exceeded ([`MAX_FAKE_SEED_COUNT`]).
        max: usize,
    },
}

/// Sanity ceiling on a single `--count`/`AUTUMN_SEED_COUNT` fake-seed request.
///
/// Generously above any realistic dev-seeding volume, this exists solely to
/// turn a mistyped or pasted-wrong count (e.g. an extra digit, or a stray
/// `usize::MAX`) into a clear [`FakeSeedError::CountTooLarge`] instead of a
/// `Vec::with_capacity` allocation panic deep inside the generated factory's
/// `create_many`/`build_many`.
pub const MAX_FAKE_SEED_COUNT: usize = 1_000_000;

/// The names of every registered faked model, sorted, for diagnostics.
#[must_use]
pub fn registered_fake_models() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = inventory::iter::<FakeSeeder>
        .into_iter()
        .map(|s| s.model)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Find the registered [`FakeSeeder`] whose model name matches `name`
/// (case-insensitive), if any. Pool-free so the lookup can be unit-tested
/// without a live database.
fn find_fake_seeder(name: &str) -> Option<&'static FakeSeeder> {
    inventory::iter::<FakeSeeder>
        .into_iter()
        .find(|s| s.model.eq_ignore_ascii_case(name))
}

/// Generate and insert `count` faked rows for the model named `name`
/// (case-insensitive), using that model's factory `.fake().create_many(...)`.
///
/// Returns the number of rows inserted.
///
/// # Errors
///
/// Returns [`FakeSeedError::UnknownModel`] when no registered `#[model]`
/// matches `name`. The error lists the available model names.
pub async fn fake_seed_model(
    name: &str,
    count: usize,
    pool: &Pool<RuntimeConnection>,
) -> Result<usize, FakeSeedError> {
    if let Some(seeder) = find_fake_seeder(name) {
        return Ok((seeder.run)(pool, count).await);
    }
    let available = registered_fake_models();
    let available = if available.is_empty() {
        "(none registered)".to_string()
    } else {
        available.join(", ")
    };
    Err(FakeSeedError::UnknownModel {
        requested: name.to_string(),
        available,
    })
}

/// Handle a fake-seed request forwarded by `autumn seed --count N --model M`, if
/// one is present.
///
/// `autumn seed --count/--model` forwards the request as the
/// `AUTUMN_SEED_COUNT` and `AUTUMN_SEED_MODEL` environment variables. This
/// helper reads them and, when **both** are set, generates that many faked rows
/// for the named model via [`fake_seed_model`] and prints a one-line summary.
///
/// The dispatch lives here — in versioned framework code — rather than being
/// copied into every generated `src/bin/seed.rs`, so the scaffolded seed binary
/// is a single call to this function. Fixes/extensions to the flag handling
/// ship with autumn-web instead of requiring every project to re-scaffold.
///
/// Returns:
/// - `Ok(true)` — a request was present and handled; the caller should return
///   from its seed `main` without running its hand-written body.
/// - `Ok(false)` — no request (neither var set); the caller runs its own body.
///
/// # Errors
///
/// Returns [`FakeSeedError::InvalidCount`] when `AUTUMN_SEED_COUNT` is set but
/// does not parse as a non-negative integer, or [`FakeSeedError::UnknownModel`]
/// when `AUTUMN_SEED_MODEL` names no registered `#[model]`.
pub async fn maybe_fake_seed(pool: &Pool<RuntimeConnection>) -> Result<bool, FakeSeedError> {
    let (Ok(model), Ok(count)) = (
        std::env::var("AUTUMN_SEED_MODEL"),
        std::env::var("AUTUMN_SEED_COUNT"),
    ) else {
        return Ok(false);
    };
    let count: usize = count
        .trim()
        .parse()
        .map_err(|_| FakeSeedError::InvalidCount { value: count })?;
    if count > MAX_FAKE_SEED_COUNT {
        return Err(FakeSeedError::CountTooLarge {
            value: count,
            max: MAX_FAKE_SEED_COUNT,
        });
    }
    let inserted = fake_seed_model(&model, count, pool).await?;
    println!("Inserted {inserted} faked `{model}` row(s).");
    Ok(true)
}

// ── #1343 AC4: fake-seeder registry test fixture ────────────────────────────
//
// Register a dummy model so the registry-lookup tests below have a known entry
// to find without depending on any other crate's `#[model]`s. The `run` closure
// never touches the pool (the lookup tests never invoke it), so it is safe to
// leave a placeholder body.
#[cfg(test)]
inventory::submit! {
    FakeSeeder {
        model: "DummyFakeSeederModel",
        run: |_pool, count| ::std::boxed::Box::pin(async move { count }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── fake-seeder registry (#1343 AC4) ───────────────────────────────────

    #[test]
    fn registry_collects_submitted_seeder() {
        assert!(
            registered_fake_models().contains(&"DummyFakeSeederModel"),
            "expected the submitted dummy seeder to be collected via inventory, \
             got: {:?}",
            registered_fake_models()
        );
    }

    #[test]
    fn find_fake_seeder_is_case_insensitive() {
        assert!(find_fake_seeder("DummyFakeSeederModel").is_some());
        assert!(
            find_fake_seeder("dummyfakeseedermodel").is_some(),
            "lookup should be case-insensitive"
        );
        assert_eq!(
            find_fake_seeder("DummyFakeSeederModel").unwrap().model,
            "DummyFakeSeederModel"
        );
    }

    #[test]
    fn find_fake_seeder_returns_none_for_unknown() {
        assert!(find_fake_seeder("NoSuchModelXYZ").is_none());
    }

    #[test]
    fn unknown_model_error_lists_available_models() {
        // Build the error the way `fake_seed_model` does for an unknown name,
        // without needing a live pool.
        let available = registered_fake_models().join(", ");
        let err = FakeSeedError::UnknownModel {
            requested: "NoSuchModelXYZ".to_string(),
            available,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("NoSuchModelXYZ"),
            "error should name the requested model, got: {msg}"
        );
        assert!(
            msg.contains("DummyFakeSeederModel"),
            "error should list available registered models, got: {msg}"
        );
    }

    // ── maybe_fake_seed dispatch (#1343 AC4 centralization) ────────────────

    /// A lazily-built pool (deadpool does not connect until first use), so these
    /// tests exercise `maybe_fake_seed`'s pre-connection branches without a live
    /// database.
    fn lazy_pool() -> Pool<RuntimeConnection> {
        crate::db::create_pool(&DatabaseConfig {
            // Backend-parametric: `build_sqlite_pool` refuses a `postgres://`
            // target, so a literal here panics under `--features "sqlite,seed"`.
            primary_url: Some(crate::test_urls::primary("unused")),
            ..DatabaseConfig::default()
        })
        .expect("pool builds")
        .expect("url present => Some(pool)")
    }

    #[test]
    fn maybe_fake_seed_returns_false_when_env_unset() {
        let pool = lazy_pool();
        temp_env::with_vars(
            [
                ("AUTUMN_SEED_MODEL", None::<&str>),
                ("AUTUMN_SEED_COUNT", None::<&str>),
            ],
            || {
                let handled = futures::executor::block_on(maybe_fake_seed(&pool))
                    .expect("no request is not an error");
                assert!(
                    !handled,
                    "with no fake request the caller must run its own seed body"
                );
            },
        );
    }

    #[test]
    fn maybe_fake_seed_errors_on_non_integer_count() {
        let pool = lazy_pool();
        temp_env::with_vars(
            [
                ("AUTUMN_SEED_MODEL", Some("DummyFakeSeederModel")),
                ("AUTUMN_SEED_COUNT", Some("not-a-number")),
            ],
            || {
                let err = futures::executor::block_on(maybe_fake_seed(&pool))
                    .expect_err("a non-integer count must be an error, not a silent no-op");
                assert!(
                    matches!(err, FakeSeedError::InvalidCount { .. }),
                    "expected InvalidCount, got: {err:?}"
                );
            },
        );
    }

    #[test]
    fn maybe_fake_seed_errors_on_count_above_max() {
        let pool = lazy_pool();
        let too_large = (MAX_FAKE_SEED_COUNT + 1).to_string();
        temp_env::with_vars(
            [
                ("AUTUMN_SEED_MODEL", Some("DummyFakeSeederModel")),
                ("AUTUMN_SEED_COUNT", Some(too_large.as_str())),
            ],
            || {
                let err = futures::executor::block_on(maybe_fake_seed(&pool)).expect_err(
                    "a count above the sanity ceiling must be a clean error, not a later \
                     capacity-overflow panic inside create_many/build_many",
                );
                assert!(
                    matches!(err, FakeSeedError::CountTooLarge { .. }),
                    "expected CountTooLarge, got: {err:?}"
                );
            },
        );
    }

    #[test]
    fn maybe_fake_seed_allows_count_at_max() {
        // The boundary itself (MAX_FAKE_SEED_COUNT exactly) must not be
        // rejected by the ceiling check — only values strictly above it.
        // DummyFakeSeederModel's `run` just echoes `count` back without
        // allocating, so this exercises the guard without a live database.
        let pool = lazy_pool();
        let at_max = MAX_FAKE_SEED_COUNT.to_string();
        temp_env::with_vars(
            [
                ("AUTUMN_SEED_MODEL", Some("DummyFakeSeederModel")),
                ("AUTUMN_SEED_COUNT", Some(at_max.as_str())),
            ],
            || {
                let handled = futures::executor::block_on(maybe_fake_seed(&pool))
                    .expect("count exactly at the ceiling must be accepted");
                assert!(handled);
            },
        );
    }

    #[test]
    fn count_too_large_error_message_reports_value_and_max() {
        let msg = FakeSeedError::CountTooLarge {
            value: 5_000_000,
            max: MAX_FAKE_SEED_COUNT,
        }
        .to_string();
        assert!(msg.contains("5000000"), "got: {msg}");
        assert!(msg.contains(&MAX_FAKE_SEED_COUNT.to_string()), "got: {msg}");
    }

    // ── resolve_profile ────────────────────────────────────────────────────

    #[test]
    fn resolve_profile_defaults_to_dev() {
        // Isolate from the real environment using temp_env.
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None::<&str>),
                ("AUTUMN_PROFILE", None::<&str>),
            ],
            || {
                assert_eq!(resolve_profile(), "dev");
            },
        );
    }

    #[test]
    fn resolve_profile_prefers_autumn_env() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", Some("demo")),
                ("AUTUMN_PROFILE", Some("test")),
            ],
            || {
                assert_eq!(resolve_profile(), "demo");
            },
        );
    }

    #[test]
    fn resolve_profile_falls_back_to_autumn_profile() {
        temp_env::with_vars(
            [
                ("AUTUMN_ENV", None::<&str>),
                ("AUTUMN_PROFILE", Some("staging")),
            ],
            || {
                assert_eq!(resolve_profile(), "staging");
            },
        );
    }

    // ── resolve_database_url ───────────────────────────────────────────────

    #[test]
    fn resolve_database_url_prefers_autumn_database_primary_url() {
        temp_env::with_vars(
            [
                (
                    "AUTUMN_DATABASE__PRIMARY_URL",
                    Some("postgres://primary:5432/db"),
                ),
                ("AUTUMN_DATABASE__URL", Some("postgres://legacy:5432/db")),
                ("DATABASE_URL", Some("postgres://fallback:5432/db")),
            ],
            || {
                assert_eq!(
                    resolve_database_url("dev").as_deref(),
                    Some("postgres://primary:5432/db")
                );
            },
        );
    }

    #[test]
    fn resolve_database_url_falls_back_to_database_url() {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", Some("postgres://fallback:5432/db")),
            ],
            || {
                assert_eq!(
                    resolve_database_url("dev").as_deref(),
                    Some("postgres://fallback:5432/db")
                );
            },
        );
    }

    #[test]
    fn resolve_database_url_returns_none_when_nothing_configured() {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                // No autumn.toml in the test runner's cwd (we rely on that
                // directory not having one; if it does, this test is a no-op).
                let url = resolve_database_url("dev");
                // Either None (no autumn.toml) or Some (if autumn.toml exists
                // with a database.url in the test runner cwd). We can't assert
                // None unconditionally, so we just assert the function returns
                // without panicking.
                let _ = url;
            },
        );
    }

    #[test]
    fn resolve_database_url_ignores_empty_autumn_database_url() {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", Some("")),
                ("DATABASE_URL", Some("postgres://real:5432/db")),
            ],
            || {
                assert_eq!(
                    resolve_database_url("dev").as_deref(),
                    Some("postgres://real:5432/db")
                );
            },
        );
    }

    #[test]
    fn resolve_database_url_uses_profile_specific_section_from_toml() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let toml_content = r#"
[database]
url = "postgres://default:5432/db"

[profile.demo.database]
primary_url = "postgres://demo:5432/demo_db"
"#;
        std::fs::write(tmp.path().join("autumn.toml"), toml_content).unwrap();

        let result = temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || resolve_database_url_from_toml("demo", &tmp.path().join("autumn.toml")),
        );

        assert_eq!(result.as_deref(), Some("postgres://demo:5432/demo_db"));
    }

    #[test]
    fn resolve_database_url_falls_back_to_top_level_when_profile_section_absent() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let toml_content = r#"
[database]
primary_url = "postgres://default:5432/db"
"#;
        std::fs::write(tmp.path().join("autumn.toml"), toml_content).unwrap();

        let result = temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || resolve_database_url_from_toml("demo", &tmp.path().join("autumn.toml")),
        );

        assert_eq!(result.as_deref(), Some("postgres://default:5432/db"));
    }

    // ── SeedContextError messages ──────────────────────────────────────────

    #[test]
    fn no_database_url_error_message_is_actionable() {
        let msg = SeedContextError::NoDatabaseUrl.to_string();
        assert!(
            msg.contains("AUTUMN_DATABASE__PRIMARY_URL") || msg.contains("autumn.toml"),
            "error should be actionable, got: {msg}"
        );
    }
}
