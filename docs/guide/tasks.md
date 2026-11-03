# One-Off Tasks

Use one-off tasks for operational scripts that need normal Autumn context but
should not become HTTP routes: backfills, cleanup jobs, customer-specific data
fixes, replay scripts, and reports.

Tasks run in the app binary through `autumn task <name>`. The framework loads
the active profile, initializes tracing, creates the configured database pool,
installs mail/config extractors, runs startup hooks, executes the task, then
runs shutdown hooks.

## Declare A Task

```rust
use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct CleanupArgs {
    #[serde(default)]
    pub confirm: bool,
}

/// Clean up unpublished draft posts.
#[autumn_web::task(name = "cleanup-posts")]
pub async fn cleanup_posts(
    mut db: Db,
    config: AutumnConfig,
    TaskArgs(args): TaskArgs<CleanupArgs>,
) -> AutumnResult<()> {
    if !args.confirm {
        autumn_web::reexports::tracing::info!(
            profile = ?config.profile,
            "dry run; pass --confirm to apply changes"
        );
        return Ok(());
    }

    // Use `db` exactly as you would in a route handler.
    Ok(())
}
```

Register it on the app builder:

```rust
mod tasks;

use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index])
        .one_off_tasks(one_off_tasks![tasks::cleanup_posts])
        .run()
        .await;
}
```

## Run Tasks

List registered tasks:

```bash
autumn task --list
```

Run a task with the active profile:

```bash
autumn task cleanup-posts --confirm
autumn task --profile prod cleanup-posts --confirm
```

Workspace apps can select a package and binary:

```bash
autumn task --package blog --bin blog --list
autumn task --package blog cleanup-posts --confirm
```

Task arguments use long-flag syntax. Flags become snake_case fields on
`TaskArgs<T>`:

```bash
autumn task backfill-user --user-id 42 --dry-run
```

deserializes as:

```rust
#[derive(serde::Deserialize)]
struct BackfillArgs {
    user_id: i64,
    dry_run: bool,
}
```

A task returning `Err(AutumnError)` exits non-zero and prints the error to
stderr, so CI and cron can rely on normal process status.

## Run A Task In A Deployed Image

`autumn task` builds the app binary with cargo before it runs it. A release
image has the binary but no toolchain, so run the binary directly. The CLI
sets three environment variables, and you can set them yourself:

| Variable | Effect |
| --- | --- |
| `AUTUMN_RUN_TASK=<name>` | Run the named task with full app context, then exit. |
| `AUTUMN_TASK_ARGS_JSON` | The task flags as a JSON array of strings, for example `["--user-id","42","--dry-run"]`. Omit it for a task with no flags. |
| `AUTUMN_LIST_TASKS=1` | Print the registered tasks as JSON and exit. |

```bash
AUTUMN_RUN_TASK=backfill-user AUTUMN_TASK_ARGS_JSON='["--user-id","42"]' ./blog
```

JSON quoting through a remote shell (`fly ssh console -C`, `kubectl exec`) is
easy to get wrong. Ship a small wrapper script in the image that takes
`<name> --flag value`, encodes the flags, and sets the variables.

The task runs inside the app binary, so it reads the same config files,
profile and environment as the server, and gets the same database pool.
Startup hooks run before the task. A hook that starts a background loop should
skip it when `AUTUMN_RUN_TASK` is set, because the process exits as soon as
the task returns.

## A Task, Not A Separate Binary

Write an operational script as a task. Do not add a `src/bin/` program that
opens its own database connection. A separate binary reads a raw URL and
skips what the app resolves: the active profile, TLS and pooler settings,
encryption keys, and the rest of `autumn.toml`. It can work on a laptop and fail
against a managed database. A task gets all of it from the app, and it ships
in the image the app already deploys.

## Generate A Skeleton

```bash
autumn generate task cleanup_users
```

This creates `tasks/cleanup_users.rs` with a `#[task]` skeleton and adds
`serde` if the project does not already depend on it. To register a generated
root-level task file from `src/main.rs`:

```rust
#[path = "../tasks/cleanup_users.rs"]
mod cleanup_users_task;

autumn_web::app()
    .routes(routes![index])
    .one_off_tasks(one_off_tasks![cleanup_users_task::cleanup_users]);
```

For larger apps, keeping task modules under `src/tasks/` and using normal Rust
module declarations is often cleaner. The runtime only requires that the task
function is registered with `.one_off_tasks(...)`.
