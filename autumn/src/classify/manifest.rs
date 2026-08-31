//! The build-time data-flow manifest (issue #1654).
//!
//! The compiler is the gate: a classified column has no path to a sink except a
//! declared [`Declassification`](super::Declassification). This module is the
//! *record* of what those declarations add up to -- one row per classified
//! column, listing every sink it is proven reachable to. An empty reachable set
//! means the column cannot leave the process through any gated sink.
//!
//! # Why it is assembled from `inventory`
//!
//! Reachability is a whole-binary fact. A classified column declared in one
//! crate can be released by a boundary declared in another, or in a plugin the
//! app merely depends on, and link-time `inventory` collection is the only place
//! all of those registrations exist together. `autumn data-flow` therefore
//! builds the app and runs it under `AUTUMN_DUMP_DATA_FLOW=1` to read the
//! manifest back -- the same shape as `autumn routes audit` (#1604) and
//! `autumn cache audit` (#1716).
//!
//! See `docs/guide/data-classification.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{Classification, Sink};

/// Schema version of the emitted data-flow manifest. Bumped only on breaking
/// changes to the document shape.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Machine-readable stdout marker preceding the manifest JSON emitted by the
/// `AUTUMN_DUMP_DATA_FLOW=1` dump mode.
///
/// A process-boundary protocol: `autumn data-flow` runs the built binary as a
/// child and scans its stdout for this marker, so an app that prints anything
/// else during startup cannot corrupt the parse.
pub const DATA_FLOW_MANIFEST_MARKER: &str = "[autumn:data-flow] ";

// ── Descriptors published by the macros ──────────────────────────────

/// One `#[classified]` column, published by `#[model]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifiedFieldDescriptor {
    /// The model type's name, for display.
    pub model: &'static str,
    /// The model type's module-qualified path -- the join key, so two crates
    /// that each define a `Customer` cannot share one manifest row.
    pub model_path: &'static str,
    /// The column's Rust field name.
    pub field: &'static str,
    /// The tier it was annotated with.
    pub classification: Classification,
}

inventory::collect!(ClassifiedFieldDescriptor);

/// One declared declassification boundary, published by
/// [`declassify!`](crate::declassify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclassificationDescriptor {
    /// The model the released column belongs to, for display.
    pub model: &'static str,
    /// That model's module-qualified path -- the join key.
    pub model_path: &'static str,
    /// The released column's field name.
    pub field: &'static str,
    /// The tier the column was classified at.
    pub classification: Classification,
    /// The declared purpose.
    pub purpose: &'static str,
    /// The sink the release is approved for.
    pub sink: Sink,
    /// The declarer's justification.
    pub reason: &'static str,
}

inventory::collect!(DeclassificationDescriptor);

// ── The manifest document ────────────────────────────────────────────

/// One sink a classified column is proven reachable to, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReachableSink {
    /// Where the released value may go.
    pub sink: Sink,
    /// The declared purpose of the release.
    pub purpose: String,
    /// The declarer's justification.
    pub reason: String,
}

/// One classified column and everywhere it can reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedFieldFlow {
    /// The model type's name, for display.
    pub model: String,
    /// The model type's module-qualified path, which is what makes this row
    /// distinct from a same-named model in another module or crate.
    pub model_path: String,
    /// The column's Rust field name.
    pub field: String,
    /// The tier it was annotated with.
    pub classification: Classification,
    /// Every sink a declared boundary releases it to. Empty means no leak.
    pub reachable_sinks: Vec<ReachableSink>,
}

/// The whole binary's classified-data flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataFlowManifest {
    /// Document shape version.
    pub schema_version: u32,
    /// The sinks this build can prove about, in manifest order.
    pub gated_sinks: Vec<String>,
    /// One row per classified column, sorted by `(model, field)`.
    pub fields: Vec<ClassifiedFieldFlow>,
}

impl DataFlowManifest {
    /// Join classified columns against declared boundaries.
    ///
    /// The join key is `(model, field)` -- the pair `#[model]` publishes and the
    /// pair [`declassify!`](crate::declassify) recovers from the field marker,
    /// so the two sides can never disagree about which column a boundary
    /// releases.
    #[must_use]
    pub fn build(
        fields: &[ClassifiedFieldDescriptor],
        releases: &[DeclassificationDescriptor],
    ) -> Self {
        // Keyed on the module-qualified path, not the bare name: two linked
        // crates can each define a `Customer` with a classified `email`, and
        // merging them would hide one model's column behind the other's row.
        let mut rows: BTreeMap<(&str, &str), ClassifiedFieldFlow> = BTreeMap::new();
        for descriptor in fields {
            rows.entry((descriptor.model_path, descriptor.field))
                .or_insert_with(|| ClassifiedFieldFlow {
                    model: descriptor.model.to_string(),
                    model_path: descriptor.model_path.to_string(),
                    field: descriptor.field.to_string(),
                    classification: descriptor.classification,
                    reachable_sinks: Vec::new(),
                });
        }
        for release in releases {
            // A boundary for a column no `#[model]` published cannot happen
            // through `declassify!` (the field marker comes from the model), so
            // there is nothing to report -- but the row is created rather than
            // dropped, because silently losing a release edge is the one failure
            // mode a leak manifest must not have.
            let row = rows
                .entry((release.model_path, release.field))
                .or_insert_with(|| ClassifiedFieldFlow {
                    model: release.model.to_string(),
                    model_path: release.model_path.to_string(),
                    field: release.field.to_string(),
                    classification: release.classification,
                    reachable_sinks: Vec::new(),
                });
            row.reachable_sinks.push(ReachableSink {
                sink: release.sink,
                purpose: release.purpose.to_string(),
                reason: release.reason.to_string(),
            });
        }
        let mut fields: Vec<ClassifiedFieldFlow> = rows.into_values().collect();
        for row in &mut fields {
            // Sort on every field `dedup` compares. `inventory` hands descriptors
            // back in link order, which is unspecified across builds, so two
            // boundaries sharing a sink and purpose but differing in reason would
            // otherwise reorder between builds and show up as spurious drift in
            // `autumn data-flow --check`.
            row.reachable_sinks.sort_by(|a, b| {
                (a.sink, &a.purpose, &a.reason).cmp(&(b.sink, &b.purpose, &b.reason))
            });
            row.reachable_sinks.dedup();
        }
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            gated_sinks: Sink::all().iter().map(|s| s.as_str().to_string()).collect(),
            fields,
        }
    }

    /// The manifest as pretty JSON, ready to commit and diff.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// The single stdout line the dump mode emits.
    #[must_use]
    pub fn to_dump_line(&self) -> String {
        format!(
            "{DATA_FLOW_MANIFEST_MARKER}{}",
            serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
        )
    }

    /// A human report: one line per classified column and where it can go.
    #[must_use]
    pub fn summary(&self) -> String {
        let released = self
            .fields
            .iter()
            .filter(|f| !f.reachable_sinks.is_empty())
            .count();
        let mut out = format!(
            "{} classified field{} across {} model{}; {} released to a sink, {} reaching none.\n\
             Gated sinks: {}.",
            self.fields.len(),
            plural(self.fields.len()),
            self.model_count(),
            plural(self.model_count()),
            released,
            self.fields.len().saturating_sub(released),
            self.gated_sinks.join(", "),
        );
        for row in &self.fields {
            if row.reachable_sinks.is_empty() {
                let _ = write!(
                    out,
                    "\n  {}.{} ({}) -> no sink (no declassification boundary declared)",
                    self.display_model(row),
                    row.field,
                    row.classification
                );
            } else {
                for reach in &row.reachable_sinks {
                    let _ = write!(
                        out,
                        "\n  {}.{} ({}) -> {} for {}",
                        self.display_model(row),
                        row.field,
                        row.classification,
                        reach.sink,
                        reach.purpose
                    );
                }
            }
        }
        out
    }

    fn model_count(&self) -> usize {
        let mut models: Vec<&str> = self.fields.iter().map(|f| f.model_path.as_str()).collect();
        models.sort_unstable();
        models.dedup();
        models.len()
    }

    /// How a row is named in the human report: the short name, unless another
    /// row's model shares it, in which case the full path is the only honest
    /// spelling.
    fn display_model(&self, row: &ClassifiedFieldFlow) -> String {
        let ambiguous = self
            .fields
            .iter()
            .any(|other| other.model == row.model && other.model_path != row.model_path);
        if ambiguous {
            row.model_path.clone()
        } else {
            row.model.clone()
        }
    }
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Assemble the manifest from everything linked into this binary.
#[must_use]
pub fn audit() -> DataFlowManifest {
    let fields: Vec<ClassifiedFieldDescriptor> = inventory::iter::<ClassifiedFieldDescriptor>
        .into_iter()
        .copied()
        .collect();
    let releases: Vec<DeclassificationDescriptor> = inventory::iter::<DeclassificationDescriptor>
        .into_iter()
        .copied()
        .collect();
    DataFlowManifest::build(&fields, &releases)
}

/// Whether the process was started to dump the manifest rather than serve.
#[must_use]
pub fn is_dump_mode() -> bool {
    std::env::var("AUTUMN_DUMP_DATA_FLOW").as_deref() == Ok("1")
}

/// Print the marker-prefixed manifest line the CLI parses.
pub fn print_manifest_dump(manifest: &DataFlowManifest) {
    println!("{}", manifest.to_dump_line());
}

/// Recover a manifest from a child process's stdout.
///
/// Scans for [`DATA_FLOW_MANIFEST_MARKER`] so unrelated startup output cannot
/// corrupt the parse. Returns `None` when no marker line parses.
#[must_use]
pub fn parse_manifest_dump(stdout: &str) -> Option<DataFlowManifest> {
    stdout
        .lines()
        .filter_map(|line| line.split_once(DATA_FLOW_MANIFEST_MARKER))
        .filter_map(|(_, json)| serde_json::from_str::<DataFlowManifest>(json.trim()).ok())
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(model: &'static str, name: &'static str) -> ClassifiedFieldDescriptor {
        ClassifiedFieldDescriptor {
            model,
            model_path: model,
            field: name,
            classification: Classification::PersonalData,
        }
    }

    /// Same display name, different module -- the collision the join key exists
    /// to keep apart.
    fn field_in(
        path: &'static str,
        model: &'static str,
        name: &'static str,
    ) -> ClassifiedFieldDescriptor {
        ClassifiedFieldDescriptor {
            model,
            model_path: path,
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
            model_path: model,
            field: name,
            classification: Classification::PersonalData,
            purpose,
            sink: Sink::JsonResponse,
            reason: "Support agents need it.",
        }
    }

    #[test]
    fn a_field_with_no_boundary_reaches_no_sink() {
        let manifest = DataFlowManifest::build(&[field("Order", "card_number")], &[]);
        assert_eq!(manifest.fields.len(), 1);
        assert!(manifest.fields[0].reachable_sinks.is_empty());
        assert!(
            manifest.summary().contains("no sink"),
            "{}",
            manifest.summary()
        );
    }

    #[test]
    fn the_join_is_keyed_on_model_and_field() {
        let manifest = DataFlowManifest::build(
            &[field("User", "email"), field("Order", "email")],
            &[release("User", "email", "support_lookup")],
        );
        assert_eq!(manifest.fields.len(), 2);
        // Sorted by (model, field): Order first.
        assert_eq!(manifest.fields[0].model, "Order");
        assert!(manifest.fields[0].reachable_sinks.is_empty());
        assert_eq!(manifest.fields[1].model, "User");
        assert_eq!(manifest.fields[1].reachable_sinks.len(), 1);
        assert_eq!(
            manifest.fields[1].reachable_sinks[0].purpose,
            "support_lookup"
        );
    }

    #[test]
    fn several_boundaries_on_one_field_all_appear() {
        let manifest = DataFlowManifest::build(
            &[field("User", "email")],
            &[
                release("User", "email", "support_lookup"),
                release("User", "email", "billing_receipt"),
            ],
        );
        let purposes: Vec<&str> = manifest.fields[0]
            .reachable_sinks
            .iter()
            .map(|r| r.purpose.as_str())
            .collect();
        assert_eq!(purposes, ["billing_receipt", "support_lookup"]);
    }

    #[test]
    fn the_row_order_does_not_depend_on_registration_order() {
        let a = release("User", "email", "support_lookup");
        let mut b = release("User", "email", "support_lookup");
        b.reason = "A different justification, same sink and purpose.";
        let forwards = DataFlowManifest::build(&[field("User", "email")], &[a, b]);
        let backwards = DataFlowManifest::build(&[field("User", "email")], &[b, a]);
        assert_eq!(forwards, backwards);
        assert_eq!(forwards.fields[0].reachable_sinks.len(), 2);
    }

    #[test]
    fn same_named_models_in_different_modules_stay_separate_rows() {
        let manifest = DataFlowManifest::build(
            &[
                field_in("billing::Customer", "Customer", "email"),
                field_in("support::Customer", "Customer", "email"),
            ],
            &[],
        );
        assert_eq!(manifest.fields.len(), 2, "{manifest:?}");
        // Ambiguous short names are reported by their full path, or the report
        // would show the same row twice.
        let summary = manifest.summary();
        assert!(summary.contains("billing::Customer.email"), "{summary}");
        assert!(summary.contains("support::Customer.email"), "{summary}");
        assert!(summary.contains("2 models"), "{summary}");
    }

    #[test]
    fn identical_boundaries_are_deduplicated() {
        let manifest = DataFlowManifest::build(
            &[field("User", "email")],
            &[
                release("User", "email", "support_lookup"),
                release("User", "email", "support_lookup"),
            ],
        );
        assert_eq!(manifest.fields[0].reachable_sinks.len(), 1);
    }

    #[test]
    fn a_release_for_an_unpublished_field_still_appears() {
        let manifest = DataFlowManifest::build(&[], &[release("Ghost", "email", "p")]);
        assert_eq!(manifest.fields.len(), 1);
        assert_eq!(manifest.fields[0].model, "Ghost");
        assert_eq!(manifest.fields[0].reachable_sinks.len(), 1);
    }

    #[test]
    fn duplicate_field_registrations_collapse_to_one_row() {
        let manifest =
            DataFlowManifest::build(&[field("User", "email"), field("User", "email")], &[]);
        assert_eq!(manifest.fields.len(), 1);
    }

    #[test]
    fn the_dump_line_round_trips_through_the_marker() {
        let manifest = DataFlowManifest::build(
            &[field("User", "email")],
            &[release("User", "email", "support_lookup")],
        );
        let stdout = format!("booting\n{}\ndone\n", manifest.to_dump_line());
        let parsed = parse_manifest_dump(&stdout).expect("manifest parses");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn stdout_without_a_marker_parses_to_nothing() {
        assert!(parse_manifest_dump("booting\ndone\n").is_none());
        assert!(parse_manifest_dump(&format!("{DATA_FLOW_MANIFEST_MARKER}not json")).is_none());
    }

    #[test]
    fn the_json_document_carries_its_schema_version_and_gated_sinks() {
        let manifest = DataFlowManifest::build(&[field("User", "email")], &[]);
        let json = manifest.to_json();
        assert!(json.contains("\"schema_version\": 1"), "{json}");
        assert!(json.contains("json_response"), "{json}");
        assert!(json.contains("personal_data"), "{json}");
    }

    #[test]
    fn the_summary_counts_models_and_released_fields() {
        let manifest = DataFlowManifest::build(
            &[
                field("User", "email"),
                field("User", "phone"),
                field("Order", "email"),
            ],
            &[release("User", "email", "support_lookup")],
        );
        let summary = manifest.summary();
        assert!(summary.contains("3 classified fields"), "{summary}");
        assert!(summary.contains("2 models"), "{summary}");
        assert!(summary.contains("1 released to a sink"), "{summary}");
        assert!(summary.contains("2 reaching none"), "{summary}");
    }
}
