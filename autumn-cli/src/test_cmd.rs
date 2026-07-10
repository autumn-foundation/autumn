//! `autumn test` — provision the test database, migrate it, then run the suite.
//!
//! `autumn test` is a thin, safety-first wrapper around `cargo test` for
//! projects that talk to a real Postgres database. It:
//!
//!   1. Resolves the **test-profile** database URL the exact same way
//!      `autumn migrate` does (`autumn.toml` → `AUTUMN_DATABASE__*` →
//!      `DATABASE_URL`), forcing the `test` profile (`AUTUMN_ENV=test`).
//!   2. Derives a **test** database from that URL: if the resolved database
//!      name is not already test-shaped (`test` or `*_test`), it appends
//!      `_test` — so a bare base URL of `…/myapp` targets `…/myapp_test`.
//!   3. Creates that database if it is missing (leaving existing data intact
//!      unless `--reset` drops and recreates it first).
//!   4. Runs all pending app + framework migrations against it, reusing the
//!      existing `autumn migrate` path (no separate migration engine).
//!   5. Shells out to `cargo test` with `AUTUMN_ENV=test` and the resolved
//!      test `DATABASE_URL` exported, forwarding any trailing arguments.
//!
//! The command refuses to run against a database whose name cannot be made
//! test-shaped, mirroring the production-safety guardrails of `autumn migrate`
//! / `autumn db`. Its exit code is the `cargo test` exit code, so a failing
//! suite fails the command.

use std::process::Command;

use crate::migrate;

/// Entry point dispatched from `main`. Provisions and migrates the test
/// database, then execs `cargo test`, and never returns: it always exits with
/// either a non-zero setup-failure code or the exact `cargo test` exit code.
pub fn run(reset: bool, cargo_args: &[String]) -> ! {
    eprintln!("\u{1F342} autumn test\n");

    // 1. Resolve the base URL under the `test` profile using the same
    //    precedence chain as `autumn migrate` (autumn.toml → AUTUMN_DATABASE__*
    //    → DATABASE_URL). Forcing the `test` profile means an `autumn-test.toml`
    //    / `[profile.test.database]` overlay is honored automatically.
    let Some(base_url) = migrate::resolve_primary_url(Some("test")) else {
        eprintln!("\u{2717} No test database URL found.");
        eprintln!(
            "  Set database.primary_url (or database.url) in autumn.toml (optionally under \
             [profile.test.database]), or set AUTUMN_DATABASE__PRIMARY_URL / \
             AUTUMN_DATABASE__URL / DATABASE_URL."
        );
        std::process::exit(1);
    };

    // 2. Derive the test database URL, defaulting the name to `*_test`. This is
    //    also the guard (AC-6): a URL whose database name cannot be made
    //    test-shaped is refused here rather than provisioned.
    let (test_url, db_name) = match derive_test_url(&base_url) {
        Ok(derived) => derived,
        Err(e) => {
            eprintln!("\u{2717} {e}");
            std::process::exit(1);
        }
    };
    eprintln!("  Test database: {db_name:?}");
    eprintln!("  Profile:       test (AUTUMN_ENV=test)\n");

    // 3. Provision the database, reusing the existing `autumn db` paths by
    //    self-invoking this same binary with the derived test URL pinned via
    //    the highest-precedence env var (mirrors `autumn db reset`).
    if reset {
        // Clean slate: drop first (AC-5), then create below. `--force` is
        // redundant under the test profile (the destructive guard already
        // allows `test`) but mirrors `db reset` and stays correct even if the
        // child resolves the profile differently.
        run_step("drop", &["db", "drop", "--force"], &test_url);
    }
    // Create-if-missing; idempotent — without `--reset` an existing database is
    // left intact (AC-2), and after a `--reset` drop this recreates it fresh.
    run_step("create", &["db", "create"], &test_url);

    // 4. Apply all pending app + framework migrations, reusing `autumn migrate`
    //    (no separate migration engine).
    run_step("migrate", &["migrate"], &test_url);

    // 5. Shell out to `cargo test` with the test environment exported and the
    //    trailing args forwarded verbatim, then exit with its exact code.
    eprintln!("\u{2500}\u{2500} cargo test \u{2500}\u{2500}");
    let status = Command::new("cargo")
        .arg("test")
        .args(cargo_args)
        .env("AUTUMN_ENV", "test")
        .env("DATABASE_URL", &test_url)
        .env("AUTUMN_DATABASE__PRIMARY_URL", &test_url)
        .status();

    match status {
        Ok(status) => {
            // Propagate the exact `cargo test` exit code (AC-7). A process
            // terminated by a signal has no code; treat that as a failure.
            std::process::exit(status.code().unwrap_or(1));
        }
        Err(e) => {
            eprintln!("\u{2717} Failed to run `cargo test`: {e}");
            std::process::exit(1);
        }
    }
}

/// Run one provisioning step by self-invoking the `autumn` binary with the test
/// environment pinned. Exits non-zero (naming the step) if the child fails.
fn run_step(name: &str, args: &[&str], test_url: &str) {
    eprintln!("\u{2500}\u{2500} {name} \u{2500}\u{2500}");
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("\u{2717} Could not locate the autumn executable: {e}");
        std::process::exit(1);
    });
    let status = Command::new(exe)
        .args(args)
        .env("AUTUMN_ENV", "test")
        .env("AUTUMN_DATABASE__PRIMARY_URL", test_url)
        .env("DATABASE_URL", test_url)
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!(
                "\u{2717} `autumn {}` failed ({}).",
                args.join(" "),
                status.code().map_or_else(
                    || "terminated by signal".to_owned(),
                    |c| format!("exit {c}")
                ),
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("\u{2717} Failed to spawn `autumn {}`: {e}", args.join(" "));
            std::process::exit(1);
        }
    }
}

/// Whether a database name already satisfies the test-database convention:
/// exactly `test`, or any name ending in `_test`.
fn is_test_db_name(name: &str) -> bool {
    name == "test" || name.ends_with("_test")
}

/// Assert that a database name is test-shaped, producing a credential-free,
/// `migrate`-toned refusal otherwise (AC-6).
fn ensure_test_db_name(name: &str) -> Result<(), String> {
    if is_test_db_name(name) {
        Ok(())
    } else {
        Err(format!(
            "Refusing to run tests against the non-test database {name:?}.\n  \
             `autumn test` only operates on a database named `test` or ending in `_test`."
        ))
    }
}

/// Derive the `(test_url, test_db_name)` pair from a resolved base URL.
///
/// The database name is the first path segment (percent-decoded, mirroring
/// `db::maintenance_target`). When it is not already test-shaped, `_test` is
/// appended — so a base URL of `…/myapp` yields `…/myapp_test` (AC-1's `_test`
/// defaulting). The derived name is then guarded to be test-shaped (AC-6).
///
/// Returns `Err` (a human-readable message) when the URL cannot be parsed or
/// names no database — such a target cannot be made test-safe, so the command
/// refuses rather than guessing.
fn derive_test_url(base_url: &str) -> Result<(String, String), String> {
    let mut parsed = url::Url::parse(base_url)
        .map_err(|_| "The resolved test database URL could not be parsed.".to_owned())?;

    let name = parsed
        .path_segments()
        .and_then(|mut segments| segments.next())
        .map(decode_percent)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            "The resolved test database URL does not name a database (nothing to make test-safe)."
                .to_owned()
        })?;

    let test_name = if is_test_db_name(&name) {
        name
    } else {
        format!("{name}_test")
    };
    // Defensive guard — appending `_test` always satisfies the convention, but
    // assert it explicitly so any future change to the derivation can't silently
    // target a non-test database.
    ensure_test_db_name(&test_name)?;

    parsed.set_path(&format!("/{test_name}"));
    Ok((parsed.to_string(), test_name))
}

/// Minimal percent-decoding for a URL path segment (database name), matching
/// `db::maintenance_target`'s handling so the derived name equals the name the
/// `db`/`migrate` paths will operate on.
fn decode_percent(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo)
                && let Ok(byte) = u8::try_from(hi * 16 + lo)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_test_db_name_accepts_test_and_underscore_test() {
        assert!(is_test_db_name("test"));
        assert!(is_test_db_name("myapp_test"));
        assert!(is_test_db_name("a_b_c_test"));
    }

    #[test]
    fn is_test_db_name_rejects_non_test_names() {
        assert!(!is_test_db_name("myapp"));
        assert!(!is_test_db_name("production"));
        assert!(!is_test_db_name("testing")); // does not end in `_test`
        assert!(!is_test_db_name("test_db"));
    }

    #[test]
    fn ensure_test_db_name_refuses_non_test_name() {
        let err = ensure_test_db_name("production").unwrap_err();
        assert!(err.contains("Refusing"), "message: {err}");
        assert!(err.contains("production"), "message: {err}");
        assert!(err.contains("_test"), "message: {err}");
    }

    #[test]
    fn ensure_test_db_name_allows_test_shaped_names() {
        assert!(ensure_test_db_name("test").is_ok());
        assert!(ensure_test_db_name("myapp_test").is_ok());
    }

    #[test]
    fn derive_appends_test_suffix_to_bare_name() {
        let (url, name) = derive_test_url("postgres://user:pw@db.example.com:6543/myapp").unwrap();
        assert_eq!(name, "myapp_test");
        assert!(url.ends_with("/myapp_test"), "url: {url}");
        // Host, port, and credentials are preserved.
        assert!(url.contains("db.example.com:6543"), "url: {url}");
    }

    #[test]
    fn derive_keeps_already_test_shaped_names() {
        let (url, name) = derive_test_url("postgres://localhost/myapp_test").unwrap();
        assert_eq!(name, "myapp_test");
        assert!(url.ends_with("/myapp_test"), "url: {url}");

        let (url, name) = derive_test_url("postgres://localhost/test").unwrap();
        assert_eq!(name, "test");
        assert!(url.ends_with("/test"), "url: {url}");
    }

    #[test]
    fn derive_decodes_percent_encoded_name_before_suffixing() {
        let (_url, name) = derive_test_url("postgres://localhost/my%5Fapp").unwrap();
        // %5F is `_`; decoded name is `my_app`, which is not test-shaped.
        assert_eq!(name, "my_app_test");
    }

    #[test]
    fn derive_refuses_url_without_database_name() {
        let err = derive_test_url("postgres://localhost/").unwrap_err();
        assert!(err.contains("does not name a database"), "message: {err}");
    }

    #[test]
    fn derive_refuses_unparsable_url() {
        let err = derive_test_url("not a url").unwrap_err();
        assert!(err.contains("could not be parsed"), "message: {err}");
    }
}
