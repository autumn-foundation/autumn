//! `autumn openapi export` — emit the app's `OpenAPI` document without booting it.
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_DUMP_OPENAPI=1`, and captures the generated `OpenAPI` 3.1 JSON from its
//! stdout — the same no-boot child-process protocol `autumn routes` uses. No
//! HTTP port is bound and no database is touched.
//!
//! The app builds that document through the exact pair the `/openapi.json` route
//! uses, so what this command writes is what the running server serves. That
//! makes the output directly consumable by the standard `OpenAPI` toolchain:
//!
//! ```text
//! autumn openapi export --out openapi.json
//! npx openapi-typescript openapi.json -o src/api.d.ts
//! ```
//!
//! `--check <path>` re-exports and compares against a committed copy, so a
//! contract change that was not reviewed fails CI instead of shipping.

use std::path::Path;
use std::process::Command;

use autumn_web::openapi::{OpaqueSchema, OpenApiSpec, opaque_component_schemas};

use crate::routes::{CargoFeatures, compile_binary_with_profile, find_binary_in_profile};

/// Options controlling `autumn openapi export`.
pub struct ExportOptions<'a> {
    pub package: Option<&'a str>,
    /// Binary target name for packages that expose multiple bin targets.
    pub bin: Option<&'a str>,
    /// Write the document here instead of stdout.
    pub out: Option<&'a Path>,
    /// Compare against this committed document and fail on drift.
    pub check: Option<&'a Path>,
    /// Fail when any component schema degraded to the opaque placeholder.
    pub strict: bool,
    pub features: CargoFeatures,
    /// Build and run the release binary instead of the debug one.
    ///
    /// A route or schema behind `#[cfg(not(debug_assertions))]` exists only in
    /// the release build, so a debug-built export describes a contract the
    /// deployed binary does not serve — and `--check` would pass against it.
    /// Mirrors the profile flag the other manifest commands take.
    pub release: bool,
}

/// Run `autumn openapi export`.
pub fn run(opts: &ExportOptions<'_>) {
    eprintln!("\u{1F342} autumn openapi export\n");

    let spec_json = dump_spec(opts);

    // Parse for the quality report only — the bytes written out are the child's
    // verbatim, so `--check` compares exactly what a previous run committed.
    let report = match serde_json::from_str::<OpenApiSpec>(&spec_json) {
        Ok(spec) => opaque_component_schemas(&spec),
        Err(e) => {
            eprintln!("\u{2717} Failed to parse the emitted OpenAPI document: {e}");
            std::process::exit(1);
        }
    };
    report_opaque_schemas(&report);

    match (opts.check, opts.out) {
        (Some(path), _) => run_check(path, &spec_json, &report, opts.strict),
        (None, Some(path)) => {
            write_spec(path, &spec_json);
            eprintln!("\u{2713} OpenAPI spec written \u{2192} {}", path.display());
            exit_on_strict(&report, opts.strict);
        }
        (None, None) => {
            print!("{spec_json}");
            exit_on_strict(&report, opts.strict);
        }
    }
}

/// Build the app and read its `OpenAPI` dump, exiting with an actionable message
/// when the binary has no spec to give.
fn dump_spec(opts: &ExportOptions<'_>) -> String {
    // Build and locate under the SAME profile: a command that builds then runs
    // must agree with itself about which binary it means.
    compile_binary_with_profile(opts.package, opts.bin, &opts.features, opts.release);
    let binary = find_binary_in_profile(opts.package, opts.bin, opts.release);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_OPENAPI", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    // stderr is captured rather than inherited so the unavailable marker can be
    // scanned for, but everything else the child said is forwarded verbatim —
    // a swallowed config or telemetry warning would make a failing export much
    // harder to diagnose than a slightly noisier one.
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr
        .lines()
        .filter(|l| !l.contains(autumn_web::openapi::OPENAPI_UNAVAILABLE_MARKER))
    {
        eprintln!("{line}");
    }

    // The app reports "I have no spec" on a marker rather than by failing to
    // print JSON, so the two causes get distinct, fixable advice instead of a
    // parse error on empty stdout.
    if let Some(reason) = unavailable_reason(&stderr) {
        eprintln!("\u{2717} No OpenAPI spec to export: {reason}.\n");
        // The marker already distinguishes the two causes, so show only the
        // remedy that applies rather than making the reader work out which half
        // of a combined message is theirs.
        if reason == autumn_web::openapi::OPENAPI_UNAVAILABLE_FEATURE {
            eprintln!("  Enable it in the app's Cargo.toml:");
            eprintln!("      autumn-web = {{ version = \"..\", features = [\"openapi\"] }}");
        } else {
            eprintln!("  Configure it in main():");
            eprintln!("      .openapi(OpenApiConfig::new(\"My API\", \"1.0.0\"))");
        }
        std::process::exit(1);
    }

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping the OpenAPI spec",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The reason text following the unavailable marker, if the child emitted it.
pub fn unavailable_reason(stderr: &str) -> Option<&str> {
    stderr.lines().find_map(|line| {
        line.split_once(autumn_web::openapi::OPENAPI_UNAVAILABLE_MARKER)
            .map(|(_, reason)| reason.trim())
    })
}

/// Print the opaque-component report to stderr (never stdout, which may be
/// carrying the document itself).
fn report_opaque_schemas(report: &[OpaqueSchema]) {
    if report.is_empty() {
        return;
    }
    eprintln!(
        "\u{26A0} {} component schema(s) have no field-level type and export as an \
         opaque object:",
        report.len()
    );
    for entry in report {
        if entry.referenced_by.is_empty() {
            eprintln!("    {}", entry.schema);
        } else {
            eprintln!(
                "    {} \u{2190} {}",
                entry.schema,
                entry.referenced_by.join(", ")
            );
        }
    }
    eprintln!(
        "\n  A client generated from this spec sees `unknown` (TypeScript) or \
         `serde_json::Value` (Rust)\n  for each of them. Add \
         `#[derive(OpenApiSchema)]` to the type, or register a hand-written\n  \
         schema with `OpenApiConfig::register_schema`.\n"
    );
}

/// Compare a freshly exported document against a committed one.
fn run_check(path: &Path, spec_json: &str, report: &[OpaqueSchema], strict: bool) {
    let committed = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            eprintln!("\u{2717} Cannot read {} for --check: {e}", path.display());
            eprintln!(
                "  Write it first: autumn openapi export --out {}",
                path.display()
            );
            std::process::exit(1);
        }
    };

    // Compare parsed values, not bytes: a committed file that differs only in
    // trailing newline or indentation is not a contract change, and failing CI
    // over one would train people to ignore this gate.
    let (Ok(lhs), Ok(rhs)) = (
        serde_json::from_str::<serde_json::Value>(&committed),
        serde_json::from_str::<serde_json::Value>(spec_json),
    ) else {
        eprintln!("\u{2717} {} is not valid JSON", path.display());
        std::process::exit(1);
    };

    if lhs == rhs {
        eprintln!("\u{2713} {} is up to date", path.display());
        exit_on_strict(report, strict);
        return;
    }

    eprintln!(
        "\u{2717} {} is out of date \u{2014} the API contract changed.",
        path.display()
    );
    for line in describe_drift(&lhs, &rhs) {
        eprintln!("    {line}");
    }
    eprintln!(
        "\n  Regenerate and review the diff: autumn openapi export --out {}",
        path.display()
    );
    std::process::exit(1);
}

/// A short, human-readable summary of how two spec documents differ.
///
/// Deliberately operation-level rather than a full JSON diff: "which endpoints
/// appeared, vanished or changed shape" is the question a reviewer is actually
/// asking, and the file diff is one command away for the rest.
pub fn describe_drift(committed: &serde_json::Value, current: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();

    let ops = |doc: &serde_json::Value| -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut map = std::collections::BTreeMap::new();
        if let Some(paths) = doc.get("paths").and_then(serde_json::Value::as_object) {
            for (path, item) in paths {
                if let Some(methods) = item.as_object() {
                    for (method, operation) in methods {
                        map.insert(
                            format!("{} {path}", method.to_uppercase()),
                            operation.clone(),
                        );
                    }
                }
            }
        }
        map
    };

    let before = ops(committed);
    let after = ops(current);

    for key in after.keys().filter(|k| !before.contains_key(*k)) {
        out.push(format!("+ {key}"));
    }
    for key in before.keys().filter(|k| !after.contains_key(*k)) {
        out.push(format!("- {key}"));
    }
    for (key, value) in &before {
        if after.get(key).is_some_and(|current| current != value) {
            out.push(format!("~ {key}"));
        }
    }

    // Operations can be identical while the schemas they `$ref` changed, so say
    // so rather than reporting "out of date" with an empty explanation.
    if out.is_empty() {
        out.push("component schemas or document metadata changed".to_owned());
    }
    out
}

fn write_spec(path: &Path, spec_json: &str) {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("\u{2717} Failed to create {}: {e}", parent.display());
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(path, spec_json) {
        eprintln!("\u{2717} Failed to write {}: {e}", path.display());
        std::process::exit(1);
    }
}

/// Exit non-zero when `--strict` was passed and the spec has opaque components.
fn exit_on_strict(report: &[OpaqueSchema], strict: bool) {
    if strict && !report.is_empty() {
        eprintln!(
            "\u{2717} --strict: {} opaque component schema(s) in the exported spec",
            report.len()
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── unavailable_reason ────────────────────────────────────────────────

    #[test]
    fn reads_the_reason_after_the_marker() {
        let stderr = format!(
            "some unrelated log\n{}this app never called `.openapi(..)`\n",
            autumn_web::openapi::OPENAPI_UNAVAILABLE_MARKER
        );
        assert_eq!(
            unavailable_reason(&stderr),
            Some("this app never called `.openapi(..)`")
        );
    }

    #[test]
    fn no_marker_means_available() {
        assert_eq!(unavailable_reason("just a warning\n"), None);
    }

    // ── describe_drift ────────────────────────────────────────────────────

    fn doc(paths: &serde_json::Value) -> serde_json::Value {
        json!({ "openapi": "3.1.0", "paths": paths })
    }

    #[test]
    fn reports_added_and_removed_operations() {
        let before = doc(&json!({ "/a": { "get": { "operationId": "a" } } }));
        let after = doc(&json!({ "/b": { "post": { "operationId": "b" } } }));
        let drift = describe_drift(&before, &after);
        assert!(drift.contains(&"+ POST /b".to_owned()), "{drift:?}");
        assert!(drift.contains(&"- GET /a".to_owned()), "{drift:?}");
    }

    #[test]
    fn reports_a_changed_operation() {
        let before = doc(&json!({ "/a": { "get": { "operationId": "a" } } }));
        let after = doc(&json!({ "/a": { "get": { "operationId": "a", "deprecated": true } } }));
        assert_eq!(describe_drift(&before, &after), vec!["~ GET /a".to_owned()]);
    }

    #[test]
    fn falls_back_to_a_generic_note_when_only_components_moved() {
        let before = doc(&json!({ "/a": { "get": { "operationId": "a" } } }));
        let after = doc(&json!({ "/a": { "get": { "operationId": "a" } } }));
        // Same operations: the caller only reaches this when the documents
        // differ elsewhere, and an empty explanation would be useless.
        assert_eq!(
            describe_drift(&before, &after),
            vec!["component schemas or document metadata changed".to_owned()]
        );
    }
}
