//! `autumn plugin index check` and `autumn plugin index record`: the
//! maintainer side of the plugin index (issue #1625).
//!
//! `check` is the gate: admission rules plus re-verification against the
//! current release. `record` writes an `autumn plugin-check --format json`
//! report into a listing. A pass lists it. A fail flags it incompatible. A
//! second fail on a later release delists it.

use std::path::{Path, PathBuf};

use super::index::{self, CheckOutcome, Listing, ListingOrigin, Status, Tier};
use crate::plugin_check::{CheckStatus, ConformanceReport};

/// What `record` did to a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The run passed: the listing is listed.
    Listed,
    /// The run failed: the listing is flagged incompatible.
    Flagged,
    /// The run failed again on a later release: the listing is delisted.
    Delisted,
}

/// Write `report` into `listing`, as a run against `against` on `date`.
///
/// # Errors
///
/// When the report names a different plugin.
pub fn apply_report(
    listing: &mut Listing,
    report: &ConformanceReport,
    against: &str,
    date: &str,
) -> Result<Transition, String> {
    if report.plugin_name != listing.name {
        return Err(format!(
            "the report is for `{}`, not `{}`",
            report.plugin_name, listing.name
        ));
    }
    // The contract is the machine-checked source of the range and the tier.
    if let Some(contract) = &report.contract {
        if let Some(range) = &contract.autumn_web {
            listing.autumn_web.clone_from(range);
        }
        if let Some(version) = &contract.plugin_version {
            listing.version.clone_from(version);
        }
        listing
            .experimental_surfaces
            .clone_from(&contract.experimental_surfaces);
        listing.tier = if listing.experimental_surfaces.is_empty() {
            Tier::Stable
        } else {
            Tier::Experimental
        };
    }

    let previous = std::mem::replace(&mut listing.conformance.autumn_web, against.to_owned());
    date.clone_into(&mut listing.conformance.checked);
    listing.conformance.reason.clear();

    if report.passed() {
        listing.conformance.result = CheckOutcome::Pass;
        listing.status = Status::Listed;
        listing.note.clear();
        return Ok(Transition::Listed);
    }

    listing.conformance.result = CheckOutcome::Fail;
    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .map(|c| c.name.as_str())
        .collect();
    let failed = failed.join(", ");
    let second_release = listing.status != Status::Listed && previous != against;
    if second_release {
        listing.status = Status::Delisted;
        listing.note =
            format!("failed plugin-check on autumn-web {previous} and {against} ({failed})");
        Ok(Transition::Delisted)
    } else {
        listing.status = Status::Incompatible;
        listing.note = format!("failed plugin-check on autumn-web {against} ({failed})");
        Ok(Transition::Flagged)
    }
}

/// Refresh an exempt listing after the install gate passed on `against`.
///
/// # Errors
///
/// When the listing is not exempt: a `Plugin` must bring a report.
pub fn apply_exempt(listing: &mut Listing, against: &str, date: &str) -> Result<(), String> {
    if listing.conformance.result != CheckOutcome::Exempt {
        return Err(format!(
            "`{}` is not exempt; record its `autumn plugin-check` report instead",
            listing.name
        ));
    }
    against.clone_into(&mut listing.conformance.autumn_web);
    date.clone_into(&mut listing.conformance.checked);
    // First-party crates are lockstep: the release is their version.
    if listing.origin == ListingOrigin::FirstParty {
        against.clone_into(&mut listing.version);
    }
    Ok(())
}

/// Write the fields `record` owns back into the index text, and keep every
/// other line (comments, order) as it is.
///
/// # Errors
///
/// When the text does not parse or has no listing for `listing.name`.
pub fn write_listing(src: &str, listing: &Listing) -> Result<String, String> {
    use toml_edit::{Array, DocumentMut, Item, Table, value};

    fn set_or_remove(table: &mut Table, key: &str, text: &str) {
        if text.is_empty() {
            table.remove(key);
        } else {
            table[key] = value(text);
        }
    }
    fn to_value<T: serde::Serialize>(v: &T) -> String {
        serde_json::to_value(v)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    let mut doc: DocumentMut = src
        .parse()
        .map_err(|e| format!("the index does not parse: {e}"))?;
    let tables = doc
        .get_mut("plugin")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or("the index has no [[plugin]] listings")?;
    let table = tables
        .iter_mut()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(listing.name.as_str()))
        .ok_or_else(|| format!("the index has no listing for `{}`", listing.name))?;

    table["version"] = value(&listing.version);
    table["autumn_web"] = value(&listing.autumn_web);
    table["tier"] = value(to_value(&listing.tier));
    table["status"] = value(to_value(&listing.status));
    set_or_remove(table, "note", &listing.note);
    if listing.experimental_surfaces.is_empty() {
        table.remove("experimental_surfaces");
    } else {
        table["experimental_surfaces"] =
            value(listing.experimental_surfaces.iter().collect::<Array>());
    }
    let run = &listing.conformance;
    let conformance = table
        .get_mut("conformance")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("`{}` has no [plugin.conformance] table", listing.name))?;
    conformance["result"] = value(to_value(&run.result));
    conformance["autumn_web"] = value(&run.autumn_web);
    conformance["checked"] = value(&run.checked);
    set_or_remove(conformance, "reason", &run.reason);
    Ok(doc.to_string())
}

/// Render the findings `check` found.
#[must_use]
pub fn render_findings(findings: &[index::Finding], source: &str, against: &str) -> String {
    use std::fmt::Write as _;

    if findings.is_empty() {
        return format!("Plugin index {source} passes for autumn-web {against}.");
    }
    let mut out = format!(
        "Plugin index {source}: {} finding{} for autumn-web {against}:\n",
        findings.len(),
        if findings.len() == 1 { "" } else { "s" }
    );
    for finding in findings {
        let _ = writeln!(out, "  {}: {}", finding.plugin, finding.message);
    }
    out.push_str(
        "\nRe-verify with `autumn plugin-check --format json`, then \
         `autumn plugin index record`. See autumn-cli/plugin-index/README.md.",
    );
    out
}

/// Options for `autumn plugin index check`.
#[derive(Debug, Clone, Copy)]
pub struct CheckOptions<'a> {
    /// The index file. `None`: [`index::OVERRIDE_ENV`] or the bundled copy.
    pub index: Option<&'a Path>,
    /// The current `autumn-web` release.
    pub against: &'a str,
    /// Emit JSON.
    pub json: bool,
}

/// Run `autumn plugin index check`. Returns the exit code.
#[must_use]
pub fn run_check(opts: &CheckOptions<'_>) -> i32 {
    let loaded = opts
        .index
        .map_or_else(index::load_from_env, |path| index::load(Some(path)));
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("autumn plugin index check: {err}");
            return 1;
        }
    };
    let source = match &loaded.source {
        index::Source::Bundled => "(bundled)".to_owned(),
        index::Source::Override(path) => path.display().to_string(),
    };
    let findings = index::check(&loaded.index, opts.against);
    if opts.json {
        let document = serde_json::json!({
            "index": source,
            "autumn_web": opts.against,
            "passed": findings.is_empty(),
            "findings": findings,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        println!("{}", render_findings(&findings, &source, opts.against));
    }
    i32::from(!findings.is_empty())
}

/// Options for `autumn plugin index record`.
#[derive(Debug, Clone)]
pub struct RecordOptions<'a> {
    /// The index file to update.
    pub index: &'a Path,
    /// `autumn plugin-check --format json` reports.
    pub reports: &'a [PathBuf],
    /// Exempt listings whose install gate passed.
    pub exempt: &'a [String],
    /// The `autumn-web` release the runs used.
    pub against: &'a str,
    /// The run date, `YYYY-MM-DD`.
    pub date: &'a str,
}

/// Run `autumn plugin index record`. Returns the exit code.
#[must_use]
pub fn run_record(opts: &RecordOptions<'_>) -> i32 {
    match record(opts) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(err) => {
            eprintln!("autumn plugin index record: {err}. The index was not changed.");
            1
        }
    }
}

/// Apply every report and exemption, then write the file once. Nothing is
/// written when any step fails, or when the result breaks an admission rule.
fn record(opts: &RecordOptions<'_>) -> Result<Vec<String>, String> {
    if chrono::NaiveDate::parse_from_str(opts.date, "%Y-%m-%d").is_err() {
        return Err(format!("`--date {}` is not a YYYY-MM-DD date", opts.date));
    }
    if semver::Version::parse(opts.against).is_err() {
        return Err(format!(
            "`--against {}` is not a semver version",
            opts.against
        ));
    }
    let mut src = std::fs::read_to_string(opts.index)
        .map_err(|e| format!("{}: {e}", opts.index.display()))?;
    let parsed = index::parse(&src).map_err(|e| e.to_string())?;
    let mut lines = Vec::new();

    for path in opts.reports {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let report: ConformanceReport = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a plugin-check JSON report: {e}", path.display()))?;
        let mut listing = parsed
            .get(&report.plugin_name)
            .cloned()
            .ok_or_else(|| format!("the index has no listing for `{}`", report.plugin_name))?;
        let transition = apply_report(&mut listing, &report, opts.against, opts.date)?;
        src = write_listing(&src, &listing)?;
        lines.push(format!(
            "{}: {transition:?} on autumn-web {}",
            listing.name, opts.against
        ));
    }
    for name in opts.exempt {
        let mut listing = parsed
            .get(name)
            .cloned()
            .ok_or_else(|| format!("the index has no listing for `{name}`"))?;
        apply_exempt(&mut listing, opts.against, opts.date)?;
        src = write_listing(&src, &listing)?;
        lines.push(format!(
            "{name}: exempt, refreshed for autumn-web {}",
            opts.against
        ));
    }

    let result = index::parse(&src).map_err(|e| e.to_string())?;
    let findings = index::validate(&result);
    if !findings.is_empty() {
        return Err(format!(
            "the result breaks {} admission rule(s), first: {}: {}",
            findings.len(),
            findings[0].plugin,
            findings[0].message
        ));
    }
    std::fs::write(opts.index, &src).map_err(|e| format!("{}: {e}", opts.index.display()))?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_check::CheckResult;
    use autumn_web::plugin_contract::PluginContract;

    fn listing(name: &str) -> Listing {
        index::parse(index::BUNDLED)
            .expect("bundled")
            .get(name)
            .expect("listing")
            .clone()
    }

    fn check(name: &str, status: CheckStatus) -> CheckResult {
        CheckResult {
            name: name.to_owned(),
            status,
            message: String::new(),
            diagnostics: vec![],
        }
    }

    fn report(name: &str, pass: bool, contract: Option<PluginContract>) -> ConformanceReport {
        ConformanceReport {
            plugin_name: name.to_owned(),
            checks: vec![
                check("installability", CheckStatus::Pass),
                check(
                    "route-collision",
                    if pass {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                ),
            ],
            contract,
        }
    }

    fn lockstep(name: &str, version: &str) -> PluginContract {
        autumn_web::plugin_contract::lockstep_contract(name, version)
    }

    // ── apply_report ────────────────────────────────────────────────────

    /// AC 4: a pass on a new release refreshes the listing for it.
    #[test]
    fn a_pass_lists_the_plugin_on_the_new_release() {
        let mut l = listing("autumn-admin-plugin");
        let r = report(
            "autumn-admin-plugin",
            true,
            Some(lockstep("autumn-admin-plugin", "0.8.0")),
        );
        let t = apply_report(&mut l, &r, "0.8.0", "2026-10-01").expect("apply");
        assert_eq!(t, Transition::Listed);
        assert_eq!(l.status, Status::Listed);
        assert_eq!(l.conformance.result, CheckOutcome::Pass);
        assert_eq!(l.conformance.autumn_web, "0.8.0");
        assert_eq!(l.conformance.checked, "2026-10-01");
        // The range and version come from the contract, not a hand edit.
        assert_eq!(l.autumn_web, "0.8");
        assert_eq!(l.version, "0.8.0");
    }

    /// AC 4: a failed re-verification flags the listing incompatible and
    /// names the failed checks.
    #[test]
    fn a_fail_flags_the_listing() {
        let mut l = listing("autumn-admin-plugin");
        let r = report("autumn-admin-plugin", false, None);
        let t = apply_report(&mut l, &r, "0.8.0", "2026-10-01").expect("apply");
        assert_eq!(t, Transition::Flagged);
        assert_eq!(l.status, Status::Incompatible);
        assert_eq!(l.conformance.result, CheckOutcome::Fail);
        assert!(l.note.contains("route-collision"), "{}", l.note);
        assert!(l.note.contains("0.8.0"), "{}", l.note);
    }

    /// AC 4: a second fail on a later release delists it.
    #[test]
    fn a_second_fail_on_a_later_release_delists_the_listing() {
        let mut l = listing("autumn-admin-plugin");
        let r = report("autumn-admin-plugin", false, None);
        apply_report(&mut l, &r, "0.8.0", "2026-10-01").expect("flag");
        let t = apply_report(&mut l, &r, "0.9.0", "2026-11-01").expect("delist");
        assert_eq!(t, Transition::Delisted);
        assert_eq!(l.status, Status::Delisted);
        assert!(
            l.note.contains("0.8.0") && l.note.contains("0.9.0"),
            "{}",
            l.note
        );
    }

    /// A second run on the SAME release is a retry, not a second release.
    #[test]
    fn a_repeat_fail_on_the_same_release_stays_flagged() {
        let mut l = listing("autumn-admin-plugin");
        let r = report("autumn-admin-plugin", false, None);
        apply_report(&mut l, &r, "0.8.0", "2026-10-01").expect("flag");
        let t = apply_report(&mut l, &r, "0.8.0", "2026-10-02").expect("retry");
        assert_eq!(t, Transition::Flagged);
    }

    /// A pass after a flag relists and clears the note.
    #[test]
    fn a_pass_after_a_flag_relists() {
        let mut l = listing("autumn-admin-plugin");
        apply_report(
            &mut l,
            &report("autumn-admin-plugin", false, None),
            "0.8.0",
            "2026-10-01",
        )
        .expect("flag");
        apply_report(
            &mut l,
            &report("autumn-admin-plugin", true, None),
            "0.8.0",
            "2026-10-02",
        )
        .expect("relist");
        assert_eq!(l.status, Status::Listed);
        assert!(l.note.is_empty(), "{}", l.note);
    }

    /// AC 5: the tier comes from the contract the report carries.
    #[test]
    fn the_tier_follows_the_declared_experimental_surface() {
        let surface = autumn_web::plugin_contract::experimental_surface_names()
            .next()
            .expect("surface");
        let contract = lockstep("autumn-admin-plugin", "0.7.0").uses_experimental(surface);
        let mut l = listing("autumn-admin-plugin");
        apply_report(
            &mut l,
            &report("autumn-admin-plugin", true, Some(contract)),
            "0.7.0",
            "2026-10-01",
        )
        .expect("apply");
        assert_eq!(l.tier, Tier::Experimental);
        assert_eq!(l.experimental_surfaces, [surface]);
    }

    #[test]
    fn a_report_for_another_plugin_is_refused() {
        let mut l = listing("autumn-admin-plugin");
        let err = apply_report(
            &mut l,
            &report("autumn-search", true, None),
            "0.7.0",
            "2026-10-01",
        )
        .unwrap_err();
        assert!(err.contains("autumn-search"), "{err}");
    }

    // ── apply_exempt ────────────────────────────────────────────────────

    #[test]
    fn an_exempt_listing_is_refreshed_for_the_release() {
        let mut l = listing("autumn-storage-s3");
        apply_exempt(&mut l, "0.8.0", "2026-10-01").expect("exempt");
        assert_eq!(l.conformance.autumn_web, "0.8.0");
        assert_eq!(l.conformance.result, CheckOutcome::Exempt);
        assert_eq!(l.version, "0.8.0");
    }

    #[test]
    fn a_plugin_listing_cannot_be_refreshed_as_exempt() {
        let mut l = listing("autumn-admin-plugin");
        let err = apply_exempt(&mut l, "0.8.0", "2026-10-01").unwrap_err();
        assert!(err.contains("report"), "{err}");
    }

    // ── write_listing ───────────────────────────────────────────────────

    /// `record` edits only the fields it owns: comments and order stay.
    #[test]
    fn write_listing_keeps_comments_and_updates_fields() {
        let mut l = listing("autumn-admin-plugin");
        apply_report(
            &mut l,
            &report("autumn-admin-plugin", false, None),
            "0.7.0",
            "2026-10-01",
        )
        .expect("flag");
        let out = write_listing(index::BUNDLED, &l).expect("write");
        assert!(out.starts_with("# The Autumn plugin index"), "comment lost");
        let parsed = index::parse(&out).expect("still parses");
        assert_eq!(parsed.get("autumn-admin-plugin"), Some(&l));
        assert_eq!(
            parsed.get("autumn-search"),
            index::parse(index::BUNDLED)
                .expect("bundled")
                .get("autumn-search"),
            "other listings must not change"
        );
    }

    /// Clearing a note removes the key rather than writing `note = ""`.
    #[test]
    fn write_listing_removes_empty_optional_keys() {
        let mut l = listing("autumn-admin-plugin");
        apply_report(
            &mut l,
            &report("autumn-admin-plugin", false, None),
            "0.7.0",
            "2026-10-01",
        )
        .expect("flag");
        let flagged = write_listing(index::BUNDLED, &l).expect("write");
        apply_report(
            &mut l,
            &report("autumn-admin-plugin", true, None),
            "0.7.0",
            "2026-10-02",
        )
        .expect("relist");
        let out = write_listing(&flagged, &l).expect("write");
        assert!(!out.contains("note ="), "{out}");
    }

    #[test]
    fn write_listing_refuses_an_unknown_name() {
        let mut l = listing("autumn-admin-plugin");
        l.name = "autumn-plugin-ghost".to_owned();
        assert!(write_listing(index::BUNDLED, &l).is_err());
    }

    // ── render_findings ─────────────────────────────────────────────────

    #[test]
    fn findings_render_one_per_line_with_a_count() {
        let findings = vec![index::Finding {
            plugin: "autumn-plugin-x".to_owned(),
            message: "last verified against autumn-web 0.7.0; re-verify against 0.8.0".to_owned(),
        }];
        let out = render_findings(&findings, "index.toml", "0.8.0");
        assert!(out.contains("1 finding"), "{out}");
        assert!(out.contains("autumn-plugin-x: last verified"), "{out}");
        let clean = render_findings(&[], "index.toml", "0.8.0");
        assert!(clean.contains("passes"), "{clean}");
    }

    // ── run_record end to end ───────────────────────────────────────────

    #[test]
    fn run_record_writes_reports_into_the_index_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("index.toml");
        std::fs::write(&index_path, index::BUNDLED).expect("write index");
        let report_path = dir.path().join("admin.json");
        let r = report("autumn-admin-plugin", false, None);
        std::fs::write(&report_path, serde_json::to_string(&r).expect("json")).expect("write");

        let code = run_record(&RecordOptions {
            index: &index_path,
            reports: &[report_path],
            exempt: &["autumn-storage-s3".to_owned()],
            against: "0.7.0",
            date: "2026-10-01",
        });
        assert_eq!(code, 0);
        let written =
            index::parse(&std::fs::read_to_string(&index_path).expect("read")).expect("parse");
        let admin = written.get("autumn-admin-plugin").expect("admin");
        assert_eq!(admin.status, Status::Incompatible);
        let s3 = written.get("autumn-storage-s3").expect("s3");
        assert_eq!(s3.conformance.checked, "2026-10-01");
    }

    #[test]
    fn run_record_refuses_a_date_that_is_not_a_date() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("index.toml");
        std::fs::write(&index_path, index::BUNDLED).expect("write index");
        let code = run_record(&RecordOptions {
            index: &index_path,
            reports: &[],
            exempt: &["autumn-storage-s3".to_owned()],
            against: "0.7.0",
            date: "tomorrow",
        });
        assert_eq!(code, 1);
        assert_eq!(
            std::fs::read_to_string(&index_path).expect("read"),
            index::BUNDLED,
            "a refused record writes nothing"
        );
    }

    #[test]
    fn run_check_passes_the_bundled_index_on_this_release() {
        let code = run_check(&CheckOptions {
            index: None,
            against: env!("CARGO_PKG_VERSION"),
            json: false,
        });
        assert_eq!(code, 0);
    }

    #[test]
    fn run_check_fails_the_bundled_index_on_a_later_release() {
        let code = run_check(&CheckOptions {
            index: None,
            against: "99.0.0",
            json: true,
        });
        assert_eq!(code, 1);
    }
}
