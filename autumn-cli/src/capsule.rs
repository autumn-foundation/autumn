//! `autumn capsule` — turn a recorded failure capsule into a committed
//! regression test, and replay a whole committed corpus (issue #1634).
//!
//! `autumn replay` answers "is this bug still there?" once. This command
//! answers "can it ever come back?": the capsule is copied into the app's test
//! tree and a `#[tokio::test]` is generated beside it, so from then on the
//! failure is re-checked by `cargo test` with no network, database or queue.
//!
//! Conversion is deliberately developer-invoked and never auto-commits: the
//! generated files land in the working tree for review, and the router hook
//! (`capsule_support.rs`) is scaffolded once and then left alone, because only
//! the developer knows which routes the capsule's request needs.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Exit code for a capsule that could not be converted; matches `autumn
/// replay`'s "refused" code so a script can treat them the same way.
const EXIT_REFUSED: i32 = 2;

/// Where generated fixtures live, relative to the tests directory.
const FIXTURE_SUBDIR: &str = "capsules";
/// Where generated tests live, relative to the tests directory — the
/// consolidated integration module the workspace layout guidelines describe.
const TEST_SUBDIR: &str = "integration";
/// The scaffolded router hook every generated test calls.
const SUPPORT_MODULE: &str = "capsule_support";

/// Options for `autumn capsule test`.
pub struct GenerateOptions<'a> {
    /// Path to the capsule JSON file to convert.
    pub capsule: &'a str,
    /// Test name override; defaults to a slug derived from the capsule id.
    pub name: Option<&'a str>,
    /// The crate's `tests/` directory.
    pub tests_dir: &'a str,
    /// Overwrite an existing fixture and test of the same name.
    pub force: bool,
}

/// Run `autumn capsule test`.
pub fn generate(opts: &GenerateOptions<'_>) {
    match generate_inner(opts) {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("autumn capsule test: {error}");
            std::process::exit(EXIT_REFUSED);
        }
    }
}

/// The fallible half of [`generate`], so every failure is one `Err` with a
/// message rather than a scattering of `process::exit` calls.
#[allow(
    clippy::too_many_lines,
    reason = "one linear conversion: validate, slug, write the fixture, write \
              the test, scaffold the hook, register the module. Splitting it \
              would scatter the refusal conditions that have to be checked \
              before anything is written."
)]
fn generate_inner(opts: &GenerateOptions<'_>) -> Result<String, String> {
    let source = Path::new(opts.capsule);
    let json = std::fs::read_to_string(source)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?;
    let capsule = autumn_web::capsule::Capsule::from_json(&json).map_err(|error| {
        format!(
            "{} is not a capsule this build can convert: {error}",
            source.display()
        )
    })?;
    // A capsule replay refuses is not one to commit: the generated test would
    // fail on every run for a reason that has nothing to do with the code.
    if let Some(reason) = autumn_web::capsule::refusal_reason(&capsule) {
        return Err(format!(
            "{} cannot be replayed, so it cannot become a regression test: {reason}",
            source.display()
        ));
    }
    // A job capsule has no request to drive through a router, so the generated
    // test would fail on every run for a reason that has nothing to do with the
    // code. `autumn replay` dispatches the job's handler instead.
    if let Some(job) = capsule.job.as_ref() {
        return Err(format!(
            "{} records a failure inside job {:?}, not a request. Job capsules replay with \
             `autumn replay`, which dispatches the job's handler; there is no router-driven \
             test to generate for one.",
            source.display(),
            job.name
        ));
    }

    let slug = opts.name.map_or_else(|| slugify(&capsule.id), slugify);
    if slug.is_empty() {
        return Err(
            "the capsule's id has no characters usable in a test name; pass --name".to_owned(),
        );
    }

    let tests_dir = PathBuf::from(opts.tests_dir);
    let capsules = tests_dir.join(FIXTURE_SUBDIR);
    let suite = tests_dir.join(TEST_SUBDIR);
    let fixture_path = capsules.join(format!("{slug}.json"));
    let test_path = suite.join(format!("capsule_{slug}.rs"));

    if !opts.force {
        for existing in [&fixture_path, &test_path] {
            if existing.exists() {
                return Err(format!(
                    "{} already exists; pass --force to overwrite it, or --name to convert this \
                     capsule under a different name",
                    existing.display()
                ));
            }
        }
    }

    std::fs::create_dir_all(&capsules)
        .map_err(|error| format!("could not create {}: {error}", capsules.display()))?;
    std::fs::create_dir_all(&suite)
        .map_err(|error| format!("could not create {}: {error}", suite.display()))?;

    // The fixture is the capsule's own bytes, copied verbatim. Nothing is
    // re-derived, re-read from a live system, or re-serialized: whatever
    // redaction removed on the way to disk is exactly what stays removed on the
    // way into the repository, so committing a generated test can never
    // introduce a secret the capsule did not already hold.
    std::fs::write(&fixture_path, json.as_bytes())
        .map_err(|error| format!("could not write {}: {error}", fixture_path.display()))?;
    std::fs::write(&test_path, generated_test(&slug, &capsule).as_bytes())
        .map_err(|error| format!("could not write {}: {error}", test_path.display()))?;

    let mut report = String::new();
    let _ = writeln!(report, "wrote {}", fixture_path.display());
    let _ = writeln!(report, "wrote {}", test_path.display());

    let support_path = suite.join(format!("{SUPPORT_MODULE}.rs"));
    if support_path.exists() {
        let _ = writeln!(
            report,
            "kept {} (edit it to add the routes this capsule needs)",
            support_path.display()
        );
    } else {
        std::fs::write(&support_path, SUPPORT_SCAFFOLD.as_bytes())
            .map_err(|error| format!("could not write {}: {error}", support_path.display()))?;
        let _ = writeln!(
            report,
            "wrote {} — add the app's routes to `router` before running the test",
            support_path.display()
        );
    }

    let mod_path = suite.join("mod.rs");
    match register_module(&mod_path, &format!("capsule_{slug}"), SUPPORT_MODULE) {
        Ok(true) => {
            let _ = writeln!(report, "registered the modules in {}", mod_path.display());
        }
        Ok(false) => {
            let _ = writeln!(
                report,
                "{} already declares the modules",
                mod_path.display()
            );
        }
        Err(error) => {
            // Not fatal: the files are written and a developer can add two
            // `mod` lines. Failing the whole conversion over it would be worse
            // than saying exactly what is left to do.
            let _ = writeln!(
                report,
                "could not update {}: {error}\n  add these lines yourself:\n    mod \
                 {SUPPORT_MODULE};\n    mod capsule_{slug};",
                mod_path.display()
            );
        }
    }
    let _ = write!(
        report,
        "\nRun it with:  cargo test capsule_{slug}\nThe whole corpus:  autumn capsule verify"
    );
    Ok(report)
}

/// Reduce a capsule id to characters legal in a Rust module and test name.
fn slugify(id: &str) -> String {
    let mut slug = String::with_capacity(id.len());
    for character in id.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('_') {
            slug.push('_');
        }
    }
    let trimmed = slug.trim_matches('_').to_owned();
    // A Rust identifier cannot start with a digit, and a capsule id routinely
    // does (request ids are often ULIDs). A keyword is prefixed for the same
    // reason: the slug becomes a module name, a file name *and* a test
    // function name, and `mod type;` does not compile — a generated test that
    // cannot build is worse than no generated test.
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) || is_rust_keyword(&trimmed) {
        format!("c{trimmed}")
    } else {
        trimmed
    }
}

/// Whether `word` is a Rust keyword (including the reserved-for-future set).
///
/// Raw identifiers (`r#type`) would cover most positions, but not all — `r#`
/// is not valid in a file name, and `crate`/`self`/`super`/`Self` cannot be
/// raw at all — so the generator prefixes instead of escaping.
fn is_rust_keyword(word: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false",
        "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
        "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "box", "become", "do", "final", "gen", "macro",
        "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
    ];
    KEYWORDS.contains(&word)
}

/// Add `mod` declarations to the consolidated test module, idempotently.
///
/// Returns `Ok(false)` when both were already declared.
fn register_module(
    mod_path: &Path,
    test_module: &str,
    support_module: &str,
) -> Result<bool, String> {
    let existing = if mod_path.exists() {
        std::fs::read_to_string(mod_path)
            .map_err(|error| format!("could not read {}: {error}", mod_path.display()))?
    } else {
        String::new()
    };
    let mut added = String::new();
    if !declares(&existing, support_module) {
        let _ = writeln!(added, "mod {support_module};");
    }
    if !declares(&existing, test_module) {
        let _ = writeln!(added, "mod {test_module};");
    }
    if added.is_empty() {
        return Ok(false);
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&added);
    std::fs::write(mod_path, updated.as_bytes())
        .map_err(|error| format!("could not write {}: {error}", mod_path.display()))?;
    Ok(true)
}

/// Whether `source` already declares `module`.
///
/// Matched line-wise on the exact declaration so a module whose name is a
/// prefix of another (`capsule_a` vs `capsule_ab`) is not mistaken for it.
fn declares(source: &str, module: &str) -> bool {
    let needle = format!("mod {module};");
    source
        .lines()
        .any(|line| line.trim_start().trim_start_matches("pub ") == needle)
}

/// The `#[tokio::test]` written beside a converted capsule.
fn generated_test(slug: &str, capsule: &autumn_web::capsule::Capsule) -> String {
    let route = capsule
        .request
        .route
        .clone()
        .unwrap_or_else(|| capsule.request.uri.clone());
    let recorded = capsule.captured_at.to_rfc3339();
    let id = capsule.id.replace(['\n', '\r'], " ");
    format!(
        r#"//! Regression test generated by `autumn capsule test` from capsule `{id}`.
//!
//! Recorded {recorded} — `{method} {route}`.
//!
//! The capsule beside this file is the *whole* input: the clock, randomness,
//! outbound HTTP, job enqueues, cache, mail, the tenant and the database are
//! all served from it, so this test needs no network, database or queue.
//!
//! Regenerate with:
//!   autumn capsule test tests/capsules/{slug}.json --force
//!
//! A `mismatch` here usually means the bug is fixed and this test has done its
//! job — re-record it or delete it. A `diverged` means the handler's effects
//! changed underneath the capsule.

use autumn_web::capsule::regression::RegressionCase;

const CAPSULE: &str = include_str!("../capsules/{slug}.json");

#[tokio::test]
async fn capsule_{slug}_still_reproduces() {{
    RegressionCase::from_json(CAPSULE)
        .expect("the committed capsule must parse; regenerate it after an Autumn upgrade")
        .assert_reproduces(super::{SUPPORT_MODULE}::router)
        .await;
}}
"#,
        method = capsule.request.method,
    )
}

/// The router hook, written once and then owned by the developer.
const SUPPORT_SCAFFOLD: &str = r"//! Router factory shared by every generated capsule regression test.
//!
//! `autumn capsule test` scaffolds this file once and never overwrites it —
//! only you know which routes your capsules need. Add them below; everything
//! else (the clock, randomness, outbound HTTP, jobs, cache, mail, the tenant,
//! the database) is served from the capsule, so nothing here should reach a
//! live service.

use autumn_web::capsule::regression::RegressionContext;
use autumn_web::test::TestApp;

/// Build the application router a capsule replays against.
pub fn router(ctx: &RegressionContext<'_>) -> axum::Router {
    TestApp::new()
        // TODO: add the routes your capsules exercise, e.g.
        //   .routes(autumn_web::routes![checkout, charge])
        .with_clock(ctx.clock())
        .with_entropy(ctx.entropy())
        .build()
        .into_router()
}
";

// ── Whole-corpus verification ───────────────────────────────────────────────

/// Options for `autumn capsule verify`.
pub struct VerifyOptions<'a> {
    /// Directory holding the committed capsules.
    pub dir: &'a str,
    /// Report on the corpus without running the generated tests.
    pub check_only: bool,
}

/// Run `autumn capsule verify` — replay the whole committed corpus.
///
/// Two halves, in order. First the corpus-level checks `cargo test` cannot
/// make: that the directory exists, that it is **not empty** (an empty corpus
/// is reported as a failure, never as a vacuous pass), and that every capsule
/// in it is still readable and replayable by *this* build — exactly the
/// question an Autumn upgrade raises. Then `cargo test capsule_`, which runs
/// the generated tests themselves.
///
/// Running them by delegation rather than re-implementing replay here is the
/// point: the generated tests are ordinary tests driving the same
/// `capsule::execute` engine `autumn replay` does, so there is exactly one
/// replay engine and no way for two of them to disagree.
pub fn verify(opts: &VerifyOptions<'_>) {
    let report = match verify_inner(Path::new(opts.dir)) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("autumn capsule verify: {error}");
            std::process::exit(EXIT_REFUSED);
        }
    };
    println!("{}", report.text);
    if report.unusable > 0 {
        std::process::exit(1);
    }
    if opts.check_only {
        return;
    }
    let status = std::process::Command::new("cargo")
        .args(["test", TEST_NAME_PREFIX])
        .status();
    match status {
        Ok(status) if status.success() => println!("\nthe corpus replays clean"),
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("autumn capsule verify: could not run `cargo test`: {error}");
            std::process::exit(EXIT_REFUSED);
        }
    }
}

/// The name prefix every generated regression test carries, which is also the
/// filter `verify` runs them with.
const TEST_NAME_PREFIX: &str = "capsule_";

/// What one corpus verification found.
#[derive(Debug)]
struct CorpusReport {
    /// The human-readable listing.
    text: String,
    /// How many committed capsules this build cannot replay.
    unusable: usize,
}

/// The fallible half of [`verify`], so the corpus rules are testable without
/// a process exit.
fn verify_inner(dir: &Path) -> Result<CorpusReport, String> {
    let paths = autumn_web::capsule::regression::RegressionCase::corpus(dir)
        .map_err(|error| format!("could not read the corpus at {}: {error}", dir.display()))?;
    if paths.is_empty() {
        // An empty corpus must not be a silent pass: "no capsules committed"
        // and "every capsule passed" are opposite facts, and a corpus that
        // quietly stopped testing anything is the failure mode this whole
        // feature exists to prevent.
        return Err(format!(
            "no capsules in {} — convert one with `autumn capsule test`",
            dir.display()
        ));
    }

    let mut text = String::new();
    let mut unusable = 0_usize;
    for path in &paths {
        match autumn_web::capsule::regression::RegressionCase::from_path(path) {
            Ok(case) => {
                if let Some(reason) = autumn_web::capsule::refusal_reason(case.capsule()) {
                    unusable = unusable.saturating_add(1);
                    let _ = writeln!(text, "REFUSED     {}\n  {reason}", path.display());
                } else {
                    let _ = writeln!(text, "ok          {}", path.display());
                }
            }
            Err(error) => {
                unusable = unusable.saturating_add(1);
                let _ = writeln!(text, "UNREADABLE  {}\n  {error}", path.display());
            }
        }
    }
    let _ = writeln!(
        text,
        "\n{} capsule(s) in {}, {unusable} unusable by this build",
        paths.len(),
        dir.display()
    );
    let _ = write!(text, "running the generated tests: cargo test capsule_");
    Ok(CorpusReport { text, unusable })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, valid capsule document at the current format version.
    ///
    /// Written as JSON rather than through `schema::test_support` because the
    /// CLI depends on `autumn-web` without its `test-support` feature — and
    /// building the document the way a reader will actually meet it is the
    /// more faithful fixture anyway.
    fn fixture_capsule(id: &str) -> String {
        capsule_json(id, false)
    }

    fn capsule_json(id: &str, truncated: bool) -> String {
        serde_json::json!({
            "format_version": autumn_web::capsule::CAPSULE_FORMAT_VERSION,
            "id": id,
            "captured_at": "2026-08-27T10:00:00Z",
            "autumn_version": "0.7.0",
            "app": {"name": "shop", "profile": "prod", "debug_assertions": true},
            "request": {
                "method": "GET",
                "uri": "/orders",
                "route": "/orders",
                "http_version": "HTTP/1.1",
                "headers": [["authorization", "[FILTERED]"]],
                "body": "absent",
                "redacted_keys": ["header:authorization"],
            },
            "outcome": {"status": {"code": 500, "message": "boom"}},
            "truncated": truncated,
        })
        .to_string()
    }

    #[test]
    fn a_capsule_id_becomes_a_legal_rust_identifier() {
        assert_eq!(slugify("req-01JB2K7Q"), "req_01jb2k7q");
        assert_eq!(slugify("01JB2K7Q"), "c01jb2k7q");
        assert_eq!(slugify("a//b__c"), "a_b_c");
        assert_eq!(slugify("---"), "");
        // A slug that lands on a keyword would generate `mod type;`, which does
        // not compile — and a generated regression test that cannot build is
        // worse than none.
        assert_eq!(slugify("type"), "ctype");
        assert_eq!(slugify("mod"), "cmod");
        assert_eq!(slugify("Self"), "cself");
    }

    #[test]
    fn conversion_writes_the_capsule_bytes_verbatim() {
        // AC: redaction survives conversion. The fixture must be the capsule's
        // own bytes — nothing re-derived, nothing re-serialized — so whatever
        // redaction removed stays removed.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("capsule.json");
        let json = fixture_capsule("req-1");
        std::fs::write(&source, &json).expect("write capsule");
        let tests = dir.path().join("tests");

        generate_inner(&GenerateOptions {
            capsule: source.to_str().expect("utf-8 path"),
            name: None,
            tests_dir: tests.to_str().expect("utf-8 path"),
            force: false,
        })
        .expect("conversion succeeds");

        let fixture = std::fs::read_to_string(tests.join("capsules/req_1.json")).expect("fixture");
        assert_eq!(
            fixture, json,
            "the committed fixture must be byte-identical to the capsule"
        );
    }

    #[test]
    fn conversion_writes_a_test_the_support_module_and_the_mod_declarations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("capsule.json");
        std::fs::write(&source, fixture_capsule("req-2")).expect("write capsule");
        let tests = dir.path().join("tests");

        generate_inner(&GenerateOptions {
            capsule: source.to_str().expect("utf-8 path"),
            name: None,
            tests_dir: tests.to_str().expect("utf-8 path"),
            force: false,
        })
        .expect("conversion succeeds");

        let test = std::fs::read_to_string(tests.join("integration/capsule_req_2.rs"))
            .expect("generated test");
        assert!(
            test.contains("include_str!(\"../capsules/req_2.json\")"),
            "the test must load the committed fixture: {test}"
        );
        assert!(
            test.contains("assert_reproduces"),
            "the test must fail when the outcome diverges: {test}"
        );
        assert!(
            tests.join("integration/capsule_support.rs").exists(),
            "the router hook must be scaffolded"
        );
        let module = std::fs::read_to_string(tests.join("integration/mod.rs")).expect("mod.rs");
        assert!(module.contains("mod capsule_req_2;"), "{module}");
        assert!(module.contains("mod capsule_support;"), "{module}");
    }

    #[test]
    fn a_second_conversion_does_not_duplicate_the_module_or_clobber_the_hook() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tests = dir.path().join("tests");
        for id in ["req-3", "req-4"] {
            let source = dir.path().join(format!("{id}.json"));
            std::fs::write(&source, fixture_capsule(id)).expect("write capsule");
            generate_inner(&GenerateOptions {
                capsule: source.to_str().expect("utf-8 path"),
                name: None,
                tests_dir: tests.to_str().expect("utf-8 path"),
                force: false,
            })
            .expect("conversion succeeds");
        }
        // The developer's edits to the router hook survive the second run.
        let hook = tests.join("integration/capsule_support.rs");
        std::fs::write(&hook, "// edited by hand\n").expect("edit hook");
        let source = dir.path().join("req-5.json");
        std::fs::write(&source, fixture_capsule("req-5")).expect("write capsule");
        generate_inner(&GenerateOptions {
            capsule: source.to_str().expect("utf-8 path"),
            name: None,
            tests_dir: tests.to_str().expect("utf-8 path"),
            force: false,
        })
        .expect("conversion succeeds");

        assert_eq!(
            std::fs::read_to_string(&hook).expect("hook"),
            "// edited by hand\n",
            "the scaffolded hook must never be overwritten"
        );
        let module = std::fs::read_to_string(tests.join("integration/mod.rs")).expect("mod.rs");
        assert_eq!(
            module.matches("mod capsule_support;").count(),
            1,
            "the support module must be declared exactly once: {module}"
        );
    }

    #[test]
    fn converting_the_same_capsule_twice_is_refused_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("capsule.json");
        std::fs::write(&source, fixture_capsule("req-6")).expect("write capsule");
        let tests = dir.path().join("tests");
        let opts = GenerateOptions {
            capsule: source.to_str().expect("utf-8 path"),
            name: None,
            tests_dir: tests.to_str().expect("utf-8 path"),
            force: false,
        };
        generate_inner(&opts).expect("first conversion succeeds");
        let error = generate_inner(&opts).expect_err("the second must be refused");
        assert!(error.contains("--force"), "{error}");
    }

    #[test]
    fn a_truncated_capsule_is_never_committed_as_a_test() {
        // A capsule replay refuses would produce a test that fails forever for
        // a reason that has nothing to do with the code.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("capsule.json");
        std::fs::write(&source, capsule_json("req-7", true)).expect("write capsule");

        let error = generate_inner(&GenerateOptions {
            capsule: source.to_str().expect("utf-8 path"),
            name: None,
            tests_dir: dir.path().join("tests").to_str().expect("utf-8 path"),
            force: false,
        })
        .expect_err("a truncated capsule must not be converted");
        assert!(error.contains("truncated"), "{error}");
    }

    #[test]
    fn an_incompatible_capsule_version_is_refused_with_an_actionable_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("capsule.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&fixture_capsule("req-8")).expect("parses");
        value["format_version"] = serde_json::json!(1);
        std::fs::write(&source, value.to_string()).expect("write capsule");

        let error = generate_inner(&GenerateOptions {
            capsule: source.to_str().expect("utf-8 path"),
            name: None,
            tests_dir: dir.path().join("tests").to_str().expect("utf-8 path"),
            force: false,
        })
        .expect_err("an incompatible capsule must not be converted");
        assert!(error.contains("format version 1"), "{error}");
    }

    #[test]
    fn a_job_capsule_is_not_converted_into_a_router_driven_test() {
        // The generated test drives a router; a job capsule has no request to
        // drive, so converting one would commit a test that fails forever.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("capsule.json");
        let mut value: serde_json::Value =
            serde_json::from_str(&fixture_capsule("req-11")).expect("parses");
        value["job"] = serde_json::json!({"name": "send_receipt", "payload": {"order": 7}});
        std::fs::write(&source, value.to_string()).expect("write");

        let error = generate_inner(&GenerateOptions {
            capsule: source.to_str().expect("utf-8 path"),
            name: None,
            tests_dir: dir.path().join("tests").to_str().expect("utf-8 path"),
            force: false,
        })
        .expect_err("a job capsule must not be converted");
        assert!(error.contains("autumn replay"), "{error}");
        assert!(error.contains("send_receipt"), "{error}");
    }

    #[test]
    fn an_empty_corpus_is_a_failure_not_a_vacuous_pass() {
        // "no capsules committed" and "every capsule passed" are opposite
        // facts; reporting the first as the second is how a regression corpus
        // silently stops testing anything.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("capsules")).expect("mkdir");
        let error = verify_inner(&dir.path().join("capsules"))
            .expect_err("an empty corpus must not report success");
        assert!(error.contains("no capsules"), "{error}");

        let missing = verify_inner(&dir.path().join("nope"))
            .expect_err("a missing corpus must not report success");
        assert!(missing.contains("could not read"), "{missing}");
    }

    #[test]
    fn a_corpus_reports_readable_and_unusable_capsules_separately() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corpus = dir.path().join("capsules");
        std::fs::create_dir_all(&corpus).expect("mkdir");
        std::fs::write(corpus.join("good.json"), fixture_capsule("req-9")).expect("write");
        std::fs::write(corpus.join("stale.json"), {
            let mut value: serde_json::Value =
                serde_json::from_str(&fixture_capsule("req-10")).expect("parses");
            value["format_version"] = serde_json::json!(1);
            value.to_string()
        })
        .expect("write");

        let report = verify_inner(&corpus).expect("a non-empty corpus reports");
        assert_eq!(
            report.unusable, 1,
            "the stale capsule must be counted, not skipped: {}",
            report.text
        );
        assert!(report.text.contains("UNREADABLE"), "{}", report.text);
        assert!(report.text.contains("ok  "), "{}", report.text);
    }

    #[test]
    fn module_declarations_are_matched_whole_not_by_prefix() {
        assert!(declares("mod capsule_a;\n", "capsule_a"));
        assert!(!declares("mod capsule_ab;\n", "capsule_a"));
        assert!(declares("pub mod capsule_a;\n", "capsule_a"));
    }
}
