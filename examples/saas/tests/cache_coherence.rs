//! Build-time cache coherence for this app (issue #1716).
//!
//! `cached_project_count` is a 30-second memoized read derived from `Project`;
//! every `ProjectRepository` write can strand it. The repository's
//! `invalidates(cached_project_count)` clause is what discharges that
//! obligation — and these tests prove both halves of the claim:
//!
//! * **Green** — with the clause present, the whole app audits clean.
//! * **Red** — take the clause away and the gate fires, naming the read, the
//!   write and the `Project` model they share. Without this half the green
//!   result would be indistinguishable from a checker that never looks.
//!
//! `cargo test --test cache_coherence` is the same proof `autumn cache audit`
//! runs against the built binary. See the Cache Coherence guide:
//! <https://github.com/autumn-foundation/autumn/blob/trunk/docs/guide/cache-coherence.md>

use autumn_web::cache::coherence::{self, DependencyProvenance, model_key};

// Linking the app crate is what puts its `#[cached]` and `#[repository]`
// registrations into this test binary.
use saas as _;

#[test]
fn the_app_is_provably_cache_coherent() {
    let manifest = coherence::audit();
    assert!(
        manifest.violations.is_empty(),
        "the build gate would fail:\n{}",
        coherence::format_diagnostic(&manifest.violations)
    );
}

#[test]
fn the_coherence_manifest_is_emitted_as_a_build_artifact() {
    let manifest = coherence::audit();
    let json: serde_json::Value = serde_json::from_str(&manifest.to_json()).unwrap();

    assert_eq!(json["schema_version"], 1);
    // The app's own cached read is in it, with a declared dependency set.
    let reads = json["dimensions"]["cached_reads"]["entries"]
        .as_array()
        .unwrap();
    let entry = reads
        .iter()
        .find(|e| e["id"].as_str().unwrap().ends_with("cached_project_count"))
        .expect("the app's cached read must be in the manifest");
    assert_eq!(entry["provenance"], "declared");
    assert!(
        entry["reads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| model_key(m.as_str().unwrap()) == "Project")
    );

    // As is the write surface it depends on, and the edge between them.
    let mutations = json["dimensions"]["mutations"]["entries"]
        .as_array()
        .unwrap();
    assert!(
        mutations
            .iter()
            .any(|m| m["name"] == "ProjectRepository::save" && m["table"] == "projects")
    );
    assert!(
        json["dimensions"]["invalidations"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["mutation"] == "ProjectRepository::save"
                && e["read"]
                    .as_str()
                    .unwrap()
                    .ends_with("cached_project_count"))
    );
}

#[test]
fn removing_the_invalidation_edge_turns_the_build_red() {
    // Exactly the diff a developer would make by deleting
    // `invalidates(cached_project_count)` from the repository attribute.
    let reads = coherence::registered_reads();
    let mut mutations = coherence::registered_mutations();
    for mutation in &mut mutations {
        if mutation.repository == "ProjectRepository" {
            mutation.invalidates.clear();
        }
    }

    let findings = coherence::check(&reads, &mutations);
    assert!(
        !findings.is_empty(),
        "the gate must catch the staleness bug the edge was covering"
    );

    let diagnostic = coherence::format_diagnostic(&findings);
    assert!(diagnostic.contains("cached_project_count"), "{diagnostic}");
    assert!(
        diagnostic.contains("ProjectRepository::save"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("Project"), "{diagnostic}");
    assert!(diagnostic.contains("invalidates("), "{diagnostic}");
}

#[test]
fn the_apps_cached_read_declares_rather_than_guesses_its_dependencies() {
    // A `derived` or `undetermined` set would still audit, but it would be a
    // weaker claim than this app is entitled to make.
    let read = coherence::registered_reads()
        .into_iter()
        .find(|r| r.id.ends_with("cached_project_count"))
        .expect("cached_project_count must register itself");
    assert_eq!(read.provenance, DependencyProvenance::Declared);
    assert!(read.acknowledged_stale.is_none());
}

/// The dump mode `autumn cache audit` drives, end to end against this app's own
/// binary.
///
/// Everything above works on registrations read in-process. This is the other
/// half of the contract: the CLI never links the app, it *runs* it — so the
/// marker protocol, the JSON on the far side, and the promise that dump mode
/// touches no database and binds no port all have to hold in a real child
/// process, or the gate reports "no manifest" against a perfectly good app.
#[test]
fn the_app_emits_its_manifest_in_dump_mode_without_a_database() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_saas"))
        .env("AUTUMN_DUMP_CACHE_COHERENCE", "1")
        // No DATABASE_URL: dump mode must return before anything connects.
        .env_remove("DATABASE_URL")
        .output()
        .expect("the app binary must run");

    assert!(
        output.status.success(),
        "dump mode must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let manifest = coherence::parse_manifest_dump(&stdout)
        .unwrap_or_else(|| panic!("no manifest on the marker line; stdout was:\n{stdout}"));

    assert_eq!(manifest.schema_version, 1);
    assert!(
        manifest.violations.is_empty(),
        "the shipped app must audit clean through the real dump path:\n{}",
        coherence::format_diagnostic(&manifest.violations)
    );
    assert!(
        manifest
            .dimensions
            .cached_reads
            .entries
            .iter()
            .any(|e| e.id.ends_with("cached_project_count")),
        "the app's cached read must survive the process boundary"
    );
    assert!(
        manifest
            .dimensions
            .mutations
            .entries
            .iter()
            .any(|e| e.name == "ProjectRepository::save"),
        "so must its write surface"
    );
}
