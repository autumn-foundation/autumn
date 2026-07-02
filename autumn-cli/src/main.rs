use clap::{Parser, Subcommand};

mod assets;
mod build;
mod canary;
mod check;
mod cold_start_driver;
mod config;
mod credentials;
mod data;
mod db;
mod db_pull;
mod dev;
mod dev_loop_bench;
mod dev_loop_scaling;
mod doctor;
mod experiments;
mod export;
mod flags;
mod generate;
mod http;
mod maintenance;
mod migrate;
mod monitor;
mod new;
mod paths;
mod plugin_check;
mod process;
mod release;
mod routes;
mod scaling_driver;
mod seed;
mod serve;
mod setup;
mod shard;
mod starters;
mod task;
mod token;
mod webhook;
/// Subcommands for `autumn check`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum CheckSubcommands {
    /// Check for active routes past their sunset date
    Deprecations {
        /// Package to build/check (for workspaces)
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to check (for packages with multiple bin targets)
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
    },
}

/// The Autumn web framework CLI.
#[derive(Parser)]
#[command(name = "autumn", version, about = "The Autumn web framework CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Available subcommands.
#[derive(Subcommand)]
enum Commands {
    /// Create a new Autumn project
    New {
        /// Project name (must be a valid Rust package name). Optional only when
        /// `--list-starters` is given.
        name: Option<String>,
        /// Scaffold from a starter instead of the minimal base project. Accepts
        /// a built-in name (see `--list-starters`), a local directory, a full
        /// git URL, or an `owner/repo` GitHub shorthand (optionally `@ref`).
        #[arg(long)]
        starter: Option<String>,
        /// Pin a git starter to a tag, branch, or revision. Mutually exclusive
        /// with an inline `@ref` suffix on `--starter`.
        #[arg(long)]
        starter_ref: Option<String>,
        /// List the available built-in starters and exit.
        #[arg(long)]
        list_starters: bool,
        /// Skip the provenance confirmation prompt for community (git/local)
        /// starters. Required to apply a community starter non-interactively.
        #[arg(long)]
        yes: bool,
        /// Scaffold the optional i18n module (Project Fluent translations
        /// at `i18n/en.ftl`, the `[i18n]` block in `autumn.toml`, and the
        /// `i18n` feature flag on `autumn-web`).
        #[arg(long)]
        with_i18n: bool,
        /// Scaffold a stub `src/bin/seed.rs` for database seeding (default off)
        #[arg(long)]
        with_seed: bool,
        /// Daemon starter: a model-free app that builds with no Postgres,
        /// ready to run as a local daemon via `autumn serve`.
        #[arg(long)]
        daemon: bool,
        /// Managed/bundled-Postgres daemon starter: keeps the database and
        /// wires a managed local Postgres provider (implies a daemon app).
        #[arg(long = "bundled-pg")]
        bundled_pg: bool,
    },
    /// Pre-render static routes to dist/
    Build {
        /// Build in debug mode instead of release
        #[arg(long)]
        debug: bool,
        /// Package to build (for workspaces)
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to build (for packages with multiple \[\[bin\]\] targets)
        #[arg(long)]
        bin: Option<String>,
        /// Embed static assets + i18n locales into the binary for a true
        /// single-binary deploy (enables the `autumn-web/embed-assets` feature
        /// and fingerprints before compiling so the manifest is baked in).
        #[arg(long)]
        embed: bool,
        /// Extra Cargo features to enable (comma-separated). Forwarded to both
        /// the fingerprint phase and the embed compile so features like
        /// `autumn-web/managed-pg-bundled` are active throughout all build steps.
        #[arg(long, value_name = "FEATURES")]
        features: Option<String>,
    },
    /// Start the dev server with hot reload (watch mode)
    Dev {
        /// Package to run (for workspaces)
        #[arg(short, long)]
        package: Option<String>,
        /// Log all registered routes, tasks, middleware, and config at startup
        #[arg(long)]
        show_config: bool,
    },
    /// Run the app as a production (non-watch) server, optionally as a daemon.
    ///
    /// Unlike `autumn dev`, `serve` does not watch files or hot-reload. With
    /// `--daemon` it backgrounds the server under a PID lockfile and binds a
    /// Unix domain socket under a platform runtime dir; `stop`, `status`, and
    /// `restart` manage that daemon.
    Serve {
        /// Lifecycle action (omit to start in the foreground / with --daemon).
        #[command(subcommand)]
        action: Option<ServeCommands>,
        /// Run in the background as a managed daemon.
        #[arg(long)]
        daemon: bool,
        /// Build and run in release mode (optimized production binary).
        #[arg(long)]
        release: bool,
        /// Bundled/managed-Postgres build (implies --daemon). Recorded in the
        /// address file; the app must be built with the managed-pg feature.
        #[arg(long = "bundled-pg")]
        bundled_pg: bool,
        /// Package to run (for workspaces)
        #[arg(short, long)]
        package: Option<String>,
    },
    /// Download and configure external tools (Tailwind CSS)
    Setup {
        /// Re-download even if the binary already exists
        #[arg(long)]
        force: bool,
    },
    /// Pin, vendor, and integrity-verify JS dependencies
    Assets {
        #[command(subcommand)]
        action: AssetsCommands,
    },
    /// Run or inspect database migrations
    Migrate {
        #[command(subcommand)]
        action: Option<MigrateCommands>,
        /// Enable maintenance mode before running migrations and disable it
        /// after a successful run. If migrations fail, maintenance mode stays
        /// on so no corrupt traffic reaches the database.
        #[arg(long)]
        with_maintenance: bool,
        /// Target a single shard by its configured `[[database.shards]]`
        /// name instead of all databases.
        #[arg(long, value_name = "NAME", conflicts_with = "control_only")]
        shard: Option<String>,
        /// Target only the control database (`database.primary_url`),
        /// skipping any configured shards.
        #[arg(long)]
        control_only: bool,
        /// Resolve database URLs through a profile overlay: deep-merge
        /// `autumn-<profile>.toml` over `autumn.toml` before reading the
        /// control and shard URLs. When omitted, the profile is selected from
        /// `AUTUMN_ENV` (preferred) or the legacy `AUTUMN_PROFILE`, matching the
        /// app's runtime precedence — so env vars are not overridden by this flag.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Wait up to SECS seconds for the database to become reachable before
        /// failing, retrying with capped exponential backoff. Overrides
        /// `database.startup_wait_secs` from the config file and
        /// `AUTUMN_DATABASE__STARTUP_WAIT_SECS` from the environment.
        /// When omitted, the config value is used (default `0` = fail fast).
        #[arg(long, value_name = "SECS")]
        wait: Option<u64>,
    },
    /// Create, drop, or reset the database itself.
    ///
    /// These commands resolve the connection the same way `autumn migrate`
    /// does (defaults → `autumn.toml` → `autumn-{profile}.toml` → `AUTUMN_*`,
    /// plus `DATABASE_URL` / `primary_url`) and operate only on the primary
    /// write role, connecting to the server's maintenance database to issue
    /// `CREATE`/`DROP`.
    ///
    ///   autumn db create
    ///   autumn db drop --force
    ///   autumn db reset
    #[command(subcommand, verbatim_doc_comment, name = "db")]
    Db(DbCommands),
    /// Shard operations (e.g. moving a tenant's data between shards)
    Shard(ShardCommands),
    /// Live monitoring dashboard for a running Autumn application
    Monitor {
        /// URL of the running Autumn application
        #[arg(short, long, default_value = "http://localhost:3000")]
        url: String,
        /// Polling interval in seconds
        #[arg(short, long, default_value = "1")]
        interval: u64,
    },
    /// Export an offline diagnostic snapshot of the application
    Export {
        /// URL of the running Autumn application
        #[arg(short, long, default_value = "http://localhost:3000")]
        url: String,
        /// Output file for diagnostics
        #[arg(short, long, default_value = "autumn-diag.json")]
        output: String,
    },
    /// Export or import model data as CSV.
    ///
    /// `autumn data export` streams all rows of a model to a CSV file.
    /// `autumn data import` reads a CSV file and inserts (or upserts) rows.
    ///
    /// Both commands call the application's admin HTTP layer, so the app must
    /// be running and the admin plugin must be mounted.
    ///
    /// # Examples
    ///
    ///   autumn data export posts --out posts.csv
    ///   autumn data export posts --search hello --out results.csv
    ///   autumn data import posts --in posts.csv
    ///   autumn data import posts --in posts.csv --dry-run
    ///   autumn data import posts --in posts.csv --upsert-by id
    #[command(subcommand, verbatim_doc_comment, name = "data")]
    Data(DataCommands),

    /// Run the project's seed binary to populate the database with representative data.
    ///
    /// Requires `src/bin/seed.rs` (a Cargo binary named `seed`) to exist.
    /// If it is missing, `autumn seed` prints an actionable error and exits 1.
    ///
    /// `autumn seed` checks for pending migrations before running and exits 1
    /// if any are found — run `autumn migrate` first.
    Seed {
        /// Profile forwarded to the seed binary via `AUTUMN_ENV`
        /// (default: `dev`).
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Package to run (for workspaces)
        #[arg(short, long)]
        package: Option<String>,
    },
    /// Run or list one-off operational tasks registered by the application.
    Task {
        /// Package to run (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to run (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Profile forwarded to the app binary via `AUTUMN_ENV`.
        #[arg(long, default_value = "dev")]
        profile: String,
        /// List registered tasks instead of running one.
        #[arg(long)]
        list: bool,
        /// Task name to run.
        name: Option<String>,
        /// Arguments forwarded to the task, e.g. `--user-id 42`.
        #[arg(
            value_name = "ARGS",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
    /// Scaffold models, migrations, and CRUD code for a new resource.
    ///
    /// Four subcommands collapse the repetitive five-file dance of adding
    /// a resource — `#[model]` struct, Diesel migration, schema entry,
    /// `#[repository]`, route handlers, Maud templates, `routes![]`
    /// registration, smoke test — into a single command.
    ///
    /// # Field-type DSL
    ///
    /// Fields are passed as `name:Type` tokens. Supported types:
    ///
    ///   String, Text                 (TEXT)
    ///   i32, i64                     (INTEGER, BIGINT)
    ///   bool                         (BOOLEAN)
    ///   f32, f64                     (REAL, DOUBLE PRECISION)
    ///   Uuid                         (UUID)
    ///   `NaiveDateTime`, `DateTime`      (TIMESTAMP, TIMESTAMPTZ)
    ///   `Vec<u8>`, Bytea               (BYTEA)
    ///   Option<...>                  (any of the above, nullable)
    ///
    /// # Example
    ///
    ///   autumn generate scaffold Post title:String body:Text published:bool
    #[command(subcommand, verbatim_doc_comment)]
    Generate(GenerateCommands),

    /// Scaffold production deployment artifacts (Dockerfile, .dockerignore,
    /// runtime config template, and optional target-specific files).
    ///
    /// Run from the project root directory. Does not overwrite existing files
    /// unless `--force` is given.
    ///
    /// # Examples
    ///
    ///   autumn release init
    ///   autumn release init --force
    ///   autumn release init --target fly
    ///   autumn release init --target docker-compose
    #[command(subcommand, verbatim_doc_comment)]
    Release(ReleaseCommands),

    /// Simulate a signed webhook request to the local application.
    #[command(subcommand, verbatim_doc_comment)]
    Webhook(WebhookCommands),
    /// Issue and revoke API bearer tokens backed by the `api_tokens` table.
    ///
    /// Requires the `api_tokens` table to exist. Run `autumn migrate` first;
    /// it applies both your app migrations and Autumn's framework migration
    /// for the token table.
    /// The database URL is read from `autumn.toml` or the `DATABASE_URL` /
    /// `AUTUMN_DATABASE__URL` environment variables.
    ///
    /// # Examples
    ///
    ///   autumn token issue user:42
    ///   autumn token revoke `<RAW_TOKEN>`
    #[command(subcommand, verbatim_doc_comment)]
    Token(TokenCommands),

    /// Inspect and toggle feature flags at runtime without redeploying.
    ///
    /// Feature flags control which actors see a feature. Mutations propagate
    /// to all running replicas within seconds via Postgres LISTEN/NOTIFY cache
    /// invalidation.
    ///
    /// The database URL is resolved from `autumn.toml`, profile overrides, or
    /// the `AUTUMN_DATABASE__PRIMARY_URL` / `AUTUMN_DATABASE__URL` /
    /// `DATABASE_URL` environment variables.
    ///
    /// # Examples
    ///
    ///   autumn flags list
    ///   autumn flags enable dark_mode
    ///   autumn flags disable dark_mode --actor ops@example.com
    ///   autumn flags set-rollout new_checkout 10
    ///   autumn flags allow beta_inbox user:42
    #[command(subcommand, verbatim_doc_comment)]
    #[allow(clippy::doc_markdown)]
    Flags(FlagsCommands),

    /// Manage A/B experiments at runtime.
    ///
    /// Experiments declare named variants with weights, assign actors to variants
    /// deterministically, and emit structured exposure events to your analytics
    /// pipeline.  Weight changes propagate to new actors immediately; existing
    /// sticky assignments are preserved.
    ///
    /// The database URL is resolved from `autumn.toml`, profile overrides, or
    /// the `AUTUMN_DATABASE__PRIMARY_URL` / `AUTUMN_DATABASE__URL` /
    /// `DATABASE_URL` environment variables.
    ///
    /// # Examples
    ///
    ///   autumn experiments list
    ///   autumn experiments status checkout_v2
    ///   autumn experiments set-weights checkout_v2 control=50,treatment=50
    ///   autumn experiments conclude checkout_v2 treatment
    ///   autumn experiments override checkout_v2 qa@example.com treatment
    #[command(subcommand, verbatim_doc_comment)]
    #[allow(clippy::doc_markdown)]
    Experiments(ExperimentsCommands),

    /// Run accessibility (WCAG 2.1 AA) checks against rendered HTML.
    ///
    /// `autumn check --a11y` runs a pure-Rust static HTML analysis pass and
    /// reports Critical and Serious violations that would block a11y compliance.
    /// Point it at a running Autumn app with `--url`, or supply raw HTML via
    /// `--html` for CI pre-render workflows.
    ///
    /// # Examples
    ///
    ///   autumn check --a11y --url <http://localhost:3000>
    ///   autumn check --a11y --html "$(cat dist/index.html)"
    #[command(verbatim_doc_comment)]
    Check {
        /// Run the WCAG 2.1 AA accessibility audit.
        #[arg(long)]
        a11y: bool,
        /// URL of a running Autumn app to audit (fetches the root page).
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Inline HTML string to audit instead of fetching from a URL.
        #[arg(long, value_name = "HTML")]
        html: Option<String>,
        /// Fail only on Critical violations; treat Serious as warnings.
        #[arg(long)]
        critical_only: bool,
        /// Run the config typo/validity check on autumn.toml and profiles.
        #[arg(long)]
        config: bool,

        #[command(subcommand)]
        subcommand: Option<CheckSubcommands>,
    },

    /// Check the local environment and project configuration for common
    /// first-run problems (Rust MSRV, autumn.toml validity, database
    /// connectivity, port availability, Tailwind binary, and more).
    Doctor {
        /// Emit machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
        /// Treat warnings as failures (exit 1 on any ⚠️).
        #[arg(long)]
        strict: bool,
    },

    /// Run conformance checks against a plugin's route contributions.
    ///
    /// Compiles the application (debug profile), introspects its route table,
    /// and verifies that the named plugin satisfies five checks: installability,
    /// route attribution, route prefix, route collision, and sensitive-surface
    /// gating.  Exits 0 on pass, 1 on failure.
    ///
    /// # Examples
    ///
    ///   autumn plugin-check --plugin-name autumn-admin-plugin --prefix /admin \
    ///       --sensitive-route /admin:"Role: admin required"
    #[command(verbatim_doc_comment)]
    PluginCheck {
        /// Package to build (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to build (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Documented plugin name to check (e.g. `autumn-admin-plugin`).
        #[arg(long, value_name = "NAME")]
        plugin_name: String,
        /// Expected route prefix for all plugin routes (e.g. `/admin`).
        #[arg(long, value_name = "PREFIX")]
        prefix: Option<String>,
        /// Declare a sensitive route with its auth/profile gating mechanism.
        /// Format: `PATH_PREFIX:DESCRIPTION` (e.g. `/admin:Role admin required`).
        /// Repeatable.
        #[arg(long, value_name = "PATH:DESCRIPTION")]
        sensitive_route: Vec<String>,
        /// Output format: `text` (default) or `json`.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        format: String,
    },

    /// Inspect and mutate live runtime configuration values.
    ///
    /// Runtime config values are typed, schema-validated knobs that change
    /// without a redeploy.  They are stored in `autumn_runtime_config_values`
    /// and every mutation is audited in `autumn_runtime_config_changes`.
    ///
    /// The database URL is resolved from `autumn.toml`, `autumn-<profile>.toml`,
    /// or the `AUTUMN_DATABASE__PRIMARY_URL` / `AUTUMN_DATABASE__URL` /
    /// `DATABASE_URL` environment variables.
    ///
    /// # Examples
    ///
    ///   autumn config list
    ///   autumn config get `max_upload_mb`
    ///   autumn config set `max_upload_mb` 200
    ///   autumn config unset `max_upload_mb`
    ///   autumn config history `max_upload_mb`
    ///   autumn config history `max_upload_mb` --limit 50
    #[command(subcommand, verbatim_doc_comment)]
    Config(ConfigCommands),

    /// Manage encrypted credentials for the current Autumn project.
    ///
    /// Secrets are stored in `config/credentials/<env>.toml.enc` encrypted with
    /// AES-256-GCM.  The master key is read from the `AUTUMN_MASTER_KEY`
    /// environment variable or `config/master.key` (first found wins).
    ///
    /// # Examples
    ///
    ///   autumn credentials edit
    ///   autumn credentials edit --env production
    ///   autumn credentials show
    ///   autumn credentials show --reveal
    #[command(subcommand, verbatim_doc_comment)]
    Credentials(CredentialsCommands),

    /// Enable or disable maintenance mode without restarting the process.
    ///
    /// Writes (or removes) a JSON flag file that the running app polls every
    /// 500 ms. Within one second every replica responds 503 to non-bypassed
    /// HTTP traffic while health-check routes stay green.
    ///
    /// # Examples
    ///
    ///   autumn maintenance on --message "Migrating database"
    ///   autumn maintenance on --readonly
    ///   autumn maintenance on --allow-ips 10.0.0.0/8
    ///   autumn maintenance off
    #[command(subcommand, verbatim_doc_comment)]
    Maintenance(MaintenanceCommands),

    /// Drive canary rollback / promotion at the framework level.
    ///
    /// Autumn does not own the load-balancer traffic split (platform concern).
    /// These commands drive the framework primitives a canary controller needs:
    /// `rollback` tells a bad canary replica to drain and exit cleanly (no
    /// manual SIGTERM); `promote` clears the rollback signal; `status` reports
    /// whether a rollback is pending.
    ///
    /// # Examples
    ///
    ///   autumn canary rollback --reason "p99 latency exceeded"
    ///   autumn canary promote
    ///   autumn canary status
    #[command(subcommand, verbatim_doc_comment)]
    Canary(CanaryCommands),

    /// Print every mounted route — method, path, handler, source, middleware.
    ///
    /// Compiles the application (debug profile) and introspects its route
    /// table without starting the HTTP server or connecting to a database.
    ///
    /// Rows are stable-sorted by path, then method, so the output is
    /// diff-friendly. Redirect to a file and `git diff` two snapshots to
    /// audit route changes between commits.
    Routes {
        /// Package to inspect (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to inspect (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Output format.
        #[arg(long, default_value = "table", value_name = "FORMAT")]
        format: String,
        /// Show only routes whose path starts with PREFIX (positional shorthand for --filter).
        #[arg(value_name = "PREFIX")]
        prefix: Option<String>,
        /// Show only routes whose path starts with FILTER.
        #[arg(long, value_name = "FILTER")]
        filter: Option<String>,
        /// Restrict to one or more HTTP methods (comma-separated, e.g. `GET,POST`).
        #[arg(long, value_delimiter = ',', value_name = "METHOD")]
        method: Vec<String>,
        /// Hide framework-internal routes (`/actuator/*`, probes, htmx assets).
        #[arg(long)]
        user_only: bool,
    },

    /// Measure and gate dev-loop latency for `autumn dev`.
    ///
    /// Reports p50, p95, and maximum end-to-end latency for each change
    /// class (Rust edit, CSS/Tailwind edit, static asset, config edit, etc.)
    /// and compares the results against the accepted budget defined in
    /// `docs/guide/dev-loop-latency.md`.
    ///
    /// Use `--dry-run` to print the budget table without starting a server.
    /// Use `--fail-on-regression` in CI to exit 1 when a budget is exceeded.
    ///
    /// # Examples
    ///
    ///   autumn dev-loop-bench --dry-run
    ///   autumn dev-loop-bench --example examples/hello --runs 5 --output report.json
    ///   autumn dev-loop-bench --fail-on-regression
    #[command(name = "dev-loop-bench", verbatim_doc_comment)]
    DevLoopBench {
        /// Example project to benchmark (path relative to workspace root).
        #[arg(long, default_value = "examples/hello")]
        example: String,
        /// Number of measurement runs per change class.
        #[arg(long, default_value = "5")]
        runs: u32,
        /// Write the machine-readable JSON report to this file path.
        #[arg(long, value_name = "PATH")]
        output: Option<String>,
        /// Emit machine-readable JSON to stdout instead of the human summary.
        #[arg(long)]
        json: bool,
        /// Exit 1 if any change class exceeds its latency budget.
        #[arg(long)]
        fail_on_regression: bool,
        /// Print the budget table and exit without starting a server.
        #[arg(long)]
        dry_run: bool,
        /// Measure the cold-start onboarding journey (`autumn new` → first 200,
        /// including the first clean compile) instead of the warm dev loop.
        #[arg(long)]
        cold_start: bool,
        /// With `--cold-start`, also measure the database-backed shape as an
        /// informational (non-gating) result.
        #[arg(long)]
        include_db: bool,
        /// Run the macro-scaling sweep: measure warm incremental rebuild at
        /// multiple app sizes (N handlers + model/repository pairs) to gate
        /// that the edit-refresh loop stays near-flat as the app grows.
        #[arg(long)]
        scaling: bool,
        /// Comma-separated list of app sizes to sweep (e.g. `1,25,50,100`).
        /// Only used with `--scaling`.
        #[arg(long, default_value = crate::dev_loop_scaling::DEFAULT_SIZES)]
        sizes: String,
        /// Path to `benchmarks/dev-loop-scaling/baseline.json` for the
        /// `>20%`-slope-regression check. Omit to skip baseline gating.
        /// Only used with `--scaling`.
        #[arg(long, value_name = "PATH")]
        baseline: Option<String>,
    },
}

/// Subcommands for `autumn assets`.
#[derive(Subcommand)]
enum AssetsCommands {
    /// Download a JS dependency, compute a sha384 SRI hash, and record it in the manifest.
    ///
    /// Example: `autumn assets add htmx@2.0.4`
    Add {
        /// Package spec in `<name>@<version>` format (e.g. `htmx@2.0.4`).
        spec: String,
        /// Override the download URL (required for packages not in the built-in registry).
        #[arg(long)]
        url: Option<String>,
    },
    /// Print all pinned JS dependencies with their version and integrity hash.
    List,
    /// Re-download and re-pin a dependency (or all if no name given).
    ///
    /// Examples:
    ///   `autumn assets update htmx`         — re-pin the recorded version
    ///   `autumn assets update htmx@2.0.5`   — re-pin to a new version
    ///   `autumn assets update`               — refresh all vendored assets
    Update {
        /// Name or `<name>@<version>` spec to update. Omit to update all.
        name: Option<String>,
    },
    /// Recompute sha384 hashes for all vendored files and compare to the manifest.
    Verify,
}

/// Subcommands for `autumn config`.
#[derive(Subcommand)]
enum ConfigCommands {
    /// List all active config overrides.
    ///
    /// Prints key, current value, and last-updated timestamp for every key
    /// that has been set via `autumn config set`.  Keys using their compile-time
    /// default are not shown.
    List,
    /// Print the stored override for a single config key.
    ///
    /// Exits with a non-zero code and a clear message when the key has no
    /// active override (i.e. the application is using the compile-time default).
    Get {
        /// Config key name (must be declared in the application schema).
        key: String,
    },
    /// Set a runtime config key to a new value.
    ///
    /// The value is stored as-is; type validation is performed by the running
    /// application when it reads the key. To check that a value is valid before
    /// setting it, verify the declared type in the application schema.
    ///
    /// Every set records actor, old value, and new value in the change log.
    Set {
        /// Config key name.
        key: String,
        /// New raw value (must be parseable as the key's declared type).
        #[arg(allow_hyphen_values = true)]
        value: String,
        /// Actor identifier stored in the change log (e.g. your email).
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
    /// Revert a config key to its compile-time default.
    ///
    /// Removes the active override so the running application falls back to
    /// the value declared in its `ConfigRegistry`.
    Unset {
        /// Config key name.
        key: String,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
    /// Show the change history for a config key.
    ///
    /// Prints actor, old value, new value, and timestamp for the most recent
    /// changes, newest first.
    History {
        /// Config key name.
        key: String,
        /// Maximum number of history entries to return (default: 20).
        #[arg(long, default_value = "20", value_name = "N")]
        limit: usize,
    },
}

/// Subcommands for `autumn credentials`.
#[derive(Subcommand)]
enum CredentialsCommands {
    /// Decrypt the credentials file, open it in $VISUAL/$EDITOR, and re-encrypt on save.
    ///
    /// Falls back to `vi` on Unix or `notepad` on Windows when neither editor env var is set.
    /// The plaintext temp file is zeroed before removal.
    Edit {
        /// Environment name (controls which `config/credentials/<env>.toml.enc` is used).
        #[arg(long, default_value = "development")]
        env: String,
    },
    /// Print a summary of the decrypted credentials (keys only, values redacted by default).
    Show {
        /// Environment name.
        #[arg(long, default_value = "development")]
        env: String,
        /// Print the decrypted values instead of redacting them.
        #[arg(long)]
        reveal: bool,
    },
}

/// Subcommands for `autumn db`.
#[derive(Subcommand)]
enum DbCommands {
    /// Create the configured database (idempotent: a no-op notice if it exists).
    Create {
        /// Resolve the connection through a profile overlay. When omitted, the
        /// profile is selected from `AUTUMN_ENV` (preferred) or the legacy
        /// `AUTUMN_PROFILE`, matching the app's runtime precedence.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },
    /// Drop the configured database (idempotent if already absent).
    ///
    /// Refuses to run outside the `dev`/`test` profile unless `--force` is
    /// passed. Credentials are never printed.
    #[command(verbatim_doc_comment)]
    Drop {
        /// Resolve the connection through a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Allow the drop against a non-dev/test (e.g. production) profile.
        #[arg(long)]
        force: bool,
    },
    /// Drop → create → migrate → seed, in that order, as a single command.
    ///
    /// Stops and exits non-zero if any step fails, naming the failed step. The
    /// seed step is skipped (with a notice) when `src/bin/seed.rs` is absent.
    /// Refuses to run outside the `dev`/`test` profile unless `--force` is set.
    #[command(verbatim_doc_comment)]
    Reset {
        /// Resolve the connection through a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Allow the reset against a non-dev/test (e.g. production) profile.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold Autumn models from an existing database (read-only introspection).
    ///
    /// Connects to the resolved primary database (the same way `autumn migrate`
    /// does) and emits, for each selected table, a `#[model]` struct in
    /// `src/models/`, a `diesel::table!` entry in `src/schema.rs`, and the
    /// `pub mod` aggregator line — using the same file-emission machinery as
    /// `autumn generate`. No migration is written and no data is touched.
    ///
    /// # Examples
    ///
    ///   # Pull every table:
    ///   autumn db pull
    ///
    ///   # Pull specific tables, also emitting repositories:
    ///   autumn db pull posts comments --with-repository
    #[command(verbatim_doc_comment)]
    Pull {
        /// Tables to pull. When omitted, every non-system table is pulled.
        #[arg(value_name = "TABLE")]
        tables: Vec<String>,
        /// Resolve the connection through a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Also emit a `#[repository(Model)]` trait per table.
        #[arg(long)]
        with_repository: bool,
        /// Print the planned actions without writing any files.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing model/repository files instead of erroring.
        #[arg(long)]
        force: bool,
    },
}

impl DbCommands {
    /// Translate a lifecycle subcommand (`create`/`drop`/`reset`) into the `db`
    /// module's command and the optional profile override the connection should
    /// be resolved under. `pull` is dispatched separately (it does not map onto
    /// [`db::DbCommand`]).
    fn into_command(self) -> (db::DbCommand, Option<String>) {
        match self {
            Self::Create { profile } => (db::DbCommand::Create, profile),
            Self::Drop { profile, force } => (db::DbCommand::Drop { force }, profile),
            Self::Reset { profile, force } => (db::DbCommand::Reset { force }, profile),
            Self::Pull { .. } => unreachable!("db pull is dispatched before into_command"),
        }
    }
}

/// Lifecycle subcommands for `autumn serve`.
#[derive(Subcommand)]
enum ServeCommands {
    /// Stop the running daemon (graceful drain, then force-kill on timeout).
    Stop,
    /// Report whether the daemon is running and where it is reachable.
    Status,
    /// Stop the daemon (if running) and start it again in the background.
    Restart,
}

/// Subcommands for `autumn migrate`.
#[derive(Subcommand)]
enum MigrateCommands {
    /// Show migration status (applied and pending)
    Status,
    /// Run a production-safety preflight check on all migration SQL files.
    ///
    /// Classifies every `up.sql` and `down.sql` in the migrations directory
    /// into one of: safe, potentially-blocking, destructive, irreversible,
    /// data-backfill, or manual-review-required.
    ///
    /// Exits with code 0 when all migrations are safe for a rolling deploy.
    /// Exits with code 1 and prints a detailed report when any unsafe or
    /// unclassified operations are detected.
    ///
    /// Does not require a database connection — safe to run in CI before deploy.
    ///
    /// # Example
    ///
    ///   autumn migrate check
    #[command(verbatim_doc_comment)]
    Check,
    /// Revert the most recently applied user migration(s).
    ///
    /// Executes each migration's `down.sql` in reverse chronological order and
    /// removes its record from `__diesel_schema_migrations`.
    ///
    /// Framework-owned migrations (the ones Autumn ships internally) are
    /// **never** rolled back by this command — they are forward-only by design.
    ///
    /// # Examples
    ///
    ///   # Revert the most recently applied migration (default --steps 1):
    ///   autumn migrate down
    ///
    ///   # Revert the last 3 applied user migrations:
    ///   autumn migrate down --steps 3
    ///
    ///   # Revert until VERSION is the latest applied:
    ///   autumn migrate down --to 20260101000000
    ///
    ///   # Required when the active profile is prod/production:
    ///   autumn migrate down --yes-i-mean-prod
    #[command(verbatim_doc_comment)]
    Down {
        /// Number of user migrations to revert in newest-first order (default: 1).
        ///
        /// Mutually exclusive with --to.
        #[arg(long, value_name = "N", conflicts_with = "to")]
        steps: Option<usize>,
        /// Revert user migrations until VERSION is the latest applied.
        ///
        /// VERSION must be a currently applied *user* migration (fails cleanly
        /// otherwise). Framework migrations are forward-only and cannot be used
        /// as a boundary. Mutually exclusive with --steps.
        #[arg(long, value_name = "VERSION", conflicts_with = "steps")]
        to: Option<String>,
        /// Required when the active profile is prod or production.
        ///
        /// Without this flag the command exits non-zero with a clear message
        /// before touching the database.
        #[arg(long)]
        yes_i_mean_prod: bool,
    },
}

/// Subcommands for `autumn shard`.
#[derive(clap::Args)]
struct ShardCommands {
    #[command(subcommand)]
    command: ShardSubcommand,
}

#[derive(Subcommand)]
enum ShardSubcommand {
    /// Move a set of tenants' rows from one configured shard to another.
    ///
    /// Resolves --from / --to by their `[[database.shards]]` names (honoring
    /// --profile and env, like `autumn migrate`), copies the rows, verifies
    /// counts + a content checksum, and deletes the source rows only with
    /// --confirm. It never edits routing — copy & verify, re-route the tenant
    /// (pin it in the directory router), then re-run with --confirm to delete.
    ///
    /// # Example
    ///
    ///   autumn shard move-slot --from shard0 --to shard1 \
    ///     --table bookmarks --tenant acme
    ///   # …pin acme to shard1 (directory router), deploy, then:
    ///   autumn shard move-slot --from shard0 --to shard1 \
    ///     --table bookmarks --tenant acme --confirm
    #[command(verbatim_doc_comment)]
    MoveSlot {
        /// Source shard name (a `[[database.shards]]` entry).
        #[arg(long, value_name = "SHARD")]
        from: String,
        /// Destination shard name.
        #[arg(long, value_name = "SHARD")]
        to: String,
        /// Table holding the tenant data to move.
        #[arg(long, value_name = "TABLE")]
        table: String,
        /// Column holding the tenant/routing key. Default: `tenant_id`.
        #[arg(long, value_name = "COLUMN", default_value = "tenant_id")]
        key_column: String,
        /// Primary-key column whose `BIGSERIAL`/identity sequence is advanced on
        /// the destination after the copy (PK values are copied as-is).
        /// Default: `id`.
        #[arg(long, value_name = "COLUMN", default_value = "id")]
        id_column: String,
        /// Tenant key to move (repeat for several).
        #[arg(long = "tenant", value_name = "KEY", required = true)]
        tenants: Vec<String>,
        /// Delete the source rows after a successful, verified copy.
        #[arg(long)]
        confirm: bool,
        /// Resolve shard URLs through a profile overlay (like `autumn migrate`).
        /// When omitted, the profile is selected from `AUTUMN_ENV` (preferred)
        /// or the legacy `AUTUMN_PROFILE`, matching the app's runtime precedence.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },
}

/// Subcommands for `autumn data`.
#[derive(Subcommand)]
enum DataCommands {
    /// Export all rows of a model to a CSV file.
    ///
    /// Calls `GET {url}/{model}/export.csv` on the running application.
    /// The admin plugin must be mounted and the model must support CSV export.
    ///
    /// # Examples
    ///
    ///   autumn data export posts --out posts.csv
    ///   autumn data export posts --out posts.csv --url <http://localhost:3000/admin>
    #[command(verbatim_doc_comment)]
    Export {
        /// Model slug (e.g. `posts`, `users`).
        model: String,
        /// Admin prefix URL including the mount path (e.g. `http://host/admin`).
        #[arg(short, long, default_value = "http://localhost:3000/admin")]
        url: String,
        /// Output file path (defaults to `<model>.csv`).
        #[arg(short, long, value_name = "FILE")]
        out: Option<String>,
        /// Free-text search forwarded as `?q=<text>` to the admin export
        /// endpoint. The admin model's `list` implementation must honour the
        /// `search` field; use `filter.<field>=<value>` query params for
        /// exact field filtering.
        #[arg(long, value_name = "TEXT")]
        search: Option<String>,
        /// Raw `Cookie` header value for authenticated admin installs.
        /// Copy from browser dev tools, e.g. `autumn_session=abc123`.
        #[arg(long, value_name = "COOKIE")]
        cookie: Option<String>,
    },
    /// Import rows from a CSV file into a model.
    ///
    /// Calls `POST {url}/{model}/import` on the running application with the
    /// CSV file as a multipart upload.  The admin plugin must be mounted and
    /// the model must have `supports_csv_import()` returning `true`.
    ///
    /// # Examples
    ///
    ///   autumn data import posts --in posts.csv
    ///   autumn data import posts --in posts.csv --dry-run
    ///   autumn data import posts --in posts.csv --upsert-by id
    #[command(verbatim_doc_comment)]
    Import {
        /// Model slug (e.g. `posts`, `users`).
        model: String,
        /// Admin prefix URL including the mount path (e.g. `http://host/admin`).
        #[arg(short, long, default_value = "http://localhost:3000/admin")]
        url: String,
        /// Path to the CSV file to import.
        #[arg(short = 'i', long = "in", value_name = "FILE")]
        input: String,
        /// Validate rows but do not write to the database.
        #[arg(long)]
        dry_run: bool,
        /// Column to use as the upsert key (enables upsert mode).
        #[arg(long, value_name = "COL")]
        upsert_by: Option<String>,
        /// Raw `Cookie` header value for authenticated admin installs.
        /// Copy from browser dev tools, e.g. `autumn_session=abc123`.
        #[arg(long, value_name = "COOKIE")]
        cookie: Option<String>,
    },
}

/// Subcommands for `autumn maintenance`.
#[derive(Subcommand)]
enum MaintenanceCommands {
    /// Enable maintenance mode: write the flag file so running replicas return 503.
    ///
    /// Exits 0 on success. The running app detects the flag within 500 ms.
    ///
    /// # Examples
    ///
    ///   autumn maintenance on
    ///   autumn maintenance on --message "Upgrading database schema"
    ///   autumn maintenance on --readonly
    ///   autumn maintenance on --allow-ips 10.0.0.0/8 --bypass-header X-Dev-Bypass:mytoken
    #[command(verbatim_doc_comment)]
    On {
        /// Human-readable message shown to users in the 503 response body.
        #[arg(long, value_name = "MSG")]
        message: Option<String>,
        /// CIDR block or IP address whose requests bypass maintenance.
        /// Repeatable: `--allow-ips 10.0.0.0/8 --allow-ips 172.16.0.1`
        #[arg(long, value_name = "CIDR")]
        allow_ips: Vec<String>,
        /// Allow GET, HEAD, OPTIONS through while blocking writes.
        #[arg(long)]
        readonly: bool,
        /// Bypass header in NAME:VALUE format.
        /// Requests carrying this header+value bypass the 503.
        /// Example: `--bypass-header X-Autumn-Maintenance-Bypass:mytoken`
        #[arg(long, value_name = "NAME:VALUE")]
        bypass_header: Option<String>,
    },
    /// Disable maintenance mode: remove the flag file so replicas resume normal traffic.
    ///
    /// Exits 0 on success (or when maintenance was already off).
    Off,
}

/// Subcommands for `autumn canary`.
#[derive(Subcommand)]
enum CanaryCommands {
    /// Signal a canary rollback: write the flag file so the canary replica
    /// drains (/ready → 503) and exits cleanly without a manual SIGTERM.
    ///
    /// The running replica detects the flag within ~500 ms.
    ///
    /// # Examples
    ///
    ///   autumn canary rollback
    ///   autumn canary rollback --reason "error rate spiked" --by ci-controller
    #[command(verbatim_doc_comment)]
    Rollback {
        /// Human-readable reason recorded in the rollback flag.
        #[arg(long, value_name = "REASON")]
        reason: Option<String>,
        /// Identifier of the actor or controller requesting the rollback.
        #[arg(long, value_name = "WHO")]
        by: Option<String>,
    },
    /// Promote the canary: clear any pending rollback flag.
    ///
    /// Shifting platform traffic to 100% remains a platform action.
    Promote,
    /// Report whether a canary rollback is currently pending.
    Status,
}

/// Subcommands for `autumn token`.

#[derive(Subcommand)]
enum WebhookCommands {
    /// Send a simulated webhook request with a generated HMAC signature.
    Sim {
        /// The provider to simulate (stripe, github, slack, generic).
        provider: String,
        /// The target URL to send the webhook to.
        url: String,
        /// The webhook secret used to sign the request.
        #[arg(long)]
        #[arg(long, env = "AUTUMN_WEBHOOK_SECRET")]
        secret: String,
        /// The payload to send in the request body.
        #[arg(long)]
        payload: String,
    },
}

/// Subcommands for `autumn token`.
#[derive(Subcommand)]
enum TokenCommands {
    /// Issue a new API bearer token for a principal and print it to stdout.
    ///
    /// The token is generated with 256 bits of OS-backed randomness and stored
    /// as a SHA-256 hash. It is printed **once** — there is no way to recover
    /// it later. Store it securely (e.g. in a secrets manager).
    ///
    /// # Example
    ///
    ///   TOKEN=$(autumn token issue user:42)
    ///   curl -H "Authorization: Bearer $TOKEN" <http://localhost:3000/api/data>
    #[command(verbatim_doc_comment)]
    Issue {
        /// Principal identifier to associate with the token (e.g. `user:42`).
        principal_id: String,
        /// Human-readable name for the token (e.g. `ci`, `partner-integration`).
        #[arg(long, default_value = "")]
        name: String,
        /// Grant a scope (flat string, e.g. `posts:read`). Repeatable.
        #[arg(long = "scope")]
        scope: Vec<String>,
        /// Optional expiry as an ISO-8601 / SQL timestamp (e.g.
        /// `2026-12-31T23:59:59`). Omit for a non-expiring token.
        #[arg(long)]
        expires_at: Option<String>,
    },
    /// List non-secret metadata for a principal's API tokens.
    ///
    /// Prints name, scopes, expiry, last-used, and revocation status. The raw
    /// token and its hash are never shown.
    ///
    /// # Example
    ///
    ///   autumn token list service:ci
    #[command(verbatim_doc_comment)]
    List {
        /// Principal identifier whose tokens to list (e.g. `service:ci`).
        principal_id: String,
    },
    /// Rotate an API token: revoke it and issue a replacement with the same
    /// name and scopes. Prints the new raw token once.
    ///
    /// # Example
    ///
    ///   autumn token rotate `<RAW_TOKEN>`
    #[command(verbatim_doc_comment)]
    Rotate {
        /// The raw bearer token string to rotate.
        raw_token: String,
    },
    /// Revoke an existing API bearer token.
    ///
    /// Hashes the provided raw token and sets `revoked_at` in the database.
    /// Subsequent requests presenting the token will receive `401 Unauthorized`.
    ///
    /// # Example
    ///
    ///   autumn token revoke `<RAW_TOKEN>`
    #[command(verbatim_doc_comment)]
    Revoke {
        /// The raw bearer token string to revoke.
        raw_token: String,
    },
}

/// Subcommands for `autumn flags`.
#[derive(Subcommand)]
#[allow(clippy::doc_markdown)]
enum FlagsCommands {
    /// List all feature flags and their current state.
    List,
    /// Globally enable a flag (all actors will see it as enabled).
    ///
    /// Creates the flag if it does not exist.
    ///
    /// # Example
    ///
    ///   autumn flags enable dark_mode
    ///   autumn flags enable dark_mode --actor ops@example.com
    #[command(verbatim_doc_comment)]
    Enable {
        /// Flag key (must be snake_case).
        key: String,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
    /// Globally disable a flag (all actors will see it as disabled).
    ///
    /// Creates the flag if it does not exist.
    ///
    /// # Example
    ///
    ///   autumn flags disable dark_mode
    #[command(verbatim_doc_comment)]
    Disable {
        /// Flag key.
        key: String,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
    /// Set the percent-rollout gate for a flag (0–100).
    ///
    /// Actors are bucketed deterministically by (flag_name, actor_id) so a
    /// given user never flips between cohorts on repeated requests.
    ///
    /// Use 0 to disable the rollout gate. Use 100 to enable for all actors.
    ///
    /// # Example
    ///
    ///   autumn flags set-rollout new_checkout 10
    ///   autumn flags set-rollout new_checkout 50 --actor ops@example.com
    #[command(name = "set-rollout", verbatim_doc_comment)]
    SetRollout {
        /// Flag key.
        key: String,
        /// Rollout percentage (0–100).
        pct: u8,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
    /// Add an actor to the explicit allowlist for a flag.
    ///
    /// The actor will always see the flag as enabled regardless of the
    /// global gate or rollout percentage.
    ///
    /// # Example
    ///
    ///   autumn flags allow beta_inbox user:42
    #[command(verbatim_doc_comment)]
    Allow {
        /// Flag key.
        key: String,
        /// Actor ID to allowlist (e.g. `user:42`).
        actor_id: String,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
}

/// Subcommands for `autumn experiments`.
#[derive(Subcommand)]
#[allow(clippy::doc_markdown)]
enum ExperimentsCommands {
    /// List all experiments and their current state.
    List,
    /// Show detailed status for a single experiment.
    ///
    /// # Example
    ///
    ///   autumn experiments status checkout_v2
    #[command(verbatim_doc_comment)]
    Status {
        /// Experiment name.
        name: String,
    },
    /// Update the variant weights for an experiment.
    ///
    /// Existing sticky assignments are NOT re-bucketed. New actors will be
    /// bucketed against the updated weights immediately.
    ///
    /// Weights are specified as comma-separated `variant=weight` pairs. Weights
    /// are relative and do not need to sum to 100.
    ///
    /// # Example
    ///
    ///   autumn experiments set-weights checkout_v2 control=50,treatment=50
    ///   autumn experiments set-weights pricing_v3 control=33,low=33,high=34
    #[command(name = "set-weights", verbatim_doc_comment)]
    SetWeights {
        /// Experiment name.
        name: String,
        /// Variant weights as `"variant=weight,..."` (e.g. `"control=50,treatment=50"`).
        weights: String,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
    /// Conclude an experiment and pin a winning variant.
    ///
    /// After concluding, `assign()` returns the winner for all actors without
    /// emitting new exposure events.
    ///
    /// # Example
    ///
    ///   autumn experiments conclude checkout_v2 treatment
    #[command(verbatim_doc_comment)]
    Conclude {
        /// Experiment name.
        name: String,
        /// Winning variant name.
        winner: String,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
    /// Pin a staff/QA actor to a specific variant, bypassing weight-based bucketing.
    ///
    /// The override is tagged with `is_override = true` in exposure events so
    /// analytics pipelines can exclude overridden assignments from results.
    ///
    /// # Example
    ///
    ///   autumn experiments override checkout_v2 qa@example.com treatment
    #[command(verbatim_doc_comment)]
    Override {
        /// Experiment name.
        name: String,
        /// Actor ID to pin (e.g. `user:42` or `qa@example.com`).
        actor_id: String,
        /// Variant to force for this actor.
        variant: String,
        /// Actor identifier stored in the change log.
        #[arg(long, value_name = "ACTOR")]
        actor: Option<String>,
    },
}

/// Subcommands for `autumn release`.
#[derive(Subcommand)]
enum ReleaseCommands {
    /// Emit production-ready deployment files at the project root.
    ///
    /// Default (no --target): Dockerfile + .dockerignore + autumn.production.toml.example.
    /// --target fly        : also emits fly.toml.
    /// --target docker-compose : also emits docker-compose.yml with app + Postgres.
    Init {
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
        /// Deployment target: fly | docker-compose (omit for bare Dockerfile).
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
}

/// Subcommands for `autumn generate`.
#[derive(Subcommand)]
enum GenerateCommands {
    /// Generate a `#[model]` struct, Diesel migration, and schema entry.
    ///
    /// Field types: String, Text, i32, i64, bool, f32, f64, Uuid, `NaiveDateTime`,
    /// `DateTime`, Vec<u8>/Bytea, Attachment, references, Option<...>.
    ///
    /// `field:references` scaffolds a foreign key: a `field_id BIGINT` column
    /// with a `REFERENCES <table>(id)` constraint and an index, where `<table>`
    /// is the pluralised form of `field`. Append `?` for a nullable FK
    /// (`post:references?` -> `post_id: Option<i64>`).
    ///
    /// Examples:
    ///
    ///   autumn generate model Post title:String body:Text published:bool
    ///   autumn generate model Comment body:Text post:references
    #[command(verbatim_doc_comment)]
    Model {
        /// Resource name (`PascalCase` or `snake_case`, e.g. `Post`).
        name: String,
        /// Field DSL tokens, each `name:Type`.
        fields: Vec<String>,
        /// Add a `deleted_at` column and use soft-delete in the repository.
        #[arg(long)]
        soft_delete: bool,
        /// Primary-key type: `bigint` (default) or `uuid`.
        #[arg(long, value_name = "TYPE")]
        id: Option<String>,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate an empty Diesel migration directory.
    ///
    /// When the migration name follows the `Add<Field>To<Table>` or
    /// `Remove<Field>From<Table>` convention, the generator emits the
    /// matching `ALTER TABLE` statements automatically.
    Migration {
        /// Migration name (`PascalCase` or `snake_case`).
        name: String,
        /// Field DSL tokens — only used for `Add…To…` / `Remove…From…` names.
        fields: Vec<String>,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate a one-off operational `#[task]` skeleton.
    Task {
        /// Task function name (`snake_case`, e.g. `cleanup_users`).
        name: String,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold a `#[job]` background-job handler, args struct,
    /// `src/jobs/mod.rs` aggregator, and `.jobs(jobs::registered_jobs())`
    /// registration in `src/main.rs`.
    ///
    /// Creates:
    ///
    /// - `src/jobs/<snake>.rs` — `<Pascal>Args` struct + `#[job]` handler
    ///   \+ commented enqueue snippet + smoke test
    /// - `src/jobs/mod.rs` — created/updated with `pub mod` and
    ///   idempotent `registered_jobs()` aggregator
    /// - `src/main.rs` — `mod jobs;` + `.jobs(jobs::registered_jobs())`
    /// - `Cargo.toml` — `serde` dependency added if missing
    ///
    /// The `#[job]` macro generates a companion struct `<Pascal>Job` with
    /// `NAME`, `enqueue`, `enqueue_in`, and `enqueue_at` methods.
    ///
    /// Example:
    ///
    ///   autumn generate job `SendWelcomeEmail` `user_id:i64` `email:String`
    #[command(verbatim_doc_comment)]
    Job {
        /// Job name (`PascalCase` or `snake_case`, e.g. `SendWelcomeEmail`).
        name: String,
        /// Fields for the args struct in `name:Type` format
        /// (e.g. `user_id:i64 email:String`).
        fields: Vec<String>,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold a `#[mailer]` struct, HTML+text templates, preview
    /// registration, and a smoke test.
    ///
    /// Creates:
    ///   - `src/mailers/<snake>.rs`        — mailer struct + `#[mailer]` impl
    ///   - `templates/mailers/<snake>.html` — HTML template placeholder
    ///   - `templates/mailers/<snake>.txt`  — plain-text template placeholder
    ///   - `src/mailers/mod.rs`             — created/updated with `pub mod`
    ///   - `tests/<snake>_mailer.rs`        — smoke test
    ///   - `src/main.rs`                   — wired into dev preview registry
    ///   - `Cargo.toml`                    — `"mail"` feature added to autumn-web
    ///
    /// The `#[mailer]` macro generates `send_<name>` (async) and
    /// `deliver_later_<name>` (fire-and-forget) from each method in the impl.
    ///
    /// Example:
    ///
    ///   autumn generate mailer Welcome
    #[command(verbatim_doc_comment)]
    Mailer {
        /// Mailer name (`PascalCase` or `snake_case`, e.g. `Welcome`).
        name: String,
        /// Opt into RFC 8058 one-click List-Unsubscribe for the given logical
        /// list / suppression scope (e.g. `weekly_digest`). Scaffolds the
        /// `#[mailer(list_unsubscribe = "...")]` attribute and a
        /// `mail_unsubscribes` suppression migration. Use only for bulk mail
        /// (newsletters, digests, drip campaigns) — never for password resets,
        /// MFA codes, or security alerts.
        #[arg(long, value_name = "SCOPE")]
        list_unsubscribe: Option<String>,
        /// Opt out of the shared mailer layout. By default the generator wraps
        /// the per-mailer body fragment in `templates/mailers/_layout.html` and
        /// `_layout.txt` at build time. Use `--no-layout` for one-line plaintext
        /// notifications or fully-custom HTML that must not inherit the shared
        /// document shell.
        #[arg(long)]
        no_layout: bool,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold a real-time channel: a pub/sub handler over the built-in
    /// `Channels` API, an htmx SSE live view (default) or a raw `#[ws]`
    /// socket handler, `main.rs` route wiring, and an in-process smoke test.
    ///
    /// Creates:
    ///
    /// - `src/channels/<snake>.rs` — channel handler(s) subscribing/
    ///   publishing through the existing `Channels` API
    /// - `src/channels/mod.rs`     — created/updated with `pub mod`
    /// - `src/main.rs`             — `mod channels;` + route registration
    /// - `tests/<snake>_channel.rs` — smoke test that publishes a message
    ///   and asserts a subscriber receives it
    /// - `Cargo.toml`              — `"ws"` feature added to autumn-web
    ///   (+ transport-specific deps and dev-deps)
    ///
    /// SSE-over-htmx is the default transport (zero client JS authored by
    /// the user). Pass `--ws` for a raw `#[ws]` WebSocket handler instead.
    ///
    /// Example:
    ///
    ///   autumn generate channel Chat
    #[command(verbatim_doc_comment)]
    Channel {
        /// Channel name (`PascalCase` or `snake_case`, e.g. `Chat`).
        name: String,
        /// Use SSE-over-htmx transport (default when neither flag is given).
        #[arg(long, conflicts_with = "ws")]
        sse: bool,
        /// Emit a raw `#[ws]` WebSocket handler instead of the SSE view.
        #[arg(long)]
        ws: bool,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate a complete browser authentication flow: signup, login, logout,
    /// account/profile, forgot-password, and reset-password.
    ///
    /// The generated code uses Autumn's existing session, CSRF, password
    /// hashing, and mail primitives. Only password digests and reset-token
    /// digests are stored — raw secrets are never persisted or logged.
    ///
    /// Pass `--oauth` to additionally scaffold OAuth2/OIDC social-login handlers
    /// for the listed providers (google, github, microsoft are built-in presets;
    /// custom providers are configurable via `autumn.toml`).
    ///
    /// Examples:
    ///
    ///   autumn generate auth User
    ///   autumn generate auth User --oauth github,google
    #[command(verbatim_doc_comment)]
    Auth {
        /// Model name (`PascalCase` or `snake_case`, e.g. `User`).
        name: String,
        /// Comma-separated OAuth2/OIDC providers to scaffold
        /// (e.g. `github,google` or `github,google,microsoft`).
        /// Adds redirect + callback handlers, an `oauth_identities` migration,
        /// the `oauth2` feature on `autumn-web`, and `docs/guide/oauth.md`.
        #[arg(long, value_delimiter = ',', value_name = "PROVIDER")]
        oauth: Vec<String>,
        /// Scaffold optional TOTP two-factor authentication (off by default).
        /// Adds `totp_secret_encrypted` / `totp_enabled` columns to the user
        /// model, a `recovery_codes` table, enrollment + login-verify handlers,
        /// encrypted-at-rest secrets, single-use recovery codes, and generated
        /// 2FA integration tests.
        #[arg(long)]
        totp: bool,
        /// Scaffold `WebAuthn` passkey authentication (off by default).
        /// Adds a `webauthn_credentials` table, ceremony handlers for
        /// register/login begin+finish, a passkey list/revoke surface,
        /// Maud templates with navigator.credentials JS, and integration tests.
        #[arg(long)]
        passkeys: bool,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate an `AdminModel` adapter for an existing model so it can be
    /// managed through `autumn-admin-plugin`.
    ///
    /// Requires the target model to already exist (`src/models/<snake>.rs`).
    /// Run `autumn generate model` or `autumn generate scaffold` first.
    ///
    /// The generator derives sensible field metadata (widget kinds, searchable,
    /// filterable, readonly) from the field-type DSL and lets you refine
    /// individual fields with `--hidden`, `--readonly`, `--password`, or
    /// `--exclude`.
    ///
    /// Example:
    ///
    ///   autumn generate admin Post title:String body:Text published:bool
    #[command(verbatim_doc_comment)]
    Admin {
        /// Model name (`PascalCase` or `snake_case`, e.g. `Post`).
        name: String,
        /// Field DSL tokens, each `name:Type` — same syntax as `scaffold`.
        fields: Vec<String>,
        /// Render this field as `AdminFieldKind::Hidden`. Repeatable.
        #[arg(long, value_name = "FIELD")]
        hidden: Vec<String>,
        /// Mark this field as read-only (`.readonly()`). Repeatable.
        #[arg(long, value_name = "FIELD")]
        readonly: Vec<String>,
        /// Render this field as `AdminFieldKind::Password`. Repeatable.
        #[arg(long, value_name = "FIELD")]
        password: Vec<String>,
        /// Render this field as a `Select` dropdown. Provide option values as
        /// `field=val1,val2,…`; the bare `field` form emits an empty
        /// placeholder. Repeatable.
        ///
        /// Example: `--select status=draft,published,archived`
        #[arg(long, value_name = "FIELD[=VAL1,VAL2,...]")]
        select: Vec<String>,
        /// Exclude this field from the generated adapter entirely. Repeatable.
        #[arg(long, value_name = "FIELD")]
        exclude: Vec<String>,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate an `#[inbound_mail]` handler skeleton for receiving email via
    /// provider webhooks (Mailgun, SES, or generic RFC 5322).
    ///
    /// Creates:
    ///   - `src/inbound_mailers/<snake>.rs`  — handler with `#[inbound_mail]` macro
    ///   - `src/inbound_mailers/mod.rs`      — created/updated with `pub mod`
    ///   - `tests/<snake>_inbound_mail.rs`   — integration smoke test
    ///   - `src/main.rs`                    — wired into `InboundMailRouter`
    ///   - `Cargo.toml`                     — `inbound-mail` feature added
    ///
    /// Example:
    ///
    ///   autumn generate inbound-mail Support
    ///   autumn generate inbound-mail Support --dry-run
    #[command(name = "inbound-mail", verbatim_doc_comment)]
    InboundMail {
        /// Handler name (`PascalCase` or `snake_case`, e.g. `Support`).
        name: String,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate a system-test skeleton under `tests/system/`.
    ///
    /// The generated test is gated behind `#[cfg(feature = "system-tests")]` and
    /// marked `#[ignore]` by default so it only runs when Chromium is available.
    ///
    /// Example:
    ///
    ///   autumn generate system-test NAME
    ///   autumn generate system-test NAME --dry-run
    ///
    /// After generation, run with:
    ///
    ///   cargo test --features system-tests --test NAME -- --include-ignored
    #[command(name = "system-test", verbatim_doc_comment)]
    SystemTest {
        /// Test name (`PascalCase` or `snake_case`, e.g. `TodoFlow`).
        name: String,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold an installable Progressive Web App (manifest, service worker,
    /// icons, and layout meta tags).
    ///
    /// Creates:
    ///   - `static/manifest.webmanifest` — Web App Manifest (served as application/manifest+json)
    ///   - `static/service-worker.js`    — Offline-shell service worker
    ///   - `static/icons/icon.svg`       — Placeholder icon (replace with real PNG for full compat)
    ///   - `static/icons/maskable-icon.svg` — Maskable icon variant
    ///   - `src/main.rs`                 — Route handlers + PWA `<link>`/`<meta>` tags in layout
    ///   - `tests/system/pwa_smoke.rs`   — Smoke test for manifest content-type + SW registration
    ///
    /// Example:
    ///
    ///   autumn generate pwa
    ///   autumn generate pwa --dry-run
    #[command(verbatim_doc_comment)]
    Pwa {
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold a Tauri desktop wrapper that ships the autumn app as a native installer.
    ///
    /// Uses the **sidecar model**: the autumn server binary runs as a supervised child
    /// of the Tauri shell, and the webview loads the app from a free loopback port.
    /// The existing autumn app (routes, Maud/htmx, sessions) runs unmodified.
    ///
    /// The sidecar is built with `autumn-web/embed-assets` (#1004) and
    /// `autumn-web/managed-pg-bundled` (#1119) so the packaged desktop app needs
    /// no separately-installed database or loose asset files.
    ///
    /// Creates:
    ///   - `src-tauri/`                 — standalone Tauri shell crate
    ///   - `src-tauri/tauri.conf.json`  — Tauri v2 config (productName, bundle, sidecar)
    ///   - `src-tauri/src/lib.rs`       — sidecar lifecycle glue (ephemeral port,
    ///     /health polling, kill-on-close)
    ///   - `src-tauri/icons/`           — placeholder icons for immediate buildability
    ///   - `src-tauri/stage-sidecar.sh` — build + stage the sidecar (Unix)
    ///   - `src-tauri/stage-sidecar.ps1`— build + stage the sidecar (Windows)
    ///
    /// Example:
    ///
    ///   autumn generate tauri
    ///   autumn generate tauri --dry-run
    #[command(verbatim_doc_comment)]
    Tauri {
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold a multi-step form wizard with session-backed state and per-step validation.
    ///
    /// Emits step structs, GET + POST handlers, progress rendering, commit and
    /// cancel handlers, and a generated integration test.  All step state is
    /// persisted through the existing `Session` under namespaced keys.
    ///
    /// Example:
    ///
    ///   autumn generate wizard checkout shipping payment review
    #[command(verbatim_doc_comment)]
    Wizard {
        /// Wizard name (`snake_case` or `PascalCase`, e.g. `checkout`).
        name: String,
        /// Ordered list of step names (`snake_case`, e.g. `shipping payment review`).
        steps: Vec<String>,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate model, migration, repository, HTML routes, smoke test, and
    /// register the new routes in `src/main.rs`.
    ///
    /// Field types: String, Text, i32, i64, bool, f32, f64, Uuid, `NaiveDateTime`,
    /// `DateTime`, Vec<u8>/Bytea, Attachment, references, Option<...>.
    ///
    /// `field:references` scaffolds a foreign key (`field_id BIGINT
    /// REFERENCES <table>(id)` plus an index), e.g.
    /// `autumn generate scaffold Comment body:Text post:references`.
    Scaffold {
        /// Resource name (`PascalCase` or `snake_case`, e.g. `Post`).
        name: String,
        /// Field DSL tokens, each `name:Type`.
        fields: Vec<String>,
        /// Add `#[indexed]` and a SQL index for this field. Repeatable.
        #[arg(long, value_name = "FIELD")]
        index: Vec<String>,
        /// Add a validator rule, e.g. `url=url` or `title=length:min=1,max=200`.
        #[arg(long, value_name = "FIELD=RULE")]
        validate: Vec<String>,
        /// Add `#[default]` and a SQL default, e.g. `alive=true`.
        #[arg(long, value_name = "FIELD=VALUE")]
        default: Vec<String>,
        /// Add a derived repository query, e.g. `find_by_tag:tag`.
        #[arg(long, value_name = "METHOD:FIELD")]
        query: Vec<String>,
        /// Load scaffold metadata from a TOML config file (e.g. `autumn.generate.toml`).
        /// CLI flags take precedence over values in the config file.
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        /// Add a `deleted_at` column and use soft-delete in the repository.
        #[arg(long)]
        soft_delete: bool,
        /// Primary-key type: `bigint` (default) or `uuid`.
        #[arg(long, value_name = "TYPE")]
        id: Option<String>,
        /// Scaffold a JSON-only API resource (no HTML/Maud views, mount CRUD endpoints).
        #[arg(long)]
        api: bool,
        /// Generate shard-aware handlers: uses `ShardedDb` instead of `Db` and
        /// calls `from_shard(&db)` on generated repositories.
        #[arg(long)]
        sharded: bool,
        /// The model field used as the sharding key (e.g. `tenant_id`).
        /// Defaults to `tenant_id` if that field is present, otherwise `id`.
        #[arg(long, value_name = "FIELD")]
        shard_key: Option<String>,
        /// Emit `broadcasts = true` on the repository, a `LiveFragment` impl,
        /// an SSE stream route, and an SSE-wired list container in the index view.
        #[arg(long)]
        live: bool,
        /// Emit per-field inline validation endpoints and `hx-post` attributes on
        /// form inputs (implies `--live`).
        #[arg(long)]
        live_validation: bool,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Scaffold an installable/conformant plugin crate.
    ///
    /// Creates:
    ///   - `<target-dir>/Cargo.toml`       — plugin crate cargo file
    ///   - `<target-dir>/src/lib.rs`       — main plugin implementation
    ///   - `<target-dir>/README.md`        — installation & setup documentation
    ///   - `<target-dir>/tests/conformance.rs` — conformance tests verification
    ///
    /// Example:
    ///
    ///   autumn generate plugin custom-auth
    ///   autumn generate plugin custom-auth --path custom/path
    #[command(verbatim_doc_comment)]
    Plugin {
        /// Plugin name (`snake_case` or `kebab-case`, e.g. `admin` or `custom-auth`).
        name: String,
        /// Custom destination path for the generated plugin (defaults to `autumn-<name>-plugin` in the project root).
        #[arg(long)]
        path: Option<String>,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    run_command(cli.command);
}

#[allow(clippy::too_many_lines)]
fn run_command(command: Commands) {
    match command {
        Commands::Build {
            debug,
            package,
            bin,
            embed,
            features,
        } => build::run(
            debug,
            embed,
            package.as_deref(),
            bin.as_deref(),
            features.as_deref(),
        ),
        Commands::Dev {
            package,
            show_config,
        } => dev::run(package.as_deref(), show_config),
        Commands::Serve {
            action,
            daemon,
            release,
            bundled_pg,
            package,
        } => {
            let action = action.map(|a| match a {
                ServeCommands::Stop => serve::ServeAction::Stop,
                ServeCommands::Status => serve::ServeAction::Status,
                ServeCommands::Restart => serve::ServeAction::Restart,
            });
            serve::run(
                action,
                &serve::ServeOptions {
                    package,
                    // --bundled-pg implies --daemon.
                    daemon: daemon || bundled_pg,
                    release,
                    bundled_pg,
                    // Normal start: the child inherits this shell's env. Only
                    // `restart` sets this, to restore a lost profile.
                    profile: None,
                },
            );
        }
        Commands::Migrate {
            action,
            with_maintenance,
            shard,
            control_only,
            profile,
            wait,
        } => {
            let action = match action {
                Some(MigrateCommands::Status) => migrate::MigrateAction::Status,
                Some(MigrateCommands::Check) => migrate::MigrateAction::Check,
                Some(MigrateCommands::Down {
                    steps,
                    to,
                    yes_i_mean_prod,
                }) => migrate::MigrateAction::Down(migrate::DownArgs {
                    steps,
                    to,
                    yes_i_mean_prod,
                }),
                None => migrate::MigrateAction::Run,
            };
            let target = match (shard, control_only) {
                (Some(name), _) => migrate::MigrateTarget::Shard(name),
                (None, true) => migrate::MigrateTarget::ControlOnly,
                (None, false) => migrate::MigrateTarget::All,
            };
            migrate::run(&action, with_maintenance, &target, profile.as_deref(), wait);
        }
        Commands::Db(cmd) => match cmd {
            DbCommands::Pull {
                tables,
                profile,
                with_repository,
                dry_run,
                force,
            } => db_pull::run(&db_pull::PullArgs {
                profile,
                tables,
                with_repository,
                dry_run,
                force,
            }),
            other => {
                let (command, profile) = other.into_command();
                db::run(&command, profile.as_deref());
            }
        },
        Commands::Shard(cmd) => match cmd.command {
            ShardSubcommand::MoveSlot {
                from,
                to,
                table,
                key_column,
                id_column,
                tenants,
                confirm,
                profile,
            } => shard::run_move_slot(&shard::MoveSlotArgs {
                from,
                to,
                table,
                key_column,
                id_column,
                tenants,
                confirm,
                profile,
            }),
        },
        Commands::Maintenance(cmd) => match cmd {
            MaintenanceCommands::On {
                message,
                allow_ips,
                readonly,
                bypass_header,
            } => {
                let parsed_bypass = bypass_header.as_deref().map(|s| {
                    maintenance::parse_bypass_header(s).unwrap_or_else(|e| {
                        eprintln!("autumn maintenance on: {e}");
                        std::process::exit(1);
                    })
                });
                maintenance::run_on(&maintenance::MaintenanceOnOptions {
                    message: message.as_deref(),
                    allow_ips: &allow_ips,
                    readonly,
                    bypass_header: parsed_bypass,
                    flag_file: None,
                });
            }
            MaintenanceCommands::Off => {
                maintenance::run_off(None);
            }
        },
        Commands::Canary(cmd) => match cmd {
            CanaryCommands::Rollback { reason, by } => {
                canary::run_rollback(&canary::RollbackOptions {
                    reason: reason.as_deref(),
                    requested_by: by.as_deref(),
                    flag_file: None,
                });
            }
            CanaryCommands::Promote => canary::run_promote(None),
            CanaryCommands::Status => canary::run_status(None),
        },
        Commands::Monitor { url, interval } => monitor::run(&url, interval),
        Commands::Export { url, output } => export::run(&url, &output),
        Commands::Data(DataCommands::Export {
            model,
            url,
            out,
            search,
            cookie,
        }) => data::run_export(
            &model,
            &url,
            out.as_deref(),
            search.as_deref(),
            cookie.as_deref(),
        ),
        Commands::Data(DataCommands::Import {
            model,
            url,
            input,
            dry_run,
            upsert_by,
            cookie,
        }) => data::run_import(
            &model,
            &url,
            &input,
            dry_run,
            upsert_by.as_deref(),
            cookie.as_deref(),
        ),
        Commands::New {
            name,
            starter,
            starter_ref,
            list_starters,
            yes,
            with_i18n,
            with_seed,
            daemon,
            bundled_pg,
        } => {
            if list_starters {
                starters::print_list();
                return;
            }
            let Some(name) = name else {
                eprintln!(
                    "Error: a project name is required (e.g. `autumn new my-app`), \
                     unless --list-starters is given"
                );
                std::process::exit(1);
            };
            if let Some(starter) = starter {
                // A starter brings a complete composition; the base-project
                // scaffolding toggles do not apply.
                if with_i18n || with_seed || daemon || bundled_pg {
                    eprintln!(
                        "Error: --starter cannot be combined with --with-i18n, \
                         --with-seed, --daemon, or --bundled-pg (a starter brings \
                         its own composition)"
                    );
                    std::process::exit(1);
                }
                starters::run(
                    &name,
                    &starter,
                    starter_ref.as_deref(),
                    yes,
                    generate::Flags::default(),
                );
            } else {
                new::run(
                    &name,
                    new::GenerateOptions {
                        with_i18n,
                        with_seed,
                        // --bundled-pg is a daemon flavor that keeps the database.
                        with_daemon: daemon || bundled_pg,
                        with_bundled_pg: bundled_pg,
                    },
                );
            }
        }

        Commands::Webhook(WebhookCommands::Sim {
            provider,
            url,
            secret,
            payload,
        }) => webhook::run_sim(&provider, &url, &secret, &payload),
        Commands::Seed { profile, package } => seed::run(&profile, package.as_deref()),
        Commands::Task {
            package,
            bin,
            profile,
            list,
            name,
            args,
        } => run_task_command(
            package.as_deref(),
            bin.as_deref(),
            &profile,
            list,
            name.as_deref(),
            &args,
        ),
        Commands::Setup { force } => setup::run(force),
        Commands::Assets { action } => match action {
            AssetsCommands::Add { spec, url } => assets::run_add(&spec, url.as_deref()),
            AssetsCommands::List => assets::run_list(),
            AssetsCommands::Update { name } => assets::run_update(name.as_deref()),
            AssetsCommands::Verify => {
                let manifest_path = std::path::PathBuf::from(assets::VENDOR_MANIFEST_PATH);
                let static_dir = std::path::PathBuf::from("static");
                assets::run_verify(&manifest_path, &static_dir);
            }
        },
        Commands::Routes {
            package,
            bin,
            format,
            prefix,
            filter,
            method,
            user_only,
        } => run_routes_command(
            package.as_deref(),
            bin.as_deref(),
            &format,
            prefix.as_deref(),
            filter.as_deref(),
            &method,
            user_only,
        ),
        Commands::Release(cmd) => run_release_command(cmd),
        Commands::Token(cmd) => match cmd {
            TokenCommands::Issue {
                principal_id,
                name,
                scope,
                expires_at,
            } => token::run_issue(&principal_id, &name, &scope, expires_at.as_deref()),
            TokenCommands::List { principal_id } => token::run_list(&principal_id),
            TokenCommands::Rotate { raw_token } => token::run_rotate(&raw_token),
            TokenCommands::Revoke { raw_token } => token::run_revoke(&raw_token),
        },
        Commands::Check {
            a11y,
            url,
            html,
            critical_only,
            config,
            subcommand,
        } => {
            if let Some(sub) = subcommand {
                match sub {
                    CheckSubcommands::Deprecations { package, bin } => {
                        run_deprecations_check(package.as_deref(), bin.as_deref());
                    }
                }
            } else if config {
                match check::run_config_check() {
                    Ok(()) => {
                        println!(
                            "Configuration check passed: all keys in autumn.toml and profile configurations are valid."
                        );
                    }
                    Err(e) => {
                        eprintln!("Configuration check failed:\n{e}");
                        std::process::exit(1);
                    }
                }
            } else if a11y {
                let opts = check::A11yCheckOptions {
                    url: url.clone(),
                    html,
                };
                let label = url.as_deref().unwrap_or("<inline>");
                match check::run_a11y_check(&opts) {
                    Ok(violations) => {
                        if check::print_report(&violations, label, critical_only) {
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        eprintln!("autumn check --a11y: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!(
                    "autumn check: specify at least one check flag (e.g. --a11y, --config) or a subcommand (e.g. deprecations)"
                );
                std::process::exit(1);
            }
        }
        Commands::Doctor { json, strict } => {
            doctor::run(doctor::DoctorOptions { json, strict });
        }
        Commands::PluginCheck {
            package,
            bin,
            plugin_name,
            prefix,
            sensitive_route,
            format,
        } => {
            run_plugin_check_command(
                package.as_deref(),
                bin.as_deref(),
                &plugin_name,
                prefix.as_deref(),
                &sensitive_route,
                &format,
            );
        }
        Commands::Generate(cmd) => run_generate_command(cmd),
        Commands::Credentials(cmd) => match cmd {
            CredentialsCommands::Edit { env } => {
                if let Err(e) = credentials::run_edit(&credentials::EditOptions { env }) {
                    eprintln!("autumn credentials edit: {e}");
                    std::process::exit(1);
                }
            }
            CredentialsCommands::Show { env, reveal } => {
                credentials::run_show(&credentials::ShowOptions { env, reveal });
            }
        },
        Commands::Config(cmd) => match cmd {
            ConfigCommands::List => config::run_list(&config::ListOptions),
            ConfigCommands::Get { key } => config::run_get(&config::GetOptions { key }),
            ConfigCommands::Set { key, value, actor } => {
                config::run_set(&config::SetOptions { key, value, actor });
            }
            ConfigCommands::Unset { key, actor } => {
                config::run_unset(&config::UnsetOptions { key, actor });
            }
            ConfigCommands::History { key, limit } => {
                config::run_history(&config::HistoryOptions { key, limit });
            }
        },
        Commands::Flags(cmd) => match cmd {
            FlagsCommands::List => flags::run_list(&flags::ListOptions),
            FlagsCommands::Enable { key, actor } => {
                flags::run_enable(&flags::EnableOptions { key, actor });
            }
            FlagsCommands::Disable { key, actor } => {
                flags::run_disable(&flags::DisableOptions { key, actor });
            }
            FlagsCommands::SetRollout { key, pct, actor } => {
                flags::run_set_rollout(&flags::SetRolloutOptions { key, pct, actor });
            }
            FlagsCommands::Allow {
                key,
                actor_id,
                actor,
            } => {
                flags::run_allow(&flags::AllowOptions {
                    key,
                    actor_id,
                    actor,
                });
            }
        },
        Commands::Experiments(cmd) => match cmd {
            ExperimentsCommands::List => experiments::run_list(&experiments::ListOptions),
            ExperimentsCommands::Status { name } => {
                experiments::run_status(&experiments::StatusOptions { name });
            }
            ExperimentsCommands::SetWeights {
                name,
                weights,
                actor,
            } => {
                experiments::run_set_weights(&experiments::SetWeightsOptions {
                    name,
                    weights,
                    actor,
                });
            }
            ExperimentsCommands::Conclude {
                name,
                winner,
                actor,
            } => {
                experiments::run_conclude(&experiments::ConcludeOptions {
                    name,
                    winner,
                    actor,
                });
            }
            ExperimentsCommands::Override {
                name,
                actor_id,
                variant,
                actor,
            } => {
                experiments::run_override(&experiments::OverrideOptions {
                    name,
                    actor_id,
                    variant,
                    actor,
                });
            }
        },
        Commands::DevLoopBench {
            example,
            runs,
            output,
            json,
            fail_on_regression,
            dry_run,
            cold_start,
            include_db,
            scaling,
            sizes,
            baseline,
        } => {
            let exit_code = if scaling {
                scaling_driver::run_scaling(
                    &sizes,
                    runs,
                    output.as_deref(),
                    json,
                    fail_on_regression,
                    dry_run,
                    baseline.as_deref(),
                )
            } else if cold_start {
                cold_start_driver::run_cold_start(
                    runs,
                    output.as_deref(),
                    json,
                    fail_on_regression,
                    dry_run,
                    include_db,
                )
            } else {
                dev_loop_bench::run(
                    &example,
                    runs,
                    output.as_deref(),
                    json,
                    fail_on_regression,
                    dry_run,
                )
            };
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
    }
}

fn run_task_command(
    package: Option<&str>,
    bin: Option<&str>,
    profile: &str,
    list: bool,
    name: Option<&str>,
    args: &[String],
) {
    task::run(&task::TaskOptions {
        package,
        bin,
        profile,
        list,
        name,
        args,
    });
}

fn run_plugin_check_command(
    package: Option<&str>,
    bin: Option<&str>,
    plugin_name: &str,
    prefix: Option<&str>,
    sensitive_route_args: &[String],
    format: &str,
) {
    let fmt = format.parse().unwrap_or_else(|e| {
        eprintln!("autumn plugin-check: {e}");
        std::process::exit(1);
    });

    let mut sensitive_routes: Vec<plugin_check::SensitiveRouteDecl> = Vec::new();
    for arg in sensitive_route_args {
        if let Some((path, desc)) = arg.split_once(':') {
            sensitive_routes.push(plugin_check::SensitiveRouteDecl {
                path_pattern: path.to_owned(),
                auth_mechanism: desc.to_owned(),
            });
        } else {
            eprintln!(
                "autumn plugin-check: invalid --sensitive-route '{arg}'; expected PATH:DESCRIPTION"
            );
            std::process::exit(1);
        }
    }

    plugin_check::run(&plugin_check::PluginCheckOptions {
        package,
        bin,
        plugin_name,
        expected_prefix: prefix,
        sensitive_routes: &sensitive_routes,
        format: fmt,
    });
}

fn run_deprecations_check(package: Option<&str>, bin: Option<&str>) {
    routes::compile_binary(package, bin);
    let binary = routes::find_binary(package, bin);

    let output = std::process::Command::new(&binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping routes",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let routes: Vec<routes::RouteInfo> = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        eprintln!("\u{2717} Failed to parse route listing JSON: {e}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    let mut sunsetted_routes = Vec::new();
    let mut opted_out_routes = Vec::new();
    for route in &routes {
        if route.status.as_deref() == Some("sunset") {
            if route.sunset_opt_out == Some(true) {
                opted_out_routes.push(route);
            } else {
                sunsetted_routes.push(route);
            }
        }
    }

    let failed = !opted_out_routes.is_empty() || !sunsetted_routes.is_empty();

    if !opted_out_routes.is_empty() {
        eprintln!(
            "\u{2717} Found {} active past-sunset route(s) (opted out):",
            opted_out_routes.len()
        );
        for route in &opted_out_routes {
            eprintln!(
                "  {} {} (handler: {}, version: {})",
                route.method,
                route.path,
                route.handler,
                route.api_version.as_deref().unwrap_or("-")
            );
        }
    }

    if !sunsetted_routes.is_empty() {
        eprintln!(
            "\u{2717} Found {} inactive past-sunset route(s) (returning 410 Gone):",
            sunsetted_routes.len()
        );
        for route in &sunsetted_routes {
            eprintln!(
                "  {} {} (handler: {}, version: {})",
                route.method,
                route.path,
                route.handler,
                route.api_version.as_deref().unwrap_or("-")
            );
        }
    }

    if failed {
        std::process::exit(1);
    } else {
        println!("\u{2705} No past-sunset routes detected.");
    }
}

fn run_routes_command(
    package: Option<&str>,
    bin: Option<&str>,
    format: &str,
    prefix: Option<&str>,
    filter: Option<&str>,
    method: &[String],
    user_only: bool,
) {
    let fmt = format.parse().unwrap_or_else(|e| {
        eprintln!("autumn routes: {e}");
        std::process::exit(1);
    });
    // Positional prefix takes precedence over --filter when both are given.
    let effective_filter = prefix.or(filter);
    routes::run(&routes::RoutesOptions {
        package,
        bin,
        format: fmt,
        filter: effective_filter,
        methods: method,
        user_only,
    });
}

fn run_release_command(cmd: ReleaseCommands) {
    match cmd {
        ReleaseCommands::Init { force, target } => {
            let t = target.as_deref().map_or(release::Target::Default, |s| {
                s.parse().unwrap_or_else(|e| {
                    eprintln!("autumn release init: {e}");
                    std::process::exit(1);
                })
            });
            release::run(release::ReleaseAction::Init { force, target: t });
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_generate_command(cmd: GenerateCommands) {
    match cmd {
        GenerateCommands::Model {
            name,
            fields,
            soft_delete,
            id,
            dry_run,
            force,
        } => {
            // Precedence: CLI --id > [generate] id in autumn.generate.toml > BigSerial.
            // The CLI flag is parsed first and wins outright, so a valid --id
            // overrides a stale or invalid project default rather than being
            // blocked by it.
            let id_type = id.as_deref().map_or_else(
                || {
                    // No CLI --id: fall back to the auto-discovered project default.
                    let auto_cfg = std::env::current_dir()
                        .unwrap_or_default()
                        .join(generate::config::GENERATE_CONFIG_FILENAME);
                    if auto_cfg.exists() {
                        generate::config::read_generate_defaults(&auto_cfg).unwrap_or_else(|e| {
                            eprintln!(
                                "Error reading {}: {e}",
                                generate::config::GENERATE_CONFIG_FILENAME
                            );
                            std::process::exit(1);
                        })
                    } else {
                        generate::dsl::IdType::default()
                    }
                },
                |s| {
                    generate::dsl::IdType::parse(s).unwrap_or_else(|e| {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    })
                },
            );
            let options = generate::model::ModelOptions {
                soft_delete,
                id_type,
                ..Default::default()
            };
            let timestamp = generate::timestamp_now();
            match generate::model::plan_model_with_options(
                &std::env::current_dir().unwrap_or_default(),
                &name,
                &fields,
                &timestamp,
                &options,
            )
            .and_then(|p| p.execute(generate::Flags { dry_run, force }))
            {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        GenerateCommands::Migration {
            name,
            fields,
            dry_run,
            force,
        } => generate::migration::run(&name, &fields, generate::Flags { dry_run, force }),
        GenerateCommands::Task {
            name,
            dry_run,
            force,
        } => generate::task::run(&name, generate::Flags { dry_run, force }),
        GenerateCommands::Job {
            name,
            fields,
            dry_run,
            force,
        } => generate::job::run(&name, &fields, generate::Flags { dry_run, force }),
        GenerateCommands::Mailer {
            name,
            list_unsubscribe,
            no_layout,
            dry_run,
            force,
        } => generate::mailer::run(
            &name,
            list_unsubscribe.as_deref(),
            no_layout,
            generate::Flags { dry_run, force },
        ),
        GenerateCommands::Channel {
            name,
            sse: _,
            ws,
            dry_run,
            force,
        } => {
            let transport = if ws {
                generate::channel::Transport::Ws
            } else {
                generate::channel::Transport::Sse
            };
            generate::channel::run(&name, transport, generate::Flags { dry_run, force });
        }
        GenerateCommands::InboundMail {
            name,
            dry_run,
            force,
        } => generate::inbound_mail::run(&name, generate::Flags { dry_run, force }),
        GenerateCommands::SystemTest {
            name,
            dry_run,
            force,
        } => generate::system_test::run(&name, generate::Flags { dry_run, force }),
        GenerateCommands::Pwa { dry_run, force } => {
            generate::pwa::run(generate::Flags { dry_run, force });
        }
        GenerateCommands::Tauri { dry_run, force } => {
            generate::tauri::run(generate::Flags { dry_run, force });
        }
        GenerateCommands::Auth {
            name,
            oauth,
            totp,
            passkeys,
            dry_run,
            force,
        } => {
            let oauth_options = generate::auth::AuthOAuthOptions { providers: oauth };
            generate::auth::run_with_options(
                &name,
                generate::Flags { dry_run, force },
                &oauth_options,
                totp,
                passkeys,
            );
        }
        GenerateCommands::Admin {
            name,
            fields,
            hidden,
            readonly,
            password,
            select,
            exclude,
            dry_run,
            force,
        } => {
            let select_specs = generate::admin::parse_select_specs(&select).unwrap_or_else(|e| {
                eprintln!("autumn generate admin: {e}");
                std::process::exit(1);
            });
            let options = generate::admin::AdminOptions {
                hidden,
                readonly,
                password,
                select: select_specs,
                exclude,
                // Encrypted-column flags are auto-detected from the model source.
                ..Default::default()
            };
            generate::admin::run(&name, &fields, generate::Flags { dry_run, force }, &options);
        }
        GenerateCommands::Wizard {
            name,
            steps,
            dry_run,
            force,
        } => {
            generate::wizard::run(&name, &steps, generate::Flags { dry_run, force });
        }
        GenerateCommands::Scaffold {
            name,
            fields,
            index,
            validate,
            default,
            query,
            config,
            soft_delete,
            id,
            api,
            sharded,
            shard_key,
            live,
            live_validation,
            dry_run,
            force,
        } => {
            // Resolve the scaffold config entry. Precedence for id_type:
            //   CLI --id > [scaffold.X] id > [generate] id > BigSerial.
            //
            // An explicit --config opts into the full per-resource recipe and is
            // treated strictly (a missing [scaffold.X] section is an error unless
            // the file is a pure [generate] defaults file or the fields came from
            // the CLI), preserving typo protection.
            //
            // An auto-discovered autumn.generate.toml contributes ONLY the
            // project-level [generate] defaults — a checked-in [scaffold.X]
            // recipe must not silently change an ordinary CLI scaffold.
            let cli_has_fields = !fields.is_empty();
            let exit_on_err = |result| match result {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            };
            let config_entry = config.as_deref().map_or_else(
                || {
                    let auto = std::env::current_dir()
                        .unwrap_or_default()
                        .join(generate::config::GENERATE_CONFIG_FILENAME);
                    if auto.exists() {
                        exit_on_err(generate::config::read_generate_defaults_entry(&auto))
                    } else {
                        generate::config::ScaffoldConfigEntry::default()
                    }
                },
                |path| {
                    exit_on_err(generate::config::read_explicit_scaffold_config(
                        path,
                        &name,
                        cli_has_fields,
                    ))
                },
            );
            let (fields, options) = match generate::config::merge_config_with_cli(
                config_entry,
                &fields,
                &index,
                &validate,
                &default,
                &query,
                soft_delete,
                api,
                sharded,
                shard_key.as_deref(),
                live,
                id.as_deref(),
                live_validation,
            ) {
                Ok(result) => result,
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            };
            generate::scaffold::run(&name, &fields, generate::Flags { dry_run, force }, &options);
        }
        GenerateCommands::Plugin {
            name,
            path,
            dry_run,
            force,
        } => {
            let cwd = match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Error: cannot determine current directory: {e}");
                    std::process::exit(1);
                }
            };
            match generate::plugin::plan_plugin(
                &cwd,
                &name,
                path.as_deref().map(std::path::Path::new),
                generate::Flags { dry_run, force },
            ) {
                Ok(plugin_plan) => {
                    match plugin_plan.plan.execute(generate::Flags { dry_run, force }) {
                        Ok(()) => {
                            if !dry_run {
                                println!("\nNext steps:");
                                println!(
                                    "  1. Add the plugin to your workspace members in `Cargo.toml`:"
                                );
                                println!("       [workspace]");
                                println!("       members = [");
                                println!("           # ...,");
                                println!("           \"{}\",", plugin_plan.target_dir_relative);
                                println!("       ]");
                                println!(
                                    "  2. Add the dependency to your host app's `Cargo.toml`:"
                                );
                                println!("       [dependencies]");
                                println!(
                                    "       autumn-{}-plugin = {{ path = \"./{}\" }}",
                                    plugin_plan.name_kebab, plugin_plan.target_dir_relative
                                );
                                println!(
                                    "  3. Register the plugin with your host app in `src/main.rs`:"
                                );
                                println!(
                                    "       app.plugin(autumn_{}_plugin::{}::new())",
                                    plugin_plan.name_snake, plugin_plan.struct_name
                                );
                                println!("  4. Run the conformance test to verify:");
                                println!(
                                    "       cargo test -p autumn-{}-plugin\n",
                                    plugin_plan.name_kebab
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("Error: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_subcommand() {
        let cli = Cli::try_parse_from(["autumn", "new", "my-app"]).unwrap();
        match cli.command {
            Commands::New { ref name, .. } => {
                assert_eq!(name.as_deref(), Some("my-app"));
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_with_underscores() {
        let cli = Cli::try_parse_from(["autumn", "new", "my_app"]).unwrap();
        match cli.command {
            Commands::New { ref name, .. } => {
                assert_eq!(name.as_deref(), Some("my_app"));
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_with_i18n_flag() {
        let cli = Cli::try_parse_from(["autumn", "new", "my-app", "--with-i18n"]).unwrap();
        match cli.command {
            Commands::New {
                ref name,
                with_i18n,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("my-app"));
                assert!(with_i18n);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_without_i18n_flag_defaults_off() {
        let cli = Cli::try_parse_from(["autumn", "new", "my-app"]).unwrap();
        match cli.command {
            Commands::New { with_i18n, .. } => assert!(!with_i18n),
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_starter_flag() {
        let cli = Cli::try_parse_from(["autumn", "new", "acme", "--starter", "saas"]).unwrap();
        match cli.command {
            Commands::New {
                ref name,
                ref starter,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("acme"));
                assert_eq!(starter.as_deref(), Some("saas"));
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_list_starters_without_name() {
        // `--list-starters` makes the positional name optional.
        let cli = Cli::try_parse_from(["autumn", "new", "--list-starters"]).unwrap();
        match cli.command {
            Commands::New {
                name,
                list_starters,
                ..
            } => {
                assert!(name.is_none());
                assert!(list_starters);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_starter_ref_and_yes() {
        let cli = Cli::try_parse_from([
            "autumn",
            "new",
            "acme",
            "--starter",
            "owner/repo",
            "--starter-ref",
            "v1.2.0",
            "--yes",
        ])
        .unwrap();
        match cli.command {
            Commands::New {
                starter,
                starter_ref,
                yes,
                ..
            } => {
                assert_eq!(starter.as_deref(), Some("owner/repo"));
                assert_eq!(starter_ref.as_deref(), Some("v1.2.0"));
                assert!(yes);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_without_starter_defaults_none() {
        let cli = Cli::try_parse_from(["autumn", "new", "acme"]).unwrap();
        match cli.command {
            Commands::New {
                starter,
                list_starters,
                yes,
                ..
            } => {
                assert!(starter.is_none());
                assert!(!list_starters);
                assert!(!yes);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_setup_subcommand() {
        let cli = Cli::try_parse_from(["autumn", "setup"]).unwrap();
        assert!(matches!(cli.command, Commands::Setup { force: false }));
    }

    #[test]
    fn parse_setup_with_force() {
        let cli = Cli::try_parse_from(["autumn", "setup", "--force"]).unwrap();
        assert!(matches!(cli.command, Commands::Setup { force: true }));
    }

    #[test]
    fn new_rejects_removed_wasm_flag() {
        assert!(Cli::try_parse_from(["autumn", "new", "my-app", "--wasm"]).is_err());
    }

    #[test]
    fn setup_rejects_removed_wasm_flag() {
        assert!(Cli::try_parse_from(["autumn", "setup", "--wasm"]).is_err());
    }

    #[test]
    fn no_args_is_error() {
        assert!(Cli::try_parse_from(["autumn"]).is_err());
    }

    #[test]
    fn parse_generate_auth_totp_flag() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User", "--totp"]).unwrap();
        match cli.command {
            Commands::Generate(GenerateCommands::Auth { name, totp, .. }) => {
                assert_eq!(name, "User");
                assert!(totp, "--totp must set the totp flag");
            }
            _ => panic!("expected Generate Auth command"),
        }
    }

    #[test]
    fn generate_auth_totp_defaults_off() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User"]).unwrap();
        match cli.command {
            Commands::Generate(GenerateCommands::Auth { totp, .. }) => {
                assert!(!totp, "totp must default to off");
            }
            _ => panic!("expected Generate Auth command"),
        }
    }

    #[test]
    fn new_without_name_parses_with_name_none() {
        // The positional name is optional at the clap level so `--list-starters`
        // can run without one. When neither a name nor `--list-starters` is
        // given, the requirement is enforced at dispatch (a clean runtime error),
        // not by the parser.
        let cli = Cli::try_parse_from(["autumn", "new"]).unwrap();
        match cli.command {
            Commands::New {
                name,
                list_starters,
                ..
            } => {
                assert!(name.is_none());
                assert!(!list_starters);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_build_subcommand() {
        let cli = Cli::try_parse_from(["autumn", "build"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Build {
                debug: false,
                package: None,
                bin: None,
                embed: false,
                features: None,
            }
        ));
    }

    #[test]
    fn parse_build_debug() {
        let cli = Cli::try_parse_from(["autumn", "build", "--debug"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Build {
                debug: true,
                package: None,
                bin: None,
                embed: false,
                features: None,
            }
        ));
    }

    #[test]
    fn parse_build_with_package() {
        let cli = Cli::try_parse_from(["autumn", "build", "-p", "blog"]).unwrap();
        match cli.command {
            Commands::Build {
                debug,
                package,
                bin,
                embed,
                ..
            } => {
                assert!(!debug);
                assert!(!embed);
                assert!(bin.is_none());
                assert_eq!(package.as_deref(), Some("blog"));
            }
            _ => panic!("expected Build command"),
        }
    }

    #[test]
    fn parse_build_with_bin() {
        let cli = Cli::try_parse_from(["autumn", "build", "--embed", "--bin", "server"]).unwrap();
        match cli.command {
            Commands::Build { embed, bin, .. } => {
                assert!(embed);
                assert_eq!(bin.as_deref(), Some("server"));
            }
            _ => panic!("expected Build command"),
        }
    }

    #[test]
    fn parse_build_with_embed() {
        let cli = Cli::try_parse_from(["autumn", "build", "--embed"]).unwrap();
        match cli.command {
            Commands::Build { embed, debug, .. } => {
                assert!(embed, "--embed must set the embed flag");
                assert!(!debug);
            }
            _ => panic!("expected Build command"),
        }
    }

    #[test]
    fn parse_build_with_long_package() {
        let cli = Cli::try_parse_from(["autumn", "build", "--package", "blog", "--debug"]).unwrap();
        match cli.command {
            Commands::Build { debug, package, .. } => {
                assert!(debug);
                assert_eq!(package.as_deref(), Some("blog"));
            }
            _ => panic!("expected Build command"),
        }
    }

    #[test]
    fn parse_build_with_features() {
        let cli = Cli::try_parse_from([
            "autumn",
            "build",
            "--embed",
            "--features",
            "autumn-web/managed-pg-bundled",
        ])
        .unwrap();
        match cli.command {
            Commands::Build {
                embed, features, ..
            } => {
                assert!(embed);
                assert_eq!(
                    features.as_deref(),
                    Some("autumn-web/managed-pg-bundled"),
                    "--features must be captured"
                );
            }
            _ => panic!("expected Build command"),
        }
    }

    #[test]
    fn parse_dev_subcommand() {
        let cli = Cli::try_parse_from(["autumn", "dev"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Dev {
                package: None,
                show_config: false
            }
        ));
    }

    #[test]
    fn serve_parses_daemon_flag() {
        let cli = Cli::try_parse_from(["autumn", "serve", "--daemon"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Serve {
                action: None,
                daemon: true,
                bundled_pg: false,
                ..
            }
        ));
    }

    #[test]
    fn serve_stop_subcommand_parses() {
        let cli = Cli::try_parse_from(["autumn", "serve", "stop"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Serve {
                action: Some(ServeCommands::Stop),
                ..
            }
        ));
    }

    #[test]
    fn serve_status_parses() {
        let cli = Cli::try_parse_from(["autumn", "serve", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Serve {
                action: Some(ServeCommands::Status),
                ..
            }
        ));
    }

    #[test]
    fn serve_restart_parses() {
        let cli = Cli::try_parse_from(["autumn", "serve", "restart"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Serve {
                action: Some(ServeCommands::Restart),
                ..
            }
        ));
    }

    #[test]
    fn serve_parses_bundled_pg_and_release() {
        let cli = Cli::try_parse_from(["autumn", "serve", "--bundled-pg", "--release"]).unwrap();
        match cli.command {
            Commands::Serve {
                bundled_pg,
                release,
                ..
            } => {
                assert!(bundled_pg);
                assert!(release);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn parse_dev_with_package() {
        let cli = Cli::try_parse_from(["autumn", "dev", "-p", "hello"]).unwrap();
        match cli.command {
            Commands::Dev {
                package,
                show_config,
            } => {
                assert_eq!(package.as_deref(), Some("hello"));
                assert!(!show_config);
            }
            _ => panic!("expected Dev command"),
        }
    }

    #[test]
    fn parse_dev_with_show_config() {
        let cli = Cli::try_parse_from(["autumn", "dev", "--show-config"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Dev {
                package: None,
                show_config: true
            }
        ));
    }

    #[test]
    fn parse_migrate_subcommand() {
        let cli = Cli::try_parse_from(["autumn", "migrate"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Migrate { action: None, .. }
        ));
    }

    #[test]
    fn parse_migrate_status() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Migrate {
                action: Some(MigrateCommands::Status),
                ..
            }
        ));
    }

    #[test]
    fn parse_migrate_check() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Migrate {
                action: Some(MigrateCommands::Check),
                ..
            }
        ));
    }

    #[test]
    fn parse_migrate_no_subcommand_runs_migrations() {
        let cli = Cli::try_parse_from(["autumn", "migrate"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Migrate { action: None, .. }
        ));
    }

    #[test]
    fn parse_monitor_defaults() {
        let cli = Cli::try_parse_from(["autumn", "monitor"]).unwrap();
        match cli.command {
            Commands::Monitor { url, interval } => {
                assert_eq!(url, "http://localhost:3000");
                assert_eq!(interval, 1);
            }
            _ => panic!("expected Monitor command"),
        }
    }

    #[test]
    fn parse_monitor_custom_url() {
        let cli = Cli::try_parse_from(["autumn", "monitor", "-u", "http://prod:8080", "-i", "5"])
            .unwrap();
        match cli.command {
            Commands::Monitor { url, interval } => {
                assert_eq!(url, "http://prod:8080");
                assert_eq!(interval, 5);
            }
            _ => panic!("expected Monitor command"),
        }
    }

    #[test]
    fn parse_export_defaults() {
        let cli = Cli::try_parse_from(["autumn", "export"]).unwrap();
        match cli.command {
            Commands::Export { url, output } => {
                assert_eq!(url, "http://localhost:3000");
                assert_eq!(output, "autumn-diag.json");
            }
            _ => panic!("expected Export command"),
        }
    }

    #[test]
    fn parse_export_custom() {
        let cli = Cli::try_parse_from([
            "autumn",
            "export",
            "-u",
            "http://prod:8080",
            "-o",
            "snapshot.json",
        ])
        .unwrap();
        match cli.command {
            Commands::Export { url, output } => {
                assert_eq!(url, "http://prod:8080");
                assert_eq!(output, "snapshot.json");
            }
            _ => panic!("expected Export command"),
        }
    }

    #[test]
    fn unknown_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "bogus"]).is_err());
    }

    #[test]
    fn parse_generate_model() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "model",
            "Post",
            "title:String",
            "body:Text",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Model {
            name,
            fields,
            dry_run,
            force,
            ..
        }) = cli.command
        else {
            panic!("expected generate model");
        };
        assert_eq!(name, "Post");
        assert_eq!(fields, vec!["title:String", "body:Text"]);
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_model_with_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "model",
            "Post",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Model { dry_run, force, .. }) = cli.command else {
            panic!("expected generate model");
        };
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_migration() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "migration",
            "AddTitleToPosts",
            "title:String",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Migration { name, fields, .. }) = cli.command
        else {
            panic!("expected generate migration");
        };
        assert_eq!(name, "AddTitleToPosts");
        assert_eq!(fields, vec!["title:String"]);
    }

    #[test]
    fn parse_generate_task() {
        let cli = Cli::try_parse_from(["autumn", "generate", "task", "cleanup_users", "--dry-run"])
            .unwrap();
        let Commands::Generate(GenerateCommands::Task {
            name,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected generate task");
        };
        assert_eq!(name, "cleanup_users");
        assert!(dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_job_basic() {
        let cli = Cli::try_parse_from(["autumn", "generate", "job", "SendWelcomeEmail"]).unwrap();
        let Commands::Generate(GenerateCommands::Job {
            name,
            fields,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected generate job");
        };
        assert_eq!(name, "SendWelcomeEmail");
        assert!(fields.is_empty());
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_job_with_fields() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "job",
            "SendWelcomeEmail",
            "user_id:i64",
            "email:String",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Job { name, fields, .. }) = cli.command else {
            panic!("expected generate job");
        };
        assert_eq!(name, "SendWelcomeEmail");
        assert_eq!(fields, vec!["user_id:i64", "email:String"]);
    }

    #[test]
    fn parse_generate_job_with_dry_run_and_force() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "job",
            "SendWelcomeEmail",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Job { dry_run, force, .. }) = cli.command else {
            panic!("expected generate job");
        };
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_scaffold() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold { name, fields, .. }) = cli.command
        else {
            panic!("expected generate scaffold");
        };
        assert_eq!(name, "Post");
        assert_eq!(fields.len(), 3);
    }

    #[test]
    fn parse_generate_scaffold_metadata_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "scaffold",
            "Bookmark",
            "url:String",
            "alive:bool",
            "--index",
            "url",
            "--validate",
            "url=url",
            "--default",
            "alive=true",
            "--query",
            "find_by_alive:alive",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold {
            index,
            validate,
            default,
            query,
            ..
        }) = cli.command
        else {
            panic!("expected generate scaffold");
        };
        assert_eq!(index, vec!["url"]);
        assert_eq!(validate, vec!["url=url"]);
        assert_eq!(default, vec!["alive=true"]);
        assert_eq!(query, vec!["find_by_alive:alive"]);
    }

    #[test]
    fn parse_generate_scaffold_config_flag() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "scaffold",
            "Post",
            "--config",
            "autumn.generate.toml",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold { config, .. }) = cli.command else {
            panic!("expected generate scaffold");
        };
        assert_eq!(
            config,
            Some(std::path::PathBuf::from("autumn.generate.toml"))
        );
    }

    #[test]
    fn parse_generate_without_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "generate"]).is_err());
    }

    #[test]
    fn parse_generate_auth_with_user_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth {
            name,
            dry_run,
            force,
            ..
        }) = cli.command
        else {
            panic!("expected generate auth");
        };
        assert_eq!(name, "User");
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_auth_with_dry_run() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User", "--dry-run"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth { dry_run, force, .. }) = cli.command else {
            panic!("expected generate auth");
        };
        assert!(dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_auth_with_force() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User", "--force"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth { dry_run, force, .. }) = cli.command else {
            panic!("expected generate auth");
        };
        assert!(!dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_auth_snake_case_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "account"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth { name, .. }) = cli.command else {
            panic!("expected generate auth");
        };
        assert_eq!(name, "account");
    }

    #[test]
    fn parse_generate_auth_without_name_is_error() {
        assert!(Cli::try_parse_from(["autumn", "generate", "auth"]).is_err());
    }

    #[test]
    fn parse_generate_model_without_name_is_error() {
        assert!(Cli::try_parse_from(["autumn", "generate", "model"]).is_err());
    }

    // ── autumn db tests ────────────────────────────────────────────────────

    #[test]
    fn parse_db_create() {
        let cli = Cli::try_parse_from(["autumn", "db", "create"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Db(DbCommands::Create { profile: None })
        ));
    }

    #[test]
    fn parse_db_create_with_profile() {
        let cli = Cli::try_parse_from(["autumn", "db", "create", "--profile", "test"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Db(DbCommands::Create { profile: Some(p) }) if p == "test"
        ));
    }

    #[test]
    fn parse_db_drop_defaults_force_false() {
        let cli = Cli::try_parse_from(["autumn", "db", "drop"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Db(DbCommands::Drop {
                profile: None,
                force: false
            })
        ));
    }

    #[test]
    fn parse_db_drop_with_force() {
        let cli = Cli::try_parse_from(["autumn", "db", "drop", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Db(DbCommands::Drop { force: true, .. })
        ));
    }

    #[test]
    fn parse_db_reset_with_profile_and_force() {
        let cli =
            Cli::try_parse_from(["autumn", "db", "reset", "--profile", "prod", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Db(DbCommands::Reset {
                profile: Some(p),
                force: true
            }) if p == "prod"
        ));
    }

    #[test]
    fn parse_db_without_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "db"]).is_err());
    }

    #[test]
    fn db_into_command_maps_every_variant() {
        assert!(matches!(
            DbCommands::Create { profile: None }.into_command(),
            (db::DbCommand::Create, None)
        ));
        assert!(matches!(
            DbCommands::Drop {
                profile: Some("dev".to_owned()),
                force: true,
            }
            .into_command(),
            (db::DbCommand::Drop { force: true }, Some(p)) if p == "dev"
        ));
        assert!(matches!(
            DbCommands::Reset {
                profile: None,
                force: false,
            }
            .into_command(),
            (db::DbCommand::Reset { force: false }, None)
        ));
    }

    #[test]
    fn parse_db_pull_defaults() {
        let cli = Cli::try_parse_from(["autumn", "db", "pull"]).unwrap();
        let Commands::Db(DbCommands::Pull {
            tables,
            profile,
            with_repository,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected db pull");
        };
        assert!(tables.is_empty());
        assert!(profile.is_none());
        assert!(!with_repository);
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_db_pull_with_tables_and_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "db",
            "pull",
            "posts",
            "comments",
            "--with-repository",
            "--dry-run",
            "--force",
            "--profile",
            "test",
        ])
        .unwrap();
        let Commands::Db(DbCommands::Pull {
            tables,
            profile,
            with_repository,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected db pull");
        };
        assert_eq!(tables, vec!["posts".to_owned(), "comments".to_owned()]);
        assert_eq!(profile.as_deref(), Some("test"));
        assert!(with_repository);
        assert!(dry_run);
        assert!(force);
    }

    // ── autumn seed tests ──────────────────────────────────────────────────

    #[test]
    fn parse_seed_defaults() {
        let cli = Cli::try_parse_from(["autumn", "seed"]).unwrap();
        match cli.command {
            Commands::Seed { profile, package } => {
                assert_eq!(profile, "dev");
                assert!(package.is_none());
            }
            _ => panic!("expected Seed command"),
        }
    }

    #[test]
    fn parse_seed_with_profile() {
        let cli = Cli::try_parse_from(["autumn", "seed", "--profile", "demo"]).unwrap();
        match cli.command {
            Commands::Seed { profile, .. } => {
                assert_eq!(profile, "demo");
            }
            _ => panic!("expected Seed command"),
        }
    }

    #[test]
    fn parse_seed_with_package() {
        let cli = Cli::try_parse_from(["autumn", "seed", "-p", "my-app"]).unwrap();
        match cli.command {
            Commands::Seed { package, .. } => {
                assert_eq!(package.as_deref(), Some("my-app"));
            }
            _ => panic!("expected Seed command"),
        }
    }

    #[test]
    fn parse_seed_test_profile() {
        let cli = Cli::try_parse_from(["autumn", "seed", "--profile", "test"]).unwrap();
        match cli.command {
            Commands::Seed { profile, .. } => assert_eq!(profile, "test"),
            _ => panic!("expected Seed command"),
        }
    }

    #[test]
    fn parse_seed_prod_profile() {
        let cli = Cli::try_parse_from(["autumn", "seed", "--profile", "prod"]).unwrap();
        match cli.command {
            Commands::Seed { profile, .. } => assert_eq!(profile, "prod"),
            _ => panic!("expected Seed command"),
        }
    }

    // ── autumn routes tests ────────────────────────────────────────────────

    #[test]
    fn parse_task_run_with_cli_args() {
        let cli =
            Cli::try_parse_from(["autumn", "task", "cleanup-user", "--user-id", "42"]).unwrap();
        match cli.command {
            Commands::Task {
                name,
                args,
                list,
                profile,
                package,
                bin,
            } => {
                assert_eq!(name.as_deref(), Some("cleanup-user"));
                assert_eq!(args, vec!["--user-id", "42"]);
                assert!(!list);
                assert_eq!(profile, "dev");
                assert!(package.is_none());
                assert!(bin.is_none());
            }
            _ => panic!("expected Task command"),
        }
    }

    #[test]
    fn parse_task_list_with_package_and_bin() {
        let cli = Cli::try_parse_from([
            "autumn",
            "task",
            "--list",
            "--package",
            "blog",
            "--bin",
            "blog",
        ])
        .unwrap();
        match cli.command {
            Commands::Task {
                name,
                list,
                package,
                bin,
                ..
            } => {
                assert!(name.is_none());
                assert!(list);
                assert_eq!(package.as_deref(), Some("blog"));
                assert_eq!(bin.as_deref(), Some("blog"));
            }
            _ => panic!("expected Task command"),
        }
    }

    #[test]
    fn parse_task_with_profile() {
        let cli =
            Cli::try_parse_from(["autumn", "task", "--profile", "prod", "cleanup-user"]).unwrap();
        match cli.command {
            Commands::Task { profile, name, .. } => {
                assert_eq!(profile, "prod");
                assert_eq!(name.as_deref(), Some("cleanup-user"));
            }
            _ => panic!("expected Task command"),
        }
    }

    #[test]
    fn parse_routes_defaults() {
        let cli = Cli::try_parse_from(["autumn", "routes"]).unwrap();
        match cli.command {
            Commands::Routes {
                package,
                bin,
                format,
                prefix,
                filter,
                method,
                user_only,
            } => {
                assert!(package.is_none());
                assert!(bin.is_none());
                assert_eq!(format, "table");
                assert!(prefix.is_none());
                assert!(filter.is_none());
                assert!(method.is_empty());
                assert!(!user_only);
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_with_package() {
        let cli = Cli::try_parse_from(["autumn", "routes", "-p", "blog"]).unwrap();
        match cli.command {
            Commands::Routes { package, .. } => {
                assert_eq!(package.as_deref(), Some("blog"));
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_with_long_package() {
        let cli = Cli::try_parse_from(["autumn", "routes", "--package", "my-app"]).unwrap();
        match cli.command {
            Commands::Routes { package, .. } => {
                assert_eq!(package.as_deref(), Some("my-app"));
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_format_json() {
        let cli = Cli::try_parse_from(["autumn", "routes", "--format", "json"]).unwrap();
        match cli.command {
            Commands::Routes { format, .. } => {
                assert_eq!(format, "json");
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_with_filter() {
        let cli = Cli::try_parse_from(["autumn", "routes", "--filter", "/api"]).unwrap();
        match cli.command {
            Commands::Routes { filter, .. } => {
                assert_eq!(filter.as_deref(), Some("/api"));
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_with_method() {
        let cli = Cli::try_parse_from(["autumn", "routes", "--method", "GET"]).unwrap();
        match cli.command {
            Commands::Routes { method, .. } => {
                assert_eq!(method, vec!["GET"]);
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_with_multiple_methods() {
        let cli = Cli::try_parse_from(["autumn", "routes", "--method", "GET,POST"]).unwrap();
        match cli.command {
            Commands::Routes { method, .. } => {
                assert_eq!(method, vec!["GET", "POST"]);
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_with_user_only() {
        let cli = Cli::try_parse_from(["autumn", "routes", "--user-only"]).unwrap();
        match cli.command {
            Commands::Routes { user_only, .. } => {
                assert!(user_only);
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_with_bin() {
        let cli = Cli::try_parse_from(["autumn", "routes", "--bin", "server"]).unwrap();
        match cli.command {
            Commands::Routes { bin, .. } => {
                assert_eq!(bin.as_deref(), Some("server"));
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_all_options() {
        let cli = Cli::try_parse_from([
            "autumn",
            "routes",
            "-p",
            "blog",
            "--format",
            "json",
            "--filter",
            "/api",
            "--method",
            "GET,POST",
            "--user-only",
        ])
        .unwrap();
        match cli.command {
            Commands::Routes {
                package,
                bin,
                format,
                prefix,
                filter,
                method,
                user_only,
            } => {
                assert_eq!(package.as_deref(), Some("blog"));
                assert!(bin.is_none());
                assert_eq!(format, "json");
                assert!(prefix.is_none());
                assert_eq!(filter.as_deref(), Some("/api"));
                assert_eq!(method, vec!["GET", "POST"]);
                assert!(user_only);
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_positional_prefix() {
        let cli = Cli::try_parse_from(["autumn", "routes", "/api"]).unwrap();
        match cli.command {
            Commands::Routes { prefix, filter, .. } => {
                assert_eq!(prefix.as_deref(), Some("/api"));
                assert!(filter.is_none());
            }
            _ => panic!("expected Routes command"),
        }
    }

    #[test]
    fn parse_routes_positional_prefix_with_package() {
        let cli = Cli::try_parse_from(["autumn", "routes", "-p", "blog", "/api"]).unwrap();
        match cli.command {
            Commands::Routes {
                package, prefix, ..
            } => {
                assert_eq!(package.as_deref(), Some("blog"));
                assert_eq!(prefix.as_deref(), Some("/api"));
            }
            _ => panic!("expected Routes command"),
        }
    }

    // ── autumn doctor tests ────────────────────────────────────────────────

    #[test]
    fn parse_doctor_defaults() {
        let cli = Cli::try_parse_from(["autumn", "doctor"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Doctor {
                json: false,
                strict: false
            }
        ));
    }

    #[test]
    fn parse_doctor_json_flag() {
        let cli = Cli::try_parse_from(["autumn", "doctor", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Doctor {
                json: true,
                strict: false
            }
        ));
    }

    #[test]
    fn parse_doctor_strict_flag() {
        let cli = Cli::try_parse_from(["autumn", "doctor", "--strict"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Doctor {
                json: false,
                strict: true
            }
        ));
    }

    #[test]
    fn parse_doctor_json_and_strict() {
        let cli = Cli::try_parse_from(["autumn", "doctor", "--json", "--strict"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Doctor {
                json: true,
                strict: true
            }
        ));
    }

    // ── autumn release tests ───────────────────────────────────────────────

    #[test]
    fn parse_release_init_defaults() {
        let cli = Cli::try_parse_from(["autumn", "release", "init"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target }) = cli.command else {
            panic!("expected release init");
        };
        assert!(!force);
        assert!(target.is_none());
    }

    #[test]
    fn parse_release_init_with_force() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--force"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target }) = cli.command else {
            panic!("expected release init");
        };
        assert!(force);
        assert!(target.is_none());
    }

    #[test]
    fn parse_release_init_with_fly_target() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--target", "fly"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target }) = cli.command else {
            panic!("expected release init");
        };
        assert!(!force);
        assert_eq!(target.as_deref(), Some("fly"));
    }

    #[test]
    fn parse_release_init_with_docker_compose_target() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--target", "docker-compose"])
            .unwrap();
        let Commands::Release(ReleaseCommands::Init { target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert_eq!(target.as_deref(), Some("docker-compose"));
    }

    #[test]
    fn parse_release_init_force_and_target() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--force", "--target", "fly"])
            .unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target }) = cli.command else {
            panic!("expected release init");
        };
        assert!(force);
        assert_eq!(target.as_deref(), Some("fly"));
    }

    #[test]
    fn parse_release_without_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "release"]).is_err());
    }

    // ── autumn new --with-seed tests ───────────────────────────────────────

    #[test]
    fn parse_new_without_with_seed_defaults_false() {
        let cli = Cli::try_parse_from(["autumn", "new", "my-app"]).unwrap();
        match cli.command {
            Commands::New {
                name, with_seed, ..
            } => {
                assert_eq!(name.as_deref(), Some("my-app"));
                assert!(!with_seed);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_with_with_seed_flag() {
        let cli = Cli::try_parse_from(["autumn", "new", "my-app", "--with-seed"]).unwrap();
        match cli.command {
            Commands::New {
                name, with_seed, ..
            } => {
                assert_eq!(name.as_deref(), Some("my-app"));
                assert!(with_seed);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_with_i18n_and_seed_flags() {
        let cli =
            Cli::try_parse_from(["autumn", "new", "my-app", "--with-i18n", "--with-seed"]).unwrap();
        match cli.command {
            Commands::New {
                name,
                with_i18n,
                with_seed,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("my-app"));
                assert!(with_i18n);
                assert!(with_seed);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_daemon_flag() {
        let cli = Cli::try_parse_from(["autumn", "new", "svc", "--daemon"]).unwrap();
        match cli.command {
            Commands::New {
                daemon, bundled_pg, ..
            } => {
                assert!(daemon);
                assert!(!bundled_pg);
            }
            _ => panic!("expected New command"),
        }
    }

    #[test]
    fn parse_new_bundled_pg_flag() {
        let cli = Cli::try_parse_from(["autumn", "new", "svc", "--bundled-pg"]).unwrap();
        match cli.command {
            Commands::New { bundled_pg, .. } => assert!(bundled_pg),
            _ => panic!("expected New command"),
        }
    }

    // ── autumn token tests ─────────────────────────────────────────────────

    #[test]
    fn parse_token_issue() {
        let cli = Cli::try_parse_from([
            "autumn",
            "token",
            "issue",
            "user:42",
            "--name",
            "ci",
            "--scope",
            "posts:read",
            "--scope",
            "posts:write",
        ])
        .unwrap();
        let Commands::Token(TokenCommands::Issue {
            principal_id,
            name,
            scope,
            expires_at,
        }) = cli.command
        else {
            panic!("expected token issue");
        };
        assert_eq!(principal_id, "user:42");
        assert_eq!(name, "ci");
        assert_eq!(scope, vec!["posts:read", "posts:write"]);
        assert!(expires_at.is_none());
    }

    #[test]
    fn parse_token_list() {
        let cli = Cli::try_parse_from(["autumn", "token", "list", "service:ci"]).unwrap();
        let Commands::Token(TokenCommands::List { principal_id }) = cli.command else {
            panic!("expected token list");
        };
        assert_eq!(principal_id, "service:ci");
    }

    #[test]
    fn parse_token_rotate() {
        let cli = Cli::try_parse_from(["autumn", "token", "rotate", "abc123"]).unwrap();
        let Commands::Token(TokenCommands::Rotate { raw_token }) = cli.command else {
            panic!("expected token rotate");
        };
        assert_eq!(raw_token, "abc123");
    }

    #[test]
    fn parse_token_revoke() {
        let cli = Cli::try_parse_from(["autumn", "token", "revoke", "abc123deadbeef"]).unwrap();
        let Commands::Token(TokenCommands::Revoke { raw_token }) = cli.command else {
            panic!("expected token revoke");
        };
        assert_eq!(raw_token, "abc123deadbeef");
    }

    #[test]
    fn parse_token_without_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "token"]).is_err());
    }

    #[test]
    fn parse_token_issue_without_principal_is_error() {
        assert!(Cli::try_parse_from(["autumn", "token", "issue"]).is_err());
    }

    #[test]
    fn parse_token_revoke_without_token_is_error() {
        assert!(Cli::try_parse_from(["autumn", "token", "revoke"]).is_err());
    }

    // ── autumn plugin-check tests ──────────────────────────────────────────

    #[test]
    fn parse_plugin_check_required_plugin_name() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "--plugin-name",
            "autumn-admin-plugin",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck { plugin_name, .. } => {
                assert_eq!(plugin_name, "autumn-admin-plugin");
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_missing_plugin_name_is_error() {
        assert!(Cli::try_parse_from(["autumn", "plugin-check"]).is_err());
    }

    #[test]
    fn parse_plugin_check_with_prefix() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "--plugin-name",
            "autumn-admin-plugin",
            "--prefix",
            "/admin",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck { prefix, .. } => {
                assert_eq!(prefix.as_deref(), Some("/admin"));
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_with_package() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "-p",
            "my-app",
            "--plugin-name",
            "myplugin",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck { package, .. } => {
                assert_eq!(package.as_deref(), Some("my-app"));
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_with_json_format() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "--plugin-name",
            "myplugin",
            "--format",
            "json",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck { format, .. } => {
                assert_eq!(format, "json");
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_default_format_is_text() {
        let cli =
            Cli::try_parse_from(["autumn", "plugin-check", "--plugin-name", "myplugin"]).unwrap();
        match cli.command {
            Commands::PluginCheck { format, .. } => {
                assert_eq!(format, "text");
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_with_sensitive_route() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "--plugin-name",
            "myplugin",
            "--sensitive-route",
            "/admin:Role admin required",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck {
                sensitive_route, ..
            } => {
                assert_eq!(sensitive_route, vec!["/admin:Role admin required"]);
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_multiple_sensitive_routes() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "--plugin-name",
            "myplugin",
            "--sensitive-route",
            "/admin:Role admin required",
            "--sensitive-route",
            "/debug:Internal use only",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck {
                sensitive_route, ..
            } => {
                assert_eq!(sensitive_route.len(), 2);
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_with_bin() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "--plugin-name",
            "myplugin",
            "--bin",
            "server",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck { bin, .. } => {
                assert_eq!(bin.as_deref(), Some("server"));
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    #[test]
    fn parse_plugin_check_all_options() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "-p",
            "my-app",
            "--bin",
            "server",
            "--plugin-name",
            "autumn-admin-plugin",
            "--prefix",
            "/admin",
            "--sensitive-route",
            "/admin:Role: admin required",
            "--format",
            "json",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck {
                package,
                bin,
                plugin_name,
                prefix,
                sensitive_route,
                format,
            } => {
                assert_eq!(package.as_deref(), Some("my-app"));
                assert_eq!(bin.as_deref(), Some("server"));
                assert_eq!(plugin_name, "autumn-admin-plugin");
                assert_eq!(prefix.as_deref(), Some("/admin"));
                assert_eq!(sensitive_route, vec!["/admin:Role: admin required"]);
                assert_eq!(format, "json");
            }
            _ => panic!("expected PluginCheck"),
        }
    }

    // ── autumn generate admin tests ────────────────────────────────────────

    #[test]
    fn parse_generate_admin_with_model_name_and_fields() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "admin",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Admin {
            name,
            fields,
            hidden,
            readonly,
            password,
            select,
            exclude,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected generate admin");
        };
        assert_eq!(name, "Post");
        assert_eq!(fields, vec!["title:String", "body:Text", "published:bool"]);
        assert!(hidden.is_empty());
        assert!(readonly.is_empty());
        assert!(password.is_empty());
        assert!(select.is_empty());
        assert!(exclude.is_empty());
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_admin_with_dry_run_and_force() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "admin",
            "Post",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Admin { dry_run, force, .. }) = cli.command else {
            panic!("expected generate admin");
        };
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_admin_with_option_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "admin",
            "User",
            "email:String",
            "password_hash:String",
            "--hidden",
            "password_hash",
            "--readonly",
            "email",
            "--exclude",
            "password_hash",
            "--password",
            "raw_password",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Admin {
            hidden,
            readonly,
            exclude,
            password,
            ..
        }) = cli.command
        else {
            panic!("expected generate admin");
        };
        assert_eq!(hidden, vec!["password_hash"]);
        assert_eq!(readonly, vec!["email"]);
        assert_eq!(exclude, vec!["password_hash"]);
        assert_eq!(password, vec!["raw_password"]);
    }

    #[test]
    fn parse_generate_admin_snake_case_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "admin", "blog_post"]).unwrap();
        let Commands::Generate(GenerateCommands::Admin { name, .. }) = cli.command else {
            panic!("expected generate admin");
        };
        assert_eq!(name, "blog_post");
    }

    #[test]
    fn parse_generate_admin_with_select_flag() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "admin",
            "Post",
            "status:String",
            "--select",
            "status=draft,published,archived",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Admin { select, .. }) = cli.command else {
            panic!("expected generate admin");
        };
        assert_eq!(select, vec!["status=draft,published,archived"]);
    }

    #[test]
    fn parse_generate_admin_without_name_is_error() {
        assert!(Cli::try_parse_from(["autumn", "generate", "admin"]).is_err());
    }

    // ── autumn credentials tests ───────────────────────────────────────────

    #[test]
    fn parse_credentials_edit_defaults() {
        let cli = Cli::try_parse_from(["autumn", "credentials", "edit"]).unwrap();
        let Commands::Credentials(CredentialsCommands::Edit { env }) = cli.command else {
            panic!("expected credentials edit");
        };
        assert_eq!(env, "development", "default env should be 'development'");
    }

    #[test]
    fn parse_credentials_edit_with_env_flag() {
        let cli =
            Cli::try_parse_from(["autumn", "credentials", "edit", "--env", "production"]).unwrap();
        let Commands::Credentials(CredentialsCommands::Edit { env }) = cli.command else {
            panic!("expected credentials edit");
        };
        assert_eq!(env, "production");
    }

    #[test]
    fn parse_credentials_show_defaults() {
        let cli = Cli::try_parse_from(["autumn", "credentials", "show"]).unwrap();
        let Commands::Credentials(CredentialsCommands::Show { env, reveal }) = cli.command else {
            panic!("expected credentials show");
        };
        assert_eq!(env, "development");
        assert!(!reveal, "reveal should default to false");
    }

    #[test]
    fn parse_credentials_show_with_reveal() {
        let cli = Cli::try_parse_from(["autumn", "credentials", "show", "--reveal"]).unwrap();
        let Commands::Credentials(CredentialsCommands::Show { reveal, .. }) = cli.command else {
            panic!("expected credentials show");
        };
        assert!(reveal);
    }

    #[test]
    fn parse_credentials_show_with_env_and_reveal() {
        let cli = Cli::try_parse_from([
            "autumn",
            "credentials",
            "show",
            "--env",
            "staging",
            "--reveal",
        ])
        .unwrap();
        let Commands::Credentials(CredentialsCommands::Show { env, reveal }) = cli.command else {
            panic!("expected credentials show");
        };
        assert_eq!(env, "staging");
        assert!(reveal);
    }

    #[test]
    fn parse_credentials_without_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "credentials"]).is_err());
    }

    // ── autumn generate mailer tests ───────────────────────────────────────

    #[test]
    fn parse_generate_mailer_with_pascal_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "mailer", "Welcome"]).unwrap();
        let Commands::Generate(GenerateCommands::Mailer {
            name,
            dry_run,
            force,
            ..
        }) = cli.command
        else {
            panic!("expected generate mailer");
        };
        assert_eq!(name, "Welcome");
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_mailer_with_dry_run() {
        let cli =
            Cli::try_parse_from(["autumn", "generate", "mailer", "Welcome", "--dry-run"]).unwrap();
        let Commands::Generate(GenerateCommands::Mailer { dry_run, force, .. }) = cli.command
        else {
            panic!("expected generate mailer");
        };
        assert!(dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_mailer_with_force() {
        let cli =
            Cli::try_parse_from(["autumn", "generate", "mailer", "Welcome", "--force"]).unwrap();
        let Commands::Generate(GenerateCommands::Mailer { dry_run, force, .. }) = cli.command
        else {
            panic!("expected generate mailer");
        };
        assert!(!dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_mailer_snake_case_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "mailer", "welcome_email"]).unwrap();
        let Commands::Generate(GenerateCommands::Mailer { name, .. }) = cli.command else {
            panic!("expected generate mailer");
        };
        assert_eq!(name, "welcome_email");
    }

    #[test]
    fn parse_generate_mailer_without_name_is_error() {
        assert!(Cli::try_parse_from(["autumn", "generate", "mailer"]).is_err());
    }

    #[test]
    fn parse_generate_mailer_with_no_layout() {
        let cli = Cli::try_parse_from(["autumn", "generate", "mailer", "Welcome", "--no-layout"])
            .unwrap();
        let Commands::Generate(GenerateCommands::Mailer { no_layout, .. }) = cli.command else {
            panic!("expected generate mailer");
        };
        assert!(no_layout, "--no-layout flag must set no_layout = true");
    }

    #[test]
    fn parse_generate_mailer_no_layout_defaults_false() {
        let cli = Cli::try_parse_from(["autumn", "generate", "mailer", "Welcome"]).unwrap();
        let Commands::Generate(GenerateCommands::Mailer { no_layout, .. }) = cli.command else {
            panic!("expected generate mailer");
        };
        assert!(!no_layout, "no_layout must default to false");
    }

    // ── autumn generate channel tests ──────────────────────────────────────

    #[test]
    fn parse_generate_channel_with_pascal_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "channel", "Chat"]).unwrap();
        let Commands::Generate(GenerateCommands::Channel {
            name,
            sse,
            ws,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected generate channel");
        };
        assert_eq!(name, "Chat");
        assert!(
            !sse,
            "--sse must default to false (SSE is the implicit default)"
        );
        assert!(!ws, "--ws must default to false");
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_channel_with_ws_flag() {
        let cli = Cli::try_parse_from(["autumn", "generate", "channel", "Chat", "--ws"]).unwrap();
        let Commands::Generate(GenerateCommands::Channel { ws, sse, .. }) = cli.command else {
            panic!("expected generate channel");
        };
        assert!(ws, "--ws flag must set ws = true");
        assert!(!sse);
    }

    #[test]
    fn parse_generate_channel_with_explicit_sse_flag() {
        let cli = Cli::try_parse_from(["autumn", "generate", "channel", "Chat", "--sse"]).unwrap();
        let Commands::Generate(GenerateCommands::Channel { ws, sse, .. }) = cli.command else {
            panic!("expected generate channel");
        };
        assert!(sse, "--sse flag must set sse = true");
        assert!(!ws);
    }

    #[test]
    fn parse_generate_channel_sse_and_ws_conflict_is_error() {
        assert!(
            Cli::try_parse_from(["autumn", "generate", "channel", "Chat", "--sse", "--ws"])
                .is_err(),
            "--sse and --ws are mutually exclusive"
        );
    }

    #[test]
    fn parse_generate_channel_with_dry_run_and_force() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "channel",
            "Chat",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Channel { dry_run, force, .. }) = cli.command
        else {
            panic!("expected generate channel");
        };
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_channel_snake_case_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "channel", "chat_room"]).unwrap();
        let Commands::Generate(GenerateCommands::Channel { name, .. }) = cli.command else {
            panic!("expected generate channel");
        };
        assert_eq!(name, "chat_room");
    }

    #[test]
    fn parse_generate_channel_without_name_is_error() {
        assert!(Cli::try_parse_from(["autumn", "generate", "channel"]).is_err());
    }

    // ── autumn maintenance tests ───────────────────────────────────────────────

    #[test]
    fn parse_maintenance_on_defaults() {
        let cli = Cli::try_parse_from(["autumn", "maintenance", "on"]).unwrap();
        let Commands::Maintenance(MaintenanceCommands::On {
            message,
            allow_ips,
            readonly,
            bypass_header,
        }) = cli.command
        else {
            panic!("expected maintenance on");
        };
        assert!(message.is_none());
        assert!(allow_ips.is_empty());
        assert!(!readonly);
        assert!(bypass_header.is_none());
    }

    #[test]
    fn parse_maintenance_on_with_message() {
        let cli = Cli::try_parse_from([
            "autumn",
            "maintenance",
            "on",
            "--message",
            "Upgrading database",
        ])
        .unwrap();
        let Commands::Maintenance(MaintenanceCommands::On { message, .. }) = cli.command else {
            panic!("expected maintenance on");
        };
        assert_eq!(message.as_deref(), Some("Upgrading database"));
    }

    #[test]
    fn parse_maintenance_on_readonly() {
        let cli = Cli::try_parse_from(["autumn", "maintenance", "on", "--readonly"]).unwrap();
        let Commands::Maintenance(MaintenanceCommands::On { readonly, .. }) = cli.command else {
            panic!("expected maintenance on");
        };
        assert!(readonly);
    }

    #[test]
    fn parse_maintenance_on_with_allow_ips() {
        let cli = Cli::try_parse_from([
            "autumn",
            "maintenance",
            "on",
            "--allow-ips",
            "10.0.0.0/8",
            "--allow-ips",
            "192.168.1.1",
        ])
        .unwrap();
        let Commands::Maintenance(MaintenanceCommands::On { allow_ips, .. }) = cli.command else {
            panic!("expected maintenance on");
        };
        assert_eq!(allow_ips, vec!["10.0.0.0/8", "192.168.1.1"]);
    }

    #[test]
    fn parse_maintenance_on_with_bypass_header() {
        let cli = Cli::try_parse_from([
            "autumn",
            "maintenance",
            "on",
            "--bypass-header",
            "X-Bypass:secret",
        ])
        .unwrap();
        let Commands::Maintenance(MaintenanceCommands::On { bypass_header, .. }) = cli.command
        else {
            panic!("expected maintenance on");
        };
        assert_eq!(bypass_header.as_deref(), Some("X-Bypass:secret"));
    }

    #[test]
    fn parse_maintenance_off() {
        let cli = Cli::try_parse_from(["autumn", "maintenance", "off"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Maintenance(MaintenanceCommands::Off)
        ));
    }

    #[test]
    fn parse_maintenance_without_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "maintenance"]).is_err());
    }

    #[test]
    fn parse_shard_move_slot() {
        let cli = Cli::try_parse_from([
            "autumn",
            "shard",
            "move-slot",
            "--from",
            "shard0",
            "--to",
            "shard1",
            "--table",
            "bookmarks",
            "--tenant",
            "acme",
            "--tenant",
            "globex",
            "--confirm",
        ])
        .unwrap();
        let Commands::Shard(cmd) = cli.command else {
            panic!("expected shard");
        };
        let ShardSubcommand::MoveSlot {
            from,
            to,
            table,
            key_column,
            tenants,
            confirm,
            ..
        } = cmd.command;
        assert_eq!(from, "shard0");
        assert_eq!(to, "shard1");
        assert_eq!(table, "bookmarks");
        assert_eq!(key_column, "tenant_id"); // default
        assert_eq!(tenants, vec!["acme".to_owned(), "globex".to_owned()]);
        assert!(confirm);
    }

    #[test]
    fn parse_shard_move_slot_requires_tenant() {
        assert!(
            Cli::try_parse_from([
                "autumn",
                "shard",
                "move-slot",
                "--from",
                "shard0",
                "--to",
                "shard1",
                "--table",
                "bookmarks",
            ])
            .is_err()
        );
    }

    #[test]
    fn parse_migrate_with_maintenance() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "--with-maintenance"]).unwrap();
        let Commands::Migrate {
            with_maintenance, ..
        } = cli.command
        else {
            panic!("expected migrate");
        };
        assert!(with_maintenance);
    }

    #[test]
    fn parse_migrate_without_maintenance_defaults_false() {
        let cli = Cli::try_parse_from(["autumn", "migrate"]).unwrap();
        let Commands::Migrate {
            with_maintenance, ..
        } = cli.command
        else {
            panic!("expected migrate");
        };
        assert!(!with_maintenance);
    }

    #[test]
    fn parse_migrate_with_maintenance_before_subcommand() {
        // --with-maintenance is a flag on the parent Migrate command;
        // it must appear before the subcommand name.
        let cli =
            Cli::try_parse_from(["autumn", "migrate", "--with-maintenance", "status"]).unwrap();
        let Commands::Migrate {
            action,
            with_maintenance,
            ..
        } = cli.command
        else {
            panic!("expected migrate");
        };
        assert!(matches!(action, Some(MigrateCommands::Status)));
        assert!(with_maintenance);
    }

    #[test]
    fn parse_migrate_down_default() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "down"]).unwrap();
        let Commands::Migrate { action, .. } = cli.command else {
            panic!("expected migrate");
        };
        assert!(matches!(
            action,
            Some(MigrateCommands::Down {
                steps: None,
                to: None,
                yes_i_mean_prod: false
            })
        ));
    }

    #[test]
    fn parse_migrate_down_steps() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "down", "--steps", "3"]).unwrap();
        let Commands::Migrate { action, .. } = cli.command else {
            panic!("expected migrate");
        };
        assert!(matches!(
            action,
            Some(MigrateCommands::Down {
                steps: Some(3),
                to: None,
                yes_i_mean_prod: false
            })
        ));
    }

    #[test]
    fn parse_migrate_down_to_version() {
        let cli =
            Cli::try_parse_from(["autumn", "migrate", "down", "--to", "20260101000000"]).unwrap();
        let Commands::Migrate { action, .. } = cli.command else {
            panic!("expected migrate");
        };
        let Some(MigrateCommands::Down { to, steps, .. }) = action else {
            panic!("expected Down");
        };
        assert_eq!(to.as_deref(), Some("20260101000000"));
        assert!(steps.is_none());
    }

    #[test]
    fn parse_migrate_down_yes_i_mean_prod() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "down", "--yes-i-mean-prod"]).unwrap();
        let Commands::Migrate { action, .. } = cli.command else {
            panic!("expected migrate");
        };
        let Some(MigrateCommands::Down {
            yes_i_mean_prod, ..
        }) = action
        else {
            panic!("expected Down");
        };
        assert!(yes_i_mean_prod);
    }

    #[test]
    fn parse_migrate_down_steps_and_to_are_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "autumn", "migrate", "down", "--steps", "2", "--to", "20260101",
        ]);
        assert!(
            result.is_err(),
            "--steps and --to must be mutually exclusive"
        );
    }

    #[test]
    fn parse_migrate_shard_flag() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "--shard", "shard1"]).unwrap();
        let Commands::Migrate { shard, .. } = cli.command else {
            panic!("expected migrate");
        };
        assert_eq!(shard.as_deref(), Some("shard1"));
    }

    #[test]
    fn parse_migrate_control_only_flag() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "--control-only", "status"]).unwrap();
        let Commands::Migrate {
            action,
            control_only,
            ..
        } = cli.command
        else {
            panic!("expected migrate");
        };
        assert!(control_only);
        assert!(matches!(action, Some(MigrateCommands::Status)));
    }

    #[test]
    fn parse_migrate_shard_conflicts_with_control_only() {
        assert!(
            Cli::try_parse_from(["autumn", "migrate", "--shard", "shard1", "--control-only"])
                .is_err()
        );
    }

    #[test]
    fn parse_migrate_wait_flag() {
        let cli = Cli::try_parse_from(["autumn", "migrate", "--wait", "60"]).unwrap();
        let Commands::Migrate { wait, .. } = cli.command else {
            panic!("expected migrate");
        };
        assert_eq!(wait, Some(60u64));
    }

    #[test]
    fn parse_migrate_wait_defaults_none() {
        let cli = Cli::try_parse_from(["autumn", "migrate"]).unwrap();
        let Commands::Migrate { wait, .. } = cli.command else {
            panic!("expected migrate");
        };
        assert_eq!(wait, None);
    }

    #[test]
    fn parse_dev_loop_bench_defaults() {
        let cli = Cli::try_parse_from(["autumn", "dev-loop-bench"]).unwrap();
        let Commands::DevLoopBench {
            example,
            runs,
            output,
            json,
            fail_on_regression,
            dry_run,
            cold_start,
            include_db,
            scaling,
            sizes,
            baseline,
        } = cli.command
        else {
            panic!("expected dev-loop-bench");
        };
        assert_eq!(example, "examples/hello");
        assert_eq!(runs, 5);
        assert!(output.is_none());
        assert!(!json);
        assert!(!fail_on_regression);
        assert!(!dry_run);
        assert!(!cold_start);
        assert!(!include_db);
        assert!(!scaling);
        assert_eq!(sizes, crate::dev_loop_scaling::DEFAULT_SIZES);
        assert!(baseline.is_none());
    }

    #[test]
    fn parse_dev_loop_bench_dry_run() {
        let cli = Cli::try_parse_from(["autumn", "dev-loop-bench", "--dry-run"]).unwrap();
        let Commands::DevLoopBench { dry_run, .. } = cli.command else {
            panic!("expected dev-loop-bench");
        };
        assert!(dry_run);
    }

    #[test]
    fn parse_dev_loop_bench_cold_start_flags() {
        let cli = Cli::try_parse_from(["autumn", "dev-loop-bench", "--cold-start", "--include-db"])
            .unwrap();
        let Commands::DevLoopBench {
            cold_start,
            include_db,
            ..
        } = cli.command
        else {
            panic!("expected dev-loop-bench");
        };
        assert!(cold_start);
        assert!(include_db);
    }

    #[test]
    fn parse_dev_loop_bench_custom_example_and_runs() {
        let cli = Cli::try_parse_from([
            "autumn",
            "dev-loop-bench",
            "--example",
            "examples/todo-app",
            "--runs",
            "10",
        ])
        .unwrap();
        let Commands::DevLoopBench { example, runs, .. } = cli.command else {
            panic!("expected dev-loop-bench");
        };
        assert_eq!(example, "examples/todo-app");
        assert_eq!(runs, 10);
    }

    #[test]
    fn parse_dev_loop_bench_fail_on_regression() {
        let cli =
            Cli::try_parse_from(["autumn", "dev-loop-bench", "--fail-on-regression"]).unwrap();
        let Commands::DevLoopBench {
            fail_on_regression, ..
        } = cli.command
        else {
            panic!("expected dev-loop-bench");
        };
        assert!(fail_on_regression);
    }

    #[test]
    fn parse_dev_loop_bench_json_output() {
        let cli = Cli::try_parse_from(["autumn", "dev-loop-bench", "--json"]).unwrap();
        let Commands::DevLoopBench { json, .. } = cli.command else {
            panic!("expected dev-loop-bench");
        };
        assert!(json);
    }

    #[test]
    fn parse_dev_loop_bench_output_path() {
        let cli =
            Cli::try_parse_from(["autumn", "dev-loop-bench", "--output", "report.json"]).unwrap();
        let Commands::DevLoopBench { output, .. } = cli.command else {
            panic!("expected dev-loop-bench");
        };
        assert_eq!(output.as_deref(), Some("report.json"));
    }

    #[test]
    fn parse_dev_loop_bench_scaling_flag() {
        let cli = Cli::try_parse_from(["autumn", "dev-loop-bench", "--scaling"]).unwrap();
        let Commands::DevLoopBench { scaling, .. } = cli.command else {
            panic!("expected dev-loop-bench");
        };
        assert!(scaling);
    }

    #[test]
    fn parse_dev_loop_bench_scaling_custom_sizes() {
        let cli = Cli::try_parse_from([
            "autumn",
            "dev-loop-bench",
            "--scaling",
            "--sizes",
            "1,10,50",
        ])
        .unwrap();
        let Commands::DevLoopBench { scaling, sizes, .. } = cli.command else {
            panic!("expected dev-loop-bench");
        };
        assert!(scaling);
        assert_eq!(sizes, "1,10,50");
    }

    #[test]
    fn parse_dev_loop_bench_scaling_baseline_path() {
        let cli = Cli::try_parse_from([
            "autumn",
            "dev-loop-bench",
            "--scaling",
            "--baseline",
            "benchmarks/dev-loop-scaling/baseline.json",
        ])
        .unwrap();
        let Commands::DevLoopBench {
            scaling, baseline, ..
        } = cli.command
        else {
            panic!("expected dev-loop-bench");
        };
        assert!(scaling);
        assert_eq!(
            baseline.as_deref(),
            Some("benchmarks/dev-loop-scaling/baseline.json")
        );
    }

    // ── autumn config tests ────────────────────────────────────────────────────

    #[test]
    fn parse_config_list() {
        let cli = Cli::try_parse_from(["autumn", "config", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Config(ConfigCommands::List)
        ));
    }

    #[test]
    fn parse_config_get() {
        let cli = Cli::try_parse_from(["autumn", "config", "get", "max_upload_mb"]).unwrap();
        let Commands::Config(ConfigCommands::Get { key }) = cli.command else {
            panic!("expected config get");
        };
        assert_eq!(key, "max_upload_mb");
    }

    #[test]
    fn parse_config_get_requires_key() {
        assert!(Cli::try_parse_from(["autumn", "config", "get"]).is_err());
    }

    #[test]
    fn parse_config_set() {
        let cli = Cli::try_parse_from(["autumn", "config", "set", "max_upload_mb", "200"]).unwrap();
        let Commands::Config(ConfigCommands::Set { key, value, actor }) = cli.command else {
            panic!("expected config set");
        };
        assert_eq!(key, "max_upload_mb");
        assert_eq!(value, "200");
        assert!(actor.is_none());
    }

    #[test]
    fn parse_config_set_accepts_hyphen_prefixed_value() {
        let cli =
            Cli::try_parse_from(["autumn", "config", "set", "offset_seconds", "-30"]).unwrap();
        let Commands::Config(ConfigCommands::Set { key, value, actor }) = cli.command else {
            panic!("expected config set");
        };
        assert_eq!(key, "offset_seconds");
        assert_eq!(value, "-30");
        assert!(actor.is_none());
    }

    #[test]
    fn parse_config_set_with_actor() {
        let cli = Cli::try_parse_from([
            "autumn",
            "config",
            "set",
            "max_upload_mb",
            "200",
            "--actor",
            "ops@example.com",
        ])
        .unwrap();
        let Commands::Config(ConfigCommands::Set { actor, .. }) = cli.command else {
            panic!("expected config set");
        };
        assert_eq!(actor.as_deref(), Some("ops@example.com"));
    }

    #[test]
    fn parse_config_set_requires_key_and_value() {
        assert!(Cli::try_parse_from(["autumn", "config", "set"]).is_err());
        assert!(Cli::try_parse_from(["autumn", "config", "set", "key"]).is_err());
    }

    #[test]
    fn parse_config_unset() {
        let cli = Cli::try_parse_from(["autumn", "config", "unset", "max_upload_mb"]).unwrap();
        let Commands::Config(ConfigCommands::Unset { key, actor }) = cli.command else {
            panic!("expected config unset");
        };
        assert_eq!(key, "max_upload_mb");
        assert!(actor.is_none());
    }

    #[test]
    fn parse_config_unset_with_actor() {
        let cli = Cli::try_parse_from([
            "autumn",
            "config",
            "unset",
            "max_upload_mb",
            "--actor",
            "alice",
        ])
        .unwrap();
        let Commands::Config(ConfigCommands::Unset { actor, .. }) = cli.command else {
            panic!("expected config unset");
        };
        assert_eq!(actor.as_deref(), Some("alice"));
    }

    #[test]
    fn parse_config_unset_requires_key() {
        assert!(Cli::try_parse_from(["autumn", "config", "unset"]).is_err());
    }

    #[test]
    fn parse_config_history() {
        let cli = Cli::try_parse_from(["autumn", "config", "history", "max_upload_mb"]).unwrap();
        let Commands::Config(ConfigCommands::History { key, limit }) = cli.command else {
            panic!("expected config history");
        };
        assert_eq!(key, "max_upload_mb");
        assert_eq!(limit, 20, "default limit should be 20");
    }

    #[test]
    fn parse_config_history_with_limit() {
        let cli = Cli::try_parse_from([
            "autumn",
            "config",
            "history",
            "max_upload_mb",
            "--limit",
            "50",
        ])
        .unwrap();
        let Commands::Config(ConfigCommands::History { limit, .. }) = cli.command else {
            panic!("expected config history");
        };
        assert_eq!(limit, 50);
    }

    #[test]
    fn parse_config_history_requires_key() {
        assert!(Cli::try_parse_from(["autumn", "config", "history"]).is_err());
    }

    #[test]
    fn parse_config_without_subcommand_is_error() {
        assert!(Cli::try_parse_from(["autumn", "config"]).is_err());
    }

    // ── autumn generate auth --oauth tests (RED phase) ─────────────────────

    #[test]
    fn parse_generate_auth_with_oauth_flag_single_provider() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User", "--oauth", "github"])
            .unwrap();
        let Commands::Generate(GenerateCommands::Auth { name, oauth, .. }) = cli.command else {
            panic!("expected generate auth");
        };
        assert_eq!(name, "User");
        assert_eq!(oauth, vec!["github"]);
    }

    #[test]
    fn parse_generate_auth_with_oauth_multiple_providers() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "auth",
            "User",
            "--oauth",
            "github,google",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Auth { oauth, .. }) = cli.command else {
            panic!("expected generate auth");
        };
        assert_eq!(oauth, vec!["github", "google"]);
    }

    #[test]
    fn parse_generate_auth_without_oauth_defaults_empty() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth { oauth, .. }) = cli.command else {
            panic!("expected generate auth");
        };
        assert!(
            oauth.is_empty(),
            "oauth must default to empty when flag not given"
        );
    }

    #[test]
    fn parse_check_deprecations() {
        let cli = Cli::try_parse_from(["autumn", "check", "deprecations"]).unwrap();
        let Commands::Check { subcommand, .. } = cli.command else {
            panic!("expected check");
        };
        assert!(matches!(
            subcommand,
            Some(CheckSubcommands::Deprecations {
                package: None,
                bin: None
            })
        ));
    }

    #[test]
    fn parse_check_deprecations_with_package_and_bin() {
        let cli = Cli::try_parse_from([
            "autumn",
            "check",
            "deprecations",
            "-p",
            "my-app",
            "--bin",
            "my-bin",
        ])
        .unwrap();
        let Commands::Check { subcommand, .. } = cli.command else {
            panic!("expected check");
        };
        assert_eq!(
            subcommand,
            Some(CheckSubcommands::Deprecations {
                package: Some("my-app".to_string()),
                bin: Some("my-bin".to_string())
            })
        );
    }

    #[test]
    fn parse_generate_auth_passkeys_flag() {
        let cli =
            Cli::try_parse_from(["autumn", "generate", "auth", "User", "--passkeys"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth { name, passkeys, .. }) = cli.command else {
            panic!("wrong variant");
        };
        assert_eq!(name, "User");
        assert!(passkeys, "--passkeys must set the passkeys flag");
    }

    #[test]
    fn generate_auth_passkeys_defaults_off() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth { passkeys, .. }) = cli.command else {
            panic!("wrong variant");
        };
        assert!(!passkeys, "passkeys must default to off");
    }

    #[test]
    fn parse_generate_system_test() {
        let cli = Cli::try_parse_from(["autumn", "generate", "system-test", "TodoFlow"]).unwrap();
        let Commands::Generate(GenerateCommands::SystemTest {
            ref name,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected SystemTest variant");
        };
        assert_eq!(name, "TodoFlow");
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_system_test_dry_run() {
        let cli = Cli::try_parse_from(["autumn", "generate", "system-test", "MyTest", "--dry-run"])
            .unwrap();
        let Commands::Generate(GenerateCommands::SystemTest { dry_run, .. }) = cli.command else {
            panic!("expected SystemTest variant");
        };
        assert!(dry_run);
    }

    #[test]
    fn parse_generate_system_test_force() {
        let cli = Cli::try_parse_from(["autumn", "generate", "system-test", "MyTest", "--force"])
            .unwrap();
        let Commands::Generate(GenerateCommands::SystemTest { force, .. }) = cli.command else {
            panic!("expected SystemTest variant");
        };
        assert!(force);
    }

    // ── autumn generate pwa ────────────────────────────────────────────────

    #[test]
    fn parse_generate_pwa() {
        let cli = Cli::try_parse_from(["autumn", "generate", "pwa"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Generate(GenerateCommands::Pwa {
                dry_run: false,
                force: false
            })
        ));
    }

    #[test]
    fn parse_generate_pwa_dry_run() {
        let cli = Cli::try_parse_from(["autumn", "generate", "pwa", "--dry-run"]).unwrap();
        let Commands::Generate(GenerateCommands::Pwa { dry_run, force }) = cli.command else {
            panic!("expected Pwa variant");
        };
        assert!(dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_pwa_force() {
        let cli = Cli::try_parse_from(["autumn", "generate", "pwa", "--force"]).unwrap();
        let Commands::Generate(GenerateCommands::Pwa { dry_run, force }) = cli.command else {
            panic!("expected Pwa variant");
        };
        assert!(!dry_run);
        assert!(force);
    }

    // ── autumn generate tauri ──────────────────────────────────────────────

    #[test]
    fn parse_generate_tauri() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Generate(GenerateCommands::Tauri {
                dry_run: false,
                force: false
            })
        ));
    }

    #[test]
    fn parse_generate_tauri_dry_run() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri", "--dry-run"]).unwrap();
        let Commands::Generate(GenerateCommands::Tauri { dry_run, force }) = cli.command else {
            panic!("expected Tauri variant");
        };
        assert!(dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_tauri_force() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri", "--force"]).unwrap();
        let Commands::Generate(GenerateCommands::Tauri { dry_run, force }) = cli.command else {
            panic!("expected Tauri variant");
        };
        assert!(!dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_plugin() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "plugin",
            "foo",
            "--path",
            "custom-path",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Plugin {
            name,
            path,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected Plugin variant");
        };
        assert_eq!(name, "foo");
        assert_eq!(path.as_deref(), Some("custom-path"));
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_plugin_defaults() {
        let cli = Cli::try_parse_from(["autumn", "generate", "plugin", "foo"]).unwrap();
        let Commands::Generate(GenerateCommands::Plugin {
            name,
            path,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected Plugin variant");
        };
        assert_eq!(name, "foo");
        assert!(path.is_none());
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_scaffold_sharded_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--sharded",
            "--shard-key",
            "tenant_id",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold {
            name,
            sharded,
            shard_key,
            ..
        }) = cli.command
        else {
            panic!("expected Scaffold variant");
        };
        assert_eq!(name, "Post");
        assert!(sharded, "--sharded flag must set sharded=true");
        assert_eq!(
            shard_key.as_deref(),
            Some("tenant_id"),
            "--shard-key must capture the field name"
        );
    }

    #[test]
    fn parse_scaffold_sharded_without_shard_key() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "scaffold",
            "Post",
            "name:String",
            "--sharded",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold {
            sharded, shard_key, ..
        }) = cli.command
        else {
            panic!("expected Scaffold variant");
        };
        assert!(sharded);
        assert!(
            shard_key.is_none(),
            "shard_key should be None when not specified"
        );
    }

    #[test]
    fn parse_check_config() {
        let cli = Cli::try_parse_from(["autumn", "check", "--config"]).unwrap();
        let Commands::Check { config, .. } = cli.command else {
            panic!("expected check");
        };
        assert!(config);
    }
}
