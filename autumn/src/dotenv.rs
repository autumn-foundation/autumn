//! Auto-loading of a project-root `.env` file in local development.
//!
//! A `.env` file is a **local-dev feeder for the highest configuration
//! layer** — the `AUTUMN_*` environment-variable layer. It does *not*
//! introduce a new precedence tier: `.env`-derived values are layered *under*
//! the real environment via an overlay [`Env`] (`DotenvEnv` /
//! [`DotenvOsEnv`]) rather than mutating the process environment, so a real
//! shell environment variable always wins over the same key in a `.env` file.
//!
//! Files are loaded in order — `.env`, then `.env.local`, then
//! `.env.{profile}`, then `.env.{profile}.local` — and, within that order,
//! earlier files (and any real environment variable) win: a later file only
//! fills keys that are still unset.
//!
//! Auto-loading happens only in the `dev` and `test` profiles. Every other
//! profile (`prod`, `staging`, and any custom profile) skips it unless
//! `AUTUMN_DOTENV=1` (or `true`) is set; conversely it can be disabled anywhere
//! with `AUTUMN_DOTENV=0` (or `false`).
//!
//! There is intentionally **no** `${VAR}` interpolation: values are treated
//! as literal strings (with optional surrounding single or double quotes
//! stripped).
//!
//! The environment/profile **selector** keys (`AUTUMN_ENV`, `AUTUMN_PROFILE`,
//! `AUTUMN_IS_DEBUG`) are intentionally **excluded** from the overlay — a
//! `.env` file must not be able to change the active profile or bypass the
//! prod / destructive-command guards. Set those in the real shell environment
//! or via `--profile`. See `PROFILE_SELECTOR_KEYS`.

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

/// Environment-variable keys that select the active profile / environment and
/// are therefore deliberately **never** honored from a `.env` file.
///
/// Safety: the active profile is a safety-critical bootstrapping decision — it
/// gates the production / destructive-command confirmations in `autumn migrate
/// down`, `autumn db drop`, and `autumn db reset`, and it picks which config
/// overlay (and thus which database) an app targets. A `.env` file must not be
/// able to change it. Were `.env` allowed to set `AUTUMN_ENV=prod`, it could
/// flip a command onto a production target *after* that command's real-env
/// (dev) guard had already passed, bypassing the confirmation flag. Profile
/// selection therefore stays sourced only from the real shell environment and
/// the `--profile` flag — mirroring Rails/Next, where `.env` does not set
/// `RAILS_ENV`/`NODE_ENV`.
///
/// These are exactly the keys [`crate::config::resolve_profile_input`] consults
/// to pick the profile; keep the two lists in sync.
const PROFILE_SELECTOR_KEYS: [&str; 3] = ["AUTUMN_ENV", "AUTUMN_PROFILE", "AUTUMN_IS_DEBUG"];

/// Whether `key` selects the active profile / environment and so must be
/// dropped from the `.env` overlay (see [`PROFILE_SELECTOR_KEYS`]).
fn is_profile_selector_key(key: &str) -> bool {
    PROFILE_SELECTOR_KEYS.contains(&key)
}

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
/// forces it off. Absent that override, only the `dev` and `test` profiles
/// auto-load; every other profile (`prod`, `staging`, and any custom profile)
/// requires `AUTUMN_DOTENV=1`.
fn should_load(profile: &str, env: &dyn Env) -> bool {
    if let Ok(raw) = env.var("AUTUMN_DOTENV") {
        match raw.trim() {
            "1" | "true" => return true,
            "0" | "false" => return false,
            _ => {}
        }
    }
    matches!(profile, "dev" | "test")
}

/// Parse the contents of a single `.env` file into ordered key/value pairs.
///
/// Blank lines and `#` comments are skipped. A leading `export ` prefix is
/// stripped. Keys must match `[A-Za-z_][A-Za-z0-9_]*`. Values are taken after
/// the first `=`, trimmed, with matching surrounding single or double quotes
/// stripped and any inline comment ignored (see [`parse_value`]). There is no
/// `${VAR}` interpolation.
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
        let value = parse_value(value_raw);
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

/// Parse a value, stripping surrounding quotes if present and ignoring inline
/// comments. The caller passes an already-trimmed value.
///
/// If the value starts with `"` or `'`, the inner content up to the matching
/// closing quote is returned (respecting `\`-escapes while scanning) and
/// anything after the closing quote — such as a trailing inline comment — is
/// ignored; a `#` inside the quotes is preserved literally. Otherwise the value
/// is unquoted: the first `#` that is at the start or preceded by ASCII
/// whitespace begins an inline comment and the part before it (trimmed) is
/// returned, while a `#` not preceded by whitespace (e.g. inside
/// `postgres://u:p#ss@h`) is preserved. There is no `${VAR}` interpolation.
fn parse_value(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let bytes = raw.as_bytes();
    let quote = bytes[0];
    if quote == b'"' || quote == b'\'' {
        let mut escaped = false;
        let mut end_idx = None;
        for (i, &b) in bytes.iter().enumerate().skip(1) {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == quote {
                end_idx = Some(i);
                break;
            }
        }
        if let Some(end) = end_idx {
            return raw[1..end].to_owned();
        }
    }
    let comment_start = (0..bytes.len())
        .find(|&i| bytes[i] == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()));
    comment_start.map_or_else(|| raw.to_owned(), |idx| raw[..idx].trim().to_owned())
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
/// Profile/environment selector keys ([`PROFILE_SELECTOR_KEYS`] — `AUTUMN_ENV`,
/// `AUTUMN_PROFILE`, `AUTUMN_IS_DEBUG`) are dropped unconditionally, even when
/// present in `.env`: a `.env` file must not be able to change the active
/// profile or bypass the prod / destructive-command guards. See
/// [`PROFILE_SELECTOR_KEYS`] for the full rationale.
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
            // Safety: `.env` must never select the active profile/environment,
            // so drop the profile-selector keys regardless of the file.
            if is_profile_selector_key(&key) {
                continue;
            }
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

/// The base directory `.env` files are resolved from — the SAME directory the
/// config loader uses for `autumn.toml`.
///
/// `autumn.toml` is located via
/// [`find_config_file_named`](crate::config), which prefers
/// `AUTUMN_MANIFEST_DIR` (the crate root baked in by `#[autumn_web::main]`, or a
/// launch-time override) when set and otherwise falls back to the process
/// working directory. Resolving `.env` from the same base means a binary
/// launched from outside its crate root (e.g. `cargo run -p app` from a
/// workspace root) reads the `.env` next to its `autumn.toml` rather than the
/// process CWD.
///
/// `AUTUMN_MANIFEST_DIR` must be read from the real/base [`Env`] (before the
/// `.env` overlay is built), so the overlay can never redirect its own base
/// directory.
pub(crate) fn dotenv_base_dir(env: &dyn Env) -> PathBuf {
    if let Ok(manifest_dir) = env.var("AUTUMN_MANIFEST_DIR") {
        return PathBuf::from(manifest_dir);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve `.env` vars for the config manifest directory using the real OS env
/// and resolved profile.
///
/// The base directory is `dotenv_base_dir` — `AUTUMN_MANIFEST_DIR` when set,
/// else the process working directory — so `.env` is always read from the same
/// place as `autumn.toml`.
///
/// Returns the `(key, value)` pairs to apply (empty when gating disables
/// loading — see `should_load`).
///
/// # Errors
/// Returns a [`DotenvError`] (with a `path:line` location) on a malformed or
/// unreadable `.env` file.
pub fn resolve_process_dotenv() -> Result<Vec<(String, String)>, DotenvError> {
    let base = OsEnv;
    let profile = resolve_profile(&base);
    let dir = dotenv_base_dir(&base);
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
        overlay: resolve_process_dotenv()?.into_iter().collect(),
    })
}

/// Like [`os_env_with_dotenv`], but selects `.env.<profile>` for an EXPLICIT
/// `profile` instead of the ambient one (which [`resolve_process_dotenv`]
/// derives from `AUTUMN_ENV`/`AUTUMN_PROFILE`).
///
/// CLI paths that resolve config under a caller-forced profile — e.g.
/// `autumn db backup --profile <p>` or an `offsite:<p>/…` restore ref — must load
/// `.env.<p>` even when the ambient shell profile differs, otherwise a
/// profile-specific `.env.<p>` (offsite overrides, credential values) is silently
/// missed. Real environment variables still win over `.env`, and `.env`
/// profile-selector keys are still ignored, exactly as in [`os_env_with_dotenv`].
///
/// # Errors
/// Returns a [`DotenvError`] if a project-root `.env` file exists but cannot be
/// read or parsed.
pub fn os_env_with_dotenv_for_profile(profile: &str) -> Result<DotenvOsEnv, DotenvError> {
    let base = OsEnv;
    let dir = dotenv_base_dir(&base);
    Ok(DotenvOsEnv {
        overlay: resolve_dotenv_vars(&dir, profile, &base)?
            .into_iter()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MockEnv;
    use std::path::PathBuf;

    #[test]
    fn dotenv_profile_selection_reproduces_offsite_wrong_profile_bug() {
        // Reproduction for issue #1619 P2 #6: an offsite override + credential
        // live ONLY in `.env.test`. With the ambient shell profile unset (default
        // "dev"), resolving `.env` for "dev" MISSES `.env.test` — the bug the
        // offsite path hit by feeding `dotenv_env()` the ambient profile.
        // Resolving for the TARGET profile "test" (the fix) picks them up.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env.test"),
            "AUTUMN_BACKUP__OFFSITE__S3__BUCKET=from-dot-env-test\n\
             AUTUMN_OFFSITE_SECRET=sekret\n",
        )
        .unwrap();
        let ambient = MockEnv::new(); // AUTUMN_ENV / AUTUMN_PROFILE unset

        // Buggy path: the ambient/default profile "dev" does NOT load `.env.test`.
        let dev: std::collections::HashMap<_, _> = resolve_dotenv_vars(dir.path(), "dev", &ambient)
            .unwrap()
            .into_iter()
            .collect();
        assert!(
            !dev.contains_key("AUTUMN_BACKUP__OFFSITE__S3__BUCKET"),
            "reproduction: dev profile must miss the .env.test offsite override"
        );
        assert!(!dev.contains_key("AUTUMN_OFFSITE_SECRET"));

        // Fixed path: resolving for the target profile "test" loads `.env.test`,
        // so both the offsite override and the credential value are honored.
        let test: std::collections::HashMap<_, _> =
            resolve_dotenv_vars(dir.path(), "test", &ambient)
                .unwrap()
                .into_iter()
                .collect();
        assert_eq!(
            test.get("AUTUMN_BACKUP__OFFSITE__S3__BUCKET")
                .map(String::as_str),
            Some("from-dot-env-test")
        );
        assert_eq!(
            test.get("AUTUMN_OFFSITE_SECRET").map(String::as_str),
            Some("sekret")
        );
    }

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
    fn strips_unquoted_inline_comment() {
        let out = parse_dotenv(&p(), "KEY=val # c\n").unwrap();
        assert_eq!(out[0], ("KEY".to_owned(), "val".to_owned()));
    }

    #[test]
    fn strips_trailing_comment_after_quoted_value() {
        let out = parse_dotenv(&p(), "KEY=\"a b\" # c\n").unwrap();
        assert_eq!(out[0], ("KEY".to_owned(), "a b".to_owned()));
    }

    #[test]
    fn hash_inside_quotes_is_preserved() {
        let out = parse_dotenv(&p(), "KEY=\"a#b\"\n").unwrap();
        assert_eq!(out[0], ("KEY".to_owned(), "a#b".to_owned()));
    }

    #[test]
    fn hash_not_preceded_by_whitespace_is_not_a_comment() {
        let out = parse_dotenv(&p(), "KEY=a#b\n").unwrap();
        assert_eq!(out[0], ("KEY".to_owned(), "a#b".to_owned()));
    }

    #[test]
    fn hash_in_unquoted_connection_string_is_preserved() {
        let out = parse_dotenv(&p(), "KEY=postgres://u:p#w@h/db\n").unwrap();
        assert_eq!(
            out[0],
            ("KEY".to_owned(), "postgres://u:p#w@h/db".to_owned())
        );
    }

    #[test]
    fn empty_value_is_empty_string() {
        let out = parse_dotenv(&p(), "KEY=\n").unwrap();
        assert_eq!(out[0], ("KEY".to_owned(), String::new()));
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
    fn gating_test_profile_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "test", &env).unwrap();
        assert_eq!(out, vec![("FOO".to_owned(), "bar".to_owned())]);
    }

    #[test]
    fn gating_custom_profile_does_not_load_by_default() {
        // A non-prod custom profile such as `staging` must NOT auto-load `.env`
        // without the explicit opt-in — only `dev`/`test` auto-load.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "staging", &env).unwrap();
        assert!(
            out.is_empty(),
            "a custom profile must not auto-load .env by default"
        );
    }

    #[test]
    fn gating_custom_profile_loads_with_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FOO=bar\n").unwrap();
        let env = MockEnv::new().with("AUTUMN_DOTENV", "1");
        let out = resolve_dotenv_vars(dir.path(), "staging", &env).unwrap();
        assert_eq!(out, vec![("FOO".to_owned(), "bar".to_owned())]);
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
    fn dotenv_env_prod_does_not_flip_resolved_profile() {
        // End-to-end guard: a `.env`-provided `AUTUMN_ENV=prod` must NOT change
        // the profile the overlay resolves to. `resolve_profile` is exactly the
        // selector `autumn migrate` / config load consult, so this proves the
        // prod/destructive guards can't be flipped by `.env`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "AUTUMN_ENV=prod\nAUTUMN_DATABASE__URL=postgres://localhost/app\n",
        )
        .unwrap();
        let base = MockEnv::new();
        let vars = resolve_dotenv_vars(dir.path(), "dev", &base).unwrap();
        let env = DotenvEnv::new(&base, vars);
        // The selector is not honored from `.env`, so the profile stays `dev`.
        assert_eq!(crate::config::resolve_profile(&env), "dev");
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
    fn profile_selector_keys_are_dropped_from_dotenv() {
        // Safety: `.env` must never select the active profile/environment.
        // `AUTUMN_ENV`, `AUTUMN_PROFILE`, and `AUTUMN_IS_DEBUG` are dropped even
        // when present in `.env`, while ordinary keys still flow through.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "AUTUMN_ENV=prod\n\
             AUTUMN_PROFILE=prod\n\
             AUTUMN_IS_DEBUG=0\n\
             AUTUMN_DATABASE__URL=postgres://localhost/app\n",
        )
        .unwrap();
        let env = MockEnv::new();
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert!(
            !map.contains_key("AUTUMN_ENV"),
            "AUTUMN_ENV must not be honored from .env"
        );
        assert!(
            !map.contains_key("AUTUMN_PROFILE"),
            "AUTUMN_PROFILE must not be honored from .env"
        );
        assert!(
            !map.contains_key("AUTUMN_IS_DEBUG"),
            "AUTUMN_IS_DEBUG must not be honored from .env"
        );
        // Non-selector keys are unaffected.
        assert_eq!(
            map.get("AUTUMN_DATABASE__URL").map(String::as_str),
            Some("postgres://localhost/app")
        );
    }

    #[test]
    fn real_env_profile_selector_is_unaffected_by_exclusion() {
        // Excluding the selector keys from `.env` must not touch the REAL
        // environment: a real-env `AUTUMN_ENV` remains readable through the
        // base env even though a `.env`-provided one is dropped.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "AUTUMN_ENV=prod\nOTHER=x\n").unwrap();
        let env = MockEnv::new().with("AUTUMN_ENV", "dev");
        let out = resolve_dotenv_vars(dir.path(), "dev", &env).unwrap();
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        // The `.env` selector is dropped, not surfaced.
        assert!(!map.contains_key("AUTUMN_ENV"));
        // The real-env value is still what callers observe.
        assert_eq!(env.var("AUTUMN_ENV").unwrap(), "dev");
        assert_eq!(map.get("OTHER").map(String::as_str), Some("x"));
    }

    #[test]
    fn dotenv_base_dir_prefers_manifest_dir() {
        // The `.env` base directory tracks config's `autumn.toml` base:
        // AUTUMN_MANIFEST_DIR wins over the process CWD when set.
        let dir = tempfile::tempdir().unwrap();
        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());
        assert_eq!(dotenv_base_dir(&env), dir.path().to_path_buf());
    }

    #[test]
    fn dotenv_base_dir_falls_back_to_cwd_when_unset() {
        let env = MockEnv::new();
        let expected = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        assert_eq!(dotenv_base_dir(&env), expected);
    }

    #[test]
    fn resolve_reads_env_from_manifest_dir_not_cwd() {
        // With AUTUMN_MANIFEST_DIR pointing at a temp dir that contains a `.env`,
        // resolution reads THAT file — proving the base directory is the manifest
        // dir, not the process working directory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FROM_MANIFEST=yes\n").unwrap();
        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());
        let base = dotenv_base_dir(&env);
        assert_eq!(base, dir.path().to_path_buf());
        let out = resolve_dotenv_vars(&base, "dev", &env).unwrap();
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert_eq!(map.get("FROM_MANIFEST").map(String::as_str), Some("yes"));
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
