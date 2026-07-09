//! Auto-loading of a project-root `.env` file in local development.
//!
//! A `.env` file is a **local-dev feeder for the highest configuration
//! layer** — the `AUTUMN_*` environment-variable layer. It does *not*
//! introduce a new precedence tier: `.env`-derived values are layered *under*
//! the real environment via an overlay [`Env`] ([`DotenvEnv`] /
//! [`DotenvOsEnv`]) rather than mutating the process environment, so a real
//! shell environment variable always wins over the same key in a `.env` file.
//!
//! Files are loaded in order — `.env`, then `.env.local`, then
//! `.env.{profile}`, then `.env.{profile}.local` — and, within that order,
//! earlier files (and any real environment variable) win: a later file only
//! fills keys that are still unset.
//!
//! Auto-loading happens in the `dev` and `test` profiles. It is skipped in
//! `prod` unless `AUTUMN_DOTENV=1` (or `true`) is set; conversely it can be
//! disabled anywhere with `AUTUMN_DOTENV=0` (or `false`).
//!
//! There is intentionally **no** `${VAR}` interpolation: values are treated
//! as literal strings (with optional surrounding single or double quotes
//! stripped).

use std::path::{Path, PathBuf};

use crate::config::{Env, OsEnv, resolve_profile};

/// An error encountered while reading or parsing a `.env` file.
///
/// The [`Display`](std::fmt::Display) form is `"{path}:{line}: {message}"`,
/// mirroring the compiler-style `file:line: message` convention so the
/// offending location is obvious.
#[derive(Debug)]
pub struct DotenvError {
    /// The `.env` file the error came from.
    pub path: PathBuf,
    /// The 1-based line number the error occurred on (`0` for whole-file
    /// errors such as an I/O failure).
    pub line: usize,
    /// A human-readable description of what went wrong.
    pub message: String,
}

impl std::fmt::Display for DotenvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.path.display(), self.line, self.message)
    }
}

impl std::error::Error for DotenvError {}

/// The ordered list of candidate `.env` files for a given profile.
///
/// Order encodes precedence: earlier files win (a later file only fills keys
/// that are still unset).
fn candidate_files(profile: &str) -> Vec<String> {
    vec![
        ".env".to_owned(),
        ".env.local".to_owned(),
        format!(".env.{profile}"),
        format!(".env.{profile}.local"),
    ]
}

/// Decide whether `.env` auto-loading should run for `profile`.
///
/// `AUTUMN_DOTENV=1`/`true` forces loading on; `AUTUMN_DOTENV=0`/`false`
/// forces it off. Absent that override, every profile except `prod` loads.
fn should_load(profile: &str, env: &dyn Env) -> bool {
    if let Ok(raw) = env.var("AUTUMN_DOTENV") {
        match raw.trim() {
            "1" | "true" => return true,
            "0" | "false" => return false,
            _ => {}
        }
    }
    profile != "prod"
}

/// Parse the contents of a single `.env` file into ordered key/value pairs.
///
/// Blank lines and `#` comments are skipped. A leading `export ` prefix is
/// stripped. Keys must match `[A-Za-z_][A-Za-z0-9_]*`. Values are taken
/// verbatim after the first `=`, trimmed, with matching surrounding single or
/// double quotes stripped. There is no `${VAR}` interpolation.
///
/// # Errors
/// Returns a [`DotenvError`] (with the correct 1-based line number) for a line
/// with no `=`, or a syntactically invalid key.
fn parse_dotenv(path: &Path, contents: &str) -> Result<Vec<(String, String)>, DotenvError> {
    let mut out = Vec::new();
    for (idx, raw_line) in contents.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Strip an optional leading `export ` prefix.
        let body = trimmed
            .strip_prefix("export ")
            .unwrap_or(trimmed)
            .trim_start();

        let Some(eq) = body.find('=') else {
            return Err(DotenvError {
                path: path.to_path_buf(),
                line: line_no,
                message: format!("expected KEY=VALUE, found `{raw_line}`"),
            });
        };

        let key = body[..eq].trim();
        if !is_valid_key(key) {
            return Err(DotenvError {
                path: path.to_path_buf(),
                line: line_no,
                message: format!("invalid variable name `{key}` in `{raw_line}`"),
            });
        }

        let value_raw = body[eq + 1..].trim();
        let value = strip_quotes(value_raw);
        out.push((key.to_owned(), value));
    }
    Ok(out)
}

/// A key is valid when it is non-empty, starts with an ASCII letter or `_`,
/// and otherwise contains only ASCII alphanumerics or `_`.
fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Strip a single matching pair of surrounding single or double quotes, if
/// present. The content is otherwise returned verbatim (no interpolation).
fn strip_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

/// Resolve the merged `.env` variable set for a directory and profile.
///
/// Returns an empty vector when loading is disabled (see [`should_load`]).
/// Otherwise reads each candidate file in order; a missing file is skipped
/// (not an error), any other read error becomes a [`DotenvError`]. Within the
/// merge, a key already present in the real environment is dropped entirely
/// (real env always wins), and the first file to set a key wins over later
/// files.
///
/// # Errors
/// Returns a [`DotenvError`] if a candidate file exists but cannot be read, or
/// if a present file fails to parse.
pub(crate) fn resolve_dotenv_vars(
    dir: &Path,
    profile: &str,
    env: &dyn Env,
) -> Result<Vec<(String, String)>, DotenvError> {
    if !should_load(profile, env) {
        return Ok(Vec::new());
    }

    let mut merged: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for filename in candidate_files(profile) {
        let path = dir.join(&filename);
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(DotenvError {
                    path,
                    line: 0,
                    message: format!("failed to read: {e}"),
                });
            }
        };

        for (key, value) in parse_dotenv(&path, &contents)? {
            // Real environment variables always win.
            if env.var(&key).is_ok() {
                continue;
            }
            // Earlier file / already-seen key wins.
            if seen.insert(key.clone()) {
                merged.push((key, value));
            }
        }
    }

    Ok(merged)
}

/// An [`Env`] that overlays `.env`-derived values UNDER a base environment:
/// the base (real process env) is consulted first; overlay values only fill
/// keys the base does not define. Feeds the `AUTUMN_*` env-var layer from
/// `.env` without mutating the process environment.
pub(crate) struct DotenvEnv<'a> {
    base: &'a dyn Env,
    overlay: std::collections::BTreeMap<String, String>,
}

impl<'a> DotenvEnv<'a> {
    /// Wrap `base`, layering `vars` (from [`resolve_dotenv_vars`]) underneath it.
    pub(crate) fn new(base: &'a dyn Env, vars: Vec<(String, String)>) -> Self {
        Self {
            base,
            overlay: vars.into_iter().collect(),
        }
    }
}

impl Env for DotenvEnv<'_> {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        // Base (real env) wins; the overlay only fills keys the base lacks.
        self.base.var(key).or_else(|_| {
            self.overlay
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        })
    }
}

/// Owning overlay over the real OS environment, for CLI callers that cannot
/// borrow a local [`OsEnv`]. The real environment wins; `.env`-derived values
/// only fill keys the OS environment does not define.
pub struct DotenvOsEnv {
    overlay: std::collections::BTreeMap<String, String>,
}

impl Env for DotenvOsEnv {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        // Real OS env wins; the overlay only fills keys the OS env lacks.
        OsEnv.var(key).or_else(|_| {
            self.overlay
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        })
    }
}

/// Resolve `.env` vars for the current working directory using the real OS env
/// and resolved profile.
///
/// Returns the `(key, value)` pairs to apply (empty when gating disables
/// loading — see [`should_load`]). If the current directory cannot be
/// determined, resolution falls back to `.`.
///
/// # Errors
/// Returns a [`DotenvError`] (with a `path:line` location) on a malformed or
/// unreadable `.env` file.
pub fn resolve_dotenv_for_cwd() -> Result<Vec<(String, String)>, DotenvError> {
    let base = OsEnv;
    let profile = resolve_profile(&base);
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_dotenv_vars(&dir, &profile, &base)
}

/// The real OS environment overlaid with `.env` values, for CLI resolution that
/// must consult `.env` (e.g. DB URL resolution in `autumn migrate`).
///
/// # Errors
/// Returns a [`DotenvError`] if a project-root `.env` file exists but cannot be
/// read or parsed.
pub fn os_env_with_dotenv() -> Result<DotenvOsEnv, DotenvError> {
    Ok(DotenvOsEnv {
        overlay: resolve_dotenv_for_cwd()?.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MockEnv;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("/tmp/.env")
    }

    #[test]
    fn parses_simple_pairs() {
        let out = parse_dotenv(&p(), "FOO=bar\nBAZ=qux\n").unwrap();
        assert_eq!(
            out,
            vec![
                ("FOO".to_owned(), "bar".to_owned()),
                ("BAZ".to_owned(), "qux".to_owned()),
            ]
        );
    }

    #[test]
    fn strips_matching_quotes() {
        let out = parse_dotenv(
            &p(),
            "A=\"double quoted\"\nB='single quoted'\nC=unquoted value\n",
        )
        .unwrap();
        assert_eq!(out[0], ("A".to_owned(), "double quoted".to_owned()));
        assert_eq!(out[1], ("B".to_owned(), "single quoted".to_owned()));
        assert_eq!(out[2], ("C".to_owned(), "unquoted value".to_owned()));
    }

    #[test]
    fn no_interpolation_of_dollar_vars() {
        let out = parse_dotenv(&p(), "A=${FOO}/literal\n").unwrap();
        assert_eq!(out[0], ("A".to_owned(), "${FOO}/literal".to_owned()));
    }

    #[test]
    fn skips_comments_blanks_and_handles_export() {
        let contents = "\
# a comment
   # indented comment

FOO=bar
export BAZ=qux
   export SPACED=value
";
        let out = parse_dotenv(&p(), contents).unwrap();
        assert_eq!(
            out,
            vec![
                ("FOO".to_owned(), "bar".to_owned()),
                ("BAZ".to_owned(), "qux".to_owned()),
                ("SPACED".to_owned(), "value".to_owned()),
            ]
        );
    }

    #[test]
    fn malformed_line_reports_correct_line_and_path() {
        let contents = "FOO=bar\nNOEQUALS\nBAZ=qux\n";
        let err = parse_dotenv(&p(), contents).unwrap_err();
        assert_eq!(err.line, 2);
        assert_eq!(err.path, p());
        assert!(err.message.contains("KEY=VALUE"), "got: {}", err.message);
        // Display form is file:line: message.
        assert!(err.to_string().contains("/tmp/.env:2:"), "{err}");
    }

    #[test]
    fn invalid_key_is_an_error() {
        let contents = "FOO=ok\n1BAD=nope\n";
        let err = parse_dotenv(&p(), contents).unwrap_err();
        assert_eq!(err.line, 2);
        assert!(
            err.message.contains("invalid variable name"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn missing_files_are_ok_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn file_order_earlier_wins() {
        let dir = tempfile::tempdir().unwrap();
        // `.env` sets KEY=from_env and ONLY_BASE.
        std::fs::write(dir.path().join(".env"), "KEY=from_env\nONLY_BASE=base\n").unwrap();
        // `.env.local` tries to override KEY and adds ONLY_LOCAL.
        std::fs::write(
            dir.path().join(".env.local"),
            "KEY=from_local\nONLY_LOCAL=local\n",
        )
        .unwrap();
        // `.env.dev` tries to override KEY and adds ONLY_DEV.
        std::fs::write(dir.path().join(".env.dev"), "KEY=from_dev\nONLY_DEV=dev\n").unwrap();

        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        // Earlier file wins for overlapping keys.
        assert_eq!(map.get("KEY").map(String::as_str), Some("from_env"));
        assert_eq!(map.get("ONLY_BASE").map(String::as_str), Some("base"));
        assert_eq!(map.get("ONLY_LOCAL").map(String::as_str), Some("local"));
        assert_eq!(map.get("ONLY_DEV").map(String::as_str), Some("dev"));
    }

    #[test]
    fn real_env_always_wins() {
        // AC #2 precedence test: a key already set in the real environment is
        // excluded from the resolved `.env` set entirely.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "AUTUMN_DATABASE__URL=postgres://from-dotenv\nOTHER=x\n",
        )
        .unwrap();

        let env = MockEnv::new().with("AUTUMN_DATABASE__URL", "postgres://from-real-env");
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert!(
            !map.contains_key("AUTUMN_DATABASE__URL"),
            "real env var must win and be excluded from the .env result"
        );
        assert_eq!(map.get("OTHER").map(String::as_str), Some("x"));
    }

    #[test]
    fn bare_database_url_key_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "DATABASE_URL=postgres://localhost/app\n",
        )
        .unwrap();
        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert_eq!(
            map.get("DATABASE_URL").map(String::as_str),
            Some("postgres://localhost/app")
        );
    }

    #[test]
    fn read_error_becomes_dotenv_error() {
        let dir = tempfile::tempdir().unwrap();
        // Create `.env` as a directory so reading it as a file errors (not
        // NotFound), which must surface as a DotenvError.
        std::fs::create_dir(dir.path().join(".env")).unwrap();
        let env = MockEnv::new();
        let err = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap_err();
        assert_eq!(err.line, 0);
        assert!(
            err.message.contains("failed to read"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn gating_prod_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "prod", &env).unwrap();
        assert!(out.is_empty(), "prod must not auto-load .env");
    }

    #[test]
    fn gating_prod_loads_with_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
        let env = MockEnv::new().with("AUTUMN_DOTENV", "1");
        let out = resolve_dotenv_vars(dir.path(), "prod", &env).unwrap();
        assert_eq!(out, vec![("FOO".to_owned(), "bar".to_owned())]);
    }

    #[test]
    fn gating_dev_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        assert_eq!(out, vec![("FOO".to_owned(), "bar".to_owned())]);
    }

    #[test]
    fn gating_dev_disabled_with_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
        let env = MockEnv::new().with("AUTUMN_DOTENV", "0");
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        assert!(out.is_empty(), "AUTUMN_DOTENV=0 must disable loading");
    }

    #[test]
    fn candidate_file_order() {
        assert_eq!(
            candidate_files("dev"),
            vec![".env", ".env.local", ".env.dev", ".env.dev.local"]
        );
    }

    #[test]
    fn dotenv_env_base_wins_over_overlay() {
        // Real env (base) defines the key: base value must win.
        let base = MockEnv::new().with("AUTUMN_DATABASE__URL", "postgres://from-real-env");
        let env = DotenvEnv::new(
            &base,
            vec![(
                "AUTUMN_DATABASE__URL".to_owned(),
                "postgres://from-dotenv".to_owned(),
            )],
        );
        assert_eq!(
            env.var("AUTUMN_DATABASE__URL").unwrap(),
            "postgres://from-real-env"
        );
    }

    #[test]
    fn dotenv_env_overlay_fills_unset_key() {
        // Base does not define the key: overlay value fills it.
        let base = MockEnv::new();
        let env = DotenvEnv::new(
            &base,
            vec![(
                "AUTUMN_DATABASE__URL".to_owned(),
                "postgres://from-dotenv".to_owned(),
            )],
        );
        assert_eq!(
            env.var("AUTUMN_DATABASE__URL").unwrap(),
            "postgres://from-dotenv"
        );
    }

    #[test]
    fn dotenv_env_miss_is_not_present() {
        let base = MockEnv::new();
        let env = DotenvEnv::new(&base, vec![("FOO".to_owned(), "bar".to_owned())]);
        assert_eq!(env.var("MISSING"), Err(std::env::VarError::NotPresent));
    }

    #[test]
    fn dotenv_os_env_base_wins_over_overlay() {
        // A real OS env var must win over the overlay value for the same key.
        // Uses a key that is essentially never set in the test environment,
        // set locally for the duration of this test via `temp_env`.
        let overlay = std::collections::BTreeMap::from([(
            "AUTUMN_TEST_DOTENV_OVERLAY_KEY".to_owned(),
            "from-dotenv".to_owned(),
        )]);
        let env = DotenvOsEnv { overlay };
        temp_env::with_var(
            "AUTUMN_TEST_DOTENV_OVERLAY_KEY",
            Some("from-real-env"),
            || {
                assert_eq!(
                    env.var("AUTUMN_TEST_DOTENV_OVERLAY_KEY").unwrap(),
                    "from-real-env"
                );
            },
        );
    }

    #[test]
    fn dotenv_os_env_overlay_fills_unset_key() {
        let overlay = std::collections::BTreeMap::from([(
            "AUTUMN_TEST_DOTENV_OVERLAY_UNSET".to_owned(),
            "from-dotenv".to_owned(),
        )]);
        let env = DotenvOsEnv { overlay };
        temp_env::with_var_unset("AUTUMN_TEST_DOTENV_OVERLAY_UNSET", || {
            assert_eq!(
                env.var("AUTUMN_TEST_DOTENV_OVERLAY_UNSET").unwrap(),
                "from-dotenv"
            );
        });
    }

    #[test]
    fn dotenv_resolves_into_config_via_overlay() {
        // End-to-end: a `.env` value flows into `config.database.url` through
        // the overlay `Env`, with no process-environment mutation.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "AUTUMN_DATABASE__URL=postgres://localhost/from-dotenv\n",
        )
        .unwrap();

        let base = MockEnv::new();
        let vars = resolve_dotenv_vars(dir.path(), "dev", &base).unwrap();
        let env = DotenvEnv::new(&base, vars);
        let config = crate::config::AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(
            config.database.url.as_deref(),
            Some("postgres://localhost/from-dotenv")
        );
    }

    #[test]
    fn real_env_wins_over_dotenv_end_to_end() {
        // A real env var of the same key must win over the `.env` value when
        // resolved end-to-end into config.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "AUTUMN_DATABASE__URL=postgres://localhost/from-dotenv\n",
        )
        .unwrap();

        let base =
            MockEnv::new().with("AUTUMN_DATABASE__URL", "postgres://localhost/from-real-env");
        let vars = resolve_dotenv_vars(dir.path(), "dev", &base).unwrap();
        let env = DotenvEnv::new(&base, vars);
        let config = crate::config::AutumnConfig::load_with_env(&env).unwrap();
        assert_eq!(
            config.database.url.as_deref(),
            Some("postgres://localhost/from-real-env")
        );
    }
}
