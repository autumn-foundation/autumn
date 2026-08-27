//! `autumn cache audit` — prove cached reads are never left stale (issue #1716).
//!
//! Runs the app's own binary in cache-coherence dump mode
//! (`AUTUMN_DUMP_CACHE_COHERENCE=1`), reads back the manifest the framework
//! assembles from every `#[cached]` read and every `#[repository]` write it
//! links, writes it out as a build artifact, and **fails the build** when a
//! mutation can strand a cached value with no invalidation covering it.
//!
//! Why run the binary rather than parse the sources: the dependency graph is a
//! *whole-app* fact. A cached read in one crate can be dirtied by a repository
//! in another, or in a plugin the app merely depends on, and link-time
//! `inventory` collection is the only place all of those registrations exist
//! together. This is the same shape as `autumn routes audit` (#1604).
//!
//! See `docs/guide/cache-coherence.md`.

use std::process::Command;

use autumn_web::cache::coherence::{CoherenceManifest, format_diagnostic, parse_manifest_dump};

use crate::routes;

/// Options controlling `autumn cache audit`.
pub struct CacheAuditOptions<'a> {
    /// Cargo package to build and run.
    pub package: Option<&'a str>,
    /// Binary target name for packages that expose multiple bin targets.
    pub bin: Option<&'a str>,
    /// Write the JSON manifest to this path (in addition to any stdout).
    pub manifest: Option<&'a str>,
    /// Emit the JSON manifest to stdout instead of the human report.
    pub json: bool,
    /// Also fail when any cached read's dependency set is undetermined.
    pub strict: bool,
}

/// Render the human report for a manifest.
#[must_use]
pub fn format_report(manifest: &CoherenceManifest, strict: bool) -> String {
    let mut out = manifest.summary();
    if !manifest.violations.is_empty() {
        out.push_str("\n\n");
        out.push_str(&format_diagnostic(&manifest.violations));
    }
    if !manifest.undetermined_reads.is_empty() {
        out.push('\n');
        out.push_str(&format_undetermined_diagnostic(
            &manifest.undetermined_reads,
            strict,
        ));
    }
    out
}

/// Diagnostic for reads whose dependency set could not be established.
///
/// A warning by default — a checker that fails on what it merely could not read
/// gets deleted from CI — and an error under `--strict`.
#[must_use]
pub fn format_undetermined_diagnostic(ids: &[String], strict: bool) -> String {
    let level = if strict { "error" } else { "warning" };
    let mut out = format!(
        "{level}: {} cached read{} could not be linked to any model, so nothing about \
         {} coherence was proven\n",
        ids.len(),
        if ids.len() == 1 { "" } else { "s" },
        if ids.len() == 1 { "its" } else { "their" },
    );
    for id in ids {
        out.push_str(&format!("  {id}\n"));
    }
    out.push_str(
        "  fix: declare the dependency set with #[cached(reads(Model, …))], or acknowledge \
         the gap with #[cached(acknowledge_stale = \"…\")].\n",
    );
    out
}

/// Run `autumn cache audit`.
pub fn run(opts: &CacheAuditOptions<'_>) {
    eprintln!("\u{1F342} autumn cache audit\n");
    routes::compile_binary(opts.package, opts.bin);
    let binary = routes::find_binary(opts.package, opts.bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_CACHE_COHERENCE", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping the cache-coherence manifest",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(manifest) = parse_manifest_dump(&stdout) else {
        eprintln!(
            "\u{2717} No cache-coherence manifest in the app's output. Rebuild against an \
             autumn-web that supports `autumn cache audit` (0.8+)."
        );
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    };

    let json = manifest.to_json();
    if let Some(path) = opts.manifest {
        if let Err(e) = std::fs::write(path, format!("{json}\n")) {
            eprintln!("\u{2717} Failed to write manifest to {path}: {e}");
            std::process::exit(1);
        }
        eprintln!("\u{2713} Wrote cache-coherence manifest \u{2192} {path}");
    }

    if opts.json {
        println!("{json}");
    } else {
        println!("{}", format_report(&manifest, opts.strict));
    }

    if manifest.violations.is_empty() && !(opts.strict && !manifest.undetermined_reads.is_empty()) {
        eprintln!("\u{2713} No cached read can be left stale by a repository write.");
    }

    // The gate fails on a proven violation always, and on an undetermined
    // dependency set only under `--strict` — the default never fails on what it
    // merely could not read.
    let failed = !manifest.violations.is_empty()
        || (opts.strict && !manifest.undetermined_reads.is_empty());
    std::process::exit(i32::from(failed));
}

#[cfg(test)]
mod tests {
    use autumn_web::cache::coherence::{CachedRead, DependencyProvenance, Mutation, ReadKind};

    use super::*;

    fn read(id: &str, models: &[&str], provenance: DependencyProvenance) -> CachedRead {
        CachedRead {
            id: id.to_string(),
            kind: ReadKind::Cached,
            reads: models.iter().map(|m| (*m).to_string()).collect(),
            provenance,
            acknowledged_stale: None,
            location: "src/views.rs:10".to_string(),
        }
    }

    fn mutation(model: &str) -> Mutation {
        Mutation {
            repository: "PostRepository".to_string(),
            method: "save".to_string(),
            model: model.to_string(),
            table: "posts".to_string(),
            invalidates: Vec::new(),
            acknowledged_stale: None,
            location: "src/repositories.rs:20".to_string(),
        }
    }

    #[test]
    fn report_names_the_violation() {
        let manifest = CoherenceManifest::build(
            &[read("blog::recent", &["Post"], DependencyProvenance::Declared)],
            &[mutation("blog::models::Post")],
        );
        let report = format_report(&manifest, false);
        assert!(report.contains("blog::recent"), "{report}");
        assert!(report.contains("PostRepository::save"), "{report}");
        assert!(report.contains("#[invalidates("), "{report}");
    }

    #[test]
    fn report_summary_counts_survive_the_manifest_round_trip() {
        let manifest = CoherenceManifest::build(
            &[
                read("a", &["Post"], DependencyProvenance::Declared),
                read("b", &["Post"], DependencyProvenance::Derived),
                read("c", &[], DependencyProvenance::Undetermined),
            ],
            &[mutation("Post")],
        );
        let summary = manifest.summary();
        assert!(summary.contains("3 cached reads"), "{summary}");
        assert!(summary.contains("1 declared"), "{summary}");
        assert!(summary.contains("1 derived"), "{summary}");
        assert!(summary.contains("1 undetermined"), "{summary}");
        assert!(summary.contains("1 repository mutations"), "{summary}");
    }

    #[test]
    fn undetermined_reads_are_a_warning_by_default_and_an_error_under_strict() {
        let ids = vec!["blog::mystery".to_string()];
        assert!(format_undetermined_diagnostic(&ids, false).starts_with("warning:"));
        assert!(format_undetermined_diagnostic(&ids, true).starts_with("error:"));
        assert!(format_undetermined_diagnostic(&ids, false).contains("blog::mystery"));
    }

    #[test]
    fn a_coherent_app_reports_nothing() {
        let manifest = CoherenceManifest::build(
            &[read("blog::recent", &["Tag"], DependencyProvenance::Declared)],
            &[mutation("Post")],
        );
        let report = format_report(&manifest, false);
        assert!(!report.contains("error:"), "{report}");
        assert!(manifest.violations.is_empty());
    }

    #[test]
    fn manifest_round_trips_through_the_dump_protocol() {
        let manifest = CoherenceManifest::build(
            &[read("blog::recent", &["Post"], DependencyProvenance::Declared)],
            &[mutation("Post")],
        );
        let mut dump = String::from("some unrelated startup logging\n");
        dump.push_str(&format!(
            "{}{}\n",
            autumn_web::cache::coherence::COHERENCE_MANIFEST_MARKER,
            serde_json::to_string(&manifest).unwrap()
        ));
        let parsed = parse_manifest_dump(&dump).expect("marker line must be found");
        assert_eq!(parsed.violations.len(), 1);
        assert_eq!(parsed.violations[0].read, "blog::recent");
    }

    #[test]
    fn a_dump_without_the_marker_is_not_mistaken_for_an_empty_manifest() {
        assert!(parse_manifest_dump("no marker here\n").is_none());
    }
}
