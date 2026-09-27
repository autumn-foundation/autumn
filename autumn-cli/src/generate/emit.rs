//! File-emission engine shared by every generator.
//!
//! Generators describe what they want to do as a list of [`Action`]s, and
//! [`Plan::execute`] handles all the side-effecting filesystem work — including
//! collision detection, `--force` / `--dry-run`, and the human-readable
//! "Created/Modified" output that mirrors `autumn new`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use autumn_web::config::DatabaseBackend;

use super::{Flags, GenerateError, provenance};

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
    /// Remove the `[[security.webhooks.endpoints]]` entry `autumn generate
    /// webhook` added to `path` (`autumn.toml`) — matched on both `name` and
    /// `route_path`, so a hand-written endpoint that merely shares a name
    /// survives — along with its CSRF/CAPTCHA path exemptions and, once no
    /// endpoint is left, the shared `[security.webhooks.replay]` block.
    WebhookEndpointStub {
        path: PathBuf,
        name: String,
        route_path: String,
    },
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
    /// Remove one resource's block of generated Fluent keys from `path`
    /// (`i18n/en.ftl`) — the marked `# <Pascal> — generated by …` comment plus
    /// every `<snake>.` key `autumn generate scaffold --i18n` added (issue
    /// #1349).
    ///
    /// The shared `common.*` chrome block and the file itself always survive:
    /// sibling resources reuse the chrome, and the generated app calls
    /// `.i18n_auto()`, which panics at startup when the default locale's file is
    /// missing. Destroying the last `--i18n` resource therefore leaves an inert
    /// chrome-only bundle rather than a broken boot.
    I18nFtlKeys {
        path: PathBuf,
        pascal: String,
        snake: String,
    },
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
            | Self::WebhookEndpointStub { path, .. }
            | Self::SubmitTokenValidateExempt { path, .. }
            | Self::RememberMiddleware { path }
            | Self::SeedBinLinks { path, .. }
            | Self::NestedChildSection { path, .. }
            | Self::I18nFtlKeys { path, .. } => path,
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
            | Self::WebhookEndpointStub { .. }
            | Self::SubmitTokenValidateExempt { .. }
            | Self::NestedChildSection { .. }
            | Self::I18nFtlKeys { .. }
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
                // In addition to the `owner_dir` sibling-directory check the caller
                // already applied, remove a name only if nothing else in the project's
                // source tree still references it — a hand-added dependency unrelated
                // to any generator (#1048 PR review) survives the way a dependency
                // shared with a sibling resource already does. Best-effort: usage via a
                // re-export, or a derive macro that never spells out the crate's own
                // name — a plain `#[derive(Serialize)]` with no qualified `serde::`
                // path — is not detected.
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
                if autumn_web_feature_pinned_by_backend(feature, project_root)
                    || autumn_web_feature_still_needed_elsewhere(
                        feature,
                        project_root,
                        excluding,
                        overrides,
                    )
                {
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
            Self::WebhookEndpointStub {
                name, route_path, ..
            } => super::webhook::remove_webhook_endpoint(content, name, route_path),
            Self::SubmitTokenValidateExempt {
                plural, segment, ..
            } => super::scaffold::remove_submit_token_exempt_from_toml(content, plural, segment),
            Self::SeedBinLinks { .. } => super::schema_edit::unlink_models_from_seed_bin(content),
            Self::NestedChildSection { child_plural, .. } => {
                super::nested::remove_nested_child_section(content, child_plural)
            }
            Self::I18nFtlKeys { pascal, snake, .. } => {
                remove_i18n_keys(content, pascal, snake, project_root, excluding, overrides)
            }
        }
    }
}

/// Apply [`Revert::I18nFtlKeys`] to one `.ftl` bundle.
///
/// Split out of [`Revert::apply`] only to keep that match arm short; the
/// interesting part is the second argument to `remove_en_ftl_keys`. Whether any
/// OTHER generated resource still looks chrome keys up is decided by scanning
/// the surviving route modules, NOT by whether they happen to carry a `.ftl`
/// marker comment: a resource whose keys were all hand-authored before it was
/// scaffolded gets no marker (the merge had nothing to add), and taking the
/// shared chrome out from under it would break its build — `t!` validates key
/// existence at COMPILE time. Erring the other way only leaves an unused key,
/// which is a lint warning.
fn remove_i18n_keys(
    content: &str,
    pascal: &str,
    snake: &str,
    project_root: &Path,
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> String {
    // The WHOLE source tree, not just `src/routes`. Chrome keys are ordinary
    // Fluent keys once written, and nothing stops application code reusing
    // them — a `t!(locale, "common.save")` in `src/components/navigation.rs` is
    // exactly the sort of thing a project does. Scanning only the routes
    // directory would call such a key unused when the last scaffold is
    // destroyed, prune it, and break the surviving call at COMPILE time, which
    // is the one failure mode this set exists to prevent. `autumn i18n check`
    // reads the whole tree for the same reason.
    let surviving = keys_still_referenced(content, project_root, excluding, overrides);
    super::scaffold_i18n::remove_en_ftl_keys(content, pascal, snake, &surviving)
}

/// The keys `bundle` defines that project source still reaches — statically, or
/// through a runtime-built key.
///
/// Shared by the two paths that PRUNE a resource's keys, because they have to
/// agree: `destroy` takes the whole block, and a `--force` regeneration that
/// dropped a field takes the keys the new render no longer emits. A key a
/// surviving call site names must outlive both, and `t!` rejects a missing key
/// at COMPILE time, so a set one path honours and the other does not is just a
/// slower way to break the same build.
pub(super) fn keys_still_referenced(
    bundle: &str,
    project_root: &Path,
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> HashSet<String> {
    let scan = scan_surviving_sources(project_root, excluding, overrides);
    keys_still_referenced_in(&scan, bundle)
}

/// [`keys_still_referenced`] against a scan already taken.
///
/// One scan, several bundles: a project whose `t!` validator reads a different
/// file from the one the app loads has BOTH kept in step, and re-walking the
/// source tree per bundle would only give the two a chance to disagree.
pub(super) fn keys_still_referenced_in(
    scan: &crate::i18n::ScanResult,
    bundle: &str,
) -> HashSet<String> {
    // Every key the surviving tree references, NOT just the `common.*` ones.
    // The resource's own keys are ordinary Fluent keys too, and hand-written
    // code reaches for them: a dashboard card or a nav link labelled
    // `t!(locale, "post.index.title")` outlives `src/routes/post.rs`. Deleting
    // the whole marked `post.*` block out from under it breaks the build at
    // COMPILE time — the exact failure this set exists to prevent — and the
    // `common.` prefix was never what made a call site live.
    let mut surviving: HashSet<String> = scan.referenced.iter().cloned().collect();

    // A key built at runtime — `locale.t(&format!("common.{action}"))`, or a bare
    // `locale.t(key)` — names no key a static scan can record, so `scan_source` files it
    // under `dynamic`. Dropping those prunes definitions such a call still reaches at
    // runtime, leaving a missing-key marker in a surviving view. `autumn i18n check`
    // suppresses its unused-key report for exactly these sites, so this must be at least
    // as conservative, or pruning would delete what the checker declines to complain
    // about. `key_prefix` is the leading literal, empty meaning "could be any key", so
    // `starts_with` covers both.
    if !scan.dynamic.is_empty() {
        surviving.extend(
            super::scaffold_i18n::defined_keys(bundle)
                .into_keys()
                .filter(|key| {
                    scan.dynamic
                        .iter()
                        .any(|site| key.starts_with(&site.key_prefix))
                }),
        );
    }
    surviving
}

/// Every file the plan is about to write, as pending content the scan must read
/// INSTEAD of what is on disk.
///
/// A regeneration's own freshly rendered routes are the authority on which keys
/// it still uses; the superseded file on disk names the keys being dropped, and
/// reading that would protect exactly what the reconciliation exists to remove.
pub(super) fn pending_contents(plan: &Plan) -> HashMap<PathBuf, String> {
    plan.actions
        .iter()
        .filter_map(|action| match action {
            Action::Create { path, contents }
            | Action::Modify { path, contents }
            | Action::CreateIfAbsent { path, contents } => Some((path.clone(), contents.clone())),
            Action::CreateBytes { .. } => None,
        })
        .collect()
}

/// Every key still referenced by surviving project source.
///
/// The SET, not a yes/no, and per KEY rather than per block: chrome keys are
/// per-surface, so a resource can be the only user of some of them. Destroying
/// a `--soft-delete` resource while a plain one survives leaves
/// `common.trash`/`restore`/`purge` referenced by nothing, and `autumn i18n
/// check --strict` fails on exactly that — while `common.create`/`save`/… are
/// still very much in use and must stay. A resource's own keys work the same
/// way once hand-written code outside the module reaches for one.
///
/// Parsing is delegated to [`crate::i18n::scan_source`], the `syn`-based
/// scanner behind `autumn i18n check`. That is the authority this set has to
/// agree with — a key it counts and this does not gets pruned out from under a
/// live call, and `t!` rejects that at COMPILE time — and agreement is not
/// something two independently hand-written scanners converge on. The one here
/// had been extended, round after round, for rustfmt line breaks, comments,
/// string literals, arbitrary locale expressions, raw-string keys, and comments
/// before the key; it still recognised only `t!(…)`, never the equally
/// supported `locale.t(…)` / `Locale::t_with(…)`, and its walk started at
/// `src/` while the checker reads the whole project. Both were bugs of the same
/// shape, and neither can recur now.
///
/// The traversal stays here rather than calling `scan_project`, because the
/// generator must read the files it is ABOUT to change from the plan rather
/// than from disk: `overrides` supplies pending content, and `excluding` drops
/// the module being destroyed.
pub(super) fn scan_surviving_sources(
    project_root: &Path,
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> crate::i18n::ScanResult {
    let mut files = Vec::new();
    crate::i18n::collect_rs_files(project_root, &mut files);
    // A pending file that does not exist on disk yet still counts.
    files.extend(overrides.keys().cloned());
    files.sort();
    files.dedup();

    let mut scan = crate::i18n::ScanResult::default();
    for path in files {
        let content = overrides.get(&path).cloned().or_else(|| {
            if excluding.contains(&path) {
                None
            } else {
                fs::read_to_string(&path).ok()
            }
        });
        let Some(content) = content else { continue };
        crate::i18n::scan_source(&content, &path.to_string_lossy(), &mut scan);
    }
    scan
}

/// A complete generator plan — a sequence of actions plus the project root
/// they are anchored against.
#[derive(Debug)]
pub struct Plan {
    /// Project root all action paths are interpreted relative to.
    pub project_root: PathBuf,
    /// The command this plan belongs to, as
    /// [`provenance::current_invocation`] spells it. Recorded beside each
    /// owned file's digest so `destroy` honours the digest only for the same
    /// command with the same arguments (issue #1835).
    pub invocation: String,
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
        let project_root = project_root.into();
        Self {
            invocation: provenance::current_invocation(&project_root),
            project_root,
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

        // Digests of the files this run actually owns, for `revert` to compare
        // against once the template has moved on (issue #1835). Recorded even
        // when a later action fails: the files already written are ours, and a
        // later `destroy` still has to recognise them.
        let mut written: Vec<(PathBuf, String)> = Vec::new();
        let result = self.write_actions(&mut written);
        self.record_provenance(written);
        result
    }

    /// Write every action, collecting `(path, digest)` for each file this plan
    /// owns. Split out of [`Self::execute`] so provenance is recorded on the
    /// error path too.
    fn write_actions(&self, written: &mut Vec<(PathBuf, String)>) -> Result<(), GenerateError> {
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
                    // A `Modify` target is shared — other resources and the
                    // developer write to it too — so this plan never owns it.
                    if matches!(action, Action::Create { .. }) {
                        written.push((path.to_path_buf(), provenance::text_digest(contents)));
                    }
                    println!("  {label} {}", relative_display(path, &self.project_root));
                }
                Action::CreateBytes { bytes, .. } => {
                    fs::write(path, bytes)?;
                    written.push((path.to_path_buf(), provenance::bytes_digest(bytes)));
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
                            written.push((path.to_path_buf(), provenance::text_digest(contents)));
                            println!("  Created {}", relative_display(path, &self.project_root));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            // Another process already created the file; leave it
                            // untouched — and unrecorded: this run did not write it.
                        }
                        Err(e) => return Err(GenerateError::Io(e)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Record what this run wrote, so a later `destroy` can tell its own
    /// output from a developer's edit even after the template changed
    /// (issue #1835).
    ///
    /// Best effort: a manifest that cannot be written leaves `destroy`
    /// comparing against the current render alone — the behaviour before this
    /// existed — so it warns rather than failing a generator run that has
    /// already written every file it promised.
    fn record_provenance(&self, written: Vec<(PathBuf, String)>) {
        if written.is_empty() {
            return;
        }
        let mut recorded = provenance::Provenance::load(&self.project_root);
        for (path, digest) in written {
            recorded.record(&self.project_root, &path, digest, &self.invocation);
        }
        if let Err(e) = recorded.save(&self.project_root) {
            eprintln!(
                "Warning: could not record generated-file provenance in {}: {e}. \
                 `autumn destroy` may need --force after a CLI upgrade.",
                provenance::MANIFEST_PATH
            );
        }
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
        // What was actually removed, so the manifest is pruned to match even
        // when a later removal fails part-way (issue #1835).
        let mut removed = Removed::default();
        let result = self.apply_revert(&plan, &mut touched_dirs, &mut removed);

        for dir in touched_dirs {
            prune_empty_ancestors(dir, &self.project_root);
        }

        self.forget_provenance(&removed);
        result?;

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

    /// Delete and rewrite what `plan` calls for, recording what actually went.
    ///
    /// Split out of [`Self::revert`] so the manifest is pruned on the error
    /// path too: a run that removes three files and then fails on the fourth
    /// must not leave the manifest describing files that are already gone.
    fn apply_revert(
        &self,
        plan: &RevertPlan,
        touched_dirs: &mut Vec<PathBuf>,
        removed: &mut Removed,
    ) -> Result<(), GenerateError> {
        for path in &plan.files_to_remove {
            fs::remove_file(path)?;
            removed.files.push(path.clone());
            println!("  Removed {}", relative_display(path, &self.project_root));
            if let Some(parent) = path.parent() {
                touched_dirs.push(parent.to_path_buf());
            }
        }

        for dir in &plan.migrations_to_remove {
            fs::remove_dir_all(dir)?;
            removed.dirs.push(dir.clone());
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
        Ok(())
    }

    /// Drop the provenance entries for the files this revert removed, so the
    /// manifest does not outlive what it describes (issue #1835).
    ///
    /// Best effort, like [`Self::record_provenance`]: the files are already
    /// gone, and failing here would report a destroy that in fact succeeded.
    fn forget_provenance(&self, removed: &Removed) {
        if removed.files.is_empty() && removed.dirs.is_empty() {
            return;
        }
        let mut recorded = provenance::Provenance::load(&self.project_root);
        if recorded.is_empty() {
            return;
        }
        for path in &removed.files {
            recorded.forget(&self.project_root, path);
        }
        for dir in &removed.dirs {
            recorded.forget_dir(&self.project_root, dir);
        }
        if let Err(e) = recorded.save(&self.project_root) {
            eprintln!(
                "Warning: could not update {} after destroy: {e}",
                provenance::MANIFEST_PATH
            );
        }
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
        // What a matching `generate` recorded writing. A file matching either
        // the current render or this baseline is the generator's own output,
        // however far the template has moved since (issue #1835).
        let recorded = provenance::Provenance::load(&self.project_root);
        let baseline = Baseline {
            recorded: &recorded,
            invocation: &self.invocation,
        };

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
            let matches = is_generator_output(action, path, &self.project_root, baseline);
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
            let matches = is_generator_output(action, path, &self.project_root, baseline);
            if matches {
                files_to_remove.push(path.to_path_buf());
            } else {
                // Unlike an owned `Create`, a `CreateIfAbsent` target may have existed
                // before any generator ran — `generate` silently skips writing to it
                // either way — such as a hand-rolled
                // `templates/mailers/_layout.html`. Divergence here cannot be
                // attributed to this destroy's own edit gone stale the way it can for
                // an owned `Create`, so it is never a blocking error and never
                // force-deleted (#1048 PR review): guessing wrong would destroy real,
                // pre-existing project content this destroy never touched. Leave it and
                // say why.
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
                baseline,
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
        // Every path this destroy is itself rewriting — `src/schema.rs`, emptied by a
        // `SchemaTable` revert — must be excluded from
        // `crate_referenced_elsewhere_in_project`'s scan alongside `files_to_remove`.
        // Its pre-destroy content, still on disk at scan time since the real rewrite
        // happens later in `Plan::revert`'s apply phase, is not evidence of anything a
        // different resource still needs, because this same operation is about to change
        // it too. `scan_overrides` below supplies each such file's real final content
        // instead, so this exclusion only ever falls back to "treat as absent" for a file
        // that becomes empty, never for one that survives with other content.
        let mut cargo_deps_excluding = files_to_remove.clone();
        cargo_deps_excluding.extend(grouped_reverts.iter().map(|(path, _)| path.clone()));

        // Precompute the final, post-destroy content of every modified file other than
        // `Cargo.toml`, via each file's own self-contained reverts — none of
        // `SchemaTable`, `ModDecl`, and the rest consult another file's content.
        // `Cargo.toml`'s `CargoDeps`, `CargoAutumnWebFeature`, and
        // `CargoAutumnWebDevFeature` reverts are the only ones that do, through the
        // project-wide crate and feature usage scan, and they always target `Cargo.toml`
        // itself — so computing every other file's result first and excluding
        // `Cargo.toml` here avoids an ordering cycle. That is what lets the scan see what
        // `src/schema.rs`, `src/main.rs`, or a `mod.rs` will actually look like once this
        // destroy is done: a table this destroy is not touching, still present after its
        // own removal. The alternatives are worse — stale pre-destroy content would
        // wrongly count a table being removed as still needed, and hiding the file from
        // the scan entirely would wrongly hide unrelated content in the same file that
        // legitimately still needs the dependency (#1048 PR review).
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

/// What a real [`Plan::revert`] actually deleted — the plan minus whatever a
/// mid-run failure stopped it from reaching. The provenance manifest is pruned
/// against this, never against the plan (issue #1835).
#[derive(Default)]
struct Removed {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
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
    crate_reference_site(project_root, crate_name, excluding, overrides).is_some()
}

/// The first file under `project_root`'s `src/`, `tests/`, or `benches/` tree
/// that still names `crate_name`, or `None` when nothing does.
///
/// The site-returning half of [`crate_referenced_elsewhere_in_project`], with
/// the same best-effort caveats. `autumn plugin remove` (issue #1631) uses it
/// to decide whether stripping a plugin's dependency would break code the user
/// wrote by hand — and, when it would, to say which file to look at.
pub fn crate_reference_site(
    project_root: &Path,
    crate_name: &str,
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> Option<PathBuf> {
    let markers = crate_reference_markers(crate_name);
    // `examples` alongside the build targets: an example that uses the crate is
    // a target that stops compiling when the dependency goes (issue #1631
    // review). Erring toward retaining a dependency only ever costs an unused
    // manifest line; erring the other way breaks a build.
    ["src", "tests", "benches", "examples"]
        .iter()
        .find_map(|dir| {
            rs_tree_marker_site(&project_root.join(dir), &markers, excluding, overrides)
        })
}

/// The text markers that indicate `crate_name` is in use in Rust source.
///
/// One definition, because two callers scan different file sets with them —
/// [`crate_reference_site`]'s conventional trees and `plugin::remove`'s
/// explicit Cargo target paths — and a marker present in one set but not the
/// other is a dependency stripped from under code that still needs it.
///
/// Three spellings reach a crate by name:
///
/// - `{ident}::` — a qualified path.
/// - `use {ident}` — an import, including `use {ident} as alias;`, which this
///   prefix already covers.
/// - `extern crate {ident}` — the 2015-edition form, still legal in every
///   edition. It is the one spelling that carries NEITHER of the others when
///   aliased: `extern crate autumn_admin_plugin as admin;` followed by
///   `admin::…` contains no `{ident}::` and no `use {ident}` anywhere (issue
///   #1631 review).
///
/// These are substring probes, not a parse. Best-effort in both directions: a
/// crate reached only through a re-export, or a derive macro that never spells
/// the crate's own name, is not detected; and a crate whose name EXTENDS this
/// one (`autumn-admin-plugin-extras`) matches. The two errors are not
/// symmetric — over-retaining leaves an unused manifest line, under-retaining
/// strips a dependency out from under code that still compiles against it — so
/// the loose direction is the deliberate one.
#[must_use]
pub fn crate_reference_markers(crate_name: &str) -> Vec<String> {
    let ident = crate_name.replace('-', "_");
    vec![
        format!("{ident}::"),
        format!("use {ident}"),
        format!("extern crate {ident}"),
    ]
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
        // `maud` and `htmx` are in autumn-web's own `default = [...]` feature set (see
        // `autumn/Cargo.toml`), so a project's default-features dependency already
        // carries them regardless of any generator. Removing the explicit `features =
        // [...]` entry therefore never disables the capability, and a project-wide
        // marker check would wrongly treat autumn's own default-feature boilerplate —
        // `src/main.rs`'s stock `layout()`, which always calls `maud::html!` — as still
        // needed in every project. No check needed: fall through to the `owner_dir`
        // check alone.
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
        // Attachment scaffolds (`generate scaffold ... --attachment`) enable both
        // `multipart` and `storage`, but a hand-written route can use these APIs
        // directly. Destroying the last attachment model must not strip a feature such
        // a route still needs (PR #1867 review).
        //
        // Bare `Multipart`, not `extract::Multipart`, so the marker also catches a
        // hand-written route that pulls the extractor in through `use
        // autumn_web::prelude::*;` and names it unqualified. The prelude re-exports
        // `Multipart` (`autumn/src/prelude.rs`), so such a route contains neither
        // `extract::Multipart` nor any storage path, and the narrower marker would let
        // `destroy` wrongly strip the feature. This still matches the fully-qualified
        // `autumn_web::extract::Multipart` the scaffold emits and any `use
        // ...::Multipart;` import; the only extra hits are identifiers like
        // `MultipartField` and `MultipartError`, themselves part of the multipart API
        // surface, so over-retaining on them is harmless.
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
/// Whether an `autumn-web` `feature` is pinned by the app's own database
/// backend rather than by any generated resource (issue #1924).
///
/// `sqlite` is a whole-app backend flip: `autumn new` never writes it, no
/// generated code contains a marker naming it, and without it the app's
/// `sqlite://` URL is refused at boot with `UnsupportedBackend`. Destroying the
/// last model must not take it out from under a `SQLite` app, so the backend
/// decides here rather than the generic `owner_dir` rule.
fn autumn_web_feature_pinned_by_backend(feature: &str, project_root: &Path) -> bool {
    feature == "sqlite" && super::detect_backend(project_root) == DatabaseBackend::Sqlite
}

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
    rs_tree_marker_site(dir, markers, excluding, overrides).is_some()
}

/// The first `.rs` file under `dir` containing any of `markers`, or `None`.
///
/// The site-returning half of [`rs_tree_contains_marker`]: `autumn plugin
/// remove` (issue #1631) has to *name* the file that keeps a plugin
/// dependency alive, because "kept the dependency" without "because
/// `src/support.rs` still uses it" is an unactionable report.
fn rs_tree_marker_site(
    dir: &Path,
    markers: &[String],
    excluding: &[PathBuf],
    overrides: &HashMap<PathBuf, String>,
) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    // Sorted so the reported site is stable across runs and filesystems —
    // `read_dir` order is not specified, and a report that names a different
    // file on each run reads like a different finding.
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            if let Some(found) = rs_tree_marker_site(&path, markers, excluding, overrides) {
                return Some(found);
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
            return Some(path);
        }
    }
    None
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
/// Compares against the current render only, never a recorded digest: this
/// picks WHICH directory to delete, out of candidates found by scanning
/// `migrations/`, rather than gating a path the plan already named. The
/// manifest is project content, so honouring it here would let a crafted or
/// stale entry aim `remove_dir_all` at a hand-written migration. Candidates
/// that no longer match the current render stay `Ambiguous` — the pre-#1835
/// answer, and the safe one. Provenance still relaxes the per-file check once
/// one directory has been identified.
fn migration_dir_matches_actions(dir: &Path, actions: &[&Action]) -> bool {
    actions.iter().all(|action| {
        let Some(file_name) = action.path().file_name() else {
            return true;
        };
        let expected: &str = match action {
            Action::Create { contents, .. } | Action::CreateIfAbsent { contents, .. } => contents,
            Action::CreateBytes { .. } | Action::Modify { .. } => return true,
        };
        fs::read_to_string(dir.join(file_name)).is_ok_and(|actual| text_matches(&actual, expected))
    })
}

/// Whether the file at `path` is generator output rather than a developer's.
///
/// True when it matches what this plan would write now, and also when it
/// matches the digest `generate` recorded for it — the same file, written by a
/// CLI whose template has since moved on (issue #1835). A developer's edit
/// matches neither, and stays protected.
///
/// The recorded digest is keyed by PATH, not by which plan wrote it, so a file
/// one generator owns also reads as generator output to another plan naming
/// the same path. That is deliberate: the caller has already decided the path
/// belongs to the resource being destroyed, and the question left here is only
/// whether a human has since changed the file.
///
/// A `Modify` target is shared, never owned by one plan; every caller filters
/// those out first, and the arm below is the belt-and-braces answer.
fn is_generator_output(
    action: &Action,
    path: &Path,
    project_root: &Path,
    baseline: Baseline<'_>,
) -> bool {
    let (on_disk_digest, planned) = match action {
        Action::Create { contents, .. } | Action::CreateIfAbsent { contents, .. } => {
            let Ok(on_disk) = fs::read_to_string(path) else {
                return false;
            };
            if text_matches(&on_disk, contents) {
                return true;
            }
            (provenance::text_digest(&on_disk), Claim::of(action))
        }
        Action::CreateBytes { bytes, .. } => {
            let Ok(on_disk) = fs::read(path) else {
                return false;
            };
            if &on_disk == bytes {
                return true;
            }
            (provenance::bytes_digest(&on_disk), Claim::of(action))
        }
        Action::Modify { .. } => return false,
    };
    baseline.accepts(planned, project_root, path, &on_disk_digest)
}

/// What a revert compares an on-disk file against: the digests `generate`
/// recorded, and the command allowed to claim them.
#[derive(Clone, Copy)]
struct Baseline<'a> {
    recorded: &'a provenance::Provenance,
    invocation: &'a str,
}

impl Baseline<'_> {
    /// Whether `claim` holds for `digest` at `path`.
    fn accepts(self, claim: Claim, project_root: &Path, path: &Path, digest: &str) -> bool {
        match claim {
            Claim::ByThisCommand => {
                self.recorded
                    .is_ours(project_root, path, digest, self.invocation)
            }
            Claim::ByAnyCommand => self.recorded.was_written(project_root, path, digest),
        }
    }
}

/// Who has to have written a file for its recorded digest to count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// The command doing the destroying — the rule for a file it owns
    /// outright, so one command cannot delete another's output.
    ByThisCommand,
    /// Any command — the rule for a file several resources share, where only
    /// the first writer was ever recorded, so requiring the destroying command
    /// to BE that writer would strand the file the moment a template change
    /// stopped the content compare from matching. Every path that reaches this
    /// establishes first that no sibling still needs the file: the
    /// `CreateIfAbsent` pass checks the directory, and the shared
    /// `mail_unsubscribes` migration has its own still-needed-elsewhere guard
    /// after the content check.
    ByAnyCommand,
}

impl Claim {
    /// A shared file is written once for all its consumers; anything else
    /// belongs to the one command that wrote it.
    const fn of(action: &Action) -> Self {
        match action {
            Action::CreateIfAbsent { .. } => Self::ByAnyCommand,
            Action::Create { .. } | Action::CreateBytes { .. } | Action::Modify { .. } => {
                Self::ByThisCommand
            }
        }
    }
}

/// Whether two texts are the same file, ignoring line endings.
///
/// `git config core.autocrlf true` rewrites every text file on checkout, and
/// that is a checkout artifact rather than an edit — the same reason the
/// recorded digest is taken over LF-normalised text. Comparing raw here would
/// make the tolerance depend on a manifest entry surviving.
fn text_matches(on_disk: &str, expected: &str) -> bool {
    on_disk == expected || provenance::text_digest(on_disk) == provenance::text_digest(expected)
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
    baseline: Baseline<'_>,
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
        // A byte asset inside a migration directory was never content-checked
        // here, and still is not — the unplanned-file sweep below is what
        // guards this directory. No generator emits one today.
        if matches!(action, Action::CreateBytes { .. } | Action::Modify { .. }) {
            continue;
        }
        // The on-disk directory carries the timestamp `generate` used, not the
        // fresh one this recomputed plan holds, so compare against the real
        // file — under its own recorded digest too (issue #1835).
        let real_file = real_dir.join(file_name);
        if !is_generator_output(action, &real_file, project_root, baseline) && !force {
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
    // `autumn generate webhook` (issue #1366) — directory-backed like the
    // entries above, so `shared_module_backing_path_exists` treats it as
    // orphaned once `src/webhooks/` is gone.
    "webhooks",
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

pub fn relative_display(path: &Path, root: &Path) -> String {
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

    /// The `common.*` keys a scan of `root` still finds referenced.
    fn surviving_common_keys(root: &Path) -> HashSet<String> {
        scan_surviving_sources(root, &[], &HashMap::new())
            .referenced
            .into_iter()
            .filter(|key| key.starts_with("common."))
            .collect()
    }

    /// `common.*` keys the project scanner finds in one source string.
    fn common_keys_in(src: &str) -> Vec<String> {
        let mut scan = crate::i18n::ScanResult::default();
        crate::i18n::scan_source(src, "test.rs", &mut scan);
        scan.referenced
            .into_iter()
            .filter(|k| k.starts_with("common."))
            .collect()
    }

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

    /// A key built at runtime reaches definitions no static scan can name.
    /// Pruning them leaves a missing-key marker in a surviving view — and
    /// `autumn i18n check` deliberately does NOT report such keys as unused, so
    /// destroy must not delete what the checker declines to complain about.
    #[test]
    fn chrome_reachable_only_through_a_dynamic_key_survives() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("nav.rs"),
            "fn nav(locale: &Locale, action: &str) -> String {\n                 locale.t(&format!(\"common.{action}\"))\n}\n",
        )
        .unwrap();

        let ftl = concat!(
            "# Shared chrome — reused by every scaffolded resource.\n",
            "common.save = Save\n",
            "common.back = Back\n",
            "# — end shared chrome —\n",
            "\n",
            "# Post — generated by `autumn generate scaffold --i18n`.\n",
            "post.new = New Post\n",
            "# — end Post —\n",
        );
        let out = remove_i18n_keys(ftl, "Post", "post", root, &[], &HashMap::new());
        assert!(
            out.contains("common.save") && out.contains("common.back"),
            "a `common.{{action}}` lookup can reach both:\n{out}"
        );
        // The resource's own block still goes.
        assert!(!out.contains("post.new"), "{out}");
    }

    /// `t!` compiles in `tests/` and `examples/` too, and `autumn i18n check`
    /// reads the whole project. A scan rooted at `src/` calls such a key unused
    /// and prunes it, breaking that target's build.
    #[test]
    fn chrome_survives_when_only_a_non_src_target_uses_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/routes")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::create_dir_all(root.join("examples")).unwrap();
        std::fs::write(
            root.join("tests").join("views.rs"),
            "fn t() { let _ = t!(locale, \"common.save\"); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("examples").join("demo.rs"),
            "fn main() { let _ = locale.t(\"common.back\"); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/routes").join("posts.rs"),
            "fn show() { let _ = t!(locale, \"post.new\"); }\n",
        )
        .unwrap();

        let found = surviving_common_keys(root);
        assert!(found.contains("common.save"), "tests/ counts: {found:?}");
        assert!(found.contains("common.back"), "examples/ counts: {found:?}");
    }

    /// The forms a surviving `common.*` lookup can take.
    ///
    /// Every row here was once a bug: the scan this replaced was a hand-rolled
    /// text search, extended a round at a time as reviews found the next token
    /// class it could not read. Parsing now goes through the same `syn` scanner
    /// as `autumn i18n check`, so these are no longer separate features to
    /// implement — but they stay as the regression contract, because pruning a
    /// key one of them still references breaks the user's build at COMPILE
    /// time, and that is the failure this whole set exists to prevent.
    #[test]
    fn chrome_scan_reads_every_supported_lookup_form() {
        let cases: &[(&str, &str)] = &[
            ("plain", r#"let a = t!(locale, "common.plain");"#),
            (
                "rustfmt-wrapped",
                "let b = t!(\n    locale,\n    \"common.wrapped\",\n    n = &c.to_string()\n);",
            ),
            (
                "comment before key",
                r#"let c = t!(locale, /* shared */ "common.commented");"#,
            ),
            ("raw string key", r#"let d = t!(locale, r"common.raw");"#),
            (
                "hashed raw key",
                r##"let e = t!(locale, r#"common.hashed"#);"##,
            ),
            (
                "other locale binding",
                r#"let f = t!(lang, "common.lang");"#,
            ),
            ("field locale", r#"let g = t!(ctx.locale, "common.field");"#),
            ("spaced call", r#"let h = t! (locale, "common.spaced");"#),
            // Not a macro at all — the `Locale` methods are equally supported,
            // and were invisible to the text scanner.
            ("method call", r#"let i = locale.t("common.method");"#),
            (
                "method with args",
                r#"let j = locale.t_with("common.method_args", &[]);"#,
            ),
            (
                "associated call",
                r#"let k = Locale::t(&locale, "common.assoc");"#,
            ),
            // Inside `html!`, where view translations actually live.
            (
                "inside a macro body",
                r#"let l = html! { span { (t!(locale, "common.nested")) } };"#,
            ),
        ];
        for (what, src) in cases {
            let found = common_keys_in(src);
            assert!(!found.is_empty(), "no key found for the {what} form: {src}");
        }
    }

    /// Not every `t` is a translation, and a key that is not code is not a
    /// reference. Over-counting only strands an unused key; both directions are
    /// still worth pinning.
    #[test]
    fn chrome_scan_ignores_non_lookups() {
        for src in [
            r#"let a = "common.just-a-string";"#,
            r#"// t!(locale, "common.commented-out")"#,
            r#"/* t!(locale, "common.blocked") */"#,
            // A bare `t(...)` is not the `Locale` API.
            r#"let b = t("common.bare-fn");"#,
        ] {
            assert!(
                common_keys_in(src).is_empty(),
                "unexpectedly counted a reference in: {src}"
            );
        }
    }

    /// Chrome keys are ordinary keys once written, and application code outside
    /// `src/routes` may well reuse them. Pruning one that a surviving
    /// `src/components/…` still calls breaks the build, because `t!` validates
    /// key existence at compile time.
    #[test]
    fn chrome_survives_when_only_non_route_source_still_uses_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("routes")).unwrap();
        std::fs::create_dir_all(src.join("components")).unwrap();
        // The only surviving caller of `common.save` lives outside `routes/`.
        std::fs::write(
            src.join("components").join("navigation.rs"),
            "fn nav(locale: &Locale) -> Markup { html! { (t!(locale, \"common.save\")) } }\n",
        )
        .unwrap();
        std::fs::write(
            src.join("routes").join("posts.rs"),
            "fn show(locale: &Locale) -> Markup { html! { (t!(locale, \"post.new\")) } }\n",
        )
        .unwrap();

        let found = surviving_common_keys(&src);
        assert!(
            found.contains("common.save"),
            "a non-route caller keeps the key alive: {found:?}"
        );
    }

    /// The same protection the chrome set gives `common.*`, for the resource's
    /// own keys. A nav link or dashboard card outside the generated module can
    /// render `t!(locale, "post.index.title")`; destroying the marked `post.*`
    /// block out from under it breaks the build, because `t!` validates key
    /// existence at COMPILE time.
    #[test]
    fn a_resource_key_a_surviving_source_still_uses_is_not_destroyed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("components")).unwrap();
        std::fs::write(
            src.join("components").join("nav.rs"),
            "fn nav(locale: &Locale) -> Markup { html! { (t!(locale, \"post.index.title\")) } }\n",
        )
        .unwrap();

        let ftl = super::super::scaffold_i18n::merge_en_ftl_keeping(
            "",
            "Post",
            "post",
            &[
                ("post.index.title", "Posts"),
                ("post.new.title", "New Post"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
            &HashSet::new(),
        );

        let out = remove_i18n_keys(&ftl, "Post", "post", &src, &[], &HashMap::new());
        assert!(
            out.contains("post.index.title = Posts"),
            "the surviving call site keeps its key:\n{out}"
        );
        assert!(
            !out.contains("post.new.title"),
            "keys nothing references still go:\n{out}"
        );
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

    // ---- issue #1835: provenance-tolerant revert -------------------------
    //
    // `destroy` run by a NEWER CLI recomputes the plan from the CURRENT
    // template, so any generator whose template changed since the project was
    // generated reported `Diverged` on untouched files. These tests pin the
    // provenance manifest that tells the two cases apart.

    /// The plan a newer CLI would recompute: same path, newer template text.
    fn newer_template_plan(tmp: &tempfile::TempDir, path: &Path, contents: &str) -> Plan {
        let mut plan = Plan::new(tmp.path());
        plan.create(path.to_path_buf(), contents);
        plan
    }

    #[test]
    fn revert_removes_untouched_file_whose_template_changed_since_generation() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("src/routes/auth.rs");
        plan.create(target.clone(), "// v1 template\n");
        plan.execute(Flags::default()).unwrap();

        // Newer CLI: the renderer now emits different text for the same file.
        let newer = newer_template_plan(&tmp, &target, "// v2 template\n");
        newer.revert(Flags::default()).unwrap();

        assert!(!target.exists(), "untouched generated file must be removed");
    }

    #[test]
    fn revert_still_refuses_a_hand_edited_file_whose_template_changed() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("src/routes/auth.rs");
        plan.create(target.clone(), "// v1 template\n");
        plan.execute(Flags::default()).unwrap();
        fs::write(&target, "// hand-edited by user\n").unwrap();

        let newer = newer_template_plan(&tmp, &target, "// v2 template\n");
        let err = newer.revert(Flags::default()).unwrap_err();

        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(target.exists(), "a real edit must survive");
    }

    #[test]
    fn execute_records_a_provenance_digest_for_every_owned_file() {
        let (tmp, mut plan) = fixture();
        plan.create(tmp.path().join("src/models/post.rs"), "// model\n");
        plan.create_bytes(tmp.path().join("static/logo.png"), vec![1, 2, 3]);
        plan.create_if_absent(tmp.path().join("templates/_layout.html"), "<html>\n");
        plan.modify(tmp.path().join("src/main.rs"), "fn main() {}\n");
        plan.execute(Flags::default()).unwrap();

        let recorded = provenance::Provenance::load(tmp.path());
        assert!(recorded.contains("src/models/post.rs"));
        assert!(recorded.contains("static/logo.png"));
        assert!(recorded.contains("templates/_layout.html"));
        assert!(
            !recorded.contains("src/main.rs"),
            "a Modify target is shared, never owned by one plan"
        );
    }

    #[test]
    fn dry_run_execute_records_no_provenance() {
        let (tmp, mut plan) = fixture();
        plan.create(tmp.path().join("out.txt"), "hello");
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(!tmp.path().join(provenance::MANIFEST_PATH).exists());
    }

    #[test]
    fn dry_run_revert_records_no_provenance_change() {
        let (tmp, mut plan) = fixture();
        plan.create(tmp.path().join("out.txt"), "hello");
        plan.execute(Flags::default()).unwrap();

        plan.revert(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();

        assert!(
            provenance::Provenance::load(tmp.path()).contains("out.txt"),
            "a dry run must leave the manifest alone"
        );
    }

    #[test]
    fn revert_prunes_the_provenance_entries_it_removed() {
        let (tmp, mut plan) = fixture();
        plan.create(tmp.path().join("out.txt"), "hello");
        plan.execute(Flags::default()).unwrap();
        plan.revert(Flags::default()).unwrap();

        assert!(!provenance::Provenance::load(tmp.path()).contains("out.txt"));
        assert!(
            !tmp.path().join(provenance::MANIFEST_PATH).exists(),
            "an emptied manifest is removed, not left as a stub"
        );
    }

    #[test]
    fn a_skipped_create_if_absent_is_not_recorded_as_ours() {
        let (tmp, mut plan) = fixture();
        let shared = tmp.path().join("templates/_layout.html");
        fs::create_dir_all(shared.parent().unwrap()).unwrap();
        fs::write(&shared, "<html>hand-written</html>\n").unwrap();

        plan.create_if_absent(shared, "<html>generated</html>\n");
        plan.execute(Flags::default()).unwrap();

        assert!(
            !provenance::Provenance::load(tmp.path()).contains("templates/_layout.html"),
            "generate skipped the write, so it owns nothing"
        );
    }

    #[test]
    fn create_if_absent_matching_provenance_is_removed_after_a_template_change() {
        let (tmp, mut plan) = fixture();
        let shared = tmp.path().join("templates/_layout.html");
        plan.create_if_absent(shared.clone(), "<html>v1</html>\n");
        plan.execute(Flags::default()).unwrap();

        let mut newer = Plan::new(tmp.path());
        newer.create_if_absent(shared.clone(), "<html>v2</html>\n");
        newer.revert(Flags::default()).unwrap();

        assert!(!shared.exists());
    }

    #[test]
    fn create_bytes_matching_provenance_is_removed_after_a_template_change() {
        let (tmp, mut plan) = fixture();
        let asset = tmp.path().join("static/vendor.js");
        plan.create_bytes(asset.clone(), b"v1".to_vec());
        plan.execute(Flags::default()).unwrap();

        let mut newer = Plan::new(tmp.path());
        newer.create_bytes(asset.clone(), b"v2".to_vec());
        newer.revert(Flags::default()).unwrap();

        assert!(!asset.exists());
    }

    #[test]
    fn a_crlf_checkout_of_a_generated_file_is_not_divergence() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("src/models/post.rs");
        plan.create(target.clone(), "line one\nline two\n");
        plan.execute(Flags::default()).unwrap();
        fs::write(&target, "line one\r\nline two\r\n").unwrap();

        plan.revert(Flags::default()).unwrap();

        assert!(!target.exists(), "core.autocrlf is not a user edit");
    }

    #[test]
    fn a_project_without_provenance_still_refuses_a_changed_template() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("src/routes/auth.rs");
        plan.create(target.clone(), "// v1 template\n");
        plan.execute(Flags::default()).unwrap();
        // An app generated before the manifest existed has no baseline.
        fs::remove_file(tmp.path().join(provenance::MANIFEST_PATH)).unwrap();

        let newer = newer_template_plan(&tmp, &target, "// v2 template\n");
        let err = newer.revert(Flags::default()).unwrap_err();

        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(target.exists());
    }

    #[test]
    fn revert_removes_a_migration_whose_template_changed_since_generation() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            let dir = tmp.path().join("migrations/20260101000000_create_posts");
            plan.create(dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(dir.join("down.sql"), "DROP TABLE posts;\n");
            plan.execute(Flags::default()).unwrap();

            // Destroy recomputes with a fresh timestamp AND a newer template.
            let mut newer = Plan::new(tmp.path());
            let fresh = tmp.path().join("migrations/99999999999999_create_posts");
            newer.create(fresh.join("up.sql"), "CREATE TABLE posts (id BIGINT);\n");
            newer.create(fresh.join("down.sql"), "DROP TABLE posts;\n");
            newer.revert(Flags::default()).unwrap();

            assert!(!dir.exists());
        });
    }

    #[test]
    fn a_hand_edited_migration_whose_template_changed_is_still_refused() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            let dir = tmp.path().join("migrations/20260101000000_create_posts");
            plan.create(dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(dir.join("down.sql"), "DROP TABLE posts;\n");
            plan.execute(Flags::default()).unwrap();
            fs::write(dir.join("up.sql"), "CREATE TABLE posts (mine INT);\n").unwrap();

            let mut newer = Plan::new(tmp.path());
            let fresh = tmp.path().join("migrations/99999999999999_create_posts");
            newer.create(fresh.join("up.sql"), "CREATE TABLE posts (id BIGINT);\n");
            newer.create(fresh.join("down.sql"), "DROP TABLE posts;\n");
            let err = newer.revert(Flags::default()).unwrap_err();

            assert!(matches!(err, GenerateError::Diverged(_)));
            assert!(dir.exists());
        });
    }

    #[test]
    fn regenerating_after_a_template_change_refreshes_the_recorded_digest() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("src/routes/auth.rs");
        plan.create(target.clone(), "// v1 template\n");
        plan.execute(Flags::default()).unwrap();

        let mut newer = Plan::new(tmp.path());
        newer.create(target.clone(), "// v2 template\n");
        newer
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();
        newer.revert(Flags::default()).unwrap();

        assert!(!target.exists());
    }

    #[test]
    fn provenance_accumulates_across_generator_runs() {
        let (tmp, mut first) = fixture();
        first.invocation = "model\u{1f}Post".to_owned();
        first.create(tmp.path().join("src/models/post.rs"), "// post\n");
        first.execute(Flags::default()).unwrap();

        let mut second = Plan::new(tmp.path());
        second.invocation = "model\u{1f}Comment".to_owned();
        second.create(tmp.path().join("src/models/comment.rs"), "// comment\n");
        second.execute(Flags::default()).unwrap();

        let recorded = provenance::Provenance::load(tmp.path());
        assert!(
            recorded.contains("src/models/post.rs"),
            "a second generator must not drop the first's baseline"
        );
        assert!(recorded.contains("src/models/comment.rs"));
    }

    #[test]
    fn destroying_one_resource_keeps_another_resources_provenance() {
        let (tmp, mut first) = fixture();
        let post = tmp.path().join("src/models/post.rs");
        first.invocation = "model\u{1f}Post".to_owned();
        first.create(post.clone(), "// post\n");
        first.execute(Flags::default()).unwrap();

        let mut second = Plan::new(tmp.path());
        let comment = tmp.path().join("src/models/comment.rs");
        second.invocation = "model\u{1f}Comment".to_owned();
        second.create(comment.clone(), "// comment\n");
        second.execute(Flags::default()).unwrap();

        first.revert(Flags::default()).unwrap();

        assert!(!post.exists());
        assert!(comment.exists(), "a sibling resource survives");
        let recorded = provenance::Provenance::load(tmp.path());
        assert!(!recorded.contains("src/models/post.rs"));
        assert!(recorded.contains("src/models/comment.rs"));
    }

    #[test]
    fn the_same_command_owns_the_path_it_recorded() {
        // Within one command's arguments the digest is keyed by path, so a
        // recomputed plan whose render has moved on still deletes the file.
        let (tmp, mut generated) = fixture();
        let path = tmp.path().join("src/controllers/post.rs");
        generated.invocation = "controller\u{1f}Post\u{1f}index".to_owned();
        generated.create(path.clone(), "// v1 template\n");
        generated.execute(Flags::default()).unwrap();

        let mut destroy = Plan::new(tmp.path());
        destroy.invocation = "controller\u{1f}Post\u{1f}index".to_owned();
        destroy.create(path.clone(), "// v2 template\n");
        destroy.revert(Flags::default()).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn a_different_command_never_owns_a_recorded_path() {
        // `autumn destroy model Post` with the fields omitted renders nothing
        // that matches, and must not borrow the digest recorded for the full
        // command — the shared-file reverts are argument-derived and would
        // silently no-op, leaving a half-destroyed project.
        let (tmp, mut generated) = fixture();
        let path = tmp.path().join("src/models/post.rs");
        generated.invocation = "model\u{1f}Post\u{1f}title:String".to_owned();
        generated.create(path.clone(), "// v1 template\n");
        generated.execute(Flags::default()).unwrap();

        let mut destroy = Plan::new(tmp.path());
        destroy.invocation = "model\u{1f}Post".to_owned();
        destroy.create(path.clone(), "// v2 template\n");
        let err = destroy.revert(Flags::default()).unwrap_err();

        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(path.exists());
    }

    #[test]
    fn a_starter_scaffold_never_owns_a_generators_output() {
        // `autumn new --starter` writes its files through this same engine.
        // Its entries are recorded under its own command, so a later
        // `destroy` of a generator that renders the same path cannot claim
        // the starter's hand-written file.
        let (tmp, mut starter) = fixture();
        let routes = tmp.path().join("src/routes/auth.rs");
        starter.invocation = "new\u{1f}--starter\u{1f}saas\u{1f}app".to_owned();
        starter.create(routes.clone(), "// the starter's own auth routes\n");
        starter.execute(Flags::default()).unwrap();

        let mut destroy = Plan::new(tmp.path());
        destroy.invocation = "auth\u{1f}User".to_owned();
        destroy.create(routes.clone(), "// what generate auth renders\n");
        let err = destroy.revert(Flags::default()).unwrap_err();

        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(
            routes.exists(),
            "a starter file is never a generator's to delete"
        );
    }

    #[test]
    fn execute_records_provenance_even_when_a_later_action_fails() {
        let (tmp, mut plan) = fixture();
        let good = tmp.path().join("src/models/post.rs");
        // A directory cannot be overwritten by a file write.
        let blocked = tmp.path().join("src/models/blocked.rs");
        fs::create_dir_all(&blocked).unwrap();

        plan.create(good.clone(), "// post\n");
        plan.create(blocked, "// never lands\n");
        plan.execute(Flags {
            dry_run: false,
            force: true,
        })
        .unwrap_err();

        assert!(good.exists());
        assert!(
            provenance::Provenance::load(tmp.path()).contains("src/models/post.rs"),
            "a file already written is still ours"
        );
    }

    #[test]
    fn revert_prunes_provenance_for_what_it_removed_before_failing() {
        let (tmp, mut plan) = fixture();
        let removable = tmp.path().join("a.rs");
        let blocked = tmp.path().join("z.rs");
        plan.create(removable.clone(), "// a\n");
        plan.create(blocked.clone(), "// z\n");
        plan.execute(Flags::default()).unwrap();

        // Replace the second target with a directory: `remove_file` fails on it.
        fs::remove_file(&blocked).unwrap();
        fs::create_dir_all(&blocked).unwrap();

        plan.revert(Flags {
            dry_run: false,
            force: true,
        })
        .unwrap_err();

        assert!(!removable.exists());
        assert!(
            !provenance::Provenance::load(tmp.path()).contains("a.rs"),
            "an entry must never outlive the file it describes"
        );
    }

    #[test]
    fn destroying_a_migration_prunes_its_provenance_entries() {
        no_db_env(|| {
            let (tmp, mut plan) = fixture();
            let dir = tmp.path().join("migrations/20260101000000_create_posts");
            plan.create(dir.join("up.sql"), "CREATE TABLE posts ();\n");
            plan.create(dir.join("down.sql"), "DROP TABLE posts;\n");
            plan.execute(Flags::default()).unwrap();
            assert!(
                provenance::Provenance::load(tmp.path())
                    .contains("migrations/20260101000000_create_posts/up.sql")
            );

            let mut newer = Plan::new(tmp.path());
            let fresh = tmp.path().join("migrations/99999999999999_create_posts");
            newer.create(fresh.join("up.sql"), "CREATE TABLE posts ();\n");
            newer.create(fresh.join("down.sql"), "DROP TABLE posts;\n");
            newer.revert(Flags::default()).unwrap();

            let recorded = provenance::Provenance::load(tmp.path());
            assert!(!recorded.contains("migrations/20260101000000_create_posts/up.sql"));
            assert!(!recorded.contains("migrations/20260101000000_create_posts/down.sql"));
        });
    }

    #[test]
    fn same_suffix_migrations_are_disambiguated_by_the_current_render_alone() {
        // Two directories share a suffix. One still matches what the plan
        // renders; the other only matches its own recorded digest. Selection
        // must follow the render, so manifest content can never aim
        // `remove_dir_all` at the wrong directory.
        no_db_env(|| {
            let (tmp, mut older) = fixture();
            let older_dir = tmp.path().join("migrations/20260101000000_create_posts");
            older.create(older_dir.join("up.sql"), "CREATE TABLE posts (old INT);\n");
            older.create(older_dir.join("down.sql"), "DROP TABLE posts;\n");
            older.execute(Flags::default()).unwrap();

            let mut current = Plan::new(tmp.path());
            let current_dir = tmp.path().join("migrations/20260202000000_create_posts");
            current.create(current_dir.join("up.sql"), "CREATE TABLE posts ();\n");
            current.create(current_dir.join("down.sql"), "DROP TABLE posts;\n");
            current.execute(Flags::default()).unwrap();

            let mut destroy = Plan::new(tmp.path());
            let fresh = tmp.path().join("migrations/99999999999999_create_posts");
            destroy.create(fresh.join("up.sql"), "CREATE TABLE posts ();\n");
            destroy.create(fresh.join("down.sql"), "DROP TABLE posts;\n");
            destroy.revert(Flags::default()).unwrap();

            assert!(!current_dir.exists(), "the matching directory is removed");
            assert!(older_dir.exists(), "the other one is never guessed at");
        });
    }

    #[test]
    fn a_crlf_rewritten_binary_asset_is_still_divergence() {
        let (tmp, mut plan) = fixture();
        let asset = tmp.path().join("static/vendor.bin");
        plan.create_bytes(asset.clone(), b"a\nb".to_vec());
        plan.execute(Flags::default()).unwrap();
        fs::write(&asset, b"a\r\nb").unwrap();

        let err = plan.revert(Flags::default()).unwrap_err();

        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(asset.exists(), "bytes are never LF-normalised");
    }

    #[test]
    fn a_modify_only_plan_records_nothing() {
        let (tmp, mut plan) = fixture();
        plan.modify(tmp.path().join("src/main.rs"), "fn main() {}\n");
        plan.execute(Flags::default()).unwrap();

        assert!(!tmp.path().join(provenance::MANIFEST_PATH).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_generator_run_still_writes_its_files_when_provenance_cannot_be_recorded() {
        let (tmp, mut plan) = fixture();
        let outside = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".autumn")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("escaped.toml"),
            tmp.path().join(provenance::MANIFEST_PATH),
        )
        .unwrap();

        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags::default())
            .expect("recording is best effort, never fatal to a generator run");

        assert!(target.exists());
        assert!(!outside.path().join("escaped.toml").exists());
    }

    #[test]
    fn editing_the_generator_config_drops_the_baseline() {
        // `autumn.generate.toml` never appears in the arguments, so an edited
        // recipe would otherwise leave a textually identical `destroy` looking
        // like the same inputs while the plan is rebuilt from different fields
        // (Codex review of PR #2551).
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tmp.path().join("autumn.generate.toml");
        fs::write(&config, "[scaffold.Post]\nfields = [\"title:String\"]\n").unwrap();
        // Built after the config exists: the identity is captured when the
        // plan is, as it is in a real run.
        let mut generated = Plan::new(tmp.path());
        let target = tmp.path().join("src/models/post.rs");
        generated.create(target.clone(), "// built from the first recipe\n");
        generated.execute(Flags::default()).unwrap();

        fs::write(&config, "[scaffold.Post]\nfields = [\"body:Text\"]\n").unwrap();
        let mut destroy = Plan::new(tmp.path());
        destroy.create(target.clone(), "// built from the second recipe\n");
        let err = destroy.revert(Flags::default()).unwrap_err();

        assert!(matches!(err, GenerateError::Diverged(_)));
        assert!(target.exists());
    }

    #[test]
    fn an_unchanged_generator_config_keeps_the_baseline() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(
            tmp.path().join("autumn.generate.toml"),
            "[scaffold.Post]\nfields = [\"title:String\"]\n",
        )
        .unwrap();
        let mut generated = Plan::new(tmp.path());
        let target = tmp.path().join("src/models/post.rs");
        generated.create(target.clone(), "// v1 template\n");
        generated.execute(Flags::default()).unwrap();

        let mut destroy = Plan::new(tmp.path());
        destroy.create(target.clone(), "// v2 template\n");
        destroy.revert(Flags::default()).unwrap();

        assert!(!target.exists());
    }

    #[test]
    fn the_last_consumer_of_a_shared_file_can_still_remove_it() {
        // Only the FIRST writer of a `CreateIfAbsent` file is recorded. The
        // sibling that destroys last has a different command, so requiring it
        // to be that writer would strand the layout the moment a template
        // change stopped the content compare from matching (Codex review of
        // PR #2551).
        let (tmp, mut first) = fixture();
        let shared = tmp.path().join("templates/mailers/_layout.html");
        first.invocation = "mailer\u{1f}Welcome".to_owned();
        first.create_if_absent(shared.clone(), "<html>v1</html>\n");
        first.execute(Flags::default()).unwrap();

        // The second mailer's own run skips the write — the file is there.
        let mut second = Plan::new(tmp.path());
        second.invocation = "mailer\u{1f}Receipt".to_owned();
        second.create_if_absent(shared.clone(), "<html>v2</html>\n");
        second.execute(Flags::default()).unwrap();

        // Destroying the last consumer, under a newer template.
        let mut destroy = Plan::new(tmp.path());
        destroy.invocation = "mailer\u{1f}Receipt".to_owned();
        destroy.create_if_absent(shared.clone(), "<html>v2</html>\n");
        destroy.revert(Flags::default()).unwrap();

        assert!(
            !shared.exists(),
            "the last consumer takes the layout with it"
        );
    }

    #[test]
    fn a_shared_file_still_survives_while_a_sibling_needs_it() {
        let (tmp, mut first) = fixture();
        let shared = tmp.path().join("templates/mailers/_layout.html");
        let sibling = tmp.path().join("templates/mailers/receipt.html");
        first.invocation = "mailer\u{1f}Welcome".to_owned();
        first.create_if_absent(shared.clone(), "<html>v1</html>\n");
        first.execute(Flags::default()).unwrap();
        fs::write(&sibling, "<html>receipt</html>\n").unwrap();

        let mut destroy = Plan::new(tmp.path());
        destroy.invocation = "mailer\u{1f}Welcome".to_owned();
        destroy.create_if_absent(shared.clone(), "<html>v2</html>\n");
        destroy.revert(Flags::default()).unwrap();

        assert!(shared.exists(), "a sibling still renders through it");
    }

    #[test]
    fn a_hand_written_shared_file_is_still_never_removed() {
        // The relaxed claim only accepts a digest Autumn recorded. Content
        // nobody recorded stays put, with a warning, exactly as before.
        let (tmp, mut plan) = fixture();
        let shared = tmp.path().join("templates/mailers/_layout.html");
        fs::create_dir_all(shared.parent().unwrap()).unwrap();
        fs::write(&shared, "<html>hand-written</html>\n").unwrap();

        plan.create_if_absent(shared.clone(), "<html>generated</html>\n");
        plan.execute(Flags::default()).unwrap();
        plan.revert(Flags::default()).unwrap();

        assert!(
            shared.exists(),
            "content Autumn never wrote is never deleted"
        );
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
    /// Every spelling that reaches a crate by name is a marker; a dependency
    /// used only through an `extern crate … as` alias must not read as unused
    /// (issue #1631 review).
    #[test]
    fn crate_reference_markers_cover_every_spelling_that_names_a_crate() {
        let markers = crate_reference_markers("autumn-admin-plugin");
        let matches = |src: &str| markers.iter().any(|m| src.contains(m.as_str()));
        assert!(matches("let p = autumn_admin_plugin::AdminPlugin::new();"));
        assert!(matches("use autumn_admin_plugin::AdminPlugin;"));
        assert!(matches("use autumn_admin_plugin as admin;"));
        assert!(matches("extern crate autumn_admin_plugin as admin;"));
        assert!(matches("extern crate autumn_admin_plugin;"));
        assert!(!matches("let x = 1;"));
        assert!(!matches("use some_other_crate::Thing;"));
        // A crate whose name merely EXTENDS this one does match, and that is
        // deliberate: these are substring probes, and the two possible errors
        // are not symmetric. Over-retaining leaves an unused manifest line;
        // under-retaining strips a dependency out from under code that still
        // compiles against it. The tie goes to the build.
        assert!(matches("use autumn_admin_plugin_extras::Thing;"));
    }
}
