//! The TLA+ specs in `verification/tla/` must keep up with their Stateright
//! twins. `scripts/verify-tla.sh` runs TLC, but only in the `TLA+ specs`
//! workflow; this test runs in `cargo test --workspace`, with no Java.
//!
//! - Each seeded bug of a Stateright model has a `violates` or `holds` line
//!   in `verification/tla/checks.txt`, and its spec lists it in `Variants`.
//! - Each spec has a `holds` and a `reaches` line for `Correct`.
//! - `.github/workflows/tla.yml` runs `scripts/verify-tla.sh`, and the script
//!   runs every line of `checks.txt` (checked with a stub `java`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Each Stateright model and its TLA+ twin.
const TWINS: [(&str, &str); 3] = [
    ("job_claim", "JobClaim"),
    ("tick_election", "TickElection"),
    ("lease_lock", "LeaseLock"),
];

const WORKFLOW: &str = ".github/workflows/tla.yml";
const SCRIPT: &str = "scripts/verify-tla.sh";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_dir().join("..").join("..")
}

fn tla_dir() -> PathBuf {
    crate_dir().join("..").join("tla")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// One line of `checks.txt`: the expectation, the spec and the variant.
fn checks() -> BTreeSet<(String, String, String)> {
    read(&tla_dir().join("checks.txt"))
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut words = line.split_whitespace().map(str::to_owned);
            let mut next = || words.next().unwrap_or_default();
            (next(), next(), next())
        })
        .collect()
}

/// The variants of `pub enum Variant` in a model's source.
fn stateright_variants(module: &str) -> Vec<String> {
    let source = read(&crate_dir().join("src").join(format!("{module}.rs")));
    let body = source
        .split_once("pub enum Variant {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map_or_else(
            || panic!("{module}.rs has no `pub enum Variant`"),
            |(body, _)| body,
        );
    body.lines()
        .map(str::trim)
        .filter_map(|line| line.strip_suffix(','))
        .filter(|name| name.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_seeded_bug_has_a_tlc_check() {
    let checks = checks();
    let mut missing = Vec::new();
    for (module, spec) in TWINS {
        let tla = read(&tla_dir().join(format!("{spec}.tla")));
        let variants = stateright_variants(module);
        assert!(
            variants.len() > 1,
            "{module}.rs: no seeded bugs found in {variants:?}"
        );
        for variant in variants {
            if !tla.contains(&format!("\"{variant}\"")) {
                missing.push(format!("{spec}.tla does not list {variant} in Variants"));
            }
            let expects: &[&str] = if variant == "Correct" {
                &["holds", "reaches"]
            } else {
                &["violates"]
            };
            for expect in expects {
                // A seeded bug that the protocol tolerates is a `holds` line.
                let found =
                    checks.contains(&((*expect).to_owned(), spec.to_owned(), variant.clone()))
                        || (*expect == "violates"
                            && checks.contains(&(
                                "holds".to_owned(),
                                spec.to_owned(),
                                variant.clone(),
                            )));
                if !found {
                    missing.push(format!("checks.txt has no `{expect} {spec} {variant}`"));
                }
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn every_tla_spec_is_checked() {
    let checks = checks();
    let mut specs: Vec<String> = fs::read_dir(tla_dir())
        .expect("read verification/tla")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "tla").then(|| path.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect();
    specs.sort();
    assert!(!specs.is_empty(), "verification/tla has no specs");
    for spec in specs {
        for expect in ["holds", "reaches"] {
            assert!(
                checks.contains(&(expect.to_owned(), spec.clone(), "Correct".to_owned())),
                "checks.txt has no `{expect} {spec} Correct`"
            );
        }
        let seeded = checks
            .iter()
            .filter(|(expect, s, _)| expect == "violates" && *s == spec)
            .count();
        assert!(seeded > 0, "{spec}.tla has no seeded bug in checks.txt");
    }
}

/// The number of TLC runs that `checks.txt` asks for: one per `holds` line,
/// and one per property of a `violates` or `reaches` line.
fn expected_runs() -> (usize, usize) {
    let (mut holds, mut must_fail) = (0, 0);
    for line in read(&tla_dir().join("checks.txt")).lines().map(str::trim) {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.first() {
            Some(&"holds") => holds += 1,
            Some(&"violates" | &"reaches") => must_fail += words.len().saturating_sub(3),
            _ => {}
        }
    }
    (holds, must_fail)
}

#[test]
fn workflow_runs_the_tla_script() {
    let workflow = read(&workspace_root().join(WORKFLOW));
    assert!(
        workflow.contains(&format!("./{SCRIPT}")),
        "{WORKFLOW} must run ./{SCRIPT}:\n{workflow}"
    );
    for trigger in ["pull_request:", "schedule:", "workflow_dispatch:"] {
        assert!(
            workflow.contains(trigger),
            "{WORKFLOW} must trigger on `{trigger}`"
        );
    }
    for path in ["verification/tla/**", SCRIPT, WORKFLOW] {
        assert!(
            workflow.contains(&format!("\"{path}\"")),
            "{WORKFLOW} must run when `{path}` changes"
        );
    }
    assert!(
        workflow.contains("sha256sum --check"),
        "{WORKFLOW} must verify the checksum of tla2tools.jar"
    );
    let readme = read(&workspace_root().join("verification").join("README.md"));
    assert!(
        readme.contains(WORKFLOW) && readme.contains(SCRIPT),
        "verification/README.md must name {WORKFLOW} and {SCRIPT}"
    );
}

/// With a stub `java` that reports "no error" for every run, each `holds`
/// line passes and each `violates` or `reaches` property fails. So the script
/// runs every line, and one failure does not hide the others.
#[cfg(unix)]
#[test]
fn script_runs_every_check_and_fails_on_a_missing_counterexample() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    let tmp = std::env::temp_dir().join(format!("tla-stub-{}", std::process::id()));
    fs::create_dir_all(&tmp).expect("create the stub dir");
    let log = tmp.join("runs.log");
    let stub = tmp.join("java");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\ngrep '^MC_Check' MC.tla >> '{}'\n\
             echo 'Model checking completed. No error has been found.'\n\
             echo '1,234 distinct states found'\necho '42 distinct states found'\n",
            log.display()
        ),
    )
    .expect("write the stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod the stub");

    let path = format!(
        "{}:{}",
        tmp.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("bash")
        .arg(workspace_root().join(SCRIPT))
        .env("PATH", path)
        // Any file: the stub does not read it.
        .env("TLA2TOOLS_JAR", tla_dir().join("checks.txt"))
        .output()
        .expect("run the TLA+ script");
    let runs = fs::read_to_string(&log).unwrap_or_default();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = fs::remove_dir_all(&tmp);

    let (holds, must_fail) = expected_runs();
    assert!(holds > 0 && must_fail > 0, "checks.txt is empty");
    assert!(
        !output.status.success(),
        "a missing counterexample must fail the script"
    );
    assert_eq!(
        runs.lines().count(),
        holds + must_fail,
        "the script must run TLC once per check:\n{stdout}"
    );
    assert_eq!(
        stderr.matches("FAIL:").count(),
        must_fail,
        "each missing counterexample or witness must be named:\n{stderr}"
    );
    // The final count, without the progress line's commas.
    assert_eq!(
        stdout.matches("    42 distinct states").count(),
        holds,
        "{stdout}"
    );
}
