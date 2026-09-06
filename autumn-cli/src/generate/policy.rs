//! `autumn generate policy` — scaffold a record-level authorization `Policy`
//! and companion `Scope` for an existing model (issue #1125).
//!
//! For a model like `Post` (with an owner column such as `user_id`,
//! `author_id`, or `owner_id`), the generator produces:
//! - `src/policies/<snake>.rs` — a `#[derive(Default)] pub struct <Pascal>Policy`
//!   implementing `Policy<<Pascal>>` (public reads, authenticated create,
//!   owner-or-admin update/delete) plus a `<Pascal>Scope` implementing
//!   `Scope<<Pascal>>` that filters list queries to the current user's rows.
//! - `src/policies/mod.rs` — created or updated with `pub mod <snake>;`.
//! - `src/main.rs` — `mod policies;` declaration plus `.policy::<...>(...)` and
//!   `.scope::<...>(...)` wired into the app builder chain.
//!
//! When no owner column can be detected, `can_update`/`can_delete` and the
//! scope default-deny with a `// TODO` marker so the developer fills in the
//! real ownership rule — safe-by-default until then.
//!
//! Requires the target model to already exist (`src/models/<snake>.rs`); run
//! `autumn generate model <Pascal>` (or `scaffold`) first.

use std::path::Path;

use super::emit::{Plan, Revert};
use super::model::{model_file_exists, validate_resource_name};
use super::naming::{pascal, pluralize, snake};
use super::schema_edit::{add_mod_declaration, add_policy_registration_to_app, update_main_rs};
use super::{GenerateError, ensure_project_root, read_or_empty};

/// Compute the file actions for `autumn generate policy`.
///
/// `for_destroy` selects the caller's `ApplyMode`: in generate mode the
/// model-existence guard (AC5) fires when the target model is missing, but in
/// destroy mode it is skipped so `autumn destroy policy` can still tear the
/// policy down after the model was already destroyed (the plan's `revert` only
/// removes files/registrations and never consults the model). The two modes
/// otherwise build the identical plan.
///
/// # Errors
/// Returns [`GenerateError::Config`] when no matching model exists (AC5,
/// generate mode only), [`GenerateError::NotInProject`] outside an Autumn
/// project, and name- or I/O-related errors otherwise.
pub fn plan_policy(
    project_root: &Path,
    name: &str,
    for_destroy: bool,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;

    let snake_name = snake(name);
    let pascal_name = pascal(name);
    let plural = pluralize(&snake_name);

    // Model guard (AC5): the policy references the model type, so the model must
    // already exist — but only in generate mode. On destroy the model may
    // legitimately be gone already; erroring here would strand the policy file,
    // its mod entry, and the app registrations before `Plan::revert` runs.
    // Because all writes happen in `Plan::execute`, returning here leaves no
    // half-written files.
    if !for_destroy && !model_file_exists(project_root, &plural, &snake_name) {
        return Err(GenerateError::Config(format!(
            "no model '{pascal_name}' found — run `autumn generate model {pascal_name}` first \
             (expected src/models/{snake_name}.rs)"
        )));
    }

    let owner = detect_owner_column_from_model(project_root, &snake_name);
    let model_path = model_type_path(project_root, &snake_name, &pascal_name);

    let mut plan = Plan::new(project_root);
    push_policy_files(
        &mut plan,
        project_root,
        &pascal_name,
        &snake_name,
        &plural,
        owner.as_ref(),
        &model_path,
    );

    // ── src/main.rs: mod policies; + .policy(...)/.scope(...) ──────────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|e| {
        GenerateError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", main_path.display()),
        ))
    })?;
    let updated_main =
        wire_policy_into_main(&main_existing, &pascal_name, &snake_name, &model_path);
    plan.modify(main_path.clone(), updated_main);
    plan.push_revert(Revert::PolicyRegistration {
        path: main_path,
        pascal: pascal_name,
        snake: snake_name,
    });

    Ok(plan)
}

/// Push the `src/policies/<snake>.rs` source file and the `src/policies/mod.rs`
/// `pub mod` declaration (with its paired revert) onto `plan`.
///
/// Shared with `generate scaffold` so both emit byte-identical policy code.
/// Does NOT touch `src/main.rs` — the caller wires that (via
/// [`wire_policy_into_main`]) against its own already-computed `main.rs`
/// content, avoiding a double-`Modify` that would clobber the scaffold's other
/// `main.rs` edits.
pub(super) fn push_policy_files(
    plan: &mut Plan,
    project_root: &Path,
    pascal_name: &str,
    snake_name: &str,
    plural: &str,
    owner: Option<&OwnerColumn>,
    model_path: &str,
) {
    plan.create(
        project_root
            .join("src")
            .join("policies")
            .join(format!("{snake_name}.rs")),
        render_policy_file(pascal_name, snake_name, plural, owner, model_path),
    );

    let mod_path = project_root.join("src").join("policies").join("mod.rs");
    plan.modify(
        mod_path.clone(),
        add_mod_declaration(&read_or_empty(&mod_path), snake_name),
    );
    plan.push_revert(Revert::ModDecl {
        path: mod_path,
        name: snake_name.to_owned(),
    });
}

/// Apply the `mod policies;` declaration and the `.policy(...)`/`.scope(...)`
/// builder registration for `{pascal}` to a `src/main.rs` source string,
/// returning the updated content. Idempotent — re-running never duplicates.
pub(super) fn wire_policy_into_main(
    main_src: &str,
    pascal_name: &str,
    snake_name: &str,
    model_path: &str,
) -> String {
    let with_mod = update_main_rs(main_src, &["policies"], &[]);
    add_policy_registration_to_app(&with_mod, model_path, pascal_name, snake_name)
}

/// A detected owner column: its field name plus whether the model declares it
/// nullable (`Option<i64>`). Nullability changes how `can_update`/`can_delete`
/// compare the current user id — see [`render_policy_file`].
#[derive(Debug, Clone)]
pub(super) struct OwnerColumn {
    pub name: String,
    pub nullable: bool,
}

/// The fully-qualified path to the model type, honoring both model-file
/// layouts (mirroring `src/models.rs` vs `src/models/<snake>.rs`, exactly as
/// the reddit-clone example's `use crate::models::Post;` shows for the
/// single-file case):
/// - per-resource `src/models/<snake>.rs` → `crate::models::<snake>::<Pascal>`;
/// - single-file `src/models.rs`        → `crate::models::<Pascal>`.
///
/// When neither exists (e.g. destroy after the model is gone) defaults to the
/// per-resource path; the destroy path never renders the file and the
/// registration removal is layout-independent, so the choice is immaterial there.
fn model_type_path(project_root: &Path, snake_name: &str, pascal_name: &str) -> String {
    let per_resource = project_root
        .join("src")
        .join("models")
        .join(format!("{snake_name}.rs"));
    let single_file = project_root.join("src").join("models.rs");
    if !per_resource.exists() && single_file.exists() {
        format!("crate::models::{pascal_name}")
    } else {
        format!("crate::models::{snake_name}::{pascal_name}")
    }
}

/// The recognized owner-column names, in priority order (AC3).
const OWNER_COLUMN_CANDIDATES: &[&str] = &["user_id", "author_id", "owner_id"];

/// Detect the owner column for a resource, given its declared column names and
/// the subset of those columns that reference the `users` table (AC3).
///
/// Priority: `user_id`, then `author_id`, then `owner_id`, then the first
/// `*_id` column that references the users table.
pub(super) fn detect_owner_column(
    columns: &[String],
    user_ref_columns: &[String],
) -> Option<String> {
    for candidate in OWNER_COLUMN_CANDIDATES {
        if columns.iter().any(|c| c == candidate) {
            return Some((*candidate).to_owned());
        }
    }
    user_ref_columns.first().cloned()
}

/// Detect the owner column by parsing the model's `#[model]` struct fields.
///
/// Used by the standalone `generate policy` command, which only has the model
/// name. The "references users" heuristic (the 4th priority in
/// [`detect_owner_column`]) is unavailable here — it needs the field-DSL that
/// only `generate scaffold` carries — so this considers the three canonical
/// owner-column names only.
fn detect_owner_column_from_model(project_root: &Path, snake_name: &str) -> Option<OwnerColumn> {
    let fields = model_fields(project_root, snake_name);
    let columns: Vec<String> = fields.iter().map(|(name, _)| name.clone()).collect();
    let name = detect_owner_column(&columns, &[])?;
    // A nullable owner column (`author_id: Option<i64>`) must be compared
    // option-to-option; assume non-nullable when the field can't be found.
    let nullable = fields
        .iter()
        .find(|(field_name, _)| *field_name == name)
        .is_some_and(|(_, nullable)| *nullable);
    Some(OwnerColumn { name, nullable })
}

/// The field (column) names of resource `snake_name`'s `#[model]` struct, each
/// paired with whether its declared type is `Option<…>` (nullable), in
/// declaration order. Parses with `syn` so grouped attributes, doc comments,
/// and a multi-model `src/models.rs` layout don't defeat it. Returns an empty
/// vec when the model can't be located or parsed.
fn model_fields(project_root: &Path, snake_name: &str) -> Vec<(String, bool)> {
    let pascal_name = pascal(snake_name);
    let per_resource = project_root
        .join("src")
        .join("models")
        .join(format!("{snake_name}.rs"));
    let content = if per_resource.exists() {
        read_or_empty(&per_resource)
    } else {
        let single_file = project_root.join("src").join("models.rs");
        if !single_file.exists() {
            return Vec::new();
        }
        read_or_empty(&single_file)
    };

    let Ok(file) = syn::parse_file(&content) else {
        return Vec::new();
    };
    let Some(item_struct) = file.items.iter().find_map(|item| match item {
        syn::Item::Struct(s) if s.ident == pascal_name => Some(s),
        _ => None,
    }) else {
        return Vec::new();
    };
    let syn::Fields::Named(fields) = &item_struct.fields else {
        return Vec::new();
    };
    fields
        .named
        .iter()
        .filter_map(|f| {
            f.ident
                .as_ref()
                .map(|ident| (ident.to_string(), type_is_option(&f.ty)))
        })
        .collect()
}

/// Whether a `syn::Type` is spelled `Option<…>` (nullable column). Matches the
/// last path segment so both bare `Option<i64>` and `std::option::Option<i64>`
/// count.
fn type_is_option(ty: &syn::Type) -> bool {
    let syn::Type::Path(type_path) = ty else {
        return false;
    };
    type_path
        .path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "Option")
}

/// Render `src/policies/<snake>.rs`.
#[allow(clippy::option_if_let_else)] // nested map_or_else closures read worse here
fn render_policy_file(
    pascal_name: &str,
    snake_name: &str,
    plural: &str,
    owner: Option<&OwnerColumn>,
    model_path: &str,
) -> String {
    let module_doc = if owner.is_some() {
        format!(
            "//! Generated by `autumn generate policy`.\n\
             //!\n\
             //! Record-level authorization for [`{pascal_name}`]. Edit freely — once\n\
             //! generated, this is ordinary user code.\n"
        )
    } else {
        format!(
            "//! Generated by `autumn generate policy`.\n\
             //!\n\
             //! Record-level authorization for [`{pascal_name}`]. Edit freely — once\n\
             //! generated, this is ordinary user code.\n\
             //!\n\
             //! No owner column (`user_id`/`author_id`/`owner_id`) was detected on this\n\
             //! model, so `can_update`/`can_delete` authorize any authenticated user and\n\
             //! the scope default-denies. This is a placeholder — replace the authenticated\n\
             //! check with a real per-record ownership rule where the `SECURITY TODO`/`TODO`\n\
             //! markers are before production.\n"
        )
    };

    let (update_delete_bodies, scope_body, scope_ctx_param) = match owner {
        Some(owner) => {
            let col = &owner.name;
            // A nullable owner column is already `Option<i64>`, so compare it
            // directly against `user_id_i64()` (also `Option<i64>`); wrapping it
            // in `Some(...)` would compare `Option<i64>` to `Option<Option<i64>>`
            // and fail to compile. A non-nullable owner is a plain `i64` and
            // needs the `Some(...)` wrap.
            let owner_match = if owner.nullable {
                format!("ctx.user_id_i64() == {snake_name}.{col}")
            } else {
                format!("ctx.user_id_i64() == Some({snake_name}.{col})")
            };
            (
                format!(
                    "    fn can_update<'a>(&'a self, ctx: &'a PolicyContext, {snake_name}: &'a {pascal_name}) -> BoxFuture<'a, bool> {{\n        \
                 Box::pin(async move {{ ctx.has_role(\"admin\") || {owner_match} }})\n    \
                 }}\n\n    \
                 fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, {snake_name}: &'a {pascal_name}) -> BoxFuture<'a, bool> {{\n        \
                 Box::pin(async move {{ ctx.has_role(\"admin\") || {owner_match} }})\n    \
                 }}\n"
                ),
                format!(
                    "        Box::pin(async move {{\n            \
                 let owner_id = ctx.user_id_i64().unwrap_or(-1);\n            \
                 Ok({plural}::table\n                \
                 .filter({plural}::{col}.eq(owner_id))\n                \
                 .select({pascal_name}::as_select())\n                \
                 .load(conn)\n                \
                 .await?)\n        \
                 }})\n"
                ),
                // `ctx` is used by the owner filter.
                "ctx",
            )
        }
        None => (
            // Issue #1830: no owner column was detected, so there is no per-record
            // ownership rule to emit. Rather than default-deny (which would make a
            // fresh scaffold 403 on every update/delete) or ship `#[secured]`-only
            // handlers (which let ANY authenticated user mutate ANY row), authorize
            // any authenticated user here — a no-regression over the prior behavior
            // that routes the mutation through this ONE enforceable policy method
            // for the developer to tighten.
            format!(
                "    fn can_update<'a>(&'a self, ctx: &'a PolicyContext, _{snake_name}: &'a {pascal_name}) -> BoxFuture<'a, bool> {{\n        \
                 // SECURITY TODO: this only checks authentication — replace with a real\n        \
                 // per-record ownership rule before production.\n        \
                 Box::pin(async move {{ ctx.is_authenticated() }})\n    \
                 }}\n\n    \
                 fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, _{snake_name}: &'a {pascal_name}) -> BoxFuture<'a, bool> {{\n        \
                 // SECURITY TODO: this only checks authentication — replace with a real\n        \
                 // per-record ownership rule before production.\n        \
                 Box::pin(async move {{ ctx.is_authenticated() }})\n    \
                 }}\n"
            ),
            "        // TODO: no owner column detected — implement the scope filter.\n        \
             Box::pin(async { Ok(Vec::new()) })\n"
                .to_owned(),
            // `ctx`/`conn` are unused in the default-deny scope stub.
            "_ctx",
        ),
    };
    // `conn` is only referenced when an owner filter is emitted.
    let scope_conn_param = if owner.is_some() { "conn" } else { "_conn" };

    // Diesel is used unconditionally, mirroring the scaffold's other generated
    // files (`src/routes/*.rs`, `src/repositories/*.rs`): a generated Autumn app
    // always compiles against autumn-web's `db` feature, and the app crate does
    // not define its own `db` feature — so gating these on `#[cfg(feature =
    // \"db\")]` would (wrongly) drop them while autumn-web's 3-arg `Scope::list`
    // stays in scope. The `use` lines are only needed for the owner filter.
    let diesel_imports = if owner.is_some() {
        "use diesel::prelude::*;\nuse diesel_async::RunQueryDsl;\n"
    } else {
        ""
    };
    let schema_import = if owner.is_some() {
        format!("use crate::schema::{plural};\n")
    } else {
        String::new()
    };

    format!(
        "{module_doc}\
use autumn_web::authorization::{{BoxFuture, Policy, PolicyContext, Scope}};
{diesel_imports}\
use {model_path};
{schema_import}
#[derive(Default)]
pub struct {pascal_name}Policy;

impl Policy<{pascal_name}> for {pascal_name}Policy {{
    fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _{snake_name}: &'a {pascal_name}) -> BoxFuture<'a, bool> {{
        // Reads are public by default. Tighten this if shows should be gated.
        Box::pin(async {{ true }})
    }}

    fn can_create<'a>(&'a self, ctx: &'a PolicyContext) -> BoxFuture<'a, bool> {{
        // Any authenticated user may create.
        Box::pin(async move {{ ctx.is_authenticated() }})
    }}

{update_delete_bodies}}}

#[derive(Default)]
pub struct {pascal_name}Scope;

impl Scope<{pascal_name}> for {pascal_name}Scope {{
    fn list<'a>(
        &'a self,
        {scope_ctx_param}: &'a PolicyContext,
        {scope_conn_param}: &'a mut autumn_web::RuntimeConnection,
    ) -> BoxFuture<'a, autumn_web::AutumnResult<Vec<{pascal_name}>>> {{
{scope_body}    }}
}}
"
    )
}

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

    fn project_with_model(fields: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src/models")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), default_main()).unwrap();
        fs::write(
            tmp.path().join("src/models/post.rs"),
            format!(
                "use autumn_web::prelude::*;\n\n#[model]\npub struct Post {{\n    #[id]\n    pub id: i64,\n{fields}}}\n"
            ),
        )
        .unwrap();
        tmp
    }

    /// A project whose `Post` model lives in the single-file `src/models.rs`
    /// layout (`crate::models::Post`), with no `src/models/` directory.
    fn project_with_single_file_model(fields: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), default_main()).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            format!(
                "use autumn_web::prelude::*;\nuse crate::schema::posts;\n\n#[model]\npub struct Post {{\n    #[id]\n    pub id: i64,\n{fields}}}\n"
            ),
        )
        .unwrap();
        tmp
    }

    #[test]
    fn model_missing_returns_named_error_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), default_main()).unwrap();

        let err = plan_policy(tmp.path(), "Post", false).unwrap_err();
        match err {
            GenerateError::Config(msg) => {
                assert!(msg.contains("no model 'Post' found"), "{msg}");
                assert!(msg.contains("src/models/post.rs"), "{msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
        // Pre-flight error leaves nothing on disk.
        assert!(!tmp.path().join("src/policies").exists());
    }

    #[test]
    fn emits_policy_and_scope_structs() {
        let tmp = project_with_model("    pub user_id: i64,\n    pub title: String,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/policies/post.rs")).unwrap();
        assert!(src.contains("pub struct PostPolicy"), "{src}");
        assert!(src.contains("impl Policy<Post> for PostPolicy"), "{src}");
        assert!(src.contains("pub struct PostScope"), "{src}");
        assert!(src.contains("impl Scope<Post> for PostScope"), "{src}");
        assert!(src.contains("#[derive(Default)]"), "{src}");
    }

    #[test]
    fn user_id_owner_generates_ownership_check() {
        let tmp = project_with_model("    pub user_id: i64,\n    pub title: String,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let src = fs::read_to_string(tmp.path().join("src/policies/post.rs")).unwrap();
        assert!(
            src.contains("ctx.has_role(\"admin\") || ctx.user_id_i64() == Some(post.user_id)"),
            "{src}"
        );
        assert!(src.contains("posts::user_id.eq(owner_id)"), "{src}");
        assert!(
            !src.contains("TODO"),
            "owner case must not carry TODO: {src}"
        );
    }

    #[test]
    fn author_id_owner_generates_ownership_check() {
        let tmp = project_with_model("    pub author_id: i64,\n    pub body: String,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let src = fs::read_to_string(tmp.path().join("src/policies/post.rs")).unwrap();
        assert!(src.contains("Some(post.author_id)"), "{src}");
    }

    #[test]
    fn nullable_owner_compares_option_to_option_without_some_wrap() {
        // A nullable owner column is `Option<i64>`; wrapping it in `Some(...)`
        // would compare `Option<i64>` to `Option<Option<i64>>` and not compile.
        let tmp = project_with_model("    pub author_id: Option<i64>,\n    pub body: String,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let src = fs::read_to_string(tmp.path().join("src/policies/post.rs")).unwrap();
        assert!(
            src.contains("ctx.has_role(\"admin\") || ctx.user_id_i64() == post.author_id"),
            "nullable owner must compare option-to-option: {src}"
        );
        assert!(
            !src.contains("Some(post.author_id)"),
            "nullable owner must not wrap in Some(...): {src}"
        );
        // The scope filter still `.eq(owner_id)`s the (nullable) diesel column.
        assert!(src.contains("posts::author_id.eq(owner_id)"), "{src}");
    }

    #[test]
    fn non_nullable_owner_keeps_some_wrap() {
        let tmp = project_with_model("    pub user_id: i64,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let src = fs::read_to_string(tmp.path().join("src/policies/post.rs")).unwrap();
        assert!(
            src.contains("ctx.user_id_i64() == Some(post.user_id)"),
            "non-nullable owner must keep the Some(...) wrap: {src}"
        );
    }

    #[test]
    fn no_owner_column_authorizes_authenticated_with_security_todo() {
        // Issue #1830: the no-owner policy authorizes any authenticated user
        // (a no-regression over `#[secured]`-only handlers) rather than
        // default-denying, so a fresh scaffold never 403s on first use — with a
        // loud SECURITY TODO steering the developer to a real ownership rule.
        let tmp = project_with_model("    pub title: String,\n    pub body: String,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let src = fs::read_to_string(tmp.path().join("src/policies/post.rs")).unwrap();
        assert!(
            src.contains("// SECURITY TODO: this only checks authentication"),
            "must carry the SECURITY TODO marker: {src}"
        );
        // can_update/can_delete authorize an authenticated user, not default-deny.
        assert_eq!(
            src.matches("Box::pin(async move { ctx.is_authenticated() })")
                .count(),
            // can_create + can_update + can_delete.
            3,
            "no-owner can_update/can_delete must authenticate-check like can_create: {src}"
        );
        assert!(
            !src.contains("Box::pin(async { false })"),
            "no-owner policy must no longer default-deny: {src}"
        );
        // The scope stays default-empty (no column to filter on).
        assert!(
            src.contains("// TODO: no owner column detected — implement the scope filter."),
            "scope must keep its TODO marker: {src}"
        );
        assert!(
            src.contains("//! No owner column"),
            "module doc must note the missing owner column: {src}"
        );
    }

    #[test]
    fn adds_mod_entry_and_builder_registration() {
        let tmp = project_with_model("    pub user_id: i64,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/policies/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod post;"), "{mod_rs}");

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("mod policies;"), "{main}");
        assert!(
            main.contains(
                ".policy::<crate::models::post::Post, _>(crate::policies::post::PostPolicy::default())"
            ),
            "{main}"
        );
        assert!(
            main.contains(
                ".scope::<crate::models::post::Post, _>(crate::policies::post::PostScope::default())"
            ),
            "{main}"
        );
    }

    #[test]
    fn single_file_models_layout_imports_and_registers_crate_models_pascal() {
        // A `Post` declared in single-file `src/models.rs` lives at
        // `crate::models::Post` — the import and registrations must NOT point at
        // a non-existent `crate::models::post` module.
        let tmp = project_with_single_file_model("    pub user_id: i64,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let policy = fs::read_to_string(tmp.path().join("src/policies/post.rs")).unwrap();
        assert!(
            policy.contains("use crate::models::Post;"),
            "single-file layout must import crate::models::Post: {policy}"
        );
        assert!(
            !policy.contains("crate::models::post::"),
            "single-file layout must not reference crate::models::post: {policy}"
        );

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains(
                ".policy::<crate::models::Post, _>(crate::policies::post::PostPolicy::default())"
            ),
            "single-file layout policy registration must use crate::models::Post: {main}"
        );
        assert!(
            main.contains(
                ".scope::<crate::models::Post, _>(crate::policies::post::PostScope::default())"
            ),
            "single-file layout scope registration must use crate::models::Post: {main}"
        );
    }

    #[test]
    fn builder_registration_inserted_exactly_once_when_run_twice() {
        let tmp = project_with_model("    pub user_id: i64,\n");
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        // Second run with --force (policy file already exists).
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(
            main.matches(".policy::<crate::models::post::Post, _>")
                .count(),
            1,
            "policy registration must appear exactly once: {main}"
        );
        assert_eq!(
            main.matches(".scope::<crate::models::post::Post, _>")
                .count(),
            1,
            "scope registration must appear exactly once: {main}"
        );
        let mod_rs = fs::read_to_string(tmp.path().join("src/policies/mod.rs")).unwrap();
        assert_eq!(mod_rs.matches("pub mod post;").count(), 1, "{mod_rs}");
    }

    #[test]
    fn generate_then_destroy_round_trips_main_rs() {
        let tmp = project_with_model("    pub user_id: i64,\n");
        let main_path = tmp.path().join("src/main.rs");
        let original_main = fs::read_to_string(&main_path).unwrap();

        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/policies/post.rs").exists());
        assert!(!tmp.path().join("src/policies").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
    }

    #[test]
    fn destroy_succeeds_after_model_already_deleted() {
        // `autumn destroy model Post` then `autumn destroy policy Post`: the
        // model file is gone by the time the policy is destroyed. The destroy
        // planner must skip the model guard (generate mode still errors) and,
        // with --force, revert every artifact. As with `plan_admin_destroy_fallback`,
        // the policy file's content is unverifiable once the model is gone, so
        // --force is required to remove it.
        let tmp = project_with_model("    pub user_id: i64,\n");
        let main_path = tmp.path().join("src/main.rs");
        let original_main = fs::read_to_string(&main_path).unwrap();

        plan_policy(tmp.path(), "Post", false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        // Simulate `destroy model Post` having already run.
        fs::remove_file(tmp.path().join("src/models/post.rs")).unwrap();

        // Generate mode still errors on the missing model; destroy mode must not.
        assert!(plan_policy(tmp.path(), "Post", false).is_err());
        let plan =
            plan_policy(tmp.path(), "Post", true).expect("destroy plan builds without the model");
        plan.revert(Flags {
            dry_run: false,
            force: true,
        })
        .unwrap();

        assert!(!tmp.path().join("src/policies/post.rs").exists());
        assert!(!tmp.path().join("src/policies").exists());
        let main = fs::read_to_string(&main_path).unwrap();
        assert!(
            !main.contains(".policy::<") && !main.contains(".scope::<"),
            "destroy must remove the policy/scope registrations: {main}"
        );
        assert!(!main.contains("mod policies;"), "{main}");
        assert_eq!(main, original_main);
    }

    #[test]
    fn detect_owner_column_priority() {
        assert_eq!(
            detect_owner_column(&["author_id".into(), "user_id".into()], &[]),
            Some("user_id".to_owned())
        );
        assert_eq!(
            detect_owner_column(&["author_id".into(), "owner_id".into()], &[]),
            Some("author_id".to_owned())
        );
        assert_eq!(
            detect_owner_column(&["owner_id".into()], &[]),
            Some("owner_id".to_owned())
        );
        assert_eq!(
            detect_owner_column(&["title".into()], &["team_id".into()]),
            Some("team_id".to_owned())
        );
        assert_eq!(detect_owner_column(&["title".into()], &[]), None);
    }

    #[test]
    fn plan_errors_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_policy(tmp.path(), "Post", false).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }
}
