use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

mod a11y;
mod agents;
mod alert;
mod assets;
mod build;
mod cache_audit;
mod canary;
mod capacity;
mod capacity_driver;
mod capsule;
mod check;
mod cold_start_driver;
mod config;
mod console;
mod credentials;
mod data;
mod data_flow;
mod db;
mod db_pull;
mod deploy;
mod deps;
mod dev;
mod dev_loop_bench;
mod dev_loop_scaling;
mod doctor;
mod edge_scan;
mod experiments;
mod export;
mod flags;
mod generate;
mod graph;
mod http;
mod i18n;
mod jobs;
mod lifecycle;
mod maintenance;
mod migrate;
mod monitor;
mod new;
mod overload_driver;
mod paths;
mod pg;
mod platform;
mod plugin;
mod plugin_check;
mod plugin_sandbox;
mod posture;
mod process;
mod release;
mod replay;
mod retention;
mod routes;
mod routes_audit;
mod rust_source;
mod sbom;
mod scaling_driver;
mod schema;
mod search;
mod seed;
mod serve;
mod setup;
mod shard;
mod starters;
mod task;
mod test_cmd;
mod text_width;
mod token;
mod upgrade;
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

/// Subcommands for `autumn routes`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum RoutesSubcommands {
    /// Prove route authentication coverage at build time (issue #1604).
    ///
    /// Compiles the app, classifies every route from its macro-expanded auth
    /// posture, and emits a stable-ordered security manifest. Exits non-zero
    /// when any route is unclassified — i.e. neither framework-owned, guarded
    /// (`#[secured]` / `#[authorize]`), nor explicitly `#[public]` — naming each
    /// offending route so it can be closed. This is the CI gate.
    Audit {
        /// Package to inspect (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to inspect (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Write the JSON security manifest to this file path.
        #[arg(long, value_name = "PATH")]
        manifest: Option<String>,
        /// Emit the JSON manifest to stdout instead of the human summary.
        #[arg(long)]
        json: bool,
        /// Reserved for tightening the gate; fail-on-unclassified is already the
        /// default behavior.
        #[arg(long)]
        strict: bool,
    },
    /// Diff, acknowledge, and verify security posture across commits (#1624).
    ///
    /// `routes audit` proves what the security surface *is*; `routes posture`
    /// answers what a change *did to it*, and whether a human agreed.
    ///
    ///   autumn routes posture diff --base base.json --head posture.json
    ///   autumn routes posture digest --manifest security-posture.json
    ///   autumn routes posture verify --manifest security-posture.json \
    ///     --expect-digest <digest> --repo owner/repo
    #[command(subcommand, verbatim_doc_comment)]
    Posture(PostureSubcommands),
}

/// Subcommands for `autumn routes posture` (issue #1624).
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum PostureSubcommands {
    /// Diff two security posture manifests and gate on surface widening.
    ///
    /// Exits 0 when nothing widened (or the widening is acknowledged), 1 when a
    /// widening is unacknowledged, and 2 on a usage or I/O problem — so CI can
    /// tell "this PR widens the surface" from "the tool could not run".
    ///
    /// A widening blocks until someone comments the marker the report prints:
    ///
    ///   /ack-posture <digest>  optional reason
    ///
    /// The digest binds the acknowledgment to that exact set of widenings, so
    /// pushing unrelated commits keeps it valid while a *new* widening
    /// re-blocks.
    #[command(verbatim_doc_comment)]
    Diff {
        /// The previously accepted manifest (e.g. the base branch's copy).
        #[arg(long, value_name = "PATH")]
        base: String,
        /// The manifest for this commit, as built by `autumn routes audit`.
        #[arg(long, value_name = "PATH")]
        head: String,
        /// Output format: `markdown` (default), `text`, or `json`.
        #[arg(long, default_value = "markdown", value_name = "FORMAT")]
        format: String,
        /// Also write the rendered report to this path.
        #[arg(long, value_name = "PATH")]
        output: Option<String>,
        /// An acknowledgment digest, without the comment ceremony (repeatable).
        #[arg(long, value_name = "DIGEST")]
        ack: Vec<String>,
        /// File of pull-request text to scan for `/ack-posture` markers.
        ///
        /// The workflow harvests it from comments whose author is an OWNER,
        /// MEMBER or COLLABORATOR: this command trusts what it is given and
        /// enforces no authorization of its own.
        #[arg(long, value_name = "PATH")]
        ack_file: Option<String>,
        /// Treat a missing base manifest as "no baseline yet" (exit 0) instead
        /// of an error. What a repository enabling the gate wants on its first
        /// run.
        #[arg(long)]
        allow_missing_base: bool,
    },
    /// Print a manifest's posture digest — the number a release records.
    ///
    /// Computed over the manifest's security-relevant content only, so a
    /// handler rename or a moved line does not change it.
    Digest {
        /// Manifest to digest.
        #[arg(long, value_name = "PATH")]
        manifest: String,
        /// Output format: `text` (default) or `json`.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        format: String,
    },
    /// Verify a shipped manifest is the acknowledged one, and genuinely signed.
    ///
    /// Two checks: the posture digest matches what CI acknowledged, and
    /// `gh attestation verify` accepts the file (the same keyless Sigstore
    /// pipeline the rest of the supply chain uses — see
    /// docs/guide/supply-chain.md).
    Verify {
        /// Manifest to verify.
        #[arg(long, value_name = "PATH")]
        manifest: String,
        /// The digest recorded when the posture was acknowledged.
        #[arg(long, value_name = "DIGEST")]
        expect_digest: Option<String>,
        /// `owner/repo` whose CI minted the attestation.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        /// Skip the signature check. Air-gapped hosts only — it is reported as
        /// waived, never as passed.
        #[arg(long)]
        skip_signature: bool,
    },
}

/// Arguments for [`CacheSubcommands::Audit`].
///
/// A separate `Args` struct rather than inline variant fields, for the same
/// reason as `UpgradeArgs`: clap's derive builds every inline field of every
/// variant inside one `Commands::augment_subcommands` frame, and that frame is
/// already within a kilobyte of libtest's 2 MiB thread stack. Five more inline
/// fields is exactly the kind of increment that decides whether the
/// argument-parsing tests overflow, so this command keeps its share out of the
/// shared frame: an `Args` struct gets its own `CacheAuditArgs::augment_args`
/// frame, which pops before the next variant is built.
#[derive(clap::Args, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent CLI flags, not a state machine
pub struct CacheAuditArgs {
    /// Package to inspect (for workspaces).
    #[arg(short, long)]
    package: Option<String>,
    /// Binary target to inspect (for packages with multiple bin targets).
    #[arg(long, value_name = "BIN")]
    bin: Option<String>,
    /// Write the JSON cache-coherence manifest to this file path.
    #[arg(long, value_name = "PATH")]
    manifest: Option<String>,
    /// Emit the JSON manifest to stdout instead of the human report.
    #[arg(long)]
    json: bool,
    /// Also fail when a cached read's dependency set could not be established
    /// (the default only warns, so the gate never cries wolf).
    #[arg(long)]
    strict: bool,
    /// Cargo features to build the audited binary with (repeatable; a
    /// comma-separated list also works). A `#[cached]` read or `#[repository]`
    /// write behind a feature the build does not enable is not compiled in, so
    /// it cannot appear in the manifest — audit the feature set you deploy.
    #[arg(long, value_name = "FEATURES")]
    features: Vec<String>,
    /// Build the audited binary with all Cargo features enabled.
    #[arg(long)]
    all_features: bool,
    /// Build the audited binary without default Cargo features.
    #[arg(long)]
    no_default_features: bool,
}

/// Arguments for `autumn data-flow`.
///
/// A separate `Args` struct for the same reason as [`CacheAuditArgs`]: clap's
/// derive builds every inline variant field inside one
/// `Commands::augment_subcommands` frame, which is already close to libtest's
/// thread-stack limit.
#[derive(clap::Args, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent CLI flags, not a state machine
pub struct DataFlowArgs {
    /// Package to inspect (for workspaces).
    #[arg(short, long)]
    package: Option<String>,
    /// Binary target to inspect (for packages with multiple bin targets).
    #[arg(long, value_name = "BIN")]
    bin: Option<String>,
    /// Write the JSON data-flow manifest to this file path.
    #[arg(long, value_name = "PATH")]
    manifest: Option<String>,
    /// Emit the JSON manifest to stdout instead of the human report.
    #[arg(long)]
    json: bool,
    /// Compare against a committed manifest and exit non-zero on drift, so a
    /// new release edge has to be reviewed rather than merged silently.
    #[arg(long, value_name = "PATH")]
    check: Option<String>,
    /// Cargo features to build the inspected binary with (repeatable; a
    /// comma-separated list also works). A `#[classified]` column or a
    /// declassification boundary behind a feature the build does not enable is
    /// not compiled in, so it cannot appear in the manifest.
    #[arg(long, value_name = "FEATURES")]
    features: Vec<String>,
    /// Build the inspected binary with all Cargo features enabled.
    #[arg(long)]
    all_features: bool,
    /// Build the inspected binary without default Cargo features.
    #[arg(long)]
    no_default_features: bool,
    /// Audit the release binary rather than the debug one.
    ///
    /// The manifest describes the binary that produced it, and a debug binary
    /// is not the one that ships: a classified column or a declassification
    /// boundary behind `#[cfg(not(debug_assertions))]` exists only in the
    /// release build. Run `--check` in CI under the profile you deploy.
    #[arg(long)]
    release: bool,
}

/// Arguments for `autumn agents manifest`.
///
/// A separate `Args` struct for the same reason as [`DataFlowArgs`]: clap's
/// derive builds every inline variant field inside one
/// `Commands::augment_subcommands` frame, which is already close to libtest's
/// thread-stack limit.
#[derive(clap::Args, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent CLI flags, not a state machine
pub struct AgentsManifestArgs {
    /// Package to inspect (for workspaces).
    #[arg(short, long)]
    package: Option<String>,
    /// Binary target to inspect (for packages with multiple bin targets).
    #[arg(long, value_name = "BIN")]
    bin: Option<String>,
    /// Write the JSON agent-authority manifest to this file path.
    #[arg(long, value_name = "PATH")]
    manifest: Option<String>,
    /// Emit the JSON manifest to stdout instead of the human report.
    #[arg(long)]
    json: bool,
    /// Compare against a committed manifest and exit non-zero on drift, so a
    /// widened authority envelope has to be reviewed rather than merged
    /// silently.
    ///
    /// This is the CI gate. Every check this command performs beyond the
    /// compiler's own — drift, mutating tools with no envelope, actions nothing
    /// can undo and nothing records, and routes naming an authority nothing
    /// registered — runs only under `--check`. Without it the command reports
    /// and warns but never fails, so a run that does not pass `--check` proves
    /// nothing. Wire `autumn agents manifest --check <path>` into CI next to
    /// `autumn data-flow --check`, and commit the manifest it writes with
    /// `--manifest <path>`.
    #[arg(long, value_name = "PATH")]
    check: Option<String>,
    /// Let `--check` pass with MCP-exposed mutating tools that carry no
    /// authority envelope.
    ///
    /// Adoption is incremental and `#[repository(api, mcp)]` generates CRUD
    /// tools with no annotation site, so the hatch exists — but it is a flag,
    /// never a default: a mutating tool an agent can call with nothing declared
    /// about it is what this command exists to surface. Allowed tools are still
    /// listed.
    ///
    /// `requires = "check"`: the gate it relaxes only runs under `--check`, so
    /// passing it alone means nothing. Saying so is better than accepting it
    /// silently and leaving the author believing a gate was waived.
    #[arg(long, requires = "check")]
    allow_ungoverned: bool,
    /// Let `--check` pass when no agent audit sink is configured even though
    /// the binary can take an action nothing can undo.
    ///
    /// The one combination the runtime cannot catch: with no sink installed the
    /// audit write trivially succeeds, so the fail-closed refusal never fires
    /// and the invocation leaves no trace at all. A development binary
    /// legitimately has no sink, so the hatch exists — but the default is to
    /// fail, because "irreversible and unrecorded" is not a state to discover
    /// afterwards.
    ///
    /// `requires = "check"`, for the same reason as `--allow-ungoverned`.
    #[arg(long, requires = "check")]
    allow_unaudited: bool,
    /// Cargo features to build the inspected binary with (repeatable; a
    /// comma-separated list also works). An `#[agent_operable]` action or a
    /// grant behind a feature the build does not enable is not compiled in, so
    /// it cannot appear in the manifest.
    #[arg(long, value_name = "FEATURES")]
    features: Vec<String>,
    /// Build the inspected binary with all Cargo features enabled.
    #[arg(long)]
    all_features: bool,
    /// Build the inspected binary without default Cargo features.
    #[arg(long)]
    no_default_features: bool,
    /// Audit the release binary rather than the debug one.
    ///
    /// The manifest describes the binary that produced it, and a debug binary
    /// is not the one that ships: an action or a grant behind
    /// `#[cfg(not(debug_assertions))]` exists only in the release build. Run
    /// `--check` in CI under the profile you deploy.
    #[arg(long)]
    release: bool,
}

/// Arguments for `autumn graph` (issue #1747).
///
/// A separate `Args` struct for the same reason as [`AgentsManifestArgs`]:
/// clap's derive builds every inline variant field inside one
/// `Commands::augment_subcommands` frame, which is already close to libtest's
/// thread-stack limit.
#[derive(clap::Args, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent CLI flags, not a state machine
pub struct GraphArgs {
    /// Package to inspect (for workspaces).
    #[arg(short, long)]
    package: Option<String>,
    /// Binary target to inspect (for packages with multiple bin targets).
    #[arg(long, value_name = "BIN")]
    bin: Option<String>,
    /// Write the JSON architecture graph to this file path.
    #[arg(long, value_name = "PATH")]
    manifest: Option<String>,
    /// Emit the JSON graph to stdout instead of the human report.
    ///
    /// Only meaningful for `show`: `touches` and `impact` are answers, not
    /// documents.
    #[arg(long)]
    json: bool,
    /// Compare against a committed graph and exit non-zero on drift, so a
    /// route that quietly lost its access to a table — or a declared element
    /// that vanished from the graph — has to be reviewed rather than merged
    /// silently. This is the CI gate.
    #[arg(long, value_name = "PATH")]
    check: Option<String>,
    /// Cargo features to build the inspected binary with (repeatable; a
    /// comma-separated list also works). A model, route or job behind a
    /// feature the build does not enable is not compiled in, so it cannot
    /// appear in the graph.
    #[arg(long, value_name = "FEATURES")]
    features: Vec<String>,
    /// Build the inspected binary with all Cargo features enabled.
    #[arg(long)]
    all_features: bool,
    /// Build the inspected binary without default Cargo features.
    #[arg(long)]
    no_default_features: bool,
    /// Inspect the release binary rather than the debug one.
    ///
    /// The graph describes the binary that produced it, and a debug binary is
    /// not the one that ships: an element behind `#[cfg(not(debug_assertions))]`
    /// exists only in the release build. Run `--check` in CI under the profile
    /// you deploy.
    #[arg(long)]
    release: bool,
}

/// Subcommands for `autumn graph`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum GraphSubcommands {
    /// Print the whole architecture graph.
    Show(GraphArgs),
    /// Which routes and jobs touch a model, table or repository.
    Touches {
        /// Model name, table name, repository trait, or generated `Pg*` type.
        name: String,
        #[command(flatten)]
        args: GraphArgs,
    },
    /// What a change to a model, table or repository would affect.
    Impact {
        /// Model name, table name, repository trait, or generated `Pg*` type.
        name: String,
        #[command(flatten)]
        args: GraphArgs,
    },
}

/// Subcommands for `autumn agents`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum AgentsSubcommands {
    /// Emit the agent-authority manifest (#1691) and check it for drift.
    Manifest(AgentsManifestArgs),
}

/// Subcommands for `autumn cache`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum CacheSubcommands {
    /// Prove cached reads are never left stale by a repository write (#1716).
    ///
    /// Compiles the app, reads back the cache-coherence manifest the framework
    /// assembles from every `#[cached]` read and every `#[repository]` write it
    /// links, and exits non-zero when a mutation's model appears in a cached
    /// read's dependency set with no invalidation covering the pair — naming
    /// the read, the mutation and the shared model. This is the CI gate.
    Audit(CacheAuditArgs),
}

/// Subcommands for `autumn i18n`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum I18nSubcommands {
    /// Compare translation keys referenced in code against each `i18n/*.ftl`
    /// locale, reporting missing, untranslated, and unused keys. Exits
    /// non-zero when any locale is missing a referenced key (CI-friendly).
    Check {
        /// Output format: `text` (default) or `json`.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        format: String,
        /// Treat untranslated/unused warnings as failures (exit non-zero).
        #[arg(long)]
        strict: bool,
    },
}

/// Subcommands for `autumn a11y`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum A11ySubcommands {
    /// Statically audit raw `html!` markup for accessibility violations that
    /// bypass the typed `autumn_web::a11y` primitives. Scans project `.rs`
    /// files, reports WCAG-keyed findings, and exits non-zero when any are
    /// found (CI-friendly). Code using the typed primitives is proven at
    /// compile time and is intentionally not re-scanned.
    Verify {
        /// Project root to scan (defaults to the current directory).
        #[arg(value_name = "PATH", default_value = ".")]
        path: String,
        /// Output format: `text` (default) or `json`.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        format: String,
        /// Lower the failure threshold so any finding (Moderate and above)
        /// fails, consistent with `autumn i18n check --strict`.
        #[arg(long)]
        strict: bool,
    },
}

/// Subcommands for `autumn lifecycle`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum LifecycleSubcommands {
    /// Statically verify the soundness of every `#[lifecycle]` state machine in
    /// the project: existence of every referenced state, reachability of every
    /// state from the initial state, and that every reachable non-terminal state
    /// can reach some terminal state. Exits non-zero when any lifecycle is
    /// unsound (CI-friendly).
    ///
    /// Note: this is a best-effort source scanner — it resolves bare, qualified,
    /// and same-module-aliased `#[lifecycle]` attributes, but not cross-file or
    /// glob-reexport aliases (tracked in #1925). The compile-time typestate is
    /// the by-construction guarantee.
    Check {
        /// Project root to scan (defaults to the current directory).
        #[arg(value_name = "PATH", default_value = ".")]
        path: String,
        /// Output format: `text` (default) or `json`.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        format: String,
    },
    /// Emit a lifecycle diagram for every `#[lifecycle]` state machine, in
    /// Graphviz DOT or Mermaid `stateDiagram-v2` form (highlighting the initial
    /// and terminal states).
    Diagram {
        /// Project root to scan (defaults to the current directory).
        #[arg(value_name = "PATH", default_value = ".")]
        path: String,
        /// Diagram format: `mermaid` (default) or `dot`.
        #[arg(long, default_value = "mermaid", value_name = "FORMAT")]
        format: String,
        /// Write the diagram(s) to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<String>,
    },
}

/// Subcommands for `autumn search`.
#[derive(Subcommand)]
pub enum SearchSubcommands {
    /// Rebuild search indexes from the system of record.
    ///
    /// Runs the application binary with `AUTUMN_SEARCH_BACKFILL` set, which
    /// makes `autumn-search`'s startup hook run a full backfill and exit
    /// instead of serving traffic. Only the app knows which models are
    /// searchable and which backend/embedder are installed, so the reindex has
    /// to run inside it — the same technique `autumn jobs manifest` uses.
    ///
    /// # Examples
    ///
    ///   autumn search reindex
    ///   autumn search reindex --index articles
    ///   autumn search reindex --purge
    ///   autumn search reindex --profile prod
    #[command(verbatim_doc_comment)]
    Reindex {
        /// Index to rebuild. Omit to rebuild every registered index.
        #[arg(long)]
        index: Option<String>,

        /// Profile whose `[search]` configuration to rebuild against
        /// (`dev`, `prod`, or a custom name).
        ///
        /// The reindex runs the application binary, and that binary resolves
        /// its own `[search]` section — including `[profile.<name>.search]`.
        /// The CLI builds a DEBUG binary, which core reads as `dev` when no
        /// selector is set, so rebuilding a production index requires saying
        /// so. Forwarded as `AUTUMN_ENV`.
        #[arg(long)]
        profile: Option<String>,

        /// Clear each index before rebuilding it.
        ///
        /// Use after a schema change, when documents the source no longer
        /// produces would otherwise survive. Searches return nothing until the
        /// rebuild finishes.
        #[arg(long)]
        purge: bool,

        /// Package to run (for workspaces).
        #[arg(long)]
        package: Option<String>,

        /// Binary target to run (for packages with multiple bin targets).
        #[arg(long)]
        bin: Option<String>,
    },
}

/// Subcommands for `autumn plugin` — both plugin lanes.
///
/// `list`/`add` are consumer-facing discovery and install for a native plugin
/// crate (issue #1606); `package`/`inspect` build and review a
/// `.autumn-plugin` artifact for the capability-sandboxed lane (issue #1609).
/// The author-facing conformance gate for a native plugin stays at
/// `autumn plugin-check`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum PluginSubcommands {
    /// List installable plugins with the version compatible with this app.
    ///
    /// Covers every first-party plugin plus community crates discoverable on
    /// crates.io through the documented `autumn-plugin-<name>` convention.
    List {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Do not query crates.io; list the first-party catalog only.
        #[arg(long)]
        offline: bool,
    },
    /// Add a plugin: dependency, builder-chain mount, and post-install steps.
    Add {
        /// Plugin crate name, e.g. `autumn-admin-plugin`.
        name: String,
        /// Print what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Do not query crates.io. First-party plugins install normally;
        /// a community crate cannot have its version resolved and is refused.
        #[arg(long)]
        offline: bool,
    },
    /// Remove a plugin: dependency and builder-chain mount. Never the database.
    ///
    /// The exact reverse of `add`, and safe in the same ways: it refuses to
    /// edit a builder chain it cannot read (printing the lines to delete
    /// instead), keeps a dependency the app still names elsewhere, and never
    /// touches the database — it lists what the plugin owns there and leaves
    /// it in place unless `--drop-data` is given.
    ///
    /// Examples:
    ///   autumn plugin remove autumn-admin-plugin
    ///   autumn plugin remove autumn-media-plugin --dry-run
    ///   autumn plugin remove autumn-media-plugin --drop-data --yes
    Remove {
        /// Plugin crate name, e.g. `autumn-admin-plugin`.
        name: String,
        /// Print every file edit and every data consequence without writing
        /// anything. Exits 3 when there is something to change, 0 when there
        /// is not.
        #[arg(long)]
        dry_run: bool,
        /// Also revert the plugin's declared migrations and drop the tables it
        /// owns. Destructive and irreversible; asks for confirmation first.
        #[arg(long)]
        drop_data: bool,
        /// Answer the `--drop-data` confirmation with "yes". Required to drop
        /// data non-interactively.
        #[arg(long)]
        yes: bool,
    },

    /// Bind a manifest to a `wasm32-wasip1` module and write a
    /// `.autumn-plugin` artifact.
    ///
    /// The module's SHA-256 is computed here and stamped into the manifest, so
    /// an author never types the digest and can never ship one that describes
    /// different bytes. The module is loaded into the same sandbox the runtime
    /// uses before anything is written: an artifact that could not run is
    /// refused at the author's desk rather than at the operator's boot.
    ///
    /// # Examples
    ///
    ///   autumn plugin package --manifest plugin.toml \
    ///       --module target/wasm32-wasip1/release/plugin.wasm \
    ///       --out hello.autumn-plugin
    #[command(verbatim_doc_comment)]
    Package {
        /// The authored manifest, as TOML.
        #[arg(long, value_name = "FILE")]
        manifest: String,
        /// The `wasm32-wasip1` module the manifest describes.
        #[arg(long, value_name = "FILE")]
        module: String,
        /// Where to write the artifact.
        #[arg(long, value_name = "FILE")]
        out: String,
    },

    /// Review a `.autumn-plugin` artifact before installing it.
    ///
    /// Prints the capability grant, the routes it may serve, the module digest
    /// that was reviewed, every host function it imports, and the classes of
    /// authority the sandbox denies unconditionally. Then it loads the module
    /// into this build's sandbox and runs the same route-conformance checks
    /// `autumn plugin-check` runs against a native plugin — with no binary to
    /// build and no process to start. Exits 1 if the artifact is not fit to
    /// install.
    ///
    /// # Examples
    ///
    ///   autumn plugin inspect hello.autumn-plugin
    ///   autumn plugin inspect hello.autumn-plugin --format json
    #[command(verbatim_doc_comment)]
    Inspect {
        /// The artifact to review.
        #[arg(value_name = "ARTIFACT")]
        artifact: String,
        /// Output format: `text` (default) or `json`.
        #[arg(long, default_value = "text", value_name = "FORMAT")]
        format: String,
        /// The artifact currently installed, to review this one as an *upgrade*
        /// (issue #1632).
        ///
        /// An upgrade is the moment a plugin's authority can grow without
        /// anybody looking. With this, `inspect` prints exactly what the new
        /// manifest asks for that the approved one did not — new capabilities,
        /// new hosts, tables, job types, render slots, raised quotas — and
        /// exits non-zero when there is anything, so an unattended install
        /// stops rather than consenting on the operator's behalf.
        #[arg(long, value_name = "ARTIFACT")]
        against: Option<String>,
    },
}

/// Subcommands for `autumn jobs`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum JobsSubcommands {
    /// Emit the effective drained-queue manifest the running app declares.
    ///
    /// Compiles the application (debug profile) and runs it under
    /// `AUTUMN_DUMP_JOBS=1` to capture the ground-truth drained-queue set — the
    /// configured `[jobs.queues]` unioned with every `#[job(queue = "…")]`-declared
    /// queue — without starting the HTTP server or connecting to a database.
    /// Writes a TOML `queues = [...]` document to `<path>`, which `autumn doctor`
    /// consumes via `[jobs.fleet] manifest = "<path>"`.
    Manifest {
        /// Path to write the manifest to (e.g. `target/jobs-manifest.toml`).
        #[arg(value_name = "PATH")]
        path: String,
        /// Package to inspect (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to inspect (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
    },
}

/// Subcommands for `autumn capsule`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum CapsuleCommands {
    /// Convert one capsule into a committed regression test.
    ///
    /// Copies the capsule's bytes verbatim into `<tests-dir>/capsules/` — so
    /// whatever redaction removed stays removed — writes a `#[tokio::test]`
    /// beside it in `<tests-dir>/integration/`, registers both in that
    /// directory's `mod.rs`, and scaffolds the shared router hook the first
    /// time. The generated test runs under plain `cargo test` with no network,
    /// database or queue.
    Test {
        /// Path to the capsule JSON file to convert.
        #[arg(value_name = "CAPSULE")]
        capsule: String,
        /// Name for the generated test and fixture. Defaults to a slug of the
        /// capsule's id.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The crate's tests directory.
        #[arg(long, value_name = "DIR", default_value = "tests")]
        tests_dir: String,
        /// Overwrite an existing fixture and test of the same name.
        #[arg(long)]
        force: bool,
    },
    /// Replay the whole committed corpus.
    ///
    /// First checks every committed capsule is still readable and replayable by
    /// this build — the question an Autumn upgrade raises — then runs the
    /// generated tests with `cargo test capsule_`. An empty corpus is reported
    /// as a failure, never as a pass.
    Verify {
        /// Directory holding the committed capsules.
        #[arg(long, value_name = "DIR", default_value = "tests/capsules")]
        dir: String,
        /// Report on the corpus without running the generated tests.
        #[arg(long)]
        check_only: bool,
    },
}

/// The Autumn web framework CLI.
#[derive(Parser)]
#[command(name = "autumn", version, about = "The Autumn web framework CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Arguments for [`Commands::Upgrade`].
///
/// A separate `Args` struct rather than inline variant fields, deliberately.
/// clap's derive builds every inline field of every variant inside one
/// `Commands::augment_subcommands` frame, and with this many subcommands that
/// frame is within a kilobyte of libtest's 2 MiB thread stack — close enough
/// that a codegen difference between two rustc builds decides whether the
/// argument-parsing tests overflow. An `Args` struct moves this command's share
/// into `UpgradeArgs::augment_args`, which gets its own frame and pops.
#[allow(clippy::struct_excessive_bools)]
#[derive(clap::Args, Debug)]
struct UpgradeArgs {
    /// Project directory to migrate (defaults to the current directory).
    #[arg(value_name = "PATH", default_value = ".")]
    path: String,
    /// Release this app is upgrading from. Defaults to the `autumn-web`
    /// requirement recorded in the project's `Cargo.toml`.
    #[arg(long, value_name = "VERSION")]
    from: Option<String>,
    /// Release to upgrade to. Defaults to this CLI's own version.
    #[arg(long, value_name = "VERSION")]
    to: Option<String>,
    /// Write the rewrites. Without it the command only previews them.
    #[arg(long)]
    apply: bool,
    /// Emit the machine-readable report instead of the human one.
    #[arg(long)]
    json: bool,
    /// List the shipped app-code migrations and exit without scanning.
    #[arg(long = "list-migrations")]
    list_migrations: bool,
    /// Report framework-owned scaffold files that have drifted from this
    /// release and exit 3 if any have. Writes nothing; for CI.
    #[arg(long, conflicts_with = "apply")]
    check: bool,
    /// Record a framework-owned file as yours, so reconciliation leaves it
    /// alone from now on. Repeatable. Writes only the provenance manifest.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["apply", "check"])]
    accept: Vec<String>,
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
        /// JSON-first API starter: a lean skeleton with no HTML/CSS/Tailwind
        /// artifacts. Handlers return JSON; the view stack (maud/htmx/tailwind)
        /// is dropped. Keeps the database/migrations. Composes with --with-i18n
        /// and --with-seed; not combinable with --daemon or --bundled-pg.
        #[arg(long)]
        api: bool,
        /// Scaffold the app with this plugin already wired (repeatable).
        ///
        /// Takes the same names as `autumn plugin add`: a first-party plugin,
        /// or a community `autumn-plugin-<name>` crate. Every name is resolved
        /// and version-checked BEFORE any file is written, so an unknown or
        /// incompatible plugin leaves no half-scaffolded project behind.
        #[arg(long = "with", value_name = "PLUGIN", conflicts_with = "list_starters")]
        with: Vec<String>,
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
        /// Also compile the `#[edge]` routes into a `wasm32-wasip1` edge
        /// capsule. Release builds do this automatically when the project has
        /// `#[edge]` routes; pass `--edge` to build it from a `--debug` build
        /// too (the capsule itself is always compiled in release profile).
        /// Errors when the project has no `#[edge]` routes.
        #[arg(long)]
        edge: bool,
        /// Compile through `cargo auditable`, embedding the resolved dependency
        /// list into the binary.
        ///
        /// The binary can then report exactly which crate versions are inside
        /// it with no source tree and no lockfile — `autumn sbom --binary
        /// <path>`. Requires `cargo-auditable` on PATH (`cargo install
        /// --locked cargo-auditable`); the production Dockerfile `autumn
        /// release init` generates installs it and passes this flag.
        #[arg(long)]
        auditable: bool,
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
        /// Process role: web serves HTTP only, worker runs jobs+scheduler only,
        /// combined (default) does both.
        #[arg(long, value_enum)]
        role: Option<ServeRole>,
        /// Pin this process to a subset of job queues (issue #1623). Repeatable
        /// and comma-separated: `--pin critical,default` or `--pin critical
        /// --pin default`. A pinned process never claims jobs from queues
        /// outside the subset, on every backend. Forwarded to the app binary as
        /// `AUTUMN_JOBS__PIN`; omit it to let the app read `[jobs] pin` from its
        /// own config (the default: drain every configured queue).
        #[arg(long, value_delimiter = ',', value_name = "QUEUE")]
        pin: Vec<String>,
    },
    /// Download and configure external tools (Tailwind CSS)
    Setup {
        /// Re-download even if the binary already exists
        #[arg(long)]
        force: bool,
    },
    /// Generate or verify a `CycloneDX` Software Bill of Materials.
    ///
    /// With no flags, reads `cargo metadata` for the project in the current
    /// directory and writes a deterministic `CycloneDX` 1.5 document to stdout.
    ///
    ///   autumn sbom --output sbom.cdx.json
    ///     Write the SBOM for this source tree to a file.
    ///
    ///   autumn sbom --verify sbom.cdx.json --locked
    ///     Regenerate from the source tree and fail if the file drifted. This
    ///     is the release gate: it reports which components were added,
    ///     removed, or changed rather than a byte diff.
    ///
    ///   autumn sbom --binary /usr/local/bin/my-app
    ///     Report the exact crate versions compiled into a binary, using the
    ///     dependency list cargo-auditable embeds — no source tree, no
    ///     lockfile, no network. See docs/guide/supply-chain.md.
    Sbom {
        /// Path to a `Cargo.toml` to describe (defaults to the current directory).
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,
        /// Write the SBOM here instead of stdout.
        #[arg(long, short, value_name = "FILE", conflicts_with = "verify")]
        output: Option<PathBuf>,
        /// Regenerate and compare against this SBOM; exit non-zero if it drifted.
        #[arg(long, value_name = "FILE")]
        verify: Option<PathBuf>,
        /// Read the embedded dependency list out of an already-compiled binary.
        ///
        /// Mutually exclusive with every flag that only means something when
        /// reading a source tree — a compiled binary has no manifest, no
        /// lockfile and no feature set to resolve.
        #[arg(
            long,
            value_name = "FILE",
            conflicts_with_all = ["manifest_path", "locked", "all_features", "verify"]
        )]
        binary: Option<PathBuf>,
        /// Pass `--locked` to `cargo metadata`, failing if `Cargo.lock` is stale.
        #[arg(long)]
        locked: bool,
        /// Resolve with every optional feature enabled.
        ///
        /// Off by default: the default feature set is what a build actually
        /// links, so it is what the document should describe.
        #[arg(long, conflicts_with = "binary")]
        all_features: bool,
        /// Extra Cargo features to enable, comma-separated.
        ///
        /// Pass the same features the binary was built with, so the SBOM
        /// describes the crates that are actually linked. The generated
        /// production Dockerfile does this for `embed-assets` builds.
        #[arg(long, value_name = "FEATURES", conflicts_with = "binary")]
        features: Option<String>,
        /// Restrict resolution to one target triple.
        ///
        /// Without it the document lists target-specific dependencies for
        /// every platform — the whole `windows-*` family in a Linux image.
        /// The generated production Dockerfile passes the builder's host
        /// triple. Leave unset for a source release consumed on every
        /// platform, which is why it is not the default.
        #[arg(long, value_name = "TRIPLE", conflicts_with = "binary")]
        filter_platform: Option<String>,
        /// Require the SBOM's top-level component to be exactly this version.
        ///
        /// The release gate passes the tag being released, so an SBOM that is
        /// internally consistent but describes the wrong source tree still fails.
        #[arg(long, value_name = "VERSION")]
        expect_version: Option<String>,
    },
    /// Pin, vendor, and integrity-verify JS dependencies
    Assets {
        #[command(subcommand)]
        action: AssetsCommands,
    },
    /// Bring an app up to a release: its own code, and its scaffold files.
    ///
    /// For each release between the `autumn-web` version this app records and
    /// the target, `autumn upgrade` applies that release's machine-applyable
    /// migrations -- today, API renames -- to the app's own Rust code. In the
    /// same run it reconciles the project's framework-owned files (Dockerfile,
    /// build.rs, autumn.toml, the toolchain/style configs, the CI workflow)
    /// against the current release's scaffold. Application source under src/ is
    /// out of bounds for that half.
    ///
    /// It writes nothing by default: a bare `autumn upgrade` prints a per-file
    /// diff plus a count of affected sites, and `--apply` is the explicit write
    /// step. Anything it cannot safely rewrite (a call site inside a macro
    /// invocation, or a change with no mechanical form) is listed with its
    /// location and a link to the guide section, never guessed at. A scaffold
    /// file you have edited since it was generated is reported as a conflict
    /// with its diff, never overwritten.
    ///
    /// Run it BEFORE bumping the `autumn-web` dependency: the release it
    /// migrates *from* is the one the project manifest records. If the bump
    /// already happened, pass `--from <previous-version>`.
    ///
    ///   autumn upgrade                     # preview
    ///   autumn upgrade --apply             # write the rewrites
    ///   autumn upgrade --check             # CI gate: exit 3 on scaffold drift
    ///   autumn upgrade --accept Dockerfile # this file is mine; stop offering it
    ///   autumn upgrade --list-migrations   # what ships today
    #[allow(clippy::doc_markdown)]
    #[command(verbatim_doc_comment)]
    Upgrade(UpgradeArgs),
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
    /// Declarative schema tooling (experimental; wave-15).
    ///
    /// Reads `#[model]` structs into the shared schema IR. Slice 2 ships only
    /// the read-only `parse` action; `diff`/`snapshot`/… arrive in later slices.
    Schema {
        #[command(subcommand)]
        action: schema::SchemaAction,
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
    /// Provision the test database, migrate it, then run `cargo test`.
    ///
    /// A safety-first wrapper around `cargo test` for database-backed apps.
    /// It always runs under the test profile and exports these for the suite:
    ///
    ///   AUTUMN_ENV=test
    ///   DATABASE_URL / AUTUMN_DATABASE__PRIMARY_URL = `<resolved test URL>`
    ///
    /// Lifecycle (create → migrate → run):
    ///
    ///   1. Resolve the test database URL with the same precedence as
    ///      `autumn migrate` (autumn.toml → AUTUMN_DATABASE__* → DATABASE_URL),
    ///      defaulting the database name to `*_test` when only a base URL is
    ///      given (a bare `…/myapp` targets `…/myapp_test`).
    ///   2. Create the database if missing (existing data is left intact).
    ///   3. Run all pending app + framework migrations against it.
    ///   4. Shell out to `cargo test`, forwarding trailing args to the test
    ///      harness after `--` (like `cargo test -- <args>`), and exit with its
    ///      exit code (a failing suite fails the command).
    ///
    /// `--reset` drops and recreates the database first (clean slate for schema
    /// drift). The command refuses to run against a non-test database name.
    ///
    ///   autumn test
    ///   autumn test --reset
    ///   autumn test -- --nocapture some_test
    // Help text is shown verbatim by clap; backticks would leak into `--help`
    // output, so keep the identifiers bare and silence the doc-markdown lint.
    #[allow(clippy::doc_markdown)]
    #[command(verbatim_doc_comment)]
    Test {
        /// Drop and recreate the test database before migrating, for a clean
        /// slate (otherwise the database is created only if missing and existing
        /// data is left intact).
        #[arg(long)]
        reset: bool,
        /// Arguments forwarded to the test harness after `--` (mirroring
        /// `cargo test -- <args>`), e.g. `autumn test -- --nocapture some_test`
        /// runs `cargo test -- --nocapture some_test`.
        #[arg(
            value_name = "CARGO_TEST_ARGS",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        cargo_args: Vec<String>,
    },
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

    /// Run a pre-wired data playground against the project's database.
    ///
    /// Autumn's answer to `rails console` / `manage.py shell`. Rust has no
    /// stable `eval`, so instead of a line-by-line REPL this scaffolds an
    /// editable binary — `src/bin/playground.rs` — already wired with the same
    /// config and database-URL resolution `autumn dev` and `autumn seed` use
    /// (`AUTUMN_DATABASE__*` → `DATABASE_URL` → `autumn.toml`), a constructed
    /// async pool, and a checked-out connection. Put a query in the marked
    /// region, re-run `autumn console`, and it compiles and executes against
    /// the live database.
    ///
    /// An existing playground is never overwritten; pass `--force` to
    /// regenerate it from the template.
    ///
    /// # Examples
    ///
    ///   autumn console
    ///   autumn console --profile demo
    ///   autumn console --force
    #[command(visible_alias = "c", verbatim_doc_comment)]
    Console {
        /// Profile forwarded to the playground via `AUTUMN_ENV`
        /// (default: `dev`).
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Package to run (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Overwrite an existing playground with a fresh copy of the template.
        #[arg(long)]
        force: bool,
        /// Scaffold and wire the playground, then stop without building or
        /// running it.
        #[arg(long)]
        scaffold_only: bool,
    },

    /// Run the project's seed binary to populate the database with representative data.
    ///
    /// Requires `src/bin/seed.rs` (a Cargo binary named `seed`) to exist.
    /// If it is missing, `autumn seed` prints an actionable error and exits 1.
    ///
    /// `autumn seed` checks for pending migrations before running and exits 1
    /// if any are found — run `autumn migrate` first.
    ///
    /// A `--count`/`--model` fake-seed request against a `prod`/`production`
    /// profile is blocked unless `--yes-i-mean-prod` is also given.
    Seed {
        /// Profile forwarded to the seed binary via `AUTUMN_ENV`
        /// (default: `dev`).
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Package to run (for workspaces)
        #[arg(short, long)]
        package: Option<String>,
        /// Number of faked rows to generate. Requires --model.
        ///
        /// When both `--count` and `--model` are given, the seed binary
        /// generates and inserts that many faked rows for the model via its
        /// factory, instead of running its hand-written seed body.
        #[arg(long, requires = "model")]
        count: Option<usize>,
        /// Model to fake rows for (e.g. `Post`). Requires --count.
        #[arg(long, requires = "count")]
        model: Option<String>,
        /// Confirm generating faked rows (`--count`/`--model`) against a
        /// `prod`/`production` profile. Required to bypass the production
        /// guard; has no effect otherwise.
        #[arg(long)]
        yes_i_mean_prod: bool,
    },
    /// Convert failure capsules into committed regression tests, and check a
    /// committed corpus.
    ///
    /// `autumn replay` answers "is this bug still there?" once. This answers
    /// "can it ever come back?": the capsule is copied into `tests/capsules/`
    /// and a `#[tokio::test]` is generated beside it, so `cargo test` re-checks
    /// the failure from then on — with no network, database or queue.
    ///
    /// Nothing is committed for you: the files land in the working tree for
    /// review, and the generated router hook is scaffolded once and then left
    /// alone.
    ///
    /// # Examples
    ///
    ///   autumn capsule test tmp/autumn-capsules/01JB2K7Q.json
    ///   autumn capsule test tmp/autumn-capsules/01JB2K7Q.json --name `checkout_500`
    ///   autumn capsule verify
    #[command(verbatim_doc_comment)]
    Capsule {
        #[command(subcommand)]
        command: CapsuleCommands,
    },
    /// Replay a recorded failure capsule against the application.
    ///
    /// A capsule is written by `[failure_capture] enabled = true` whenever a
    /// request fails (a 5xx or a caught panic). Replaying one rebuilds the app
    /// offline — the clock and the database are served from the capsule, no
    /// socket is opened — drives the recorded request through it, and reports
    /// whether the failure still happens.
    ///
    /// Exit codes: 0 the failure reproduced, 1 it did not (a mismatch, or the
    /// code left the recorded database tape), 2 the capsule was refused and
    /// nothing was replayed.
    ///
    /// # Examples
    ///
    ///   autumn replay tmp/autumn-capsules/01JB2K7Q.json
    ///   autumn replay --package api tmp/autumn-capsules/01JB2K7Q.json
    #[command(verbatim_doc_comment)]
    Replay {
        /// Path to the capsule JSON file to replay.
        capsule: String,
        /// Package to run (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to run (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Profile forwarded to the app binary via `AUTUMN_ENV`.
        ///
        /// Defaults to the profile the capsule recorded, so profile-gated
        /// routes and configuration match the failing run; falls back to
        /// `dev` for capsules that recorded none.
        #[arg(long)]
        profile: Option<String>,
        /// Compile the replay binary with `cargo build --release`.
        ///
        /// Defaults to the build kind the capsule recorded, so
        /// `cfg(debug_assertions)`-gated code and release-only behaviour
        /// match the failing run; falls back to a debug build for capsules
        /// that recorded none.
        #[arg(long, conflicts_with = "debug")]
        release: bool,
        /// Compile the replay binary as a debug build, even when the capsule
        /// was recorded by a release build.
        #[arg(long)]
        debug: bool,
        /// Cargo features to compile the replay binary with (forwarded to
        /// `cargo build --features`).
        ///
        /// The capsule does not record the recording binary's feature set;
        /// pass the features the failing binary was built with when they
        /// gate code the failure depends on.
        #[arg(long, value_name = "FEATURES")]
        features: Option<String>,
        /// Compile without default features (forwarded to `cargo build`).
        #[arg(long)]
        no_default_features: bool,
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
    /// Report what declared `#[repository(..., retention(...))]` policies
    /// would sweep, without deleting anything.
    ///
    /// Validates a policy before it runs for real: every recurring sweep a
    /// declared policy registers is otherwise fully automatic (issue #1342).
    ///
    /// # Examples
    ///
    ///   autumn retention --dry-run
    ///   autumn retention --dry-run --model Session
    #[command(verbatim_doc_comment)]
    Retention {
        /// Package to run (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to run (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Profile forwarded to the app binary via `AUTUMN_ENV`.
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Report what would be swept without deleting anything. Currently
        /// required — there is no separate command to trigger a real sweep.
        #[arg(long)]
        dry_run: bool,
        /// Narrow the report to a single model's policy. Accepts either the
        /// model name or the table name; if two different modules declare
        /// same-named models with their own policy, the model name is
        /// ambiguous and the table name is required instead.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
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
    ///   decimal{precision,scale}     (NUMERIC, default {12,2})
    ///   Option<...>                  (any of the above, nullable)
    ///
    /// # Field modifiers
    ///
    ///   String{encrypted}                    at-rest encrypted column
    ///   String{encrypted:deterministic}      ...and equality-queryable
    ///
    /// `{encrypted}` emits `#[encrypted]` on the generated model field, so the
    /// column is stored as an opaque base64 ciphertext envelope (unbounded
    /// TEXT) and is redacted in the generated admin. Use
    /// `{encrypted:deterministic}` when the column still needs
    /// `find_by`/`exists_by` lookups or a UNIQUE index. Requires key material
    /// under `[active_record_encryption]` — run `autumn credentials edit`.
    ///
    /// # Example
    ///
    ///   autumn generate scaffold Post title:String body:Text published:bool
    ///   autumn generate scaffold Account 'token:String{encrypted}'
    #[command(subcommand, verbatim_doc_comment)]
    Generate(GenerateCommands),

    /// Cleanly reverse a matching `autumn generate` invocation (issue #1048).
    ///
    /// Deletes every file that invocation would have created (refusing when
    /// a targeted file's content has diverged from what `generate` would
    /// produce, unless `--force`), removes exactly the lines it inserted
    /// into shared files (`mod` declarations, `routes![]` entries,
    /// `Cargo.toml` deps/features, `schema.rs` table blocks), and prunes any
    /// now-empty generated directories.
    ///
    /// Takes the same subcommand and positional arguments as the matching
    /// `autumn generate` call — pass the identical resource name, fields,
    /// and flags (`--api`, `--live`, `--id`, `--soft-delete`, ...) so the
    /// recomputed plan matches what was originally generated.
    ///
    /// A migration directory is matched by resource-name suffix (a fresh
    /// timestamp won't match the original) and removed only when it is not
    /// yet applied to a configured database — `destroy` never touches the
    /// database itself.
    ///
    /// # Examples
    ///
    ///   autumn generate scaffold Post title:String
    ///   autumn destroy scaffold Post title:String
    ///   autumn destroy model Post title:String --dry-run
    #[command(subcommand, verbatim_doc_comment)]
    Destroy(GenerateCommands),

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
    ///   autumn release init --target azure-container-apps
    ///   autumn release init --target gcp-cloud-run
    #[command(subcommand, verbatim_doc_comment)]
    Release(ReleaseCommands),

    /// Push-button, zero-downtime deploys to a VPS or a fleet (issues #1607, #1621).
    ///
    /// Run from the project root. `check` runs a preflight against every configured
    /// host, `plan` prints the dry-run plan, `up` performs a real rolling deploy over
    /// SSH, and `rollback` returns the fleet to its previous release. Configure the
    /// target under `[deploy] host` (one server) or `[deploy] hosts` (a fleet, in
    /// rollout order) in autumn.toml.
    ///
    /// Fleet flags:
    ///   --only <HOST>   repeatable; restrict `up`/`rollback` to these hosts. A
    ///                   REPAIR LEVER: it leaves the skipped hosts on their current
    ///                   release, so the fleet may end up mixed.
    ///   --no-rollback   on `up`, halt and FREEZE a failed rollout for inspection
    ///                   instead of automatically rolling the cut-over hosts back.
    ///
    /// Fleet visibility and control (issue #1621):
    ///   status          read-only per-host state (release, readiness, maintenance,
    ///                   proxy) plus version/state drift; `--json` for machines,
    ///                   `--strict` exits non-zero on drift (cron-alertable).
    ///   maintenance     turn maintenance mode on|off on EVERY configured host over
    ///                   SSH (the local `autumn maintenance` only writes this
    ///                   machine's working directory). NOTE: maintenance does not
    ///                   drain a host from your load balancer — /ready stays 200.
    ///
    /// # Examples
    ///
    ///   autumn deploy check
    ///   autumn deploy plan
    ///   autumn deploy rollback
    ///   autumn deploy rollback --only web-2.example.com
    ///   autumn deploy up
    ///   autumn deploy up --only web-2.example.com
    ///   autumn deploy up --no-rollback
    ///   autumn deploy status --json --strict
    ///   autumn deploy maintenance on --message "Upgrading database schema"
    ///   autumn deploy maintenance off
    #[command(subcommand, verbatim_doc_comment)]
    Deploy(DeployCommands),

    /// Simulate a signed webhook request to the local application.
    #[command(subcommand, verbatim_doc_comment)]
    Webhook(WebhookCommands),

    /// Fire a synthetic operator alert through configured delivery channels.
    #[command(subcommand)]
    Alert(AlertCommands),
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
        /// Run active network probes (ACME preflight: port 80/443 reachability
        /// and DNS-points-here for the configured domains). Off by default so
        /// `doctor` stays offline and non-flaky.
        #[arg(long, alias = "preflight")]
        online: bool,
    },

    /// Inspect the project's Fluent i18n translations.
    I18n {
        #[command(subcommand)]
        action: I18nSubcommands,
    },

    /// Statically audit accessibility of raw `html!` markup at build time.
    ///
    /// The typed `autumn_web::a11y` primitives (`Img`, `Button`, `Link`,
    /// `MenuItem`, `TextField`) prove accessible-name obligations at compile
    /// time. `autumn a11y verify` covers the escape hatch they cannot see: raw
    /// markup written directly in `html!` blocks. It scans the project's `.rs`
    /// files, reports WCAG-keyed findings, and exits non-zero when any exist.
    ///
    /// # Examples
    ///
    ///   autumn a11y verify
    ///   autumn a11y verify --format json
    ///   autumn a11y verify ./crates/web --strict
    #[command(verbatim_doc_comment)]
    A11y {
        #[command(subcommand)]
        action: A11ySubcommands,
    },

    /// Verify the soundness of `#[lifecycle]` state machines and render their
    /// lifecycle diagrams at build time.
    ///
    /// The `#[lifecycle]` macro proves that transition endpoints are real
    /// variants and that only declared edges are callable. `autumn lifecycle
    /// check` closes the remaining gap by verifying the *shape* of the
    /// reachability graph: every referenced state exists, every state is
    /// reachable from the initial state, and every reachable non-terminal state
    /// can reach some terminal. Exits non-zero when any lifecycle is unsound.
    ///
    /// # Examples
    ///
    ///   autumn lifecycle check
    ///   autumn lifecycle check --format json
    ///   autumn lifecycle diagram --format mermaid
    #[command(verbatim_doc_comment)]
    Lifecycle {
        #[command(subcommand)]
        action: LifecycleSubcommands,
    },

    /// Inspect the application's background jobs.
    Jobs {
        #[command(subcommand)]
        action: JobsSubcommands,
    },

    /// Operate the application's search indexes (`autumn-search`).
    Search {
        #[command(subcommand)]
        action: SearchSubcommands,
    },

    /// Discover, install, package and review Autumn plugins.
    ///
    /// `list` shows every installable plugin with the version compatible with
    /// this app (querying crates.io for community crates unless `--offline`);
    /// `add` writes the dependency, mounts the plugin in the
    /// `autumn_web::app()` builder chain, and prints the post-install steps.
    ///
    /// `package` and `inspect` are the capability-sandboxed lane: a sandboxed
    /// plugin runs as a `wasm32-wasip1` module inside a deny-by-default
    /// sandbox, serving HTTP under the one prefix its manifest declares with no
    /// filesystem, no network, no environment and no database. See
    /// `docs/guide/sandboxed-plugins.md`.
    ///
    /// Writing a native plugin instead? `autumn generate plugin`. Auditing one
    /// you wrote? `autumn plugin-check`.
    ///
    /// # Examples
    ///
    ///   autumn plugin list
    ///   autumn plugin list --json --offline
    ///   autumn plugin add autumn-admin-plugin
    ///   autumn plugin add autumn-cache-redis --dry-run
    ///   autumn plugin package --manifest plugin.toml --module hello.wasm \
    ///       --out hello.autumn-plugin
    ///   autumn plugin inspect hello.autumn-plugin
    #[command(verbatim_doc_comment)]
    Plugin {
        /// The plugin subcommand to run.
        #[command(subcommand)]
        action: PluginSubcommands,
    },

    /// Run conformance checks against a plugin's route contributions.
    ///
    /// Compiles the application (debug profile), introspects its route table,
    /// and verifies that the named plugin satisfies eight checks: installability,
    /// route attribution, route prefix, route collision, sensitive-surface
    /// gating, duplicate registration, and — from the contract the binary dumps
    /// (issue #1601) — that the plugin declares a usable `autumn-web` range and
    /// which experimental surface it depends on.  Exits 0 on pass, 1 on failure.
    ///
    /// This is the AUTHOR-facing gate. To discover and install a plugin as a
    /// consumer, use `autumn plugin list` / `autumn plugin add`.
    ///
    /// A *sandboxed* plugin is checked with `autumn plugin inspect` instead,
    /// which runs these same checks over its manifest with no binary to build.
    /// A sandboxed plugin mounted into an app also passes this command's
    /// route-attribution and route-prefix checks unchanged.
    ///
    /// # Examples
    ///
    ///   autumn plugin-check --plugin-name autumn-admin-plugin --prefix /admin \
    ///       --sensitive-route /admin:"Role: admin required"
    ///   autumn plugin-check --plugin-name autumn-admin-plugin --deny-experimental
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
        /// Fail the run when the plugin declares any dependency on
        /// experimental plugin surface (issue #1601).
        ///
        /// Off by default: the `experimental-surface` check reports what a
        /// plugin leans on, and leaning on it is an informed choice. Set this
        /// in a plugin's own CI to forbid it.
        #[arg(long)]
        deny_experimental: bool,
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
    /// LOCAL only: the flag lands in THIS working directory. For servers
    /// managed by `autumn deploy` (`[deploy] host`/`hosts`), use
    /// `autumn deploy maintenance on|off`, which fans the same flag out to
    /// every host over SSH (issue #1621).
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

    /// Agent-authority tooling — what an agent-operable handler may do (#1691).
    ///
    /// `autumn agents manifest` compiles the application, reads back the
    /// manifest the framework assembles from every `#[agent_operable]` action
    /// and every declared `authority_grant!`, joins it against the route table,
    /// and writes the diffable record. `--check` is the CI gate: it fails on
    /// drift, on an MCP-exposed *mutating* tool with no envelope, on a binary
    /// that can act irreversibly with no audit sink, and on a route naming an
    /// authority nothing registered — none of which the compiler can catch,
    /// because a tool with no grant has no assertion to fail.
    ///
    /// # Examples
    ///
    ///   autumn agents manifest
    ///   autumn agents manifest --manifest agent-authority.json
    ///   autumn agents manifest --check agent-authority.json --release
    #[command(subcommand, verbatim_doc_comment)]
    Agents(AgentsSubcommands),

    /// Cache-coherence tooling — prove no write can leave a cached read stale.
    ///
    /// `autumn cache audit` compiles the application, reads back the
    /// cache-coherence manifest the framework assembles from every `#[cached]`
    /// read and every `#[repository]` write it links, and exits non-zero when a
    /// write can strand a cached value with no invalidation covering the pair.
    ///
    /// # Examples
    ///
    ///   autumn cache audit
    ///   autumn cache audit --manifest target/cache-coherence.json
    ///   autumn cache audit --strict -p blog
    #[command(subcommand, verbatim_doc_comment)]
    Cache(CacheSubcommands),
    /// Emit the classified-data flow manifest (#1654).
    ///
    /// Compiles the app and reads back the manifest the framework assembles from
    /// every `#[classified]` column and every declared declassification
    /// boundary: one row per classified column, listing every sink it is proven
    /// reachable to. An empty reachable set means the column cannot leave the
    /// process through a gated sink. The compiler is the gate; this is the
    /// diffable record, and `--check` fails when it drifts from the committed
    /// copy.
    #[command(name = "data-flow")]
    DataFlow(DataFlowArgs),

    /// Query the application's architecture graph (#1747).
    ///
    /// Compiles the app and reads back the graph the framework derives from its
    /// macros: a node for every `#[route]`/`#[static_get]`, `#[model]`,
    /// `#[repository]` and `#[job]`/`#[scheduled]`/`#[task]`, and an edge for
    /// every repository→model declaration and every model, table or repository
    /// a route or job names. Because the elements are declared through macros
    /// autumn owns, no declared element can be missing.
    ///
    /// # Examples
    ///
    ///   autumn graph show
    ///   autumn graph touches posts
    ///   autumn graph impact Post
    ///   autumn graph show --manifest architecture-graph.json
    ///   autumn graph show --check architecture-graph.json --release
    #[command(subcommand, verbatim_doc_comment)]
    Graph(GraphSubcommands),

    /// Derive and enforce this build's capacity contract (issue #1733).
    ///
    /// Builds the app in release mode, reads its route graph, walks a seeded
    /// concurrency ladder against it, and records the saturation envelope in
    /// `capacity.lock`. With `--check`, compares a rebuild against the
    /// committed contract instead of writing one — the CI gate.
    Calibrate {
        /// Package to calibrate (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to calibrate (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Path of the capacity contract to write, or to check against.
        #[arg(long, default_value = autumn_web::capacity::CONTRACT_FILE_NAME, value_name = "PATH")]
        contract: String,
        /// Gate mode: fail with a diff when this build regresses beyond
        /// tolerance versus the committed contract. Writes nothing.
        #[arg(long)]
        check: bool,
        /// Autumn profile to calibrate under — the configuration the contract
        /// will govern.
        /// [default: prod; with --check, the committed contract's own profile]
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Cargo features for the calibrated build (repeatable; comma- or
        /// space-separated inside one). Measure the binary you deploy.
        #[arg(long, value_name = "FEATURES")]
        features: Vec<String>,
        /// Build the calibrated binary with `--all-features`.
        #[arg(long)]
        all_features: bool,
        /// Build the calibrated binary with `--no-default-features`.
        #[arg(long)]
        no_default_features: bool,
        /// Drive load against these paths instead of the discovered ones
        /// (repeatable). Use when a route needs query parameters or headers
        /// the driver cannot invent.
        #[arg(long = "target", value_name = "PATH")]
        targets: Vec<String>,
        /// Seed for the request profile, so a calibration is replayable.
        /// [default: 1733; with --check, the committed contract's own seed]
        #[arg(long, value_name = "SEED")]
        seed: Option<u64>,
        /// Concurrency ladder to walk (comma-separated).
        /// [default: 1,2,4,8,16,32,64; with --check, the committed ladder]
        #[arg(long, value_delimiter = ',', value_name = "N")]
        concurrency: Vec<usize>,
        /// Milliseconds to hold each rung of the ladder.
        /// [default: 2000; with --check, the committed value]
        #[arg(long, value_name = "MS")]
        rung_ms: Option<u64>,
        /// Milliseconds of discarded warmup before the ladder.
        /// [default: 1000; with --check, the committed value]
        #[arg(long, value_name = "MS")]
        warmup_ms: Option<u64>,
        /// Measurements per rung; the median is recorded. Raise it on a noisy
        /// machine.
        /// [default: 3; with --check, the committed value]
        #[arg(long, value_name = "N")]
        runs: Option<u32>,
        /// Fractional sustained-throughput drop `--check` tolerates.
        #[arg(long, default_value_t = crate::capacity::DEFAULT_RPS_TOLERANCE, value_name = "FRACTION")]
        tolerance_rps: f64,
        /// Fractional P99-latency rise `--check` tolerates.
        #[arg(long, default_value_t = crate::capacity::DEFAULT_P99_TOLERANCE, value_name = "FRACTION")]
        tolerance_p99: f64,
        /// Also emit the measured contract as JSON on stdout.
        #[arg(long)]
        json: bool,
    },
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
        /// Optional subcommand (e.g. `audit`). When omitted, lists routes.
        #[command(subcommand)]
        command: Option<RoutesSubcommands>,
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
        /// Measure the overload / load-shedding Success Metric (issue #1006):
        /// offered load = `--load-multiplier` x `--ceiling` against handlers
        /// that block `--block-ms`, asserting admitted-request p99 stays
        /// within budget, shedding is fast, and RSS stays bounded.
        #[arg(long)]
        overload: bool,
        /// Concurrent in-flight ceiling (`server.max_concurrent_requests`)
        /// configured on the scaffolded app. Only used with `--overload`.
        #[arg(long, default_value = "64")]
        ceiling: usize,
        /// How long (ms) the scaffolded app's benchmark handler blocks.
        /// Only used with `--overload`.
        #[arg(long, default_value = "200")]
        block_ms: u64,
        /// Offered load during the overload phase, as a multiple of
        /// `--ceiling`. Only used with `--overload`.
        #[arg(long, default_value = "2")]
        load_multiplier: u32,
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
    /// Back up the configured database(s) to a timestamped, compressed artifact.
    ///
    /// Captures the control database plus every configured shard (or a single
    /// `--shard`), resolving the connection exactly like `autumn migrate`. The
    /// default `custom` format is compressed and integrity-checked with
    /// `pg_restore --list` before success is reported; a partial/empty artifact
    /// is removed and the command exits non-zero. For managed-Postgres apps the
    /// bundled `pg_dump`/`pg_restore` are used — no externally installed tools.
    ///
    /// Artifacts are written to `<dir>/<profile>/<timestamp>/` (default
    /// `./backups`), each run self-described by a `manifest.json`.
    ///
    /// # Scheduling recipe
    ///
    /// cron (daily 02:00, keep 7):
    ///
    ///   0 2 * * *  cd /srv/myapp && AUTUMN_ENV=prod autumn db backup --keep 7
    ///
    /// systemd timer:
    ///
    ///   # myapp-backup.service
    ///   `[Service]`
    ///   Type=oneshot
    ///   Environment=AUTUMN_ENV=prod
    ///   WorkingDirectory=/srv/myapp
    ///   ExecStart=/usr/local/bin/autumn db backup --keep 7
    ///
    ///   # myapp-backup.timer
    ///   `[Timer]`
    ///   OnCalendar=*-*-* 02:00:00
    ///   Persistent=true
    ///   `[Install]`
    ///   WantedBy=timers.target
    // The scheduling recipe above is shell/unit-file text shown verbatim in
    // `--help`; backticking every `KEY=value` token would leak into that output.
    #[allow(clippy::doc_markdown)]
    #[command(verbatim_doc_comment)]
    Backup {
        /// Resolve the connection through a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Directory to write backup run directories under (default: `./backups`).
        #[arg(long, value_name = "DIR")]
        dir: Option<std::path::PathBuf>,
        /// Artifact format: `custom` (compressed, default) or `plain` (SQL text).
        #[arg(long, value_name = "FORMAT", default_value = "custom")]
        format: String,
        /// Retention: keep only the newest N run directories, pruning older ones
        /// after a successful backup so a schedule can't fill the disk.
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
        /// Back up only this shard (by configured name), mirroring `migrate --shard`.
        #[arg(long, value_name = "NAME", conflicts_with = "control_only")]
        shard: Option<String>,
        /// Back up only the control database (skip shards).
        #[arg(long)]
        control_only: bool,
        /// After local verification + prune, upload the run to the configured
        /// offsite destination ([backup.offsite]) and verify each remote object
        /// (issue #1619). Also enabled by backup.offsite.auto_upload = true.
        #[arg(long)]
        upload: bool,
    },
    /// Restore the configured database(s) from a backup artifact.
    ///
    /// ARTIFACT is a backup run directory (with `manifest.json`) or a single
    /// `.dump`/`.sql` file. Every artifact's integrity is verified before any
    /// database is touched, and the restore is gated by the same production
    /// guard as `autumn db drop` (refuses non-dev/test profiles without `--force`).
    #[command(verbatim_doc_comment)]
    Restore {
        /// Path to the backup run directory or artifact file to restore — or an
        /// offsite reference `offsite:<profile>/<timestamp|latest>` (see
        /// `--offsite`).
        #[arg(value_name = "ARTIFACT")]
        artifact: std::path::PathBuf,
        /// Resolve the connection through a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Allow the restore against a non-dev/test (e.g. production) profile.
        #[arg(long)]
        force: bool,
        /// Restore only this shard from the artifact.
        #[arg(long, value_name = "NAME")]
        shard: Option<String>,
        /// Restore from the offsite destination: interpret ARTIFACT as
        /// `<profile>/<timestamp|latest>` and download the run before restoring
        /// (issue #1619). An `offsite:` prefix on ARTIFACT implies this.
        #[arg(long)]
        offsite: bool,
    },
    /// Anonymize a database (or a backup artifact) for non-production use.
    ///
    /// Rewrites every PII-classified column with deterministic, constraint-valid
    /// fake values so a production copy is safe on a laptop or a shared staging
    /// box. Classification is fail-closed: `#[encrypted]` model columns and
    /// tables registered with the GDPR anonymize strategy are classified
    /// automatically, everything else must be declared in `scrub.toml`, and a
    /// column that is neither PII nor explicitly `safe` aborts the scrub — so a
    /// newly added column can never silently pass through with real data.
    ///
    /// Refuses to run outside the `dev`/`test` profile without `--force`, the
    /// same guard as `autumn db drop`.
    ///
    /// # Examples
    ///
    ///   # Refresh staging from a production backup:
    ///   `AUTUMN_ENV=staging` autumn db scrub --artifact backups/prod/latest-run --force
    ///
    ///   # Prove the classification is complete (CI):
    ///   autumn db scrub --check
    #[command(verbatim_doc_comment)]
    Scrub {
        /// Resolve the connection through a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Restore this backup run directory (or artifact file) into the
        /// resolved database(s) before scrubbing.
        #[arg(long, value_name = "ARTIFACT")]
        artifact: Option<std::path::PathBuf>,
        /// After a successful scrub, write a fresh (scrubbed) backup run here.
        #[arg(long, value_name = "DIR")]
        output: Option<std::path::PathBuf>,
        /// Path to the PII declaration file (default: `./scrub.toml`).
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        /// Classify only: report the plan (or the unclassified columns) and
        /// write nothing. Exits non-zero when any column is unclassified.
        #[arg(long, conflicts_with_all = ["artifact", "output", "dry_run"])]
        check: bool,
        /// Print the exact SQL the scrub would run and write nothing.
        #[arg(long, conflicts_with_all = ["artifact", "output"])]
        dry_run: bool,
        /// Allow the scrub against a non-dev/test (e.g. production) profile.
        #[arg(long)]
        force: bool,
        /// Allow writing over the database an artifact's own (non-dev/test)
        /// profile config file declares. Separate from `--force`, which the
        /// staging drill always passes.
        #[arg(long)]
        allow_source_overwrite: bool,
        /// Emit a referentially-intact SUBSET instead of the whole copy, rooted
        /// on this many rows of TABLE: `--sample users=1%` or
        /// `--sample users=500`. Repeatable. Every row the selected roots
        /// relate to is carried along, so every foreign key still resolves; the
        /// subset is scrubbed in the same pass. Per-table `always_include` /
        /// `never_include` rules live in `[sample]` in `scrub.toml`.
        #[arg(long, value_name = "TABLE=COUNT|PERCENT%")]
        sample: Vec<String>,
        /// The seed `--sample` derives its row selection from. The same seed
        /// against the same source data reproduces the identical subset, so a
        /// teammate can rebuild the exact rows that exhibit a bug.
        #[arg(long, value_name = "N", default_value_t = 0, requires = "sample")]
        seed: u64,
    },
    /// Report, dry-run, or enforce the retention policy for framework-owned data.
    ///
    /// Autumn creates and fills persistent stores your app never asked for —
    /// the job queue and its tracking records, idempotency replay records,
    /// sticky experiment assignments, webhook replay markers, sessions, audit
    /// archives. This reports every one of them: the retention window in
    /// effect, which setting produced it, how it is enforced, and how many
    /// rows are eligible for purge right now.
    ///
    /// Windows are declared in the `[retention]` section of `autumn.toml` and
    /// are enforced automatically on a recurring in-process sweep — this
    /// command is for inspecting the policy and for running it on demand, not
    /// a cron replacement.
    ///
    /// Runs your application binary (compiling it first if needed) so the
    /// report reflects the app's own resolved config, GDPR legal holds, and
    /// audit sinks.
    ///
    /// # Examples
    ///
    ///   # What is kept, and how much is eligible right now:
    ///   autumn db retention
    ///
    ///   # What a sweep would delete, without deleting it:
    ///   autumn db retention --dry-run
    ///
    ///   # Enforce the policy now, for one dataset:
    ///   autumn db retention --purge --dataset `job_history`
    #[command(verbatim_doc_comment)]
    Retention {
        /// Package to run (for workspaces).
        #[arg(short, long)]
        package: Option<String>,
        /// Binary target to run (for packages with multiple bin targets).
        #[arg(long, value_name = "BIN")]
        bin: Option<String>,
        /// Profile forwarded to the app binary via `AUTUMN_ENV`.
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Restrict to one dataset. Rejected up front if it is not one of the
        /// framework-owned dataset keys, so a typo cannot silently sweep
        /// nothing.
        #[arg(long, value_name = "DATASET", value_parser = RETENTION_DATASET_KEYS)]
        dataset: Option<String>,
        /// Report what a sweep would remove, without removing anything.
        #[arg(long, conflicts_with = "purge")]
        dry_run: bool,
        /// Enforce the configured policy immediately.
        ///
        /// Deletes data. Against a non-dev/test profile this additionally
        /// requires `--force`, the same guard `autumn db drop` and
        /// `autumn db scrub` apply.
        #[arg(long)]
        purge: bool,
        /// Allow `--purge` against a non-dev/test (e.g. production) profile.
        #[arg(long)]
        force: bool,
        /// Print the raw JSON report instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Inspect the offsite backup destination ([backup.offsite], issue #1619).
    #[command(subcommand)]
    Offsite(OffsiteCommands),
    /// Restore, inspect or verify a continuously replicated `SQLite` database.
    ///
    /// `[replication]` ships this app's `SQLite` write-ahead log to an offsite
    /// destination as it is written (issue #1628). These commands are the other
    /// half: rebuilding the database on a fresh machine that has nothing but
    /// this binary, autumn.toml and the destination credentials.
    ///
    /// # Examples
    ///
    ///   # Fresh box, latest replicated state:
    ///   autumn db replica restore --profile prod
    ///
    ///   # Point-in-time, over the existing database:
    ///   autumn db replica restore --timestamp 2026-09-02T14:29:00Z --force --overwrite
    ///
    ///   # How fresh is the replica right now?
    ///   autumn db replica status
    #[command(subcommand, verbatim_doc_comment)]
    Replica(ReplicaCommands),
}

/// Subcommands for `autumn db offsite` (issue #1619).
#[derive(Subcommand)]
enum OffsiteCommands {
    /// List offsite backups for the active profile (timestamp, size, files).
    List {
        /// Resolve the destination under a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },
}

/// Subcommands of `autumn db replica` (issue #1628).
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
enum ReplicaCommands {
    /// Rebuild the database from the replica, optionally at a point in time.
    ///
    /// Verifies the whole chain before anything is written: a hole in the
    /// segment sequence, a payload whose digest does not match, or a rebuilt
    /// database that fails `PRAGMA integrity_check` is refused rather than
    /// restored. Gated by the same production guard as `autumn db restore`, and
    /// overwriting an existing database always needs `--force`.
    #[command(verbatim_doc_comment)]
    Restore {
        /// Resolve the destination under a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Restore to this RFC 3339 instant instead of the latest state.
        #[arg(long, value_name = "RFC3339")]
        timestamp: Option<String>,
        /// Write the database here instead of the configured database.url.
        ///
        /// A restore to an explicit path writes nothing the app uses, so it is
        /// not subject to the production guard (the overwrite guard still applies).
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        /// Allow the restore against a non-dev/test (e.g. production) profile.
        #[arg(long)]
        force: bool,
        /// Allow replacing a database file that already exists.
        ///
        /// Separate from `--force`, which is about the profile: a drill that
        /// always passes `--force` must not silently also destroy a database.
        #[arg(long)]
        overwrite: bool,
    },
    /// Report the replica's current generation, segment count and lag.
    Status {
        /// Resolve the destination under a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Print the report as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Prove the replica restorable by restoring it into a scratch directory.
    Verify {
        /// Resolve the destination under a profile overlay (see `db create`).
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },
}

impl ReplicaCommands {
    /// Translate the parsed CLI shape into the `db::replica` command.
    fn into_command(self) -> db::replica::ReplicaCommand {
        match self {
            Self::Restore {
                profile,
                timestamp,
                output,
                force,
                overwrite,
            } => db::replica::ReplicaCommand::Restore {
                profile,
                timestamp,
                output,
                force,
                overwrite,
            },
            Self::Status { profile, json } => db::replica::ReplicaCommand::Status { profile, json },
            Self::Verify { profile } => db::replica::ReplicaCommand::Verify { profile },
        }
    }
}

impl DbCommands {
    /// Translate a lifecycle subcommand (`create`/`drop`/`reset`) into the `db`
    /// module's command and the optional profile override the connection should
    /// be resolved under. `pull`/`backup`/`restore`/`scrub` are dispatched
    /// separately (they do not map onto [`db::DbCommand`]).
    fn into_command(self) -> (db::DbCommand, Option<String>) {
        match self {
            Self::Create { profile } => (db::DbCommand::Create, profile),
            Self::Drop { profile, force } => (db::DbCommand::Drop { force }, profile),
            Self::Reset { profile, force } => (db::DbCommand::Reset { force }, profile),
            Self::Pull { .. }
            | Self::Backup { .. }
            | Self::Restore { .. }
            | Self::Scrub { .. }
            | Self::Retention { .. }
            | Self::Offsite(_)
            | Self::Replica(_) => {
                unreachable!(
                    "db pull/backup/restore/scrub/retention/offsite/replica are dispatched \
                     before into_command"
                )
            }
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

/// Process role selector for `autumn serve --role`.
///
/// Mirrors `autumn_web::config::ProcessRole`: `web` serves HTTP only, `worker`
/// runs job workers + the cron scheduler only, and `combined` (the default)
/// does both. The chosen role is forwarded to the app binary via `AUTUMN_ROLE`.
#[derive(Clone, Copy, ValueEnum)]
enum ServeRole {
    Combined,
    Web,
    Worker,
}

impl ServeRole {
    /// Stable lowercase identifier forwarded to the app via `AUTUMN_ROLE`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Combined => "combined",
            Self::Web => "web",
            Self::Worker => "worker",
        }
    }
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
    /// Record content hashes for applied migrations (issue #1203).
    ///
    /// Content hashes (SHA-256 of each migration's `up.sql`) live in the
    /// `autumn_migration_checksums` table and are validated before every
    /// `autumn migrate` run so a migration that was edited after being
    /// applied fails loudly instead of silently forking the schema.
    ///
    /// # Examples
    ///
    ///   # Backfill hashes for legacy migrations applied before the checksum
    ///   # table existed. Idempotent — safe to re-run.
    ///   autumn migrate baseline
    ///
    ///   # Escape hatch: overwrite one version's stored hash with the current
    ///   # on-disk hash. Use ONLY when you deliberately edited an applied
    ///   # migration and accept that other environments running the previous
    ///   # content will now report a mismatch.
    ///   autumn migrate baseline --force 20260101000000
    #[command(verbatim_doc_comment)]
    Baseline {
        /// Re-baseline the checksum for a single applied version, overwriting
        /// whatever hash is currently recorded. The escape hatch for a
        /// deliberate edit. Without this flag, `baseline` only records
        /// hashes for applied migrations that don't already have one.
        #[arg(long = "force", value_name = "VERSION")]
        force: Option<String>,
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

/// Subcommands for `autumn alert`.
#[derive(Subcommand)]
enum AlertCommands {
    /// Send a synthetic test alert through each configured delivery channel and
    /// report per-channel success or an actionable error (issue #1630).
    ///
    /// Exercises the outbound-HTTP transports the runtime installs —
    /// `PagerDuty`, Slack, Discord, and the generic signed webhook — using the
    /// exact same channel implementations, so a green run proves real wiring
    /// before an incident. Reads the effective `[alerts]` config (env vars and
    /// profiles honoured, just like the server).
    Test {
        /// Only fire through the named channel (pagerduty, slack, discord,
        /// webhook). Omit to fire through every configured channel.
        #[arg(long)]
        channel: Option<String>,
    },
}

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
        /// Event type to announce, for the providers that carry it in a header
        /// (github: `X-GitHub-Event`, generic: `X-Webhook-Event`).
        ///
        /// Defaults to `sim.event`, which no real handler dispatches on — pass
        /// the event your handler expects (e.g. `--event push`) to exercise it.
        /// Stripe and Slack read their event type from the payload's `type`
        /// field instead, so this is ignored for those two.
        #[arg(long, value_name = "TYPE")]
        event: Option<String>,
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
    /// --target fly                    : also emits fly.toml.
    /// --target docker-compose         : also emits docker-compose.yml with app + Postgres.
    /// --target azure-container-apps   : also emits main.tf, variables.tf, outputs.tf,
    ///                                   terraform.tfvars.example, and
    ///                                   .github/workflows/azure-deploy.yml.
    /// --target aws-app-runner         : also emits main.tf, variables.tf, outputs.tf, and
    ///                                   terraform.tfvars.example (ECR + App Runner + RDS,
    ///                                   no CI workflow — fast/minimal path).
    /// --target aws-ecs                : also emits main.tf, variables.tf, outputs.tf,
    ///                                   terraform.tfvars.example, and
    ///                                   .github/workflows/aws-deploy.yml (VPC/ALB/ECS
    ///                                   Fargate/RDS — production path).
    /// --target gcp-cloud-run          : also emits main.tf, variables.tf, outputs.tf,
    ///                                   terraform.tfvars.example, and
    ///                                   .github/workflows/gcp-deploy.yml (Artifact
    ///                                   Registry + Cloud Run + Cloud SQL behind a VPC
    ///                                   connector, opt-in Memorystore Redis).
    Init {
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
        /// Deployment target: fly | docker-compose | azure-container-apps | aws-app-runner |
        /// aws-ecs | gcp-cloud-run (omit for bare Dockerfile).
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        /// Scaffold a separate worker-role service in the generated
        /// docker-compose.yml (opt-in split topology). The `app` service runs
        /// the web role and a new `worker` service runs jobs+scheduler, both on
        /// the shared `postgres` jobs backend. Only affects `--target
        /// docker-compose`; the default output is a single combined service.
        #[arg(long)]
        split_workers: bool,
    },
}

/// Subcommands for `autumn deploy`.
#[derive(Subcommand)]
enum DeployCommands {
    /// Run the deploy preflight and report pass/fail.
    ///
    /// Checks SSH reachability, signing-secret presence, database URL, and that
    /// `migrate check` is clean. Exits non-zero if any check fails. Also runs
    /// (config-gated) as a section of `autumn doctor`.
    Check,

    /// Print the systemd unit and the ordered zero-downtime deploy plan.
    ///
    /// Pure dry-run — renders the plan without touching anything remote.
    Plan,

    /// Run the preflight, then perform a REAL on-demand rollback over SSH.
    ///
    /// Resolves the previous release on the target, brings its slot back up,
    /// flips the proxy back to it, repoints `current`, and re-probes `/ready`.
    /// Fails loudly (non-zero) when there is no previous release to roll back to.
    ///
    /// With `[deploy] hosts` this rolls back EVERY host, newest first, continuing
    /// past a host that fails (each is reported) and exiting non-zero if any host
    /// did not come back.
    Rollback {
        /// Roll back only this host; repeat the flag for several (issue #1621).
        ///
        /// Each value must appear in `[deploy] hosts`. The hosts left out keep
        /// running whatever they are running now, so the fleet may end up mixed.
        #[arg(long, value_name = "HOST")]
        only: Vec<String>,
    },

    /// Run the preflight, then perform a REAL deploy over SSH.
    ///
    /// Aborts before touching the server if preflight fails, then uploads the
    /// `autumn build --embed` release binary, writes the (0600) env file and the
    /// systemd unit, enables the service, and gates on `/ready` — a first deploy
    /// installs the proxy and stands the release up behind it; a redeploy runs a
    /// zero-downtime cutover and auto-rolls-back the candidate on a pre-cutover
    /// failure.
    ///
    /// With `[deploy] hosts` the hosts are replaced ONE AT A TIME in declaration
    /// order, migrations run exactly once, and a mid-rollout failure halts the
    /// rollout and rolls the hosts that already cut over back.
    Up {
        /// Deploy only this host; repeat the flag for several (issue #1621).
        ///
        /// Each value must appear in `[deploy] hosts`. A REPAIR LEVER, not a faster
        /// deploy: the skipped hosts keep their current release, so finish with a
        /// full `autumn deploy up` to converge the fleet.
        #[arg(long, value_name = "HOST")]
        only: Vec<String>,

        /// Halt and FREEZE a failed rollout instead of rolling the cut-over hosts
        /// back (issue #1621).
        ///
        /// Every host is left exactly as it is — including the ones already on the
        /// new release — and named in the final state table, so the failure can be
        /// inspected before anything else moves.
        #[arg(long)]
        no_rollback: bool,
    },

    /// Report every configured host's deploy state, read-only (issue #1621).
    ///
    /// One row per `[deploy] hosts` entry: mode, deployed release (from the
    /// `current` symlink), live slot, /ready status, maintenance flag, and proxy
    /// port — plus version drift (hosts on different releases) and state drift
    /// (per-host marker damage that will fail the NEXT deploy closed). Touches
    /// nothing; safe mid-incident.
    Status {
        /// Emit the stable JSON report on stdout instead of the table.
        #[arg(long)]
        json: bool,
        /// Exit non-zero when ANY drift is detected, so drift is alertable from
        /// cron. The default exits 0 — status is a report, not a judgement.
        #[arg(long)]
        strict: bool,
    },

    /// Fleet-wide maintenance mode over SSH (issue #1621).
    ///
    /// Applies to the DEPLOY-CONFIGURED host(s) — `[deploy] host` or `[deploy]
    /// hosts` — unlike the top-level `autumn maintenance`, which only writes this
    /// machine's own working directory. Best-effort: every host is attempted, the
    /// per-host table names what changed, and the command exits non-zero if any
    /// host failed (the changed hosts are NOT reversed).
    ///
    /// Maintenance mode does NOT drain a host from your load balancer: /ready
    /// stays 200 by design, so a maintained host keeps taking traffic and answers
    /// it with 503. Drain at the load balancer if you need a host out of rotation.
    #[command(subcommand)]
    Maintenance(DeployMaintenanceCommands),
}

/// Subcommands for `autumn deploy maintenance` (issue #1621).
#[derive(Subcommand)]
enum DeployMaintenanceCommands {
    /// Enable maintenance mode on every configured deploy host.
    ///
    /// Writes the same flag file the local `autumn maintenance on` writes, to the
    /// per-app shared dir on each host (and, for hosts still running pre-#1621
    /// units, to the current release's tmp/ dir), so running apps react within
    /// 500 ms without a restart.
    ///
    /// # Examples
    ///
    ///   autumn deploy maintenance on
    ///   autumn deploy maintenance on --message "Upgrading database schema"
    ///   autumn deploy maintenance on --readonly
    ///   autumn deploy maintenance on --allow-ips 10.0.0.0/8 --bypass-header X-Dev-Bypass:mytoken
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
    /// Disable maintenance mode on every configured deploy host.
    ///
    /// Removes the flag file(s); a host where maintenance was already off is a
    /// success, not an error.
    Off,
}

/// Subcommands for `autumn generate`.
#[derive(Subcommand)]
enum GenerateCommands {
    /// Generate a `#[model]` struct, Diesel migration, and schema entry.
    ///
    /// Field types: String, Text, i32, i64, bool, f32, f64, Uuid, `NaiveDateTime`,
    /// `DateTime`, `Vec<u8>`/Bytea, Attachment, references, `enum{a,b,...}`, `Option<...>`.
    ///
    /// `field:references` scaffolds a foreign key: a `field_id BIGINT` column
    /// with a `REFERENCES <table>(id)` constraint and an index, where `<table>`
    /// is the pluralised form of `field`. Append `?` for a nullable FK
    /// (`post:references?` -> `post_id: Option<i64>`).
    ///
    /// `field:enum{a,b,c}` scaffolds a closed-set column: a generated Rust
    /// enum (`PascalCase` variants) stored as `TEXT` with a `CHECK` constraint
    /// enumerating the allowed values, plus `--default field=variant` support.
    /// Quote the token in bash/zsh — an unquoted `enum{a,b}` is brace-expanded
    /// by the shell before `autumn` ever sees it.
    ///
    /// `field:Type:unique` (e.g. `email:String:unique`) scaffolds a `CREATE
    /// UNIQUE INDEX` for the column, distinct from the plain, non-unique
    /// `--index`/`--unique` output. `--unique FIELD` is the flag-based
    /// equivalent, mirroring `--index`'s ergonomics.
    ///
    /// `field:String:states(from -> to, from -> to: guard, ...)` declares a
    /// state machine on a non-nullable `String`/`Text` field: it emits a
    /// `#[state_machine(transitions(...))]` attribute so the model gains
    /// `transition_field_to`/`can_transition_field_to` guard-checked methods.
    /// Each `from -> to` edge may carry an optional `: guard` plain method
    /// name. Quote the token in bash/zsh so the shell doesn't split it.
    ///
    /// `field:String{encrypted}` stores the column encrypted at rest: it emits
    /// `#[encrypted]` on the model field, so the value is a base64 AES-256-GCM
    /// envelope on disk and a plain `String` in Rust. Use
    /// `{encrypted:deterministic}` when the column still needs
    /// `find_by`/`exists_by` lookups or a UNIQUE index — randomized ciphertext
    /// can never match an equality predicate, so those combinations are
    /// refused at generate time. `String`/`Text` and non-null only. Requires
    /// key material under `[active_record_encryption]`: run
    /// `autumn credentials edit`. Quote the token in bash/zsh (brace expansion).
    ///
    /// Examples:
    ///
    ///   autumn generate model Post title:String body:Text published:bool
    ///   autumn generate model Comment body:Text post:references
    ///   autumn generate model Post 'status:enum{draft,published,archived}'
    ///   autumn generate model User email:String:unique
    ///   autumn generate model Account 'token:String{encrypted}'
    ///   autumn generate model Page 'status:String:states(draft -> published, published -> archived)'
    #[command(verbatim_doc_comment)]
    Model {
        /// Resource name (`PascalCase` or `snake_case`, e.g. `Post`).
        name: String,
        /// Field DSL tokens, each `name:Type`.
        fields: Vec<String>,
        /// Add a `CREATE UNIQUE INDEX` for this field. Repeatable. Mirrors
        /// the DSL's inline `:unique` modifier (`email:String:unique`).
        #[arg(long, value_name = "FIELD")]
        unique: Vec<String>,
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
    /// matching `ALTER TABLE` statements automatically. Accepts the same
    /// field DSL as `generate model`/`scaffold`, including `enum{a,b,c}`
    /// (emits `TEXT` + a `CHECK` constraint) and `:unique` (emits a `CREATE
    /// UNIQUE INDEX`) — quote the token in bash/zsh.
    Migration {
        /// Migration name (`PascalCase` or `snake_case`).
        name: String,
        /// Field DSL tokens — only used for `Add…To…` / `Remove…From…` names.
        fields: Vec<String>,
        /// Add a `CREATE UNIQUE INDEX` for this field. Repeatable. Mirrors
        /// the DSL's inline `:unique` modifier (`email:String:unique`).
        #[arg(long, value_name = "FIELD")]
        unique: Vec<String>,
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
    /// Generate a record-level authorization `Policy` and companion `Scope`
    /// for an existing model.
    ///
    /// Creates:
    ///   - `src/policies/<snake>.rs` — `<Pascal>Policy` (`Policy<<Pascal>>`) and
    ///     `<Pascal>Scope` (`Scope<<Pascal>>`)
    ///   - `src/policies/mod.rs`     — created/updated with `pub mod`
    ///   - `src/main.rs`             — `mod policies;` + `.policy(...)`/`.scope(...)`
    ///     wired into the app builder
    ///
    /// When an owner column (`user_id`, `author_id`, or `owner_id`) is present,
    /// the generated `can_update`/`can_delete` allow the record owner or an
    /// `admin`, and the scope filters lists to the current user's rows.
    /// Otherwise those default-deny with a `TODO` marker.
    ///
    /// Requires the target model to already exist (`src/models/<snake>.rs`).
    /// Run `autumn generate model <Pascal>` (or `scaffold`) first.
    ///
    /// Example:
    ///
    ///   autumn generate policy Post
    #[command(verbatim_doc_comment)]
    Policy {
        /// Model name (`PascalCase` or `snake_case`, e.g. `Post`).
        name: String,
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
    },
    /// Generate team membership: organizations, roles (Owner/Admin/Member),
    /// and email invitations (issue #1261).
    ///
    /// Composes already-stable primitives — `#[repository(tenant_scoped)]`
    /// (issue #695), the session `"role"` key (issue #496), and the Mail
    /// stack (`#[mailer]`) — rather than introducing a new authorization
    /// mechanism. Takes no name: it always emits the same fixed
    /// `Organization`/`Membership`/`Invitation` set.
    ///
    /// Creates:
    ///   - `src/teams/`              — models, schema, repositories, role
    ///     guard, invitation mailer, and route handlers
    ///   - `migrations/<timestamp>_create_teams/` — organizations,
    ///     memberships, invitations tables
    ///   - `src/main.rs`             — `mod teams;` + routes wired into the
    ///     app builder
    ///   - `Cargo.toml`              — `"mail"` feature added to `autumn-web`
    ///
    /// Does NOT generate `routes/auth.rs` — your app's own login/signup
    /// already exists. See `docs/generate-teams.md` for the two-line
    /// integration seam, or `examples/teams` for a fully-wired reference app.
    ///
    /// Example:
    ///
    ///   autumn generate teams
    #[command(verbatim_doc_comment)]
    Teams {
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
    /// Scaffold the in-app notification feed: a `notifications` table
    /// migration, notify/feed/unread-count/mark-read routes over the built-in
    /// `Notifications` extractor, `main.rs` route wiring, and an in-process
    /// smoke test.
    ///
    /// Notifications are a fixed, single-instance resource (the framework's
    /// `Notifications` extractor reads one conventional `notifications`
    /// table), so this command takes no name argument.
    ///
    /// Creates:
    ///
    /// - `migrations/<ts>_create_notifications/` — backend-aware table DDL
    /// - `src/notifications.rs`  — notify / feed / unread-count / mark-read /
    ///   mark-all-read route handlers
    /// - `src/main.rs`           — `mod notifications;` + route registration
    /// - `tests/notifications_feed.rs` — smoke test over the in-process
    ///   `TestApp` (no database needed: memory-store fallback)
    /// - `Cargo.toml`            — `serde`/`serde_json` deps and the tokio
    ///   dev-dependency test features
    ///
    /// Example:
    ///
    ///   autumn generate notifications
    #[command(verbatim_doc_comment)]
    Notifications {
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
        /// Scaffold passwordless email magic-link login (off by default).
        /// Adds a `magic_link_tokens` table, request → email → verify routes
        /// (`/login/magic`, `/login/magic/verify`), a rate-limited request
        /// endpoint, single-use SHA-256-digest tokens with a configurable TTL,
        /// and generated integration tests. Composable with `--oauth`,
        /// `--passkeys`, and `--totp`.
        #[arg(long = "magic-link")]
        magic_link: bool,
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
    /// Generate a signed, replay-protected inbound webhook endpoint for a
    /// third-party provider (Stripe, GitHub, Slack, or a generic HMAC source).
    ///
    /// The handler takes the shipped `SignedWebhook` extractor — signature
    /// verification, raw-body capture, timestamp tolerance, replay rejection,
    /// and secret rotation are the framework's, never hand-rolled.
    ///
    /// Creates:
    ///   - `src/webhooks/<snake>.rs` — `#[post]` handler, event dispatch, and tests
    ///   - `src/webhooks/mod.rs`     — created/updated with `pub mod`
    ///   - `src/main.rs`             — `mod webhooks;` + the route in `routes![...]`
    ///   - `autumn.toml`             — endpoint stub, replay backend, exemptions
    ///   - `Cargo.toml`              — `serde_json` + tokio test features
    ///
    /// Example:
    ///
    ///   autumn generate webhook stripe Payments
    ///   autumn generate webhook github Repo --path /hooks/github
    ///   autumn generate webhook stripe Payments --dry-run
    #[command(verbatim_doc_comment)]
    Webhook {
        /// Provider preset: `stripe`, `github`, `slack`, or `generic`.
        provider: String,
        /// Endpoint name (`PascalCase` or `snake_case`, e.g. `Payments`).
        name: String,
        /// Route path for the endpoint (default: `/webhooks/<provider>`).
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Environment variable holding the signing secret
        /// (default: `<PROVIDER>_WEBHOOK_SECRET`).
        #[arg(long, value_name = "VAR")]
        secret_env: Option<String>,
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
    /// With `--remote-url <URL>` the generator instead scaffolds a **mobile
    /// thin client** (issue #1506): no sidecar — the webview loads the given
    /// remote HTTPS Autumn server directly, and a `capabilities/remote-app.json`
    /// grants that origin access to the notification/biometric/store plugins.
    ///
    /// Example:
    ///
    ///   autumn generate tauri
    ///   autumn generate tauri --dry-run
    ///   autumn generate tauri --remote-url <https://app.example.com>
    #[allow(clippy::doc_markdown)]
    #[command(verbatim_doc_comment)]
    Tauri {
        /// Print the file plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing files instead of erroring on collision.
        #[arg(long)]
        force: bool,
        /// Scaffold a mobile thin client whose webview loads this remote HTTPS
        /// URL instead of a local sidecar (plain http allowed for localhost dev).
        #[arg(long, value_name = "URL")]
        remote_url: Option<String>,
    },
    /// Scaffold a Tauri mobile shell (iOS/Android) that runs the autumn server in-process.
    ///
    /// Uses the **in-process model** (issue #1507, Option B): mobile sandboxes forbid
    /// spawning child processes, so — unlike the desktop sidecar of `generate tauri` —
    /// the Autumn Axum server runs on a background thread inside the app process
    /// itself, connecting to a REMOTE Postgres database over the device network.
    ///
    /// Also extracts your app's `src/main.rs` into `src/lib.rs::serve()` (only when
    /// the stock scaffold layout is detected; skipped with a warning otherwise) so
    /// the shell crate can call the server as a library.
    ///
    /// Creates:
    ///   - `src-tauri/`                 — standalone Tauri mobile shell crate
    ///     (staticlib/cdylib; no externalBin, no sidecar, no staging scripts)
    ///   - `src-tauri/src/lib.rs`       — spawns the server thread inside
    ///     `tauri::Builder::default().setup(...)`, polls /health, then opens the
    ///     webview at `http://127.0.0.1:<port>`
    ///   - `src-tauri/icons/`           — placeholder icons for immediate buildability
    ///
    /// See docs/guide/tauri-mobile-in-process.md for mobile sandboxing restrictions,
    /// remote-Postgres pool tuning for flaky networks, and App Store compliance.
    ///
    /// Example:
    ///
    ///   autumn generate tauri-mobile
    ///   autumn generate tauri-mobile --offline-sync
    ///   autumn generate tauri-mobile --dry-run
    #[command(verbatim_doc_comment)]
    TauriMobile {
        /// Wire offline-first local storage + background sync (issue #1508):
        /// app data lives in a `SyncStore`-backed `SQLite` file inside the app
        /// sandbox and a background `SyncEngine` syncs it with the remote
        /// deployment's `/sync` endpoints whenever the network allows. Adds
        /// the `offline-sync` feature (on `autumn-web`) to the app crate and
        /// mounts the server-side sync router in the extracted `serve()`.
        /// See docs/guide/tauri-mobile-offline-sync.md. Pass the same flag to
        /// `autumn destroy tauri-mobile` so the recomputed plan matches.
        #[arg(long)]
        offline_sync: bool,
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
    /// Generate a handler-only, non-CRUD route module (no model, migration, or
    /// database).
    ///
    /// Each action maps to `/<controller>/<action>`, except an action literally
    /// named `index`, which maps to `/<controller>`. Under `--api` the prefix is
    /// `/api/<controller>[/<action>]`.
    ///
    /// Actions default to GET; request another method with `action:method`
    /// (method ∈ get, post, put, patch, delete), e.g. `submit:post`.
    ///
    /// HTML mode (default) emits Maud stub views returning HTTP 200; `--api`
    /// emits JSON actions with no view stubs. Re-running against an existing
    /// controller fails without `--force`.
    ///
    /// Example:
    ///
    ///   autumn generate controller pages home about contact
    #[command(verbatim_doc_comment)]
    Controller {
        /// Controller name (`snake_case` or `PascalCase`, e.g. `pages`).
        name: String,
        /// Action names, each optionally suffixed with `:method`
        /// (e.g. `home`, `submit:post`, `index`).
        #[arg(required = true)]
        actions: Vec<String>,
        /// Emit JSON actions (no HTML/Maud views).
        #[arg(long)]
        api: bool,
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
    /// `DateTime`, `Vec<u8>`/Bytea, Attachment, references, `enum{a,b,...}`, `Option<...>`.
    ///
    /// `field:references` scaffolds a foreign key (`field_id BIGINT
    /// REFERENCES <table>(id)` plus an index), e.g.
    /// `autumn generate scaffold Comment body:Text post:references`.
    ///
    /// `field:enum{a,b,c}` scaffolds a closed-set column: a generated Rust
    /// enum, a `CHECK` constraint, a `<select>` form widget, and
    /// request-boundary validation that rejects an out-of-set value with a
    /// 400. Quote the token in bash/zsh (see `generate model --help`).
    ///
    /// `field:Type:unique` (e.g. `email:String:unique`) scaffolds a `CREATE
    /// UNIQUE INDEX`, a derived `find_by_<field>` repository lookup, and a
    /// create/update handler that renders a duplicate submission as an
    /// inline "already exists" field error (HTTP 422) instead of a 500.
    /// `--unique FIELD` is the flag-based equivalent. The inline-error
    /// handling is HTML-only: an `--api` scaffold still gets the `CREATE
    /// UNIQUE INDEX` and the `find_by_<field>` lookup, but its JSON CRUD
    /// routes are auto-generated by `#[repository]`, and a duplicate
    /// create/update there still 500s (out of scope for this slice).
    ///
    /// `field:String{encrypted}` scaffolds an at-rest encrypted column: the
    /// model field gets `#[encrypted]`, so the value is a base64 AES-256-GCM
    /// envelope on disk and a plain `String` in Rust; the migration column is
    /// unbounded `TEXT`, sized for the envelope. Use
    /// `{encrypted:deterministic}` when the column still needs
    /// `find_by`/`exists_by` lookups or a UNIQUE index — randomized ciphertext
    /// can never match an equality predicate, so pairing it with
    /// `:unique`/`--unique`/`--query`/`--index` is refused at generate time.
    /// The generated admin redacts the column, the index list renders
    /// `••••••••` (unsorted) and the CSV export omits it; the show view and
    /// edit form still render the value. `String`/`Text` and non-null only.
    /// Requires key material under `[active_record_encryption]`: run
    /// `autumn credentials edit`. Quote the token in bash/zsh (brace
    /// expansion), e.g.
    /// `autumn generate scaffold Account 'api_token:String{encrypted}'` or
    /// `autumn generate scaffold Account 'email:String{encrypted:deterministic}:unique'`.
    Scaffold {
        /// Resource name (`PascalCase` or `snake_case`, e.g. `Post`).
        name: String,
        /// Field DSL tokens, each `name:Type`.
        fields: Vec<String>,
        /// Add `#[indexed]` and a SQL index for this field. Repeatable.
        #[arg(long, value_name = "FIELD")]
        index: Vec<String>,
        /// Add a `CREATE UNIQUE INDEX` and a derived `find_by_<field>`
        /// repository lookup for this field. Repeatable. Mirrors the DSL's
        /// inline `:unique` modifier (`email:String:unique`).
        #[arg(long, value_name = "FIELD")]
        unique: Vec<String>,
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
        /// Skip generating a record-level authorization `Policy`/`Scope` for the
        /// resource. By default the scaffold generates and registers a policy;
        /// with an owner column (`user_id`/`author_id`/`owner_id`) it also
        /// authorizes the mutating HTML handlers and scopes the index. Pass
        /// `--no-policy` to keep the older `#[secured]`-only output. Ignored for
        /// `--api` scaffolds, which never generate a policy.
        #[arg(long)]
        no_policy: bool,
        /// Bind this resource to a parent as its child (issue #1323), e.g.
        /// `--belongs-to Post` alongside a `post:references` column. Adds a
        /// nested read route (`GET /posts/{post_id}/comments`), a nested create
        /// route that takes the foreign key from the path instead of the
        /// submitted body, a children list + inline "add" form on the parent's
        /// generated show view, and back-links in both directions. The parent
        /// must already be scaffolded, and must be an `id`-keyed resource whose
        /// `show` view is the one the flat scaffold generated (not `slug`-keyed,
        /// not carrying a `:states(...)` column, not hand-rewritten). Not
        /// supported with `--api`, `--live`, `--live-validation`, `--sharded`,
        /// an `Attachment` column, or a nullable/self-referential parent
        /// reference.
        #[arg(long, value_name = "PARENT")]
        belongs_to: Option<String>,
        /// Maintain a `{child}_count` column on the parent (issue #1325).
        /// Requires `--belongs-to`. Adds `counter_cache` to the generated
        /// child model's `#[belongs_to(...)]`, so the child repository keeps
        /// `{parent}.{child}_count` current atomically and in the same
        /// transaction as each insert/delete, and emits a migration adding
        /// `{child}_count BIGINT NOT NULL DEFAULT 0` to the parent's table.
        #[arg(long)]
        counter_cache: bool,
        /// Make these text fields full-text searchable (issue #1319): comma-
        /// separated or repeatable, e.g. `--searchable title,body`. Adds
        /// `#[searchable]` to the model, `searchable` to the repository, a
        /// `search_vector` migration, and a wired search box in the index view.
        #[arg(long, value_name = "FIELD,FIELD", value_delimiter = ',')]
        searchable: Vec<String>,
        /// Emit i18n-ready views (issue #1349): every page title, heading,
        /// button, link, and field label in the generated HTML views becomes a
        /// `t!(locale, "key")` lookup, each view handler takes the `Locale`
        /// extractor, and the referenced keys are written to `i18n/en.ftl` with
        /// their English values — so an `en` app renders identically and adding
        /// a locale means translating a `.ftl`, not editing Rust. Also enables
        /// autumn-web's `i18n` feature, adds `[i18n]` to `autumn.toml`, and
        /// wires `.i18n_auto()` into `main.rs`. Composes with `--searchable`,
        /// `--soft-delete`, and `--sharded`; `--api` scaffolds render no labels,
        /// so the flag is a no-op there. Not supported with `--live`,
        /// `--live-validation`, or `--belongs-to`.
        #[arg(long)]
        i18n: bool,
        /// Emit a CSV import surface (issue #1393): a `GET /<plural>/import`
        /// upload form and a `POST /<plural>/import` handler that parses the
        /// uploaded multipart CSV, previews it with `import_csv` in
        /// `ImportMode::DryRun` — reporting total rows, rows that would
        /// insert, and a per-row error list with line numbers — and only
        /// writes when the submit explicitly confirms a commit, through the
        /// repository's transactional `save_many_skip_invalid`. Decodes rows
        /// against the same `CsvSchema` impl the CSV export emits, so it is
        /// honoured wherever that export is: not for `--api`, `--live`,
        /// `--sharded`, an owner-scoped `--live-validation` scaffold, or a model
        /// with an at-rest `#[encrypted]` column (the export omits that column
        /// but the form requires it) — the generator warns and emits nothing
        /// there. Composes with `--i18n`, `--searchable`, `--soft-delete`,
        /// `--belongs-to` and `--counter-cache`, and enables autumn-web's
        /// `multipart` feature. Insert-only: every row becomes a NEW record.
        #[arg(long)]
        import: bool,
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
            edge,
            auditable,
        } => build::run(
            debug,
            embed,
            edge,
            package.as_deref(),
            bin.as_deref(),
            features.as_deref(),
            auditable,
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
            role,
            pin,
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
                    // Forwarded to the app binary via `AUTUMN_ROLE`. `None` lets
                    // the child pick its default (combined) or read its own env.
                    role: role.map(|r| r.as_str().to_owned()),
                    // Forwarded via `AUTUMN_JOBS__PIN` (#1623, AC3). No `--pin`
                    // at all leaves the variable untouched so the child reads
                    // `[jobs] pin`; `--pin ""` is a deliberate unpin and must
                    // stay distinguishable from that, so presence is carried by
                    // the `Option`, not by the list being non-empty. Normalized
                    // here (trim, drop blanks) so the recorded pin and the pin
                    // the app parses are the same list.
                    pin: (!pin.is_empty()).then(|| {
                        pin.iter()
                            .map(|q| q.trim())
                            .filter(|q| !q.is_empty())
                            .map(str::to_owned)
                            .collect()
                    }),
                },
            );
        }
        Commands::Schema { action } => schema::run(action),
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
                Some(MigrateCommands::Baseline { force }) => {
                    migrate::MigrateAction::Baseline(migrate::BaselineArgs {
                        force_version: force,
                    })
                }
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
            DbCommands::Backup {
                profile,
                dir,
                format,
                keep,
                shard,
                control_only,
                upload,
            } => {
                let format = match db::backup::BackupFormat::parse(&format) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("\u{2717} {e}");
                        std::process::exit(2);
                    }
                };
                let target = match (shard, control_only) {
                    (Some(name), _) => db::backup::TargetSelector::Shard(name),
                    (None, true) => db::backup::TargetSelector::ControlOnly,
                    (None, false) => db::backup::TargetSelector::All,
                };
                db::backup::run_backup(&db::backup::BackupArgs {
                    profile,
                    dir,
                    format,
                    keep,
                    target,
                    upload,
                });
            }
            DbCommands::Restore {
                artifact,
                profile,
                force,
                shard,
                offsite,
            } => db::backup::run_restore(&db::backup::RestoreArgs {
                artifact,
                profile,
                force,
                shard,
                offsite,
            }),
            DbCommands::Scrub {
                profile,
                artifact,
                output,
                config,
                check,
                dry_run,
                force,
                allow_source_overwrite,
                sample,
                seed,
            } => db::scrub::run(&db::scrub::ScrubArgs {
                profile,
                artifact,
                output,
                config,
                check,
                dry_run,
                force,
                allow_source_overwrite,
                sample,
                seed,
            }),
            DbCommands::Retention {
                package,
                bin,
                profile,
                dataset,
                dry_run,
                purge,
                force,
                json,
            } => db::retention::run(&db::retention::RetentionOptions {
                package: package.as_deref(),
                bin: bin.as_deref(),
                profile: &profile,
                mode: db_retention_mode(dry_run, purge),
                dataset: dataset.as_deref(),
                force,
                json,
            }),
            DbCommands::Offsite(OffsiteCommands::List { profile }) => {
                db::backup::run_offsite_list(profile.as_deref());
            }
            DbCommands::Replica(cmd) => db::replica::run(&cmd.into_command()),
            other => {
                let (command, profile) = other.into_command();
                db::run(&command, profile.as_deref());
            }
        },
        Commands::Test { reset, cargo_args } => test_cmd::run(reset, &cargo_args),
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
            api,
            with,
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
                if with_i18n || with_seed || daemon || bundled_pg || api {
                    eprintln!(
                        "Error: --starter cannot be combined with --with-i18n, \
                         --with-seed, --daemon, --bundled-pg, or --api (a starter \
                         brings its own composition)"
                    );
                    std::process::exit(1);
                }
                // AC #6: every `--with` name is resolved and version-checked
                // BEFORE the scaffold writes a byte, so a typo or an
                // incompatible plugin never leaves a half-built project behind.
                let plugins = resolve_scaffold_plugins(&with, None);
                starters::run(
                    &name,
                    &starter,
                    starter_ref.as_deref(),
                    yes,
                    generate::Flags::default(),
                );
                wire_scaffold_plugins_into(&name, &plugins);
            } else {
                let plugins = resolve_scaffold_plugins(&with, Some(plugin::first_party_version()));
                new::run(
                    &name,
                    new::GenerateOptions {
                        with_i18n,
                        with_seed,
                        // --bundled-pg is a daemon flavor that keeps the database.
                        with_daemon: daemon || bundled_pg,
                        with_bundled_pg: bundled_pg,
                        with_api: api,
                    },
                );
                wire_scaffold_plugins_into(&name, &plugins);
            }
        }

        Commands::Webhook(WebhookCommands::Sim {
            provider,
            url,
            secret,
            payload,
            event,
        }) => webhook::run_sim(&provider, &url, &secret, &payload, event.as_deref()),
        Commands::Alert(AlertCommands::Test { channel }) => alert::run_test(channel.as_deref()),
        Commands::Console {
            profile,
            package,
            force,
            scaffold_only,
        } => console::run(&profile, package.as_deref(), force, scaffold_only),
        Commands::Seed {
            profile,
            package,
            count,
            model,
            yes_i_mean_prod,
        } => seed::run(
            &profile,
            package.as_deref(),
            count,
            model.as_deref(),
            yes_i_mean_prod,
        ),
        Commands::Capsule { command } => match command {
            CapsuleCommands::Test {
                capsule: path,
                name,
                tests_dir,
                force,
            } => capsule::generate(&capsule::GenerateOptions {
                capsule: &path,
                name: name.as_deref(),
                tests_dir: &tests_dir,
                force,
            }),
            CapsuleCommands::Verify { dir, check_only } => {
                capsule::verify(&capsule::VerifyOptions {
                    dir: &dir,
                    check_only,
                });
            }
        },
        Commands::Replay {
            capsule,
            package,
            bin,
            profile,
            release,
            debug,
            features,
            no_default_features,
        } => run_replay_command(
            &capsule,
            package.as_deref(),
            bin.as_deref(),
            profile.as_deref(),
            match (release, debug) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            },
            features.as_deref(),
            no_default_features,
        ),
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
        Commands::Retention {
            package,
            bin,
            profile,
            dry_run,
            model,
        } => retention::run(&retention::RetentionOptions {
            package: package.as_deref(),
            bin: bin.as_deref(),
            profile: &profile,
            dry_run,
            model: model.as_deref(),
        }),
        Commands::Setup { force } => setup::run(force),
        Commands::Sbom {
            manifest_path,
            output,
            verify,
            binary,
            locked,
            all_features,
            features,
            filter_platform,
            expect_version,
        } => sbom::run(&sbom::SbomOptions {
            manifest_path,
            output,
            verify,
            binary,
            locked,
            all_features,
            features,
            filter_platform,
            expect_version,
        }),
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
        Commands::Agents(AgentsSubcommands::Manifest(args)) => {
            let features = routes::CargoFeatures {
                features: args.features,
                all: args.all_features,
                no_default: args.no_default_features,
            };
            agents::run(&agents::AgentsManifestOptions {
                package: args.package.as_deref(),
                bin: args.bin.as_deref(),
                manifest: args.manifest.as_deref(),
                json: args.json,
                check: args.check.as_deref(),
                allow_ungoverned: args.allow_ungoverned,
                allow_unaudited: args.allow_unaudited,
                features,
                release: args.release,
            });
        }
        Commands::Cache(CacheSubcommands::Audit(args)) => {
            let features = routes::CargoFeatures {
                features: args.features,
                all: args.all_features,
                no_default: args.no_default_features,
            };
            cache_audit::run(&cache_audit::CacheAuditOptions {
                package: args.package.as_deref(),
                bin: args.bin.as_deref(),
                manifest: args.manifest.as_deref(),
                json: args.json,
                strict: args.strict,
                features,
            });
        }
        Commands::Graph(command) => {
            let (query, args) = match command {
                GraphSubcommands::Show(args) => (graph::Query::Show, args),
                GraphSubcommands::Touches { name, args } => (graph::Query::Touches(name), args),
                GraphSubcommands::Impact { name, args } => (graph::Query::Impact(name), args),
            };
            let features = routes::CargoFeatures {
                features: args.features,
                all: args.all_features,
                no_default: args.no_default_features,
            };
            graph::run(&graph::GraphOptions {
                query,
                package: args.package.as_deref(),
                bin: args.bin.as_deref(),
                manifest: args.manifest.as_deref(),
                json: args.json,
                check: args.check.as_deref(),
                features,
                release: args.release,
            });
        }
        Commands::DataFlow(args) => {
            let features = routes::CargoFeatures {
                features: args.features,
                all: args.all_features,
                no_default: args.no_default_features,
            };
            data_flow::run(&data_flow::DataFlowOptions {
                package: args.package.as_deref(),
                bin: args.bin.as_deref(),
                manifest: args.manifest.as_deref(),
                json: args.json,
                check: args.check.as_deref(),
                features,
                release: args.release,
            });
        }
        Commands::Calibrate {
            package,
            bin,
            contract,
            check,
            profile,
            features,
            all_features,
            no_default_features,
            targets,
            seed,
            concurrency,
            rung_ms,
            warmup_ms,
            runs,
            tolerance_rps,
            tolerance_p99,
            json,
        } => {
            let exit_code = capacity_driver::run(&capacity_driver::CalibrateOptions {
                package: package.as_deref(),
                bin: bin.as_deref(),
                contract_path: &contract,
                check,
                profile,
                named_features: !features.is_empty() || all_features || no_default_features,
                features: routes::CargoFeatures {
                    features,
                    all: all_features,
                    no_default: no_default_features,
                },
                targets,
                seed,
                concurrency,
                rung_ms,
                warmup_ms,
                runs,
                tolerances: capacity::Tolerances {
                    rps: tolerance_rps,
                    p99: tolerance_p99,
                },
                json,
            });
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Commands::Routes {
            package,
            bin,
            format,
            prefix,
            filter,
            method,
            user_only,
            command,
        } => match command {
            Some(RoutesSubcommands::Audit {
                package: audit_package,
                bin: audit_bin,
                manifest,
                json,
                strict,
            }) => {
                // Fall back to the parent `routes` target flags so both
                // `autumn routes -p blog audit` and `autumn routes audit -p blog`
                // select the same binary in multi-target workspaces.
                let package = audit_package.or(package);
                let bin = audit_bin.or(bin);
                routes_audit::run(&routes_audit::AuditOptions {
                    package: package.as_deref(),
                    bin: bin.as_deref(),
                    manifest: manifest.as_deref(),
                    json,
                    strict,
                });
            }
            Some(RoutesSubcommands::Posture(command)) => {
                let code = match &command {
                    PostureSubcommands::Diff {
                        base,
                        head,
                        format,
                        output,
                        ack,
                        ack_file,
                        allow_missing_base,
                    } => posture::run_diff(&posture::DiffOptions {
                        base,
                        head,
                        format,
                        output: output.as_deref(),
                        acks: ack,
                        ack_file: ack_file.as_deref(),
                        allow_missing_base: *allow_missing_base,
                    }),
                    PostureSubcommands::Digest { manifest, format } => {
                        posture::run_digest(manifest, format)
                    }
                    PostureSubcommands::Verify {
                        manifest,
                        expect_digest,
                        repo,
                        skip_signature,
                    } => posture::run_verify(&posture::verify::VerifyOptions {
                        manifest,
                        expect_digest: expect_digest.as_deref(),
                        repo: repo.as_deref(),
                        skip_signature: *skip_signature,
                    }),
                };
                std::process::exit(code);
            }
            None => run_routes_command(
                package.as_deref(),
                bin.as_deref(),
                &format,
                prefix.as_deref(),
                filter.as_deref(),
                &method,
                user_only,
            ),
        },
        Commands::Release(cmd) => run_release_command(cmd),
        Commands::Deploy(cmd) => run_deploy_command(&cmd),
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
        Commands::Upgrade(UpgradeArgs {
            path,
            from,
            to,
            apply,
            json,
            list_migrations,
            check,
            accept,
        }) => {
            let code = upgrade::run_in(
                std::path::Path::new(&path),
                &upgrade::UpgradeOptions {
                    from,
                    to,
                    apply,
                    json,
                    list: list_migrations,
                    check,
                    accept,
                },
            );
            std::process::exit(code);
        }
        Commands::Doctor {
            json,
            strict,
            online,
        } => {
            doctor::run(doctor::DoctorOptions {
                json,
                strict,
                online,
            });
        }
        Commands::I18n { action } => match action {
            I18nSubcommands::Check { format, strict } => {
                let format = match format.as_str() {
                    "json" => i18n::OutputFormat::Json,
                    "text" => i18n::OutputFormat::Text,
                    other => {
                        eprintln!(
                            "autumn i18n check: unknown --format `{other}` (expected `text` or `json`)"
                        );
                        std::process::exit(2);
                    }
                };
                i18n::run(i18n::I18nCheckOptions { format, strict });
            }
        },
        Commands::A11y { action } => match action {
            A11ySubcommands::Verify {
                path,
                format,
                strict,
            } => {
                let format = match format.as_str() {
                    "json" => a11y::OutputFormat::Json,
                    "text" => a11y::OutputFormat::Text,
                    other => {
                        eprintln!(
                            "autumn a11y verify: unknown --format `{other}` (expected `text` or `json`)"
                        );
                        std::process::exit(2);
                    }
                };
                let code = a11y::run_in(
                    std::path::Path::new(&path),
                    a11y::A11yVerifyOptions { format, strict },
                );
                std::process::exit(code);
            }
        },
        Commands::Lifecycle { action } => match action {
            LifecycleSubcommands::Check { path, format } => {
                let format = match format.as_str() {
                    "json" => lifecycle::OutputFormat::Json,
                    "text" => lifecycle::OutputFormat::Text,
                    other => {
                        eprintln!(
                            "autumn lifecycle check: unknown --format `{other}` (expected `text` or `json`)"
                        );
                        std::process::exit(2);
                    }
                };
                let code = lifecycle::run_check(
                    std::path::Path::new(&path),
                    lifecycle::CheckOptions { format },
                );
                std::process::exit(code);
            }
            LifecycleSubcommands::Diagram { path, format, out } => {
                let format = match format.as_str() {
                    "mermaid" => lifecycle::DiagramFormat::Mermaid,
                    "dot" => lifecycle::DiagramFormat::Dot,
                    other => {
                        eprintln!(
                            "autumn lifecycle diagram: unknown --format `{other}` (expected `mermaid` or `dot`)"
                        );
                        std::process::exit(2);
                    }
                };
                let code = lifecycle::run_diagram(
                    std::path::Path::new(&path),
                    &lifecycle::DiagramOptions {
                        format,
                        out: out.map(std::path::PathBuf::from),
                    },
                );
                std::process::exit(code);
            }
        },
        Commands::Jobs { action } => match action {
            JobsSubcommands::Manifest { path, package, bin } => {
                jobs::run(&jobs::ManifestOptions {
                    package: package.as_deref(),
                    bin: bin.as_deref(),
                    output: &path,
                });
            }
        },
        Commands::Search { action } => match action {
            SearchSubcommands::Reindex {
                index,
                profile,
                purge,
                package,
                bin,
            } => {
                search::run(&search::ReindexOptions {
                    package: package.as_deref(),
                    bin: bin.as_deref(),
                    index: index.as_deref(),
                    profile: profile.as_deref(),
                    purge,
                });
            }
        },
        Commands::Plugin { action } => {
            let root = std::path::Path::new(".");
            let code = match action {
                PluginSubcommands::List { json, offline } => {
                    plugin::run_list(&plugin::ListOptions {
                        root,
                        json,
                        offline,
                    })
                }
                PluginSubcommands::Add {
                    name,
                    dry_run,
                    offline,
                } => plugin::run_add(&plugin::AddOptions {
                    root,
                    name: &name,
                    dry_run,
                    offline,
                }),
                PluginSubcommands::Remove {
                    name,
                    dry_run,
                    drop_data,
                    yes,
                } => plugin::run_remove(&plugin::RemoveOptions {
                    root,
                    name: &name,
                    dry_run,
                    drop_data,
                    yes,
                }),
                PluginSubcommands::Package {
                    manifest,
                    module,
                    out,
                } => {
                    plugin_sandbox::run_package(&plugin_sandbox::PackageOptions {
                        manifest: std::path::Path::new(&manifest),
                        module: std::path::Path::new(&module),
                        out: std::path::Path::new(&out),
                    });
                    0
                }
                PluginSubcommands::Inspect {
                    artifact,
                    format,
                    against,
                } => {
                    let format = format.parse().unwrap_or_else(|e| {
                        eprintln!("autumn plugin inspect: {e}");
                        std::process::exit(1);
                    });
                    plugin_sandbox::run_inspect(
                        std::path::Path::new(&artifact),
                        &format,
                        against.as_deref().map(std::path::Path::new),
                    );
                    0
                }
            };
            if code != 0 {
                std::process::exit(code);
            }
        }
        Commands::PluginCheck {
            package,
            bin,
            plugin_name,
            prefix,
            sensitive_route,
            format,
            deny_experimental,
        } => {
            run_plugin_check_command(
                package.as_deref(),
                bin.as_deref(),
                &plugin_name,
                prefix.as_deref(),
                &sensitive_route,
                &format,
                deny_experimental,
            );
        }
        Commands::Generate(cmd) => run_generate_command(cmd, ApplyMode::Generate),
        Commands::Destroy(cmd) => run_generate_command(cmd, ApplyMode::Destroy),
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
            overload,
            ceiling,
            block_ms,
            load_multiplier,
        } => {
            let exit_code = if overload {
                overload_driver::run_overload(
                    ceiling,
                    block_ms,
                    load_multiplier,
                    runs,
                    output.as_deref(),
                    json,
                    fail_on_regression,
                    dry_run,
                )
            } else if scaling {
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

fn run_replay_command(
    capsule: &str,
    package: Option<&str>,
    bin: Option<&str>,
    profile: Option<&str>,
    build: Option<bool>,
    features: Option<&str>,
    no_default_features: bool,
) {
    replay::run(&replay::ReplayOptions {
        capsule,
        package,
        bin,
        profile,
        build,
        features,
        no_default_features,
    });
}

/// Map the `--dry-run` / `--purge` flags onto a [`db::retention::RetentionMode`]
/// (issue #1605).
///
/// Neither flag means "report": the default is always read-only. Clap already
/// rejects passing both, but the ordering here makes the safe direction the
/// fallback rather than a coincidence — a future flag-handling change can only
/// ever fail towards *not* deleting.
/// The valid `autumn db retention --dataset` values, mirroring
/// `autumn_web::data_retention::RETENTION_DATASETS` (issue #1605).
///
/// A clap `value_parser` so a typo is rejected before the app binary is even
/// compiled, rather than after a full build-and-boot round trip.
const RETENTION_DATASET_KEYS: [&str; 8] = [
    "job_history",
    "commit_hooks",
    "job_tracking",
    "idempotency",
    "experiment_assignments",
    "webhook_replay",
    "sessions",
    "audit_archives",
];

const fn db_retention_mode(dry_run: bool, purge: bool) -> db::retention::RetentionMode {
    if purge {
        db::retention::RetentionMode::Purge
    } else if dry_run {
        db::retention::RetentionMode::DryRun
    } else {
        db::retention::RetentionMode::Report
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

/// Resolve every `autumn new --with` name, exiting before the scaffold runs if
/// any of them cannot be installed (issue #1631, AC #6).
///
/// The whole point of doing this here is ordering: `autumn new` creates a
/// directory tree, and a project that exists but is missing the plugin the user
/// asked for is worse than no project at all.
///
/// `scaffold_autumn_web` is the `autumn-web` the scaffold will pin, or `None`
/// when that is not knowable yet. `autumn new`'s own template pins this CLI's
/// version, so the gate is exact there. A `--starter` brings its own manifest,
/// which does not exist until the starter is fetched — so only the half that
/// IS knowable (does this name resolve at all, and to what version) runs
/// before the write, and the compatibility answer comes from
/// `plugin::wire_scaffold_plugins` reading the starter's real manifest
/// afterwards.
fn resolve_scaffold_plugins(
    names: &[String],
    scaffold_autumn_web: Option<&str>,
) -> Vec<plugin::ScaffoldPlugin> {
    if names.is_empty() {
        return Vec::new();
    }
    match plugin::preflight_scaffold_plugins(
        names,
        scaffold_autumn_web,
        plugin::registry::latest_version,
    ) {
        Ok(plugins) => plugins,
        Err(err) => {
            eprintln!("autumn new: {err}");
            std::process::exit(1);
        }
    }
}

/// Wire the preflighted plugins into the project `autumn new` just created.
fn wire_scaffold_plugins_into(project_name: &str, plugins: &[plugin::ScaffoldPlugin]) {
    if plugins.is_empty() {
        return;
    }
    // Built the same way the scaffolders build it (`new::run` joins onto
    // `current_dir`), rather than leaning on the process CWD staying put.
    let root = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(project_name);
    let code = plugin::wire_scaffold_plugins(&root, plugins);
    if code != 0 {
        std::process::exit(code);
    }
}

fn run_plugin_check_command(
    package: Option<&str>,
    bin: Option<&str>,
    plugin_name: &str,
    prefix: Option<&str>,
    sensitive_route_args: &[String],
    format: &str,
    deny_experimental: bool,
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
        // Populated by `run` from the built binary's contract dump.
        contracts: &plugin_check::ContractDump::Absent,
        deny_experimental,
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
        ReleaseCommands::Init {
            force,
            target,
            split_workers,
        } => {
            let t = target.as_deref().map_or(release::Target::Default, |s| {
                s.parse().unwrap_or_else(|e| {
                    eprintln!("autumn release init: {e}");
                    std::process::exit(1);
                })
            });
            release::run(release::ReleaseAction::Init {
                force,
                target: t,
                split_workers,
            });
        }
    }
}

/// Map a `deploy` subcommand onto the (action, options) pair `deploy::run` takes.
///
/// [`deploy::DeployAction`] stays a fieldless `Copy` enum and the flags travel
/// beside it in [`deploy::DeployOptions`] (issue #1621, §3.1), so adding a flag
/// never ripples through the action enum or its call sites. Every construction here
/// spreads `..Default::default()` for the same reason.
fn run_deploy_command(cmd: &DeployCommands) {
    let (action, options) = match cmd {
        DeployCommands::Check => (
            deploy::DeployAction::Check,
            deploy::DeployOptions::default(),
        ),
        DeployCommands::Plan => (deploy::DeployAction::Plan, deploy::DeployOptions::default()),
        DeployCommands::Rollback { only } => (
            deploy::DeployAction::Rollback,
            deploy::DeployOptions {
                only: only.clone(),
                ..deploy::DeployOptions::default()
            },
        ),
        DeployCommands::Up { only, no_rollback } => (
            deploy::DeployAction::Up,
            deploy::DeployOptions {
                only: only.clone(),
                no_rollback: *no_rollback,
                ..deploy::DeployOptions::default()
            },
        ),
        DeployCommands::Status { json, strict } => (
            deploy::DeployAction::Status,
            deploy::DeployOptions {
                json: *json,
                strict: *strict,
                ..deploy::DeployOptions::default()
            },
        ),
        DeployCommands::Maintenance(cmd) => match cmd {
            DeployMaintenanceCommands::On {
                message,
                allow_ips,
                readonly,
                bypass_header,
            } => {
                // Same parse (and same failure behavior) as the local
                // `autumn maintenance on`, so the two surfaces reject a malformed
                // NAME:VALUE identically.
                let parsed_bypass = bypass_header.as_deref().map(|s| {
                    maintenance::parse_bypass_header(s).map_or_else(
                        |e| {
                            eprintln!("autumn deploy maintenance on: {e}");
                            std::process::exit(1);
                        },
                        |(name, value)| (name.to_owned(), value.to_owned()),
                    )
                });
                (
                    deploy::DeployAction::MaintenanceOn,
                    deploy::DeployOptions {
                        maintenance: Some(deploy::MaintenanceOnArgs {
                            message: message.clone(),
                            allow_ips: allow_ips.clone(),
                            readonly: *readonly,
                            bypass_header: parsed_bypass,
                        }),
                        ..deploy::DeployOptions::default()
                    },
                )
            }
            DeployMaintenanceCommands::Off => (
                deploy::DeployAction::MaintenanceOff,
                deploy::DeployOptions::default(),
            ),
        },
    };
    if let Err(e) = deploy::run(action, &options) {
        eprintln!("autumn deploy: {e}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
/// Whether a [`GenerateCommands`] invocation should apply its plan forward
/// (`autumn generate`) or in reverse (`autumn destroy`, issue #1048). Both
/// subcommands share the same argument parsing and plan-building code —
/// `destroy` recomputes the identical [`generate::emit::Plan`] a matching
/// `generate` call would have built, then interprets it in reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyMode {
    Generate,
    Destroy,
}

/// Resolve the current working directory, or print an error and exit(1).
fn resolve_cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("Error: cannot determine current directory: {e}");
        std::process::exit(1);
    })
}

/// Execute or revert `plan` depending on `mode`, printing `Error: ...` and
/// exiting non-zero on failure — the shared tail every `generate`/`destroy`
/// subcommand arm ends with.
fn apply_plan(
    plan: Result<generate::emit::Plan, generate::GenerateError>,
    flags: generate::Flags,
    mode: ApplyMode,
) {
    let result = plan.and_then(|p| match mode {
        ApplyMode::Generate => p.execute(flags),
        ApplyMode::Destroy => p.revert(flags),
    });
    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one match arm per GenerateCommands variant, each a short, independent \
              plan-then-apply dispatch — splitting the match itself would not make any \
              single arm clearer"
)]
fn run_generate_command(cmd: GenerateCommands, mode: ApplyMode) {
    match cmd {
        GenerateCommands::Model {
            name,
            fields,
            unique,
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
                uniques: unique,
                soft_delete,
                id_type,
                ..Default::default()
            };
            let timestamp = generate::timestamp_now();
            // `destroy model` recomputes the plan it is about to revert, so it
            // must not be blocked by generation-only semantic checks: a model
            // created before those checks existed still has to be removable, and
            // the refusal would land before `Plan::revert` ever sees `--force`.
            let project_root = std::env::current_dir().unwrap_or_default();
            let plan = match mode {
                ApplyMode::Generate => generate::model::plan_model_with_options(
                    &project_root,
                    &name,
                    &fields,
                    &timestamp,
                    &options,
                ),
                ApplyMode::Destroy => generate::model::plan_model_with_options_for_revert(
                    &project_root,
                    &name,
                    &fields,
                    &timestamp,
                    &options,
                ),
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Migration {
            name,
            fields,
            unique,
            dry_run,
            force,
        } => {
            let timestamp = generate::timestamp_now();
            let project_root = resolve_cwd();
            let plan = generate::migration::plan_migration_with_options(
                &project_root,
                &name,
                &fields,
                &timestamp,
                &unique,
            );
            // `plan_migration_with_options` needs the model's `#[searchable]`
            // config to render `AddSearchTo<Table>`'s SQL — meaningless (and
            // an error) once the model is already gone. A common cleanup
            // order like `destroy model Post` then `destroy migration
            // AddSearchToPosts` would otherwise strand the migration
            // directory, since the failure happens before `Plan::revert`
            // ever sees `--force` (issue #1048 PR review). Fall back to a
            // suffix-only removal plan in destroy mode when the real plan
            // can't be built.
            let plan = if mode == ApplyMode::Destroy && plan.is_err() {
                generate::migration::plan_migration_destroy_fallback(
                    &project_root,
                    &name,
                    &timestamp,
                )
            } else {
                plan
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Task {
            name,
            dry_run,
            force,
        } => {
            let plan = generate::task::plan_task(&resolve_cwd(), &name);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Job {
            name,
            fields,
            dry_run,
            force,
        } => {
            let plan = generate::job::plan_job(&resolve_cwd(), &name, &fields);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Mailer {
            name,
            list_unsubscribe,
            no_layout,
            dry_run,
            force,
        } => {
            // The SQLite generate-time rejection is generate-only: `destroy
            // mailer` recomputes this same plan before reverting it, so the
            // destroy path passes `for_revert` to skip the reject and let
            // cleanup remove generated files on a SQLite app (mirrors the auth
            // destroy path, which uses a distinct `_for_revert` builder).
            let cwd = resolve_cwd();
            let plan = match mode {
                ApplyMode::Generate => generate::mailer::plan_mailer(
                    &cwd,
                    &name,
                    list_unsubscribe.as_deref(),
                    no_layout,
                ),
                ApplyMode::Destroy => generate::mailer::plan_mailer_ex(
                    &cwd,
                    &name,
                    list_unsubscribe.as_deref(),
                    no_layout,
                    true,
                ),
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Policy {
            name,
            dry_run,
            force,
        } => {
            let plan =
                generate::policy::plan_policy(&resolve_cwd(), &name, mode == ApplyMode::Destroy);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Teams { dry_run, force } => {
            let plan = generate::teams::plan_teams(
                &resolve_cwd(),
                &generate::timestamp_now(),
                mode == ApplyMode::Destroy,
            );
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
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
            let plan = generate::channel::plan_channel(&resolve_cwd(), &name, transport);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Notifications { dry_run, force } => {
            let plan = generate::notifications::plan_notifications(&resolve_cwd());
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::InboundMail {
            name,
            dry_run,
            force,
        } => {
            let plan = generate::inbound_mail::plan_inbound_mail(&resolve_cwd(), &name);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Webhook {
            provider,
            name,
            path,
            secret_env,
            dry_run,
            force,
        } => {
            let options = generate::webhook::WebhookOptions { path, secret_env };
            let project_root = resolve_cwd();
            // `destroy` recovers a `--path`/`--secret-env` it was not given from
            // the endpoint block `generate` recorded, so cleanup does not depend
            // on the user repeating flags (issue #1366, Codex review).
            let plan = match mode {
                ApplyMode::Generate => {
                    generate::webhook::plan_webhook(&project_root, &provider, &name, &options)
                }
                ApplyMode::Destroy => generate::webhook::plan_webhook_for_revert(
                    &project_root,
                    &provider,
                    &name,
                    &options,
                ),
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
            // Printed after the file list, and only for a real generate run:
            // `apply_plan` exits on failure, and neither a dry run nor a
            // destroy has next steps to take (issue #1366 AC #5).
            if mode == ApplyMode::Generate
                && !dry_run
                && let Some(steps) = generate::webhook::next_steps(&provider, &name, &options)
            {
                println!("{steps}");
            }
        }
        GenerateCommands::SystemTest {
            name,
            dry_run,
            force,
        } => {
            let plan = generate::system_test::plan_system_test(&resolve_cwd(), &name);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Pwa { dry_run, force } => {
            let project_root = resolve_cwd();
            // `plan_pwa` validates `src/main.rs`'s `layout()` arity — needed
            // so a fresh generate never emits a call with the wrong shape,
            // but irrelevant to destroy (which never consults that arity)
            // and can wrongly block cleanup if `main.rs` no longer matches
            // (issue #1048 PR review). Always use the destroy-only fallback
            // for `ApplyMode::Destroy`; it produces the identical plan for
            // every other case.
            let plan = match mode {
                ApplyMode::Generate => generate::pwa::plan_pwa(&project_root),
                ApplyMode::Destroy => generate::pwa::plan_pwa_destroy_fallback(&project_root),
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Tauri {
            dry_run,
            force,
            remote_url,
        } => {
            let flags = generate::Flags { dry_run, force };
            let project_root = resolve_cwd();
            // Mixed-mode guard (issue #1506): generating one Tauri mode over
            // the other's files is rejected outright, even with --force —
            // --force means "overwrite within the same mode", and the other
            // mode's leftovers would actively break the new scaffold's build
            // (stale capability files fail tauri-build validation on desktop;
            // stale per-OS overlays keep running the sidecar staging scripts
            // for a thin client). Destroy is exempt: `autumn destroy tauri
            // [--remote-url <URL>]` is the documented remedy and must keep
            // working on a mixed tree.
            let guard = if mode == ApplyMode::Generate {
                generate::tauri::ensure_no_opposite_mode_scaffold(
                    &project_root,
                    remote_url.is_some(),
                )
            } else {
                Ok(())
            };
            let plan = guard.and_then(|()| {
                remote_url.as_ref().map_or_else(
                    || generate::tauri::plan_tauri(&project_root),
                    |url| generate::tauri::plan_tauri_thin_client(&project_root, url),
                )
            });
            let result = plan.and_then(|p| match mode {
                ApplyMode::Generate => p.execute(flags),
                ApplyMode::Destroy => p.revert(flags),
            });
            match result {
                Ok(()) => {
                    if mode == ApplyMode::Generate && !dry_run {
                        match &remote_url {
                            Some(url) => println!(
                                "\n{}",
                                generate::tauri::render_thin_client_prerequisites(url)
                            ),
                            None => println!("\n{}", generate::tauri::render_prerequisites()),
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        GenerateCommands::TauriMobile {
            offline_sync,
            dry_run,
            force,
        } => {
            let flags = generate::Flags { dry_run, force };
            let opts = generate::tauri_mobile::TauriMobileOptions { offline_sync };
            let project_root = resolve_cwd();
            // Mixed-mode guard (mirrors the `tauri` arm above): generating
            // the mobile in-process shell over a desktop-sidecar or
            // thin-client src-tauri/ is rejected outright, even with --force
            // — the other mode's leftovers (per-OS overlay confs, staging
            // scripts, capability files) actively break the mobile build.
            // Destroy is exempt: it is the documented remedy and must keep
            // working on a mixed tree.
            let guard = if mode == ApplyMode::Generate {
                generate::tauri_mobile::ensure_no_other_mode_scaffold(&project_root)
            } else {
                Ok(())
            };
            let plan =
                guard.and_then(|()| generate::tauri_mobile::plan_tauri_mobile(&project_root, opts));
            let result = plan.and_then(|p| match mode {
                ApplyMode::Generate => p.execute(flags),
                ApplyMode::Destroy => p.revert(flags),
            });
            match result {
                Ok(()) => {
                    if mode == ApplyMode::Generate && !dry_run {
                        println!(
                            "\n{}",
                            generate::tauri_mobile::render_mobile_prerequisites(opts)
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        GenerateCommands::Auth {
            name,
            oauth,
            totp,
            passkeys,
            magic_link,
            dry_run,
            force,
        } => {
            let oauth_options = generate::auth::AuthOAuthOptions { providers: oauth };
            let timestamp = generate::timestamp_now();
            // `destroy auth` recomputes this same plan before reverting it. The
            // shared-layout preflight in the plan builder is a generate-time
            // guard only: running it on the destroy path would hard-fail cleanup
            // in a project whose shared `pub fn layout` is missing or renamed,
            // stranding the generated files (issue #1353 follow-up). Use the
            // revert-only plan builder for `ApplyMode::Destroy`, which skips it.
            let plan = match mode {
                ApplyMode::Generate => generate::auth::plan_auth_full_ex2(
                    &resolve_cwd(),
                    &name,
                    &timestamp,
                    &oauth_options,
                    totp,
                    passkeys,
                    magic_link,
                ),
                ApplyMode::Destroy => generate::auth::plan_auth_full_ex2_for_revert(
                    &resolve_cwd(),
                    &name,
                    &timestamp,
                    &oauth_options,
                    totp,
                    passkeys,
                    magic_link,
                ),
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
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
            let project_root = resolve_cwd();
            // `plan_admin_with_options` reads `src/models/<name>.rs` to
            // detect fields/encrypted columns for rendering — meaningless
            // (and an error) once the model is gone. A common cleanup order
            // like `destroy model Post` then `destroy admin Post` would
            // otherwise fail before ever reaching `Plan::revert`, stranding
            // `src/admin/post.rs` (issue #1048 PR review). Fall back to a
            // model-independent plan in that specific case.
            let model_missing = mode == ApplyMode::Destroy
                && !project_root
                    .join("src")
                    .join("models")
                    .join(format!("{}.rs", generate::naming::snake(&name)))
                    .exists();
            let plan = if model_missing {
                generate::admin::plan_admin_destroy_fallback(&project_root, &name)
            } else {
                generate::admin::plan_admin_with_options(&project_root, &name, &fields, &options)
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Wizard {
            name,
            steps,
            dry_run,
            force,
        } => {
            let plan = generate::wizard::plan_wizard(&resolve_cwd(), &name, &steps);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Controller {
            name,
            actions,
            api,
            dry_run,
            force,
        } => {
            let plan = generate::controller::plan_controller(&resolve_cwd(), &name, &actions, api);
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Scaffold {
            name,
            fields,
            index,
            unique,
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
            no_policy,
            belongs_to,
            counter_cache,
            searchable,
            i18n,
            import,
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
                &unique,
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
                no_policy,
                belongs_to.as_deref(),
                counter_cache,
                &searchable,
                i18n,
                import,
            ) {
                Ok(result) => result,
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            };
            let timestamp = generate::timestamp_now();
            // `destroy scaffold` recomputes this same plan before reverting it.
            // The shared-layout preflight in the plan builder is a generate-time
            // guard only: running it on the destroy path would hard-fail cleanup
            // in a project whose shared `pub fn layout` is missing or renamed,
            // stranding the generated files (issue #1834). Use the revert-only
            // plan builder for `ApplyMode::Destroy`, which skips it.
            let plan = match mode {
                ApplyMode::Generate => generate::scaffold::plan_scaffold_with_options(
                    &resolve_cwd(),
                    &name,
                    &fields,
                    &timestamp,
                    &options,
                ),
                ApplyMode::Destroy => generate::scaffold::plan_scaffold_with_options_for_revert(
                    &resolve_cwd(),
                    &name,
                    &fields,
                    &timestamp,
                    &options,
                ),
            };
            apply_plan(plan, generate::Flags { dry_run, force }, mode);
        }
        GenerateCommands::Plugin {
            name,
            path,
            dry_run,
            force,
        } => {
            let cwd = resolve_cwd();
            let flags = generate::Flags { dry_run, force };
            // `plan_plugin` refuses a non-empty target directory unless
            // `--force` — a generate-time collision guard that makes no
            // sense in destroy mode, where the directory legitimately
            // exists (holding the files this destroy is about to remove).
            // Always bypass it when building the plan for destroy, while
            // still passing the user's real `flags` to `revert` below so
            // its own, per-file content-divergence check still applies
            // (issue #1048 PR review).
            let plan_flags = match mode {
                ApplyMode::Generate => flags,
                ApplyMode::Destroy => generate::Flags {
                    force: true,
                    ..flags
                },
            };
            match generate::plugin::plan_plugin(
                &cwd,
                &name,
                path.as_deref().map(std::path::Path::new),
                plan_flags,
            ) {
                Ok(plugin_plan) => {
                    let result = match mode {
                        ApplyMode::Generate => plugin_plan.plan.execute(flags),
                        ApplyMode::Destroy => plugin_plan.plan.revert(flags),
                    };
                    match result {
                        Ok(()) => {
                            if mode == ApplyMode::Generate && !dry_run {
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
    fn parse_sbom_defaults_to_stdout() {
        let cli = Cli::try_parse_from(["autumn", "sbom"]).unwrap();
        let Commands::Sbom {
            output,
            verify,
            binary,
            locked,
            manifest_path,
            expect_version,
            all_features,
            features,
            filter_platform,
        } = cli.command
        else {
            panic!("expected Sbom command");
        };
        assert!(output.is_none());
        assert!(verify.is_none());
        assert!(binary.is_none());
        assert!(manifest_path.is_none());
        assert!(expect_version.is_none());
        assert!(
            !locked,
            "--locked must be opt-in so a stale app lockfile still builds"
        );
        assert!(
            !all_features,
            "the default feature set is what a build actually links, so it is \
             what the document describes by default"
        );
        assert!(features.is_none());
        assert!(
            filter_platform.is_none(),
            "a source release is consumed on every platform, so no filter by default"
        );
    }

    #[test]
    fn parse_sbom_filter_platform() {
        let cli = Cli::try_parse_from([
            "autumn",
            "sbom",
            "--filter-platform",
            "aarch64-unknown-linux-gnu",
        ])
        .unwrap();
        let Commands::Sbom {
            filter_platform, ..
        } = cli.command
        else {
            panic!("expected Sbom command");
        };
        assert_eq!(
            filter_platform.as_deref(),
            Some("aarch64-unknown-linux-gnu")
        );
    }

    #[test]
    fn parse_sbom_features() {
        let cli = Cli::try_parse_from(["autumn", "sbom", "--features", "embed-assets"]).unwrap();
        let Commands::Sbom { features, .. } = cli.command else {
            panic!("expected Sbom command");
        };
        assert_eq!(features.as_deref(), Some("embed-assets"));
    }

    #[test]
    fn parse_sbom_output_and_locked() {
        let cli = Cli::try_parse_from(["autumn", "sbom", "--output", "sbom.cdx.json", "--locked"])
            .unwrap();
        let Commands::Sbom { output, locked, .. } = cli.command else {
            panic!("expected Sbom command");
        };
        assert_eq!(output.unwrap().to_str().unwrap(), "sbom.cdx.json");
        assert!(locked);
    }

    #[test]
    fn parse_sbom_expect_version() {
        let cli = Cli::try_parse_from(["autumn", "sbom", "--expect-version", "0.7.0"]).unwrap();
        let Commands::Sbom { expect_version, .. } = cli.command else {
            panic!("expected Sbom command");
        };
        assert_eq!(expect_version.as_deref(), Some("0.7.0"));
    }

    #[test]
    fn parse_sbom_verify() {
        let cli = Cli::try_parse_from(["autumn", "sbom", "--verify", "sbom.cdx.json"]).unwrap();
        let Commands::Sbom { verify, .. } = cli.command else {
            panic!("expected Sbom command");
        };
        assert_eq!(verify.unwrap().to_str().unwrap(), "sbom.cdx.json");
    }

    #[test]
    fn parse_sbom_binary() {
        let cli =
            Cli::try_parse_from(["autumn", "sbom", "--binary", "/usr/local/bin/app"]).unwrap();
        let Commands::Sbom { binary, .. } = cli.command else {
            panic!("expected Sbom command");
        };
        assert_eq!(binary.unwrap().to_str().unwrap(), "/usr/local/bin/app");
    }

    #[test]
    fn sbom_rejects_verifying_and_writing_at_once() {
        // Both would be silently contradictory: `--verify` never writes.
        assert!(
            Cli::try_parse_from(["autumn", "sbom", "--verify", "a.json", "--output", "b.json"])
                .is_err()
        );
    }

    #[test]
    fn sbom_rejects_a_binary_combined_with_source_tree_flags() {
        // Each of these only means something when resolving a manifest; with
        // `--binary` they would be silently ignored.
        for flag in [
            vec!["--locked"],
            vec!["--all-features"],
            vec!["--features", "embed-assets"],
            vec!["--filter-platform", "x86_64-unknown-linux-gnu"],
            vec!["--verify", "sbom.cdx.json"],
        ] {
            let mut args = vec!["autumn", "sbom", "--binary", "app"];
            args.extend(flag.iter().copied());
            assert!(
                Cli::try_parse_from(&args).is_err(),
                "`autumn sbom --binary` must reject {flag:?}"
            );
        }
    }

    #[test]
    fn sbom_rejects_a_binary_and_a_manifest_at_once() {
        // `--binary` reads a compiled artifact; a manifest path is meaningless
        // there and would quietly be ignored.
        assert!(
            Cli::try_parse_from([
                "autumn",
                "sbom",
                "--binary",
                "app",
                "--manifest-path",
                "Cargo.toml"
            ])
            .is_err()
        );
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
                edge: false,
                auditable: false,
            }
        ));
    }

    #[test]
    fn parse_build_auditable() {
        // The production Dockerfile passes this so the shipped binary carries
        // its own dependency list (issue #1615).
        let cli = Cli::try_parse_from(["autumn", "build", "--embed", "--auditable"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Build {
                embed: true,
                auditable: true,
                ..
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
                edge: false,
                auditable: false,
            }
        ));
    }

    #[test]
    fn parse_build_edge_flag() {
        let cli = Cli::try_parse_from(["autumn", "build", "--edge"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Build {
                debug: false,
                embed: false,
                edge: true,
                ..
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

    /// Issue #1623, AC3: a worker-role process must be pinnable to a subset of
    /// queues "via config/flags". `autumn serve --pin` is the flag half; it
    /// accepts a comma-separated list (matching `AUTUMN_JOBS__PIN`, which it
    /// forwards) and may be repeated.
    #[test]
    fn serve_parses_pin_as_a_comma_separated_list() {
        let cli = Cli::try_parse_from([
            "autumn",
            "serve",
            "--role",
            "worker",
            "--pin",
            "critical,default",
        ])
        .unwrap();
        match cli.command {
            Commands::Serve { pin, .. } => {
                assert_eq!(pin, vec!["critical".to_owned(), "default".to_owned()]);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn serve_pin_can_be_repeated() {
        let cli =
            Cli::try_parse_from(["autumn", "serve", "--pin", "critical", "--pin", "bulk"]).unwrap();
        match cli.command {
            Commands::Serve { pin, .. } => {
                assert_eq!(pin, vec!["critical".to_owned(), "bulk".to_owned()]);
            }
            _ => panic!("expected Serve command"),
        }
    }

    /// AC4: an app that configures nothing new keeps today's behavior, so a bare
    /// `autumn serve` must produce an empty pin (the CLI then leaves
    /// `AUTUMN_JOBS__PIN` untouched and the child reads its own config).
    #[test]
    fn serve_pin_defaults_to_empty() {
        let cli = Cli::try_parse_from(["autumn", "serve"]).unwrap();
        match cli.command {
            Commands::Serve { pin, .. } => assert!(pin.is_empty()),
            _ => panic!("expected Serve command"),
        }
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

    #[test]
    fn parse_db_backup_defaults() {
        let cli = Cli::try_parse_from(["autumn", "db", "backup"]).unwrap();
        let Commands::Db(DbCommands::Backup {
            profile,
            dir,
            format,
            keep,
            shard,
            control_only,
            upload,
        }) = cli.command
        else {
            panic!("expected db backup");
        };
        assert!(profile.is_none());
        assert!(dir.is_none());
        assert_eq!(format, "custom");
        assert!(keep.is_none());
        assert!(shard.is_none());
        assert!(!control_only);
        assert!(!upload);
    }

    #[test]
    fn parse_db_backup_with_all_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "db",
            "backup",
            "--profile",
            "prod",
            "--dir",
            "/var/backups",
            "--format",
            "plain",
            "--keep",
            "7",
            "--shard",
            "east",
            "--upload",
        ])
        .unwrap();
        let Commands::Db(DbCommands::Backup {
            profile,
            dir,
            format,
            keep,
            shard,
            control_only,
            upload,
        }) = cli.command
        else {
            panic!("expected db backup");
        };
        assert_eq!(profile.as_deref(), Some("prod"));
        assert_eq!(dir.as_deref(), Some(std::path::Path::new("/var/backups")));
        assert_eq!(format, "plain");
        assert_eq!(keep, Some(7));
        assert_eq!(shard.as_deref(), Some("east"));
        assert!(!control_only);
        assert!(upload);
    }

    #[test]
    fn parse_db_backup_shard_conflicts_with_control_only() {
        assert!(
            Cli::try_parse_from([
                "autumn",
                "db",
                "backup",
                "--shard",
                "east",
                "--control-only",
            ])
            .is_err()
        );
    }

    #[test]
    fn parse_db_restore_requires_artifact() {
        assert!(Cli::try_parse_from(["autumn", "db", "restore"]).is_err());
    }

    #[test]
    fn parse_db_restore_with_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "db",
            "restore",
            "backups/prod/20260710T040506Z",
            "--force",
            "--profile",
            "prod",
            "--shard",
            "east",
        ])
        .unwrap();
        let Commands::Db(DbCommands::Restore {
            artifact,
            profile,
            force,
            shard,
            offsite,
        }) = cli.command
        else {
            panic!("expected db restore");
        };
        assert_eq!(
            artifact,
            std::path::PathBuf::from("backups/prod/20260710T040506Z")
        );
        assert_eq!(profile.as_deref(), Some("prod"));
        assert!(force);
        assert_eq!(shard.as_deref(), Some("east"));
        assert!(!offsite);
    }

    #[test]
    fn parse_db_restore_offsite_flag() {
        let cli = Cli::try_parse_from([
            "autumn",
            "db",
            "restore",
            "prod/latest",
            "--offsite",
            "--force",
        ])
        .unwrap();
        let Commands::Db(DbCommands::Restore {
            artifact,
            offsite,
            force,
            ..
        }) = cli.command
        else {
            panic!("expected db restore");
        };
        assert_eq!(artifact, std::path::PathBuf::from("prod/latest"));
        assert!(offsite);
        assert!(force);
    }

    #[test]
    fn parse_db_offsite_list() {
        let cli =
            Cli::try_parse_from(["autumn", "db", "offsite", "list", "--profile", "prod"]).unwrap();
        let Commands::Db(DbCommands::Offsite(OffsiteCommands::List { profile })) = cli.command
        else {
            panic!("expected db offsite list");
        };
        assert_eq!(profile.as_deref(), Some("prod"));
    }

    // ── autumn db retention tests (issue #1605) ────────────────────────────

    #[test]
    fn parse_db_retention_defaults_to_a_read_only_report() {
        let cli = Cli::try_parse_from(["autumn", "db", "retention"]).unwrap();
        let Commands::Db(DbCommands::Retention {
            profile,
            dataset,
            dry_run,
            purge,
            json,
            ..
        }) = cli.command
        else {
            panic!("expected db retention");
        };
        assert_eq!(profile, "dev");
        assert_eq!(dataset, None);
        assert!(!dry_run);
        assert!(!purge, "the bare command must never delete anything");
        assert!(!json);
        assert_eq!(
            db_retention_mode(dry_run, purge),
            db::retention::RetentionMode::Report
        );
    }

    #[test]
    fn parse_db_retention_with_dataset_and_purge() {
        let cli = Cli::try_parse_from([
            "autumn",
            "db",
            "retention",
            "--purge",
            "--dataset",
            "job_history",
            "--profile",
            "prod",
            "--json",
        ])
        .unwrap();
        let Commands::Db(DbCommands::Retention {
            profile,
            dataset,
            dry_run,
            purge,
            json,
            ..
        }) = cli.command
        else {
            panic!("expected db retention");
        };
        assert_eq!(profile, "prod");
        assert_eq!(dataset.as_deref(), Some("job_history"));
        assert!(!dry_run);
        assert!(purge);
        assert!(json);
        assert_eq!(
            db_retention_mode(dry_run, purge),
            db::retention::RetentionMode::Purge
        );
    }

    #[test]
    fn parse_db_retention_dry_run_conflicts_with_purge() {
        assert!(
            Cli::try_parse_from(["autumn", "db", "retention", "--dry-run", "--purge"]).is_err(),
            "--dry-run and --purge are contradictory and must not both be accepted"
        );
    }

    #[test]
    fn db_retention_mode_falls_back_to_report() {
        assert_eq!(
            db_retention_mode(false, false),
            db::retention::RetentionMode::Report
        );
        assert_eq!(
            db_retention_mode(true, false),
            db::retention::RetentionMode::DryRun
        );
    }

    // ── autumn db scrub tests (issue #1602) ────────────────────────────────

    #[test]
    fn parse_db_scrub_defaults() {
        let cli = Cli::try_parse_from(["autumn", "db", "scrub"]).unwrap();
        let Commands::Db(DbCommands::Scrub {
            profile,
            artifact,
            output,
            config,
            check,
            dry_run,
            force,
            allow_source_overwrite,
            sample,
            seed,
        }) = cli.command
        else {
            panic!("expected db scrub");
        };
        assert!(!allow_source_overwrite);
        assert!(profile.is_none());
        assert!(artifact.is_none());
        assert!(output.is_none());
        assert!(config.is_none());
        assert!(!check);
        assert!(!dry_run);
        assert!(!force);
        assert!(sample.is_empty(), "sampling is opt-in");
        assert_eq!(seed, 0);
    }

    #[test]
    fn parse_db_scrub_with_artifact_output_and_force() {
        let cli = Cli::try_parse_from([
            "autumn",
            "db",
            "scrub",
            "--profile",
            "staging",
            "--artifact",
            "backups/prod/20260101T000000Z",
            "--output",
            "scrubbed",
            "--config",
            "config/scrub.toml",
            "--force",
        ])
        .unwrap();
        let Commands::Db(DbCommands::Scrub {
            profile,
            artifact,
            output,
            config,
            check,
            dry_run,
            force,
            allow_source_overwrite,
            sample: _,
            seed: _,
        }) = cli.command
        else {
            panic!("expected db scrub");
        };
        assert!(!allow_source_overwrite);
        assert_eq!(profile.as_deref(), Some("staging"));
        assert_eq!(
            artifact.as_deref(),
            Some(std::path::Path::new("backups/prod/20260101T000000Z"))
        );
        assert_eq!(output.as_deref(), Some(std::path::Path::new("scrubbed")));
        assert_eq!(
            config.as_deref(),
            Some(std::path::Path::new("config/scrub.toml"))
        );
        assert!(!check);
        assert!(!dry_run);
        assert!(force);
    }

    // ── autumn db scrub --sample tests (issue #1636) ───────────────────────

    #[test]
    fn parse_db_scrub_with_repeated_sample_roots_and_a_seed() {
        let cli = Cli::try_parse_from([
            "autumn",
            "db",
            "scrub",
            "--sample",
            "users=1%",
            "--sample",
            "orders=500",
            "--seed",
            "42",
        ])
        .unwrap();
        let Commands::Db(DbCommands::Scrub { sample, seed, .. }) = cli.command else {
            panic!("expected db scrub");
        };
        assert_eq!(sample, vec!["users=1%".to_owned(), "orders=500".to_owned()]);
        assert_eq!(seed, 42);
    }

    #[test]
    fn parse_db_scrub_seed_requires_sample() {
        // A seed with nothing to seed is a mistyped command, not a no-op: it
        // reads as "this run is reproducible" when nothing was subsetted.
        assert!(
            Cli::try_parse_from(["autumn", "db", "scrub", "--seed", "42"]).is_err(),
            "--seed only means something alongside --sample"
        );
    }

    #[test]
    fn parse_db_scrub_sample_works_with_check_and_dry_run() {
        // Both write nothing, and both must still be able to prove the sample
        // plan is complete — that is the CI gate for a graph gap.
        for mode in ["--check", "--dry-run"] {
            assert!(
                Cli::try_parse_from(["autumn", "db", "scrub", "--sample", "users=1%", mode])
                    .is_ok(),
                "--sample must be inspectable with {mode}"
            );
        }
    }

    #[test]
    fn parse_db_scrub_check_conflicts_with_dry_run() {
        assert!(
            Cli::try_parse_from(["autumn", "db", "scrub", "--check", "--dry-run"]).is_err(),
            "--check and --dry-run are two different no-write modes; asking for both is a mistake"
        );
    }

    // ── autumn console tests (issue #1039) ─────────────────────────────────

    #[test]
    fn parse_console_defaults() {
        let cli = Cli::try_parse_from(["autumn", "console"]).unwrap();
        match cli.command {
            Commands::Console {
                profile,
                package,
                force,
                scaffold_only,
            } => {
                assert_eq!(profile, "dev");
                assert!(package.is_none());
                assert!(!force);
                assert!(!scaffold_only);
            }
            _ => panic!("expected Console command"),
        }
    }

    #[test]
    fn parse_console_short_alias_c() {
        let cli = Cli::try_parse_from(["autumn", "c"]).unwrap();
        assert!(matches!(cli.command, Commands::Console { .. }));
    }

    #[test]
    fn parse_console_with_force() {
        let cli = Cli::try_parse_from(["autumn", "console", "--force"]).unwrap();
        match cli.command {
            Commands::Console { force, .. } => assert!(force),
            _ => panic!("expected Console command"),
        }
    }

    #[test]
    fn parse_console_with_scaffold_only() {
        let cli = Cli::try_parse_from(["autumn", "console", "--scaffold-only"]).unwrap();
        match cli.command {
            Commands::Console { scaffold_only, .. } => assert!(scaffold_only),
            _ => panic!("expected Console command"),
        }
    }

    #[test]
    fn parse_console_with_profile_and_package() {
        let cli = Cli::try_parse_from(["autumn", "console", "--profile", "demo", "-p", "my-app"])
            .unwrap();
        match cli.command {
            Commands::Console {
                profile, package, ..
            } => {
                assert_eq!(profile, "demo");
                assert_eq!(package.as_deref(), Some("my-app"));
            }
            _ => panic!("expected Console command"),
        }
    }

    // ── autumn seed tests ──────────────────────────────────────────────────

    #[test]
    fn parse_seed_defaults() {
        let cli = Cli::try_parse_from(["autumn", "seed"]).unwrap();
        match cli.command {
            Commands::Seed {
                profile,
                package,
                count,
                model,
                yes_i_mean_prod,
            } => {
                assert_eq!(profile, "dev");
                assert!(package.is_none());
                assert!(count.is_none());
                assert!(model.is_none());
                assert!(!yes_i_mean_prod);
            }
            _ => panic!("expected Seed command"),
        }
    }

    #[test]
    fn parse_seed_with_yes_i_mean_prod() {
        let cli = Cli::try_parse_from([
            "autumn",
            "seed",
            "--count",
            "200",
            "--model",
            "Post",
            "--profile",
            "prod",
            "--yes-i-mean-prod",
        ])
        .unwrap();
        match cli.command {
            Commands::Seed {
                yes_i_mean_prod, ..
            } => assert!(yes_i_mean_prod),
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
    fn parse_seed_with_count_and_model() {
        let cli =
            Cli::try_parse_from(["autumn", "seed", "--count", "200", "--model", "Post"]).unwrap();
        match cli.command {
            Commands::Seed { count, model, .. } => {
                assert_eq!(count, Some(200));
                assert_eq!(model.as_deref(), Some("Post"));
            }
            _ => panic!("expected Seed command"),
        }
    }

    #[test]
    fn parse_seed_with_count_only() {
        // `--count` declares `requires = "model"`, so clap rejects it at parse
        // time when `--model` is absent. `resolve_fake_request` remains as a
        // secondary run-time guard for callers that bypass clap.
        assert!(Cli::try_parse_from(["autumn", "seed", "--count", "50"]).is_err());
    }

    #[test]
    fn parse_seed_with_model_only() {
        // Symmetrically, `--model` requires `--count`.
        assert!(Cli::try_parse_from(["autumn", "seed", "--model", "Post"]).is_err());
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
                command,
            } => {
                assert!(package.is_none());
                assert!(bin.is_none());
                assert_eq!(format, "table");
                assert!(prefix.is_none());
                assert!(filter.is_none());
                assert!(method.is_empty());
                assert!(!user_only);
                assert!(command.is_none());
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
    fn parse_jobs_manifest_with_path_and_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "jobs",
            "manifest",
            "target/jobs-manifest.toml",
            "--package",
            "myapp",
            "--bin",
            "server",
        ])
        .unwrap();
        match cli.command {
            Commands::Jobs {
                action: JobsSubcommands::Manifest { path, package, bin },
            } => {
                assert_eq!(path, "target/jobs-manifest.toml");
                assert_eq!(package.as_deref(), Some("myapp"));
                assert_eq!(bin.as_deref(), Some("server"));
            }
            _ => panic!("expected Jobs manifest command"),
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
                command,
            } => {
                assert_eq!(package.as_deref(), Some("blog"));
                assert!(bin.is_none());
                assert_eq!(format, "json");
                assert!(prefix.is_none());
                assert_eq!(filter.as_deref(), Some("/api"));
                assert_eq!(method, vec!["GET", "POST"]);
                assert!(user_only);
                assert!(command.is_none());
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

    #[test]
    fn parse_routes_audit_subcommand() {
        let cli = Cli::try_parse_from([
            "autumn",
            "routes",
            "audit",
            "--manifest",
            "target/security.json",
            "--json",
            "-p",
            "blog",
        ])
        .unwrap();
        match cli.command {
            Commands::Routes {
                command:
                    Some(RoutesSubcommands::Audit {
                        package,
                        manifest,
                        json,
                        strict,
                        ..
                    }),
                ..
            } => {
                assert_eq!(package.as_deref(), Some("blog"));
                assert_eq!(manifest.as_deref(), Some("target/security.json"));
                assert!(json);
                assert!(!strict);
            }
            _ => panic!("expected Routes audit subcommand"),
        }
    }

    /// Parent `-p`/`--bin` flags placed before the `audit` subcommand land on
    /// the parent `Routes` variant, not on `Audit`. The dispatch falls back to
    /// them (`audit.package.or(parent.package)`), so `autumn routes -p blog
    /// audit` and `autumn routes audit -p blog` resolve the same target (#1604).
    #[test]
    fn parse_routes_parent_package_flows_into_audit() {
        // `-p blog` BEFORE `audit`: parent carries it, audit's own is None.
        let cli = Cli::try_parse_from(["autumn", "routes", "-p", "blog", "audit"]).unwrap();
        match cli.command {
            Commands::Routes {
                package: parent_package,
                bin: parent_bin,
                command:
                    Some(RoutesSubcommands::Audit {
                        package: audit_package,
                        bin: audit_bin,
                        ..
                    }),
                ..
            } => {
                assert_eq!(parent_package.as_deref(), Some("blog"));
                assert_eq!(audit_package, None);
                // The fallback the dispatch applies.
                let resolved_package = audit_package.or(parent_package);
                let resolved_bin = audit_bin.or(parent_bin);
                assert_eq!(resolved_package.as_deref(), Some("blog"));
                assert_eq!(resolved_bin, None);
            }
            _ => panic!("expected Routes audit subcommand"),
        }
    }

    /// The subcommand's own `-p`/`--bin` (placed after `audit`) still win — the
    /// fallback only fills in when the audit value is `None`.
    #[test]
    fn parse_routes_audit_own_package_takes_precedence() {
        let cli =
            Cli::try_parse_from(["autumn", "routes", "-p", "blog", "audit", "-p", "shop"]).unwrap();
        match cli.command {
            Commands::Routes {
                package: parent_package,
                command:
                    Some(RoutesSubcommands::Audit {
                        package: audit_package,
                        ..
                    }),
                ..
            } => {
                assert_eq!(parent_package.as_deref(), Some("blog"));
                assert_eq!(audit_package.as_deref(), Some("shop"));
                let resolved = audit_package.or(parent_package);
                assert_eq!(resolved.as_deref(), Some("shop"));
            }
            _ => panic!("expected Routes audit subcommand"),
        }
    }

    #[test]
    fn parse_routes_without_subcommand_lists() {
        let cli = Cli::try_parse_from(["autumn", "routes"]).unwrap();
        match cli.command {
            Commands::Routes { command, .. } => assert!(command.is_none()),
            _ => panic!("expected Routes command"),
        }
    }

    // ── autumn cache audit tests (#1716) ───────────────────────────────────

    #[test]
    fn parse_cache_audit_defaults() {
        let cli = Cli::try_parse_from(["autumn", "cache", "audit"]).unwrap();
        match cli.command {
            Commands::Cache(CacheSubcommands::Audit(args)) => {
                assert!(args.package.is_none());
                assert!(args.bin.is_none());
                assert!(args.manifest.is_none());
                assert!(!args.json);
                // The default gate never fails on what it merely could not
                // read; `--strict` is opt-in.
                assert!(!args.strict);
                assert!(args.features.is_empty());
                assert!(!args.all_features);
                assert!(!args.no_default_features);
            }
            _ => panic!("expected Cache audit subcommand"),
        }
    }

    /// The manifest describes the binary that produced it, so the audited
    /// build has to be the one that ships. A read or a repository behind a
    /// non-default feature is not compiled into a default build at all — it
    /// cannot appear in the manifest, and the gate exits green on a
    /// configuration it never looked at.
    #[test]
    fn parse_cache_audit_forwards_the_cargo_feature_selection() {
        let cli = Cli::try_parse_from([
            "autumn",
            "cache",
            "audit",
            "--no-default-features",
            "--features",
            "db,cache-moka",
            "--features",
            "redis",
        ])
        .unwrap();
        match cli.command {
            Commands::Cache(CacheSubcommands::Audit(args)) => {
                assert_eq!(args.features, vec!["db,cache-moka", "redis"]);
                assert!(args.no_default_features);
                assert!(!args.all_features);
            }
            _ => panic!("expected Cache audit subcommand"),
        }

        let all = Cli::try_parse_from(["autumn", "cache", "audit", "--all-features"]).unwrap();
        match all.command {
            Commands::Cache(CacheSubcommands::Audit(args)) => assert!(args.all_features),
            _ => panic!("expected Cache audit subcommand"),
        }
    }

    #[test]
    fn parse_cache_audit_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "cache",
            "audit",
            "-p",
            "blog",
            "--bin",
            "server",
            "--manifest",
            "target/cache-coherence.json",
            "--json",
            "--strict",
        ])
        .unwrap();
        match cli.command {
            Commands::Cache(CacheSubcommands::Audit(args)) => {
                assert_eq!(args.package.as_deref(), Some("blog"));
                assert_eq!(args.bin.as_deref(), Some("server"));
                assert_eq!(
                    args.manifest.as_deref(),
                    Some("target/cache-coherence.json")
                );
                assert!(args.json);
                assert!(args.strict);
            }
            _ => panic!("expected Cache audit subcommand"),
        }
    }

    #[test]
    fn cache_requires_a_subcommand() {
        assert!(Cli::try_parse_from(["autumn", "cache"]).is_err());
    }

    /// A variant inserted between a doc comment and the variant it documents
    /// silently steals that help text and leaves the other with none — which is
    /// exactly what happened to `routes` when `cache` was added.
    ///
    /// Runs on its own 16 MiB thread for the reason documented on
    /// [`UpgradeArgs`]: building the whole `Command` tree walks
    /// `Commands::augment_subcommands`, whose stack frame is already close to
    /// libtest's 2 MiB per-test stack. This is the only test that materializes
    /// it, so it brings its own headroom rather than making the suite depend on
    /// `RUST_MIN_STACK`.
    #[test]
    fn every_command_has_its_own_help_text() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                use clap::CommandFactory as _;
                let cmd = Cli::command();
                for name in ["cache", "routes"] {
                    let sub = cmd
                        .get_subcommands()
                        .find(|s| s.get_name() == name)
                        .unwrap_or_else(|| panic!("`{name}` subcommand must exist"));
                    let about = sub
                        .get_about()
                        .unwrap_or_else(|| panic!("`{name}` must have help text"))
                        .to_string();
                    assert!(!about.trim().is_empty(), "`{name}` has empty help text");
                    assert!(
                        !about.contains("mounted route") || name == "routes",
                        "`{name}` is showing another command's help: {about}"
                    );
                }
                // And clap itself is satisfied with the whole definition.
                cmd.debug_assert();
            })
            .expect("spawn help-text check thread")
            .join()
            .expect("help-text check panicked");
    }

    // ── autumn doctor tests ────────────────────────────────────────────────

    #[test]
    fn parse_doctor_defaults() {
        let cli = Cli::try_parse_from(["autumn", "doctor"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Doctor {
                json: false,
                strict: false,
                online: false
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
                strict: false,
                online: false
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
                strict: true,
                online: false
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
                strict: true,
                online: false
            }
        ));
    }

    #[test]
    fn parse_doctor_online_flag_and_preflight_alias() {
        for flag in ["--online", "--preflight"] {
            let cli = Cli::try_parse_from(["autumn", "doctor", flag]).unwrap();
            assert!(matches!(
                cli.command,
                Commands::Doctor {
                    json: false,
                    strict: false,
                    online: true
                }
            ));
        }
    }

    // ── autumn i18n tests ──────────────────────────────────────────────────

    #[test]
    fn parse_i18n_check_defaults() {
        let cli = Cli::try_parse_from(["autumn", "i18n", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::I18n {
                action: I18nSubcommands::Check {
                    ref format,
                    strict: false,
                }
            } if format == "text"
        ));
    }

    #[test]
    fn parse_i18n_check_json_and_strict() {
        let cli = Cli::try_parse_from(["autumn", "i18n", "check", "--format", "json", "--strict"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Commands::I18n {
                action: I18nSubcommands::Check {
                    ref format,
                    strict: true,
                }
            } if format == "json"
        ));
    }

    // ── autumn a11y tests ──────────────────────────────────────────────────

    #[test]
    fn parse_a11y_verify_defaults() {
        let cli = Cli::try_parse_from(["autumn", "a11y", "verify"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::A11y {
                action: A11ySubcommands::Verify {
                    ref path,
                    ref format,
                    strict: false,
                }
            } if path == "." && format == "text"
        ));
    }

    #[test]
    fn parse_a11y_verify_path_json_and_strict() {
        let cli = Cli::try_parse_from([
            "autumn",
            "a11y",
            "verify",
            "./crates/web",
            "--format",
            "json",
            "--strict",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::A11y {
                action: A11ySubcommands::Verify {
                    ref path,
                    ref format,
                    strict: true,
                }
            } if path == "./crates/web" && format == "json"
        ));
    }

    // ── autumn release tests ───────────────────────────────────────────────

    #[test]
    fn parse_release_init_defaults() {
        let cli = Cli::try_parse_from(["autumn", "release", "init"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert!(!force);
        assert!(target.is_none());
    }

    #[test]
    fn parse_release_init_with_force() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--force"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert!(force);
        assert!(target.is_none());
    }

    #[test]
    fn parse_release_init_with_fly_target() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--target", "fly"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target, .. }) = cli.command else {
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
    fn parse_release_init_with_azure_container_apps_target() {
        let cli = Cli::try_parse_from([
            "autumn",
            "release",
            "init",
            "--target",
            "azure-container-apps",
        ])
        .unwrap();
        let Commands::Release(ReleaseCommands::Init { target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert_eq!(target.as_deref(), Some("azure-container-apps"));
    }

    #[test]
    fn parse_release_init_with_aws_app_runner_target() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--target", "aws-app-runner"])
            .unwrap();
        let Commands::Release(ReleaseCommands::Init { target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert_eq!(target.as_deref(), Some("aws-app-runner"));
    }

    #[test]
    fn parse_release_init_with_aws_ecs_target() {
        let cli =
            Cli::try_parse_from(["autumn", "release", "init", "--target", "aws-ecs"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert_eq!(target.as_deref(), Some("aws-ecs"));
    }

    #[test]
    fn parse_release_init_with_gcp_cloud_run_target() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--target", "gcp-cloud-run"])
            .unwrap();
        let Commands::Release(ReleaseCommands::Init { target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert_eq!(target.as_deref(), Some("gcp-cloud-run"));
    }

    #[test]
    fn parse_release_init_force_and_target() {
        let cli = Cli::try_parse_from(["autumn", "release", "init", "--force", "--target", "fly"])
            .unwrap();
        let Commands::Release(ReleaseCommands::Init { force, target, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert!(force);
        assert_eq!(target.as_deref(), Some("fly"));
    }

    #[test]
    fn parse_release_init_split_workers_defaults_false() {
        let cli = Cli::try_parse_from(["autumn", "release", "init"]).unwrap();
        let Commands::Release(ReleaseCommands::Init { split_workers, .. }) = cli.command else {
            panic!("expected release init");
        };
        assert!(!split_workers, "split_workers must default to false");
    }

    #[test]
    fn parse_release_init_with_split_workers() {
        let cli = Cli::try_parse_from([
            "autumn",
            "release",
            "init",
            "--target",
            "docker-compose",
            "--split-workers",
        ])
        .unwrap();
        let Commands::Release(ReleaseCommands::Init {
            target,
            split_workers,
            ..
        }) = cli.command
        else {
            panic!("expected release init");
        };
        assert_eq!(target.as_deref(), Some("docker-compose"));
        assert!(split_workers, "--split-workers must set the flag");
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

    // ── autumn plugin (sandboxed) tests ────────────────────────────────────

    #[test]
    fn parse_plugin_package_requires_all_three_paths() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin",
            "package",
            "--manifest",
            "plugin.toml",
            "--module",
            "plugin.wasm",
            "--out",
            "hello.autumn-plugin",
        ])
        .expect("parses");
        match cli.command {
            Commands::Plugin {
                action:
                    PluginSubcommands::Package {
                        manifest,
                        module,
                        out,
                    },
            } => {
                assert_eq!(manifest, "plugin.toml");
                assert_eq!(module, "plugin.wasm");
                assert_eq!(out, "hello.autumn-plugin");
            }
            _ => panic!("expected plugin package"),
        }
        assert!(
            Cli::try_parse_from(["autumn", "plugin", "package", "--manifest", "plugin.toml"])
                .is_err()
        );
    }

    #[test]
    fn parse_plugin_inspect_defaults_to_text() {
        let cli = Cli::try_parse_from(["autumn", "plugin", "inspect", "hello.autumn-plugin"])
            .expect("parses");
        match cli.command {
            Commands::Plugin {
                action:
                    PluginSubcommands::Inspect {
                        artifact,
                        format,
                        against,
                    },
            } => {
                assert_eq!(artifact, "hello.autumn-plugin");
                assert_eq!(format, "text");
                // No `--against`: reviewing an artifact on its own, not as an
                // upgrade. The upgrade gate must not fire when nobody asked
                // for it (issue #1632).
                assert_eq!(against, None);
            }
            _ => panic!("expected plugin inspect"),
        }
    }

    #[test]
    fn parse_plugin_inspect_accepts_an_upgrade_baseline() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin",
            "inspect",
            "shop-0.2.autumn-plugin",
            "--against",
            "shop-0.1.autumn-plugin",
        ])
        .expect("parses");
        match cli.command {
            Commands::Plugin {
                action: PluginSubcommands::Inspect { against, .. },
            } => {
                assert_eq!(against.as_deref(), Some("shop-0.1.autumn-plugin"));
            }
            _ => panic!("expected plugin inspect"),
        }
    }

    #[test]
    fn parse_plugin_inspect_accepts_json() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin",
            "inspect",
            "hello.autumn-plugin",
            "--format",
            "json",
        ])
        .expect("parses");
        match cli.command {
            Commands::Plugin {
                action: PluginSubcommands::Inspect { format, .. },
            } => {
                assert_eq!(format, "json");
            }
            _ => panic!("expected plugin inspect"),
        }
    }

    // ── autumn plugin (list/add) tests ─────────────────────────────────────

    #[test]
    fn parse_plugin_list_defaults() {
        let cli = Cli::try_parse_from(["autumn", "plugin", "list"]).unwrap();
        match cli.command {
            Commands::Plugin {
                action: PluginSubcommands::List { json, offline },
            } => {
                assert!(!json);
                assert!(!offline);
            }
            _ => panic!("expected plugin list"),
        }
    }

    #[test]
    fn parse_plugin_list_json_and_offline() {
        let cli = Cli::try_parse_from(["autumn", "plugin", "list", "--json", "--offline"]).unwrap();
        match cli.command {
            Commands::Plugin {
                action: PluginSubcommands::List { json, offline },
            } => {
                assert!(json);
                assert!(offline);
            }
            _ => panic!("expected plugin list"),
        }
    }

    #[test]
    fn parse_plugin_add_requires_a_name() {
        assert!(Cli::try_parse_from(["autumn", "plugin", "add"]).is_err());
    }

    #[test]
    fn parse_plugin_add_with_dry_run() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin",
            "add",
            "autumn-admin-plugin",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Commands::Plugin {
                action: PluginSubcommands::Add { name, dry_run, .. },
            } => {
                assert_eq!(name, "autumn-admin-plugin");
                assert!(dry_run);
            }
            _ => panic!("expected plugin add"),
        }
    }

    /// `autumn plugin-check` predates `autumn plugin` and must keep working
    /// as its own top-level command.
    #[test]
    fn plugin_and_plugin_check_are_distinct_commands() {
        let check =
            Cli::try_parse_from(["autumn", "plugin-check", "--plugin-name", "myplugin"]).unwrap();
        assert!(matches!(check.command, Commands::PluginCheck { .. }));
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

    /// `--deny-experimental` turns the `experimental-surface` report into a
    /// gate (issue #1601). It has to be opt-in, so the default is asserted too.
    #[test]
    fn parse_plugin_check_deny_experimental_flag() {
        let cli = Cli::try_parse_from([
            "autumn",
            "plugin-check",
            "--plugin-name",
            "myplugin",
            "--deny-experimental",
        ])
        .unwrap();
        match cli.command {
            Commands::PluginCheck {
                deny_experimental, ..
            } => assert!(deny_experimental),
            _ => panic!("expected PluginCheck"),
        }

        let default =
            Cli::try_parse_from(["autumn", "plugin-check", "--plugin-name", "myplugin"]).unwrap();
        match default.command {
            Commands::PluginCheck {
                deny_experimental, ..
            } => assert!(
                !deny_experimental,
                "experimental use is reported, not gated"
            ),
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
                deny_experimental,
            } => {
                assert!(!deny_experimental, "the flag defaults off");
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
    fn parse_generate_policy_with_pascal_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "policy", "Post"]).unwrap();
        let Commands::Generate(GenerateCommands::Policy {
            name,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected generate policy");
        };
        assert_eq!(name, "Post");
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_policy_with_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "policy",
            "Post",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Policy { dry_run, force, .. }) = cli.command
        else {
            panic!("expected generate policy");
        };
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_scaffold_no_policy_flag() {
        // Default: policy on.
        let cli = Cli::try_parse_from(["autumn", "generate", "scaffold", "Post", "title:String"])
            .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold { no_policy, .. }) = cli.command else {
            panic!("expected generate scaffold");
        };
        assert!(!no_policy, "policy is on by default");

        // --no-policy opts out.
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--no-policy",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold { no_policy, .. }) = cli.command else {
            panic!("expected generate scaffold");
        };
        assert!(no_policy);
    }

    #[test]
    fn parse_generate_scaffold_belongs_to_flag() {
        // Default: flat scaffold, no parent.
        let cli = Cli::try_parse_from(["autumn", "generate", "scaffold", "Comment", "body:Text"])
            .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold { belongs_to, .. }) = cli.command else {
            panic!("expected generate scaffold");
        };
        assert_eq!(belongs_to, None, "flat scaffolds have no parent");

        // `--belongs-to Post` binds the child to its parent resource.
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--belongs-to",
            "Post",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Scaffold { belongs_to, .. }) = cli.command else {
            panic!("expected generate scaffold");
        };
        assert_eq!(belongs_to.as_deref(), Some("Post"));
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
    fn parse_generate_webhook_defaults() {
        let cli =
            Cli::try_parse_from(["autumn", "generate", "webhook", "stripe", "Payments"]).unwrap();
        let Commands::Generate(GenerateCommands::Webhook {
            provider,
            name,
            path,
            secret_env,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected generate webhook");
        };
        assert_eq!(provider, "stripe");
        assert_eq!(name, "Payments");
        assert!(path.is_none(), "--path defaults to /webhooks/<provider>");
        assert!(
            secret_env.is_none(),
            "--secret-env defaults to <PROVIDER>_WEBHOOK_SECRET"
        );
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_webhook_with_path_and_secret_env() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "webhook",
            "generic",
            "Partner",
            "--path",
            "/hooks/partner",
            "--secret-env",
            "PARTNER_WEBHOOK_SECRET",
            "--dry-run",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Webhook {
            provider,
            path,
            secret_env,
            dry_run,
            ..
        }) = cli.command
        else {
            panic!("expected generate webhook");
        };
        assert_eq!(provider, "generic");
        assert_eq!(path.as_deref(), Some("/hooks/partner"));
        assert_eq!(secret_env.as_deref(), Some("PARTNER_WEBHOOK_SECRET"));
        assert!(dry_run);
    }

    #[test]
    fn parse_generate_webhook_requires_both_provider_and_name() {
        assert!(
            Cli::try_parse_from(["autumn", "generate", "webhook", "stripe"]).is_err(),
            "the endpoint name is required"
        );
        assert!(
            Cli::try_parse_from(["autumn", "generate", "webhook"]).is_err(),
            "the provider preset is required"
        );
    }

    #[test]
    fn parse_destroy_webhook() {
        let cli =
            Cli::try_parse_from(["autumn", "destroy", "webhook", "stripe", "Payments"]).unwrap();
        let Commands::Destroy(GenerateCommands::Webhook { provider, name, .. }) = cli.command
        else {
            panic!("expected destroy webhook");
        };
        assert_eq!(provider, "stripe");
        assert_eq!(name, "Payments");
    }

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

    // ── autumn generate notifications tests ────────────────────────────────

    #[test]
    fn parse_generate_notifications_takes_no_name() {
        let cli = Cli::try_parse_from(["autumn", "generate", "notifications"]).unwrap();
        let Commands::Generate(GenerateCommands::Notifications { dry_run, force }) = cli.command
        else {
            panic!("expected generate notifications");
        };
        assert!(!dry_run);
        assert!(!force);
        // A fixed resource: a stray name argument must be rejected.
        assert!(Cli::try_parse_from(["autumn", "generate", "notifications", "Feed"]).is_err());
    }

    #[test]
    fn parse_generate_notifications_with_dry_run_and_force() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "notifications",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Notifications { dry_run, force }) = cli.command
        else {
            panic!("expected generate notifications");
        };
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parse_destroy_notifications() {
        let cli = Cli::try_parse_from(["autumn", "destroy", "notifications"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Destroy(GenerateCommands::Notifications { .. })
        ));
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
            overload,
            ceiling,
            block_ms,
            load_multiplier,
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
        assert!(!overload);
        assert_eq!(ceiling, 64);
        assert_eq!(block_ms, 200);
        assert_eq!(load_multiplier, 2);
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

    // ── autumn calibrate (#1733) ────────────────────────────────────

    #[test]
    fn parse_calibrate_defaults() {
        let cli = Cli::try_parse_from(["autumn", "calibrate"]).unwrap();
        let Commands::Calibrate {
            contract,
            check,
            seed,
            concurrency,
            rung_ms,
            warmup_ms,
            tolerance_rps,
            tolerance_p99,
            ..
        } = cli.command
        else {
            panic!("expected calibrate");
        };
        assert_eq!(contract, "capacity.lock");
        assert!(
            !check,
            "calibrate writes a contract unless --check is passed"
        );
        // Unspecified, so `--check` can replay whatever the committed
        // contract recorded rather than this invocation's defaults.
        assert_eq!(seed, None);
        assert!(concurrency.is_empty());
        assert_eq!(rung_ms, None);
        assert_eq!(warmup_ms, None);
        assert!((tolerance_rps - capacity::DEFAULT_RPS_TOLERANCE).abs() < f64::EPSILON);
        assert!((tolerance_p99 - capacity::DEFAULT_P99_TOLERANCE).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_calibrate_check_mode_with_custom_ladder_and_tolerances() {
        let cli = Cli::try_parse_from([
            "autumn",
            "calibrate",
            "--check",
            "-p",
            "blog",
            "--contract",
            "deploy/capacity.lock",
            "--concurrency",
            "1,8,64,256",
            "--tolerance-rps",
            "0.1",
            "--tolerance-p99",
            "0.3",
        ])
        .unwrap();
        let Commands::Calibrate {
            package,
            contract,
            check,
            concurrency,
            tolerance_rps,
            tolerance_p99,
            ..
        } = cli.command
        else {
            panic!("expected calibrate");
        };
        assert!(check);
        assert_eq!(package.as_deref(), Some("blog"));
        assert_eq!(contract, "deploy/capacity.lock");
        assert_eq!(concurrency, vec![1, 8, 64, 256]);
        assert!((tolerance_rps - 0.1).abs() < f64::EPSILON);
        assert!((tolerance_p99 - 0.3).abs() < f64::EPSILON);
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

    #[test]
    fn parse_dev_loop_bench_overload_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "dev-loop-bench",
            "--overload",
            "--ceiling",
            "32",
            "--block-ms",
            "150",
            "--load-multiplier",
            "3",
        ])
        .unwrap();
        let Commands::DevLoopBench {
            overload,
            ceiling,
            block_ms,
            load_multiplier,
            ..
        } = cli.command
        else {
            panic!("expected dev-loop-bench");
        };
        assert!(overload);
        assert_eq!(ceiling, 32);
        assert_eq!(block_ms, 150);
        assert_eq!(load_multiplier, 3);
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
    fn parse_generate_auth_magic_link_flag() {
        let cli =
            Cli::try_parse_from(["autumn", "generate", "auth", "User", "--magic-link"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth {
            name, magic_link, ..
        }) = cli.command
        else {
            panic!("wrong variant");
        };
        assert_eq!(name, "User");
        assert!(magic_link, "--magic-link must set the magic_link flag");
    }

    #[test]
    fn generate_auth_magic_link_defaults_off() {
        let cli = Cli::try_parse_from(["autumn", "generate", "auth", "User"]).unwrap();
        let Commands::Generate(GenerateCommands::Auth { magic_link, .. }) = cli.command else {
            panic!("wrong variant");
        };
        assert!(!magic_link, "magic_link must default to off");
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
                force: false,
                remote_url: None
            })
        ));
    }

    #[test]
    fn parse_generate_tauri_dry_run() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri", "--dry-run"]).unwrap();
        let Commands::Generate(GenerateCommands::Tauri {
            dry_run,
            force,
            remote_url,
        }) = cli.command
        else {
            panic!("expected Tauri variant");
        };
        assert!(dry_run);
        assert!(!force);
        assert!(remote_url.is_none());
    }

    #[test]
    fn parse_generate_tauri_force() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri", "--force"]).unwrap();
        let Commands::Generate(GenerateCommands::Tauri {
            dry_run,
            force,
            remote_url,
        }) = cli.command
        else {
            panic!("expected Tauri variant");
        };
        assert!(!dry_run);
        assert!(force);
        assert!(remote_url.is_none());
    }

    #[test]
    fn parse_generate_tauri_remote_url() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::Tauri {
            dry_run,
            force,
            remote_url,
        }) = cli.command
        else {
            panic!("expected Tauri variant");
        };
        assert!(!dry_run);
        assert!(!force);
        assert_eq!(remote_url.as_deref(), Some("https://app.example.com"));
    }

    #[test]
    fn parse_generate_tauri_remote_url_defaults_none() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri"]).unwrap();
        let Commands::Generate(GenerateCommands::Tauri { remote_url, .. }) = cli.command else {
            panic!("expected Tauri variant");
        };
        assert!(
            remote_url.is_none(),
            "--remote-url must default to None so the desktop sidecar path stays the default"
        );
    }

    // ── autumn generate tauri-mobile ───────────────────────────────────────

    #[test]
    fn parse_generate_tauri_mobile() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri-mobile"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Generate(GenerateCommands::TauriMobile {
                offline_sync: false,
                dry_run: false,
                force: false
            })
        ));
    }

    #[test]
    fn parse_generate_tauri_mobile_dry_run() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri-mobile", "--dry-run"]).unwrap();
        let Commands::Generate(GenerateCommands::TauriMobile {
            offline_sync,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected TauriMobile variant");
        };
        assert!(!offline_sync);
        assert!(dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_tauri_mobile_force() {
        let cli = Cli::try_parse_from(["autumn", "generate", "tauri-mobile", "--force"]).unwrap();
        let Commands::Generate(GenerateCommands::TauriMobile {
            offline_sync,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected TauriMobile variant");
        };
        assert!(!offline_sync);
        assert!(!dry_run);
        assert!(force);
    }

    #[test]
    fn parse_generate_tauri_mobile_offline_sync() {
        let cli =
            Cli::try_parse_from(["autumn", "generate", "tauri-mobile", "--offline-sync"]).unwrap();
        let Commands::Generate(GenerateCommands::TauriMobile {
            offline_sync,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected TauriMobile variant");
        };
        assert!(offline_sync);
        assert!(!dry_run);
        assert!(!force);
    }

    #[test]
    fn parse_generate_tauri_mobile_offline_sync_composes_with_flags() {
        let cli = Cli::try_parse_from([
            "autumn",
            "generate",
            "tauri-mobile",
            "--offline-sync",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        let Commands::Generate(GenerateCommands::TauriMobile {
            offline_sync,
            dry_run,
            force,
        }) = cli.command
        else {
            panic!("expected TauriMobile variant");
        };
        assert!(offline_sync);
        assert!(dry_run);
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
