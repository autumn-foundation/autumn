//! `autumn generate teams` — scaffold team membership: organizations, roles
//! (`Owner`/`Admin`/`Member`), and email invitations (issue #1261).
//!
//! Unlike every other generator in this module, `teams` takes no
//! user-supplied name — it always emits the same fixed `Organization` /
//! `Membership` / `Invitation` set under `src/teams/`. It composes three
//! already-stable Autumn primitives rather than introducing a new
//! authorization mechanism:
//! - `#[repository(..., tenant_scoped)]` (issue #695) — every `Membership`/
//!   `Invitation` read and write is automatically filtered/stamped by the
//!   active organization.
//! - The session `"role"` key (issue #496) — the same key
//!   `#[secured("...")]`/`PolicyContext::has_role` already read.
//! - The Mail stack (`#[mailer]`) — the generated `InvitationMailer` sends
//!   the invite email.
//!
//! For the exact working design this generator templates from, see the
//! hand-built reference app at `examples/teams` (read-only ground truth —
//! this generator adapts it to a single-binary-crate generated app rather
//! than that example's own `lib.rs` + `main.rs` split, and drops its
//! `routes/auth.rs` + `users` table since a generated app already has its
//! own login/signup via `autumn generate auth`).
//!
//! Creates:
//! - `src/teams/mod.rs` — module doc (the composition-boundary contract) +
//!   `pub mod` declarations.
//! - `src/teams/schema.rs` — `organizations`/`memberships`/`invitations`
//!   Diesel `table!` blocks, deliberately not `joinable!`-coupled to the
//!   app's own `users` table.
//! - `src/teams/models.rs` — `Organization`/`Membership`/`Invitation`
//!   `#[model]` structs plus the small form structs.
//! - `src/teams/role.rs` — the closed `Role` enum, `require_role`, and
//!   `establish_org_session` (the session-writing half of the integration
//!   seam).
//! - `src/teams/repositories.rs` — `#[repository]` traits for all three
//!   models.
//! - `src/teams/mailers/{mod,invitation_mailer}.rs` — the invite email.
//! - `src/teams/routes/{mod,organizations,invitations,members}.rs` — route
//!   handlers, plus `provision_default_organization` (the signup half of the
//!   integration seam) and a small dependency-free `minimal_page` HTML
//!   wrapper (this module never assumes anything about the app's own
//!   `crate::layout`).
//! - `migrations/<timestamp>_create_teams/{up,down}.sql`.
//!
//! Modifies:
//! - `src/main.rs` — `mod teams;` declaration and the ten generated routes
//!   wired into `routes![...]`.
//! - `Cargo.toml` — ensures the `mail` and `maud` Cargo features are enabled
//!   on the `autumn-web` dependency (needed for `InvitationMailer`'s
//!   `#[mailer]` and the generated routes' `Markup`/`html!` respectively —
//!   `maud` matters even on non-`--api` projects, which already have it, but
//!   is required for `--api` projects, whose starter strips it), and ensures
//!   the generated code's own direct dependencies (`diesel`, `diesel-async`,
//!   `pq-sys`, `chrono`, `serde`, `serde_json`, `validator`, `maud`) are
//!   present. Requires the Postgres backend — see `sqlite_teams_unsupported_error`.
//!
//! Warns (does not auto-edit `autumn.toml` — too many possible existing
//! shapes/profiles to safely text-patch) that `[tenancy]` needs
//! `session_key = "organization_id"`.

use std::path::{Path, PathBuf};

use super::emit::{Plan, Revert};
use super::model::ensure_cargo_dependencies;
use super::schema_edit::{ensure_autumn_web_feature, update_main_rs};
use super::{GenerateError, ensure_project_root, read_or_empty};

/// Direct dependencies the generated `src/teams/*` code requires at compile
/// time — a bare `autumn new` project's `Cargo.toml` doesn't carry these by
/// default (they're only pulled in by other generators, e.g.
/// `autumn generate model`), so `teams` must ensure them itself rather than
/// silently relying on some other generator having already run.
///
/// Mirrors `model.rs`'s `MODEL_DEPS` (every `#[autumn_web::model(...)]`
/// struct's generated code needs the same base set, including `serde_json`,
/// which the macro expansion references even though no template field is
/// itself JSON), minus `uuid` (every id here is a bare `i64`, and `model.rs`
/// itself only adds `uuid` when a model actually has a UUID-typed field).
/// `validator` is added on top: `model.rs` only pulls it in when its
/// CLI-parsed field metadata says a model has validation rules, but that
/// metadata doesn't exist for this generator's fixed, hand-written
/// templates — `Organization.name`'s `#[validate(length(...))]` (see
/// `models.rs`) needs it unconditionally. `diesel_migrations` is omitted:
/// already in every project's template `Cargo.toml` via
/// `TEMPLATE_SHIPPED_CARGO_DEPS`.
const TEAMS_DEPS: &[(&str, &str)] = &[
    ("chrono", "{ version = \"0.4\", features = [\"serde\"] }"),
    (
        "diesel",
        "{ version = \"2\", features = [\"postgres\", \"chrono\"] }",
    ),
    (
        "diesel-async",
        "{ version = \"0.9\", features = [\"postgres\"] }",
    ),
    (
        "pq-sys",
        "{ version = \"0.7\", features = [\"bundled_without_openssl\"] }",
    ),
    ("serde", "{ version = \"1\", features = [\"derive\"] }"),
    ("serde_json", "\"1\""),
    (
        "validator",
        "{ version = \"0.20\", features = [\"derive\"] }",
    ),
    // `minimal_page`/`routes::organizations::*`/etc. all render `Markup`
    // via `html!`/`maud::DOCTYPE`. A default `autumn new` project already
    // has `maud` as a direct dependency and enables autumn-web's default
    // `maud` feature, but an `--api` project strips both (see
    // `new.rs`'s JSON-only starter) — matching `channel.rs`'s SSE
    // transport, which enables the same pair when its own generated views
    // need them.
    ("maud", "{ version = \"0.27\", features = [\"axum\"] }"),
];

/// Compute the file actions for `autumn generate teams`.
///
/// `timestamp` is the 14-digit migration-directory prefix (see
/// [`super::timestamp_now`]), threaded down from the caller so it stays
/// injectable/deterministic in tests, mirroring
/// [`super::migration::plan_migration_with_options`].
///
/// `for_destroy` mirrors [`super::policy::plan_policy`]'s parameter for
/// signature symmetry with the other generators' generate/destroy split, but
/// `teams` has no existence guard to skip on the destroy path (unlike
/// `policy`, which requires a target model) — the plan built here is
/// identical either way.
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] outside an Autumn project, or
/// [`GenerateError::Io`] if `src/main.rs` can't be read.
pub fn plan_teams(
    project_root: &Path,
    timestamp: &str,
    for_destroy: bool,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    // `teams`' migration and model/repository templates are fixed Postgres
    // DDL/Diesel code with no SQLite-aware rendering path (see
    // `sqlite_teams_unsupported_error`'s doc comment) — reject up front
    // rather than emit a migration `autumn migrate` would fail to apply.
    // Skipped on the destroy path so a project that somehow already has
    // `teams` generated (e.g. from before this backend check existed) can
    // still be cleaned up.
    if !for_destroy
        && super::detect_backend(project_root) == autumn_web::config::DatabaseBackend::Sqlite
    {
        return Err(super::sqlite_teams_unsupported_error());
    }

    let mut plan = Plan::new(project_root);

    // ── src/teams/* ──────────────────────────────────────────────────────
    let teams_dir = project_root.join("src").join("teams");
    plan.create(teams_dir.join("mod.rs"), render_mod_rs());
    plan.create(teams_dir.join("schema.rs"), render_schema_rs());
    plan.create(teams_dir.join("models.rs"), render_models_rs());
    plan.create(teams_dir.join("role.rs"), render_role_rs());
    plan.create(teams_dir.join("repositories.rs"), render_repositories_rs());
    plan.create(
        teams_dir.join("mailers").join("mod.rs"),
        render_mailers_mod_rs(),
    );
    plan.create(
        teams_dir.join("mailers").join("invitation_mailer.rs"),
        render_invitation_mailer_rs(),
    );
    plan.create(
        teams_dir.join("routes").join("mod.rs"),
        render_routes_mod_rs(),
    );
    plan.create(
        teams_dir.join("routes").join("organizations.rs"),
        render_routes_organizations_rs(),
    );
    plan.create(
        teams_dir.join("routes").join("invitations.rs"),
        render_routes_invitations_rs(),
    );
    plan.create(
        teams_dir.join("routes").join("members.rs"),
        render_routes_members_rs(),
    );

    // ── migrations/<timestamp>_create_teams ─────────────────────────────
    // `teams` is a singleton scaffold (fixed table set, no per-invocation
    // resource name): re-running — especially with `--force` to refresh
    // it — must not mint a SECOND `*_create_teams` directory, since two
    // `CREATE TABLE organizations/memberships/invitations` migrations would
    // fail the next `autumn migrate` on the duplicate tables (Codex review
    // finding; mirrors `notifications.rs`'s identical singleton-migration
    // fix from PR #2144 finding B). Reuse an existing `*_create_teams` dir
    // when one is present, minting a fresh timestamped dir only on a clean
    // project. `autumn destroy teams` matches by the same suffix regardless
    // (see `emit.rs`'s `resolve_migration_removal`), so this doesn't change
    // destroy behavior.
    let migration_dir = existing_teams_migration_dir(project_root).unwrap_or_else(|| {
        project_root
            .join("migrations")
            .join(format!("{timestamp}_create_teams"))
    });
    plan.create(migration_dir.join("up.sql"), MIGRATION_UP.to_owned());
    plan.create(migration_dir.join("down.sql"), MIGRATION_DOWN.to_owned());

    // ── src/main.rs: mod teams; + routes![...] ──────────────────────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|e| {
        GenerateError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", main_path.display()),
        ))
    })?;
    let route_entries = teams_route_entries();
    let updated_main = update_main_rs(&main_existing, &["teams"], &route_entries);
    plan.modify(main_path.clone(), updated_main);
    plan.push_revert(Revert::RoutesEntries {
        path: main_path,
        entries: route_entries,
    });

    plan_teams_cargo_toml(&mut plan, project_root, teams_dir);

    // ── [tenancy] config reminder ────────────────────────────────────────
    // Never auto-edited (too many possible existing shapes/profiles to
    // safely text-patch) — a warning is the safe, honest choice.
    plan.warn(
        "teams' Membership/Invitation repositories are tenant_scoped by the active \
         organization (issue #695). Make sure autumn.toml has:\n\
         \x20   [tenancy]\n\
         \x20   enabled = true\n\
         \x20   source = \"session\"\n\
         \x20   session_key = \"organization_id\"\n\
         if it isn't already configured — see docs/generate-teams.md. If you want \
         logged-out invitees to load the invite-accept page, add \"/invite\" (NOT \
         \"/invitations\") to [tenancy] public_paths — public_paths matches by path \
         prefix, so listing \"/invitations\" would also exempt the Admin-only \
         create/revoke/resend routes from tenant resolution. src/teams/routes/invitations.rs' \
         `caller_email_lookup` module hard-codes a `users` table name — if you generated \
         auth with a custom resource name (e.g. `autumn generate auth Account`, whose table \
         is `accounts`, not `users`), edit that module to match your actual auth table or \
         invitation acceptance will fail with a missing-relation database error.",
    );

    Ok(plan)
}

/// Ensures `Cargo.toml` has the `mail`/`maud` autumn-web features and the
/// generated code's direct dependencies (see [`TEAMS_DEPS`]), and pushes
/// their matching `autumn destroy` reverts.
fn plan_teams_cargo_toml(plan: &mut Plan, project_root: &Path, teams_dir: PathBuf) {
    // All edits land in a single `plan.modify()` call: `ensure_*` helpers
    // are pure string transforms over an in-memory `String`, not aware of
    // each other's pending edits, so chaining them here (rather than
    // separate `read_or_empty` + `plan.modify` round trips against the same
    // path) is what stops a later edit from being computed against
    // pre-earlier-edit disk content and clobbering it when `Plan::execute`
    // applies each `Action::Modify` in order (mirrors `channel.rs`'s
    // combined feature+deps Cargo.toml edit). "maud" is needed even though
    // a default `autumn new` project already has it: an `--api` project's
    // starter strips both the direct `maud` dependency and autumn-web's
    // `maud` feature (see `TEAMS_DEPS`'s doc comment on the `maud` entry).
    const AUTUMN_WEB_FEATURES: &[&str] = &["mail", "maud"];

    let cargo_path = project_root.join("Cargo.toml");
    let cargo_existing = read_or_empty(&cargo_path);
    let mut updated_cargo = cargo_existing.clone();
    for feature in AUTUMN_WEB_FEATURES {
        updated_cargo = ensure_autumn_web_feature(&updated_cargo, feature);
    }
    updated_cargo = ensure_cargo_dependencies(&updated_cargo, TEAMS_DEPS);
    if updated_cargo != cargo_existing {
        plan.modify(cargo_path.clone(), updated_cargo);
    }
    for feature in AUTUMN_WEB_FEATURES {
        plan.push_revert(Revert::CargoAutumnWebFeature {
            path: cargo_path.clone(),
            feature: (*feature).to_owned(),
            owner_dir: Some(teams_dir.clone()),
        });
    }
    // Pushed unconditionally — see `plan_cargo_deps`'s matching comment in
    // model.rs: destroy recomputes this plan against the already-generated
    // Cargo.toml, where these entries are by definition already present.
    //
    // Unlike `plan_cargo_deps`, this doesn't exclude every
    // `TEMPLATE_SHIPPED_CARGO_DEPS` name — only `autumn-web` (every Autumn
    // app unconditionally needs it) and `diesel_migrations` (every
    // `autumn new` template ships it regardless of `teams`). `maud` stays
    // revertible: a default project already has it (so `Revert::CargoDeps`'s
    // own whole-project reference check — see `emit.rs` — finds it still
    // used by the app's own `maud::`-qualified `src/main.rs` layout and
    // leaves it alone), but an `--api` project has no other use for it once
    // `teams` is destroyed, and *did* get it added solely by `teams` (see
    // `TEAMS_DEPS`'s doc comment on the `maud` entry) — excluding it the
    // same way `autumn-web` is excluded would wrongly treat it as always
    // pre-existing.
    let dep_names: Vec<String> = TEAMS_DEPS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| *name != "autumn-web" && *name != "diesel_migrations")
        .map(str::to_owned)
        .collect();
    if !dep_names.is_empty() {
        plan.push_revert(Revert::CargoDeps {
            path: cargo_path,
            names: dep_names,
            owner_dir: teams_dir,
        });
    }
}

/// The existing `migrations/<ts>_create_teams/` directory, if the project
/// already has one — so a re-run reuses it in place rather than minting a
/// second singleton migration. Matched by the same `_create_teams` suffix
/// destroy uses (see `emit::resolve_migration_removal`), so the two stay in
/// agreement. Mirrors `notifications.rs`'s `existing_notifications_migration_dir`.
///
/// If more than one somehow exists (a hand-added duplicate, or one left
/// over from before this reuse check existed), the lexicographically-smallest
/// is chosen deterministically — the same tie-break `emit`'s destroy scan
/// applies — rather than depending on `read_dir` order.
fn existing_teams_migration_dir(project_root: &Path) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(project_root.join("migrations"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("_create_teams"))
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// The ten generated route handlers, in the fully-qualified
/// `teams::routes::<module>::<fn>` form `routes![...]` needs (`mod teams;`
/// makes `teams` a top-level module from `src/main.rs`'s perspective, one
/// level deeper than `generate scaffold`'s bare `routes::{plural}::...` —
/// see `channel.rs`/`scaffold.rs` for the same convention at their own
/// nesting depth).
fn teams_route_entries() -> Vec<String> {
    vec![
        "teams::routes::organizations::create_organization".to_owned(),
        "teams::routes::organizations::switch_organization".to_owned(),
        "teams::routes::invitations::create_invitation".to_owned(),
        "teams::routes::invitations::show_invitation".to_owned(),
        "teams::routes::invitations::accept_invitation".to_owned(),
        "teams::routes::invitations::revoke_invitation".to_owned(),
        "teams::routes::invitations::resend_invitation".to_owned(),
        "teams::routes::members::list_members".to_owned(),
        "teams::routes::members::change_role".to_owned(),
        "teams::routes::members::remove_member".to_owned(),
    ]
}

// ── File renderers ──────────────────────────────────────────────────────
//
// Every rendered file is fixed content (no user-supplied name to
// interpolate — `teams` itself is the fixed name), so these are plain
// `&'static str` constants rather than the `format!`-built templates the
// per-resource generators (`mailer.rs`, `policy.rs`, ...) need.

fn render_mod_rs() -> String {
    MOD_RS.to_owned()
}

const MOD_RS: &str = r#"//! Team membership, roles, and email invitations (issue #1261).
//!
//! Generated by `autumn generate teams`. Composes three already-stable
//! Autumn primitives rather than introducing a new authorization mechanism:
//! - `#[repository(..., tenant_scoped)]` (issue #695) — every `Membership`/
//!   `Invitation` read and write is automatically filtered/stamped by the
//!   active organization.
//! - The session `"role"` key (issue #496) — the same key
//!   `#[secured("...")]`/`PolicyContext::has_role` already read, so
//!   [`role::require_role`] layers on top of existing session plumbing
//!   rather than adding a second one.
//! - The Mail stack (`#[mailer]`) — [`mailers::invitation_mailer::InvitationMailer`]
//!   sends the invite email.
//!
//! ## Composition boundary with your own auth
//!
//! This module intentionally does NOT generate a `routes/auth.rs` — your
//! app's login/signup already exists (e.g. via `autumn generate auth`), and
//! this module has no idea what your `User` model or `users` table look
//! like. `Membership::user_id` / `Invitation::invited_by_user_id` are bare
//! `i64` with no foreign-key coupling to your schema (see `schema.rs`'s doc
//! comment).
//!
//! Two lines of integration code are all that's needed to wire teams into
//! your existing auth flow (see `docs/generate-teams.md` for the full
//! walkthrough):
//!
//! 1. **At signup**, right after your handler creates the account row, call
//!    [`routes::organizations::provision_default_organization`] to create
//!    the new user's personal organization and make them its `Owner`. This
//!    only writes the organization/membership rows — it deliberately does
//!    NOT touch the session (see its own doc comment for why: signup and
//!    "authenticated" aren't the same event once email confirmation is
//!    involved, e.g. with `autumn generate auth`'s scaffolded `signup`):
//!
//!    ```ignore
//!    teams::routes::organizations::provision_default_organization(
//!        user.id, &mut db,
//!    ).await?;
//!    ```
//!
//!    This covers the organization + membership pair atomically, but not
//!    your own preceding user insert — if your signup handler doesn't wrap
//!    that insert in a transaction (like `autumn generate auth`'s
//!    scaffolded one), a failure here still leaves the account stranded
//!    with no organization. For full 3-way atomicity, wrap your own user
//!    insert in `db.tx(...)` and call
//!    [`routes::organizations::provision_default_organization_on_conn`] on
//!    the same `conn` instead — see `docs/generate-teams.md` for a worked
//!    example.
//!
//! 2. **At login**, after your handler authenticates the user, resolve their
//!    active organization/role and set it on the session — this is what
//!    actually establishes the org session, on the user's first real login:
//!
//!    ```ignore
//!    let memberships = membership_repo.across_tenants().find_by_user_id(user.id).await?;
//!    if let Some(active) = memberships.into_iter().min_by_key(|m| m.id) {
//!        let role = teams::role::Role::parse(&active.role).unwrap_or(teams::role::Role::Member);
//!        teams::role::establish_org_session(&session, user.id, &active.tenant_id, role).await;
//!    }
//!    ```
//!
//! 3. **At account deletion**, before (or as part of) your handler deletes
//!    the account row, call
//!    [`routes::organizations::remove_all_memberships`] — `Membership` has
//!    no FK to your `users` table (see above), so nothing else notices the
//!    account is gone. Skipping this step leaves a deleted user's team
//!    access live on any device with a still-valid session cookie, since
//!    [`role::require_role`] only re-checks the `Membership` row, not the
//!    account itself:
//!
//!    ```ignore
//!    teams::routes::organizations::remove_all_memberships(user.id, &mut db).await?;
//!    ```
//!
//! Gate any admin-only handler with
//! `teams::role::require_role(&session, &membership_repo, teams::role::Role::Admin).await?;`
//! (taking `membership_repo: teams::repositories::PgMembershipRepository` as
//! a handler parameter).
//!
//! See `examples/teams` in the Autumn repository for a fully-wired reference
//! application — it owns its whole app (including the `users` table), so it
//! inlines steps 1-2 above directly into its own `routes/auth.rs` rather than
//! exposing them as a separate seam, and step 3 is unnecessary there:
//! `examples/teams`'s own `memberships.user_id` is a real
//! `REFERENCES users (id) ON DELETE CASCADE` FK, since that app owns its
//! `users` table and can afford the coupling this generator can't.

pub mod mailers;
pub mod models;
pub mod repositories;
pub mod role;
pub mod routes;
pub mod schema;
"#;

fn render_schema_rs() -> String {
    SCHEMA_RS.to_owned()
}

const SCHEMA_RS: &str = r"//! Diesel table definitions for the `teams` module (issue #1261). These
//! mirror `migrations/<timestamp>_create_teams/`.
//!
//! Deliberately NOT `joinable!`/FK-coupled to your app's own `users` table —
//! its shape is unknown at generate time. `user_id` / `invited_by_user_id`
//! are bare `Int8`; see `models.rs`'s doc comment on `Membership::tenant_id`
//! for why `tenant_id` is `Text` rather than a typed FK to `organizations`
//! (the `tenant_scoped` repository macro's generated queries filter on a
//! column literally named `tenant_id`).

diesel::table! {
    organizations (id) {
        id -> Int8,
        name -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    memberships (id) {
        id -> Int8,
        tenant_id -> Text,
        user_id -> Int8,
        role -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    invitations (id) {
        id -> Int8,
        tenant_id -> Text,
        email -> Text,
        role -> Text,
        token_hash -> Text,
        status -> Text,
        invited_by_user_id -> Int8,
        expires_at -> Timestamp,
        created_at -> Timestamp,
    }
}
";

fn render_models_rs() -> String {
    MODELS_RS.to_owned()
}

const MODELS_RS: &str = r#"//! `Organization`, `Membership`, `Invitation` models and their form structs
//! (issue #1261).

use serde::Deserialize;

use crate::teams::schema::{invitations, memberships, organizations};

// ── Organization ─────────────────────────────────────────────────────────
//
// The tenant itself. Not tenant-scoped: creating one, and listing the ones a
// user belongs to, both necessarily run before/across any single active
// tenant.

/// An organization (tenant).
#[autumn_web::model(table = "organizations")]
pub struct Organization {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 200))]
    pub name: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// ── Membership ───────────────────────────────────────────────────────────
//
// The user <-> organization join row, carrying the closed `Role` (stored as
// `TEXT`, validated by `crate::teams::role::Role::parse` everywhere it's
// read). `tenant_scoped` fills `tenant_id` from the active-organization
// session context and filters every read by it — reusing #695, not a second
// isolation mechanism.

/// A user's role within a single organization.
///
/// `tenant_id` holds the owning `Organization.id` in its string form (the
/// `tenant_scoped` repository macro's generated queries filter on a column
/// literally named `tenant_id` — see `../../migrations/`); look the
/// `Organization` row itself up via `tenant_id.parse::<i64>()` when needed.
#[autumn_web::model(table = "memberships")]
pub struct Membership {
    #[id]
    pub id: i64,
    #[default]
    pub tenant_id: String,
    pub user_id: i64,
    pub role: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// ── Invitation ───────────────────────────────────────────────────────────

/// A pending (or resolved) email invitation into an organization. See
/// [`Membership`]'s doc comment for why `tenant_id` is a `String`.
#[autumn_web::model(table = "invitations")]
pub struct Invitation {
    #[id]
    pub id: i64,
    #[default]
    pub tenant_id: String,
    pub email: String,
    pub role: String,
    pub token_hash: String,
    // Not `#[default]`: revoking/resending/accepting need to write this
    // field through `UpdateInvitation`, and `#[default]` fields are excluded
    // from the generated `Update*` struct as well as `New*`. The DB column's
    // `DEFAULT 'pending'` is a redundant safety net; every insert site here
    // also sets it explicitly.
    pub status: String,
    pub invited_by_user_id: i64,
    pub expires_at: chrono::NaiveDateTime,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// ── Forms ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct NewOrganizationForm {
    pub name: String,
}

#[derive(Deserialize)]
pub struct InviteForm {
    pub email: String,
    pub role: String,
}

#[derive(Deserialize)]
pub struct ChangeRoleForm {
    pub role: String,
}
"#;

fn render_role_rs() -> String {
    ROLE_RS.to_owned()
}

const ROLE_RS: &str = r#"//! The closed membership-role enum, the `require_role` guard, and
//! `establish_org_session` — the session-writing half of the auth
//! integration seam (issue #1261; see `crate::teams`'s module doc for the
//! other half, `provision_default_organization`).
//!
//! `Membership.role` is stored as `TEXT` (with a SQL `CHECK` constraint as a
//! defense-in-depth backstop), but every place that *reads* a role goes
//! through [`Role::parse`] — an unrecognized string can never silently pass
//! an authorization check.
//!
//! [`require_role`] resolves the caller's role by looking up their live
//! `Membership` row in the active organization — it only uses the session to
//! find *which user* is asking (`"user_id"`, set by
//! [`establish_org_session`]), the same identity signal
//! `#[secured("...")]`/`PolicyContext::has_role` rely on (issue #496), so this
//! is a hierarchy-aware guard layered on top of the existing session/Policy
//! plumbing, not a second authorization mechanism (issue #1261 AC2). Roles are
//! never trusted from the session's cached `"role"` string: that value can go
//! stale the instant another request revokes or changes the caller's
//! membership, so every check re-reads the database.

use autumn_web::prelude::*;

use crate::teams::repositories::{MembershipRepository, PgMembershipRepository};

/// A membership role, ordered `Member < Admin < Owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Member,
    Admin,
    Owner,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            _ => None,
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Role::Member => 0,
            Role::Admin => 1,
            Role::Owner => 2,
        }
    }

    /// Whether this role satisfies a `required` role or higher in the
    /// hierarchy (e.g. an `Owner` satisfies `require_role(Role::Admin)`).
    #[must_use]
    pub const fn at_least(self, required: Role) -> bool {
        self.rank() >= required.rank()
    }
}

/// Require that the signed-in user's role in the active organization is
/// `required` or higher, e.g.
/// `require_role(&session, &membership_repo, Role::Admin).await?`.
///
/// Resolves `user_id` from the session (set by [`establish_org_session`]),
/// then re-reads that user's `Membership` row for the active organization
/// (`membership_repo` is `tenant_scoped`, so this is automatically filtered
/// to the caller's active tenant — see `repositories.rs`) rather than
/// trusting the session's cached `"role"` string, which can go stale as soon
/// as another request changes or revokes the caller's membership. Returns
/// the resolved role on success so callers that need it (e.g. to decide
/// whether to show owner-only controls) don't have to look it up twice.
///
/// # Errors
///
/// - `401 Unauthorized` when there's no signed-in user, or the signed-in user
///   has no membership in the active organization.
/// - `403 Forbidden` when the resolved role does not meet `required`.
pub async fn require_role(
    session: &Session,
    membership_repo: &PgMembershipRepository,
    required: Role,
) -> AutumnResult<Role> {
    let Some(user_id) = session
        .get("user_id")
        .await
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Err(AutumnError::unauthorized_msg("no active organization"));
    };
    let memberships = membership_repo.find_by_user_id(user_id).await?;
    let Some(membership) = memberships.into_iter().next() else {
        return Err(AutumnError::unauthorized_msg("no active organization"));
    };
    let Some(role) = Role::parse(&membership.role) else {
        return Err(AutumnError::forbidden_msg("insufficient permissions"));
    };
    if role.at_least(required) {
        Ok(role)
    } else {
        Err(AutumnError::forbidden_msg("insufficient permissions"))
    }
}

/// Record the active organization + role on the session — e.g. after login,
/// after switching organizations, or after accepting an invitation. Not
/// called by
/// [`super::routes::organizations::provision_default_organization`] itself
/// (see that function's doc comment for why signup and "authenticated"
/// aren't always the same event) — the first login after signup is what
/// establishes the org session for a newly-provisioned organization.
///
/// `tenant_id` is the active `Organization.id` in its string form — the same
/// value the `tenant_scoped` `Membership`/`Invitation` repositories filter by
/// (see `models.rs`'s doc comment on `Membership::tenant_id`).
///
/// Deliberately does NOT call `session.rotate_id()` — that is your own
/// login/signup handler's responsibility (once per authentication event,
/// not once per "also set org context"). Call this only after your handler
/// has already established (and, for login, rotated) the base session.
pub async fn establish_org_session(session: &Session, user_id: i64, tenant_id: &str, role: Role) {
    session.insert("user_id", user_id.to_string()).await;
    session.insert("organization_id", tenant_id).await;
    session.insert("role", role.as_str()).await;
}

// `require_role` needs a live `Membership` row (see above), so it is exercised
// end-to-end by your own integration tests against a real database rather
// than with a session-only unit test here.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_owner_satisfies_admin_and_member() {
        assert!(Role::Owner.at_least(Role::Owner));
        assert!(Role::Owner.at_least(Role::Admin));
        assert!(Role::Owner.at_least(Role::Member));
    }

    #[test]
    fn hierarchy_admin_satisfies_member_but_not_owner() {
        assert!(Role::Admin.at_least(Role::Admin));
        assert!(Role::Admin.at_least(Role::Member));
        assert!(!Role::Admin.at_least(Role::Owner));
    }

    #[test]
    fn hierarchy_member_satisfies_only_member() {
        assert!(Role::Member.at_least(Role::Member));
        assert!(!Role::Member.at_least(Role::Admin));
        assert!(!Role::Member.at_least(Role::Owner));
    }

    #[test]
    fn parse_round_trips_known_roles() {
        for role in [Role::Owner, Role::Admin, Role::Member] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
    }

    #[test]
    fn parse_rejects_unknown_strings() {
        assert_eq!(Role::parse("superadmin"), None);
        assert_eq!(Role::parse(""), None);
        assert_eq!(Role::parse("Owner"), None); // case-sensitive, no silent coercion
    }
}
"#;

fn render_repositories_rs() -> String {
    REPOSITORIES_RS.to_owned()
}

const REPOSITORIES_RS: &str = r#"//! Data-access repositories for `teams` (issue #1261).
//!
//! `Organization` is not tenant-scoped — it *is* the tenant, so creating one
//! and listing the ones a user belongs to both necessarily run outside any
//! single active tenant.
//!
//! `Membership` and `Invitation` are `tenant_scoped` by the active
//! organization (`organization_id`, resolved from the session — see
//! `crate::teams`'s module doc for the `[tenancy]` config this needs). Every
//! read/write against them is filtered/stamped by it automatically. The few
//! places that must legitimately cross organizations (resolving *all* of a
//! user's memberships to pick one at login; looking up an invitation by its
//! token before any organization is active) use the explicit, auditable
//! `across_tenants()` escape hatch from issue #695 rather than a second
//! isolation mechanism.

use crate::teams::models::{
    Invitation, Membership, NewInvitation, NewMembership, NewOrganization, Organization,
    UpdateInvitation, UpdateMembership, UpdateOrganization,
};
use crate::teams::schema::{invitations, memberships, organizations};

#[autumn_web::repository(Organization, table = "organizations")]
pub trait OrganizationRepository {}

#[autumn_web::repository(Membership, table = "memberships", tenant_scoped)]
pub trait MembershipRepository {
    /// All of a user's memberships, across every organization. Always called
    /// via `.across_tenants()` — resolving which organizations a user
    /// belongs to has to run before any one of them is "the" active tenant.
    fn find_by_user_id(user_id: i64) -> Vec<Membership>;
}

#[autumn_web::repository(Invitation, table = "invitations", tenant_scoped)]
pub trait InvitationRepository {
    /// Look up an invitation by its token hash. Always called via
    /// `.across_tenants()` — an invite accept link is followed before the
    /// visitor has an active organization (they may not even be signed in
    /// yet).
    fn find_by_token_hash(token_hash: String) -> Vec<Invitation>;
}
"#;

fn render_mailers_mod_rs() -> String {
    "pub mod invitation_mailer;\n".to_owned()
}

fn render_invitation_mailer_rs() -> String {
    INVITATION_MAILER_RS.to_owned()
}

const INVITATION_MAILER_RS: &str = r#"//! The invite email, sent through the shipped Mail stack (`#[mailer]`).
//!
//! A scaffolded, overridable template (issue #1261 AC4): edit
//! [`InvitationMailer::invite`] to change copy/branding, or replace `.html`
//! with your own Maud partial.

use autumn_web::prelude::*;

pub struct InvitationMailer;

#[mailer]
impl InvitationMailer {
    /// `accept_url` is the fully-qualified `/invite/{token}` link; the
    /// token itself is never logged or persisted in the clear (see
    /// `routes::invitations::create_invitation`).
    pub fn invite(
        &self,
        to: String,
        organization_name: String,
        role: String,
        accept_url: String,
    ) -> Mail {
        Mail::builder()
            .to(to)
            .subject(format!("You're invited to join {organization_name}"))
            .html(html! {
                p {
                    "You've been invited to join " strong { (organization_name) }
                    " as " (role) "."
                }
                p {
                    a href=(accept_url) { "Accept the invitation" }
                }
                p class="text-sm text-gray-500" { "This invitation expires in 7 days." }
            })
            .text(format!(
                "You've been invited to join {organization_name} as {role}.\n\n\
                 Accept: {accept_url}\n\n\
                 This invitation expires in 7 days."
            ))
            .build()
            .expect("static invite template should be valid")
    }
}

#[mailer_preview]
impl InvitationMailer {
    fn invite_preview() -> Mail {
        InvitationMailer.invite(
            "preview@example.com".to_owned(),
            "Acme, Inc.".to_owned(),
            "admin".to_owned(),
            "https://example.com/invite/preview-token".to_owned(),
        )
    }
}

pub fn mail_previews() -> Vec<MailPreview> {
    mail_previews![InvitationMailer]
}
"#;

fn render_routes_mod_rs() -> String {
    ROUTES_MOD_RS.to_owned()
}

const ROUTES_MOD_RS: &str = r#"//! Route handlers for `teams` (issue #1261): organizations, invitations,
//! members.

pub mod invitations;
pub mod members;
pub mod organizations;

use autumn_web::prelude::*;
use autumn_web::security::CsrfToken;

/// A minimal, dependency-free HTML wrapper for this module's own pages.
///
/// Deliberately does NOT call into your app's own `crate::layout` — this
/// module is meant to compose into any app regardless of that function's
/// signature/branding (see `crate::teams`'s composition-boundary note).
/// Swap these handlers to render through your own layout once you've wired
/// the integration seam.
pub(super) fn minimal_page(title: &str, content: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (title) }
            }
            body {
                (content)
            }
        }
    }
}

/// The CSRF token to embed in a form's hidden `_csrf` field, or `""` when
/// `CsrfLayer` isn't mounted (e.g. `[security.csrf] enabled = false`).
///
/// Handlers extract `Option<CsrfToken>` (not the bare, non-optional
/// `CsrfToken`) specifically so they degrade gracefully rather than 500ing
/// when CSRF is disabled — every generated form embeds this in a hidden
/// `_csrf` field, since `CsrfLayer` is on by default (including Autumn's
/// production-profile smart default) and would otherwise reject every POST
/// this module's forms submit. Mirrors `examples/teams`'s `layout.rs`
/// helper of the same name.
pub(super) fn csrf_value(csrf: &Option<CsrfToken>) -> &str {
    csrf.as_ref().map_or("", CsrfToken::token)
}
"#;

fn render_routes_organizations_rs() -> String {
    ROUTES_ORGANIZATIONS_RS.to_owned()
}

const ROUTES_ORGANIZATIONS_RS: &str = r#"//! Creating additional organizations, switching the active one, and
//! provisioning the personal organization a new signup gets (issue #1261).

use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::scoped_futures::ScopedFutureExt;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::teams::models::{Membership, Organization};
use crate::teams::repositories::{MembershipRepository, PgMembershipRepository};
use crate::teams::role::{Role, establish_org_session};
use crate::teams::schema::{invitations, memberships, organizations};

#[derive(diesel::Insertable)]
#[diesel(table_name = organizations)]
struct InsertOrganization {
    name: String,
}

#[derive(diesel::Insertable)]
#[diesel(table_name = memberships)]
struct InsertMembership {
    tenant_id: String,
    user_id: i64,
    role: String,
}

/// Create a personal organization for a brand-new user and make them its
/// `Owner` — call this as the last line of your own signup handler (issue
/// #1261 AC3; see `crate::teams`'s module doc for the full two-step
/// integration).
///
/// Deliberately does NOT call [`establish_org_session`] or otherwise touch
/// the session: signup and "the user is authenticated" are not the same
/// event for every app. `autumn generate auth`'s own scaffolded `signup`
/// creates the account but does not log it in — it's gated on email
/// confirmation, and only starts a session once the user actually confirms
/// and logs in. If this function established the org session here, calling
/// it from that handler (the most natural place to call it, and the one
/// this crate's own docs point at) would silently authenticate an
/// unconfirmed account, bypassing the confirmation gate entirely. Creating
/// the organization/membership rows is safe unconditionally — resolving
/// the active org and calling `establish_org_session` is the *login*
/// flow's job (step 2 of the two-step integration), which naturally picks
/// this membership up the first time the user actually logs in.
///
/// `db` is your own signup handler's `Db` extractor: the organization and
/// membership inserts run in one transaction on it, so a failure between
/// the two can never leave an orphaned organization with no members. This
/// can't extend to your own preceding user insert — a plain function call
/// invoked *after* that insert has no way to join a transaction it wasn't
/// called inside of (even sharing the same connection wouldn't help once
/// the earlier insert has already committed on its own). If your signup
/// handler creates the account outside its own transaction (e.g.
/// `autumn generate auth`'s scaffolded `signup`, which doesn't wrap its
/// user insert in `db.tx(...)`), a failure here still leaves that account
/// stranded with no organization. Closing that gap needs the account
/// insert to run inside a transaction too, with
/// [`provision_default_organization_on_conn`] called on the same `conn` —
/// see that function's doc comment and `docs/generate-teams.md` for the
/// full atomic pattern.
pub async fn provision_default_organization(user_id: i64, db: &mut Db) -> AutumnResult<()> {
    db.tx(move |conn| provision_default_organization_on_conn(conn, user_id).scope_boxed())
        .await
}

/// The building block behind [`provision_default_organization`]: the same
/// two inserts, run directly on an already-open `conn` instead of opening
/// its own transaction.
///
/// Call this instead of [`provision_default_organization`] from *inside*
/// your own signup handler's `db.tx(move |conn| { ... }.scope_boxed())`
/// block — alongside your own user insert, on the same `conn` — when a
/// failure partway through must roll back the account too, not just leave
/// it with no organization. This is what makes true 3-way atomicity
/// (account + organization + membership) possible despite
/// [`provision_default_organization`] itself only being able to cover the
/// latter two: a plain function call has no way to join a transaction it
/// wasn't invoked inside of. See `docs/generate-teams.md` for a full
/// worked example.
pub async fn provision_default_organization_on_conn(
    conn: &mut autumn_web::db::PooledConnection,
    user_id: i64,
) -> AutumnResult<()> {
    let org: Organization = diesel::insert_into(organizations::table)
        .values(&InsertOrganization {
            name: format!("User {user_id}'s Organization"),
        })
        .returning(Organization::as_returning())
        .get_result(conn)
        .await?;

    diesel::insert_into(memberships::table)
        .values(&InsertMembership {
            tenant_id: org.id.to_string(),
            user_id,
            role: Role::Owner.as_str().to_owned(),
        })
        .execute(conn)
        .await?;

    Ok(())
}

/// Remove every membership `user_id` holds, across every organization —
/// call this from your own account-deletion handler, before (or as part of)
/// deleting the account row itself (Codex review finding: `Membership`
/// deliberately has no FK to your `users` table — see `crate::teams`'s
/// module doc — so deleting a user through e.g. `autumn generate auth`'s
/// scaffolded `account_destroy` does not touch these rows on its own; a
/// stale session cookie on another device would otherwise keep
/// `require_role` passing indefinitely for a user whose account no longer
/// exists, since it only re-checks the live `Membership` row, not the
/// account itself).
///
/// For every organization where `user_id` is currently the sole `Owner`
/// (checked by first looking for another live `Owner` in that same
/// organization — Codex review finding: an org that already has a second
/// Owner must not have anyone else promoted into a redundant `Owner` role),
/// promotes another existing member (preferring an `Admin`) to `Owner`
/// first, before removing `user_id`'s own membership — bypassing this would
/// silently defeat the same last-owner protection `remove_member`/
/// `change_role` already enforce for interactive requests: an organization
/// stripped of its only Owner by account deletion would still have Admins/
/// Members, but no one left able to ever grant a replacement Owner
/// (`create_invitation`/`change_role` both require an existing Owner to
/// grant the `Owner` role).
///
/// If no other member exists in that organization at all, there is no one
/// to promote — the organization is left with zero members, so its pending
/// invitations are revoked too (Codex review finding): left live, one could
/// later be accepted and repopulate a headless organization no one will
/// ever be able to manage or promote a new Owner in.
///
/// Runs across every organization in one transaction: not tenant-scoped, so
/// raw queries against `db` rather than the generated `tenant_scoped`
/// repository, which can only ever see the caller's single active tenant.
///
/// This opens and commits its own transaction, independent of whatever your
/// account-deletion handler does around it (Codex review finding): called
/// *before* your account `DELETE`, a subsequent failure of that `DELETE`
/// leaves a still-live account permanently stripped of its memberships;
/// called *after*, a failure partway through this call itself leaves a
/// deleted account's memberships behind — the exact stale-access gap this
/// function exists to close. If your account-deletion handler already runs
/// inside its own `db.tx(...)` (or you're willing to add one), call
/// [`remove_all_memberships_on_conn`] on that same `conn` instead, so
/// cleanup and the account `DELETE` commit or roll back together:
///
/// ```ignore
/// use scoped_futures::ScopedFutureExt;
///
/// db.tx(move |conn| {
///     async move {
///         teams::routes::organizations::remove_all_memberships_on_conn(conn, user_id).await?;
///         diesel::delete(users::table.find(user_id)).execute(conn).await?;
///         Ok::<_, AutumnError>(())
///     }
///     .scope_boxed()
/// })
/// .await?;
/// ```
///
/// ```ignore
/// teams::routes::organizations::remove_all_memberships(user_id, &mut db).await?;
/// ```
pub async fn remove_all_memberships(user_id: i64, db: &mut Db) -> AutumnResult<()> {
    db.tx(move |conn| remove_all_memberships_on_conn(conn, user_id).scope_boxed())
        .await
}

/// The building block behind [`remove_all_memberships`]: the same cleanup,
/// run directly on an already-open `conn` instead of opening its own
/// transaction — see that function's doc comment for when to reach for this
/// instead, and `docs/generate-teams.md` for a full worked example.
pub async fn remove_all_memberships_on_conn(
    conn: &mut autumn_web::db::PooledConnection,
    user_id: i64,
) -> AutumnResult<()> {
    // Deliberately NOT `for_update()` here: this is just a scan to discover
    // which organizations this user needs handling in below, where the
    // actual locks are taken (org row first, then this user's own
    // membership row only after that). Locking these rows up front —
    // before any org row — is what caused a real deadlock (Codex review
    // finding): if two owners of the *same* organization delete their
    // accounts concurrently, each would lock its own row here first, then
    // each need the org lock *and* the other's row (via the other-owner
    // check below) — a lock-order cycle Postgres has no choice but to
    // break by aborting one deletion.
    //
    // Every organization this user is a member of at all — not just the
    // ones they currently *own* — is scanned and processed one at a time
    // below, each removed only after its own organization lock is held
    // (Codex review finding): scanning only `role = 'owner'` here missed a
    // real scenario — two users deleting their accounts concurrently,
    // where one is merely an Admin in an organization solely owned by the
    // other. That Admin's own scan would never even look at that
    // organization (it doesn't own it *yet*), so nothing would stop this
    // function's final step from unconditionally deleting the Admin's
    // membership row there — including one a concurrent sibling
    // transaction just promoted to Owner moments earlier — leaving the
    // organization ownerless despite the successor logic below having
    // already run for it.
    // Ordered deterministically: if two users being deleted concurrently
    // share two or more organizations, and each transaction locked them in
    // a different order, one could lock org A and wait for org B while the
    // other locks org B and waits for org A — a real deadlock Postgres can
    // only resolve by aborting one deletion (Codex review finding). Every
    // caller of this function scans in the same order, so whichever
    // transaction gets to the first shared organization first always
    // proceeds to the next one uncontested.
    let member_tenant_ids: Vec<String> = memberships::table
        .filter(memberships::user_id.eq(user_id))
        .select(memberships::tenant_id)
        .order(memberships::tenant_id.asc())
        .load(conn)
        .await?;

    for tenant_id in &member_tenant_ids {
        // Lock the organization row FIRST — before any membership row for
        // this organization — as the shared serialization point with
        // `accept_invitation` (see that function's own matching org-row
        // lock). Without a lock shared across both functions, this
        // successor lookup below could decide "no successor exists" a
        // moment before a concurrent `accept_invitation` commits a
        // brand-new membership for this same organization — this function
        // would then delete the sole Owner anyway, leaving the newly
        // joined member in an organization that can never regain an Owner
        // (Codex review finding). Both functions lock organizations before
        // memberships/invitations, in that same order, so whichever gets
        // here first completes wholly before the other proceeds, instead
        // of deadlocking.
        let org_id: i64 = tenant_id.parse().map_err(|_| {
            AutumnError::internal_server_error_msg("Corrupt organization id in membership")
        })?;
        organizations::table
            .find(org_id)
            .for_update()
            .select(Organization::as_select())
            .first(conn)
            .await
            .optional()?;

        // Re-fetch and lock this user's own membership row now that the
        // organization lock is held — the unlocked scan above could be
        // stale (e.g. a concurrent `change_role` may have already demoted
        // this user in this organization, or — the scenario the widened
        // scan above exists for — a concurrent sibling deletion may have
        // just *promoted* this user to Owner here as its own successor).
        let Some(own_membership) = memberships::table
            .filter(memberships::tenant_id.eq(tenant_id))
            .filter(memberships::user_id.eq(user_id))
            .select(Membership::as_select())
            .for_update()
            .first(conn)
            .await
            .optional()?
        else {
            continue;
        };

        // Only a *sole* Owner needs a successor — if this user isn't
        // currently Owner here at all (the common case for a plain
        // Admin/Member row), or another live Owner remains, ownership
        // doesn't need transferring, and promoting anyone here would grant
        // an unrequested second Owner (Codex review finding).
        if own_membership.role == Role::Owner.as_str() {
            let other_owner: Option<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&own_membership.tenant_id))
                .filter(memberships::user_id.ne(user_id))
                .filter(memberships::role.eq(Role::Owner.as_str()))
                .select(Membership::as_select())
                .for_update()
                .first(conn)
                .await
                .optional()?;
            if other_owner.is_none() {
                let mut successor: Option<Membership> = memberships::table
                    .filter(memberships::tenant_id.eq(&own_membership.tenant_id))
                    .filter(memberships::user_id.ne(user_id))
                    .filter(memberships::role.eq(Role::Admin.as_str()))
                    .select(Membership::as_select())
                    .order(memberships::id.asc())
                    .for_update()
                    .first(conn)
                    .await
                    .optional()?;
                if successor.is_none() {
                    successor = memberships::table
                        .filter(memberships::tenant_id.eq(&own_membership.tenant_id))
                        .filter(memberships::user_id.ne(user_id))
                        .select(Membership::as_select())
                        .order(memberships::id.asc())
                        .for_update()
                        .first(conn)
                        .await
                        .optional()?;
                }
                if let Some(successor) = successor {
                    diesel::update(memberships::table.filter(memberships::id.eq(successor.id)))
                        .set(memberships::role.eq(Role::Owner.as_str()))
                        .execute(conn)
                        .await?;
                } else {
                    // No other member exists at all: this organization is
                    // about to be left with zero memberships, permanently —
                    // revoke any pending invitations too, so a later accept
                    // can't repopulate a headless organization no one will
                    // ever be able to manage or promote a new Owner in
                    // (Codex review finding).
                    diesel::update(
                        invitations::table
                            .filter(invitations::tenant_id.eq(&own_membership.tenant_id))
                            .filter(invitations::status.eq("pending")),
                    )
                    .set(invitations::status.eq("revoked"))
                    .execute(conn)
                    .await?;
                }
            }
        }

        // Remove this user's own membership in *this* organization now,
        // under its lock — rather than one blanket `DELETE ... WHERE
        // user_id = user_id` after the loop, which would fall outside
        // every organization's lock and could remove a row a concurrent
        // sibling deletion just promoted to Owner moments after this loop
        // last read it (the same finding the widened scan above closes).
        diesel::delete(memberships::table.filter(memberships::id.eq(own_membership.id)))
            .execute(conn)
            .await?;
    }

    Ok(())
}

/// Create a new organization; the caller becomes its `Owner` and it becomes
/// the active organization (same rule as signup — issue #1261 AC3).
#[post("/organizations")]
pub async fn create_organization(
    session: Session,
    mut db: Db,
    Form(form): Form<crate::teams::models::NewOrganizationForm>,
) -> AutumnResult<Response> {
    let Some(user_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    let name = form.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(AutumnError::unprocessable_msg(
            "Organization name must be between 1 and 200 characters",
        ));
    }

    // One transaction: without it, a failure between the org insert and the
    // owner-membership insert would leave an orphaned `Organization` row no
    // one is a member of.
    let org: Organization = db
        .tx(move |conn| {
            async move {
                let org: Organization = diesel::insert_into(organizations::table)
                    .values(&InsertOrganization { name })
                    .returning(Organization::as_returning())
                    .get_result(conn)
                    .await?;

                diesel::insert_into(memberships::table)
                    .values(&InsertMembership {
                        tenant_id: org.id.to_string(),
                        user_id,
                        role: Role::Owner.as_str().to_owned(),
                    })
                    .execute(conn)
                    .await?;

                Ok::<_, AutumnError>(org)
            }
            .scope_boxed()
        })
        .await?;

    establish_org_session(&session, user_id, &org.id.to_string(), Role::Owner).await;
    Ok(Redirect::to("/members").into_response())
}

/// Switch the active organization to one the caller already belongs to.
#[post("/organizations/{id}/switch")]
pub async fn switch_organization(
    session: Session,
    membership_repo: PgMembershipRepository,
    Path(target_org_id): Path<i64>,
) -> AutumnResult<Response> {
    let Some(user_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    let target_tenant_id = target_org_id.to_string();
    let memberships: Vec<Membership> = membership_repo
        .across_tenants()
        .find_by_user_id(user_id)
        .await?;
    let Some(membership) = memberships
        .into_iter()
        .find(|m| m.tenant_id == target_tenant_id)
    else {
        return Err(AutumnError::forbidden_msg(
            "You are not a member of that organization",
        ));
    };
    let Some(role) = Role::parse(&membership.role) else {
        return Err(AutumnError::internal_server_error_msg(
            "Corrupt membership role",
        ));
    };

    establish_org_session(&session, user_id, &target_tenant_id, role).await;
    Ok(Redirect::to("/members").into_response())
}
"#;

fn render_routes_invitations_rs() -> String {
    ROUTES_INVITATIONS_RS.to_owned()
}

const ROUTES_INVITATIONS_RS: &str = r#"//! Email invitations: create, accept, revoke, resend (issue #1261 AC4-AC6).
//!
//! Accepting an invitation requires an authenticated session — unlike
//! `examples/teams` (which owns its whole app, `users` table included),
//! this module doesn't know your app's `User`/`users` shape (see
//! `crate::teams`'s composition-boundary note), so it cannot inline "create
//! an account and join" for a logged-out visitor. A logged-out visitor is
//! shown a message pointing at your app's `/login` and `/signup`; once
//! signed in they can revisit the same invitation link to accept.
//!
//! Accepting is idempotent (AC5): a second click never creates a second
//! `Membership` row or 500s. Expired/revoked/already-consumed tokens render
//! a clear error page (AC6), never a panic.
//!
//! The invitee-facing routes live under `/invite/...` — a *different* path
//! prefix than the admin-only `/invitations` routes below (`create`/`revoke`/
//! `resend`) — deliberately. `[tenancy] public_paths` matches by path
//! *prefix* (see `docs/generate-teams.md`), so an anonymous visitor following
//! an accept link needs `/invite` exempted from the tenancy gate, but the
//! admin routes must stay gated (they need the ambient tenant the gate
//! establishes). A single shared `/invitations` prefix would have exempted
//! the admin routes too — this split keeps the public surface exactly as
//! small as it needs to be instead of relying on a fragile allowlist.

use autumn_web::auth::{generate_raw_token, hash_api_token};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::scoped_futures::ScopedFutureExt;
use autumn_web::security::CsrfToken;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::teams::mailers::invitation_mailer::InvitationMailer;
use crate::teams::models::{InviteForm, Invitation, Membership, Organization};
use crate::teams::repositories::{
    InvitationRepository, OrganizationRepository, PgInvitationRepository, PgMembershipRepository,
    PgOrganizationRepository,
};
use crate::teams::role::{Role, establish_org_session, require_role};
use crate::teams::schema::{invitations, memberships, organizations};

use super::{csrf_value, minimal_page};

const INVITATION_TTL_DAYS: i64 = 7;

// Raw-insert struct for `resend_invitation`'s hand-rolled `db.tx`
// transaction (revoke old + create new in one atomic step) — the generated
// `tenant_scoped` repository's CRUD methods each acquire their own pooled
// connection, which can't share an outer transaction's connection. See also
// the `InsertMembership` struct inside `accept_invitation`'s own `db.tx`.
#[derive(diesel::Insertable)]
#[diesel(table_name = invitations)]
struct InsertInvitation {
    tenant_id: String,
    email: String,
    role: String,
    token_hash: String,
    status: String,
    invited_by_user_id: i64,
    expires_at: chrono::NaiveDateTime,
}

// A minimal, read-only view onto your app's own `users` table — just enough
// to verify an accept-invitation caller's email address (see
// `accept_invitation` below). This module has no `User` model to import (see
// `crate::teams`'s module doc), so it leans on the same i64-primary-key,
// `users`-table-name convention it already assumes for
// `Membership::user_id`/`Invitation::invited_by_user_id`. If your app's
// `users` table's primary key or email column differs from this, adjust the
// two lines below to match.
//
// The table NAME itself is a placeholder, not introspected from your
// project: `autumn generate auth` accepts a custom resource name (e.g.
// `autumn generate auth Account` creates a table named `accounts`, not
// `users`), and `teams` has no way to know which name you used when you ran
// it — a different generator invocation entirely, with no shared state.
// Rename `users` below to your actual auth table if it isn't the default,
// or invitation acceptance will fail at runtime with a missing-relation
// database error (Codex review finding).
mod caller_email_lookup {
    diesel::table! {
        users (id) {
            id -> BigInt,
            email -> Text,
        }
    }
}

// ── Create ───────────────────────────────────────────────────────────────

/// Absolute base URL for links embedded in emails, matching the convention
/// `autumn generate auth`'s scaffolded email flows use.
fn app_base_url() -> String {
    std::env::var("APP_BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_owned())
}

/// Send an invitation into the *active* organization. Gated `Admin` or
/// higher (issue #1261 AC7); inviting someone as `Owner` itself requires the
/// caller to already be an `Owner` — otherwise an Admin could mint a fresh
/// Owner account of their own choosing.
#[post("/invitations")]
pub async fn create_invitation(
    session: Session,
    Tenant(tenant_id): Tenant,
    mut db: Db,
    org_repo: PgOrganizationRepository,
    membership_repo: PgMembershipRepository,
    mailer: Mailer,
    Form(form): Form<InviteForm>,
) -> AutumnResult<Response> {
    let caller_role = require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(inviter_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    let email = form.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return Err(AutumnError::unprocessable_msg("Enter a valid email address"));
    }
    let Some(role) = Role::parse(&form.role) else {
        return Err(AutumnError::unprocessable_msg("Unknown role"));
    };
    if role == Role::Owner && caller_role != Role::Owner {
        return Err(AutumnError::forbidden_msg(
            "Only an owner can invite someone as owner",
        ));
    }

    let org_id: i64 = tenant_id.parse().map_err(|_| {
        AutumnError::internal_server_error_msg("Corrupt organization id in session")
    })?;
    let Some(organization) = org_repo.find_by_id(org_id).await? else {
        return Err(AutumnError::not_found_msg("Organization not found"));
    };

    let raw_token = generate_raw_token();
    let token_hash = hash_api_token(&raw_token);
    let insert_email = email.clone();
    let insert_role = role.as_str().to_owned();
    db.tx(move |conn| {
        async move {
            // Lock the organization row FIRST — the same shared
            // serialization point `accept_invitation`/
            // `remove_all_memberships_on_conn` use — before issuing a new
            // pending invitation. Without it, an in-flight account
            // deletion's cleanup could decide "no successor exists",
            // revoke every pending invitation, and delete the
            // organization's last Owner, all while this uncoordinated
            // transaction inserts a fresh pending invitation the cleanup
            // never saw — accepting that token later repopulates the
            // organization without an Owner, recreating the exact
            // invariant cleanup exists to prevent (Codex review finding).
            organizations::table
                .find(org_id)
                .for_update()
                .select(Organization::as_select())
                .first(conn)
                .await
                .optional()?;

            // Revalidate the inviter's own membership now that the
            // organization lock is held — the pre-transaction `require_role`
            // result could be stale: another request (a demotion, a
            // removal, or this same user's account being deleted) may have
            // committed while this request waited for the lock above
            // (Codex review finding).
            let inviter_membership: Option<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .filter(memberships::user_id.eq(inviter_id))
                .select(Membership::as_select())
                .for_update()
                .first(conn)
                .await
                .optional()?;
            let Some(inviter_membership) = inviter_membership else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(current_caller_role) = Role::parse(&inviter_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !current_caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }
            if role == Role::Owner && current_caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can invite someone as owner",
                ));
            }

            // Reject rather than silently revoke-and-replace a still-pending
            // invitation to this email — now that the organization lock
            // above serializes concurrent `create_invitation` calls for
            // this organization, a second request for an email that's
            // already pending is a genuine conflict the caller should see,
            // not a race to resolve by rewriting history underneath them.
            // Revoking-and-replacing here also sent two invite emails out
            // of release-lock order, so a reordered delivery could leave
            // the invitee's newest message holding an already-revoked
            // token (Codex review finding).
            let existing_pending: Option<Invitation> = invitations::table
                .filter(invitations::tenant_id.eq(&tenant_id))
                .filter(invitations::email.eq(&insert_email))
                .filter(invitations::status.eq("pending"))
                .select(Invitation::as_select())
                .first(conn)
                .await
                .optional()?;
            if existing_pending.is_some() {
                return Err(AutumnError::conflict_msg(
                    "An invitation to this email is already pending for this organization",
                ));
            }

            diesel::insert_into(invitations::table)
                .values(&InsertInvitation {
                    tenant_id,
                    email: insert_email,
                    role: insert_role,
                    token_hash,
                    status: "pending".to_owned(),
                    invited_by_user_id: inviter_id,
                    expires_at: chrono::Utc::now().naive_utc()
                        + chrono::Duration::days(INVITATION_TTL_DAYS),
                })
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
    .map_err(|err| {
        // The organization lock plus the explicit pending-email check above
        // already serialize concurrent `create_invitation` calls for this
        // organization, so this should be unreachable in practice.
        // `idx_invitations_pending_email` (a partial unique index on
        // `(tenant_id, email) WHERE status = 'pending'`, see MIGRATION_UP)
        // stays as defense in depth: any INSERT that still slips past the
        // check above fails closed instead of leaving two live pending
        // tokens for the same invitee.
        if autumn_web::error::unique_violation_field(
            &err,
            &[(
                "idx_invitations_pending_email",
                "email",
                "An invitation to this email is already pending",
            )],
        )
        .is_some()
        {
            AutumnError::conflict_msg(
                "An invitation to this email is already pending for this organization",
            )
        } else {
            err
        }
    })?;

    let accept_url = format!("{}/invite/{raw_token}", app_base_url());
    // Sent synchronously (not `deliver_later_invite`): the success metric is
    // "an invite email is delivered to the dev mailbox within the same
    // request cycle" (issue #1261), which a fire-and-forget background send
    // can't guarantee.
    InvitationMailer
        .send_invite(
            &mailer,
            email,
            organization.name,
            role.as_str().to_owned(),
            accept_url,
        )
        .await?;

    Ok(Redirect::to("/members").into_response())
}

// ── Accept (show) ───────────────────────────────────────────────────────

/// `Invitation::tenant_id` is a `String` (the `tenant_scoped` macro's
/// convention — see `models.rs`); parse it back to the `Organization.id` it
/// names when a lookup on the `Organization` row itself is needed.
fn parse_tenant_id(tenant_id: &str) -> AutumnResult<i64> {
    tenant_id
        .parse()
        .map_err(|_| AutumnError::internal_server_error_msg("Corrupt tenant id"))
}

fn invitation_error_page(message: &str) -> Markup {
    minimal_page(
        "Invitation",
        html! {
            p { (message) }
        },
    )
}

async fn load_pending_invitation(
    invitation_repo: &PgInvitationRepository,
    raw_token: &str,
) -> AutumnResult<Option<Invitation>> {
    let token_hash = hash_api_token(raw_token);
    let mut matches = invitation_repo
        .across_tenants()
        .find_by_token_hash(token_hash)
        .await?;
    Ok(matches.pop())
}

fn invitation_status_error(invitation: &Invitation) -> Option<&'static str> {
    match invitation.status.as_str() {
        "accepted" => Some("This invitation has already been used."),
        "revoked" => Some("This invitation was revoked."),
        _ if invitation.expires_at <= chrono::Utc::now().naive_utc() => {
            Some("This invitation has expired.")
        }
        _ => None,
    }
}

#[get("/invite/{token}")]
pub async fn show_invitation(
    session: Session,
    invitation_repo: PgInvitationRepository,
    org_repo: PgOrganizationRepository,
    csrf: Option<CsrfToken>,
    Path(raw_token): Path<String>,
) -> AutumnResult<Response> {
    let Some(invitation) = load_pending_invitation(&invitation_repo, &raw_token).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page("This invitation link is invalid."),
        )
            .into_response());
    };
    if let Some(message) = invitation_status_error(&invitation) {
        return Ok((StatusCode::GONE, invitation_error_page(message)).into_response());
    }
    let Some(organization) = org_repo.find_by_id(parse_tenant_id(&invitation.tenant_id)?).await?
    else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page("This invitation's organization no longer exists."),
        )
            .into_response());
    };

    let signed_in = session.get("user_id").await.is_some();

    let page = if signed_in {
        minimal_page(
            "Accept invitation",
            html! {
                h1 { "Join " (organization.name) }
                p { "You've been invited as " (invitation.role) "." }
                form action={"/invite/" (raw_token) "/accept"} method="post" {
                    input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                    button type="submit" { "Accept invitation" }
                }
            },
        )
    } else {
        minimal_page(
            "Accept invitation",
            html! {
                h1 { "Join " (organization.name) }
                p {
                    "You've been invited as " (invitation.role)
                    ". Log in or sign up with " strong { (invitation.email) }
                    ", then come back to this link to accept."
                }
                p {
                    a href="/login" { "Log in" } " · " a href="/signup" { "Sign up" }
                }
            },
        )
    };
    Ok(page.into_response())
}

// ── Accept (confirm) ────────────────────────────────────────────────────

/// Accept a pending invitation into the caller's currently-active session.
/// Requires an authenticated session — see this module's doc comment.
#[post("/invite/{token}/accept")]
pub async fn accept_invitation(
    session: Session,
    mut db: Db,
    invitation_repo: PgInvitationRepository,
    Path(raw_token): Path<String>,
) -> AutumnResult<Response> {
    let Some(user_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg(
            "log in or sign up first, then revisit this invitation link",
        ));
    };

    let Some(invitation) = load_pending_invitation(&invitation_repo, &raw_token).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page("This invitation link is invalid."),
        )
            .into_response());
    };

    // Security-critical: an authenticated caller may only redeem an invite
    // addressed to their own email. Without this check, anyone who obtains a
    // token not meant for them (a forwarded link, a mail-scanner fetch, a
    // shared clipboard) while already signed in as a different account could
    // join — at whatever role the invite grants — an organization they were
    // never invited to.
    use caller_email_lookup::users as caller_users;
    let caller_email: String = caller_users::table
        .filter(caller_users::id.eq(user_id))
        .select(caller_users::email)
        .first(&mut *db)
        .await?;
    if caller_email.trim().to_lowercase() != invitation.email.trim().to_lowercase() {
        return Err(AutumnError::forbidden_msg(
            "This invitation was sent to a different email address",
        ));
    }

    let invitation_id = invitation.id;
    let tenant_id = invitation.tenant_id.clone();
    let invitation_email = invitation.email.clone();
    let role = invitation.role.clone();

    // Raw queries (not the generated `tenant_scoped` repository) inside one
    // transaction: the repository's CRUD methods each acquire their own
    // pooled connection, which can't share this transaction's connection —
    // and atomicity across the row-lock + insert + status update is exactly
    // what makes double-click acceptance idempotent instead of racy.
    let membership: Membership = db
        .tx(move |conn| {
            async move {
                // Lock and revalidate the caller's own account row FIRST —
                // before the organization lock below, and before anything
                // else in this transaction. The email-match check earlier
                // in this handler ran on a standalone, unlocked connection
                // before this transaction even started, checking only that
                // the account existed with a matching email at that moment
                // — if the account is deleted, or its email is changed via
                // your app's own email-change flow, in the gap between
                // that check and this transaction actually acquiring this
                // lock, nothing else would notice: `Membership.user_id` has
                // no FK to your `users` table, so the insert below would
                // otherwise happily create a membership — and this handler
                // would establish a session — for an account that no
                // longer exists, or no longer owns the invited address
                // (Codex review finding — re-compares the live email, not
                // just existence).
                //
                // This must be the FIRST lock taken, before organizations,
                // and your own account-deletion handler must lock/verify
                // its `users` row before calling
                // `remove_all_memberships_on_conn` too (see
                // `docs/generate-teams.md`'s worked example) — otherwise
                // account deletion could finish its membership cleanup,
                // which never touches this not-yet-existing membership,
                // before this transaction gets a chance to insert it,
                // leaving a live membership for a since-deleted account
                // cleanup never saw. Locking the account row first, in the
                // same order on both sides, closes that gap: whichever
                // transaction reaches it first now runs to completion
                // before the other proceeds at all.
                let live_caller_email: Option<String> = caller_users::table
                    .filter(caller_users::id.eq(user_id))
                    .select(caller_users::email)
                    .for_update()
                    .first(conn)
                    .await
                    .optional()?;
                let Some(live_caller_email) = live_caller_email else {
                    return Err(AutumnError::unauthorized_msg(
                        "log in or sign up first, then revisit this invitation link",
                    ));
                };
                if live_caller_email.trim().to_lowercase() != invitation_email.trim().to_lowercase()
                {
                    return Err(AutumnError::forbidden_msg(
                        "This invitation was sent to a different email address",
                    ));
                }

                // Lock the organization row — as the shared serialization
                // point with `remove_all_memberships_on_conn` (see its own
                // matching org-row lock, and the account-row-first note
                // above). Without a lock shared across both functions, an
                // account deletion concurrently cleaning up this same
                // organization could decide "no successor member exists" a
                // moment before this accept commits a brand-new
                // membership, then delete the sole Owner anyway — leaving
                // this invitee joined to an organization that can never
                // regain an Owner (Codex review finding). Both functions
                // lock organizations before memberships/invitations, in
                // that same order, so whichever gets here first completes
                // wholly before the other proceeds, instead of
                // deadlocking.
                let org_id: i64 = tenant_id.parse().map_err(|_| {
                    AutumnError::internal_server_error_msg("Corrupt organization id in invitation")
                })?;
                organizations::table
                    .find(org_id)
                    .for_update()
                    .select(Organization::as_select())
                    .first(conn)
                    .await
                    .optional()?;

                // Lock the invitation row so two concurrent accepts serialize
                // rather than race past the pending/expiry check together.
                let invitation: Invitation = invitations::table
                    .filter(invitations::id.eq(invitation_id))
                    .for_update()
                    .select(Invitation::as_select())
                    .first(conn)
                    .await?;

                // Reject a revoked, or expired-while-still-pending, token
                // up front — before the existing-membership shortcut below.
                // This check must NOT run for an already-`accepted` token:
                // that's the idempotent double-click case, whose
                // `expires_at` (fixed at creation) may well have passed by
                // now without that being a problem, since no new grant is
                // happening. Checked before the `existing` lookup so a
                // revoked/expired-pending invitation can never silently
                // reuse someone's unrelated existing membership to switch
                // their active organization and return success (Codex
                // review finding).
                if invitation.status == "revoked"
                    || (invitation.status == "pending"
                        && invitation.expires_at <= chrono::Utc::now().naive_utc())
                {
                    return Err(AutumnError::gone_msg(
                        "This invitation is no longer available",
                    ));
                }

                let existing: Option<Membership> = memberships::table
                    .filter(memberships::tenant_id.eq(&tenant_id))
                    .filter(memberships::user_id.eq(user_id))
                    .select(Membership::as_select())
                    .first(conn)
                    .await
                    .optional()?;
                if let Some(existing) = existing {
                    // The user already belongs to this organization — either
                    // this exact token was already redeemed (idempotent
                    // double-click: `invitation.status` is already
                    // "accepted"), or they joined some other way while this
                    // invitation sat pending (and, per the check above, not
                    // expired). Either way there's no new membership to
                    // insert, but a still-pending invitation must be
                    // consumed here too — otherwise its token stays valid
                    // indefinitely and, if this membership is later removed,
                    // redeeming the lingering token again would silently
                    // regrant it.
                    if invitation.status == "pending" {
                        diesel::update(invitations::table.filter(invitations::id.eq(invitation_id)))
                            .set(invitations::status.eq("accepted"))
                            .execute(conn)
                            .await?;
                    }
                    return Ok::<_, AutumnError>(existing);
                }

                if invitation.status != "pending" {
                    // Already "accepted" (checked for "revoked"/expired
                    // above) but no existing membership: the token was
                    // consumed by an earlier accept whose membership has
                    // since been removed. Nothing left to (re)grant.
                    return Err(AutumnError::gone_msg(
                        "This invitation is no longer available",
                    ));
                }

                #[derive(diesel::Insertable)]
                #[diesel(table_name = memberships)]
                struct InsertMembership {
                    tenant_id: String,
                    user_id: i64,
                    role: String,
                }
                let membership: Membership = diesel::insert_into(memberships::table)
                    .values(&InsertMembership {
                        tenant_id: tenant_id.clone(),
                        user_id,
                        role: role.clone(),
                    })
                    .returning(Membership::as_returning())
                    .get_result(conn)
                    .await?;

                diesel::update(invitations::table.filter(invitations::id.eq(invitation_id)))
                    .set(invitations::status.eq("accepted"))
                    .execute(conn)
                    .await?;

                Ok(membership)
            }
            .scope_boxed()
        })
        .await?;

    let Some(resolved_role) = Role::parse(&membership.role) else {
        return Err(AutumnError::internal_server_error_msg(
            "Corrupt membership role",
        ));
    };
    establish_org_session(&session, user_id, &membership.tenant_id, resolved_role).await;
    Ok(Redirect::to("/members").into_response())
}

// ── Revoke / resend ──────────────────────────────────────────────────────

/// Revoke a pending invitation. Gated `Admin` or higher.
#[post("/invitations/{id}/revoke")]
pub async fn revoke_invitation(
    session: Session,
    mut db: Db,
    invitation_repo: PgInvitationRepository,
    membership_repo: PgMembershipRepository,
    Path(invitation_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(caller_id) = session.get("user_id").await.and_then(|s| s.parse::<i64>().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };
    // `find_by_id` is tenant-scoped, so this also proves `invitation_id`
    // belongs to the caller's active organization before anything is written.
    let Some(old) = invitation_repo.find_by_id(invitation_id).await? else {
        return Err(AutumnError::not_found_msg("Invitation not found"));
    };
    let tenant_id = old.tenant_id.clone();

    db.tx(move |conn| {
        async move {
            // Revalidate the caller's own membership inside the
            // transaction, instead of trusting the pre-transaction
            // `require_role` result — see `create_invitation`'s identical
            // fix for the full rationale (Codex review finding).
            let caller_membership: Option<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .filter(memberships::user_id.eq(caller_id))
                .select(Membership::as_select())
                .for_update()
                .first(conn)
                .await
                .optional()?;
            let Some(caller_membership) = caller_membership else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(current_caller_role) = Role::parse(&caller_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !current_caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }

            // Lock and re-check status inside the transaction: a stale
            // revoke form submitted after the invitee already accepted — or
            // an accept that commits while this transaction waits for the
            // row lock above — must not stomp the terminal `accepted` row
            // back to `revoked`. That would leave the granted membership in
            // place while a later revisit of the (already-consumed) link
            // reports "revoked" instead of following the accepted-token
            // idempotency path, corrupting the invitation's audit trail
            // (Codex review finding).
            let current: Invitation = invitations::table
                .filter(invitations::id.eq(invitation_id))
                .for_update()
                .select(Invitation::as_select())
                .first(conn)
                .await?;
            if current.status != "pending" {
                return Err(AutumnError::conflict_msg(
                    "This invitation is no longer pending",
                ));
            }

            diesel::update(invitations::table.filter(invitations::id.eq(invitation_id)))
                .set(invitations::status.eq("revoked"))
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Redirect::to("/members").into_response())
}

/// Re-send a pending invitation: revoke the old token and mint a fresh one
/// with a new expiry (issue #1261 AC6 — "new token, old token invalidated").
/// The revoke-then-create pair runs in one transaction so a failure between
/// the two steps can never leave the organization with neither a valid old
/// nor a new invitation.
#[post("/invitations/{id}/resend")]
pub async fn resend_invitation(
    session: Session,
    mut db: Db,
    org_repo: PgOrganizationRepository,
    invitation_repo: PgInvitationRepository,
    membership_repo: PgMembershipRepository,
    mailer: Mailer,
    Path(invitation_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(inviter_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };
    // `find_by_id` is tenant-scoped, so this also proves `invitation_id`
    // belongs to the caller's active organization before anything is written.
    let Some(old) = invitation_repo.find_by_id(invitation_id).await? else {
        return Err(AutumnError::not_found_msg("Invitation not found"));
    };

    let tenant_id = old.tenant_id.clone();
    let email = old.email.clone();
    let role = old.role.clone();
    let raw_token = generate_raw_token();
    let token_hash = hash_api_token(&raw_token);
    let new: Invitation = db
        .tx(move |conn| {
            async move {
                // Lock the organization row FIRST — the same shared
                // serialization point `create_invitation`/
                // `accept_invitation`/`remove_all_memberships_on_conn` use
                // — before issuing the replacement invitation, for the same
                // reason `create_invitation` does (see its own comment):
                // this is another path that issues a fresh pending
                // invitation and must not slip past an in-flight account
                // deletion's cleanup (Codex review finding).
                let org_id: i64 = tenant_id.parse().map_err(|_| {
                    AutumnError::internal_server_error_msg("Corrupt organization id in invitation")
                })?;
                organizations::table
                    .find(org_id)
                    .for_update()
                    .select(Organization::as_select())
                    .first(conn)
                    .await
                    .optional()?;

                // Revalidate the caller's own membership now that the
                // organization lock is held — see `create_invitation`'s
                // identical fix for the full rationale (Codex review
                // finding).
                let inviter_membership: Option<Membership> = memberships::table
                    .filter(memberships::tenant_id.eq(&tenant_id))
                    .filter(memberships::user_id.eq(inviter_id))
                    .select(Membership::as_select())
                    .for_update()
                    .first(conn)
                    .await
                    .optional()?;
                let Some(inviter_membership) = inviter_membership else {
                    return Err(AutumnError::unauthorized_msg("no active organization"));
                };
                let Some(current_caller_role) = Role::parse(&inviter_membership.role) else {
                    return Err(AutumnError::forbidden_msg("insufficient permissions"));
                };
                if !current_caller_role.at_least(Role::Admin) {
                    return Err(AutumnError::forbidden_msg("insufficient permissions"));
                }

                // Lock and re-check status inside the transaction: a
                // double-clicked resend (or two concurrent ones) must not
                // both pass a stale "it's pending" read from before the
                // transaction and each mint their own replacement, which
                // would leave two live pending invitations instead of the
                // single refreshed one this endpoint promises.
                let current: Invitation = invitations::table
                    .filter(invitations::id.eq(invitation_id))
                    .for_update()
                    .select(Invitation::as_select())
                    .first(conn)
                    .await?;
                if current.status != "pending" {
                    return Err(AutumnError::conflict_msg(
                        "This invitation is no longer pending",
                    ));
                }
                // Same rule `create_invitation` enforces for a fresh Owner
                // invitation: resending must not become a back door for an
                // Admin to indefinitely renew an Owner grant they could
                // never have created themselves (Codex review finding).
                if current.role == Role::Owner.as_str() && current_caller_role != Role::Owner {
                    return Err(AutumnError::forbidden_msg(
                        "Only an owner can resend an invitation to join as owner",
                    ));
                }

                diesel::update(invitations::table.filter(invitations::id.eq(invitation_id)))
                    .set(invitations::status.eq("revoked"))
                    .execute(conn)
                    .await?;

                let new: Invitation = diesel::insert_into(invitations::table)
                    .values(&InsertInvitation {
                        tenant_id,
                        email,
                        role,
                        token_hash,
                        status: "pending".to_owned(),
                        invited_by_user_id: inviter_id,
                        expires_at: chrono::Utc::now().naive_utc()
                            + chrono::Duration::days(INVITATION_TTL_DAYS),
                    })
                    .returning(Invitation::as_returning())
                    .get_result(conn)
                    .await?;
                Ok::<_, AutumnError>(new)
            }
            .scope_boxed()
        })
        .await?;

    let Some(organization) = org_repo
        .find_by_id(parse_tenant_id(&new.tenant_id)?)
        .await?
    else {
        return Err(AutumnError::not_found_msg("Organization not found"));
    };
    let accept_url = format!("{}/invite/{raw_token}", app_base_url());
    InvitationMailer
        .send_invite(&mailer, new.email, organization.name, new.role, accept_url)
        .await?;

    Ok(Redirect::to("/members").into_response())
}
"#;

fn render_routes_members_rs() -> String {
    ROUTES_MEMBERS_RS.to_owned()
}

const ROUTES_MEMBERS_RS: &str = r#"//! Member-management surface: list members + pending invitations, change
//! role, remove member (issue #1261 AC7).
//!
//! Every mutating action is gated `require_role(&session, &membership_repo, Role::Admin)`,
//! but an `Admin` may only change or remove `Member`/`Admin` memberships —
//! never an `Owner`'s, no matter how many owners the organization has
//! (`Admin`+ can still invite/remove/change non-Owner roles regardless).
//! Without that, `Owner > Admin` wouldn't hold: an Admin could demote or
//! eject any owner as long as a second one existed. Only an `Owner` may
//! touch another `Owner`'s membership, and even an `Owner` can neither
//! demote nor remove the organization's *last* `Owner` — otherwise no one
//! could ever grant `Owner` again. Granting the `Owner` role itself
//! additionally requires the *caller* to already be an `Owner` — an `Admin`
//! gate alone would let any Admin promote themselves (or an ally) to
//! `Owner` and from there demote/remove the organization's real owners.
//!
//! Member rows are labeled by `user_id` only — this module doesn't know your
//! app's `User`/`users` shape (see `crate::teams`'s composition-boundary
//! note), so it can't join in an email/display name the way a
//! self-contained app can. Look `user_id` up against your own users table if
//! you want to show something friendlier.

use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::scoped_futures::ScopedFutureExt;
use autumn_web::security::CsrfToken;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::teams::models::{ChangeRoleForm, Invitation, Membership, Organization};
use crate::teams::repositories::{
    InvitationRepository, MembershipRepository, PgInvitationRepository, PgMembershipRepository,
};
use crate::teams::role::{Role, require_role};
use crate::teams::schema::{memberships, organizations};

use super::{csrf_value, minimal_page};

/// Count how many `owner` members remain in the active organization. Used to
/// block demoting/removing the last one.
fn owner_count(memberships: &[Membership]) -> usize {
    memberships
        .iter()
        .filter(|m| m.role == Role::Owner.as_str())
        .count()
}

#[get("/members")]
pub async fn list_members(
    session: Session,
    membership_repo: PgMembershipRepository,
    invitation_repo: PgInvitationRepository,
    csrf: Option<CsrfToken>,
) -> AutumnResult<Response> {
    // Any member (not just Admin+) can view the roster; only Admin+ sees the
    // management controls below.
    let caller_role = require_role(&session, &membership_repo, Role::Member).await?;

    let memberships = membership_repo.find_all().await?;
    let pending_invitations: Vec<Invitation> = invitation_repo
        .find_all()
        .await?
        .into_iter()
        .filter(|i| i.status == "pending")
        .collect();

    let can_manage = caller_role.at_least(Role::Admin);
    let owners = owner_count(&memberships);

    let page = minimal_page(
        "Members",
        html! {
            h1 { "Members" }

            @if can_manage {
                form action="/invitations" method="post" {
                    input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                    input name="email" type="email" required placeholder="teammate@example.com"
                          aria-label="Email to invite";
                    select name="role" aria-label="Role" {
                        option value="member" { "Member" }
                        option value="admin" { "Admin" }
                        option value="owner" { "Owner" }
                    }
                    button type="submit" { "Invite" }
                }
            }

            ul {
                @for membership in &memberships {
                    li {
                        span { "User " (membership.user_id) }
                        span { " (" (membership.role) ")" }
                        @if can_manage {
                            // Locked when the target is an Owner and either
                            // the caller isn't one too (only an Owner may
                            // touch another Owner's membership) or it's the
                            // organization's last Owner (never demotable/
                            // removable by anyone) — mirrors the server-side
                            // checks in `change_role`/`remove_member`.
                            @let locked = membership.role == Role::Owner.as_str()
                                && (caller_role != Role::Owner || owners <= 1);
                            form action={"/members/" (membership.id) "/role"} method="post" {
                                input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                select name="role" aria-label="Change role" disabled[locked] {
                                    option value="member" selected[membership.role == "member"] { "Member" }
                                    option value="admin" selected[membership.role == "admin"] { "Admin" }
                                    option value="owner" selected[membership.role == "owner"] { "Owner" }
                                }
                                button type="submit" disabled[locked] { "Update" }
                            }
                            form action={"/members/" (membership.id) "/remove"} method="post" {
                                input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                button type="submit" disabled[locked] { "Remove" }
                            }
                        }
                    }
                }
            }

            @if can_manage {
                h2 { "Pending invitations" }
                ul {
                    @for invitation in &pending_invitations {
                        li {
                            span { (invitation.email) }
                            span { " (" (invitation.role) ")" }
                            form action={"/invitations/" (invitation.id) "/resend"} method="post" {
                                input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                button type="submit" { "Resend" }
                            }
                            form action={"/invitations/" (invitation.id) "/revoke"} method="post" {
                                input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                button type="submit" { "Revoke" }
                            }
                        }
                    }
                    @if pending_invitations.is_empty() {
                        li { "No pending invitations." }
                    }
                }
            }
        },
    );
    Ok(page.into_response())
}

/// Change a member's role within the active organization. Gated `Admin` or
/// higher — but an `Admin` may only change a `Member`'s or `Admin`'s role,
/// never an existing `Owner`'s (granting `Owner`, same as touching one,
/// requires the caller to already be an `Owner`); refuses to demote the
/// last `Owner`.
#[post("/members/{id}/role")]
pub async fn change_role(
    session: Session,
    mut db: Db,
    Tenant(tenant_id): Tenant,
    membership_repo: PgMembershipRepository,
    Path(membership_id): Path<i64>,
    Form(form): Form<ChangeRoleForm>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(caller_id) = session.get("user_id").await.and_then(|s| s.parse::<i64>().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };
    let Some(new_role) = Role::parse(&form.role) else {
        return Err(AutumnError::unprocessable_msg("Unknown role"));
    };

    // Raw query (not the `tenant_scoped` repository's plain `find_all()`)
    // inside one locked transaction: reading the owner count and mutating
    // the target must be atomic, or two concurrent requests each demoting/
    // removing a *different* one of exactly two remaining owners could both
    // read the same two-owner snapshot, both pass the last-owner check
    // below, and leave the organization with none (Codex review finding —
    // mirrors `examples/teams/src/routes/members.rs`'s identical fix).
    db.tx(move |conn| {
        async move {
            // Lock the organization row FIRST — before any membership row
            // below — matching `remove_all_memberships_on_conn`'s lock
            // order. Without it, an in-flight account-deletion cleanup
            // (which locks the organization, then the departing owner's
            // own row, then a successor candidate's row, in that order)
            // could deadlock against this transaction's own unordered
            // `memberships` load: if this load happens to lock a
            // successor candidate's row before the owner's row cleanup
            // already holds, each transaction ends up waiting on a row
            // the other holds, and Postgres aborts one with no retry
            // (Codex review finding).
            let org_id: i64 = tenant_id.parse().map_err(|_| {
                AutumnError::internal_server_error_msg("Corrupt organization id in session")
            })?;
            organizations::table
                .find(org_id)
                .for_update()
                .select(Organization::as_select())
                .first(conn)
                .await
                .optional()?;

            let memberships: Vec<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .for_update()
                .select(Membership::as_select())
                .load(conn)
                .await?;

            // Revalidate the caller's own role from this same locked
            // snapshot instead of the `require_role` result computed
            // before the transaction — another request could have
            // demoted or removed the caller while this one waited to
            // acquire the lock above, and the pre-lock `caller_role`
            // would otherwise still be trusted (Codex review finding).
            let Some(caller_membership) = memberships.iter().find(|m| m.user_id == caller_id)
            else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(caller_role) = Role::parse(&caller_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }
            if new_role == Role::Owner && caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can grant the owner role",
                ));
            }

            let Some(target) = memberships.iter().find(|m| m.id == membership_id) else {
                return Err(AutumnError::not_found_msg("Member not found"));
            };
            // The `Owner > Admin` hierarchy must hold regardless of how many
            // owners exist: an Admin changing an Owner's role — even with
            // other owners still in place — is a demotion of a higher role
            // by a lower one.
            if target.role == Role::Owner.as_str() && caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can change another owner's role",
                ));
            }
            if target.role == Role::Owner.as_str()
                && new_role != Role::Owner
                && owner_count(&memberships) <= 1
            {
                return Err(AutumnError::conflict_msg(
                    "Cannot demote the last owner of an organization",
                ));
            }

            diesel::update(memberships::table.filter(memberships::id.eq(membership_id)))
                .set(memberships::role.eq(new_role.as_str()))
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Redirect::to("/members").into_response())
}

/// Remove a member from the active organization. Gated `Admin` or higher —
/// but an `Admin` may never remove an `Owner` (regardless of how many
/// owners exist); refuses to remove the last `Owner` even for a caller who
/// is themselves an `Owner`.
#[post("/members/{id}/remove")]
pub async fn remove_member(
    session: Session,
    mut db: Db,
    Tenant(tenant_id): Tenant,
    membership_repo: PgMembershipRepository,
    Path(membership_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(caller_id) = session.get("user_id").await.and_then(|s| s.parse::<i64>().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    // Same race as `change_role` (see its comment): the owner-count check
    // and the removal must run inside one locked transaction, or two
    // concurrent requests removing two different owners could both pass
    // the last-owner check and leave the organization with none (Codex
    // review finding).
    db.tx(move |conn| {
        async move {
            // Lock the organization row FIRST — before any membership row
            // below — matching `remove_all_memberships_on_conn`'s and
            // `change_role`'s lock order (see `change_role`'s comment for
            // the full deadlock rationale; Codex review finding).
            let org_id: i64 = tenant_id.parse().map_err(|_| {
                AutumnError::internal_server_error_msg("Corrupt organization id in session")
            })?;
            organizations::table
                .find(org_id)
                .for_update()
                .select(Organization::as_select())
                .first(conn)
                .await
                .optional()?;

            let memberships: Vec<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .for_update()
                .select(Membership::as_select())
                .load(conn)
                .await?;

            // Revalidate the caller's own role from this same locked
            // snapshot instead of the `require_role` result computed
            // before the transaction (Codex review finding — see
            // `change_role`'s identical fix for the full rationale).
            let Some(caller_membership) = memberships.iter().find(|m| m.user_id == caller_id)
            else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(caller_role) = Role::parse(&caller_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }

            let Some(target) = memberships.iter().find(|m| m.id == membership_id) else {
                return Err(AutumnError::not_found_msg("Member not found"));
            };
            if target.role == Role::Owner.as_str() && caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can remove another owner",
                ));
            }
            if target.role == Role::Owner.as_str() && owner_count(&memberships) <= 1 {
                return Err(AutumnError::conflict_msg(
                    "Cannot remove the last owner of an organization",
                ));
            }

            diesel::delete(memberships::table.filter(memberships::id.eq(membership_id)))
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Redirect::to("/members").into_response())
}
"#;

// ── Migration SQL ────────────────────────────────────────────────────────
//
// Postgres DDL only, mirroring `examples/teams`'s migration byte-for-byte
// (minus its `users` table — a generated app already has its own from
// `autumn generate auth`). Unlike the `model`/`scaffold`/`migration`
// generators, `teams` has no `SQLite`-dialect variant (issue #1614's
// backend-aware DDL work did not extend to this generator).

const MIGRATION_UP: &str = r"-- Organizations, memberships, and invitations for team membership (issue
-- #1261). Deliberately does NOT create (or reference via REFERENCES) a
-- `users` table — your app already has its own from `autumn generate auth`;
-- `memberships.user_id` / `invitations.invited_by_user_id` are bare BIGINT
-- with no FK, so this migration has no ordering dependency on when your
-- users table was created.

-- An organization (tenant). Creating one makes the creator an `owner`
-- member (see `src/teams/routes/organizations.rs`).
CREATE TABLE organizations (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT      NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

-- The user <-> organization join row, carrying the closed `role` enum
-- (`owner` | `admin` | `member`, see `src/teams/role.rs`). `tenant_scoped`
-- on `#[repository(Membership, ...)]` filters every read/write by the
-- active organization at the SQL level (issue #695's row-level
-- multi-tenancy seam, not a second isolation mechanism) — but the macro's
-- generated queries unconditionally filter on a column literally named
-- `tenant_id` (`TEXT`), so this holds the organization's id in its string
-- form rather than a typed FK; application code parses it back to `i64`
-- where it needs to look up the `Organization` row itself. The UNIQUE
-- constraint is the idempotency backstop for a double-clicked invitation
-- accept: a second INSERT for the same (tenant_id, user_id) pair fails
-- closed instead of creating a duplicate membership.
CREATE TABLE memberships (
    id         BIGSERIAL PRIMARY KEY,
    tenant_id  TEXT      NOT NULL,
    user_id    BIGINT    NOT NULL,
    role       TEXT      NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    created_at TIMESTAMP NOT NULL DEFAULT NOW(),
    UNIQUE (tenant_id, user_id)
);
CREATE INDEX idx_memberships_tenant ON memberships (tenant_id);
CREATE INDEX idx_memberships_user ON memberships (user_id);

-- A single-use, expiring, cryptographically-random email invitation. Only
-- the SHA-256 hash of the token is stored (`hash_api_token`) — never the
-- raw token — so a database leak cannot be replayed as an accept link.
-- `status` starts `pending`; accepting sets it to `accepted`, revoking sets
-- it to `revoked`. Both are terminal: the accept handler checks
-- `status = 'pending'` AND `expires_at > now()` before creating a
-- membership, so an expired/revoked/already-accepted token renders a clear
-- error instead of a second membership row or a panic. The partial unique
-- index below is the concurrency backstop for `create_invitation`: two
-- concurrent requests for the same (tenant_id, email) each revoke-then-insert
-- in their own transaction, so a `SELECT ... WHERE status = 'pending'` alone
-- cannot serialize them (no prior row to lock when none exists yet) — the
-- second transaction's INSERT fails closed against this index instead of
-- leaving two live pending tokens for the same invitee.
CREATE TABLE invitations (
    id                 BIGSERIAL PRIMARY KEY,
    tenant_id          TEXT      NOT NULL,
    email              TEXT      NOT NULL,
    role               TEXT      NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    token_hash         TEXT      NOT NULL UNIQUE,
    status             TEXT      NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'accepted', 'revoked')),
    invited_by_user_id BIGINT    NOT NULL,
    expires_at         TIMESTAMP NOT NULL,
    created_at         TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_invitations_tenant ON invitations (tenant_id);
CREATE INDEX idx_invitations_email ON invitations (email);
CREATE UNIQUE INDEX idx_invitations_pending_email ON invitations (tenant_id, email) WHERE status = 'pending';
";

const MIGRATION_DOWN: &str = "DROP TABLE IF EXISTS invitations;\nDROP TABLE IF EXISTS memberships;\nDROP TABLE IF EXISTS organizations;\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use std::fs;
    use tempfile::TempDir;

    fn default_main() -> &'static str {
        r#"use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str { "ok" }

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index])
        .run()
        .await;
}
"#
    }

    fn project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), default_main()).unwrap();
        tmp
    }

    // ── (a) plain generate creates all expected files with expected markers ─

    #[test]
    fn plan_errors_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_teams(tmp.path(), "20260101000000", false).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn generates_all_expected_files_with_content_markers() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let role = fs::read_to_string(tmp.path().join("src/teams/role.rs")).unwrap();
        assert!(role.contains("pub enum Role"), "{role}");
        assert!(role.contains("Owner"), "{role}");
        assert!(role.contains("Admin"), "{role}");
        assert!(role.contains("Member"), "{role}");
        assert!(role.contains("pub async fn require_role"), "{role}");
        assert!(
            role.contains("pub async fn establish_org_session"),
            "{role}"
        );

        let models = fs::read_to_string(tmp.path().join("src/teams/models.rs")).unwrap();
        assert!(models.contains("pub struct Organization"), "{models}");
        assert!(models.contains("pub struct Membership"), "{models}");
        assert!(models.contains("pub struct Invitation"), "{models}");

        let mailer =
            fs::read_to_string(tmp.path().join("src/teams/mailers/invitation_mailer.rs")).unwrap();
        assert!(mailer.contains("pub struct InvitationMailer"), "{mailer}");
        assert!(mailer.contains("#[mailer]"), "{mailer}");

        assert!(tmp.path().join("src/teams/mod.rs").exists());
        assert!(tmp.path().join("src/teams/schema.rs").exists());
        assert!(tmp.path().join("src/teams/repositories.rs").exists());
        assert!(tmp.path().join("src/teams/mailers/mod.rs").exists());
        assert!(tmp.path().join("src/teams/routes/mod.rs").exists());
        assert!(
            tmp.path()
                .join("src/teams/routes/organizations.rs")
                .exists()
        );
        assert!(tmp.path().join("src/teams/routes/invitations.rs").exists());
        assert!(tmp.path().join("src/teams/routes/members.rs").exists());

        let migration_dir = tmp.path().join("migrations/20260101000000_create_teams");
        let up = fs::read_to_string(migration_dir.join("up.sql")).unwrap();
        assert!(up.contains("CREATE TABLE organizations"), "{up}");
        assert!(up.contains("CREATE TABLE memberships"), "{up}");
        assert!(up.contains("CREATE TABLE invitations"), "{up}");
        let down = fs::read_to_string(migration_dir.join("down.sql")).unwrap();
        assert!(
            down.contains("DROP TABLE IF EXISTS organizations"),
            "{down}"
        );
    }

    /// Every generated mutating form must embed a CSRF hidden field —
    /// otherwise `CsrfLayer` (on by default, including Autumn's
    /// production-profile smart default) rejects every one of these POSTs
    /// before the handler runs (Codex review finding).
    #[test]
    fn generated_forms_embed_csrf_token() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let members = fs::read_to_string(tmp.path().join("src/teams/routes/members.rs")).unwrap();

        let csrf_field_count = invitations
            .matches("input type=\"hidden\" name=\"_csrf\" value=(csrf_value(&csrf));")
            .count()
            + members
                .matches("input type=\"hidden\" name=\"_csrf\" value=(csrf_value(&csrf));")
                .count();
        assert_eq!(
            csrf_field_count,
            6,
            "expected one CSRF field per generated form (accept, invite, role, remove, \
             resend, revoke): invitations.rs has {}, members.rs has {}",
            invitations.matches("form action=").count(),
            members.matches("form action=").count()
        );
        assert!(
            invitations.contains("csrf: Option<CsrfToken>"),
            "show_invitation must extract Option<CsrfToken>: {invitations}"
        );
        assert!(
            members.contains("csrf: Option<CsrfToken>"),
            "list_members must extract Option<CsrfToken>: {members}"
        );
    }

    // ── (b) --dry-run writes nothing ─────────────────────────────────────

    #[test]
    fn dry_run_writes_nothing_and_prints_header() {
        let tmp = project();
        let plan = plan_teams(tmp.path(), "20260101000000", false).unwrap();
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();

        assert!(!tmp.path().join("src/teams").exists());
        assert!(!tmp.path().join("migrations").exists());
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(!main.contains("mod teams;"), "{main}");
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(!cargo.contains("\"mail\""), "{cargo}");
    }

    // ── (c) collision without --force ────────────────────────────────────

    #[test]
    fn second_run_without_force_collides() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let err = plan_teams(tmp.path(), "20260102000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap_err();
        assert!(matches!(err, GenerateError::Collisions(_)));
    }

    // ── (d) --force overwrites ───────────────────────────────────────────

    #[test]
    fn force_overwrites_existing_files() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_teams(tmp.path(), "20260102000000", false)
            .unwrap()
            .execute(Flags {
                dry_run: false,
                force: true,
            })
            .unwrap();

        // The second run reuses the first run's migration directory — a
        // singleton scaffold like `teams` must not mint a second
        // `*_create_teams` directory on `--force` (two `CREATE TABLE
        // organizations` migrations would fail `autumn migrate` on the
        // duplicate table).
        assert!(
            tmp.path()
                .join("migrations/20260101000000_create_teams")
                .exists()
        );
        assert!(
            !tmp.path()
                .join("migrations/20260102000000_create_teams")
                .exists()
        );
    }

    /// Codex review finding: `--force` regeneration must reuse the existing
    /// `*_create_teams` migration in place, not mint a second one.
    #[test]
    fn force_reuses_existing_migration_up_and_down_sql() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        // Hand-edit the migration to prove the second run actually reuses
        // (overwrites in place) this exact directory rather than leaving it
        // untouched alongside a fresh one.
        let up_path = tmp
            .path()
            .join("migrations/20260101000000_create_teams/up.sql");
        fs::write(&up_path, "-- stale\n").unwrap();

        plan_teams(tmp.path(), "20260102000000", false)
            .unwrap()
            .execute(Flags {
                dry_run: false,
                force: true,
            })
            .unwrap();

        let migrations_root = tmp.path().join("migrations");
        let entries: Vec<_> = fs::read_dir(&migrations_root)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one *_create_teams migration directory must exist after --force: {entries:?}"
        );
        let up = fs::read_to_string(&up_path).unwrap();
        assert!(
            up.contains("CREATE TABLE organizations"),
            "the original directory's up.sql must be refreshed, not left stale: {up}"
        );
    }

    // ── (e) Cargo.toml gets the mail feature ─────────────────────────────

    #[test]
    fn cargo_toml_gets_mail_feature() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("\"mail\""), "{cargo}");
    }

    /// The generated routes render `Markup` via `html!`/`maud::DOCTYPE`
    /// (`minimal_page`), so the `maud` Cargo feature/dependency must be
    /// ensured too — not just `mail` — even on a project shaped like
    /// `autumn new --api`, whose starter strips both (Codex review finding).
    #[test]
    fn cargo_toml_gets_maud_feature_and_dependency_for_api_style_project() {
        let tmp = TempDir::new().unwrap();
        // Mirrors `--api`'s starter: no direct `maud` dependency, no `maud`
        // autumn-web feature (see `new.rs`'s `DEFAULT_MINUS_HTML_VIEW_STACK`).
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = { version = \"0.6\", default-features = false }\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), default_main()).unwrap();

        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("\"maud\""), "{cargo}");
        assert!(cargo.contains("maud ="), "{cargo}");
    }

    /// `teams`' migration/model templates are fixed Postgres DDL with no
    /// SQLite-aware rendering path — a SQLite-backed project must be
    /// rejected at generate time with an actionable error, not emit a
    /// migration `autumn migrate` would fail to apply (Codex review finding).
    #[test]
    fn plan_rejects_sqlite_backend() {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let tmp = project();
                fs::write(
                    tmp.path().join("autumn.toml"),
                    "[database]\nprimary_url = \"sqlite://app.db\"\n",
                )
                .unwrap();

                let err = plan_teams(tmp.path(), "20260101000000", false).unwrap_err();
                assert!(matches!(err, GenerateError::Config(_)), "{err:?}");
                assert!(err.to_string().contains("Postgres"), "{err}");
                assert!(!tmp.path().join("src/teams").exists());
            },
        );
    }

    /// The `SQLite` rejection must not block `autumn destroy teams` cleaning
    /// up a project that somehow already has `teams` generated (e.g. from
    /// before this backend check existed).
    #[test]
    fn destroy_is_not_blocked_by_sqlite_rejection() {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let tmp = project();
                plan_teams(tmp.path(), "20260101000000", false)
                    .unwrap()
                    .execute(Flags::default())
                    .unwrap();

                fs::write(
                    tmp.path().join("autumn.toml"),
                    "[database]\nprimary_url = \"sqlite://app.db\"\n",
                )
                .unwrap();

                plan_teams(tmp.path(), "20260101000000", true)
                    .unwrap()
                    .revert(Flags::default())
                    .unwrap();
                assert!(!tmp.path().join("src/teams").exists());
            },
        );
    }

    // ── (f) main.rs wiring is idempotent across two --force runs ────────

    #[test]
    fn main_rs_gets_mod_and_routes_wired_idempotently() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("mod teams;"), "{main}");
        for entry in teams_route_entries() {
            assert!(main.contains(&entry), "missing {entry} in:\n{main}");
        }

        // Re-running with --force must not duplicate the mod declaration or
        // any route entry.
        plan_teams(tmp.path(), "20260103000000", false)
            .unwrap()
            .execute(Flags {
                dry_run: false,
                force: true,
            })
            .unwrap();
        let main_again = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(main_again.matches("mod teams;").count(), 1, "{main_again}");
        for entry in teams_route_entries() {
            assert_eq!(
                main_again.matches(&entry).count(),
                1,
                "duplicated {entry} in:\n{main_again}"
            );
        }
    }

    // ── warnings ─────────────────────────────────────────────────────────

    #[test]
    fn plan_warns_about_tenancy_config() {
        let tmp = project();
        let plan = plan_teams(tmp.path(), "20260101000000", false).unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("[tenancy]")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn plan_warns_about_invite_public_path_not_invitations() {
        let tmp = project();
        let plan = plan_teams(tmp.path(), "20260101000000", false).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("public_paths") && w.contains("/invite")),
            "{:?}",
            plan.warnings
        );
    }

    // ── security: invite-accept routes use a `/invite` prefix, not
    //    `/invitations` (a shared prefix would let `[tenancy] public_paths`
    //    accidentally exempt the Admin-only create/revoke/resend routes) ───

    #[test]
    fn invitations_route_uses_invite_prefix_for_invitee_facing_routes() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        assert!(
            invitations.contains(r#"#[get("/invite/{token}")]"#),
            "{invitations}"
        );
        assert!(
            invitations.contains(r#"#[post("/invite/{token}/accept")]"#),
            "{invitations}"
        );
        // Admin-only mutation routes must stay under a distinct prefix.
        assert!(
            invitations.contains(r#"#[post("/invitations")]"#),
            "{invitations}"
        );
        assert!(
            invitations.contains(r#"#[post("/invitations/{id}/revoke")]"#),
            "{invitations}"
        );
        assert!(
            invitations.contains(r#"#[post("/invitations/{id}/resend")]"#),
            "{invitations}"
        );
        assert!(
            !invitations.contains(r#"#[get("/invitations/{token}")]"#),
            "{invitations}"
        );
        assert!(
            !invitations.contains(r#"#[post("/invitations/{token}/accept")]"#),
            "{invitations}"
        );
    }

    #[test]
    fn invitations_route_sends_invite_mail_synchronously() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        assert!(invitations.contains(".send_invite("), "{invitations}");
        assert!(
            !invitations.contains(".deliver_later_invite("),
            "{invitations}"
        );
    }

    /// `resend_invitation` must lock and re-check the source row's status
    /// inside its transaction before minting a replacement — otherwise a
    /// double-clicked (or concurrent) resend can create two live pending
    /// invitations instead of the single refreshed one it promises (Codex
    /// review finding).
    #[test]
    fn resend_invitation_locks_and_rechecks_pending_status() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        assert!(invitations.contains(".for_update()"), "{invitations}");
        assert!(
            invitations.contains("This invitation is no longer pending"),
            "{invitations}"
        );
    }

    // ── security: an Admin cannot self-promote (or promote an ally) to
    //    Owner via invite-creation or role-change ────────────────────────

    #[test]
    fn create_invitation_rejects_owner_grant_from_non_owner_caller() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        assert!(
            invitations.contains(
                "let caller_role = require_role(&session, &membership_repo, Role::Admin).await?;"
            ),
            "{invitations}"
        );
        assert!(
            invitations.contains("role == Role::Owner && caller_role != Role::Owner"),
            "{invitations}"
        );
        assert!(
            invitations.contains("Only an owner can invite someone as owner"),
            "{invitations}"
        );
    }

    #[test]
    fn change_role_rejects_owner_grant_from_non_owner_caller() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let members = fs::read_to_string(tmp.path().join("src/teams/routes/members.rs")).unwrap();
        assert!(
            members.contains("require_role(&session, &membership_repo, Role::Admin).await?;"),
            "{members}"
        );
        assert!(
            members.contains("new_role == Role::Owner && caller_role != Role::Owner"),
            "{members}"
        );
        assert!(
            members.contains("Only an owner can grant the owner role"),
            "{members}"
        );
    }

    /// `Owner > Admin` must hold regardless of headcount: an Admin must
    /// never be able to demote or remove an Owner, even with multiple
    /// owners present (only the last-owner protection is about headcount).
    #[test]
    fn members_route_blocks_admin_from_touching_an_owner_membership() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let members = fs::read_to_string(tmp.path().join("src/teams/routes/members.rs")).unwrap();
        assert!(
            members.contains("Only an owner can change another owner's role"),
            "{members}"
        );
        assert!(
            members.contains("Only an owner can remove another owner"),
            "{members}"
        );
    }

    /// A second, independent pending invitation to an already-a-member email
    /// must be consumed (marked accepted) on accept, not left dangling —
    /// otherwise the lingering token stays redeemable indefinitely.
    #[test]
    fn accept_invitation_consumes_lingering_invite_for_existing_member() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        assert!(
            invitations.contains("if invitation.status == \"pending\""),
            "{invitations}"
        );
    }

    /// A second `create_invitation` call for the same organization/email
    /// while an earlier one is still pending must not leave two live
    /// tokens — the older one must be revoked in the same transaction as
    /// the new one is inserted (Codex review finding).
    #[test]
    fn create_invitation_rejects_rather_than_replaces_a_pending_invitation_for_same_email() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let create_start = invitations.find("pub async fn create_invitation(").unwrap();
        let create_end = invitations
            .find("// ── Accept (show) ")
            .unwrap_or(invitations.len());
        let body = &invitations[create_start..create_end];
        assert!(
            body.contains("if existing_pending.is_some()")
                && body.contains("AutumnError::conflict_msg("),
            "{body}"
        );
        assert!(
            !body.contains(".set(invitations::status.eq(\"revoked\"))"),
            "create_invitation must reject rather than revoke a prior pending invitation: {body}"
        );
    }

    /// `provision_default_organization` must never establish the org
    /// session itself: `autumn generate auth`'s scaffolded `signup` creates
    /// an account without logging it in (gated on email confirmation), and
    /// this is the natural place a caller would append the call — doing so
    /// would silently authenticate an unconfirmed account (Codex review
    /// finding).
    #[test]
    fn provision_default_organization_does_not_establish_session() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations
                .contains("pub async fn provision_default_organization(user_id: i64, db: &mut Db)"),
            "{organizations}"
        );
        let body_start = organizations
            .find("pub async fn provision_default_organization")
            .unwrap();
        let body_end = organizations
            .find("pub async fn create_organization")
            .unwrap();
        let body = &organizations[body_start..body_end];
        assert!(
            !body.contains("establish_org_session"),
            "provision_default_organization must not establish a session: {body}"
        );
    }

    /// `provision_default_organization_on_conn` must exist and take a raw
    /// `conn` (not its own `Db`), so a caller who needs full 3-way atomicity
    /// (account + organization + membership) can run it inside their own
    /// `db.tx(...)` block alongside their own user insert — a plain
    /// standalone call can never cover an insert that already committed
    /// before it ran (Codex review finding).
    #[test]
    fn provision_default_organization_on_conn_exists_for_full_atomicity() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations.contains("pub async fn provision_default_organization_on_conn("),
            "{organizations}"
        );
        assert!(
            organizations.contains("conn: &mut autumn_web::db::PooledConnection"),
            "{organizations}"
        );
    }

    // ── generate then destroy round-trips main.rs/Cargo.toml ────────────

    #[test]
    fn generate_then_destroy_round_trips_main_rs_and_cargo_toml() {
        let tmp = project();
        let main_path = tmp.path().join("src/main.rs");
        let cargo_path = tmp.path().join("Cargo.toml");
        let original_main = fs::read_to_string(&main_path).unwrap();
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();

        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_teams(tmp.path(), "20260101000000", true)
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/teams").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    // ── seventh round of Codex review findings ──────────────────────────

    /// `accept_invitation`'s existing-membership shortcut must not run
    /// before a revoked/expired-pending invitation is rejected — otherwise
    /// posting a dead token for an org the caller already belongs to some
    /// other way would silently succeed and switch their active
    /// organization instead of surfacing the documented gone error (Codex
    /// review finding).
    #[test]
    fn accept_invitation_rejects_revoked_or_expired_before_existing_member_shortcut() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let reject_pos = invitations
            .find("invitation.status == \"revoked\"")
            .unwrap_or_else(|| panic!("missing revoked/expired guard: {invitations}"));
        let existing_pos = invitations
            .find("let existing: Option<Membership> = memberships::table")
            .unwrap_or_else(|| panic!("missing existing-membership lookup: {invitations}"));
        assert!(
            reject_pos < existing_pos,
            "revoked/expired check must run before the existing-membership shortcut"
        );
    }

    /// Two concurrent `create_invitation` calls for the same tenant/email
    /// must not both leave a live pending token: the transaction's
    /// revoke-then-insert alone can't serialize two requests that both see
    /// no prior pending row, so a partial unique index is the backstop —
    /// and the generated handler must translate that constraint's
    /// violation into a friendly conflict rather than a raw 500 (Codex
    /// review finding).
    #[test]
    fn migration_has_partial_unique_index_for_pending_invitation_email() {
        assert!(
            MIGRATION_UP.contains(
                "CREATE UNIQUE INDEX idx_invitations_pending_email ON invitations (tenant_id, email) WHERE status = 'pending';"
            ),
            "{MIGRATION_UP}"
        );
    }

    #[test]
    fn create_invitation_translates_pending_email_unique_violation_to_conflict() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        assert!(
            invitations.contains("autumn_web::error::unique_violation_field")
                && invitations.contains("\"idx_invitations_pending_email\""),
            "{invitations}"
        );
    }

    /// Deleting an account through `autumn generate auth` doesn't cascade
    /// into `teams`' unrelated `memberships` table (no FK — see
    /// `schema.rs`'s doc comment), so a stale session cookie on another
    /// device could otherwise keep passing `require_role` for a
    /// now-deleted account indefinitely. `remove_all_memberships` is the
    /// account-deletion half of the integration seam that closes this gap
    /// (Codex review finding).
    #[test]
    fn remove_all_memberships_exists_for_account_deletion_integration() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations
                .contains("pub async fn remove_all_memberships(user_id: i64, db: &mut Db)"),
            "{organizations}"
        );
        assert!(
            organizations.contains(
                "diesel::delete(memberships::table.filter(memberships::id.eq(own_membership.id)))"
            ),
            "{organizations}"
        );
    }

    /// `remove_all_memberships` bulk-deletes every membership a user holds —
    /// if that user is the sole `Owner` of an organization with other
    /// members still in it, deleting the membership outright (without first
    /// promoting a successor) would silently bypass the exact same
    /// last-owner protection `change_role`/`remove_member` already enforce
    /// for interactive requests, leaving the organization with Admins/
    /// Members but no one who can ever grant a replacement Owner (Codex
    /// review finding).
    #[test]
    fn remove_all_memberships_promotes_a_successor_before_dropping_the_last_owner() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations.contains(".filter(memberships::role.eq(Role::Owner.as_str()))"),
            "{organizations}"
        );
        assert!(
            organizations.contains(".filter(memberships::role.eq(Role::Admin.as_str()))"),
            "{organizations}"
        );
        assert!(
            organizations.contains(".set(memberships::role.eq(Role::Owner.as_str()))"),
            "{organizations}"
        );
        // The promotion query (successor lookup) must run, and complete,
        // before this user's own membership row is deleted — verified by
        // source order, since the delete would otherwise remove the very
        // row being promoted.
        let promote_pos = organizations
            .find(".filter(memberships::role.eq(Role::Admin.as_str()))")
            .unwrap();
        let delete_pos = organizations
            .find(
                "diesel::delete(memberships::table.filter(memberships::id.eq(own_membership.id)))",
            )
            .unwrap();
        assert!(promote_pos < delete_pos);
    }

    /// If the departing user shares an organization with another live
    /// `Owner`, nobody should be promoted at all — the earlier fix's loop
    /// picked a successor unconditionally, which would grant an unrequested
    /// second `Owner` even when ownership didn't need transferring (Codex
    /// review finding).
    #[test]
    fn remove_all_memberships_skips_promotion_when_another_owner_remains() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        let other_owner_pos = organizations
            .find("let other_owner: Option<Membership> = memberships::table")
            .unwrap_or_else(|| panic!("missing other-owner guard: {organizations}"));
        assert!(
            organizations.contains("if other_owner.is_none() {"),
            "{organizations}"
        );
        let admin_lookup_pos = organizations
            .find(".filter(memberships::role.eq(Role::Admin.as_str()))")
            .unwrap();
        assert!(
            other_owner_pos < admin_lookup_pos,
            "the other-owner check must run before any successor is looked up"
        );
    }

    /// When the departing user is the sole Owner AND the only member left,
    /// the organization is about to have zero memberships permanently — its
    /// pending invitations must be revoked too, or a later accept could
    /// repopulate a headless organization no one can ever manage or promote
    /// a new Owner in (Codex review finding).
    #[test]
    fn remove_all_memberships_revokes_pending_invitations_when_no_successor_exists() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations
                .contains("use crate::teams::schema::{invitations, memberships, organizations};"),
            "{organizations}"
        );
        assert!(
            organizations.contains(".filter(invitations::tenant_id.eq(&own_membership.tenant_id))")
                && organizations.contains(".filter(invitations::status.eq(\"pending\"))")
                && organizations.contains(".set(invitations::status.eq(\"revoked\"))"),
            "{organizations}"
        );
    }

    /// `remove_all_memberships` opens its own transaction, independent of
    /// whatever an account-deletion handler does around it — a failure on
    /// either side of that boundary leaves the account and its memberships
    /// out of sync. `remove_all_memberships_on_conn` is the shared-connection
    /// building block that lets a caller compose cleanup with their own
    /// account `DELETE` in one outer transaction instead (Codex review
    /// finding, mirrors `provision_default_organization`/
    /// `provision_default_organization_on_conn`'s identical split).
    #[test]
    fn remove_all_memberships_on_conn_exists_for_full_atomicity() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations.contains("pub async fn remove_all_memberships_on_conn("),
            "{organizations}"
        );
        assert!(
            organizations.contains("conn: &mut autumn_web::db::PooledConnection"),
            "{organizations}"
        );
        assert!(
            organizations.contains(
                "db.tx(move |conn| remove_all_memberships_on_conn(conn, user_id).scope_boxed())"
            ),
            "{organizations}"
        );
    }

    /// The `caller_email_lookup` module's hard-coded `users` table name is a
    /// placeholder, not introspected from the project — a project generated
    /// with a custom `autumn generate auth` resource name (producing a
    /// differently-named table) needs to edit it manually, or invitation
    /// acceptance fails at runtime. This must be surfaced at generate time,
    /// not only in prose docs a user might not read (Codex review finding).
    #[test]
    fn plan_warns_about_custom_auth_resource_table_name() {
        let tmp = project();
        let plan = plan_teams(tmp.path(), "20260101000000", false).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("caller_email_lookup")
                    && w.contains("autumn generate auth Account")),
            "{:?}",
            plan.warnings
        );
    }

    /// `remove_all_memberships_on_conn` and `accept_invitation` both touch a
    /// given organization's memberships/invitations, from two entirely
    /// separate `db.tx` transactions — one triggered by account deletion,
    /// one by a concurrent invitation accept. Without a lock shared between
    /// them, the cleanup could decide "no successor exists" a moment before
    /// the accept commits a brand-new membership, then delete the sole
    /// Owner anyway. Both must lock the `organizations` row for that tenant
    /// FIRST — before touching memberships or invitations — so whichever
    /// runs first completes wholly before the other proceeds, and in the
    /// same order in both functions, so they serialize instead of
    /// deadlocking (Codex review finding).
    #[test]
    fn accept_invitation_and_remove_all_memberships_lock_organizations_first() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let accept_org_lock_pos = invitations
            .find("organizations::table\n                    .find(org_id)\n                    .for_update()")
            .unwrap_or_else(|| panic!("missing org-row lock in accept_invitation: {invitations}"));
        let accept_invitation_lock_pos = invitations
            .find("let invitation: Invitation = invitations::table\n                    .filter(invitations::id.eq(invitation_id))\n                    .for_update()")
            .unwrap_or_else(|| panic!("missing invitation-row lock: {invitations}"));
        assert!(
            accept_org_lock_pos < accept_invitation_lock_pos,
            "accept_invitation must lock organizations before invitations"
        );

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        let cleanup_org_lock_pos = organizations
            .find("organizations::table\n            .find(org_id)\n            .for_update()")
            .unwrap_or_else(|| {
                panic!("missing org-row lock in remove_all_memberships_on_conn: {organizations}")
            });
        let cleanup_other_owner_pos = organizations
            .find("let other_owner: Option<Membership> = memberships::table")
            .unwrap();
        assert!(
            cleanup_org_lock_pos < cleanup_other_owner_pos,
            "remove_all_memberships_on_conn must lock organizations before checking for a successor"
        );
    }

    /// `remove_all_memberships_on_conn`'s initial scan for which
    /// organizations to process must NOT lock the rows it finds — locking
    /// a user's own membership row before the organization row created a
    /// real deadlock: two owners of the *same* organization deleting their
    /// accounts concurrently would each lock their own row first, then
    /// each need the org lock *and* the other's row (via the other-owner
    /// check), a lock-order cycle Postgres can only break by aborting one
    /// deletion (Codex review finding).
    #[test]
    fn remove_all_memberships_on_conn_does_not_lock_the_initial_scan() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations.contains(
                "let member_tenant_ids: Vec<String> = memberships::table\n        .filter(memberships::user_id.eq(user_id))\n        .select(memberships::tenant_id)\n        .order(memberships::tenant_id.asc())\n        .load(conn)\n        .await?;"
            ),
            "{organizations}"
        );
        // The user's own membership row for each organization must be
        // re-locked only after the organization lock.
        let org_lock_pos = organizations
            .find("organizations::table\n            .find(org_id)\n            .for_update()")
            .unwrap();
        let own_row_relock_pos = organizations
            .find("let Some(own_membership) = memberships::table")
            .unwrap();
        assert!(org_lock_pos < own_row_relock_pos);
    }

    /// `create_invitation` and `resend_invitation` both mint a fresh
    /// pending invitation — each must lock the organization row first, the
    /// same shared serialization point `accept_invitation`/
    /// `remove_all_memberships_on_conn` use. Without it, an in-flight
    /// account deletion's cleanup could decide "no successor exists,"
    /// revoke every pending invitation, and delete the last Owner, while
    /// one of these uncoordinated transactions inserts a fresh invitation
    /// the cleanup never saw — repopulating the organization without an
    /// Owner once accepted (Codex review finding).
    #[test]
    fn create_and_resend_invitation_lock_organizations_before_issuing_a_token() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();

        let create_org_lock_pos = invitations
            .find("organizations::table\n                .find(org_id)\n                .for_update()")
            .unwrap_or_else(|| panic!("missing org-row lock in create_invitation: {invitations}"));
        let create_insert_pos = invitations
            .find("diesel::insert_into(invitations::table)")
            .unwrap();
        assert!(
            create_org_lock_pos < create_insert_pos,
            "create_invitation must lock organizations before inserting"
        );

        let resend_org_lock_pos = invitations
            .find("organizations::table\n                    .find(org_id)\n                    .for_update()")
            .unwrap_or_else(|| panic!("missing org-row lock in resend_invitation: {invitations}"));
        let resend_status_check_pos = invitations
            .find("if current.status != \"pending\"")
            .unwrap();
        assert!(
            resend_org_lock_pos < resend_status_check_pos,
            "resend_invitation must lock organizations before its own invitation-status recheck"
        );
    }

    /// `change_role`/`remove_member` must read the owner count and perform
    /// the mutation inside one locked transaction, not a separate
    /// `find_all()` followed by an independent `.update()`/
    /// `.delete_by_id()` — otherwise two concurrent requests each
    /// demoting/removing a *different* one of exactly two remaining owners
    /// could both read the same two-owner snapshot and leave the
    /// organization with none (Codex review finding — mirrors
    /// `examples/teams/src/routes/members.rs`'s identical, earlier fix).
    #[test]
    fn generated_change_role_and_remove_member_lock_memberships_transactionally() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let members = fs::read_to_string(tmp.path().join("src/teams/routes/members.rs")).unwrap();
        assert!(
            !members.contains("membership_repo.update("),
            "change_role must no longer use the non-transactional repository update: {members}"
        );
        assert!(
            !members.contains("membership_repo.delete_by_id("),
            "remove_member must no longer use the non-transactional repository delete: {members}"
        );
        assert_eq!(
            members.matches("db.tx(move |conn| {").count(),
            2,
            "both change_role and remove_member must run inside a locked transaction: {members}"
        );
        assert_eq!(
            members
                .matches(
                    ".filter(memberships::tenant_id.eq(&tenant_id))\n                .for_update()"
                )
                .count(),
            2,
            "both handlers must lock every membership row in the active organization: {members}"
        );
    }

    /// `change_role`/`remove_member` must derive the caller's role from the
    /// *locked* membership snapshot taken inside the transaction, not the
    /// `require_role` result computed before it — another request could
    /// demote or remove the caller while this one waited to acquire the
    /// lock (Codex review finding).
    #[test]
    fn change_role_and_remove_member_revalidate_caller_from_locked_snapshot() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let members = fs::read_to_string(tmp.path().join("src/teams/routes/members.rs")).unwrap();
        assert_eq!(
            members
                .matches("memberships.iter().find(|m| m.user_id == caller_id)")
                .count(),
            2,
            "both handlers must re-derive the caller's membership from the locked snapshot: {members}"
        );
        assert!(
            members.matches("caller_role.at_least(Role::Admin)").count() >= 2,
            "{members}"
        );
    }

    /// `create_invitation` and `resend_invitation` must revalidate the
    /// inviter's own live membership after acquiring the organization lock
    /// — the pre-transaction `require_role` result could be stale by the
    /// time the lock is actually granted (Codex review finding).
    #[test]
    fn create_and_resend_invitation_revalidate_inviter_after_organization_lock() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        assert_eq!(
            invitations
                .matches("let inviter_membership: Option<Membership> = memberships::table")
                .count(),
            2,
            "both create_invitation and resend_invitation must re-lock and revalidate the \
             inviter's membership: {invitations}"
        );

        // In create_invitation specifically, the revalidated role (not the
        // stale pre-transaction one) must gate granting Owner.
        let create_org_lock_pos = invitations
            .find("organizations::table\n                .find(org_id)\n                .for_update()")
            .unwrap();
        let create_revalidate_pos = invitations
            .find("let inviter_membership: Option<Membership> = memberships::table")
            .unwrap();
        let create_owner_grant_check_pos = invitations
            .find("if role == Role::Owner && current_caller_role != Role::Owner")
            .unwrap_or_else(|| panic!("missing revalidated owner-grant check: {invitations}"));
        assert!(create_org_lock_pos < create_revalidate_pos);
        assert!(create_revalidate_pos < create_owner_grant_check_pos);
    }

    /// `accept_invitation` must re-lock and revalidate the caller's own
    /// account row (via the same `caller_email_lookup` peek table used for
    /// the earlier email-match check) after acquiring the organization
    /// lock, before inserting a membership — otherwise a concurrent account
    /// deletion could commit in the gap between the standalone pre-
    /// transaction email check and this transaction actually acquiring its
    /// lock, letting this handler create a membership (and a session) for
    /// an account that no longer exists (`Membership.user_id` has no FK to
    /// catch this — Codex review finding).
    #[test]
    fn accept_invitation_relocks_the_caller_account_row() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let account_relock_pos = invitations
            .find("let live_caller_email: Option<String> = caller_users::table")
            .unwrap_or_else(|| panic!("missing account-row relock: {invitations}"));
        let org_lock_pos = invitations
            .find("organizations::table\n                    .find(org_id)\n                    .for_update()")
            .unwrap();
        let insert_pos = invitations
            .find("let membership: Membership = diesel::insert_into(memberships::table)")
            .unwrap();
        assert!(
            account_relock_pos < org_lock_pos,
            "account row must be locked FIRST, before the organization lock — the same order \
             an account-deletion handler must use (see docs/generate-teams.md)"
        );
        assert!(
            org_lock_pos < insert_pos,
            "organization lock must precede the membership insert"
        );
        assert!(
            invitations.contains(
                ".filter(caller_users::id.eq(user_id))\n                    .select(caller_users::email)\n                    .for_update()"
            ),
            "{invitations}"
        );
        // The live email must be re-compared, not just existence checked
        // (Codex review finding: an email-change flow could move the
        // account off the invited address between the standalone
        // pre-transaction check and this lock).
        assert!(
            invitations.contains(
                "live_caller_email.trim().to_lowercase() != invitation_email.trim().to_lowercase()"
            ),
            "{invitations}"
        );
    }

    /// `remove_all_memberships_on_conn` must scan and lock *every*
    /// organization a departing user is a member of at all, not only the
    /// ones they currently own — otherwise a user who is merely an Admin
    /// in an organization solely owned by someone else (also deleting
    /// their account concurrently) would never have that organization
    /// org-locked at all, and the final cleanup could remove a membership
    /// row a concurrent sibling deletion just promoted to Owner, leaving
    /// the organization ownerless despite the successor logic already
    /// having run for it (Codex review finding).
    #[test]
    fn remove_all_memberships_on_conn_scans_every_membership_not_just_owned_ones() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations.contains(
                "let member_tenant_ids: Vec<String> = memberships::table\n        .filter(memberships::user_id.eq(user_id))\n        .select(memberships::tenant_id)\n        .order(memberships::tenant_id.asc())\n        .load(conn)\n        .await?;"
            ),
            "the initial scan must not filter by role — a plain Admin/Member row must be \
             included too: {organizations}"
        );
        // No blanket delete outside the per-organization loop — every
        // removal must happen under that organization's own lock.
        assert!(
            !organizations.contains(
                "diesel::delete(memberships::table.filter(memberships::user_id.eq(user_id)))"
            ),
            "{organizations}"
        );
        assert!(
            organizations.contains(
                "diesel::delete(memberships::table.filter(memberships::id.eq(own_membership.id)))"
            ),
            "{organizations}"
        );
    }

    /// The initial, unlocked scan of which organizations to process must be
    /// ordered deterministically — otherwise two users being deleted
    /// concurrently who share two or more organizations could each lock
    /// them in a different order (whatever order an unordered `SELECT`
    /// happened to return), producing a real Postgres deadlock instead of
    /// clean serialization (Codex review finding).
    #[test]
    fn remove_all_memberships_on_conn_orders_the_scan_deterministically() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let organizations =
            fs::read_to_string(tmp.path().join("src/teams/routes/organizations.rs")).unwrap();
        assert!(
            organizations.contains(".order(memberships::tenant_id.asc())"),
            "{organizations}"
        );
    }

    /// Resending a pending Owner-role invitation must require the caller to
    /// currently be an Owner, the same rule `create_invitation` enforces
    /// for a fresh one — otherwise an Admin could keep an otherwise-expired
    /// Owner grant alive indefinitely by repeatedly resending it, a back
    /// door around "only an owner can invite someone as owner" (Codex
    /// review finding).
    #[test]
    fn resend_invitation_requires_owner_to_resend_an_owner_invitation() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let resend_body = invitations
            .split("pub async fn resend_invitation(")
            .nth(1)
            .unwrap_or_else(|| panic!("missing resend_invitation: {invitations}"));
        assert!(
            resend_body.contains(
                "if current.role == Role::Owner.as_str() && current_caller_role != Role::Owner"
            ),
            "{resend_body}"
        );
    }

    /// `revoke_invitation` must revalidate the caller's own membership
    /// inside a transaction, the same as `create_invitation`/
    /// `resend_invitation`/the member-mutation handlers — a separate,
    /// non-transactional repository update after `require_role` could act
    /// on a caller who was demoted or removed in the interim (Codex review
    /// finding).
    #[test]
    fn revoke_invitation_revalidates_caller_transactionally() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let revoke_body = invitations
            .split("pub async fn revoke_invitation(")
            .nth(1)
            .and_then(|rest| rest.split("pub async fn resend_invitation(").next())
            .unwrap_or_else(|| panic!("missing revoke_invitation: {invitations}"));
        assert!(revoke_body.contains("db.tx(move |conn| {"), "{revoke_body}");
        assert!(
            revoke_body.contains("let caller_membership: Option<Membership> = memberships::table"),
            "{revoke_body}"
        );
        assert!(
            !revoke_body.contains("invitation_repo\n        .update("),
            "revoke must no longer use the non-transactional repository update: {revoke_body}"
        );
    }

    /// `revoke_invitation` must lock and re-check the invitation's own
    /// status before overwriting it — an admin's stale revoke form
    /// submitted after the invitee already accepted (or an accept that
    /// commits while this transaction waits for the lock) must not stomp
    /// the terminal `accepted` row back to `revoked` (Codex review
    /// finding).
    #[test]
    fn revoke_invitation_rejects_a_non_pending_invitation() {
        let tmp = project();
        plan_teams(tmp.path(), "20260101000000", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let invitations =
            fs::read_to_string(tmp.path().join("src/teams/routes/invitations.rs")).unwrap();
        let revoke_body = invitations
            .split("pub async fn revoke_invitation(")
            .nth(1)
            .and_then(|rest| rest.split("pub async fn resend_invitation(").next())
            .unwrap_or_else(|| panic!("missing revoke_invitation: {invitations}"));
        assert!(
            revoke_body.contains(
                "let current: Invitation = invitations::table\n                .filter(invitations::id.eq(invitation_id))\n                .for_update()"
            ),
            "{revoke_body}"
        );
        assert!(
            revoke_body.contains("if current.status != \"pending\""),
            "{revoke_body}"
        );
    }
}
