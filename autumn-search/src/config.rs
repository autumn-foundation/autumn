//! The plugin-owned `[search]` section of `autumn.toml`.

use std::path::{Path, PathBuf};

use autumn_web::config::Env;
use serde::{Deserialize, Serialize};

/// Configuration for the search subsystem.
///
/// ```toml
/// [search]
/// queue = "search"            # the #[job] queue reindex/backfill run on
/// batch_size = 500            # rows per backfill batch
/// enabled = true              # false ⇒ index writes become no-ops
/// embedding_dimensions = 768  # declared width; enables the pgvector fast path
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SearchConfig {
    /// Named `#[job]` queue the reindex and backfill jobs are routed to.
    ///
    /// A dedicated queue by default: indexing is bulk, off-request work that
    /// should never head-of-line-block a user-visible job.
    #[serde(default = "default_queue")]
    pub queue: String,

    /// Rows read (and documents written) per backfill batch.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// When `false`, index writes become no-ops and queries return empty
    /// pages. The kill switch for an incident: turning search off must not
    /// require a deploy, and must not start failing writes to the model.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Declared embedding width.
    ///
    /// Optional: the in-memory backend infers it from the vectors it is given.
    /// The Postgres backend needs it to declare a `pgvector` column, so
    /// setting it is what enables the accelerated k-NN path; without it,
    /// vectors fall back to a portable `double precision[]` column.
    #[serde(default)]
    pub embedding_dimensions: Option<usize>,
}

fn default_queue() -> String {
    "search".to_owned()
}

const fn default_batch_size() -> usize {
    500
}

const fn default_enabled() -> bool {
    true
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            queue: default_queue(),
            batch_size: default_batch_size(),
            enabled: default_enabled(),
            embedding_dimensions: None,
        }
    }
}

/// An `autumn.toml` document, seen only through its `[search]` table.
///
/// Not `deny_unknown_fields` — every other top-level section (`server`, `log`,
/// `profile`, …) belongs to core or another plugin and is none of this crate's
/// business. `SearchConfig` itself is strict, so a typo *inside* `[search]`
/// still fails.
#[derive(Deserialize)]
struct Document {
    #[serde(default)]
    search: Option<SearchConfig>,
}

/// Why a `[search]` section was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchConfigError {
    /// The TOML did not parse, or `[search]` had an unknown/ill-typed key.
    #[error("invalid [search] configuration: {0}")]
    Invalid(String),
}

impl SearchConfig {
    /// Read `[search]` from the `autumn.toml` at `path`.
    ///
    /// Matches `autumn-media-plugin`'s `MediaConfig::from_autumn_toml`, which
    /// also takes a **path** — the string-taking form here is
    /// [`SearchConfig::from_toml_str`], so a caller who passes `"autumn.toml"`
    /// to the wrong one gets a compile error rather than a TOML parse error
    /// about the filename.
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] when the file cannot be read or
    /// its `[search]` table is malformed.
    pub fn from_autumn_toml(path: impl AsRef<std::path::Path>) -> Result<Self, SearchConfigError> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|e| {
            SearchConfigError::Invalid(format!("cannot read {}: {e}", path.display()))
        })?;
        Self::from_toml_str(&contents)
    }

    /// Parse the `[search]` table out of an `autumn.toml` document.
    ///
    /// A missing section yields the defaults. An unknown key is an **error**,
    /// not a warning: a typo'd `queu = "indexing"` would otherwise silently
    /// leave indexing on the default queue with no signal at all.
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] when the document does not
    /// parse, when `[search]` has an unknown or ill-typed key, or when a value
    /// is out of range.
    pub fn from_toml_str(contents: &str) -> Result<Self, SearchConfigError> {
        let document: Document =
            toml_from_str(contents).map_err(|e| SearchConfigError::Invalid(e.to_string()))?;
        let config = document.search.unwrap_or_default();
        config.validate()?;
        Ok(config)
    }

    /// Resolve the effective `[search]` section for the **active profile**,
    /// reading the process environment.
    ///
    /// This is what the plugin calls at boot. The pure, testable core is
    /// [`resolve_with_env`](Self::resolve_with_env).
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] when a contributing file cannot
    /// be read or parsed, when the merged `[search]` table has an unknown or
    /// ill-typed key, or when a value is out of range.
    pub fn resolve() -> Result<Self, SearchConfigError> {
        // The OS environment **overlaid with `.env`**, not `OsEnv` alone.
        //
        // Core's loader reads `.env`, `.env.local`, `.env.<profile>` into an
        // overlay `Env` and deliberately does NOT export them into the process
        // environment. A resolver built on `OsEnv` therefore cannot see them
        // at all — so `AUTUMN_SEARCH__ENABLED=false` in a project `.env`, the
        // most ordinary way to set it in development, would leave search on
        // while every other framework setting in the same file took effect.
        //
        // A `.env` that exists but cannot be read or parsed falls back to the
        // real environment rather than failing the whole resolution: core
        // surfaces that error on its own path, and search refusing to boot
        // over it would be a second, more confusing report of one problem.
        autumn_web::dotenv::os_env_with_dotenv().map_or_else(
            |_| Self::resolve_with_env(&autumn_web::config::OsEnv),
            |env| Self::resolve_with_env(&env),
        )
    }

    /// Pure core of [`resolve`](Self::resolve): build the effective `[search]`
    /// section from `env` without touching process-global state.
    ///
    /// Layering mirrors core's own loader (`AutumnConfig::load_with_env`),
    /// restricted to the `[search]` subtree:
    ///
    /// 1. base `autumn.toml` `[search]`,
    /// 2. inline `[profile.<name>.search]` (alias then canonical, so the
    ///    canonical spelling wins),
    /// 3. `autumn-<profile>.toml` `[search]` (first existing file in the
    ///    shared [`profile_override_file_lookup_names`] order wins),
    /// 4. `AUTUMN_SEARCH__*` environment overrides — including values from the
    ///    project's `.env` files, which core overlays onto the environment
    ///    without exporting them into the process.
    ///
    /// Reading only layer 1 — which is what a naive plugin loader does — means
    /// `enabled = false` set under `[profile.prod.search]` is silently ignored
    /// in production. That turns the documented incident kill switch into a
    /// no-op precisely when it is being reached for, which is why the plugin
    /// pays for the full merge rather than a single `read_to_string`.
    ///
    /// Each filename is resolved independently through `AUTUMN_MANIFEST_DIR`
    /// then the working directory, exactly as core's `find_config_file_named`
    /// does — so the plugin reads the same files the host runtime does.
    ///
    /// `env` is core's [`Env`](autumn_web::config::Env) abstraction, **not** a
    /// snapshot of `std::env::vars()`. That distinction is load-bearing:
    /// [`OsEnv`](autumn_web::config::OsEnv) synthesizes `AUTUMN_MANIFEST_DIR`
    /// and `AUTUMN_IS_DEBUG` from the `#[autumn_web::main]` macro context, and
    /// neither is a real process variable. Reading the raw process environment
    /// instead means a release binary launched with no explicit selector is
    /// seen as `dev` here while core resolves `prod` — so `[profile.prod]`
    /// would be skipped — and the app's crate directory is invisible, so a
    /// binary run from anywhere but the crate root misses its own config.
    ///
    /// [`profile_override_file_lookup_names`]: autumn_web::config::profile_override_file_lookup_names
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] when a contributing file cannot
    /// be read or parsed, when the merged `[search]` table has an unknown or
    /// ill-typed key, or when a value is out of range.
    pub fn resolve_with_env(env: &dyn Env) -> Result<Self, SearchConfigError> {
        let (selected_input, canonical) = resolve_active_profile(env);
        let mut merged = toml::Value::Table(toml::map::Map::new());

        // Layer 1: base `autumn.toml`. Absent is fine — a zero-config app.
        let base = read_optional_toml(&find_config_file("autumn.toml", env))?;
        if let Some(base) = &base {
            deep_merge(&mut merged, base.clone());

            // Layer 2: inline `[profile.<name>]`, alias first.
            for name in profile_inline_lookup_names(&canonical) {
                if let Some(section) = profile_section(base, name) {
                    deep_merge(&mut merged, section);
                }
            }
        }

        // Layer 3: `autumn-<profile>.toml`; the first existing file wins.
        for name in
            autumn_web::config::profile_override_file_lookup_names(&canonical, &selected_input)
        {
            let path = find_config_file(&format!("autumn-{name}.toml"), env);
            if let Some(overlay) = read_optional_toml(&path)? {
                deep_merge(&mut merged, overlay);
                break;
            }
        }

        let document: Document = merged
            .try_into()
            .map_err(|e: toml::de::Error| SearchConfigError::Invalid(e.to_string()))?;
        let mut config = document.search.unwrap_or_default();
        // Layer 4: env overrides, highest priority — the same precedence core
        // gives `AUTUMN_SERVER__*`.
        config.apply_env_overrides(env);
        config.validate()?;
        Ok(config)
    }

    /// Apply `AUTUMN_SEARCH__<FIELD>` scalar overrides.
    ///
    /// An unparseable value is **ignored** rather than fatal, matching core's
    /// own `apply_env_overrides`: a malformed env var must not take down a
    /// process that would otherwise boot on its file config.
    fn apply_env_overrides(&mut self, env: &dyn Env) {
        if let Some(queue) = env_trimmed(env, "AUTUMN_SEARCH__QUEUE") {
            self.queue = queue;
        }
        if let Some(value) = env_trimmed(env, "AUTUMN_SEARCH__BATCH_SIZE")
            && let Ok(parsed) = value.parse::<usize>()
        {
            self.batch_size = parsed;
        }
        if let Some(value) = env_trimmed(env, "AUTUMN_SEARCH__ENABLED") {
            match value.to_ascii_lowercase().as_str() {
                "true" | "1" => self.enabled = true,
                "false" | "0" => self.enabled = false,
                _ => {}
            }
        }
        if let Some(value) = env_trimmed(env, "AUTUMN_SEARCH__EMBEDDING_DIMENSIONS")
            && let Ok(parsed) = value.parse::<usize>()
        {
            self.embedding_dimensions = Some(parsed);
        }
    }

    /// Reject values that would misbehave at runtime.
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] for a zero batch size, an empty
    /// queue name, or a zero embedding width.
    pub fn validate(&self) -> Result<(), SearchConfigError> {
        if self.batch_size == 0 {
            return Err(SearchConfigError::Invalid(
                "search.batch_size must be at least 1".to_owned(),
            ));
        }
        if self.queue.trim().is_empty() {
            return Err(SearchConfigError::Invalid(
                "search.queue must not be empty".to_owned(),
            ));
        }
        if self.embedding_dimensions == Some(0) {
            return Err(SearchConfigError::Invalid(
                "search.embedding_dimensions must be at least 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// `toml::from_str` behind a helper so the dependency edge is in one place.
fn toml_from_str<T: serde::de::DeserializeOwned>(contents: &str) -> Result<T, toml::de::Error> {
    toml::from_str(contents)
}

/// Non-blank env value (a blank value reads as unset, as core treats it).
fn env_trimmed(env: &dyn Env, key: &str) -> Option<String> {
    env.var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The `(selected spelling, canonical name)` of the active profile.
///
/// Mirrors core's `resolve_profile`: `AUTUMN_ENV` → `AUTUMN_PROFILE` →
/// `--profile` → `AUTUMN_IS_DEBUG=0` ⇒ `prod` → `dev`. Both halves are needed
/// because the override-file lookup prefers the spelling that was actually
/// selected (`autumn-production.toml` vs `autumn-prod.toml`).
fn resolve_active_profile(env: &dyn Env) -> (String, String) {
    let selected = resolve_profile_input(env);
    let canonical =
        autumn_web::config::normalize_profile_name(&selected).unwrap_or_else(|| "dev".to_owned());
    (selected, canonical)
}

/// The raw profile selector, before normalization.
fn resolve_profile_input(env: &dyn Env) -> String {
    if let Some(value) = env_trimmed(env, "AUTUMN_ENV") {
        return value;
    }
    if let Some(value) = env_trimmed(env, "AUTUMN_PROFILE") {
        return value;
    }
    // `--profile <name>` / `--profile=<name>`: the runtime consults process
    // args too, and `autumn search reindex --profile prod` re-executes the app
    // binary with exactly that flag.
    let args: Vec<String> = std::env::args().collect();
    for (index, arg) in args.iter().enumerate() {
        if arg == "--profile"
            && let Some(profile) = args.get(index.saturating_add(1))
            && !profile.trim().is_empty()
        {
            return profile.trim().to_owned();
        }
        if let Some(profile) = arg.strip_prefix("--profile=")
            && !profile.trim().is_empty()
        {
            return profile.trim().to_owned();
        }
    }
    if env_trimmed(env, "AUTUMN_IS_DEBUG").as_deref() == Some("0") {
        return "prod".to_owned();
    }
    "dev".to_owned()
}

/// Resolve one config filename the way core's `find_config_file_named` does:
/// `AUTUMN_MANIFEST_DIR` when the file is actually there, else relative to the
/// working directory. Per-file, not per-directory — a base `autumn.toml` in the
/// crate root and an `autumn-prod.toml` in the working directory both count.
fn find_config_file(filename: &str, env: &dyn Env) -> PathBuf {
    if let Some(dir) = env_trimmed(env, "AUTUMN_MANIFEST_DIR") {
        let candidate = PathBuf::from(dir).join(filename);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(filename)
}

/// Read a TOML file as a raw value; `Ok(None)` when it does not exist.
///
/// A file that exists but cannot be read is a real problem — a permissions
/// mistake must not read as "zero-config".
fn read_optional_toml(path: &Path) -> Result<Option<toml::Value>, SearchConfigError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let table = toml::from_str::<toml::Table>(&contents).map_err(|e| {
                SearchConfigError::Invalid(format!("cannot parse {}: {e}", path.display()))
            })?;
            Ok(Some(toml::Value::Table(table)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SearchConfigError::Invalid(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

/// Inline `[profile.<name>]` names for a canonical profile, legacy alias first
/// so the canonical spelling merges last and wins. Mirrors core's
/// `profile_lookup_names`.
fn profile_inline_lookup_names(canonical: &str) -> Vec<&str> {
    match canonical {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Lift `[profile.<name>]` out of a parsed document as a standalone table.
fn profile_section(base: &toml::Value, profile: &str) -> Option<toml::Value> {
    base.get("profile")
        .and_then(toml::Value::as_table)
        .and_then(|profiles| profiles.get(profile))
        .and_then(toml::Value::as_table)
        .map(|table| toml::Value::Table(table.clone()))
}

/// Deep-merge `overlay` into `base`: tables merge recursively, everything else
/// replaces. A faithful copy of core's private `deep_merge`, bounded by the
/// same depth, so the plugin's layering matches the runtime's exactly.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    deep_merge_at(base, overlay, 0);
}

fn deep_merge_at(base: &mut toml::Value, overlay: toml::Value, depth: usize) {
    /// Matches core's `MAX_MERGE_DEPTH`.
    const MAX_MERGE_DEPTH: usize = 16;
    if depth > MAX_MERGE_DEPTH {
        return;
    }
    let toml::Value::Table(overlay_table) = overlay else {
        return;
    };
    let Some(base_table) = base.as_table_mut() else {
        return;
    };
    for (key, overlay_value) in overlay_table {
        let recurse =
            overlay_value.is_table() && base_table.get(&key).is_some_and(toml::Value::is_table);
        if recurse {
            if let Some(base_value) = base_table.get_mut(&key) {
                deep_merge_at(base_value, overlay_value, depth.saturating_add(1));
            }
        } else {
            base_table.insert(key, overlay_value);
        }
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::config::MockEnv;

    use super::*;

    #[test]
    fn defaults_route_indexing_to_its_own_queue() {
        let config = SearchConfig::default();
        assert_eq!(config.queue, "search");
        assert_eq!(config.batch_size, 500);
        assert!(config.enabled);
        assert_eq!(config.embedding_dimensions, None);
    }

    #[test]
    fn a_partial_section_keeps_the_other_defaults() {
        let config =
            SearchConfig::from_toml_str("[search]\nqueue = \"indexing\"\n").expect("parse");
        assert_eq!(config.queue, "indexing");
        assert_eq!(config.batch_size, 500);
        assert!(config.enabled);
    }

    #[test]
    fn an_unrelated_document_yields_the_defaults() {
        assert_eq!(
            SearchConfig::from_toml_str("[server]\nport = 3000\n").expect("parse"),
            SearchConfig::default()
        );
        assert_eq!(
            SearchConfig::from_toml_str("").expect("parse"),
            SearchConfig::default()
        );
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_silent_default() {
        assert!(SearchConfig::from_toml_str("[search]\nqueu = \"x\"\n").is_err());
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        assert!(SearchConfig::from_toml_str("[search]\nbatch_size = 0\n").is_err());
        assert!(SearchConfig::from_toml_str("[search]\nqueue = \"\"\n").is_err());
        assert!(SearchConfig::from_toml_str("[search]\nembedding_dimensions = 0\n").is_err());
    }

    #[test]
    fn an_ill_typed_value_is_rejected() {
        assert!(SearchConfig::from_toml_str("[search]\nbatch_size = \"lots\"\n").is_err());
    }

    // ── Profile-aware resolution ────────────────────────────────────────────

    /// A project directory plus the `Env` that points the resolver at it.
    ///
    /// `MockEnv` is core's own test double for the same abstraction the
    /// runtime uses, so these exercise the real resolution path rather than a
    /// parallel one built on `std::env::vars()`.
    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, MockEnv) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).expect("write");
        }
        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", &dir.path().display().to_string());
        (dir, env)
    }

    #[test]
    fn an_absent_config_resolves_to_the_defaults() {
        let (_dir, env) = project(&[]);
        assert_eq!(
            SearchConfig::resolve_with_env(&env).expect("resolve"),
            SearchConfig::default()
        );
    }

    #[test]
    fn the_base_section_resolves_under_the_default_profile() {
        let (_dir, env) = project(&[("autumn.toml", "[search]\nqueue = \"indexing\"\n")]);
        assert_eq!(
            SearchConfig::resolve_with_env(&env).expect("resolve").queue,
            "indexing"
        );
    }

    #[test]
    fn an_inline_profile_section_overrides_the_base() {
        // The kill switch: `enabled = false` under `[profile.prod.search]` has
        // to actually turn search off in prod. Reading only the base section
        // would leave it enabled — with no signal at all.
        let (_dir, mut env) = project(&[(
            "autumn.toml",
            "[search]\nenabled = true\nqueue = \"search\"\n\n\
             [profile.prod.search]\nenabled = false\n",
        )]);

        let dev = SearchConfig::resolve_with_env(&env).expect("dev");
        assert!(dev.enabled, "the base section governs the dev profile");

        env = env.with("AUTUMN_ENV", "prod");
        let prod = SearchConfig::resolve_with_env(&env).expect("prod");
        assert!(!prod.enabled, "[profile.prod.search] must win in prod");
        assert_eq!(
            prod.queue, "search",
            "keys the profile does not mention keep the base value"
        );
    }

    #[test]
    fn a_profile_override_file_overrides_the_inline_section() {
        let (_dir, mut env) = project(&[
            (
                "autumn.toml",
                "[search]\nbatch_size = 100\n\n[profile.prod.search]\nbatch_size = 200\n",
            ),
            ("autumn-prod.toml", "[search]\nbatch_size = 300\n"),
        ]);
        env = env.with("AUTUMN_ENV", "prod");
        assert_eq!(
            SearchConfig::resolve_with_env(&env)
                .expect("resolve")
                .batch_size,
            300
        );
    }

    #[test]
    fn a_section_living_only_in_the_profile_file_is_still_found() {
        // No base `[search]` at all — an app that configures search per
        // environment and nowhere else.
        let (_dir, mut env) = project(&[
            ("autumn.toml", "[server]\nport = 3000\n"),
            ("autumn-prod.toml", "[search]\nenabled = false\n"),
        ]);
        env = env.with("AUTUMN_ENV", "prod");
        assert!(
            !SearchConfig::resolve_with_env(&env)
                .expect("resolve")
                .enabled
        );
    }

    #[test]
    fn the_legacy_production_spelling_resolves_to_the_same_profile() {
        let (_dir, mut env) = project(&[(
            "autumn.toml",
            "[search]\nenabled = true\n\n[profile.production.search]\nenabled = false\n",
        )]);
        env = env.with("AUTUMN_ENV", "production");
        assert!(
            !SearchConfig::resolve_with_env(&env)
                .expect("resolve")
                .enabled
        );
    }

    #[test]
    fn an_env_override_beats_every_file_layer() {
        let (_dir, mut env) = project(&[(
            "autumn.toml",
            "[search]\nenabled = true\n\n[profile.prod.search]\nenabled = true\n",
        )]);
        env = env.with("AUTUMN_ENV", "prod");
        env = env.with("AUTUMN_SEARCH__ENABLED", "false");
        let config = SearchConfig::resolve_with_env(&env).expect("resolve");
        assert!(!config.enabled, "AUTUMN_SEARCH__ENABLED is the last word");

        env = env.with("AUTUMN_SEARCH__QUEUE", "urgent");
        env = env.with("AUTUMN_SEARCH__BATCH_SIZE", "7");
        env = env.with("AUTUMN_SEARCH__EMBEDDING_DIMENSIONS", "384");
        let config = SearchConfig::resolve_with_env(&env).expect("resolve");
        assert_eq!(config.queue, "urgent");
        assert_eq!(config.batch_size, 7);
        assert_eq!(config.embedding_dimensions, Some(384));
    }

    #[test]
    fn a_typo_in_a_profile_section_is_an_error_rather_than_a_silent_default() {
        let (_dir, mut env) = project(&[(
            "autumn.toml",
            "[search]\nenabled = true\n\n[profile.prod.search]\nenabld = false\n",
        )]);
        env = env.with("AUTUMN_ENV", "prod");
        assert!(SearchConfig::resolve_with_env(&env).is_err());
    }

    #[test]
    fn an_out_of_range_env_override_is_still_rejected() {
        let (_dir, mut env) = project(&[("autumn.toml", "[search]\nbatch_size = 100\n")]);
        env = env.with("AUTUMN_SEARCH__BATCH_SIZE", "0");
        assert!(SearchConfig::resolve_with_env(&env).is_err());
    }

    #[test]
    fn an_unparseable_env_override_leaves_the_file_value_alone() {
        // Core's own overrides ignore a malformed value rather than refusing
        // to boot; a process that would run on its file config should.
        let (_dir, mut env) = project(&[("autumn.toml", "[search]\nbatch_size = 100\n")]);
        env = env.with("AUTUMN_SEARCH__BATCH_SIZE", "lots");
        assert_eq!(
            SearchConfig::resolve_with_env(&env)
                .expect("resolve")
                .batch_size,
            100
        );
    }

    #[test]
    fn a_release_binary_with_no_selector_resolves_prod_not_dev() {
        // `AUTUMN_IS_DEBUG` is NOT a real process variable — core's `OsEnv`
        // synthesizes it from the `#[autumn_web::main]` macro context. A
        // resolver reading `std::env::vars()` never sees it, so a release
        // binary launched with no explicit selector reads as `dev` while core
        // reads `prod`, and `[profile.prod.search] enabled = false` is skipped
        // in exactly the environment it was written for.
        let (_dir, env) = project(&[(
            "autumn.toml",
            "[search]\nenabled = true\n\n[profile.prod.search]\nenabled = false\n",
        )]);

        let debug_build = env.clone().with("AUTUMN_IS_DEBUG", "1");
        assert!(
            SearchConfig::resolve_with_env(&debug_build)
                .expect("resolve")
                .enabled,
            "a debug build is `dev`"
        );

        let release_build = env.with("AUTUMN_IS_DEBUG", "0");
        assert!(
            !SearchConfig::resolve_with_env(&release_build)
                .expect("resolve")
                .enabled,
            "a release build with no selector must resolve `prod`, as core does"
        );
    }

    #[test]
    fn an_explicit_selector_still_beats_the_build_mode() {
        let (_dir, env) = project(&[(
            "autumn.toml",
            "[search]\nqueue = \"base\"\n\n[profile.prod.search]\nqueue = \"prod\"\n\n\
             [profile.staging.search]\nqueue = \"staging\"\n",
        )]);
        let env = env
            .with("AUTUMN_IS_DEBUG", "0")
            .with("AUTUMN_ENV", "staging");
        assert_eq!(
            SearchConfig::resolve_with_env(&env).expect("resolve").queue,
            "staging",
            "AUTUMN_ENV outranks the build-mode fallback"
        );
    }

    #[test]
    fn the_manifest_directory_need_not_be_a_process_variable() {
        // Same point for the other synthesized value: `OsEnv` falls back to
        // the macro-supplied crate directory when `AUTUMN_MANIFEST_DIR` is not
        // in the process environment. Going through `Env` is what lets the
        // plugin find the app's config from any working directory.
        let (dir, env) = project(&[("autumn.toml", "[search]\nqueue = \"from-manifest\"\n")]);
        assert_eq!(
            find_config_file("autumn.toml", &env),
            dir.path().join("autumn.toml")
        );
        assert_eq!(
            SearchConfig::resolve_with_env(&env).expect("resolve").queue,
            "from-manifest"
        );
    }

    #[test]
    fn a_custom_profile_reads_its_own_layers_only() {
        let (_dir, mut env) = project(&[
            (
                "autumn.toml",
                "[search]\nqueue = \"base\"\n\n[profile.staging.search]\nqueue = \"staging\"\n\n\
                 [profile.prod.search]\nqueue = \"prod\"\n",
            ),
            ("autumn-prod.toml", "[search]\nqueue = \"prod-file\"\n"),
        ]);
        env = env.with("AUTUMN_ENV", "staging");
        assert_eq!(
            SearchConfig::resolve_with_env(&env).expect("resolve").queue,
            "staging",
            "neither [profile.prod] nor autumn-prod.toml may leak into staging"
        );
    }

    #[test]
    fn file_resolution_prefers_the_manifest_directory_over_the_cwd() {
        // `autumn search reindex --package app` runs the binary from the
        // workspace root, where the CWD holds a different `autumn.toml` (or
        // none). Core resolves through `AUTUMN_MANIFEST_DIR` first; the
        // plugin-owned section must resolve the same way or it reads a
        // different file than the rest of the config.
        let (dir, env) = project(&[("autumn.toml", "[search]\n")]);
        assert_eq!(
            find_config_file("autumn.toml", &env),
            dir.path().join("autumn.toml")
        );
        // A file the manifest dir does NOT hold falls through to the CWD,
        // per-file — core resolves each filename independently.
        assert_eq!(
            find_config_file("autumn-prod.toml", &env),
            PathBuf::from("autumn-prod.toml")
        );
        assert_eq!(
            find_config_file("autumn.toml", &MockEnv::new()),
            PathBuf::from("autumn.toml"),
            "with no manifest dir the CWD is the only candidate"
        );
    }

    #[test]
    fn a_malformed_contributing_file_is_reported_rather_than_ignored() {
        let (_dir, env) = project(&[("autumn.toml", "[search\nqueue = \"x\"\n")]);
        assert!(SearchConfig::resolve_with_env(&env).is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let config = SearchConfig {
            queue: "indexing".to_owned(),
            batch_size: 42,
            enabled: false,
            embedding_dimensions: Some(768),
        };
        let document = format!("[search]\n{}", toml::to_string(&config).expect("serialize"));
        assert_eq!(
            SearchConfig::from_toml_str(&document).expect("parse"),
            config
        );
    }
}
