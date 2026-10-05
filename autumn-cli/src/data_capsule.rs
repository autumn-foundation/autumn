//! `autumn data capsule export|import|verify` (issue #1811).
//!
//! The CLI compiles the app and runs it with `AUTUMN_DATA_CAPSULE=<mode>`.
//! The app uses its own capsule models, database, blob store, and signing
//! secret. It prints one JSON line that starts with
//! `AUTUMN_DATA_CAPSULE_REPORT=`, then exits.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Matched against `autumn_web`'s `app::DATA_CAPSULE_JSON_PREFIX`.
const REPORT_PREFIX: &str = "AUTUMN_DATA_CAPSULE_REPORT=";
const MODE_ENV: &str = "AUTUMN_DATA_CAPSULE";
const SUBJECT_ENV: &str = "AUTUMN_DATA_CAPSULE_SUBJECT";
const PATH_ENV: &str = "AUTUMN_DATA_CAPSULE_PATH";

/// Every env var of this one-shot protocol.
const ONE_SHOT_ENV: [&str; 3] = [MODE_ENV, SUBJECT_ENV, PATH_ENV];

/// Remove the capsule protocol from a command that starts a server.
///
/// `AppBuilder::run` reads `AUTUMN_DATA_CAPSULE` before it starts the server.
/// An inherited value would make `autumn serve` import a capsule and exit.
pub fn clear_inherited_one_shot_env(command: &mut Command) {
    for var in ONE_SHOT_ENV {
        command.env_remove(var);
    }
}

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapsuleAction {
    /// Write a capsule for one subject.
    Export {
        /// The subject id.
        subject: String,
    },
    /// Verify, then import a capsule.
    Import,
    /// Verify a capsule only.
    Verify,
}

impl CapsuleAction {
    const fn env_value(&self) -> &'static str {
        match self {
            Self::Export { .. } => "export",
            Self::Import => "import",
            Self::Verify => "verify",
        }
    }
}

/// Options of `autumn data capsule`.
pub struct CapsuleOptions<'a> {
    /// Package to run (for workspaces).
    pub package: Option<&'a str>,
    /// Binary target to run.
    pub bin: Option<&'a str>,
    /// Profile for `AUTUMN_ENV`.
    pub profile: &'a str,
    /// What to do.
    pub action: CapsuleAction,
    /// The capsule directory.
    pub path: &'a str,
    /// Allow `import` on a profile that is not `dev` or `test`.
    pub force: bool,
    /// Print the raw JSON report.
    pub json: bool,
}

/// Run `autumn data capsule`.
pub fn run(opts: &CapsuleOptions<'_>) {
    eprintln!(
        "\u{1F342} autumn data capsule {}\n",
        opts.action.env_value()
    );
    if let Some(refusal) = production_refusal(opts) {
        eprintln!("\u{2717} {refusal}");
        std::process::exit(1);
    }
    let path = std::path::absolute(opts.path).unwrap_or_else(|error| {
        eprintln!("\u{2717} Bad path {}: {error}", opts.path);
        std::process::exit(1);
    });
    crate::routes::compile_binary(opts.package, opts.bin);
    let binary = crate::routes::find_binary(opts.package, opts.bin);
    let mut command = build_command(&binary, opts, &path);
    crate::task::apply_managed_pg_env(&mut command, opts.package);

    let output = command.output().unwrap_or_else(|error| {
        eprintln!("\u{2717} Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(json) = extract_report_json(&stdout) else {
        eprintln!("Failed to find the capsule report in the binary's output.");
        // A binary that exits 0 with no report did not run the command.
        std::process::exit(output.status.code().filter(|code| *code != 0).unwrap_or(1));
    };
    if opts.json {
        println!("{json}");
    } else {
        println!("{}", format_report(json, &path));
    }
    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }
}

/// Why an `import` is refused, if it is.
///
/// Import writes data. On a profile that is not `dev` or `test` it needs
/// `--force`, the same guard as `autumn db retention --purge`.
fn production_refusal(opts: &CapsuleOptions<'_>) -> Option<String> {
    if opts.action != CapsuleAction::Import || opts.force {
        return None;
    }
    let profile = crate::migrate::canonical_profile(opts.profile);
    if matches!(profile.as_str(), "dev" | "test") {
        return None;
    }
    Some(format!(
        "Refusing to import a capsule into the {profile:?} profile.\n  \
         Import writes records. Re-run with --force if you mean it."
    ))
}

/// The child command. Separate from [`run`] so tests can read its env.
fn build_command(binary: &Path, opts: &CapsuleOptions<'_>, path: &Path) -> Command {
    let mut command = Command::new(binary);
    clear_competing_one_shot_env(&mut command);
    command
        .env(MODE_ENV, opts.action.env_value())
        .env(PATH_ENV, path)
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    match &opts.action {
        CapsuleAction::Export { subject } => command.env(SUBJECT_ENV, subject),
        _ => command.env_remove(SUBJECT_ENV),
    };
    command
}

/// Clear the one-shot modes that `AppBuilder::run` reads before
/// `AUTUMN_DATA_CAPSULE`. An inherited value would take this run.
fn clear_competing_one_shot_env(command: &mut Command) {
    for var in [
        "AUTUMN_BUILD_STATIC",
        "AUTUMN_DUMP_ROUTES",
        "AUTUMN_DUMP_CACHE_COHERENCE",
        "AUTUMN_DUMP_DATA_FLOW",
        "AUTUMN_DUMP_AGENT_AUTHORITY",
        "AUTUMN_DUMP_GRAPH",
        "AUTUMN_DUMP_JOBS",
        "AUTUMN_LIST_TASKS",
        "AUTUMN_RUN_TASK",
        "AUTUMN_MIGRATE",
        "AUTUMN_RETENTION_DRY_RUN",
        "AUTUMN_DB_RETENTION",
        "AUTUMN_DB_RETENTION_DATASET",
    ] {
        command.env_remove(var);
    }
}

/// The JSON of the last report line. Logs can come before it.
fn extract_report_json(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(REPORT_PREFIX))
}

/// A short human summary of the report.
fn format_report(json: &str, path: &Path) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_owned();
    };
    if value["ok"] != true {
        let error = value["error"].as_str().unwrap_or("unknown error");
        return format!("\u{2717} {error}");
    }
    let report = &value["report"];
    let path = PathBuf::from(path);
    match value["mode"].as_str() {
        Some("export") => format!(
            "\u{2713} Exported subject {} ({} records, {} blobs) to {}\n  \
             Open {} in a browser to view it.",
            report["subject"].as_str().unwrap_or("?"),
            report["records"],
            report["blobs"],
            path.display(),
            path.join("viewer").join("index.html").display()
        ),
        Some("import") => format!(
            "\u{2713} Imported {} records from {}",
            report["records"],
            path.display()
        ),
        _ => format!(
            "\u{2713} Capsule {} is intact ({} files, {} records, subject {})",
            path.display(),
            report["files_checked"],
            report["records"],
            report["subject"].as_str().unwrap_or("?")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(action: CapsuleAction, profile: &str, force: bool) -> CapsuleOptions<'static> {
        CapsuleOptions {
            package: None,
            bin: None,
            profile: Box::leak(profile.to_owned().into_boxed_str()),
            action,
            path: "capsule",
            force,
            json: false,
        }
    }

    fn env_of(command: &Command, var: &str) -> Option<Option<String>> {
        command
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(var))
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn import_needs_force_outside_dev_and_test() {
        assert!(production_refusal(&opts(CapsuleAction::Import, "prod", false)).is_some());
        assert!(production_refusal(&opts(CapsuleAction::Import, "prod", true)).is_none());
        assert!(production_refusal(&opts(CapsuleAction::Import, "dev", false)).is_none());
        assert!(production_refusal(&opts(CapsuleAction::Import, "development", false)).is_none());
        assert!(production_refusal(&opts(CapsuleAction::Verify, "prod", false)).is_none());
        let export = CapsuleAction::Export {
            subject: "1".into(),
        };
        assert!(production_refusal(&opts(export, "prod", false)).is_none());
    }

    #[test]
    fn export_command_carries_mode_subject_and_path() {
        let o = opts(
            CapsuleAction::Export {
                subject: "42".into(),
            },
            "dev",
            false,
        );
        let cmd = build_command(Path::new("/bin/true"), &o, Path::new("/tmp/c"));
        assert_eq!(env_of(&cmd, MODE_ENV), Some(Some("export".into())));
        assert_eq!(env_of(&cmd, SUBJECT_ENV), Some(Some("42".into())));
        assert_eq!(env_of(&cmd, PATH_ENV), Some(Some("/tmp/c".into())));
        assert_eq!(env_of(&cmd, "AUTUMN_DB_RETENTION"), Some(None));
        assert_eq!(env_of(&cmd, "AUTUMN_RUN_TASK"), Some(None));
    }

    #[test]
    fn import_command_removes_an_inherited_subject() {
        let o = opts(CapsuleAction::Import, "dev", false);
        let cmd = build_command(Path::new("/bin/true"), &o, Path::new("/tmp/c"));
        assert_eq!(env_of(&cmd, MODE_ENV), Some(Some("import".into())));
        assert_eq!(env_of(&cmd, SUBJECT_ENV), Some(None));
    }

    #[test]
    fn clear_inherited_removes_every_protocol_var() {
        let mut cmd = Command::new("/bin/true");
        clear_inherited_one_shot_env(&mut cmd);
        for var in ONE_SHOT_ENV {
            assert_eq!(env_of(&cmd, var), Some(None), "{var}");
        }
    }

    #[test]
    fn report_json_is_the_last_prefixed_line() {
        let stdout = "log line\nAUTUMN_DATA_CAPSULE_REPORT={\"a\":1}\nmore\n\
                      AUTUMN_DATA_CAPSULE_REPORT={\"a\":2}\n";
        assert_eq!(extract_report_json(stdout), Some("{\"a\":2}"));
        assert_eq!(extract_report_json("nothing"), None);
    }

    #[test]
    fn report_text_names_the_viewer_after_export() {
        let json = r#"{"ok":true,"mode":"export","report":{"subject":"42","records":3,"blobs":1}}"#;
        let text = format_report(json, Path::new("/tmp/c"));
        assert!(text.contains("subject 42"), "{text}");
        assert!(text.contains("3 records"), "{text}");
        assert!(text.contains("viewer"), "{text}");
    }

    #[test]
    fn report_text_shows_the_error() {
        let json = r#"{"ok":false,"mode":"verify","error":"capsule integrity check failed: x"}"#;
        assert!(format_report(json, Path::new("c")).contains("integrity check failed"));
    }
}
