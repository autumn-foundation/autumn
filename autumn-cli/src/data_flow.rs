//! `autumn data-flow` — emit the classified-data flow manifest (issue #1654).
//!
//! Runs the app's own binary in data-flow dump mode (`AUTUMN_DUMP_DATA_FLOW=1`),
//! reads back the manifest the framework assembles from every `#[classified]`
//! column and every declared declassification boundary it links, and writes it
//! out as a build artifact.
//!
//! Why run the binary rather than parse the sources: reachability is a
//! *whole-app* fact. A classified column declared in one crate can be released
//! by a boundary declared in another, or in a plugin the app merely depends on,
//! and link-time `inventory` collection is the only place all of those
//! registrations exist together. This is the same shape as `autumn routes audit`
//! (#1604) and `autumn cache audit` (#1716).
//!
//! There is no gate here, on purpose: the *compiler* is the gate. A classified
//! column with no declared boundary has no expression that reaches a sink, so
//! there is no violation for this command to find. What it produces is the
//! diffable record — `--check` fails when the committed manifest and the build
//! disagree, which is what turns a new release edge into something a reviewer
//! must approve.
//!
//! See `docs/guide/data-classification.md`.

use std::process::Command;

use autumn_web::classify::manifest::{DataFlowManifest, parse_manifest_dump};

use crate::routes;

/// Options controlling `autumn data-flow`.
pub struct DataFlowOptions<'a> {
    /// Cargo package to build and run.
    pub package: Option<&'a str>,
    /// Binary target name for packages that expose multiple bin targets.
    pub bin: Option<&'a str>,
    /// Write the JSON manifest to this path (in addition to any stdout).
    pub manifest: Option<&'a str>,
    /// Emit the JSON manifest to stdout instead of the human report.
    pub json: bool,
    /// Compare against a committed manifest and fail on drift.
    pub check: Option<&'a str>,
    /// Cargo feature selection the inspected binary is built under.
    ///
    /// The manifest describes the binary that produced it. A classified column
    /// or a boundary behind a non-default feature is simply not compiled in, so
    /// it cannot appear in the manifest.
    pub features: routes::CargoFeatures,
}

/// Render the human report for a manifest.
#[must_use]
pub fn format_report(manifest: &DataFlowManifest) -> String {
    manifest.summary()
}

/// Describe the difference between a committed manifest and a fresh one.
///
/// Returns `None` when they agree. The report names *which* rows moved, because
/// "the manifest changed" is not reviewable but "`User.email` gained the
/// `json_response` sink for `marketing_export`" is.
#[must_use]
pub fn format_drift(committed: &DataFlowManifest, current: &DataFlowManifest) -> Option<String> {
    if committed == current {
        return None;
    }
    let mut lines = Vec::new();
    if committed.schema_version != current.schema_version {
        lines.push(format!(
            "  manifest schema version {} -> {}",
            committed.schema_version, current.schema_version
        ));
    }
    let key = |f: &autumn_web::classify::manifest::ClassifiedFieldFlow| {
        format!("{}.{}", f.model, f.field)
    };
    for row in &current.fields {
        match committed.fields.iter().find(|c| key(c) == key(row)) {
            None => lines.push(format!("  + classified field {}", key(row))),
            Some(before) if before != row => {
                let was = sink_list(before);
                let now = sink_list(row);
                lines.push(format!("  ~ {} reaches {was} -> {now}", key(row)));
            }
            Some(_) => {}
        }
    }
    for row in &committed.fields {
        if !current.fields.iter().any(|c| key(c) == key(row)) {
            lines.push(format!("  - classified field {}", key(row)));
        }
    }
    if lines.is_empty() {
        lines.push("  (the documents differ in a field this report does not name)".to_string());
    }
    Some(format!(
        "\u{2717} The data-flow manifest has drifted from the committed copy:\n{}",
        lines.join("\n")
    ))
}

fn sink_list(flow: &autumn_web::classify::manifest::ClassifiedFieldFlow) -> String {
    if flow.reachable_sinks.is_empty() {
        return "no sink".to_string();
    }
    flow.reachable_sinks
        .iter()
        .map(|s| format!("{} for {}", s.sink, s.purpose))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Write the manifest JSON to `path`.
///
/// # Errors
///
/// Returns the underlying I/O error when the file cannot be written.
pub fn write_manifest(manifest: &DataFlowManifest, path: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(path, format!("{}\n", manifest.to_json()))
}

/// Run `autumn data-flow`.
pub fn run(opts: &DataFlowOptions<'_>) {
    eprintln!("\u{1F342} autumn data-flow\n");
    if !opts.features.is_default() {
        eprintln!("Building with {}\n", opts.features.to_args().join(" "));
    }
    routes::compile_binary_with(opts.package, opts.bin, &opts.features);
    let binary = routes::find_binary(opts.package, opts.bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_DATA_FLOW", "1")
        // All three are checked BEFORE the data-flow dump in `AppBuilder::run`,
        // so an exported one in the ambient environment would silently win and
        // hand us a marker-less stdout.
        .env_remove("AUTUMN_BUILD_STATIC")
        .env_remove("AUTUMN_DUMP_ROUTES")
        .env_remove("AUTUMN_DUMP_CACHE_COHERENCE")
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
            "\u{2717} Binary exited with status {} while dumping the data-flow manifest",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(manifest) = parse_manifest_dump(&stdout) else {
        eprintln!(
            "\u{2717} The app produced no data-flow manifest. Either it was built against an \
             autumn-web without `autumn data-flow` support, or it took a different startup path \
             first \u{2014} `AUTUMN_BUILD_STATIC`, `AUTUMN_DUMP_ROUTES` and \
             `AUTUMN_DUMP_CACHE_COHERENCE` are all handled before the manifest dump and are \
             cleared for this run, so an app that exits earlier for its own reasons will land \
             here too."
        );
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    };

    if let Some(path) = opts.manifest {
        if let Err(e) = write_manifest(&manifest, std::path::Path::new(path)) {
            eprintln!("\u{2717} Failed to write manifest to {path}: {e}");
            std::process::exit(1);
        }
        eprintln!("\u{2713} Wrote data-flow manifest \u{2192} {path}");
    }

    if opts.json {
        println!("{}", manifest.to_json());
    } else {
        println!("{}", format_report(&manifest));
    }

    if let Some(path) = opts.check {
        let committed = match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<DataFlowManifest>(&text) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("\u{2717} {path} is not a data-flow manifest: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("\u{2717} Failed to read {path}: {e}");
                std::process::exit(1);
            }
        };
        if let Some(drift) = format_drift(&committed, &manifest) {
            eprintln!("{drift}");
            eprintln!(
                "\nIf the change is intended, re-run with `--manifest {path}` and commit the result."
            );
            std::process::exit(1);
        }
        eprintln!("\u{2713} The data-flow manifest matches {path}.");
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::classify::manifest::{
        ClassifiedFieldDescriptor, DeclassificationDescriptor,
    };
    use autumn_web::classify::{Classification, Sink};

    use super::*;

    fn field(model: &'static str, name: &'static str) -> ClassifiedFieldDescriptor {
        ClassifiedFieldDescriptor {
            model,
            field: name,
            classification: Classification::PersonalData,
        }
    }

    fn release(
        model: &'static str,
        name: &'static str,
        purpose: &'static str,
    ) -> DeclassificationDescriptor {
        DeclassificationDescriptor {
            model,
            field: name,
            classification: Classification::PersonalData,
            purpose,
            sink: Sink::JsonResponse,
            reason: "Support agents need it.",
        }
    }

    #[test]
    fn the_report_names_each_field_and_where_it_can_go() {
        let manifest = DataFlowManifest::build(
            &[field("User", "email"), field("Order", "card_number")],
            &[release("User", "email", "support_lookup")],
        );
        let report = format_report(&manifest);
        assert!(report.contains("User.email"), "{report}");
        assert!(report.contains("json_response for support_lookup"), "{report}");
        assert!(report.contains("Order.card_number"), "{report}");
        assert!(report.contains("no sink"), "{report}");
    }

    #[test]
    fn an_identical_manifest_reports_no_drift() {
        let manifest = DataFlowManifest::build(&[field("User", "email")], &[]);
        assert!(format_drift(&manifest, &manifest).is_none());
    }

    #[test]
    fn a_new_release_edge_is_named_in_the_drift_report() {
        let before = DataFlowManifest::build(&[field("User", "email")], &[]);
        let after = DataFlowManifest::build(
            &[field("User", "email")],
            &[release("User", "email", "marketing_export")],
        );
        let drift = format_drift(&before, &after).expect("drift");
        assert!(drift.contains("User.email"), "{drift}");
        assert!(drift.contains("no sink"), "{drift}");
        assert!(drift.contains("json_response for marketing_export"), "{drift}");
    }

    #[test]
    fn an_added_or_removed_classified_field_is_named() {
        let before = DataFlowManifest::build(&[field("User", "email")], &[]);
        let after = DataFlowManifest::build(
            &[field("User", "email"), field("User", "phone")],
            &[],
        );
        let added = format_drift(&before, &after).expect("drift");
        assert!(added.contains("+ classified field User.phone"), "{added}");
        let removed = format_drift(&after, &before).expect("drift");
        assert!(removed.contains("- classified field User.phone"), "{removed}");
    }

    #[test]
    fn the_manifest_written_to_disk_reads_back_as_the_same_document() {
        let manifest = DataFlowManifest::build(
            &[field("User", "email")],
            &[release("User", "email", "support_lookup")],
        );
        let dir = std::env::temp_dir().join(format!(
            "autumn-data-flow-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("data-flow-manifest.json");
        write_manifest(&manifest, &path).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        let parsed: DataFlowManifest = serde_json::from_str(&text).expect("parse");
        assert_eq!(parsed, manifest);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
