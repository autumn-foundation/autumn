//! `autumn generate scaffold` — full CRUD scaffold.
//!
//! Builds on top of [`model::plan_model`](super::model::plan_model) and adds:
//!
//! - A `#[repository(Model, api = "/api/<plural>")]` block for JSON reads/writes.
//! - HTML route handlers for `index`, `show`, `new_form`, `create`, `edit_form`,
//!   and `update`, returning Maud `Markup`.
//! - A `tests/<snake>.rs` smoke test that asserts the index route returns 200.
//! - Updates to `src/main.rs` registering all new routes in `routes![ … ]`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use super::dsl::{Field, FieldKind, IdType, parse_fields};
use super::emit::{Action, Plan};
use super::model::{
    ModelOptions, augment_fields_for_soft_delete, field_by_name, parse_model_metadata,
    plan_cargo_deps, plan_model_with_options,
};
use super::naming::{humanize_label, pascal, pluralize, snake};
use super::schema_edit::{
    add_mod_declaration, create_table_sql_with_metadata_and_id, ensure_autumn_web_feature,
    ensure_dev_dependency_test_support, ensure_dev_dependency_tokio_test_features, update_main_rs,
};
use super::{Flags, GenerateError, ensure_project_root, read_or_empty, timestamp_now};

/// Extra dependencies the *scaffold* generator's output requires on top of
/// [`super::model::MODEL_DEPS`] — `maud` for HTML rendering and URL-encoded
/// form helpers for blank nullable-field normalization.
const SCAFFOLD_EXTRA_DEPS: &[(&str, &str)] = &[
    ("maud", "{ version = \"0.27\", features = [\"axum\"] }"),
    ("serde_urlencoded", "\"0.7\""),
    ("url", "\"2\""),
];

/// Optional metadata applied by `autumn generate scaffold`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScaffoldOptions {
    /// Model-level field metadata.
    pub model: ModelOptions,
    /// Repository derived-query specs in `method:field` form.
    pub queries: Vec<String>,
    /// Scaffold a JSON-only API resource.
    pub api: bool,
    /// Emit `broadcasts = true` on the repository, a `LiveFragment` impl,
    /// an SSE events route, and an SSE-wired list container in the index view.
    pub live: bool,
    /// Emit per-field inline validation endpoints and `hx-post` attributes on
    /// form inputs (requires `--live`).
    pub live_validation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuerySpec {
    method: String,
    field_name: String,
    rust_type: String,
}

/// Compute the file actions for `autumn generate scaffold`.
///
/// # Errors
/// Surfaces any planning error from the underlying [`plan_model`] call as
/// well as project-layout problems (missing `src/main.rs`).
#[cfg(test)]
pub fn plan_scaffold(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
) -> Result<Plan, GenerateError> {
    plan_scaffold_with_options(
        project_root,
        name,
        field_tokens,
        timestamp,
        &ScaffoldOptions::default(),
    )
}

/// Compute the file actions for `autumn generate scaffold`, using optional
/// metadata flags.
///
/// # Errors
/// Surfaces any planning error from the underlying model generation as well
/// as project-layout, repository query, and metadata problems.
#[allow(clippy::too_many_lines)]
pub fn plan_scaffold_with_options(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
    options: &ScaffoldOptions,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    // Gate: UUID primary keys are not yet supported for scaffolds. Every scaffold
    // emits a `#[autumn_web::repository]`, whose macro-generated REST API is
    // currently hard-coded to `i64` primary keys (`Path<i64>`, `find_by_id`,
    // cursor pagination), so a UUID-keyed scaffold would not compile. The model
    // generator (`generate model --id uuid`) has no such limitation.
    if options.model.id_type == IdType::Uuid {
        return Err(GenerateError::Config(
            "UUID primary keys are not yet supported for `generate scaffold`: the \
             generated `#[repository]` REST API is currently limited to i64 primary \
             keys. Use `generate model --id uuid` for the model and migration, or \
             omit `--id` to use the default BIGSERIAL key."
                .to_owned(),
        ));
    }
    let fields = parse_fields(field_tokens)?;
    // Resolve shard key before planning the model (propagates to model render).
    let resolved_shard_key = resolve_shard_key(&fields, &options.model)?;
    let model_options_with_key = ModelOptions {
        shard_key: resolved_shard_key,
        ..options.model.clone()
    };
    let options_with_key = ScaffoldOptions {
        model: model_options_with_key,
        queries: options.queries.clone(),
        api: options.api,
        live: options.live,
        live_validation: options.live_validation,
    };
    let mut plan = plan_model_with_options(
        project_root,
        name,
        field_tokens,
        timestamp,
        &options_with_key.model,
    )?;
    let metadata = parse_model_metadata(&fields, &options_with_key.model)?;
    let queries = parse_query_specs(&fields, &options_with_key.queries)?;
    let form_fields = fields
        .iter()
        .filter(|field| !metadata.defaults().contains_key(&field.name))
        .cloned()
        .collect::<Vec<_>>();
    let pascal_name = pascal(name);
    let snake_name = snake(name);
    let plural = pluralize(&snake_name);

    // Repository file under `src/repositories/<snake>.rs`
    let repos_dir = project_root.join("src").join("repositories");
    plan.create(
        repos_dir.join(format!("{snake_name}.rs")),
        render_repository_file(
            &pascal_name,
            &snake_name,
            &queries,
            options_with_key.model.soft_delete,
            options_with_key.api,
            options_with_key.model.sharded,
            options_with_key.live,
        ),
    );
    let repo_mod_path = repos_dir.join("mod.rs");
    plan.modify(
        repo_mod_path.clone(),
        add_mod_declaration(&read_or_empty(&repo_mod_path), &snake_name),
    );

    // Route file under `src/routes/<plural>.rs`
    if !options_with_key.api {
        let routes_dir = project_root.join("src").join("routes");
        plan.create(
            routes_dir.join(format!("{plural}.rs")),
            render_routes_file(
                &pascal_name,
                &snake_name,
                &plural,
                &form_fields,
                &fields,
                options_with_key.model.sharded,
                options_with_key.model.soft_delete,
                options_with_key.model.id_type,
                options_with_key.live,
                options_with_key.live_validation,
                metadata.validations(),
            ),
        );
        let route_mod_path = routes_dir.join("mod.rs");
        plan.modify(
            route_mod_path.clone(),
            add_mod_declaration(&read_or_empty(&route_mod_path), &plural),
        );
    }

    // Smoke test under `tests/<snake>.rs`. Uses the same soft-delete-augmented
    // field list as the real migration (see `augment_fields_for_soft_delete`)
    // so the smoke test's throwaway table matches the real schema exactly.
    let smoke_test_fields =
        augment_fields_for_soft_delete(&fields, options_with_key.model.soft_delete)?;
    plan.create(
        project_root.join("tests").join(format!("{snake_name}.rs")),
        render_smoke_test(
            &pascal_name,
            &plural,
            options_with_key.api,
            &smoke_test_fields,
            options_with_key.model.id_type,
            metadata.indexes(),
            metadata.defaults(),
        ),
    );

    // `src/main.rs` updates: declare modules + register all new routes.
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|_| {
        GenerateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("missing {}", main_path.display()),
        ))
    })?;
    let validated_field_names: Vec<String> = if options_with_key.live_validation {
        metadata.validations().keys().cloned().collect()
    } else {
        Vec::new()
    };
    let route_entries = main_route_entries(
        &plural,
        &snake_name,
        options_with_key.api,
        options_with_key.live,
        &validated_field_names,
    );
    let mut mods = vec!["models", "schema", "repositories"];
    if !options_with_key.api {
        mods.push("routes");
    }
    let updated = update_main_rs(&main_existing, &mods, &route_entries);
    plan.modify(main_path, updated);

    // The Maud `html!` macro pulls in a direct `maud` dep on top of the
    // model's deps. Both modify actions target Cargo.toml, so we combine
    // them into a single deduplicated call — otherwise the second write
    // would clobber the first (each rendering is computed at plan time
    // against the on-disk Cargo.toml).
    plan.actions.retain(|a| !a.path().ends_with("Cargo.toml"));
    let mut combined: Vec<(&str, &str)> = super::model::MODEL_DEPS
        .iter()
        .copied()
        .chain(SCAFFOLD_EXTRA_DEPS.iter().copied())
        .collect();
    if metadata.has_validator_rules() {
        combined.push((
            "validator",
            "{ version = \"0.20\", features = [\"derive\"] }",
        ));
    }
    plan_cargo_deps(&mut plan, project_root, &combined);

    // --live requires `ws` (sse::stream), `maud` (LiveFragment/Markup), and `htmx`.
    // --live-validation alone also emits Markup-returning validate handlers and
    // references HTMX_JS_PATH, so it requires `htmx` + `maud` even without `ws`.
    if options_with_key.live || options_with_key.live_validation {
        let cargo_path = project_root.join("Cargo.toml");
        let base = plan
            .actions
            .iter()
            .rev()
            .find_map(|a| match a {
                Action::Modify { path, contents } if path == &cargo_path => Some(contents.clone()),
                _ => None,
            })
            .unwrap_or_else(|| read_or_empty(&cargo_path));
        let mut updated = base.clone();
        let feats: &[&str] = if options.live {
            &["htmx", "maud", "ws"]
        } else {
            &["htmx", "maud"]
        };
        for feat in feats {
            updated = ensure_autumn_web_feature(&updated, feat);
        }
        if updated != base {
            plan.actions.retain(|a| a.path() != cargo_path);
            plan.modify(cargo_path, updated);
        }
    }

    // The generated smoke test uses `autumn_web::test::TestDb` (a real,
    // throwaway Postgres testcontainer) to back its in-process `TestApp`
    // request. `TestDb` is compiled only when the `test-support` feature is
    // enabled, and that feature must stay out of `[dependencies]` so release
    // builds don't pull in `testcontainers` — so it goes in
    // `[dev-dependencies]` instead.
    {
        let cargo_path = project_root.join("Cargo.toml");
        let base = plan
            .actions
            .iter()
            .rev()
            .find_map(|a| match a {
                Action::Modify { path, contents } if path == &cargo_path => Some(contents.clone()),
                _ => None,
            })
            .unwrap_or_else(|| read_or_empty(&cargo_path));
        let updated = ensure_dev_dependency_test_support(&base, env!("CARGO_PKG_VERSION"));
        if updated != base {
            plan.actions.retain(|a| a.path() != cargo_path);
            plan.modify(cargo_path, updated);
        }
    }

    // The generated smoke test also uses `#[tokio::test]`, which needs the
    // `rt` and `macros` tokio features to compile. Every `autumn new`
    // project already has these (see `templates/Cargo.toml.tmpl`), but a
    // hand-rolled or edited-down Cargo.toml might not -- and `cargo test
    // --tests` still compiles `#[ignore]`d tests, so a missing dev-dependency
    // here would leave the project unable to compile its test targets.
    {
        let cargo_path = project_root.join("Cargo.toml");
        let base = plan
            .actions
            .iter()
            .rev()
            .find_map(|a| match a {
                Action::Modify { path, contents } if path == &cargo_path => Some(contents.clone()),
                _ => None,
            })
            .unwrap_or_else(|| read_or_empty(&cargo_path));
        let updated = ensure_dev_dependency_tokio_test_features(&base);
        if updated != base {
            plan.actions.retain(|a| a.path() != cargo_path);
            plan.modify(cargo_path, updated);
        }
    }

    Ok(plan)
}

/// CLI entry point.
pub fn run(name: &str, field_tokens: &[String], flags: Flags, options: &ScaffoldOptions) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: cannot determine current directory: {e}");
            std::process::exit(1);
        }
    };
    let timestamp = timestamp_now();
    let plan = plan_scaffold_with_options(&cwd, name, field_tokens, &timestamp, options);
    match plan.and_then(|p| p.execute(flags)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_query_specs(
    fields: &[Field],
    queries: &[String],
) -> Result<Vec<QuerySpec>, GenerateError> {
    let mut parsed = Vec::with_capacity(queries.len());
    for query in queries {
        let (method, field_name) =
            query
                .split_once(':')
                .ok_or_else(|| GenerateError::InvalidField {
                    token: query.clone(),
                    reason: "expected `method:field`, for example `find_by_tag:tag`".into(),
                })?;
        let method = method.trim();
        let field_name = field_name.trim();
        if !method.starts_with("find_by_") || !is_valid_fn_name(method) {
            return Err(GenerateError::InvalidField {
                token: query.clone(),
                reason: "query method must be a valid `find_by_<field>` function name".into(),
            });
        }
        let method_field = method
            .strip_prefix("find_by_")
            .expect("prefix checked above");
        let field =
            field_by_name(fields, field_name).ok_or_else(|| GenerateError::InvalidField {
                token: query.clone(),
                reason: format!("unknown field '{field_name}'"),
            })?;
        if method_field != field_name {
            return Err(GenerateError::InvalidField {
                token: query.clone(),
                reason: format!(
                    "query method suffix '{method_field}' must match field '{field_name}'"
                ),
            });
        }
        if field.is_enum() {
            // The repository file's `use crate::models::<snake>::{...}` import
            // only brings in the model + New/Update companions — not a
            // per-field generated enum type — so a derived-query parameter of
            // that type wouldn't resolve. Reject rather than emit code that
            // fails to compile.
            return Err(GenerateError::InvalidField {
                token: query.clone(),
                reason: format!("`--query` on enum field '{field_name}' is not yet supported"),
            });
        }
        if parsed.iter().any(|spec: &QuerySpec| spec.method == method) {
            return Err(GenerateError::InvalidField {
                token: query.clone(),
                reason: format!("duplicate query method '{method}'"),
            });
        }
        parsed.push(QuerySpec {
            method: method.to_owned(),
            field_name: field_name.to_owned(),
            rust_type: field.rust_type(),
        });
    }
    Ok(parsed)
}

fn is_valid_fn_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first == '_')
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// Resolve the sharding key field from options and field list.
///
/// Returns `None` when sharding is not enabled. When sharding is enabled,
/// returns the explicitly requested key (validated against model fields and
/// `id`), or falls back to `tenant_id` if present, then `id`.
fn resolve_shard_key(
    fields: &[Field],
    options: &ModelOptions,
) -> Result<Option<String>, GenerateError> {
    if !options.sharded {
        return Ok(None);
    }
    if let Some(ref key) = options.shard_key {
        let valid = key == "id" || field_by_name(fields, key).is_some();
        if !valid {
            return Err(GenerateError::InvalidField {
                token: key.clone(),
                reason: format!(
                    "shard_key field `{key}` does not exist on this model; \
                     pass an existing field name or `id`"
                ),
            });
        }
        return Ok(Some(key.clone()));
    }
    if field_by_name(fields, "tenant_id").is_some() {
        return Ok(Some("tenant_id".to_owned()));
    }
    Ok(Some("id".to_owned()))
}

/// Render a plain `#[repository(Model)]` trait for `autumn db pull --with-repository`.
///
/// No derived queries, soft-delete, or sharding — introspection cannot recover
/// those from the database. The introspected `table` name is passed through
/// explicitly — both as the schema import and as `table = "..."` in the macro —
/// because the repository macro otherwise infers the table from the model name
/// (`Status` -> `statuss`), which is wrong for irregular plurals.
pub(super) fn render_repository_for_pull(
    pascal_name: &str,
    snake_name: &str,
    table: &str,
) -> String {
    format!(
        "//! Generated by `autumn db pull`.\n\
         //!\n\
         //! `#[repository]` auto-generates CRUD methods and JSON REST handlers.\n\
         //! Mount mutating API handlers only after adding a repository policy.\n\
         \n\
         use crate::models::{snake_name}::{{{pascal_name}, New{pascal_name}, Update{pascal_name}}};\n\
         use crate::schema::{table};\n\
         \n\
         #[autumn_web::repository({pascal_name}, table = \"{table}\", api = \"/api/{table}\")]\n\
         pub trait {pascal_name}Repository {{\n\
         }}\n"
    )
}

#[allow(clippy::fn_params_excessive_bools)]
fn render_repository_file(
    pascal_name: &str,
    snake_name: &str,
    queries: &[QuerySpec],
    soft_delete: bool,
    api: bool,
    sharded: bool,
    live: bool,
) -> String {
    let plural = pluralize(snake_name);
    let query_body = render_repository_queries(pascal_name, queries);
    let soft_delete_attr = if soft_delete { ", soft_delete" } else { "" };
    let broadcasts_attr = if live { ", broadcasts = true" } else { "" };
    let sharded_note = if sharded {
        format!(
            "//!\n\
             //! This is a shard-aware repository. Handlers construct it via\n\
             //! `Pg{pascal_name}Repository::from_shard(&db)` where `db` is a `ShardedDb` extractor;\n\
             //! the extractor routes the request to the correct shard automatically.\n"
        )
    } else {
        String::new()
    };
    let api_sharded_note = if sharded && api {
        "//!\n\
         //! Note: auto-generated REST handlers (mounted via `api = ...`) route through\n\
         //! the control pool, not individual shards. Shard-aware REST is planned for a\n\
         //! future release. Use the HTML handlers or build custom shard-aware endpoints\n\
         //! with `ShardedDb` in the meantime.\n"
    } else {
        ""
    };
    let doc_comment = if api {
        format!(
            "//! Generated by `autumn generate scaffold --api`.\n\
             //!\n\
             //! `#[repository]` auto-generates CRUD methods and JSON REST handlers.\n\
             //! When using `--api`, all 5 JSON CRUD endpoints are mounted in `src/main.rs`.\n\
             //! Note: To start the application in a production profile, you must either\n\
             //! add a policy (e.g. `policy = SomePolicy`) to this repository or explicitly\n\
             //! allow unguarded writes by setting `allow_unauthorized_repository_api = true`\n\
             //! under `[security]` in `autumn.toml`.\n\
             {api_sharded_note}\
             {sharded_note}"
        )
    } else {
        format!(
            "//! Generated by `autumn generate scaffold`.\n\
             //!\n\
             //! `#[repository]` auto-generates CRUD methods and JSON REST handlers.\n\
             //! The scaffold registers only read handlers in `src/main.rs` by\n\
             //! default. Mount mutating API handlers only after adding a policy.\n\
             {sharded_note}"
        )
    };
    // For API scaffolds with --live, emit the stream route directly in the
    // repository file since there is no separate routes file.
    let api_stream_handler = if api && live {
        format!(
            "\n/// `GET /{plural}/stream` — SSE stream for live OOB fragments.\n\
             ///\n\
             /// Clients subscribe here to receive `hx-swap-oob` fragments whenever a\n\
             /// `{snake_name}` is saved, updated, or deleted via the API.\n\
             #[autumn_web::get(\"/{plural}/stream\")]\n\
             pub async fn stream(\n\
             \x20\x20\x20\x20state: autumn_web::extract::State<autumn_web::AppState>,\n\
             ) -> impl autumn_web::reexports::axum::response::IntoResponse {{\n\
             \x20\x20\x20\x20autumn_web::sse::stream(&state, \"{plural}\")\n\
             }}\n"
        )
    } else {
        String::new()
    };
    let list_id = format!("{plural}-list");
    // API scaffolds have no HTML show route — emit plain text; HTML scaffolds link to show page.
    let fragment_item_content = if api {
        "(self.id)".to_string()
    } else {
        format!("a href=(format!(\"/{plural}/{{}}\", self.id)) {{ (self.id) }}")
    };
    let live_fragment_impl = if live {
        format!(
            "\nimpl autumn_web::live::LiveFragment for {pascal_name} {{\n\
             \x20\x20\x20\x20fn dom_id_for(id: i64) -> String {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20format!(\"{snake_name}-{{id}}\")\n\
             \x20\x20\x20\x20}}\n\
             \x20\x20\x20\x20fn dom_id(&self) -> String {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Self::dom_id_for(self.id)\n\
             \x20\x20\x20\x20}}\n\
             \x20\x20\x20\x20fn render_fragment(&self) -> maud::Markup {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20maud::html! {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20li id=(self.dom_id()) {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20{fragment_item_content}\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20}}\n\
             \x20\x20\x20\x20\x20\x20\x20\x20}}\n\
             \x20\x20\x20\x20}}\n\
             \x20\x20\x20\x20fn insert_swap() -> autumn_web::htmx::OobSwap {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20autumn_web::htmx::OobSwap::Target(\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20autumn_web::htmx::OobMethod::BeforeEnd,\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\"#{list_id}\".to_string(),\n\
             \x20\x20\x20\x20\x20\x20\x20\x20)\n\
             \x20\x20\x20\x20}}\n\
             }}\n"
        )
    } else {
        String::new()
    };
    format!(
        "{doc_comment}\n\
         use crate::models::{snake_name}::{{{pascal_name}, New{pascal_name}, Update{pascal_name}}};\n\
         use crate::schema::{plural};\n\
         \n\
         #[autumn_web::repository({pascal_name}, api = \"/api/{plural}\"{soft_delete_attr}{broadcasts_attr})]\n\
         pub trait {pascal_name}Repository {{\n\
{query_body}\
         }}\n\
{live_fragment_impl}\
{api_stream_handler}"
    )
}

fn render_repository_queries(pascal_name: &str, queries: &[QuerySpec]) -> String {
    let mut out = String::with_capacity(queries.len() * 64);
    for query in queries {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "    fn {method}({field}: {rust_type}) -> Vec<{pascal_name}>;",
            method = query.method,
            field = query.field_name,
            rust_type = query.rust_type,
        );
    }
    out
}

/// Normalizes an HTML `datetime-local` value (`YYYY-MM-DDTHH:MM`, seconds
/// omitted by the browser at minute granularity) into the
/// `YYYY-MM-DDTHH:MM:SS` shape chrono's parser expects at minimum. Callers
/// parse with the `%.f` format specifier, so fractional seconds (submitted
/// when a finer-grained `step` is used) are accepted whether or not this
/// function's padding runs.
const NORMALIZE_DATETIME_LOCAL_FN: &str = r#"
fn normalize_datetime_local(raw: &str) -> String {
    if raw.chars().count() == 16 {
        format!("{raw}:00")
    } else {
        raw.to_string()
    }
}
"#;

const DESERIALIZE_NAIVE_DATETIME_LOCAL_FN: &str = r#"
fn deserialize_naive_datetime_local<'de, D>(deserializer: D) -> Result<chrono::NaiveDateTime, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    chrono::NaiveDateTime::parse_from_str(&normalize_datetime_local(&raw), "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(serde::de::Error::custom)
}
"#;

const DESERIALIZE_OPTION_NAIVE_DATETIME_LOCAL_FN: &str = r#"
fn deserialize_option_naive_datetime_local<'de, D>(
    deserializer: D,
) -> Result<Option<chrono::NaiveDateTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    match raw {
        Some(s) if !s.is_empty() => {
            chrono::NaiveDateTime::parse_from_str(&normalize_datetime_local(&s), "%Y-%m-%dT%H:%M:%S%.f")
                .map(Some)
                .map_err(serde::de::Error::custom)
        }
        _ => Ok(None),
    }
}
"#;

const DESERIALIZE_UTC_DATETIME_LOCAL_FN: &str = r#"
fn deserialize_utc_datetime_local<'de, D>(
    deserializer: D,
) -> Result<chrono::DateTime<chrono::Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    chrono::NaiveDateTime::parse_from_str(&normalize_datetime_local(&raw), "%Y-%m-%dT%H:%M:%S%.f")
        .map(|ndt| ndt.and_utc())
        .map_err(serde::de::Error::custom)
}
"#;

const DESERIALIZE_OPTION_UTC_DATETIME_LOCAL_FN: &str = r#"
fn deserialize_option_utc_datetime_local<'de, D>(
    deserializer: D,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    match raw {
        Some(s) if !s.is_empty() => {
            chrono::NaiveDateTime::parse_from_str(&normalize_datetime_local(&s), "%Y-%m-%dT%H:%M:%S%.f")
                .map(|ndt| Some(ndt.and_utc()))
                .map_err(serde::de::Error::custom)
        }
        _ => Ok(None),
    }
}
"#;

/// Tracks which `datetime-local` deserialize helpers a `DecodedForm` actually
/// references, so [`datetime_helper_fns`] emits only what's used — an unused
/// helper would be dead code in the generated project.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // orthogonal flags on a plain tally, not a state machine
struct DatetimeHelpersNeeded {
    naive_scalar: bool,
    naive_option: bool,
    utc_scalar: bool,
    utc_option: bool,
}

/// The `#[serde(...)]` attribute + struct-field line for a `NaiveDateTime`/
/// `DateTime` field, recording which shared deserialize helper it needs.
fn datetime_struct_field_line(f: &Field, needed: &mut DatetimeHelpersNeeded) -> String {
    let attr = match (f.kind, f.nullable) {
        (FieldKind::NaiveDateTime, false) => {
            needed.naive_scalar = true;
            "#[serde(deserialize_with = \"deserialize_naive_datetime_local\")]"
        }
        (FieldKind::NaiveDateTime, true) => {
            needed.naive_option = true;
            "#[serde(default, deserialize_with = \"deserialize_option_naive_datetime_local\")]"
        }
        (FieldKind::DateTime, false) => {
            needed.utc_scalar = true;
            "#[serde(deserialize_with = \"deserialize_utc_datetime_local\")]"
        }
        (FieldKind::DateTime, true) => {
            needed.utc_option = true;
            "#[serde(default, deserialize_with = \"deserialize_option_utc_datetime_local\")]"
        }
        _ => unreachable!("called only for NaiveDateTime | DateTime fields"),
    };
    format!(
        "    {attr}\n    pub {name}: {rust_type},\n",
        name = f.name,
        rust_type = f.rust_type()
    )
}

/// Assemble just the shared helper functions a `DecodedForm` needs, per
/// [`DatetimeHelpersNeeded`].
fn datetime_helper_fns(needed: &DatetimeHelpersNeeded) -> String {
    let mut helper_fns = String::new();
    if needed.naive_scalar || needed.naive_option || needed.utc_scalar || needed.utc_option {
        helper_fns.push_str(NORMALIZE_DATETIME_LOCAL_FN);
    }
    if needed.naive_scalar {
        helper_fns.push_str(DESERIALIZE_NAIVE_DATETIME_LOCAL_FN);
    }
    if needed.naive_option {
        helper_fns.push_str(DESERIALIZE_OPTION_NAIVE_DATETIME_LOCAL_FN);
    }
    if needed.utc_scalar {
        helper_fns.push_str(DESERIALIZE_UTC_DATETIME_LOCAL_FN);
    }
    if needed.utc_option {
        helper_fns.push_str(DESERIALIZE_OPTION_UTC_DATETIME_LOCAL_FN);
    }
    helper_fns
}

/// Emit the `DecodedForm` struct, its field-by-field mapping into
/// `New{Pascal}`, and any shared helper functions its `#[serde(...)]`
/// attributes reference (currently just the `datetime-local` deserializers).
///
/// Returns `(decoded_struct, mapping_fields, helper_fns)`. `helper_fns` is
/// empty unless a `NaiveDateTime`/`DateTime` field is present.
fn render_decoded_form(_pascal_name: &str, fields: &[Field]) -> (String, String, String) {
    use std::fmt::Write;
    let mut struct_fields = String::new();
    let mut mapping_fields = String::new();
    let mut needed = DatetimeHelpersNeeded::default();

    for f in fields {
        if f.kind.is_attachment() {
            let _ = writeln!(
                struct_fields,
                "    pub {name}: Option<String>,",
                name = f.name
            );
            let _ = writeln!(
                mapping_fields,
                "        {name}: if let Some(ref key) = decoded.{name} {{\n\
                     if key.is_empty() {{\n\
                         None\n\
                     }} else {{\n\
                         let store = state.extension::<autumn_web::storage::BlobStoreState>()\n\
                             .ok_or_else(|| autumn_web::AutumnError::internal_server_error_msg(\"storage not configured\"))?\n\
                             .store();\n\
                         let blob = autumn_web::storage::complete_direct_upload(&**store, key).await\n\
                             .map_err(|err| autumn_web::AutumnError::bad_request_msg(format!(\"file upload verification failed: {{err}}\")))?;\n\
                         Some(blob)\n\
                     }}\n\
                 }} else {{\n\
                     None\n\
                 }},",
                name = f.name
            );
        } else if f.kind == FieldKind::Bool {
            // Unchecked checkboxes are absent from submitted form data;
            // `#[serde(default)]` maps that absence to `false` instead of a
            // "missing field" 400.
            let _ = writeln!(
                struct_fields,
                "    #[serde(default)]\n    pub {name}: {rust_type},",
                name = f.name,
                rust_type = f.rust_type()
            );
            let _ = writeln!(
                mapping_fields,
                "        {name}: decoded.{name},",
                name = f.name
            );
        } else if matches!(f.kind, FieldKind::NaiveDateTime | FieldKind::DateTime) {
            struct_fields.push_str(&datetime_struct_field_line(f, &mut needed));
            let _ = writeln!(
                mapping_fields,
                "        {name}: decoded.{name},",
                name = f.name
            );
        } else if let Some(enum_ty) = f.enum_type_name() {
            // Decoded as a plain `String` (not `serde`-deserialized straight
            // into the generated enum — `serde_urlencoded`'s support for
            // unit-variant enums is unreliable and its error wouldn't name
            // the field), then parsed via the enum's `FromStr`. An
            // out-of-set value yields a 400 naming the field, not a 500 or a
            // silently-coerced/dropped value.
            if f.nullable {
                // A blank nullable field is already filtered out of the
                // encoded pairs above (`is_nullable_form_field`), and
                // `serde_urlencoded` treats a wholly-absent key as `None` for
                // an `Option<…>` field — the same convention every other
                // nullable field kind relies on in the `else` branch below.
                let _ = writeln!(
                    struct_fields,
                    "    pub {name}: Option<String>,",
                    name = f.name
                );
                let _ = writeln!(
                    mapping_fields,
                    "        {name}: decoded.{name}\n\
                         \x20\x20\x20\x20\x20\x20\x20\x20.map(|v| v.parse::<{enum_ty}>())\n\
                         \x20\x20\x20\x20\x20\x20\x20\x20.transpose()\n\
                         \x20\x20\x20\x20\x20\x20\x20\x20.map_err(|err| autumn_web::AutumnError::bad_request_msg(format!(\"{name}: {{err}}\")))?,",
                    name = f.name,
                    enum_ty = enum_ty
                );
            } else {
                let _ = writeln!(struct_fields, "    pub {name}: String,", name = f.name);
                let _ = writeln!(
                    mapping_fields,
                    "        {name}: decoded.{name}.parse::<{enum_ty}>()\n\
                         \x20\x20\x20\x20\x20\x20\x20\x20.map_err(|err| autumn_web::AutumnError::bad_request_msg(format!(\"{name}: {{err}}\")))?,",
                    name = f.name,
                    enum_ty = enum_ty
                );
            }
        } else {
            let _ = writeln!(
                struct_fields,
                "    pub {name}: {rust_type},",
                name = f.name,
                rust_type = f.rust_type()
            );
            let _ = writeln!(
                mapping_fields,
                "        {name}: decoded.{name},",
                name = f.name
            );
        }
    }

    let decoded_struct = format!(
        "#[derive(serde::Deserialize)]\n\
         struct DecodedForm {{\n\
         {struct_fields}\
         }}"
    );

    (decoded_struct, mapping_fields, datetime_helper_fns(&needed))
}

#[allow(
    clippy::too_many_lines,
    reason = "This is a single template — splitting it produces less readable output, \
              not more. The whole point is one place that prints one file."
)]
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn render_routes_file(
    pascal_name: &str,
    snake_name: &str,
    plural: &str,
    fields: &[Field],
    all_fields: &[Field],
    sharded: bool,
    soft_delete: bool,
    id_type: IdType,
    live: bool,
    live_validation: bool,
    validations: &BTreeMap<String, Vec<String>>,
) -> String {
    let id_rust = id_type.rust_type();
    let validated_fields: Vec<&str> = validations.keys().map(String::as_str).collect();
    let create_inputs =
        render_create_form_inputs(fields, live_validation, &validated_fields, plural);
    let edit_inputs = render_edit_form_inputs(fields, live_validation, &validated_fields, plural);
    let update_columns = render_update_columns(plural, fields);
    let nullable_field_match = render_nullable_field_match(fields);
    let has_attachments = has_attachment_fields(fields);
    let (decoded_form_struct, decoded_form_mapping, datetime_helper_fns) =
        render_decoded_form(pascal_name, fields);
    // Enum fields need their generated Rust type in scope here — the
    // `DecodedForm` mapping parses into it (see `render_decoded_form`) and
    // the edit form's `selected[...]` expressions compare against its
    // variants (see the `FieldKind::Enum` arm of `render_edit_form_inputs`).
    let enum_import_suffix: String =
        fields
            .iter()
            .filter_map(Field::enum_type_name)
            .fold(String::new(), |mut out, ty| {
                let _ = write!(out, ", {ty}");
                out
            });
    // The destroy handler must honour the resource's delete semantics: when the
    // scaffold was generated with `--soft-delete`, mark `deleted_at` (matching
    // the soft-delete repository) instead of issuing a physical `DELETE`.
    let destroy_stmt = if live {
        if sharded {
            format!(
                "let repo = Pg{pascal_name}Repository::from_shard(&db);\n    \
                 repo.delete_by_id(*id).await?;\n    \
                 let deleted = 1;"
            )
        } else {
            "repo.delete_by_id(*id).await?;\n    let deleted = 1;".to_owned()
        }
    } else if soft_delete {
        // Filter on `deleted_at IS NULL` so deleting an already-soft-deleted row
        // affects zero rows and returns 404, matching the physical-delete path.
        format!(
            "let deleted = diesel::update(\n        {plural}::table.find(*id).filter({plural}::deleted_at.is_null()),\n    )\n        \
                 .set({plural}::deleted_at.eq(Some(chrono::Utc::now().naive_utc())))\n        \
                 .execute(&mut *db)\n        .await?;"
        )
    } else {
        format!(
            "let deleted = diesel::delete({plural}::table.find(*id))\n        \
                 .execute(&mut *db)\n        .await?;"
        )
    };

    let create_stmt = if live {
        if sharded {
            format!(
                "let repo = Pg{pascal_name}Repository::from_shard(&db);\n    \
                 repo.save(&new).await?;"
            )
        } else {
            "repo.save(&new).await?;".to_owned()
        }
    } else {
        format!(
            "diesel::insert_into({plural}::table)\n        \
             .values(&new)\n        \
             .execute(&mut *db)\n        .await?;"
        )
    };

    let update_changeset_expr = render_update_changeset_expr(pascal_name, fields);
    let update_stmt = if live {
        if sharded {
            format!(
                "let repo = Pg{pascal_name}Repository::from_shard(&db);\n    \
                 let update_changes = {update_changeset_expr};\n    \
                 repo.update(*id, &update_changes).await?;\n    \
                 let updated = 1;"
            )
        } else {
            format!(
                "let update_changes = {update_changeset_expr};\n    \
                 repo.update(*id, &update_changes).await?;\n    \
                 let updated = 1;"
            )
        }
    } else {
        format!(
            "let updated = diesel::update({plural}::table.find(*id))\n        \
             .set(({update_columns}))\n        \
             .execute(&mut *db)\n        .await?;"
        )
    };

    // Forms remain URL-encoded for compatibility with the generated handlers.
    // File uploads are handled separately via direct-upload URLs generated in
    // a CSRF-protected endpoint (see docs/guide/storage.md#direct-uploads).
    let form_enctype = "";

    let db_ty = if sharded { "ShardedDb" } else { "Db" };
    let create_signature = if live && !sharded {
        if has_attachments {
            format!(
                "flash: Flash, state: autumn_web::extract::State<autumn_web::AppState>, repo: Pg{pascal_name}Repository, body: Bytes"
            )
        } else {
            format!("flash: Flash, repo: Pg{pascal_name}Repository, body: Bytes")
        }
    } else {
        if has_attachments {
            format!(
                "flash: Flash, state: autumn_web::extract::State<autumn_web::AppState>, mut db: {db_ty}, body: Bytes"
            )
        } else {
            format!("flash: Flash, mut db: {db_ty}, body: Bytes")
        }
    };

    let update_signature = if live && !sharded {
        if has_attachments {
            format!(
                "flash: Flash,\n    state: autumn_web::extract::State<autumn_web::AppState>,\n    id: Path<{id_rust}>,\n    repo: Pg{pascal_name}Repository,\n    body: Bytes,"
            )
        } else {
            format!(
                "flash: Flash,\n    id: Path<{id_rust}>,\n    repo: Pg{pascal_name}Repository,\n    body: Bytes,"
            )
        }
    } else {
        if has_attachments {
            format!(
                "flash: Flash,\n    state: autumn_web::extract::State<autumn_web::AppState>,\n    id: Path<{id_rust}>,\n    mut db: {db_ty},\n    body: Bytes,"
            )
        } else {
            format!(
                "flash: Flash,\n    id: Path<{id_rust}>,\n    mut db: {db_ty},\n    body: Bytes,"
            )
        }
    };

    let destroy_signature_arg = if live && !sharded {
        format!("repo: Pg{pascal_name}Repository")
    } else {
        format!("mut db: {db_ty}")
    };

    let (decode_create_call, decode_update_call, decode_form_sig) = if has_attachments {
        (
            "decode_form(&state, body).await?".to_owned(),
            "decode_form(&state, body).await?".to_owned(),
            format!(
                "async fn decode_form(state: &autumn_web::AppState, body: Bytes) -> AutumnResult<New{pascal_name}>"
            ),
        )
    } else {
        (
            "decode_form(body)?".to_owned(),
            "decode_form(body)?".to_owned(),
            format!("fn decode_form(body: Bytes) -> AutumnResult<New{pascal_name}>"),
        )
    };

    // The `index` handler: when sharded, use from_shard explicitly so the
    // generated code shows the canonical sharding pattern.
    //
    // Live (SSE) variant: keep the <ul>/<li> structure intact. LiveFragment
    // renders `li id=…` and insert_swap() targets `#{plural}-list` via
    // OobSwap::Target(BeforeEnd, …). Swapping to <table> would cause the SSE
    // broadcast to append <li> into a <table> at runtime (invalid HTML). The
    // table migration for the live path is a follow-up once LiveFragment
    // supports <tr> fragments.
    //
    // Non-live variants: use data_table so the index shows real fields out of
    // the box — no hand-authored <table>/<th>/<td> tags needed.
    let li_render = if live {
        format!(
            r#"li id=(format!("{snake_name}-{{}}", row.id)) {{ a href=(format!("/{plural}/{{}}", row.id)) {{ "{pascal_name} #{{}}" (row.id) }} }}"#
        )
    } else {
        String::new() // unused in the non-live path
    };

    // For the live path we keep the original <ul> list so the SSE OOB-swap
    // contract remains valid.
    let live_ul_render = if live {
        format!(
            r#"@if page_req.page() == 1 {{
            ul id="{plural}-list" hx-ext="sse" sse-connect="/{plural}/events" sse-swap="message" hx-swap="none" {{
                @for row in &page_data.content {{
                    {li_render}
                }}
            }}
        }} @else {{
            ul id="{plural}-list" {{
                @for row in &page_data.content {{
                    {li_render}
                }}
            }}
        }}"#
        )
    } else {
        String::new()
    };

    // For non-live paths, generate the data_table columns and call.
    let columns_let = if live {
        String::new()
    } else {
        render_columns_vec(pascal_name, plural, fields)
    };
    let table_render = if live {
        String::new()
    } else {
        format!(
            r#"(autumn_web::widgets::data_table(&page_data.content, &columns, &autumn_web::widgets::DataTableConfig::new("No {plural} yet.").base_path("/{plural}")))"#
        )
    };

    let list_render = if live { &live_ul_render } else { &table_render };
    let show_rows = render_show_property_rows(all_fields);

    let index_handler = if sharded {
        if live {
            format!(
                r#"/// `GET /{plural}` — paginated list of {snake_name}s.
///
/// Accepts `?page=N&size=M` query parameters via the [`PageRequest`] extractor.
/// Out-of-range or missing values are clamped silently — list endpoints never
/// return HTTP 400 for bad paging parameters.
#[get("/{plural}")]
pub async fn index(
    page_req: PageRequest,
    db: ShardedDb,
    flash: Flash,
) -> AutumnResult<Markup> {{
    let repo = Pg{pascal_name}Repository::from_shard(&db);
    let page_data: Page<{pascal_name}> = repo.page(&page_req).await?;
    Ok(layout("{pascal_name} index", flash.render().await, html! {{
        h1 {{ "{pascal_name}s" }}
        a href="/{plural}/new" {{ "New {pascal_name}" }}
        {list_render}
        (pagination_nav(&page_data, &PagerOptions::new("/{plural}")))
    }}))
}}"#
            )
        } else {
            format!(
                r#"/// `GET /{plural}` — paginated list of {snake_name}s.
///
/// Accepts `?page=N&size=M` query parameters via the [`PageRequest`] extractor.
/// Out-of-range or missing values are clamped silently — list endpoints never
/// return HTTP 400 for bad paging parameters.
#[get("/{plural}")]
pub async fn index(
    page_req: PageRequest,
    db: ShardedDb,
    flash: Flash,
) -> AutumnResult<Markup> {{
    let repo = Pg{pascal_name}Repository::from_shard(&db);
    let page_data: Page<{pascal_name}> = repo.page(&page_req).await?;
{columns_let}    Ok(layout("{pascal_name} index", flash.render().await, html! {{
        h1 {{ "{pascal_name}s" }}
        a href="/{plural}/new" {{ "New {pascal_name}" }}
        {list_render}
        (pagination_nav(&page_data, &PagerOptions::new("/{plural}")))
    }}))
}}"#
            )
        }
    } else if live {
        format!(
            r#"/// `GET /{plural}` — paginated list of {snake_name}s.
///
/// Accepts `?page=N&size=M` query parameters via the [`PageRequest`] extractor.
/// Out-of-range or missing values are clamped silently — list endpoints never
/// return HTTP 400 for bad paging parameters.
#[get("/{plural}")]
pub async fn index(
    page_req: PageRequest,
    repo: Pg{pascal_name}Repository,
    flash: Flash,
) -> AutumnResult<Markup> {{
    let page_data: Page<{pascal_name}> = repo.page(&page_req).await?;
    Ok(layout("{pascal_name} index", flash.render().await, html! {{
        h1 {{ "{pascal_name}s" }}
        a href="/{plural}/new" {{ "New {pascal_name}" }}
        {list_render}
        (pagination_nav(&page_data, &PagerOptions::new("/{plural}")))
    }}))
}}"#
        )
    } else {
        format!(
            r#"/// `GET /{plural}` — paginated list of {snake_name}s.
///
/// Accepts `?page=N&size=M` query parameters via the [`PageRequest`] extractor.
/// Out-of-range or missing values are clamped silently — list endpoints never
/// return HTTP 400 for bad paging parameters.
#[get("/{plural}")]
pub async fn index(
    page_req: PageRequest,
    repo: Pg{pascal_name}Repository,
    flash: Flash,
) -> AutumnResult<Markup> {{
    let page_data: Page<{pascal_name}> = repo.page(&page_req).await?;
{columns_let}    Ok(layout("{pascal_name} index", flash.render().await, html! {{
        h1 {{ "{pascal_name}s" }}
        a href="/{plural}/new" {{ "New {pascal_name}" }}
        {list_render}
        (pagination_nav(&page_data, &PagerOptions::new("/{plural}")))
    }}))
}}"#
        )
    };

    // Imports: when sharded, drop Db from brace-import and add ShardedDb separately.
    // The stream handler uses the fully-qualified axum path so no extra IntoResponse
    // import is needed.
    let db_import = if sharded {
        "use autumn_web::flash::Flash;\n\
         use autumn_web::sharding::ShardedDb;\n\
         use autumn_web::{AutumnError, AutumnResult, Markup, get, html, post, secured};"
            .to_owned()
    } else {
        "use autumn_web::flash::Flash;\n\
         use autumn_web::{AutumnError, AutumnResult, Db, Markup, get, html, post, secured};"
            .to_owned()
    };

    // When `--live-validation`, emit one inline-validation handler per validated field.
    // Each handler runs the actual declared validation rule(s) at runtime, not just
    // an empty-check stub.
    let validate_handlers = if live_validation {
        let mut vh = String::new();
        for (field_name, rules) in validations {
            let rule_comment = rules.join(", ");
            // Build the error chain: start with an empty-value check, then
            // append one branch per declared rule (url, email, length).
            // Nullable fields are not required — leave them empty → None.
            let is_required = fields
                .iter()
                .find(|f| f.name == *field_name)
                .is_none_or(|f| !f.nullable);
            let mut error_chain = if is_required {
                String::from("if value.is_empty() {\n        Some(\"required\")\n    }")
            } else {
                String::from("if value.is_empty() {\n        None\n    }")
            };
            for rule in rules {
                if rule == "url" {
                    error_chain.push_str(
                        " else if url::Url::parse(&value).is_err() {\n        Some(\"must be a valid URL\")\n    }",
                    );
                } else if rule == "email" {
                    error_chain.push_str(
                        " else if !value.contains('@')\n            || value.split_once('@').map_or(true, |(_, d)| !d.contains('.')) {\n        Some(\"must be a valid email address\")\n    }",
                    );
                } else if let Some(args_str) = rule
                    .strip_prefix("length(")
                    .and_then(|s| s.strip_suffix(")"))
                {
                    let mut min: Option<u64> = None;
                    let mut max: Option<u64> = None;
                    for part in args_str.split(',') {
                        let part = part.trim();
                        if let Some(n_str) = part.strip_prefix("min = ") {
                            if let Ok(n) = n_str.trim().parse::<u64>() {
                                min = Some(n);
                            }
                        } else if let Some(n_str) = part.strip_prefix("max = ")
                            && let Ok(n) = n_str.trim().parse::<u64>()
                        {
                            max = Some(n);
                        }
                    }
                    if min.is_none() && max.is_none() {
                        continue;
                    }
                    let cond = match (min, max) {
                        (Some(mn), Some(mx)) => {
                            format!("value.chars().count() < {mn} || value.chars().count() > {mx}")
                        }
                        (Some(mn), None) => format!("value.chars().count() < {mn}"),
                        (None, Some(mx)) => format!("value.chars().count() > {mx}"),
                        (None, None) => unreachable!(),
                    };
                    let msg = match (min, max) {
                        (Some(mn), Some(mx)) => {
                            format!("must be between {mn} and {mx} characters")
                        }
                        (Some(mn), None) => format!("must be at least {mn} characters"),
                        (None, Some(mx)) => format!("must be at most {mx} characters"),
                        (None, None) => unreachable!(),
                    };
                    let _ = write!(
                        error_chain,
                        " else if {cond} {{\n        Some(\"{msg}\")\n    }}"
                    );
                }
            }
            error_chain.push_str(" else {\n        None\n    }");

            // Build the handler string via push_str to avoid brace-escaping issues
            // between the format! template and the generated Rust { } delimiters.
            let _ = write!(
                vh,
                "\n\n/// `POST /{plural}/validate/{field_name}` — inline validation fragment.\n"
            );
            let _ = write!(
                vh,
                "///\n/// Returns an `<span id=\"{field_name}-error\">` OOB fragment with an error\n"
            );
            let _ = writeln!(
                vh,
                "/// message when the value fails the `{rule_comment}` rule, or an empty span"
            );
            vh.push_str(
                "/// when it passes. Consumed by htmx `hx-swap=\"outerHTML\"` on `hx-trigger=\"change\"`.\n",
            );
            let _ = writeln!(vh, "#[post(\"/{plural}/validate/{field_name}\")]");
            let _ = writeln!(
                vh,
                "pub async fn validate_{field_name}(body: autumn_web::reexports::axum::body::Bytes) -> autumn_web::Markup {{"
            );
            let _ = write!(
                vh,
                "    let value = url::form_urlencoded::parse(body.as_ref())\n        .find(|(k, _)| k == \"{field_name}\")\n"
            );
            vh.push_str("        .map(|(_, v)| v.to_string())\n");
            vh.push_str("        .unwrap_or_default();\n");
            let _ = writeln!(vh, "    let error: Option<&str> = {error_chain};");
            vh.push_str("    autumn_web::html! {\n");
            let _ = writeln!(vh, "        span id=\"{field_name}-error\" {{");
            vh.push_str("            @if let Some(msg) = error {\n");
            vh.push_str("                span style=\"color:red\" { (msg) }\n");
            vh.push_str("            }\n");
            vh.push_str("        }\n");
            vh.push_str("    }\n");
            vh.push_str("}\n");
        }
        vh
    } else {
        String::new()
    };

    format!(
        r"//! Generated by `autumn generate scaffold`.
//!
//! HTML route handlers for the resource. Edit freely — once generated,
//! these are ordinary user code.
{attachment_note}
use autumn_web::extract::Path;
use autumn_web::pagination::{{Page, PageRequest}};
use autumn_web::reexports::axum::body::Bytes;
use autumn_web::reexports::serde_json;
use autumn_web::security::{{CsrfFormField, CsrfToken}};
use autumn_web::ui::pagination::{{PagerOptions, pagination_nav}};
{db_import}
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{snake_name}::{{{pascal_name}, New{pascal_name}, Update{pascal_name}{enum_import_suffix}}};
use crate::repositories::{snake_name}::{{{pascal_name}Repository, Pg{pascal_name}Repository}};
use crate::schema::{plural};",
        attachment_note = if has_attachments {
            "//!\n\
             //! This scaffold includes file-attachment fields. File uploads are handled\n\
             //! via direct browser-to-storage uploads, bypassing the app process:\n\
             //!\n\
             //! 1. Add `autumn-web = {{ features = [\"storage\", \"multipart\"] }}` to Cargo.toml.\n\
             //! 2. Configure `[storage]` in `autumn.toml` (local disk for dev, S3 for prod).\n\
             //! 3. Create a CSRF-protected endpoint that calls `store.presign_put()` to\n\
             //!    generate presigned URLs for the browser.\n\
             //! 4. In your JavaScript, use the presigned URL to upload directly to storage,\n\
             //!    then call `complete_direct_upload()` before form submission.\n\
             //! See `docs/guide/storage.md#direct-uploads` for the full worked example\n\
             //! and the `examples/reddit-clone` for a complete implementation."
        } else {
            ""
        },
    ) + &{
        // Load htmx + SSE extension whenever live features are active.
        // `--live-validation` alone (without `--live`) still requires htmx for
        // the `hx-post` / `hx-trigger` / `hx-swap` attributes to fire.
        let live_head_scripts = if live {
            "\n                script src=(autumn_web::htmx::HTMX_JS_PATH) {};\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20script src=(autumn_web::htmx::HTMX_SSE_JS_PATH) {};\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20script src=(autumn_web::htmx::IDIOMORPH_JS_PATH) {};"
                .to_owned()
        } else if live_validation {
            "\n                script src=(autumn_web::htmx::HTMX_JS_PATH) {};\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20script src=(autumn_web::htmx::HTMX_SSE_JS_PATH) {};"
                .to_owned()
        } else {
            String::new()
        };
        let live_body_open = if live {
            r#"body hx-ext="morph""#
        } else {
            "body"
        };
        format!(
            r#"

fn csrf_input(csrf: Option<&CsrfToken>, field: Option<&CsrfFormField>) -> Markup {{
    let csrf_field_name = field.map(|field| field.0.as_str()).unwrap_or("_csrf");
    html! {{
        @if let Some(csrf) = csrf {{
            input type="hidden" name=(csrf_field_name) value=(csrf.token());
        }}
    }}
}}

/// Wrap content in a minimal HTML layout. Replace with your real layout
/// once you wire in Tailwind / your design system.
///
/// Pass `flash.render().await` for the `flash` argument so one-shot notices
/// (set with `flash.success(...)` before a redirect) appear on the next page.
fn layout(title: &str, flash: Markup, content: Markup) -> Markup {{
    html! {{
        (autumn_web::PreEscaped("<!DOCTYPE html>"))
        html lang="en" {{
            head {{
                meta charset="utf-8";
                title {{ (title) }}
                link rel="stylesheet" href=(autumn_web::flash::FLASH_CSS_PATH);{live_head_scripts}
            }}
            {live_body_open} {{
                (flash)
                (content)
            }}
        }}
    }}
}}

{index_handler}

/// `GET /{plural}/{{id}}` — show one {snake_name}.
#[get("/{plural}/{{id}}")]
pub async fn show(id: Path<{id_rust}>, mut db: {db_ty}, flash: Flash) -> AutumnResult<Markup> {{
    let row: {pascal_name} = {plural}::table
        .find(*id)
        .select({pascal_name}::as_select())
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;
    let props: Vec<(&str, maud::Markup)> = vec![
{show_rows}    ];
    Ok(layout(&format!("{pascal_name} #{{}}", row.id), flash.render().await, html! {{
        h1 {{ "{pascal_name} #" (row.id) }}
        (autumn_web::widgets::property_list(&props))
        a href="/{plural}" {{ "Back to list" }}
        " "
        a href=(format!("/{plural}/{{}}/edit", row.id)) {{ "Edit" }}
    }}))
}}

/// `GET /{plural}/new` — render the new-{snake_name} form.
#[secured]
#[get("/{plural}/new")]
pub async fn new_form(
    flash: Flash,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {{
    Ok(layout("New {pascal_name}", flash.render().await, html! {{
        h1 {{ "New {pascal_name}" }}
        form action="/{plural}" method="post"{form_enctype} {{
            (csrf_input(csrf.as_ref(), csrf_field.as_ref()))
{create_inputs}            button type="submit" {{ "Create" }}
        }}
    }}))
}}

/// `POST /{plural}` — accept a form submission and create a {snake_name}.
#[secured]
#[post("/{plural}")]
pub async fn create({create_signature}) -> AutumnResult<Markup> {{
    let new = {decode_create_call};
    {create_stmt}
    flash.success("{pascal_name} created").await;
    Ok(redirect_to("/{plural}"))
}}

/// `GET /{plural}/{{id}}/edit` — render the edit form. Submission goes to
/// the `update` handler below as a plain HTML POST (browsers can't submit
/// PUT directly without JS); the auto-generated JSON `PUT /api/{plural}/{{id}}`
/// remains available for API clients.
#[secured]
#[get("/{plural}/{{id}}/edit")]
pub async fn edit_form(
    id: Path<{id_rust}>,
    mut db: {db_ty},
    flash: Flash,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {{
    let row: {pascal_name} = {plural}::table
        .find(*id)
        .select({pascal_name}::as_select())
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;
    Ok(layout(&format!("Edit {pascal_name} #{{}}", row.id), flash.render().await, html! {{
        h1 {{ "Edit {pascal_name} #" (row.id) }}
        form action=(format!("/{plural}/{{}}/update", row.id)) method="post"{form_enctype} {{
            (csrf_input(csrf.as_ref(), csrf_field.as_ref()))
{edit_inputs}            button type="submit" {{ "Save" }}
        }}
        // Delete lives on this secured page (the public show page must not
        // expose a control that anonymous users can't use).
        form action=(format!("/{plural}/{{}}/delete", row.id)) method="post" {{
            (csrf_input(csrf.as_ref(), csrf_field.as_ref()))
            button type="submit" onclick="return confirm('Delete this {pascal_name}?')" {{ "Delete" }}
        }}
    }}))
}}

/// `POST /{plural}/{{id}}/update` — apply form data to a row, then redirect
/// to its show page. Uses column-by-column `diesel::update().set(...)` (same
/// convention as `examples/todo-app`) so we don't need `AsChangeset` on the
/// `New{pascal_name}` insert type.
#[secured]
#[post("/{plural}/{{id}}/update")]
pub async fn update(
    {update_signature}
) -> AutumnResult<Markup> {{
    let form = {decode_update_call};
    {update_stmt}
    if updated == 0 {{
        return Err(AutumnError::not_found_msg(format!(
            "{pascal_name} with id {{}} not found", *id
        )));
    }}
    flash.success("{pascal_name} updated").await;
    Ok(redirect_to(&format!("/{plural}/{{}}", *id)))
}}

/// `POST /{plural}/{{id}}/delete` — delete a row, then redirect to the list.
/// Browsers can't submit `DELETE` without JS, so the show page's delete button
/// posts here; the JSON `DELETE /api/{plural}/{{id}}` stays available for API
/// clients via the auto-generated repository handler. Honours the resource's
/// soft-delete configuration (marks `deleted_at` when `--soft-delete` is set).
#[secured]
#[post("/{plural}/{{id}}/delete")]
pub async fn destroy(
    id: Path<{id_rust}>,
    {destroy_signature_arg},
    flash: Flash,
) -> AutumnResult<Markup> {{
    {destroy_stmt}
    if deleted == 0 {{
        return Err(AutumnError::not_found_msg(format!(
            "{pascal_name} with id {{}} not found", *id
        )));
    }}
    flash.success("{pascal_name} deleted").await;
    Ok(redirect_to("/{plural}"))
}}

{decoded_form_struct}
{datetime_helper_fns}
{decode_form_sig} {{
    let pairs: Vec<_> = url::form_urlencoded::parse(body.as_ref())
        .filter(|(key, value)| !(value.is_empty() && is_nullable_form_field(key)))
        .collect();
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(key, value)| (key.as_ref(), value.as_ref())))
        .finish();

    let decoded: DecodedForm = serde_urlencoded::from_str(&encoded)
        .map_err(|err| AutumnError::bad_request_msg(format!("invalid form submission: {{err}}")))?;

    Ok(New{pascal_name} {{
{decoded_form_mapping}    }})
}}

fn is_nullable_form_field(name: &str) -> bool {{
    {nullable_field_match}
}}

fn redirect_to(url: &str) -> Markup {{
    html! {{
        (autumn_web::PreEscaped("<!DOCTYPE html>"))
        html {{ head {{
            meta http-equiv="refresh" content=(format!("0;url={{url}}"));
        }} body {{ p {{ "Redirecting to " a href=(url) {{ (url) }} "…" }} }} }}
    }}
}}
"#
        )
    } + &if live {
        format!(
            r#"

/// `GET /{plural}/events` — Server-Sent Events stream for live updates.
#[get("/{plural}/events")]
pub async fn events(
    state: autumn_web::extract::State<autumn_web::AppState>,
) -> impl autumn_web::reexports::axum::response::IntoResponse {{
    autumn_web::sse::stream(&state, "{plural}")
}}"#
        )
    } else {
        String::new()
    } + &validate_handlers
}

fn render_update_changeset_expr(pascal_name: &str, fields: &[Field]) -> String {
    use std::fmt::Write;
    let mut out = format!("Update{pascal_name} {{\n");
    for f in fields {
        let name = &f.name;
        writeln!(
            out,
            "        {name}: autumn_web::hooks::Patch::Set(form.{name}.clone()),"
        )
        .unwrap();
    }
    out.push_str("    }");
    out
}

/// Whether any field in `fields` is a file attachment.
fn has_attachment_fields(fields: &[Field]) -> bool {
    fields.iter().any(|f| f.kind.is_attachment())
}

// `render_create_form_inputs` and `render_edit_form_inputs` below hand-roll
// bare HTML (no `Changeset`/`ChangesetForm`, no `autumn-field` wrapper divs
// or ARIA wiring) rather than calling `autumn_web::form::{checkbox_input,
// number_input, date_input, datetime_input, select_input}` — consistent
// with how every other field kind (including plain `String`) has always
// been emitted here, not a pattern introduced for these widgets. That means
// the two are independently maintained and can drift; at minimum, keep
// these invariants in sync with the `autumn_web::form` helpers of the same
// name when either changes:
//   - `Bool` (non-nullable): a bare `<input type="checkbox">`, **no** hidden
//     `value="false"` sibling sharing the `name` — see `checkbox_input`'s
//     doc comment for why (duplicate-key 400 on every checked submission).
//   - `Bool` (nullable): a 3-option `<select>` (unset/true/false), never a
//     checkbox — a checkbox can't represent a `None` distinct from `false`.
//   - `I32`/`I64`/`F32`/`F64`: `type="number"` with `step="1"` for integers,
//     `step="any"` for floats.
//   - `NaiveDateTime`/`DateTime`: `type="datetime-local"`, decoded via a
//     `%.f`-tolerant parser (see `DESERIALIZE_*_DATETIME_LOCAL_FN` below).
fn render_create_form_inputs(
    fields: &[Field],
    live_validation: bool,
    validated: &[&str],
    plural: &str,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for f in fields {
        if f.kind.is_attachment() {
            // Attachment fields render as file inputs; the form must use
            // enctype="multipart/form-data" (set by render_routes_file when
            // attachment fields are present). Upload logic (storage backend
            // + blob binding) requires the `autumn-web` `storage` and
            // `multipart` features and is left for the app author to wire.
            let _ = writeln!(
                out,
                "            label {{ \"{name}\" }} input type=\"file\" name=\"{name}\";",
                name = f.name
            );
        } else {
            let required = required_attr(f);
            let hx_attrs = if live_validation && validated.contains(&f.name.as_str()) {
                format!(
                    " hx-post=\"/{plural}/validate/{name}\" hx-trigger=\"change\" hx-target=\"#{name}-error\" hx-swap=\"outerHTML\"",
                    plural = plural,
                    name = f.name
                )
            } else {
                String::new()
            };
            let error_span = if live_validation && validated.contains(&f.name.as_str()) {
                format!("\n            span id=\"{name}-error\" {{}}", name = f.name)
            } else {
                String::new()
            };
            let input_tag = match (f.kind, f.nullable) {
                // No hidden `false` fallback: a checked box would then submit
                // the key twice (`field=false` from the hidden input,
                // `field=true` from the checkbox), and serde_urlencoded
                // rejects duplicate keys instead of taking the last value —
                // every checked submission would 400. `#[serde(default)]` on
                // the DecodedForm field (see render_decoded_form) recovers
                // `false` from the key's *absence* when unchecked instead.
                (FieldKind::Bool, false) => format!(
                    "input type=\"checkbox\" name=\"{name}\" value=\"true\"{hx_attrs}",
                    name = f.name,
                    hx_attrs = hx_attrs
                ),
                // A checkbox can't losslessly represent a nullable bool (no
                // way to distinguish "leave false" from "set to null" when
                // unchecked) — a 3-option select keeps NULL reachable.
                (FieldKind::Bool, true) => format!(
                    "select name=\"{name}\"{hx_attrs} {{ \
                         option value=\"\" {{ \"— Unset —\" }} \
                         option value=\"true\" {{ \"Yes\" }} \
                         option value=\"false\" {{ \"No\" }} \
                     }}",
                    name = f.name,
                    hx_attrs = hx_attrs
                ),
                (FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64, _) => format!(
                    "input type=\"number\" name=\"{name}\" step=\"{step}\"{required}{hx_attrs}",
                    name = f.name,
                    step = number_step(f.kind),
                    required = required,
                    hx_attrs = hx_attrs
                ),
                // `step="any"` lets the browser's picker show/accept
                // seconds — see edit_datetime_local_value_expr for why a
                // value with seconds must not be step-mismatch-rejected.
                (FieldKind::NaiveDateTime | FieldKind::DateTime, _) => format!(
                    "input type=\"datetime-local\" name=\"{name}\" step=\"any\"{required}{hx_attrs}",
                    name = f.name,
                    required = required,
                    hx_attrs = hx_attrs
                ),
                // A closed-set field always renders as a `<select>` — one
                // `<option>` per variant, matching the admin generator's
                // `--select` widget output (see `admin::render_select_kind`).
                (FieldKind::Enum, _) => {
                    let placeholder = if f.nullable {
                        "— Unset —"
                    } else {
                        "— Select —"
                    };
                    let mut options_body = format!("option value=\"\" {{ \"{placeholder}\" }}");
                    for v in &f.variants {
                        let label = humanize_label(v);
                        let _ = write!(options_body, " option value=\"{v}\" {{ \"{label}\" }}");
                    }
                    format!(
                        "select name=\"{name}\"{required}{hx_attrs} {{ {options_body} }}",
                        name = f.name,
                        required = required,
                        hx_attrs = hx_attrs,
                        options_body = options_body
                    )
                }
                _ => format!(
                    "input type=\"text\" name=\"{name}\"{required}{hx_attrs}",
                    name = f.name,
                    required = required,
                    hx_attrs = hx_attrs
                ),
            };
            let _ = writeln!(
                out,
                "            label {{ \"{name}\" }} {input_tag};{error_span}",
                name = f.name,
                input_tag = input_tag,
                error_span = error_span
            );
        }
    }
    out
}

/// The HTML `step` attribute value for a `number_input`-shaped `FieldKind`.
/// Integers step by whole numbers; floating-point fields allow any value.
const fn number_step(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::F32 | FieldKind::F64 => "any",
        _ => "1",
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "One match arm per field-kind widget — splitting it produces less \
              readable output, not more. See render_create_form_inputs's \
              module-level comment for the shared invariants across both."
)]
fn render_edit_form_inputs(
    fields: &[Field],
    live_validation: bool,
    validated: &[&str],
    plural: &str,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for f in fields {
        if f.kind.is_attachment() {
            let _ = writeln!(
                out,
                "            label {{ \"{name}\" }} input type=\"file\" name=\"{name}\";\n\
                 @if let Some(ref blob) = row.{name} {{\n\
                     input type=\"hidden\" name=\"{name}\" value=(blob.key);\n\
                 }}",
                name = f.name
            );
        } else {
            let required = required_attr(f);
            let hx_attrs = if live_validation && validated.contains(&f.name.as_str()) {
                format!(
                    " hx-post=\"/{plural}/validate/{name}\" hx-trigger=\"change\" hx-target=\"#{name}-error\" hx-swap=\"outerHTML\"",
                    plural = plural,
                    name = f.name
                )
            } else {
                String::new()
            };
            let error_span = if live_validation && validated.contains(&f.name.as_str()) {
                format!("\n            span id=\"{name}-error\" {{}}", name = f.name)
            } else {
                String::new()
            };
            let input_tag = match (f.kind, f.nullable) {
                // See render_create_form_inputs for why there is no hidden
                // `false` fallback sibling here.
                (FieldKind::Bool, false) => format!(
                    "input type=\"checkbox\" name=\"{name}\" value=\"true\" checked[{checked}]{hx_attrs}",
                    name = f.name,
                    checked = edit_checked_expr(f),
                    hx_attrs = hx_attrs
                ),
                (FieldKind::Bool, true) => {
                    let (unset, is_true, is_false) = edit_bool_select_selected_exprs(f);
                    format!(
                        "select name=\"{name}\"{hx_attrs} {{ \
                             option value=\"\" selected[{unset}] {{ \"— Unset —\" }} \
                             option value=\"true\" selected[{is_true}] {{ \"Yes\" }} \
                             option value=\"false\" selected[{is_false}] {{ \"No\" }} \
                         }}",
                        name = f.name,
                        hx_attrs = hx_attrs
                    )
                }
                (FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64, _) => format!(
                    "input type=\"number\" name=\"{name}\" step=\"{step}\" value=({value}){required}{hx_attrs}",
                    name = f.name,
                    step = number_step(f.kind),
                    value = edit_value_expr(f),
                    required = required,
                    hx_attrs = hx_attrs
                ),
                (FieldKind::NaiveDateTime | FieldKind::DateTime, _) => format!(
                    "input type=\"datetime-local\" name=\"{name}\" step=\"any\" value=({value}){required}{hx_attrs}",
                    name = f.name,
                    value = edit_datetime_local_value_expr(f),
                    required = required,
                    hx_attrs = hx_attrs
                ),
                (FieldKind::Enum, _) => {
                    let placeholder = if f.nullable {
                        "— Unset —"
                    } else {
                        "— Select —"
                    };
                    let unset_selected = if f.nullable {
                        format!("row.{}.is_none()", f.name)
                    } else {
                        "false".to_owned()
                    };
                    let mut options_body = format!(
                        "option value=\"\" selected[{unset_selected}] {{ \"{placeholder}\" }}"
                    );
                    let enum_ty = f
                        .enum_type_name()
                        .expect("FieldKind::Enum always has an enum_type_name");
                    for v in &f.variants {
                        let label = humanize_label(v);
                        let variant = pascal(v);
                        let selected_expr = if f.nullable {
                            format!("row.{} == Some({enum_ty}::{variant})", f.name)
                        } else {
                            format!("row.{} == {enum_ty}::{variant}", f.name)
                        };
                        let _ = write!(
                            options_body,
                            " option value=\"{v}\" selected[{selected_expr}] {{ \"{label}\" }}"
                        );
                    }
                    format!(
                        "select name=\"{name}\"{required}{hx_attrs} {{ {options_body} }}",
                        name = f.name,
                        required = required,
                        hx_attrs = hx_attrs,
                        options_body = options_body
                    )
                }
                _ => format!(
                    "input type=\"text\" name=\"{name}\" value=({value}){required}{hx_attrs}",
                    name = f.name,
                    value = edit_value_expr(f),
                    required = required,
                    hx_attrs = hx_attrs
                ),
            };
            let _ = writeln!(
                out,
                "            label {{ \"{name}\" }} {input_tag};{error_span}",
                name = f.name,
                input_tag = input_tag,
                error_span = error_span
            );
        }
    }
    out
}

const fn required_attr(field: &Field) -> &'static str {
    if field.nullable { "" } else { " required" }
}

fn edit_value_expr(field: &Field) -> String {
    let name = &field.name;
    match (field.nullable, field.kind) {
        // Attachment fields don't render a value in text inputs — they have
        // their own <input type="file"> generated by render_edit_form_inputs.
        (_, FieldKind::Attachment) => String::new(),
        (true, FieldKind::Bytea) => {
            format!(
                "row.{name}.as_ref().map(|value| String::from_utf8_lossy(value).to_string()).unwrap_or_default()"
            )
        }
        // f32/f64 Display renders NaN/Infinity as "NaN"/"inf"/"-inf", none of
        // which satisfy HTML5's <input type="number"> value grammar — the
        // browser would silently blank the field. Render an explicit empty
        // value for non-finite floats instead of an invalid one.
        (true, FieldKind::F32 | FieldKind::F64) => {
            format!(
                "row.{name}.as_ref().filter(|value| value.is_finite()).map(ToString::to_string).unwrap_or_default()"
            )
        }
        (false, FieldKind::F32 | FieldKind::F64) => {
            format!(
                "if row.{name}.is_finite() {{ row.{name}.to_string() }} else {{ String::new() }}"
            )
        }
        (true, _) => {
            format!("row.{name}.as_ref().map(ToString::to_string).unwrap_or_default()")
        }
        (false, FieldKind::Bytea) => {
            format!("String::from_utf8_lossy(&row.{name}).to_string()")
        }
        (false, _) => format!("row.{name}.to_string()"),
    }
}

/// Boolean expression for the `checked[...]` attribute of an edit-form
/// checkbox. Only called for non-nullable `bool` fields — nullable
/// `Option<bool>` fields render as a 3-option select instead (see
/// [`edit_bool_select_selected_exprs`]), so there is no `Option<bool>` case
/// to unwrap here.
fn edit_checked_expr(field: &Field) -> String {
    format!("row.{}", field.name)
}

/// The three `selected[...]` boolean expressions (unset / true / false) for
/// an edit-form `<select>` rendering a nullable `Option<bool>` field.
///
/// A checkbox cannot losslessly represent a nullable bool (no way to
/// distinguish "leave false" from "set to null" when unchecked), so nullable
/// `Bool` fields render as this 3-option select instead of a checkbox.
fn edit_bool_select_selected_exprs(field: &Field) -> (String, String, String) {
    let name = &field.name;
    (
        format!("row.{name}.is_none()"),
        format!("row.{name} == Some(true)"),
        format!("row.{name} == Some(false)"),
    )
}

/// Value expression for an edit-form `datetime-local` input. Unlike
/// [`edit_value_expr`]'s `.to_string()` (which relies on `Display`, e.g.
/// `"2024-01-15 10:30:00 UTC"` for `DateTime<Utc>` — a shape browsers
/// reject), this formats explicitly as `YYYY-MM-DDTHH:MM[:SS[.fff]]`, the
/// value shape `<input type="datetime-local">` accepts.
///
/// Seconds/fractional-seconds are included when present (`%.f` omits them
/// entirely when zero) rather than truncated to `YYYY-MM-DDTHH:MM`: a
/// minute-only value round-trips back through the generated project's
/// `normalize_datetime_local` (see `NORMALIZE_DATETIME_LOCAL_FN` below) as
/// `:00` seconds, and the generated `update` handler writes every column
/// unconditionally — a stored `12:34:56.789` would otherwise be silently
/// overwritten as `12:34:00` by re-submitting the form without touching
/// this field. Pair with `step="any"` on the input (see
/// `render_edit_form_inputs`) so a value with seconds doesn't fail the
/// browser's step constraint validation.
fn edit_datetime_local_value_expr(field: &Field) -> String {
    let name = &field.name;
    if field.nullable {
        format!(
            "row.{name}.as_ref().map(|value| value.format(\"%Y-%m-%dT%H:%M:%S%.f\").to_string()).unwrap_or_default()"
        )
    } else {
        format!("row.{name}.format(\"%Y-%m-%dT%H:%M:%S%.f\").to_string()")
    }
}

/// Produce the cell-body expression for a `data_table` column closure.
///
/// Every arm must evaluate to a type that implements `maud::Render` (`&str`,
/// `String`, `Cow<str>`, integers). `bool`, `Option<T>`, chrono types, `Uuid`,
/// `Vec<u8>`, and `Blob` do NOT implement `Render` in maud 0.27, so we always
/// coerce via `to_string()` / `unwrap_or_default()`.
fn cell_value_expr(field: &Field) -> String {
    let name = &field.name;
    match (field.nullable, field.kind) {
        // Attachment: always Option<Blob>; show presence only, no Blob internals.
        (_, FieldKind::Attachment) => {
            format!("if row.{name}.is_some() {{ \"attachment\" }} else {{ \"—\" }}")
        }
        (true, FieldKind::Bytea) => {
            format!(
                "row.{name}.as_ref().map(|v| String::from_utf8_lossy(v).to_string()).unwrap_or_default()"
            )
        }
        // Nullable String/Text: use as_deref to avoid heap allocation.
        (true, FieldKind::String | FieldKind::Text) => {
            format!("row.{name}.as_deref().unwrap_or_default()")
        }
        // Nullable: Option<T> — no Render impl; unwrap to String.
        (true, _) => format!("row.{name}.as_ref().map(ToString::to_string).unwrap_or_default()"),
        // Non-nullable Bytea: Cow<str> does implement Render.
        (false, FieldKind::Bytea) => format!("String::from_utf8_lossy(&row.{name})"),
        // String/Text: &String implements Render via deref coercion.
        (false, FieldKind::String | FieldKind::Text) => format!("&row.{name}"),
        // Numerics (i32, i64, f32, f64): implement Render directly.
        (false, FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64) => {
            format!("row.{name}")
        }
        // Bool, Uuid, chrono types: no Render impl in maud 0.27; convert via Display.
        (false, _) => format!("row.{name}.to_string()"),
    }
}

/// Emit the `let columns: Vec<Column<Pascal>> = vec![…];` block for the index handler.
///
/// Includes an "Id" column, one column per scaffold field (title-cased header),
/// and a trailing "Show" actions column. All columns are non-sortable — server-side
/// ordering per-column is out of scope; dead sort links would be worse than none.
fn render_columns_vec(pascal_name: &str, plural: &str, fields: &[Field]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(fields.len() * 150 + 300);
    let _ = writeln!(
        out,
        "    let columns: Vec<autumn_web::widgets::Column<{pascal_name}>> = vec!["
    );
    // ID column
    let _ = writeln!(
        out,
        "        autumn_web::widgets::Column::new(\"Id\", |row: &{pascal_name}| maud::html! {{ (row.id) }}),"
    );
    // One column per field
    for f in fields {
        let header = title_case(&f.name);
        let cell_expr = cell_value_expr(f);
        let _ = writeln!(
            out,
            "        autumn_web::widgets::Column::new(\"{header}\", |row: &{pascal_name}| maud::html! {{ ({cell_expr}) }}),"
        );
    }
    // Show link column
    let _ = writeln!(
        out,
        "        autumn_web::widgets::Column::new(\"\", |row: &{pascal_name}| maud::html! {{ a href=(format!(\"/{plural}/{{}}\", row.id)) {{ \"Show\" }} }}),"
    );
    let _ = writeln!(out, "    ];");
    out
}

/// Emit the `vec![…]` body for the `props` binding in the `show` handler.
///
/// Produces one `("Label", maud::html! { value_expr })` tuple per row:
/// `id`, every DSL-declared field (humanized label), then `created_at`.
fn render_show_property_rows(fields: &[Field]) -> String {
    let mut out = String::with_capacity(fields.len() * 100 + 150);
    out.push_str("        (\"Id\", maud::html! { (row.id) }),\n");
    for f in fields {
        let label = humanize(&f.name);
        let cell_expr = cell_value_expr(f);
        out.push_str("        (\"");
        out.push_str(&label);
        out.push_str("\", maud::html! { (");
        out.push_str(&cell_expr);
        out.push_str(") }),\n");
    }
    out.push_str("        (\"Created at\", maud::html! { (row.created_at.to_string()) }),\n");
    out
}

/// Humanize a `snake_case` field name: capitalize only the first word.
///
/// `created_at` → `"Created at"`, `user_name` → `"User name"`.
/// Matches the humanization convention used in Phoenix / Rails form labels.
fn humanize(s: &str) -> String {
    let replaced = s.replace('_', " ");
    let mut chars = replaced.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().to_string() + chars.as_str()
    })
}

/// Convert `snake_case` field name to `Title Case` header label.
fn title_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().to_string() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_nullable_field_match(fields: &[Field]) -> String {
    let names = fields
        .iter()
        .filter(|field| field.nullable)
        .map(|field| format!("\"{}\"", field.name))
        .collect::<Vec<_>>();
    if names.is_empty() {
        "false".to_owned()
    } else {
        format!("matches!(name, {})", names.join(" | "))
    }
}

/// Render the column-update tuple body for the `update` handler. Emits
/// `tablename::field.eq(form.field.clone()), …` per user field, leaving the
/// auto-managed `id` and `created_at` columns alone. With no user fields the
/// body is empty (Diesel accepts `set(())` as a no-op update).
///
/// ⚡ Bolt optimization: Avoids intermediate `Vec` allocations during string formatting
/// by pre-allocating capacity and utilizing `std::fmt::Write` sequentially.
fn render_update_columns(plural: &str, fields: &[Field]) -> String {
    use std::fmt::Write;
    // Estimate 50 chars per field
    let mut out = String::with_capacity(fields.len() * 50);
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write!(
            out,
            "{plural}::{name}.eq(form.{name}.clone())",
            name = f.name
        )
        .unwrap();
    }
    out
}

/// Render one `db.execute_sql("...").await;` call per statement in `sql`,
/// escaping the SQL text so it survives as a Rust string literal in the
/// generated test.
///
/// Postgres's extended query protocol -- which `diesel::sql_query` uses --
/// rejects a single prepared statement that contains more than one SQL
/// command, so a migration's `CREATE TABLE` followed by one or more
/// `CREATE INDEX` statements must be executed one at a time. Reuses
/// [`crate::migrate::safety::split_statements`] (dollar-quote- and
/// comment-aware) rather than a naive `;`-terminated-line splitter, so it
/// stays correct if a future `--default` value embeds a semicolon.
fn render_execute_sql_calls(sql: &str) -> String {
    let mut out = String::new();
    for statement in crate::migrate::safety::split_statements(sql) {
        let escaped = escape_sql_for_rust_literal(&statement);
        let _ = writeln!(out, "db.execute_sql(\"{escaped};\").await;");
    }
    out
}

/// Escape a raw SQL string so it survives verbatim as a Rust string literal
/// (`"..."`) in generated test code — used both for the `db.execute_sql(...)`
/// setup calls above and for the raw `diesel::sql_query(...)` issued by the
/// enum out-of-set rejection smoke test (see `enum_rejection_insert_sql`).
fn escape_sql_for_rust_literal(sql: &str) -> String {
    sql.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render the `tests/<snake>.rs` smoke test's HTML (non-`--api`) shape: an
/// in-process, real-database read of the scaffolded index route.
///
/// The handler under test is a stand-in for `routes::{plural}::index` rather
/// than a literal re-export of it. A freshly scaffolded project is a binary
/// crate (no `src/lib.rs`), so a `tests/*.rs` integration binary cannot import
/// the project's own handler functions -- see
/// `docs/guide/tutorial/11-testing.md`, which documents this exact
/// constraint and the same "redeclare the handler under test" workaround.
fn render_index_smoke_test(
    pascal_name: &str,
    plural: &str,
    id_schema_type: &str,
    setup_calls: &str,
) -> String {
    format!(
        "//! Smoke test generated by `autumn generate scaffold`.\n\
         //!\n\
         //! Boots a stand-in for the scaffolded `GET /{plural}` route in-process\n\
         //! via `autumn_web::test::{{TestApp, TestClient, TestDb}}` and asserts a\n\
         //! real response against a real (throwaway) Postgres database -- no\n\
         //! running server, no hard-coded server base URL env var, no silent skip.\n\
         //!\n\
         //! The handler below is NOT the generated `routes::{plural}::index`\n\
         //! handler: a `tests/` integration binary cannot import a project's own\n\
         //! code when the project has no `src/lib.rs` (see\n\
         //! `docs/guide/tutorial/11-testing.md`), so this test redeclares a\n\
         //! minimal handler that runs the same kind of query against the same\n\
         //! table instead. That proves the database, migration, and in-process\n\
         //! request pipeline all work -- it does NOT exercise the real handler's\n\
         //! pagination, `data_table` rendering, CSRF, or flash-message logic. For\n\
         //! coverage of that, add unit tests inside `src/routes/{plural}.rs`\n\
         //! itself (it's part of the binary crate, so it can call the real code).\n\
         //!\n\
         //! `cargo test` reports this test as `ignored` (Docker is not assumed to\n\
         //! be available in every environment); run it for real with:\n\
         //!\n\
         //!     cargo test -- --ignored\n\
         \n\
         use autumn_web::prelude::*;\n\
         use autumn_web::test::{{TestApp, TestClient, TestDb}};\n\
         use diesel::prelude::*;\n\
         use diesel_async::RunQueryDsl;\n\
         \n\
         diesel::table! {{\n\
         {plural} (id) {{\n\
         id -> {id_schema_type},\n\
         }}\n\
         }}\n\
         \n\
         #[get(\"/{plural}\")]\n\
         async fn index(mut db: Db) -> AutumnResult<Markup> {{\n\
         let total: i64 = {plural}::table.count().get_result(&mut db).await?;\n\
         Ok(html! {{\n\
         h1 {{ \"{pascal_name}s\" }}\n\
         p {{ (total) \" row(s)\" }}\n\
         }})\n\
         }}\n\
         \n\
         #[tokio::test]\n\
         #[ignore = \"requires Docker (testcontainers) via TestDb; run `cargo test -- --ignored`\"]\n\
         async fn {plural}_index_renders_scaffolded_rows() {{\n\
         let db = TestDb::shared().await;\n\
         {setup_calls}\
         db.execute_sql(\"TRUNCATE {plural} RESTART IDENTITY\").await;\n\
         \n\
         let client: TestClient = TestApp::new().routes(routes![index]).with_db(db.pool()).build();\n\
         \n\
         client.get(\"/{plural}\").send().await\n\
         .assert_ok()\n\
         .assert_body_contains(\"{pascal_name}s\");\n\
         }}\n"
    )
}

/// Render the `tests/<snake>.rs` smoke test's `--api` shape: an in-process,
/// real-database read of the scaffolded JSON list route.
///
/// See [`render_index_smoke_test`] for why the handler under test is a
/// stand-in rather than a literal re-export of the generated
/// `{snake}_api_list` repository handler.
fn render_api_smoke_test(plural: &str, id_schema_type: &str, setup_calls: &str) -> String {
    format!(
        "//! Smoke test generated by `autumn generate scaffold --api`.\n\
         //!\n\
         //! Boots a stand-in for the scaffolded `GET /api/{plural}` route\n\
         //! in-process via `autumn_web::test::{{TestApp, TestClient, TestDb}}`\n\
         //! and asserts a real response against a real (throwaway) Postgres\n\
         //! database -- no running server, no hard-coded server base URL env\n\
         //! var, no silent skip.\n\
         //!\n\
         //! The handler below is NOT the generated repository's JSON list\n\
         //! handler: a `tests/` integration binary cannot import a project's own\n\
         //! code when the project has no `src/lib.rs` (see\n\
         //! `docs/guide/tutorial/11-testing.md`). This test only proves the\n\
         //! database, migration, and in-process request pipeline work -- it does\n\
         //! NOT exercise the real repository handler's serialization, filtering,\n\
         //! or pagination logic.\n\
         //!\n\
         //! `cargo test` reports this test as `ignored` (Docker is not assumed to\n\
         //! be available in every environment); run it for real with:\n\
         //!\n\
         //!     cargo test -- --ignored\n\
         \n\
         use autumn_web::prelude::*;\n\
         use autumn_web::test::{{TestApp, TestClient, TestDb}};\n\
         use diesel::prelude::*;\n\
         use diesel_async::RunQueryDsl;\n\
         \n\
         diesel::table! {{\n\
         {plural} (id) {{\n\
         id -> {id_schema_type},\n\
         }}\n\
         }}\n\
         \n\
         #[get(\"/api/{plural}\")]\n\
         async fn api_list(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {{\n\
         let total: i64 = {plural}::table.count().get_result(&mut db).await?;\n\
         Ok(Json(serde_json::json!({{ \"count\": total }})))\n\
         }}\n\
         \n\
         #[tokio::test]\n\
         #[ignore = \"requires Docker (testcontainers) via TestDb; run `cargo test -- --ignored`\"]\n\
         async fn {plural}_api_list_returns_ok_against_a_real_database() {{\n\
         let db = TestDb::shared().await;\n\
         {setup_calls}\
         db.execute_sql(\"TRUNCATE {plural} RESTART IDENTITY\").await;\n\
         \n\
         let client: TestClient = TestApp::new().routes(routes![api_list]).with_db(db.pool()).build();\n\
         \n\
         client.get(\"/api/{plural}\").send().await\n\
         .assert_ok()\n\
         .assert_json::<serde_json::Value, _>(|body| {{\n\
         assert_eq!(body[\"count\"], 0);\n\
         }});\n\
         }}\n"
    )
}

/// Emit `CREATE TABLE IF NOT EXISTS <target> (id BIGSERIAL PRIMARY KEY);` for
/// every distinct table a `references` field points at.
///
/// `TestDb::shared()` starts one Postgres testcontainer per `tests/*.rs`
/// binary (a process-global `OnceLock`), so a scaffold's own smoke test can't
/// assume some *other* scaffolded resource's smoke test already created the
/// referenced table — `CREATE TABLE comments (... REFERENCES posts(id) ...)`
/// would otherwise fail with "relation posts does not exist" whenever
/// `Comment` is scaffolded without also scaffolding `Post` in the same run.
/// A minimal stand-in table (just the `id` column the FK constraint checks
/// against) is enough to satisfy Postgres without duplicating the target's
/// full schema.
///
/// Skips `own_table` — a self-referential `references` field (e.g. a
/// `Category` model with a `category:references` field) targets the model's
/// own table, which is about to be created for real by the very next
/// statement. Postgres allows a self-referential FK within the same
/// `CREATE TABLE`, so no stub is needed there; emitting one anyway would
/// collide with the real (non-`IF NOT EXISTS`) `CREATE TABLE` that follows.
fn render_reference_stub_tables_sql(fields: &[Field], own_table: &str) -> String {
    let mut out = String::new();
    let mut seen = BTreeSet::new();
    for f in fields {
        if let Some(target) = f.reference_table()
            && target != own_table
            && seen.insert(target.clone())
        {
            let _ = writeln!(
                out,
                "CREATE TABLE IF NOT EXISTS {target} (id BIGSERIAL PRIMARY KEY);"
            );
            // Seed one row (id 1, since BIGSERIAL starts there) so a NOT NULL
            // `references` column pointing at this stub has a real id to
            // reference. Without this, any raw INSERT the smoke test issues
            // against the table under test — e.g. the enum out-of-set
            // rejection test's deliberately-invalid INSERT, see
            // `enum_rejection_insert_sql` — would fail on this FK constraint
            // regardless of the column it's actually trying to exercise,
            // masking the real assertion behind an unrelated failure.
            let _ = writeln!(out, "INSERT INTO {target} DEFAULT VALUES;");
        }
    }
    out
}

/// Render the `tests/<snake>.rs` smoke test.
///
/// Delegates to [`render_index_smoke_test`] or [`render_api_smoke_test`]
/// depending on `api`, after computing the exact `CREATE TABLE`/`CREATE
/// INDEX` SQL the generated migration also emits (see
/// [`create_table_sql_with_metadata_and_id`]), so the throwaway table the
/// test creates in `TestDb` matches the real schema. Any `references` field
/// also gets a stub target table created first — see
/// [`render_reference_stub_tables_sql`].
fn render_smoke_test(
    pascal_name: &str,
    plural: &str,
    api: bool,
    fields: &[Field],
    id_type: IdType,
    indexes: &BTreeSet<String>,
    defaults: &BTreeMap<String, String>,
) -> String {
    let stub_tables_sql = render_reference_stub_tables_sql(fields, plural);
    let create_table_sql =
        create_table_sql_with_metadata_and_id(plural, fields, indexes, defaults, id_type);
    let mut setup_calls = render_execute_sql_calls(&stub_tables_sql);
    setup_calls.push_str(&render_execute_sql_calls(&create_table_sql));
    let id_schema_type = id_type.schema_type();

    let base = if api {
        render_api_smoke_test(plural, id_schema_type, &setup_calls)
    } else {
        render_index_smoke_test(pascal_name, plural, id_schema_type, &setup_calls)
    };

    if fields.iter().any(Field::is_enum) {
        base + &render_enum_rejection_smoke_test(plural, fields, defaults, &setup_calls, api)
    } else {
        base
    }
}

/// A representative non-`NULL` literal for a `FieldKind`, used to fill in the
/// "other" `NOT NULL` columns of the raw `INSERT` [`render_enum_rejection_smoke_test`]
/// issues directly against the database — every column but the one under
/// test needs *some* valid value so the `INSERT`'s failure is attributable to
/// the target enum column's `CHECK` constraint rather than some other
/// required column being left out.
const fn sql_sample_literal(kind: FieldKind) -> &'static str {
    match kind {
        // `Enum`'s "'sample'" here is never actually used: `enum_rejection_insert_sql`
        // special-cases every enum column (the field under test gets the
        // deliberately out-of-set literal; any *other* required enum column
        // gets one of its own real variants instead — see that function).
        FieldKind::String | FieldKind::Text | FieldKind::Enum => "'sample'",
        FieldKind::I32 | FieldKind::I64 | FieldKind::References => "1",
        FieldKind::Bool => "TRUE",
        FieldKind::F32 | FieldKind::F64 => "1.0",
        FieldKind::Uuid => "gen_random_uuid()",
        FieldKind::NaiveDateTime | FieldKind::DateTime => "NOW()",
        FieldKind::Bytea => "'\\x00'::bytea",
        // Always nullable (see `FieldKind::Attachment`'s doc comment), so it
        // never needs a sample literal to satisfy a `NOT NULL` constraint.
        FieldKind::Attachment => "NULL",
    }
}

/// Build a raw `INSERT INTO <plural> (...)` statement that sets `target`'s
/// enum column to a value guaranteed to be outside its declared variant set,
/// and every other required (`NOT NULL`, no `DEFAULT`) column to a valid
/// sample value — see [`sql_sample_literal`].
fn enum_rejection_insert_sql(
    plural: &str,
    fields: &[Field],
    target: &Field,
    defaults: &BTreeMap<String, String>,
) -> String {
    let mut columns = Vec::new();
    let mut values = Vec::new();
    for f in fields {
        if f.name == target.name {
            columns.push(f.name.clone());
            values.push("'__not_a_real_variant__'".to_owned());
        } else if !f.nullable && !defaults.contains_key(&f.name) {
            columns.push(f.name.clone());
            // A scaffold can have more than one required enum column; the
            // generic `sql_sample_literal` fallback ("'sample'") isn't a
            // real variant of any *other* enum field's own closed set, which
            // would trip that field's CHECK too and muddy which column's
            // constraint actually failed.
            let value = if f.is_enum() {
                format!("'{}'", f.variants.first().expect("enum field has variants"))
            } else {
                sql_sample_literal(f.kind).to_owned()
            };
            values.push(value);
        }
    }
    format!(
        "INSERT INTO {plural} ({}) VALUES ({})",
        columns.join(", "),
        values.join(", ")
    )
}

/// Render one `#[ignore]`d `#[tokio::test]` per enum field that asserts the
/// closed set is enforced at both layers (issue #1030's success metric):
///
/// 1. A raw `INSERT` (bypassing the app entirely) with an out-of-set value
///    for the field must fail — proving the database-level `CHECK`
///    constraint, independent of any application code.
/// 2. For an HTML (non-`--api`) scaffold, a stand-in `POST` handler — same
///    convention as [`render_index_smoke_test`]'s stand-in `GET` handler —
///    rejects an out-of-set form value with `400` naming the field, proving
///    the request-boundary validation (issue #1030's `--query`-free path
///    through `decode_form`'s `FromStr` parse, reproduced here since a
///    `tests/*.rs` binary cannot import the project's own handler code).
/// 3. Either way, the table ends up with zero rows.
fn render_enum_rejection_smoke_test(
    plural: &str,
    fields: &[Field],
    defaults: &BTreeMap<String, String>,
    setup_calls: &str,
    api: bool,
) -> String {
    let mut out = String::new();
    for target in fields.iter().filter(|f| f.is_enum()) {
        let field_name = &target.name;
        let insert_sql = enum_rejection_insert_sql(plural, fields, target, defaults);
        let escaped_insert = escape_sql_for_rust_literal(&insert_sql);
        let allowed_values = target
            .variants
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(" | ");

        let request_boundary_check = if api {
            String::new()
        } else {
            format!(
                "#[post(\"/{plural}\")]\n\
                 async fn create(body: autumn_web::reexports::axum::body::Bytes) -> AutumnResult<&'static str> {{\n\
                 \x20\x20\x20\x20let value = url::form_urlencoded::parse(body.as_ref())\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20.find(|(k, _)| k == \"{field_name}\")\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20.map(|(_, v)| v.into_owned())\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20.unwrap_or_default();\n\
                 \x20\x20\x20\x20if !matches!(value.as_str(), {allowed_values}) {{\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20return Err(AutumnError::bad_request_msg(format!(\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\"{field_name}: must be one of {variants_display}\"\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20)));\n\
                 \x20\x20\x20\x20}}\n\
                 \x20\x20\x20\x20Ok(\"ok\")\n\
                 }}\n\
                 \n\
                 \x20\x20\x20\x20let client: TestClient = TestApp::new().routes(routes![create]).with_db(db.pool()).build();\n\
                 \n\
                 \x20\x20\x20\x20client.post(\"/{plural}\").form(\"{field_name}=__not_a_real_variant__\").send().await\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20.assert_status(400)\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20.assert_body_contains(\"{field_name}\");\n",
                field_name = field_name,
                allowed_values = allowed_values,
                variants_display = target.variants.join(", "),
            )
        };

        let _ = write!(
            out,
            "\n#[tokio::test]\n\
             #[ignore = \"requires Docker (testcontainers) via TestDb; run `cargo test -- --ignored`\"]\n\
             async fn {plural}_rejects_out_of_set_{field_name}() {{\n\
             \x20\x20\x20\x20let db = TestDb::shared().await;\n\
             {setup_calls}\
             \x20\x20\x20\x20db.execute_sql(\"TRUNCATE {plural} RESTART IDENTITY\").await;\n\
             \n\
             {request_boundary_check}\
             \n\
             \x20\x20\x20\x20let mut conn = db.pool().get().await.expect(\"failed to get db connection\");\n\
             \x20\x20\x20\x20let result = diesel::sql_query(\"{escaped_insert}\").execute(&mut *conn).await;\n\
             \x20\x20\x20\x20assert!(result.is_err(), \"out-of-set {field_name} must violate the CHECK constraint\");\n\
             \n\
             \x20\x20\x20\x20let count: i64 = {plural}::table.count().get_result(&mut *conn).await.unwrap();\n\
             \x20\x20\x20\x20assert_eq!(count, 0, \"the rejected row must not have been written\");\n\
             }}\n",
        );
    }
    out
}

fn main_route_entries(
    plural: &str,
    snake_name: &str,
    api: bool,
    live: bool,
    validated_field_names: &[String],
) -> Vec<String> {
    if api {
        let mut entries = vec![
            format!("repositories::{snake_name}::{snake_name}_api_list"),
            format!("repositories::{snake_name}::{snake_name}_api_get"),
            format!("repositories::{snake_name}::{snake_name}_api_create"),
            format!("repositories::{snake_name}::{snake_name}_api_update"),
            format!("repositories::{snake_name}::{snake_name}_api_delete"),
        ];
        if live {
            entries.push(format!("repositories::{snake_name}::stream"));
        }
        entries
    } else {
        let mut entries = vec![
            format!("routes::{plural}::index"),
            format!("routes::{plural}::show"),
            format!("routes::{plural}::new_form"),
            format!("routes::{plural}::create"),
            format!("routes::{plural}::edit_form"),
            format!("routes::{plural}::update"),
            format!("routes::{plural}::destroy"),
        ];
        if live {
            entries.push(format!("routes::{plural}::events"));
        }
        for field_name in validated_field_names {
            entries.push(format!("routes::{plural}::validate_{field_name}"));
        }
        entries.push(format!("repositories::{snake_name}::{snake_name}_api_list"));
        entries.push(format!("repositories::{snake_name}::{snake_name}_api_get"));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn project_with_main(template: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), template).unwrap();
        tmp
    }

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

    #[test]
    fn plan_creates_full_scaffold() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "body:Text".into(),
                "published:bool".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        let paths: Vec<String> = plan
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(&plan.project_root)
                    .unwrap()
                    .display()
                    .to_string()
                    // Normalize for cross-platform comparisons (Windows uses `\`).
                    .replace('\\', "/")
            })
            .collect();
        for expected in [
            "src/models/post.rs",
            "src/models/mod.rs",
            "migrations/20260427000000_create_posts/up.sql",
            "migrations/20260427000000_create_posts/down.sql",
            "src/schema.rs",
            "src/repositories/post.rs",
            "src/repositories/mod.rs",
            "src/routes/posts.rs",
            "src/routes/mod.rs",
            "tests/post.rs",
            "src/main.rs",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "missing expected action for {expected}; got {paths:?}"
            );
        }
    }

    #[test]
    fn plan_errors_when_main_rs_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        let err = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap_err();
        assert!(matches!(err, GenerateError::Io(_)));
    }

    #[test]
    fn execute_writes_a_routes_file_referencing_model() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(routes.contains("use crate::models::post::{Post, NewPost, UpdatePost};"));
        assert!(routes.contains("#[get(\"/posts\")]"));
        assert!(routes.contains("#[get(\"/posts/{id}\")]"));
        assert!(
            !routes.contains("#[secured]\n#[get(\"/posts\")]"),
            "index should be reachable by the five-command scaffold smoke test"
        );
        assert!(
            !routes.contains("#[secured]\n#[get(\"/posts/{id}\")]"),
            "read-only show pages should stay public when generated"
        );
        assert!(routes.contains("#[get(\"/posts/new\")]"));
        assert!(routes.contains("#[post(\"/posts\")]"));
        assert!(routes.contains("#[get(\"/posts/{id}/edit\")]"));
        // The HTML edit form posts to a regular `POST /posts/{id}/update`
        // (browsers can't submit PUT natively); the JSON `PUT /api/posts/{id}`
        // remains available via the auto-generated repository handler.
        assert!(routes.contains("#[post(\"/posts/{id}/update\")]"));
        assert!(routes.contains("pub async fn new_form("));
        assert!(routes.contains("Ok(layout(\"New Post\""));
        assert!(routes.contains("posts::title.eq(form.title.clone())"));
        // `execute()` returns the affected row count — `Ok(0)` means the id
        // didn't exist, and we must return 404 instead of redirecting as if
        // the save succeeded. DB errors stay distinct from "not found".
        assert!(routes.contains("if updated == 0"));
        assert!(routes.contains("AutumnError::not_found_msg"));
        // The HTML edit form must point at the new HTML update handler, not
        // the JSON PUT endpoint — browsers cannot submit PUT without JS.
        assert!(routes.contains("/posts/{}/update"));
        assert!(!routes.contains("/api/posts/{}\""));
        // HTML handlers use POST for update and delete (browsers can't submit
        // PUT or DELETE natively); `#[put(` and `#[delete(` appear only in the
        // auto-generated JSON repository handlers, not in the HTML routes file.
        assert!(!routes.contains("#[put("));
        assert!(!routes.contains("#[delete("));
        // The HTML delete route must be present and use POST (not DELETE).
        assert!(routes.contains(r#"#[post("/posts/{id}/delete")]"#));
    }

    // ── enum field: form widgets, boundary validation, imports (issue #1030) ─

    fn plan_and_execute_post_scaffold_with_status_enum(tmp: &TempDir) {
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "status:enum{draft,published,archived}".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
    }

    #[test]
    fn scaffold_create_form_renders_enum_select() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("select name=\"status\"") && routes.contains("required"),
            "got:\n{routes}"
        );
        assert!(
            routes.contains("option value=\"\" { \"— Select —\" }"),
            "got:\n{routes}"
        );
        assert!(
            routes.contains("option value=\"draft\" { \"Draft\" }"),
            "got:\n{routes}"
        );
        assert!(
            routes.contains("option value=\"published\" { \"Published\" }"),
            "got:\n{routes}"
        );
        assert!(
            routes.contains("option value=\"archived\" { \"Archived\" }"),
            "got:\n{routes}"
        );
    }

    #[test]
    fn scaffold_edit_form_marks_current_enum_variant_selected() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains(
                "option value=\"draft\" selected[row.status == Status::Draft] { \"Draft\" }"
            ),
            "got:\n{routes}"
        );
        assert!(
            routes.contains(
                "option value=\"published\" selected[row.status == Status::Published] { \"Published\" }"
            ),
            "got:\n{routes}"
        );
    }

    #[test]
    fn scaffold_edit_form_required_enum_select_carries_required_attr() {
        // Regression test: a required (non-nullable) enum field's edit-form
        // `<select>` must carry `required`, matching the create form's own
        // enum select and every other non-nullable field kind's edit input.
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        let select_line = routes
            .lines()
            .find(|l| l.contains("select name=\"status\""))
            .unwrap_or_else(|| panic!("no status select found in:\n{routes}"));
        assert!(
            select_line.contains("select name=\"status\" required"),
            "required enum field's edit select must carry `required`: {select_line}"
        );
    }

    #[test]
    fn scaffold_decoded_form_parses_enum_with_field_error() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(routes.contains("pub status: String,"), "got:\n{routes}");
        assert!(
            routes.contains("decoded.status.parse::<Status>()"),
            "got:\n{routes}"
        );
        assert!(
            routes.contains("AutumnError::bad_request_msg(format!(\"status: {err}\"))"),
            "got:\n{routes}"
        );
    }

    #[test]
    fn scaffold_routes_import_enum_type() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("use crate::models::post::{Post, NewPost, UpdatePost, Status};"),
            "got:\n{routes}"
        );
    }

    #[test]
    fn scaffold_update_columns_set_enum_directly() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("posts::status.eq(form.status.clone())"),
            "got:\n{routes}"
        );
    }

    #[test]
    fn scaffold_default_enum_field_is_dropped_from_forms() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "status:enum{draft,published,archived}".into(),
            ],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    defaults: vec!["status=draft".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            !routes.contains("select name=\"status\""),
            "a --default field must be excluded from the create/edit forms: {routes}"
        );
    }

    #[test]
    fn scaffold_query_on_enum_field_is_rejected() {
        let tmp = project_with_main(default_main());
        let err = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "status:enum{draft,published}".into()],
            "20260427000000",
            &ScaffoldOptions {
                queries: vec!["find_by_status:status".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidField { .. }));
    }

    // ── enum field: nullable (issue #1030) ──────────────────────────────────

    fn plan_and_execute_post_scaffold_with_nullable_status_enum(tmp: &TempDir) {
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "status:Option<enum{draft,published}>".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
    }

    #[test]
    fn scaffold_nullable_enum_create_form_has_no_required_attr_and_unset_placeholder() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_nullable_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("option value=\"\" { \"— Unset —\" }"),
            "got:\n{routes}"
        );
        // The select for `status` itself must not carry ` required` — check
        // the specific select tag rather than the whole file (title's own
        // `required` text input is expected to remain required).
        let select_line = routes
            .lines()
            .find(|l| l.contains("select name=\"status\""))
            .unwrap_or_else(|| panic!("no status select found in:\n{routes}"));
        assert!(
            !select_line.contains("required"),
            "nullable enum select must not be required: {select_line}"
        );
    }

    #[test]
    fn scaffold_nullable_enum_edit_form_selected_exprs_wrap_in_some() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_nullable_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("option value=\"\" selected[row.status.is_none()] { \"— Unset —\" }"),
            "got:\n{routes}"
        );
        assert!(
            routes.contains(
                "option value=\"draft\" selected[row.status == Some(Status::Draft)] { \"Draft\" }"
            ),
            "got:\n{routes}"
        );
    }

    #[test]
    fn scaffold_nullable_enum_decoded_form_is_option_string_with_transpose() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_nullable_status_enum(&tmp);
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("pub status: Option<String>,"),
            "got:\n{routes}"
        );
        assert!(
            routes.contains("decoded.status") && routes.contains(".transpose()"),
            "got:\n{routes}"
        );
    }

    #[test]
    fn execute_writes_csrf_aware_form_handlers() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(routes.contains("use autumn_web::security::{CsrfFormField, CsrfToken};"));
        assert!(routes.contains("fn csrf_input("));
        assert!(routes.contains("input type=\"hidden\" name=(csrf_field_name"));
        assert!(routes.contains("value=(csrf.token());"));
        assert!(routes.contains("pub async fn new_form("));
        assert!(routes.contains("csrf: Option<CsrfToken>"));
        assert!(routes.contains("csrf_field: Option<CsrfFormField>"));
        assert!(routes.contains("(csrf_input(csrf.as_ref(), csrf_field.as_ref()))"));
        assert!(routes.contains("pub async fn edit_form("));
    }

    #[test]
    fn execute_writes_edit_form_with_prefilled_values_and_nullable_optional_inputs() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "subtitle:Option<String>".into(),
                "views:Option<i64>".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains(
                r#"label { "title" } input type="text" name="title" value=(row.title.to_string()) required;"#
            ),
            "edit form must prefill required fields from the loaded row: {routes}"
        );
        assert!(
            routes.contains(
                r#"label { "subtitle" } input type="text" name="subtitle" value=(row.subtitle.as_ref().map(ToString::to_string).unwrap_or_default());"#
            ),
            "edit form must prefill nullable text fields from the loaded row: {routes}"
        );
        assert!(
            routes.contains(
                r#"label { "views" } input type="number" name="views" step="1" value=(row.views.as_ref().map(ToString::to_string).unwrap_or_default());"#
            ),
            "edit form must prefill nullable numeric fields from the loaded row as a number input (issue #1131): {routes}"
        );
        assert!(
            routes.contains(r#"label { "subtitle" } input type="text" name="subtitle";"#),
            "new form must not mark nullable fields required: {routes}"
        );
        assert!(
            routes.contains(r#"label { "views" } input type="number" name="views" step="1";"#),
            "new form must not mark nullable numeric fields required: {routes}"
        );
    }

    #[test]
    fn execute_writes_form_decoder_that_drops_blank_nullable_fields() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "nickname:Option<String>".into(),
                "views:Option<i64>".into(),
                "published_at:Option<NaiveDateTime>".into(),
                "token:Option<Uuid>".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains("use autumn_web::reexports::axum::body::Bytes;"),
            "generated routes must be able to inspect raw form bytes: {routes}"
        );
        assert!(
            routes.contains("pub async fn create(flash: Flash, mut db: Db, body: Bytes)"),
            "create must decode after blank nullable normalization: {routes}"
        );
        assert!(
            routes.contains(
                "pub async fn update(\n    flash: Flash,\n    id: Path<i64>,\n    mut db: Db,\n    body: Bytes,\n)"
            ),
            "update must decode after blank nullable normalization: {routes}"
        );
        assert!(
            routes.contains("let new = decode_form(body)?;"),
            "create handler must use the generated decoder: {routes}"
        );
        assert!(
            routes.contains("let form = decode_form(body)?;"),
            "update handler must use the generated decoder: {routes}"
        );
        assert!(
            routes.contains(r#"matches!(name, "nickname" | "views" | "published_at" | "token")"#),
            "decoder must drop blank submissions for every nullable field: {routes}"
        );
    }

    #[test]
    fn execute_writes_a_repository_with_json_api_attribute() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let repo = fs::read_to_string(tmp.path().join("src/repositories/post.rs")).unwrap();
        assert!(repo.contains("#[autumn_web::repository(Post, api = \"/api/posts\")]"));
        assert!(repo.contains("pub trait PostRepository"));
    }

    #[test]
    fn execute_updates_main_rs_with_mods_and_routes() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("mod models;"));
        assert!(main.contains("mod routes;"));
        assert!(main.contains("mod schema;"));
        assert!(main.contains("mod repositories;"));
        assert!(main.contains("routes::posts::index"));
        assert!(main.contains("routes::posts::show"));
        assert!(main.contains("routes::posts::new_form"));
        assert!(main.contains("routes::posts::create"));
        assert!(main.contains("routes::posts::edit_form"));
        assert!(main.contains("routes::posts::update"));
        assert!(main.contains("routes::posts::destroy"));
        assert!(main.contains("repositories::post::post_api_list"));
        assert!(main.contains("repositories::post::post_api_get"));
        assert!(!main.contains("repositories::post::post_api_create"));
        assert!(!main.contains("repositories::post::post_api_update"));
        assert!(!main.contains("repositories::post::post_api_delete"));
    }

    #[test]
    fn scaffold_emits_flash_messages_and_destroy_handler() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        // Flash is imported and set on every mutating action before the redirect.
        assert!(
            routes.contains("use autumn_web::flash::Flash;"),
            "routes file must import Flash: {routes}"
        );
        assert!(routes.contains(r#"flash.success("Post created")"#));
        assert!(routes.contains(r#"flash.success("Post updated")"#));
        assert!(routes.contains(r#"flash.success("Post deleted")"#));
        // A destroy handler now exists, wired as a browser-friendly POST.
        assert!(routes.contains("pub async fn destroy("));
        assert!(routes.contains(r#"#[post("/posts/{id}/delete")]"#));
        // The show page exposes a delete control that targets it.
        assert!(routes.contains("/posts/{}/delete"));
        // The layout threads flash markup and renders it in one line.
        assert!(routes.contains("fn layout(title: &str, flash: Markup, content: Markup)"));
        assert!(routes.contains("flash.render().await"));

        // main.rs registers the new destroy route.
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains("routes::posts::destroy"),
            "main.rs must register the destroy route: {main}"
        );
    }

    #[test]
    fn execute_writes_smoke_test() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(test.contains("posts_index_renders_scaffolded_rows"));
        assert!(!test.contains("AUTUMN_TEST_SESSION_COOKIE"));
        assert!(!test.contains("Cookie: {session_cookie}"));
        assert!(test.contains("/posts"));
    }

    // ── in-process TestApp/TestClient smoke test (issue #1023) ────────────

    #[test]
    fn smoke_test_uses_in_process_test_app_not_tcp_stream() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();

        // Uses the real in-process harness -- no raw sockets, no env-var gate.
        assert!(
            test.contains("autumn_web::test::{TestApp, TestClient, TestDb}"),
            "smoke test must use the TestApp/TestClient/TestDb harness: {test}"
        );
        assert!(
            !test.contains("TcpStream"),
            "smoke test must not hand-roll a raw TCP request: {test}"
        );
        assert!(
            !test.contains("AUTUMN_TEST_BASE_URL"),
            "smoke test must not gate on a running server's base URL: {test}"
        );
        assert!(
            !test.contains("eprintln!(\"skipping"),
            "smoke test must not silently skip via an env-gated return: {test}"
        );

        // DB-backed handler: a Docker-backed test is a *visible* `ignored`,
        // never a silent green pass, and carries an explicit reason.
        assert!(
            test.contains("#[ignore = \"requires Docker"),
            "DB-backed smoke test must be explicitly #[ignore]d with a reason: {test}"
        );

        // Real assertions: 200 plus the rendered heading.
        assert!(test.contains(".assert_ok()"));
        assert!(test.contains(".assert_body_contains(\"Posts\")"));
    }

    #[test]
    fn smoke_test_index_handler_returns_wrong_status_fails_the_assertion() {
        // Red-phase spike: if the (re-declared) handler under test returned the
        // wrong status or dropped the heading, `assert_ok`/`assert_body_contains`
        // must be the thing that fails -- i.e. the test has real failure power,
        // not just a status check that always trivially passes.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();

        assert!(
            test.contains("Ok(html! {"),
            "handler must actually render Markup that assert_body_contains can inspect: {test}"
        );
        assert!(
            test.contains("h1 { \"Posts\" }"),
            "handler must render the heading the test asserts on: {test}"
        );
        // The count query touches the real (throwaway) database -- a broken
        // query or handler bubbles up as an Err via `?`, which axum turns into
        // a non-200 response, which `assert_ok()` then catches.
        assert!(test.contains(".get_result(&mut db).await?;"));
    }

    #[test]
    fn smoke_test_wires_dev_dependency_test_support() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("[dev-dependencies]"),
            "Cargo.toml must have a [dev-dependencies] section: {cargo}"
        );
        assert!(
            cargo.contains("test-support"),
            "Cargo.toml must enable autumn-web's test-support feature for TestDb: {cargo}"
        );
        // Must not leak into the production dependency set.
        let deps_section = cargo.split("[dev-dependencies]").next().unwrap();
        assert!(
            !deps_section.contains("test-support"),
            "test-support must stay out of [dependencies]: {cargo}"
        );
    }

    #[test]
    fn smoke_test_wires_dev_dependency_tokio_test_features() {
        // Regression test (Codex review, issue #1023): the generated smoke
        // test uses `#[tokio::test]`, which needs the `rt` and `macros`
        // tokio features to compile. A project not created from `autumn
        // new` -- like this test's bare Cargo.toml -- has no tokio
        // dev-dependency at all, so without this wiring the generated test
        // target would fail to compile (cargo test --tests still compiles
        // #[ignore]d tests).
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        let tokio_line = cargo
            .lines()
            .find(|l| l.trim_start().starts_with("tokio"))
            .unwrap_or_else(|| panic!("Cargo.toml must have a tokio dev-dependency: {cargo}"));
        assert!(
            tokio_line.contains("\"rt\"") && tokio_line.contains("\"macros\""),
            "tokio dev-dependency must enable rt and macros for #[tokio::test]: {tokio_line}"
        );
    }

    #[test]
    fn delete_button_has_destructive_confirmation() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        // The delete button must require an explicit confirmation so a single
        // misclick cannot silently destroy a row (AC: destructive confirmation).
        assert!(
            routes.contains(r#"onclick="return confirm("#) || routes.contains("hx-confirm="),
            "delete button must have an onclick confirm or hx-confirm: {routes}"
        );
    }

    #[test]
    fn smoke_test_no_longer_includes_write_path_round_trip() {
        // The old create/delete round-trip test hit a *running server* over a
        // raw TcpStream, gated on `AUTUMN_TEST_BASE_URL` -- the very
        // false-positive-green pattern issue #1023 fixes. Write-path coverage
        // (create -> redirect, update, delete) is deferred as a follow-up (see
        // that issue's "Out of Scope"); the generator now emits exactly one
        // real, in-process, DB-backed index/read smoke test.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            !test.contains("delete_round_trip"),
            "write-path round-trip coverage is deferred, not converted: {test}"
        );
        // Positive check alongside the negative one above: exactly one test
        // function is generated (the index/read smoke test), so a future
        // change can't quietly reintroduce write-path coverage under a
        // different name without this test catching the count changing.
        assert_eq!(
            test.matches("#[tokio::test]").count(),
            1,
            "expected exactly one generated test function: {test}"
        );
        assert!(test.contains("posts_index_renders_scaffolded_rows"));
    }

    #[test]
    fn dry_run_does_not_modify_main() {
        let tmp = project_with_main(default_main());
        let original = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        let after = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(original, after);
    }

    #[test]
    fn collision_lists_existing_files_without_force() {
        let tmp = project_with_main(default_main());
        // Pre-create one of the files so the next run collides.
        let dir = tmp.path().join("src/models");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("post.rs"), "// existing").unwrap();
        let plan = plan_scaffold(tmp.path(), "Post", &[], "20260427000000").unwrap();
        let err = plan.execute(Flags::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("post.rs"));
    }

    // ── Soft-delete scaffold generation (issue #689) ──────────────

    #[test]
    fn scaffold_soft_delete_destroy_handler_marks_deleted_at_not_physical_delete() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    soft_delete: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        // The browser delete button must respect soft-delete: mark deleted_at,
        // matching the soft-delete repository, instead of physically deleting.
        assert!(
            routes.contains("posts::deleted_at.eq(Some(chrono::Utc::now().naive_utc()))"),
            "soft-delete destroy must mark deleted_at: {routes}"
        );
        assert!(
            routes.contains("posts::deleted_at.is_null()"),
            "soft-delete destroy must skip already-deleted rows so a repeat delete 404s: {routes}"
        );
        assert!(
            !routes.contains("diesel::delete(posts::table.find(*id))"),
            "soft-delete destroy must not physically delete the row: {routes}"
        );
    }

    #[test]
    fn scaffold_without_soft_delete_destroy_handler_physically_deletes() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains("diesel::delete(posts::table.find(*id))"),
            "non-soft-delete destroy must issue a physical delete: {routes}"
        );
        assert!(
            !routes.contains("deleted_at.eq("),
            "non-soft-delete destroy must not mark deleted_at: {routes}"
        );
    }

    #[test]
    fn scaffold_soft_delete_repository_annotation_includes_soft_delete() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    soft_delete: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let repo = fs::read_to_string(tmp.path().join("src/repositories/post.rs")).unwrap();
        assert!(
            repo.contains("soft_delete"),
            "repository file must include soft_delete in the #[repository] annotation: {repo}"
        );
    }

    #[test]
    fn scaffold_soft_delete_model_includes_deleted_at_field() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    soft_delete: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("deleted_at"),
            "model struct must include deleted_at field when soft_delete is enabled: {model}"
        );
        // Regression test: `deleted_at` must be excluded from `NewX`/`UpdateX`
        // via `#[default]` -- it's DB-managed (NULL on insert, set only by the
        // destroy handler), so neither the create nor update handler ever
        // populates it. Without `#[default]` here, `#[model]` treats it as a
        // required field and the generated `create` handler fails to compile
        // (`NewPost { .. }` missing `deleted_at`).
        assert!(
            model.contains("#[default]\n    pub deleted_at:"),
            "deleted_at must be #[default] (DB-managed, excluded from NewX/UpdateX): {model}"
        );
        // Regression test: the model struct's field order must match the
        // column order `create_table_sql_with_metadata_and_id`/
        // `schema_table_block_with_id` emit for the migration and
        // `schema.rs` (soft-delete field before `created_at`, which is
        // always appended last). The `#[repository]` macro's generated
        // insert-then-`RETURNING` query loads into this struct positionally,
        // so a mismatched order produces a Diesel `CompatibleType` error at
        // compile time rather than at the point of the actual typo.
        let deleted_at_pos = model.find("pub deleted_at:").expect("deleted_at field");
        let created_at_pos = model.find("pub created_at:").expect("created_at field");
        assert!(
            deleted_at_pos < created_at_pos,
            "deleted_at must be declared before created_at, matching schema.rs's column order: {model}"
        );

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        let schema_deleted_at_pos = schema.find("deleted_at ->").expect("deleted_at column");
        let schema_created_at_pos = schema.find("created_at ->").expect("created_at column");
        assert!(
            schema_deleted_at_pos < schema_created_at_pos,
            "schema.rs must declare deleted_at before created_at: {schema}"
        );
    }

    #[test]
    fn scaffold_soft_delete_smoke_test_table_includes_deleted_at() {
        // Regression test (code review, issue #1023): the smoke test's
        // throwaway CREATE TABLE must use the same soft-delete-augmented field
        // list as the real migration, so it doesn't drift from the actual
        // schema for --soft-delete models.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    soft_delete: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let migration = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            migration.contains("deleted_at"),
            "real migration must include deleted_at: {migration}"
        );
        assert!(
            test.contains("deleted_at"),
            "smoke test's throwaway table must match the real migration schema \
             and include deleted_at for a --soft-delete model: {test}"
        );
    }

    #[test]
    fn scaffold_without_soft_delete_does_not_include_soft_delete_annotation() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let repo = fs::read_to_string(tmp.path().join("src/repositories/post.rs")).unwrap();
        assert!(
            !repo.contains("soft_delete"),
            "repository without soft_delete must not include soft_delete annotation: {repo}"
        );
    }

    #[test]
    fn execute_writes_edit_form_with_attachment_hidden_input() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into(), "avatar:Attachment".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        // Assert edit form contains input type="file" AND the hidden input for existing avatar
        assert!(routes.contains("input type=\"file\" name=\"avatar\""));
        assert!(routes.contains("input type=\"hidden\" name=\"avatar\" value=(blob.key)"));

        // Assert decode_form contains DecodedForm struct
        assert!(routes.contains("struct DecodedForm"));
        assert!(routes.contains("pub avatar: Option<String>"));
    }

    // ── Typed form-input widgets (issue #1131) ──────────────────────

    #[test]
    fn execute_writes_checkbox_for_bool_field() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into(), "active:bool".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        // Both create and edit forms must render a real checkbox for `active`,
        // never a text box.
        assert!(
            routes.contains("input type=\"checkbox\" name=\"active\""),
            "{routes}"
        );
        assert!(
            !routes.contains("input type=\"text\" name=\"active\""),
            "bool field must not render input type=\"text\": {routes}"
        );
        // No hidden "false" fallback sharing the checkbox's name: a checked
        // box would then submit the key twice (active=false&active=true),
        // and serde_urlencoded rejects duplicate keys — every checked
        // submission would 400 (issue #1131 follow-up fix). Exactly one
        // `name="active"` input must exist per form.
        assert!(
            !routes.contains("input type=\"hidden\" name=\"active\" value=\"false\""),
            "{routes}"
        );
        assert_eq!(
            routes.matches("name=\"active\"").count(),
            2,
            "expected exactly one `name=\"active\"` input in each of the \
             create and edit forms (no duplicate-key hidden fallback): {routes}"
        );
        // Edit form must reflect the current value via `checked[...]`.
        assert!(routes.contains("checked[row.active]"), "{routes}");
        // DecodedForm must default the field so an unchecked submission
        // (missing key) doesn't 400.
        assert!(
            routes.contains("#[serde(default)]\n    pub active: bool,"),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_select_for_nullable_bool_field() {
        // A checkbox can't losslessly represent Option<bool> (no way to
        // distinguish "leave false" from "set to null" when unchecked), so
        // nullable bool fields must render a 3-option select instead.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into(), "archived:Option<bool>".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            !routes.contains("input type=\"checkbox\" name=\"archived\""),
            "nullable bool must not render a checkbox: {routes}"
        );
        assert!(routes.contains("select name=\"archived\""), "{routes}");
        assert!(routes.contains("option value=\"\""), "{routes}");
        assert!(routes.contains("option value=\"true\""), "{routes}");
        assert!(routes.contains("option value=\"false\""), "{routes}");
        // Edit form must reflect the current tri-state value.
        assert!(
            routes.contains("selected[row.archived.is_none()]"),
            "{routes}"
        );
        assert!(
            routes.contains("selected[row.archived == Some(true)]"),
            "{routes}"
        );
        assert!(
            routes.contains("selected[row.archived == Some(false)]"),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_number_input_for_integer_fields() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into(), "views:i64".into(), "rank:i32".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("input type=\"number\" name=\"views\" step=\"1\""),
            "{routes}"
        );
        assert!(
            routes.contains("input type=\"number\" name=\"rank\" step=\"1\""),
            "{routes}"
        );
        assert!(
            !routes.contains("input type=\"text\" name=\"views\""),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_number_input_with_any_step_for_float_fields() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "price:f64".into(),
                "weight:f32".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("input type=\"number\" name=\"price\" step=\"any\""),
            "{routes}"
        );
        assert!(
            routes.contains("input type=\"number\" name=\"weight\" step=\"any\""),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_finite_guard_for_float_edit_form_value() {
        // f32/f64 Display renders NaN/Infinity as "NaN"/"inf"/"-inf", none
        // of which satisfy HTML5's <input type="number"> value grammar —
        // the browser would silently blank the field. The generated value
        // expression must guard with is_finite() instead of a bare
        // .to_string() for float fields.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "price:f64".into(),
                "weight:Option<f32>".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains(
                "value=(if row.price.is_finite() { row.price.to_string() } else { String::new() })"
            ),
            "{routes}"
        );
        assert!(
            routes.contains(
                "value=(row.weight.as_ref().filter(|value| value.is_finite()).map(ToString::to_string).unwrap_or_default())"
            ),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_number_input_value_on_edit_form() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into(), "views:i64".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains(
                "input type=\"number\" name=\"views\" step=\"1\" value=(row.views.to_string())"
            ),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_datetime_local_input_for_naive_datetime_field() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into(), "published_at:NaiveDateTime".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("input type=\"datetime-local\" name=\"published_at\""),
            "{routes}"
        );
        assert!(
            !routes.contains("input type=\"text\" name=\"published_at\""),
            "{routes}"
        );
        // DecodedForm must use the local-shape-aware deserializer so a
        // browser-submitted `YYYY-MM-DDTHH:MM` value parses without a
        // hand-edit.
        assert!(
            routes.contains(
                "#[serde(deserialize_with = \"deserialize_naive_datetime_local\")]\n    pub published_at: chrono::NaiveDateTime,"
            ),
            "{routes}"
        );
        assert!(
            routes.contains("fn deserialize_naive_datetime_local"),
            "{routes}"
        );
        assert!(routes.contains("fn normalize_datetime_local"), "{routes}");
        // No DateTime<Utc> field present — the tz-aware helper must not be
        // emitted as dead code.
        assert!(
            !routes.contains("fn deserialize_utc_datetime_local"),
            "{routes}"
        );
        // The parse format must accept optional fractional seconds
        // (`%.f`) — without it, a datetime-local value submitted with
        // milliseconds (e.g. from a finer-grained `step`) fails to parse.
        assert!(routes.contains("\"%Y-%m-%dT%H:%M:%S%.f\")"), "{routes}");
        // Regression test: the edit form's value must preserve seconds/
        // fractional precision, not truncate to minutes. A minute-only
        // value round-trips as `:00` seconds via normalize_datetime_local,
        // and the generated update handler writes every column
        // unconditionally — a no-op re-submit of a row with non-zero
        // seconds would otherwise silently corrupt the stored timestamp
        // (e.g. `12:34:56` -> `12:34:00`).
        assert!(
            routes
                .contains("value=(row.published_at.format(\"%Y-%m-%dT%H:%M:%S%.f\").to_string())"),
            "{routes}"
        );
        // `step="any"` so a value carrying seconds/fractional seconds
        // doesn't fail the browser's step-mismatch constraint validation
        // (default step is minute-granularity) and block submission.
        assert!(
            routes.contains("input type=\"datetime-local\" name=\"published_at\" step=\"any\""),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_datetime_local_input_for_tz_datetime_field() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into(), "scheduled_at:DateTime".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();

        assert!(
            routes.contains("input type=\"datetime-local\" name=\"scheduled_at\""),
            "{routes}"
        );
        assert!(
            routes.contains(
                "#[serde(deserialize_with = \"deserialize_utc_datetime_local\")]\n    pub scheduled_at: chrono::DateTime<chrono::Utc>,"
            ),
            "{routes}"
        );
        assert!(
            routes.contains("fn deserialize_utc_datetime_local"),
            "{routes}"
        );
        assert!(
            !routes.contains("fn deserialize_naive_datetime_local"),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_datetime_local_input_for_nullable_datetime_field() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "published_at:Option<NaiveDateTime>".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains(
                "#[serde(default, deserialize_with = \"deserialize_option_naive_datetime_local\")]\n    pub published_at: Option<chrono::NaiveDateTime>,"
            ),
            "{routes}"
        );
        assert!(
            routes.contains("fn deserialize_option_naive_datetime_local"),
            "{routes}"
        );
        // Scalar variant must not be emitted when only the nullable form is used.
        assert!(
            !routes.contains("fn deserialize_naive_datetime_local<'de, D>(deserializer: D) -> Result<chrono::NaiveDateTime, D::Error>"),
            "{routes}"
        );
    }

    #[test]
    fn execute_writes_text_input_only_for_genuine_string_fields() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "body:Text".into(),
                "active:bool".into(),
                "views:i64".into(),
                "published_at:NaiveDateTime".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains("input type=\"text\" name=\"title\""),
            "{routes}"
        );
        assert!(
            routes.contains("input type=\"text\" name=\"body\""),
            "{routes}"
        );
        assert!(
            !routes.contains("input type=\"text\" name=\"active\""),
            "{routes}"
        );
        assert!(
            !routes.contains("input type=\"text\" name=\"views\""),
            "{routes}"
        );
        assert!(
            !routes.contains("input type=\"text\" name=\"published_at\""),
            "{routes}"
        );
    }

    #[test]
    fn plan_scaffold_api_only_skips_html() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                api: true,
                ..Default::default()
            },
        )
        .unwrap();
        let paths: Vec<String> = plan
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(&plan.project_root)
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();
        assert!(!paths.iter().any(|p| p.contains("src/routes/posts.rs")));
        assert!(!paths.iter().any(|p| p.contains("src/routes/mod.rs")));
    }

    #[test]
    fn plan_scaffold_api_only_mounts_all_five_json_endpoints() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                api: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("repositories::post::post_api_create"));
        assert!(main.contains("repositories::post::post_api_update"));
        assert!(main.contains("repositories::post::post_api_delete"));
        assert!(main.contains("repositories::post::post_api_list"));
        assert!(main.contains("repositories::post::post_api_get"));
        assert!(!main.contains("routes::posts::index"));
    }

    // ── sharding tests ─────────────────────────────────────────────────────

    fn sharded_options_with_key(key: &str) -> ScaffoldOptions {
        ScaffoldOptions {
            model: ModelOptions {
                sharded: true,
                shard_key: Some(key.into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn resolves_shard_key_explicit_field() {
        let fields = parse_fields(&["tenant_id:i64".into(), "name:String".into()]).unwrap();
        let opts = ModelOptions {
            sharded: true,
            shard_key: Some("tenant_id".into()),
            ..Default::default()
        };
        let key = resolve_shard_key(&fields, &opts).unwrap();
        assert_eq!(key, Some("tenant_id".to_owned()));
    }

    #[test]
    fn resolves_shard_key_explicit_id() {
        let fields = parse_fields(&["name:String".into()]).unwrap();
        let opts = ModelOptions {
            sharded: true,
            shard_key: Some("id".into()),
            ..Default::default()
        };
        let key = resolve_shard_key(&fields, &opts).unwrap();
        assert_eq!(key, Some("id".to_owned()));
    }

    #[test]
    fn resolves_shard_key_invalid_field_errors() {
        let fields = parse_fields(&["name:String".into()]).unwrap();
        let opts = ModelOptions {
            sharded: true,
            shard_key: Some("bogus".into()),
            ..Default::default()
        };
        assert!(
            resolve_shard_key(&fields, &opts).is_err(),
            "unknown shard_key field must return an error"
        );
    }

    #[test]
    fn resolves_shard_key_defaults_to_tenant_id_when_present() {
        let fields = parse_fields(&["tenant_id:i64".into(), "name:String".into()]).unwrap();
        let opts = ModelOptions {
            sharded: true,
            shard_key: None,
            ..Default::default()
        };
        let key = resolve_shard_key(&fields, &opts).unwrap();
        assert_eq!(key, Some("tenant_id".to_owned()));
    }

    #[test]
    fn resolves_shard_key_defaults_to_id_when_no_tenant_id() {
        let fields = parse_fields(&["name:String".into()]).unwrap();
        let opts = ModelOptions {
            sharded: true,
            shard_key: None,
            ..Default::default()
        };
        let key = resolve_shard_key(&fields, &opts).unwrap();
        assert_eq!(key, Some("id".to_owned()));
    }

    #[test]
    fn resolves_shard_key_none_when_not_sharded() {
        let fields = parse_fields(&["tenant_id:i64".into()]).unwrap();
        let opts = ModelOptions {
            sharded: false,
            shard_key: None,
            ..Default::default()
        };
        let key = resolve_shard_key(&fields, &opts).unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn routes_use_sharded_db_when_sharded() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Account",
            &["tenant_id:i64".into(), "name:String".into()],
            "20260427000000",
            &sharded_options_with_key("tenant_id"),
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/accounts.rs")).unwrap();
        // ShardedDb must be imported from the correct path (not crate root).
        assert!(
            routes.contains("use autumn_web::sharding::ShardedDb;"),
            "sharded routes must import ShardedDb from autumn_web::sharding: {routes}"
        );
        // Db must NOT appear in the brace-import or as a handler param type.
        assert!(
            !routes.contains("mut db: Db"),
            "sharded routes must not use Db extractor: {routes}"
        );
        // ShardedDb must be used in handler signatures.
        assert!(
            routes.contains("mut db: ShardedDb"),
            "sharded routes must use ShardedDb in handler signatures: {routes}"
        );
        // index must call from_shard explicitly for a literal proof.
        assert!(
            routes.contains("from_shard(&db)"),
            "sharded index handler must call from_shard(&db): {routes}"
        );
    }

    #[test]
    fn routes_use_db_when_not_sharded() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains("mut db: Db"),
            "non-sharded routes must still use Db"
        );
        assert!(
            !routes.contains("ShardedDb"),
            "non-sharded routes must not reference ShardedDb"
        );
    }

    #[test]
    fn repository_notes_sharded() {
        let rendered = render_repository_file("Account", "account", &[], false, false, true, false);
        assert!(
            rendered.contains("shard-aware"),
            "sharded repository doc must mention shard-aware: {rendered}"
        );
        assert!(
            rendered.contains("from_shard"),
            "sharded repository doc must mention from_shard: {rendered}"
        );
    }

    #[test]
    fn repository_notes_api_sharded_caveat() {
        let rendered = render_repository_file("Account", "account", &[], false, true, true, false);
        assert!(
            rendered.contains("control pool"),
            "sharded api repository doc must note control pool: {rendered}"
        );
    }

    #[test]
    fn repository_no_sharded_note_when_not_sharded() {
        let rendered = render_repository_file("Post", "post", &[], false, false, false, false);
        assert!(
            !rendered.contains("shard-aware"),
            "non-sharded repository must not mention shard-aware: {rendered}"
        );
    }

    #[test]
    fn plan_scaffold_api_only_emits_json_smoke_test() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "published:bool".into()],
            "20260427000000",
            &ScaffoldOptions {
                api: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let test_file = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(test_file.contains("/api/posts"));
        assert!(
            test_file.contains("autumn_web::test::{TestApp, TestClient, TestDb}"),
            "api smoke test must use the in-process harness: {test_file}"
        );
        assert!(!test_file.contains("TcpStream"));
        assert!(!test_file.contains("AUTUMN_TEST_BASE_URL"));
        assert!(
            test_file.contains("#[ignore = \"requires Docker"),
            "DB-backed api smoke test must be explicitly ignored with a reason: {test_file}"
        );
        assert!(!test_file.contains("contains(\"Posts\")"));
    }

    // ── data_table scaffold integration ────────────────────────────────

    #[test]
    fn index_uses_data_table_not_ul() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "body:Text".into(),
                "published:bool".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(routes.contains("data_table("), "{routes}");
        assert!(routes.contains("DataTableConfig::new("), "{routes}");
        assert!(routes.contains("Column::new(\"Title\""), "{routes}");
        assert!(
            !routes.contains("ul id=\"posts-list\""),
            "still uses ul: {routes}"
        );
    }

    #[test]
    fn index_data_table_cell_handles_nullable_field() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:Option<String>".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(routes.contains("unwrap_or_default"), "{routes}");
    }

    #[test]
    fn index_data_table_has_show_link_column() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(routes.contains("/posts/{}"), "{routes}");
        assert!(routes.contains("row.id"), "{routes}");
    }

    #[test]
    fn sharded_index_uses_data_table() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["tenant_id:i64".into(), "title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    sharded: true,
                    shard_key: Some("tenant_id".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(routes.contains("data_table("), "{routes}");
        assert!(routes.contains("from_shard"), "{routes}");
    }

    #[test]
    fn live_index_keeps_sse_list_container() {
        let tmp = project_with_main(default_main());
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                live: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        // Live variant must keep the ul/li SSE contract intact
        assert!(routes.contains("ul id=\"posts-list\""), "{routes}");
        assert!(routes.contains("sse-connect=\"/posts/events\""), "{routes}");
    }

    #[test]
    fn plan_scaffold_live_views() {
        let tmp = project_with_main(default_main());
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                live: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(routes.contains("/posts/events"));
        assert!(routes.contains("autumn_web::sse::stream"));
        assert!(routes.contains("hx-ext=\"sse\""));
        assert!(routes.contains("sse-connect=\"/posts/events\""));
        assert!(routes.contains("hx-swap=\"none\""));
        assert!(routes.contains("autumn_web::htmx::HTMX_JS_PATH"));
        assert!(routes.contains("autumn_web::htmx::HTMX_SSE_JS_PATH"));
        assert!(routes.contains("title: autumn_web::hooks::Patch::Set(form.title.clone())"));

        let main_rs = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main_rs.contains("routes::posts::events"));

        let repo = fs::read_to_string(tmp.path().join("src/repositories/post.rs")).unwrap();
        assert!(repo.contains("broadcasts = true"));

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("\"ws\""));
        assert!(cargo.contains("\"maud\""));
        assert!(cargo.contains("\"htmx\""));
    }

    // ── property_list scaffold conformance (issue #1120) ──────────────────

    #[test]
    fn show_uses_property_list_widget_with_declared_fields() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "body:Text".into(),
                "published:bool".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        // show handler references property_list widget
        assert!(
            routes.contains("autumn_web::widgets::property_list"),
            "show must use property_list widget: {routes}"
        );
        // Each declared field appears with humanized label
        assert!(
            routes.contains("\"Title\""),
            "show must list 'title' field: {routes}"
        );
        assert!(
            routes.contains("\"Body\""),
            "show must list 'body' field: {routes}"
        );
        assert!(
            routes.contains("\"Published\""),
            "show must list 'published' field: {routes}"
        );
        // id and created_at always present
        assert!(routes.contains("\"Id\""), "show must include id: {routes}");
        assert!(
            routes.contains("\"Created at\""),
            "show must include created_at: {routes}"
        );
    }

    #[test]
    fn show_property_list_label_humanization() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "published_at:NaiveDateTime".into(),
                "user_name:String".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        // humanized: first word capitalized, rest lowercase (snake_case → "Word rest")
        assert!(
            routes.contains("\"Published at\""),
            "humanize must produce 'Published at': {routes}"
        );
        assert!(
            routes.contains("\"User name\""),
            "humanize must produce 'User name': {routes}"
        );
    }

    #[test]
    fn show_includes_defaulted_fields_in_property_list() {
        // Regression test: fields with `#[default]` are excluded from the
        // form (form_fields), but must still appear in the show property list.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "views:i64".into()],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    defaults: vec!["views=0".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/posts.rs")).unwrap();
        assert!(
            routes.contains("\"Views\""),
            "show must include defaulted field 'views': {routes}"
        );
    }

    #[test]
    fn smoke_test_default_value_containing_semicolon_does_not_corrupt_sql() {
        // Regression test (PR review, issue #1023): `create_table_sql_with_metadata_and_id`
        // emits `DEFAULT '...'` verbatim for a String/Text `--default`, so a
        // value containing a semicolon (e.g. a bio with "; " in it) must not
        // be split into two broken `db.execute_sql(...)` calls.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ScaffoldOptions {
                model: ModelOptions {
                    defaults: vec!["title=hello;world".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            test.contains("DEFAULT 'hello;world'"),
            "generated default literal must survive intact: {test}"
        );
        // Every db.execute_sql(...) call's string literal argument must itself
        // be balanced/complete SQL -- i.e. the CREATE TABLE statement was not
        // split mid-literal. Count occurrences of "CREATE TABLE" (must be
        // exactly one, not spread across two calls).
        assert_eq!(
            test.matches("CREATE TABLE").count(),
            1,
            "CREATE TABLE must appear in a single execute_sql call, not split across two: {test}"
        );
    }

    // ── enum field: generated out-of-set rejection smoke test (issue #1030) ─

    #[test]
    fn scaffold_smoke_test_asserts_out_of_set_post_rejected() {
        let tmp = project_with_main(default_main());
        plan_and_execute_post_scaffold_with_status_enum(&tmp);

        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            test.contains("CHECK (status IN ('draft', 'published', 'archived'))"),
            "the raw-INSERT test needs the real CHECK constraint in its setup SQL: {test}"
        );
        assert!(
            test.contains("fn posts_rejects_out_of_set_status"),
            "got:\n{test}"
        );
        assert!(test.contains(".assert_status(400)"), "got:\n{test}");
        assert!(
            test.contains(".assert_body_contains(\"status\")"),
            "got:\n{test}"
        );
        assert!(
            test.contains("is_err()"),
            "must assert the raw out-of-set INSERT fails at the DB layer: {test}"
        );
        assert!(
            test.contains("posts::table.count()"),
            "must assert zero rows were written: {test}"
        );
    }

    #[test]
    fn scaffold_api_smoke_test_asserts_check_constraint_but_no_http_assertion() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold_with_options(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "status:enum{draft,published,archived}".into(),
            ],
            "20260427000000",
            &ScaffoldOptions {
                api: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            test.contains("fn posts_rejects_out_of_set_status"),
            "got:\n{test}"
        );
        assert!(
            test.contains("is_err()"),
            "must assert the raw out-of-set INSERT fails at the DB layer: {test}"
        );
        assert!(
            !test.contains(".assert_status(400)"),
            "the --api smoke test has no HTML form route to POST to: {test}"
        );
    }

    #[test]
    fn scaffold_smoke_test_absent_when_no_enum_fields() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            !test.contains("rejects_out_of_set"),
            "no enum field means no rejection test: {test}"
        );
    }

    #[test]
    fn scaffold_multiple_enum_fields_each_use_own_variant_as_other_columns_sample() {
        // A second required enum column's raw-INSERT sample value must be one
        // of *its own* variants, not the generic `'sample'` placeholder —
        // otherwise it would trip its own CHECK too and the test would no
        // longer isolate the failure to the column actually under test.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "status:enum{draft,published,archived}".into(),
                "priority:enum{low,high}".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            test.contains(
                "INSERT INTO posts (status, priority) VALUES ('__not_a_real_variant__', 'low')"
            ),
            "got:\n{test}"
        );
        assert!(
            test.contains(
                "INSERT INTO posts (status, priority) VALUES ('draft', '__not_a_real_variant__')"
            ),
            "got:\n{test}"
        );
    }

    #[test]
    fn scaffold_enum_rejection_test_isolates_check_failure_from_required_references_field() {
        // Regression test: a required `references` field co-occurring with
        // the enum field under test must not make the raw out-of-set INSERT
        // fail for the wrong reason (an FK violation on a dangling stub-table
        // reference) instead of the CHECK constraint actually being tested.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Post",
            &[
                "author:references".into(),
                "status:enum{draft,published,archived}".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/post.rs")).unwrap();
        assert!(
            test.contains("INSERT INTO authors DEFAULT VALUES;"),
            "the stub `authors` table must be seeded so author_id=1 is a valid FK: {test}"
        );
        assert!(
            test.contains(
                "INSERT INTO posts (author_id, status) VALUES (1, '__not_a_real_variant__')"
            ),
            "got:\n{test}"
        );
    }

    // ── references field: scaffold + smoke-test wiring (issue #1026) ───────

    #[test]
    fn scaffold_references_field_emits_fk_column_constraint_and_index() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Comment",
            &["body:Text".into(), "post:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_comments/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("post_id BIGINT NOT NULL REFERENCES posts(id)"),
            "up.sql: {up}"
        );
        assert!(
            up.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"),
            "up.sql: {up}"
        );
    }

    #[test]
    fn scaffold_references_field_warns_when_target_model_missing() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("posts"));
    }

    #[test]
    fn scaffold_smoke_test_creates_a_stub_referenced_table_before_the_real_one() {
        // The generated smoke test runs in its own throwaway `TestDb` (one
        // Postgres testcontainer per `tests/*.rs` binary, per-process — see
        // `TestDb::shared()`), so it can't rely on some *other* scaffolded
        // resource's smoke test having already created the referenced table.
        // A `CREATE TABLE comments (... REFERENCES posts(id) ...)` would fail
        // with "relation posts does not exist" unless the comments smoke test
        // creates a minimal stand-in `posts` table itself first.
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Comment",
            &["body:Text".into(), "post:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/comment.rs")).unwrap();
        let stub_pos = test
            .find("CREATE TABLE IF NOT EXISTS posts")
            .unwrap_or_else(|| panic!("expected a stub `posts` table in the smoke test: {test}"));
        let real_pos = test.find("CREATE TABLE comments").unwrap_or_else(|| {
            panic!("expected the real `comments` table in the smoke test: {test}")
        });
        assert!(
            stub_pos < real_pos,
            "the stub referenced table must be created before the table under test: {test}"
        );
    }

    #[test]
    fn scaffold_smoke_test_emits_one_stub_per_distinct_reference_target() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Follow",
            &["follower:references".into(), "followee:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/follow.rs")).unwrap();
        assert!(test.contains("CREATE TABLE IF NOT EXISTS followers"));
        assert!(test.contains("CREATE TABLE IF NOT EXISTS followees"));
    }

    #[test]
    fn render_reference_stub_tables_sql_dedupes_identical_targets() {
        // Two references that resolve to the same target table must only
        // emit one `CREATE TABLE IF NOT EXISTS` for it. Constructed directly
        // (bypassing `parse_fields`, which enforces unique column names) so
        // the collision is deterministic rather than relying on two English
        // words coincidentally pluralising to the same string.
        let fields = vec![
            Field {
                name: "author_id".to_string(),
                kind: FieldKind::References,
                nullable: false,
                variants: Vec::new(),
            },
            Field {
                name: "author_id".to_string(),
                kind: FieldKind::References,
                nullable: true,
                variants: Vec::new(),
            },
        ];
        assert_eq!(
            fields[0].reference_table(),
            fields[1].reference_table(),
            "test setup: both fields must target the same table"
        );
        let sql = render_reference_stub_tables_sql(&fields, "unrelated_table");
        assert_eq!(
            sql.matches("CREATE TABLE IF NOT EXISTS").count(),
            1,
            "identical targets must be de-duplicated: {sql}"
        );
    }

    #[test]
    fn render_reference_stub_tables_sql_seeds_a_row_so_fk_id_1_is_valid() {
        // Regression test: a raw INSERT the smoke test issues against the
        // table under test (e.g. the enum out-of-set rejection test) uses a
        // NOT NULL `references` column's sample literal ("1" — see
        // `sql_sample_literal`); without a seeded row, that INSERT would
        // fail on the FK constraint regardless of what it's actually trying
        // to exercise.
        let fields = super::super::dsl::parse_fields(&["author:references".into()]).unwrap();
        let sql = render_reference_stub_tables_sql(&fields, "posts");
        assert_eq!(
            sql,
            "CREATE TABLE IF NOT EXISTS authors (id BIGSERIAL PRIMARY KEY);\n\
             INSERT INTO authors DEFAULT VALUES;\n",
            "got:\n{sql}"
        );
    }

    #[test]
    fn render_reference_stub_tables_sql_seeds_only_once_per_distinct_target() {
        let fields = vec![
            Field {
                name: "author_id".to_string(),
                kind: FieldKind::References,
                nullable: false,
                variants: Vec::new(),
            },
            Field {
                name: "author_id".to_string(),
                kind: FieldKind::References,
                nullable: true,
                variants: Vec::new(),
            },
        ];
        let sql = render_reference_stub_tables_sql(&fields, "unrelated_table");
        assert_eq!(
            sql.matches("INSERT INTO authors DEFAULT VALUES;").count(),
            1,
            "got:\n{sql}"
        );
    }

    #[test]
    fn render_reference_stub_tables_sql_skips_own_table() {
        // A self-referential `references` field (e.g. `Category` with
        // `category:references`) targets the model's own table, which the
        // real (non-`IF NOT EXISTS`) `CREATE TABLE` creates right after —
        // stubbing it first would collide with that statement.
        let fields = super::super::dsl::parse_fields(&["category:references".into()]).unwrap();
        assert_eq!(fields[0].reference_table().as_deref(), Some("categories"));
        let sql = render_reference_stub_tables_sql(&fields, "categories");
        assert!(sql.is_empty(), "must not stub the model's own table: {sql}");
    }

    #[test]
    fn scaffold_self_referential_reference_compiles_one_create_table() {
        let tmp = project_with_main(default_main());
        let plan = plan_scaffold(
            tmp.path(),
            "Category",
            &["name:String".into(), "category:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let test = fs::read_to_string(tmp.path().join("tests/category.rs")).unwrap();
        assert_eq!(
            test.matches("CREATE TABLE").count(),
            1,
            "a self-referential FK must not stub its own table before creating it for real: {test}"
        );
        assert!(test.contains("category_id BIGINT NOT NULL REFERENCES categories(id)"));
    }
}
