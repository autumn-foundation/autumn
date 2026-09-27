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
    if index::canonical(&report.plugin_name) != index::canonical(&listing.name) {
        return Err(format!(
            "the report is for `{}`, not `{}`",
            index::sanitize(&report.plugin_name),
            listing.name
        ));
    }
    if listing.trust == index::Trust::Sandboxed {
        return Err(format!(
            "`{}` is sandboxed; record its `autumn plugin inspect --format json` report with \
             `--inspect` instead",
            listing.name
        ));
    }
    check_report_shape(report)?;
    // The contract is the machine-checked source of the range and the tier.
    // Only a pass replaces them: a failed contract may not even parse, and
    // the listing keeps what was last verified.
    if let Some(contract) = report.contract.as_ref().filter(|_| report.passed()) {
        // The pin is the version that was built and checked. A contract that
        // reports another one is refused, never used to move the pin.
        if let Some(version) = &contract.plugin_version
            && listing.origin == ListingOrigin::Community
            && version != &listing.version
        {
            return Err(format!(
                "the report for `{}` says version {}, but the index verified {}. Change \
                 `version` in the listing and re-run the check",
                listing.name,
                index::sanitize(version),
                listing.version
            ));
        }
        if let Some(range) = &contract.autumn_web {
            listing.autumn_web.clone_from(range);
        }
        // A first-party crate is lockstep: it is built from this workspace,
        // so its contract version is the release.
        if let Some(version) = &contract.plugin_version
            && listing.origin == ListingOrigin::FirstParty
        {
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

    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .map(|c| c.name.as_str())
        .collect();
    let failed = (!report.passed()).then(|| failed.join(", "));
    Ok(transition(listing, failed.as_deref(), against, date))
}

/// A report is recorded only for the release it tested: a pass from an older
/// release proves nothing about this one, even when its range admits both.
/// A failing report that names no release (an install that failed before
/// plugin-check ran) is still recorded as a failure.
///
/// # Errors
///
/// When the report tested another release, or passes without naming one.
pub fn check_tested_release(report: &ConformanceReport, against: &str) -> Result<(), String> {
    let name = index::sanitize(&report.plugin_name);
    match report.autumn_web.as_deref() {
        Some(tested) if tested == against => Ok(()),
        Some(tested) => Err(format!(
            "the report for `{name}` tested autumn-web {}, not {against}. Re-run \
             `autumn plugin-check` against {against}",
            index::sanitize(tested)
        )),
        None if !report.passed() => Ok(()),
        None => Err(format!(
            "the report for `{name}` does not say which autumn-web it tested. Re-run it with \
             this CLI's `autumn plugin-check --format json`"
        )),
    }
}

/// Record one run: `failed` is `None` for a pass, or the failed checks.
fn transition(
    listing: &mut Listing,
    failed: Option<&str>,
    against: &str,
    date: &str,
) -> Transition {
    let previous = std::mem::replace(&mut listing.conformance.autumn_web, against.to_owned());
    date.clone_into(&mut listing.conformance.checked);
    listing.conformance.reason.clear();

    let Some(failed) = failed else {
        listing.conformance.result = CheckOutcome::Pass;
        listing.status = Status::Listed;
        listing.note.clear();
        return Transition::Listed;
    };

    listing.conformance.result = CheckOutcome::Fail;
    let second_release = listing.status != Status::Listed && previous != against;
    if second_release {
        listing.status = Status::Delisted;
        listing.note =
            format!("failed re-verification on autumn-web {previous} and {against} ({failed})");
        Transition::Delisted
    } else if listing.status == Status::Delisted {
        // A retry on the same release does not bring a delisted plugin back.
        Transition::Delisted
    } else {
        listing.status = Status::Incompatible;
        listing.note = format!("failed re-verification on autumn-web {against} ({failed})");
        Transition::Flagged
    }
}

/// The checks every `autumn plugin-check` report carries. A report without
/// them was not made by `plugin-check`.
const REQUIRED_CHECKS: [&str; 4] = [
    "installability",
    "route-attribution",
    "plugin-contract",
    "experimental-surface",
];

/// Refuse a report that `plugin-check` did not make, or a pass with no
/// declared range (AC 3: a listing needs the #1601 contract).
fn check_report_shape(report: &ConformanceReport) -> Result<(), String> {
    let name = index::sanitize(&report.plugin_name);
    // A failing report is always taken: it can only flag, never vouch.
    if !report.passed() {
        return Ok(());
    }
    for required in REQUIRED_CHECKS {
        if !report.checks.iter().any(|c| c.name == required) {
            return Err(format!(
                "the report for `{name}` has no `{required}` check; use a report from \
                 `autumn plugin-check --format json`"
            ));
        }
    }
    let contract_passed = report
        .checks
        .iter()
        .any(|c| c.name == "plugin-contract" && c.status == CheckStatus::Pass);
    let range = report.contract.as_ref().and_then(|c| c.autumn_web.as_ref());
    if !contract_passed || range.is_none() {
        return Err(format!(
            "the report for `{name}` has no passing contract with an autumn-web range; \
             implement `Plugin::contract`"
        ));
    }
    Ok(())
}

/// The fields `record --inspect` reads from `autumn plugin inspect --format
/// json`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct InspectReport {
    /// The manifest name.
    pub name: String,
    /// The manifest version.
    pub version: String,
    /// The digest of the whole artifact.
    pub artifact_sha256: Option<String>,
    /// The capabilities the manifest asks for.
    pub capabilities: Vec<String>,
    /// Whether the module loads into the sandbox.
    pub loads: bool,
    /// The route-conformance report.
    pub conformance: ConformanceReport,
    /// What the artifact asks for beyond the one named by `--against`.
    #[serde(default)]
    pub upgrade: Option<serde_json::Value>,
    /// The artifact digest of the `--against` baseline.
    #[serde(default)]
    pub upgrade_against: Option<String>,
    /// What each granted capability is scoped to. Required, as every other
    /// authority field: a missing one would erase the recorded scopes.
    pub grants: InspectGrants,
    /// The per-request quotas the manifest declares.
    #[serde(default)]
    pub quotas: std::collections::BTreeMap<String, u32>,
    /// The per-request resource limits the manifest declares.
    #[serde(default)]
    pub limits: std::collections::BTreeMap<String, u64>,
    /// The `autumn-web` whose sandbox the artifact was loaded in.
    #[serde(default)]
    pub autumn_web: Option<String>,
}

/// The `grants` object `inspect` emits. Every list is required: `inspect`
/// always prints all four, empty or not.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct InspectGrants {
    /// Hostnames `http-outbound` may call.
    pub hosts: Vec<String>,
    /// Logical tables `db` owns.
    pub tables: Vec<String>,
    /// Job types `jobs` may enqueue.
    pub job_types: Vec<String>,
    /// Render slots `render` may fill.
    pub slots: Vec<String>,
}

impl From<&InspectGrants> for index::Grants {
    fn from(g: &InspectGrants) -> Self {
        Self {
            hosts: g.hosts.clone(),
            tables: g.tables.clone(),
            job_types: g.job_types.clone(),
            slots: g.slots.clone(),
        }
    }
}

impl InspectReport {
    /// Whether `--against` found new authority: any non-empty list in the
    /// delta. Read generically, so a field added later still counts.
    #[must_use]
    pub fn needs_consent(&self) -> bool {
        self.upgrade.as_ref().is_some_and(|delta| {
            delta.as_object().is_some_and(|fields| {
                fields
                    .values()
                    .any(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            })
        })
    }
}

/// Refuse an inspect report this CLI's `inspect` did not make: the route
/// checks, every quota and limit, and a complete upgrade delta.
fn check_inspect_shape(listing: &Listing, report: &InspectReport) -> Result<(), String> {
    // `inspect` runs the route checks from `plugin-check` over the manifest.
    // A report without them was not made by `inspect`.
    if index::canonical(&report.conformance.plugin_name) != index::canonical(&listing.name) {
        return Err(format!(
            "the inspect report's conformance is for `{}`, not `{}`",
            index::sanitize(&report.conformance.plugin_name),
            listing.name
        ));
    }
    for required in ["route-attribution", "route-prefix", "route-collision"] {
        if !report.conformance.checks.iter().any(|c| c.name == required) {
            return Err(format!(
                "the inspect report for `{}` has no `{required}` check; use a report from \
                 `autumn plugin inspect --format json`",
                listing.name
            ));
        }
    }
    // `inspect` emits every quota and limit. A report missing one would
    // erase the recorded ceilings, so it was not made by this `inspect`.
    let quotas = autumn_web::plugin_sandbox::CapabilityQuotas::default().fields();
    let limits = autumn_web::plugin_sandbox::ResourceLimits::default().fields();
    let missing: Vec<String> = quotas
        .iter()
        .filter(|(key, _)| !report.quotas.contains_key(*key))
        .map(|(key, _)| format!("quotas.{key}"))
        .chain(
            limits
                .iter()
                .filter(|(key, _)| !report.limits.contains_key(*key))
                .map(|(key, _)| format!("limits.{key}")),
        )
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the inspect report for `{}` has no {}; use a report from this CLI's \
             `autumn plugin inspect --format json`",
            listing.name,
            missing.join(", ")
        ));
    }
    // A delta must carry every `ConsentDelta` field as a list: `{}` or a
    // partial one would read as "nothing new".
    if let Some(upgrade) = &report.upgrade {
        let expected = serde_json::to_value(autumn_web::plugin_sandbox::ConsentDelta::default())
            .unwrap_or_default();
        let missing: Vec<&str> = expected
            .as_object()
            .into_iter()
            .flat_map(|fields| fields.keys())
            .filter(|key| upgrade.get(key.as_str()).is_none_or(|v| !v.is_array()))
            .map(String::as_str)
            .collect();
        if !upgrade.is_object() || !missing.is_empty() {
            return Err(format!(
                "the inspect report's `upgrade` for `{}` is not a complete delta (missing {}); \
                 use `autumn plugin inspect --against`",
                listing.name,
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

/// Write an `autumn plugin inspect --format json` result into a sandboxed
/// listing. The capabilities and digest come from the artifact, not a hand
/// edit.
///
/// # Errors
///
/// When the listing is not sandboxed, or the report names another plugin.
pub fn apply_inspect(
    listing: &mut Listing,
    report: &InspectReport,
    against: &str,
    date: &str,
) -> Result<Transition, String> {
    if listing.trust != index::Trust::Sandboxed {
        return Err(format!(
            "`{}` is not sandboxed; record its `autumn plugin-check` report instead",
            listing.name
        ));
    }
    if index::canonical(&report.name) != index::canonical(&listing.name) {
        return Err(format!(
            "the inspect report is for `{}`, not `{}`",
            index::sanitize(&report.name),
            listing.name
        ));
    }
    check_inspect_shape(listing, report)?;
    // Recorded only for the release whose sandbox loaded it: automated
    // re-verification skips sandboxed listings, so nothing else would catch
    // a stale report.
    match report.autumn_web.as_deref() {
        Some(tested) if tested == against => {}
        tested => {
            return Err(format!(
                "the inspect report for `{}` {}. Re-run `autumn plugin inspect --format json` \
                 with the autumn {against} CLI",
                listing.name,
                tested.map_or_else(
                    || "does not say which autumn-web loaded it".to_owned(),
                    |v| format!(
                        "was made on autumn-web {}, not {against}",
                        index::sanitize(v)
                    )
                )
            ));
        }
    }
    // Replacing a recorded artifact needs a consent check against it. Without
    // `--against` the report has no delta, and new authority would pass.
    let replacing = !listing.artifact_sha256.is_empty()
        && report.artifact_sha256.as_deref() != Some(listing.artifact_sha256.as_str());
    // The delta must be against the recorded artifact, not any baseline:
    // `--against new.autumn-plugin` would compare the artifact with itself.
    if replacing
        && (report.upgrade.is_none()
            || report.upgrade_against.as_deref() != Some(listing.artifact_sha256.as_str()))
    {
        return Err(format!(
            "`{}` records artifact sha256 {}, and this report is for other bytes. Run \
             `autumn plugin inspect <new>.autumn-plugin --against <recorded>.autumn-plugin \
             --format json` and record that",
            listing.name, listing.artifact_sha256
        ));
    }
    // The delta is the report's own claim. Compare the authority it reports
    // with what the listing recorded, and refuse a delta that hides growth.
    let widened = widened_authority(listing, report);
    if !widened.is_empty() && !report.needs_consent() {
        return Err(format!(
            "the inspect report for `{}` grants more than the listing records ({}), but its \
             `upgrade` delta says nothing is new. Re-run `autumn plugin inspect --against`",
            listing.name,
            widened.join(", ")
        ));
    }
    let mut failed = Vec::new();
    if !report.loads {
        failed.push("load".to_owned());
    }
    // `inspect --against` exits 1 when the artifact grows its authority. A
    // listing must not vouch for a grant nobody consented to.
    if report.needs_consent() || !widened.is_empty() {
        failed.push("upgrade-consent".to_owned());
    }
    failed.extend(
        report
            .conformance
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .map(|c| c.name.clone()),
    );
    if report.artifact_sha256.is_none() {
        failed.push("artifact-digest".to_owned());
    }
    // Only a pass replaces the reviewed artifact. On a fail the listing keeps
    // the version, capabilities and digest that were consented to.
    if failed.is_empty()
        && let Some(digest) = &report.artifact_sha256
    {
        digest.clone_into(&mut listing.artifact_sha256);
        report.version.clone_into(&mut listing.version);
        listing.capabilities.clone_from(&report.capabilities);
        listing.grants = index::Grants::from(&report.grants);
        listing.quotas.clone_from(&report.quotas);
        listing.limits.clone_from(&report.limits);
    }
    let failed = (!failed.is_empty()).then(|| failed.join(", "));
    Ok(transition(listing, failed.as_deref(), against, date))
}

/// Authority `report` has beyond what `listing` recorded: a capability, a
/// scoped grant, or a quota or limit that is new or higher. Empty for a
/// listing with no recorded artifact, whose first review is the PR itself.
fn widened_authority(listing: &Listing, report: &InspectReport) -> Vec<String> {
    fn added(label: &str, before: &[String], after: &[String], out: &mut Vec<String>) {
        for item in after.iter().filter(|item| !before.contains(item)) {
            out.push(format!("{label} {}", index::sanitize(item)));
        }
    }
    fn raised<T: PartialOrd + Copy>(
        label: &str,
        before: &std::collections::BTreeMap<String, T>,
        after: &std::collections::BTreeMap<String, T>,
        out: &mut Vec<String>,
    ) {
        for (key, value) in after {
            if before.get(key).is_none_or(|old| value > old) {
                out.push(format!("{label} {}", index::sanitize(key)));
            }
        }
    }

    let mut out = Vec::new();
    if listing.artifact_sha256.is_empty() {
        return out;
    }
    let granted = &listing.grants;
    added(
        "capability",
        &listing.capabilities,
        &report.capabilities,
        &mut out,
    );
    added("host", &granted.hosts, &report.grants.hosts, &mut out);
    added("table", &granted.tables, &report.grants.tables, &mut out);
    added(
        "job type",
        &granted.job_types,
        &report.grants.job_types,
        &mut out,
    );
    added("slot", &granted.slots, &report.grants.slots, &mut out);
    raised("quota", &listing.quotas, &report.quotas, &mut out);
    raised("limit", &listing.limits, &report.limits, &mut out);
    out
}

/// Refresh an exempt listing after the install gate passed on `against`.
///
/// # Errors
///
/// When the listing is not exempt: a `Plugin` must bring a report.
pub fn apply_exempt(listing: &mut Listing, against: &str, date: &str) -> Result<(), String> {
    require_exempt_class(listing)?;
    listing.conformance.result = CheckOutcome::Exempt;
    listing.status = Status::Listed;
    listing.note.clear();
    against.clone_into(&mut listing.conformance.autumn_web);
    date.clone_into(&mut listing.conformance.checked);
    // First-party crates are lockstep: the release is their version, and its
    // series is their range.
    if listing.origin == ListingOrigin::FirstParty {
        against.clone_into(&mut listing.version);
        listing.autumn_web = autumn_web::plugin_contract::lockstep_range(against);
    }
    Ok(())
}

/// Record a failed install gate for an exempt listing: flag it, or delist it
/// on a second release. The exemption stays, so a later pass recovers it.
///
/// # Errors
///
/// When the listing is not exempt.
pub fn apply_exempt_failed(
    listing: &mut Listing,
    against: &str,
    date: &str,
) -> Result<Transition, String> {
    require_exempt_class(listing)?;
    let reason = std::mem::take(&mut listing.conformance.reason);
    let t = transition(listing, Some("installability"), against, date);
    listing.conformance.reason = reason;
    Ok(t)
}

/// An exempt listing is verified by its install gate, not by plugin-check:
/// its result is `exempt`, or a failure that kept its exemption `reason`.
fn require_exempt_class(listing: &Listing) -> Result<(), String> {
    let exempt = listing.conformance.result == CheckOutcome::Exempt
        || (listing.conformance.result == CheckOutcome::Fail
            && !listing.conformance.reason.trim().is_empty());
    if exempt {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not exempt; record its `autumn plugin-check` report instead",
            listing.name
        ))
    }
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
    if listing.capabilities.is_empty() {
        table.remove("capabilities");
    } else {
        table["capabilities"] = value(listing.capabilities.iter().collect::<Array>());
    }
    set_or_remove(table, "artifact_sha256", &listing.artifact_sha256);
    if listing.grants.is_empty() {
        table.remove("grants");
    } else {
        let mut grants = toml_edit::Table::new();
        for (key, list) in [
            ("hosts", &listing.grants.hosts),
            ("tables", &listing.grants.tables),
            ("job_types", &listing.grants.job_types),
            ("slots", &listing.grants.slots),
        ] {
            if !list.is_empty() {
                grants[key] = value(list.iter().collect::<Array>());
            }
        }
        table["grants"] = Item::Table(grants);
    }
    if listing.quotas.is_empty() {
        table.remove("quotas");
    } else {
        let mut quotas = toml_edit::Table::new();
        for (key, v) in &listing.quotas {
            quotas[key.as_str()] = value(i64::from(*v));
        }
        table["quotas"] = Item::Table(quotas);
    }
    if listing.limits.is_empty() {
        table.remove("limits");
    } else {
        let mut limits = toml_edit::Table::new();
        for (key, v) in &listing.limits {
            // TOML integers are i64; a limit past that is written as the max.
            limits[key.as_str()] = value(i64::try_from(*v).unwrap_or(i64::MAX));
        }
        table["limits"] = Item::Table(limits);
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
        let _ = writeln!(
            out,
            "  {}: {}",
            index::sanitize(&finding.plugin),
            index::sanitize(&finding.message)
        );
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

/// The JSON document `check --format json` prints.
#[must_use]
pub fn check_json(loaded: &index::Loaded, against: &str) -> serde_json::Value {
    let findings = index::check(&loaded.index, against);
    serde_json::json!({
        "index": source_label(&loaded.source),
        "autumn_web": against,
        "passed": findings.is_empty(),
        "findings": findings,
    })
}

/// Where the index came from, for output.
fn source_label(source: &index::Source) -> String {
    match source {
        index::Source::Bundled => "(bundled)".to_owned(),
        index::Source::Override(path) => path.display().to_string(),
    }
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
    let findings = index::check(&loaded.index, opts.against);
    if opts.json {
        let document = check_json(&loaded, opts.against);
        println!(
            "{}",
            serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let source = source_label(&loaded.source);
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
    /// `autumn plugin inspect --format json` reports, for sandboxed listings.
    pub inspects: &'a [PathBuf],
    /// Exempt listings whose install gate passed.
    pub exempt: &'a [String],
    /// Exempt listings whose install gate failed.
    pub exempt_failed: &'a [String],
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
            eprintln!(
                "autumn plugin index record: {}. The index was not changed.",
                index::sanitize(&err)
            );
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
    let mut lines = Vec::new();

    for path in opts.reports {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let report: ConformanceReport = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a plugin-check JSON report: {e}", path.display()))?;
        check_tested_release(&report, opts.against)?;
        // Parse the text as it stands now, so each report builds on the last.
        let parsed = index::parse(&src).map_err(|e| e.to_string())?;
        let mut listing = parsed.get(&report.plugin_name).cloned().ok_or_else(|| {
            format!(
                "the index has no listing for `{}`",
                index::sanitize(&report.plugin_name)
            )
        })?;
        let transition = apply_report(&mut listing, &report, opts.against, opts.date)?;
        src = write_listing(&src, &listing)?;
        lines.push(format!(
            "{}: {transition:?} on autumn-web {}",
            listing.name, opts.against
        ));
    }
    for path in opts.inspects {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let report: InspectReport = serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not a plugin inspect JSON report: {e}",
                path.display()
            )
        })?;
        let parsed = index::parse(&src).map_err(|e| e.to_string())?;
        let mut listing = parsed.get(&report.name).cloned().ok_or_else(|| {
            format!(
                "the index has no listing for `{}`",
                index::sanitize(&report.name)
            )
        })?;
        let transition = apply_inspect(&mut listing, &report, opts.against, opts.date)?;
        src = write_listing(&src, &listing)?;
        lines.push(format!(
            "{}: {transition:?} on autumn-web {}",
            listing.name, opts.against
        ));
    }
    for name in opts.exempt {
        let parsed = index::parse(&src).map_err(|e| e.to_string())?;
        let mut listing = parsed
            .get(name)
            .cloned()
            .ok_or_else(|| format!("the index has no listing for `{}`", index::sanitize(name)))?;
        apply_exempt(&mut listing, opts.against, opts.date)?;
        src = write_listing(&src, &listing)?;
        lines.push(format!(
            "{name}: exempt, refreshed for autumn-web {}",
            opts.against
        ));
    }
    for name in opts.exempt_failed {
        let parsed = index::parse(&src).map_err(|e| e.to_string())?;
        let mut listing = parsed
            .get(name)
            .cloned()
            .ok_or_else(|| format!("the index has no listing for `{}`", index::sanitize(name)))?;
        let transition = apply_exempt_failed(&mut listing, opts.against, opts.date)?;
        src = write_listing(&src, &listing)?;
        lines.push(format!(
            "{name}: exempt, {transition:?} on autumn-web {}",
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
                check("route-attribution", CheckStatus::Pass),
                check(
                    "route-collision",
                    if pass {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                ),
                check("plugin-contract", CheckStatus::Pass),
                check("experimental-surface", CheckStatus::Pass),
            ],
            contract,
            autumn_web: None,
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
            &report(
                "autumn-admin-plugin",
                true,
                Some(lockstep("autumn-admin-plugin", "0.8.0")),
            ),
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

    /// AC 3: a pass lists only with a declared range, from the contract.
    #[test]
    fn a_passing_report_without_a_contract_is_refused() {
        let mut l = listing("autumn-admin-plugin");
        let r = report("autumn-admin-plugin", true, None);
        let err = apply_report(&mut l, &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("contract"), "{err}");
    }

    /// A hand-made report with no checks is not a plugin-check run.
    #[test]
    fn a_report_without_the_standard_checks_is_refused() {
        let mut l = listing("autumn-admin-plugin");
        let contract = Some(lockstep("autumn-admin-plugin", "0.7.0"));
        let mut r = report("autumn-admin-plugin", true, contract);
        r.checks.retain(|c| c.name != "plugin-contract");
        let err = apply_report(&mut l, &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("plugin-contract"), "{err}");
        r.checks.clear();
        assert!(apply_report(&mut l, &r, "0.7.0", "2026-10-01").is_err());
    }

    /// A delisted listing stays delisted when a retry fails again.
    #[test]
    fn a_delisted_listing_stays_delisted_on_a_retry() {
        let mut l = listing("autumn-admin-plugin");
        let r = report("autumn-admin-plugin", false, None);
        apply_report(&mut l, &r, "0.8.0", "2026-10-01").expect("flag");
        apply_report(&mut l, &r, "0.9.0", "2026-11-01").expect("delist");
        let t = apply_report(&mut l, &r, "0.9.0", "2026-11-02").expect("retry");
        assert_eq!(t, Transition::Delisted);
        assert_eq!(l.status, Status::Delisted);
    }

    /// The pin is what was built. A contract that reports another version
    /// does not move it.
    #[test]
    fn a_community_contract_version_must_match_the_pin() {
        let mut l = listing("autumn-admin-plugin");
        l.origin = ListingOrigin::Community;
        l.name = "autumn-plugin-x".to_owned();
        l.version = "0.3.0".to_owned();
        let before = l.clone();
        let mut contract = lockstep("autumn-plugin-x", "0.7.0");
        contract.plugin_version = Some("0.4.0".to_owned());
        let r = report("autumn-plugin-x", true, Some(contract));
        let err = apply_report(&mut l, &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("0.4.0") && err.contains("0.3.0"), "{err}");
        assert_eq!(l, before);
    }

    /// A sandboxed listing is verified by `inspect`, never by plugin-check.
    #[test]
    fn a_plugin_check_report_for_a_sandboxed_listing_is_refused() {
        let mut l = sandboxed();
        let before = l.clone();
        let contract = Some(lockstep("autumn-plugin-hello", "0.7.0"));
        let r = report("autumn-plugin-hello", true, contract);
        let err = apply_report(&mut l, &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("--inspect"), "{err}");
        assert_eq!(l, before);
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

    // ── apply_inspect ───────────────────────────────────────────────────

    fn sandboxed() -> Listing {
        let mut l = listing("autumn-admin-plugin");
        l.name = "autumn-plugin-hello".to_owned();
        l.origin = ListingOrigin::Community;
        l.trust = index::Trust::Sandboxed;
        // The consented authority: what `inspect(true)` reports.
        let r = inspect(true);
        l.capabilities.clone_from(&r.capabilities);
        l.grants = index::Grants::from(&r.grants);
        l.quotas.clone_from(&r.quotas);
        l.limits.clone_from(&r.limits);
        l.artifact_sha256 = "00".repeat(32);
        l
    }

    /// A delta that says nothing is new, over a report that grants more
    /// than the listing records, is refused. With a delta that admits it,
    /// the listing is flagged and keeps the consented authority.
    #[test]
    fn widened_authority_is_checked_against_the_listing() {
        let widen: [fn(&mut InspectReport); 4] = [
            |r| r.capabilities.push("sql".to_owned()),
            |r| r.grants.hosts.push("evil.example".to_owned()),
            |r| *r.quotas.values_mut().next().unwrap() += 1,
            |r| *r.limits.get_mut("fuel").unwrap() += 1,
        ];
        for grow in widen {
            let mut r = inspect(true);
            grow(&mut r);
            let before = sandboxed();
            let mut l = before.clone();
            let err = apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").unwrap_err();
            assert!(
                err.contains("grants more than the listing records"),
                "{err}"
            );
            assert_eq!(l, before);

            r.upgrade = Some(delta(&["sql"]));
            let t = apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").expect("apply");
            assert_eq!(t, Transition::Flagged);
            assert_eq!(l.quotas, before.quotas);
            assert_eq!(l.limits, before.limits);
            assert_eq!(l.grants, before.grants);
            assert_eq!(l.capabilities, before.capabilities);
        }
        // Lower ceilings are less authority, not more.
        let mut r = inspect(true);
        *r.limits.get_mut("fuel").unwrap() -= 1;
        let t = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(t, Transition::Listed);
    }

    /// A complete `ConsentDelta`, with `added` capabilities.
    fn delta(added: &[&str]) -> serde_json::Value {
        let mut value =
            serde_json::to_value(autumn_web::plugin_sandbox::ConsentDelta::default()).unwrap();
        value["added_capabilities"] = serde_json::json!(added);
        value
    }

    /// An inspect report is recorded only for the release whose sandbox
    /// loaded it.
    #[test]
    fn an_inspect_report_must_name_the_release_it_loaded_in() {
        let mut r = inspect(true);
        r.autumn_web = Some("0.6.9".to_owned());
        let err = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("autumn-web 0.6.9, not 0.7.0"), "{err}");
        r.autumn_web = None;
        let err = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("does not say"), "{err}");
    }

    /// An empty or partial delta is not "nothing new": it is not a delta
    /// `inspect --against` wrote.
    #[test]
    fn a_partial_upgrade_delta_is_refused() {
        for partial in [
            serde_json::json!({}),
            serde_json::json!({"added_capabilities": []}),
        ] {
            let mut r = inspect(true);
            r.upgrade = Some(partial);
            let err = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").unwrap_err();
            assert!(err.contains("upgrade"), "{err}");
        }
        let mut r = inspect(true);
        r.upgrade = Some(serde_json::json!({"added_hosts": "api.example.com"}));
        assert!(apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").is_err());
    }

    fn inspect(pass: bool) -> InspectReport {
        InspectReport {
            name: "autumn-plugin-hello".to_owned(),
            version: "0.2.0".to_owned(),
            artifact_sha256: Some("ab".repeat(32)),
            capabilities: vec!["http-request".to_owned(), "kv".to_owned()],
            loads: pass,
            conformance: ConformanceReport {
                plugin_name: "autumn-plugin-hello".to_owned(),
                checks: vec![
                    check("route-attribution", CheckStatus::Pass),
                    check("route-prefix", CheckStatus::Pass),
                    check("route-collision", CheckStatus::Pass),
                ],
                contract: None,
                autumn_web: None,
            },
            // A baseline with nothing new: the new artifact asks for no more.
            upgrade: Some(delta(&[])),
            upgrade_against: Some("00".repeat(32)),
            grants: InspectGrants::default(),
            autumn_web: Some("0.7.0".to_owned()),
            quotas: autumn_web::plugin_sandbox::CapabilityQuotas::default()
                .fields()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            limits: autumn_web::plugin_sandbox::ResourceLimits::default()
                .fields()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), u64::try_from(v).unwrap()))
                .collect(),
        }
    }

    /// `grants` and each of its lists are required, not defaulted.
    #[test]
    fn an_inspect_report_without_grants_does_not_parse() {
        let full = serde_json::json!({
            "name": "autumn-plugin-hello", "version": "0.2.0",
            "artifact_sha256": "ab".repeat(32), "capabilities": [], "loads": true,
            "conformance": {"plugin_name": "autumn-plugin-hello", "checks": []},
            "quotas": {}, "limits": {},
            "grants": {"hosts": [], "tables": [], "job_types": [], "slots": []},
        });
        assert!(serde_json::from_value::<InspectReport>(full.clone()).is_ok());
        let mut no_grants = full.clone();
        no_grants.as_object_mut().unwrap().remove("grants");
        assert!(serde_json::from_value::<InspectReport>(no_grants).is_err());
        let mut no_hosts = full;
        no_hosts["grants"].as_object_mut().unwrap().remove("hosts");
        assert!(serde_json::from_value::<InspectReport>(no_hosts).is_err());
    }

    /// A report missing an authority map (older, truncated or hand-edited)
    /// would erase the recorded ceilings. `inspect` always emits every key.
    #[test]
    fn an_inspect_report_missing_quotas_or_limits_is_refused() {
        let mut r = inspect(true);
        r.quotas.clear();
        let err = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("quotas"), "{err}");
        let mut r = inspect(true);
        r.limits.remove("fuel");
        let err = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("fuel"), "{err}");
    }

    /// A failed run keeps the last verified range, version and tier: the
    /// failed contract may not even parse, and the flag must still be written.
    #[test]
    fn a_failed_report_keeps_the_last_verified_contract_fields() {
        let mut l = listing("autumn-admin-plugin");
        let before = l.clone();
        let mut contract = lockstep("autumn-admin-plugin", "9.9.9");
        contract.autumn_web = Some("not a range".to_owned());
        let r = report("autumn-admin-plugin", false, Some(contract));
        apply_report(&mut l, &r, "0.7.0", "2026-10-01").expect("flag");
        assert_eq!(l.status, Status::Incompatible);
        assert_eq!(l.autumn_web, before.autumn_web);
        assert_eq!(l.version, before.version);
        assert_eq!(l.tier, before.tier);
        let one = index::PluginIndex {
            schema: index::SCHEMA,
            plugins: vec![l],
        };
        assert!(index::validate(&one).is_empty());
    }

    /// A new artifact must be compared with the recorded one: without
    /// `--against` there is no consent check, so `record` refuses it.
    #[test]
    fn a_new_artifact_without_a_baseline_is_refused() {
        let mut l = sandboxed();
        let before = l.clone();
        let mut no_baseline = inspect(true);
        no_baseline.upgrade = None;
        let err = apply_inspect(&mut l, &no_baseline, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("--against"), "{err}");
        assert_eq!(l, before, "a refusal changes nothing");

        // A delta against any other artifact (here: itself) is refused too.
        let mut self_baseline = inspect(true);
        self_baseline.upgrade_against = self_baseline.artifact_sha256.clone();
        let err = apply_inspect(&mut l, &self_baseline, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("--against"), "{err}");
        assert_eq!(l, before);

        // The same digest needs no baseline: nothing changed.
        let mut same = inspect(true);
        same.upgrade = None;
        same.artifact_sha256 = Some(l.artifact_sha256.clone());
        assert!(apply_inspect(&mut l, &same, "0.7.0", "2026-10-01").is_ok());
    }

    /// AC 6: the scoped grants are recorded, not only capability names.
    #[test]
    fn an_inspect_pass_records_the_scoped_grants() {
        // A first recording: no consented artifact to compare with.
        let mut l = sandboxed();
        l.artifact_sha256.clear();
        let mut r = inspect(true);
        r.grants.hosts = vec!["api.example.com".to_owned()];
        r.grants.tables = vec!["notes".to_owned()];
        apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(l.grants.hosts, ["api.example.com"]);
        assert_eq!(l.grants.tables, ["notes"]);
    }

    /// An inspect report with no route checks, or checks for another plugin,
    /// is not an `inspect` run.
    #[test]
    fn an_inspect_report_without_its_route_checks_is_refused() {
        let mut r = inspect(true);
        r.conformance.checks.clear();
        let err = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("route-attribution"), "{err}");

        let mut r = inspect(true);
        r.conformance.plugin_name = "autumn-plugin-other".to_owned();
        assert!(apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").is_err());
    }

    /// A raised resource limit is authority too.
    #[test]
    fn an_inspect_pass_records_the_limits() {
        let mut l = sandboxed();
        let mut r = inspect(true);
        r.limits.insert("fuel".to_owned(), 1_000);
        apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(l.limits.get("fuel"), Some(&1_000));
    }

    /// A raised quota is authority: the approved ceilings are recorded.
    #[test]
    fn an_inspect_pass_records_the_quotas() {
        // A first recording: no consented artifact to compare with.
        let mut l = sandboxed();
        l.artifact_sha256.clear();
        let mut r = inspect(true);
        r.quotas.insert("kv_reads".to_owned(), 500);
        apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(l.quotas.get("kv_reads"), Some(&500));
    }

    /// A failed inspect keeps the artifact that was consented to.
    #[test]
    fn a_failed_inspect_keeps_the_consented_artifact() {
        let mut l = sandboxed();
        let before = l.clone();
        let mut r = inspect(true);
        r.upgrade = Some(delta(&["kv"]));
        apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").expect("flag");
        assert_eq!(l.status, Status::Incompatible);
        assert_eq!(l.capabilities, before.capabilities);
        assert_eq!(l.artifact_sha256, before.artifact_sha256);
        assert_eq!(l.version, before.version);
    }

    /// `inspect --against` refuses an artifact that grows its authority.
    /// `record` must not turn that refusal into a listing.
    #[test]
    fn an_upgrade_that_needs_consent_is_flagged() {
        let mut l = sandboxed();
        let mut r = inspect(true);
        r.upgrade = Some(delta(&["kv"]));
        let t = apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(t, Transition::Flagged);
        assert!(l.note.contains("upgrade-consent"), "{}", l.note);

        let mut r = inspect(true);
        r.upgrade = Some(delta(&[]));
        let t = apply_inspect(&mut sandboxed(), &r, "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(t, Transition::Listed);
    }

    /// AC 1: a sandboxed listing's manifest comes from the artifact.
    #[test]
    fn an_inspect_pass_records_the_manifest_and_digest() {
        let mut l = sandboxed();
        let t = apply_inspect(&mut l, &inspect(true), "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(t, Transition::Listed);
        assert_eq!(l.capabilities, ["http-request", "kv"]);
        assert_eq!(l.artifact_sha256, "ab".repeat(32));
        assert_eq!(l.version, "0.2.0");
        assert_eq!(l.conformance.result, CheckOutcome::Pass);
    }

    #[test]
    fn an_artifact_that_does_not_load_is_flagged() {
        let mut l = sandboxed();
        let t = apply_inspect(&mut l, &inspect(false), "0.7.0", "2026-10-01").expect("apply");
        assert_eq!(t, Transition::Flagged);
        assert!(l.note.contains("load"), "{}", l.note);
    }

    #[test]
    fn inspect_is_refused_for_a_native_listing() {
        let mut l = listing("autumn-admin-plugin");
        let mut r = inspect(true);
        r.name = "autumn-admin-plugin".to_owned();
        let err = apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").unwrap_err();
        assert!(err.contains("sandboxed"), "{err}");
    }

    #[test]
    fn write_listing_writes_capabilities_and_digest() {
        let mut src = index::BUNDLED.to_owned();
        src.push_str(
            "\n[[plugin]]\nname = \"autumn-plugin-hello\"\ndescription = \"x\"\n\
             origin = \"community\"\nrepository = \"https://example.com/x\"\n\
             version = \"0.1.0\"\nautumn_web = \"0.7\"\ntier = \"stable\"\n\
             trust = \"sandboxed\"\ncapabilities = [\"http-request\"]\n\
             artifact_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n\
             status = \"listed\"\n\n[plugin.conformance]\nresult = \"pass\"\n\
             autumn_web = \"0.7.0\"\nchecked = \"2026-09-27\"\n",
        );
        let mut l = index::parse(&src)
            .unwrap()
            .get("autumn-plugin-hello")
            .unwrap()
            .clone();
        // A first recording: no consented artifact to compare with.
        l.artifact_sha256.clear();
        let mut r = inspect(true);
        r.grants.hosts = vec!["api.example.com".to_owned()];
        r.quotas.insert("kv_reads".to_owned(), 500);
        r.limits.insert("fuel".to_owned(), 1_000);
        apply_inspect(&mut l, &r, "0.7.0", "2026-10-01").expect("apply");
        let out = write_listing(&src, &l).expect("write");
        let parsed = index::parse(&out).expect("parse");
        assert_eq!(parsed.get("autumn-plugin-hello"), Some(&l));
    }

    // ── apply_exempt ────────────────────────────────────────────────────

    #[test]
    fn an_exempt_listing_is_refreshed_for_the_release() {
        let mut l = listing("autumn-storage-s3");
        apply_exempt(&mut l, "0.8.0", "2026-10-01").expect("exempt");
        assert_eq!(l.conformance.autumn_web, "0.8.0");
        // Lockstep: the range moves to the new series, or the gate fails.
        assert_eq!(l.autumn_web, "0.8");
        let one = index::PluginIndex {
            schema: index::SCHEMA,
            plugins: vec![l.clone()],
        };
        assert!(index::check(&one, "0.8.0").is_empty());
        assert_eq!(l.conformance.result, CheckOutcome::Exempt);
        assert_eq!(l.version, "0.8.0");
    }

    /// A failed exempt check is recorded: flag, then delist on a later
    /// release. The exemption (its `reason`) stays, so a later pass recovers.
    #[test]
    fn a_failed_exempt_check_flags_then_recovers() {
        let mut l = listing("autumn-storage-s3");
        let reason = l.conformance.reason.clone();
        assert!(!reason.is_empty());
        let t = apply_exempt_failed(&mut l, "0.8.0", "2026-10-01").expect("flag");
        assert_eq!(t, Transition::Flagged);
        assert_eq!(l.status, Status::Incompatible);
        assert_eq!(l.conformance.result, CheckOutcome::Fail);
        assert_eq!(l.conformance.reason, reason);
        let one = |l: &Listing| index::PluginIndex {
            schema: index::SCHEMA,
            plugins: vec![l.clone()],
        };
        assert!(
            index::validate(&one(&l)).is_empty(),
            "{:?}",
            index::validate(&one(&l))
        );
        // Recovery: the next passing compile relists it as exempt.
        apply_exempt(&mut l, "0.8.0", "2026-10-02").expect("recover");
        assert_eq!(l.status, Status::Listed);
        assert_eq!(l.conformance.result, CheckOutcome::Exempt);
        assert!(l.note.is_empty());
        // Two failures on different releases delist it.
        apply_exempt_failed(&mut l, "0.8.0", "2026-10-03").expect("flag");
        let t = apply_exempt_failed(&mut l, "0.9.0", "2026-11-01").expect("delist");
        assert_eq!(t, Transition::Delisted);
    }

    #[test]
    fn a_plugin_listing_cannot_be_recorded_as_exempt_failed() {
        let mut l = listing("autumn-admin-plugin");
        assert!(apply_exempt_failed(&mut l, "0.8.0", "2026-10-01").is_err());
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
            &report(
                "autumn-admin-plugin",
                true,
                Some(lockstep("autumn-admin-plugin", "0.7.0")),
            ),
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
        let hostile = vec![index::Finding {
            plugin: "autumn-plugin-x\u{1b}]0;PWN\u{7}".to_owned(),
            message: "`kv\u{1b}[2J` is not a sandbox capability".to_owned(),
        }];
        let out = render_findings(&hostile, "index.toml", "0.8.0");
        assert!(!out.contains(['\u{1b}', '\u{7}']), "{out:?}");
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
            inspects: &[],
            reports: &[report_path],
            exempt: &["autumn-storage-s3".to_owned()],
            exempt_failed: &[],
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

    fn write_report(dir: &Path, file: &str, r: &ConformanceReport) -> PathBuf {
        let path = dir.join(file);
        std::fs::write(&path, serde_json::to_string(r).expect("json")).expect("write");
        path
    }

    /// Reports apply in order, each to the result of the one before.
    #[test]
    fn run_record_applies_two_reports_for_one_plugin_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("index.toml");
        std::fs::write(&index_path, index::BUNDLED).expect("write index");
        let release = env!("CARGO_PKG_VERSION");
        let fail = write_report(
            dir.path(),
            "fail.json",
            &report("autumn-admin-plugin", false, None),
        );
        let mut ok = report(
            "autumn-admin-plugin",
            true,
            Some(lockstep("autumn-admin-plugin", release)),
        );
        ok.autumn_web = Some(release.to_owned());
        let pass = write_report(dir.path(), "pass.json", &ok);
        let code = run_record(&RecordOptions {
            index: &index_path,
            inspects: &[],
            reports: &[fail, pass],
            exempt: &[],
            exempt_failed: &[],
            against: release,
            date: "2026-10-01",
        });
        assert_eq!(code, 0);
        let written = index::parse(&std::fs::read_to_string(&index_path).unwrap()).unwrap();
        let admin = written.get("autumn-admin-plugin").unwrap();
        assert_eq!(admin.status, Status::Listed);
        assert!(admin.note.is_empty(), "{}", admin.note);
    }

    /// A report is recorded for the release it tested, and a pass must say
    /// which that was.
    #[test]
    fn a_report_must_name_the_release_it_tested() {
        let mut pass = report("autumn-admin-plugin", true, None);
        assert!(
            check_tested_release(&pass, "0.7.1")
                .unwrap_err()
                .contains("does not say")
        );
        pass.autumn_web = Some("0.7.0".to_owned());
        let err = check_tested_release(&pass, "0.7.1").unwrap_err();
        assert!(err.contains("tested autumn-web 0.7.0, not 0.7.1"), "{err}");
        assert!(check_tested_release(&pass, "0.7.0").is_ok());
        // A failed install names no release, and is still a failure.
        let fail = report("autumn-admin-plugin", false, None);
        assert!(check_tested_release(&fail, "0.7.1").is_ok());
    }

    /// `run_record` refuses a stale pass and writes nothing.
    #[test]
    fn run_record_refuses_a_report_from_another_release() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("index.toml");
        std::fs::write(&index_path, index::BUNDLED).expect("write index");
        let mut r = report(
            "autumn-admin-plugin",
            true,
            Some(lockstep("autumn-admin-plugin", "0.7.0")),
        );
        r.autumn_web = Some("0.6.9".to_owned());
        let stale = write_report(dir.path(), "stale.json", &r);
        let code = run_record(&RecordOptions {
            index: &index_path,
            inspects: &[],
            reports: &[stale],
            exempt: &[],
            exempt_failed: &[],
            against: "0.7.0",
            date: "2026-10-01",
        });
        assert_eq!(code, 1);
        assert_eq!(
            std::fs::read_to_string(&index_path).unwrap(),
            index::BUNDLED
        );
    }

    /// A result that breaks an admission rule writes nothing.
    #[test]
    fn run_record_refuses_a_result_that_breaks_a_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("index.toml");
        std::fs::write(&index_path, index::BUNDLED).expect("write index");
        let mut contract = lockstep("autumn-admin-plugin", "0.7.0");
        contract.autumn_web = Some(">=0.1".to_owned());
        let mut r = report("autumn-admin-plugin", true, Some(contract));
        r.autumn_web = Some("0.7.0".to_owned());
        let open_range = write_report(dir.path(), "open.json", &r);
        let code = run_record(&RecordOptions {
            index: &index_path,
            inspects: &[],
            reports: &[open_range],
            exempt: &[],
            exempt_failed: &[],
            against: "0.7.0",
            date: "2026-10-01",
        });
        assert_eq!(code, 1);
        assert_eq!(
            std::fs::read_to_string(&index_path).unwrap(),
            index::BUNDLED
        );
    }

    #[test]
    fn check_json_names_every_finding() {
        let loaded = index::load(None).expect("bundled");
        let value = check_json(&loaded, "99.0.0");
        assert_eq!(value["passed"], false);
        assert_eq!(value["autumn_web"], "99.0.0");
        let findings = value["findings"].as_array().expect("findings");
        assert!(!findings.is_empty());
        assert!(
            findings
                .iter()
                .all(|f| f["plugin"].is_string() && f["message"].is_string())
        );
        assert_eq!(
            check_json(&loaded, env!("CARGO_PKG_VERSION"))["passed"],
            true
        );
    }

    #[test]
    fn run_record_refuses_a_date_that_is_not_a_date() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("index.toml");
        std::fs::write(&index_path, index::BUNDLED).expect("write index");
        let code = run_record(&RecordOptions {
            index: &index_path,
            inspects: &[],
            reports: &[],
            exempt: &["autumn-storage-s3".to_owned()],
            exempt_failed: &[],
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
