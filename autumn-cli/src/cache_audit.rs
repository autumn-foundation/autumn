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

use std::fmt::Write as _;
use std::process::Command;

use autumn_web::cache::coherence::{
    CoherenceManifest, UndeterminedRead, audit_exit_code, format_diagnostic, gate_failed,
    parse_manifest_dump,
};

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
    /// Cargo feature selection the audited binary is built under.
    ///
    /// The manifest describes the binary that produced it. Auditing the default
    /// feature set says nothing about a deployment that enables others: a read
    /// or a repository behind a non-default feature is simply not compiled in,
    /// so it cannot appear in the manifest and cannot be found incoherent.
    pub features: routes::CargoFeatures,
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
    if !manifest.duplicate_read_ids.is_empty() {
        out.push('\n');
        out.push_str(&format_duplicate_diagnostic(&manifest.duplicate_read_ids));
    }
    let acknowledged = format_acknowledged_report(manifest);
    if !acknowledged.is_empty() {
        out.push('\n');
        out.push_str(&acknowledged);
    }
    out
}

/// Diagnostic for identities claimed by more than one cached read.
///
/// A warning, not a failure: invalidation clears every store registered under
/// the identity, so nothing is silently left stale. But an `invalidates(...)`
/// edge cannot say which of them it means, so the ambiguity gets named.
#[must_use]
pub fn format_duplicate_diagnostic(ids: &[String]) -> String {
    let mut out = format!(
        "warning: {} cache-read {} claimed by more than one registration\n",
        ids.len(),
        if ids.len() == 1 {
            "identity is"
        } else {
            "identities are"
        },
    );
    for id in ids {
        let _ = writeln!(out, "  {id}");
    }
    out.push_str(
        "  an invalidates(...) edge cannot distinguish them, and they share a runtime \
         namespace.\n  fix: rename one of the cached functions, or give the \
         declare_cached_read! entry its own id.\n",
    );
    out
}

/// Diagnostic for reads whose dependency set could not be established.
///
/// A warning by default — a checker that fails on what it merely could not read
/// gets deleted from CI — and an error under `--strict`.
#[must_use]
pub fn format_undetermined_diagnostic(reads: &[UndeterminedRead], strict: bool) -> String {
    let level = if strict { "error" } else { "warning" };
    let mut out = format!(
        "{level}: {} cached read{} could not be linked to any model, so nothing about \
         {} coherence was proven\n",
        reads.len(),
        if reads.len() == 1 { "" } else { "s" },
        if reads.len() == 1 { "its" } else { "their" },
    );
    for read in reads {
        let _ = writeln!(out, "  {} at {}", read.id, read.location);
    }
    out.push_str(
        "  fix: declare the dependency set with #[cached(reads(Model, …))], or acknowledge \
         the gap with #[cached(acknowledge_stale = \"…\")].\n",
    );
    out
}

/// Every acknowledged-stale opt-out in the manifest, so a hatch is never merely
/// a number in the summary line.
#[must_use]
pub fn format_acknowledged_report(manifest: &CoherenceManifest) -> String {
    let mut lines: Vec<String> = Vec::new();
    for read in &manifest.dimensions.cached_reads.entries {
        if let Some(reason) = &read.acknowledged_stale {
            lines.push(format!("  {} at {}\n    {reason}", read.id, read.location));
        }
    }
    for mutation in &manifest.dimensions.mutations.entries {
        if let Some(reason) = &mutation.acknowledged_stale {
            lines.push(format!(
                "  {} at {}\n    {reason}",
                mutation.name, mutation.location
            ));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "note: {} acknowledged-stale opt-out{} are silencing the gate:\n{}\n",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
        lines.join("\n"),
    )
}

/// Write the manifest to `path` as the build artifact CI archives.
///
/// Trailing newline so the file is well-formed for `cat`, `diff` and every
/// line-oriented tool a CI job is likely to point at it.
///
/// # Errors
///
/// Returns the underlying I/O error when the file cannot be written.
pub fn write_manifest(manifest: &CoherenceManifest, path: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(path, format!("{}\n", manifest.to_json()))
}

/// Run `autumn cache audit`.
pub fn run(opts: &CacheAuditOptions<'_>) {
    eprintln!("\u{1F342} autumn cache audit\n");
    // Say which build is being audited whenever it is not the default one, so
    // a manifest is never mistaken for a claim about a feature set it was not
    // built under.
    if !opts.features.is_default() {
        eprintln!("Building with {}\n", opts.features.to_args().join(" "));
    }
    routes::compile_binary_with(opts.package, opts.bin, &opts.features);
    let binary = routes::find_binary(opts.package, opts.bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_CACHE_COHERENCE", "1")
        // Both of these are checked BEFORE the coherence dump in `AppBuilder::run`,
        // so an exported one in the ambient environment would silently win and
        // hand us a marker-less stdout.
        .env_remove("AUTUMN_BUILD_STATIC")
        .env_remove("AUTUMN_DUMP_ROUTES")
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
            "\u{2717} The app produced no cache-coherence manifest. Either it was built against \
             an autumn-web without `autumn cache audit` support, or it took a different startup \
             path first \u{2014} `AUTUMN_BUILD_STATIC` and `AUTUMN_DUMP_ROUTES` are both handled \
             before the manifest dump and are cleared for this run, so an app that exits earlier \
             for its own reasons will land here too."
        );
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    };

    let json = manifest.to_json();
    if let Some(path) = opts.manifest {
        if let Err(e) = write_manifest(&manifest, std::path::Path::new(path)) {
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

    if !gate_failed(&manifest, opts.strict) {
        eprintln!("\u{2713} No cached read can be left stale by a repository write.");
    }

    // The rule lives in `audit_exit_code`, not in a second copy here: the CLI
    // and the framework must never be able to disagree about what fails.
    std::process::exit(audit_exit_code(&manifest, opts.strict));
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
            &[read(
                "blog::recent",
                &["Post"],
                DependencyProvenance::Declared,
            )],
            &[mutation("blog::models::Post")],
        );
        let report = format_report(&manifest, false);
        assert!(report.contains("blog::recent"), "{report}");
        assert!(report.contains("PostRepository::save"), "{report}");
        assert!(report.contains("invalidates("), "{report}");
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

    /// `--strict` must not fail a read the author already opted out of. The
    /// exit code and the printed report have to agree about that, since the
    /// report is what a reviewer reads to decide whether the green is real.
    #[test]
    fn an_acknowledged_undetermined_read_stays_green_under_strict() {
        let mut r = read("blog::ticker", &[], DependencyProvenance::Undetermined);
        r.acknowledged_stale = Some("5s lag is fine on the ticker".to_string());
        let manifest = CoherenceManifest::build(std::slice::from_ref(&r), &[]);

        assert!(manifest.undetermined_reads.is_empty());
        assert_eq!(audit_exit_code(&manifest, true), 0);

        let report = format_report(&manifest, true);
        assert!(!report.contains("error:"), "{report}");
        assert!(report.contains("5s lag is fine on the ticker"), "{report}");
    }

    #[test]
    fn undetermined_reads_are_a_warning_by_default_and_an_error_under_strict() {
        let reads = vec![UndeterminedRead {
            id: "blog::mystery".to_string(),
            location: "src/views.rs:7".to_string(),
        }];
        assert!(format_undetermined_diagnostic(&reads, false).starts_with("warning:"));
        assert!(format_undetermined_diagnostic(&reads, true).starts_with("error:"));
        let text = format_undetermined_diagnostic(&reads, false);
        assert!(text.contains("blog::mystery"), "{text}");
        assert!(
            text.contains("src/views.rs:7"),
            "an id with no location makes the reader grep: {text}"
        );
    }

    #[test]
    fn the_report_names_every_acknowledged_stale_opt_out() {
        // A hatch that shows only as a number in the summary is a hatch nobody
        // reviews.
        let mut r = read("blog::ticker", &["Post"], DependencyProvenance::Declared);
        r.acknowledged_stale = Some("5s lag is fine on the ticker".to_string());
        let mut m = mutation("Post");
        m.acknowledged_stale = Some("seed-only writes".to_string());

        let report = format_report(&CoherenceManifest::build(&[r], &[m]), false);
        assert!(report.contains("2 acknowledged-stale opt-outs"), "{report}");
        assert!(report.contains("5s lag is fine on the ticker"), "{report}");
        assert!(report.contains("seed-only writes"), "{report}");
        assert!(report.contains("src/views.rs:10"), "{report}");
        assert!(report.contains("PostRepository::save"), "{report}");
    }

    #[test]
    fn a_report_with_no_opt_outs_says_nothing_about_them() {
        let manifest = CoherenceManifest::build(
            &[read(
                "blog::recent",
                &["Tag"],
                DependencyProvenance::Declared,
            )],
            &[mutation("Post")],
        );
        assert!(format_acknowledged_report(&manifest).is_empty());
        assert!(!format_report(&manifest, false).contains("acknowledged-stale opt-out"));
    }

    #[test]
    fn the_report_groups_a_missing_edge_and_still_names_every_write() {
        let manifest = CoherenceManifest::build(
            &[read(
                "blog::recent",
                &["Post"],
                DependencyProvenance::Declared,
            )],
            &[
                mutation("Post"),
                Mutation {
                    method: "delete_by_id".to_string(),
                    ..mutation("Post")
                },
            ],
        );
        let report = format_report(&manifest, false);
        assert_eq!(
            report.matches("fix: add invalidates(").count(),
            1,
            "{report}"
        );
        assert!(report.contains("PostRepository::delete_by_id"), "{report}");
    }

    #[test]
    fn a_coherent_app_reports_nothing() {
        let manifest = CoherenceManifest::build(
            &[read(
                "blog::recent",
                &["Tag"],
                DependencyProvenance::Declared,
            )],
            &[mutation("Post")],
        );
        let report = format_report(&manifest, false);
        assert!(!report.contains("error:"), "{report}");
        assert!(manifest.violations.is_empty());
    }

    #[test]
    fn the_manifest_is_written_as_a_reparseable_build_artifact() {
        let dir = std::env::temp_dir().join(format!(
            "autumn-cache-audit-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache-coherence.json");

        let manifest = CoherenceManifest::build(
            &[read(
                "blog::recent",
                &["Post"],
                DependencyProvenance::Declared,
            )],
            &[mutation("blog::models::Post")],
        );
        write_manifest(&manifest, &path).expect("manifest must be written");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.ends_with('\n'),
            "a line-oriented CI tool should not choke on the last line"
        );
        // The artifact is the manifest, not a rendering of it: reading it back
        // must yield the same document, violations included.
        let reparsed: CoherenceManifest = serde_json::from_str(&written).unwrap();
        assert_eq!(reparsed.summary(), manifest.summary());
        assert_eq!(reparsed.violations, manifest.violations);
        assert_eq!(reparsed.violations[0].read, "blog::recent");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writing_the_manifest_to_an_unwritable_path_is_an_error_not_a_panic() {
        let manifest = CoherenceManifest::build(&[], &[]);
        assert!(
            write_manifest(
                &manifest,
                std::path::Path::new("/nonexistent-dir-autumn-1716/manifest.json")
            )
            .is_err()
        );
    }

    #[test]
    fn manifest_round_trips_through_the_dump_protocol() {
        let manifest = CoherenceManifest::build(
            &[read(
                "blog::recent",
                &["Post"],
                DependencyProvenance::Declared,
            )],
            &[mutation("Post")],
        );
        let mut dump = String::from("some unrelated startup logging\n");
        writeln!(
            dump,
            "{}{}",
            autumn_web::cache::coherence::COHERENCE_MANIFEST_MARKER,
            serde_json::to_string(&manifest).unwrap()
        )
        .expect("writing to a String never fails");
        let parsed = parse_manifest_dump(&dump).expect("marker line must be found");
        assert_eq!(parsed.violations.len(), 1);
        assert_eq!(parsed.violations[0].read, "blog::recent");
    }

    #[test]
    fn a_dump_without_the_marker_is_not_mistaken_for_an_empty_manifest() {
        assert!(parse_manifest_dump("no marker here\n").is_none());
    }
}
