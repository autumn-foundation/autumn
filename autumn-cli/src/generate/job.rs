//! `autumn generate job` — scaffold a `#[job]` handler, args struct,
//! `src/jobs/mod.rs` aggregator, and main.rs wiring.
//!
//! For a name like `SendWelcomeEmail`, the generator produces:
//! - `src/jobs/send_welcome_email.rs` — `SendWelcomeEmailArgs` struct and a
//!   `#[job]` handler with a commented enqueue snippet and an inline smoke test.
//! - `src/jobs/mod.rs` — created or updated with `pub mod send_welcome_email;`
//!   and an idempotent `registered_jobs()` aggregator.
//! - `src/main.rs` — `mod jobs;` declaration and
//!   `.jobs(jobs::registered_jobs())` wired into the app builder chain.

use std::path::Path;

use super::dsl::{Field, FieldKind, parse_fields};
use super::emit::Plan;
use super::model::validate_resource_name;
use super::naming::{pascal, snake};
use super::schema_edit::{
    add_jobs_registration_to_app, add_mod_declaration, augment_registered_jobs, update_main_rs,
};
use autumn_web::config::DatabaseBackend;

use super::{GenerateError, ensure_project_root, read_or_empty};

/// Cargo dependencies always required by generated job files.
const JOB_DEPS_BASE: &[(&str, &str)] = &[("serde", "{ version = \"1\", features = [\"derive\"] }")];

/// Extra deps needed when the args struct uses `uuid::Uuid` fields.
const JOB_DEPS_UUID: (&str, &str) = ("uuid", "{ version = \"1\", features = [\"serde\"] }");

/// Extra deps needed when the args struct uses `chrono` date/time fields.
const JOB_DEPS_CHRONO: (&str, &str) = ("chrono", "{ version = \"0.4\", features = [\"serde\"] }");

/// Extra dep needed when the args struct uses a `decimal` field — mirrors
/// `model.rs`'s `MODEL_DEPS`/`plan_model` conditional so `render_fields`'s
/// `f.rust_type()` (`"rust_decimal::Decimal"`) always has a matching
/// Cargo.toml entry, the same way `JOB_DEPS_UUID`/`JOB_DEPS_CHRONO` already
/// do for `uuid::Uuid`/`chrono` fields.
///
/// Backend-aware for the same reason `model.rs`'s is (issue #1924): asking a
/// `SQLite` app for `rust_decimal`'s *Postgres* diesel feature is wrong, and the
/// two generators share one `rust_decimal` line.
const fn job_deps_decimal(backend: DatabaseBackend) -> (&'static str, &'static str) {
    (
        "rust_decimal",
        match backend {
            DatabaseBackend::Postgres => {
                "{ version = \"1\", features = [\"db-diesel2-postgres\", \"serde\"] }"
            }
            DatabaseBackend::Sqlite => "{ version = \"1\", features = [\"serde\"] }",
        },
    )
}

/// Build the complete dep list for a job based on which field types are used.
fn job_deps(fields: &[Field], backend: DatabaseBackend) -> Vec<(&'static str, &'static str)> {
    let mut deps: Vec<(&str, &str)> = JOB_DEPS_BASE.to_vec();
    if fields.iter().any(|f| matches!(f.kind, FieldKind::Uuid)) {
        deps.push(JOB_DEPS_UUID);
    }
    if fields
        .iter()
        .any(|f| matches!(f.kind, FieldKind::NaiveDateTime | FieldKind::DateTime))
    {
        deps.push(JOB_DEPS_CHRONO);
    }
    if fields.iter().any(|f| f.kind.is_decimal()) {
        deps.push(job_deps_decimal(backend));
    }
    deps
}

/// Compute the file actions for `autumn generate job`.
///
/// # Errors
/// Returns errors for project-layout issues and name-validation failures.
pub fn plan_job(project_root: &Path, name: &str, fields: &[String]) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;

    let parsed_fields = parse_fields(fields)?;
    let snake_name = snake(name);
    // Derive pascal_name from the normalized snake_name so lower-camelCase input
    // (e.g. `sendWelcomeEmail`) produces `SendWelcomeEmail`, not `sendWelcomeEmail`.
    let pascal_name = pascal(&snake_name);

    // Attachment fields require the `storage` autumn-web feature and are not
    // meaningful as job arguments (pass a storage key or file ID instead).
    // `parse_fields` builds `parsed_fields` from `fields` one-to-one with no
    // skips, so `fields[i]` is the token that produced `parsed_fields[i]` — the
    // refusals below can echo what the user actually typed.
    for (i, field) in parsed_fields.iter().enumerate() {
        if matches!(field.kind, FieldKind::Attachment) {
            return Err(GenerateError::InvalidField {
                token: format!("{}:Attachment", field.name),
                reason: "Attachment fields are not supported in job args structs; \
                         pass a storage key (String) or file ID (i64) instead"
                    .into(),
            });
        }
        // Issue #1340: at-rest column encryption is a `#[model]` concern. A job
        // args struct is a serde payload serialized into the queue, not a
        // database column, so `#[encrypted]` has nothing to attach to — and
        // silently dropping the modifier would hand back a plaintext argument
        // the author believed was protected.
        if field.is_encrypted() {
            return Err(GenerateError::InvalidField {
                // Echo the token the user typed rather than synthesizing one:
                // `notes:Text{encrypted:deterministic}` must not be reported
                // back as `notes:String{encrypted}`.
                token: fields[i].clone(),
                reason: "the `encrypted` modifier applies to model columns, not job arguments: \
                         job args are serialized into the queue payload, not stored in a \
                         column, so there is no `#[model]` field for `#[encrypted]` to attach \
                         to. Declare the encrypted column with `generate model`/`generate \
                         scaffold` and pass the record's id as the job argument instead."
                    .into(),
            });
        }
        // An enum field's Rust type is generated by `generate model`/
        // `scaffold`/`migration` alongside the model file it lives in; a job
        // args struct has no model file to put the generated enum in, so
        // `field.rust_type()` would name a type this file never declares.
        if field.is_enum() {
            return Err(GenerateError::InvalidField {
                token: format!("{}:enum{{...}}", field.name),
                reason: "enum fields are supported by `generate model`/`scaffold`/`migration`, \
                         not `generate job`; use String for a free-form job argument instead"
                    .into(),
            });
        }
    }
    let args_struct = format!("{pascal_name}Args");
    let companion_struct = format!("{pascal_name}Job");
    let job_entry = format!("{snake_name}::{snake_name}");

    let mut plan = Plan::new(project_root);

    // ── src/jobs/<snake>.rs ───────────────────────────────────────────────
    plan.create(
        project_root
            .join("src")
            .join("jobs")
            .join(format!("{snake_name}.rs")),
        render_job_file(&snake_name, &args_struct, &companion_struct, &parsed_fields),
    );

    // ── src/jobs/mod.rs (create or update) ────────────────────────────────
    let mod_path = project_root.join("src").join("jobs").join("mod.rs");
    let existing_mod = read_or_empty(&mod_path);
    let with_mod_decl = add_mod_declaration(&existing_mod, &snake_name);
    let with_aggregator = augment_registered_jobs(&with_mod_decl, &job_entry);
    plan.modify(mod_path.clone(), with_aggregator);
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: mod_path.clone(),
        name: snake_name,
    });
    plan.push_revert(crate::generate::emit::Revert::JobEntry {
        path: mod_path,
        entry: job_entry,
    });

    // ── src/main.rs: add mod jobs; and .jobs(…) ───────────────────────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|e| {
        GenerateError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", main_path.display()),
        ))
    })?;
    let with_mod = update_main_rs(&main_existing, &["jobs"], &[]);
    let updated_main = add_jobs_registration_to_app(&with_mod);
    plan.modify(main_path.clone(), updated_main);
    // `mod jobs;` is a shared infrastructure declaration (see
    // `emit::sync_main_rs_mod_declarations`) — only the `.jobs(...)` call is
    // reverted here, and only once no other job file remains (a sibling job
    // still listed in the shared `jobs![...]` needs this call to actually run).
    plan.push_revert(crate::generate::emit::Revert::JobsRegistration {
        path: main_path,
        owner_dir: project_root.join("src").join("jobs"),
    });

    // ── Cargo.toml: ensure serde (always) plus uuid/chrono/decimal if used ──
    // The decimal dep's features are backend-specific (issue #1924).
    let backend = super::detect_backend(project_root);
    if parsed_fields.iter().any(|f| f.kind.is_decimal()) {
        let existing_cargo_toml = read_or_empty(&project_root.join("Cargo.toml"));
        // Job args structs only derive Serialize/Deserialize (no Diesel
        // Queryable/Insertable), so strictly only need `serde` — but
        // JOB_DEPS_DECIMAL below requests the same feature set as
        // MODEL_DEPS's rust_decimal entry for consistency (a project using
        // both `generate job` and `generate model`/`scaffold` shares one
        // `rust_decimal` dep line), so this warning checks the same set.
        super::model::warn_if_existing_dep_missing_features(
            &mut plan,
            &existing_cargo_toml,
            "rust_decimal",
            super::model::decimal_dep_features(backend),
        );
    }
    super::model::plan_cargo_deps(
        &mut plan,
        project_root,
        &job_deps(&parsed_fields, backend),
        &project_root.join("src/jobs"),
    );

    Ok(plan)
}

/// Render `src/jobs/<snake>.rs`.
fn render_job_file(
    snake_name: &str,
    args_struct: &str,
    companion_struct: &str,
    fields: &[Field],
) -> String {
    let fields_block = render_fields(fields);
    let smoke_test = render_smoke_test(snake_name, companion_struct);
    format!(
        r#"//! Generated by `autumn generate job`.
//!
//! Edit freely — once generated, this is ordinary user code.
//!
//! The `#[job]` macro generates a companion struct `{companion_struct}` with:
//!   - `{companion_struct}::NAME: &'static str` — the registered job name.
//!   - `{companion_struct}::enqueue(args).await?` — at-least-once enqueue.
//!   - `{companion_struct}::enqueue_in(delay, args).await?` — delayed enqueue.
//!   - `{companion_struct}::enqueue_at(time, args).await?` — scheduled enqueue.
//!
//! To enqueue from a handler, pick one of:
//!
//!     // At-least-once (idempotency burden is yours):
//!     {companion_struct}::enqueue(args).await?;
//!
//!     // Transactional — enqueues only if the surrounding DB transaction commits:
//!     autumn_web::job::enqueue_on_conn({companion_struct}::NAME, &args, conn).await?;

use autumn_web::prelude::*;
use serde::{{Deserialize, Serialize}};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct {args_struct} {{{fields_block}}}

#[job(name = "{snake_name}", max_attempts = 5, backoff_ms = 500)]
pub async fn {snake_name}(_state: AppState, _args: {args_struct}) -> AutumnResult<()> {{
    // TODO: implement job body here.
    Ok(())
}}
{smoke_test}"#
    )
}

/// Render the fields block for the args struct.
fn render_fields(fields: &[Field]) -> String {
    use std::fmt::Write as _;
    if fields.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for f in fields {
        write!(out, "\n    pub {}: {},", f.name, f.rust_type()).unwrap();
    }
    out.push('\n');
    out
}

/// Render the inline smoke test.
fn render_smoke_test(snake_name: &str, companion_struct: &str) -> String {
    format!(
        r#"
#[cfg(test)]
mod {snake_name}_job_tests {{
    use super::*;

    #[test]
    fn {snake_name}_job_name_matches_handler() {{
        assert_eq!({companion_struct}::NAME, "{snake_name}");
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use std::fs;
    use tempfile::TempDir;

    fn project_with_main(main_content: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), main_content).unwrap();
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
    fn generate_then_destroy_job_round_trips_to_original_project_state() {
        let tmp = project_with_main(default_main());
        let cargo_path = tmp.path().join("Cargo.toml");
        let main_path = tmp.path().join("src/main.rs");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        let plan = plan_job(tmp.path(), "SendWelcomeEmail", &[]).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/jobs/send_welcome_email.rs").exists());
        assert!(main_path.exists());
        assert!(fs::read_to_string(&main_path).unwrap().contains(".jobs("));

        let destroy_plan = plan_job(tmp.path(), "SendWelcomeEmail", &[]).unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/jobs/send_welcome_email.rs").exists());
        assert!(!tmp.path().join("src/jobs/mod.rs").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn destroying_one_of_two_jobs_keeps_the_jobs_registration_call() {
        let tmp = project_with_main(default_main());
        let main_path = tmp.path().join("src/main.rs");

        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_job(tmp.path(), "SendGoodbyeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(fs::read_to_string(&main_path).unwrap().contains(".jobs("));

        // Destroying one job alone must NOT strip the `.jobs(...)` call —
        // the surviving job's `jobs![...]` entry still needs it to actually run.
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/jobs/send_welcome_email.rs").exists());
        assert!(tmp.path().join("src/jobs/send_goodbye_email.rs").exists());
        let main_after = fs::read_to_string(&main_path).unwrap();
        assert!(
            main_after.contains(".jobs(jobs::registered_jobs())"),
            "the .jobs(...) call must survive — the surviving job still needs it to run: {main_after}"
        );
        let mod_after = fs::read_to_string(tmp.path().join("src/jobs/mod.rs")).unwrap();
        assert!(mod_after.contains("send_goodbye_email"));
        assert!(!mod_after.contains("send_welcome_email"));

        // Now destroy the last remaining job — the call must finally go too.
        plan_job(tmp.path(), "SendGoodbyeEmail", &[])
            .unwrap()
            .revert(Flags::default())
            .unwrap();
        assert!(!tmp.path().join("src/jobs/send_goodbye_email.rs").exists());
        assert!(
            !fs::read_to_string(&main_path).unwrap().contains(".jobs("),
            "the .jobs(...) call must be removed once no job needs it anymore"
        );
    }

    // ── RED: plan assertions ─────────────────────────────────────────────

    #[test]
    fn plan_creates_job_file() {
        let tmp = project_with_main(default_main());
        let plan = plan_job(tmp.path(), "SendWelcomeEmail", &[]).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/jobs/send_welcome_email.rs")),
            "plan must include src/jobs/send_welcome_email.rs"
        );
    }

    #[test]
    fn plan_includes_jobs_mod_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_job(tmp.path(), "SendWelcomeEmail", &[]).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/jobs/mod.rs")),
            "plan must include src/jobs/mod.rs"
        );
    }

    #[test]
    fn plan_modifies_main_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_job(tmp.path(), "SendWelcomeEmail", &[]).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/main.rs")),
            "plan must modify src/main.rs"
        );
    }

    #[test]
    fn plan_errors_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_job(tmp.path(), "SendWelcomeEmail", &[]).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn plan_errors_when_main_rs_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let err = plan_job(tmp.path(), "SendWelcomeEmail", &[]).unwrap_err();
        assert!(matches!(err, GenerateError::Io(_)));
    }

    #[test]
    fn plan_rejects_invalid_name() {
        let tmp = project_with_main(default_main());
        let err = plan_job(tmp.path(), "123invalid", &[]).unwrap_err();
        assert!(
            matches!(err, GenerateError::InvalidName(_, _)),
            "must reject numeric-prefix name"
        );
    }

    // ── GREEN: execute and inspect written content ───────────────────────

    #[test]
    fn execute_writes_args_struct_with_correct_derives() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &["user_id:i64".to_string()])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/jobs/send_welcome_email.rs")).unwrap();
        assert!(
            src.contains("pub struct SendWelcomeEmailArgs"),
            "must define args struct"
        );
        assert!(
            src.contains("pub user_id: i64"),
            "must include parsed field"
        );
        assert!(
            src.contains("#[derive(Debug, Clone, Serialize, Deserialize)]"),
            "args struct must derive Serialize + Deserialize"
        );
    }

    #[test]
    fn execute_writes_job_handler_with_correct_attribute() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/jobs/send_welcome_email.rs")).unwrap();
        assert!(
            src.contains(
                r#"#[job(name = "send_welcome_email", max_attempts = 5, backoff_ms = 500)]"#
            ),
            "#[job] attribute must be present: {src}"
        );
        assert!(
            src.contains("pub async fn send_welcome_email("),
            "handler fn must be pub async: {src}"
        );
    }

    #[test]
    fn execute_writes_enqueue_snippet() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/jobs/send_welcome_email.rs")).unwrap();
        assert!(
            src.contains("SendWelcomeEmailJob::enqueue"),
            "file must contain at-least-once enqueue snippet"
        );
        assert!(
            src.contains("enqueue_on_conn"),
            "file must contain transactional enqueue snippet"
        );
    }

    #[test]
    fn execute_writes_inline_smoke_test_with_name_assertion() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/jobs/send_welcome_email.rs")).unwrap();
        assert!(
            src.contains("#[cfg(test)]"),
            "must include an inline test module"
        );
        assert!(
            src.contains("SendWelcomeEmailJob::NAME"),
            "smoke test must assert the NAME constant"
        );
        assert!(
            src.contains(r#""send_welcome_email""#),
            "smoke test must check the string value of NAME"
        );
    }

    #[test]
    fn execute_writes_mod_rs_with_pub_mod_and_aggregator() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/jobs/mod.rs")).unwrap();
        assert!(
            mod_rs.contains("pub mod send_welcome_email;"),
            "must declare pub mod"
        );
        assert!(
            mod_rs.contains("pub fn registered_jobs()"),
            "must define registered_jobs fn"
        );
        assert!(
            mod_rs.contains("jobs![send_welcome_email::send_welcome_email]"),
            "registered_jobs must list the new job"
        );
    }

    #[test]
    fn execute_updates_main_rs_with_mod_declaration() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("mod jobs;"), "main.rs must declare mod jobs");
    }

    #[test]
    fn execute_updates_main_rs_with_jobs_call() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains(".jobs(jobs::registered_jobs())"),
            "main.rs must include .jobs() call: {main}"
        );
        let jobs_pos = main.find(".jobs(").unwrap();
        let run_pos = main.find(".run()").unwrap();
        assert!(jobs_pos < run_pos, ".jobs() must precede .run()");
    }

    // ── Name normalisation ────────────────────────────────────────────────

    #[test]
    fn snake_case_input_is_normalised() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "send_welcome_email", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        assert!(tmp.path().join("src/jobs/send_welcome_email.rs").exists());
        let src = fs::read_to_string(tmp.path().join("src/jobs/send_welcome_email.rs")).unwrap();
        assert!(src.contains("SendWelcomeEmailArgs"));
        assert!(src.contains("SendWelcomeEmailJob"));
    }

    #[test]
    fn pascal_case_input_is_normalised() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        assert!(tmp.path().join("src/jobs/send_welcome_email.rs").exists());
    }

    #[test]
    fn lower_camel_input_produces_correct_pascal_name() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "sendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        // File must exist and struct names must be PascalCase, not lower-camelCase.
        let src = fs::read_to_string(tmp.path().join("src/jobs/send_welcome_email.rs")).unwrap();
        assert!(
            src.contains("pub struct SendWelcomeEmailArgs"),
            "args struct must be PascalCase: {src}"
        );
        assert!(
            src.contains("SendWelcomeEmailJob::NAME"),
            "companion struct must be PascalCase: {src}"
        );
        assert!(
            !src.contains("sendWelcomeEmailJob"),
            "must not contain lower-camelCase companion: {src}"
        );
    }

    #[test]
    fn attachment_field_is_rejected() {
        let tmp = project_with_main(default_main());
        let err = plan_job(
            tmp.path(),
            "ProcessUpload",
            &["avatar:Attachment".to_string()],
        )
        .unwrap_err();
        assert!(
            matches!(err, GenerateError::InvalidField { .. }),
            "Attachment field must be rejected: {err:?}"
        );
    }

    #[test]
    fn enum_field_is_rejected() {
        // issue #1030: an enum field's generated Rust type lives in a model
        // file `generate job` never creates, so `field.rust_type()` would
        // name a type this file doesn't declare.
        let tmp = project_with_main(default_main());
        let err = plan_job(
            tmp.path(),
            "ProcessOrder",
            &["status:enum{pending,shipped}".to_string()],
        )
        .unwrap_err();
        assert!(
            matches!(err, GenerateError::InvalidField { .. }),
            "enum field must be rejected: {err:?}"
        );
        assert!(
            err.to_string().contains("generate model"),
            "error should point to the right generator: {err}"
        );
    }

    // ── Field rendering ───────────────────────────────────────────────────

    #[test]
    fn multiple_fields_all_rendered_in_args_struct() {
        let tmp = project_with_main(default_main());
        plan_job(
            tmp.path(),
            "PostNotification",
            &[
                "post_id:i64".to_string(),
                "title:String".to_string(),
                "published:bool".to_string(),
            ],
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/jobs/post_notification.rs")).unwrap();
        assert!(src.contains("pub post_id: i64"));
        assert!(src.contains("pub title: String"));
        assert!(src.contains("pub published: bool"));
    }

    #[test]
    fn optional_field_is_rendered_as_option() {
        let tmp = project_with_main(default_main());
        plan_job(
            tmp.path(),
            "SendEmail",
            &["reply_to:Option<String>".to_string()],
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/jobs/send_email.rs")).unwrap();
        assert!(src.contains("pub reply_to: Option<String>"));
    }

    // ── Flag behaviour ────────────────────────────────────────────────────

    #[test]
    fn dry_run_writes_no_new_files() {
        let tmp = project_with_main(default_main());
        let original_main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags {
                dry_run: true,
                force: false,
            })
            .unwrap();

        assert!(
            !tmp.path().join("src/jobs/send_welcome_email.rs").exists(),
            "dry run must not write the job file"
        );
        assert!(
            !tmp.path().join("src/jobs/mod.rs").exists(),
            "dry run must not write mod.rs"
        );
        let main_after = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(original_main, main_after, "dry run must not modify main.rs");
    }

    #[test]
    fn collision_without_force_returns_error() {
        let tmp = project_with_main(default_main());
        fs::create_dir_all(tmp.path().join("src/jobs")).unwrap();
        fs::write(
            tmp.path().join("src/jobs/send_welcome_email.rs"),
            "// existing",
        )
        .unwrap();
        let err = plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap_err();
        assert!(
            matches!(err, GenerateError::Collisions(_)),
            "should return collision error; got {err:?}"
        );
    }

    #[test]
    fn force_overwrites_existing_job() {
        let tmp = project_with_main(default_main());
        fs::create_dir_all(tmp.path().join("src/jobs")).unwrap();
        fs::write(tmp.path().join("src/jobs/send_welcome_email.rs"), "// old").unwrap();
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/jobs/send_welcome_email.rs")).unwrap();
        assert!(
            src.contains("SendWelcomeEmailArgs"),
            "force must regenerate the job file"
        );
    }

    // ── Idempotency: second job augments but doesn't duplicate ─────────────

    #[test]
    fn second_job_augments_mod_rs_without_duplication() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_job(tmp.path(), "PostNotification", &["post_id:i64".to_string()])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/jobs/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod send_welcome_email;"));
        assert!(mod_rs.contains("pub mod post_notification;"));
        assert!(mod_rs.contains("send_welcome_email::send_welcome_email"));
        assert!(mod_rs.contains("post_notification::post_notification"));
        assert_eq!(
            mod_rs.matches("pub fn registered_jobs").count(),
            1,
            "registered_jobs must appear exactly once"
        );
    }

    #[test]
    fn second_job_does_not_duplicate_jobs_call_in_main() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_job(tmp.path(), "PostNotification", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(
            main.matches(".jobs(jobs::registered_jobs())").count(),
            1,
            ".jobs() must appear exactly once in main.rs"
        );
        assert_eq!(
            main.matches("mod jobs;").count(),
            1,
            "mod jobs; must appear exactly once in main.rs"
        );
    }

    #[test]
    fn rerunning_same_job_is_idempotent_for_mod_rs() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        // Second run with --force (job file already exists)
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/jobs/mod.rs")).unwrap();
        assert_eq!(
            mod_rs.matches("pub mod send_welcome_email;").count(),
            1,
            "pub mod must appear exactly once"
        );
        assert_eq!(
            mod_rs
                .matches("send_welcome_email::send_welcome_email")
                .count(),
            1,
            "jobs entry must appear exactly once"
        );
    }

    // ── Cargo.toml ────────────────────────────────────────────────────────

    #[test]
    fn execute_adds_serde_to_cargo_toml() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "SendWelcomeEmail", &[])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("serde"),
            "Cargo.toml must include serde: {cargo}"
        );
    }

    #[test]
    fn uuid_field_adds_uuid_dep_to_cargo_toml() {
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "ProcessUser", &["user_id:Uuid".to_string()])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("uuid"),
            "Cargo.toml must include uuid: {cargo}"
        );
    }

    #[test]
    fn datetime_field_adds_chrono_dep_to_cargo_toml() {
        let tmp = project_with_main(default_main());
        plan_job(
            tmp.path(),
            "ScheduleReport",
            &["run_at:NaiveDateTime".to_string()],
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("chrono"),
            "Cargo.toml must include chrono: {cargo}"
        );
    }

    #[test]
    fn primitive_fields_do_not_add_uuid_or_chrono() {
        let tmp = project_with_main(default_main());
        plan_job(
            tmp.path(),
            "SendWelcomeEmail",
            &["user_id:i64".to_string(), "email:String".to_string()],
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            !cargo.contains("uuid"),
            "Cargo.toml must not include uuid for primitive fields"
        );
        assert!(
            !cargo.contains("chrono"),
            "Cargo.toml must not include chrono for primitive fields"
        );
    }

    #[test]
    fn decimal_field_adds_rust_decimal_dep_to_cargo_toml() {
        // Regression test (issue #1038 PR review): `render_fields` emits
        // `f.rust_type()` ("rust_decimal::Decimal") into the args struct for
        // any `decimal` field, so the generated Cargo.toml must declare the
        // crate — job.rs previously had its own `job_deps` list that only
        // special-cased `Uuid`/chrono and silently omitted `rust_decimal`,
        // producing a job args struct that failed to compile.
        let tmp = project_with_main(default_main());
        plan_job(tmp.path(), "ProcessRefund", &["amount:decimal".to_string()])
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("rust_decimal"),
            "Cargo.toml must include rust_decimal: {cargo}"
        );

        let job_file = fs::read_to_string(tmp.path().join("src/jobs/process_refund.rs")).unwrap();
        assert!(
            job_file.contains("pub amount: rust_decimal::Decimal,"),
            "job args struct must declare the decimal field: {job_file}"
        );
    }

    #[test]
    fn primitive_fields_do_not_add_rust_decimal() {
        let tmp = project_with_main(default_main());
        plan_job(
            tmp.path(),
            "SendWelcomeEmail",
            &["user_id:i64".to_string(), "email:String".to_string()],
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            !cargo.contains("rust_decimal"),
            "Cargo.toml must not include rust_decimal for primitive fields"
        );
    }

    #[test]
    fn decimal_field_warns_when_existing_rust_decimal_dep_lacks_diesel_feature() {
        let tmp = project_with_main(default_main());
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nrust_decimal = \"1\"\n",
        )
        .unwrap();
        let plan = plan_job(tmp.path(), "ProcessRefund", &["amount:decimal".to_string()]).unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("rust_decimal"));
        assert!(plan.warnings[0].contains("db-diesel2-postgres"));
    }

    // ── `{encrypted}` is not a `generate job` token (issue #1340) ───────────

    /// R9: a job args struct is a serde payload, not a database column — there
    /// is no `#[model]` for `#[encrypted]` to attach to, and the args are
    /// serialized into the job queue. Refuse rather than silently drop the
    /// modifier and hand back a plaintext argument.
    #[test]
    fn job_args_reject_the_encrypted_modifier() {
        let tmp = project_with_main("fn main() {}\n");
        let err = plan_job(
            tmp.path(),
            "SendWelcomeEmail",
            &["api_token:String{encrypted}".into()],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("encrypted"), "must name the modifier: {msg}");
        assert!(
            msg.contains("generate model") || msg.contains("generate scaffold"),
            "must point at where the modifier belongs: {msg}"
        );
    }
}
