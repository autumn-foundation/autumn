//! `autumn generate controller` — handler-only, non-CRUD route modules.
//!
//! Unlike [`scaffold`](super::scaffold), which emits a full model + migration +
//! repository + CRUD routes, this generator emits *only* a route module: one
//! handler function per named action, wired into `routes![...]` and registered
//! in `src/routes/mod.rs`. No model, no migration, no database, no schema
//! changes, no `Cargo.toml` changes — the generated code compiles and serves
//! immediately.
//!
//! # Path rule
//!
//! Each action maps to `/<controller>/<action>`, except an action literally
//! named `index`, which maps to `/<controller>` (the collection root). Under
//! `--api` the prefix becomes `/api/<controller>[/<action>]`.
//!
//! # Method rule
//!
//! Actions default to `GET`. Request another method with `action:method`, where
//! `method` is one of `get`, `post`, `put`, `patch`, `delete`.
//!
//! # Modes
//!
//! - HTML (default): each action returns a Maud [`Markup`](maud::Markup) stub
//!   rendered through a local `layout(...)` helper → HTTP 200 with placeholder
//!   content.
//! - `--api`: each action returns `AutumnResult<Json<serde_json::Value>>` with a
//!   JSON placeholder body; no view stubs.

use std::fmt::Write as _;
use std::path::Path;

use super::emit::{Plan, Revert};
use super::naming::{humanize_label, snake};
use super::schema_edit::{add_mod_declaration, remove_routes_entries_with_prefix, update_main_rs};
use super::{GenerateError, ensure_project_root, read_or_empty};

/// HTTP methods a controller action may bind to.
const VALID_METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// A single parsed controller action: its handler function name, HTTP method,
/// fully-qualified route path, and display title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerAction {
    /// Snake-case handler function name (also the route path segment).
    pub fn_name: String,
    /// Lowercased HTTP method (`get`/`post`/`put`/`patch`/`delete`).
    pub method: String,
    /// Fully-qualified route path (e.g. `/pages/home`, `/api/pages`).
    pub path: String,
    /// Humanized display title (e.g. `Home`).
    pub title: String,
}

/// Parse and validate the CLI action tokens against `controller` (already
/// snake-normalized), honouring the `--api` path prefix.
fn parse_actions(
    controller: &str,
    actions: &[String],
    api: bool,
) -> Result<Vec<ControllerAction>, GenerateError> {
    if actions.is_empty() {
        return Err(GenerateError::InvalidName(
            controller.to_owned(),
            "at least one action is required (e.g. `autumn generate controller pages home about`)"
                .to_owned(),
        ));
    }

    let base = if api {
        format!("/api/{controller}")
    } else {
        format!("/{controller}")
    };

    let mut parsed = Vec::with_capacity(actions.len());
    for token in actions {
        let (raw_action, method) = match token.split_once(':') {
            Some((a, m)) => (a, m.to_ascii_lowercase()),
            None => (token.as_str(), "get".to_owned()),
        };
        if !VALID_METHODS.contains(&method.as_str()) {
            return Err(GenerateError::InvalidName(
                token.clone(),
                format!(
                    "invalid HTTP method '{method}' — expected one of {}",
                    VALID_METHODS.join(", ")
                ),
            ));
        }
        let fn_name = snake(raw_action);
        if !super::dsl::is_valid_ident(&fn_name)
            || super::dsl::RUST_KEYWORDS.contains(&fn_name.as_str())
        {
            return Err(GenerateError::InvalidName(
                token.clone(),
                format!("action '{raw_action}' is not a valid Rust identifier"),
            ));
        }
        // In HTML mode the module also emits a private `fn layout` helper, so an
        // action named `layout` would produce two items with the same name.
        if !api && fn_name == "layout" {
            return Err(GenerateError::InvalidName(
                token.clone(),
                "action `layout` is reserved in HTML mode (it collides with the \
                 generated `layout` view helper); rename the action or use `--api`"
                    .to_owned(),
            ));
        }
        let path = if fn_name == "index" {
            base.clone()
        } else {
            format!("{base}/{fn_name}")
        };
        let title = humanize_label(&fn_name);
        parsed.push(ControllerAction {
            fn_name,
            method,
            path,
            title,
        });
    }
    // Each action becomes one `pub async fn <fn_name>`; two actions with the
    // same handler name (e.g. `new:get new:post`, or `home home`) would emit
    // duplicate functions and fail to compile. A single fn can't bind two
    // methods, so this is always a user error.
    for (i, action) in parsed.iter().enumerate() {
        if let Some(dup) = parsed[..i].iter().find(|a| a.fn_name == action.fn_name) {
            return Err(GenerateError::InvalidName(
                action.fn_name.clone(),
                format!(
                    "action `{}` specified twice — handler names must be unique; \
                     a single fn can't bind both {} and {}",
                    action.fn_name, dup.method, action.method
                ),
            ));
        }
    }
    Ok(parsed)
}

/// Render the HTML (Maud) controller module for `actions`.
fn render_html(actions: &[ControllerAction]) -> String {
    let mut out = String::with_capacity(actions.len() * 200 + 512);
    out.push_str(
        "//! Generated by `autumn generate controller`.\n\
         //!\n\
         //! Handler-only routes — no model, no migration, no database. Edit freely.\n\
         \n\
         use autumn_web::prelude::*;\n\
         \n\
         fn layout(title: &str, content: Markup) -> Markup {\n\
         \x20   html! {\n\
         \x20       (PreEscaped(\"<!DOCTYPE html>\"))\n\
         \x20       html lang=\"en\" {\n\
         \x20           head {\n\
         \x20               meta charset=\"utf-8\";\n\
         \x20               title { (title) }\n\
         \x20           }\n\
         \x20           body {\n\
         \x20               (content)\n\
         \x20           }\n\
         \x20       }\n\
         \x20   }\n\
         }\n",
    );
    for action in actions {
        let ControllerAction {
            fn_name,
            method,
            path,
            title,
        } = action;
        let _ = write!(
            out,
            "\n#[{method}(\"{path}\")]\n\
             pub async fn {fn_name}() -> Markup {{\n\
             \x20   layout(\"{title}\", html! {{\n\
             \x20       h1 {{ \"{title}\" }}\n\
             \x20       p {{ \"Placeholder for the {fn_name} action — replace with real content.\" }}\n\
             \x20   }})\n\
             }}\n"
        );
    }
    out
}

/// Render the `--api` (JSON) controller module for `actions`.
fn render_api(actions: &[ControllerAction]) -> String {
    let mut out = String::with_capacity(actions.len() * 160 + 320);
    out.push_str(
        "//! Generated by `autumn generate controller --api`.\n\
         //!\n\
         //! JSON handler-only routes — no model, no migration, no database. Edit freely.\n\
         \n\
         use autumn_web::prelude::*;\n\
         use autumn_web::reexports::serde_json;\n",
    );
    for action in actions {
        let ControllerAction {
            fn_name,
            method,
            path,
            ..
        } = action;
        let _ = write!(
            out,
            "\n#[{method}(\"{path}\")]\n\
             pub async fn {fn_name}() -> AutumnResult<Json<serde_json::Value>> {{\n\
             \x20   Ok(Json(serde_json::json!({{ \"action\": \"{fn_name}\" }})))\n\
             }}\n"
        );
    }
    out
}

/// Compute the file plan for `autumn generate controller <name> <action>...`.
///
/// Emits a single handler module `src/routes/<name>.rs`, registers it in
/// `src/routes/mod.rs`, and wires each action's handler into `routes![...]` in
/// `src/main.rs`. No model, migration, schema, or `Cargo.toml` edits.
///
/// # Errors
/// - [`GenerateError::NotInProject`] outside an Autumn project root.
/// - [`GenerateError::InvalidName`] for an empty/invalid controller name, an
///   empty action list, an invalid method suffix, a non-identifier action, a
///   duplicate action handler name, or (in HTML mode) the reserved `layout`
///   action.
/// - [`GenerateError::Io`] when `src/main.rs` is missing.
pub fn plan_controller(
    project_root: &Path,
    name: &str,
    actions: &[String],
    api: bool,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    let controller = snake(name);
    if controller.is_empty()
        || !super::dsl::is_valid_ident(&controller)
        || super::dsl::RUST_KEYWORDS.contains(&controller.as_str())
    {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            "controller name must be a valid Rust identifier".to_owned(),
        ));
    }

    let parsed = parse_actions(&controller, actions, api)?;

    let mut plan = Plan::new(project_root.to_path_buf());

    // ── Handler module ────────────────────────────────────────────────────
    let rendered = if api {
        render_api(&parsed)
    } else {
        render_html(&parsed)
    };
    plan.create(
        project_root
            .join("src")
            .join("routes")
            .join(format!("{controller}.rs")),
        rendered,
    );

    // ── src/routes/mod.rs registration ────────────────────────────────────
    let mod_path = project_root.join("src").join("routes").join("mod.rs");
    plan.modify(
        mod_path.clone(),
        add_mod_declaration(&read_or_empty(&mod_path), &controller),
    );
    plan.push_revert(Revert::ModDecl {
        path: mod_path,
        name: controller.clone(),
    });

    // ── src/main.rs — `mod routes;` + route registration ──────────────────
    let entries: Vec<String> = parsed
        .iter()
        .map(|a| format!("routes::{controller}::{}", a.fn_name))
        .collect();
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|_| {
        GenerateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("missing {}", main_path.display()),
        ))
    })?;
    if !main_existing.contains("routes![") {
        // `update_main_rs` can only inject entries into an existing
        // `routes![...]` invocation; without one the handlers are created but
        // never mounted. Tell the user to wire them by hand rather than
        // silently emit dead routes.
        plan.warn(format!(
            "no `routes![...]` macro found in {} — the generated handlers were \
             not mounted; add them manually, e.g. `routes![{}]`",
            main_path.display(),
            entries.join(", ")
        ));
    }
    // On a `--force` regeneration with a changed action set, the overwritten
    // handler file no longer defines the old actions, so any stale
    // `routes::<controller>::*` entries left in `routes![...]` would reference
    // functions that no longer exist and break compilation. Strip every
    // existing entry for THIS controller first, then add the fresh set. A
    // no-op on first generation (nothing matches the prefix), and scoped to
    // this controller's `routes::<snake>::` prefix so other controllers' and
    // unrelated routes are untouched.
    let pruned =
        remove_routes_entries_with_prefix(&main_existing, &format!("routes::{controller}::"));
    plan.modify(
        main_path.clone(),
        update_main_rs(&pruned, &["routes"], &entries),
    );
    plan.push_revert(Revert::RoutesEntries {
        path: main_path,
        entries,
    });

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use std::fs;
    use tempfile::TempDir;

    fn project_with_main() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/main.rs"),
            "use autumn_web::prelude::*;\n\
             \n\
             #[get(\"/\")]\n\
             async fn index() -> &'static str { \"ok\" }\n\
             \n\
             #[autumn_web::main]\n\
             async fn main() {\n\
             \x20   autumn_web::app()\n\
             \x20       .routes(routes![index])\n\
             \x20       .run()\n\
             \x20       .await;\n\
             }\n",
        )
        .unwrap();
        // A fresh `autumn new` project has no src/routes/; the generator creates it.
        tmp
    }

    fn actions(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn run(
        tmp: &TempDir,
        name: &str,
        list: &[&str],
        api: bool,
        force: bool,
    ) -> Result<(), GenerateError> {
        let plan = plan_controller(tmp.path(), name, &actions(list), api)?;
        plan.execute(Flags {
            force,
            dry_run: false,
        })
    }

    #[test]
    fn html_controller_creates_handlers_and_wires_routes() {
        let tmp = project_with_main();
        run(&tmp, "pages", &["home", "about", "contact"], false, false).unwrap();

        let file = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();
        assert!(file.contains("#[get(\"/pages/home\")]"), "{file}");
        assert!(file.contains("pub async fn home"), "{file}");
        assert!(file.contains("#[get(\"/pages/about\")]"), "{file}");
        assert!(file.contains("#[get(\"/pages/contact\")]"), "{file}");
        assert!(file.contains("fn layout"), "{file}");
        assert!(file.contains("html!"), "{file}");

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("routes::pages::home"), "{main}");
        assert!(main.contains("routes::pages::about"), "{main}");
        assert!(main.contains("routes::pages::contact"), "{main}");
        assert!(main.contains("mod routes;"), "{main}");

        let routes_mod = fs::read_to_string(tmp.path().join("src/routes/mod.rs")).unwrap();
        assert!(routes_mod.contains("pub mod pages;"), "{routes_mod}");

        // No model / migration / schema / Cargo.toml side effects.
        assert!(!tmp.path().join("migrations").exists(), "no migrations");
        assert!(!tmp.path().join("src/models").exists(), "no models dir");
        assert!(!tmp.path().join("src/schema.rs").exists(), "no schema.rs");
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(cargo, "[package]\nname=\"x\"\n", "Cargo.toml unchanged");
    }

    #[test]
    fn action_method_suffix_parsed() {
        let tmp = project_with_main();
        run(&tmp, "pages", &["submit:post"], false, false).unwrap();
        let file = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();
        assert!(file.contains("#[post(\"/pages/submit\")]"), "{file}");
        assert!(file.contains("pub async fn submit"), "{file}");
    }

    #[test]
    fn index_action_maps_to_controller_root() {
        let tmp = project_with_main();
        run(&tmp, "pages", &["index"], false, false).unwrap();
        let file = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();
        assert!(file.contains("#[get(\"/pages\")]"), "{file}");
        assert!(!file.contains("#[get(\"/pages/index\")]"), "{file}");
    }

    #[test]
    fn api_mode_emits_json_and_no_views() {
        let tmp = project_with_main();
        run(&tmp, "pages", &["index", "stats"], true, false).unwrap();
        let file = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();
        assert!(
            file.contains("AutumnResult<Json<serde_json::Value>>"),
            "{file}"
        );
        assert!(file.contains("#[get(\"/api/pages\")]"), "{file}");
        assert!(file.contains("#[get(\"/api/pages/stats\")]"), "{file}");
        assert!(!file.contains("layout("), "{file}");
        assert!(!file.contains("html!"), "{file}");
    }

    #[test]
    fn rerun_without_force_fails_and_preserves_file() {
        let tmp = project_with_main();
        run(&tmp, "pages", &["home"], false, false).unwrap();
        let original = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();

        // A second run with different actions must NOT overwrite.
        let err = run(&tmp, "pages", &["home", "about"], false, false).unwrap_err();
        assert!(matches!(err, GenerateError::Collisions(_)), "{err:?}");
        let after = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();
        assert_eq!(original, after, "file must be untouched on collision");

        // With --force it succeeds and rewrites.
        run(&tmp, "pages", &["home", "about"], false, true).unwrap();
        let forced = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();
        assert!(forced.contains("#[get(\"/pages/about\")]"), "{forced}");
    }

    #[test]
    fn empty_actions_rejected() {
        let tmp = project_with_main();
        let err = plan_controller(tmp.path(), "pages", &[], false).unwrap_err();
        assert!(matches!(err, GenerateError::InvalidName(_, _)), "{err:?}");
    }

    #[test]
    fn bad_method_suffix_rejected() {
        let tmp = project_with_main();
        let err = plan_controller(tmp.path(), "pages", &actions(&["submit:frobnicate"]), false)
            .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidName(_, _)), "{err:?}");
    }

    #[test]
    fn empty_name_rejected() {
        let tmp = project_with_main();
        let err = plan_controller(tmp.path(), "", &actions(&["home"]), false).unwrap_err();
        assert!(matches!(err, GenerateError::InvalidName(_, _)), "{err:?}");
    }

    #[test]
    fn duplicate_action_names_rejected() {
        let tmp = project_with_main();
        // Same stem via two methods → two `pub async fn new` → would not compile.
        let err = plan_controller(
            tmp.path(),
            "posts",
            &actions(&["new:get", "new:post"]),
            false,
        )
        .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidName(_, _)), "{err:?}");

        // Plain repeated stem is rejected too.
        let err2 =
            plan_controller(tmp.path(), "pages", &actions(&["home", "home"]), false).unwrap_err();
        assert!(matches!(err2, GenerateError::InvalidName(_, _)), "{err2:?}");
    }

    #[test]
    fn layout_action_reserved_in_html_mode() {
        let tmp = project_with_main();
        // HTML mode emits a `fn layout` helper, so an action named `layout` collides.
        let err = plan_controller(tmp.path(), "pages", &actions(&["layout"]), false).unwrap_err();
        assert!(matches!(err, GenerateError::InvalidName(_, _)), "{err:?}");

        // In --api mode there is no `layout` helper, so it's allowed.
        run(&tmp, "pages", &["layout"], true, false).unwrap();
        let file = fs::read_to_string(tmp.path().join("src/routes/pages.rs")).unwrap();
        assert!(file.contains("pub async fn layout"), "{file}");
    }

    #[test]
    fn force_regeneration_prunes_stale_route_entries() {
        let tmp = project_with_main();
        // Seed an unrelated controller's entry so we can prove it survives the
        // prune-and-rewrite of `pages`.
        let main_path = tmp.path().join("src/main.rs");
        let seeded = fs::read_to_string(&main_path)
            .unwrap()
            .replace("routes![index]", "routes![index, routes::other::index]");
        fs::write(&main_path, seeded).unwrap();

        run(&tmp, "pages", &["home", "about"], false, false).unwrap();
        let after_first = fs::read_to_string(&main_path).unwrap();
        assert!(after_first.contains("routes::pages::home"), "{after_first}");
        assert!(
            after_first.contains("routes::pages::about"),
            "{after_first}"
        );

        // Regenerate with a changed action set (`about` dropped, `contact`
        // added) using --force.
        run(&tmp, "pages", &["home", "contact"], false, true).unwrap();
        let after_force = fs::read_to_string(&main_path).unwrap();
        assert!(after_force.contains("routes::pages::home"), "{after_force}");
        assert!(
            after_force.contains("routes::pages::contact"),
            "{after_force}"
        );
        // The stale entry for the now-removed `about` action must be gone.
        assert!(
            !after_force.contains("routes::pages::about"),
            "stale routes::pages::about should have been pruned:\n{after_force}"
        );
        // Unrelated entries must be untouched.
        assert!(
            after_force.contains("routes::other::index"),
            "unrelated route entry must survive:\n{after_force}"
        );
        assert!(after_force.contains("index"), "{after_force}");
    }
}
