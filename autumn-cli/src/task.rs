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
fn apply_managed_pg_env(cmd: &mut Command, package: Option<&str>) {
    // The attach URL is CLI→child plumbing, not an operator knob. Clear any
    // inherited value up front so a stale/foreign one can't override the data dir
    // (the provider checks the attach URL first) — including when an explicit
    // `AUTUMN_MANAGED_PG_DATA_DIR` override makes `managed_pg_env` return `None`.
    // We re-set it only when we discover a live cluster to attach to.
    cmd.env_remove(crate::serve::MANAGED_PG_ATTACH_URL_ENV);
    let Some(env) = crate::serve::managed_pg_env(package) else {
        return;
    };
    cmd.env(crate::serve::MANAGED_PG_DATA_DIR_ENV, &env.data_dir);
    if let Some(url) = env.attach_url {
        cmd.env(crate::serve::MANAGED_PG_ATTACH_URL_ENV, url);
    }
}

fn list_tasks(binary: &std::path::Path, opts: &TaskOptions<'_>) {
    let mut command = Command::new(binary);
    command
        .env("AUTUMN_LIST_TASKS", "1")
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
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

    let mut command = Command::new(binary);
    command
        .env("AUTUMN_RUN_TASK", name)
        .env("AUTUMN_TASK_ARGS_JSON", args_json)
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
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
