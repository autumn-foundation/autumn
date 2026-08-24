//! Integration tests for `autumn deploy` (issue #1607).
//!
//! Exercise the locally-verifiable spine: `deploy plan` renders the systemd
//! unit and the ordered step list, `deploy check` fails fast with an actionable
//! message when `[deploy] host` is unset, and the group exposes `--help`.

use std::fs;

use crate::common::{run_autumn, run_autumn_fail};
use tempfile::TempDir;

/// A minimal project directory with a `Cargo.toml` (for the package-name
/// default) and an `autumn.toml` containing the given `[deploy]` body.
fn project(deploy_body: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(
        dir.path().join("autumn.toml"),
        format!("[deploy]\n{deploy_body}"),
    )
    .expect("write autumn.toml");
    dir
}

#[test]
fn deploy_plan_prints_unit_and_steps() {
    let dir = project("host = \"203.0.113.10\"\n");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["deploy", "plan"], &[]);
    assert_eq!(
        code,
        Some(0),
        "deploy plan should succeed\nstderr:\n{stderr}"
    );

    // Renders the systemd unit with the resolved paths and an EnvironmentFile
    // (secrets are never inlined into the unit). ExecStart runs the uploaded
    // standalone app binary at the `current` symlink directly — NOT
    // `autumn serve --release` (which would rebuild from source).
    assert!(
        stdout.contains("ExecStart=/srv/autumn/demoapp/current/demoapp"),
        "stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("autumn serve --release"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("WantedBy=multi-user.target"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("EnvironmentFile=/srv/autumn/demoapp/shared/autumn.env"),
        "stdout:\n{stdout}"
    );

    // Renders the ordered deploy steps, with migrations before cutover.
    let migrate = stdout.find("[migrate]").expect("migrate step present");
    let cutover = stdout.find("[cutover]").expect("cutover step present");
    assert!(
        migrate < cutover,
        "migrations must precede cutover\nstdout:\n{stdout}"
    );
    assert!(stdout.contains("[readiness-gate]"), "stdout:\n{stdout}");
    assert!(stdout.contains("[prune]"), "stdout:\n{stdout}");
}

#[test]
fn deploy_check_fails_fast_without_host() {
    // A bare [deploy] table has no host; check must fail with an actionable
    // message naming the key to set, and exit non-zero.
    //
    // #1621: the fleet spelling (`[deploy] hosts`) EXTENDS this message rather
    // than replacing it — the literal `[deploy] host` substring is quoted in
    // operator runbooks, so it must survive verbatim while the message also
    // offers the fleet alternative.
    let dir = project("");
    let (stdout, stderr) = run_autumn_fail(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("host"),
        "check should mention the missing host\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("[deploy] host"),
        "check should name the config key\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("hosts"),
        "check should also offer the #1621 fleet spelling\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_rejects_host_and_hosts_configured_together() {
    // #1621 (AC-1): `[deploy] host` and `[deploy] hosts` are mutually exclusive —
    // with both set there is no unambiguous rollout order, so the CLI refuses
    // before any remote work, naming BOTH keys so the operator knows which one to
    // delete.
    let dir = project("host = \"203.0.113.10\"\nhosts = [\"web-1.example.com\"]\n");
    let (stdout, stderr) = run_autumn_fail(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("[deploy] host"),
        "the refusal must name the legacy key\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("[deploy] hosts"),
        "the refusal must name the fleet key\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_rejects_a_blank_hosts_entry() {
    // #1621 (AC-1): a blank fleet entry is a typo that would otherwise resolve to
    // a hostless SSH target mid-rollout. The refusal names the 0-based index so
    // the operator can find the offending line.
    let dir = project("hosts = [\"web-1.example.com\", \"  \"]\n");
    let (stdout, stderr) = run_autumn_fail(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("[deploy] hosts"),
        "the refusal must name the fleet key\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains('1'),
        "the refusal must name the 0-based index of the blank entry\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_rejects_duplicate_hosts_entries() {
    // #1621 (AC-1/AC-3): deploying the same machine twice corrupts its blue/green
    // previous-release chain, which a fleet rollback depends on. Duplicates are
    // compared after trimming and the repeated value is named.
    let dir = project("hosts = [\"web-1.example.com\", \" web-1.example.com \"]\n");
    let (stdout, stderr) = run_autumn_fail(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("web-1.example.com"),
        "the refusal must name the repeated host\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("[deploy] hosts"),
        "the refusal must name the fleet key\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn doctor_fails_offline_on_deploy_without_host() {
    // A `[deploy]` table with no host makes `autumn deploy check` fail
    // immediately, so default/OFFLINE `autumn doctor` (no `--online`) must fail
    // on it too — the host-present validation runs offline, only the TCP probe
    // is gated behind `--online`.
    let dir = project("");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["doctor"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert_ne!(
        code,
        Some(0),
        "offline doctor must fail on a hostless [deploy]\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("deploy_host"),
        "doctor must surface the offline deploy_host check\n{combined}"
    );
    assert!(
        combined.contains("[deploy] host"),
        "doctor must name the config key to set\n{combined}"
    );
}

#[test]
fn doctor_reads_deploy_host_from_dotenv() {
    // Regression: doctor must layer .env like `deploy check` (Codex round-10 P2)
    // — bare OsEnv would skip this. With NO `[deploy]` in autumn.toml and the
    // deploy host supplied ONLY via `AUTUMN_DEPLOY__HOST` in a `.env` file, the
    // profile-aware dotenv overlay must materialize the deploy config so the
    // `deploy_host` preflight RUNS (and passes) instead of being skipped — which
    // is exactly what happens if doctor resolves through a bare `OsEnv`.
    let dir = tempfile::tempdir().expect("create temp project dir");
    // Package name for the app-name default; deliberately NO `[deploy]` section
    // in autumn.toml so that, absent dotenv, the deploy preflight is skipped.
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(dir.path().join("autumn.toml"), "").expect("write autumn.toml");
    // Host arrives ONLY through `.env` — never the process env — so the test
    // proves the dotenv overlay path, not an OS-env read.
    fs::write(
        dir.path().join(".env"),
        "AUTUMN_DEPLOY__HOST=deploy.example.test\n",
    )
    .expect("write .env");

    // `AUTUMN_DOTENV=1` force-loads `.env` regardless of the resolved profile so
    // the test is deterministic; it does NOT carry the deploy host itself.
    let (stdout, stderr, _code) = run_autumn(dir.path(), &["doctor"], &[("AUTUMN_DOTENV", "1")]);
    let combined = format!("{stdout}{stderr}");
    // Passing check renders as `✅ deploy_host — deploy target host is configured`
    // (see `format_check_line` / `grade_deploy_host_present`). Its presence means
    // the env-only host materialized a deploy config and the preflight ran.
    assert!(
        combined.contains("deploy_host — deploy target host is configured"),
        "doctor must resolve the .env-only deploy host and run the deploy_host \
         check (bare OsEnv would skip it)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn doctor_reads_deploy_signing_secret_from_dotenv() {
    // Regression: doctor must read the deploy signing secret through .env like
    // `deploy check` (Codex P2 on 2dc71f7); bare OsEnv would report it missing
    // under --strict. With a `[deploy]` host (so the deploy preflight runs) and
    // a STRONG `AUTUMN_SECURITY__SIGNING_SECRET` supplied ONLY via `.env`, the
    // production-mode `deploy_signing_secret` grader must PASS — resolving the
    // secret through the profile-aware dotenv overlay, not a bare `OsEnv` that
    // would see no secret and fail the check.
    let dir = project("host = \"deploy.example.test\"\n");
    // 64 hex chars (>= MIN_SECRET_LEN, not a known demo value): a valid
    // production secret. Delivered ONLY through `.env`, never the process env.
    fs::write(
        dir.path().join(".env"),
        "AUTUMN_SECURITY__SIGNING_SECRET=\
         0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
    )
    .expect("write .env");

    // `AUTUMN_ENV=production` (a profile selector, allowed only from the real
    // env) puts the signing-secret grader in production mode where a strong
    // secret is required; `AUTUMN_DOTENV=1` force-loads `.env`. The secret
    // itself is NOT in the process env.
    let (stdout, stderr, _code) = run_autumn(
        dir.path(),
        &["doctor"],
        &[("AUTUMN_ENV", "production"), ("AUTUMN_DOTENV", "1")],
    );
    let combined = format!("{stdout}{stderr}");
    // Passing check renders as `✅ deploy_signing_secret — signing secret is
    // configured` (see `format_check_line` / `grade_signing_secret`).
    assert!(
        combined.contains("deploy_signing_secret — signing secret is configured"),
        "doctor must resolve the .env-only deploy signing secret and pass the \
         production deploy_signing_secret check (bare OsEnv would report it \
         missing)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn doctor_fails_on_malformed_previous_secrets() {
    // Regression: doctor silently DROPPED a malformed
    // `security.signing_secret.previous_secrets` (e.g. `[123]` or a non-array)
    // via `as_array`/`filter_map`, so `deploy_signing_secret` PASSED with a
    // strong current secret — but `autumn deploy check` loads via
    // `AutumnConfig::load()`, which deserializes the field as `Vec<String>` and
    // HARD-FAILS the same config on every profile. Doctor must surface a FAILING
    // `deploy_signing_secret` check to match. A strong current secret is supplied
    // via env so the failure is attributable to the malformed `previous_secrets`,
    // not a missing current secret.
    let dir =
        project("host = \"203.0.113.10\"\n\n[security.signing_secret]\nprevious_secrets = [123]\n");
    let secret = "a".repeat(64);
    let (stdout, stderr, code) = run_autumn(
        dir.path(),
        &["doctor"],
        &[("AUTUMN_SECURITY__SIGNING_SECRET", secret.as_str())],
    );
    let combined = format!("{stdout}{stderr}");
    assert_ne!(
        code,
        Some(0),
        "doctor must fail on a malformed previous_secrets\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("deploy_signing_secret"),
        "doctor must surface the deploy_signing_secret check\n{combined}"
    );
    assert!(
        combined.contains("previous_secrets is present but invalid"),
        "doctor must report the malformed previous_secrets\n{combined}"
    );
}

#[test]
fn doctor_grades_deploy_signing_secret_against_deploy_profile() {
    // Regression: doctor must grade the DEPLOY signing secret against the resolved
    // `[deploy] profile` (default `prod`), NOT the ambient CLI runtime profile.
    // On a dev box with the ambient profile dev (no `AUTUMN_ENV=production`) and a
    // WEAK/demo deploy signing secret, the OLD behavior graded `deploy_signing_secret`
    // with the ambient (non-production) flag and PASSED it — even though
    // `autumn deploy check`/`deploy up` grade against the deploy profile (`prod`)
    // and FAIL the same weak secret. Doctor and `deploy check` must agree: the
    // check must FAIL here.
    let dir = project("host = \"deploy.example.test\"\n");
    // A known demo/template value ("changeme") — invalid under production grading.
    // Delivered via the process env (resolved env-first like `deploy check`).
    // Crucially, `AUTUMN_ENV=production` is NOT set, so the ambient profile is dev:
    // only the deploy-profile-aware grade can catch this weak secret.
    let (stdout, stderr, code) = run_autumn(
        dir.path(),
        &["doctor"],
        &[("AUTUMN_SECURITY__SIGNING_SECRET", "changeme")],
    );
    let combined = format!("{stdout}{stderr}");
    assert_ne!(
        code,
        Some(0),
        "doctor must fail on a weak deploy signing secret under the default (prod) \
         deploy profile, even with an ambient dev profile\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("deploy_signing_secret"),
        "doctor must surface the deploy_signing_secret check\n{combined}"
    );
    assert!(
        combined.contains("known demo/template value not allowed in production"),
        "doctor must fail the deploy signing secret as a known demo value under the \
         deploy profile\n{combined}"
    );
    // The demo secret value must never be echoed.
    assert!(
        !combined.contains("changeme"),
        "doctor must not echo the demo secret value\n{combined}"
    );
}

#[test]
fn doctor_flags_dotenv_db_backed_runtime_without_db() {
    // Regression: `.env`-only postgres backend must make doctor require a deploy
    // DB, matching `deploy check` (Codex P2 on 2dc71f7); bare OsEnv wrongly
    // passed. With a `[deploy]` host, `AUTUMN_JOBS__BACKEND=postgres` supplied
    // ONLY via `.env`, and NO database URL / `[database]` / migrations dir
    // anywhere, the db-backed runtime must be detected from `.env` so the
    // `deploy_database_url` grader FAILS (a Postgres-backed jobs runtime needs a
    // writable pool). A bare `OsEnv` would miss the `.env` backend and pass.
    let dir = project("host = \"deploy.example.test\"\n");
    fs::write(dir.path().join(".env"), "AUTUMN_JOBS__BACKEND=postgres\n").expect("write .env");

    // `AUTUMN_DOTENV=1` force-loads `.env`; no DATABASE_URL / AUTUMN_DATABASE__URL
    // is set anywhere, so the grader has no writable target.
    let (stdout, stderr, _code) = run_autumn(dir.path(), &["doctor"], &[("AUTUMN_DOTENV", "1")]);
    let combined = format!("{stdout}{stderr}");
    // Failing check renders as `❌ deploy_database_url — no writable database
    // URL: ...` (see `format_check_line` / `grade_database_url`).
    assert!(
        combined.contains("deploy_database_url — no writable database URL"),
        "the .env-only postgres jobs backend must make doctor require a deploy \
         DB and fail deploy_database_url (bare OsEnv would pass)\nstdout:\n\
         {stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn doctor_reads_deploy_db_url_from_dotenv() {
    // Regression: doctor must read the .env-only deploy DB URL like `deploy check`
    // (Codex P2 on 5a12eb3); bare OsEnv reported it missing. With a `[deploy]`
    // host (so the deploy preflight runs), a Postgres-backed jobs runtime AND the
    // writable database URL supplied ONLY via `.env` — never the process env —
    // the `deploy_database_url` grader must PASS, resolving the URL through the
    // profile-aware dotenv overlay just as `AutumnConfig::load()`/`deploy check`
    // do. A bare `OsEnv` would see no URL and fail the check.
    let dir = project("host = \"deploy.example.test\"\n");
    // Postgres jobs backend makes the app db-backed; the URL is the writable
    // target. BOTH arrive only through `.env`, never the process env.
    fs::write(
        dir.path().join(".env"),
        "AUTUMN_JOBS__BACKEND=postgres\n\
         AUTUMN_DATABASE__URL=postgres://u:p@db.example.test/app\n",
    )
    .expect("write .env");

    // `AUTUMN_DOTENV=1` force-loads `.env`; the DB URL itself is NOT in the
    // process env, so a pass proves the dotenv overlay path.
    let (stdout, stderr, _code) = run_autumn(dir.path(), &["doctor"], &[("AUTUMN_DOTENV", "1")]);
    let combined = format!("{stdout}{stderr}");
    // Passing check renders as `✅ deploy_database_url — database URL is
    // configured` (see `format_check_line` / `grade_database_url`).
    assert!(
        combined.contains("deploy_database_url — database URL is configured"),
        "doctor must resolve the .env-only deploy DB URL and pass \
         deploy_database_url (bare OsEnv would report it missing)\nstdout:\n\
         {stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn doctor_ignores_tuning_only_deploy_database_table() {
    // Regression: a tuning-only `[database]` (no url/shards/migrations) must pass
    // deploy_database_url, matching `deploy check` (Codex P2 on 5a12eb3); mere
    // `[database]` table presence wrongly failed. doctor now derives
    // db-configured from resolved URL/shard presence (like `deploy.rs`
    // `collect_preflight`), so a `[database]` carrying only tuning keys
    // (`pool_size`) with no writable URL and no migrations dir is treated as
    // DB-free and PASSES.
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    // `[deploy]` host so the preflight runs; `[database]` with ONLY a tuning key,
    // no url/primary_url/replica_url/shards, and no migrations dir.
    fs::write(
        dir.path().join("autumn.toml"),
        "[deploy]\nhost = \"deploy.example.test\"\n\n[database]\npool_size = 10\n",
    )
    .expect("write autumn.toml");

    let (stdout, stderr, _code) = run_autumn(dir.path(), &["doctor"], &[]);
    let combined = format!("{stdout}{stderr}");
    // DB-free pass renders as `✅ deploy_database_url — no database configured
    // (nothing to connect to)` (see `format_check_line` / `grade_database_url`).
    assert!(
        combined.contains("deploy_database_url — no database configured (nothing to connect to)"),
        "a tuning-only [database] must pass deploy_database_url as DB-free, \
         matching deploy check\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // And it must NOT surface the missing-URL failure.
    assert!(
        !combined.contains("deploy_database_url — no writable database URL"),
        "a tuning-only [database] must not trigger the missing-URL failure\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_grades_and_uploads_profile_scoped_prod_values() {
    // Regression (P1 follow-up to #1956): `autumn deploy` must reload its config
    // under the TARGET deploy profile (default `prod`), so a signing secret and a
    // DB URL that live ONLY under `[profile.prod]` are loaded and graded — instead
    // of the operator's ambient/dev config. Here the ambient profile is dev (no
    // `AUTUMN_ENV`), the base/dev signing secret is a weak demo value, and the
    // strong prod secret + prod DB URL exist ONLY under `[profile.prod]`.
    //
    // Under the OLD behavior (config loaded under the ambient dev profile), the
    // signing-secret grader would see the weak `changeme`, grade it as production
    // (per #1956), and FAIL; and `database_url` would report "no database
    // configured" because the dev config has no DB. With the fix, the reload under
    // `prod` loads the strong prod secret (PASS) and the prod DB URL (configured).
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    // 64 hex chars (>= MIN_SECRET_LEN, not a known demo value): a valid production
    // secret, present ONLY under `[profile.prod]`. The base/dev secret is a known
    // demo value that would FAIL production grading if it leaked through.
    fs::write(
        dir.path().join("autumn.toml"),
        "[deploy]\n\
         host = \"deploy.example.test\"\n\
         \n\
         [security.signing_secret]\n\
         secret = \"changeme\"\n\
         \n\
         [profile.prod.database]\n\
         primary_url = \"postgres://prod:pw@proddb.internal/app\"\n\
         \n\
         [profile.prod.security.signing_secret]\n\
         secret = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
    )
    .expect("write autumn.toml");

    // No `AUTUMN_ENV` → the ambient CLI profile is dev; only the deploy-profile
    // reload can surface the prod-only values.
    let (stdout, stderr, _code) = run_autumn(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");

    // The strong PROD signing secret is graded (and passes) — the weak base secret
    // never leaks into the grade.
    assert!(
        combined.contains("signing_secret: signing secret is configured"),
        "deploy check must load and pass the PROD-only signing secret\nstdout:\n\
         {stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("known demo/template value"),
        "the weak base/dev secret must not be graded (it would fail as a demo \
         value)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("changeme"),
        "the demo secret value must never be echoed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The PROD-only DB URL is loaded, so the DB check sees a configured database
    // rather than reporting the dev config's "no database configured".
    assert!(
        combined.contains("database_url: database URL is configured"),
        "deploy check must load the PROD-only database URL\nstdout:\n{stdout}\n\
         stderr:\n{stderr}"
    );
    assert!(
        !combined.contains("no database configured"),
        "the dev config's DB-free state must not surface once reloaded under \
         prod\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_loads_prod_values_from_dotenv_file() {
    // Regression (Gemini review on #1966): `autumn deploy` must load the target
    // deploy profile's `.env.<profile>` file even from a dev shell. `.env.prod`
    // is a documented place for prod-only values (the signing secret, the DB
    // URL), but dotenv auto-loading is gated OFF for non-`dev`/`test` profiles
    // unless `AUTUMN_DOTENV=1` — so before the fix the deploy-time reload
    // silently SKIPPED `.env.prod` and graded the weak dev secret / reported no
    // DB. The fix has `ForcedProfileEnv` report `AUTUMN_DOTENV=1` to the dotenv
    // gating base (without mutating the global env), so `.env.prod` loads.
    //
    // Here the strong prod secret and prod DB URL live ONLY in a `.env.prod`
    // FILE (NOT in `autumn.toml` `[profile.prod]`), the ambient profile is dev
    // (no `AUTUMN_ENV`), and the test does NOT set `AUTUMN_DOTENV` in its own
    // env — proving it works out of the box.
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    // Base config: deploy host + a weak/demo dev signing secret that would FAIL
    // production grading if it leaked through. No `[profile.prod]` section and no
    // database — the prod values come exclusively from `.env.prod` below.
    fs::write(
        dir.path().join("autumn.toml"),
        "[deploy]\n\
         host = \"deploy.example.test\"\n\
         \n\
         [security.signing_secret]\n\
         secret = \"changeme\"\n",
    )
    .expect("write autumn.toml");
    // Prod-only values live ONLY in `.env.prod` via the highest (`AUTUMN_*`)
    // config layer. `AUTUMN_ENV=dev` here is a profile-SELECTOR key: the dotenv
    // overlay strips it unconditionally, so it can never flip the active profile
    // (safety gate preserved).
    fs::write(
        dir.path().join(".env.prod"),
        "AUTUMN_ENV=dev\n\
         AUTUMN_SECURITY__SIGNING_SECRET=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n\
         AUTUMN_DATABASE__PRIMARY_URL=postgres://prod:pw@proddb.internal/app\n",
    )
    .expect("write .env.prod");

    // No `AUTUMN_ENV` / `AUTUMN_DOTENV` in the test env: the deploy-profile
    // reload must opt into `.env.prod` loading on its own.
    let (stdout, stderr, _code) = run_autumn(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");

    // The strong PROD signing secret from `.env.prod` is loaded and graded
    // (passes) — the weak base secret never leaks into the grade.
    assert!(
        combined.contains("signing_secret: signing secret is configured"),
        "deploy check must load and pass the PROD signing secret from .env.prod\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("known demo/template value"),
        "the weak base/dev secret must not be graded (it would fail as a demo \
         value)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("changeme"),
        "the demo secret value must never be echoed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The PROD DB URL from `.env.prod` is loaded, so the DB check sees a
    // configured database rather than the dev config's "no database configured".
    assert!(
        combined.contains("database_url: database URL is configured"),
        "deploy check must load the PROD database URL from .env.prod\nstdout:\n\
         {stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("no database configured"),
        "the DB-free base state must not surface once .env.prod is loaded\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_loads_dotenv_for_production_alias() {
    // Regression (Codex P2 on #1966): the `.env.<profile>` overlay must be
    // selected by the CANONICAL deploy profile, not the operator's raw
    // `[deploy] profile` spelling. A `production` (or `PROD`) alias normalizes to
    // the canonical `prod` in `AutumnConfig::load()`, so the prod-only values
    // live in `.env.prod`. Before the fix, the deploy-time reload passed the RAW
    // `production` alias to the dotenv overlay and looked for `.env.production`,
    // silently missing `.env.prod` and grading the weak dev secret / no DB.
    //
    // Here `[deploy] profile = "production"` (the ALIAS), the strong prod secret
    // and prod DB URL live ONLY in `.env.prod`, the ambient profile is dev (no
    // `AUTUMN_ENV`), and the test does NOT set `AUTUMN_DOTENV` — proving the
    // canonical `.env.prod` is selected despite the `production` alias.
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    // Base config: deploy host + a weak/demo dev signing secret that would FAIL
    // production grading if it leaked through. `[deploy] profile` is the
    // `production` ALIAS (not the canonical `prod`). No `[profile.*]` section and
    // no database — the prod values come exclusively from `.env.prod` below.
    fs::write(
        dir.path().join("autumn.toml"),
        "[deploy]\n\
         host = \"deploy.example.test\"\n\
         profile = \"production\"\n\
         \n\
         [security.signing_secret]\n\
         secret = \"changeme\"\n",
    )
    .expect("write autumn.toml");
    // Prod-only values live ONLY in the CANONICAL `.env.prod` (NOT
    // `.env.production`), matching how `AutumnConfig::load()` reads them after
    // profile normalization.
    fs::write(
        dir.path().join(".env.prod"),
        "AUTUMN_ENV=dev\n\
         AUTUMN_SECURITY__SIGNING_SECRET=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n\
         AUTUMN_DATABASE__PRIMARY_URL=postgres://prod:pw@proddb.internal/app\n",
    )
    .expect("write .env.prod");

    // No `AUTUMN_ENV` / `AUTUMN_DOTENV` in the test env: the deploy-profile
    // reload must opt into the canonical `.env.prod` on its own.
    let (stdout, stderr, _code) = run_autumn(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");

    // The strong PROD signing secret from `.env.prod` is loaded and graded
    // (passes) even though `[deploy] profile` is the `production` alias.
    assert!(
        combined.contains("signing_secret: signing secret is configured"),
        "deploy check must load and pass the PROD signing secret from .env.prod \
         via the canonical `prod` selection\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("known demo/template value"),
        "the weak base/dev secret must not be graded (it would fail as a demo \
         value)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("changeme"),
        "the demo secret value must never be echoed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The PROD DB URL from `.env.prod` is loaded, so the DB check sees a
    // configured database rather than the base config's "no database configured".
    assert!(
        combined.contains("database_url: database URL is configured"),
        "deploy check must load the PROD database URL from the canonical \
         .env.prod\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains("no database configured"),
        "the DB-free base state must not surface once .env.prod is loaded via \
         the canonical selection\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_respects_explicit_autumn_dotenv_off() {
    // Regression (Codex P2 on #1966): an operator who runs
    // `AUTUMN_DOTENV=0 autumn deploy ...` deliberately opts OUT of
    // `.env.<profile>` loading. Before the fix `ForcedProfileEnv` synthesized
    // `AUTUMN_DOTENV=1` UNCONDITIONALLY, overriding the explicit `0`, so
    // `.env.prod` loaded anyway. The fix delegates to the inner env when it
    // provides `AUTUMN_DOTENV`, preserving the explicit off switch.
    //
    // Here the strong prod secret + prod DB URL live ONLY in `.env.prod`, but the
    // deploy child env sets `AUTUMN_DOTENV=0`, so `.env.prod` must NOT load: the
    // weak base `changeme` secret is graded (and flagged as a demo value) and the
    // DB-free base state surfaces instead.
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(
        dir.path().join("autumn.toml"),
        "[deploy]\n\
         host = \"deploy.example.test\"\n\
         \n\
         [security.signing_secret]\n\
         secret = \"changeme\"\n",
    )
    .expect("write autumn.toml");
    fs::write(
        dir.path().join(".env.prod"),
        "AUTUMN_ENV=dev\n\
         AUTUMN_SECURITY__SIGNING_SECRET=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n\
         AUTUMN_DATABASE__PRIMARY_URL=postgres://prod:pw@proddb.internal/app\n",
    )
    .expect("write .env.prod");

    // Explicit `AUTUMN_DOTENV=0` in the deploy child env opts OUT of dotenv.
    let (stdout, stderr, _code) =
        run_autumn(dir.path(), &["deploy", "check"], &[("AUTUMN_DOTENV", "0")]);
    let combined = format!("{stdout}{stderr}");

    // The prod signing secret from `.env.prod` must NOT be loaded: the weak base
    // `changeme` is graded and flagged as a demo/template value.
    assert!(
        combined.contains("known demo/template value"),
        "with AUTUMN_DOTENV=0 the weak base secret must be graded (a demo value), \
         proving .env.prod was NOT loaded\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The prod DB URL from `.env.prod` must NOT be loaded: the DB-free base state
    // surfaces instead.
    assert!(
        combined.contains("no database configured"),
        "with AUTUMN_DOTENV=0 the base config's DB-free state must surface, \
         proving .env.prod was NOT loaded\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The prod-only DB URL never appears.
    assert!(
        !combined.contains("proddb.internal"),
        "the prod DB URL from .env.prod must not be loaded when AUTUMN_DOTENV=0\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// The single fleet-wide migrate-placement sentence `deploy plan` prints for a
/// multi-host `[deploy] hosts` (issue #1621, AC-4). Kept in sync by hand with
/// `deploy::fleet::FLEET_MIGRATE_PLACEMENT_NOTE`; the unit-level drift guard is
/// `fleet_plan_matches_fleet_ops_sequence`.
const MIGRATE_PLACEMENT_NOTE: &str =
    "runs once, on the first host in rollout order, before its cutover";

#[test]
fn deploy_plan_renders_the_fleet_rollout_order_and_one_migrate_note() {
    // #1621 (AC-4, T2.2): `deploy plan` is offline — it contacts no host, so it
    // cannot know which hosts are first deploys. It therefore prints the rollout
    // ORDER (declaration order, the documented contract) plus the migrate
    // placement as a single fleet-wide RULE, never a per-host line.
    let dir =
        project("hosts = [\"web-1.example.com\", \"web-2.example.com\", \"web-3.example.com\"]\n");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["deploy", "plan"], &[]);
    assert_eq!(
        code,
        Some(0),
        "deploy plan should succeed for a fleet\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let first = stdout
        .find("web-1.example.com")
        .unwrap_or_else(|| panic!("web-1 must be listed\nstdout:\n{stdout}"));
    let second = stdout
        .find("web-2.example.com")
        .unwrap_or_else(|| panic!("web-2 must be listed\nstdout:\n{stdout}"));
    let third = stdout
        .find("web-3.example.com")
        .unwrap_or_else(|| panic!("web-3 must be listed\nstdout:\n{stdout}"));
    assert!(
        first < second && second < third,
        "the fleet plan must list hosts in declaration (rollout) order\nstdout:\n{stdout}"
    );

    assert_eq!(
        stdout.matches(MIGRATE_PLACEMENT_NOTE).count(),
        1,
        "the fleet plan must carry exactly one migrate-placement note\nstdout:\n{stdout}"
    );
}

/// The grader names `deploy check` printed, in order, read off its report lines
/// (`✅ name: detail`, `❌ name (host): detail`, `⚠️  name: detail`).
///
/// Scope suffixes are stripped, so this returns the STABLE identifiers — which is
/// exactly the set `autumn doctor` must mirror (issue #1621, plan §5.5).
fn preflight_grader_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("\u{2705} ")
                .or_else(|| line.strip_prefix("\u{274C} "))
                .or_else(|| line.strip_prefix("\u{26A0}\u{FE0F}  "))?;
            let name = rest.split(':').next()?.trim();
            // Drop the ` (host)` scope suffix a fleet row carries.
            let name = name.split(" (").next()?.trim();
            (!name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
                .then(|| name.to_owned())
        })
        .collect()
}

#[test]
fn deploy_check_reports_ssh_reachability_per_fleet_host() {
    // #1621 (T2.3, AC-7): every host is graded BEFORE anything is touched, and each
    // per-host row names its host — otherwise "cannot reach the server" in a
    // three-host fleet does not say which server. The three project-wide graders
    // (signing secret, database URL, migrate check) grade the PROJECT, so they run
    // once and stay unscoped.
    //
    // `.test` addresses never resolve (RFC 2606), so the ssh grader fails fast per
    // host with no network dependence.
    let dir = project(
        "hosts = [\"web-1.example.test\", \"web-2.example.test\", \"web-3.example.test\"]\n",
    );
    let (stdout, stderr) = run_autumn_fail(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");

    for host in [
        "web-1.example.test",
        "web-2.example.test",
        "web-3.example.test",
    ] {
        assert!(
            combined.contains(&format!("ssh_reachability ({host})")),
            "every fleet host needs its own scoped ssh_reachability row; missing \
             {host}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    let names = preflight_grader_names(&combined);
    assert_eq!(
        names
            .iter()
            .filter(|name| *name == "ssh_reachability")
            .count(),
        3,
        "one ssh_reachability row per host\nnames: {names:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    for project_grader in ["signing_secret", "database_url", "migrate_check"] {
        assert_eq!(
            names.iter().filter(|name| *name == project_grader).count(),
            1,
            "the project-wide grader `{project_grader}` runs ONCE for the whole \
             fleet\nnames: {names:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            !combined.contains(&format!("{project_grader} (")),
            "`{project_grader}` grades the project, not a host, so it must not be \
             scoped\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    // Six checks in total (three hosts + three project graders) and the count
    // includes every failing host, so the operator learns about host 3 before host 1
    // is touched.
    assert!(
        combined.contains("of 6 preflight check(s) failed"),
        "the report must count every host's grader\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        combined.matches("could not resolve web-").count(),
        3,
        "every host must be probed, not just the first\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_check_and_doctor_grade_the_same_fleet_graders() {
    // #1621 (T2.5, plan §5.5): `autumn doctor` mirroring the deploy preflight is a
    // hard invariant that has, until now, been enforced only by comments. Both
    // surfaces build their names from ONE ordered list
    // (`deploy::PREFLIGHT_GRADERS` / `deploy::DOCTOR_PREFLIGHT_GRADERS`, whose
    // `deploy_`-prefix derivation is pinned by a unit test); this asserts the two
    // really do emit the same grader set for the SAME fleet config.
    //
    // Doctor emits one `deploy_ssh_reachability` for the whole fleet (its check
    // names are `&'static str` `--json` keys and must stay unique) while
    // `deploy check` emits one row per host — so the comparison is over the SET of
    // stable identifiers, not the row count.
    let dir = project("hosts = [\"web-1.example.test\", \"web-2.example.test\"]\n");

    let (check_out, check_err, _) = run_autumn(dir.path(), &["deploy", "check"], &[]);
    let mut from_check: Vec<String> = preflight_grader_names(&format!("{check_out}{check_err}"))
        .into_iter()
        .map(|name| format!("deploy_{name}"))
        .collect();
    from_check.sort_unstable();
    from_check.dedup();

    // `--online` is what enables doctor's TCP reachability probe; without it the
    // deploy branch deliberately runs only the offline graders.
    let (doctor_out, doctor_err, _) =
        run_autumn(dir.path(), &["doctor", "--json", "--online"], &[]);
    let report: serde_json::Value = serde_json::from_str(&doctor_out).unwrap_or_else(|e| {
        panic!(
            "doctor --json must emit valid JSON: {e}\nstdout:\n{doctor_out}\nstderr:\n{doctor_err}"
        )
    });
    let mut from_doctor: Vec<String> = report["checks"]
        .as_array()
        .expect("doctor --json carries a checks array")
        .iter()
        .filter_map(|check| check["name"].as_str())
        .filter(|name| name.starts_with("deploy_"))
        // `deploy_host` (offline host presence) and `deploy_config` (a config-load
        // guard) are doctor-only by design — `deploy check` folds host presence into
        // `ssh_reachability`, which reports the identical missing-host detail.
        .filter(|name| *name != "deploy_host" && *name != "deploy_config")
        .map(str::to_owned)
        .collect();
    from_doctor.sort_unstable();
    from_doctor.dedup();

    assert_eq!(
        from_check, from_doctor,
        "`deploy check` and `doctor` must grade the SAME deploy graders for the same \
         fleet config\ncheck stdout:\n{check_out}\ncheck stderr:\n{check_err}\ndoctor \
         stdout:\n{doctor_out}"
    );
    assert!(
        !from_check.is_empty(),
        "the parity assertion must not pass vacuously\ncheck stderr:\n{check_err}"
    );

    // …and doctor really did enumerate the fleet rather than reporting "no target
    // host configured" because the scalar `[deploy] host` is unset.
    assert!(
        doctor_out.contains("web-1.example.test") && doctor_out.contains("web-2.example.test"),
        "doctor must enumerate the configured fleet hosts\nstdout:\n{doctor_out}"
    );
}

#[test]
fn deploy_check_output_is_identical_for_host_and_a_single_entry_hosts_list() {
    // #1621 (AC-1, the `check` half of T2.1): the same differential proof the `plan`
    // half makes, over the surface that grades. `deploy check` now runs the FLEET
    // preflight, so this is what pins "a one-entry `hosts` list is byte-for-byte
    // today's single-server deploy" against the scope field, the per-host rows and
    // the failure counting all at once.
    let scalar = project("host = \"deploy.example.test\"\n");
    let list = project("hosts = [\"deploy.example.test\"]\n");

    let (scalar_out, scalar_err, scalar_code) =
        run_autumn(scalar.path(), &["deploy", "check"], &[]);
    let (list_out, list_err, list_code) = run_autumn(list.path(), &["deploy", "check"], &[]);

    assert_eq!(
        scalar_code, list_code,
        "exit codes must match\n`host` stderr:\n{scalar_err}\n`hosts` stderr:\n{list_err}"
    );
    assert_eq!(
        scalar_out, list_out,
        "`deploy check` stdout must be byte-identical under both spellings"
    );
    assert_eq!(
        scalar_err, list_err,
        "`deploy check` stderr must be byte-identical under both spellings"
    );
    // Non-vacuous: the report really was produced (not two empty strings from an
    // early config refusal).
    assert!(
        scalar_err.contains("ssh_reachability"),
        "the differential must compare a real preflight report\nstderr:\n{scalar_err}"
    );
    // A single-host report carries NO scope suffix — that is what "identical" means
    // here (AC-1 artifact 4).
    assert!(
        !scalar_err.contains("ssh_reachability ("),
        "a single-host preflight must not print a scope suffix\nstderr:\n{scalar_err}"
    );
}

#[test]
fn deploy_plan_output_is_identical_for_host_and_a_single_entry_hosts_list() {
    // #1621 (AC-1, differential half of T2.1): a one-entry `hosts` list IS today's
    // single-server deploy. The two projects differ by exactly the `[deploy]` host
    // spelling, so any fleet-specific divergence in `deploy plan` — an extra
    // section, a reordered line, a changed byte — shows up here. Stronger than a
    // checked-in golden fixture: it can never go stale and depends on nothing
    // about the machine it runs on.
    let scalar = project("host = \"203.0.113.10\"\n");
    let list = project("hosts = [\"203.0.113.10\"]\n");

    let (scalar_out, scalar_err, scalar_code) = run_autumn(scalar.path(), &["deploy", "plan"], &[]);
    let (list_out, list_err, list_code) = run_autumn(list.path(), &["deploy", "plan"], &[]);

    assert_eq!(
        scalar_code, list_code,
        "exit codes must match\n`host` stderr:\n{scalar_err}\n`hosts` stderr:\n{list_err}"
    );
    assert_eq!(
        scalar_out, list_out,
        "`deploy plan` stdout must be byte-identical under both spellings"
    );
    assert_eq!(
        scalar_err, list_err,
        "`deploy plan` stderr must be byte-identical under both spellings"
    );
}

#[test]
fn deploy_help_lists_subcommands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["deploy", "--help"], &[]);
    assert_eq!(
        code,
        Some(0),
        "deploy --help should succeed\nstderr:\n{stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("check"),
        "help should list check\n{combined}"
    );
    assert!(
        combined.contains("plan"),
        "help should list plan\n{combined}"
    );
    assert!(
        combined.contains("rollback"),
        "help should list rollback\n{combined}"
    );
    // #1621 (T2.4): the group doc comment IS the help text (verbatim_doc_comment),
    // so the fleet flags must be documented there or `autumn deploy --help` starts
    // lying about what the command can do.
    assert!(
        combined.contains("--only"),
        "help should document the fleet `--only` flag\n{combined}"
    );
    assert!(
        combined.contains("--no-rollback"),
        "help should document the fleet `--no-rollback` flag\n{combined}"
    );
    // #1621 (T2.4): the fleet status + maintenance surfaces must be discoverable
    // from the group help, flags included.
    assert!(
        combined.contains("status"),
        "help should list the status subcommand\n{combined}"
    );
    assert!(
        combined.contains("maintenance"),
        "help should list the maintenance subcommand\n{combined}"
    );
    assert!(
        combined.contains("--json"),
        "help should document status --json\n{combined}"
    );
    assert!(
        combined.contains("--strict"),
        "help should document status --strict\n{combined}"
    );
}

#[test]
fn maintenance_help_cross_references_the_deploy_fan_out() {
    // #1621 (plan §3.3): the top-level `autumn maintenance` is LOCAL-only (it
    // writes the flag in the CLI's own working directory), and the fleet fan-out
    // lives under `autumn deploy maintenance`. The local command's help must point
    // at the deploy one, or operators of deploy-managed hosts will run the local
    // command and wonder why nothing happened.
    let dir = tempfile::tempdir().expect("temp dir");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["maintenance", "--help"], &[]);
    assert_eq!(
        code,
        Some(0),
        "maintenance --help should succeed\nstderr:\n{stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("deploy maintenance"),
        "the local maintenance help must cross-reference `autumn deploy maintenance` \
         for deploy-managed hosts\n{combined}"
    );
}

#[test]
fn deploy_up_rejects_an_only_host_that_is_not_in_the_fleet() {
    // #1621 (§3.2): `--only` is checked against `[deploy] hosts` before anything
    // else happens — no preflight, no SSH, no build. A typo names itself and the
    // configured hosts are listed, because the alternative (guessing, or silently
    // deploying nothing) can put the wrong machine into production.
    let dir = project("hosts = [\"web-1.example.com\", \"web-2.example.com\"]\n");
    let (stdout, stderr) = run_autumn_fail(
        dir.path(),
        &["deploy", "up", "--only", "web-9.example.com"],
        &[],
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("web-9.example.com"),
        "the error must quote the unmatched host\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("web-1.example.com") && combined.contains("web-2.example.com"),
        "the error must list the configured hosts\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_up_fails_fast_without_host() {
    // #2274: forgetting `[deploy] host` is an ordinary first-run mistake, and `up`
    // must report it the way `check` does — at the preflight boundary, before any
    // local prerequisite work and long before anything indexes the fleet's first
    // host. The tier-2 harness runs with an empty `PATH`, so this asserts the
    // fail-fast boundary itself: the run must end in the missing-host preflight
    // report, never a panic.
    let dir = project("");
    let (stdout, stderr) = run_autumn_fail(dir.path(), &["deploy", "up"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !combined.contains("panicked"),
        "a hostless `up` must fail cleanly, not panic\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("[deploy] host") && combined.contains("hosts"),
        "the missing-host report must name both spellings\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
