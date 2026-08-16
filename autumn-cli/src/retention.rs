//! `autumn retention --dry-run` -- report what every declared
//! `#[repository(..., retention(...))]` policy would sweep, without deleting
//! anything (issue #1342).
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_RETENTION_DRY_RUN=1`, and parses the JSON report from its stdout.
//! Running from inside the app is the only sound source: policies are
//! compiled into the app's own model/repository types, so the standalone CLI
//! (which links `autumn-web` but never the user's models) cannot see them —
//! mirrors `autumn jobs manifest` / `autumn task --list`.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// Options controlling `autumn retention`.
pub struct RetentionOptions<'a> {
    /// Package to run (for workspaces).
    pub package: Option<&'a str>,
    /// Binary target to run (for packages with multiple bin targets).
    pub bin: Option<&'a str>,
    /// Profile forwarded to the app binary via `AUTUMN_ENV`.
    pub profile: &'a str,
    /// Report what would be swept without deleting anything. Currently the
    /// only supported mode — see [`run`].
    pub dry_run: bool,
    /// Narrow the report to a single model's policy.
    pub model: Option<&'a str>,
}

/// One model's dry-run report, deserialized from the app's
/// `AUTUMN_RETENTION_DRY_RUN=1` stdout — mirrors
/// `autumn_web::retention::RetentionSweepReport`.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionSweepReport {
    pub model: String,
    pub rows_swept: u64,
    pub duration_ms: u64,
    pub dry_run: bool,
}

/// Run `autumn retention`.
pub fn run(opts: &RetentionOptions<'_>) {
    if !opts.dry_run {
        eprintln!("autumn retention: only --dry-run is supported today.");
        eprintln!(
            "Declared policies already run on their own fleet-coordinated schedule inside \
             the app the moment it boots; there is no separate command to trigger a real \
             sweep by hand."
        );
        std::process::exit(1);
    }

    eprintln!("\u{1F342} autumn retention --dry-run\n");
    crate::routes::compile_binary(opts.package, opts.bin);
    let binary = crate::routes::find_binary(opts.package, opts.bin);

    let mut command = Command::new(&binary);
    command
        .env("AUTUMN_RETENTION_DRY_RUN", "1")
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(model) = opts.model {
        command.env("AUTUMN_RETENTION_MODEL", model);
    }
    crate::task::apply_managed_pg_env(&mut command, opts.package);

    let output = command.output().unwrap_or_else(|error| {
        eprintln!("\u{2717} Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while running the retention dry-run",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reports: Vec<RetentionSweepReport> = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        eprintln!("Failed to parse retention dry-run JSON: {error}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    print!("{}", format_retention_report(&reports));
}

/// Render a dry-run report as a fixed-width table, sorted by model name (the
/// app already sorts its JSON, but formatting is defensive against a future
/// caller feeding in unsorted data).
pub fn format_retention_report(reports: &[RetentionSweepReport]) -> String {
    if reports.is_empty() {
        return "No retention(...) policies are registered.\n".to_string();
    }

    let mut sorted: Vec<&RetentionSweepReport> = reports.iter().collect();
    sorted.sort_by(|a, b| a.model.cmp(&b.model));

    let model_width = sorted
        .iter()
        .map(|r| r.model.len())
        .max()
        .unwrap_or("Model".len())
        .max("Model".len());
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<model_width$}  Rows that would be swept  Duration (ms)",
        "Model"
    );
    let _ = writeln!(out, "{:-<model_width$}  ------------------------  -------------", "");
    for report in &sorted {
        let _ = writeln!(
            out,
            "{:<model_width$}  {:<24}  {}",
            report.model, report.rows_swept, report.duration_ms
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_retention_report_includes_model_and_row_count() {
        let table = format_retention_report(&[RetentionSweepReport {
            model: "Widget".to_string(),
            rows_swept: 42,
            duration_ms: 7,
            dry_run: true,
        }]);

        assert!(table.contains("Widget"));
        assert!(table.contains('42'));
    }

    #[test]
    fn format_retention_report_sorts_by_model_name() {
        let table = format_retention_report(&[
            RetentionSweepReport {
                model: "Zeta".to_string(),
                rows_swept: 1,
                duration_ms: 1,
                dry_run: true,
            },
            RetentionSweepReport {
                model: "Alpha".to_string(),
                rows_swept: 2,
                duration_ms: 2,
                dry_run: true,
            },
        ]);

        let alpha_pos = table.find("Alpha").expect("Alpha present");
        let zeta_pos = table.find("Zeta").expect("Zeta present");
        assert!(alpha_pos < zeta_pos, "Alpha must sort before Zeta: {table}");
    }

    #[test]
    fn format_retention_report_reports_no_policies_when_empty() {
        let table = format_retention_report(&[]);
        assert!(table.contains("No retention"));
    }
}
