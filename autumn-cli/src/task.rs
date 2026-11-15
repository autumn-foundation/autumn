//! `autumn task` -- list and run one-off operational tasks.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// Options controlling `autumn task`.
pub struct TaskOptions<'a> {
    pub package: Option<&'a str>,
    pub bin: Option<&'a str>,
    pub profile: &'a str,
    pub list: bool,
    pub name: Option<&'a str>,
    pub args: &'a [String],
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskListing {
    pub name: String,
    pub description: String,
}

/// Run `autumn task`.
pub fn run(opts: &TaskOptions<'_>) {
    eprintln!("autumn task\n");
    crate::routes::compile_binary(opts.package, opts.bin);
    let binary = crate::routes::find_binary(opts.package, opts.bin);

    if opts.list {
        list_tasks(&binary, opts);
    } else {
        run_task(&binary, opts);
    }
}

/// Point a one-off task run at the serve daemon's managed-Postgres cluster:
/// share its data dir and, when the cluster is live, attach to it instead of
/// starting a second postmaster on the daemon's locked data dir. A no-op for
/// apps that don't use managed Postgres (the env vars are simply unread).
///
/// `pub` (the enclosing `task` module is private, so this is crate-visible
/// only) so other DB-touching one-shot CLI commands (e.g. `autumn retention
/// --dry-run`) can share it instead of re-deriving the same attach-vs-spawn
/// logic.
pub fn apply_managed_pg_env(cmd: &mut Command, package: Option<&str>) {
    // The attach URL is CLI-to-child plumbing. Clear any inherited value first: the provider
    // checks it before the data dir, so a stale one would win, even when
    // `AUTUMN_MANAGED_PG_DATA_DIR` makes `managed_pg_env` return `None`.
    cmd.env_remove(crate::serve::MANAGED_PG_ATTACH_URL_ENV);
    let Some(env) = crate::serve::managed_pg_env(package) else {
        return;
    };
    cmd.env(crate::serve::MANAGED_PG_DATA_DIR_ENV, &env.data_dir);
    if let Some(url) = env.attach_url {
        cmd.env(crate::serve::MANAGED_PG_ATTACH_URL_ENV, url);
    }
}

/// Selects the task-list one-shot mode in the app.
const LIST_TASKS_ENV: &str = "AUTUMN_LIST_TASKS";
/// Selects the run-task one-shot mode in the app. The value is the task name.
const RUN_TASK_ENV: &str = "AUTUMN_RUN_TASK";

/// Makes the command for `autumn task --list`.
fn list_tasks_command(binary: &std::path::Path, profile: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .env(LIST_TASKS_ENV, "1")
        .env("AUTUMN_ENV", profile)
        .env("AUTUMN_PROFILE", profile)
        .stdout(Stdio::piped())
        // `output()` captures stderr by default. Show the app errors to the user.
        .stderr(Stdio::inherit());
    crate::routes::clear_other_dump_modes(&mut command, LIST_TASKS_ENV);
    command
}

fn list_tasks(binary: &std::path::Path, opts: &TaskOptions<'_>) {
    let mut command = list_tasks_command(binary, opts.profile);
    apply_managed_pg_env(&mut command, opts.package);
    let output = command.output().unwrap_or_else(|error| {
        eprintln!("Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });

    if !output.status.success() {
        eprintln!(
            "Binary exited with status {} while listing tasks",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let tasks: Vec<TaskListing> = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        eprintln!("Failed to parse task listing JSON: {error}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    print_task_table(&tasks);
}

/// Makes the command for `autumn task <name>`.
///
/// `AppBuilder::run` reads the dump modes and `AUTUMN_LIST_TASKS` before
/// `AUTUMN_RUN_TASK`. This function removes them from the child environment.
/// Thus a value from the parent shell cannot stop the task (#3733).
fn run_task_command(
    binary: &std::path::Path,
    profile: &str,
    name: &str,
    args_json: &str,
) -> Command {
    let mut command = Command::new(binary);
    command
        .env(RUN_TASK_ENV, name)
        .env("AUTUMN_TASK_ARGS_JSON", args_json)
        .env("AUTUMN_ENV", profile)
        .env("AUTUMN_PROFILE", profile)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    crate::routes::clear_other_dump_modes(&mut command, RUN_TASK_ENV);
    command.env_remove(LIST_TASKS_ENV);
    command
}

fn run_task(binary: &std::path::Path, opts: &TaskOptions<'_>) {
    let Some(name) = opts.name else {
        eprintln!("autumn task: missing task name");
        eprintln!("Try `autumn task --list` to see registered tasks.");
        std::process::exit(1);
    };

    let args_json = serde_json::to_string(opts.args).unwrap_or_else(|error| {
        eprintln!("Failed to encode task args: {error}");
        std::process::exit(1);
    });

    let mut command = run_task_command(binary, opts.profile, name, &args_json);
    apply_managed_pg_env(&mut command, opts.package);
    let status = command.status().unwrap_or_else(|error| {
        eprintln!("Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

pub fn format_task_table(tasks: &[TaskListing]) -> String {
    if tasks.is_empty() {
        return "No tasks registered.\n".to_string();
    }

    let name_width = tasks
        .iter()
        .map(|task| task.name.len())
        .max()
        .unwrap_or("Name".len())
        .max("Name".len());
    let mut out = String::new();
    let _ = writeln!(out, "{:<name_width$}  Description", "Name");
    let _ = writeln!(out, "{:-<name_width$}  -----------", "");
    for task in tasks {
        let _ = writeln!(out, "{:<name_width$}  {}", task.name, task.description);
    }
    out
}

fn print_task_table(tasks: &[TaskListing]) {
    print!("{}", format_task_table(tasks));
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsStr;
    use std::path::Path;

    /// Returns the entry for `key` in `command`. `None` means inherited.
    /// `Some(None)` means removed. `Some(Some(v))` means set to `v`.
    fn env_entry<'a>(command: &'a Command, key: &str) -> Option<Option<&'a OsStr>> {
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(key))
            .map(|(_, value)| value)
    }

    fn set(value: &str) -> Option<Option<&OsStr>> {
        Some(Some(OsStr::new(value)))
    }

    #[test]
    fn run_task_command_clears_inherited_list_tasks() {
        // Tests the fix for #3733. `AppBuilder::run` reads `AUTUMN_LIST_TASKS`
        // first. An inherited value lists the tasks and does not run the task.
        let command = run_task_command(Path::new("app"), "dev", "cleanup", "[]");

        assert_eq!(env_entry(&command, "AUTUMN_LIST_TASKS"), Some(None));
    }

    #[test]
    fn run_task_command_clears_every_dump_mode() {
        // `AppBuilder::run` reads each dump mode before the task modes (#2370).
        let command = run_task_command(Path::new("app"), "dev", "cleanup", "[]");

        for &var in crate::routes::ALL_DUMP_MODE_ENVS {
            assert_eq!(
                env_entry(&command, var),
                Some(None),
                "{var} must be removed"
            );
        }
    }

    #[test]
    fn run_task_command_sets_task_name_args_and_profile() {
        let command = run_task_command(Path::new("app"), "prod", "backfill", r#"["--dry-run"]"#);

        assert_eq!(env_entry(&command, "AUTUMN_RUN_TASK"), set("backfill"));
        assert_eq!(
            env_entry(&command, "AUTUMN_TASK_ARGS_JSON"),
            set(r#"["--dry-run"]"#)
        );
        assert_eq!(env_entry(&command, "AUTUMN_ENV"), set("prod"));
        assert_eq!(env_entry(&command, "AUTUMN_PROFILE"), set("prod"));
    }

    #[test]
    fn list_tasks_command_clears_every_dump_mode_and_keeps_its_own() {
        let command = list_tasks_command(Path::new("app"), "staging");

        for &var in crate::routes::ALL_DUMP_MODE_ENVS {
            assert_eq!(
                env_entry(&command, var),
                Some(None),
                "{var} must be removed"
            );
        }
        assert_eq!(env_entry(&command, "AUTUMN_LIST_TASKS"), set("1"));
        assert_eq!(env_entry(&command, "AUTUMN_ENV"), set("staging"));
        assert_eq!(env_entry(&command, "AUTUMN_PROFILE"), set("staging"));
    }

    #[test]
    fn format_task_table_includes_names_and_descriptions() {
        let table = format_task_table(&[
            TaskListing {
                name: "cleanup".to_string(),
                description: "Clean stale rows".to_string(),
            },
            TaskListing {
                name: "backfill".to_string(),
                description: "Backfill values".to_string(),
            },
        ]);

        assert!(table.contains("cleanup"));
        assert!(table.contains("Clean stale rows"));
        assert!(table.contains("backfill"));
        assert!(table.contains("Backfill values"));
    }
}
