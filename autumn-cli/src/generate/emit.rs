//! File-emission engine shared by every generator.
//!
//! Generators describe what they want to do as a list of [`Action`]s, and
//! [`Plan::execute`] handles all the side-effecting filesystem work — including
//! collision detection, `--force` / `--dry-run`, and the human-readable
//! "Created/Modified" output that mirrors `autumn new`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{Flags, GenerateError};

/// One filesystem operation the generator wants to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create a new file. Treated as a collision if the file already exists.
    Create { path: PathBuf, contents: String },
    /// Modify an existing file (or create it if absent). Never a collision.
    Modify { path: PathBuf, contents: String },
    /// Create a new file from raw bytes, with no UTF-8 or template handling.
    /// Used for verbatim/binary starter assets (e.g. vendored JS). Treated as a
    /// collision if the file already exists, like [`Action::Create`].
    CreateBytes { path: PathBuf, bytes: Vec<u8> },
    /// Create a file only if it does not already exist; skip silently if it
    /// does. Uses an exclusive-create open (analogous to `O_CREAT|O_EXCL`) so
    /// concurrent generator invocations cannot both win the race.
    CreateIfAbsent { path: PathBuf, contents: String },
}

impl Action {
    /// The path this action targets.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Create { path, .. }
            | Self::Modify { path, .. }
            | Self::CreateBytes { path, .. }
            | Self::CreateIfAbsent { path, .. } => path,
        }
    }
}

/// One in-place edit `generate` made to a shared file, recorded so `autumn
/// destroy` (issue #1048) can remove exactly that edit later — the
/// per-generator "revoke" hook the issue calls for.
///
/// Lives on [`Plan`] rather than on the [`Action::Modify`] it accompanies:
/// some generators (e.g. `scaffold`) collapse several `Modify` actions
/// targeting the same file (`Cargo.toml`) into one via
/// `plan.actions.retain(...)` + re-push, which would silently drop
/// per-action metadata. A plan-level list survives that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revert {
    /// Remove a `pub mod <name>;` declaration from a `mod.rs`-style file.
    ModDecl { path: PathBuf, name: String },
    /// Remove `entries` from the first `routes![...]` invocation in `path`
    /// (`src/main.rs`).
    RoutesEntries { path: PathBuf, entries: Vec<String> },
    /// Remove the `diesel::table! { <table> ... }` block from `path`
    /// (`src/schema.rs`) — but only if its content is still byte-identical
    /// to `expected_block` (the literal text this generator invocation
    /// would append for `table`), so a same-named table that pre-existed
    /// with different columns is never destroyed (issue #1048 PR review).
    SchemaTable {
        path: PathBuf,
        table: String,
        expected_block: String,
    },
    /// Remove the named crates' shorthand `[dependencies]` lines from `path`
    /// (`Cargo.toml`) — but only once `owner_dir` (e.g. `src/models`) has no
    /// other resource file left in it, so a sibling resource of the same
    /// generator that still needs this dependency (e.g. a second `model`
    /// also using `uuid`) survives destroying just one of them.
    CargoDeps {
        path: PathBuf,
        names: Vec<String>,
        owner_dir: PathBuf,
    },
    /// Remove `feature` from `autumn-web`'s `[dependencies]` features list
    /// in `path` (`Cargo.toml`), collapsing to a bare version string if that
    /// empties it — but only once `owner_dir` has no other resource file
    /// left in it (see [`Revert::CargoDeps`]); `None` when the generator
    /// pushing this revert has no single clean owning directory to check
    /// (falls back to the old always-remove behavior for that generator).
    CargoAutumnWebFeature {
        path: PathBuf,
        feature: String,
        owner_dir: Option<PathBuf>,
    },
    /// Remove `feature` from `autumn-web`'s `[dev-dependencies]` features
    /// list in `path` (`Cargo.toml`), deleting the whole line if that
    /// empties it (that entry was always freshly inserted, never
    /// pre-existing) — but only once `owner_dir` has no other resource file
    /// left in it (see [`Revert::CargoDeps`]).
    CargoAutumnWebDevFeature {
        path: PathBuf,
        feature: String,
        owner_dir: Option<PathBuf>,
    },
    /// Remove `entry` from the `jobs![...]` list in `path`
    /// (`src/jobs/mod.rs`), dropping the whole freshly-generated
    /// `registered_jobs()` function if it was the only entry.
    JobEntry { path: PathBuf, entry: String },
    /// Remove the `.policy::<...>(...)` and `.scope::<...>(...)` registration
    /// lines this generator injected for `{pascal}` into the `AppBuilder` chain
    /// in `path` (`src/main.rs`) (issue #1125). Unlike [`Revert::JobsRegistration`]
    /// (one shared call gated on `owner_dir`), each resource carries its own
    /// pair of registration lines keyed by its type, so removal is per-resource
    /// and needs no sibling-directory check.
    PolicyRegistration {
        path: PathBuf,
        pascal: String,
        snake: String,
    },
    /// Remove the `.jobs(jobs::registered_jobs())` call from the
    /// `AppBuilder` chain in `path` (`src/main.rs`) — but only once
    /// `owner_dir` (`src/jobs`) has no other job file left in it, so a
    /// sibling job that's still registered in the SAME shared
    /// `jobs![...]` list doesn't lose its only path to actually running
    /// (unlike [`Revert::MailPreview`]/[`Revert::InboundMailHandler`],
    /// whose entry-removal already collapses their own wrapper call only
    /// when their own list empties, this call lives in a *different* file
    /// than the `jobs![...]` list it depends on, so it needs the same
    /// directory-sibling check [`Revert::CargoDeps`] uses instead).
    JobsRegistration { path: PathBuf, owner_dir: PathBuf },
    /// Remove `mailer_type` from the `mail_previews![...]` list in `path`
    /// (`src/main.rs`), dropping the whole freshly-inserted
    /// `.mail_previews(...)` call if it was the only entry.
    MailPreview { path: PathBuf, mailer_type: String },
    /// Remove `.handler(<handler_module_path>())` from the
    /// `.inbound_mail_router(...)` chain in `path` (`src/main.rs`), dropping
    /// the whole freshly-inserted `.inbound_mail_router(...)` call if it was
    /// the only handler registered.
    InboundMailHandler {
        path: PathBuf,
        handler_module_path: String,
    },
    /// Remove `snake_name`'s `[[test]]` entry from `path` (`Cargo.toml`),
    /// plus the shared `system-tests` feature declaration too if no other
    /// `tests/system/*.rs` entry still needs it.
    SystemTestCargoPatch { path: PathBuf, snake_name: String },
    /// Remove the PWA `<link>`/`<meta>` tags and route-handler functions
    /// `autumn generate pwa` injected into `path` (`src/main.rs`).
    PwaMainRsInjection { path: PathBuf },
    /// Remove each `[auth.oauth2.<provider>]` stub block `autumn generate
    /// auth --oauth` appended to `path` (`autumn.toml`).
    AuthOAuthProviderStubs {
        path: PathBuf,
        providers: Vec<String>,
    },
    /// Remove the `[auth.webauthn]` stub block `autumn generate auth
    /// --passkeys` appended to `path` (`autumn.toml`).
    AuthWebauthnStub { path: PathBuf },
    /// Remove `/{plural}/{segment}` from `[security.submit_token]
    /// exempt_paths` in `path` (`autumn.toml`) — a submit-token exemption
    /// `autumn generate scaffold` appended — dropping the block if that empties
    /// a freshly-generated `exempt_paths` key.
    ///
    /// `segment` is `"validate"` for the `--live-validation` inline-validation
    /// routes (issue #1360) or `"preview"` for a `richtext` column's live
    /// Markdown preview routes (issue #1255). Both hx-include the whole form,
    /// so both must be exempt from the one-time submit-token guard.
    SubmitTokenValidateExempt {
        path: PathBuf,
        plural: String,
        segment: String,
    },
    /// Remove the remember-me middleware layer + startup-hook calls
    /// `autumn generate auth` injected into the `AppBuilder` chain in `path`
    /// (`src/main.rs`) (issue #1397).
    RememberMiddleware { path: PathBuf },
    /// Remove the `#[path]`-qualified `mod schema;` / `mod models;` links
    /// `generate model`/`scaffold` injected into `path` (`src/bin/seed.rs`)
    /// via [`super::schema_edit::link_models_into_seed_bin`] (issue #1718) —
    /// but only once `owner_dir` (`src/models`) has no other model file left
    /// in it, i.e. the LAST model is being destroyed and `src/models/mod.rs` /
    /// `src/schema.rs` (the link targets) are themselves removed. Destroying
    /// one of several models keeps the links, matching the surviving modules.
    SeedBinLinks { path: PathBuf, owner_dir: PathBuf },
    /// Remove the children-list + inline-create-form lines `autumn generate
    /// scaffold … --belongs-to` injected into the PARENT resource's generated
    /// `show` handler in `path` (`src/routes/<parents>.rs`) for `child_plural`
    /// (issue #1323).
    ///
    /// Every injected line carries a `// autumn:nested:<child_plural>` trailer,
    /// so removal is exact even when the same parent has several nested
    /// children; the extra CSRF/submit-token extractors spliced into the `show`
    /// signature are shared by all of them and come off only with the last one.
    NestedChildSection { path: PathBuf, child_plural: String },
}

impl Revert {
    /// The path this revert targets.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::ModDecl { path, .. }
            | Self::RoutesEntries { path, .. }
            | Self::SchemaTable { path, .. }
            | Self::CargoDeps { path, .. }
            | Self::CargoAutumnWebFeature { path, .. }
            | Self::CargoAutumnWebDevFeature { path, .. }
            | Self::JobEntry { path, .. }
            | Self::JobsRegistration { path, .. }
            | Self::PolicyRegistration { path, .. }
            | Self::MailPreview { path, .. }
            | Self::InboundMailHandler { path, .. }
            | Self::SystemTestCargoPatch { path, .. }
            | Self::PwaMainRsInjection { path }
            | Self::AuthOAuthProviderStubs { path, .. }
            | Self::AuthWebauthnStub { path }
            | Self::SubmitTokenValidateExempt { path, .. }
            | Self::RememberMiddleware { path }
            | Self::SeedBinLinks { path, .. }
            | Self::NestedChildSection { path, .. } => path,
        }
    }

    /// The directory whose continued occupancy (by any file other than
    /// this same destroy's own deletions) means this revert must NOT be
    /// applied yet — a sibling resource of the same generator may still
    /// need the dependency/feature. `None` means always apply (every
    /// variant except the three Cargo-editing ones, plus the Cargo ones
    /// where the pushing generator has no single owning directory).
    fn owner_dir(&self) -> Option<&Path> {
        match self {
            Self::CargoDeps { owner_dir, .. }
            | Self::JobsRegistration { owner_dir, .. }
            | Self::SeedBinLinks { owner_dir, .. } => Some(owner_dir),
            Self::CargoAutumnWebFeature { owner_dir, .. }
            | Self::CargoAutumnWebDevFeature { owner_dir, .. } => owner_dir.as_deref(),
            Self::ModDecl { .. }
            | Self::RoutesEntries { .. }
            | Self::SchemaTable { .. }
            | Self::JobEntry { .. }
            | Self::PolicyRegistration { .. }
            | Self::MailPreview { .. }
            | Self::InboundMailHandler { .. }
            | Self::SystemTestCargoPatch { .. }
            | Self::PwaMainRsInjection { .. }
            | Self::AuthOAuthProviderStubs { .. }
            | Self::AuthWebauthnStub { .. }
            | Self::SubmitTokenValidateExempt { .. }
            | Self::NestedChildSection { .. }
            | Self::RememberMiddleware { .. } => None,
        }
    }

    /// Apply this revert to `content`, returning the new content. Every
    /// underlying transform is idempotent — a revert whose target is
    /// already absent returns `content` unchanged.
    ///
    /// `overrides` supplies the already-computed post-destroy content of
    /// OTHER files this same `Plan::revert` is also modifying (e.g.
    /// `src/schema.rs`, once its own `SchemaTable` revert has run) — the
    /// project-wide crate/feature usage scan ([`crate_referenced_elsewhere_in_project`]/
    /// [`autumn_web_feature_still_needed_elsewhere`]) reads from there
    /// instead of stale pre-destroy disk content when a path is present,
    /// falling back to `excluding`-gated disk reads otherwise (issue #1048
    /// PR review).
    fn apply(
        &self,
        content: &str,
        project_root: &Path,
        excluding: &[PathBuf],
        overrides: &HashMap<PathBuf, String>,
    ) -> String {
        use super::inbound_mail::remove_inbound_mail_handler;
        use super::schema_edit::{
            remove_autumn_web_dev_dependency_feature, remove_autumn_web_feature, remove_job_entry,
            remove_jobs_registration_from_app, remove_mail_preview_from_app,
            remove_mod_declaration, remove_policy_registration_from_app,
            remove_remember_middleware_from_app, remove_routes_entries, remove_schema_table,
        };
        match self {
            Self::ModDecl { name, .. } => remove_mod_declaration(content, name),
            Self::RoutesEntries { entries, .. } => remove_routes_entries(content, entries),
            Self::SchemaTable {
                table,
                expected_block,
                ..
            } => remove_schema_table(content, table, expected_block),
            Self::CargoDeps { names, .. } => {
                // In addition to the `owner_dir` sibling-directory check
                // already applied by the caller, only remove a name if
                // nothing else in the project's source tree still
                // references it — a hand-added dependency unrelated to any
                // generator (issue #1048 PR review) survives the same way a
                // dependency shared with a sibling resource already does.
                // Best-effort: usage via a re-export or a derive macro that
                // never spells out the crate's own name (e.g. plain
                // `#[derive(Serialize)]` with no qualified `serde::` path)
                // isn't detected.
                let survives: Vec<&str> = names
                    .iter()
                    .map(String::as_str)
                    .filter(|name| {
                        !crate_referenced_elsewhere_in_project(
                            project_root,
                            name,
                            excluding,
                            overrides,
                        )
                    })
                    .collect();
                if survives.is_empty() {
                    content.to_owned()
                } else {
                    super::model::remove_cargo_dependencies(content, &survives)
                }
            }
            Self::CargoAutumnWebFeature { feature, .. } => {
                if autumn_web_feature_still_needed_elsewhere(
                    feature,
                    project_root,
                    excluding,
                    overrides,
                ) {
                    content.to_owned()
                } else {
                    remove_autumn_web_feature(content, feature)
                }
            }
            Self::CargoAutumnWebDevFeature { feature, .. } => {
                if autumn_web_feature_still_needed_elsewhere(
                    feature,
                    project_root,
                    excluding,
                    overrides,
                ) {
                    content.to_owned()
                } else {
                    remove_autumn_web_dev_dependency_feature(content, feature)
                }
            }
            Self::JobEntry { entry, .. } => remove_job_entry(content, entry),
            Self::JobsRegistration { .. } => remove_jobs_registration_from_app(content),
            Self::PolicyRegistration { pascal, snake, .. } => {
                remove_policy_registration_from_app(content, pascal, snake)
            }
            Self::MailPreview { mailer_type, .. } => {
                remove_mail_preview_from_app(content, mailer_type)
            }
            Self::InboundMailHandler {
                handler_module_path,
                ..
            } => remove_inbound_mail_handler(content, handler_module_path),
            Self::SystemTestCargoPatch { snake_name, .. } => {
                super::system_test::remove_cargo_toml_patch(
                    content,
                    snake_name,
                    project_root,
                    excluding,
                )
            }
            Self::PwaMainRsInjection { .. } => super::pwa::remove_pwa_injection(content),
            Self::RememberMiddleware { .. } => remove_remember_middleware_from_app(content),
            Self::AuthOAuthProviderStubs { providers, .. } => {
                super::auth::remove_oauth_provider_stubs(content, providers)
            }
            Self::AuthWebauthnStub { .. } => super::auth::remove_webauthn_stub(content),
            Self::SubmitTokenValidateExempt {
                plural, segment, ..
            } => super::scaffold::remove_submit_token_exempt_from_toml(content, plural, segment),
            Self::SeedBinLinks { .. } => super::schema_edit::unlink_models_from_seed_bin(content),
            Self::NestedChildSection { child_plural, .. } => {
                super::nested::remove_nested_child_section(content, child_plural)
            }
        }
    }
}

/// A complete generator plan — a sequence of actions plus the project root
/// they are anchored against.
#[derive(Debug, Default)]
pub struct Plan {
    /// Project root all action paths are interpreted relative to.
    pub project_root: PathBuf,
    /// The actions this plan will perform when executed.
    pub actions: Vec<Action>,
    /// Advisory messages surfaced to the user on [`Plan::execute`] (both
    /// `--dry-run` and a real run), without failing the generator — e.g. a
    /// `references` field whose target model doesn't exist yet.
    pub warnings: Vec<String>,
    /// In-place edits recorded alongside [`Action::Modify`]s, so
    /// [`Plan::revert`] (`autumn destroy`, issue #1048) can remove exactly
    /// what this plan would have inserted into a shared file.
    pub reverts: Vec<Revert>,
}

impl Plan {
    /// Create an empty plan rooted at `project_root`.
    #[must_use]
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            actions: Vec::new(),
            warnings: Vec::new(),
            reverts: Vec::new(),
        }
    }

    /// Record an advisory message, printed by [`Plan::execute`] but never
    /// fatal to the plan.
    pub fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    /// Record a [`Revert`] describing how to undo an in-place edit this plan
    /// makes elsewhere via [`Plan::modify`]. Used by `autumn destroy`
    /// (issue #1048) — irrelevant to a normal `generate` run.
    pub fn push_revert(&mut self, revert: Revert) {
        self.reverts.push(revert);
    }

    /// Push a [`Action::Create`] action.
    pub fn create(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.actions.push(Action::Create {
            path: path.into(),
            contents: contents.into(),
        });
    }

    /// Push a [`Action::Modify`] action.
    pub fn modify(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.actions.push(Action::Modify {
            path: path.into(),
            contents: contents.into(),
        });
    }

    /// Push a [`Action::CreateBytes`] action (verbatim/binary file).
    pub fn create_bytes(&mut self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
        self.actions.push(Action::CreateBytes {
            path: path.into(),
            bytes: bytes.into(),
        });
    }

    /// Push a [`Action::CreateIfAbsent`] action.
    ///
    /// The file is created atomically using an exclusive open; if the file
    /// already exists (including from a concurrent generator run) the action is
    /// silently skipped — the existing content is left untouched.
    pub fn create_if_absent(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.actions.push(Action::CreateIfAbsent {
            path: path.into(),
            contents: contents.into(),
        });
    }

    /// All `Create` actions whose target file already exists on disk.
    fn collisions(&self) -> Vec<PathBuf> {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::Create { path, .. } | Action::CreateBytes { path, .. } if path.exists() => {
                    Some(path.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Run the plan, honouring `--dry-run` and `--force`.
    ///
    /// On `--dry-run` we print the action list and exit early without touching
    /// the filesystem. On a real run we emit a `Created`/`Modified` line per
    /// action, in the same style as `autumn new`.
    ///
    /// # Errors
    /// Returns [`GenerateError::Collisions`] when any `Create` would overwrite
    /// an existing file and `--force` was not passed; or [`GenerateError::Io`]
    /// for filesystem failures during emission.
    pub fn execute(&self, flags: Flags) -> Result<(), GenerateError> {
        if flags.dry_run {
            self.print_warnings();
            self.print_dry_run();
            return Ok(());
        }

        if !flags.force {
            let collisions = self.collisions();
            if !collisions.is_empty() {
                return Err(GenerateError::Collisions(collisions));
            }
        }

        self.print_warnings();

        // Make sure parent directories of every file action exist.
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for action in &self.actions {
            let path = action.path();
            if let Some(parent) = path.parent() {
                dirs.insert(parent.to_path_buf());
            }
        }
        for dir in &dirs {
            fs::create_dir_all(dir)?;
        }

        for action in &self.actions {
            let path = action.path();
            match action {
                Action::Create { contents, .. } | Action::Modify { contents, .. } => {
                    let label = if matches!(action, Action::Modify { .. }) && path.exists() {
                        "Modified"
                    } else {
                        "Created"
                    };
                    fs::write(path, contents)?;
                    println!("  {label} {}", relative_display(path, &self.project_root));
                }
                Action::CreateBytes { bytes, .. } => {
                    fs::write(path, bytes)?;
                    println!("  Created {}", relative_display(path, &self.project_root));
                }
                Action::CreateIfAbsent { contents, .. } => {
                    match fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)
                    {
                        Ok(mut f) => {
                            if let Err(e) = f.write_all(contents.as_bytes()) {
                                // Remove the empty/partial file so the next run
                                // does not skip creation due to AlreadyExists.
                                let _ = fs::remove_file(path);
                                return Err(GenerateError::Io(e));
                            }
                            println!("  Created {}", relative_display(path, &self.project_root));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            // Another process already created the file; leave it untouched.
                        }
                        Err(e) => return Err(GenerateError::Io(e)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Print every advisory warning to stderr. Called only on a path that
    /// will actually run (dry-run or a real, non-colliding execution) — never
    /// before a `--force`/collision check that might abort the plan, so a
    /// failed, no-op run never emits misleading warnings about a plan that
    /// was never applied.
    fn print_warnings(&self) {
        for warning in &self.warnings {
            eprintln!("Warning: {warning}");
        }
    }

    fn print_dry_run(&self) {
        println!("Dry run — no files written.");
        for action in &self.actions {
            let label = match action {
                Action::Create { path, .. } | Action::CreateBytes { path, .. } if path.exists() => {
                    "Would overwrite"
                }
                Action::Modify { path, .. } if path.exists() => "Would modify",
                Action::CreateIfAbsent { path, .. } if path.exists() => "Would skip (exists)",
                Action::Create { .. }
                | Action::Modify { .. }
                | Action::CreateBytes { .. }
                | Action::CreateIfAbsent { .. } => "Would create",
            };
            println!(
                "  {label} {}",
                relative_display(action.path(), &self.project_root)
            );
        }
    }

    /// Undo this plan — the deterministic inverse of [`Plan::execute`]
    /// driving `autumn destroy` (issue #1048).
    ///
    /// For each `Create`/`CreateBytes` action, deletes the target file
    /// (refusing when its on-disk content has diverged from what `generate`
    /// would have produced, unless `--force`). A `CreateIfAbsent` target
    /// (a file shared across resources that `generate` only ever writes
    /// once) is deleted only when its content still matches exactly —
    /// divergence there is left alone with a warning instead, never
    /// force-deleted, since it may predate this resource entirely rather
    /// than being this destroy's own edit gone stale. Either way, prunes any
    /// now-empty generated directories afterward. For each [`Revert`] recorded via
    /// [`Plan::push_revert`], removes exactly the lines/entries `generate`
    /// inserted from the current file — never touching content this plan
    /// didn't itself add. A file emptied down to nothing by its reverts is
    /// deleted outright (e.g. a `schema.rs`/`mod.rs` that only ever held
    /// this resource's single entry).
    ///
    /// Migration directories are matched by resource-name suffix, since
    /// destroy recomputes the plan with a fresh timestamp that won't match
    /// the original directory, and are removed only when not yet applied to
    /// a configured database — destroy never touches the database itself.
    ///
    /// Honours `--dry-run` (prints a `Removed:`/`Reverted:` plan, touches
    /// nothing) exactly like [`Plan::execute`].
    ///
    /// # Errors
    /// Returns [`GenerateError::Diverged`] when any targeted file's content
    /// no longer matches what `generate` would have produced and `--force`
    /// was not given; or [`GenerateError::Io`] for filesystem failures.
    pub fn revert(&self, flags: Flags) -> Result<(), GenerateError> {
        let plan = self.compute_revert_plan(flags.force);

        if flags.dry_run {
            println!("Dry run — no files removed.");
            for path in &plan.files_to_remove {
                println!(
                    "  Would remove {}",
                    relative_display(path, &self.project_root)
                );
            }
            for dir in &plan.migrations_to_remove {
                println!(
                    "  Would remove {}",
                    relative_display(dir, &self.project_root)
                );
            }
            for (path, new_content) in &plan.modifies {
                let label = if new_content.is_none() {
                    "Would remove"
                } else {
                    "Would revert"
                };
                println!("  {label} {}", relative_display(path, &self.project_root));
            }
            if !plan.diverged.is_empty() {
                println!(
                    "  A real run would refuse (without --force) — content has diverged from what generate produced:"
                );
                for path in &plan.diverged {
                    println!(
                        "    Diverged {}",
                        relative_display(path, &self.project_root)
                    );
                }
            }
            for warning in &plan.warnings {
                eprintln!("Warning: {warning}");
            }
            return Ok(());
        }

        if !plan.diverged.is_empty() && !flags.force {
            return Err(GenerateError::Diverged(plan.diverged));
        }

        for warning in &plan.warnings {
            eprintln!("Warning: {warning}");
        }

        let mut touched_dirs: Vec<PathBuf> = Vec::new();

        for path in &plan.files_to_remove {
            fs::remove_file(path)?;
            println!("  Removed {}", relative_display(path, &self.project_root));
            if let Some(parent) = path.parent() {
                touched_dirs.push(parent.to_path_buf());
            }
        }

        for dir in &plan.migrations_to_remove {
            fs::remove_dir_all(dir)?;
            println!("  Removed {}", relative_display(dir, &self.project_root));
            touched_dirs.push(self.project_root.join("migrations"));
        }

        for (path, new_content) in &plan.modifies {
            if let Some(content) = new_content {
                fs::write(path, content)?;
                println!("  Reverted {}", relative_display(path, &self.project_root));
            } else {
                fs::remove_file(path)?;
                println!("  Removed {}", relative_display(path, &self.project_root));
                if let Some(parent) = path.parent() {
                    touched_dirs.push(parent.to_path_buf());
                }
            }
        }

        for dir in touched_dirs {
            prune_empty_ancestors(dir, &self.project_root);
        }

        // Nested sub-module declarations (e.g. `src/mailers/mod.rs`'s
        // `pub mod previews;`) must be synced BEFORE `src/main.rs`'s, so a
        // now-empty-and-deleted `src/mailers/mod.rs` is already gone by the
        // time the `mod mailers;` orphan check runs.
        sync_mod_declarations_in(
            &self.project_root.join("src").join("mailers"),
            &["previews"],
            &self.project_root,
        );
        sync_main_rs_mod_declarations(&self.project_root);

        Ok(())
    }

    /// Compute what [`Plan::revert`] would do, without touching disk except
    /// to read files for divergence comparison. Shared by the `--dry-run`
    /// and real-run paths of `revert` so they can never disagree.
    #[allow(
        clippy::too_many_lines,
        reason = "a linear sequence of independent passes (normal files, migrations, \
                  create-if-absent shared files, modify reverts) that share the \
                  files_to_remove/diverged accumulators — splitting it up would just \
                  move the same state-threading around, not simplify it"
    )]
    fn compute_revert_plan(&self, force: bool) -> RevertPlan {
        let migrations_root = self.project_root.join("migrations");

        // `Create`/`CreateBytes`/`CreateIfAbsent` actions living directly
        // under `migrations/<dir>/` need suffix-based matching (see
        // `resolve_migration_removal`); everything else is a normal,
        // path-stable file to check-and-delete.
        let mut migration_groups: Vec<(PathBuf, Vec<&Action>)> = Vec::new();
        let mut normal_actions: Vec<&Action> = Vec::new();
        // `CreateIfAbsent` actions for a *shared* file (e.g. a mailer's
        // `templates/mailers/_layout.html`) are deferred to their own pass
        // below — unlike an owned `Create`, a later resource's `generate`
        // call silently skips writing to it once an earlier resource's call
        // already created it, so the action being present in THIS resource's
        // plan doesn't mean this resource is the only one still using it.
        let mut create_if_absent_actions: Vec<&Action> = Vec::new();
        for action in &self.actions {
            if matches!(action, Action::Modify { .. }) {
                continue;
            }
            let path = action.path();
            if let Some(dir) = path.parent()
                && dir.parent() == Some(migrations_root.as_path())
            {
                if let Some(entry) = migration_groups.iter_mut().find(|(d, _)| d == dir) {
                    entry.1.push(action);
                } else {
                    migration_groups.push((dir.to_path_buf(), vec![action]));
                }
                continue;
            }
            if matches!(action, Action::CreateIfAbsent { .. }) {
                create_if_absent_actions.push(action);
            } else {
                normal_actions.push(action);
            }
        }

        let mut files_to_remove = Vec::new();
        let mut diverged = Vec::new();
        let mut warnings = Vec::new();
        for action in normal_actions {
            let path = action.path();
            if !path.exists() {
                continue; // already gone — idempotent skip.
            }
            let matches = match action {
                Action::Create { contents, .. } => {
                    fs::read_to_string(path).is_ok_and(|d| &d == contents)
                }
                Action::CreateBytes { bytes, .. } => fs::read(path).is_ok_and(|d| &d == bytes),
                Action::CreateIfAbsent { .. } | Action::Modify { .. } => {
                    unreachable!("filtered out above")
                }
            };
            if matches || force {
                files_to_remove.push(path.to_path_buf());
            } else {
                diverged.push(path.to_path_buf());
            }
        }

        // Now resolve the deferred `CreateIfAbsent` actions: only remove a
        // shared file once its directory holds no other file this destroy
        // isn't itself already removing — including sibling `CreateIfAbsent`
        // files from the SAME batch (excluded up front, so whichever one is
        // checked first doesn't see the other, still-unprocessed one as
        // false evidence that the directory is still occupied).
        let create_if_absent_paths: Vec<PathBuf> = create_if_absent_actions
            .iter()
            .map(|a| a.path().to_path_buf())
            .collect();
        for action in create_if_absent_actions {
            let path = action.path();
            if !path.exists() {
                continue;
            }
            if let Some(dir) = path.parent() {
                let mut excluding = files_to_remove.clone();
                excluding.extend(create_if_absent_paths.iter().cloned());
                if resource_dir_has_other_files(dir, &excluding) {
                    continue; // a sibling resource's file still lives here — keep it.
                }
            }
            let Action::CreateIfAbsent { contents, .. } = action else {
                unreachable!("filtered to CreateIfAbsent above");
            };
            let matches = fs::read_to_string(path).is_ok_and(|d| &d == contents);
            if matches {
                files_to_remove.push(path.to_path_buf());
            } else {
                // Unlike an owned `Create`, a `CreateIfAbsent` target may
                // have pre-existed before ANY generator ever ran (`generate`
                // silently skips writing to it either way) — e.g. a
                // hand-rolled `templates/mailers/_layout.html`. Divergence
                // here can't be attributed to "this destroy's own edit gone
                // stale" the way it can for an owned `Create`, so it's never
                // treated as a blocking error and never force-deleted
                // (issue #1048 PR review): guessing wrong would destroy
                // real, pre-existing project content this destroy never
                // touched. Just leave it and say why.
                warnings.push(format!(
                    "{} doesn't match what this generator produces; leaving it in \
                     place since it may predate this resource — remove it by hand \
                     if it's actually orphaned.",
                    relative_display(path, &self.project_root),
                ));
            }
        }

        let mut migrations_to_remove = Vec::new();
        for (dir, actions) in &migration_groups {
            match resolve_migration_removal(
                dir,
                actions,
                &migrations_root,
                force,
                &self.project_root,
                &files_to_remove,
            ) {
                MigrationOutcome::Remove(real_dir) => migrations_to_remove.push(real_dir),
                MigrationOutcome::Diverged(path) => diverged.push(path),
                MigrationOutcome::Applied(real_dir) => warnings.push(format!(
                    "migration {} appears to be applied to the database; skipping it \
                     — write a down-migration instead. destroy never touches the database.",
                    relative_display(&real_dir, &self.project_root),
                )),
                MigrationOutcome::Ambiguous(suffix) => warnings.push(format!(
                    "multiple migrations directories match the expected suffix '{suffix}'; \
                     skipping — remove the correct one manually."
                )),
                MigrationOutcome::StillNeededElsewhere(real_dir) => warnings.push(format!(
                    "migration {} is still needed by another mailer's --list-unsubscribe; \
                     skipping — pass --force to remove it anyway.",
                    relative_display(&real_dir, &self.project_root),
                )),
                MigrationOutcome::NotFound => {}
            }
        }

        let grouped_reverts = group_reverts_by_path(&self.reverts);
        // Every path this destroy is ITSELF also rewriting (e.g.
        // `src/schema.rs`, emptied by a `SchemaTable` revert) must be
        // excluded from `crate_referenced_elsewhere_in_project`'s scan
        // alongside `files_to_remove`: its pre-destroy content (still on
        // disk at scan time — the real rewrite/deletion happens later, in
        // `Plan::revert`'s apply phase) isn't evidence of anything a
        // *different* resource still needs, since this same operation is
        // about to change it too. `scan_overrides` (below) supplies each
        // such file's real final content instead, so this exclusion only
        // ever falls back to "treat as absent" for a file that becomes
        // empty (deleted) — never for one that survives with other content.
        let mut cargo_deps_excluding = files_to_remove.clone();
        cargo_deps_excluding.extend(grouped_reverts.iter().map(|(path, _)| path.clone()));

        // Precompute the final, post-destroy content of every modified file
        // OTHER than `Cargo.toml` via each file's own, self-contained
        // reverts (none of them — `SchemaTable`, `ModDecl`, etc. — consult
        // another file's content). `Cargo.toml`'s `CargoDeps`/
        // `CargoAutumnWebFeature`/`CargoAutumnWebDevFeature` reverts are the
        // only ones that DO (the project-wide crate/feature usage scan),
        // and always target `Cargo.toml` itself, so computing every other
        // file's result first and excluding `Cargo.toml` here avoids any
        // ordering cycle. This is what lets that scan see what
        // `src/schema.rs` (or `src/main.rs`, a `mod.rs`, ...) will actually
        // look like once this destroy is done — e.g. a table this destroy
        // ISN'T touching, still present after its own removal — rather than
        // either stale pre-destroy content (would wrongly count a table
        // being removed as "still needed") or hiding the file from the scan
        // entirely (would wrongly hide unrelated content in the same file
        // that legitimately still needs the dependency) — issue #1048 PR
        // review.
        let cargo_toml_path = self.project_root.join("Cargo.toml");
        let mut scan_overrides: HashMap<PathBuf, String> = HashMap::new();
        for (path, reverts) in &grouped_reverts {
            if *path == cargo_toml_path || !path.exists() {
                continue;
            }
            let Ok(original) = fs::read_to_string(path) else {
                continue;
            };
            let mut content = original;
            for revert in reverts {
                if let Some(dir) = revert.owner_dir()
                    && resource_dir_has_other_files(dir, &files_to_remove)
                {
                    continue;
                }
                content = revert.apply(
                    &content,
                    &self.project_root,
                    &cargo_deps_excluding,
                    &scan_overrides,
                );
            }
            if !content.trim().is_empty() {
                scan_overrides.insert(path.clone(), content);
            }
        }

        let mut modifies = Vec::new();
        for (path, reverts) in grouped_reverts {
            if !path.exists() {
                continue;
            }
            let Ok(original) = fs::read_to_string(&path) else {
                continue;
            };
            let mut content = original.clone();
            for revert in &reverts {
                if let Some(dir) = revert.owner_dir()
                    && resource_dir_has_other_files(dir, &files_to_remove)
                {
                    // A sibling resource of the same generator still lives
                    // in `owner_dir` and may still need this dependency or
                    // feature — leave the Cargo.toml edit in place.
                    continue;
                }
                content = revert.apply(
                    &content,
                    &self.project_root,
                    &cargo_deps_excluding,
                    &scan_overrides,
                );
            }
            if content == original {
                continue;
            }
            if content.trim().is_empty() {
                modifies.push((path, None));
            } else {
                modifies.push((path, Some(content)));
            }
        }

        RevertPlan {
            files_to_remove,
            migrations_to_remove,
            modifies,
            diverged,
            warnings,
        }
    }
}

/// The concrete result of [`Plan::compute_revert_plan`] — everything
/// [`Plan::revert`] needs to either print (`--dry-run`) or perform (a real
/// run), computed exactly once so the two paths can't disagree.
struct RevertPlan {
    files_to_remove: Vec<PathBuf>,
    migrations_to_remove: Vec<PathBuf>,
    /// `(path, new_content)`; `None` means the file should be deleted
    /// (its reverts emptied it down to nothing).
    modifies: Vec<(PathBuf, Option<String>)>,
    diverged: Vec<PathBuf>,
    warnings: Vec<String>,
}

/// Group `reverts` by target path, preserving push order both across groups
/// and within each group.
fn group_reverts_by_path(reverts: &[Revert]) -> Vec<(PathBuf, Vec<&Revert>)> {
    let mut groups: Vec<(PathBuf, Vec<&Revert>)> = Vec::new();
    for revert in reverts {
        let path = revert.path().to_path_buf();
        if let Some(entry) = groups.iter_mut().find(|(p, _)| p == &path) {
            entry.1.push(revert);
        } else {
            groups.push((path, vec![revert]));
        }
    }
    groups
}

/// Remove `dir` and walk up removing each now-empty ancestor, stopping at
/// the first non-empty directory (or `project_root`). `fs::remove_dir`
/// fails harmlessly on a non-empty or already-missing directory, which is
/// exactly the stopping condition we want.
fn prune_empty_ancestors(mut dir: PathBuf, project_root: &Path) {
    while dir != *project_root && dir.starts_with(project_root) {
        if fs::remove_dir(&dir).is_err() {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
}

/// Whether `dir` contains any regular file other than the ones this same
/// destroy operation is itself about to delete (`excluding` — the
/// already-computed `files_to_remove`). Used to gate
/// [`Revert::CargoDeps`]/[`Revert::CargoAutumnWebFeature`]/[`Revert::CargoAutumnWebDevFeature`]:
/// a dependency or feature shared by multiple resources of the same
/// generator (e.g. two `model`s both using `uuid`, two `mailer`s both using
/// the `mail` feature) must survive destroying just one of them. A missing
/// or unreadable `dir` counts as "no other files" (nothing left to protect).
fn resource_dir_has_other_files(dir: &Path, excluding: &[PathBuf]) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        // `mod.rs` is the shared aggregator every one of these directories
        // has (declaring `pub mod <resource>;` per file) — it is not itself
        // a resource, so its mere presence must never count as "a sibling
        // resource still needs this dependency/feature".
        path.is_file()
            && path.file_name() != Some(std::ffi::OsStr::new("mod.rs"))
            && !excluding.contains(&path)
    })
}

/// Whether `crate_name`'s Rust identifier (`-` replaced with `_`) is
/// referenced anywhere under `project_root`'s `src/`, `tests/`, or
/// `benches/` trees — other than in `excluding` (this same destroy's own
/// file deletions). A whole-project extension of [`resource_dir_has_other_files`]
/// for [`Revert::CargoDeps`]: a project may hand-add a dependency for its
/// own reasons with no generated resource ever touching it, so checking
/// only the generator's `owner_dir` isn't enough to avoid stripping it.
///
/// Best-effort, not exhaustive: looks for `{ident}::` (a qualified path) or
/// `use {ident}` (an import) as plain substrings; a crate used only via a
/// re-export or a derive macro that never spells out its own name (e.g.
/// bare `#[derive(Serialize)]` with no qualified `serde::` path anywhere)
/// is not detected.
fn crate_referenced_elsewhere_in_project(
    project_root: &Path,
    crate_name: &str,
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> bool {
    let ident = crate_name.replace('-', "_");
    let markers = [format!("{ident}::"), format!("use {ident}")];
    ["src", "tests", "benches"]
        .iter()
        .any(|dir| rs_tree_contains_marker(&project_root.join(dir), &markers, excluding, overrides))
}

/// Text markers that reliably indicate `feature`'s autumn-web API surface
/// is in use somewhere in the project — used as a whole-project supplement
/// to the same-generator `owner_dir` sibling check for
/// [`Revert::CargoAutumnWebFeature`]/[`Revert::CargoAutumnWebDevFeature`],
/// since a feature can be needed by a COMPLETELY DIFFERENT generator kind
/// than the one being destroyed (e.g. `generate auth` and `generate mailer`
/// both need `"mail"` — destroying the only mailer must not strip it while
/// auth's routes still call `Mail::builder()`). Any one marker matching is
/// sufficient evidence. Empty for a feature with no reliable marker — falls
/// back to the `owner_dir` check alone.
fn autumn_web_feature_markers(feature: &str) -> &'static [&'static str] {
    match feature {
        // `maud`/`htmx` are in autumn-web's own `default = [...]` feature
        // set (see `autumn/Cargo.toml`) — a project's default-features
        // dependency already carries them regardless of any generator, so
        // removing the *explicit* `features = [...]` entry never actually
        // disables the capability, and a project-wide marker check would
        // wrongly treat autumn's own default-feature boilerplate (e.g.
        // `src/main.rs`'s stock `layout()`, which always calls
        // `maud::html!`) as "still needed" in every project. No check
        // needed — fall through to the `owner_dir` check alone.
        "mail" => &["Mail::builder("],
        "oauth2" => &["OAuth2"],
        "webauthn" => &["Webauthn"],
        // Real WebSocket-transport channels (`#[ws]`) AND SSE-transport
        // channels/`--live` scaffolds (`autumn_web::sse::stream(`, no
        // `#[ws]` marker at all) are both gated behind the same "ws"
        // feature (issue #1048 PR review: destroying the only channel/live
        // scaffold of one transport must not strip a feature the other
        // transport, generated separately, still needs).
        "ws" => &["#[ws]", "autumn_web::sse::stream("],
        // `TestDb::` (not bare `TestDb`) so a doc comment merely mentioning
        // the type (e.g. the template-shipped `tests/integration_test.rs`'s
        // "Add DB-backed tests with `TestDb`...") doesn't count as usage.
        "test-support" => &["TestDb::"],
        // Attachment scaffolds (`generate scaffold ... --attachment`) enable
        // both `multipart` and `storage`, but a hand-written route can also
        // use these APIs directly. Destroying the last attachment model must
        // not strip a feature such a route still needs (PR #1867 review).
        //
        // Bare `Multipart` (not `extract::Multipart`) so the marker also
        // catches a hand-written route that pulls the extractor in through
        // `use autumn_web::prelude::*;` and names it UNQUALIFIED — the prelude
        // re-exports `Multipart` (`autumn/src/prelude.rs`), so such a route
        // contains neither `extract::Multipart` nor any storage path, and the
        // narrower marker would let `destroy` wrongly strip the feature (PR
        // #1867 review follow-up). This still matches the fully-qualified
        // `autumn_web::extract::Multipart` the scaffold itself emits and any
        // `use ...::Multipart;` import; the only extra hits are identifiers
        // like `MultipartField`/`MultipartError`, which are themselves part of
        // the multipart API surface, so over-retaining on them is harmless.
        "multipart" => &["Multipart"],
        // `autumn_web::storage::` covers the model's blob column type
        // (`autumn_web::storage::Blob`) as well as route usage of the store
        // (`autumn_web::storage::BlobStoreState`, `save_to_blob_store`
        // call sites that reference the `autumn_web::storage::` path, etc.).
        // Unlike `multipart`, the prelude does NOT re-export any storage type
        // (`Blob`, `BlobStore`, `BlobStoreState`, …), so a hand-written
        // storage user must reach them through a `autumn_web::storage::…`
        // path — either fully qualified or via a `use autumn_web::storage::{…}`
        // import line — both of which this marker already catches. There is no
        // prelude-unqualified spelling to miss, so no extra marker is needed.
        "storage" => &["autumn_web::storage::"],
        // A `richtext` scaffold (issue #1255) enables `markdown` for the
        // sanitizing `render_user_content` its show/preview paths call, but a
        // hand-written route can render trusted Markdown through the same
        // feature's `render`/`MarkdownRegistry`. `autumn_web::markdown::`
        // catches every spelling: the prelude re-exports none of these types,
        // so any user must reach them through that path — fully qualified or
        // via a `use autumn_web::markdown::{…};` import line.
        "markdown" => &["autumn_web::markdown::"],
        // A scaffolded CSV export (issue #1315) enables `csv` for the
        // `CsvSchema` impl and `export_csv` call its `export.csv` route emits,
        // but a hand-written route, job or task can use the same module —
        // `import_csv` has no generator at all, so an author wiring an upload
        // form is doing it by hand by definition. Destroying the last
        // scaffolded resource must not strip the feature out from under them.
        // The prelude re-exports nothing from `data::csv`, so every spelling
        // goes through this path. Deliberately NO trailing `::`: an author can
        // import the MODULE (`use autumn_web::data::csv;` or `… as data_csv;`)
        // and then call `csv::export_csv(…)`, and that import line ends at the
        // module name. A `autumn_web::data::csv::` marker misses it, and
        // destroying the last export-enabled scaffold would then strip a
        // feature the surviving code needs — a build break. Dropping the `::`
        // costs only over-retention (a doc comment naming the module now
        // counts as usage, leaving the feature enabled), which is the same
        // harmless direction `multipart` already accepts above; under-retention
        // is the one that does not compile.
        "csv" => &["autumn_web::data::csv"],
        _ => &[],
    }
}

/// Whether `feature` is still referenced anywhere under `project_root`'s
/// `src/`, `tests/`, or `benches/` trees, other than in `excluding` — see
/// [`autumn_web_feature_markers`]. Always `false` (never blocks removal) for
/// a feature with no known marker.
fn autumn_web_feature_still_needed_elsewhere(
    feature: &str,
    project_root: &Path,
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> bool {
    let markers = autumn_web_feature_markers(feature);
    if markers.is_empty() {
        return false;
    }
    let markers: Vec<String> = markers.iter().map(|m| (*m).to_owned()).collect();
    ["src", "tests", "benches"]
        .iter()
        .any(|dir| rs_tree_contains_marker(&project_root.join(dir), &markers, excluding, overrides))
}

/// Whether a local Cargo `feature` (not an `autumn-web` feature — see
/// [`autumn_web_feature_still_needed_elsewhere`] for that) is still gated on
/// anywhere under `project_root`'s `src/`, `tests/`, or `benches/` trees,
/// other than in `excluding`. Used by [`Revert::SystemTestCargoPatch`]:
/// destroying the last generated system test (or `generate pwa`'s smoke
/// test) must not strip a shared `[features] system-tests = [...]` entry
/// that pre-existed for unrelated hand-written `#[cfg(feature =
/// "system-tests")]` code (issue #1048 PR review).
pub(super) fn cargo_feature_still_gated_elsewhere(
    feature: &str,
    project_root: &Path,
    excluding: &[PathBuf],
) -> bool {
    let markers = [format!("feature = \"{feature}\"")];
    let no_overrides = HashMap::new();
    ["src", "tests", "benches"].iter().any(|dir| {
        rs_tree_contains_marker(&project_root.join(dir), &markers, excluding, &no_overrides)
    })
}

/// Recursively scan `dir` for any `.rs` file containing one of `markers` as
/// a plain substring.
///
/// `overrides` (path → already-computed post-destroy content) takes
/// priority over both disk and `excluding` — it's how a caller supplies the
/// real final content of a file this same `Plan::revert` is also modifying
/// in place (e.g. `src/schema.rs`), so scanning it doesn't see stale
/// pre-destroy content (issue #1048 PR review). A path in `excluding` but
/// absent from `overrides` is skipped entirely — it's either being deleted
/// outright, or emptied down to nothing by its own reverts, so it won't
/// exist to provide evidence either way once this destroy finishes.
fn rs_tree_contains_marker(
    dir: &Path,
    markers: &[String],
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if rs_tree_contains_marker(&path, markers, excluding, overrides) {
                return true;
            }
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let content = overrides.get(&path).cloned().or_else(|| {
            if excluding.contains(&path) {
                None
            } else {
                fs::read_to_string(&path).ok()
            }
        });
        if content.is_some_and(|c| markers.iter().any(|m| c.contains(m.as_str()))) {
            return true;
        }
    }
    false
}

/// Outcome of matching one plan-time migration directory (built with a
/// fresh, destroy-time timestamp) against what's actually on disk.
enum MigrationOutcome {
    /// Safe to remove — content matched (or `--force`) and it is not applied.
    Remove(PathBuf),
    /// The real directory's content no longer matches what `generate` would
    /// have produced.
    Diverged(PathBuf),
    /// The migration appears to be applied to a configured database (or the
    /// database couldn't be reached, which is treated conservatively the
    /// same way) — never removed, not even with `--force`: deleting the
    /// files backing an applied migration would leave the database unable
    /// to reconstruct or roll back that step.
    Applied(PathBuf),
    /// More than one on-disk migration directory normalizes to the same
    /// suffix (e.g. a `model`-generated migration and an independently
    /// hand-run `generate migration` for the same resource) and content
    /// comparison couldn't tell them apart either — never guess which one
    /// to delete.
    Ambiguous(String),
    /// This is the `mail_unsubscribes` suppression migration (the one
    /// migration in the codebase reused across multiple resources of the
    /// same generator, via `create_if_absent` — see
    /// `mailer::plan_unsubscribe_migration`) and another mailer file besides
    /// the ones this destroy is itself removing still opts into
    /// `--list-unsubscribe` — never removed except with `--force`.
    StillNeededElsewhere(PathBuf),
    /// No on-disk directory matches this suffix — already destroyed, or
    /// never generated.
    NotFound,
}

/// Whether every `Create`/`CreateIfAbsent` action's expected content
/// exactly matches the corresponding file in `dir` — used only to
/// disambiguate between multiple same-suffix migration directories, so it
/// never honours `--force` (a loose match here would let `--force` guess
/// wrong on top of bypassing safety, rather than just bypassing safety).
fn migration_dir_matches_actions(dir: &Path, actions: &[&Action]) -> bool {
    actions.iter().all(|action| {
        let Some(file_name) = action.path().file_name() else {
            return true;
        };
        let expected: &str = match action {
            Action::Create { contents, .. } | Action::CreateIfAbsent { contents, .. } => contents,
            Action::CreateBytes { .. } | Action::Modify { .. } => return true,
        };
        fs::read_to_string(dir.join(file_name)).is_ok_and(|actual| actual == *expected)
    })
}

/// Split a migration directory name into its leading numeric timestamp
/// ("version") and the remaining suffix, e.g.
/// `"20260706120000_create_posts"` → `("20260706120000", "create_posts")`.
fn split_migration_dir_name(name: &str) -> (&str, &str) {
    let digit_len = name
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(name.len());
    let (version, rest) = name.split_at(digit_len);
    (version, rest.strip_prefix('_').unwrap_or(rest))
}

/// Resolve one migration-directory group from the (destroy-time,
/// fresh-timestamp) plan against the real on-disk `migrations/` directory,
/// matched by suffix, and decide whether it's safe to remove.
fn resolve_migration_removal(
    plan_dir: &Path,
    actions: &[&Action],
    migrations_root: &Path,
    force: bool,
    project_root: &Path,
    excluding: &[PathBuf],
) -> MigrationOutcome {
    let Some(plan_dir_name) = plan_dir.file_name().and_then(|n| n.to_str()) else {
        return MigrationOutcome::NotFound;
    };
    let (_, suffix) = split_migration_dir_name(plan_dir_name);

    let Ok(entries) = fs::read_dir(migrations_root) else {
        return MigrationOutcome::NotFound;
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| split_migration_dir_name(name).1 == suffix)
        })
        .collect();
    candidates.sort();

    let real_dir = match candidates.len() {
        0 => return MigrationOutcome::NotFound,
        1 => candidates.into_iter().next().expect("len checked above"),
        _ => {
            // Multiple on-disk directories share this suffix — disambiguate
            // by exact content match rather than guessing (filesystem read
            // order is unspecified, so picking the first would be
            // non-deterministic).
            let mut matching: Vec<PathBuf> = candidates
                .into_iter()
                .filter(|dir| migration_dir_matches_actions(dir, actions))
                .collect();
            if matching.len() != 1 {
                return MigrationOutcome::Ambiguous(suffix.to_owned());
            }
            matching.remove(0)
        }
    };

    for action in actions {
        let Some(file_name) = action.path().file_name() else {
            continue;
        };
        let expected: &str = match action {
            Action::Create { contents, .. } | Action::CreateIfAbsent { contents, .. } => contents,
            Action::CreateBytes { .. } | Action::Modify { .. } => continue,
        };
        let real_file = real_dir.join(file_name);
        let matches = fs::read_to_string(&real_file).is_ok_and(|actual| actual == *expected);
        if !matches && !force {
            return MigrationOutcome::Diverged(real_file);
        }
    }

    // Refuse to sweep up files this generator never planned — a hand-added
    // `README.md` or an auxiliary fixture living alongside `up.sql`/
    // `down.sql` — unless `--force` is passed (issue #1048 PR review).
    // Without this, `remove_dir_all` below would silently delete
    // hand-authored content just because it happens to share a directory
    // with the generated migration files.
    if !force && let Ok(entries) = fs::read_dir(&real_dir) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let planned = actions
                .iter()
                .any(|action| action.path().file_name() == Some(name.as_os_str()));
            if !planned {
                return MigrationOutcome::Diverged(entry.path());
            }
        }
    }

    if !force
        && mail_unsubscribes_migration_still_needed_elsewhere(&real_dir, project_root, excluding)
    {
        return MigrationOutcome::StillNeededElsewhere(real_dir);
    }

    // Deliberately NOT gated on `force`: `--force` bypasses content
    // divergence and the sibling-resource "still needed elsewhere" caution,
    // but never the applied-migration check (issue #1048 PR review) —
    // removing files backing a migration the database already recorded as
    // applied would leave that database unable to reconstruct or roll back
    // the step, which is a data-safety concern `--force`'s documented
    // purpose (bypassing content mismatches) was never meant to cover.
    let real_dir_name = real_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let (version, _) = split_migration_dir_name(real_dir_name);
    match migration_applied_status(version, migrations_root) {
        MigrationStatus::Applied | MigrationStatus::Unknown => MigrationOutcome::Applied(real_dir),
        MigrationStatus::NotApplied | MigrationStatus::NotConfigured => {
            MigrationOutcome::Remove(real_dir)
        }
    }
}

/// Whether `real_dir` is the `mail_unsubscribes` suppression migration and
/// another mailer file (besides ones this same destroy is itself removing)
/// still opts into `--list-unsubscribe` — the one migration in the codebase
/// reused across multiple resources of the same generator via
/// `create_if_absent` (see `mailer::plan_unsubscribe_migration`), so it
/// needs its own sibling check rather than the generic `owner_dir`
/// mechanism `Revert::CargoDeps` and friends use.
fn mail_unsubscribes_migration_still_needed_elsewhere(
    real_dir: &Path,
    project_root: &Path,
    excluding: &[PathBuf],
) -> bool {
    let is_unsubscribes_migration = real_dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| split_migration_dir_name(name).1 == "create_mail_unsubscribes");
    if !is_unsubscribes_migration {
        return false;
    }
    let Ok(entries) = fs::read_dir(project_root.join("src").join("mailers")) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        path.extension().is_some_and(|ext| ext == "rs")
            && !excluding.contains(&path)
            && fs::read_to_string(&path)
                .is_ok_and(|content| content.contains("list_unsubscribe = "))
    })
}

/// Best-effort classification of whether a migration has already been
/// applied to a database.
enum MigrationStatus {
    /// No database URL is configured — this project has never run
    /// migrations against a real database, so the just-generated migration
    /// cannot be applied either.
    NotConfigured,
    /// A live database check confirmed it has not been applied.
    NotApplied,
    /// A live database check confirmed it has been applied.
    Applied,
    /// A database URL is configured but couldn't be reached/queried —
    /// treated the same as `Applied` (conservative: never delete migration
    /// files destroy can't confirm are safe to remove).
    Unknown,
}

fn migration_applied_status(version: &str, migrations_dir: &Path) -> MigrationStatus {
    // Resolve the same profile/overlay `autumn migrate` would (see
    // `migrate::resolve_targets`), not just the bare base `autumn.toml` — a
    // project that only configures its database via `[profile.prod.database]`
    // or an `autumn-prod.toml` overlay must not look "NotConfigured" here,
    // which would defeat the applied-migration safety guard below.
    let profile = crate::migrate::effective_profile(None);
    let table = crate::migrate::read_autumn_toml_table_with_profile(Some(&profile));
    let control_url = crate::migrate::resolve_primary_database_url_from_sources(
        |k| std::env::var(k),
        table.as_ref(),
    );
    // `autumn migrate run --shard <name>` applies user migrations to shard
    // databases too, independently of the control database (issue #1048 PR
    // review) — a sharded project checking only the control URL could
    // delete a migration's files while a shard still records it as applied,
    // leaving that shard unable to reconstruct or roll back the step.
    let shard_urls = crate::migrate::resolve_shard_database_urls_from_sources(
        |k| std::env::var(k),
        table.as_ref(),
    );
    let urls: Vec<String> = control_url
        .into_iter()
        .chain(shard_urls.into_iter().map(|(_, url)| url))
        .collect();
    if urls.is_empty() {
        return MigrationStatus::NotConfigured;
    }
    // Applied on ANY target (control or a shard) blocks removal outright;
    // otherwise an unreachable target is treated the same as "applied"
    // (conservative: never delete migration files destroy can't confirm are
    // safe to remove on every configured database).
    let mut any_unreachable = false;
    for url in urls {
        match autumn_web::migrate::applied_user_migrations(&url, migrations_dir) {
            Ok(applied) if applied.iter().any(|m| m.version == version) => {
                return MigrationStatus::Applied;
            }
            Ok(_) => {}
            Err(_) => any_unreachable = true,
        }
    }
    if any_unreachable {
        MigrationStatus::Unknown
    } else {
        MigrationStatus::NotApplied
    }
}

/// Shared, infrastructure-only module names any generator's `update_main_rs`
/// call might declare in `src/main.rs` — never resource-specific. Destroy
/// only ever removes one of these once its backing file/directory no longer
/// exists, so a project with a second resource still using it is untouched
/// (issue #1048's "never remove shared declarations" guarantee, made
/// precise: shared while still needed, removed once genuinely orphaned).
const SHARED_MAIN_MODULE_NAMES: &[&str] = &[
    "models",
    "schema",
    "repositories",
    "routes",
    "channels",
    "jobs",
    "mailers",
    "inbound_mailers",
    "policies",
    // A fixed single-file module (`src/notifications.rs`, no directory) —
    // `shared_module_backing_path_exists` already handles that shape.
    "notifications",
    // `autumn generate teams` (issue #1261) — like the other directory-backed
    // entries above, `shared_module_backing_path_exists` treats this as
    // orphaned once `src/teams/` is gone.
    "teams",
];

/// Whether `name`'s backing module still exists on disk — either as
/// `src/<name>.rs` (the shape `schema` and a single-file resource module
/// use) or `src/<name>/mod.rs` (a directory of per-resource files).
fn shared_module_backing_path_exists(project_root: &Path, name: &str) -> bool {
    let src = project_root.join("src");
    src.join(format!("{name}.rs")).is_file() || src.join(name).join("mod.rs").is_file()
}

/// After a real (non-dry-run) `Plan::revert`, strip any [`SHARED_MAIN_MODULE_NAMES`]
/// declaration from `src/main.rs` whose backing module no longer exists —
/// the corresponding directory/file was just emptied and deleted by this
/// same `revert` call. A no-op if `src/main.rs` is missing or nothing
/// qualifies.
fn sync_main_rs_mod_declarations(project_root: &Path) {
    let main_path = project_root.join("src").join("main.rs");
    let Ok(content) = fs::read_to_string(&main_path) else {
        return;
    };
    let orphaned: Vec<&str> = SHARED_MAIN_MODULE_NAMES
        .iter()
        .copied()
        .filter(|name| {
            content.lines().any(|l| l.trim() == format!("mod {name};"))
                && !shared_module_backing_path_exists(project_root, name)
        })
        .collect();
    if orphaned.is_empty() {
        return;
    }
    let updated = super::schema_edit::remove_main_mod_declarations(&content, &orphaned);
    if updated != content && fs::write(&main_path, &updated).is_ok() {
        println!("  Reverted {}", relative_display(&main_path, project_root));
    }
}

/// Strip a `pub mod <name>;` declaration from `<dir>/mod.rs` for any `name`
/// in `shared_names` whose backing file/directory no longer exists — the
/// same "shared while still needed, removed once orphaned" rule
/// [`sync_main_rs_mod_declarations`] applies to `src/main.rs`, but for a
/// nested shared sub-module declared in some *other* generated `mod.rs`
/// (e.g. `src/mailers/mod.rs`'s `pub mod previews;`, shared by every
/// generated mailer, not owned by one). A no-op if `<dir>/mod.rs` is missing
/// or nothing qualifies. If removing every orphaned declaration empties the
/// file, it is deleted instead of left blank.
fn sync_mod_declarations_in(dir: &Path, shared_names: &[&str], project_root: &Path) {
    let mod_path = dir.join("mod.rs");
    let Ok(content) = fs::read_to_string(&mod_path) else {
        return;
    };
    let orphaned: Vec<&str> = shared_names
        .iter()
        .copied()
        .filter(|name| {
            content
                .lines()
                .any(|l| l.trim() == format!("pub mod {name};"))
                && !dir.join(format!("{name}.rs")).is_file()
                && !dir.join(name).join("mod.rs").is_file()
        })
        .collect();
    if orphaned.is_empty() {
        return;
    }
    let mut updated = content.clone();
    for name in orphaned {
        updated = super::schema_edit::remove_mod_declaration(&updated, name);
    }
    if updated == content {
        return;
    }
    if updated.trim().is_empty() {
        if fs::remove_file(&mod_path).is_ok() {
            println!("  Removed {}", relative_display(&mod_path, project_root));
            prune_empty_ancestors(dir.to_path_buf(), project_root);
        }
    } else if fs::write(&mod_path, &updated).is_ok() {
        println!("  Reverted {}", relative_display(&mod_path, project_root));
    }
}

fn relative_display(path: &Path, root: &Path) -> String {
    let display = path
        .strip_prefix(root)
        .map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
    // Always render with forward slashes so the generator's output (and any
    // tests that grep for it) is platform-consistent.
    display.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Plan) {
        let tmp = tempfile::TempDir::new().unwrap();
        let plan = Plan::new(tmp.path());
        (tmp, plan)
    }

    #[test]
    fn new_plan_has_no_warnings() {
        let (_tmp, plan) = fixture();
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn warn_records_message_and_execute_does_not_fail() {
        let (_tmp, mut plan) = fixture();
        plan.warn("referenced table 'posts' is assumed to exist");
        assert_eq!(plan.warnings.len(), 1);
        // A warning never fails the plan — it's advisory only.
        plan.execute(Flags::default()).unwrap();
    }

    #[test]
    fn warnings_do_not_block_a_collision_error() {
        // A colliding Create must still fail with Collisions even when the
        // plan also carries a warning (the warning is printed to stderr,
        // which this test can't capture in-process, but it must never be
        // allowed to suppress or alter the underlying error).
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "existing").unwrap();
        plan.warn("some advisory message");
        plan.create(target, "new");
        let err = plan.execute(Flags::default()).unwrap_err();
        assert!(matches!(err, GenerateError::Collisions(_)));
    }

    #[test]
    fn create_action_writes_file() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags::default()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
    }

    #[test]
    fn create_action_creates_parent_dirs() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("nested/dir/out.txt");
        plan.create(target.clone(), "hi");
        plan.execute(Flags::default()).unwrap();
        assert!(target.exists());
    }

    #[test]
    fn collision_without_force_errors() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "old").unwrap();
        plan.create(target.clone(), "new");
        let err = plan.execute(Flags::default()).unwrap_err();
        match err {
            GenerateError::Collisions(paths) => {
                assert_eq!(paths, vec![target.clone()]);
            }
            _ => panic!("expected collision error, got {err:?}"),
        }
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
    }

    #[test]
    fn collision_with_force_overwrites() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "old").unwrap();
        plan.create(target.clone(), "new");
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn modify_action_overwrites_without_force() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "old").unwrap();
        plan.modify(target.clone(), "new");
        plan.execute(Flags::default()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn dry_run_skips_collision_check() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "existing").unwrap();
        plan.create(target.clone(), "new");
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "existing");
    }

    #[test]
    fn collision_lists_every_offender() {
        let (tmp, mut plan) = fixture();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, "x").unwrap();
        fs::write(&b, "y").unwrap();
        plan.create(a.clone(), "1");
        plan.create(b.clone(), "2");
        let err = plan.execute(Flags::default()).unwrap_err();
        let msg = err.to_string();
        // The error message normalises path separators to `/` so the
        // assertion needs to match that form (Windows uses `\` natively).
        assert!(msg.contains(a.display().to_string().replace('\\', "/").as_str()));
        assert!(msg.contains(b.display().to_string().replace('\\', "/").as_str()));
    }

    // ── `Plan::revert` — the inverse of `Plan::execute` (issue #1048) ──────

    fn no_db_env<R>(f: impl FnOnce() -> R) -> R {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            f,
        )
    }

    #[test]
    fn revert_removes_create_action_file() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags::default()).unwrap();
        assert!(target.exists());
        plan.revert(Flags::default()).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn revert_is_idempotent_when_file_already_missing() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        // Never executed — file was never created.
        plan.revert(Flags::default()).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn revert_refuses_on_diverged_content_without_force() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags::default()).unwrap();
        fs::write(&target, "hand-edited by user").unwrap();
        let err = plan.revert(Flags::default()).unwrap_err();
        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(target.exists(), "diverged file must not be deleted");
    }

    #[test]
    fn revert_with_force_deletes_diverged_file() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags::default()).unwrap();
        fs::write(&target, "hand-edited by user").unwrap();
        plan.revert(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn revert_dry_run_does_not_touch_disk() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags::default()).unwrap();
        plan.revert(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(target.exists());
    }

    #[test]
    fn revert_applies_mod_decl_revert_and_deletes_now_empty_file() {
        let (tmp, mut plan) = fixture();
        let mod_path = tmp.path().join("mod.rs");
        plan.modify(
            mod_path.clone(),
            crate::generate::schema_edit::add_mod_declaration("", "post"),
        );
        plan.push_revert(Revert::ModDecl {
            path: mod_path.clone(),
            name: "post".to_owned(),
        });
        plan.execute(Flags::default()).unwrap();
        assert!(mod_path.exists());
        plan.revert(Flags::default()).unwrap();
        assert!(!mod_path.exists());
    }

    #[test]
    fn revert_applies_mod_decl_revert_preserving_other_mods() {
        let (tmp, mut plan) = fixture();
        let mod_path = tmp.path().join("mod.rs");
        fs::write(&mod_path, "pub mod user;\n").unwrap();
        plan.modify(
            mod_path.clone(),
            crate::generate::schema_edit::add_mod_declaration("pub mod user;\n", "post"),
        );
        plan.push_revert(Revert::ModDecl {
            path: mod_path.clone(),
            name: "post".to_owned(),
        });
        plan.execute(Flags::default()).unwrap();
        plan.revert(Flags::default()).unwrap();
        assert!(mod_path.exists());
        assert_eq!(fs::read_to_string(&mod_path).unwrap(), "pub mod user;\n");
    }

    #[test]
    fn revert_prunes_now_empty_generated_directory_but_not_sibling_content() {
        let (tmp, mut plan) = fixture();
        let posts_tmpl = tmp.path().join("templates/posts/index.html.tmpl");
        plan.create(posts_tmpl.clone(), "content");
        plan.execute(Flags::default()).unwrap();
        // A sibling directory under `templates/` must survive pruning.
        fs::create_dir_all(tmp.path().join("templates/other")).unwrap();
        fs::write(tmp.path().join("templates/other/keep.tmpl"), "keep").unwrap();

        plan.revert(Flags::default()).unwrap();

        assert!(!posts_tmpl.exists());
        assert!(!tmp.path().join("templates/posts").exists());
        assert!(tmp.path().join("templates").exists());
        assert!(tmp.path().join("templates/other/keep.tmpl").exists());
    }

    #[test]
    fn revert_never_deletes_a_diverged_create_if_absent_file_even_with_force() {
        // A `CreateIfAbsent` target (e.g. a shared mailer layout) may
        // pre-exist before ANY generator ever wrote to it — `generate`
        // silently skips it either way, so content divergence here can't be
        // attributed to "this destroy's own edit gone stale" the way it can
        // for an owned `Create`. `--force` must never delete it (issue
        // #1048 PR review): guessing wrong would destroy real, pre-existing
        // project content this destroy never touched.
        let (tmp, mut plan) = fixture();
        let layout = tmp.path().join("templates/mailers/_layout.html");
        fs::create_dir_all(layout.parent().unwrap()).unwrap();
        fs::write(&layout, "hand-rolled layout, predates any mailer").unwrap();
        plan.create_if_absent(layout.clone(), "generated default layout");

        plan.revert(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();

        assert!(
            layout.exists(),
            "pre-existing content diverging from the generated default must survive \
             even with --force"
        );
        assert_eq!(
            fs::read_to_string(&layout).unwrap(),
            "hand-rolled layout, predates any mailer"
        );
    }

    #[test]
    fn revert_deletes_a_create_if_absent_file_when_content_matches_the_generated_default() {
        let (tmp, mut plan) = fixture();
        let layout = tmp.path().join("templates/mailers/_layout.html");
        fs::create_dir_all(layout.parent().unwrap()).unwrap();
        plan.create_if_absent(layout.clone(), "generated default layout");
        plan.execute(Flags::default()).unwrap();

        plan.revert(Flags::default()).unwrap();

        assert!(!layout.exists());
    }

    #[test]
    fn revert_keeps_a_dependency_still_needed_by_another_table_in_the_same_schema_file() {
        // issue #1048 PR review: excluding the WHOLE `src/schema.rs` from
        // the crate-usage scan (because this destroy is also rewriting it)
        // hid a second, unrelated `diesel::table!` block that survives the
        // destroy — wrongly treating `diesel` as unused project-wide when
        // it's still needed by that other table. The scan must see
        // `schema.rs`'s real POST-destroy content, not exclude the file
        // outright.
        use crate::generate::dsl::parse_fields;
        use crate::generate::schema_edit::append_schema_table;

        let (tmp, mut plan) = fixture();
        let fields = parse_fields(&["title:String".to_owned()]).unwrap();
        let posts_block = append_schema_table("", "posts", &fields);
        let schema_path = tmp.path().join("src/schema.rs");
        fs::create_dir_all(schema_path.parent().unwrap()).unwrap();
        // A second, pre-existing table this destroy never touches.
        let schema_with_users = append_schema_table("", "users", &fields);
        let schema_content = append_schema_table(&schema_with_users, "posts", &fields);
        fs::write(&schema_path, &schema_content).unwrap();

        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(
            &cargo_path,
            "[package]\nname = \"x\"\n\n[dependencies]\ndiesel = \"2\"\n",
        )
        .unwrap();

        plan.push_revert(Revert::SchemaTable {
            path: schema_path.clone(),
            table: "posts".to_owned(),
            expected_block: posts_block,
        });
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        plan.push_revert(Revert::CargoDeps {
            path: cargo_path.clone(),
            names: vec!["diesel".to_owned()],
            owner_dir: models_dir,
        });

        plan.revert(Flags::default()).unwrap();

        let schema_after = fs::read_to_string(&schema_path).unwrap();
        assert!(
            !schema_after.contains("posts ("),
            "posts table must be removed"
        );
        assert!(
            schema_after.contains("users ("),
            "the other table must survive: {schema_after}"
        );
        let cargo_after = fs::read_to_string(&cargo_path).unwrap();
        assert!(
            cargo_after.contains("diesel"),
            "diesel must survive — schema.rs still has a diesel::table! for `users`: \
             {cargo_after}"
        );
    }

    #[test]
    fn revert_keeps_ws_feature_when_a_live_scaffold_route_still_uses_sse_stream() {
        // `--live` scaffolds and SSE-transport channels both need the "ws"
        // feature via `autumn_web::sse::stream(...)`, with no `#[ws]`
        // marker at all (issue #1048 PR review) — destroying the only
        // WebSocket-transport channel must not strip "ws" while a live
        // scaffold's route in a completely different directory still needs
        // it.
        let (tmp, mut plan) = fixture();
        let routes_dir = tmp.path().join("src/routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("posts.rs"),
            "pub async fn stream_posts() {\n    autumn_web::sse::stream(&state, \"posts\")\n}\n",
        )
        .unwrap();

        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(
            &cargo_path,
            "[package]\nname = \"x\"\n\n[dependencies]\nautumn-web = { version = \"0.6\", features = [\"ws\"] }\n",
        )
        .unwrap();

        let channels_dir = tmp.path().join("src/channels");
        fs::create_dir_all(&channels_dir).unwrap();
        plan.push_revert(Revert::CargoAutumnWebFeature {
            path: cargo_path.clone(),
            feature: "ws".to_owned(),
            owner_dir: Some(channels_dir),
        });

        plan.revert(Flags::default()).unwrap();

        let cargo_after = fs::read_to_string(&cargo_path).unwrap();
        assert!(
            cargo_after.contains("\"ws\""),
            "ws must survive — a live scaffold route still uses autumn_web::sse::stream: \
             {cargo_after}"
        );
    }

    #[test]
    fn revert_removes_unapplied_migration_matched_by_suffix() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            fs::create_dir_all(tmp.path().join("migrations")).unwrap();
            // Destroy recomputes the plan with a FRESH timestamp, which won't
            // match the real, already-generated directory on disk.
            let plan_dir = tmp.path().join("migrations/99999999999999_create_posts");
            let real_dir = tmp.path().join("migrations/20260101000000_create_posts");
            fs::create_dir_all(&real_dir).unwrap();
            fs::write(real_dir.join("up.sql"), "CREATE TABLE posts ();\n").unwrap();
            fs::write(real_dir.join("down.sql"), "DROP TABLE posts;\n").unwrap();

            plan.create(plan_dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(plan_dir.join("down.sql"), "DROP TABLE posts;\n");
            // Not executing this plan (it would create the wrong-timestamp
            // dir) — the point is that revert must find `real_dir` by suffix.

            plan.revert(Flags::default()).unwrap();

            assert!(!real_dir.exists());
        });
    }

    #[test]
    fn revert_refuses_diverged_migration_sql_without_force() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            fs::create_dir_all(tmp.path().join("migrations")).unwrap();
            let plan_dir = tmp.path().join("migrations/99999999999999_create_posts");
            let real_dir = tmp.path().join("migrations/20260101000000_create_posts");
            fs::create_dir_all(&real_dir).unwrap();
            fs::write(
                real_dir.join("up.sql"),
                "CREATE TABLE posts (extra_col INT);\n",
            )
            .unwrap();
            fs::write(real_dir.join("down.sql"), "DROP TABLE posts;\n").unwrap();

            plan.create(plan_dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(plan_dir.join("down.sql"), "DROP TABLE posts;\n");

            let err = plan.revert(Flags::default()).unwrap_err();
            assert!(matches!(err, GenerateError::Diverged(_)));
            assert!(real_dir.exists());
        });
    }

    #[test]
    fn revert_never_removes_a_migration_with_an_unreachable_database_even_with_force() {
        // A configured but unreachable database is treated the same as
        // "applied" (conservative: destroy can't confirm it's safe). Even
        // `--force` must not remove the migration files in that case
        // (issue #1048 PR review) — `--force` bypasses content divergence
        // and shared-resource caution, never the applied-migration guard,
        // since deleting files backing a migration the database might
        // already record as applied would leave it unable to reconstruct
        // or roll back that step.
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                (
                    "AUTUMN_DATABASE__URL",
                    Some("postgres://postgres:x@127.0.0.1:1/nope"),
                ),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let (tmp, mut plan) = fixture();
                fs::create_dir_all(tmp.path().join("migrations")).unwrap();
                let plan_dir = tmp.path().join("migrations/99999999999999_create_posts");
                let real_dir = tmp.path().join("migrations/20260101000000_create_posts");
                fs::create_dir_all(&real_dir).unwrap();
                fs::write(real_dir.join("up.sql"), "CREATE TABLE posts ();\n").unwrap();
                fs::write(real_dir.join("down.sql"), "DROP TABLE posts;\n").unwrap();

                plan.create(plan_dir.join("up.sql"), "CREATE TABLE posts ();\n");
                plan.create(plan_dir.join("down.sql"), "DROP TABLE posts;\n");

                plan.revert(Flags {
                    force: true,
                    dry_run: false,
                })
                .unwrap();

                assert!(
                    real_dir.exists(),
                    "migration must survive even with --force when the database is unreachable"
                );
            },
        );
    }

    #[test]
    fn revert_never_removes_a_migration_when_only_an_unreachable_shard_is_configured() {
        // A shard-only deployment (no control database role) is a valid
        // shape (issue #1048 PR review): `autumn migrate run --shard
        // <name>` applies user migrations to shard databases independently
        // of any control URL. Checking only the control URL would see "no
        // database configured" and happily delete migration files a shard
        // still needs.
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
                ("AUTUMN_DATABASE__SHARDS__0__NAME", Some("shard0")),
                (
                    "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL",
                    Some("postgres://postgres:x@127.0.0.1:1/nope"),
                ),
            ],
            || {
                let (tmp, mut plan) = fixture();
                fs::create_dir_all(tmp.path().join("migrations")).unwrap();
                let plan_dir = tmp.path().join("migrations/99999999999999_create_posts");
                let real_dir = tmp.path().join("migrations/20260101000000_create_posts");
                fs::create_dir_all(&real_dir).unwrap();
                fs::write(real_dir.join("up.sql"), "CREATE TABLE posts ();\n").unwrap();
                fs::write(real_dir.join("down.sql"), "DROP TABLE posts;\n").unwrap();

                plan.create(plan_dir.join("up.sql"), "CREATE TABLE posts ();\n");
                plan.create(plan_dir.join("down.sql"), "DROP TABLE posts;\n");

                plan.revert(Flags {
                    force: true,
                    dry_run: false,
                })
                .unwrap();

                assert!(
                    real_dir.exists(),
                    "migration must survive when a shard is configured but unreachable, \
                     even with no control database URL and even with --force"
                );
            },
        );
    }

    #[test]
    fn revert_is_idempotent_when_migration_dir_missing() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            fs::create_dir_all(tmp.path().join("migrations")).unwrap();
            let plan_dir = tmp.path().join("migrations/99999999999999_create_posts");
            plan.create(plan_dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(plan_dir.join("down.sql"), "DROP TABLE posts;\n");

            // No matching real directory on disk at all — nothing to do.
            plan.revert(Flags::default()).unwrap();
        });
    }

    #[test]
    fn revert_refuses_to_guess_between_two_migrations_sharing_a_suffix() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            fs::create_dir_all(tmp.path().join("migrations")).unwrap();
            let plan_dir = tmp.path().join("migrations/99999999999999_create_posts");
            // Two independently-created directories normalize to the same
            // suffix (e.g. a `model`-generated one and a hand-run `generate
            // migration` for the same resource), and BOTH have content that
            // exactly matches what this plan expects — content-based
            // disambiguation genuinely can't tell them apart either.
            let first = tmp.path().join("migrations/20260101000000_create_posts");
            let second = tmp.path().join("migrations/20260201000000_create_posts");
            fs::create_dir_all(&first).unwrap();
            fs::write(first.join("up.sql"), "CREATE TABLE posts ();\n").unwrap();
            fs::write(first.join("down.sql"), "DROP TABLE posts;\n").unwrap();
            fs::create_dir_all(&second).unwrap();
            fs::write(second.join("up.sql"), "CREATE TABLE posts ();\n").unwrap();
            fs::write(second.join("down.sql"), "DROP TABLE posts;\n").unwrap();

            plan.create(plan_dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(plan_dir.join("down.sql"), "DROP TABLE posts;\n");

            // Refuses to guess, even without --force: neither directory is removed.
            plan.revert(Flags::default()).unwrap();
            assert!(
                first.exists(),
                "ambiguous suffix must not delete either dir"
            );
            assert!(
                second.exists(),
                "ambiguous suffix must not delete either dir"
            );
        });
    }

    #[test]
    fn revert_disambiguates_shared_suffix_migrations_by_exact_content_match() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            fs::create_dir_all(tmp.path().join("migrations")).unwrap();
            let plan_dir = tmp.path().join("migrations/99999999999999_create_posts");
            // Two directories share a suffix, but only one has content that
            // exactly matches what this plan would have produced — that one
            // (and only that one) should be identified and removed.
            let matching = tmp.path().join("migrations/20260101000000_create_posts");
            let other = tmp.path().join("migrations/20260201000000_create_posts");
            fs::create_dir_all(&matching).unwrap();
            fs::write(matching.join("up.sql"), "CREATE TABLE posts ();\n").unwrap();
            fs::write(matching.join("down.sql"), "DROP TABLE posts;\n").unwrap();
            fs::create_dir_all(&other).unwrap();
            fs::write(other.join("up.sql"), "CREATE TABLE posts (extra INT);\n").unwrap();
            fs::write(other.join("down.sql"), "DROP TABLE posts;\n").unwrap();

            plan.create(plan_dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(plan_dir.join("down.sql"), "DROP TABLE posts;\n");

            plan.revert(Flags::default()).unwrap();

            assert!(
                !matching.exists(),
                "the content-matching directory must be removed"
            );
            assert!(
                other.exists(),
                "the non-matching directory must be left alone"
            );
        });
    }

    #[test]
    fn revert_prunes_now_empty_directory_after_shared_submodule_deletion() {
        // A single mailer's `src/mailers/previews/mod.rs` empties down to
        // nothing (deleted + pruned by the ordinary Modify-turned-empty
        // path), which then orphans `pub mod previews;` in
        // `src/mailers/mod.rs` — cleaned up by `sync_mod_declarations_in`.
        // That leaves `src/mailers/` itself empty; it must be pruned too,
        // exactly like every other now-empty generated directory.
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/main.rs"),
            "use autumn_web::prelude::*;\n\n#[autumn_web::main]\nasync fn main() {\n    \
             autumn_web::app().routes(routes![]).run().await;\n}\n",
        )
        .unwrap();

        let plan =
            crate::generate::mailer::plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/mailers/mod.rs").exists());

        plan.revert(Flags::default()).unwrap();

        assert!(
            !tmp.path().join("src/mailers").exists(),
            "src/mailers/ must be fully pruned once mod.rs and every file under it are gone, \
             just like every other now-empty generated directory"
        );
    }

    #[test]
    fn multipart_and_storage_have_feature_markers() {
        // Attachment scaffolds enable `autumn-web/multipart` and
        // `autumn-web/storage`; both must carry markers so `autumn destroy`
        // of the last attachment model can't strip a feature a hand-written
        // route still uses (PR #1867 review).
        // The marker is the bare `Multipart` substring so it also catches a
        // route that names the extractor unqualified via `use
        // autumn_web::prelude::*;` (the prelude re-exports `Multipart`), not
        // just the fully-qualified `autumn_web::extract::Multipart` spelling.
        let multipart = autumn_web_feature_markers("multipart");
        assert!(
            multipart.contains(&"Multipart"),
            "multipart marker must catch bare `Multipart` (covers prelude-unqualified \
             and `autumn_web::extract::Multipart` usage), got {multipart:?}"
        );

        let storage = autumn_web_feature_markers("storage");
        assert!(
            storage.contains(&"autumn_web::storage::"),
            "storage marker must catch `autumn_web::storage::` usage, got {storage:?}"
        );
    }

    #[test]
    fn multipart_and_storage_markers_retain_features_for_handwritten_routes() {
        // A hand-written route using the multipart extractor and the blob
        // store must keep those features alive even when the model/route the
        // attachment scaffold generated is being destroyed (excluded). The
        // route deliberately imports through `use autumn_web::prelude::*;` and
        // names `Multipart` UNQUALIFIED (the prelude re-exports it), the exact
        // shape the narrower `extract::Multipart` marker used to miss; the
        // blob store stays a `autumn_web::storage::` path since the prelude
        // does not re-export storage types.
        let tmp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src/routes")).unwrap();
        let handwritten = tmp.path().join("src/routes/manual.rs");
        fs::write(
            &handwritten,
            "use autumn_web::prelude::*;\n\n\
             async fn upload(mut mp: Multipart, store: autumn_web::storage::BlobStoreState) {}\n",
        )
        .unwrap();

        // The scaffold's own generated files are being destroyed — excluded
        // from the scan so they can't self-retain — but the hand-written
        // route is not, so both features must stay needed.
        let excluding = vec![tmp.path().join("src/models/photo.rs")];
        let overrides = HashMap::new();
        assert!(
            autumn_web_feature_still_needed_elsewhere(
                "multipart",
                tmp.path(),
                &excluding,
                &overrides
            ),
            "multipart must be retained while a hand-written route uses the Multipart extractor"
        );
        assert!(
            autumn_web_feature_still_needed_elsewhere(
                "storage",
                tmp.path(),
                &excluding,
                &overrides
            ),
            "storage must be retained while a hand-written route references autumn_web::storage::"
        );

        // With the hand-written route removed too, nothing references the
        // features anymore — they become free to strip.
        fs::remove_file(&handwritten).unwrap();
        assert!(!autumn_web_feature_still_needed_elsewhere(
            "multipart",
            tmp.path(),
            &excluding,
            &overrides
        ));
        assert!(!autumn_web_feature_still_needed_elsewhere(
            "storage",
            tmp.path(),
            &excluding,
            &overrides
        ));
    }
}
