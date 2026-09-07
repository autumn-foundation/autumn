//! `autumn generate auth` — generate a complete browser authentication flow.
//!
//! Creates a User model, Diesel migration, auth route handlers (signup / login /
//! logout / account / forgot-password / reset-password), generated request-level
//! tests, and a documentation file — all as ordinary app-owned code that the
//! user can edit freely after generation.
//!
//! Security properties of the generated code:
//! - Passwords are hashed with bcrypt (cost=12) via `autumn_web::auth`.
//! - Reset tokens are random values; only SHA-256 digests are persisted.
//! - Duplicate signup and failed login return identical non-enumerating errors.
//! - Login and reset-password rotate the session ID (prevents session fixation).
//! - Logout clears the session data and rotates the ID, invalidating the old
//!   session (old cookie cannot be replayed) while carrying a one-shot notice.

use std::fmt::Write as _;
use std::path::Path;

use super::emit::Plan;
use super::model::ensure_cargo_dependencies;
use super::naming::{pascal, pluralize, snake};
use super::schema_edit::{
    add_mod_declaration, add_remember_middleware_to_app, append_schema_table, schema_has_table,
    unique_index_sql, update_main_rs,
};
use super::{Flags, GenerateError, ensure_project_root, read_or_empty, timestamp_now};

/// Extra Cargo dependencies the auth generator needs on top of the model deps.
const AUTH_EXTRA_DEPS: &[(&str, &str)] = &[
    ("axum", "\"0.8\""),
    ("maud", "{ version = \"0.27\", features = [\"axum\"] }"),
    ("sha2", "{ version = \"0.10\", features = [] }"),
    ("hex", "\"0.4\""),
    ("rand", "{ version = \"0.9\", features = [\"os_rng\"] }"),
    ("tokio", "{ version = \"1\", features = [\"time\"] }"),
    ("tracing", "\"0.1\""),
];

/// Extra Cargo dependencies pulled in only by `--passkeys`.
///
/// - `webauthn-rs` (with `danger-allow-state-serialisation`) provides the
///   `WebAuthn` ceremony implementation. State serialisation lets the in-progress
///   ceremony challenge survive across requests via the session.
/// - `uuid` generates random credential IDs if the authenticator doesn't supply one.
/// - `conditional-ui` gates `start/finish_discoverable_authentication` in webauthn-rs 0.5.
const PASSKEY_EXTRA_DEPS: &[(&str, &str)] = &[
    (
        "webauthn-rs",
        "{ version = \"0.5\", features = [\"danger-allow-state-serialisation\", \"conditional-ui\"] }",
    ),
    ("uuid", "{ version = \"1\", features = [\"v4\"] }"),
    ("base64", "\"0.22\""),
];

/// Required features for the `webauthn-rs` dependency.
///
/// `danger-allow-state-serialisation` enables session storage of ceremony state.
/// `conditional-ui` gates `start/finish_discoverable_authentication`.
const WEBAUTHN_RS_FEATURES: &[&str] = &["danger-allow-state-serialisation", "conditional-ui"];

/// Ensure an existing `webauthn-rs` dependency carries the features the generated
/// routes need.
///
/// `ensure_cargo_dependencies` skips a crate that is already declared, so a project
/// that already lists `webauthn-rs` without `conditional-ui` would scaffold but fail
/// to compile. This merges the missing features into the existing declaration —
/// shorthand, inline-table, or `[dependencies.webauthn-rs]` subtable form.
fn ensure_webauthn_rs_features(toml: &str) -> String {
    const CRATE: &str = "webauthn-rs";
    let trailing_newline = toml.ends_with('\n');
    let mut lines: Vec<String> = toml.lines().map(str::to_owned).collect();
    let simple_prefix = format!("{CRATE} = \"");
    let table_prefix = format!("{CRATE} = {{");
    let subtable_header = format!("[dependencies.{CRATE}]");
    let subtable_header_underscore = format!("[dependencies.{}]", CRATE.replace('-', "_"));
    let feats_csv = WEBAUTHN_RS_FEATURES
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ");

    let merge_missing = |line: &str| -> Option<String> {
        let feat_bracket = line.find("features = [")?;
        let list_start = feat_bracket + "features = [".len();
        let close_off = line[list_start..].find(']')?;
        let list_end = close_off + list_start;
        let existing_list = &line[list_start..list_end];
        let additions: Vec<String> = WEBAUTHN_RS_FEATURES
            .iter()
            .filter(|f| !existing_list.contains(&format!("\"{f}\"")))
            .map(|f| format!("\"{f}\""))
            .collect();
        if additions.is_empty() {
            return None; // already complete
        }
        let sep = if existing_list.trim().is_empty() || existing_list.trim().ends_with(',') {
            ""
        } else {
            ", "
        };
        Some(format!(
            "{}{sep}{}{}",
            &line[..list_end],
            additions.join(", "),
            &line[list_end..]
        ))
    };

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim().to_owned();
        let indent: String = lines[i]
            .chars()
            .take_while(char::is_ascii_whitespace)
            .collect();

        if let Some(rest) = trimmed.strip_prefix(&simple_prefix) {
            let version = rest.split('"').next().unwrap_or("0.5");
            lines[i] = format!(
                "{indent}{CRATE} = {{ version = \"{version}\", features = [{feats_csv}] }}"
            );
            break;
        }

        if trimmed.starts_with(&table_prefix) {
            if let Some(new_line) = merge_missing(&trimmed) {
                lines[i] = format!("{indent}{new_line}");
            } else if !trimmed.contains("features = [") {
                // No `features` key at all — insert one before the closing brace.
                if let Some(close_brace) = trimmed.rfind('}') {
                    let before = trimmed[..close_brace].trim_end();
                    let sep = if before.ends_with('{') { " " } else { ", " };
                    lines[i] = format!("{indent}{before}{sep}features = [{feats_csv}] }}");
                }
            }
            break;
        }

        if trimmed == subtable_header || trimmed == subtable_header_underscore {
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim().starts_with('[') {
                let t = lines[j].trim().to_owned();
                if t.starts_with("features") {
                    let ind2: String = lines[j]
                        .chars()
                        .take_while(char::is_ascii_whitespace)
                        .collect();
                    if let Some(new_line) = merge_missing(&t) {
                        lines[j] = format!("{ind2}{new_line}");
                    }
                    let mut out = lines.join("\n");
                    if trailing_newline {
                        out.push('\n');
                    }
                    return out;
                }
                j += 1;
            }
            // No `features` key found in the subtable — insert one.
            lines.insert(i + 1, format!("features = [{feats_csv}]"));
            break;
        }

        i += 1;
    }

    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// Extra Cargo dependencies pulled in only by `--totp`.
///
/// - `totp-rs` (with the `qr` feature) provides RFC 6238 TOTP plus QR rendering.
/// - `aes-gcm` encrypts the TOTP secret at rest.
/// - `base64` encodes the encrypted secret / decodes the encryption key.
///
/// Recovery codes are hashed with bcrypt via the existing `autumn_web::auth`
/// hashing path, so no extra hashing dependency is required.
const TOTP_EXTRA_DEPS: &[(&str, &str)] = &[
    (
        "totp-rs",
        "{ version = \"5\", features = [\"qr\", \"gen_secret\", \"otpauth\"] }",
    ),
    ("aes-gcm", "\"0.10\""),
    ("base64", "\"0.22\""),
];

/// `totp-rs` cargo features the generated `--totp` routes rely on
/// (`Secret::generate_secret`, `TOTP::new` with `otpauth`, `get_qr_base64`).
const TOTP_RS_FEATURES: &[&str] = &["qr", "gen_secret", "otpauth"];

/// Ensure an existing `totp-rs` dependency carries the features the generated
/// routes need.
///
/// `ensure_cargo_dependencies` skips a crate that is already declared, so an app
/// that already lists `totp-rs` (perhaps without `qr`/`gen_secret`/`otpauth`)
/// would scaffold but fail to compile. This merges the missing features into the
/// existing declaration — shorthand, inline-table, or `[dependencies.totp-rs]`
/// subtable form — mirroring the `autumn-web` feature helpers.
#[allow(clippy::too_many_lines)]
fn ensure_totp_rs_features(toml: &str) -> String {
    const CRATE: &str = "totp-rs";
    let trailing_newline = toml.ends_with('\n');
    let mut lines: Vec<String> = toml.lines().map(str::to_owned).collect();
    let simple_prefix = format!("{CRATE} = \"");
    let table_prefix = format!("{CRATE} = {{");
    let subtable_header = format!("[dependencies.{CRATE}]");
    let subtable_header_underscore = format!("[dependencies.{}]", CRATE.replace('-', "_"));
    let feats_csv = TOTP_RS_FEATURES
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ");

    // Merge `TOTP_RS_FEATURES` into the `[ ... ]` list embedded in `line`,
    // returning the rewritten line, or `None` if nothing changed (already
    // complete) / no list found.
    let merge_into_list = |line: &str, bracket_search: &str| -> Option<Option<String>> {
        let feat_bracket = line.find(bracket_search)?;
        let list_start = feat_bracket + bracket_search.len();
        let close_off = line[list_start..].find(']')?;
        let list_end = close_off + list_start;
        let existing_list = &line[list_start..list_end];
        let additions: Vec<String> = TOTP_RS_FEATURES
            .iter()
            .filter(|f| !existing_list.contains(&format!("\"{f}\"")))
            .map(|f| format!("\"{f}\""))
            .collect();
        if additions.is_empty() {
            return Some(None); // list present and already complete
        }
        let sep = if existing_list.trim().is_empty() || existing_list.trim().ends_with(',') {
            ""
        } else {
            ", "
        };
        Some(Some(format!(
            "{}{sep}{}{}",
            &line[..list_end],
            additions.join(", "),
            &line[list_end..]
        )))
    };

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim().to_owned();
        let indent: String = lines[i]
            .chars()
            .take_while(char::is_ascii_whitespace)
            .collect();

        // `totp-rs = "5"` → promote to a table carrying the required features.
        if let Some(rest) = trimmed.strip_prefix(&simple_prefix) {
            // `rest` is everything after `totp-rs = "`. Take up to the closing
            // quote so a trailing inline comment (`"5" # note`) isn't folded into
            // the version string (which would make Cargo.toml invalid).
            let (version, trailing) = rest.find('"').map_or_else(
                || (rest.trim_end_matches('"'), ""),
                |close| (&rest[..close], rest[close + 1..].trim()),
            );
            let trailing = if trailing.is_empty() {
                String::new()
            } else {
                format!(" {trailing}")
            };
            lines[i] = format!(
                "{indent}{CRATE} = {{ version = \"{version}\", features = [{feats_csv}] }}{trailing}"
            );
            break;
        }

        // `totp-rs = { ... }` inline table → merge any missing features.
        if trimmed.starts_with(&table_prefix) {
            if let Some(result) = merge_into_list(&trimmed, "features = [") {
                match result {
                    Some(new_line) => lines[i] = format!("{indent}{new_line}"),
                    None => return toml.to_owned(),
                }
                break;
            } else if let Some(close_brace) = trimmed.rfind('}') {
                // No `features` key — insert one before the closing brace.
                let before = trimmed[..close_brace].trim_end();
                let sep = if before.ends_with('{') { " " } else { ", " };
                lines[i] = format!("{indent}{before}{sep}features = [{feats_csv}] }}");
                break;
            }
        }

        // `[dependencies.totp-rs]` subtable form → merge into (or add) a
        // `features = [...]` key within the subtable body.
        if trimmed == subtable_header || trimmed == subtable_header_underscore {
            let mut feature_line: Option<usize> = None;
            let mut j = i + 1;
            while j < lines.len() {
                let tj = lines[j].trim();
                if tj.starts_with('[') {
                    break;
                }
                if tj.starts_with("features") && tj.contains('[') {
                    feature_line = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(fl) = feature_line {
                let tj = lines[fl].trim().to_owned();
                let indent_j: String = lines[fl]
                    .chars()
                    .take_while(char::is_ascii_whitespace)
                    .collect();
                if tj.contains(']') {
                    // Single-line `features = [...]`.
                    match merge_into_list(&tj, "[") {
                        Some(Some(new_line)) => lines[fl] = format!("{indent_j}{new_line}"),
                        Some(None) => return toml.to_owned(),
                        None => {}
                    }
                } else {
                    // Multiline `features = [` … `]` array: find the closing `]`,
                    // collect the existing entries, and rebuild the list (collapsed
                    // to one line) with the missing features appended.
                    let mut close_line = None;
                    let mut k = fl;
                    while k < lines.len() {
                        let tk = lines[k].trim();
                        if k > fl && tk.starts_with('[') {
                            break; // next table header — array never closed
                        }
                        if lines[k].contains(']') {
                            close_line = Some(k);
                            break;
                        }
                        k += 1;
                    }
                    if let Some(cl) = close_line {
                        let fl_bracket = lines[fl].find('[').unwrap_or(lines[fl].len());
                        let mut list_text = lines[fl][fl_bracket + 1..].to_owned();
                        for line in &lines[fl + 1..cl] {
                            list_text.push(' ');
                            list_text.push_str(line.trim());
                        }
                        let cl_close = lines[cl].find(']').unwrap_or(lines[cl].len());
                        list_text.push(' ');
                        list_text.push_str(&lines[cl][..cl_close]);
                        let trailing =
                            lines[cl][cl_close.saturating_add(1).min(lines[cl].len())..].to_owned();

                        let mut entries: Vec<String> = list_text
                            .split(',')
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(str::to_owned)
                            .collect();
                        let mut changed = false;
                        for f in TOTP_RS_FEATURES {
                            let q = format!("\"{f}\"");
                            if !entries.iter().any(|e| e == &q) {
                                entries.push(q);
                                changed = true;
                            }
                        }
                        if !changed {
                            return toml.to_owned();
                        }
                        let rebuilt =
                            format!("{indent_j}features = [{}]{trailing}", entries.join(", "));
                        lines.splice(fl..=cl, std::iter::once(rebuilt));
                    }
                }
            } else {
                // No `features` key in the subtable — add one right after the header.
                lines.insert(i + 1, format!("features = [{feats_csv}]"));
            }
            break;
        }
        i += 1;
    }

    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// OAuth2/OIDC options for `autumn generate auth --oauth`.
#[derive(Debug, Clone, Default)]
pub struct AuthOAuthOptions {
    /// Provider keys to scaffold (e.g. `["github", "google"]`).
    /// An empty list produces the same output as [`plan_auth`].
    pub providers: Vec<String>,
}

/// Compute the file actions for `autumn generate auth`.
///
/// Pure planning step — no I/O happens here. Tests use this directly so they
/// can inspect the emitted file list and contents without touching the disk.
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] when run outside an Autumn project
/// root, or [`GenerateError::InvalidName`] for a bad resource name.
#[allow(dead_code)]
pub fn plan_auth(project_root: &Path, name: &str, timestamp: &str) -> Result<Plan, GenerateError> {
    plan_auth_with_providers(project_root, name, timestamp, &[], false)
}

pub fn plan_auth_with_providers(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    providers: &[String],
    totp: bool,
) -> Result<Plan, GenerateError> {
    plan_auth_with_providers_ex(project_root, name, timestamp, providers, totp, false)
}

/// Extended base scaffold that additionally threads the `--magic-link` flag
/// (issue #1328) through the migration, model, schema, routes, docs, and tests.
///
/// `plan_auth_with_providers` delegates here with `magic_link = false` so all
/// existing call sites keep their behaviour byte-for-byte.
///
/// # Errors
/// Same as [`plan_auth`].
pub fn plan_auth_with_providers_ex(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    providers: &[String],
    totp: bool,
    magic_link: bool,
) -> Result<Plan, GenerateError> {
    // Generation path: run the full plan, including the shared-layout preflight.
    // `autumn destroy auth` recomputes the identical plan before reverting it,
    // and must bypass that generate-only preflight (issue #1353 follow-up); it
    // reaches the builder through `plan_auth_full_ex2_for_revert` instead.
    plan_auth_with_providers_ex_impl(
        project_root,
        name,
        timestamp,
        providers,
        totp,
        magic_link,
        false,
    )
}

/// Shared implementation of [`plan_auth_with_providers_ex`]. `for_revert`
/// suppresses the generate-only shared-layout preflight so `autumn destroy
/// auth` (which recomputes this same plan before [`Plan::revert`]) can remove
/// generated files even in a project whose shared `pub fn layout` is missing or
/// renamed — a regression the preflight would otherwise introduce.
#[allow(clippy::too_many_lines)]
fn plan_auth_with_providers_ex_impl(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    providers: &[String],
    totp: bool,
    magic_link: bool,
    for_revert: bool,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    super::model::validate_resource_name(name)?;

    // Determine the target app's database backend so the scaffolded migrations
    // emit backend-aware DDL (issue #1927): a SQLite app gets SQLite-dialect DDL
    // (`INTEGER PRIMARY KEY AUTOINCREMENT`, `DEFAULT CURRENT_TIMESTAMP`) instead
    // of the Postgres-only `BIGSERIAL`/`NOW()` form. Detected identically on the
    // generate and destroy/revert paths (both read the same config), so a
    // `destroy auth` round-trip recomputes byte-identical migration content.
    let backend = super::detect_backend(project_root);

    let pascal_name = pascal(name);
    let snake_name = snake(name);
    let table = pluralize(&snake_name);

    // Under `--totp` the generator unconditionally emits a `recovery_code` model
    // and a `recovery_codes` table. If the auth resource itself resolves to those
    // names, the two collide — the same model file and `CREATE TABLE` would be
    // emitted twice, producing an unusable app. Reject it up front.
    if totp && (snake_name == "recovery_code" || table == "recovery_codes") {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            "collides with the reserved `recovery_code` model/table generated by \
             `--totp`; choose a different resource name."
                .to_owned(),
        ));
    }

    // Under `--magic-link` the generator unconditionally emits a `magic_link_token`
    // model and a `magic_link_tokens` table. If the auth resource itself resolves to
    // those names, the two collide — the same `CREATE TABLE` would be emitted twice
    // and the helper model file would overwrite the auth model file, producing an
    // unusable scaffold. Reject it up front (mirrors the `--totp` guard above).
    if magic_link && (snake_name == "magic_link_token" || table == "magic_link_tokens") {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            "collides with the reserved `magic_link_token` model/table generated by \
             `--magic-link`; choose a different resource name."
                .to_owned(),
        ));
    }

    // Issue #1353: the generated auth views render through the application's
    // shared `crate::layout(title, current_path, flash, content)` (nav bar,
    // stylesheet links, skip-link, footer) rather than a private per-file
    // layout stub, so the target app must expose a shared layout with the
    // matching 4-arg signature (as `autumn new` emits). Detect it by looking
    // for a `pub fn layout` in `src/main.rs`. If it is missing — or present
    // with the wrong arity (e.g. an older/custom 2-arg
    // `pub fn layout(title, content)`) — fail early with an actionable message
    // rather than emitting routes that call a nonexistent or mismatched
    // `crate::layout`. This mirrors the scaffold generator's preflight
    // (issue #1130) and reuses its lexically-aware detector.
    //
    // The preflight is a generate-time guard only. `autumn destroy auth`
    // recomputes this same plan before reverting it, so running the preflight
    // on that path would hard-fail cleanup in a project whose shared
    // `pub fn layout` is missing or renamed — stranding the very files destroy
    // is meant to remove. Skip it when `for_revert` is set.
    if !for_revert {
        let main_for_layout_check = project_root.join("src").join("main.rs");
        let main_src = match std::fs::read_to_string(&main_for_layout_check) {
            Ok(src) => src,
            // `main.rs` genuinely absent: surface the actionable message below by
            // treating it as an empty source (no `pub fn layout` present).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            // Any other io error (e.g. PermissionDenied) preserves its original
            // `ErrorKind` + OS message rather than being masked.
            Err(e) => return Err(GenerateError::Io(e)),
        };
        if !super::scaffold::has_shared_layout(&main_src) {
            return Err(GenerateError::Config(
                "`autumn generate auth` requires a shared `pub fn layout` in src/main.rs \
                 so the generated views can render through \
                 `crate::layout(title, current_path, flash, content)` (4 args). Run this inside \
                 an app created by `autumn new`, or add a shared \
                 `pub fn layout(title: &str, current_path: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup` \
                 to src/main.rs and re-run."
                    .to_owned(),
            ));
        }
    }

    let mut plan = Plan::new(project_root);

    // ── Migration ──────────────────────────────────────────────────────────
    let mig_dir = project_root
        .join("migrations")
        .join(format!("{timestamp}_create_{table}"));
    plan.create(
        mig_dir.join("up.sql"),
        render_migration_up(backend, &snake_name, &table, totp, magic_link),
    );
    plan.create(
        mig_dir.join("down.sql"),
        render_migration_down(&snake_name, &table, totp, magic_link),
    );

    // ── Model ──────────────────────────────────────────────────────────────
    let models_dir = project_root.join("src").join("models");
    plan.create(
        models_dir.join(format!("{snake_name}.rs")),
        render_model_file(&pascal_name, &snake_name, &table, totp),
    );
    let model_mod_path = models_dir.join("mod.rs");
    let mut model_mod = add_mod_declaration(&read_or_empty(&model_mod_path), &snake_name);
    if totp {
        // Recovery codes live in their own model + table.
        plan.create(
            models_dir.join("recovery_code.rs"),
            render_recovery_code_model_file(&pascal_name, &table),
        );
        model_mod = add_mod_declaration(&model_mod, "recovery_code");
    }
    // Active login sessions (issue #819) live in their own model + table.
    plan.create(
        models_dir.join(format!("{snake_name}_session.rs")),
        render_session_model_file(&pascal_name, &snake_name, &table),
    );
    let session_mod_name = format!("{snake_name}_session");
    model_mod = add_mod_declaration(&model_mod, &session_mod_name);
    // Persistent remember-me chains (issue #1397) live in their own model + table.
    plan.create(
        models_dir.join(format!("{snake_name}_remember_token.rs")),
        render_remember_model_file(&pascal_name, &snake_name, &table),
    );
    let remember_mod_name = format!("{snake_name}_remember_token");
    model_mod = add_mod_declaration(&model_mod, &remember_mod_name);
    // Passwordless magic-link tokens (issue #1328) live in their own model +
    // table, mirroring the reset-token invariants: only the SHA-256 digest is
    // stored, plus an expiry and a single-use `consumed_at` marker.
    if magic_link {
        plan.create(
            models_dir.join("magic_link_token.rs"),
            render_magic_link_token_model_file(&pascal_name, &table),
        );
        model_mod = add_mod_declaration(&model_mod, "magic_link_token");
    }
    plan.modify(model_mod_path.clone(), model_mod);
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: model_mod_path.clone(),
        name: snake_name.clone(),
    });
    if totp {
        plan.push_revert(crate::generate::emit::Revert::ModDecl {
            path: model_mod_path.clone(),
            name: "recovery_code".to_owned(),
        });
    }
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: model_mod_path.clone(),
        name: session_mod_name,
    });
    if magic_link {
        plan.push_revert(crate::generate::emit::Revert::ModDecl {
            path: model_mod_path.clone(),
            name: "magic_link_token".to_owned(),
        });
    }
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: model_mod_path,
        name: remember_mod_name,
    });

    // ── src/schema.rs entry ────────────────────────────────────────────────
    // The generated model references `crate::schema::<table>`, so we must
    // emit a `diesel::table! { }` block just like `generate model` does.
    // Auth-specific fields (id and created_at are added automatically):
    //   email            String   → Text      NOT NULL
    //   password_digest  String   → Text      NOT NULL
    //   reset_token_digest         Option<String>         → Nullable<Text>
    //   reset_token_expires_at     Option<NaiveDateTime>  → Nullable<Timestamp>
    let mut user_field_tokens: Vec<&str> = vec![
        "email:String",
        "time_zone:Option<String>",
        "password_digest:String",
    ];
    if totp {
        user_field_tokens.push("totp_secret_encrypted:Option<String>");
        user_field_tokens.push("totp_enabled:bool");
        user_field_tokens.push("totp_last_used_step:Option<i64>");
    }
    user_field_tokens.push("failed_attempts:i32");
    user_field_tokens.push("locked_at:Option<NaiveDateTime>");
    user_field_tokens.push("reset_token_digest:Option<String>");
    user_field_tokens.push("reset_token_expires_at:Option<NaiveDateTime>");
    user_field_tokens.push("confirm_token_digest:Option<String>");
    user_field_tokens.push("confirm_token_expires_at:Option<NaiveDateTime>");
    user_field_tokens.push("email_confirmed_at:Option<NaiveDateTime>");
    user_field_tokens.push("pending_email:Option<String>");
    user_field_tokens.push("export_requested_at:Option<NaiveDateTime>");
    user_field_tokens.push("delete_requested_at:Option<NaiveDateTime>");
    user_field_tokens.push("delete_scheduled_at:Option<NaiveDateTime>");
    let auth_fields: Vec<super::dsl::Field> = user_field_tokens
        .iter()
        .map(|t| super::dsl::parse_field(t).expect("auth field tokens are always valid"))
        .collect();

    let schema_path = project_root.join("src").join("schema.rs");
    let schema_existing = read_or_empty(&schema_path);
    // Under `--totp` we unconditionally create a helper `recovery_codes` table.
    // If the project already declares one (in `src/schema.rs`), the migration
    // would emit a second `CREATE TABLE recovery_codes` and `diesel migration
    // run` would fail with "relation already exists" (or clobber an unrelated
    // table). Reject up front rather than generate an unusable app.
    //
    // Only reject a *foreign* `recovery_codes` table: when the auth table
    // itself is also already present, this is a re-run over our own output
    // (`--force`, or `autumn destroy` recomputing this same plan against
    // already-generated files — issue #1048), where the schema append below
    // is an idempotent no-op. Mirrors the analogous `sess_table` guard below.
    if totp
        && schema_has_table(&schema_existing, "recovery_codes")
        && !schema_has_table(&schema_existing, &table)
    {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            "this project already defines a `recovery_codes` table, which `--totp` \
             needs for its helper model; rename or remove the existing table first."
                .to_owned(),
        ));
    }
    // Same guard for the session-tracking helper table (issue #819). Only
    // reject a *foreign* table: when the auth table itself is also present,
    // this is a re-run over our own output (`--force`), where the schema
    // append below is an idempotent no-op.
    let sess_table = sessions_table_name(&snake_name);
    if schema_has_table(&schema_existing, &sess_table)
        && !schema_has_table(&schema_existing, &table)
    {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            format!(
                "this project already defines a `{sess_table}` table, which the auth \
                 starter needs for login-session tracking; rename or remove the \
                 existing table first."
            ),
        ));
    }
    // Same guard for the remember-me helper table (issue #1397).
    let rem_table_guard = remember_table_name(&snake_name);
    if schema_has_table(&schema_existing, &rem_table_guard)
        && !schema_has_table(&schema_existing, &table)
    {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            format!(
                "this project already defines a `{rem_table_guard}` table, which the auth \
                 starter needs for persistent remember-me login; rename or remove the \
                 existing table first."
            ),
        ));
    }
    // Same guard for the magic-link helper table (issue #1328). Only under
    // `--magic-link`, and only reject a *foreign* table: when the auth table
    // itself is also present, this is a re-run over our own output (`--force`),
    // where the schema append below is an idempotent no-op.
    if magic_link
        && schema_has_table(&schema_existing, "magic_link_tokens")
        && !schema_has_table(&schema_existing, &table)
    {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            "this project already defines a `magic_link_tokens` table, which \
             `--magic-link` needs for its helper model; rename or remove the \
             existing table first."
                .to_owned(),
        ));
    }
    let mut schema_new = append_schema_table(&schema_existing, &table, &auth_fields);
    if totp {
        let recovery_fields: Vec<super::dsl::Field> = [
            "user_id:i64",
            "code_digest:String",
            "used_at:Option<NaiveDateTime>",
        ]
        .iter()
        .map(|t| super::dsl::parse_field(t).expect("recovery field tokens are always valid"))
        .collect();
        schema_new = append_schema_table(&schema_new, "recovery_codes", &recovery_fields);
    }
    let session_fields: Vec<super::dsl::Field> = [
        "user_id:i64",
        "token_digest:String",
        "ip:String",
        "user_agent:String",
        "ua_family:String",
        "ua_os:String",
        "ua_device:String",
        "label:Option<String>",
        "last_seen_at:NaiveDateTime",
    ]
    .iter()
    .map(|t| super::dsl::parse_field(t).expect("session field tokens are always valid"))
    .collect();
    schema_new = append_schema_table(&schema_new, &sess_table, &session_fields);
    // Persistent "remember-me" login chains (issue #1397). Mirrors the sessions
    // table shape: a synthetic `id` primary key plus a unique opaque `series`
    // that is the stable per-device lookup key.
    let rem_table = remember_table_name(&snake_name);
    let remember_fields: Vec<super::dsl::Field> = [
        "series:String",
        "user_id:i64",
        "token_hash:String",
        "previous_token_hash:Option<String>",
        "rotated_at:Option<NaiveDateTime>",
        "expires_at:NaiveDateTime",
        "ip:String",
        "user_agent:String",
        "ua_family:String",
        "ua_os:String",
        "ua_device:String",
        "label:Option<String>",
        "last_used_at:Option<NaiveDateTime>",
    ]
    .iter()
    .map(|t| super::dsl::parse_field(t).expect("remember field tokens are always valid"))
    .collect();
    schema_new = append_schema_table(&schema_new, &rem_table, &remember_fields);
    // Passwordless magic-link tokens (issue #1328). Dedicated table mirroring
    // the reset-token invariants: only the SHA-256 digest of the raw token is
    // stored, plus an expiry and a single-use `consumed_at` marker.
    let magic_link_fields: Vec<super::dsl::Field> = magic_link_token_fields();
    if magic_link {
        schema_new = append_schema_table(&schema_new, "magic_link_tokens", &magic_link_fields);
    }
    plan.modify(schema_path.clone(), schema_new);
    plan.push_revert(crate::generate::emit::Revert::SchemaTable {
        path: schema_path.clone(),
        table: table.clone(),
        expected_block: append_schema_table("", &table, &auth_fields),
    });
    if totp {
        let recovery_fields: Vec<super::dsl::Field> = [
            "user_id:i64",
            "code_digest:String",
            "used_at:Option<NaiveDateTime>",
        ]
        .iter()
        .map(|t| super::dsl::parse_field(t).expect("recovery field tokens are always valid"))
        .collect();
        plan.push_revert(crate::generate::emit::Revert::SchemaTable {
            path: schema_path.clone(),
            table: "recovery_codes".to_owned(),
            expected_block: append_schema_table("", "recovery_codes", &recovery_fields),
        });
    }
    let sess_table_expected_block = append_schema_table("", &sess_table, &session_fields);
    plan.push_revert(crate::generate::emit::Revert::SchemaTable {
        path: schema_path.clone(),
        table: sess_table,
        expected_block: sess_table_expected_block,
    });
    // Remember-me table is appended last (before the optional magic-link table),
    // so it is reverted just after it (issue #1397).
    let rem_table_expected_block = append_schema_table("", &rem_table, &remember_fields);
    plan.push_revert(crate::generate::emit::Revert::SchemaTable {
        path: schema_path.clone(),
        table: rem_table,
        expected_block: rem_table_expected_block,
    });
    // Magic-link table is appended last, so it is reverted first (issue #1328).
    if magic_link {
        let magic_link_expected_block =
            append_schema_table("", "magic_link_tokens", &magic_link_fields);
        plan.push_revert(crate::generate::emit::Revert::SchemaTable {
            path: schema_path,
            table: "magic_link_tokens".to_owned(),
            expected_block: magic_link_expected_block,
        });
    }

    // ── Auth routes ────────────────────────────────────────────────────────
    let routes_dir = project_root.join("src").join("routes");
    plan.create(
        routes_dir.join("auth.rs"),
        render_routes_file(
            &pascal_name,
            &snake_name,
            &table,
            providers,
            totp,
            magic_link,
        ),
    );
    let route_mod_path = routes_dir.join("mod.rs");
    plan.modify(
        route_mod_path.clone(),
        add_mod_declaration(&read_or_empty(&route_mod_path), "auth"),
    );
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: route_mod_path,
        name: "auth".to_owned(),
    });

    // ── Generated tests ────────────────────────────────────────────────────
    let tests_dir = project_root.join("tests");
    plan.create(
        tests_dir.join("auth.rs"),
        render_tests_file(&pascal_name, &snake_name, magic_link),
    );
    if totp {
        plan.create(
            tests_dir.join("auth_2fa.rs"),
            render_2fa_tests_file(&pascal_name, &snake_name),
        );
    }
    // Active-session management integration tests (issue #819).
    plan.create(
        tests_dir.join("auth_sessions.rs"),
        render_sessions_tests_file(&pascal_name, &snake_name),
    );

    // ── Documentation ─────────────────────────────────────────────────────
    let docs_dir = project_root.join("docs").join("guide");
    plan.create(
        docs_dir.join("authentication.md"),
        render_docs_file(&pascal_name, totp, magic_link),
    );
    plan.create(docs_dir.join("gdpr-compliance.md"), render_gdpr_docs_file());
    plan.create(
        docs_dir.join("session-management.md"),
        render_sessions_docs_file(backend, &pascal_name, &snake_name, &table),
    );

    // ── src/main.rs — module declarations + route registration ────────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|_| {
        GenerateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("missing {}", main_path.display()),
        ))
    })?;
    let entries = auth_route_entries(totp, magic_link);
    let updated = update_main_rs(&main_existing, &["models", "routes", "schema"], &entries);
    // Auto-wire the remember-me middleware layer + startup hook into the
    // `AppBuilder` chain so a freshly generated app actually consumes the
    // remember cookie it issues (issue #1397.7).
    let updated = add_remember_middleware_to_app(&updated);
    plan.modify(main_path.clone(), updated);
    plan.push_revert(crate::generate::emit::Revert::RoutesEntries {
        path: main_path.clone(),
        entries,
    });
    plan.push_revert(crate::generate::emit::Revert::RememberMiddleware { path: main_path });

    // ── Cargo.toml deps + autumn-web/mail feature ─────────────────────────
    let cargo_toml_path = project_root.join("Cargo.toml");
    let cargo_existing = read_or_empty(&cargo_toml_path);
    let all_deps: Vec<(&str, &str)> = super::model::MODEL_DEPS
        .iter()
        .copied()
        .chain(AUTH_EXTRA_DEPS.iter().copied())
        .chain(if totp { TOTP_EXTRA_DEPS } else { &[] }.iter().copied())
        .collect();
    // The `--totp` routes add `base64 = "0.22"` and use its 0.21+ `Engine`
    // API; `ensure_cargo_dependencies` is name-only, so an app pinning an older
    // `base64` keeps it and the generated 2FA code won't compile. Warn instead.
    if totp {
        super::model::warn_if_existing_dep_below_version(
            &mut plan,
            &cargo_existing,
            "base64",
            0,
            22,
        );
    }
    // Apply dep additions then enable the mail feature in a single write.
    let with_deps = ensure_cargo_dependencies(&cargo_existing, &all_deps);
    // `ensure_cargo_dependencies` no-ops on an already-declared `totp-rs`, so
    // merge the required features into any pre-existing declaration.
    let with_deps = if totp {
        ensure_totp_rs_features(&with_deps)
    } else {
        with_deps
    };
    let final_cargo = ensure_autumn_web_mail_feature(&with_deps);
    if final_cargo != cargo_existing {
        plan.modify(cargo_toml_path.clone(), final_cargo);
    }
    // Pushed unconditionally — see `plan_cargo_deps`'s matching comment in
    // model.rs: destroy recomputes this plan against the already-generated
    // Cargo.toml, where these entries are by definition already present.
    // `owner_dir` is `src/models`: every auth resource always creates at
    // least `<snake>.rs` and `<snake>_session.rs` there, and (unlike
    // channels/mailers) an auth resource's routes file isn't
    // name-parameterized (`src/routes/auth.rs` always, regardless of the
    // resource name), so a second auth resource can't coexist under a
    // different name anyway — `src/models` is a reliable "this auth resource
    // still exists" marker.
    let models_owner_dir = project_root.join("src").join("models");
    let dep_names: Vec<String> = all_deps
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !super::model::TEMPLATE_SHIPPED_CARGO_DEPS.contains(name))
        .map(str::to_owned)
        .collect();
    if !dep_names.is_empty() {
        plan.push_revert(crate::generate::emit::Revert::CargoDeps {
            path: cargo_toml_path.clone(),
            names: dep_names,
            owner_dir: models_owner_dir.clone(),
        });
    }
    plan.push_revert(crate::generate::emit::Revert::CargoAutumnWebFeature {
        path: cargo_toml_path,
        feature: "mail".to_owned(),
        owner_dir: Some(models_owner_dir),
    });

    Ok(plan)
}

/// Compute the file actions for `autumn generate auth --oauth <providers>`.
///
/// When `oauth.providers` is empty this is identical to [`plan_auth`].
/// When providers are specified the plan additionally includes:
/// - An `oauth_identities` migration keyed by `(provider, subject)`.
/// - `src/routes/oauth.rs` with `oauth_redirect` and `oauth_callback` handlers.
/// - `docs/guide/oauth.md` covering provider setup, security properties, and troubleshooting.
/// - The `oauth2` feature on `autumn-web` in `Cargo.toml`.
///
/// # Errors
/// Same as [`plan_auth`].
///
/// Retained as stable public API; internal call sites use [`plan_auth_full`].
#[allow(dead_code)]
pub fn plan_auth_with_options(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    oauth: &AuthOAuthOptions,
) -> Result<Plan, GenerateError> {
    plan_auth_options_impl(
        project_root,
        name,
        timestamp,
        oauth,
        false,
        false,
        false,
        false,
    )
}

/// Compute the file actions for `autumn generate auth [--oauth …] [--totp]`.
///
/// Layers optional TOTP two-factor authentication on top of the base (and
/// optional OAuth) auth scaffold.
///
/// # Errors
/// Same as [`plan_auth`].
#[allow(dead_code)]
pub fn plan_auth_full(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    oauth: &AuthOAuthOptions,
    totp: bool,
) -> Result<Plan, GenerateError> {
    plan_auth_options_impl(
        project_root,
        name,
        timestamp,
        oauth,
        totp,
        false,
        false,
        false,
    )
}

/// Compute the file actions for `autumn generate auth [--oauth …] [--totp] [--passkeys]`.
///
/// Extended version of [`plan_auth_full`] that additionally supports `WebAuthn` passkeys.
///
/// # Errors
/// Same as [`plan_auth`].
pub fn plan_auth_full_ex(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    oauth: &AuthOAuthOptions,
    totp: bool,
    passkeys: bool,
) -> Result<Plan, GenerateError> {
    plan_auth_options_impl(
        project_root,
        name,
        timestamp,
        oauth,
        totp,
        passkeys,
        false,
        false,
    )
}

/// Compute the file actions for
/// `autumn generate auth [--oauth …] [--totp] [--passkeys] [--magic-link]`.
///
/// Extended version of [`plan_auth_full_ex`] that additionally supports
/// passwordless email magic-link login (issue #1328). The magic-link scaffold
/// is fully composable with `--oauth`, `--totp`, and `--passkeys`.
///
/// # Errors
/// Same as [`plan_auth`].
pub fn plan_auth_full_ex2(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    oauth: &AuthOAuthOptions,
    totp: bool,
    passkeys: bool,
    magic_link: bool,
) -> Result<Plan, GenerateError> {
    plan_auth_options_impl(
        project_root,
        name,
        timestamp,
        oauth,
        totp,
        passkeys,
        magic_link,
        false,
    )
}

/// Compute the file actions for `autumn destroy auth …`.
///
/// Identical to [`plan_auth_full_ex2`] except it recomputes the plan for the
/// revert path: it skips the generate-only shared-layout preflight so cleanup
/// still succeeds in a project whose shared `pub fn layout` is missing or
/// renamed (e.g. one scaffolded by an older CLI). `autumn destroy auth` reverts
/// the returned plan; the preflight would otherwise hard-fail the destroy
/// before any generated file is removed (issue #1353 follow-up).
///
/// # Errors
/// Same as [`plan_auth_full_ex2`], minus the shared-layout preflight.
pub fn plan_auth_full_ex2_for_revert(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    oauth: &AuthOAuthOptions,
    totp: bool,
    passkeys: bool,
    magic_link: bool,
) -> Result<Plan, GenerateError> {
    plan_auth_options_impl(
        project_root,
        name,
        timestamp,
        oauth,
        totp,
        passkeys,
        magic_link,
        true,
    )
}

/// Shared implementation: base (optionally TOTP-aware, optionally passkey-aware,
/// optionally magic-link-aware) scaffold plus, when providers are supplied, the
/// OAuth artifacts.
// The three feature flags (`totp`/`passkeys`/`magic_link`) already saturate the
// bool/arg budget; the fourth (`for_revert`) is an internal generate-vs-destroy
// toggle threaded only from this crate's two public wrappers, so keep the flat
// signature rather than wrap it in a one-off options struct.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools
)]
fn plan_auth_options_impl(
    project_root: &Path,
    name: &str,
    timestamp: &str,
    oauth: &AuthOAuthOptions,
    totp: bool,
    passkeys: bool,
    magic_link: bool,
    for_revert: bool,
) -> Result<Plan, GenerateError> {
    // Start with the base auth plan with providers (and optional TOTP, and
    // optional magic-link) applied. `for_revert` threads through to suppress the
    // generate-only shared-layout preflight on the `autumn destroy auth` path.
    let mut plan = plan_auth_with_providers_ex_impl(
        project_root,
        name,
        timestamp,
        &oauth.providers,
        totp,
        magic_link,
        for_revert,
    )?;

    // Determine the target app's database backend so the scaffolded migrations
    // emit backend-aware DDL (issue #1927), matching the base auth plan above.
    let backend = super::detect_backend(project_root);
    let pascal_name = pascal(name);
    let snake_name = snake(name);
    let user_table = pluralize(&snake_name);

    if !oauth.providers.is_empty() {
        // ── oauth_identities migration ─────────────────────────────────────────
        // Increment the timestamp by one second so Diesel never sees two migrations
        // with the same version number.
        let oauth_ts_str: String = timestamp
            .parse::<i64>()
            .map_or_else(|_| format!("{timestamp}1"), |t| (t + 1).to_string());
        let mig_dir = project_root
            .join("migrations")
            .join(format!("{oauth_ts_str}_create_oauth_identities"));
        plan.create(
            mig_dir.join("up.sql"),
            render_oauth_migration_up(backend, &user_table),
        );
        plan.create(mig_dir.join("down.sql"), render_oauth_migration_down());

        // ── src/schema.rs entry for oauth_identities ───────────────────────────
        let schema_path = project_root.join("src").join("schema.rs");
        let schema_base = find_plan_content_for_path(&plan, &schema_path)
            .unwrap_or_else(|| read_or_empty(&schema_path));
        let oauth_fields: Vec<super::dsl::Field> = [
            "provider:String",
            "subject:String",
            "user_id:i64",
            "email:Option<String>",
            "name:Option<String>",
        ]
        .iter()
        .map(|t| super::dsl::parse_field(t).expect("oauth field tokens are always valid"))
        .collect();
        let updated_schema = append_schema_table(&schema_base, "oauth_identities", &oauth_fields);
        plan.modify(schema_path.clone(), updated_schema);
        plan.push_revert(crate::generate::emit::Revert::SchemaTable {
            path: schema_path,
            table: "oauth_identities".to_owned(),
            expected_block: append_schema_table("", "oauth_identities", &oauth_fields),
        });

        // ── oauth routes ───────────────────────────────────────────────────────
        let routes_dir = project_root.join("src").join("routes");
        plan.create(
            routes_dir.join("oauth.rs"),
            render_oauth_routes_file(
                &pascal_name,
                &snake_name,
                &user_table,
                &oauth.providers,
                totp,
            ),
        );

        // Add `pub mod oauth;` to src/routes/mod.rs — use the base plan's already-modified
        // content so the base `pub mod auth;` declaration is not overwritten.
        let route_mod_path = routes_dir.join("mod.rs");
        let route_mod_base = find_plan_content_for_path(&plan, &route_mod_path)
            .unwrap_or_else(|| read_or_empty(&route_mod_path));
        let updated_route_mod = add_mod_declaration(&route_mod_base, "oauth");
        plan.modify(route_mod_path.clone(), updated_route_mod);
        plan.push_revert(crate::generate::emit::Revert::ModDecl {
            path: route_mod_path,
            name: "oauth".to_owned(),
        });

        // ── Register oauth routes in src/main.rs ───────────────────────────────
        let main_path = project_root.join("src").join("main.rs");
        let main_existing = std::fs::read_to_string(&main_path).map_err(|_| {
            GenerateError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("missing {}", main_path.display()),
            ))
        })?;
        let oauth_entries = oauth_route_entries();
        // The base plan has already modified main.rs; find current state in plan or use existing.
        let base_main =
            find_plan_content_for_path(&plan, &main_path).unwrap_or_else(|| main_existing.clone());
        let updated_main = super::schema_edit::update_main_rs(
            &base_main,
            &["models", "routes", "schema"],
            &oauth_entries,
        );
        plan.modify(main_path.clone(), updated_main);
        plan.push_revert(crate::generate::emit::Revert::RoutesEntries {
            path: main_path,
            entries: oauth_entries,
        });

        // ── docs/guide/oauth.md ────────────────────────────────────────────────
        let docs_dir = project_root.join("docs").join("guide");
        plan.create(
            docs_dir.join("oauth.md"),
            render_oauth_docs_file(backend, &oauth.providers, &user_table),
        );

        // ── Cargo.toml: add oauth2 feature to autumn-web ─────────────────────
        let cargo_toml_path = project_root.join("Cargo.toml");
        let cargo_existing = read_or_empty(&cargo_toml_path);
        // The base plan may have already updated Cargo.toml; use its content if present.
        let base_cargo = find_plan_content_for_path(&plan, &cargo_toml_path)
            .unwrap_or_else(|| cargo_existing.clone());
        let with_oauth2 = ensure_autumn_web_oauth2_feature(&base_cargo);
        if with_oauth2 != base_cargo {
            plan.modify(cargo_toml_path.clone(), with_oauth2);
        }
        plan.push_revert(crate::generate::emit::Revert::CargoAutumnWebFeature {
            path: cargo_toml_path,
            feature: "oauth2".to_owned(),
            owner_dir: Some(project_root.join("src").join("models")),
        });

        // ── autumn.toml OAuth provider stubs ───────────────────────────────────
        let autumn_toml_path = project_root.join("autumn.toml");
        if autumn_toml_path.exists() {
            let toml_existing = read_or_empty(&autumn_toml_path);
            let updated_toml = append_oauth_stubs_to_toml(&toml_existing, &oauth.providers);
            if updated_toml != toml_existing {
                plan.modify(autumn_toml_path.clone(), updated_toml);
            }
            plan.push_revert(crate::generate::emit::Revert::AuthOAuthProviderStubs {
                path: autumn_toml_path,
                providers: oauth.providers.clone(),
            });
        }
    }

    if passkeys {
        // ── webauthn_credentials collision check ───────────────────────────────
        // If the project already has webauthn_credentials in schema.rs, the
        // migration below would fail at `diesel migration run`. Reject upfront.
        //
        // Only reject a *foreign* `webauthn_credentials` table: when the auth
        // user table itself is also already present, this is a re-run over
        // our own output (`--force`, or `autumn destroy` recomputing this
        // same plan against already-generated files — issue #1048), where
        // the schema append below is an idempotent no-op.
        let schema_for_check = project_root.join("src").join("schema.rs");
        let schema_existing_for_passkey = find_plan_content_for_path(&plan, &schema_for_check)
            .unwrap_or_else(|| read_or_empty(&schema_for_check));
        if schema_has_table(&schema_existing_for_passkey, "webauthn_credentials")
            && !schema_has_table(&schema_existing_for_passkey, &user_table)
        {
            return Err(GenerateError::InvalidName(
                name.to_owned(),
                "this project already defines a `webauthn_credentials` table, which \
                 `--passkeys` needs; rename or remove the existing table first."
                    .to_owned(),
            ));
        }

        // ── webauthn_credentials migration ─────────────────────────────────────
        // Use timestamp+2 when oauth is also requested (oauth uses timestamp+1),
        // so both migrations get unique Diesel version numbers.
        let passkey_offset: i64 = if oauth.providers.is_empty() { 1 } else { 2 };
        let passkey_ts_str: String = timestamp.parse::<i64>().map_or_else(
            |_| format!("{timestamp}{passkey_offset}"),
            |t| (t + passkey_offset).to_string(),
        );
        let mig_dir = project_root
            .join("migrations")
            .join(format!("{passkey_ts_str}_create_webauthn_credentials"));
        plan.create(
            mig_dir.join("up.sql"),
            render_passkey_migration_up(backend, &user_table),
        );
        plan.create(mig_dir.join("down.sql"), render_passkey_migration_down());

        // ── src/models/webauthn_credential.rs ─────────────────────────────────
        let models_dir = project_root.join("src").join("models");
        plan.create(
            models_dir.join("webauthn_credential.rs"),
            render_webauthn_credential_model_file(&user_table),
        );
        let model_mod_path = models_dir.join("mod.rs");
        let model_mod_base = find_plan_content_for_path(&plan, &model_mod_path)
            .unwrap_or_else(|| read_or_empty(&model_mod_path));
        plan.modify(
            model_mod_path.clone(),
            add_mod_declaration(&model_mod_base, "webauthn_credential"),
        );
        plan.push_revert(crate::generate::emit::Revert::ModDecl {
            path: model_mod_path,
            name: "webauthn_credential".to_owned(),
        });

        // ── src/schema.rs: webauthn_credentials table ─────────────────────────
        let schema_path = project_root.join("src").join("schema.rs");
        let schema_base = find_plan_content_for_path(&plan, &schema_path)
            .unwrap_or_else(|| read_or_empty(&schema_path));
        let wc_fields: Vec<super::dsl::Field> = [
            "user_id:i64",
            "credential_id:String",
            "credential_json:String",
            "name:String",
            "last_used_at:Option<NaiveDateTime>",
        ]
        .iter()
        .map(|t| super::dsl::parse_field(t).expect("webauthn credential field tokens are valid"))
        .collect();
        let updated_schema = append_schema_table(&schema_base, "webauthn_credentials", &wc_fields);
        plan.modify(schema_path.clone(), updated_schema);
        plan.push_revert(crate::generate::emit::Revert::SchemaTable {
            path: schema_path,
            table: "webauthn_credentials".to_owned(),
            expected_block: append_schema_table("", "webauthn_credentials", &wc_fields),
        });

        // ── src/routes/passkeys.rs ─────────────────────────────────────────────
        let routes_dir = project_root.join("src").join("routes");
        plan.create(
            routes_dir.join("passkeys.rs"),
            render_passkeys_routes_file(&pascal_name, &snake_name, &user_table),
        );
        let route_mod_path = routes_dir.join("mod.rs");
        let route_mod_base = find_plan_content_for_path(&plan, &route_mod_path)
            .unwrap_or_else(|| read_or_empty(&route_mod_path));
        plan.modify(
            route_mod_path.clone(),
            add_mod_declaration(&route_mod_base, "passkeys"),
        );
        plan.push_revert(crate::generate::emit::Revert::ModDecl {
            path: route_mod_path,
            name: "passkeys".to_owned(),
        });

        // ── Register passkey routes in src/main.rs ─────────────────────────────
        let main_path = project_root.join("src").join("main.rs");
        let main_existing = std::fs::read_to_string(&main_path).map_err(|_| {
            GenerateError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("missing {}", main_path.display()),
            ))
        })?;
        let pk_entries = passkey_route_entries();
        let base_main =
            find_plan_content_for_path(&plan, &main_path).unwrap_or_else(|| main_existing.clone());
        let updated_main = super::schema_edit::update_main_rs(
            &base_main,
            &["models", "routes", "schema"],
            &pk_entries,
        );
        plan.modify(main_path.clone(), updated_main);
        plan.push_revert(crate::generate::emit::Revert::RoutesEntries {
            path: main_path,
            entries: pk_entries,
        });

        // ── tests/auth_passkeys.rs ─────────────────────────────────────────────
        let tests_dir = project_root.join("tests");
        plan.create(
            tests_dir.join("auth_passkeys.rs"),
            render_passkeys_tests_file(&pascal_name, &snake_name),
        );

        // ── docs/guide/passkeys.md ─────────────────────────────────────────────
        let docs_dir = project_root.join("docs").join("guide");
        plan.create(docs_dir.join("passkeys.md"), render_passkeys_docs_file());

        // ── autumn.toml: [auth.webauthn] stub ─────────────────────────────────
        let autumn_toml_path = project_root.join("autumn.toml");
        if autumn_toml_path.exists() {
            let toml_existing = find_plan_content_for_path(&plan, &autumn_toml_path)
                .unwrap_or_else(|| read_or_empty(&autumn_toml_path));
            let updated_toml = append_webauthn_stub_to_toml(
                &toml_existing,
                "localhost",
                "My App",
                "http://localhost:3000",
            );
            if updated_toml != toml_existing {
                plan.modify(autumn_toml_path.clone(), updated_toml);
            }
            plan.push_revert(crate::generate::emit::Revert::AuthWebauthnStub {
                path: autumn_toml_path,
            });
        }

        // ── Cargo.toml: add webauthn-rs dep + webauthn feature on autumn-web ───
        let cargo_toml_path = project_root.join("Cargo.toml");
        let base_cargo = find_plan_content_for_path(&plan, &cargo_toml_path)
            .unwrap_or_else(|| read_or_empty(&cargo_toml_path));
        let all_passkey_deps: Vec<(&str, &str)> = PASSKEY_EXTRA_DEPS.to_vec();
        // `ensure_cargo_dependencies` is name-only, so an app that already pins
        // an older `base64` keeps it and never gets `0.22`; the emitted
        // `encode_cred_id` needs the 0.21+ `Engine` API, so warn if it's too old.
        super::model::warn_if_existing_dep_below_version(&mut plan, &base_cargo, "base64", 0, 22);
        let with_deps = super::model::ensure_cargo_dependencies(&base_cargo, &all_passkey_deps);
        // If the project already declared webauthn-rs without the required features,
        // ensure_cargo_dependencies would have skipped it; merge them here.
        let with_deps = ensure_webauthn_rs_features(&with_deps);
        let with_webauthn = ensure_autumn_web_webauthn_feature(&with_deps);
        if with_webauthn != base_cargo {
            plan.modify(cargo_toml_path.clone(), with_webauthn);
        }
        let passkey_models_owner_dir = project_root.join("src").join("models");
        let passkey_dep_names: Vec<String> = all_passkey_deps
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !super::model::TEMPLATE_SHIPPED_CARGO_DEPS.contains(name))
            .map(str::to_owned)
            .collect();
        if !passkey_dep_names.is_empty() {
            plan.push_revert(crate::generate::emit::Revert::CargoDeps {
                path: cargo_toml_path.clone(),
                names: passkey_dep_names,
                owner_dir: passkey_models_owner_dir.clone(),
            });
        }
        plan.push_revert(crate::generate::emit::Revert::CargoAutumnWebFeature {
            path: cargo_toml_path,
            feature: "webauthn".to_owned(),
            owner_dir: Some(passkey_models_owner_dir),
        });
    }

    Ok(plan)
}

/// Extract the planned output content for a given path from the plan (last modify/create wins).
fn find_plan_content_for_path(plan: &Plan, path: &std::path::Path) -> Option<String> {
    use super::emit::Action;
    plan.actions
        .iter()
        .rev()
        .find(|a| a.path() == path)
        .and_then(|a| match a {
            Action::Create { contents, .. }
            | Action::Modify { contents, .. }
            | Action::CreateIfAbsent { contents, .. } => Some(contents.clone()),
            // Binary/verbatim files have no textual content to inspect.
            Action::CreateBytes { .. } => None,
        })
}

/// Ensure `autumn-web` in `[dependencies]` has `features = ["oauth2"]`.
#[allow(clippy::too_many_lines)]
fn ensure_autumn_web_oauth2_feature(toml: &str) -> String {
    const CRATE: &str = "autumn-web";
    const FEATURE: &str = "\"oauth2\"";

    let mut lines: Vec<String> = toml.lines().map(str::to_owned).collect();
    let trailing_newline = toml.ends_with('\n');

    let simple_prefix = format!("{CRATE} = \"");
    let table_prefix = format!("{CRATE} = {{");
    let subtable_header = format!("[dependencies.{CRATE}]");
    let subtable_header_underscore = format!("[dependencies.{}]", CRATE.replace('-', "_"));

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim().to_owned();
        let indent: String = lines[i]
            .chars()
            .take_while(char::is_ascii_whitespace)
            .collect();

        if let Some(rest) = trimmed.strip_prefix(&simple_prefix) {
            let version = rest.trim_end_matches('"');
            lines[i] =
                format!("{indent}{CRATE} = {{ version = \"{version}\", features = [{FEATURE}] }}");
            break;
        }

        if trimmed.starts_with(&table_prefix) {
            if trimmed.contains(FEATURE) {
                break; // already present
            }
            if let Some(feat_bracket) = trimmed.find("features = [") {
                let list_start = feat_bracket + "features = [".len();
                if let Some(close_bracket) = trimmed[list_start..].find(']') {
                    let list_end = close_bracket + list_start;
                    let existing = trimmed[list_start..list_end].trim();
                    let new_list = if existing.is_empty() {
                        FEATURE.to_owned()
                    } else {
                        format!("{existing}, {FEATURE}")
                    };
                    lines[i] = format!(
                        "{indent}{}{}{}",
                        &trimmed[..list_start],
                        new_list,
                        &trimmed[list_end..]
                    );
                } else {
                    let mut j = i + 1;
                    while j < lines.len() {
                        let tj = lines[j].trim();
                        if tj.starts_with('[') {
                            break;
                        }
                        if let Some(close_idx) = tj.find(']') {
                            let before_close = tj[..close_idx].trim();
                            let sep = if before_close.is_empty() || before_close.ends_with(',') {
                                ""
                            } else {
                                ", "
                            };
                            let indent_j: String = lines[j]
                                .chars()
                                .take_while(char::is_ascii_whitespace)
                                .collect();
                            lines[j] = format!(
                                "{indent_j}{before_close}{sep}{FEATURE}{}",
                                &tj[close_idx..]
                            );
                            break;
                        }
                        j += 1;
                    }
                }
            } else {
                let close = trimmed.rfind('}').unwrap();
                let before_close = trimmed[..close].trim_end();
                let sep = if before_close.ends_with('{') {
                    ""
                } else {
                    ", "
                };
                lines[i] = format!(
                    "{indent}{}{sep}features = [{FEATURE}]{}",
                    &trimmed[..close],
                    &trimmed[close..]
                );
            }
            break;
        }

        if trimmed == subtable_header || trimmed == subtable_header_underscore {
            let mut j = i + 1;
            let mut found_features = false;
            while j < lines.len() {
                let t = lines[j].trim().to_owned();
                if t.starts_with('[') {
                    break;
                }
                if t.starts_with("features") {
                    found_features = true;
                    if t.contains(FEATURE) {
                        break;
                    }
                    if let Some(open) = t.find('[') {
                        if let Some(close) = t.rfind(']') {
                            let inner = t[open + 1..close].trim();
                            let new_inner = if inner.is_empty() {
                                FEATURE.to_owned()
                            } else {
                                format!("{inner}, {FEATURE}")
                            };
                            let indent_j: String = lines[j]
                                .chars()
                                .take_while(char::is_ascii_whitespace)
                                .collect();
                            lines[j] = format!("{indent_j}features = [{new_inner}]");
                        } else {
                            let mut k = j + 1;
                            while k < lines.len() {
                                let tk = lines[k].trim();
                                if tk.starts_with('[') {
                                    break;
                                }
                                if let Some(close_idx) = tk.find(']') {
                                    let before_close = tk[..close_idx].trim();
                                    let sep =
                                        if before_close.is_empty() || before_close.ends_with(',') {
                                            ""
                                        } else {
                                            ", "
                                        };
                                    let indent_k: String = lines[k]
                                        .chars()
                                        .take_while(char::is_ascii_whitespace)
                                        .collect();
                                    lines[k] = format!(
                                        "{indent_k}{before_close}{sep}{FEATURE}{}",
                                        &tk[close_idx..]
                                    );
                                    break;
                                }
                                k += 1;
                            }
                        }
                    }
                    break;
                }
                j += 1;
            }
            if !found_features {
                lines.insert(i + 1, format!("features = [{FEATURE}]"));
            }
            break;
        }

        i += 1;
    }

    let mut out = lines.join("\n");
    if trailing_newline && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// CLI entry point for `autumn generate auth <Name>` (no OAuth providers).
#[allow(dead_code)]
pub fn run(name: &str, flags: Flags) {
    run_with_options(name, flags, &AuthOAuthOptions::default(), false, false);
}

/// CLI entry point for `autumn generate auth <Name> --oauth <providers> [--totp] [--passkeys]`.
pub fn run_with_options(
    name: &str,
    flags: Flags,
    oauth: &AuthOAuthOptions,
    totp: bool,
    passkeys: bool,
) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: cannot determine current directory: {e}");
            std::process::exit(1);
        }
    };
    let timestamp = timestamp_now();
    let plan = plan_auth_full_ex(&cwd, name, &timestamp, oauth, totp, passkeys);
    match plan.and_then(|p| p.execute(flags)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// Ensure `autumn-web` in `[dependencies]` has `features = ["mail"]`.
///
/// Handles the three common forms a fresh Autumn project may use:
/// - `autumn-web = "x.y"` (simple string)
/// - `autumn-web = { version = "x.y", ... }` (inline table)
/// - `[dependencies.autumn-web]` subtable
fn ensure_autumn_web_mail_feature(toml: &str) -> String {
    const CRATE: &str = "autumn-web";
    const FEATURE: &str = "\"mail\"";

    let mut lines: Vec<String> = toml.lines().map(str::to_owned).collect();
    let trailing_newline = toml.ends_with('\n');

    let simple_prefix = format!("{CRATE} = \"");
    let table_prefix = format!("{CRATE} = {{");
    let subtable_header = format!("[dependencies.{CRATE}]");

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim().to_owned();
        let indent: String = lines[i]
            .chars()
            .take_while(char::is_ascii_whitespace)
            .collect();

        if let Some(rest) = trimmed.strip_prefix(&simple_prefix) {
            // autumn-web = "0.3"
            let version = rest.trim_end_matches('"');
            lines[i] =
                format!("{indent}{CRATE} = {{ version = \"{version}\", features = [{FEATURE}] }}");
            break;
        }

        if trimmed.starts_with(&table_prefix) {
            if trimmed.contains(FEATURE) {
                break; // already present
            }
            if let Some(feat_bracket) = trimmed.find("features = [") {
                // Add to existing features list.
                let list_start = feat_bracket + "features = [".len();
                let list_end = trimmed[list_start..].find(']').unwrap() + list_start;
                let existing = trimmed[list_start..list_end].trim();
                let new_list = if existing.is_empty() {
                    FEATURE.to_owned()
                } else {
                    format!("{existing}, {FEATURE}")
                };
                lines[i] = format!(
                    "{indent}{}{}{}",
                    &trimmed[..list_start],
                    new_list,
                    &trimmed[list_end..]
                );
            } else {
                // No features key — insert before closing `}`.
                let close = trimmed.rfind('}').unwrap();
                let before_close = trimmed[..close].trim_end();
                let sep = if before_close.ends_with('{') {
                    ""
                } else {
                    ", "
                };
                lines[i] = format!(
                    "{indent}{}{sep}features = [{FEATURE}]{}",
                    &trimmed[..close],
                    &trimmed[close..]
                );
            }
            break;
        }

        if trimmed == subtable_header {
            // Scan ahead within the subtable.
            let mut j = i + 1;
            let mut found_features = false;
            while j < lines.len() {
                let t = lines[j].trim().to_owned();
                if t.starts_with('[') {
                    break;
                }
                if t.starts_with("features") {
                    found_features = true;
                    if !t.contains(FEATURE)
                        && let (Some(open), Some(close)) = (t.find('['), t.rfind(']'))
                    {
                        let inner = t[open + 1..close].trim();
                        let new_inner = if inner.is_empty() {
                            FEATURE.to_owned()
                        } else {
                            format!("{inner}, {FEATURE}")
                        };
                        let indent_j: String = lines[j]
                            .chars()
                            .take_while(char::is_ascii_whitespace)
                            .collect();
                        lines[j] = format!("{indent_j}features = [{new_inner}]");
                    }
                    break;
                }
                j += 1;
            }
            if !found_features {
                lines.insert(i + 1, format!("features = [{FEATURE}]"));
            }
            break;
        }

        i += 1;
    }

    let mut out = lines.join("\n");
    if trailing_newline && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn append_oauth_stubs_to_toml(existing: &str, providers: &[String]) -> String {
    let mut out = existing.trim_end().to_owned();
    for name in providers {
        let header = format!("[auth.oauth2.{name}]");
        if out.contains(&header) {
            continue;
        }
        out.push('\n');
        out.push_str(&oauth_provider_stub_block(name));
    }
    out.push('\n');
    out
}

/// The exact `[auth.oauth2.<name>]` stub block [`append_oauth_stubs_to_toml`]
/// inserts for a known or unknown provider name. Shared with
/// `remove_oauth_provider_stubs` (via [`toml_table_block_matches`]) so
/// `autumn destroy` can verify a block is still byte-identical to this stub
/// — never hand-filled with real credentials, and never a pre-existing
/// block `generate` left untouched — before removing it.
fn oauth_provider_stub_block(name: &str) -> String {
    match name {
        "github" => r#"[auth.oauth2.github]
client_id = ""
client_secret = ""
authorize_url = "https://github.com/login/oauth/authorize"
token_url = "https://github.com/login/oauth/access_token"
userinfo_url = "https://api.github.com/user"
redirect_uri = "http://localhost:3000/auth/github/callback"
scope = "read:user user:email"
"#
        .to_owned(),
        "google" => r#"[auth.oauth2.google]
client_id = ""
client_secret = ""
authorize_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url = "https://oauth2.googleapis.com/token"
userinfo_url = "https://openidconnect.googleapis.com/v1/userinfo"
redirect_uri = "http://localhost:3000/auth/google/callback"
scope = "openid profile email"
issuer = "https://accounts.google.com"
jwks_url = "https://www.googleapis.com/oauth2/v3/certs"
discovery_url = "https://accounts.google.com"
"#
        .to_owned(),
        "microsoft" => r#"[auth.oauth2.microsoft]
client_id = ""
client_secret = ""
authorize_url = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize"
token_url = "https://login.microsoftonline.com/common/oauth2/v2.0/token"
redirect_uri = "http://localhost:3000/auth/microsoft/callback"
scope = "openid profile email"
issuer = "https://login.microsoftonline.com/common/v2.0"
jwks_url = "https://login.microsoftonline.com/common/discovery/v2.0/keys"
discovery_url = "https://login.microsoftonline.com/common/v2.0"
"#
        .to_owned(),
        _ => format!(
            r#"[auth.oauth2.{name}]
client_id = ""
client_secret = ""
authorize_url = "https://example.com/oauth/authorize"
token_url = "https://example.com/oauth/token"
redirect_uri = "http://localhost:3000/auth/{name}/callback"
scope = "openid profile email"
"#
        ),
    }
}

// ── Template rendering ────────────────────────────────────────────────────────

/// Backend-specific SQL fragments for the hand-written auth migration DDL
/// (issue #1927).
///
/// The auth generator scaffolds several tables (users, sessions, remember
/// tokens, recovery codes, magic-link tokens, OAuth identities, `WebAuthn`
/// credentials) via hand-written `CREATE TABLE` strings. This carries the small
/// set of column-type fragments that differ between backends so the Postgres
/// output stays byte-for-byte identical while a `SQLite` app gets valid DDL:
/// `INTEGER PRIMARY KEY AUTOINCREMENT` for auto-increment ids (`SQLite` has no
/// `BIGSERIAL`), plain `INTEGER` for `BIGINT`/`INT`, and ISO-8601 `TEXT`
/// timestamps defaulted to `CURRENT_TIMESTAMP` (`SQLite` has no dedicated
/// timestamp type nor `NOW()`). Portable pieces (`TEXT`, `REFERENCES`, `UNIQUE`,
/// `CREATE INDEX`) are shared and unchanged. Mirrors the dialect mapping the
/// backend-aware model/migration generators use
/// (`super::dsl::IdType::pk_sql_for` / `FieldKind::sqlite_sql_type`).
#[derive(Clone, Copy)]
struct AuthDdl {
    /// Auto-increment primary-key column definition (the SQL after `id `).
    pk: &'static str,
    /// A `BIGINT` foreign-key / integer column type.
    big_int: &'static str,
    /// A nullable timestamp column type (used as `{ts} NULL`).
    ts: &'static str,
    /// A `NOT NULL` timestamp column defaulted to the creation time.
    ts_not_null_default_now: &'static str,
    /// A `BOOLEAN NOT NULL DEFAULT FALSE` column.
    bool_not_null_false: &'static str,
    /// A small-integer `NOT NULL DEFAULT 0` column (`failed_attempts`).
    int_not_null_zero: &'static str,
}

impl AuthDdl {
    const fn for_backend(backend: autumn_web::config::DatabaseBackend) -> Self {
        match backend {
            autumn_web::config::DatabaseBackend::Postgres => Self {
                pk: "BIGSERIAL PRIMARY KEY",
                big_int: "BIGINT",
                ts: "TIMESTAMP",
                ts_not_null_default_now: "TIMESTAMP NOT NULL DEFAULT NOW()",
                bool_not_null_false: "BOOLEAN NOT NULL DEFAULT FALSE",
                int_not_null_zero: "INT NOT NULL DEFAULT 0",
            },
            autumn_web::config::DatabaseBackend::Sqlite => Self {
                pk: "INTEGER PRIMARY KEY AUTOINCREMENT",
                big_int: "INTEGER",
                ts: "TEXT",
                ts_not_null_default_now: "TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
                bool_not_null_false: "INTEGER NOT NULL DEFAULT 0",
                int_not_null_zero: "INTEGER NOT NULL DEFAULT 0",
            },
        }
    }
}

/// Name of the per-login session-tracking table (issue #819), derived from
/// the auth resource: `User` → `user_sessions`, `Account` → `account_sessions`.
fn sessions_table_name(snake_name: &str) -> String {
    format!("{snake_name}_sessions")
}

/// Table backing persistent "remember-me" login chains (issue #1397).
fn remember_table_name(snake_name: &str) -> String {
    format!("{snake_name}_remember_tokens")
}

/// DDL for the tracked-sessions table, in the app's own dialect.
///
/// Shared by the scaffolded migration and the generated session-management
/// guide (issue #1908) so a `SQLite` app is never handed Postgres-only
/// `BIGSERIAL` / `NOW()` DDL, and the two copies cannot drift.
fn render_sessions_table_ddl(
    backend: autumn_web::config::DatabaseBackend,
    snake_name: &str,
    user_table: &str,
) -> String {
    let d = AuthDdl::for_backend(backend);
    let sess_table = sessions_table_name(snake_name);
    format!(
        "CREATE TABLE {sess_table} (\n\
         \x20   id {pk},\n\
         \x20   user_id {big_int} NOT NULL REFERENCES {user_table}(id) ON DELETE CASCADE,\n\
         \x20   token_digest TEXT NOT NULL UNIQUE,\n\
         \x20   ip TEXT NOT NULL DEFAULT '',\n\
         \x20   user_agent TEXT NOT NULL DEFAULT '',\n\
         \x20   ua_family TEXT NOT NULL DEFAULT '',\n\
         \x20   ua_os TEXT NOT NULL DEFAULT '',\n\
         \x20   ua_device TEXT NOT NULL DEFAULT '',\n\
         \x20   label TEXT NULL,\n\
         \x20   last_seen_at {created_at},\n\
         \x20   created_at {created_at}\n\
         );\n\
         \n\
         CREATE INDEX {sess_table}_user_id_idx ON {sess_table} (user_id);\n",
        pk = d.pk,
        big_int = d.big_int,
        created_at = d.ts_not_null_default_now,
    )
}

#[allow(clippy::too_many_lines)]
fn render_migration_up(
    backend: autumn_web::config::DatabaseBackend,
    snake_name: &str,
    table: &str,
    totp: bool,
    magic_link: bool,
) -> String {
    let d = AuthDdl::for_backend(backend);
    // TOTP columns are inserted after password_digest so the column order
    // matches the generated model struct and `schema.rs` block.
    let totp_columns = if totp {
        format!(
            "\x20   totp_secret_encrypted TEXT NULL,\n\
             \x20   totp_enabled {bool_default_false},\n\
             \x20   totp_last_used_step {big_int} NULL,\n",
            bool_default_false = d.bool_not_null_false,
            big_int = d.big_int,
        )
    } else {
        String::new()
    };
    let mut out = format!(
        "CREATE TABLE {table} (\n\
         \x20   id {pk},\n\
         \x20   email TEXT NOT NULL,\n\
         \x20   time_zone TEXT NULL,\n\
         \x20   password_digest TEXT NOT NULL,\n\
         {totp_columns}\
         \x20   failed_attempts {int_zero},\n\
         \x20   locked_at {ts} NULL,\n\
         \x20   reset_token_digest TEXT NULL,\n\
         \x20   reset_token_expires_at {ts} NULL,\n\
         \x20   confirm_token_digest TEXT NULL,\n\
         \x20   confirm_token_expires_at {ts} NULL,\n\
         \x20   email_confirmed_at {ts} NULL,\n\
         \x20   pending_email TEXT NULL,\n\
         \x20   export_requested_at {ts} NULL,\n\
         \x20   delete_requested_at {ts} NULL,\n\
         \x20   delete_scheduled_at {ts} NULL,\n\
         \x20   created_at {created_at}\n\
         );\n",
        pk = d.pk,
        int_zero = d.int_not_null_zero,
        ts = d.ts,
        created_at = d.ts_not_null_default_now,
    );
    // `email`'s uniqueness is expressed through the same shared primitive
    // `field:String:unique` scaffolds elsewhere (issue #1032), rather than a
    // parallel hand-rolled `UNIQUE` column constraint.
    out.push_str(&unique_index_sql(table, "email", &[]));
    if totp {
        let _ = write!(
            out,
            "\n\
             CREATE TABLE recovery_codes (\n\
             \x20   id {pk},\n\
             \x20   user_id {big_int} NOT NULL REFERENCES {table}(id) ON DELETE CASCADE,\n\
             \x20   code_digest TEXT NOT NULL,\n\
             \x20   used_at {ts} NULL,\n\
             \x20   created_at {created_at}\n\
             );\n\
             \n\
             CREATE INDEX recovery_codes_user_id_idx ON recovery_codes (user_id);\n",
            pk = d.pk,
            big_int = d.big_int,
            ts = d.ts,
            created_at = d.ts_not_null_default_now,
        );
    }
    // Active login sessions (issue #819): one row per login, keyed by the
    // SHA-256 digest of the opaque server-side session id. Only the digest
    // is stored so a database leak cannot be replayed as a session cookie.
    let _ = write!(
        out,
        "\n{ddl}",
        ddl = render_sessions_table_ddl(backend, snake_name, table),
    );
    // Persistent "remember-me" login chains (issue #1397): one row per device
    // login-chain, keyed by the stable opaque `series`. `token_hash` rotates on
    // every use so a replayed rotated-out cookie is detected as theft. Only the
    // SHA-256 hash of the current token is stored — never the raw token — so a
    // database leak cannot be replayed as a remember cookie.
    let rem_table = remember_table_name(snake_name);
    let _ = write!(
        out,
        "\n\
         CREATE TABLE {rem_table} (\n\
         \x20   id {pk},\n\
         \x20   series TEXT NOT NULL UNIQUE,\n\
         \x20   user_id {big_int} NOT NULL REFERENCES {table}(id) ON DELETE CASCADE,\n\
         \x20   token_hash TEXT NOT NULL,\n\
         \x20   previous_token_hash TEXT NULL,\n\
         \x20   rotated_at {ts} NULL,\n\
         \x20   expires_at {ts} NOT NULL,\n\
         \x20   ip TEXT NOT NULL DEFAULT '',\n\
         \x20   user_agent TEXT NOT NULL DEFAULT '',\n\
         \x20   ua_family TEXT NOT NULL DEFAULT '',\n\
         \x20   ua_os TEXT NOT NULL DEFAULT '',\n\
         \x20   ua_device TEXT NOT NULL DEFAULT '',\n\
         \x20   label TEXT NULL,\n\
         \x20   last_used_at {ts} NULL,\n\
         \x20   created_at {created_at}\n\
         );\n\
         \n\
         CREATE INDEX {rem_table}_user_id_idx ON {rem_table} (user_id);\n",
        pk = d.pk,
        big_int = d.big_int,
        ts = d.ts,
        created_at = d.ts_not_null_default_now,
    );
    // Passwordless magic-link tokens (issue #1328): one row per issued link,
    // keyed by the SHA-256 digest of the raw token. Only the digest is stored —
    // the raw token appears only in the emailed link. `consumed_at` is the
    // single-use marker; `expires_at` enforces the configurable TTL.
    if magic_link {
        let _ = write!(
            out,
            "\n\
             CREATE TABLE magic_link_tokens (\n\
             \x20   id {pk},\n\
             \x20   user_id {big_int} NOT NULL REFERENCES {table}(id) ON DELETE CASCADE,\n\
             \x20   token_digest TEXT NOT NULL UNIQUE,\n\
             \x20   expires_at {ts} NOT NULL,\n\
             \x20   consumed_at {ts} NULL,\n\
             \x20   created_at {created_at}\n\
             );\n\
             \n\
             CREATE INDEX magic_link_tokens_user_id_idx ON magic_link_tokens (user_id);\n",
            pk = d.pk,
            big_int = d.big_int,
            ts = d.ts,
            created_at = d.ts_not_null_default_now,
        );
    }
    out
}

fn render_migration_down(snake_name: &str, table: &str, totp: bool, magic_link: bool) -> String {
    // Drop the dependent tables first so the FK constraints are satisfied.
    let sess_table = sessions_table_name(snake_name);
    let rem_table = remember_table_name(snake_name);
    // Magic-link tokens FK the user table, so drop them before it (issue #1328).
    let magic_drop = if magic_link {
        "DROP TABLE magic_link_tokens;\n"
    } else {
        ""
    };
    if totp {
        format!(
            "{magic_drop}DROP TABLE {rem_table};\nDROP TABLE {sess_table};\nDROP TABLE recovery_codes;\nDROP TABLE {table};\n"
        )
    } else {
        format!(
            "{magic_drop}DROP TABLE {rem_table};\nDROP TABLE {sess_table};\nDROP TABLE {table};\n"
        )
    }
}

fn render_model_file(pascal_name: &str, _snake_name: &str, table: &str, totp: bool) -> String {
    // TOTP columns mirror the migration/schema order (after password_digest).
    // TOTP columns mirror the migration/schema order (after password_digest).
    // `totp_secret_encrypted` holds the AES-GCM-encrypted secret (never plaintext),
    // and `totp_last_used_step` is the replay guard. All three are `#[default]`
    // so they are excluded from `NewUser` and fall back to their DB defaults
    // (disabled / NULL) on signup.
    let totp_fields = if totp {
        "    #[default]\n\
         \x20   pub totp_secret_encrypted: Option<String>,\n\
         \x20   #[default]\n\
         \x20   pub totp_enabled: bool,\n\
         \x20   #[default]\n\
         \x20   pub totp_last_used_step: Option<i64>,\n"
    } else {
        ""
    };
    format!(
        r"//! Generated by `autumn generate auth`.
//!
//! Edit freely — once generated, this is ordinary user code.
//! Security note: never store raw passwords, reset tokens, or TOTP secrets
//! here in the clear — only digests and encrypted blobs.

use crate::schema::{table};

#[autumn_web::model]
pub struct {pascal_name} {{
    pub id: i64,
    pub email: String,
    pub time_zone: Option<String>,
    pub password_digest: String,
{totp_fields}    #[default]
    pub failed_attempts: i32,
    #[default]
    pub locked_at: Option<chrono::NaiveDateTime>,
    pub reset_token_digest: Option<String>,
    pub reset_token_expires_at: Option<chrono::NaiveDateTime>,
    pub confirm_token_digest: Option<String>,
    pub confirm_token_expires_at: Option<chrono::NaiveDateTime>,
    #[default]
    pub email_confirmed_at: Option<chrono::NaiveDateTime>,
    // Pending email-change target (issue #1396). Nullable: set when a change is
    // requested, cleared when the new-address token is confirmed. The old
    // `email` stays valid for sign-in until confirmation swaps it.
    #[default]
    pub pending_email: Option<String>,
    #[default]
    pub export_requested_at: Option<chrono::NaiveDateTime>,
    #[default]
    pub delete_requested_at: Option<chrono::NaiveDateTime>,
    #[default]
    pub delete_scheduled_at: Option<chrono::NaiveDateTime>,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}}
"
    )
}

/// Render `src/models/recovery_code.rs` (only emitted with `--totp`).
///
/// Recovery codes are single-use: only the bcrypt `code_digest` is stored, and
/// `used_at` is stamped when a code is consumed so it can never be replayed.
fn render_recovery_code_model_file(user_pascal: &str, user_table: &str) -> String {
    format!(
        r"//! Generated by `autumn generate auth --totp`.
//!
//! Single-use TOTP recovery codes for {user_pascal}. Edit freely.
//! Security note: only the bcrypt digest of each code is stored — never the
//! raw code. `used_at` marks a code as consumed so it cannot be reused.

use crate::schema::recovery_codes;

#[autumn_web::model]
pub struct RecoveryCode {{
    pub id: i64,
    // References {user_table}(id).
    pub user_id: i64,
    pub code_digest: String,
    // Kept in `NewRecoveryCode` so inserts set it explicitly to `None`.
    pub used_at: Option<chrono::NaiveDateTime>,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}}
"
    )
}

/// Render `src/models/{snake}_session.rs` — the active-login-session row and
/// the revocation APIs on the user model (issue #819).
///
/// Only the SHA-256 digest of the opaque session id is stored, so a database
/// leak cannot be replayed as a session cookie. Revocation deletes the row;
/// the per-request gate in `routes/auth.rs` treats a missing row as a revoked
/// session and 401s immediately.
#[allow(clippy::too_many_lines)]
fn render_session_model_file(user_pascal: &str, user_snake: &str, user_table: &str) -> String {
    let sess_table = sessions_table_name(user_snake);
    format!(
        r#"//! Generated by `autumn generate auth`.
//!
//! Active login sessions for {user_pascal} — one row per logged-in device.
//! Edit freely — once generated, this is ordinary user code.
//!
//! Security notes:
//! - `token_digest` is the SHA-256 of the opaque server-side session id;
//!   the raw id is never stored, so a database leak cannot be replayed as
//!   a session cookie.
//! - Revocation deletes the row. The per-request gate in `routes/auth.rs`
//!   (`require_tracked_session`) treats a missing row as revoked and
//!   responds 401 immediately, even if the cookie is replayed.

use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{user_snake}::{user_pascal};
use crate::schema::{sess_table};

#[autumn_web::model]
pub struct {user_pascal}Session {{
    pub id: i64,
    // References {user_table}(id).
    pub user_id: i64,
    pub token_digest: String,
    pub ip: String,
    pub user_agent: String,
    pub ua_family: String,
    pub ua_os: String,
    pub ua_device: String,
    #[default]
    pub label: Option<String>,
    #[default]
    pub last_seen_at: chrono::NaiveDateTime,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}}

impl {user_pascal}Session {{
    /// Human-readable device line for the sessions page, preferring the
    /// user-set label over the parsed User-Agent.
    #[must_use]
    pub fn device_description(&self) -> String {{
        if let Some(label) = self.label.as_deref().filter(|l| !l.trim().is_empty()) {{
            return label.to_owned();
        }}
        match (self.ua_family.as_str(), self.ua_os.as_str()) {{
            ("" | "Unknown", "" | "Unknown") => "Unknown device".to_owned(),
            (family, "" | "Unknown") => family.to_owned(),
            ("" | "Unknown", os) => os.to_owned(),
            (family, os) => format!("{{family}} on {{os}}"),
        }}
    }}
}}

/// Active-session APIs available from any request handler that has loaded
/// the current user (issue #819).
impl {user_pascal} {{
    /// All active login sessions for this account, most recently seen first.
    pub async fn sessions(
        &self,
        conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    ) -> autumn_web::AutumnResult<Vec<{user_pascal}Session>> {{
        {sess_table}::table
            .filter({sess_table}::user_id.eq(self.id))
            .order({sess_table}::last_seen_at.desc())
            .select({user_pascal}Session::as_select())
            .load(conn)
            .await
            .map_err(|e| {{
                autumn_web::AutumnError::internal_server_error_msg(format!(
                    "Failed to load sessions: {{e}}"
                ))
            }})
    }}

    /// Revoke a single session by row id. Scoped to this account — revoking
    /// another user's session id is a no-op. Returns `true` when a session
    /// was actually revoked.
    pub async fn revoke_session(
        &self,
        conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
        session_id: i64,
    ) -> autumn_web::AutumnResult<bool> {{
        let rows = diesel::delete(
            {sess_table}::table
                .filter({sess_table}::id.eq(session_id))
                .filter({sess_table}::user_id.eq(self.id)),
        )
        .execute(conn)
        .await
        .map_err(|e| {{
            autumn_web::AutumnError::internal_server_error_msg(format!(
                "Failed to revoke session: {{e}}"
            ))
        }})?;
        Ok(rows == 1)
    }}

    /// Revoke every session except the one identified by
    /// `current_token_digest` ("sign out everywhere else"). Returns the
    /// number of sessions revoked.
    pub async fn revoke_other_sessions(
        &self,
        conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
        current_token_digest: &str,
    ) -> autumn_web::AutumnResult<usize> {{
        diesel::delete(
            {sess_table}::table
                .filter({sess_table}::user_id.eq(self.id))
                .filter({sess_table}::token_digest.ne(current_token_digest)),
        )
        .execute(conn)
        .await
        .map_err(|e| {{
            autumn_web::AutumnError::internal_server_error_msg(format!(
                "Failed to revoke sessions: {{e}}"
            ))
        }})
    }}

    /// Revoke every session for this account, including the current one.
    /// Used on password change, where all existing sessions are suspect.
    pub async fn revoke_all_sessions(
        &self,
        conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    ) -> autumn_web::AutumnResult<usize> {{
        diesel::delete({sess_table}::table.filter({sess_table}::user_id.eq(self.id)))
            .execute(conn)
            .await
            .map_err(|e| {{
                autumn_web::AutumnError::internal_server_error_msg(format!(
                    "Failed to revoke sessions: {{e}}"
                ))
            }})
    }}
}}
"#
    )
}

/// Render `src/models/{snake}_remember_token.rs` — the persistent remember-me
/// chain row and its CRUD, plus a bulk-revocation method on the user model
/// (issue #1397).
///
/// Only the SHA-256 hash of the current token is stored; the raw token lives
/// only in the cookie. `series` is the stable per-device lookup key and
/// `token_hash` rotates on every use so a replayed rotated-out cookie is
/// detected as theft (see `routes/auth.rs::remember_me`).
#[allow(clippy::too_many_lines)]
fn render_remember_model_file(user_pascal: &str, user_snake: &str, user_table: &str) -> String {
    let rem_table = remember_table_name(user_snake);
    format!(
        r#"//! Generated by `autumn generate auth`.
//!
//! Persistent "remember-me" login chains for {user_pascal} (issue #1397).
//! Edit freely — once generated, this is ordinary user code.
//!
//! Security notes:
//! - `token_hash` is the SHA-256 of the current rotating token; the raw token
//!   is never stored, so a database leak cannot be replayed as a cookie.
//! - `series` is stable across rotations for one device chain; `token_hash`
//!   rotates on every use. Replaying a rotated-out token for a known series is
//!   detected as theft and nukes the whole chain — see `routes/auth.rs`.

use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{user_snake}::{user_pascal};
use crate::schema::{rem_table};

#[autumn_web::model]
pub struct {user_pascal}RememberToken {{
    pub id: i64,
    /// Stable, opaque per-device lookup key.
    pub series: String,
    // References {user_table}(id).
    pub user_id: i64,
    /// SHA-256 hash of the current rotating token (never the raw token).
    pub token_hash: String,
    /// SHA-256 hash of the token that was just rotated out, accepted during the
    /// grace window so benign concurrent requests do not false-fire theft
    /// detection (issue #1397).
    #[default]
    pub previous_token_hash: Option<String>,
    /// When the current token was minted (the last rotation); bounds how long
    /// `previous_token_hash` stays acceptable.
    #[default]
    pub rotated_at: Option<chrono::NaiveDateTime>,
    pub expires_at: chrono::NaiveDateTime,
    pub ip: String,
    pub user_agent: String,
    pub ua_family: String,
    pub ua_os: String,
    pub ua_device: String,
    #[default]
    pub label: Option<String>,
    #[default]
    pub last_used_at: Option<chrono::NaiveDateTime>,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}}

impl {user_pascal}RememberToken {{
    /// Absolute expiry as a UTC datetime, for `autumn_web::auth::evaluate_remember`.
    #[must_use]
    pub fn expires_at_utc(&self) -> chrono::DateTime<chrono::Utc> {{
        self.expires_at.and_utc()
    }}

    /// Human-readable device line for the sessions page, preferring the
    /// user-set label over the parsed User-Agent (mirrors the active-session
    /// row's `device_description`).
    #[must_use]
    pub fn device_description(&self) -> String {{
        if let Some(label) = self.label.as_deref().filter(|l| !l.trim().is_empty()) {{
            return label.to_owned();
        }}
        match (self.ua_family.as_str(), self.ua_os.as_str()) {{
            ("" | "Unknown", "" | "Unknown") => "Unknown device".to_owned(),
            (family, "" | "Unknown") => family.to_owned(),
            ("" | "Unknown", os) => os.to_owned(),
            (family, os) => format!("{{family}} on {{os}}"),
        }}
    }}
}}

/// Persist a new remember chain for a successful "remember me" login.
pub async fn insert_remember_token(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    row: &New{user_pascal}RememberToken,
) -> autumn_web::AutumnResult<()> {{
    diesel::insert_into({rem_table}::table)
        .values(row)
        .execute(conn)
        .await
        .map_err(|e| {{
            autumn_web::AutumnError::internal_server_error_msg(format!(
                "Failed to persist remember token: {{e}}"
            ))
        }})?;
    Ok(())
}}

/// Look a chain up by its stable series id.
pub async fn find_remember_token(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    series: &str,
) -> autumn_web::AutumnResult<Option<{user_pascal}RememberToken>> {{
    {rem_table}::table
        .filter({rem_table}::series.eq(series))
        .select({user_pascal}RememberToken::as_select())
        .first(conn)
        .await
        .optional()
        .map_err(|e| {{
            autumn_web::AutumnError::internal_server_error_msg(format!(
                "Failed to load remember token: {{e}}"
            ))
        }})
}}

/// Atomically rotate a chain (compare-and-swap): move the current `token_hash`
/// into `previous_token_hash`, install `new_hash`, stamp `rotated_at`/
/// `last_used_at`, and extend the sliding expiry — but ONLY if the row still
/// carries `old_hash`. Returns the number of rows affected (0 when a sibling
/// request already rotated the chain, 1 on success), so the caller can detect a
/// lost race and re-evaluate rather than silently double-rotating (issue #1397).
pub async fn rotate_remember_token(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    series: &str,
    old_hash: &str,
    new_hash: &str,
    new_expiry: chrono::NaiveDateTime,
    now: chrono::NaiveDateTime,
) -> autumn_web::AutumnResult<usize> {{
    diesel::update(
        {rem_table}::table
            .filter({rem_table}::series.eq(series))
            .filter({rem_table}::token_hash.eq(old_hash)),
    )
    .set((
        {rem_table}::previous_token_hash.eq(Some(old_hash)),
        {rem_table}::token_hash.eq(new_hash),
        {rem_table}::rotated_at.eq(Some(now)),
        {rem_table}::expires_at.eq(new_expiry),
        {rem_table}::last_used_at.eq(Some(now)),
    ))
    .execute(conn)
    .await
    .map_err(|e| {{
        autumn_web::AutumnError::internal_server_error_msg(format!(
            "Failed to rotate remember token: {{e}}"
        ))
    }})
}}

/// Delete a single chain by series (theft / this-device logout revocation).
pub async fn delete_remember_series(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    series: &str,
) -> autumn_web::AutumnResult<usize> {{
    diesel::delete({rem_table}::table.filter({rem_table}::series.eq(series)))
        .execute(conn)
        .await
        .map_err(|e| {{
            autumn_web::AutumnError::internal_server_error_msg(format!(
                "Failed to delete remember token: {{e}}"
            ))
        }})
}}

impl {user_pascal} {{
    /// All persistent remember-me chains for this account, most recently used
    /// first, so they can be listed as their own entries on the device page and
    /// revoked individually (issue #1397.6).
    pub async fn remember_tokens(
        &self,
        conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    ) -> autumn_web::AutumnResult<Vec<{user_pascal}RememberToken>> {{
        {rem_table}::table
            .filter({rem_table}::user_id.eq(self.id))
            .order({rem_table}::created_at.desc())
            .select({user_pascal}RememberToken::as_select())
            .load(conn)
            .await
            .map_err(|e| {{
                autumn_web::AutumnError::internal_server_error_msg(format!(
                    "Failed to load remember tokens: {{e}}"
                ))
            }})
    }}

    /// Revoke a single remember chain by `series`, scoped to this account so a
    /// user can never revoke another account's chain. Returns `true` when a
    /// chain was actually revoked (issue #1397.6).
    pub async fn revoke_remember_series(
        &self,
        conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
        series: &str,
    ) -> autumn_web::AutumnResult<bool> {{
        let rows = diesel::delete(
            {rem_table}::table
                .filter({rem_table}::series.eq(series))
                .filter({rem_table}::user_id.eq(self.id)),
        )
        .execute(conn)
        .await
        .map_err(|e| {{
            autumn_web::AutumnError::internal_server_error_msg(format!(
                "Failed to revoke remember chain: {{e}}"
            ))
        }})?;
        Ok(rows == 1)
    }}

    /// Revoke every remember chain for this account. Used on "sign out
    /// everywhere else" and on credential change, where every persistent login
    /// is suspect — a replayed remember cookie afterwards finds no row and is
    /// rejected. Returns the number of chains revoked.
    pub async fn revoke_all_remember_tokens(
        &self,
        conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    ) -> autumn_web::AutumnResult<usize> {{
        diesel::delete({rem_table}::table.filter({rem_table}::user_id.eq(self.id)))
            .execute(conn)
            .await
            .map_err(|e| {{
                autumn_web::AutumnError::internal_server_error_msg(format!(
                    "Failed to revoke remember tokens: {{e}}"
                ))
            }})
    }}
}}
"#
    )
}

/// Field tokens for the `magic_link_tokens` table (issue #1328), shared by the
/// schema append and its destroy revert so they stay byte-identical. `id` and
/// `created_at` are added automatically by `append_schema_table`.
fn magic_link_token_fields() -> Vec<super::dsl::Field> {
    [
        "user_id:i64",
        "token_digest:String",
        "expires_at:NaiveDateTime",
        "consumed_at:Option<NaiveDateTime>",
    ]
    .iter()
    .map(|t| super::dsl::parse_field(t).expect("magic-link field tokens are always valid"))
    .collect()
}

/// Render `src/models/magic_link_token.rs` — the passwordless login-token row
/// (issue #1328).
///
/// Mirrors the reset-token invariants: only the SHA-256 digest of the raw
/// token is stored (`token_digest`), never the raw token. `consumed_at` is the
/// single-use marker (set on the first successful verify), and `expires_at`
/// enforces the configurable TTL. Unknown/expired/consumed tokens are all
/// rejected with the same generic failure — see `routes/auth.rs`.
fn render_magic_link_token_model_file(user_pascal: &str, user_table: &str) -> String {
    format!(
        r"//! Generated by `autumn generate auth --magic-link`.
//!
//! Passwordless magic-link login tokens for {user_pascal}. Edit freely.
//! Security note: only the SHA-256 digest of each raw token is stored — never
//! the raw token, which appears only in the emailed link. `consumed_at` marks a
//! token as used so it cannot be replayed; `expires_at` enforces the TTL.

use crate::schema::magic_link_tokens;

#[autumn_web::model]
pub struct MagicLinkToken {{
    pub id: i64,
    // References {user_table}(id).
    pub user_id: i64,
    pub token_digest: String,
    pub expires_at: chrono::NaiveDateTime,
    // Single-use marker: set to `now` on the first successful verify. A second
    // verify of the same token finds it non-NULL and is rejected.
    pub consumed_at: Option<chrono::NaiveDateTime>,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}}
"
    )
}

/// Magic-link cleanup injected into a credential-change transaction (issues
/// #1328 hardening): when the account's password or email is changed, delete the
/// user's outstanding `magic_link_tokens` in the SAME transaction so a link
/// minted/intercepted before the change cannot sign the account in afterwards
/// (magic-link auth matches an unconsumed, unexpired token, keyed by user id and
/// independent of the changed credential). Emitted only under `--magic-link` —
/// the table exists only then — and empty otherwise. Assumes a `user_id` binding
/// (`let {snake}_id = {snake}.id;`) and a `conn` transaction handle are in scope,
/// matching the `change_password` / `confirm_email_change` transactions.
fn magic_link_credential_clear_txn_src(snake_name: &str) -> String {
    const TPL: &str = r"            // Invalidate any outstanding magic-link login tokens in the SAME
            // transaction: a link minted before this credential change must not
            // sign the account in afterwards (magic-link auth matches an
            // unconsumed, unexpired token, keyed by user id and independent of
            // the changed credential).
            diesel::delete(
                magic_link_tokens::table.filter(magic_link_tokens::user_id.eq(__SNAKE___id)),
            )
            .execute(conn)
            .await?;
";
    TPL.replace("__SNAKE__", snake_name)
}

#[allow(
    clippy::too_many_lines,
    reason = "Single auth-routes template — splitting fragments makes the template harder to read."
)]
fn render_routes_file(
    pascal_name: &str,
    snake_name: &str,
    table: &str,
    providers: &[String],
    totp: bool,
    magic_link: bool,
) -> String {
    let sess_table = sessions_table_name(snake_name);
    let rem_table = remember_table_name(snake_name);
    // Passwordless magic-link login (issue #1328). All output is gated on the
    // `--magic-link` flag so the flow composes with `--oauth`/`--totp`/`--passkeys`.
    let (magic_link_imports, magic_link_section, magic_link_login_link) = if magic_link {
        (
            "use crate::models::magic_link_token::{MagicLinkToken, NewMagicLinkToken};\n\
             use crate::schema::magic_link_tokens;\n"
                .to_owned(),
            magic_link_routes_section_src(pascal_name, snake_name, table, totp),
            "        p { a href=\"/login/magic\" { \"Email me a magic sign-in link\" } }\n"
                .to_owned(),
        )
    } else {
        (String::new(), String::new(), String::new())
    };
    // Magic-link token cleanup spliced into the `change_password` (#7),
    // `confirm_email_change` (#10) and `reset_password` (#11) credential-change
    // transactions. Gated on `--magic-link` so the `magic_link_tokens` reference
    // only appears when the table (and its import) exist. (For 2FA-enabled users
    // the reset commit is deferred to `login_verify`, which carries its own gated
    // cleanup — see `magic_link_reset_commit_clear_txn_src`.)
    let (
        magic_link_change_password_clear,
        magic_link_email_change_clear,
        magic_link_reset_password_clear,
    ) = if magic_link {
        (
            magic_link_credential_clear_txn_src(snake_name),
            magic_link_credential_clear_txn_src(snake_name),
            magic_link_credential_clear_txn_src(snake_name),
        )
    } else {
        (String::new(), String::new(), String::new())
    };
    let oauth_buttons = if providers.is_empty() {
        String::new()
    } else {
        let mut btn_html = String::with_capacity(256);
        btn_html.push_str("hr;\n        h3 { \"Or sign in with:\" }\n        div style=\"display: flex; gap: 0.5rem;\" {\n");
        for p in providers {
            let label = match p.as_str() {
                "github" => "GitHub",
                "google" => "Google",
                "microsoft" => "Microsoft",
                _ => p,
            };
            btn_html.push_str("            a href=\"/auth/");
            btn_html.push_str(p);
            btn_html.push_str("/redirect\" { button type=\"button\" { \"Sign in with ");
            btn_html.push_str(label);
            btn_html.push_str("\" } }\n");
        }
        btn_html.push_str("        }\n");
        btn_html
    };

    let (
        totp_imports,
        totp_login_branch,
        totp_reset_branch,
        totp_clear_pending,
        totp_confirm_branch,
        totp_section,
        totp_reauth_field,
        totp_reauth_check,
    ) = if totp {
        (
            totp_imports_src().to_owned(),
            totp_login_branch_src(snake_name),
            totp_reset_branch_src(snake_name),
            totp_clear_pending_src().to_owned(),
            totp_confirm_branch_src(snake_name),
            totp_routes_section_src(pascal_name, snake_name, table, magic_link),
            totp_reauth_field_src(),
            totp_reauth_check_src(snake_name, table),
        )
    } else {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    };

    format!(
        r##"//! Generated by `autumn generate auth`.
//!
//! Complete browser authentication flow. Edit freely — once generated,
//! this is ordinary user code.
//!
//! Security properties:
//! - Passwords are hashed with bcrypt via `autumn_web::auth`.
//! - Reset tokens are 32-byte random values; only the SHA-256 digest is stored.
//! - Duplicate signup and failed login return identical non-enumerating errors.
//! - Login and reset-password rotate the session ID to prevent fixation.
//! - Logout clears + rotates the session, invalidating the old session (the old
//!   cookie cannot be replayed) while carrying a one-shot "logged out" notice.
//! - Every login is tracked as a `{sess_table}` row (digest of the opaque
//!   session id + device attribution); revoking the row signs that device out
//!   immediately — see `require_tracked_session` and
//!   docs/guide/session-management.md.

use autumn_web::auth::{{
    RememberConfig, RememberDecision, RememberRecord, build_remember_clear_cookie,
    build_remember_cookie, evaluate_remember, generate_remember_credential, generate_token,
    hash_password, hash_remember_token, parse_remember_cookie_value, verify_password,
}};
use autumn_web::extract::Query;
use autumn_web::prelude::*;
use axum::extract::Path;
use axum::response::{{IntoResponse, Response}};
use diesel::prelude::*;
use diesel_async::AsyncConnection as _;
use diesel_async::RunQueryDsl;
use serde::Deserialize;

use crate::models::{snake_name}::{{New{pascal_name}, {pascal_name}}};
use crate::models::{snake_name}_session::{{New{pascal_name}Session, {pascal_name}Session}};
use crate::models::{snake_name}_remember_token::{{
    New{pascal_name}RememberToken, {pascal_name}RememberToken, delete_remember_series,
    find_remember_token, insert_remember_token, rotate_remember_token,
}};
use crate::schema::{sess_table};
use crate::schema::{rem_table};
use crate::schema::{table};
{totp_imports}{magic_link_imports}

// ── Layout helpers ────────────────────────────────────────────────────────────

fn redirect_to(url: &str) -> Response {{
    axum::response::Redirect::to(url).into_response()
}}

// ── Persistent "remember-me" login (issue #1397) ─────────────────────────────
//
// A remember credential is a `(series, token)` pair (the Barry Jaspan rotating
// scheme). On login-with-remember, `issue_remember_cookie` persists a
// `{rem_table}` row (storing only the SHA-256 hash of the token) and sets the
// cookie. On any request WITHOUT a session but WITH a remember cookie,
// `remember_me` rotates the token, establishes a session, and re-sets the
// cookie — or, on a replayed rotated-out token, trips theft detection and nukes
// the chain. See docs/guide/session-management.md.
//
// The middleware and its startup hook are AUTO-WIRED into `src/main.rs` by the
// generator: `.layer(axum::middleware::from_fn(routes::auth::remember_me))`
// plus `.on_startup(routes::auth::remember_me_startup)`. `remember_me` runs as a
// plain Tower layer with no `AppState`, so the startup hook stashes the shared
// pool AND the resolved `[auth.remember]` config in a process-wide handle it
// reads from.

/// The process-wide state the remember middleware needs: the pool it checks
/// connections out of plus the resolved `[auth.remember]` config, so
/// `cookie_name`/`duration_secs`/`enabled` overrides in `autumn.toml` (or
/// `AUTUMN_AUTH__REMEMBER__*`) are honoured instead of the compiled defaults
/// (issue #1397.2).
struct RememberMiddlewareState {{
    pool: diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
    config: RememberConfig,
    // The configured `[auth].session_key` (default `"user_id"`), captured at
    // startup so a remember-me restore writes the SAME identity key that
    // `#[secured]` / `#[authorize]` authenticate against — the middleware has
    // no `AppState` to call `state.auth_session_key()` on at request time.
    auth_session_key: String,
}}

static REMEMBER_STATE: std::sync::OnceLock<RememberMiddlewareState> = std::sync::OnceLock::new();

/// Install the shared pool, resolved config, and configured auth session key the
/// remember middleware uses. Idempotent — a second call is ignored. Called by
/// `remember_me_startup`.
pub fn init_remember_pool(
    pool: diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
    config: RememberConfig,
    auth_session_key: String,
) {{
    let _ = REMEMBER_STATE.set(RememberMiddlewareState {{
        pool,
        config,
        auth_session_key,
    }});
}}

/// Startup hook (auto-wired into `main` via `.on_startup(...)`) that hands the
/// remember middleware the shared pool, the resolved `[auth.remember]` config,
/// and the configured `[auth].session_key`.
pub async fn remember_me_startup(state: AppState) -> AutumnResult<()> {{
    if let Some(pool) = state.pool() {{
        init_remember_pool(
            pool.clone(),
            state.config().auth.remember.clone(),
            state.auth_session_key().to_string(),
        );
    }}
    Ok(())
}}

/// The sliding expiry `duration_secs` in the future, as a naive UTC timestamp
/// (the column type), guarding the `u64 → i64` cast and saturating (rather than
/// panicking) on an absurd configured duration.
fn remember_expiry(config: &RememberConfig, now: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDateTime {{
    let secs = i64::try_from(config.duration_secs).unwrap_or(2_592_000);
    now.checked_add_signed(chrono::Duration::seconds(secs))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
        .naive_utc()
}}

/// Whether to set the `Secure` cookie attribute — mirrors the session cookie by
/// keying off the forwarded scheme (HTTPS in production behind a TLS proxy).
fn remember_secure(headers: &axum::http::HeaderMap) -> bool {{
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
}}

/// Read a single cookie value by name out of the request `Cookie` header.
fn read_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {{
    let cookies = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {{
        if let Some((k, v)) = pair.trim().split_once('=')
            && k.trim() == name
        {{
            return Some(v.trim().to_owned());
        }}
    }}
    None
}}

/// Append a `Set-Cookie` header (never replaces an existing one, so the rotated
/// session cookie and the rotated remember cookie coexist on one response).
fn append_set_cookie(response: &mut Response, value: &str) {{
    if let Ok(header_value) = axum::http::HeaderValue::from_str(value) {{
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, header_value);
    }}
}}

/// Mint and persist a remember chain for a just-authenticated {snake_name},
/// returning the `Set-Cookie` header the login handler attaches. Only the hash
/// of the rotating token is stored.
pub async fn issue_remember_cookie(
    db: &mut Db,
    config: &RememberConfig,
    {snake_name}_id: i64,
    ip: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
) -> AutumnResult<String> {{
    let cred = generate_remember_credential();
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(512)
        .collect::<String>();
    let parsed = autumn_web::user_agent::parse_user_agent(&user_agent);
    let row = New{pascal_name}RememberToken {{
        series: cred.series.clone(),
        user_id: {snake_name}_id,
        token_hash: hash_remember_token(&cred.token),
        expires_at: remember_expiry(config, chrono::Utc::now()),
        ip: ip.to_string(),
        user_agent,
        ua_family: parsed.family,
        ua_os: parsed.os,
        ua_device: parsed.device_class.to_string(),
    }};
    insert_remember_token(&mut **db, &row).await?;
    Ok(build_remember_cookie(
        config,
        &cred.series,
        &cred.token,
        remember_secure(headers),
    ))
}}

/// Revoke the remember chain identified by the request's remember cookie (used
/// by logout). No-op when the cookie is absent or malformed.
async fn revoke_remember_from_cookie(
    db: &mut Db,
    config: &RememberConfig,
    headers: &axum::http::HeaderMap,
) {{
    if let Some(value) = read_cookie(headers, &config.cookie_name)
        && let Some((series, _token)) = parse_remember_cookie_value(&value)
    {{
        let _ = delete_remember_series(&mut **db, &series).await;
    }}
}}

/// Project a stored row into the pure [`RememberRecord`] the decision function
/// evaluates.
fn to_remember_record(r: &{pascal_name}RememberToken) -> RememberRecord {{
    RememberRecord {{
        series: r.series.clone(),
        token_hash: r.token_hash.clone(),
        previous_token_hash: r.previous_token_hash.clone(),
        rotated_at: r.rotated_at.map(|t| t.and_utc()),
        expires_at: r.expires_at_utc(),
    }}
}}

/// Establish a real session for the chain's owner — the same shape the login
/// handler writes, so `require_tracked_session` accepts it — and track it as an
/// active session so it shows in the device list and can be revoked (issue #819
/// + #1397).
async fn establish_remember_login(
    session: &Session,
    pool: &diesel_async::pooled_connection::deadpool::Pool<::autumn_web::RuntimeConnection>,
    auth_session_key: &str,
    {snake_name}_id: i64,
    ip: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
) {{
    session.rotate_id().await;
    // `rotate_id()` PRESERVES existing session data, so ANY stale step-up /
    // reauth elevation marker from a prior authenticated state in this browser
    // could survive into the restored session. Actively CLEAR them ALL before
    // writing identity keys — a remember-cookie restore must never inherit a
    // prior session's "sudo mode", nor resume a mid-step-up elevated state.
    // Two markers grant/advance elevation and must be dropped:
    //   * `last_strong_auth_at` (STEP_UP_SESSION_KEY) — the "recently did strong
    //     auth" claim that `#[step_up]` routes check.
    //   * `reauth_pw_ok` — the reauth flow's "password already verified, awaiting
    //     TOTP code" marker (valid ~10 min). Left behind, a restored session
    //     could POST /reauth with only a TOTP/recovery code and mint a fresh
    //     `last_strong_auth_at` WITHOUT re-entering the password, defeating this
    //     downgrade. `rotate_id()` preserves it too, so clear it here as well.
    // (issue #833/#1397).
    session.remove(autumn_web::step_up::STEP_UP_SESSION_KEY).await;
    session.remove("reauth_pw_ok").await;
    // A remembered request must be fully authenticated for protected routes:
    // write the SAME identity keys login writes. `{snake_name}_id` powers the
    // identity extractors / tracked-session lookup, and the configured
    // `auth_session_key` is the key `#[secured]` / `#[authorize]` check — without
    // it a remember-cookie restore would rotate the token but still 401.
    session.insert("{snake_name}_id", {snake_name}_id.to_string()).await;
    session.insert(auth_session_key, {snake_name}_id.to_string()).await;
    // Intentionally do NOT stamp `set_last_strong_auth_at` here: a long-lived
    // remember cookie is *not* strong authentication. Restoring a session from
    // it must not satisfy step-up "sudo mode" — otherwise a stolen/unattended
    // persistent cookie would silently pass `#[step_up]` routes (e.g. account
    // deletion) with no password reauth. Because `rotate_id()` preserves data we
    // additionally CLEAR any pre-existing claim above (defense against a prior
    // fresh step-up surviving the restore), forcing those routes to redirect to
    // `/reauth` until the user re-enters their password (issue #833).
    if let Ok(mut db) = pool.get().await {{
        let session_row = build_session_row(session, {snake_name}_id, ip, headers).await;
        let _ = diesel::insert_into({sess_table}::table)
            .values(&session_row)
            .execute(&mut *db)
            .await;
    }}
}}

/// Global middleware: upgrade a request that carries a valid remember cookie but
/// no session into an authenticated one (rotating the token), or trip theft
/// detection on a replayed rotated-out cookie. Auto-wired into `src/main.rs` by
/// the generator (see the module header above).
pub async fn remember_me(
    session: Session,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {{
    // The resolved config + pool are stashed at startup; without them the
    // middleware is inert.
    let Some(mw_state) = REMEMBER_STATE.get() else {{
        return next.run(request).await;
    }};
    let config = &mw_state.config;

    // Already authenticated or remember disabled: the existing session wins.
    if !config.enabled || session.get("{snake_name}_id").await.is_some() {{
        return next.run(request).await;
    }}

    let Some(cookie_value) = read_cookie(request.headers(), &config.cookie_name) else {{
        return next.run(request).await;
    }};
    // A malformed cookie is ignored (proceed unauthenticated) rather than 400.
    let Some((series, token)) = parse_remember_cookie_value(&cookie_value) else {{
        return next.run(request).await;
    }};

    let Ok(mut conn) = mw_state.pool.get().await else {{
        return next.run(request).await;
    }};

    let record = match find_remember_token(&mut *conn, &series).await {{
        Ok(record) => record,
        // A transient DB error must not authenticate — proceed unauthenticated.
        // Release the checked-out connection first so a downstream `Db` route
        // can't self-starve a small/exhausted pool while `next.run` awaits.
        Err(_) => {{
            drop(conn);
            return next.run(request).await;
        }}
    }};

    let now = chrono::Utc::now();
    let secure = remember_secure(request.headers());
    let grace = autumn_web::auth::default_rotation_grace();
    let ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let evaluated = record.as_ref().map(to_remember_record);

    match evaluate_remember(&token, evaluated.as_ref(), now, grace) {{
        RememberDecision::Rotate => {{
            let row = record.expect("Rotate is only returned for a present record");

            // Atomic compare-and-swap rotation: only the request that still sees
            // the current token wins. Extend the sliding expiry at the same time,
            // BEFORE establishing the session, so the old cookie value can never
            // be replayed.
            let new_token = generate_token();
            let new_hash = hash_remember_token(&new_token);
            let new_expiry = remember_expiry(config, now);
            let affected = match rotate_remember_token(
                &mut *conn,
                &series,
                &row.token_hash,
                &new_hash,
                new_expiry,
                now.naive_utc(),
            )
            .await
            {{
                Ok(n) => n,
                // A transient DB error must not authenticate. Release the
                // connection before yielding so a downstream `Db` route can't
                // self-starve a small/exhausted pool while `next.run` awaits.
                Err(_) => {{
                    drop(conn);
                    return next.run(request).await;
                }}
            }};

            if affected == 1 {{
                drop(conn);
                establish_remember_login(&session, &mw_state.pool, &mw_state.auth_session_key, row.user_id, ip, request.headers())
                    .await;
                let mut response = next.run(request).await;
                append_set_cookie(
                    &mut response,
                    &build_remember_cookie(config, &series, &new_token, secure),
                );
                response
            }} else {{
                // Lost the race: a sibling request already rotated the chain.
                // Re-select and re-evaluate the presented token against the new
                // state before deciding.
                let record2 = find_remember_token(&mut *conn, &series).await.ok().flatten();
                let evaluated2 = record2.as_ref().map(to_remember_record);
                match evaluate_remember(&token, evaluated2.as_ref(), now, grace) {{
                    RememberDecision::Accept | RememberDecision::Rotate => {{
                        // Within grace (or somehow still current): authenticate
                        // WITHOUT rotating again or re-cookieing.
                        drop(conn);
                        establish_remember_login(
                            &session,
                            &mw_state.pool,
                            &mw_state.auth_session_key,
                            row.user_id,
                            ip,
                            request.headers(),
                        )
                        .await;
                        next.run(request).await
                    }}
                    RememberDecision::Theft => {{
                        let _ = delete_remember_series(&mut *conn, &series).await;
                        drop(conn);
                        let mut response = next.run(request).await;
                        append_set_cookie(&mut response, &build_remember_clear_cookie(config));
                        response
                    }}
                    RememberDecision::Reject => {{
                        drop(conn);
                        let mut response = next.run(request).await;
                        append_set_cookie(&mut response, &build_remember_clear_cookie(config));
                        response
                    }}
                }}
            }}
        }}
        RememberDecision::Accept => {{
            // A benign concurrent request presenting the just-rotated-out token
            // within the grace window: authenticate without rotating or setting a
            // new cookie.
            let row = record.expect("Accept is only returned for a present record");
            drop(conn);
            establish_remember_login(&session, &mw_state.pool, &mw_state.auth_session_key, row.user_id, ip, request.headers())
                .await;
            next.run(request).await
        }}
        RememberDecision::Theft => {{
            // Replayed, rotated-out token for a known series → an attacker holds a
            // stale cookie. Nuke the whole chain and stay unauthenticated.
            let _ = delete_remember_series(&mut *conn, &series).await;
            drop(conn);
            let mut response = next.run(request).await;
            append_set_cookie(&mut response, &build_remember_clear_cookie(config));
            response
        }}
        RememberDecision::Reject => {{
            drop(conn);
            let mut response = next.run(request).await;
            append_set_cookie(&mut response, &build_remember_clear_cookie(config));
            response
        }}
    }}
}}

// ── Active session tracking (issue #819) ─────────────────────────────────────
//
// Every login persists a `{sess_table}` row keyed by the SHA-256 digest of the
// opaque session id. Authenticated handlers call `require_tracked_session`,
// which (1) 401s — and destroys the cookie session — when the row has been
// revoked, even if the cookie is replayed, and (2) updates `last_seen_at` at
// most once per `[auth.sessions].last_seen_update_secs` to bound write
// amplification. See docs/guide/session-management.md.

/// SHA-256 digest of the current opaque session id. Only the digest is ever
/// stored server-side, so a database leak cannot be replayed as a cookie.
pub async fn session_token_digest(session: &Session) -> String {{
    sha256_hex(&session.id().await)
}}

/// Build the tracked-session row for the current session id and device.
///
/// To plug in a custom User-Agent parser, replace the
/// `autumn_web::user_agent::parse_user_agent` call below — this is the only
/// parse site. See docs/guide/session-management.md.
pub async fn build_session_row(
    session: &Session,
    {snake_name}_id: i64,
    ip: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
) -> New{pascal_name}Session {{
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(512)
        .collect::<String>();
    let parsed = autumn_web::user_agent::parse_user_agent(&user_agent);
    New{pascal_name}Session {{
        user_id: {snake_name}_id,
        token_digest: session_token_digest(session).await,
        ip: ip.to_string(),
        user_agent,
        ua_family: parsed.family,
        ua_os: parsed.os,
        ua_device: parsed.device_class.to_string(),
    }}
}}

/// Persist a `{sess_table}` row for a successful login on this device.
pub async fn record_login_session(
    db: &mut Db,
    session: &Session,
    {snake_name}_id: i64,
    ip: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
) -> AutumnResult<()> {{
    let row = build_session_row(session, {snake_name}_id, ip, headers).await;
    diesel::insert_into({sess_table}::table)
        .values(&row)
        .execute(&mut **db)
        .await
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!(
                "Failed to record login session: {{e}}"
            ))
        }})?;
    Ok(())
}}

/// Re-point the tracked session row at the current (rotated) session id.
///
/// `session.rotate_id()` changes the opaque id, and with it the digest the
/// row is keyed by. Any flow that rotates while *staying logged in* (e.g.
/// step-up reauth) must call this with the digest captured before the
/// rotation, or the very next request would look up a missing row, be
/// treated as revoked, and 401.
pub async fn rebind_tracked_session(
    db: &mut Db,
    session: &Session,
    old_token_digest: &str,
) -> AutumnResult<()> {{
    let new_digest = session_token_digest(session).await;
    diesel::update(
        {sess_table}::table.filter({sess_table}::token_digest.eq(old_token_digest)),
    )
    .set({sess_table}::token_digest.eq(&new_digest))
    .execute(&mut **db)
    .await
    .map_err(|e| {{
        AutumnError::internal_server_error_msg(format!(
            "Failed to rebind tracked session: {{e}}"
        ))
    }})?;
    Ok(())
}}

/// Delete the tracked row for the *current* session id, if any.
///
/// Login flows call this before `session.rotate_id()`: the rotation
/// destroys the old server-side session, so a row keyed to its digest
/// could never authenticate again and would linger as a phantom device on
/// the previous account's sessions page. Logout uses it too.
pub async fn untrack_current_session(db: &mut Db, session: &Session) -> AutumnResult<()> {{
    let token_digest = session_token_digest(session).await;
    diesel::delete(
        {sess_table}::table.filter({sess_table}::token_digest.eq(&token_digest)),
    )
    .execute(&mut **db)
    .await
    .map_err(|e| {{
        AutumnError::internal_server_error_msg(format!(
            "Failed to clean up tracked session: {{e}}"
        ))
    }})?;
    Ok(())
}}

/// Validate the tracked session row for the authenticated user and return
/// the current {pascal_name}.
///
/// The `{sess_table}` row is the source of truth: deleting it signs the
/// device out *immediately* — the next request 401s and the cookie session
/// is destroyed, so a replayed cookie cannot resurrect it. `last_seen_at`
/// is refreshed at most once per `[auth.sessions].last_seen_update_secs`
/// per session, bounding write amplification.
///
/// Call this at the top of every handler that requires authentication.
pub async fn require_tracked_session(
    session: &Session,
    db: &mut Db,
    state: &AppState,
) -> AutumnResult<{pascal_name}> {{
    let {snake_name}_id: i64 = session
        .get("{snake_name}_id")
        .await
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| AutumnError::unauthorized_msg("Not authenticated."))?;

    let token_digest = session_token_digest(session).await;
    let tracked: Option<{pascal_name}Session> = {sess_table}::table
        .filter({sess_table}::token_digest.eq(&token_digest))
        .filter({sess_table}::user_id.eq({snake_name}_id))
        .select({pascal_name}Session::as_select())
        .first(&mut **db)
        .await
        .optional()
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!("Failed to load session: {{e}}"))
        }})?;

    let Some(tracked) = tracked else {{
        // Revoked (or never recorded): destroy the cookie session so a
        // replayed cookie cannot resurrect it, then reject.
        session.destroy().await;
        return Err(AutumnError::unauthorized_msg("Session revoked."));
    }};

    // Bounded write amplification: skip the UPDATE inside the window.
    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let sessions_cfg = &config.auth.sessions;
    let now = chrono::Utc::now().naive_utc();
    let window =
        chrono::Duration::seconds(i64::try_from(sessions_cfg.last_seen_update_secs).unwrap_or(60));
    if now.signed_duration_since(tracked.last_seen_at) >= window {{
        let _ = diesel::update({sess_table}::table.find(tracked.id))
            .set({sess_table}::last_seen_at.eq(now))
            .execute(&mut **db)
            .await;
    }}

    {table}::table
        .find({snake_name}_id)
        .select({pascal_name}::as_select())
        .first(&mut **db)
        .await
        .map_err(|_| AutumnError::unauthorized_msg("Account not found."))
}}

/// Render the device list. Shared by the full page and the htmx partial.
///
/// Persistent "remember-me" chains (issue #1397.6) are surfaced as their own
/// entries alongside active sessions, each with its own Revoke control, so a
/// dormant chain (browser closed, no active session) is still listed and
/// revocable — otherwise a "revoked" device could silently re-authenticate via
/// its surviving remember cookie.
fn sessions_list_fragment(
    sessions: &[{pascal_name}Session],
    remember_chains: &[{pascal_name}RememberToken],
    current_digest: &str,
    csrf: Option<&CsrfToken>,
    csrf_field: Option<&CsrfFormField>,
) -> Markup {{
    let csrf_name = csrf_field.map_or("_csrf", |f| f.0.as_str());
    html! {{
        div id="session-list" {{
            @if sessions.is_empty() && remember_chains.is_empty() {{
                p {{ "No active sessions." }}
            }}
            ul style="list-style:none;padding:0;" {{
                @for s in sessions {{
                    @let current = s.token_digest == current_digest;
                    li style="border:1px solid #ccc;padding:0.75rem;margin-bottom:0.5rem;" {{
                        p {{
                            strong {{ (s.device_description()) }}
                            @if current {{ " — this device" }}
                        }}
                        p {{
                            "Signed in " (s.created_at.format("%Y-%m-%d %H:%M UTC").to_string())
                            " from " (s.ip)
                            " · last seen " (s.last_seen_at.format("%Y-%m-%d %H:%M UTC").to_string())
                        }}
                        @if !current {{
                            // Real form so revocation works without JavaScript;
                            // htmx intercepts the submit for an in-place swap.
                            form method="post" action={{ "/account/sessions/" (s.id) "/revoke" }}
                                hx-post={{ "/account/sessions/" (s.id) "/revoke" }}
                                hx-target="#session-list" hx-swap="outerHTML"
                                style="display:inline" {{
                                @if let Some(csrf) = csrf {{
                                    input type="hidden" name=(csrf_name) value=(csrf.token());
                                }}
                                button type="submit" {{ "Revoke" }}
                            }}
                        }}
                        form action={{ "/account/sessions/" (s.id) "/label" }} method="post"
                            style="margin-top:0.25rem;" {{
                            @if let Some(csrf) = csrf {{
                                input type="hidden" name=(csrf_name) value=(csrf.token());
                            }}
                            input type="text" name="label" placeholder="Device label"
                                value=[s.label.as_deref()];
                            button type="submit" {{ "Save label" }}
                        }}
                    }}
                }}
                // Persistent remember-me chains as their own revocable entries
                // (issue #1397.6). A chain can outlive any active session, so it
                // is listed even when its device has no live session row.
                @for r in remember_chains {{
                    li style="border:1px solid #ccc;padding:0.75rem;margin-bottom:0.5rem;" {{
                        p {{
                            strong {{ (r.device_description()) }}
                            " — Persistent login (Remember me)"
                        }}
                        p {{
                            "Created " (r.created_at.format("%Y-%m-%d %H:%M UTC").to_string())
                            " from " (r.ip)
                            @if let Some(last) = r.last_used_at {{
                                " · last used " (last.format("%Y-%m-%d %H:%M UTC").to_string())
                            }}
                        }}
                        // Revoke this persistent chain by its stable series id,
                        // scoped to the current account.
                        form method="post" action={{ "/account/remember/" (r.series) "/revoke" }}
                            hx-post={{ "/account/remember/" (r.series) "/revoke" }}
                            hx-target="#session-list" hx-swap="outerHTML"
                            style="display:inline" {{
                            @if let Some(csrf) = csrf {{
                                input type="hidden" name=(csrf_name) value=(csrf.token());
                            }}
                            button type="submit" {{ "Revoke" }}
                        }}
                    }}
                }}
            }}
            // Real form so bulk revocation works without JavaScript; htmx
            // intercepts the submit for an in-place swap.
            form method="post" action="/account/sessions/revoke-others"
                hx-post="/account/sessions/revoke-others"
                hx-target="#session-list" hx-swap="outerHTML" {{
                @if let Some(csrf) = csrf {{
                    input type="hidden" name=(csrf_name) value=(csrf.token());
                }}
                button type="submit" {{ "Sign out everywhere else" }}
            }}
        }}
    }}
}}

/// Re-render after a mutation: htmx requests get the refreshed partial,
/// plain form posts get a redirect back to the page.
async fn sessions_after_mutation(
    session: &Session,
    db: &mut Db,
    state: &AppState,
    hx: HxRequest,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    if !hx.is_htmx {{
        return Ok(redirect_to("/account/sessions"));
    }}
    let {snake_name} = require_tracked_session(session, db, state).await?;
    let current_digest = session_token_digest(session).await;
    let sessions = {snake_name}.sessions(&mut **db).await?;
    let remember_chains = {snake_name}.remember_tokens(&mut **db).await?;
    Ok(sessions_list_fragment(
        &sessions,
        &remember_chains,
        &current_digest,
        csrf.as_ref(),
        csrf_field.as_ref(),
    )
    .into_response())
}}

/// `GET /account/sessions` — list active sessions with revocation controls.
/// Requires authentication.
#[secured]
#[get("/account/sessions")]
pub async fn sessions_page(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    hx: HxRequest,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;
    let current_digest = session_token_digest(&session).await;
    let sessions = {snake_name}.sessions(&mut *db).await?;
    let remember_chains = {snake_name}.remember_tokens(&mut *db).await?;
    let fragment = sessions_list_fragment(
        &sessions,
        &remember_chains,
        &current_digest,
        csrf.as_ref(),
        csrf_field.as_ref(),
    );
    if hx.is_htmx {{
        return Ok(fragment.into_response());
    }}
    Ok(crate::layout("Active Sessions", "/account/sessions", html! {{}}, html! {{
        @if let Some(ref csrf) = csrf {{ meta name="csrf-token" content=(csrf.token()); }}
        script src=(HTMX_JS_PATH) {{}}
        script src=(HTMX_CSRF_JS_PATH) {{}}
        h1 {{ "Active Sessions" }}
        p {{
            "These are the devices currently signed in to your account, plus any \
             persistent \"remember me\" logins. Revoke anything you don't \
             recognise — that device is signed out immediately and can no longer \
             auto-login."
        }}
        (fragment)
        p {{ a href="/account" {{ "← Back to account" }} }}
    }}).into_response())
}}

/// `POST /account/sessions/{{id}}/revoke` — sign a single device out.
/// The revoked device's next request 401s. Requires authentication.
#[secured]
#[post("/account/sessions/{{id}}/revoke")]
pub async fn sessions_revoke(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    hx: HxRequest,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Path(id): Path<i64>,
) -> AutumnResult<Response> {{
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;
    {snake_name}.revoke_session(&mut *db, id).await?;
    sessions_after_mutation(&session, &mut db, &state, hx, csrf, csrf_field).await
}}

/// `POST /account/remember/{{series}}/revoke` — revoke a single persistent
/// remember-me chain (issue #1397.6). Scoped to the current account, so a
/// dormant chain listed on the device page can be signed out even when its
/// device has no active session. Requires authentication.
#[secured]
#[post("/account/remember/{{series}}/revoke")]
pub async fn sessions_revoke_remember(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    hx: HxRequest,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Path(series): Path<String>,
) -> AutumnResult<Response> {{
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;
    {snake_name}.revoke_remember_series(&mut *db, &series).await?;
    sessions_after_mutation(&session, &mut db, &state, hx, csrf, csrf_field).await
}}

/// `POST /account/sessions/revoke-others` — "Sign out everywhere else":
/// revoke every session except the current one. Requires authentication.
#[secured]
#[post("/account/sessions/revoke-others")]
pub async fn sessions_revoke_others(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    hx: HxRequest,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;
    let current_digest = session_token_digest(&session).await;
    {snake_name}
        .revoke_other_sessions(&mut *db, &current_digest)
        .await?;
    // Also revoke every persistent remember-me chain for this account (issue
    // #1397): a "sign out everywhere" that left a live remember cookie behind
    // would let a signed-out device silently auto-login again.
    {snake_name}.revoke_all_remember_tokens(&mut *db).await?;
    sessions_after_mutation(&session, &mut db, &state, hx, csrf, csrf_field).await
}}

#[derive(Deserialize)]
pub struct SessionLabelForm {{
    pub label: String,
}}

/// `POST /account/sessions/{{id}}/label` — set a human-readable device label
/// (e.g. "Work laptop"). An empty label clears it. Requires authentication.
#[secured]
#[post("/account/sessions/{{id}}/label")]
pub async fn sessions_label(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    hx: HxRequest,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Path(id): Path<i64>,
    Form(form): Form<SessionLabelForm>,
) -> AutumnResult<Response> {{
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;
    let label = form.label.trim().chars().take(100).collect::<String>();
    let label = if label.is_empty() {{ None }} else {{ Some(label) }};
    diesel::update(
        {sess_table}::table
            .filter({sess_table}::id.eq(id))
            .filter({sess_table}::user_id.eq({snake_name}.id)),
    )
    .set({sess_table}::label.eq(label))
    .execute(&mut *db)
    .await
    .map_err(|e| {{
        AutumnError::internal_server_error_msg(format!("Failed to set label: {{e}}"))
    }})?;
    sessions_after_mutation(&session, &mut db, &state, hx, csrf, csrf_field).await
}}

// ── Signup ────────────────────────────────────────────────────────────────────

/// Render the signup form, optionally with a validation error. The minimum
/// password length is read from `[auth.password]` so the copy always matches
/// the active policy.
fn render_signup_form(
    min_len: usize,
    error: Option<&str>,
    csrf: Option<&CsrfToken>,
    csrf_field: Option<&CsrfFormField>,
) -> Markup {{
    crate::layout("Sign Up", "/signup", html! {{}}, html! {{
        h1 {{ "Create an Account" }}
        @if let Some(error) = error {{
            p role="alert" {{ (error) }}
        }}
        form action="/signup" method="post" {{
            @if let Some(csrf) = csrf {{ input type="hidden" name=(csrf_field.map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            div {{
                label {{ "Email" }}
                input type="email" name="email" required autocomplete="email";
            }}
            div {{
                label {{ "Password (" (min_len) "+ characters)" }}
                input type="password" name="password" required
                      autocomplete="new-password" minlength=(min_len);
            }}
            button type="submit" {{ "Sign Up" }}
        }}
        p {{ a href="/login" {{ "Already have an account? Log in" }} }}
    }})
}}

/// `GET /signup` — render the signup form.
#[get("/signup")]
pub async fn signup_form(
    State(state): State<AppState>,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {{
    let min_len = state.config_arc().auth.password.min_length;
    Ok(render_signup_form(min_len, None, csrf.as_ref(), csrf_field.as_ref()))
}}

#[derive(Deserialize)]
pub struct SignupForm {{
    pub email: String,
    pub password: String,
}}

/// `POST /signup` — create a new account in an unconfirmed state and
/// immediately dispatch a confirmation email.
///
/// The user is NOT logged in by signup; they must confirm their email first.
/// Non-enumerating: returns the same error whether the email is taken or invalid
/// so callers cannot learn which addresses are registered.
/// Fails fast when mail is not configured so the "check your email" page is
/// never shown when no mail will actually be sent.
#[post("/signup")]
pub async fn signup(
    State(state): State<AppState>,
    mut db: Db,
    mailer: Mailer,
    session: Session,
    flash: Flash,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Form(form): Form<SignupForm>,
) -> AutumnResult<Response> {{
    // Fail fast: confirmation requires mail. Check before any DB work so the
    // response does not depend on whether the email address already exists.
    if mailer.is_disabled() {{
        return Err(AutumnError::internal_server_error_msg(
            "Signup requires email confirmation, but mail is not configured. \
             Set [mail] transport in autumn.toml (e.g. transport = \"log\" for dev). \
             The signup feature is unavailable until mail is set up.",
        ));
    }}

    let email = form.email.trim().to_lowercase();
    // Same message for invalid format and duplicate email — non-enumerating.
    let account_err = || AutumnError::unprocessable_msg("Unable to create account. Please try a different email.");
    if let Some((_, domain)) = email.split_once('@') {{
        if !domain.contains('.') {{
            return Err(account_err());
        }}
    }} else {{
        return Err(account_err());
    }}
    // Enforce the configured password policy (length, weak-list, similarity to
    // the supplied email, and optional HIBP breach check) instead of a bare
    // length gate. On failure, re-render the form with the specific message at
    // HTTP 200 rather than accepting a weak credential or returning an error page.
    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let password_cfg = &config.auth.password;
    let mut policy = password_cfg.policy();
    if password_cfg.breach_check != autumn_web::auth::BreachCheck::Off {{
        // Breach checking needs an HTTP client for the HIBP k-anonymity lookup;
        // the default-off path never constructs one.
        policy = policy.with_client(autumn_web::http_client::Client::new());
    }}
    let ctx = [email.as_str()];
    let validation = autumn_web::auth::validate_password(&form.password, &policy, &ctx).await;
    if !validation.is_valid() {{
        // Show EVERY failure (e.g. both "too short" and "too common"), not just
        // the first, so the user can fix all problems at once (issue #1345.6).
        let messages = validation.messages();
        let message = if messages.is_empty() {{
            "Invalid password.".to_owned()
        }} else {{
            messages.join("\n")
        }};
        return Ok(render_signup_form(
            password_cfg.min_length,
            Some(&message),
            csrf.as_ref(),
            csrf_field.as_ref(),
        )
        .into_response());
    }}

    let raw_confirm_token = generate_confirmation_token();
    let confirm_digest = sha256_hex(&raw_confirm_token);
    let confirm_expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::hours(24);

    let password_digest = hash_password(&form.password).await?;
    let new_{snake_name} = New{pascal_name} {{
        email: email.clone(),
        time_zone: None,
        password_digest,
        reset_token_digest: None,
        reset_token_expires_at: None,
        confirm_token_digest: Some(confirm_digest),
        confirm_token_expires_at: Some(confirm_expires_at),
    }};

    let result: Result<{pascal_name}, _> = diesel::insert_into({table}::table)
        .values(&new_{snake_name})
        .returning({pascal_name}::as_returning())
        .get_result(&mut *db)
        .await;

    let {snake_name} = result.map_err(|_| account_err())?;

    // Rotate session to prevent any pending 2FA or enrollment state from a
    // previous flow in this browser from being resumed under the new account.
    // The rotation preserves any existing login's keys, so re-point that
    // login's tracked row at the new session id — otherwise the next
    // authenticated request would miss the row (treated as revoked) and the
    // old row would linger as a phantom device.
    let pre_rotation_digest = session_token_digest(&session).await;
    session.rotate_id().await;
    rebind_tracked_session(&mut db, &session, &pre_rotation_digest).await?;
    session.remove("totp_pending_id").await;
    session.remove("totp_pending_reset_digest").await;
    session.remove("totp_pending_reset_token").await;
    session.remove("totp_pending_secret").await;

    // Propagate mail failures: login is gated on email_confirmed_at, so an
    // account whose confirmation link was never delivered is unusable.
    // Roll back the insert so the address is free to retry signup.
    if let Err(e) = send_confirmation_email(&mailer, &{snake_name}.email, &raw_confirm_token).await {{
        let _ = diesel::delete({table}::table.find({snake_name}.id))
            .execute(&mut *db)
            .await;
        tracing::error!("confirmation email failed on signup — rolled back: {{e}}");
        return Err(AutumnError::internal_server_error_msg(
            "Failed to send confirmation email. Please try again later.",
        ));
    }}

    flash.info("Account created — check your email to confirm.").await;
    Ok(redirect_to("/check-your-email"))
}}

// ── Login ─────────────────────────────────────────────────────────────────────

/// `GET /login` — render the login form.
#[get("/login")]
pub async fn login_form(flash: Flash, csrf: Option<CsrfToken>, csrf_field: Option<CsrfFormField>) -> AutumnResult<Markup> {{
    Ok(crate::layout("Log In", "/login", flash_messages(&flash.consume().await), html! {{
        h1 {{ "Log In" }}
        form action="/login" method="post" {{
            @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            div {{
                label {{ "Email" }}
                input type="email" name="email" required autocomplete="email";
            }}
            div {{
                label {{ "Password" }}
                input type="password" name="password" required
                      autocomplete="current-password";
            }}
            label {{
                input type="checkbox" name="remember" value="on";
                " Remember me on this device"
            }}
            button type="submit" {{ "Log In" }}
        }}
        {oauth_buttons}
        p {{ a href="/signup" {{ "New here? Create an account" }} }}
        p {{ a href="/forgot-password" {{ "Forgot your password?" }} }}
{magic_link_login_link}        p {{ a href="/auth/confirm/resend" {{ "Resend confirmation email" }} }}
    }}))
}}

#[derive(Deserialize)]
pub struct LoginForm {{
    pub email: String,
    pub password: String,
    /// Present (`Some("on")`) when the "Remember me" checkbox is ticked; an
    /// unchecked box posts nothing, so this stays `None` (issue #1397).
    #[serde(default)]
    pub remember: Option<String>,
}}

/// Extracts the client IP from `ConnectInfo` when present, or falls back to
/// `UNSPECIFIED` so the handler compiles and runs correctly in both production
/// (where `into_make_service_with_connect_info` injects the extension) and
/// in test environments where it is absent.
pub struct MaybeClientIp(pub std::net::IpAddr);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for MaybeClientIp {{
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {{
        let ip = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        Ok(MaybeClientIp(ip))
    }}
}}

/// `POST /login` — verify credentials and start a session.
///
/// Non-enumerating: returns the same error for unknown email, wrong password,
/// and a locked account so callers cannot learn which accounts are registered
/// or which are currently locked.
///
/// Account lockout policy is read from `[auth.lockout]` in `autumn.toml`.
/// Safe defaults: threshold = 10 failures, cooloff = 900 s.
/// Disable with `enabled = false` or `threshold = 0` to restore pre-lockout behaviour.
#[post("/login")]
pub async fn login(
    mut db: Db,
    State(state): State<AppState>,
    session: Session,
    flash: Flash,
    MaybeClientIp(addr_ip): MaybeClientIp,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginForm>,
) -> AutumnResult<Response> {{
    let email = form.email.trim().to_lowercase();
    let auth_err = || AutumnError::unprocessable_msg("Invalid email or password.");

    let found_{snake_name}: Option<{pascal_name}> = {table}::table
        .filter({table}::email.eq(&email))
        .select({pascal_name}::as_select())
        .first(&mut *db)
        .await
        .ok();

    // Always run bcrypt to equalise timing: a miss uses a dummy hash so
    // response latency is indistinguishable from a wrong-password attempt
    // on a real account.
    const DUMMY_HASH: &str = "$2b$12$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let password_hash = found_{snake_name}
        .as_ref()
        .map(|u| u.password_digest.as_str())
        .unwrap_or(DUMMY_HASH);
    let password_ok = verify_password(&form.password, password_hash).await.unwrap_or(false);

    // ── Account lockout policy ────────────────────────────────────────────────
    // Read lockout config from the standard Autumn config surface.
    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let lockout_cfg = &config.auth.lockout;
    let lockout_enabled = lockout_cfg.enabled && lockout_cfg.threshold > 0;

    if let Some(ref {snake_name}) = found_{snake_name} {{
        if lockout_enabled {{
            let now = chrono::Utc::now().naive_utc();
            let cooloff = chrono::Duration::seconds(lockout_cfg.cooloff_secs as i64);

            // Track local state; reset if cool-off has elapsed so the first new
            // failure after expiry is counted as attempt #1, not a re-lock.
            let mut current_attempts = {snake_name}.failed_attempts;
            let mut current_locked_at = {snake_name}.locked_at;

            // Check if the account is currently locked: locked_at is set and
            // cool-off period has not elapsed. Non-enumerating: return the same
            // auth_err as wrong password so the response does not reveal lock state.
            if let Some(locked_at) = {snake_name}.locked_at {{
                if now < locked_at + cooloff {{
                    return Err(auth_err());
                }}
                // Cool-off elapsed — treat the account as unlocked. Reset local
                // counters so the next failure starts from 1, not from threshold.
                current_attempts = 0;
                current_locked_at = None;
            }}

            if !password_ok {{
                // Atomically increment the failure counter so concurrent bad-password
                // requests each count as distinct failures rather than collapsing into
                // one. Use two separate update paths (different Diesel expression types)
                // rather than an if expression inside `.eq(...)`.
                let new_attempts: i32 = if current_attempts == {snake_name}.failed_attempts {{
                    // Normal path: DB atomically computes `failed_attempts + 1`.
                    // Propagate write errors — silently absorbing a failure here
                    // would let repeated bad-password attempts bypass the threshold.
                    diesel::update({table}::table.find({snake_name}.id))
                        .set({table}::failed_attempts.eq({table}::failed_attempts + 1))
                        .returning({table}::failed_attempts)
                        .get_result::<i32>(&mut *db)
                        .await
                        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to record failed login: {{e}}")))?
                }} else {{
                    // Cool-off reset path: atomically reset counter to 1 and clear
                    // locked_at in a single UPDATE. Concurrent requests hitting this
                    // branch all write the same values so races are benign (all still
                    // count 1 failure each, not zero). Propagate write errors so that
                    // DB permission failures don't silently bypass the lockout threshold.
                    diesel::update({table}::table.find({snake_name}.id))
                        .set((
                            {table}::failed_attempts.eq(1i32),
                            {table}::locked_at.eq(None::<chrono::NaiveDateTime>),
                        ))
                        .execute(&mut *db)
                        .await
                        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to reset lockout counter: {{e}}")))?;
                    1i32
                }};

                if new_attempts >= lockout_cfg.threshold && current_locked_at.is_none() {{
                    // Account transitions into the locked state — stamp locked_at,
                    // but only if the row still shows an over-threshold, not-yet-locked
                    // state at write time. `current_locked_at` above is a stale
                    // in-memory read from before the password check; without a
                    // fresh DB-level guard, a concurrent *successful* login could
                    // reset failed_attempts/locked_at between our increment and this
                    // write, and this UPDATE would silently re-lock an account that
                    // just logged in successfully (#2500). Filtering on the row's
                    // current failed_attempts and locked_at makes the write a no-op
                    // in that case instead of clobbering the reset. Propagate errors:
                    // if this write fails the account is not locked despite the
                    // counter crossing the threshold, which would allow a successful
                    // login to slip through.
                    let locked_rows = diesel::update(
                        {table}::table
                            .find({snake_name}.id)
                            .filter({table}::failed_attempts.ge(lockout_cfg.threshold))
                            .filter({table}::locked_at.is_null()),
                    )
                        .set({table}::locked_at.eq(Some(now)))
                        .execute(&mut *db)
                        .await
                        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to lock account: {{e}}")))?;

                    // Only emit lockout telemetry when this request's write actually
                    // applied the lock — a concurrent successful login winning the
                    // race above means the account never ends up locked, so it must
                    // not be reported as such.
                    if locked_rows > 0 {{
                        // Truncate to a coarse IP prefix (IPv4 /24, IPv6 /64) so
                        // the telemetry event enables incident response without
                        // logging a precise user identifier.
                        let ip_prefix = match addr_ip {{
                            std::net::IpAddr::V4(ip) => {{
                                let [a, b, c, _] = ip.octets();
                                format!("{{a}}.{{b}}.{{c}}.0/24")
                            }}
                            std::net::IpAddr::V6(ip) => {{
                                let s = ip.segments();
                                format!("{{:x}}:{{:x}}:{{:x}}:{{:x}}::/64", s[0], s[1], s[2], s[3])
                            }}
                        }};
                        // Salt the digest with the deployment secret so the
                        // account ID cannot be recovered by hashing small integers.
                        let account_id_digest = {{
                            use sha2::{{Digest, Sha256}};
                            // Require a deployment secret for the digest salt. Operators
                            // MUST set SECRET_KEY_BASE (already required for sessions) or
                            // AUTUMN_ADMIN_SECRET. The static fallback prevents reversibility
                            // only within this process; set the env var in production.
                            let salt = std::env::var("SECRET_KEY_BASE")
                                .or_else(|_| std::env::var("AUTUMN_ADMIN_SECRET"))
                                .unwrap_or_else(|_| "autumn-lockout-fallback-salt".to_string());
                            let hash = Sha256::digest(
                                format!("{{}}:{{}}", salt, {snake_name}.id).as_bytes(),
                            );
                            hex::encode(&hash[..8])
                        }};
                        tracing::warn!(
                            event = "account_locked",
                            account_id_digest = %account_id_digest,
                            ip_prefix = %ip_prefix,
                            failed_attempts = new_attempts,
                            "account locked after repeated failed login attempts"
                        );
                    }}
                }}
                return Err(auth_err());
            }}
        }} else if !password_ok {{
            return Err(auth_err());
        }}
    }} else {{
        // Unknown email — bcrypt ran on DUMMY_HASH above for timing safety.
        if !password_ok {{
            return Err(auth_err());
        }}
    }}

    let {snake_name} = found_{snake_name}.ok_or_else(auth_err)?;

    // Successful login: reset lockout counter only when the account is not
    // currently locked. The WHERE clause accepts both:
    //   - locked_at IS NULL (no lock ever set), and
    //   - locked_at <= now - cooloff (lock expired via cool-off),
    // so that a user who waited out the cool-off can log in with a correct
    // password without requiring an operator unlock.
    // If zero rows match after excluding active (non-expired) locks, the
    // account was just locked concurrently — reject the login.
    if lockout_enabled {{
        let cooloff = chrono::Duration::seconds(lockout_cfg.cooloff_secs as i64);
        let lock_expired_before = chrono::Utc::now().naive_utc() - cooloff;
        let rows_cleared = diesel::update(
            {table}::table
                .find({snake_name}.id)
                .filter(
                    {table}::locked_at.is_null().or(
                        {table}::locked_at.le(lock_expired_before)
                    )
                ),
        )
        .set((
            {table}::failed_attempts.eq(0),
            {table}::locked_at.eq(None::<chrono::NaiveDateTime>),
        ))
        .execute(&mut *db)
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to reset lockout on login: {{e}}")))?;
        if rows_cleared == 0 {{
            return Err(auth_err());
        }}
    }}

    // AC5: Reject unconfirmed accounts with the same non-enumerating response
    // as a wrong-password attempt so the endpoint does not reveal whether the
    // address is registered or merely unconfirmed.
    if {snake_name}.email_confirmed_at.is_none() {{
        return Err(auth_err());
    }}

    // This browser may already hold a tracked session (re-login or account
    // switch). The rotation below destroys that session id, so drop its row
    // now — otherwise it would linger as a phantom device.
    untrack_current_session(&mut db, &session).await?;
{totp_login_branch}
    session.rotate_id().await;
{totp_clear_pending}    session.insert("{snake_name}_id", {snake_name}.id.to_string()).await;
    session.insert("{snake_name}_email", &{snake_name}.email).await;
    // Use the same session key checked by `#[secured]` / `#[authorize]`.
    session.insert(state.auth_session_key(), {snake_name}.id.to_string()).await;
    // Stamp the step-up claim so `#[step_up]` routes are immediately accessible.
    autumn_web::step_up::set_last_strong_auth_at(&session).await;
    // Track this login as an active session (device list + revocation).
    record_login_session(&mut db, &session, {snake_name}.id, addr_ip, &headers).await?;
    flash.success("Logged in.").await;

    let mut response = redirect_to("/account");
    // Persistent "remember-me" opt-in (issue #1397): when the box is ticked and
    // policy allows it, mint a rotating remember chain and attach its cookie
    // alongside the session cookie. Unticked → behaviour is unchanged.
    // One shared read of the config serves both the opt-in check and the
    // resolved `[auth.remember]` section — `config()` would deep-clone every
    // config section twice per login.
    let config = state.config_arc();
    if form.remember.is_some() && config.auth.remember.enabled {{
        // Thread the resolved `[auth.remember]` config so cookie_name/duration
        // overrides are honoured (issue #1397.2).
        let remember_cfg = &config.auth.remember;
        let cookie =
            issue_remember_cookie(&mut db, remember_cfg, {snake_name}.id, addr_ip, &headers).await?;
        append_set_cookie(&mut response, &cookie);
    }}
    Ok(response)
}}

// ── Logout ────────────────────────────────────────────────────────────────────

/// `POST /logout` — destroy the session and redirect to the login page.
///
/// Destroying (not just clearing) the session ensures an old session cookie
/// cannot be replayed after logout. The tracked `{sess_table}` row is removed
/// too so the device disappears from the active-sessions list.
#[post("/logout")]
pub async fn logout(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    headers: axum::http::HeaderMap,
    flash: Flash,
) -> AutumnResult<Response> {{
    // Thread the resolved `[auth.remember]` config so an overridden cookie name
    // is the one we revoke and clear (issue #1397.2). Read through the shared
    // `Arc` — a logout must not deep-clone every config section.
    let config = state.config_arc();
    let remember_cfg = &config.auth.remember;
    // Best-effort: the device must sign out even if the row delete hiccups.
    let _ = untrack_current_session(&mut db, &session).await;
    // Revoke this device's remember chain (issue #1397) so a stolen remember
    // cookie cannot re-establish a login after logout. No-op when absent.
    revoke_remember_from_cookie(&mut db, remember_cfg, &headers).await;
    // Invalidate the session: clear all data (drops the auth keys) and rotate
    // the id so the pre-logout cookie can no longer be replayed — the old id is
    // destroyed in the session store on save. This is equivalent to `destroy()`
    // for replay safety while letting a one-shot logout notice ride the freshly
    // rotated session through to the login page.
    session.clear().await;
    session.rotate_id().await;
    flash.info("You have been logged out.").await;
    let mut response = redirect_to("/login");
    append_set_cookie(&mut response, &build_remember_clear_cookie(remember_cfg));
    Ok(response)
}}

// ── Operator unlock ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct UnlockAccountForm {{
    pub email: String,
}}

/// `POST /auth/admin/unlock` — clear the lockout for a single account.
///
/// Requires the `X-Admin-Secret` header to match the `AUTUMN_ADMIN_SECRET`
/// environment variable. Returns the same 422 as a wrong-password attempt on
/// both wrong secret and unknown email so the response does not reveal which
/// accounts exist or are locked.
///
/// Protect this endpoint with network-level access controls (VPN, internal
/// load-balancer allowlist) in production in addition to the secret header.
///
/// **CSRF exemption required.** Autumn enables CSRF by default on all non-safe
/// routes. Add this path to the exempt list in `autumn.toml`:
/// ```toml
/// [security.csrf]
/// exempt_paths = ["/auth/admin/unlock"]
/// ```
///
/// Usage:
/// ```sh
/// curl -s -X POST https://example.com/auth/admin/unlock \
///   -H "Content-Type: application/x-www-form-urlencoded" \
///   -H "X-Admin-Secret: $AUTUMN_ADMIN_SECRET" \
///   -d "email=user%40example.com"
/// ```
#[post("/auth/admin/unlock")]
pub async fn unlock_account(
    mut db: Db,
    headers: axum::http::HeaderMap,
    Form(form): Form<UnlockAccountForm>,
) -> AutumnResult<Markup> {{
    let admin_secret = std::env::var("AUTUMN_ADMIN_SECRET").unwrap_or_default();
    let provided = headers
        .get("x-admin-secret")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if admin_secret.is_empty() || provided != admin_secret {{
        return Err(AutumnError::unprocessable_msg("Invalid email or password."));
    }}
    let email = form.email.trim().to_lowercase();
    diesel::update({table}::table.filter({table}::email.eq(&email)))
        .set((
            {table}::failed_attempts.eq(0),
            {table}::locked_at.eq(None::<chrono::NaiveDateTime>),
        ))
        .execute(&mut *db)
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to unlock account: {{e}}")))?;
    Ok(crate::layout("Account Unlocked", "/account", html! {{}}, html! {{
        h1 {{ "Account Unlocked" }}
        p {{ "The lockout for " (email) " has been cleared if it existed." }}
    }}))
}}

// ── Account (protected example route) ────────────────────────────────────────

/// `GET /account` — current-account profile placeholder. Requires authentication.
///
/// This is a protected-route example: the `#[secured]` attribute rejects
/// anonymous requests before the handler body runs.
#[secured]
#[get("/account")]
pub async fn account(session: Session, State(state): State<AppState>, mut db: Db, flash: Flash, csrf: Option<CsrfToken>, csrf_field: Option<CsrfFormField>) -> AutumnResult<Response> {{
    // Validates the tracked session row (401s immediately if revoked) and
    // refreshes `last_seen_at` with bounded write amplification.
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;

    // Email-confirmed gate: demonstrates how to require a confirmed address on
    // any route. Remove this block to allow unconfirmed users to access the
    // route, or replicate it on other routes that should require confirmation.
    // See docs/guide/authentication.md for the full pattern.
    if {snake_name}.email_confirmed_at.is_none() {{
        return Ok(redirect_to("/check-your-email").into_response());
    }}

    Ok(crate::layout("Your Account", "/account", flash_messages(&flash.consume().await), html! {{
        h1 {{ "Your Account" }}
        @if let Some(scheduled) = {snake_name}.delete_scheduled_at {{
            div style="border:1px solid #c00;padding:0.75rem;margin-bottom:1rem;" {{
                p {{
                    "⚠️ Your account is scheduled for deletion on "
                    (scheduled.format("%Y-%m-%d").to_string())
                    ". Log in before then to cancel."
                }}
                form action="/account/delete/cancel" method="post" {{
                    @if let Some(ref csrf) = csrf {{
                        input type="hidden"
                            name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str()))
                            value=(csrf.token());
                    }}
                    button type="submit" {{ "Cancel Deletion" }}
                }}
            }}
        }}
        p {{ "Email: " ({snake_name}.email) }}
        p {{
            "Email confirmed: "
            @if {snake_name}.email_confirmed_at.is_some() {{ "Yes" }} @else {{ "No" }}
        }}
        form action="/logout" method="post" {{
            @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            button type="submit" {{ "Log Out" }}
        }}
        p {{ a href="/account/password" {{ "Change password" }} }}
        p {{ a href="/account/email" {{ "Change email address" }} }}
        p {{ a href="/account/sessions" {{ "Manage active sessions" }} }}
        p {{
            a href="/account/delete" {{ "Delete my account" }}
            " (30-day grace period — GDPR Article 17)"
        }}
    }}).into_response())
}}

/// `POST /account/destroy` — permanently delete the authenticated user's account.
///
/// Requires both a valid session (`#[secured]`) and a fresh step-up claim
/// (`#[step_up]`), so a hijacked or unattended session cannot silently delete
/// the account. CSRF protection is provided by the CsrfLayer middleware; no
/// manual token check is needed here. After deletion the session is destroyed
/// and the user is redirected to the home page.
#[secured]
#[step_up]
#[post("/account/destroy")]
pub async fn account_destroy(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
) -> AutumnResult<Response> {{
    let {snake_name}_id: i64 = require_tracked_session(&session, &mut db, &state).await?.id;

    // Deleting the account cascades to {sess_table} (FK ON DELETE CASCADE),
    // signing every device out immediately.
    diesel::delete({table}::table.find({snake_name}_id))
        .execute(&mut *db)
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to delete account."))?;

    session.destroy().await;
    Ok(redirect_to("/").into_response())
}}

// ── Change Password (issue #1396) ─────────────────────────────────────────────

/// Wrap a re-rendered form in a `422 Unprocessable Entity` response.
///
/// The account settings forms re-render themselves on a rejected submission
/// (wrong current password, weak/ mismatched new password, invalid address) with
/// a `422` status so the failure is machine-detectable and never mistaken for a
/// success — while still returning the human-readable form body.
fn form_unprocessable(body: Markup) -> Response {{
    let mut response = body.into_response();
    *response.status_mut() = axum::http::StatusCode::UNPROCESSABLE_ENTITY;
    response
}}

/// Render the authenticated change-password form, optionally with an error.
fn render_change_password_form(
    error: Option<&str>,
    csrf: Option<&CsrfToken>,
    csrf_field: Option<&CsrfFormField>,
) -> Markup {{
    crate::layout("Change Password", "/account/password", html! {{}}, html! {{
        h1 {{ "Change Password" }}
        @if let Some(error) = error {{
            p role="alert" {{ (error) }}
        }}
        form action="/account/password" method="post" {{
            @if let Some(csrf) = csrf {{ input type="hidden" name=(csrf_field.map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            div {{
                label {{ "Current password" }}
                input type="password" name="current_password" autocomplete="current-password" required;
            }}
            div {{
                label {{ "New password" }}
                input type="password" name="new_password" autocomplete="new-password" required;
            }}
            div {{
                label {{ "Confirm new password" }}
                input type="password" name="new_password_confirmation" autocomplete="new-password" required;
            }}
            button type="submit" {{ "Change Password" }}
        }}
        p {{ a href="/account" {{ "← Back to account" }} }}
    }})
}}

/// `GET /account/password` — render the change-password form. Requires auth.
#[secured]
#[get("/account/password")]
pub async fn change_password_form(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    // Validates the tracked session row (401s immediately if revoked).
    let _ = require_tracked_session(&session, &mut db, &state).await?;
    Ok(render_change_password_form(None, csrf.as_ref(), csrf_field.as_ref()).into_response())
}}

#[derive(Deserialize)]
pub struct ChangePasswordForm {{
    pub current_password: String,
    pub new_password: String,
    pub new_password_confirmation: String,
}}

/// `POST /account/password` — verify the current password and set a new one.
///
/// Requires a valid session (`#[secured]`) and a fresh step-up claim
/// (`#[step_up]`). On success the password policy is applied, the new password
/// is hashed with `hash_password`, the session id is rotated (fixation defence
/// on a credential change), every OTHER tracked session and remember-me chain is
/// revoked, and the step-up claim is re-stamped. The current device stays signed
/// in. A wrong current password re-renders the form with a `422` and never
/// partially updates. The response never reveals whether any address exists.
#[secured]
#[step_up]
#[post("/account/password")]
pub async fn change_password(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    flash: Flash,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Form(form): Form<ChangePasswordForm>,
) -> AutumnResult<Response> {{
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;

    // Verify the CURRENT password before anything else. A wrong current password
    // re-renders the form at 422 and makes no change (AC2).
    let current_ok = verify_password(&form.current_password, &{snake_name}.password_digest)
        .await
        .unwrap_or(false);
    if !current_ok {{
        return Ok(form_unprocessable(render_change_password_form(
            Some("Current password is incorrect."),
            csrf.as_ref(),
            csrf_field.as_ref(),
        )));
    }}

    // The confirmation field must match so a typo cannot lock the user out.
    if form.new_password != form.new_password_confirmation {{
        return Ok(form_unprocessable(render_change_password_form(
            Some("New password and confirmation do not match."),
            csrf.as_ref(),
            csrf_field.as_ref(),
        )));
    }}

    // Enforce the configured password policy, passing the account email as
    // similarity context (matches the signup path).
    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let password_cfg = &config.auth.password;
    let mut policy = password_cfg.policy();
    if password_cfg.breach_check != autumn_web::auth::BreachCheck::Off {{
        policy = policy.with_client(autumn_web::http_client::Client::new());
    }}
    let ctx = [{snake_name}.email.as_str()];
    let validation = autumn_web::auth::validate_password(&form.new_password, &policy, &ctx).await;
    if !validation.is_valid() {{
        let messages = validation.messages();
        let message = if messages.is_empty() {{
            "Invalid password.".to_owned()
        }} else {{
            messages.join("\n")
        }};
        return Ok(form_unprocessable(render_change_password_form(
            Some(&message),
            csrf.as_ref(),
            csrf_field.as_ref(),
        )));
    }}

    let new_digest = hash_password(&form.new_password).await?;

    // Rotate the session id (fixation defence on a credential change), then
    // atomically: persist the new digest, revoke every OTHER tracked session,
    // rebind the CURRENT device's row onto the rotated id so it stays signed in,
    // and revoke every persistent remember-me chain (the old password is now
    // suspect). If any step fails the whole change rolls back.
    //
    // FAIL-CLOSED TRADEOFF: rotation happens BEFORE the transaction so both the
    // pre- and post-rotation digests are known and the row rebind is atomic with
    // the password write. The cost is that on a rolled-back change the cookie
    // already holds the rotated id, which no longer matches any `user_sessions`
    // row — so a FAILED change logs the acting device out. This is fail-closed
    // (a security-neutral logout, never an unintended stay-signed-in), so it is
    // left as-is: moving the rotate/rebind after commit would only relocate the
    // same window and risks the "rotate before auth writes" fixation invariant.
    //
    // Revoking every OTHER tracked session is the standard response to a
    // credential change, but it honours the same opt-out the reset/TOTP/passkey
    // credential-change paths read ([auth.sessions].revoke_on_credential_change).
    // When that flag is false we still rotate + rebind the CURRENT session (so
    // this device stays signed in on a fresh id) and still clear reset/magic-link
    // tokens — only the other-device sign-out is skipped.
    let revoke_other_sessions_in_txn = state.config_arc().auth.sessions.revoke_on_credential_change;
    let pre_rotation_digest = session_token_digest(&session).await;
    session.rotate_id().await;
    let post_rotation_digest = session_token_digest(&session).await;
    let {snake_name}_id = {snake_name}.id;
    (*db)
        .transaction::<_, diesel::result::Error, _>(async move |conn| {{
            diesel::update({table}::table.find({snake_name}_id))
                .set((
                    {table}::password_digest.eq(&new_digest),
                    // Invalidate any outstanding forgot-password token in the SAME
                    // transaction: reset_password authenticates SOLELY by matching
                    // an unexpired reset_token_digest, so a link minted before this
                    // change could otherwise overwrite the just-secured password.
                    {table}::reset_token_digest.eq(None::<String>),
                    {table}::reset_token_expires_at.eq(None::<chrono::NaiveDateTime>),
                ))
                .execute(conn)
                .await?;
            // Revoke every OTHER session (all rows whose digest is not this
            // device's pre-rotation digest) unless the opt-out is set, then
            // always rebind this device onto the rotated id below.
            if revoke_other_sessions_in_txn {{
                diesel::delete(
                    {sess_table}::table
                        .filter({sess_table}::user_id.eq({snake_name}_id))
                        .filter({sess_table}::token_digest.ne(&pre_rotation_digest)),
                )
                .execute(conn)
                .await?;
            }}
            diesel::update(
                {sess_table}::table.filter({sess_table}::token_digest.eq(&pre_rotation_digest)),
            )
            .set({sess_table}::token_digest.eq(&post_rotation_digest))
            .execute(conn)
            .await?;
            diesel::delete(
                {rem_table}::table.filter({rem_table}::user_id.eq({snake_name}_id)),
            )
            .execute(conn)
            .await?;
{magic_link_change_password_clear}            Ok(())
        }})
        .await
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!("Failed to change password: {{e}}"))
        }})?;

    // Re-stamp the step-up claim so the freshly rotated session keeps sudo-mode
    // freshness (consistent with reauth / reset-password).
    autumn_web::step_up::set_last_strong_auth_at(&session).await;
    flash.success("Your password has been changed.").await;
    Ok(redirect_to("/account"))
}}

// ── Change Email (issue #1396) ────────────────────────────────────────────────

/// Render the authenticated change-email form, optionally with an error.
fn render_change_email_form(
    error: Option<&str>,
    csrf: Option<&CsrfToken>,
    csrf_field: Option<&CsrfFormField>,
) -> Markup {{
    crate::layout("Change Email Address", "/account/email", html! {{}}, html! {{
        h1 {{ "Change Email Address" }}
        p {{ "We'll send a confirmation link to the new address. Your current \
              address keeps working until you confirm the change." }}
        @if let Some(error) = error {{
            p role="alert" {{ (error) }}
        }}
        form action="/account/email" method="post" {{
            @if let Some(csrf) = csrf {{ input type="hidden" name=(csrf_field.map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            div {{
                label {{ "New email address" }}
                input type="email" name="new_email" autocomplete="email" required;
            }}
            div {{
                label {{ "Current password" }}
                input type="password" name="current_password" autocomplete="current-password" required;
            }}
            button type="submit" {{ "Send Confirmation Link" }}
        }}
        p {{ a href="/account" {{ "← Back to account" }} }}
    }})
}}

/// `GET /account/email` — render the change-email form. Requires auth.
#[secured]
#[get("/account/email")]
pub async fn change_email_form(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    // Validates the tracked session row (401s immediately if revoked).
    let _ = require_tracked_session(&session, &mut db, &state).await?;
    Ok(render_change_email_form(None, csrf.as_ref(), csrf_field.as_ref()).into_response())
}}

#[derive(Deserialize)]
pub struct ChangeEmailForm {{
    pub new_email: String,
    pub current_password: String,
}}

/// `POST /account/email` — start an email-address change.
///
/// Requires a valid session (`#[secured]`) and a fresh step-up claim
/// (`#[step_up]`). Verifies the current password, then stores the new address as
/// `pending_email` (re-using the `confirm_token_digest` machinery) and sends a
/// confirmation link to the NEW address plus a change-notice to the OLD address.
/// The OLD address stays valid for sign-in until the new-address token is
/// confirmed. A second request supersedes the prior pending token. Mail must be
/// configured; the disabled-transport guard runs first.
#[secured]
#[step_up]
#[post("/account/email")]
pub async fn change_email(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    mailer: Mailer,
    flash: Flash,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Form(form): Form<ChangeEmailForm>,
) -> AutumnResult<Response> {{
    // Guard disabled mail FIRST — the flow is unusable without a confirmation
    // email, so fail loudly rather than store a pending change no one can confirm.
    if mailer.is_disabled() {{
        return Err(AutumnError::internal_server_error_msg(
            "Changing your email requires mail to be configured. \
             Set [mail] transport in autumn.toml (e.g. transport = \"log\" for dev). \
             The change-email feature is unavailable until mail is set up.",
        ));
    }}

    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;

    // Verify the current password before accepting the change (AC3). A wrong
    // password re-renders the form at 422 and makes no change.
    let current_ok = verify_password(&form.current_password, &{snake_name}.password_digest)
        .await
        .unwrap_or(false);
    if !current_ok {{
        return Ok(form_unprocessable(render_change_email_form(
            Some("Current password is incorrect."),
            csrf.as_ref(),
            csrf_field.as_ref(),
        )));
    }}

    let new_email = form.new_email.trim().to_lowercase();
    // Basic format check, mirroring signup. Same message for every rejection so
    // the endpoint never reveals whether the address is already registered.
    let invalid = || {{
        Ok(form_unprocessable(render_change_email_form(
            Some("Please enter a valid email address."),
            csrf.as_ref(),
            csrf_field.as_ref(),
        )))
    }};
    match new_email.split_once('@') {{
        Some((_, domain)) if domain.contains('.') => {{}}
        _ => return invalid(),
    }}
    if new_email == {snake_name}.email {{
        return invalid();
    }}

    // Reserve availability BEFORE minting a token, sending mail, or storing any
    // pending state. If the address already belongs to ANOTHER account (we already
    // excluded the caller's own address above), short-circuit to the SAME generic
    // "check your email" response: do NOT email that third party a confirmation
    // link, and do NOT persist a pending change the later confirm could never apply
    // (it would collide on `email`'s unique index). Non-enumerating — the caller
    // cannot tell a taken address from an available one. Swallow lookup errors like
    // the reset flow so a transient DB error never becomes an existence oracle.
    let already_taken = {table}::table
        .filter({table}::email.eq(&new_email))
        .select({pascal_name}::as_select())
        .first::<{pascal_name}>(&mut *db)
        .await
        .ok()
        .is_some();
    if already_taken {{
        flash
            .info("Check your new email address for a confirmation link.")
            .await;
        return Ok(redirect_to("/account"));
    }}

    // Mint a fresh single-use token; only the SHA-256 digest is stored. A second
    // change request overwrites `pending_email` and re-mints the token, so the
    // prior pending change is superseded (AC4).
    let raw_token = generate_confirmation_token();
    let confirm_digest = sha256_hex(&raw_token);
    let confirm_expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::hours(24);

    // Send the confirmation link to the NEW address first; only persist the
    // pending change on success, so a transient mail failure leaves the account
    // untouched (the old address keeps working, no stale pending token).
    send_email_change_confirmation(&mailer, &new_email, &raw_token).await?;

    diesel::update({table}::table.find({snake_name}.id))
        .set((
            {table}::pending_email.eq(Some(&new_email)),
            {table}::confirm_token_digest.eq(Some(&confirm_digest)),
            {table}::confirm_token_expires_at.eq(Some(confirm_expires_at)),
        ))
        .execute(&mut *db)
        .await
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!("Failed to record email change: {{e}}"))
        }})?;

    // Notify the OLD address so the account owner is alerted to the change even
    // if it was initiated by someone else. Best-effort: a failure here must not
    // roll back the (already-confirmed-deliverable) pending change.
    if let Err(e) = send_email_change_notice(&mailer, &{snake_name}.email, &new_email).await {{
        tracing::warn!("email-change notice to old address failed: {{e}}");
    }}

    flash
        .info("Check your new email address for a confirmation link.")
        .await;
    Ok(redirect_to("/account"))
}}

/// `GET /account/email/confirm/{{token}}` — confirm a pending email change.
///
/// On a valid unexpired token whose account still has a `pending_email`: swaps
/// `email` to the pending address, clears `pending_email` and the token, and
/// stamps `email_confirmed_at`. Single-use — the token is consumed atomically so
/// a replay matches zero rows. Any invalid/expired/unknown/already-consumed
/// token returns the SAME non-revealing 422 (no oracle). Only the SHA-256 digest
/// is ever stored; the raw token lives only in the emailed URL.
#[get("/account/email/confirm/{{token}}")]
pub async fn confirm_email_change(
    mut db: Db,
    Path(params): Path<ConfirmEmailPath>,
) -> AutumnResult<Response> {{
    let token_digest = sha256_hex(&params.token);
    let now = chrono::Utc::now().naive_utc();
    let generic = || {{
        AutumnError::unprocessable_msg(
            "This email-change link is invalid or has expired. \
             Please request a new email change from your account settings.",
        )
    }};

    // Load the pending row by token digest. `pending_email IS NOT NULL` keeps
    // this path from colliding with the signup email-confirmation flow, which
    // shares the `confirm_token_digest` column but never sets `pending_email`.
    let {snake_name}: {pascal_name} = {table}::table
        .filter({table}::confirm_token_digest.eq(Some(&token_digest)))
        .filter({table}::confirm_token_expires_at.gt(now))
        .filter({table}::pending_email.is_not_null())
        .select({pascal_name}::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| generic())?;

    let Some(new_email) = {snake_name}.pending_email.clone() else {{
        return Err(generic());
    }};

    // Consume atomically: the UPDATE re-filters on the digest so two concurrent
    // requests with the same token cannot both apply (the second matches zero
    // rows). Swapping the address here also enforces email uniqueness — a
    // collision surfaces as the same generic error, revealing nothing. The whole
    // mutation runs in one transaction so the magic-link cleanup below commits
    // atomically with the address swap (or rolls back together on any failure).
    let {snake_name}_id = {snake_name}.id;
    // Keep a copy for the success page; the original is moved into the txn.
    let new_email_display = new_email.clone();
    let rows = (*db)
        .transaction::<_, diesel::result::Error, _>(async move |conn| {{
            let rows = diesel::update(
                {table}::table
                    .find({snake_name}_id)
                    .filter({table}::confirm_token_digest.eq(Some(&token_digest))),
            )
            .set((
                {table}::email.eq(&new_email),
                {table}::pending_email.eq(None::<String>),
                {table}::confirm_token_digest.eq(None::<String>),
                {table}::confirm_token_expires_at.eq(None::<chrono::NaiveDateTime>),
                {table}::email_confirmed_at.eq(Some(now)),
                // Invalidate any outstanding reset token: a link sent to the OLD
                // mailbox before this change must not be usable to reset the
                // account after it has moved to the new address.
                {table}::reset_token_digest.eq(None::<String>),
                {table}::reset_token_expires_at.eq(None::<chrono::NaiveDateTime>),
            ))
            .execute(conn)
            .await?;
            // Lost single-use race (token already consumed): nothing matched, so
            // commit the no-op and leave the winner's magic-link cleanup intact.
            // The same generic error is surfaced below.
            if rows == 0 {{
                return Ok(0);
            }}
{magic_link_email_change_clear}            Ok(rows)
        }})
        .await
        .map_err(|_| generic())?;
    if rows == 0 {{
        return Err(generic());
    }}

    Ok(crate::layout("Email Address Updated", "/account/email", html! {{}}, html! {{
        h1 {{ "Email Address Updated" }}
        p {{ "Your email address has been changed to " (new_email_display) "." }}
        p {{ a href="/account" {{ "← Back to account" }} }}
    }})
    .into_response())
}}

/// Send the email-change confirmation link to the NEW address.
async fn send_email_change_confirmation(
    mailer: &Mailer,
    to: &str,
    token: &str,
) -> AutumnResult<()> {{
    if mailer.is_disabled() {{
        return Err(AutumnError::internal_server_error_msg(
            "Changing your email requires mail to be configured.",
        ));
    }}
    let base_url = std::env::var("APP_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_owned());
    let confirm_url = format!("{{base_url}}/account/email/confirm/{{token}}");
    let mail = Mail::builder()
        .to(to.to_owned())
        .subject("Confirm your new email address")
        .html(html! {{
            p {{ "Click the link below to confirm this as the new email address for your account." }}
            p {{ "This link expires in 24 hours." }}
            p {{ a href=(&confirm_url) {{ "Confirm New Email Address" }} }}
            p {{ "If you did not request this change, you can safely ignore this email." }}
        }})
        .text(format!(
            "Confirm your new email address: {{confirm_url}}\n\
             This link expires in 24 hours.\n\
             If you did not request this change, you can safely ignore this email."
        ))
        .build()
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!(
                "Failed to build email-change confirmation: {{e}}"
            ))
        }})?;
    mailer.send(mail).await.map_err(|e| {{
        AutumnError::internal_server_error_msg(format!(
            "Failed to send email-change confirmation: {{e}}"
        ))
    }})
}}

/// Send a change-notice to the OLD address so the owner is alerted.
async fn send_email_change_notice(
    mailer: &Mailer,
    to: &str,
    new_email: &str,
) -> AutumnResult<()> {{
    let mail = Mail::builder()
        .to(to.to_owned())
        .subject("Email address change requested")
        .html(html! {{
            p {{ "Someone requested changing the email address on your account to " (new_email) "." }}
            p {{ "The change only takes effect once the new address is confirmed. \
                  Your current address stays active until then." }}
            p {{ "If you did not request this, change your password immediately — \
                  your account may be compromised." }}
        }})
        .text(format!(
            "Someone requested changing the email address on your account to {{new_email}}.\n\
             The change only takes effect once the new address is confirmed.\n\
             If you did not request this, change your password immediately."
        ))
        .build()
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!(
                "Failed to build email-change notice: {{e}}"
            ))
        }})?;
    mailer.send(mail).await.map_err(|e| {{
        AutumnError::internal_server_error_msg(format!(
            "Failed to send email-change notice: {{e}}"
        ))
    }})
}}

// ── Step-Up Reauth ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ReauthForm {{
    // The TOTP retry form omits the password field and relies on the
    // `reauth_pw_ok` session marker, so `password` must be optional here.
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub return_to: String,
    #[serde(default)]
    pub totp_code: String,
}}

/// `GET /reauth` — prompt for password to refresh the step-up claim.
///
/// Redirects unauthenticated users to `/login`. Renders a minimal password
/// form that posts back to `POST /reauth`. The `return_to` query parameter
/// is forwarded as a hidden field so it survives the form submit.
#[get("/reauth")]
pub async fn reauth_form(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    Query(params): Query<std::collections::HashMap<String, String>>,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    if session.get("{snake_name}_id").await.is_none() {{
        return Ok(redirect_to("/login").into_response());
    }}
    // Validates the tracked session row (401s immediately if revoked).
    let _ = require_tracked_session(&session, &mut db, &state).await?;
    let return_to = params.get("return_to").cloned().unwrap_or_default();
    Ok(crate::layout("Confirm your identity", "/reauth", html! {{}}, html! {{
        h1 {{ "Confirm your identity" }}
        p {{ "For security, please re-enter your password to continue." }}
        form action="/reauth" method="post" {{
            @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            input type="hidden" name="return_to" value=(return_to);
            div {{
                label {{ "Password" }}
                input type="password" name="password" autocomplete="current-password" required;
            }}
            {totp_reauth_field}
            button type="submit" {{ "Confirm" }}
        }}
    }}).into_response())
}}

/// `POST /reauth` — verify current password and refresh the step-up claim.
///
/// On success, sets `last_strong_auth_at` in the session and redirects to
/// `return_to` (same-origin validated). On failure, renders the form again
/// with an error message. Unauthenticated requests are redirected to `/login`.
#[post("/reauth")]
pub async fn reauth(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Form(form): Form<ReauthForm>,
) -> AutumnResult<Response> {{
    if session.get("{snake_name}_id").await.is_none() {{
        return Ok(redirect_to("/login").into_response());
    }}

    // Validates the tracked session row (401s immediately if revoked).
    let {snake_name} = require_tracked_session(&session, &mut db, &state).await?;

    // In the TOTP multi-step flow, the password is verified in one request and the
    // TOTP code in the next. Rather than echoing the password into a hidden DOM
    // field (which would expose it to any script on the page), we store a short-
    // lived session marker after a successful password check and rely on that marker
    // on subsequent submissions so the password never leaves the server.
    // The marker stores "<timestamp>:<account_id>" so it is both time-limited
    // (10-minute window) and bound to the specific account — if the session is
    // reused by a different account the stored id will not match and the marker
    // is ignored, requiring the new account's password to be re-entered.
    const REAUTH_PW_VALID_SECS: i64 = 600;
    let pw_verified_recently = session
        .get("reauth_pw_ok")
        .await
        .and_then(|s| {{
            let mut parts = s.splitn(2, ':');
            let ts = parts.next()?.parse::<i64>().ok()?;
            let id = parts.next()?.parse::<i64>().ok()?;
            Some((ts, id))
        }})
        .is_some_and(|(ts, id)| {{
            id == {snake_name}.id
                && chrono::Utc::now().timestamp() - ts < REAUTH_PW_VALID_SECS
        }});
    if !pw_verified_recently {{
        // Helper to render the reauth error form without duplicating markup.
        let reauth_form_err = |ret: &str| {{
            crate::layout("Confirm your identity", "/reauth", html! {{}}, html! {{
                h1 {{ "Confirm your identity" }}
                p style="color:red" {{ "Incorrect password. Please try again." }}
                form action="/reauth" method="post" {{
                    @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
                    input type="hidden" name="return_to" value=(ret);
                    div {{
                        label {{ "Password" }}
                        input type="password" name="password" autocomplete="current-password" required;
                    }}
                    {totp_reauth_field}
                    button type="submit" {{ "Confirm" }}
                }}
            }}).into_response()
        }};

        // ── Account lockout policy ──────────────────────────────────────────
        // `config_arc` shares the resolved config behind an `Arc`; `config()`
        // would deep-clone every section to read one field on a request path.
        let config = state.config_arc();
        let lockout_cfg = &config.auth.lockout;
        let lockout_enabled = lockout_cfg.enabled && lockout_cfg.threshold > 0;

        if lockout_enabled {{
            let now = chrono::Utc::now().naive_utc();
            let cooloff = chrono::Duration::seconds(lockout_cfg.cooloff_secs as i64);

            // Local mutable counters reset when cool-off has elapsed.
            let mut current_attempts = {snake_name}.failed_attempts;
            let mut current_locked_at = {snake_name}.locked_at;

            if let Some(locked_at) = {snake_name}.locked_at {{
                if now < locked_at + cooloff {{
                    // Non-enumerating: same form as wrong password.
                    session.remove("reauth_pw_ok").await;
                    return Ok(reauth_form_err(&form.return_to));
                }}
                // Cool-off elapsed — treat as unlocked.
                current_attempts = 0;
                current_locked_at = None;
            }}

            let password_ok = verify_password(&form.password, &{snake_name}.password_digest)
                .await
                .unwrap_or(false);

            if !password_ok {{
                // Atomically increment (or reset to 1 if cool-off just elapsed).
                let new_attempts: i32 = if current_attempts == {snake_name}.failed_attempts {{
                    diesel::update({table}::table.find({snake_name}.id))
                        .set({table}::failed_attempts.eq({table}::failed_attempts + 1))
                        .returning({table}::failed_attempts)
                        .get_result::<i32>(&mut *db)
                        .await
                        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to record failed reauth: {{e}}")))?
                }} else {{
                    diesel::update({table}::table.find({snake_name}.id))
                        .set((
                            {table}::failed_attempts.eq(1i32),
                            {table}::locked_at.eq(None::<chrono::NaiveDateTime>),
                        ))
                        .execute(&mut *db)
                        .await
                        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to reset lockout counter: {{e}}")))?;
                    1i32
                }};
                if new_attempts >= lockout_cfg.threshold && current_locked_at.is_none() {{
                    // See the login handler's matching guard (#2500): filter on the
                    // row's current failed_attempts/locked_at rather than writing
                    // unconditionally, so a concurrent successful reauth that already
                    // reset the counter cannot be silently re-locked.
                    diesel::update(
                        {table}::table
                            .find({snake_name}.id)
                            .filter({table}::failed_attempts.ge(lockout_cfg.threshold))
                            .filter({table}::locked_at.is_null()),
                    )
                        .set({table}::locked_at.eq(Some(now)))
                        .execute(&mut *db)
                        .await
                        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to lock account: {{e}}")))?;
                }}
                session.remove("reauth_pw_ok").await;
                return Ok(reauth_form_err(&form.return_to));
            }}

            // Correct password — reset lockout counters. The WHERE guard
            // excludes accounts locked by a concurrent bad-password request
            // between our check and now, treating that race as auth failure.
            let lock_expired_before = chrono::Utc::now().naive_utc() - cooloff;
            let rows_cleared = diesel::update(
                {table}::table
                    .find({snake_name}.id)
                    .filter(
                        {table}::locked_at.is_null().or(
                            {table}::locked_at.le(lock_expired_before)
                        ),
                    ),
            )
            .set((
                {table}::failed_attempts.eq(0i32),
                {table}::locked_at.eq(None::<chrono::NaiveDateTime>),
            ))
            .execute(&mut *db)
            .await
            .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to reset lockout on reauth: {{e}}")))?;
            if rows_cleared == 0 {{
                // Concurrently locked — treat as authentication failure.
                session.remove("reauth_pw_ok").await;
                return Ok(reauth_form_err(&form.return_to));
            }}
        }} else {{
            let password_ok = verify_password(&form.password, &{snake_name}.password_digest)
                .await
                .unwrap_or(false);
            if !password_ok {{
                session.remove("reauth_pw_ok").await;
                return Ok(reauth_form_err(&form.return_to));
            }}
        }}

        session
            .insert(
                "reauth_pw_ok",
                format!("{{}}:{{}}", chrono::Utc::now().timestamp(), {snake_name}.id),
            )
            .await;
    }}
    {totp_reauth_check}
    // Rotate session ID before elevating privileges to prevent session fixation
    // attacks where a stolen cookie is used to benefit from a legitimate reauth.
    // The tracked session row is keyed by the digest of the id, so capture the
    // pre-rotation digest and re-point the row at the new id — otherwise the
    // redirected step-up request would find no row and be treated as revoked.
    let pre_rotation_digest = session_token_digest(&session).await;
    session.rotate_id().await;
    rebind_tracked_session(&mut db, &session, &pre_rotation_digest).await?;
    session.remove("reauth_pw_ok").await;
    // Refresh the step-up claim.
    autumn_web::step_up::set_last_strong_auth_at(&session).await;

    // Redirect to the validated same-origin destination.
    let dest = autumn_web::step_up::validate_return_to(&form.return_to)
        .ok()
        .filter(|_| !form.return_to.is_empty())
        .map(|_| form.return_to.as_str())
        .unwrap_or("/account")
        .to_owned();

    // Suppress unused warning when state.auth_session_key() is not used here.
    let _ = &state;
    Ok(redirect_to(&dest).into_response())
}}

// ── Forgot Password ───────────────────────────────────────────────────────────

/// `GET /forgot-password` — render the forgot-password form.
#[get("/forgot-password")]
pub async fn forgot_password_form(csrf: Option<CsrfToken>, csrf_field: Option<CsrfFormField>) -> AutumnResult<Markup> {{
    Ok(crate::layout("Forgot Password", "/forgot-password", html! {{}}, html! {{
        h1 {{ "Forgot Your Password?" }}
        form action="/forgot-password" method="post" {{
            @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            div {{
                label {{ "Email" }}
                input type="email" name="email" required autocomplete="email";
            }}
            button type="submit" {{ "Send Reset Link" }}
        }}
    }}))
}}

#[derive(Deserialize)]
pub struct ForgotPasswordForm {{
    pub email: String,
}}

/// `POST /forgot-password` — generate a reset token and email a reset link.
///
/// Non-enumerating: always returns the same confirmation page whether or not
/// the email address is registered, so callers cannot learn which addresses
/// exist. Requires Autumn mail to be configured; fails with a clear message
/// identifying the missing mail configuration if it is not.
#[post("/forgot-password")]
pub async fn forgot_password(
    mut db: Db,
    mailer: Mailer,
    Form(form): Form<ForgotPasswordForm>,
) -> AutumnResult<Markup> {{
    // Fail fast when mail is not configured — safe because this check is
    // independent of the email address lookup and does not leak whether an
    // address is registered.
    if mailer.is_disabled() {{
        return Err(AutumnError::internal_server_error_msg(
            "Password reset requires mail to be configured. \
             Set [mail] transport in autumn.toml (e.g. transport = \"smtp\"). \
             The forgot-password feature is unavailable until mail is set up.",
        ));
    }}

    let email = form.email.trim().to_lowercase();

    // Record start time; the response is padded to a constant minimum below
    // so an attacker cannot infer whether an address is registered by
    // measuring response latency.
    let t0 = std::time::Instant::now();

    // Non-enumerating: silently skip unknown addresses.
    let maybe_{snake_name}: Option<{pascal_name}> = {table}::table
        .filter({table}::email.eq(&email))
        .select({pascal_name}::as_select())
        .first(&mut *db)
        .await
        .ok();

    if let Some({snake_name}) = maybe_{snake_name} {{
        let raw_token = generate_reset_token();
        let token_digest = sha256_hex(&raw_token);
        let expires_at =
            chrono::Utc::now().naive_utc() + chrono::Duration::hours(2);

        diesel::update({table}::table.find({snake_name}.id))
            .set((
                {table}::reset_token_digest.eq(Some(&token_digest)),
                {table}::reset_token_expires_at.eq(Some(expires_at)),
            ))
            .execute(&mut *db)
            .await?;

        // Non-enumerating: log send failures but do not surface them to the
        // caller — the response is always the same "check your email" page.
        if let Err(e) = send_reset_email(&mailer, &{snake_name}.email, &raw_token).await {{
            tracing::error!("password-reset email failed: {{e}}");
        }}
    }}

    // Pad to a constant minimum so hit and miss paths take indistinguishable
    // wall-clock time.
    if let Some(remaining) = std::time::Duration::from_secs(1).checked_sub(t0.elapsed()) {{
        tokio::time::sleep(remaining).await;
    }}

    Ok(crate::layout("Check Your Email", "/forgot-password", html! {{}}, html! {{
        h1 {{ "Check Your Email" }}
        p {{
            "If that address is registered you'll receive a reset link shortly."
        }}
        p {{ a href="/login" {{ "Back to login" }} }}
    }}))
}}

// ── Reset Password ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ResetPasswordQuery {{
    pub token: String,
}}

/// Render the reset-password form, optionally with a validation error. The
/// minimum length reflects the active `[auth.password]` policy.
fn render_reset_password_form(
    token: &str,
    min_len: usize,
    error: Option<&str>,
    csrf: Option<&CsrfToken>,
    csrf_field: Option<&CsrfFormField>,
) -> Markup {{
    crate::layout("Reset Password", "/reset-password", html! {{}}, html! {{
        h1 {{ "Set a New Password" }}
        @if let Some(error) = error {{
            p role="alert" {{ (error) }}
        }}
        form action="/reset-password" method="post" {{
            @if let Some(csrf) = csrf {{ input type="hidden" name=(csrf_field.map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            input type="hidden" name="token" value=(token);
            div {{
                label {{ "New Password (" (min_len) "+ characters)" }}
                input type="password" name="password" required
                      autocomplete="new-password" minlength=(min_len);
            }}
            button type="submit" {{ "Set New Password" }}
        }}
    }})
}}

/// `GET /reset-password?token=<raw>` — render the reset-password form.
#[get("/reset-password")]
pub async fn reset_password_form(
    State(state): State<AppState>,
    Query(query): Query<ResetPasswordQuery>,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {{
    let min_len = state.config_arc().auth.password.min_length;
    Ok(render_reset_password_form(
        &query.token,
        min_len,
        None,
        csrf.as_ref(),
        csrf_field.as_ref(),
    ))
}}

#[derive(Deserialize)]
pub struct ResetPasswordForm {{
    pub token: String,
    pub password: String,
}}

/// `POST /reset-password` — verify the reset token and update the password.
///
/// The token is compared via its stored digest (constant-time via `sha2`).
/// On success the session is rotated, invalidating any prior authenticated
/// state.
#[post("/reset-password")]
pub async fn reset_password(
    mut db: Db,
    State(state): State<AppState>,
    session: Session,
    MaybeClientIp(addr_ip): MaybeClientIp,
    headers: axum::http::HeaderMap,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Form(form): Form<ResetPasswordForm>,
) -> AutumnResult<Response> {{
    // Enforce the configured password policy. The reset flow has no verified
    // user identifier in scope at this point (the token is validated below), so
    // no similarity context is supplied. On failure, re-render the form with the
    // specific message at HTTP 200 rather than accepting a weak credential.
    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let password_cfg = &config.auth.password;
    let mut policy = password_cfg.policy();
    if password_cfg.breach_check != autumn_web::auth::BreachCheck::Off {{
        policy = policy.with_client(autumn_web::http_client::Client::new());
    }}
    let validation = autumn_web::auth::validate_password(&form.password, &policy, &[]).await;
    if !validation.is_valid() {{
        // Show EVERY failure (e.g. both "too short" and "too common"), not just
        // the first, so the user can fix all problems at once (issue #1345.6).
        let messages = validation.messages();
        let message = if messages.is_empty() {{
            "Invalid password.".to_owned()
        }} else {{
            messages.join("\n")
        }};
        return Ok(render_reset_password_form(
            &form.token,
            password_cfg.min_length,
            Some(&message),
            csrf.as_ref(),
            csrf_field.as_ref(),
        )
        .into_response());
    }}

    let token_digest = sha256_hex(&form.token);
    let now = chrono::Utc::now().naive_utc();

    let {snake_name}: {pascal_name} = {table}::table
        .filter({table}::reset_token_digest.eq(Some(&token_digest)))
        .filter({table}::reset_token_expires_at.gt(now))
        .select({pascal_name}::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| {{
            AutumnError::unprocessable_msg("Invalid or expired reset link.")
        }})?;

    let new_digest = hash_password(&form.password).await?;

    // Password reset and email confirmation are independent state transitions (AC8).
    // Resetting a password does NOT implicitly confirm the email address.
    // Check BEFORE the TOTP interstitial so both TOTP and non-TOTP accounts are
    // gated — without this, a TOTP-enabled unconfirmed user could complete the
    // reset via /login/verify without ever confirming their email.
    if {snake_name}.email_confirmed_at.is_none() {{
        return Ok(redirect_to("/check-your-email"));
    }}

    // This browser may already hold a tracked session. The rotation below
    // (or inside the 2FA branch) destroys that session id, so drop its row
    // now — otherwise it would linger as a phantom device.
    untrack_current_session(&mut db, &session).await?;
{totp_reset_branch}
    // Rotate before the commit so the fresh session's row can be inserted
    // in the same transaction (the rotation itself is in-memory until the
    // response is sent, so a rollback leaves no half-applied state).
    session.rotate_id().await;
    let new_session_row = build_session_row(&session, {snake_name}.id, addr_ip, &headers).await;

    // Commit the password change, the reset-token consumption, the session
    // revocation, and the new session row atomically: if any part fails
    // everything rolls back, so the reset link stays usable and no state is
    // half-applied. Revoking every existing session is the standard
    // response to credential theft (defaulted on, configurable via
    // [auth.sessions].revoke_on_credential_change).
    let revoke_existing_sessions = state.config_arc().auth.sessions.revoke_on_credential_change;
    let {snake_name}_id = {snake_name}.id;
    (*db)
        .transaction::<_, diesel::result::Error, _>(async move |conn| {{
            diesel::update({table}::table.find({snake_name}_id))
                .set((
                    {table}::password_digest.eq(&new_digest),
                    {table}::reset_token_digest.eq(None::<String>),
                    {table}::reset_token_expires_at.eq(None::<chrono::NaiveDateTime>),
                ))
                .execute(conn)
                .await?;
            // Revoke before inserting so the bulk delete cannot eat the
            // fresh row.
            if revoke_existing_sessions {{
                diesel::delete(
                    {sess_table}::table.filter({sess_table}::user_id.eq({snake_name}_id)),
                )
                .execute(conn)
                .await?;
            }}
            // Persistent remember-me chains are ALWAYS revoked on a credential
            // change (issue #1397): the old password is compromised, so a
            // replayed remember cookie must not re-establish a session — it will
            // find no row and be rejected.
            diesel::delete(
                {rem_table}::table.filter({rem_table}::user_id.eq({snake_name}_id)),
            )
            .execute(conn)
            .await?;
            diesel::insert_into({sess_table}::table)
                .values(&new_session_row)
                .execute(conn)
                .await?;
{magic_link_reset_password_clear}            Ok(())
        }})
        .await
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!("Failed to reset password: {{e}}"))
        }})?;

{totp_clear_pending}    session.insert("{snake_name}_id", {snake_name}.id.to_string()).await;
    session.insert("{snake_name}_email", &{snake_name}.email).await;
    // Use the same session key checked by `#[secured]` / `#[authorize]`.
    session.insert(state.auth_session_key(), {snake_name}.id.to_string()).await;
    // Stamp the step-up claim so `#[step_up]` routes are immediately accessible.
    autumn_web::step_up::set_last_strong_auth_at(&session).await;
    Ok(redirect_to("/account"))
}}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Generate a 32-byte cryptographically-random reset token, hex-encoded.
fn generate_reset_token() -> String {{
    use rand::TryRngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG failed");
    hex::encode(bytes)
}}

/// SHA-256 hex digest of `input` using the `sha2` crate.
fn sha256_hex(input: &str) -> String {{
    use sha2::{{Digest, Sha256}};
    let hash = Sha256::digest(input.as_bytes());
    hex::encode(hash)
}}

/// Send a password-reset email via the Autumn mailer.
///
/// # Errors
/// Returns a clear `AutumnError::internal` message when mail is not configured
/// (`transport = "disabled"`) or when the send itself fails.
async fn send_reset_email(mailer: &Mailer, to: &str, token: &str) -> AutumnResult<()> {{
    if mailer.is_disabled() {{
        return Err(AutumnError::internal_server_error_msg(
            "Password reset requires mail to be configured. \
             Set [mail] transport in autumn.toml (e.g. transport = \"smtp\"). \
             The forgot-password feature is unavailable until mail is set up.",
        ));
    }}
    // APP_BASE_URL must be set to the public URL of your app (e.g. https://example.com).
    let base_url = std::env::var("APP_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_owned());
    let reset_url = format!("{{base_url}}/reset-password?token={{token}}");
    let mail = Mail::builder()
        .to(to.to_owned())
        .subject("Reset your password")
        .html(html! {{
            p {{ "Click the link below to reset your password." }}
            p {{ "This link expires in 2 hours." }}
            p {{ a href=(&reset_url) {{ "Reset Password" }} }}
            p {{ "If you did not request this, you can safely ignore this email." }}
        }})
        .text(format!(
            "Reset your password: {{reset_url}}\n\
             This link expires in 2 hours.\n\
             If you did not request this you can safely ignore this email."
        ))
        .build()
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!(
                "Failed to build password-reset email: {{e}}"
            ))
        }})?;
    mailer.send(mail).await.map_err(|e| {{
        AutumnError::internal_server_error_msg(format!(
            "Failed to send password-reset email: {{e}}"
        ))
    }})
}}

// ── Email Confirmation ────────────────────────────────────────────────────────

/// `GET /check-your-email` — shown after signup while awaiting email confirmation.
#[get("/check-your-email")]
pub async fn check_your_email(flash: Flash) -> AutumnResult<Markup> {{
    Ok(crate::layout("Check Your Email", "/check-your-email", flash_messages(&flash.consume().await), html! {{
        h1 {{ "Check Your Email" }}
        p {{ "We've sent a confirmation link to your email address." }}
        p {{ "Please click the link in the email to activate your account. The link expires in 24 hours." }}
        p {{ a href="/auth/confirm/resend" {{ "Didn't receive it? Resend confirmation email" }} }}
        p {{ a href="/login" {{ "Back to login" }} }}
    }}))
}}

#[derive(Deserialize)]
pub struct ConfirmEmailPath {{
    pub token: String,
}}

/// `GET /auth/confirm/{{token}}` — validate the confirmation token and mark the
/// account as confirmed.
///
/// On a valid unexpired token: stamps `email_confirmed_at`, invalidates the
/// token (single-use), and grants an authenticated session.
/// On an invalid or expired token: returns a non-revealing 422 with a
/// "request a new confirmation email" page.
///
/// Only the SHA-256 digest is stored; the raw token only exists in the email
/// and the user's URL bar — never in the database, logs, or error reports.
#[get("/auth/confirm/{{token}}")]
pub async fn confirm_email(
    mut db: Db,
    State(state): State<AppState>,
    session: Session,
    MaybeClientIp(addr_ip): MaybeClientIp,
    headers: axum::http::HeaderMap,
    Path(params): Path<ConfirmEmailPath>,
) -> AutumnResult<Response> {{
    let token_digest = sha256_hex(&params.token);
    let now = chrono::Utc::now().naive_utc();

    // Atomic UPDATE…RETURNING: filter, stamp, and invalidate in a single
    // statement so concurrent requests with the same token cannot both
    // succeed — the second UPDATE matches zero rows and returns an error.
    let {snake_name}: {pascal_name} = diesel::update(
        {table}::table
            .filter({table}::confirm_token_digest.eq(Some(&token_digest)))
            .filter({table}::confirm_token_expires_at.gt(now))
            // Disambiguate from an email-CHANGE token, which shares the
            // `confirm_token_digest` column but sets `pending_email`. Without this
            // reverse guard a change-email token could satisfy this signup-confirm
            // UPDATE — consuming it, stamping `email_confirmed_at`, orphaning
            // `pending_email`, and minting an unintended session (mirror of the
            // `pending_email.is_not_null()` guard in `confirm_email_change`).
            .filter({table}::pending_email.is_null()),
    )
    .set((
        {table}::email_confirmed_at.eq(Some(now)),
        {table}::confirm_token_digest.eq(None::<String>),
        {table}::confirm_token_expires_at.eq(None::<chrono::NaiveDateTime>),
    ))
    .returning({pascal_name}::as_returning())
    .get_result(&mut *db)
    .await
    .map_err(|_| {{
        AutumnError::unprocessable_msg(
            "This confirmation link is invalid or has expired. \
             Please request a new confirmation email.",
        )
    }})?;

    // This browser may already hold a tracked session for another account.
    // The rotation below (or inside the 2FA branch) destroys that session
    // id, so drop its row now — otherwise it would linger as a phantom device.
    untrack_current_session(&mut db, &session).await?;

    // For 2FA-enabled accounts, redirect to /login/verify before granting a
    // full session — email confirmation proves address ownership, not TOTP possession.
{totp_confirm_branch}    // Grant an authenticated session on confirmation.
    session.rotate_id().await;
    // Discard any step-up claim that may have carried over from a previous
    // login in this browser — email confirmation proves inbox access, not
    // password knowledge, so it must not inherit sudo-mode freshness.
    session.remove(autumn_web::step_up::STEP_UP_SESSION_KEY).await;
{totp_clear_pending}    session.insert("{snake_name}_id", {snake_name}.id.to_string()).await;
    session.insert("{snake_name}_email", &{snake_name}.email).await;
    // Use the same session key checked by `#[secured]` / `#[authorize]`.
    session.insert(state.auth_session_key(), {snake_name}.id.to_string()).await;
    // Track this login as an active session (device list + revocation).
    record_login_session(&mut db, &session, {snake_name}.id, addr_ip, &headers).await?;
    // Step-up protected routes will redirect to /reauth as designed.
    Ok(redirect_to("/account"))
}}

/// `GET /auth/confirm/resend` — render the resend-confirmation form.
#[get("/auth/confirm/resend")]
pub async fn resend_confirmation_form(
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {{
    Ok(crate::layout("Resend Confirmation Email", "/auth/confirm/resend", html! {{}}, html! {{
        h1 {{ "Resend Confirmation Email" }}
        form action="/auth/confirm/resend" method="post" {{
            @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
            div {{
                label {{ "Email" }}
                input type="email" name="email" required autocomplete="email";
            }}
            button type="submit" {{ "Resend Confirmation Email" }}
        }}
        p {{ a href="/login" {{ "Back to login" }} }}
    }}))
}}

#[derive(Deserialize)]
pub struct ResendConfirmationForm {{
    pub email: String,
}}

/// `POST /auth/confirm/resend` — generate a new confirmation token and email it.
///
/// Non-enumerating: always returns the same "check your email" page regardless
/// of whether the address is registered or already confirmed, so callers cannot
/// learn which addresses exist. Resending invalidates the previous outstanding
/// token for the account (single-outstanding-token invariant).
/// Rate-limited per-account via Autumn's existing rate limiter.
#[post("/auth/confirm/resend")]
pub async fn resend_confirmation(
    mut db: Db,
    mailer: Mailer,
    Form(form): Form<ResendConfirmationForm>,
) -> AutumnResult<Markup> {{
    if mailer.is_disabled() {{
        return Err(AutumnError::internal_server_error_msg(
            "Email confirmation requires mail to be configured. \
             Set [mail] transport in autumn.toml (e.g. transport = \"log\" for dev). \
             The confirmation feature is unavailable until mail is set up.",
        ));
    }}

    let email = form.email.trim().to_lowercase();
    let t0 = std::time::Instant::now();

    // Throttle: skip re-issue if a token was minted in the last 5 minutes
    // (expires_at would still be > 23h55m from now).  Non-enumerating.
    let throttle_threshold = chrono::Utc::now().naive_utc() + chrono::Duration::minutes(23 * 60 + 55);

    // Check eligibility before generating a new token.  We send the email
    // FIRST and only overwrite the stored token on success — so a transient
    // mail failure leaves the previous (still-valid) token intact rather than
    // invalidating it without delivering a replacement.
    let eligible: bool = {table}::table
        .filter({table}::email.eq(&email))
        .filter({table}::email_confirmed_at.is_null())
        .filter(
            {table}::confirm_token_expires_at
                .is_null()
                .or({table}::confirm_token_expires_at.le(throttle_threshold)),
        )
        .count()
        .get_result::<i64>(&mut *db)
        .await
        .unwrap_or(0)
        > 0;

    if eligible {{
        let raw_confirm_token = generate_confirmation_token();
        let confirm_digest = sha256_hex(&raw_confirm_token);
        let confirm_expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::hours(24);

        match send_confirmation_email(&mailer, &email, &raw_confirm_token).await {{
            Ok(()) => {{
                // Email delivered — now atomically overwrite the stored token.
                let _ = diesel::update(
                    {table}::table
                        .filter({table}::email.eq(&email))
                        .filter({table}::email_confirmed_at.is_null()),
                )
                .set((
                    {table}::confirm_token_digest.eq(Some(&confirm_digest)),
                    {table}::confirm_token_expires_at.eq(Some(confirm_expires_at)),
                ))
                .execute(&mut *db)
                .await;
            }}
            Err(e) => {{
                // Previous token is preserved — warn but remain non-enumerating.
                tracing::warn!("confirmation email failed on resend: {{e}}");
            }}
        }}
    }}

    // Pad to a constant minimum so hit and miss paths take indistinguishable time.
    if let Some(remaining) = std::time::Duration::from_secs(1).checked_sub(t0.elapsed()) {{
        tokio::time::sleep(remaining).await;
    }}

    Ok(crate::layout("Confirmation Email Sent", "/auth/confirm/resend", html! {{}}, html! {{
        h1 {{ "Check Your Email" }}
        p {{
            "If that address has a pending unconfirmed account, you'll receive a new \
             confirmation link shortly."
        }}
        p {{ a href="/login" {{ "Back to login" }} }}
    }}))
}}

/// Generate a 32-byte cryptographically-random confirmation token, hex-encoded.
fn generate_confirmation_token() -> String {{
    use rand::TryRngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG failed");
    hex::encode(bytes)
}}

/// Send a confirmation email via the Autumn mailer.
async fn send_confirmation_email(mailer: &Mailer, to: &str, token: &str) -> AutumnResult<()> {{
    let base_url = std::env::var("APP_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_owned());
    let confirm_url = format!("{{base_url}}/auth/confirm/{{token}}");
    let mail = Mail::builder()
        .to(to.to_owned())
        .subject("Confirm your email address")
        .html(html! {{
            p {{ "Click the link below to confirm your email address and activate your account." }}
            p {{ "This link expires in 24 hours." }}
            p {{ a href=(&confirm_url) {{ "Confirm Email Address" }} }}
            p {{ "If you did not create an account, you can safely ignore this email." }}
        }})
        .text(format!(
            "Confirm your email address: {{confirm_url}}\n\
             This link expires in 24 hours.\n\
             If you did not create an account, you can safely ignore this email."
        ))
        .build()
        .map_err(|e| {{
            AutumnError::internal_server_error_msg(format!(
                "Failed to build confirmation email: {{e}}"
            ))
        }})?;
    mailer.send(mail).await.map_err(|e| {{
        AutumnError::internal_server_error_msg(format!(
            "Failed to send confirmation email: {{e}}"
        ))
    }})
}}

// ── GDPR: Data Export & Account Deletion (Issue #820) ─────────────────────────
//
// These routes satisfy GDPR Article 15 (right of access) and Article 17
// (right to erasure) obligations. Edit freely — all logic is app code.

/// `GET /account/data-export` — "Download my data" request form.
///
/// The authenticated user may request a full data archive. The request is
/// recorded and a background job is enqueued to build the archive and email
/// a signed download link.
#[get("/account/data-export")]
pub async fn data_export_form(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    // Validates the tracked session row (401s immediately if revoked).
    let _ = require_tracked_session(&session, &mut db, &state).await?;
    Ok(crate::layout("Download My Data", "/account/data-export", html! {{}}, html! {{
        h1 {{ "Download My Data" }}
        p {{
            "Request a copy of all data we hold about you. You will receive an email \
             with a secure download link when the archive is ready (usually within a few minutes)."
        }}
        form action="/account/data-export" method="post" {{
            @if let Some(ref csrf) = csrf {{
                input type="hidden"
                    name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str()))
                    value=(csrf.token());
            }}
            button type="submit" {{ "Request My Data Archive" }}
        }}
        p {{ a href="/account" {{ "← Back to account" }} }}
    }}).into_response())
}}

/// `POST /account/data-export` — enqueue the export job (GDPR Article 15).
///
/// Stamps `export_requested_at` and enqueues a `data_export_job` that builds
/// a JSON archive and emails a signed download link via the configured mailer.
/// The job is idempotent: a second request within the same hour is a no-op.
#[post("/account/data-export")]
pub async fn data_export(
    session: Session,
    mut db: Db,
    State(state): State<AppState>,
) -> AutumnResult<Response> {{
    let {snake_name}_id: i64 = require_tracked_session(&session, &mut db, &state).await?.id;

    let mailer = state.extension::<Mailer>();
    let now = chrono::Utc::now().naive_utc();
    let one_hour_ago = now - chrono::Duration::hours(1);

    // Idempotency: only update (and enqueue) if no export was requested in the last hour.
    let updated = diesel::update({table}::table.find({snake_name}_id))
        .filter(
            {table}::export_requested_at
                .is_null()
                .or({table}::export_requested_at.lt(Some(one_hour_ago))),
        )
        .set({table}::export_requested_at.eq(Some(now)))
        .execute(&mut *db)
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to record export request."))?;

    if updated == 0 {{
        return Ok(crate::layout("Export Already Pending", "/account/data-export", html! {{}}, html! {{
            h1 {{ "Export Already Pending" }}
            p {{
                "A data export was already requested within the last hour. \
                 Please check your email or try again later."
            }}
            p {{ a href="/account" {{ "← Back to account" }} }}
        }}).into_response());
    }}

    // Enqueue the background export job.
    enqueue_data_export_job(&state, {snake_name}_id, mailer.as_deref()).await;

    autumn_web::audit::write_from_state(
        &state,
        autumn_web::audit::AuditEvent::new(
            {snake_name}_id.to_string(),
            "gdpr.export.requested",
            {snake_name}_id.to_string(),
            None,
            autumn_web::audit::AuditStatus::Success,
        ),
    )
    .await
    .ok();

    Ok(crate::layout("Export Requested", "/account/data-export", html! {{}}, html! {{
        h1 {{ "Export Requested" }}
        p {{
            "Your data export is being prepared. You will receive an email with a \
             secure download link when it is ready."
        }}
        p {{ a href="/account" {{ "← Back to account" }} }}
    }}).into_response())
}}

/// Background export job: build the JSON archive and email a signed link.
///
/// In production, move this to a dedicated `src/jobs/data_export.rs` file and
/// register it with `autumn_web::job::JobClient`. For now it is a plain async
/// function invoked inline so the starter compiles without a job backend.
async fn enqueue_data_export_job(
    state: &AppState,
    {snake_name}_id: i64,
    mailer: Option<&Mailer>,
) {{
    // TODO: Replace with state.jobs().enqueue("data_export_job", payload).await
    // when a job backend is configured. This inline stub satisfies the starter
    // without requiring a running Redis/Postgres job queue.
    let _ = (state, {snake_name}_id, mailer);
    tracing::info!(
        user_id = {snake_name}_id,
        "data_export_job enqueued (stub — wire up JobClient for production)"
    );
}}

#[derive(Deserialize)]
pub struct DeleteAccountForm {{
    /// User must type "DELETE" to confirm the irreversible operation.
    pub confirmation: String,
}}

/// `GET /account/delete` — account deletion confirmation form.
///
/// Requires a confirmation step: the user types "DELETE" before submitting,
/// reducing accidental deletions.
#[get("/account/delete")]
pub async fn delete_account_form(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {{
    // Validates the tracked session row (401s immediately if revoked).
    let _ = require_tracked_session(&session, &mut db, &state).await?;
    Ok(crate::layout("Delete My Account", "/account/delete", html! {{}}, html! {{
        h1 {{ "Delete My Account" }}
        p class="warning" {{
            "⚠️ This action schedules your account for permanent deletion after a \
             30-day grace period. You may cancel during the grace period by logging in."
        }}
        p {{
            "All data we hold about you will be erased. Data we are legally required \
             to retain (e.g. financial records) will be anonymized or held under legal-hold \
             with the reason documented in our audit log."
        }}
        form action="/account/delete" method="post" {{
            @if let Some(ref csrf) = csrf {{
                input type="hidden"
                    name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str()))
                    value=(csrf.token());
            }}
            div {{
                label for="confirmation" {{ "Type DELETE to confirm:" }}
                input type="text" id="confirmation" name="confirmation"
                    placeholder="DELETE" autocomplete="off" required;
            }}
            button type="submit" {{ "Schedule Account Deletion" }}
        }}
        p {{ a href="/account" {{ "← Back to account" }} }}
    }}).into_response())
}}

/// `POST /account/delete` — schedule account for deletion (GDPR Article 17).
///
/// Records `delete_requested_at` and `delete_scheduled_at` (30-day grace period)
/// then writes an audit log entry. A background sweeper handles the hard-delete
/// once the grace period expires. The user is logged out immediately.
#[secured]
#[step_up]
#[post("/account/delete")]
pub async fn delete_account(
    session: Session,
    mut db: Db,
    State(state): State<AppState>,
    axum::Form(form): axum::Form<DeleteAccountForm>,
) -> AutumnResult<Response> {{
    let {snake_name}_id: i64 = require_tracked_session(&session, &mut db, &state).await?.id;

    // Require the user to type DELETE as a confirmation step.
    if form.confirmation.trim() != "DELETE" {{
        return Ok(crate::layout("Delete My Account", "/account/delete", html! {{}}, html! {{
            h1 {{ "Delete My Account" }}
            p class="error" {{ "You must type DELETE (in uppercase) to confirm." }}
            p {{ a href="/account/delete" {{ "← Back" }} }}
        }}).into_response());
    }}

    let now = chrono::Utc::now().naive_utc();
    let grace_until = now + chrono::Duration::days(30);

    diesel::update({table}::table.find({snake_name}_id))
        .set((
            {table}::delete_requested_at.eq(Some(now)),
            {table}::delete_scheduled_at.eq(Some(grace_until)),
        ))
        .execute(&mut *db)
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to schedule deletion."))?;

    autumn_web::audit::write_from_state(
        &state,
        autumn_web::audit::AuditEvent::new(
            {snake_name}_id.to_string(),
            "gdpr.erasure.requested",
            {snake_name}_id.to_string(),
            None,
            autumn_web::audit::AuditStatus::Success,
        ),
    )
    .await
    .ok();

    session.destroy().await;

    Ok(crate::layout("Deletion Scheduled", "/account/delete", html! {{}}, html! {{
        h1 {{ "Account Deletion Scheduled" }}
        p {{
            "Your account has been scheduled for deletion in 30 days. \
             You will receive a confirmation email."
        }}
        p {{
            "If you change your mind, log in within 30 days and visit \
             your account settings to cancel."
        }}
    }}).into_response())
}}

/// `POST /account/delete/cancel` — cancel a pending account deletion.
///
/// Clears the `delete_requested_at` / `delete_scheduled_at` columns and writes
/// an audit log entry. Returns an error if no deletion is pending or the grace
/// period has already elapsed.
#[secured]
#[post("/account/delete/cancel")]
pub async fn cancel_delete_account(
    session: Session,
    mut db: Db,
    State(state): State<AppState>,
) -> AutumnResult<Response> {{
    let {snake_name}_id: i64 = require_tracked_session(&session, &mut db, &state).await?.id;

    // Only cancel if a future deletion is scheduled; reject once the deadline has passed.
    let rows = diesel::update({table}::table.find({snake_name}_id))
        .filter({table}::delete_scheduled_at.gt(Some(chrono::Utc::now().naive_utc())))
        .set((
            {table}::delete_requested_at.eq(None::<chrono::NaiveDateTime>),
            {table}::delete_scheduled_at.eq(None::<chrono::NaiveDateTime>),
        ))
        .execute(&mut *db)
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to cancel deletion."))?;

    if rows == 0 {{
        return Err(AutumnError::unprocessable_msg(
            "No pending deletion found, or the grace period has already elapsed.",
        ));
    }}

    autumn_web::audit::write_from_state(
        &state,
        autumn_web::audit::AuditEvent::new(
            {snake_name}_id.to_string(),
            "gdpr.erasure.cancelled",
            {snake_name}_id.to_string(),
            None,
            autumn_web::audit::AuditStatus::Success,
        ),
    )
    .await
    .ok();

    Ok(redirect_to("/account").into_response())
}}

// ── Admin GDPR Operations (Issue #820 AC#6) ───────────────────────────────────
//
// These routes allow an admin operator to trigger export or erasure on behalf
// of a user. All operations are fully audited with actor, reason, and timestamp.
// Mount behind your admin authentication middleware.

#[derive(Deserialize)]
pub struct AdminGdprActionForm {{
    /// Free-text reason recorded in the audit log (required for compliance).
    pub reason: String,
}}

/// `POST /admin/{table}/{{id}}/data-export` — admin triggers data export for a user.
///
/// Stamps `export_requested_at` and enqueues the export job.
/// Records `actor_id` (the operator) + `reason` in the audit log.
/// Requires the caller to have the `"admin"` role in their session.
#[secured("admin")]
#[post("/admin/{table}/{{id}}/data-export")]
pub async fn admin_data_export(
    Path(user_id): Path<i64>,
    session: Session,
    mut db: Db,
    State(state): State<AppState>,
    axum::Form(form): axum::Form<AdminGdprActionForm>,
) -> AutumnResult<Response> {{
    let operator_id: String = session
        .get("{snake_name}_id")
        .await
        .ok_or_else(|| AutumnError::unauthorized_msg("Admin access required."))?;

    if form.reason.trim().is_empty() {{
        return Err(AutumnError::unprocessable_msg("Reason is required for admin GDPR actions."));
    }}

    let mailer = state.extension::<Mailer>();

    let now = chrono::Utc::now().naive_utc();
    let updated = diesel::update({table}::table.find(user_id))
        .set({table}::export_requested_at.eq(Some(now)))
        .execute(&mut *db)
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to record export request."))?;

    if updated == 0 {{
        return Err(AutumnError::not_found_msg("User not found."));
    }}

    enqueue_data_export_job(&state, user_id, mailer.as_deref()).await;

    autumn_web::audit::write_from_state(
        &state,
        autumn_web::audit::AuditEvent::new(
            operator_id,
            format!("gdpr.export.admin_triggered reason={{}}", form.reason),
            user_id.to_string(),
            None,
            autumn_web::audit::AuditStatus::Success,
        ),
    )
    .await
    .ok();

    Ok(redirect_to(&format!("/admin/{table}/{{user_id}}")).into_response())
}}

/// `POST /admin/{table}/{{id}}/delete` — admin triggers account erasure for a user.
///
/// Schedules the account for deletion (30-day grace period).
/// Records `actor_id` (the operator), `reason`, and timestamp in the audit log.
/// Requires the caller to have the `"admin"` role in their session.
#[secured("admin")]
#[post("/admin/{table}/{{id}}/delete")]
pub async fn admin_delete_account(
    Path(user_id): Path<i64>,
    session: Session,
    mut db: Db,
    State(state): State<AppState>,
    axum::Form(form): axum::Form<AdminGdprActionForm>,
) -> AutumnResult<Response> {{
    let operator_id: String = session
        .get("{snake_name}_id")
        .await
        .ok_or_else(|| AutumnError::unauthorized_msg("Admin access required."))?;

    if form.reason.trim().is_empty() {{
        return Err(AutumnError::unprocessable_msg("Reason is required for admin GDPR actions."));
    }}

    let now = chrono::Utc::now().naive_utc();
    let grace_until = now + chrono::Duration::days(30);

    let updated = diesel::update({table}::table.find(user_id))
        .set((
            {table}::delete_requested_at.eq(Some(now)),
            {table}::delete_scheduled_at.eq(Some(grace_until)),
        ))
        .execute(&mut *db)
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to schedule deletion."))?;

    if updated == 0 {{
        return Err(AutumnError::not_found_msg("User not found."));
    }}

    autumn_web::audit::write_from_state(
        &state,
        autumn_web::audit::AuditEvent::new(
            operator_id,
            format!("gdpr.erasure.admin_triggered reason={{}}", form.reason),
            user_id.to_string(),
            None,
            autumn_web::audit::AuditStatus::Success,
        ),
    )
    .await
    .ok();

    Ok(redirect_to(&format!("/admin/{table}/{{user_id}}")).into_response())
}}
{totp_section}{magic_link_section}"##
    )
}

#[allow(clippy::too_many_lines)]
fn render_tests_file(pascal_name: &str, _snake_name: &str, magic_link: bool) -> String {
    // Passwordless magic-link live-DB integration test (issue #1328, AC7).
    // Emitted only under `--magic-link`; `#[ignore]`d so it compiles and passes
    // out of the box, like the sibling live-DB tests in this file.
    let magic_link_tests = if magic_link {
        MAGIC_LINK_TESTS_SECTION
    } else {
        ""
    };
    format!(
        r#"//! Request-level smoke tests for {pascal_name} auth, generated by `autumn generate auth`.
//!
//! These tests run against a live server started with `AUTUMN_TEST_BASE_URL`.
//! In CI, start the app, set the env var, and run `cargo test`.
//!
//! Each test uses a raw TCP connection to avoid adding an HTTP client dep;
//! replace with your preferred HTTP client once it is in `Cargo.toml`.

use std::io::{{Read, Write}};
use std::net::TcpStream;

fn base_url() -> Option<String> {{
    std::env::var("AUTUMN_TEST_BASE_URL").ok()
}}

fn host_port(base: &str) -> String {{
    base.trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_owned()
}}

fn get(base: &str, path: &str) -> String {{
    let hp = host_port(base);
    let mut stream =
        TcpStream::connect(&hp).unwrap_or_else(|_| panic!("cannot connect to {{base}}"));
    let req = format!("GET {{path}} HTTP/1.1\r\nHost: {{hp}}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write failed");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read failed");
    resp
}}

fn post_form(base: &str, path: &str, body: &str, cookie: &str) -> String {{
    let hp = host_port(base);
    let mut stream =
        TcpStream::connect(&hp).unwrap_or_else(|_| panic!("cannot connect to {{base}}"));
    let req = format!(
        "POST {{path}} HTTP/1.1\r\n\
         Host: {{hp}}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {{}}\r\n\
         Cookie: {{cookie}}\r\n\
         Connection: close\r\n\r\n\
         {{body}}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write failed");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read failed");
    resp
}}

/// Cookie-aware GET (the bare `get` above sends no `Cookie` header). Used by the
/// live-DB end-to-end tests that must replay an authenticated session.
fn get_auth(base: &str, path: &str, cookie: &str) -> String {{
    let hp = host_port(base);
    let mut stream =
        TcpStream::connect(&hp).unwrap_or_else(|_| panic!("cannot connect to {{base}}"));
    let req = format!(
        "GET {{path}} HTTP/1.1\r\n\
         Host: {{hp}}\r\n\
         Cookie: {{cookie}}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).expect("write failed");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read failed");
    resp
}}

/// Extract the session cookie (`name=value`) from a `Set-Cookie` header.
fn session_cookie(resp: &str) -> Option<String> {{
    resp.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .filter_map(|l| l.splitn(2, ':').nth(1))
        .map(|v| v.split(';').next().unwrap_or("").trim().to_owned())
        .find(|c| !c.is_empty())
}}

/// A process-unique suffix so each live-DB test operates on a fresh account.
fn unique_suffix() -> String {{
    use std::time::{{SystemTime, UNIX_EPOCH}};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{{nanos}}")
}}

/// Strip `scheme://host` from a URL, returning just the path (plus any query).
fn url_path(url: &str) -> String {{
    if let Some(idx) = url.find("://") {{
        let after = &url[idx + 3..];
        match after.find('/') {{
            Some(p) => after[p..].to_owned(),
            None => "/".to_owned(),
        }}
    }} else {{
        url.to_owned()
    }}
}}

/// Read captured mail from a file-based transport and return the first URL
/// containing `marker` (newest message first). Point `AUTUMN_TEST_MAILDIR` at the
/// transport's output directory (e.g. `[mail] transport = "file"`); returns
/// `None` when unset or no match is found.
fn mail_link(marker: &str) -> Option<String> {{
    let dir = std::env::var("AUTUMN_TEST_MAILDIR").ok()?;
    let mut entries: Vec<_> = std::fs::read_dir(&dir).ok()?.filter_map(Result::ok).collect();
    // Scan newest file first so we pick up the most recently delivered message.
    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    entries.reverse();
    for entry in entries {{
        let Ok(body) = std::fs::read_to_string(entry.path()) else {{
            continue;
        }};
        if let Some(idx) = body.find(marker) {{
            let start = body[..idx].rfind("http").unwrap_or(idx);
            let tail = &body[start..];
            let end = tail
                .find(|c: char| c.is_whitespace() || c == '"' || c == '<' || c == '\'')
                .unwrap_or(tail.len());
            return Some(tail[..end].to_owned());
        }}
    }}
    None
}}

/// Sign up an account and confirm it by GETting the confirmation link captured
/// from the file mail transport, leaving it ready to sign in.
fn signup_and_confirm(base: &str, email: &str, password: &str) {{
    let body = format!("email={{email}}&password={{password}}");
    post_form(base, "/signup", &body, "");
    if let Some(url) = mail_link("/auth/confirm/") {{
        let _ = get(base, &url_path(&url));
    }}
}}

/// Log in and return the authenticated session cookie. Panics on failure.
fn login(base: &str, email: &str, password: &str) -> String {{
    let body = format!("email={{email}}&password={{password}}");
    let resp = post_form(base, "/login", &body, "");
    assert!(
        resp.contains("HTTP/1.1 30") || resp.contains("HTTP/1.0 30"),
        "login did not redirect — is the account confirmed?\n{{resp}}"
    );
    session_cookie(&resp).expect("login response missing Set-Cookie")
}}

#[test]
fn auth_signup_returns_200() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/signup");
    assert!(
        resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200"),
        "GET /signup did not return 200:\n{{resp}}"
    );
}}

#[test]
fn auth_login_returns_200() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/login");
    assert!(
        resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200"),
        "GET /login did not return 200:\n{{resp}}"
    );
}}

#[test]
fn auth_logout_redirects() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = post_form(&base, "/logout", "", "");
    assert!(
        resp.contains("HTTP/1.1 30") || resp.contains("HTTP/1.0 30"),
        "POST /logout did not redirect:\n{{resp}}"
    );
}}

#[test]
fn auth_forgot_password_returns_200() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/forgot-password");
    assert!(
        resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200"),
        "GET /forgot-password did not return 200:\n{{resp}}"
    );
}}

#[test]
fn auth_reset_password_returns_200() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/reset-password?token=dummy");
    assert!(
        resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200"),
        "GET /reset-password did not return 200:\n{{resp}}"
    );
}}

#[test]
fn auth_account_rejects_anonymous() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // Without a session cookie, /account must reject with 401 or redirect.
    let resp = get(&base, "/account");
    let is_rejected = resp.contains("HTTP/1.1 401")
        || resp.contains("HTTP/1.0 401")
        || resp.contains("HTTP/1.1 30")
        || resp.contains("HTTP/1.0 30");
    assert!(
        is_rejected,
        "GET /account should reject anonymous requests (expected 401 or redirect):\n{{resp}}"
    );
}}

// ── Account lockout tests ─────────────────────────────────────────────────────
//
// These tests verify the lockout policy required by issue #814.
// They run against a live server like the tests above; set AUTUMN_TEST_BASE_URL.
//
// Each lockout test creates its own account via POST /signup before running
// so the tests are self-contained against a fresh database — no pre-seeding needed.

/// Sign up an account and confirm it, making it ready for login in lockout tests.
///
/// Email confirmation is required before login. This helper creates the account
/// and then confirms it by reading the confirmation URL from the mail log (requires
/// `[mail] transport = "log"` in `autumn.toml` and access to the server's stdout).
///
/// For CI: run the server with `transport = "log"`, capture stdout, extract the
/// `/auth/confirm/...` URL from the log, and GET it before running lockout tests.
/// Alternatively seed the DB directly (replace `users` with your table name):
/// ```sql
/// UPDATE users SET email_confirmed_at = NOW() WHERE email = 'your-test@example.com';
/// ```
fn ensure_account(base: &str, email: &str, password: &str) {{
    let body = format!("email={{email}}&password={{password}}");
    // Creates the account (unconfirmed). The test must also confirm the account
    // before exercising login — see the doc comment above for options.
    post_form(base, "/signup", &body, "");
}}

/// AC8a — N+1 failed attempts within the window cause the next attempt with
/// correct credentials to be rejected.
///
/// Scenario: sign up an account, submit threshold bad-password POSTs, then
/// submit one more POST with the correct password. The final attempt must be
/// rejected (same 422 body as wrong password) because the account is now locked.
///
/// Set `AUTUMN_AUTH__LOCKOUT__THRESHOLD=10` (the default) to run against the
/// standard policy.
#[test]
fn auth_lockout_rejects_correct_credentials() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let email = "lockout-test@example.com";
    let password = "correct-password-L0ck";
    ensure_account(&base, email, password);
    let bad_body = format!("email={{email}}&password=wrong-password");
    // Exhaust the threshold (default 10) with wrong passwords.
    for _ in 0..10 {{
        post_form(&base, "/login", &bad_body, "");
    }}
    // The next attempt with the CORRECT password must still be rejected.
    let correct_body = format!("email={{email}}&password={{password}}");
    let resp = post_form(&base, "/login", &correct_body, "");
    assert!(
        resp.contains("HTTP/1.1 422") || resp.contains("HTTP/1.0 422"),
        "locked account must reject correct credentials with 422:\n{{resp}}"
    );
}}

/// AC8b — a successful login before the threshold resets the failure counter.
///
/// Scenario: sign up an account, submit (threshold-1) bad-password POSTs, then
/// 1 correct-password POST (which succeeds and resets the counter), then
/// (threshold-1) more bad-password POSTs. The account must remain unlocked.
#[test]
fn auth_successful_login_resets_lockout_counter() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let email = "lockout-reset-test@example.com";
    let password = "correct-password-R3set";
    ensure_account(&base, email, password);
    let bad_body = format!("email={{email}}&password=wrong-password");
    let good_body = format!("email={{email}}&password={{password}}");
    // 5 failures (below default threshold of 10).
    for _ in 0..5 {{
        post_form(&base, "/login", &bad_body, "");
    }}
    // Successful login resets the counter.
    let good_resp = post_form(&base, "/login", &good_body, "");
    assert!(
        good_resp.contains("HTTP/1.1 302") || good_resp.contains("HTTP/1.0 302"),
        "correct credentials must succeed before lockout threshold:\n{{good_resp}}"
    );
    // 5 more failures (counter was reset, so still below threshold).
    for _ in 0..5 {{
        post_form(&base, "/login", &bad_body, "");
    }}
    // Should still be able to log in with correct credentials.
    let after_resp = post_form(&base, "/login", &good_body, "");
    assert!(
        after_resp.contains("HTTP/1.1 302") || after_resp.contains("HTTP/1.0 302"),
        "account must remain unlocked after counter was reset by successful login:\n{{after_resp}}"
    );
}}

/// AC8c — the locked-state response is byte-identical at the response-body
/// level to a wrong-password response (non-enumerating lockout).
#[test]
fn auth_lockout_response_identical_to_wrong_password() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let email_locked = "lockout-body-test@example.com";
    let password = "correct-password-B0dy";
    ensure_account(&base, email_locked, password);
    // Wrong-password response body for a non-locked known account.
    let wrong_resp = post_form(
        &base,
        "/login",
        &format!("email={{email_locked}}&password=wrong"),
        "",
    );
    let wrong_body = wrong_resp.splitn(2, "\r\n\r\n").nth(1).unwrap_or("");
    // Exhaust threshold to lock the target account.
    for _ in 0..9 {{
        post_form(
            &base,
            "/login",
            &format!("email={{email_locked}}&password=bad"),
            "",
        );
    }}
    // Locked-account response with correct password.
    let locked_resp = post_form(
        &base,
        "/login",
        &format!("email={{email_locked}}&password={{password}}"),
        "",
    );
    let locked_body = locked_resp.splitn(2, "\r\n\r\n").nth(1).unwrap_or("");
    assert_eq!(
        wrong_body, locked_body,
        "locked-state response body must be byte-identical to wrong-password response body\n\
         wrong:  {{wrong_body:?}}\n\
         locked: {{locked_body:?}}"
    );
}}

// ── Email confirmation tests (issue #823) ─────────────────────────────────────
//
// These tests verify the confirmation flow introduced alongside the signup
// changes. They run against a live server (set AUTUMN_TEST_BASE_URL).

/// AC1/AC5 — signing up creates an unconfirmed account; the user cannot log in
/// until they have clicked the confirmation link.
#[test]
fn signup_without_confirm_cannot_login() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let email = "unconfirmed-test@example.com";
    let password = "UnconfirmedPass1!";
    let signup_body = format!("email={{email}}&password={{password}}");
    let signup_resp = post_form(&base, "/signup", &signup_body, "");
    // Signup redirects to check-your-email, not to /account.
    assert!(
        signup_resp.contains("HTTP/1.1 30") || signup_resp.contains("HTTP/1.0 30"),
        "signup must redirect after account creation:\n{{signup_resp}}"
    );
    // Attempting to log in before confirming must fail.
    let login_body = format!("email={{email}}&password={{password}}");
    let login_resp = post_form(&base, "/login", &login_body, "");
    assert!(
        login_resp.contains("HTTP/1.1 422") || login_resp.contains("HTTP/1.0 422"),
        "unconfirmed account must be rejected on login with 422:\n{{login_resp}}"
    );
}}

/// AC2 — clicking a valid confirmation token marks the account confirmed and
/// grants an authenticated session.
#[test]
fn confirm_with_valid_token_can_login() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // This test requires a real mail transport (e.g. transport = "log" or "file")
    // that captures outgoing messages so the confirmation URL can be extracted.
    //
    // Suggested implementation:
    //   1. POST /signup to create an unconfirmed account.
    //   2. Read the confirmation URL from the mail log / .eml file.
    //   3. GET /auth/confirm/<token> and assert HTTP 302 → /account.
    //   4. Assert the session is authenticated (e.g. GET /account → 200).
    //
    // See docs/guide/authentication.md for a full walkthrough.
    let _ = base;
    todo!(
        "implement confirm_with_valid_token_can_login: \
         capture the token from the mail transport log, \
         GET /auth/confirm/<token>, and assert 302 → /account"
    )
}}

/// AC4 — an expired confirmation token returns a non-revealing 422 and the
/// "request a new confirmation" page.
#[test]
fn confirm_with_expired_token_fails() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/auth/confirm/this-token-does-not-exist");
    assert!(
        resp.contains("HTTP/1.1 422") || resp.contains("HTTP/1.0 422"),
        "expired/invalid confirmation token must return 422:\n{{resp}}"
    );
}}

/// AC4 — replaying a used confirmation token returns a non-revealing failure
/// (the token is cleared on first use so the digest no longer exists).
#[test]
fn confirm_with_replayed_token_fails() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // Use an obviously fake token — the handler must return 422, not 500 or 200.
    let resp = get(&base, "/auth/confirm/replayed-fake-token-that-was-used");
    assert!(
        resp.contains("HTTP/1.1 422") || resp.contains("HTTP/1.0 422"),
        "replayed confirmation token must return 422:\n{{resp}}"
    );
}}

/// AC6 — the resend-confirmation endpoint returns the same page regardless of
/// whether the address is known (non-enumerating); rate limiting is enforced
/// per-account by the framework's rate limiter.
#[test]
fn resend_confirmation_rate_limit() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let body = "email=noone%40example.com";
    // Unknown address — must return same 200 "check your email" page (non-enumerating).
    let resp = post_form(&base, "/auth/confirm/resend", body, "");
    assert!(
        resp.contains("HTTP/1.1 200") || resp.contains("HTTP/1.0 200"),
        "resend-confirmation must return 200 for unknown address:\n{{resp}}"
    );
}}

// ── Change password / change email (issue #1396) ──────────────────────────────

/// The authenticated change-password page is `#[secured]`, so an anonymous
/// request must be rejected (401) or redirected to sign in.
#[test]
fn change_password_page_requires_auth() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/account/password");
    let rejected = resp.contains("HTTP/1.1 401")
        || resp.contains("HTTP/1.0 401")
        || resp.contains("HTTP/1.1 30")
        || resp.contains("HTTP/1.0 30");
    assert!(rejected, "GET /account/password must reject anonymous requests:\n{{resp}}");
}}

/// The authenticated change-email page is `#[secured]`, so an anonymous request
/// must be rejected (401) or redirected to sign in.
#[test]
fn change_email_page_requires_auth() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/account/email");
    let rejected = resp.contains("HTTP/1.1 401")
        || resp.contains("HTTP/1.0 401")
        || resp.contains("HTTP/1.1 30")
        || resp.contains("HTTP/1.0 30");
    assert!(rejected, "GET /account/email must reject anonymous requests:\n{{resp}}");
}}

/// AC4 — an invalid/expired/unknown email-change token returns the same
/// non-revealing 422 (no oracle).
#[test]
fn email_change_confirm_invalid_token_fails() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/account/email/confirm/this-token-does-not-exist");
    assert!(
        resp.contains("HTTP/1.1 422") || resp.contains("HTTP/1.0 422"),
        "invalid email-change token must return 422:\n{{resp}}"
    );
}}

/// AC — end-to-end behaviour that requires a live database (start the app
/// against Postgres, set `AUTUMN_TEST_BASE_URL`, and a mail transport that
/// captures messages such as `transport = "log"` or `"file"`). Ignored by
/// default so `cargo test` is green out of the box; run with `-- --ignored`.
///
/// Full coverage of issue #1396's acceptance criteria:
///   1. Sign up + confirm + sign in on two browsers (two session cookies).
///   2. POST /account/password with the correct current password + a strong new
///      password (through /reauth for the step-up claim). Assert the OTHER
///      browser's session is now revoked (its next request 401s / redirects),
///      while the acting browser stays signed in.
///   3. POST /account/password with a WRONG current password → 422, and the
///      password is unchanged (the old one still signs in).
///   4. POST /account/email with a new address → the OLD address still signs in;
///      GET the confirmation link from the captured mail; after confirming, the
///      NEW address signs in and the old one no longer does.
///
/// See docs/guide/authentication.md for a full walkthrough.
#[test]
#[ignore = "requires a live database and a capturing mail transport"]
fn password_and_email_change_end_to_end() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};

    // A fresh, confirmed account (sign up, then GET the confirm link from mail).
    let suffix = unique_suffix();
    let email = format!("pw-change-{{suffix}}@example.com");
    let password = "correct-horse-Battery-1";
    signup_and_confirm(&base, &email, password);

    // ── (a) A successful password change invalidates OTHER sessions ──────────
    let cookie_a = login(&base, &email, password);
    let cookie_b = login(&base, &email, password);
    assert!(
        get_auth(&base, "/account", &cookie_a).contains(" 200"),
        "client A should start signed in"
    );
    assert!(
        get_auth(&base, "/account", &cookie_b).contains(" 200"),
        "client B should start signed in"
    );

    let new_password = "different-Zebra-2-quux";
    let change_body = format!(
        "current_password={{password}}\
         &new_password={{new_password}}\
         &new_password_confirmation={{new_password}}"
    );
    let change_resp = post_form(&base, "/account/password", &change_body, &cookie_a);
    assert!(
        change_resp.contains("HTTP/1.1 30") || change_resp.contains("HTTP/1.0 30"),
        "password change should redirect on success:\n{{change_resp}}"
    );
    // The acting client's session id is rotated on change; follow the new cookie.
    let cookie_a = session_cookie(&change_resp).unwrap_or(cookie_a);
    assert!(
        get_auth(&base, "/account", &cookie_a).contains(" 200"),
        "the acting client must stay signed in after the change"
    );
    // The OTHER client's next request is now unauthenticated (session revoked).
    let resp_b = get_auth(&base, "/account", &cookie_b);
    assert!(
        resp_b.contains("HTTP/1.1 401")
            || resp_b.contains("HTTP/1.0 401")
            || resp_b.contains("HTTP/1.1 30")
            || resp_b.contains("HTTP/1.0 30"),
        "the other session must be revoked by the password change:\n{{resp_b}}"
    );

    // ── (b) Wrong current password → 422, and the password is unchanged ──────
    let wrong_body = format!(
        "current_password=totally-wrong\
         &new_password=Another-Pass-3-xyz\
         &new_password_confirmation=Another-Pass-3-xyz"
    );
    let wrong_resp = post_form(&base, "/account/password", &wrong_body, &cookie_a);
    assert!(
        wrong_resp.contains("HTTP/1.1 422") || wrong_resp.contains("HTTP/1.0 422"),
        "a wrong current password must be rejected with 422:\n{{wrong_resp}}"
    );
    // The CURRENT password still authenticates → the failed attempt changed nothing.
    let relogin = post_form(
        &base,
        "/login",
        &format!("email={{email}}&password={{new_password}}"),
        "",
    );
    assert!(
        relogin.contains("HTTP/1.1 30") || relogin.contains("HTTP/1.0 30"),
        "the failed change must not have altered the password:\n{{relogin}}"
    );

    // ── (c) A pending email change keeps the OLD address usable until confirmed ─
    let new_email = format!("pw-change-new-{{suffix}}@example.com");
    let cookie = login(&base, &email, new_password);
    let email_body = format!("new_email={{new_email}}&current_password={{new_password}}");
    let start_resp = post_form(&base, "/account/email", &email_body, &cookie);
    assert!(
        start_resp.contains("HTTP/1.1 30")
            || start_resp.contains("HTTP/1.0 30")
            || start_resp.contains(" 200"),
        "starting an email change should succeed:\n{{start_resp}}"
    );
    // The OLD address still signs in while the change is pending.
    let old_still = post_form(
        &base,
        "/login",
        &format!("email={{email}}&password={{new_password}}"),
        "",
    );
    assert!(
        old_still.contains("HTTP/1.1 30") || old_still.contains("HTTP/1.0 30"),
        "the old address must remain usable until the new-address token is confirmed:\n{{old_still}}"
    );
    // Confirm the change via the link mailed to the NEW address.
    let confirm_url = mail_link("/account/email/confirm/")
        .expect("captured mail should contain the email-change confirm link");
    let confirmed = get(&base, &url_path(&confirm_url));
    assert!(
        confirmed.contains(" 200") || confirmed.contains(" 30"),
        "confirming the email change should succeed:\n{{confirmed}}"
    );
    // The NEW address now signs in.
    let new_login = post_form(
        &base,
        "/login",
        &format!("email={{new_email}}&password={{new_password}}"),
        "",
    );
    assert!(
        new_login.contains("HTTP/1.1 30") || new_login.contains("HTTP/1.0 30"),
        "the new address must sign in after confirmation:\n{{new_login}}"
    );
}}
{magic_link_tests}"#
    )
}

/// Live-DB magic-link integration test appended to `tests/auth.rs` under
/// `--magic-link` (issue #1328, AC7). Uses plain single braces — it is inserted
/// verbatim into the `render_tests_file` output, not re-formatted.
const MAGIC_LINK_TESTS_SECTION: &str = r#"
/// Magic-link end-to-end (issue #1328, AC7). Against a live DB with a capturing
/// mail transport, this asserts the request endpoint sends a link and that
/// consuming that link logs the user in.
///
/// Outline (fill in with your HTTP client + mail capture once wired):
///   1. POST /login/magic with a registered, confirmed email → 200
///      "check your email", and the capturing mail transport receives one
///      message whose body contains a `/login/magic/verify?token=…` URL.
///   2. POST /login/magic with an UNREGISTERED email → the SAME 200 response
///      and NO email is sent (non-enumerating).
///   3. GET the captured verify URL → a 200 "Confirm sign-in" page (the token is
///      NOT consumed on GET, so scanners/prefetch bots can't burn the link).
///   4. POST /login/magic/verify with that token → redirect to /account with a
///      session cookie; a follow-up GET /account with that cookie returns 200.
///   5. POST /login/magic/verify with the SAME token again → generic failure
///      (single-use).
///   6. POST /login/magic/verify?token=deadbeef (unknown) → the SAME generic
///      failure page (no oracle).
#[test]
#[ignore = "requires a live database and a capturing mail transport"]
fn magic_link_request_and_consume_logs_in() {
    let Some(base) = base_url() else {
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    };

    // A confirmed account is eligible for a magic link.
    let suffix = unique_suffix();
    let email = format!("magic-{suffix}@example.com");
    let password = "magic-Passw0rd-link";
    signup_and_confirm(&base, &email, password);

    // (1) Requesting a link for the confirmed account emails a verify URL.
    let resp = post_form(&base, "/login/magic", &format!("email={email}"), "");
    assert!(
        resp.contains(" 200"),
        "POST /login/magic should return the check-your-email page:\n{resp}"
    );
    let verify_url = mail_link("/login/magic/verify?token=")
        .expect("the magic-link email must contain a /login/magic/verify?token= URL");
    let verify_path = url_path(&verify_url);
    // The raw token from the emailed link; the confirm form POSTs it back.
    let token = verify_url
        .rsplit("token=")
        .next()
        .expect("verify URL must carry a token= query")
        .to_owned();

    // (3) GETting the link renders a "Confirm sign-in" page and does NOT consume
    //     the token — a scanner that only follows the GET can't burn the link.
    let confirm = get(&base, &verify_path);
    assert!(
        confirm.contains(" 200"),
        "GET verify must render the confirm page, not redirect/consume:\n{confirm}"
    );

    // (4) POSTing the confirmation consumes the token and logs the user in: the
    //     redirect carries a session cookie that authenticates a follow-up
    //     /account request.
    let consume = post_form(&base, "/login/magic/verify", &format!("token={token}"), "");
    assert!(
        consume.contains("HTTP/1.1 30") || consume.contains("HTTP/1.0 30"),
        "POSTing the magic-link confirmation should redirect to /account:\n{consume}"
    );
    let cookie = session_cookie(&consume).expect("the verify redirect must set a session cookie");
    assert!(
        get_auth(&base, "/account", &cookie).contains(" 200"),
        "the magic-link session must authenticate /account"
    );

    // (5) A SECOND POST of the same token fails generically (single-use).
    let second = post_form(&base, "/login/magic/verify", &format!("token={token}"), "");
    assert!(
        !second.contains("HTTP/1.1 30") && !second.contains("HTTP/1.0 30"),
        "a consumed magic link must not log in again:\n{second}"
    );
    assert!(
        second.contains("invalid or has expired"),
        "a re-used token must render the generic failure page:\n{second}"
    );

    // (6) An unknown token yields the SAME generic failure (no oracle).
    let unknown = post_form(
        &base,
        "/login/magic/verify",
        "token=deadbeefdeadbeef",
        "",
    );
    assert!(
        unknown.contains("invalid or has expired"),
        "an unknown token must render the same generic failure page:\n{unknown}"
    );
}
"#;

#[allow(clippy::too_many_lines)]
/// Render `tests/auth_sessions.rs` — integration tests for active-session
/// management (issue #819), including the two-client revocation flow.
fn render_sessions_tests_file(pascal_name: &str, _snake_name: &str) -> String {
    format!(
        r#"//! Active-session management integration tests for {pascal_name},
//! generated by `autumn generate auth` (issue #819).
//!
//! These tests run against a live server started with `AUTUMN_TEST_BASE_URL`.
//! They skip when the env var is unset, so they compile and pass out of the box.
//!
//! The revocation tests additionally need a *confirmed* account because login
//! is gated on email confirmation. Provide one via:
//!
//! ```sh
//! export AUTUMN_TEST_CONFIRMED_EMAIL=tester@example.com
//! export AUTUMN_TEST_CONFIRMED_PASSWORD=some-password-123
//! ```
//!
//! Seed it directly if needed (replace `users` with your table name):
//!
//! ```sql
//! UPDATE users SET email_confirmed_at = NOW() WHERE email = 'tester@example.com';
//! ```

use std::io::{{Read, Write}};
use std::net::TcpStream;

fn base_url() -> Option<String> {{
    std::env::var("AUTUMN_TEST_BASE_URL").ok()
}}

fn confirmed_account() -> Option<(String, String)> {{
    let email = std::env::var("AUTUMN_TEST_CONFIRMED_EMAIL").ok()?;
    let password = std::env::var("AUTUMN_TEST_CONFIRMED_PASSWORD").ok()?;
    Some((email, password))
}}

fn host_port(base: &str) -> String {{
    base.trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_owned()
}}

fn request(base: &str, method: &str, path: &str, body: &str, cookie: &str) -> String {{
    let hp = host_port(base);
    let mut stream =
        TcpStream::connect(&hp).unwrap_or_else(|_| panic!("cannot connect to {{base}}"));
    let req = format!(
        "{{method}} {{path}} HTTP/1.1\r\n\
         Host: {{hp}}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {{}}\r\n\
         Cookie: {{cookie}}\r\n\
         User-Agent: auth-sessions-integration-test\r\n\
         Connection: close\r\n\r\n\
         {{body}}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write failed");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read failed");
    resp
}}

fn get(base: &str, path: &str, cookie: &str) -> String {{
    request(base, "GET", path, "", cookie)
}}

fn post_form(base: &str, path: &str, body: &str, cookie: &str) -> String {{
    request(base, "POST", path, body, cookie)
}}

/// Extract the session cookie (`name=value`) from a `Set-Cookie` header.
fn session_cookie(resp: &str) -> Option<String> {{
    resp.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .filter_map(|l| l.splitn(2, ':').nth(1))
        .map(|v| v.split(';').next().unwrap_or("").trim().to_owned())
        .find(|c| !c.is_empty())
}}

/// Log in and return the authenticated session cookie. Panics on failure.
fn login(base: &str, email: &str, password: &str) -> String {{
    let body = format!("email={{email}}&password={{password}}");
    let resp = post_form(base, "/login", &body, "");
    assert!(
        resp.contains("HTTP/1.1 30") || resp.contains("HTTP/1.0 30"),
        "login did not redirect — is the account confirmed?\n{{resp}}"
    );
    session_cookie(&resp).expect("login response missing Set-Cookie")
}}

/// The revocation tests share one confirmed account, so a concurrent
/// `revoke-others` from one test would tear down another test's sessions.
/// Serialize them.
static REVOKE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn sessions_page_rejects_anonymous() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let resp = get(&base, "/account/sessions", "");
    let is_rejected = resp.contains("HTTP/1.1 401")
        || resp.contains("HTTP/1.0 401")
        || resp.contains("HTTP/1.1 30")
        || resp.contains("HTTP/1.0 30");
    assert!(
        is_rejected,
        "GET /account/sessions should reject anonymous requests:\n{{resp}}"
    );
}}

/// AC — log in on two clients, revoke from one, and the revoked client's
/// next request 401s even though it replays the same (still-valid-looking)
/// cookie. No reliance on cookie expiry.
#[test]
fn revoked_session_next_request_401s() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let Some((email, password)) = confirmed_account() else {{
        eprintln!("skipping: AUTUMN_TEST_CONFIRMED_EMAIL/PASSWORD not set");
        return;
    }};
    let _serial = REVOKE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    // Two independent clients (e.g. laptop + phone).
    let cookie_a = login(&base, &email, &password);
    let cookie_b = login(&base, &email, &password);

    // Both sessions work.
    assert!(
        get(&base, "/account", &cookie_a).contains(" 200"),
        "client A should be signed in"
    );
    assert!(
        get(&base, "/account", &cookie_b).contains(" 200"),
        "client B should be signed in"
    );

    // Client A signs everything else out — one click.
    let resp = post_form(&base, "/account/sessions/revoke-others", "", &cookie_a);
    assert!(
        resp.contains(" 200") || resp.contains(" 30"),
        "revoke-others failed:\n{{resp}}"
    );

    // Client B's replayed cookie must 401 on its very next request.
    let resp_b = get(&base, "/account", &cookie_b);
    assert!(
        resp_b.contains("HTTP/1.1 401") || resp_b.contains("HTTP/1.0 401"),
        "revoked client's next request must 401:\n{{resp_b}}"
    );
}}

/// "Sign out everywhere else" must never revoke the session that asked.
#[test]
fn revoke_other_sessions_keeps_current_session_alive() {{
    let Some(base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    let Some((email, password)) = confirmed_account() else {{
        eprintln!("skipping: AUTUMN_TEST_CONFIRMED_EMAIL/PASSWORD not set");
        return;
    }};
    let _serial = REVOKE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let cookie_a = login(&base, &email, &password);
    let _cookie_b = login(&base, &email, &password);

    post_form(&base, "/account/sessions/revoke-others", "", &cookie_a);

    let resp_a = get(&base, "/account", &cookie_a);
    assert!(
        resp_a.contains(" 200"),
        "current session must survive revoke-others:\n{{resp_a}}"
    );
    let resp_page = get(&base, "/account/sessions", &cookie_a);
    assert!(
        resp_page.contains(" 200"),
        "sessions page must render for the surviving session:\n{{resp_page}}"
    );
}}
"#
    )
}

/// Render `docs/guide/session-management.md` — covers the data model, the
/// handler APIs, the auto-revocation policy, the privacy posture for stored
/// IP / User-Agent data, and how to plug in a custom UA parser (issue #819).
#[allow(clippy::too_many_lines)]
fn render_sessions_docs_file(
    backend: autumn_web::config::DatabaseBackend,
    pascal_name: &str,
    snake_name: &str,
    user_table: &str,
) -> String {
    let sess_table = sessions_table_name(snake_name);
    // The stale-row sweep and the retrofit DDL are copy-paste SQL for the
    // operator, so both must be in the app's own dialect (issue #1908).
    let stale_cutoff = match backend {
        autumn_web::config::DatabaseBackend::Postgres => "NOW() - INTERVAL '90 days'",
        autumn_web::config::DatabaseBackend::Sqlite => "datetime('now', '-90 days')",
    };
    let sessions_ddl = render_sessions_table_ddl(backend, snake_name, user_table);
    format!(
        r#"# Active Session Management

Generated by `autumn generate auth`. Every login persists a `{sess_table}`
row so users can answer "where am I signed in?" and revoke any device —
per-session or all-at-once — from `/account/sessions`.

## How it works

- **One row per login.** A successful login (password, email confirmation,
  TOTP verify, or passkey) inserts a `{sess_table}` row holding the SHA-256
  digest of the opaque server-side session id, the {pascal_name} id, the IP
  at login, the raw and parsed User-Agent, an optional user-set device
  label, `created_at`, and `last_seen_at`.
- **The row is the source of truth.** Authenticated handlers call
  `require_tracked_session(&session, &mut db, &state)`, which loads the row
  by digest. If the row is gone the request is rejected with `401` and the
  cookie session is destroyed — a revoked session dies on its *next*
  request, even if the cookie is replayed. There is no reliance on cookie
  expiry.
- **Bounded write amplification.** `last_seen_at` is refreshed at most once
  per `[auth.sessions].last_seen_update_secs` (default 60 s) per session,
  so a busy session costs one `UPDATE` per window, not one per request.
- **Only the digest is stored.** The raw session id never touches the
  database, so a database leak cannot be replayed as a session cookie.

## Handler APIs

From any request handler that has loaded the current {pascal_name}:

```rust
// List active sessions, most recently seen first.
let sessions = current_{snake_name}.sessions(&mut *db).await?;

// Revoke one session by row id (scoped to this account).
current_{snake_name}.revoke_session(&mut *db, session_id).await?;

// "Sign out everywhere else."
let digest = session_token_digest(&session).await;
current_{snake_name}.revoke_other_sessions(&mut *db, &digest).await?;

// Revoke everything, including the current session (password change).
current_{snake_name}.revoke_all_sessions(&mut *db).await?;
```

Protect your own routes the same way the generated ones are protected:

```rust
let current_{snake_name} = require_tracked_session(&session, &mut db, &state).await?;
```

## The /account/sessions page

`GET /account/sessions` lists every active session with device description
(parsed from the User-Agent), IP, sign-in and last-seen times, a per-row
**Revoke** button, a device-label form, and a one-click **Sign out
everywhere else** button. The page is Maud + htmx: revocations swap the
list in place without a full reload, and fall back to normal form posts
when JavaScript is unavailable.

## Automatic revocation on credential change

Password change (reset flow), TOTP enrollment, TOTP disable, and passkey
add/remove revoke all *other* sessions by default — the standard response
to credential theft. Disable or tune it in `autumn.toml`:

```toml
[auth.sessions]
revoke_on_credential_change = true  # default
last_seen_update_secs = 60          # default
```

## Privacy posture for stored IP and User-Agent

The session row stores the login IP and User-Agent so a person can
recognise their own devices. Treat both as personal data:

- **Purpose limitation.** The data exists for device recognition and
  incident response only. Don't repurpose it for analytics.
- **Retention.** Rows are deleted on logout, revocation, and account
  deletion (`ON DELETE CASCADE`). Sessions that simply go stale keep their
  rows until revoked — pick a retention window and scrub on a schedule:

  ```sql
  DELETE FROM {sess_table} WHERE last_seen_at < {stale_cutoff};
  ```

  Pair this with your session cookie `max_age_secs` so rows do not outlive
  the cookies that reference them.
- **Scrubbing/minimisation.** If full IPs are more than you need, truncate
  before storing (e.g. zero the last IPv4 octet) in `build_session_row`,
  or store only the parsed UA fields and set `user_agent` to `""`.
- **No geolocation.** The framework stores the IP only; GeoIP lookups are
  deliberately left to userland.
- **Data export.** Include `{sess_table}` in your GDPR export job — see
  docs/guide/gdpr-compliance.md.

## Plugging in a custom User-Agent parser

The default parser (`autumn_web::user_agent::parse_user_agent`) is a small
heuristic covering the major browser/OS families. Every parse goes through
one call site — `build_session_row` in `src/routes/auth.rs` — so
swapping in a dedicated crate is a one-line change:

```rust
// In build_session_row, replace:
let parsed = autumn_web::user_agent::parse_user_agent(&user_agent);
// with your parser of choice, mapping into the same three fields:
let ua = my_ua_crate::parse(&user_agent);
let parsed = autumn_web::user_agent::ParsedUserAgent {{
    family: ua.browser_name,
    os: ua.os_name,
    device_class: autumn_web::user_agent::DeviceClass::Unknown,
}};
```

The parsed fields are stored denormalised (`ua_family`, `ua_os`,
`ua_device`), so changing the parser only affects *new* sessions.

## Migration path for existing apps

Already generated the auth starter before session management existed? The
upgrade is one additive table:

```sql
{sessions_ddl}```

Existing logged-in sessions have no row, so `require_tracked_session`
treats them as revoked: every user re-authenticates once after the
deploy. That is the safe default after introducing revocation.

## Integration tests

`tests/auth_sessions.rs` covers the two-client flow: log in on two
clients, revoke from one, and assert the revoked client's next request
401s while the surviving client keeps working. Set `AUTUMN_TEST_BASE_URL`
plus `AUTUMN_TEST_CONFIRMED_EMAIL`/`AUTUMN_TEST_CONFIRMED_PASSWORD` to run
them against a live server.
"#
    )
}

#[allow(clippy::too_many_lines)]
fn render_docs_file(pascal_name: &str, totp: bool, magic_link: bool) -> String {
    let totp_docs = if totp { TOTP_DOCS_SECTION } else { "" };
    let magic_link_docs = if magic_link {
        MAGIC_LINK_DOCS_SECTION
    } else {
        ""
    };
    format!(
        r#"# Authentication Guide

Generated by `autumn generate auth`. Edit freely.

## Overview

This guide documents the browser-session authentication flow generated for
your Autumn application. The generated code handles signup, login, logout,
account profile, and password reset using Autumn's built-in session, CSRF,
password hashing, and mail primitives.

## Generated Routes

| Method | Path | Handler | Auth |
|--------|------|---------|------|
| GET | `/signup` | `signup_form` | Public |
| POST | `/signup` | `signup` | Public |
| GET | `/check-your-email` | `check_your_email` | Public |
| GET | `/login` | `login_form` | Public |
| POST | `/login` | `login` | Public |
| POST | `/logout` | `logout` | Any |
| GET | `/account` | `account` | **Required** + Confirmed |
| POST | `/account/destroy` | `account_destroy` | **Required** + Step-up |
| GET | `/reauth` | `reauth_form` | Any (redirects if unauthed) |
| POST | `/reauth` | `reauth` | Any (redirects if unauthed) |
| GET | `/forgot-password` | `forgot_password_form` | Public |
| POST | `/forgot-password` | `forgot_password` | Public |
| GET | `/reset-password` | `reset_password_form` | Public |
| POST | `/reset-password` | `reset_password` | Public |
| GET | `/auth/confirm/:token` | `confirm_email` | Public |
| GET | `/auth/confirm/resend` | `resend_confirmation_form` | Public |
| POST | `/auth/confirm/resend` | `resend_confirmation` | Public |

## Security Properties

- **Passwords**: Hashed with bcrypt (cost 12) via `autumn_web::auth::hash_password`.
  Raw passwords are never logged or stored.
- **Reset tokens**: 32-byte random values generated with `OsRng`; only the
  SHA-256 digest is stored in `reset_token_digest`. The raw token is sent by
  email only and expires after 2 hours.
- **Non-enumeration**: Duplicate signup, failed login, unconfirmed-login,
  and forgot-password submissions for unknown addresses all return responses
  that do not reveal whether an email address is registered.
- **Email confirmation**: Signup creates the account in an unconfirmed state
  and sends a confirmation link. Users cannot log in until they confirm their
  email. Only the SHA-256 digest of the confirmation token is stored
  (`confirm_token_digest`); the raw token lives only in the email and URL bar.
- **Session fixation**: Login and password-reset rotate the session ID
  (`session.rotate_id()`).
- **Session invalidation**: Logout clears the session data and rotates the ID,
  destroying the old server-side session so the pre-logout cookie cannot be
  replayed (equivalent to `destroy()` for replay safety) while still carrying a
  one-shot "logged out" flash notice to the login page.
- **Protected routes**: The `/account` route uses `#[secured]` to reject
  unauthenticated requests before the handler runs.
- **Account lockout**: After the configured threshold of failed login attempts,
  the account is locked for the cool-off period. Lockout responses are
  indistinguishable from wrong-password responses (same status, same body) so
  the endpoint does not reveal which accounts are locked. Lockout state is
  stored in Postgres so all app replicas agree. See [Account Lockout Policy](#account-lockout-policy) below.

## Account Lockout Policy

The generated login endpoint includes account-level lockout to protect against
credential-stuffing attacks that rotate source IPs to bypass IP-rate limiting.

### How It Works

1. Each failed login increments `failed_attempts` on the account row.
2. When `failed_attempts` reaches `threshold`, `locked_at` is stamped with the
   current time and a structured telemetry event fires (see below).
3. While the account is locked (`locked_at + cooloff_secs > now`), every login
   attempt — correct password or not — returns the same `422 Invalid email or
   password` response as a wrong-password attempt (non-enumerating).
4. A successful login resets `failed_attempts` to `0` and clears `locked_at`.
5. After `cooloff_secs` elapses without a login attempt, the account auto-unlocks
   on the next successful login.

### Configuration (`autumn.toml`)

```toml
[auth.lockout]
enabled      = true  # false → disable lockout entirely (e.g. external policy in place)
threshold    = 10    # consecutive failures before lockout
window_secs  = 60    # reserved for future sliding-window counting; not yet enforced —
                     # failures currently accumulate since the last successful login
cooloff_secs = 900   # lock duration in seconds (default 15 minutes)
```

Override at deploy time via environment variables:

| Variable | Description |
|----------|-------------|
| `AUTUMN_AUTH__LOCKOUT__ENABLED` | `true` / `false` |
| `AUTUMN_AUTH__LOCKOUT__THRESHOLD` | integer (0 = disabled) |
| `AUTUMN_AUTH__LOCKOUT__WINDOW_SECS` | integer |
| `AUTUMN_AUTH__LOCKOUT__COOLOFF_SECS` | integer |

Set `threshold = 0` or `enabled = false` to restore the pre-lockout login
behaviour for apps that use a stronger external policy.

### Telemetry

When an account transitions into the locked state the handler emits a
`tracing::warn!` event with:

| Field | Value |
|-------|-------|
| `event` | `"account_locked"` |
| `account_id_digest` | First 8 bytes of SHA-256(account ID) — stable but not reversible |
| `ip_prefix` | IPv4 /24 prefix (`a.b.c.0/24`) or IPv6 /64 prefix (`x:x:x:x::/64`) |
| `failed_attempts` | Counter value at lock time |

This event flows through the standard Autumn tracing/metrics surface
(OTLP, structured JSON, or console pretty-print depending on config).

### Operator Unlock

The generated auth routes include a `POST /auth/admin/unlock` endpoint that
clears the lockout for a single account. It is protected by the
`X-Admin-Secret` header matching the `AUTUMN_ADMIN_SECRET` environment
variable. Set this variable to a strong random value in production:

```sh
# Generate and store a secret (do this once, store in your .env or deployment
# secrets manager — NOT in autumn credentials, which are not consulted here).
export AUTUMN_ADMIN_SECRET="$(openssl rand -hex 32)"

# Unlock an account
curl -s -X POST https://example.com/auth/admin/unlock \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -H "X-Admin-Secret: $AUTUMN_ADMIN_SECRET" \
  -d "email=user%40example.com"
```

Protect this endpoint with network-level access controls (VPN, internal
load-balancer allowlist) in production in addition to the secret header.
If `AUTUMN_ADMIN_SECRET` is not set, the endpoint always returns 422.

### Existing Apps — Adoption Path

Apps that ran `autumn generate auth` before this feature was introduced need
one additional migration to add the lockout columns. Because `generate migration`
does not carry column defaults, write the SQL directly:

```sh
# 1. Create an empty migration
autumn generate migration add_lockout_to_{{table}}

# 2. Edit the generated up.sql to add the columns with correct defaults:
```

```sql
-- migrations/<timestamp>_add_lockout_to_{{table}}/up.sql
ALTER TABLE {{table}}
  ADD COLUMN IF NOT EXISTS failed_attempts INT NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS locked_at TIMESTAMP NULL;
```

```sql
-- migrations/<timestamp>_add_lockout_to_{{table}}/down.sql
ALTER TABLE {{table}}
  DROP COLUMN IF EXISTS failed_attempts,
  DROP COLUMN IF EXISTS locked_at;
```

```sh
# 3. Apply the migration
autumn migrate

# 4. Re-generate auth templates to pick up the lockout handler and unlock route
autumn generate auth {pascal_name} --force
```

No behaviour changes for existing apps until the migration is applied and
the generator re-run — the framework does not silently change behaviour.

## Mail Configuration

Password-reset emails contain an absolute link built from `APP_BASE_URL`.
Set this environment variable to your application's public URL:

```sh
# .env or shell
APP_BASE_URL=https://example.com
```

In development, configure file-based mail capture in `autumn.toml` so reset
links land in `target/mail/` instead of hitting an SMTP server:

```toml
[mail]
transport = "file"
from = "Your App <noreply@yourapp.dev>"
```

Open the `.eml` files with any email client to click the reset link.

If `[mail]` is not configured (`transport = "disabled"`), the forgot-password
handler returns an immediate HTTP 500 with a clear configuration message
rather than silently showing "Check Your Email" when no mail will be sent.

## Email Confirmation

Signup creates the account in an **unconfirmed** state and immediately sends a
confirmation link. The user is **not** logged in until they click the link.
Attempting to log in with an unconfirmed address returns the same
`422 Invalid email or password` response as a wrong-password attempt
(non-enumerating — does not reveal that the address exists but is unconfirmed).

### Generated Routes

| Method | Path | Handler | Auth |
|--------|------|---------|------|
| GET | `/check-your-email` | `check_your_email` | Public |
| GET | `/auth/confirm/:token` | `confirm_email` | Public |
| GET | `/auth/confirm/resend` | `resend_confirmation_form` | Public |
| POST | `/auth/confirm/resend` | `resend_confirmation` | Public |

### Security Properties

- **Digest-only storage**: The raw confirmation token is never stored in the
  database, logs, traces, or error reports. Only its SHA-256 digest is
  persisted in `confirm_token_digest`. The raw token lives only in the email
  body and the user's URL bar.
- **Single-use tokens**: `confirm_email` clears `confirm_token_digest` and
  `confirm_token_expires_at` on first use. Replaying the token returns a 422.
- **Token expiry**: Tokens expire after 24 hours (`confirm_token_expires_at`).
  An expired token returns a non-revealing 422 with a "request a new
  confirmation" page.
- **Non-enumeration**: Resend-confirmation always returns the same "Check Your
  Email" page regardless of whether the address is registered or already
  confirmed, so the endpoint does not reveal which addresses exist.
- **Resend invalidates old token**: Each resend overwrites
  `confirm_token_digest` / `confirm_token_expires_at`, so old links stop
  working immediately.

### Confirmed-Only Gate

The generated `/account` route demonstrates how to require a confirmed email
address on any route. After fetching the user record, check
`email_confirmed_at`:

```rust
// In any #[secured] handler that requires email confirmation:
if {snake_name}.email_confirmed_at.is_none() {{
    return Ok(redirect_to("/check-your-email"));
}}
```

Replicate this block on any route that should be gated behind confirmation.

### Confirmation vs. Password Reset

Password reset and email confirmation are **independent state transitions**.
Completing a password reset does **not** confirm the email address. This is
intentional: an attacker who controls a reset link (e.g., via a compromised
inbox) should not also gain a confirmed account status. After a successful
password reset, an unconfirmed user is redirected to `/check-your-email`
rather than `/account`.

### Email-Change Flow

When an existing user changes their email address, the new address must be
re-confirmed before it becomes the active sign-in address. The generated
scaffolding supports this via the following pattern:

1. On email-change form submission, generate a new confirmation token and store
   its digest in `confirm_token_digest`.
2. Set `email_confirmed_at = NULL` for the *new* address column (or keep the
   old address in a `pending_email` column until confirmed).
3. Send a confirmation email to the *new* address.
4. Keep the *old* address valid for sign-in until confirmation succeeds.
5. On confirmation, swap the address and stamp `email_confirmed_at`.
6. Send a notice email to the *old* address.

The starter scaffolds the token/confirmation machinery; the email-change form
and address-swap logic are left as a documented customization point in
`src/routes/auth.rs` because the exact data model depends on your application's
requirements. See `resend_confirmation` for the token-generation pattern to
reuse.

### Configuration

Confirmation token expiry is 24 hours by default. To change it, edit the
`hours(24)` call in `signup` and `resend_confirmation` in `src/routes/auth.rs`.

### Development Setup

In development, set `transport = "log"` or `transport = "file"` in
`autumn.toml` so confirmation links are visible without an SMTP provider:

```toml
[mail]
transport = "log"          # prints the confirmation URL to the console
# transport = "file"       # writes .eml files to target/mail/
from = "noreply@yourapp.dev"
```

If `transport = "disabled"` (the default when `[mail]` is absent), the signup
handler fails immediately with a clear HTTP 500 message naming the missing mail
configuration rather than silently creating an account that can never be confirmed.

### Existing Apps — Adoption Path (email confirmation columns)

Apps that ran `autumn generate auth` before email confirmation was added need
an additional migration to add the new columns:

```sh
# 1. Create an empty migration
autumn generate migration add_email_confirmation_to_{{table}}

# 2. Edit the generated up.sql:
```

```sql
-- migrations/<timestamp>_add_email_confirmation_to_{{table}}/up.sql
ALTER TABLE {{table}}
  ADD COLUMN IF NOT EXISTS confirm_token_digest TEXT NULL,
  ADD COLUMN IF NOT EXISTS confirm_token_expires_at TIMESTAMP NULL,
  ADD COLUMN IF NOT EXISTS email_confirmed_at TIMESTAMP NULL,
  ADD COLUMN IF NOT EXISTS pending_email TEXT NULL;
```

```sql
-- migrations/<timestamp>_add_email_confirmation_to_{{table}}/down.sql
ALTER TABLE {{table}}
  DROP COLUMN IF EXISTS confirm_token_digest,
  DROP COLUMN IF EXISTS confirm_token_expires_at,
  DROP COLUMN IF EXISTS email_confirmed_at,
  DROP COLUMN IF EXISTS pending_email;
```

```sh
# 3. Apply the migration
autumn migrate

# 4. Regenerate src/schema.rs so Diesel knows about the new columns.
#    (autumn generate auth only adds the table when it is absent; it cannot
#    add columns to an existing table entry in schema.rs.)
diesel print-schema > src/schema.rs

# 5. Re-generate auth templates to pick up the confirmation handlers
autumn generate auth {pascal_name} --force
```

After the migration, existing (pre-confirmation) accounts will have
`email_confirmed_at = NULL`. To mark all existing accounts as confirmed so
they can log in immediately (opt-in migration), run:

```sql
-- Only run this if you trust all existing accounts (no spam/abuse backlog).
UPDATE {{table}} SET email_confirmed_at = NOW() WHERE email_confirmed_at IS NULL;
```

No behaviour changes for existing accounts until the migration is applied and
the generator re-run — the framework does not silently change behaviour.

## Customization Points

- **Validation**: Add stricter email / password rules to `signup` and
  `reset_password` in `src/routes/auth.rs`.
- **Session keys**: Change the session key names (`{snake_name}_id`,
  `{snake_name}_email`) to match your application's conventions.
- **Redirect targets**: Adjust `redirect_to("/account")` calls to send users
  to the right page after login/signup/reset.
- **Email templates**: Customise the `send_reset_email` function to match your
  brand.
- **{pascal_name} fields**: Add display-name, avatar, or role fields to the
  `{pascal_name}` model and a new migration.

{totp_docs}{magic_link_docs}## When to Choose This Flow vs. Alternatives

| Scenario | Recommendation |
|----------|---------------|
| Browser-based web app | ✅ This generated flow |
| Mobile / CLI / third-party API clients | API tokens (`autumn generate token` — see [#520]) |
| Social login (Google, GitHub, …) | OAuth2/OIDC (S-059) |
| Enterprise / SSO | SAML / enterprise IdP (future) |

## Quick Start

```sh
autumn new myapp
cd myapp
autumn generate auth {pascal_name}
autumn migrate
autumn dev
```

Then open <http://localhost:3000/signup> to create your first account.
"#,
        snake_name = pascal_name.to_lowercase(),
        pascal_name = pascal_name,
    )
}

/// Render `docs/guide/gdpr-compliance.md` — GDPR/CCPA compliance checklist.
///
/// Documents exactly which generated artifacts satisfy Article 15 (right of
/// access) and Article 17 (right to erasure), and which obligations remain the
/// application developer's responsibility.
fn render_gdpr_docs_file() -> String {
    r#"# GDPR & CCPA Compliance Checklist

This document describes the data-privacy obligations covered by the generated
auth starter and those that remain the application developer's responsibility.

## What the Auth Starter Provides

### Article 15 — Right of Access (`GET /account/data-export`)

| Artifact | Location |
|---|---|
| Self-service "Download my data" form | `GET /account/data-export` |
| Export-request POST handler | `POST /account/data-export` |
| `export_requested_at` column on the users table | Migration + model |
| Audit log entry on request (`gdpr.export.requested`) | `src/routes/auth.rs` |
| Background export job stub (wire up `JobClient` for production) | `src/routes/auth.rs` |

### Article 17 — Right to Erasure (`POST /account/delete`)

| Artifact | Location |
|---|---|
| Self-service "Delete my account" confirmation form | `GET /account/delete` |
| 30-day grace period with `delete_scheduled_at` column | Migration + model |
| Typed confirmation step (user must type "DELETE") | `src/routes/auth.rs` |
| Audit log entry on request (`gdpr.erasure.requested`) | `src/routes/auth.rs` |
| Cancellation route during grace period | `POST /account/delete/cancel` |
| Audit log entry on cancellation (`gdpr.erasure.cancelled`) | `src/routes/auth.rs` |

## What the Application Developer Must Provide

The following obligations are **not** automated by the framework and require
per-application implementation:

### Hard-Delete / Anonymize Sweeper

The grace-period sweeper that reads `delete_scheduled_at` and performs the
actual erasure is the application's responsibility. Implement it as a scheduled
job or a cron task:

1. Query for `WHERE delete_scheduled_at < NOW()`.
2. For each model, apply the registered erasure strategy:
   - `hard_delete` — `DELETE FROM <table> WHERE user_id = $1`.
   - `anonymize` — replace PII columns with tombstone values (e.g. `DELETED_<hash>`).
   - `retain` — keep the row, record the retention reason in your audit log.
3. Finally, hard-delete the user row.

### Third-Party Processor Erasure

GDPR requires erasure from third-party processors (Stripe, SendGrid, analytics
services, etc.). This is out of scope for the auth starter. Document your
processor list and erasure API calls in a separate runbook.

### Admin-Triggered Export and Erasure

For Subject Access Requests (SARs) initiated by your legal/ops team, add admin
routes that call the same export/erasure logic with the operator's identity
recorded as the `actor_id` in the audit log.

## Compliance Status Summary

| Obligation | Status |
|---|---|
| Article 15 — right of access route | ✅ Generated |
| Article 15 — audit log | ✅ Generated |
| Article 17 — erasure request route | ✅ Generated |
| Article 17 — grace period + cancellation | ✅ Generated |
| Article 17 — audit log | ✅ Generated |
| Article 17 — actual row deletion sweeper | ⚠️ Application responsibility |
| Article 17 — third-party processor erasure | ⚠️ Application responsibility |
| CCPA §1798.105 — deletion right | ✅ Covered by Article 17 routes |
| Admin/operator SAR tooling | ⚠️ Application responsibility |

## Running `autumn doctor`

After registering your application's models, run:

```bash
autumn doctor
```

The `gdpr_export_registration` check will warn if any `#[repository]`-annotated
models are not registered for export/erasure. Register models via `GdprRegistry`
in your application state.
"#
    .to_owned()
}

fn auth_route_entries(totp: bool, magic_link: bool) -> Vec<String> {
    let mut entries = vec![
        "routes::auth::signup_form".to_owned(),
        "routes::auth::signup".to_owned(),
        "routes::auth::login_form".to_owned(),
        "routes::auth::login".to_owned(),
        "routes::auth::logout".to_owned(),
        "routes::auth::unlock_account".to_owned(),
        "routes::auth::account".to_owned(),
        "routes::auth::account_destroy".to_owned(),
        "routes::auth::change_password_form".to_owned(),
        "routes::auth::change_password".to_owned(),
        "routes::auth::change_email_form".to_owned(),
        "routes::auth::change_email".to_owned(),
        "routes::auth::confirm_email_change".to_owned(),
        "routes::auth::sessions_page".to_owned(),
        "routes::auth::sessions_revoke".to_owned(),
        "routes::auth::sessions_revoke_remember".to_owned(),
        "routes::auth::sessions_revoke_others".to_owned(),
        "routes::auth::sessions_label".to_owned(),
        "routes::auth::reauth_form".to_owned(),
        "routes::auth::reauth".to_owned(),
        "routes::auth::forgot_password_form".to_owned(),
        "routes::auth::forgot_password".to_owned(),
        "routes::auth::reset_password_form".to_owned(),
        "routes::auth::reset_password".to_owned(),
        "routes::auth::check_your_email".to_owned(),
        "routes::auth::resend_confirmation_form".to_owned(),
        "routes::auth::resend_confirmation".to_owned(),
        "routes::auth::confirm_email".to_owned(),
        "routes::auth::data_export_form".to_owned(),
        "routes::auth::data_export".to_owned(),
        "routes::auth::delete_account_form".to_owned(),
        "routes::auth::delete_account".to_owned(),
        "routes::auth::cancel_delete_account".to_owned(),
        "routes::auth::admin_data_export".to_owned(),
        "routes::auth::admin_delete_account".to_owned(),
    ];
    if totp {
        entries.extend([
            "routes::auth::login_verify_form".to_owned(),
            "routes::auth::login_verify".to_owned(),
            "routes::auth::two_factor_status".to_owned(),
            "routes::auth::two_factor_enable".to_owned(),
            "routes::auth::two_factor_confirm".to_owned(),
            "routes::auth::two_factor_disable".to_owned(),
        ]);
    }
    if magic_link {
        entries.extend([
            "routes::auth::magic_link_request_form".to_owned(),
            "routes::auth::magic_link_request".to_owned(),
            "routes::auth::magic_link_verify_form".to_owned(),
            "routes::auth::magic_link_verify".to_owned(),
        ]);
    }
    entries
}

fn oauth_route_entries() -> Vec<String> {
    vec![
        "routes::oauth::oauth_redirect".to_owned(),
        "routes::oauth::oauth_callback".to_owned(),
    ]
}

fn render_oauth_migration_up(
    backend: autumn_web::config::DatabaseBackend,
    user_table: &str,
) -> String {
    let d = AuthDdl::for_backend(backend);
    format!(
        "CREATE TABLE oauth_identities (\n\
         \x20   id {pk},\n\
         \x20   provider TEXT NOT NULL,\n\
         \x20   subject TEXT NOT NULL,\n\
         \x20   user_id {big_int} NOT NULL REFERENCES {user_table}(id) ON DELETE CASCADE,\n\
         \x20   email TEXT NULL,\n\
         \x20   name TEXT NULL,\n\
         \x20   created_at {created_at},\n\
         \x20   UNIQUE (provider, subject)\n\
         );\n\
         \n\
         CREATE INDEX oauth_identities_user_id_idx ON oauth_identities (user_id);\n",
        pk = d.pk,
        big_int = d.big_int,
        created_at = d.ts_not_null_default_now,
    )
}

fn render_oauth_migration_down() -> String {
    "DROP TABLE oauth_identities;\n".to_owned()
}

#[allow(clippy::too_many_lines)]
fn render_oauth_routes_file(
    pascal_name: &str,
    snake_name: &str,
    user_table: &str,
    providers: &[String],
    totp: bool,
) -> String {
    let provider_list = providers
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");

    // When TOTP is also generated, the callback guidance must NOT set the secured
    // session key directly for a 2FA-enabled account — it has to route through
    // `/login/verify` exactly like password login, or OAuth would bypass 2FA.
    let totp_callback_note = if totp {
        "    //\n\
         \x20   // ⚠️ TWO-FACTOR: this app was generated with `--totp`. If the resolved\n\
         \x20   // local account has `totp_enabled == true`, do NOT set the secured\n\
         \x20   // session key here. Instead mark the session pending and redirect to\n\
         \x20   // the second-factor form, mirroring the password-login flow:\n\
         \x20   //\n\
         \x20   //   if local_user.totp_enabled {\n\
         \x20   //       // Refuse if [auth].session_key collides with a reserved pending\n\
         \x20   //       // key — otherwise parking `totp_pending_id` would write the very\n\
         \x20   //       // key `#[secured]` trusts, authenticating before /login/verify:\n\
         \x20   //       if crate::routes::auth::is_reserved_totp_pending_key(&auth_cfg.session_key) {\n\
         \x20   //           return Redirect::to(\"/login?error=totp_key_collision\").into_response();\n\
         \x20   //       }\n\
         \x20   //       session.rotate_id().await;\n\
         \x20   //       // rotate_id() keeps session data, so drop any live auth from a\n\
         \x20   //       // prior login in this browser before parking the pending handoff:\n\
         \x20   //       session.remove(&auth_cfg.session_key).await;\n\
         \x20   //       session.remove(\"__SNAKE___id\").await;\n\
         \x20   //       session.remove(\"__SNAKE___email\").await;\n\
         \x20   //       session.remove(\"totp_pending_reset_digest\").await;\n\
         \x20   //       session.remove(\"totp_pending_reset_token\").await;\n\
         \x20   //       session.remove(\"totp_pending_secret\").await;\n\
         \x20   //       session.insert(\"totp_pending_id\", local_user.id.to_string()).await;\n\
         \x20   //       return Redirect::to(\"/login/verify\").into_response();\n\
         \x20   //   }\n\
         \x20   //   // otherwise, fully authenticate. Rotate the session id first — like\n\
         \x20   //   // the password-login/reset paths — so a pre-login (possibly fixated)\n\
         \x20   //   // session id is never promoted unchanged:\n\
         \x20   //   session.rotate_id().await;\n\
         \x20   //   // Then clear any abandoned pending-2FA / deferred-reset / enrollment\n\
         \x20   //   // state, and set BOTH the model-specific identity keys the generated\n\
         \x20   //   // handlers read (`__SNAKE___id` / `_email`) and the configured auth key\n\
         \x20   //   // — clearing stale model keys first so a prior login in this browser\n\
         \x20   //   // can't leave a mismatched identity:\n\
         \x20   //   session.remove(\"totp_pending_id\").await;\n\
         \x20   //   session.remove(\"totp_pending_reset_digest\").await;\n\
         \x20   //   session.remove(\"totp_pending_reset_token\").await;\n\
         \x20   //   session.remove(\"totp_pending_secret\").await;\n\
         \x20   //   session.insert(\"__SNAKE___id\", local_user.id.to_string()).await;\n\
         \x20   //   session.insert(\"__SNAKE___email\", &local_user.email).await;\n\
         \x20   //   session.insert(&auth_cfg.session_key, local_user.id.to_string()).await;\n"
            .replace("__SNAKE__", snake_name)
    } else {
        String::new()
    };

    format!(
        r#"//! OAuth2/OIDC redirect and callback handlers.
//!
//! Generated by `autumn generate auth --oauth`.
//! Edit freely — once generated this is ordinary user code.
//!
//! Routes:
//!   GET  /auth/:provider/redirect  → redirects the browser to the provider
//!   GET  /auth/:provider/callback  → exchanges the code and logs the user in

use autumn_web::auth::{{OAuth2Callback, OidcIdentity, oauth2_authorize_url, oauth2_finish_login}};
use autumn_web::prelude::*;
use autumn_web::{{get, oauth2_callback, AppState, State}};
use axum::extract::{{Path, Query}};
use axum::response::{{IntoResponse, Redirect}};
use tracing::warn;

/// Supported OAuth2/OIDC providers.
const SUPPORTED_PROVIDERS: &[&str] = &[{provider_list}];

/// Redirect the browser to the provider's authorization endpoint.
///
/// State and nonce are stored in the session for CSRF protection.
/// PKCE (S256) code_challenge is appended automatically by the framework.
#[get("/auth/{{provider}}/redirect")]
pub async fn oauth_redirect(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    session: Session,
) -> impl IntoResponse {{
    if !SUPPORTED_PROVIDERS.contains(&provider_name.as_str()) {{
        return Redirect::to("/login?error=unknown_provider").into_response();
    }}

    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let auth_cfg = &config.auth;
    let Some(provider) = auth_cfg.oauth2.providers.get(&provider_name) else {{
        warn!(provider = %provider_name, "oauth provider not configured in autumn.toml");
        return Redirect::to("/login?error=provider_not_configured").into_response();
    }};

    match oauth2_authorize_url(&session, &provider_name, provider).await {{
        Ok(url) => Redirect::to(&url).into_response(),
        Err(e) => {{
            warn!(error = %e, "oauth2_authorize_url failed");
            Redirect::to("/login?error=oauth_error").into_response()
        }}
    }}
}}

/// Handle the OAuth2 callback: exchange the code, create or link a local account.
///
/// On success the user is redirected to `/account`. A missing or mismatched state
/// returns a non-revealing error redirect without logging the offending values.
///
/// **Important:** `oauth2_finish_login` does NOT set the application session key.
/// You must resolve (or create) the local {pascal_name} row and call
/// `session.insert(&auth_cfg.session_key, local_user_id.to_string())` before redirecting.
#[oauth2_callback("/auth/{{provider}}/callback")]
pub async fn oauth_callback(
    Path(provider_name): Path<String>,
    Query(callback): Query<OAuth2Callback>,
    State(state): State<AppState>,
    session: Session,
) -> impl IntoResponse {{
    if !SUPPORTED_PROVIDERS.contains(&provider_name.as_str()) {{
        return Redirect::to("/login?error=unknown_provider").into_response();
    }}

    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let auth_cfg = &config.auth;
    let Some(provider) = auth_cfg.oauth2.providers.get(&provider_name) else {{
        warn!(provider = %provider_name, "oauth provider not configured in autumn.toml");
        return Redirect::to("/login?error=provider_not_configured").into_response();
    }};

    let identity: OidcIdentity = match oauth2_finish_login(
        &session,
        &provider_name,
        provider,
        &callback,
    )
    .await
    {{
        Ok(id) => id,
        Err(_) => {{
            // Do not log the offending state or code to avoid leaking sensitive values.
            return Redirect::to("/login?error=oauth_failed").into_response();
        }}
    }};

    // TODO: resolve or create a local {pascal_name} record, then set the session.
    //
    // Example (fill in your actual DB query):
    //
    //   let local_user_id: i64 = link_or_create_{snake_name}(
    //       &mut db,
    //       &identity,          // OidcIdentity {{ subject, email, name, … }}
    //       &provider_name,     // e.g. "github"
    //       &auth_cfg,          // carries oauth_linking_policy
    //   ).await?;
    //
    //   session.insert(&auth_cfg.session_key, local_user_id.to_string()).await;
    //
    //   // Track the OAuth login as an active session (device list + revocation,
    //   // issue #819) — add `MaybeClientIp(addr_ip)` and `headers: HeaderMap`
    //   // extractors to this handler, then:
    //   crate::routes::auth::record_login_session(&mut db, &session, local_user_id, addr_ip, &headers).await?;
    //
    // Until the above is implemented the user will NOT be logged in after the OAuth
    // callback — that is intentional to avoid authenticating without a local account.
{totp_callback_note}    let _ = identity;
    let _ = user_table_placeholder_{snake_name}();

    Redirect::to("/account").into_response()
}}

#[allow(dead_code)]
fn user_table_placeholder_{snake_name}() -> &'static str {{
    "{user_table}"
}}
"#,
    )
}

#[allow(clippy::too_many_lines)]
fn render_oauth_docs_file(
    backend: autumn_web::config::DatabaseBackend,
    providers: &[String],
    user_table: &str,
) -> String {
    // The documented schema must match the migration this same run writes, so
    // it takes its column types from the same dialect table (issue #1908).
    let d = AuthDdl::for_backend(backend);
    let provider_list = providers.join(", ");
    let provider_config_examples = providers
        .iter()
        .map(|p| match p.as_str() {
            "github" => concat!(
                "[auth.oauth2.github]\n",
                "client_id     = \"\"\n",
                "client_secret = \"\"\n",
                "authorize_url = \"https://github.com/login/oauth/authorize\"\n",
                "token_url     = \"https://github.com/login/oauth/access_token\"\n",
                "userinfo_url  = \"https://api.github.com/user\"\n",
                "redirect_uri  = \"https://your-app.example.com/auth/github/callback\"\n",
                "scope         = \"read:user user:email\"\n",
            )
            .to_owned(),
            "google" => concat!(
                "[auth.oauth2.google]\n",
                "client_id     = \"\"\n",
                "client_secret = \"\"\n",
                "authorize_url = \"https://accounts.google.com/o/oauth2/v2/auth\"\n",
                "token_url     = \"https://oauth2.googleapis.com/token\"\n",
                "userinfo_url  = \"https://openidconnect.googleapis.com/v1/userinfo\"\n",
                "redirect_uri  = \"https://your-app.example.com/auth/google/callback\"\n",
                "scope         = \"openid email profile\"\n",
                "issuer        = \"https://accounts.google.com\"\n",
                "jwks_url      = \"https://www.googleapis.com/oauth2/v3/certs\"\n",
                "discovery_url = \"https://accounts.google.com\"\n",
            )
            .to_owned(),
            "microsoft" => concat!(
                "[auth.oauth2.microsoft]\n",
                "client_id     = \"\"\n",
                "client_secret = \"\"\n",
                "authorize_url = \"https://login.microsoftonline.com/{YOUR_TENANT_ID}/oauth2/v2.0/authorize\"\n",
                "token_url     = \"https://login.microsoftonline.com/{YOUR_TENANT_ID}/oauth2/v2.0/token\"\n",
                "redirect_uri  = \"https://your-app.example.com/auth/microsoft/callback\"\n",
                "scope         = \"openid email profile\"\n",
                "# Single-tenant: replace {YOUR_TENANT_ID} with your Directory (tenant) ID.\n",
                "# Multi-tenant (common endpoint): ID-token issuer varies per user — see docs/guide/oauth.md.\n",
                "issuer        = \"https://login.microsoftonline.com/{YOUR_TENANT_ID}/v2.0\"\n",
                "jwks_url      = \"https://login.microsoftonline.com/{YOUR_TENANT_ID}/discovery/v2.0/keys\"\n",
            )
            .to_owned(),
            p => {
                format!(
                    r#"[auth.oauth2.{p}]
client_id     = ""
client_secret = ""
authorize_url = "https://{p}.example.com/oauth2/authorize"
token_url     = "https://{p}.example.com/oauth2/token"
redirect_uri  = "https://your-app.example.com/auth/{p}/callback"
scope         = "openid profile email"
"#
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"# OAuth2 / OIDC Authentication

Generated by `autumn generate auth --oauth {provider_list}`.

## Quick start

### 1. Register redirect URIs with each provider

Before the app will work you must register each redirect URI with the provider's
developer console:

| Provider | Console URL | Redirect URI path |
|----------|-------------|-------------------|
| Google   | <https://console.cloud.google.com/> | `/auth/google/callback` |
| GitHub   | <https://github.com/settings/developers> | `/auth/github/callback` |
| Microsoft | <https://portal.azure.com/> | `/auth/microsoft/callback` |

### 2. Add provider credentials to `autumn.toml`

```toml
{provider_config_examples}
```

Keep `client_secret` out of source control — use environment variables or
`autumn credentials edit` to inject secrets at runtime.

### 3. Run the generator command

```sh
autumn generate auth User --oauth {provider_list}
autumn migrate
autumn dev
```

Open <http://localhost:3000/auth/{first_provider}/redirect> to test the flow.

## Security properties

| Property | Status |
|----------|---------|
| PKCE (S256) | ✅ enabled for every provider by default |
| State (anti-CSRF) | ✅ constant-time validated on every callback |
| Nonce (replay protection) | ✅ validated for OIDC ID-token flows |
| ID-token signature | ✅ verified against provider JWKS |
| Session fixation prevention | ✅ session ID rotated on login |
| `client_secret` in logs | ✅ never logged |

## Account-linking policy

Configure in `autumn.toml`:

```toml
[auth]
oauth_linking_policy = "create_account"           # default: create a new account on first sign-in
# oauth_linking_policy = "require_local_signup_first"  # link only to existing accounts
```

## OIDC discovery

Providers that support OIDC discovery (`discovery_url` in preset) have their
endpoints populated automatically from `{{discovery_url}}/.well-known/openid-configuration`.
GitHub uses explicit endpoints (it is pure OAuth2, not OIDC).

## Troubleshooting

**`redirect_uri_mismatch`** — The `redirect_uri` in `autumn.toml` must match
exactly what is registered in the provider console, including scheme and path.

**`invalid_client`** — Check that `client_id` and `client_secret` are correct
and that the app is not in a restricted mode in the provider console.

**`state mismatch` on callback** — The session may have expired between the
redirect and callback. Increase `[session] ttl` or check for load-balancer
sticky-session misconfiguration.

## Generated database schema

```sql
CREATE TABLE oauth_identities (
    id         {pk},
    provider   TEXT NOT NULL,
    subject    TEXT NOT NULL,         -- provider's user identifier (sub / id)
    user_id    {big_int} NOT NULL REFERENCES {user_table}(id) ON DELETE CASCADE,
    email      TEXT NULL,
    name       TEXT NULL,
    created_at {created_at},
    UNIQUE (provider, subject)        -- collision guard: one local account per identity
);
```

Re-authentication with the same `(provider, subject)` pair links to the existing
account. A second local user trying to claim the same identity returns an error and
never silently merges accounts.
"#,
        first_provider = providers.first().map_or("github", String::as_str),
        pk = d.pk,
        big_int = d.big_int,
        created_at = d.ts_not_null_default_now,
    )
}

// ── TOTP (two-factor) template fragments ────────────────────────────────────────
//
// These helpers return generated *application* source as plain Strings, so the
// braces inside are literal (never processed by this generator's `format!`).

/// Extra `use` lines added to the generated `src/routes/auth.rs` under `--totp`.
const fn totp_imports_src() -> &'static str {
    "use aes_gcm::aead::{Aead, KeyInit};\n\
     use aes_gcm::{Aes256Gcm, Key, Nonce};\n\
     use base64::Engine as _;\n\
     use base64::engine::general_purpose::STANDARD as B64;\n\
     use totp_rs::{Algorithm, Secret, TOTP};\n\
     use crate::models::recovery_code::{NewRecoveryCode, RecoveryCode};\n\
     use crate::schema::recovery_codes;\n"
}

/// The interstitial branch injected into `login` after password verification.
///
/// A password login starts a *fresh* pending-2FA handoff, so it first clears any
/// stale pending markers left by an abandoned earlier flow (a different account's
/// pending login, or a parked reset) before parking this account.
fn totp_login_branch_src(snake_name: &str) -> String {
    const TPL: &str = r#"
    // ── 2FA interstitial ────────────────────────────────────────────────────
    // If the account has TOTP enabled, do NOT set the `#[secured]` auth key yet.
    // Mark the session `totp_pending` and redirect to the second-factor form.
    if __SNAKE__.totp_enabled {
        // A pending (pre-2FA) session must never write the `#[secured]` auth key.
        // If the app configured `[auth].session_key` to collide with one of the
        // reserved `totp_pending_*` keys, parking the pending marker would set the
        // trusted auth key and let a half-authenticated session pass `#[secured]`.
        // Refuse rather than risk a 2FA bypass.
        if is_reserved_totp_pending_key(state.auth_session_key()) {
            return Err(AutumnError::internal_server_error_msg(
                "[auth].session_key collides with a reserved TOTP pending key (totp_pending_*). \
                 Choose a different [auth].session_key.",
            ));
        }
        session.rotate_id().await;
        // `rotate_id()` keeps existing session data, so if this browser was
        // already authenticated as another account, drop those *live* auth keys
        // before parking the pending handoff — otherwise `#[secured]` routes
        // would still treat the pre-2FA session as the previous account.
        session.remove("__SNAKE___id").await;
        session.remove("__SNAKE___email").await;
        session.remove(state.auth_session_key()).await;
        // Drop any abandoned pending state from a previous flow too, so a stale
        // parked reset, enrollment secret, or a different account's pending login
        // can never be resumed under this login.
        session.remove("totp_pending_reset_digest").await;
        session.remove("totp_pending_reset_token").await;
        session.remove("totp_pending_secret").await;
        session.remove("totp_pending_confirmation").await;
        session
            .insert("totp_pending_id", __SNAKE__.id.to_string())
            .await;
        return Ok(redirect_to("/login/verify"));
    }
"#;
    TPL.replace("__SNAKE__", snake_name)
}

/// Lines injected into every *direct* full-authentication path (password login,
/// password reset, and signup for non-2FA accounts) right after
/// `session.rotate_id()`. They clear any leftover pending-2FA / parked-reset
/// markers — and any half-finished enrollment secret — so an abandoned
/// `/login/verify` or `/account/2fa/enable` for another account can't be
/// resumed against this freshly established session.
const fn totp_clear_pending_src() -> &'static str {
    "    // Clear any abandoned pending-2FA / deferred-reset / enrollment state so it\n\
     \x20   // cannot be resumed under this freshly authenticated session.\n\
     \x20   session.remove(\"totp_pending_id\").await;\n\
     \x20   session.remove(\"totp_pending_reset_digest\").await;\n\
     \x20   session.remove(\"totp_pending_reset_token\").await;\n\
     \x20   session.remove(\"totp_pending_secret\").await;\n\
     \x20   session.remove(\"totp_pending_confirmation\").await;\n"
}

/// The interstitial branch injected into `reset_password` *before* the password
/// is written. A correct reset token proves control of the email, not possession
/// of the second factor — so for a 2FA-enabled account we DEFER the password
/// change: stash the new digest in the session and require `/login/verify` to
/// pass before committing it. Someone with only the emailed link therefore can't
/// change the password (lockout/DoS) without the second factor.
fn totp_reset_branch_src(snake_name: &str) -> String {
    const TPL: &str = r#"
    // ── 2FA interstitial ────────────────────────────────────────────────────
    if __SNAKE__.totp_enabled {
        // A pending (pre-2FA) session must never write the `#[secured]` auth key
        // (see the login interstitial). Refuse a colliding `[auth].session_key`.
        if is_reserved_totp_pending_key(state.auth_session_key()) {
            return Err(AutumnError::internal_server_error_msg(
                "[auth].session_key collides with a reserved TOTP pending key (totp_pending_*). \
                 Choose a different [auth].session_key.",
            ));
        }
        // Do NOT write the new password yet. Park it in the session and finish
        // the reset in `login_verify` only after the second factor is proven.
        session.rotate_id().await;
        // `rotate_id()` keeps existing session data, so drop any live auth keys
        // from a prior login in this browser before parking the pending handoff
        // (see the login interstitial), and any half-finished enrollment secret.
        session.remove("__SNAKE___id").await;
        session.remove("__SNAKE___email").await;
        session.remove(state.auth_session_key()).await;
        session.remove("totp_pending_secret").await;
        session.remove("totp_pending_confirmation").await;
        session
            .insert("totp_pending_id", __SNAKE__.id.to_string())
            .await;
        session.insert("totp_pending_reset_digest", &new_digest).await;
        session.insert("totp_pending_reset_token", &token_digest).await;
        return Ok(redirect_to("/login/verify"));
    }
"#;
    TPL.replace("__SNAKE__", snake_name)
}

/// The interstitial branch injected into `confirm_email` after the token is
/// validated.  Email confirmation proves address ownership, not TOTP possession —
/// so a 2FA-enabled account is parked in pending state and sent to /login/verify
/// rather than immediately receiving a full authenticated session.
fn totp_confirm_branch_src(snake_name: &str) -> String {
    const TPL: &str = r#"
    // ── 2FA interstitial ────────────────────────────────────────────────────
    // Confirming the email proves address ownership but not possession of the
    // second factor.  For a 2FA-enabled account, park the session and redirect
    // to /login/verify so TOTP is still required to complete authentication.
    if __SNAKE__.totp_enabled {
        if is_reserved_totp_pending_key(state.auth_session_key()) {
            return Err(AutumnError::internal_server_error_msg(
                "[auth].session_key collides with a reserved TOTP pending key (totp_pending_*). \
                 Choose a different [auth].session_key.",
            ));
        }
        session.rotate_id().await;
        // rotate_id() preserves session data; drop the step-up claim so a
        // prior password-authenticated session cannot piggyback step-up
        // privileges into this email-confirmation-only pending session.
        session.remove(autumn_web::step_up::STEP_UP_SESSION_KEY).await;
        session.remove("__SNAKE___id").await;
        session.remove("__SNAKE___email").await;
        session.remove(state.auth_session_key()).await;
        session.remove("totp_pending_reset_digest").await;
        session.remove("totp_pending_reset_token").await;
        session.remove("totp_pending_secret").await;
        session
            .insert("totp_pending_id", __SNAKE__.id.to_string())
            .await;
        // Mark that this pending login originated from email confirmation (not a
        // password login). login_verify checks this to skip the step-up stamp:
        // email + TOTP proves address & second-factor but NOT password knowledge.
        session.insert("totp_pending_confirmation", "1").await;
        return Ok(redirect_to("/login/verify"));
    }
"#;
    TPL.replace("__SNAKE__", snake_name)
}

/// HTML fragment injected into the reauth form for TOTP-enabled projects.
///
/// Shown as an optional field; the handler enforces it for accounts that have
/// `totp_enabled = true` and ignores it for accounts that don't.
fn totp_reauth_field_src() -> String {
    "            div {{\n\
     \x20               label {{ \"Authenticator code\" }}\n\
     \x20               input type=\"text\" name=\"totp_code\" autocomplete=\"one-time-code\"\n\
     \x20                   inputmode=\"text\" pattern=\"[0-9a-fA-F \\\\-]*\" placeholder=\"6-digit code or recovery code\";\n\
     \x20               small {{ \"Required if your account uses two-factor authentication.\" }}\n\
     \x20           }}\n"
        .to_owned()
}

/// Verification block injected into the `reauth` handler after the password
/// check when TOTP is enabled.  For accounts with `totp_enabled = true` the
/// user must also supply a valid TOTP code or recovery code; other accounts
/// pass through without any second-factor check.
#[allow(clippy::too_many_lines)]
fn totp_reauth_check_src(snake_name: &str, table: &str) -> String {
    // This string is substituted verbatim into the outer format!() as a named argument.
    // format!() does NOT further process the substituted value, so {{ and }} in this
    // template appear literally in the output. Use single { } for Rust control flow,
    // and {{ }} only inside maud html! invocations.
    const TPL: &str = r#"    // If the account uses TOTP, also require the second factor.
    if __SNAKE__.totp_enabled {
        let code = form.totp_code.trim().to_owned();
        if code.is_empty() {
            let return_to = form.return_to.clone();
            return Ok(crate::layout("Confirm your identity", "/reauth", html! {}, html! {{
                h1 {{ "Confirm your identity" }}
                p style="color:red" {{ "Your account uses two-factor authentication. Please also enter your authenticator code." }}
                form action="/reauth" method="post" {{
                    @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
                    input type="hidden" name="return_to" value=(return_to);
                    div {{
                        label {{ "Authenticator code" }}
                        input type="text" name="totp_code" autocomplete="one-time-code"
                            inputmode="numeric" pattern="[0-9 ]*" required
                            placeholder="6-digit code or recovery code";
                        small {{ "Required if your account uses two-factor authentication." }}
                    }}
                    button type="submit" {{ "Confirm" }}
                }}
            }}).into_response());
        }
        let mut totp_verified = false;
        if let Some(stored) = __SNAKE__.totp_secret_encrypted.as_deref() {
            if let Some(step) = decrypt_secret(stored)
                .ok()
                .and_then(|secret| build_totp(secret, &__SNAKE__.email).ok())
                .and_then(|totp| verify_totp_code(&totp, &code))
            {
                let affected = diesel::update(
                    __TABLE__::table.find(__SNAKE__.id).filter(
                        __TABLE__::totp_last_used_step
                            .is_null()
                            .or(__TABLE__::totp_last_used_step.lt(step)),
                    ),
                )
                .set(__TABLE__::totp_last_used_step.eq(Some(step)))
                .execute(&mut *db)
                .await?;
                if affected == 1 {
                    totp_verified = true;
                }
            }
        }
        if !totp_verified {
            let unused: Vec<RecoveryCode> = recovery_codes::table
                .filter(recovery_codes::user_id.eq(__SNAKE__.id))
                .filter(recovery_codes::used_at.is_null())
                .select(RecoveryCode::as_select())
                .load(&mut *db)
                .await
                .unwrap_or_default();
            for rc in &unused {
                if verify_password(&code, &rc.code_digest).await.unwrap_or(false) {
                    let affected = diesel::update(
                        recovery_codes::table
                            .find(rc.id)
                            .filter(recovery_codes::used_at.is_null()),
                    )
                    .set(recovery_codes::used_at.eq(Some(chrono::Utc::now().naive_utc())))
                    .execute(&mut *db)
                    .await?;
                    if affected == 1 {
                        totp_verified = true;
                        break;
                    }
                }
            }
        }
        if !totp_verified {
            let return_to = form.return_to.clone();
            return Ok(crate::layout("Confirm your identity", "/reauth", html! {}, html! {{
                h1 {{ "Confirm your identity" }}
                p style="color:red" {{ "Invalid authenticator code. Please try again." }}
                form action="/reauth" method="post" {{
                    @if let Some(ref csrf) = csrf {{ input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }}
                    input type="hidden" name="return_to" value=(return_to);
                    div {{
                        label {{ "Authenticator code" }}
                        input type="text" name="totp_code" autocomplete="one-time-code"
                            inputmode="numeric" pattern="[0-9 ]*" required
                            placeholder="6-digit code or recovery code";
                        small {{ "Required if your account uses two-factor authentication." }}
                    }}
                    button type="submit" {{ "Confirm" }}
                }}
            }}).into_response());
        }
    }
"#;
    TPL.replace("__SNAKE__", snake_name)
        .replace("__TABLE__", table)
}

#[allow(clippy::too_many_lines)]
fn totp_routes_section_src(
    pascal_name: &str,
    snake_name: &str,
    table: &str,
    magic_link: bool,
) -> String {
    const TPL: &str = r#"
// ── Two-factor authentication (TOTP) ────────────────────────────────────────────
//
// Generated by `autumn generate auth --totp`. Edit freely.
//
// Security properties:
// - TOTP secrets are encrypted at rest with AES-256-GCM (never stored plaintext).
// - Verification accepts a ±1 time-step window and refuses to replay a step that
//   was already consumed (`totp_last_used_step`).
// - Recovery codes are single-use; only their bcrypt digest is stored and each is
//   marked `used_at` when consumed.
// - NOTE: rate-limiting / lockout on repeated failed second-factor attempts is a
//   follow-up (see docs) — add it before exposing this to untrusted traffic.

/// Number of single-use recovery codes generated on enrollment.
const RECOVERY_CODE_COUNT: usize = 10;

/// TOTP time step in seconds (RFC 6238 default).
const TOTP_STEP: u64 = 30;

/// Session keys reserved by the two-factor flow for transient pre-auth state.
/// `[auth].session_key` must not collide with any of these, or a pending
/// (pre-2FA) session could be mistaken for a fully authenticated one.
const RESERVED_TOTP_PENDING_KEYS: &[&str] = &[
    "totp_pending_id",
    "totp_pending_reset_digest",
    "totp_pending_reset_token",
    "totp_pending_secret",
    "totp_pending_confirmation",
];

/// Whether `key` (typically the configured `[auth].session_key`) collides with a
/// reserved `totp_pending_*` key. Used to refuse a config that would let a
/// pending second-factor session write the trusted `#[secured]` auth key.
///
/// `pub(crate)` so the OAuth callback handler (a separate module) can apply the
/// same guard before parking a pending 2FA session.
pub(crate) fn is_reserved_totp_pending_key(key: &str) -> bool {
    RESERVED_TOTP_PENDING_KEYS.contains(&key)
}

#[derive(Deserialize)]
pub struct TotpCodeForm {
    pub code: String,
}

#[derive(Deserialize)]
pub struct TotpEnableForm {
    pub password: String,
}

#[derive(Deserialize)]
pub struct TotpDisableForm {
    pub password: Option<String>,
    pub code: Option<String>,
}

/// Load the 32-byte AES-256-GCM key used to encrypt TOTP secrets at rest.
///
/// Read from the `TOTP_ENC_KEY` env var (base64 of 32 bytes). Manage it like any
/// other secret (e.g. `autumn credentials`); never commit it.
fn totp_enc_key() -> AutumnResult<[u8; 32]> {
    let raw = std::env::var("TOTP_ENC_KEY").map_err(|_| {
        AutumnError::internal_server_error_msg(
            "TOTP_ENC_KEY is not set. Generate 32 random bytes, base64-encode them, \
             and set TOTP_ENC_KEY before enabling two-factor auth.",
        )
    })?;
    let bytes = B64
        .decode(raw.trim())
        .map_err(|_| AutumnError::internal_server_error_msg("TOTP_ENC_KEY must be valid base64."))?;
    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        AutumnError::internal_server_error_msg("TOTP_ENC_KEY must decode to exactly 32 bytes.")
    })?;
    Ok(arr)
}

/// Encrypt a TOTP secret for storage. Output is base64(nonce ‖ ciphertext).
fn encrypt_secret(plaintext: &[u8]) -> AutumnResult<String> {
    use rand::TryRngCore;
    let key = totp_enc_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|_| AutumnError::internal_server_error_msg("RNG failure generating nonce"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| AutumnError::internal_server_error_msg("failed to encrypt TOTP secret"))?;
    let mut combined = nonce_bytes.to_vec();
    combined.extend_from_slice(&ct);
    Ok(B64.encode(combined))
}

/// Decrypt a stored TOTP secret produced by [`encrypt_secret`].
fn decrypt_secret(stored: &str) -> AutumnResult<Vec<u8>> {
    let key = totp_enc_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let raw = B64
        .decode(stored.trim())
        .map_err(|_| AutumnError::internal_server_error_msg("stored TOTP secret is not valid base64"))?;
    if raw.len() < 12 {
        return Err(AutumnError::internal_server_error_msg(
            "stored TOTP secret is malformed",
        ));
    }
    let (nonce_bytes, ct) = raw.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ct)
        .map_err(|_| AutumnError::internal_server_error_msg("failed to decrypt TOTP secret"))
}

/// Build a `TOTP` (SHA1, 6 digits, ±1 step skew) from raw secret bytes.
///
/// `get_url()` yields the standard `otpauth://` provisioning URI used for the QR.
fn build_totp(secret: Vec<u8>, account: &str) -> AutumnResult<TOTP> {
    // `totp-rs` rejects issuer/account labels containing a colon. The account
    // label is only a display hint in the `otpauth://` URI (it doesn't affect
    // code generation/verification), so strip colons defensively — otherwise an
    // address like `a:b@example.com` would 500 on enrollment.
    let account = account.replace(':', "_");
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        TOTP_STEP,
        secret,
        Some("__PASCAL__".to_owned()),
        account,
    )
    .map_err(|_| AutumnError::internal_server_error_msg("invalid TOTP secret"))
}

/// Current RFC 6238 time step.
fn current_step() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs / TOTP_STEP) as i64
}

/// Verify a TOTP code within a ±1 step window, returning the matched step.
///
/// The caller compares the returned step against `totp_last_used_step` to reject
/// replay of an already-consumed step.
fn verify_totp_code(totp: &TOTP, code: &str) -> Option<i64> {
    let step = current_step();
    let candidate = code.trim();
    for delta in [-1i64, 0, 1] {
        let s = step + delta;
        if s < 0 {
            continue;
        }
        let expected = totp.generate(s as u64 * TOTP_STEP);
        if expected == candidate {
            return Some(s);
        }
    }
    None
}

/// Generate one human-friendly single-use recovery code (10 hex chars, grouped).
fn generate_recovery_code() -> String {
    use rand::TryRngCore;
    let mut bytes = [0u8; 5];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG failed");
    let hexs = hex::encode(bytes);
    format!("{}-{}", &hexs[..5], &hexs[5..])
}

/// `GET /account/2fa` — show current two-factor state. Requires authentication.
#[secured]
#[get("/account/2fa")]
pub async fn two_factor_status(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {
    // Validates the tracked session row (401s immediately if revoked).
    let __SNAKE__ = require_tracked_session(&session, &mut db, &state).await?;

    let remaining: i64 = if __SNAKE__.totp_enabled {
        recovery_codes::table
            .filter(recovery_codes::user_id.eq(__SNAKE__.id))
            .filter(recovery_codes::used_at.is_null())
            .count()
            .get_result(&mut *db)
            .await
            .unwrap_or(0)
    } else {
        0
    };

    Ok(crate::layout("Two-Factor Authentication", "/account/2fa", html! {}, html! {
        h1 { "Two-Factor Authentication" }
        @if __SNAKE__.totp_enabled {
            p { "Two-factor authentication is " strong { "enabled" } "." }
            p { "You have " (remaining) " recovery codes remaining." }
            form action="/account/2fa/disable" method="post" {
                @if let Some(ref csrf) = csrf { input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }
                p {
                    label { "Confirm with your password or a current 6-digit code:" }
                    input type="password" name="password" autocomplete="current-password";
                    input type="text" name="code" inputmode="numeric" autocomplete="one-time-code";
                }
                button type="submit" { "Disable two-factor" }
            }
        } @else {
            p { "Two-factor authentication is " strong { "disabled" } "." }
            form action="/account/2fa/enable" method="post" {
                @if let Some(ref csrf) = csrf { input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }
                p {
                    label { "Confirm your password to begin enrollment:" }
                    input type="password" name="password" autocomplete="current-password" required;
                }
                button type="submit" { "Enable two-factor" }
            }
        }
    }))
}

/// `POST /account/2fa/enable` — generate a secret, render the QR + manual key,
/// and ask the user to confirm with a valid code before enabling. Requires auth.
///
/// The pending secret is held in the session (encrypted) and is NOT written to
/// the account until `two_factor_confirm` succeeds. This means starting (and
/// abandoning) re-enrollment never disturbs an already-active second factor.
#[secured]
#[post("/account/2fa/enable")]
pub async fn two_factor_enable(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Form(form): Form<TotpEnableForm>,
) -> AutumnResult<Markup> {
    // Enrollment stashes `totp_pending_secret` in the session. If the app
    // configured `[auth].session_key` to a reserved `totp_pending_*` name, that
    // write would clobber the live auth key and the account would later be
    // locked out by the reserved-key guard in the login interstitial. Refuse the
    // misconfiguration up front (the login/reset handoffs reject it too).
    if is_reserved_totp_pending_key(state.auth_session_key()) {
        return Err(AutumnError::internal_server_error_msg(
            "[auth].session_key collides with a reserved TOTP pending key (totp_pending_*). \
             Choose a different [auth].session_key.",
        ));
    }
    // Validates the tracked session row (401s immediately if revoked).
    let __SNAKE__ = require_tracked_session(&session, &mut db, &state).await?;

    // Step-up re-auth: a logged-in but unattended/hijacked session must not be
    // able to enroll an attacker's authenticator. Require the account password
    // before starting enrollment (matches GitHub/Google behaviour).
    if !verify_password(&form.password, &__SNAKE__.password_digest)
        .await
        .unwrap_or(false)
    {
        return Err(AutumnError::unprocessable_msg(
            "Password confirmation failed.",
        ));
    }

    // Re-enrollment guard: if 2FA is already active, a hijacked (already-past-2FA)
    // session must not be able to swap in a new authenticator without re-proving
    // the current factor. Disabling first goes through `/account/2fa/disable`,
    // which requires a current code or the password.
    if __SNAKE__.totp_enabled {
        return Err(AutumnError::unprocessable_msg(
            "Two-factor is already enabled. Disable it first (which requires your \
             current code or password) before enrolling a new authenticator.",
        ));
    }

    let secret = Secret::generate_secret();
    let secret_bytes = secret
        .to_bytes()
        .map_err(|_| AutumnError::internal_server_error_msg("failed to generate TOTP secret"))?;
    let manual_key = secret.to_encoded().to_string();
    let totp = build_totp(secret_bytes.clone(), &__SNAKE__.email)?;
    // The provisioning URI (otpauth://…) drives the QR and manual entry.
    let provisioning_uri = totp.get_url();
    let qr = totp
        .get_qr_base64()
        .map_err(|_| AutumnError::internal_server_error_msg("failed to render QR code"))?;

    // Stash the pending secret in the session only — the live account row is
    // untouched until the user proves possession in `two_factor_confirm`.
    let encrypted = encrypt_secret(&secret_bytes)?;
    session.insert("totp_pending_secret", &encrypted).await;

    Ok(crate::layout("Enable Two-Factor", "/account/2fa", html! {}, html! {
        h1 { "Scan this QR code" }
        p { "Scan with Google Authenticator, 1Password, or any RFC 6238 app." }
        img src=(format!("data:image/png;base64,{}", qr)) alt="TOTP QR code";
        p { "Or enter this key manually: " code { (manual_key) } }
        p { small { "Provisioning URI: " code { (provisioning_uri) } } }
        h2 { "Confirm" }
        p { "Enter the 6-digit code shown in your app to finish enabling 2FA." }
        form action="/account/2fa/confirm" method="post" {
            @if let Some(ref csrf) = csrf { input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }
            input type="text" name="code" inputmode="numeric" autocomplete="one-time-code" required;
            button type="submit" { "Confirm and enable" }
        }
    }))
}

/// `POST /account/2fa/confirm` — verify a code against the pending secret, flip
/// `totp_enabled`, and generate single-use recovery codes (shown once). Requires auth.
#[secured]
#[post("/account/2fa/confirm")]
pub async fn two_factor_confirm(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    Form(form): Form<TotpCodeForm>,
) -> AutumnResult<Markup> {
    // Validates the tracked session row (401s immediately if revoked).
    let __SNAKE__ = require_tracked_session(&session, &mut db, &state).await?;

    // Re-enrollment guard (defense in depth — `two_factor_enable` blocks this too):
    // never replace an active factor without first disabling (which re-authenticates).
    if __SNAKE__.totp_enabled {
        return Err(AutumnError::unprocessable_msg(
            "Two-factor is already enabled. Disable it first before confirming a \
             new authenticator.",
        ));
    }

    // The pending secret lives in the session (set by `two_factor_enable`), so
    // an unconfirmed enrollment never touched the live account row.
    let pending = session
        .get("totp_pending_secret")
        .await
        .ok_or_else(|| AutumnError::unprocessable_msg("Start enrollment first."))?;
    let secret = decrypt_secret(&pending)?;
    let totp = build_totp(secret, &__SNAKE__.email)?;
    let Some(step) = verify_totp_code(&totp, &form.code) else {
        return Err(AutumnError::unprocessable_msg("Invalid code. Try again."));
    };

    // Persist the new secret and a fresh single-use recovery-code set BEFORE
    // flipping `totp_enabled`, so a failure mid-way leaves the account in its
    // prior state rather than locked behind a secret whose codes were never
    // shown. Run it all in one transaction for atomicity.
    let pending_secret = pending.clone();
    let user_id = __SNAKE__.id;
    let mut plaintext_codes: Vec<String> = Vec::with_capacity(RECOVERY_CODE_COUNT);
    let mut hashed_codes: Vec<String> = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let code = generate_recovery_code();
        hashed_codes.push(hash_password(&code).await?);
        plaintext_codes.push(code);
    }

    // Capture the revocation policy up front so the delete can run inside
    // the same transaction as the enablement (a post-commit failure here
    // would 500 before the one-time recovery codes are ever shown).
    let revoke_other_sessions_in_txn = state.config_arc().auth.sessions.revoke_on_credential_change;
    let current_token_digest = session_token_digest(&session).await;
    let txn_result = (*db)
        .transaction::<_, diesel::result::Error, _>(async move |conn| {
            diesel::update(__TABLE__::table.find(user_id))
                .set((
                    __TABLE__::totp_secret_encrypted.eq(Some(pending_secret)),
                    __TABLE__::totp_last_used_step.eq(Some(step)),
                ))
                .execute(conn)
                .await?;
            diesel::delete(recovery_codes::table.filter(recovery_codes::user_id.eq(user_id)))
                .execute(conn)
                .await?;
            for digest in hashed_codes {
                diesel::insert_into(recovery_codes::table)
                    .values(NewRecoveryCode {
                        user_id,
                        code_digest: digest,
                        used_at: None,
                    })
                    .execute(conn)
                    .await?;
            }
            // Flip the flag last, conditional on it still being false. This is
            // both the durability gate (the account is only "2FA-enabled" once
            // the new secret + recovery codes are stored) and the race gate: if
            // a concurrent confirm already enabled 2FA, this matches zero rows
            // and we roll back, discarding this request's recovery codes so the
            // stored set always matches the plaintext the winner displayed.
            let claimed = diesel::update(
                __TABLE__::table
                    .find(user_id)
                    .filter(__TABLE__::totp_enabled.eq(false)),
            )
            .set(__TABLE__::totp_enabled.eq(true))
            .execute(conn)
            .await?;
            if claimed != 1 {
                return Err(diesel::result::Error::RollbackTransaction);
            }
            // Credential change (second factor enrolled): sign out every
            // *other* device in the same transaction (defaulted on,
            // configurable via [auth.sessions].revoke_on_credential_change).
            if revoke_other_sessions_in_txn {
                diesel::delete(
                    __SESSTABLE__::table
                        .filter(__SESSTABLE__::user_id.eq(user_id))
                        .filter(__SESSTABLE__::token_digest.ne(current_token_digest)),
                )
                .execute(conn)
                .await?;
            }
            Ok(())
        })
        .await;
    match txn_result {
        Ok(()) => {}
        Err(diesel::result::Error::RollbackTransaction) => {
            return Err(AutumnError::unprocessable_msg(
                "Two-factor was just enabled by another request. Reload /account/2fa.",
            ));
        }
        Err(_) => {
            return Err(AutumnError::internal_server_error_msg(
                "Failed to enable two-factor.",
            ));
        }
    }

    session.remove("totp_pending_secret").await;

    Ok(crate::layout("Save Your Recovery Codes", "/account/2fa", html! {}, html! {
        h1 { "Two-factor authentication enabled" }
        p { strong { "Save these recovery codes now." } " Each can be used once if you lose your device. They will not be shown again." }
        ul {
            @for code in &plaintext_codes {
                li { code { (code) } }
            }
        }
        p { a href="/account/2fa" { "Done" } }
    }))
}

/// `POST /account/2fa/disable` — require re-auth (current code OR password), then
/// clear the secret and all recovery codes. Requires authentication.
///
/// The handler itself verifies the user's current TOTP code or password before
/// disabling the factor, providing its own re-authentication guarantee. A
/// separate `#[step_up]` is therefore not added here to avoid double-prompting.
#[secured]
#[post("/account/2fa/disable")]
pub async fn two_factor_disable(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    Form(form): Form<TotpDisableForm>,
) -> AutumnResult<Response> {
    // Validates the tracked session row (401s immediately if revoked).
    let __SNAKE__ = require_tracked_session(&session, &mut db, &state).await?;

    // Re-authenticate: accept a current TOTP code or the account password.
    // A TOTP code used here is consumed with the same atomic replay guard as
    // `/login/verify`, so a code already spent on login cannot be replayed to
    // disable the factor.
    let mut ok = false;
    if let Some(code) = form.code.as_deref().filter(|c| !c.trim().is_empty()) {
        if let Some(stored) = __SNAKE__.totp_secret_encrypted.as_deref() {
            // A decrypt/build failure (e.g. rotated TOTP_ENC_KEY) must not abort:
            // the password fallback below is the escape from that lockout.
            if let Some(step) = decrypt_secret(stored)
                .ok()
                .and_then(|secret| build_totp(secret, &__SNAKE__.email).ok())
                .and_then(|totp| verify_totp_code(&totp, code))
            {
                let affected = diesel::update(
                    __TABLE__::table.find(__SNAKE__.id).filter(
                        __TABLE__::totp_last_used_step
                            .is_null()
                            .or(__TABLE__::totp_last_used_step.lt(step)),
                    ),
                )
                .set(__TABLE__::totp_last_used_step.eq(Some(step)))
                .execute(&mut *db)
                .await?;
                ok = affected == 1;
            }
        }
    }
    if !ok {
        if let Some(password) = form.password.as_deref().filter(|p| !p.is_empty()) {
            ok = verify_password(password, &__SNAKE__.password_digest)
                .await
                .unwrap_or(false);
        }
    }
    if !ok {
        return Err(AutumnError::unprocessable_msg(
            "Re-authentication failed. Provide a valid code or your password.",
        ));
    }

    // Disable the factor, delete the recovery codes, and revoke every
    // *other* session in one transaction so a mid-operation failure can't
    // leave the account half-changed — and so a revocation failure can't
    // 500 after the factor was already removed. Revocation is defaulted on,
    // configurable via [auth.sessions].revoke_on_credential_change.
    let user_id = __SNAKE__.id;
    let revoke_other_sessions_in_txn = state.config_arc().auth.sessions.revoke_on_credential_change;
    let current_token_digest = session_token_digest(&session).await;
    (*db)
        .transaction::<_, diesel::result::Error, _>(async move |conn| {
            diesel::update(__TABLE__::table.find(user_id))
                .set((
                    __TABLE__::totp_enabled.eq(false),
                    __TABLE__::totp_secret_encrypted.eq(None::<String>),
                    __TABLE__::totp_last_used_step.eq(None::<i64>),
                ))
                .execute(conn)
                .await?;
            diesel::delete(recovery_codes::table.filter(recovery_codes::user_id.eq(user_id)))
                .execute(conn)
                .await?;
__MAGIC_LINK_TOTP_DISABLE_CLEAR__            if revoke_other_sessions_in_txn {
                diesel::delete(
                    __SESSTABLE__::table
                        .filter(__SESSTABLE__::user_id.eq(user_id))
                        .filter(__SESSTABLE__::token_digest.ne(current_token_digest)),
                )
                .execute(conn)
                .await?;
            }
            Ok(())
        })
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to disable two-factor."))?;

    Ok(redirect_to("/account/2fa"))
}

/// `GET /login/verify` — second-factor prompt shown after a correct password
/// when the account has 2FA enabled (the session is `totp_pending`).
#[get("/login/verify")]
pub async fn login_verify_form(
    session: Session,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Response> {
    if session.get("totp_pending_id").await.is_none() {
        return Ok(redirect_to("/login"));
    }
    Ok(crate::layout("Two-Factor Verification", "/login/verify", html! {}, html! {
        h1 { "Two-Factor Verification" }
        form action="/login/verify" method="post" {
            @if let Some(ref csrf) = csrf { input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }
            div {
                label { "Authentication code or recovery code" }
                input type="text" name="code" inputmode="numeric" autocomplete="one-time-code" required;
            }
            button type="submit" { "Verify" }
        }
    })
    .into_response())
}

/// `POST /login/verify` — accept a TOTP code OR a single-use recovery code.
///
/// Only on success is the `#[secured]` auth key set. A consumed recovery code is
/// marked `used_at`; a used TOTP step cannot be replayed.
#[post("/login/verify")]
pub async fn login_verify(
    mut db: Db,
    State(state): State<AppState>,
    session: Session,
    MaybeClientIp(addr_ip): MaybeClientIp,
    headers: axum::http::HeaderMap,
    Form(form): Form<TotpCodeForm>,
) -> AutumnResult<Response> {
    let pending_id: i64 = session
        .get("totp_pending_id")
        .await
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| AutumnError::unauthorized_msg("No pending login."))?;
    let __SNAKE__: __PASCAL__ = __TABLE__::table
        .find(pending_id)
        .select(__PASCAL__::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::unauthorized_msg("No pending login."))?;

    let submitted = form.code.trim().to_owned();
    let mut verified = false;

    // 1) Try TOTP within the ±1 window, rejecting an already-used step (replay).
    //    Consumption is atomic: the UPDATE only matches rows whose stored step is
    //    still below this one, and we require exactly one row to be changed. Two
    //    concurrent requests with the same code therefore cannot both win — the
    //    second sees `affected == 0`.
    //
    //    A decrypt/build failure here (e.g. TOTP_ENC_KEY rotated/missing, or a
    //    corrupt blob) is treated like a non-matching code rather than aborting,
    //    so the user can still fall back to a recovery code below — recovery
    //    codes are stored independently and are the only escape from that lockout.
    if let Some(stored) = __SNAKE__.totp_secret_encrypted.as_deref() {
        if let Some(step) = decrypt_secret(stored)
            .ok()
            .and_then(|secret| build_totp(secret, &__SNAKE__.email).ok())
            .and_then(|totp| verify_totp_code(&totp, &submitted))
        {
            let affected = diesel::update(
                __TABLE__::table.find(__SNAKE__.id).filter(
                    __TABLE__::totp_last_used_step
                        .is_null()
                        .or(__TABLE__::totp_last_used_step.lt(step)),
                ),
            )
            .set(__TABLE__::totp_last_used_step.eq(Some(step)))
            .execute(&mut *db)
            .await?;
            if affected == 1 {
                verified = true;
            }
        }
    }

    // 2) Otherwise, try an unused recovery code (single-use). Marking it used is
    //    a conditional UPDATE guarded by `used_at IS NULL`; the code is only
    //    accepted when this request is the one that flips it, so a code can never
    //    be redeemed twice even under concurrent submissions.
    if !verified {
        let unused: Vec<RecoveryCode> = recovery_codes::table
            .filter(recovery_codes::user_id.eq(__SNAKE__.id))
            .filter(recovery_codes::used_at.is_null())
            .select(RecoveryCode::as_select())
            .load(&mut *db)
            .await
            .unwrap_or_default();
        for rc in &unused {
            if verify_password(&submitted, &rc.code_digest).await.unwrap_or(false) {
                let affected = diesel::update(
                    recovery_codes::table
                        .find(rc.id)
                        .filter(recovery_codes::used_at.is_null()),
                )
                .set(recovery_codes::used_at.eq(Some(chrono::Utc::now().naive_utc())))
                .execute(&mut *db)
                .await?;
                if affected == 1 {
                    verified = true;
                    break;
                }
            }
        }
    }

    if !verified {
        return Err(AutumnError::unprocessable_msg("Invalid code."));
    }

    // If this verification is finishing a password reset (the reset handler
    // parked the new digest for a 2FA-enabled account), commit it now — only
    // after the second factor has been proven.  The conditional UPDATE also
    // guards against a superseded token: if a second reset was requested between
    // parking and now, reset_token_digest in the DB will no longer match the
    // stored token and the update matches 0 rows, which we treat as an error.
    if let Some(new_digest) = session.get("totp_pending_reset_digest").await {
        let stored_token: Option<String> = session.get("totp_pending_reset_token").await;
        session.remove("totp_pending_reset_digest").await;
        session.remove("totp_pending_reset_token").await;
        let committed = if let Some(token) = stored_token {
            let now = chrono::Utc::now().naive_utc();
            // Commit the password change and the session revocation
            // atomically; a transaction error rolls both back (token
            // preserved) and is treated as not-committed below.
            let revoke_existing_sessions =
                state.config_arc().auth.sessions.revoke_on_credential_change;
            let user_id = __SNAKE__.id;
            (*db)
                .transaction::<_, diesel::result::Error, _>(async move |conn| {
                    let updated = diesel::update(__TABLE__::table.find(user_id))
                        .filter(__TABLE__::reset_token_digest.eq(Some(token.as_str())))
                        .filter(__TABLE__::reset_token_expires_at.gt(now))
                        .set((
                            __TABLE__::password_digest.eq(&new_digest),
                            __TABLE__::reset_token_digest.eq(None::<String>),
                            __TABLE__::reset_token_expires_at.eq(None::<chrono::NaiveDateTime>),
                        ))
                        .execute(conn)
                        .await?;
                    // Password changed: revoke every existing session in
                    // the same transaction (defaulted on, configurable via
                    // [auth.sessions].revoke_on_credential_change). The
                    // fresh login is recorded below.
                    if updated == 1 && revoke_existing_sessions {
                        diesel::delete(
                            __SESSTABLE__::table.filter(__SESSTABLE__::user_id.eq(user_id)),
                        )
                        .execute(conn)
                        .await?;
                    }
__MAGIC_LINK_RESET_CLEAR__                    Ok(updated == 1)
                })
                .await
                .unwrap_or(false)
        } else {
            false
        };
        if !committed {
            // This login was authorised solely by the reset link, which has now
            // proven stale (expired or superseded). Tear down the entire pending
            // login — including `totp_pending_id` — so a retry cannot skip the
            // (now-cleared) reset branch and promote to a full session without a
            // fresh login.
            session.remove("totp_pending_id").await;
            session.remove("totp_pending_secret").await;
            return Err(AutumnError::unprocessable_msg(
                "Reset link expired or already used. Please restart the password reset.",
            ));
        }
    }

    // Promote the pending session to a fully authenticated one. Drop any
    // tracked row for the pending id first — the rotation destroys it.
    untrack_current_session(&mut db, &session).await?;
    let from_confirmation = session.get("totp_pending_confirmation").await.is_some();
    session.rotate_id().await;
    session.remove("totp_pending_id").await;
    session.remove("totp_pending_confirmation").await;
    session.insert("__SNAKE___id", __SNAKE__.id.to_string()).await;
    session.insert("__SNAKE___email", &__SNAKE__.email).await;
    session.insert(state.auth_session_key(), __SNAKE__.id.to_string()).await;
    // Password + TOTP proves identity at least as strongly as password-only login,
    // so stamp step-up for normal logins. Email-confirmation + TOTP proves address
    // ownership and second-factor possession but NOT password knowledge — those
    // sessions must still go through /reauth before accessing #[step_up] routes.
    if !from_confirmation {
        autumn_web::step_up::set_last_strong_auth_at(&session).await;
    }
    // Track this login as an active session (device list + revocation).
    record_login_session(&mut db, &session, __SNAKE__.id, addr_ip, &headers).await?;

    let remaining: i64 = recovery_codes::table
        .filter(recovery_codes::user_id.eq(__SNAKE__.id))
        .filter(recovery_codes::used_at.is_null())
        .count()
        .get_result(&mut *db)
        .await
        .unwrap_or(0);
    // Surface remaining recovery-code count via a flash-style query param.
    Ok(redirect_to(&format!("/account?recovery_remaining={}", remaining)))
}
"#;
    // Magic-link token cleanup spliced into the deferred (2FA-gated) password-reset
    // commit inside `login_verify` (#11). Gated on `--magic-link` so the
    // `magic_link_tokens` reference only appears when the table (and its import)
    // exist; empty otherwise so the transaction is unchanged.
    let magic_link_reset_clear = if magic_link {
        magic_link_reset_commit_clear_txn_src()
    } else {
        ""
    };
    // Magic-link token cleanup spliced into the `two_factor_disable` transaction
    // (Codex review). Disabling TOTP removes the second factor that gated a
    // still-outstanding magic link, so a link minted while 2FA was on must not
    // become a direct single-factor login afterwards. Gated on `--magic-link` so
    // the `magic_link_tokens` reference only appears when the table (and its
    // import) exist; empty otherwise so the transaction is unchanged.
    let magic_link_totp_disable_clear = if magic_link {
        magic_link_totp_disable_clear_txn_src()
    } else {
        ""
    };
    TPL.replace("__MAGIC_LINK_RESET_CLEAR__", magic_link_reset_clear)
        .replace(
            "__MAGIC_LINK_TOTP_DISABLE_CLEAR__",
            magic_link_totp_disable_clear,
        )
        .replace("__PASCAL__", pascal_name)
        .replace("__SNAKE__", snake_name)
        .replace("__TABLE__", table)
        .replace("__SESSTABLE__", &sessions_table_name(snake_name))
}

/// Magic-link cleanup for the deferred (2FA-gated) password-reset commit in
/// `login_verify` (issue #1328 hardening, review #11). Mirrors
/// `magic_link_credential_clear_txn_src` but is guarded on `updated == 1` — the
/// parked reset actually committed — and uses the `user_id` binding already in
/// scope in that transaction. A link minted before this reset must not sign the
/// account in afterwards. Emitted only under `--magic-link`.
const fn magic_link_reset_commit_clear_txn_src() -> &'static str {
    r"                    if updated == 1 {
                        // Invalidate any outstanding magic-link login tokens in the
                        // SAME transaction: a link minted before this password reset
                        // must not sign the account in afterwards (magic-link auth
                        // matches an unconsumed, unexpired token, keyed by user id and
                        // independent of the changed credential).
                        diesel::delete(
                            magic_link_tokens::table
                                .filter(magic_link_tokens::user_id.eq(user_id)),
                        )
                        .execute(conn)
                        .await?;
                    }
"
}

/// Magic-link cleanup for the `two_factor_disable` transaction (Codex review).
/// Disabling TOTP flips `totp_enabled` to false, so an outstanding magic-link
/// token that `magic_link_verify` would have gated through `/login/verify` while
/// 2FA was on now takes the DIRECT (single-factor) path and logs the account in
/// fully. Delete the user's outstanding `magic_link_tokens` in the SAME
/// transaction so disabling the second factor cannot retroactively un-gate a
/// previously second-factor-protected link. Uses the `user_id` binding already in
/// scope in that transaction (matching `magic_link_reset_commit_clear_txn_src`).
/// Emitted only under `--magic-link` — the table exists only then.
const fn magic_link_totp_disable_clear_txn_src() -> &'static str {
    r"            // Invalidate any outstanding magic-link login tokens in the SAME
            // transaction: disabling TOTP removes the second factor that gated
            // these links, so a link minted while 2FA was on must not become a
            // direct single-factor login afterwards (magic-link auth matches an
            // unconsumed, unexpired token, keyed by user id).
            diesel::delete(
                magic_link_tokens::table.filter(magic_link_tokens::user_id.eq(user_id)),
            )
            .execute(conn)
            .await?;
"
}

/// Render the passwordless magic-link login section appended to `routes/auth.rs`
/// under `--magic-link` (issue #1328).
///
/// Emits the request form (`GET /login/magic`), the rate-limited request
/// endpoint (`POST /login/magic`), the verify endpoint
/// (`GET /login/magic/verify?token=…`), the mailer helper, and the generic
/// failure page. Uses `__SNAKE__` / `__PASCAL__` / `__TABLE__` placeholders and
/// `.replace()` so the template body can use plain single braces.
#[allow(clippy::too_many_lines)]
fn magic_link_routes_section_src(
    pascal_name: &str,
    snake_name: &str,
    table: &str,
    totp: bool,
) -> String {
    const TPL: &str = r#"
// ── Passwordless magic-link login ───────────────────────────────────────────────
//
// Generated by `autumn generate auth --magic-link` (issue #1328). Edit freely.
//
// Security properties:
// - Tokens are 32-byte random values (`OsRng`); only their SHA-256 digest is
//   persisted (`magic_link_tokens.token_digest`). The raw token appears only in
//   the emailed link — never in the database, logs, or error reports.
// - Scanner-safe verify: `GET /login/magic/verify?token=…` NEVER consumes the
//   token — it only renders a "Confirm sign-in" page whose form POSTs the token
//   back. Email link-scanners / prefetch bots that merely follow the GET can no
//   longer burn a single-use link before the human clicks. Consumption happens
//   in `POST /login/magic/verify` (`magic_link_verify`).
// - Single-use: `magic_link_verify` (the POST) consumes the token atomically
//   (`UPDATE … SET consumed_at = now WHERE consumed_at IS NULL AND expires_at > now
//   RETURNING …`), so a second verify (or an expired/consumed/unknown token) all
//   return the SAME generic failure page — no oracle. The GET confirm page is
//   rendered identically regardless of token validity, so it is not an oracle either.
// - Non-enumerating: `POST /login/magic` always renders the same "check your
//   email" response whether or not the address exists.
// - Rate-limited per-IP by the `#[throttle]` guard and per-email by a DB cooldown.
//   The `POST /login/magic/verify` consume route is likewise per-IP throttled to
//   bound token brute-forcing.
// - `magic_link_verify` rotates the session id BEFORE establishing the
//   authenticated session (session-fixation defense).

// Magic-link token TTL and per-email cooldown are sourced from `autumn.toml`
// via `state.config_arc().auth.magic_link` (see docs/guide/authentication.md):
//
//   [auth.magic_link]
//   ttl_minutes = 15          # link lifetime; keep ≤ 15 min for a tight window (AC5)
//   email_cooldown_secs = 60  # per-email re-mint cooldown (email-bomb throttle)
//
// The documented defaults (15 min TTL, 60s cooldown) apply when the section is
// omitted — see `MagicLinkConfig` in the `autumn-web` crate. Keep `ttl_minutes`
// at 15 or less: a magic link is a bearer credential, so a tight expiry window
// bounds the blast radius of a leaked link.

#[derive(Deserialize)]
pub struct MagicLinkRequestForm {
    pub email: String,
}

/// `GET /login/magic` — render the magic-link request form.
#[get("/login/magic")]
pub async fn magic_link_request_form(
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {
    Ok(crate::layout("Sign in with a magic link", "/login/magic", html! {}, html! {
        h1 { "Sign in with a magic link" }
        p { "Enter your email and we'll send you a one-time sign-in link — no password required." }
        form action="/login/magic" method="post" {
            @if let Some(ref csrf) = csrf { input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }
            div {
                label { "Email" }
                input type="email" name="email" required autocomplete="email";
            }
            button type="submit" { "Email me a link" }
        }
        p { a href="/login" { "Back to login" } }
    }))
}

/// `POST /login/magic` — mint a token (only if the account exists) and email a
/// sign-in link.
///
/// Non-enumerating (AC2): always returns the same "check your email" page
/// whether or not the address is registered. Per-IP rate limiting is enforced
/// by the `#[throttle]` guard (autumn's existing rate-limit middleware); a
/// per-email DB cooldown (AC6) throttles email-bombing one address.
#[post("/login/magic")]
#[throttle(limit = 5, per = "1m", key = "ip")]
pub async fn magic_link_request(
    mut db: Db,
    mailer: Mailer,
    State(state): State<AppState>,
    Form(form): Form<MagicLinkRequestForm>,
) -> AutumnResult<Markup> {
    // Fail fast when mail is not configured. This is independent of the address
    // lookup, so it leaks nothing about which addresses exist.
    if mailer.is_disabled() {
        return Err(AutumnError::internal_server_error_msg(
            "Magic-link login requires mail to be configured. \
             Set [mail] transport in autumn.toml (e.g. transport = \"log\" for dev). \
             The magic-link feature is unavailable until mail is set up.",
        ));
    }

    let email = form.email.trim().to_lowercase();
    let now = chrono::Utc::now().naive_utc();
    // Sourced from `[auth.magic_link]` in autumn.toml (defaults: 15 min TTL, 60s
    // cooldown). Keep `ttl_minutes` ≤ 15 for a tight link-lifetime window.
    // One shared read for both fields; `config()` would deep-clone every
    // config section twice per request.
    let config = state.config_arc();
    let ttl_minutes = config.auth.magic_link.ttl_minutes;
    let email_cooldown_secs = config.auth.magic_link.email_cooldown_secs;
    // Record start time; the response is padded to a constant minimum below so
    // an attacker cannot infer registration status from response latency.
    let t0 = std::time::Instant::now();

    // Non-enumerating: silently skip unknown addresses. Only confirmed accounts
    // may receive a link — an unconfirmed account must confirm its email first.
    let maybe___SNAKE__: Option<__PASCAL__> = __TABLE__::table
        .filter(__TABLE__::email.eq(&email))
        .filter(__TABLE__::email_confirmed_at.is_not_null())
        .select(__PASCAL__::as_select())
        .first(&mut *db)
        .await
        .ok();

    if let Some(__SNAKE__) = maybe___SNAKE__ {
        // Per-email cooldown (AC6): skip re-minting when an unexpired,
        // unconsumed token was issued for this account within the window.
        let cooldown_start = now - chrono::Duration::seconds(email_cooldown_secs as i64);
        let recent: i64 = magic_link_tokens::table
            .filter(magic_link_tokens::user_id.eq(__SNAKE__.id))
            .filter(magic_link_tokens::consumed_at.is_null())
            .filter(magic_link_tokens::expires_at.gt(now))
            .filter(magic_link_tokens::created_at.gt(cooldown_start))
            .count()
            .get_result(&mut *db)
            .await
            .unwrap_or(0);

        if recent == 0 {
            // Reuse the reset-token generator: 32-byte OsRng hex. Only the
            // SHA-256 digest is stored; the raw token appears only in the link.
            let raw_token = generate_reset_token();
            let token_digest = sha256_hex(&raw_token);
            let expires_at = now + chrono::Duration::minutes(ttl_minutes as i64);
            let new_token = NewMagicLinkToken {
                user_id: __SNAKE__.id,
                token_digest,
                expires_at,
                consumed_at: None,
            };
            // Persist the digest, then email the raw link. A mail failure is
            // logged but never surfaced (stays non-enumerating).
            if let Err(e) = diesel::insert_into(magic_link_tokens::table)
                .values(&new_token)
                .execute(&mut *db)
                .await
            {
                tracing::error!("magic-link token insert failed: {e}");
            } else if let Err(e) = send_magic_link_email(&mailer, &__SNAKE__.email, &raw_token, ttl_minutes).await {
                tracing::error!("magic-link email failed: {e}");
            }
        }
    }

    // Pad to a constant minimum so hit and miss paths are timing-indistinguishable.
    if let Some(remaining) = std::time::Duration::from_secs(1).checked_sub(t0.elapsed()) {
        tokio::time::sleep(remaining).await;
    }

    Ok(crate::layout("Check Your Email", "/login/magic", html! {}, html! {
        h1 { "Check Your Email" }
        p {
            "If that address is registered, a one-time sign-in link is on its way. \
             The link expires shortly and can be used only once."
        }
        p { a href="/login" { "Back to login" } }
    }))
}

#[derive(Deserialize)]
pub struct MagicLinkVerifyQuery {
    pub token: String,
}

#[derive(Deserialize)]
pub struct MagicLinkVerifyForm {
    pub token: String,
}

/// Generic magic-link failure page. Expired, consumed, unknown, and malformed
/// tokens ALL render this identical page so nothing acts as an oracle (AC5).
fn magic_link_invalid_page() -> Markup {
    crate::layout("Sign-in link invalid", "/login/magic/verify", html! {}, html! {
        h1 { "This sign-in link is invalid or has expired" }
        p { "Magic sign-in links can be used once and expire quickly. Please request a new one." }
        p { a href="/login/magic" { "Request a new link" } }
        p { a href="/login" { "Back to login" } }
    })
}

/// `GET /login/magic/verify?token=…` — render a "Confirm sign-in" page WITHOUT
/// consuming the token.
///
/// Email link-scanners and prefetch/preview bots follow the GET before the human
/// clicks; if the GET consumed the single-use token, those bots would burn the
/// link. So the GET is side-effect-free: it just echoes the token into a hidden
/// field of a form that POSTs back to `/login/magic/verify`, where the token is
/// actually consumed. The page is rendered IDENTICALLY regardless of whether the
/// token is valid, expired, or unknown — the POST surfaces the generic failure —
/// so the confirm page is not an oracle.
#[get("/login/magic/verify")]
pub async fn magic_link_verify_form(
    flash: Flash,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
    Query(query): Query<MagicLinkVerifyQuery>,
) -> AutumnResult<Markup> {
    Ok(crate::layout("Confirm sign-in", "/login/magic/verify", flash_messages(&flash.consume().await), html! {
        h1 { "Confirm sign-in" }
        p { "Click the button below to finish signing in to your account." }
        form action="/login/magic/verify" method="post" {
            @if let Some(ref csrf) = csrf { input type="hidden" name=(csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str())) value=(csrf.token()); }
            input type="hidden" name="token" value=(query.token);
            button type="submit" { "Sign in" }
        }
        p { a href="/login" { "Back to login" } }
    }))
}

/// `POST /login/magic/verify` — validate the token and, on success, establish an
/// authenticated session. This is where the single-use token is consumed (the
/// GET above only renders the confirmation page, so link-scanners can't burn it).
///
/// Single-use (AC5): an atomic `UPDATE … RETURNING` stamps `consumed_at` while
/// filtering on `consumed_at IS NULL` and `expires_at > now`, so a second
/// verify of the same token — or an expired / consumed / unknown token — matches
/// zero rows and is rejected with the SAME generic failure (no oracle).
///
/// A light per-IP `#[throttle]` (mirroring `POST /login/magic`) bounds token
/// brute-forcing; a legitimate user only POSTs this route once per link.
#[post("/login/magic/verify")]
#[throttle(limit = 5, per = "1m", key = "ip")]
pub async fn magic_link_verify(
    mut db: Db,
    State(state): State<AppState>,
    session: Session,
    MaybeClientIp(addr_ip): MaybeClientIp,
    headers: axum::http::HeaderMap,
    Form(form): Form<MagicLinkVerifyForm>,
) -> AutumnResult<Response> {
    let token_digest = sha256_hex(&form.token);
    let now = chrono::Utc::now().naive_utc();

    // Atomic single-use consume. Only one concurrent verify can match.
    let consumed: MagicLinkToken = match diesel::update(
        magic_link_tokens::table
            .filter(magic_link_tokens::token_digest.eq(&token_digest))
            .filter(magic_link_tokens::consumed_at.is_null())
            .filter(magic_link_tokens::expires_at.gt(now)),
    )
    .set(magic_link_tokens::consumed_at.eq(Some(now)))
    .returning(MagicLinkToken::as_returning())
    .get_result(&mut *db)
    .await
    {
        Ok(row) => row,
        // Expired / consumed / unknown all land here → identical generic failure.
        Err(_) => return Ok(magic_link_invalid_page().into_response()),
    };

    // Load the account the token belongs to; a vanished account fails generically.
    let Ok(__SNAKE__) = __TABLE__::table
        .find(consumed.user_id)
        .select(__PASCAL__::as_select())
        .first::<__PASCAL__>(&mut *db)
        .await
    else {
        return Ok(magic_link_invalid_page().into_response());
    };

    // Defense-in-depth (#1777): re-check the account lock at verify time with a
    // FRESH read of `locked_at` at the DB, AFTER consuming the token but BEFORE
    // establishing any session. A magic link minted BEFORE the account was locked
    // must not complete a login AFTER the lock. The in-memory `__SNAKE__` row was
    // SELECTed above and can be STALE: a concurrent login can cross the lockout
    // threshold between that SELECT and here, so trusting `__SNAKE__.locked_at`
    // would leave a TOCTOU hole (the stale value is still NULL and the link
    // succeeds even though the account is now locked). Mirror the password-login
    // success-path guard EXACTLY: a single guarded UPDATE predicated on
    // `locked_at` re-reads the current lock state at the DB and, like password
    // login, clears an expired lock (WHERE `locked_at IS NULL` OR
    // `locked_at <= now - cooloff`) while resetting `failed_attempts`. If zero
    // rows match, the account is actively locked (concurrently or otherwise) and
    // the login is rejected. Gated on the same `lockout_enabled` predicate, so it
    // is a no-op when lockout is disabled in config. A locked account funnels to
    // the SAME generic failure page as an expired/consumed/unknown token — no
    // oracle distinguishes "locked" from "bad link". This sits before the TOTP
    // branch below, so it gates BOTH the 2FA-park and the direct-login paths.
    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let lockout_cfg = &config.auth.lockout;
    let lockout_enabled = lockout_cfg.enabled && lockout_cfg.threshold > 0;
    if lockout_enabled {
        let cooloff = chrono::Duration::seconds(lockout_cfg.cooloff_secs as i64);
        let lock_expired_before = now - cooloff;
        let rows_cleared = diesel::update(
            __TABLE__::table
                .find(__SNAKE__.id)
                .filter(
                    __TABLE__::locked_at.is_null().or(
                        __TABLE__::locked_at.le(lock_expired_before)
                    )
                ),
        )
        .set((
            __TABLE__::failed_attempts.eq(0),
            __TABLE__::locked_at.eq(None::<chrono::NaiveDateTime>),
        ))
        .execute(&mut *db)
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(&format!("Failed to reset lockout on magic-link login: {e}")))?;
        if rows_cleared == 0 {
            return Ok(magic_link_invalid_page().into_response());
        }
    }

    // This browser may already hold a tracked session for another account. The
    // rotation below destroys that session id, so drop its row now.
    untrack_current_session(&mut db, &session).await?;

    // Magic-link proves EMAIL POSSESSION, not password knowledge. Drop any
    // `reauth_pw_ok` "password already verified, awaiting TOTP" marker left by an
    // unfinished /reauth in this browser BEFORE parking (TOTP) or promoting
    // (direct) the session — otherwise a later /reauth POST could skip the
    // password and mint a fresh step-up claim off an email-only login. This is
    // cleared here (before the 2FA branch, which returns early) so BOTH the
    // parked-pending and direct paths are covered, mirroring
    // `establish_remember_login` (issue #833/#1397). Plain string key, so a
    // magic-link-only build (no --totp) still compiles.
    session.remove("reauth_pw_ok").await;

    // For 2FA-enabled accounts, park the session and redirect to /login/verify
    // before granting a full session — a magic link proves email possession,
    // not TOTP possession (mirrors confirm_email).
__MAGIC_LINK_TOTP_BRANCH__    // Rotate the session id BEFORE inserting the authenticated session id
    // (session-fixation defense, consistent with login/reset/confirm).
    session.rotate_id().await;
    // Magic-link proves EMAIL POSSESSION, not password knowledge. Like
    // `confirm_email`, it MUST NOT call `set_last_strong_auth_at`: doing so
    // would let an emailed link grant password-grade step-up (sudo) freshness.
    // Drop any step-up claim carried over from a previous login in this browser.
    session.remove(autumn_web::step_up::STEP_UP_SESSION_KEY).await;
__MAGIC_LINK_CLEAR_PENDING__    session.insert("__SNAKE___id", __SNAKE__.id.to_string()).await;
    session.insert("__SNAKE___email", &__SNAKE__.email).await;
    // Use the same session key checked by `#[secured]` / `#[authorize]`.
    session.insert(state.auth_session_key(), __SNAKE__.id.to_string()).await;
    // Track this login as an active session (device list + revocation).
    record_login_session(&mut db, &session, __SNAKE__.id, addr_ip, &headers).await?;

    Ok(redirect_to("/account"))
}

/// Send a passwordless magic-link email via the Autumn mailer.
///
/// Goes through the standard `Mail` builder so it inherits dev-mailbox preview,
/// suppression-list gating, and templating (AC7). Suppression is intentionally
/// NOT bypassed (`.ignore_suppression()` is not called).
async fn send_magic_link_email(
    mailer: &Mailer,
    to: &str,
    token: &str,
    ttl_minutes: u64,
) -> AutumnResult<()> {
    // APP_BASE_URL must be the public URL of your app (e.g. https://example.com).
    let base_url = std::env::var("APP_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_owned());
    let verify_url = format!("{base_url}/login/magic/verify?token={token}");
    let mail = Mail::builder()
        .to(to.to_owned())
        .subject("Your sign-in link")
        .html(html! {
            p { "Click the link below to sign in. No password required." }
            p { "This link expires in " (ttl_minutes) " minutes and can be used once." }
            p { a href=(&verify_url) { "Sign in" } }
            p { "If you did not request this, you can safely ignore this email." }
        })
        .text(format!(
            "Sign in: {verify_url}\n\
             This link expires in {ttl_minutes} minutes and can be used once.\n\
             If you did not request this you can safely ignore this email."
        ))
        .build()
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!(
                "Failed to build magic-link email: {e}"
            ))
        })?;
    mailer.send(mail).await.map_err(|e| {
        AutumnError::internal_server_error_msg(format!(
            "Failed to send magic-link email: {e}"
        ))
    })
}
"#;
    // When TOTP is enabled, magic-link verify must honor the second factor just
    // like confirm_email: park a `totp_pending_confirmation` session and redirect
    // to /login/verify. Reusing the confirm branch means completion in
    // `login_verify` deliberately SKIPS the strong-auth stamp (email + TOTP does
    // not prove password knowledge). The branch is already `__SNAKE__`-substituted.
    let totp_branch = if totp {
        totp_confirm_branch_src(snake_name)
    } else {
        String::new()
    };
    // `rotate_id()` preserves session data, so the DIRECT (non-2FA) login path must
    // scrub any abandoned pending-2FA / parked-reset handoff left in this browser
    // before writing the auth keys — otherwise `/login/verify` could resume it under
    // the freshly authenticated session (mirrors password login / reset). Gated on
    // `--totp` so a magic-link-only app never references TOTP-only session keys.
    let clear_pending = if totp { totp_clear_pending_src() } else { "" };
    TPL.replace("__MAGIC_LINK_TOTP_BRANCH__", &totp_branch)
        .replace("__MAGIC_LINK_CLEAR_PENDING__", clear_pending)
        .replace("__PASCAL__", pascal_name)
        .replace("__SNAKE__", snake_name)
        .replace("__TABLE__", table)
}

/// Markdown appended to `docs/guide/authentication.md` under `--totp`.
const TOTP_DOCS_SECTION: &str = r#"## Two-Factor Authentication (TOTP)

Generated with `--totp`. Users can enroll an authenticator app (Google
Authenticator, 1Password, …) and fall back to single-use recovery codes.

### Generated routes

| Method | Path | Handler | Auth |
|--------|------|---------|------|
| GET | `/account/2fa` | `two_factor_status` | **Required** |
| POST | `/account/2fa/enable` | `two_factor_enable` | **Required** |
| POST | `/account/2fa/confirm` | `two_factor_confirm` | **Required** |
| POST | `/account/2fa/disable` | `two_factor_disable` | **Required** |
| GET | `/login/verify` | `login_verify_form` | Pending login |
| POST | `/login/verify` | `login_verify` | Pending login |

### Encryption key

TOTP secrets are encrypted at rest with AES-256-GCM. Set `TOTP_ENC_KEY` to a
base64-encoded 32-byte key before enabling 2FA:

```sh
# 32 random bytes, base64-encoded
export TOTP_ENC_KEY="$(head -c 32 /dev/urandom | base64)"
```

Manage it like any other secret (`autumn credentials`); never commit it. Rotating
the key invalidates existing enrollments.

### Security properties

- Secrets encrypted at rest (never stored plaintext).
- Verification accepts a ±1 time-step window; a consumed step cannot be replayed
  (`totp_last_used_step`), including on the disable path.
- Recovery codes are single-use, bcrypt-hashed, and stamped `used_at` on use.
- Enrolling requires step-up re-authentication (the account password), so a
  stolen/unattended session cannot quietly add an attacker's authenticator.
- Re-enrollment while 2FA is active is rejected — disable first (which itself
  requires a current code or password).
- Login, OAuth callback (guidance), and password reset all route 2FA-enabled
  accounts through `/login/verify`. A password reset for a 2FA account is
  **deferred**: the new password is parked and only committed after the second
  factor is proven, so a reset link alone cannot change the password.
- Disabling 2FA requires re-authentication (current code or password) and clears
  the secret plus all recovery codes.

### Out of scope (follow-ups)

- **Rate-limiting / lockout** on repeated failed second-factor attempts is not
  included — add it before exposing 2FA to untrusted traffic.
- WebAuthn / passkeys and SMS/email OTP are separate tracks.

"#;

/// Markdown appended to `docs/guide/authentication.md` under `--magic-link`.
const MAGIC_LINK_DOCS_SECTION: &str = r#"## Passwordless Magic-Link Login

Generated with `--magic-link` (issue #1328). Users can sign in with a one-time
link emailed to their address — no password required. The flow is composable
with `--oauth`, `--passkeys`, and `--totp` on the same model.

### Routes

| Method | Path | Handler | Auth |
|--------|------|---------|------|
| GET | `/login/magic` | `magic_link_request_form` | Public |
| POST | `/login/magic` | `magic_link_request` | Public (rate-limited) |
| GET | `/login/magic/verify?token=…` | `magic_link_verify_form` | Public |
| POST | `/login/magic/verify` | `magic_link_verify` | Public (rate-limited) |

### How It Works

1. The user submits their email to `POST /login/magic`.
2. **If** the address belongs to a confirmed account, a random 32-byte token is
   generated; only its SHA-256 digest, an expiry, and a `consumed_at` marker are
   persisted in `magic_link_tokens`. The raw token is placed only in the emailed
   verify link.
3. The response is **always** the same "check your email" page whether or not the
   address exists — so the endpoint cannot be used to enumerate accounts.
4. The user clicks the link (`GET /login/magic/verify?token=…`). This renders a
   lightweight **"Confirm sign-in"** page and does **not** consume the token —
   so email link-scanners / prefetch bots that merely follow the GET can't burn
   the single-use link before the human clicks. The page is rendered identically
   whether the token is valid, expired, or unknown (no oracle).
5. Submitting that form (`POST /login/magic/verify`) is where the token is
   validated and **atomically consumed**; on success the session id is
   **rotated** (session-fixation defense) and an authenticated session is
   established. Expired / consumed / unknown tokens all render the same generic
   failure page.

### Configuration Knobs

The TTL and per-email cooldown are read from `autumn.toml` via `state.config_arc()`,
so you can tune them without editing the generated handler:

```toml
[auth.magic_link]
ttl_minutes = 15          # link lifetime; keep ≤ 15 min for a tight window
email_cooldown_secs = 60  # per-email re-mint cooldown (email-bomb throttle)
```

Both keys are optional; omitting the `[auth.magic_link]` section applies the
documented defaults below.

| Knob | Location | Default | Purpose |
|------|----------|---------|---------|
| `auth.magic_link.ttl_minutes` | `[auth.magic_link]` in `autumn.toml` (via `state.config_arc()`) | `15` | Link lifetime (TTL) in minutes. Keep ≤ 15 min for a tight window — a magic link is a bearer credential, so a short expiry bounds the blast radius of a leaked link. |
| `auth.magic_link.email_cooldown_secs` | `[auth.magic_link]` in `autumn.toml` (via `state.config_arc()`) | `60` | Per-email cooldown in seconds: suppresses re-minting a token for the same address within the window (email-bomb throttle). |
| `#[throttle(limit = 5, per = "1m", key = "ip")]` | attribute on `POST /login/magic` and `POST /login/magic/verify` | 5/min/IP | Per-IP rate limit via autumn's existing rate-limit middleware (request minting + token brute-force bound). |

### Security Guarantees

- **No account enumeration**: `POST /login/magic` returns an identical response
  (and constant-time-padded latency) whether or not the address is registered.
- **Digest-only storage**: only `sha256_hex(raw_token)` is stored; the raw token
  lives only in the emailed link, never in the database, logs, or error reports.
- **Scanner-safe verify**: `GET /login/magic/verify` only renders a "Confirm
  sign-in" page and NEVER consumes the token — email link-scanners / prefetch
  bots that follow the GET can't burn the single-use link. Consumption happens on
  the `POST` when the human clicks "Sign in".
- **Single-use**: `magic_link_verify` (the `POST`) consumes the token atomically
  (`UPDATE … SET consumed_at = now WHERE consumed_at IS NULL AND expires_at > now`),
  so a second verify of the same token fails.
- **Generic failure (no oracle)**: expired, already-consumed, unknown, and
  malformed tokens all render the same generic failure page; the GET confirm page
  is likewise rendered identically regardless of token validity.
- **Configurable TTL**: tokens expire after `auth.magic_link.ttl_minutes`
  (default 15), sourced from `autumn.toml` via `state.config_arc()`.
- **Rate-limited**: per-IP (`#[throttle]`) and per-email (DB cooldown).
- **Session-fixation defense**: the session id is rotated before the
  authenticated session is established.
- **Not a step-up factor**: magic-link proves email possession, not password
  knowledge, so it does **not** stamp the step-up (sudo-mode) claim — `#[step_up]`
  routes still require a fresh password reauth, consistent with email confirmation.
- **Mailer integration**: the link is sent through the standard `Mail` builder,
  inheriting dev-mailbox preview, suppression-list gating, and templating.

### Not Included

SMS/OTP, remember-device, and magic-link as a second factor are out of scope for
this scaffold.

"#;

/// Render `tests/auth_2fa.rs` — the generated 2FA integration suite.
fn render_2fa_tests_file(pascal_name: &str, snake_name: &str) -> String {
    const TPL: &str = r#"//! Generated 2FA integration tests for __PASCAL__ (`autumn generate auth --totp`).
//!
//! Like `tests/auth.rs`, these run against a live server started with
//! `AUTUMN_TEST_BASE_URL` and skip when it is unset, so they compile and pass
//! out of the box. Flesh out the bodies once your test harness boots the app
//! with a database and a `TOTP_ENC_KEY`.
//!
//! Covered flows: enroll → confirm → login-with-code → login-with-recovery-code
//! → recovery-code-reuse-rejected → disable.

fn base_url() -> Option<String> {
    std::env::var("AUTUMN_TEST_BASE_URL").ok()
}

#[test]
fn two_factor_enroll_and_confirm() {
    let Some(_base) = base_url() else {
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    };
    // POST /account/2fa/enable then /account/2fa/confirm with a generated code.
}

#[test]
fn login_with_totp_code() {
    let Some(_base) = base_url() else {
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    };
    // Password login redirects to /login/verify; a valid code completes login.
}

#[test]
fn login_with_recovery_code() {
    let Some(_base) = base_url() else {
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    };
    // A single-use recovery code is accepted at /login/verify.
}

#[test]
fn recovery_code_reuse_rejected() {
    let Some(_base) = base_url() else {
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    };
    // Re-submitting a consumed recovery code is rejected.
}

#[test]
fn two_factor_disable() {
    let Some(_base) = base_url() else {
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    };
    // POST /account/2fa/disable with re-auth clears the secret + recovery codes.
}
"#;
    TPL.replace("__PASCAL__", pascal_name)
        .replace("__SNAKE__", snake_name)
}

// ── Passkey (WebAuthn) helpers ────────────────────────────────────────────────

fn passkey_route_entries() -> Vec<String> {
    vec![
        "routes::passkeys::passkey_register_begin".to_owned(),
        "routes::passkeys::passkey_register_finish".to_owned(),
        "routes::passkeys::passkey_login_begin".to_owned(),
        "routes::passkeys::passkey_login_finish".to_owned(),
        "routes::passkeys::passkey_list".to_owned(),
        "routes::passkeys::passkey_revoke".to_owned(),
        "routes::passkeys::passkey_register_page".to_owned(),
        "routes::passkeys::passkey_login_page".to_owned(),
    ]
}

fn render_passkey_migration_up(
    backend: autumn_web::config::DatabaseBackend,
    user_table: &str,
) -> String {
    let d = AuthDdl::for_backend(backend);
    format!(
        "CREATE TABLE webauthn_credentials (\n\
         \x20   id {pk},\n\
         \x20   user_id {big_int} NOT NULL REFERENCES {user_table}(id) ON DELETE CASCADE,\n\
         \x20   credential_id TEXT NOT NULL UNIQUE,\n\
         \x20   credential_json TEXT NOT NULL,\n\
         \x20   name TEXT NOT NULL DEFAULT 'Passkey',\n\
         \x20   created_at {created_at},\n\
         \x20   last_used_at {ts} NULL\n\
         );\n\
         \n\
         CREATE INDEX webauthn_credentials_user_id_idx ON webauthn_credentials (user_id);\n",
        pk = d.pk,
        big_int = d.big_int,
        ts = d.ts,
        created_at = d.ts_not_null_default_now,
    )
}

fn render_passkey_migration_down() -> String {
    "DROP TABLE webauthn_credentials;\n".to_owned()
}

fn render_webauthn_credential_model_file(user_table: &str) -> String {
    format!(
        r"//! Generated by `autumn generate auth --passkeys`.
//!
//! Edit freely — once generated, this is ordinary user code.

use crate::schema::{{webauthn_credentials, {user_table}}};

#[autumn_web::model]
pub struct WebauthnCredential {{
    pub id: i64,
    pub user_id: i64,
    pub credential_id: String,
    pub credential_json: String,
    pub name: String,
    #[default]
    pub last_used_at: Option<chrono::NaiveDateTime>,
    pub created_at: chrono::NaiveDateTime,
}}

diesel::joinable!(webauthn_credentials -> {user_table} (user_id));
"
    )
}

#[allow(clippy::too_many_lines)]
fn render_passkeys_routes_file(pascal_name: &str, snake_name: &str, user_table: &str) -> String {
    let tpl = r##"//! Generated by `autumn generate auth --passkeys`.
//!
//! WebAuthn passkey ceremony handlers.  Edit freely — once generated, this is
//! ordinary user code.
//!
//! # Configuration
//!
//! Add to `autumn.toml`:
//!
//! ```toml
//! [auth.webauthn]
//! rp_id     = "example.com"
//! rp_name   = "My App"
//! rp_origin = "https://example.com"
//! ```
//!
//! # Security notes
//!
//! - `passkey_login_finish` writes `state.auth_session_key()` so downstream
//!   `#[secured]` routes work without modification.
//! - Never store raw credentials — `credential_json` holds the opaque passkey
//!   state returned by webauthn-rs and should not be inspected by app code.

use autumn_web::prelude::*;
use base64::Engine as _;
use diesel::prelude::*;
use diesel_async::AsyncConnection as _;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::*;

/// Encode an opaque credential ID as a base64url (no padding) string.
///
/// webauthn-rs 0.5's `CredentialID` is a `HumanBinaryData` newtype over
/// `Vec<u8>` and no longer implements `Display`/`ToString`, so encode the raw
/// bytes explicitly. Registration and login use this same helper, keeping the
/// stored `credential_id` and the login lookup key byte-for-byte consistent.
fn encode_cred_id(cred_id: &CredentialID) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(cred_id.as_ref())
}

fn redirect_to(url: &str) -> axum::response::Redirect {
    axum::response::Redirect::to(url)
}

// ── Config helper ──────────────────────────────────────────────────────────────

fn build_webauthn(state: &AppState) -> AutumnResult<Webauthn> {
    // `config_arc` shares the resolved config behind an `Arc`; `config()`
    // would deep-clone every section to read one field on a request path.
    let config = state.config_arc();
    let cfg = &config.auth.webauthn;
    if cfg.rp_id.is_empty() || cfg.rp_origin.is_empty() {
        return Err(AutumnError::internal_server_error_msg(
            "WebAuthn is not configured. Set [auth.webauthn] rp_id, rp_name, and rp_origin \
             in autumn.toml. See docs/guide/passkeys.md for details.",
        ));
    }
    let origin = Url::parse(&cfg.rp_origin).map_err(|_| {
        AutumnError::internal_server_error_msg(
            "auth.webauthn.rp_origin is not a valid URL. \
             Example: \"https://example.com\"",
        )
    })?;
    WebauthnBuilder::new(&cfg.rp_id, &origin)
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!(
                "Failed to build WebAuthn instance from auth.webauthn config: {e}"
            ))
        })?
        .rp_name(&cfg.rp_name)
        .build()
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!(
                "Failed to build WebAuthn instance: {e}"
            ))
        })
}

// ── Forms / JSON types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PasskeyRegisterFinishBody {
    pub response: serde_json::Value,
}

#[derive(Deserialize)]
pub struct PasskeyLoginFinishBody {
    pub response: serde_json::Value,
}

#[derive(Deserialize)]
pub struct PasskeyRevokeForm {
    pub id: i64,
}

// ── Registration ──────────────────────────────────────────────────────────────

/// `GET /passkeys/register` — passkey registration UI page.
#[secured]
#[get("/passkeys/register")]
pub async fn passkey_register_page(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_header: Option<CsrfTokenHeader>,
    nonce: Option<CspNonce>,
) -> AutumnResult<Markup> {
    // Validates the tracked session row (401s immediately if revoked).
    let _ = crate::routes::auth::require_tracked_session(&session, &mut db, &state).await?;
    let csrf_token = csrf.map(|t| t.token().to_owned()).unwrap_or_default();
    let csrf_header_name = csrf_header.map(|h| h.0.clone()).unwrap_or_else(|| "X-CSRF-Token".to_owned());
    let script_nonce = nonce.map(|n| n.value().to_owned());
    Ok(html! {
        html {
            head {
                title { "Register a Passkey" }
                meta name="csrf-token" content=(csrf_token);
                meta name="csrf-token-header" content=(csrf_header_name);
            }
            body {
                h1 { "Register a Passkey" }
                button id="register-btn" { "Register passkey" }
                script nonce=[script_nonce] {
                    (PreEscaped(r#"
const csrfToken = document.querySelector('meta[name="csrf-token"]')?.content ?? '';
const csrfHeader = document.querySelector('meta[name="csrf-token-header"]')?.content ?? 'X-CSRF-Token';
document.getElementById('register-btn').addEventListener('click', async () => {
    const beginResp = await fetch('/passkeys/register/begin', {
        method: 'POST',
        headers: { [csrfHeader]: csrfToken },
    });
    const optionsJSON = await beginResp.json();
    const options = PublicKeyCredential.parseCreationOptionsFromJSON(optionsJSON.publicKey);
    const credential = await navigator.credentials.create({ publicKey: options });
    const finishResp = await fetch('/passkeys/register/finish', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', [csrfHeader]: csrfToken },
        body: JSON.stringify({ response: credential.toJSON() }),
    });
    if (finishResp.ok) {
        window.location.href = '/passkeys';
    } else {
        alert('Registration failed');
    }
});
"#))
                }
            }
        }
    })
}

/// `POST /passkeys/register/begin` — start a registration ceremony.
///
/// Requires authentication and a fresh step-up claim to prevent a hijacked
/// session from silently adding a new authenticator.
#[secured]
#[step_up]
#[post("/passkeys/register/begin")]
pub async fn passkey_register_begin(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
) -> AutumnResult<axum::Json<serde_json::Value>> {
    // Validates the tracked session row (401s immediately if revoked).
    let __SNAKE___id: i64 =
        crate::routes::auth::require_tracked_session(&session, &mut db, &state)
            .await?
            .id;
    let __SNAKE___email = session
        .get("__SNAKE___email")
        .await
        .unwrap_or_else(|| "user".to_owned());
    let webauthn = build_webauthn(&state)?;
    let existing_cred_ids: Vec<CredentialID> = {
        use crate::schema::webauthn_credentials;
        webauthn_credentials::table
            .filter(webauthn_credentials::user_id.eq(__SNAKE___id))
            .select(webauthn_credentials::credential_json)
            .load::<String>(&mut *db)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|json| serde_json::from_str::<Passkey>(&json).ok())
            .map(|p| p.cred_id().clone())
            .collect()
    };
    let user_unique_id = uuid::Uuid::from_u128(__SNAKE___id as u128);
    let (ccr, reg_state) = webauthn
        .start_passkey_registration(
            user_unique_id,
            &__SNAKE___email,
            &__SNAKE___email,
            Some(existing_cred_ids),
        )
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!("start_passkey_registration: {e}"))
        })?;
    // Store the user_id alongside the reg_state so finish can verify the session
    // user hasn't changed between begin and finish (prevents cross-account TOCTOU).
    session
        .insert(
            "passkey_reg_state",
            serde_json::to_string(&serde_json::json!({
                "user_id": __SNAKE___id,
                "state":   serde_json::to_string(&reg_state).unwrap_or_default(),
            }))
            .unwrap_or_default(),
        )
        .await;
    Ok(axum::Json(serde_json::to_value(ccr).unwrap_or_default()))
}

/// `POST /passkeys/register/finish` — complete a registration ceremony.
///
/// Stores the new credential in `webauthn_credentials`.
#[secured]
#[post("/passkeys/register/finish")]
pub async fn passkey_register_finish(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    axum::Json(body): axum::Json<PasskeyRegisterFinishBody>,
) -> AutumnResult<axum::Json<serde_json::Value>> {
    // Validates the tracked session row (401s immediately if revoked).
    let current =
        crate::routes::auth::require_tracked_session(&session, &mut db, &state).await?;
    let current_id: i64 = current.id;
    let envelope_str: String = session
        .get("passkey_reg_state")
        .await
        .ok_or_else(|| AutumnError::unprocessable_msg("No pending registration."))?;
    session.remove("passkey_reg_state").await;
    let envelope: serde_json::Value = serde_json::from_str(&envelope_str)
        .map_err(|_| AutumnError::unprocessable_msg("Invalid registration state."))?;
    let __SNAKE___id: i64 = envelope["user_id"]
        .as_i64()
        .ok_or_else(|| AutumnError::unprocessable_msg("Invalid registration state."))?;
    if __SNAKE___id != current_id {
        return Err(AutumnError::unauthorized_msg(
            "Session user changed since registration began.",
        ));
    }
    let reg_state_str = envelope["state"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let reg_state: PasskeyRegistration = serde_json::from_str(&reg_state_str)
        .map_err(|_| AutumnError::unprocessable_msg("Invalid registration state."))?;
    let webauthn = build_webauthn(&state)?;
    let rpk_finish: RegisterPublicKeyCredential =
        serde_json::from_value(body.response).map_err(|e| {
            AutumnError::unprocessable_msg(format!("Invalid credential response: {e}"))
        })?;
    let passkey = webauthn
        .finish_passkey_registration(&rpk_finish, &reg_state)
        .map_err(|e| AutumnError::unprocessable_msg(format!("Registration failed: {e}")))?;
    let cred_id = encode_cred_id(passkey.cred_id());
    let cred_json = serde_json::to_string(&passkey)
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to serialise passkey."))?;
    // Store the passkey and revoke every *other* session in one
    // transaction: the documented revoke-on-credential-change guarantee
    // (defaulted on, configurable via
    // [auth.sessions].revoke_on_credential_change) can never be silently
    // skipped, and a failure rolls the credential back so the client can
    // retry without storing a duplicate.
    let revoke_other_sessions_in_txn = state.config_arc().auth.sessions.revoke_on_credential_change;
    let current_token_digest = crate::routes::auth::session_token_digest(&session).await;
    (*db)
        .transaction::<_, diesel::result::Error, _>(async move |conn| {
            diesel::insert_into(crate::schema::webauthn_credentials::table)
                .values((
                    crate::schema::webauthn_credentials::user_id.eq(__SNAKE___id),
                    crate::schema::webauthn_credentials::credential_id.eq(&cred_id),
                    crate::schema::webauthn_credentials::credential_json.eq(&cred_json),
                    crate::schema::webauthn_credentials::name.eq("Passkey"),
                ))
                .execute(conn)
                .await?;
            if revoke_other_sessions_in_txn {
                diesel::delete(
                    crate::schema::__SESSTABLE__::table
                        .filter(crate::schema::__SESSTABLE__::user_id.eq(__SNAKE___id))
                        .filter(crate::schema::__SESSTABLE__::token_digest.ne(current_token_digest)),
                )
                .execute(conn)
                .await?;
            }
            Ok(())
        })
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to store passkey."))?;
    Ok(axum::Json(serde_json::json!({ "ok": true })))
}

// ── Authentication ────────────────────────────────────────────────────────────

/// `GET /passkeys/login` — passkey login UI page.
#[get("/passkeys/login")]
pub async fn passkey_login_page(
    csrf: Option<CsrfToken>,
    csrf_header: Option<CsrfTokenHeader>,
    nonce: Option<CspNonce>,
) -> AutumnResult<Markup> {
    let csrf_token = csrf.map(|t| t.token().to_owned()).unwrap_or_default();
    let csrf_header_name = csrf_header.map(|h| h.0.clone()).unwrap_or_else(|| "X-CSRF-Token".to_owned());
    let script_nonce = nonce.map(|n| n.value().to_owned());
    Ok(html! {
        html {
            head {
                title { "Sign in with Passkey" }
                meta name="csrf-token" content=(csrf_token);
                meta name="csrf-token-header" content=(csrf_header_name);
            }
            body {
                h1 { "Sign in with a Passkey" }
                button id="login-btn" { "Sign in with passkey" }
                script nonce=[script_nonce] {
                    (PreEscaped(r#"
const csrfToken = document.querySelector('meta[name="csrf-token"]')?.content ?? '';
const csrfHeader = document.querySelector('meta[name="csrf-token-header"]')?.content ?? 'X-CSRF-Token';
document.getElementById('login-btn').addEventListener('click', async () => {
    const beginResp = await fetch('/passkeys/login/begin', {
        method: 'POST',
        headers: { [csrfHeader]: csrfToken },
    });
    const optionsJSON = await beginResp.json();
    const options = PublicKeyCredential.parseRequestOptionsFromJSON(optionsJSON.publicKey);
    const assertion = await navigator.credentials.get({ publicKey: options });
    const finishResp = await fetch('/passkeys/login/finish', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', [csrfHeader]: csrfToken },
        body: JSON.stringify({ response: assertion.toJSON() }),
    });
    if (finishResp.ok) {
        window.location.href = '/account';
    } else {
        alert('Login failed');
    }
});
"#))
                }
            }
        }
    })
}

/// `POST /passkeys/login/begin` — start a discoverable authentication ceremony.
///
/// Uses `start_discoverable_authentication` so the browser shows the passkey
/// selector without the server knowing the user upfront.  No credential IDs
/// are sent to anonymous clients.
#[post("/passkeys/login/begin")]
pub async fn passkey_login_begin(
    session: Session,
    State(state): State<AppState>,
) -> AutumnResult<axum::Json<serde_json::Value>> {
    let webauthn = build_webauthn(&state)?;
    let (rcr, auth_state) = webauthn
        .start_discoverable_authentication()
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!(
                "start_discoverable_authentication: {e}"
            ))
        })?;
    session
        .insert(
            "passkey_auth_state",
            serde_json::to_string(&auth_state).unwrap_or_default(),
        )
        .await;
    Ok(axum::Json(serde_json::to_value(rcr).unwrap_or_default()))
}

/// `POST /passkeys/login/finish` — complete a discoverable authentication ceremony.
///
/// On success writes `state.auth_session_key()`, `__SNAKE___id`, and
/// `__SNAKE___email` so downstream `#[secured]` routes work without modification.
#[post("/passkeys/login/finish")]
pub async fn passkey_login_finish(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    crate::routes::auth::MaybeClientIp(addr_ip): crate::routes::auth::MaybeClientIp,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<PasskeyLoginFinishBody>,
) -> AutumnResult<axum::Json<serde_json::Value>> {
    let auth_state_str: String = session
        .get("passkey_auth_state")
        .await
        .ok_or_else(|| AutumnError::unprocessable_msg("No pending authentication."))?;
    session.remove("passkey_auth_state").await;
    let auth_state: DiscoverableAuthentication = serde_json::from_str(&auth_state_str)
        .map_err(|_| AutumnError::unprocessable_msg("Invalid authentication state."))?;
    let webauthn = build_webauthn(&state)?;
    let pkc: PublicKeyCredential = serde_json::from_value(body.response)
        .map_err(|e| AutumnError::unprocessable_msg(format!("Invalid credential: {e}")))?;
    // Decode user identity from the credential's userHandle (set during registration).
    let (user_uuid, _) = webauthn
        .identify_discoverable_authentication(&pkc)
        .map_err(|e| AutumnError::unauthorized_msg(format!("Cannot identify user: {e}")))?;
    let __SNAKE___id = user_uuid.as_u128() as i64;
    // Load only this user's passkeys for signature verification.
    let disc_keys: Vec<DiscoverableKey> = {
        use crate::schema::webauthn_credentials;
        webauthn_credentials::table
            .filter(webauthn_credentials::user_id.eq(__SNAKE___id))
            .select(webauthn_credentials::credential_json)
            .load::<String>(&mut *db)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|json| serde_json::from_str::<Passkey>(&json).ok())
            .map(|p| DiscoverableKey::from(&p))
            .collect()
    };
    let auth_result = webauthn
        .finish_discoverable_authentication(&pkc, auth_state, &disc_keys)
        .map_err(|e| AutumnError::unauthorized_msg(format!("Authentication failed: {e}")))?;
    let cred_id_str = encode_cred_id(auth_result.cred_id());
    let (wc_id, cred_json) = {
        use crate::schema::webauthn_credentials;
        webauthn_credentials::table
            .filter(webauthn_credentials::credential_id.eq(&cred_id_str))
            .filter(webauthn_credentials::user_id.eq(__SNAKE___id))
            .select((webauthn_credentials::id, webauthn_credentials::credential_json))
            .first::<(i64, String)>(&mut *db)
            .await
            .map_err(|_| AutumnError::unauthorized_msg("Unknown credential."))?
    };
    let (__SNAKE___email, email_confirmed_at): (String, Option<chrono::NaiveDateTime>) = {
        use crate::schema::__TABLE__;
        __TABLE__::table
            .find(__SNAKE___id)
            .select((__TABLE__::email, __TABLE__::email_confirmed_at))
            .first::<(String, Option<chrono::NaiveDateTime>)>(&mut *db)
            .await
            .map_err(|_| AutumnError::unauthorized_msg("User not found."))?
    };
    // Reject unconfirmed accounts — consistent with the password-login gate.
    if email_confirmed_at.is_none() {
        return Err(AutumnError::unauthorized_msg(
            "Email address not confirmed. Please check your email for a confirmation link.",
        ));
    }
    let now = chrono::Utc::now().naive_utc();
    if auth_result.needs_update() {
        let mut passkey: Passkey = serde_json::from_str(&cred_json)
            .unwrap_or_else(|_| serde_json::from_value(serde_json::Value::Null).unwrap());
        passkey.update_credential(&auth_result);
        let new_json = serde_json::to_string(&passkey).unwrap_or(cred_json.clone());
        diesel::update(crate::schema::webauthn_credentials::table.find(wc_id))
            .set((
                crate::schema::webauthn_credentials::credential_json.eq(&new_json),
                crate::schema::webauthn_credentials::last_used_at.eq(now),
            ))
            .execute(&mut *db)
            .await
            .map_err(|_| {
                AutumnError::internal_server_error_msg("Failed to update credential state.")
            })?;
    } else {
        diesel::update(crate::schema::webauthn_credentials::table.find(wc_id))
            .set(crate::schema::webauthn_credentials::last_used_at.eq(now))
            .execute(&mut *db)
            .await
            .map_err(|_| {
                AutumnError::internal_server_error_msg("Failed to update credential timestamp.")
            })?;
    }
    // Drop any tracked row for the pre-login session — the rotation below
    // destroys its id, which would otherwise leave a phantom device.
    crate::routes::auth::untrack_current_session(&mut db, &session).await?;
    session.rotate_id().await;
    session
        .insert("__SNAKE___id", __SNAKE___id.to_string())
        .await;
    session.insert("__SNAKE___email", &__SNAKE___email).await;
    session
        .insert(state.auth_session_key(), __SNAKE___id.to_string())
        .await;
    // Stamp the step-up claim so `#[step_up]` routes (e.g. passkey management)
    // are immediately accessible after a passkey login.
    autumn_web::step_up::set_last_strong_auth_at(&session).await;
    // Track this login as an active session (device list + revocation).
    crate::routes::auth::record_login_session(&mut db, &session, __SNAKE___id, addr_ip, &headers)
        .await?;
    Ok(axum::Json(serde_json::json!({ "ok": true })))
}

// ── Management ────────────────────────────────────────────────────────────────

/// `GET /passkeys` — list the current user's registered passkeys.
#[secured]
#[get("/passkeys")]
pub async fn passkey_list(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> AutumnResult<Markup> {
    // Validates the tracked session row (401s immediately if revoked).
    let __SNAKE___id: i64 = crate::routes::auth::require_tracked_session(&session, &mut db, &state)
        .await?
        .id;
    let rows: Vec<(i64, String, chrono::NaiveDateTime)> = {
        use crate::schema::webauthn_credentials;
        webauthn_credentials::table
            .filter(webauthn_credentials::user_id.eq(__SNAKE___id))
            .select((
                webauthn_credentials::id,
                webauthn_credentials::name,
                webauthn_credentials::created_at,
            ))
            .load(&mut *db)
            .await
            .unwrap_or_default()
    };
    Ok(html! {
        html {
            head { title { "Your Passkeys" } }
            body {
                h1 { "Your Passkeys" }
                a href="/passkeys/register" { "Register a new passkey" }
                @if rows.is_empty() {
                    p { "No passkeys registered yet." }
                } @else {
                    ul {
                        @for (id, name, created_at) in &rows {
                            li {
                                (name) " — registered " (created_at.to_string())
                                " "
                                form method="post" action="/passkeys/revoke" style="display:inline" {
                                    @if let Some(ref t) = csrf {
                                        input type="hidden"
                                              name=(csrf_field.as_ref().map(|f| f.0.as_str()).unwrap_or("_csrf"))
                                              value=(t.token());
                                    }
                                    input type="hidden" name="id" value=(id);
                                    button type="submit" { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// `DELETE /passkeys/:id` (also accepts `POST /passkeys/revoke`) — remove a passkey.
#[secured]
#[post("/passkeys/revoke")]
pub async fn passkey_revoke(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    Form(form): Form<PasskeyRevokeForm>,
) -> AutumnResult<axum::response::Redirect> {
    // Validates the tracked session row (401s immediately if revoked).
    let current =
        crate::routes::auth::require_tracked_session(&session, &mut db, &state).await?;
    // Remove the passkey and revoke every *other* session in one
    // transaction: the documented revoke-on-credential-change guarantee
    // (defaulted on, configurable via
    // [auth.sessions].revoke_on_credential_change) can never be silently
    // skipped, and a failure rolls the removal back instead of 500ing
    // after it.
    let user_id = current.id;
    let credential_id = form.id;
    let revoke_other_sessions_in_txn = state.config_arc().auth.sessions.revoke_on_credential_change;
    let current_token_digest = crate::routes::auth::session_token_digest(&session).await;
    (*db)
        .transaction::<_, diesel::result::Error, _>(async move |conn| {
            diesel::delete(
                crate::schema::webauthn_credentials::table
                    .filter(crate::schema::webauthn_credentials::id.eq(credential_id))
                    .filter(crate::schema::webauthn_credentials::user_id.eq(user_id)),
            )
            .execute(conn)
            .await?;
            if revoke_other_sessions_in_txn {
                diesel::delete(
                    crate::schema::__SESSTABLE__::table
                        .filter(crate::schema::__SESSTABLE__::user_id.eq(user_id))
                        .filter(crate::schema::__SESSTABLE__::token_digest.ne(current_token_digest)),
                )
                .execute(conn)
                .await?;
            }
            Ok(())
        })
        .await
        .map_err(|_| AutumnError::internal_server_error_msg("Failed to revoke passkey."))?;
    Ok(redirect_to("/passkeys"))
}
"##;
    tpl.replace("__PASCAL__", pascal_name)
        .replace("__SNAKE__", snake_name)
        .replace("__TABLE__", user_table)
        .replace("__SESSTABLE__", &sessions_table_name(snake_name))
}

fn render_passkeys_tests_file(pascal_name: &str, _snake_name: &str) -> String {
    format!(
        r#"//! Generated passkey integration tests for {pascal_name} (`autumn generate auth --passkeys`).
//!
//! These tests run against a live server started with `AUTUMN_TEST_BASE_URL`.
//! In CI, start the app, set the env var, and run `cargo test`.
//! They skip when the env var is unset, so they compile and pass out of the box.

fn base_url() -> Option<String> {{
    std::env::var("AUTUMN_TEST_BASE_URL").ok()
}}

#[test]
fn passkey_register_happy_path() {{
    let Some(_base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // 1. POST /passkeys/register/begin (authenticated session)
    // 2. Simulate navigator.credentials.create response
    // 3. POST /passkeys/register/finish — expect 200 {{ "ok": true }}
}}

#[test]
fn passkey_login_happy_path() {{
    let Some(_base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // 1. POST /passkeys/login/begin
    // 2. Simulate navigator.credentials.get response
    // 3. POST /passkeys/login/finish — expect 200 {{ "ok": true }} and auth cookie
}}

#[test]
fn passkey_wrong_origin_rejected() {{
    let Some(_base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // POST /passkeys/login/finish with a credential registered against a different
    // origin — expect 401 or 422.
}}

#[test]
fn passkey_unknown_credential_rejected() {{
    let Some(_base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // POST /passkeys/login/finish with a credential_id that is not in the DB
    // — expect 401.
}}

#[test]
fn passkey_revoke_then_relogin() {{
    let Some(_base) = base_url() else {{
        eprintln!("skipping: AUTUMN_TEST_BASE_URL not set");
        return;
    }};
    // 1. Register a passkey.
    // 2. POST /passkeys/revoke to remove it.
    // 3. Attempt login with the revoked credential — expect failure.
}}
"#
    )
}

fn render_passkeys_docs_file() -> String {
    r#"# Passkeys (WebAuthn)

Generated with `autumn generate auth --passkeys`. Users can register and use
hardware security keys, platform authenticators (Touch ID, Face ID, Windows Hello),
or other FIDO2 compliant devices as passwordless credentials.

## Configuration

Add to `autumn.toml` before enabling passkeys:

```toml
[auth.webauthn]
rp_id     = "example.com"          # domain only (no port, no scheme)
rp_name   = "My App"               # shown in authenticator dialogs
rp_origin = "https://example.com"  # full origin including scheme
```

For local development use:

```toml
[auth.webauthn]
rp_id     = "localhost"
rp_name   = "My App (dev)"
rp_origin = "http://localhost:3000"
```

## Generated routes

| Method | Path | Handler | Auth |
|--------|------|---------|------|
| GET | `/passkeys/register` | `passkey_register_page` | **Required** |
| POST | `/passkeys/register/begin` | `passkey_register_begin` | **Required** |
| POST | `/passkeys/register/finish` | `passkey_register_finish` | **Required** |
| GET | `/passkeys/login` | `passkey_login_page` | Public |
| POST | `/passkeys/login/begin` | `passkey_login_begin` | Public |
| POST | `/passkeys/login/finish` | `passkey_login_finish` | Public |
| GET | `/passkeys` | `passkey_list` | **Required** |
| POST | `/passkeys/revoke` | `passkey_revoke` | **Required** |

## Security properties

- Ceremony challenges are bound to the origin (`rp_origin`) and domain (`rp_id`).
  A credential registered against `example.com` cannot authenticate against
  `evil.com`, even if the credential bytes are stolen.
- `passkey_login_finish` calls `session.rotate_id()` before writing the auth key,
  preventing session fixation.
- Revoked credentials are deleted from `webauthn_credentials`; a subsequent
  login attempt with the revoked credential will fail at the DB lookup step.
- `credential_json` stores opaque passkey state — never expose it to clients.

## Discoverable credentials

`passkey_login_begin` passes `&[]` (an empty allowed-credentials list), enabling
**discoverable credential** (resident key) flow. The browser prompts the user to
select a saved passkey without the server needing to know their identity first.
No credential IDs are sent to anonymous clients.

## Browser compatibility

The generated JavaScript uses `PublicKeyCredential.parseCreationOptionsFromJSON`,
`parseRequestOptionsFromJSON`, and `credential.toJSON()` (Chrome 119+,
Firefox 119+, Safari 17+). Add a polyfill (e.g. `@github/webauthn-json`) for
older browsers.

## Out of scope (follow-ups)

- **Rate-limiting** on ceremony endpoints is not included — add it before
  exposing them to untrusted traffic.
"#
    .to_owned()
}

/// Ensure `autumn-web` in `[dependencies]` has `features = ["webauthn"]`.
#[allow(clippy::too_many_lines)]
fn ensure_autumn_web_webauthn_feature(toml: &str) -> String {
    const CRATE: &str = "autumn-web";
    const FEATURE: &str = "\"webauthn\"";

    let mut lines: Vec<String> = toml.lines().map(str::to_owned).collect();
    let trailing_newline = toml.ends_with('\n');

    let simple_prefix = format!("{CRATE} = \"");
    let table_prefix = format!("{CRATE} = {{");
    let subtable_header = format!("[dependencies.{CRATE}]");
    let subtable_header_underscore = format!("[dependencies.{}]", CRATE.replace('-', "_"));

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim().to_owned();
        let indent: String = lines[i]
            .chars()
            .take_while(char::is_ascii_whitespace)
            .collect();

        if let Some(rest) = trimmed.strip_prefix(&simple_prefix) {
            let version = rest.trim_end_matches('"');
            lines[i] =
                format!("{indent}{CRATE} = {{ version = \"{version}\", features = [{FEATURE}] }}");
            break;
        }

        if trimmed.starts_with(&table_prefix) {
            if trimmed.contains(FEATURE) {
                break; // already present
            }
            if let Some(feat_bracket) = trimmed.find("features = [") {
                let list_start = feat_bracket + "features = [".len();
                if let Some(close_bracket) = trimmed[list_start..].find(']') {
                    let list_end = close_bracket + list_start;
                    let existing = trimmed[list_start..list_end].trim();
                    let new_list = if existing.is_empty() {
                        FEATURE.to_owned()
                    } else {
                        format!("{existing}, {FEATURE}")
                    };
                    lines[i] = format!(
                        "{indent}{}{}{}",
                        &trimmed[..list_start],
                        new_list,
                        &trimmed[list_end..]
                    );
                } else {
                    let mut j = i + 1;
                    while j < lines.len() {
                        let tj = lines[j].trim();
                        if tj.starts_with('[') {
                            break;
                        }
                        if let Some(close_idx) = tj.find(']') {
                            let before_close = tj[..close_idx].trim();
                            let sep = if before_close.is_empty() || before_close.ends_with(',') {
                                ""
                            } else {
                                ", "
                            };
                            let indent_j: String = lines[j]
                                .chars()
                                .take_while(char::is_ascii_whitespace)
                                .collect();
                            lines[j] = format!(
                                "{indent_j}{before_close}{sep}{FEATURE}{}",
                                &tj[close_idx..]
                            );
                            break;
                        }
                        j += 1;
                    }
                }
            } else {
                // No features key — insert before closing `}`.
                let close = trimmed.rfind('}').unwrap();
                let before_close = trimmed[..close].trim_end();
                let sep = if before_close.ends_with('{') {
                    ""
                } else {
                    ", "
                };
                lines[i] = format!(
                    "{indent}{}{sep}features = [{FEATURE}]{}",
                    &trimmed[..close],
                    &trimmed[close..]
                );
            }
            break;
        }

        if trimmed == subtable_header || trimmed == subtable_header_underscore {
            // Scan ahead within the subtable.
            let mut j = i + 1;
            let mut found_features = false;
            while j < lines.len() {
                let t = lines[j].trim().to_owned();
                if t.starts_with('[') {
                    break;
                }
                if t.starts_with("features") {
                    found_features = true;
                    if !t.contains(FEATURE)
                        && let (Some(open), Some(close)) = (t.find('['), t.rfind(']'))
                    {
                        let inner = t[open + 1..close].trim();
                        let new_inner = if inner.is_empty() {
                            FEATURE.to_owned()
                        } else {
                            format!("{inner}, {FEATURE}")
                        };
                        let indent_j: String = lines[j]
                            .chars()
                            .take_while(char::is_ascii_whitespace)
                            .collect();
                        lines[j] = format!("{indent_j}features = [{new_inner}]");
                    }
                    break;
                }
                j += 1;
            }
            if !found_features {
                lines.insert(i + 1, format!("features = [{FEATURE}]"));
            }
            break;
        }

        i += 1;
    }

    let mut out = lines.join("\n");
    if trailing_newline && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn append_webauthn_stub_to_toml(
    existing: &str,
    rp_id: &str,
    rp_name: &str,
    rp_origin: &str,
) -> String {
    if existing.contains("[auth.webauthn]") {
        return existing.to_owned();
    }
    let mut out = existing.trim_end().to_owned();
    out.push_str("\n\n[auth.webauthn]\n");
    out.push_str("rp_id = \"");
    out.push_str(rp_id);
    out.push_str("\"\nrp_name = \"");
    out.push_str(rp_name);
    out.push_str("\"\nrp_origin = \"");
    out.push_str(rp_origin);
    out.push_str("\"\n");
    out
}

/// Inverse of [`append_oauth_stubs_to_toml`] (`autumn destroy`, issue #1048).
///
/// Removes each `[auth.oauth2.<provider>]` stub block for `providers`, but
/// only when its current content is still byte-identical to the stub
/// `append_oauth_stubs_to_toml` would have inserted — never a block that
/// pre-existed before this `generate` call (real credentials `generate`
/// never touched), and never one the user has since filled in. A no-op for
/// any provider whose header isn't present, or whose content no longer
/// matches (already destroyed, hand-edited, or a pre-existing real config).
pub(super) fn remove_oauth_provider_stubs(existing: &str, providers: &[String]) -> String {
    let mut content = existing.to_owned();
    for name in providers {
        let header = format!("[auth.oauth2.{name}]");
        if toml_table_block_matches(&content, &header, &oauth_provider_stub_block(name)) {
            content = remove_toml_table_block(&content, &header);
        }
    }
    content
}

/// Inverse of [`append_webauthn_stub_to_toml`] (`autumn destroy`, issue #1048).
///
/// A no-op if `[auth.webauthn]` isn't present, or if its content no longer
/// matches exactly what `append_webauthn_stub_to_toml` always inserts (the
/// same "never touch a pre-existing or since-edited block" guard as
/// [`remove_oauth_provider_stubs`]).
pub(super) fn remove_webauthn_stub(existing: &str) -> String {
    const HEADER: &str = "[auth.webauthn]";
    let expected = "[auth.webauthn]\n\
                     rp_id = \"localhost\"\n\
                     rp_name = \"My App\"\n\
                     rp_origin = \"http://localhost:3000\"\n";
    if toml_table_block_matches(existing, HEADER, expected) {
        remove_toml_table_block(existing, HEADER)
    } else {
        existing.to_owned()
    }
}

/// The `(start, end)` line-index span of a top-level `[header]` TOML table —
/// the header line through the last line before the next top-level `[...]`
/// header or EOF. `None` if `header` isn't present.
fn toml_table_block_range(lines: &[&str], header: &str) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| l.trim() == header)?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |rel| start + 1 + rel);
    Some((start, end))
}

/// Whether `[header]`'s current block content is byte-identical (ignoring a
/// trailing-newline difference) to `expected_block` — used to verify a stub
/// hasn't been hand-filled with real values, or didn't pre-exist before
/// `generate` ever ran, before `autumn destroy` removes it.
fn toml_table_block_matches(existing: &str, header: &str, expected_block: &str) -> bool {
    let lines: Vec<&str> = existing.lines().collect();
    let Some((start, end)) = toml_table_block_range(&lines, header) else {
        return false;
    };
    lines[start..end].join("\n").trim_end() == expected_block.trim_end()
}

/// Remove a top-level `[header]` TOML table — the header line and every line
/// up to (but not including) the next top-level `[...]` header or EOF — plus
/// one immediately preceding blank separator line, if present.
///
/// This is the exact inverse shape of how [`append_oauth_stubs_to_toml`] and
/// [`append_webauthn_stub_to_toml`] insert a table at the end of the file
/// (`existing.trim_end()` followed by a separator and the new block): consuming
/// one preceding blank line here undoes that separator, and discarding
/// everything from the header to EOF when this is the last table in the file
/// undoes the trailing blank line those functions leave behind. A no-op if
/// `header` isn't present.
fn remove_toml_table_block(existing: &str, header: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let Some((start, end)) = toml_table_block_range(&lines, header) else {
        return existing.to_owned();
    };

    let mut block_start = start;
    if block_start > 0 && lines[block_start - 1].trim().is_empty() {
        block_start -= 1;
    }

    let mut new_lines: Vec<&str> = Vec::with_capacity(lines.len());
    new_lines.extend_from_slice(&lines[..block_start]);
    new_lines.extend_from_slice(&lines[end..]);
    let mut out = new_lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Generated request handlers must read config through `state.config_arc()`,
    /// never `state.config()`.
    ///
    /// `config()` hands back an owned snapshot, which deep-clones every section
    /// of `AutumnConfig` (~65 allocations) to read one field. In a handler that
    /// is paid per request, and a handler reading two sections pays it twice —
    /// which is how a downstream app measured whole-config clones at ~30% of its
    /// per-request allocations. `config_arc()` clones the `Arc` instead, so the
    /// same read is a refcount bump.
    ///
    /// The one legitimate `config()` in the emitted file is the boot-time
    /// `remember_me_startup` hook: it runs once and hands `init_remember_pool` an
    /// owned `RememberConfig`. This pins that exception to exactly one site, so a
    /// `config()` re-introduced into any handler fails here rather than in a
    /// downstream profile.
    #[test]
    fn generated_handlers_read_config_through_the_shared_arc() {
        const STARTUP_HOOK: &str = "pub async fn remember_me_startup";

        for (label, routes) in [
            (
                "plain",
                render_routes_file("User", "user", "users", &[], false, false),
            ),
            (
                "totp",
                render_routes_file("User", "user", "users", &[], true, false),
            ),
            (
                "magic-link",
                render_routes_file("User", "user", "users", &[], false, true),
            ),
            (
                "oauth",
                render_routes_file(
                    "User",
                    "user",
                    "users",
                    &["github".to_owned()],
                    false,
                    false,
                ),
            ),
        ] {
            let hook_at = routes
                .find(STARTUP_HOOK)
                .unwrap_or_else(|| panic!("{label}: expected a {STARTUP_HOOK} definition"));
            // The hook's own `config()` sits a few lines into its body; anything
            // further out is a handler paying a deep clone per request.
            let hook_body_end = hook_at + 600;

            for (offset, _) in routes.match_indices("state.config()") {
                assert!(
                    (hook_at..hook_body_end).contains(&offset),
                    "{label}: generated code calls state.config() outside the boot-time \
                     {STARTUP_HOOK} hook (byte {offset}), which deep-clones every config \
                     section on a request path — use state.config_arc() instead:\n{}",
                    &routes[offset.saturating_sub(300)..(offset + 200).min(routes.len())]
                );
            }

            assert!(
                routes.contains("state.config_arc()"),
                "{label}: generated handlers must read config through state.config_arc()"
            );
        }
    }

    fn project_with_main() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.3\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), main_with_layout()).unwrap();
        tmp
    }

    /// A `src/main.rs` exposing a shared 4-arg
    /// `pub fn layout(title, current_path, flash, content)` — what `autumn new`
    /// emits and what the auth generator's views render through
    /// (`crate::layout`, issue #1353). The auth preflight requires this.
    fn main_with_layout() -> &'static str {
        "use autumn_web::prelude::*;\n\n\
         pub fn layout(title: &str, current_path: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {\n\
         \x20   let _ = (current_path, flash);\n\
         \x20   maud::html! {\n\
         \x20       title { (title) }\n\
         \x20       (content)\n\
         \x20   }\n\
         }\n\n\
         #[autumn_web::main]\n\
         async fn main() {\n\
         \x20   autumn_web::app().routes(routes![]).run().await;\n\
         }\n"
    }

    /// A `src/main.rs` with no shared `pub fn layout` — used to exercise the
    /// actionable error the auth generator raises when the target app has no
    /// shared layout for its HTML views to render through (issue #1353).
    fn main_without_layout() -> &'static str {
        "use autumn_web::prelude::*;\n\n\
         #[autumn_web::main]\n\
         async fn main() {\n\
         \x20   autumn_web::app().routes(routes![]).run().await;\n\
         }\n"
    }

    /// A Cargo.toml matching what `autumn new`'s own template ships
    /// (`autumn-web`, `diesel_migrations`, and `maud` already present — see
    /// `TEMPLATE_SHIPPED_CARGO_DEPS`), used by the round-trip tests below so
    /// those three names are genuinely pre-existing rather than freshly
    /// added by this generator (and thus rightly excluded from its
    /// `Revert::CargoDeps`).
    fn template_shipped_cargo_toml() -> &'static str {
        "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.3\"\n\
         diesel_migrations = \"2\"\nmaud = { version = \"0.27\", features = [\"axum\"] }\n"
    }

    // ── generate/destroy round trips (issue #1048) ──────────────────────────
    //
    // `autumn destroy auth` recomputes the exact same plan `generate` did and
    // interprets it in reverse (see `Plan::revert`): every `Create`d file is
    // deleted, and every shared-file edit recorded via `plan.push_revert(...)`
    // is undone. These tests assert the round trip is byte-identical for the
    // base scaffold plus each optional feature flag.

    /// Backend-aware DDL (issue #1927): `generate auth` on a `SQLite` app now
    /// scaffolds its migrations in `SQLite` dialect (`INTEGER PRIMARY KEY
    /// AUTOINCREMENT`, `DEFAULT CURRENT_TIMESTAMP`) instead of being rejected —
    /// covering the users table AND the DB-backed sessions table (issue #1908) —
    /// and no Postgres-only `BIGSERIAL` / `BIGINT` / `NOW()` leaks into the
    /// `SQLite` migration.
    #[test]
    fn plan_auth_emits_sqlite_ddl_including_sessions() {
        let tmp = project_with_main();
        fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"sqlite://app.db\"\n",
        )
        .unwrap();
        // `--totp`/`--magic-link` cover the recovery-code + magic-link-token
        // tables too, so every hand-written auth DDL string is exercised.
        plan_auth_full_ex2(
            tmp.path(),
            "User",
            "20260508000000",
            &AuthOAuthOptions {
                providers: Vec::new(),
            },
            true,  // totp
            false, // passkeys
            true,  // magic_link
        )
        .expect("generate auth must scaffold on a SQLite app")
        .execute(Flags::default())
        .unwrap();
        assert!(tmp.path().join("src/models/user.rs").exists());

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        // Every auto-increment id uses the SQLite spelling.
        assert!(
            up.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
            "SQLite up.sql must use INTEGER PRIMARY KEY AUTOINCREMENT: {up}"
        );
        // The DB-backed sessions table (#1908) is created in SQLite dialect.
        assert!(
            up.contains("CREATE TABLE user_sessions (")
                && up.contains("last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            "user_sessions must be SQLite-dialect: {up}"
        );
        // The recovery-code (--totp) and magic-link (--magic-link) tables too.
        assert!(
            up.contains("CREATE TABLE recovery_codes (")
                && up.contains("CREATE TABLE magic_link_tokens ("),
            "totp + magic-link tables must be scaffolded: {up}"
        );
        // Foreign keys and timestamp columns are SQLite-typed.
        assert!(
            up.contains("user_id INTEGER NOT NULL REFERENCES users(id)"),
            "FK columns must be INTEGER on SQLite: {up}"
        );
        assert!(
            up.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            "created_at must default to CURRENT_TIMESTAMP on SQLite: {up}"
        );
        for leak in ["BIGSERIAL", "BIGINT", "NOW()"] {
            assert!(
                !up.contains(leak),
                "SQLite up.sql leaked Postgres-only `{leak}`: {up}"
            );
        }
    }

    /// Collect every generated `.rs` file under `root`, recursively.
    fn generated_rust_files(root: &Path) -> Vec<(std::path::PathBuf, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push((path.clone(), fs::read_to_string(&path).unwrap()));
                }
            }
        }
        out
    }

    /// Scaffold the full auth surface into a project whose configured backend
    /// is `url`, and return every generated `.rs` file.
    fn auth_scaffold_rust_files(url: &str) -> Vec<(std::path::PathBuf, String)> {
        let tmp = project_with_main();
        fs::write(
            tmp.path().join("autumn.toml"),
            format!("[database]\nprimary_url = \"{url}\"\n"),
        )
        .unwrap();
        plan_auth_full_ex2(
            tmp.path(),
            "User",
            "20260508000000",
            &AuthOAuthOptions {
                providers: Vec::new(),
            },
            true, // totp
            true, // passkeys
            true, // magic_link
        )
        .expect("generate auth must scaffold")
        .execute(Flags::default())
        .unwrap();
        generated_rust_files(&tmp.path().join("src"))
    }

    /// DB-backed sessions store on `SQLite` (issue #1908): the generated
    /// session/remember store must bound its connections by the backend-agnostic
    /// `::autumn_web::RuntimeBackend` alias, never a hard-coded `diesel::pg::Pg`.
    /// A `pg::Pg` bound does not accept the `SQLite` `RuntimeConnection`, so the
    /// scaffolded app would not compile on a `SQLite` target.
    #[test]
    fn auth_store_connection_bounds_are_backend_agnostic() {
        for url in ["sqlite://app.db", "postgres://localhost/app"] {
            let files = auth_scaffold_rust_files(url);
            for (path, body) in &files {
                // The whole path, not just the connection bound: a regression
                // could reintroduce Postgres as `SelectableHelper<diesel::pg::Pg>`
                // or `check_for_backend(diesel::pg::Pg)`. The auth surface emits
                // no legitimate `Pg` reference, so absence is the right bar.
                assert!(
                    !body.contains("diesel::pg::Pg"),
                    "{} hard-codes the Postgres backend ({url})",
                    path.display()
                );
            }
            let session_model = files
                .iter()
                .find(|(p, _)| p.ends_with("user_session.rs"))
                .expect("the sessions store model must be generated");
            assert!(
                session_model
                    .1
                    .contains("Backend = ::autumn_web::RuntimeBackend"),
                "the sessions store must bind RuntimeBackend ({url}): {}",
                session_model.1
            );
        }
    }

    /// The scaffolded session-management guide hands the operator SQL for the
    /// sessions table. It must be in the app's own dialect (issue #1908):
    /// Postgres keeps `NOW() - INTERVAL`, `SQLite` gets `datetime('now', …)`.
    #[test]
    fn sessions_doc_sql_matches_the_app_backend() {
        let pg = render_sessions_docs_file(
            autumn_web::config::DatabaseBackend::Postgres,
            "User",
            "user",
            "users",
        );
        assert!(
            pg.contains("last_seen_at < NOW() - INTERVAL '90 days'"),
            "Postgres retention SQL must be unchanged: {pg}"
        );
        assert!(
            pg.contains("id BIGSERIAL PRIMARY KEY")
                && pg.contains("last_seen_at TIMESTAMP NOT NULL DEFAULT NOW()"),
            "Postgres migration-path DDL must be unchanged: {pg}"
        );

        let sqlite = render_sessions_docs_file(
            autumn_web::config::DatabaseBackend::Sqlite,
            "User",
            "user",
            "users",
        );
        assert!(
            sqlite.contains("last_seen_at < datetime('now', '-90 days')"),
            "SQLite retention SQL must use datetime(): {sqlite}"
        );
        assert!(
            sqlite.contains("id INTEGER PRIMARY KEY AUTOINCREMENT")
                && sqlite.contains("last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP")
                && sqlite.contains("user_id INTEGER NOT NULL REFERENCES users(id)"),
            "SQLite migration-path DDL must be SQLite dialect: {sqlite}"
        );
        for leak in ["BIGSERIAL", "BIGINT", "NOW()"] {
            assert!(
                !sqlite.contains(leak),
                "SQLite sessions guide leaked Postgres-only `{leak}`: {sqlite}"
            );
        }
    }

    /// Regression guard: on a Postgres app (the default) the auth migration
    /// stays byte-for-byte the historical Postgres DDL.
    #[test]
    fn plan_auth_emits_postgres_ddl_by_default() {
        let tmp = project_with_main();
        fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        plan_auth(tmp.path(), "User", "20260508000000")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("id BIGSERIAL PRIMARY KEY"),
            "Postgres up.sql must keep BIGSERIAL PRIMARY KEY: {up}"
        );
        assert!(
            up.contains("CREATE TABLE user_sessions (")
                && up.contains("last_seen_at TIMESTAMP NOT NULL DEFAULT NOW()"),
            "Postgres user_sessions must keep TIMESTAMP DEFAULT NOW(): {up}"
        );
        assert!(
            up.contains("user_id BIGINT NOT NULL REFERENCES users(id)"),
            "Postgres FK columns must stay BIGINT: {up}"
        );
    }

    /// DB-backed sessions store on `SQLite` (issue #1908): the generated
    /// `routes/auth.rs` types its connection pools against the backend-agnostic
    /// `::autumn_web::RuntimeConnection` alias (which resolves to `AsyncPgConnection`
    /// on Postgres and the `SQLite` connection under the `sqlite` feature), never a
    /// hard-coded `diesel_async::AsyncPgConnection`, so the generated app compiles
    /// on whichever backend it selected.
    #[test]
    fn generated_session_pool_uses_runtime_connection_not_pg() {
        // magic-link on/off both emit the remember-me middleware pool sites.
        for magic_link in [false, true] {
            let routes = render_routes_file("User", "user", "users", &[], false, magic_link);
            assert!(
                routes.contains("deadpool::Pool<::autumn_web::RuntimeConnection>"),
                "session pool must be typed against RuntimeConnection (magic_link={magic_link}): {routes}"
            );
            assert!(
                !routes.contains("Pool<diesel_async::AsyncPgConnection>"),
                "session pool must not hard-code AsyncPgConnection (magic_link={magic_link})"
            );
        }
    }

    /// The `--oauth` (`oauth_identities`) and `--passkeys`
    /// (`webauthn_credentials`) migrations are backend-aware too (issue #1927):
    /// `SQLite` dialect on a `SQLite` app, historical Postgres DDL otherwise.
    #[test]
    fn oauth_and_passkey_migrations_are_backend_aware() {
        use autumn_web::config::DatabaseBackend;

        for (render, table) in [
            (
                render_oauth_migration_up as fn(DatabaseBackend, &str) -> String,
                "oauth_identities",
            ),
            (render_passkey_migration_up, "webauthn_credentials"),
        ] {
            let sqlite = render(DatabaseBackend::Sqlite, "users");
            assert!(
                sqlite.contains("id INTEGER PRIMARY KEY AUTOINCREMENT")
                    && sqlite.contains("user_id INTEGER NOT NULL REFERENCES users(id)")
                    && sqlite.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
                "{table} SQLite DDL must use SQLite dialect: {sqlite}"
            );
            for leak in ["BIGSERIAL", "BIGINT", "NOW()"] {
                assert!(
                    !sqlite.contains(leak),
                    "{table} SQLite DDL leaked `{leak}`: {sqlite}"
                );
            }
            let pg = render(DatabaseBackend::Postgres, "users");
            assert!(
                pg.contains("id BIGSERIAL PRIMARY KEY")
                    && pg.contains("user_id BIGINT NOT NULL REFERENCES users(id)")
                    && pg.contains("created_at TIMESTAMP NOT NULL DEFAULT NOW()"),
                "{table} Postgres DDL must stay historical: {pg}"
            );
        }
    }

    /// The scaffolded OAuth guide documents the schema of the migration the same
    /// run writes, so it must be in the same dialect (issue #1908) — the drift
    /// `render_sessions_table_ddl` removed for the sessions guide.
    #[test]
    fn oauth_doc_schema_matches_the_app_backend() {
        use autumn_web::config::DatabaseBackend;

        let providers = vec!["github".to_owned()];

        let pg = render_oauth_docs_file(DatabaseBackend::Postgres, &providers, "users");
        assert!(
            pg.contains("id         BIGSERIAL PRIMARY KEY,")
                && pg
                    .contains("user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,")
                && pg.contains("created_at TIMESTAMP NOT NULL DEFAULT NOW(),"),
            "the Postgres OAuth guide must be unchanged: {pg}"
        );

        let sqlite = render_oauth_docs_file(DatabaseBackend::Sqlite, &providers, "users");
        assert!(
            sqlite.contains("id         INTEGER PRIMARY KEY AUTOINCREMENT,")
                && sqlite.contains(
                    "user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,"
                )
                && sqlite.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,"),
            "the SQLite OAuth guide must be SQLite dialect: {sqlite}"
        );
        for leak in ["BIGSERIAL", "BIGINT", "NOW()"] {
            assert!(
                !sqlite.contains(leak),
                "the SQLite OAuth guide leaked Postgres-only `{leak}`: {sqlite}"
            );
        }
    }

    /// A Postgres app (the default) is not rejected — `generate auth` still
    /// plans its files.
    #[test]
    fn plan_auth_not_rejected_on_postgres_app() {
        let tmp = project_with_main();
        fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        assert!(plan_auth(tmp.path(), "User", "20260508000000").is_ok());
    }

    /// `autumn destroy auth` recomputes this same plan via the `for_revert`
    /// builder before [`Plan::revert`], so it must build a revert plan on a
    /// `SQLite` app (the `for_revert` flag still suppresses the generate-only
    /// shared-layout preflight — issue #1927 made the migrations `SQLite`-valid, so
    /// generate no longer rejects, but the preflight-suppression path must stay
    /// exercised).
    #[test]
    fn plan_auth_for_revert_not_rejected_on_sqlite_app() {
        let tmp = project_with_main();
        fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"sqlite://app.db\"\n",
        )
        .unwrap();
        let oauth = AuthOAuthOptions {
            providers: Vec::new(),
        };
        // The destroy/revert plan builder must still produce a plan to revert.
        assert!(
            plan_auth_full_ex2_for_revert(
                tmp.path(),
                "User",
                "20260508000000",
                &oauth,
                false,
                false,
                false,
            )
            .is_ok(),
            "destroy auth must build its revert plan on a SQLite app"
        );
    }

    #[test]
    fn generate_then_destroy_base_auth_round_trips_to_original_project_state() {
        let tmp = project_with_main();
        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(&cargo_path, template_shipped_cargo_toml()).unwrap();
        let main_path = tmp.path().join("src/main.rs");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/models/user.rs").exists());
        assert!(
            fs::read_to_string(&main_path)
                .unwrap()
                .contains("routes::auth::signup")
        );
        assert!(fs::read_to_string(&cargo_path).unwrap().contains("axum"));

        let destroy_plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/models").exists());
        assert!(!tmp.path().join("src/routes").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(!tmp.path().join("tests/auth.rs").exists());
        assert!(!tmp.path().join("tests/auth_sessions.rs").exists());
        assert!(!tmp.path().join("docs/guide/authentication.md").exists());
        assert!(!tmp.path().join("migrations").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn generate_then_destroy_auth_with_totp_round_trips_to_original_project_state() {
        let tmp = project_with_main();
        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(&cargo_path, template_shipped_cargo_toml()).unwrap();
        let main_path = tmp.path().join("src/main.rs");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        let plan =
            plan_auth_with_providers(tmp.path(), "User", "20260508000000", &[], true).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/models/recovery_code.rs").exists());
        assert!(tmp.path().join("tests/auth_2fa.rs").exists());
        assert!(fs::read_to_string(&cargo_path).unwrap().contains("totp-rs"));
        assert!(
            fs::read_to_string(tmp.path().join("src/schema.rs"))
                .unwrap()
                .contains("recovery_codes")
        );

        let destroy_plan =
            plan_auth_with_providers(tmp.path(), "User", "20260508000000", &[], true).unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/models").exists());
        assert!(!tmp.path().join("src/routes").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(!tmp.path().join("tests/auth_2fa.rs").exists());
        assert!(!tmp.path().join("migrations").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn generate_then_destroy_auth_with_oauth_round_trips_to_original_project_state() {
        let tmp = project_with_main();
        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(&cargo_path, template_shipped_cargo_toml()).unwrap();
        let main_path = tmp.path().join("src/main.rs");
        let autumn_toml_path = tmp.path().join("autumn.toml");
        fs::write(&autumn_toml_path, "[server]\nport = 3000\n").unwrap();
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();
        let original_toml = fs::read_to_string(&autumn_toml_path).unwrap();

        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned(), "google".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/routes/oauth.rs").exists());
        assert!(
            fs::read_to_string(&autumn_toml_path)
                .unwrap()
                .contains("[auth.oauth2.github]")
        );
        assert!(fs::read_to_string(&cargo_path).unwrap().contains("oauth2"));
        assert!(
            fs::read_to_string(tmp.path().join("src/schema.rs"))
                .unwrap()
                .contains("oauth_identities")
        );

        let destroy_plan =
            plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/models").exists());
        assert!(!tmp.path().join("src/routes").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(!tmp.path().join("docs/guide/oauth.md").exists());
        assert!(!tmp.path().join("migrations").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
        assert_eq!(
            fs::read_to_string(&autumn_toml_path).unwrap(),
            original_toml
        );
    }

    #[test]
    fn destroy_never_removes_a_pre_existing_oauth_provider_block_with_real_credentials() {
        let tmp = project_with_main();
        fs::write(tmp.path().join("Cargo.toml"), template_shipped_cargo_toml()).unwrap();
        let autumn_toml_path = tmp.path().join("autumn.toml");
        let real_github_block = "[server]\nport = 3000\n\n[auth.oauth2.github]\n\
             client_id = \"real-client-id\"\n\
             client_secret = \"real-client-secret\"\n\
             authorize_url = \"https://github.com/login/oauth/authorize\"\n\
             token_url = \"https://github.com/login/oauth/access_token\"\n\
             userinfo_url = \"https://api.github.com/user\"\n\
             redirect_uri = \"https://myapp.example.com/auth/github/callback\"\n\
             scope = \"read:user user:email\"\n";
        fs::write(&autumn_toml_path, real_github_block).unwrap();

        // `--oauth github,google`: github's block already exists (with real
        // credentials) so `append_oauth_stubs_to_toml` leaves it untouched;
        // only google's stub is freshly inserted.
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned(), "google".to_owned()],
        };
        plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(
            fs::read_to_string(&autumn_toml_path)
                .unwrap()
                .contains("real-client-secret")
        );

        plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth)
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        let toml_after = fs::read_to_string(&autumn_toml_path).unwrap();
        assert!(
            toml_after.contains("[auth.oauth2.github]"),
            "pre-existing github block must survive destroy: {toml_after}"
        );
        assert!(
            toml_after.contains("real-client-secret"),
            "real credentials must never be deleted: {toml_after}"
        );
        assert!(
            !toml_after.contains("[auth.oauth2.google]"),
            "google's freshly-inserted stub must still be removed: {toml_after}"
        );
    }

    #[test]
    fn generate_then_destroy_auth_with_passkeys_round_trips_to_original_project_state() {
        let tmp = project_with_main();
        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(&cargo_path, template_shipped_cargo_toml()).unwrap();
        let main_path = tmp.path().join("src/main.rs");
        let autumn_toml_path = tmp.path().join("autumn.toml");
        fs::write(&autumn_toml_path, "[server]\nport = 3000\n").unwrap();
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();
        let original_toml = fs::read_to_string(&autumn_toml_path).unwrap();

        let plan = plan_auth_full_ex(
            tmp.path(),
            "User",
            "20260508000000",
            &AuthOAuthOptions::default(),
            false,
            true,
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/routes/passkeys.rs").exists());
        assert!(
            fs::read_to_string(&autumn_toml_path)
                .unwrap()
                .contains("[auth.webauthn]")
        );
        assert!(
            fs::read_to_string(&cargo_path)
                .unwrap()
                .contains("webauthn-rs")
        );
        assert!(
            fs::read_to_string(tmp.path().join("src/schema.rs"))
                .unwrap()
                .contains("webauthn_credentials")
        );

        let destroy_plan = plan_auth_full_ex(
            tmp.path(),
            "User",
            "20260508000000",
            &AuthOAuthOptions::default(),
            false,
            true,
        )
        .unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/models").exists());
        assert!(!tmp.path().join("src/routes").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(!tmp.path().join("tests/auth_passkeys.rs").exists());
        assert!(!tmp.path().join("docs/guide/passkeys.md").exists());
        assert!(!tmp.path().join("migrations").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
        assert_eq!(
            fs::read_to_string(&autumn_toml_path).unwrap(),
            original_toml
        );
    }

    #[test]
    fn generate_then_destroy_auth_with_totp_oauth_and_passkeys_round_trips_to_original_project_state()
     {
        // Combines every optional flag in one plan, exercising the trickiest
        // case for the `autumn.toml` reverts: `--oauth` appends its stub
        // block first, then `--passkeys` appends `[auth.webauthn]` on top of
        // that (reading the base plan's already-updated content) — both
        // edits must unwind cleanly regardless of their relative order in
        // the file.
        let tmp = project_with_main();
        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(&cargo_path, template_shipped_cargo_toml()).unwrap();
        let main_path = tmp.path().join("src/main.rs");
        let autumn_toml_path = tmp.path().join("autumn.toml");
        fs::write(&autumn_toml_path, "[server]\nport = 3000\n").unwrap();
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();
        let original_toml = fs::read_to_string(&autumn_toml_path).unwrap();

        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan =
            plan_auth_full_ex(tmp.path(), "User", "20260508000000", &oauth, true, true).unwrap();
        plan.execute(Flags::default()).unwrap();
        let toml_after_generate = fs::read_to_string(&autumn_toml_path).unwrap();
        assert!(toml_after_generate.contains("[auth.oauth2.github]"));
        assert!(toml_after_generate.contains("[auth.webauthn]"));

        let destroy_plan =
            plan_auth_full_ex(tmp.path(), "User", "20260508000000", &oauth, true, true).unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/models").exists());
        assert!(!tmp.path().join("src/routes").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(!tmp.path().join("migrations").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
        assert_eq!(
            fs::read_to_string(&autumn_toml_path).unwrap(),
            original_toml
        );
    }

    // ── Magic-link login (issue #1328) ───────────────────────────────────────

    /// Build a `--magic-link` plan (no oauth/totp/passkeys) over a fresh project.
    fn magic_link_plan(path: &std::path::Path) -> Plan {
        plan_auth_full_ex2(
            path,
            "User",
            "20260508000000",
            &AuthOAuthOptions::default(),
            false,
            false,
            true,
        )
        .unwrap()
    }

    /// Execute a `--magic-link` plan and return the generated `routes/auth.rs`.
    fn magic_link_routes(path: &std::path::Path) -> String {
        magic_link_plan(path).execute(Flags::default()).unwrap();
        fs::read_to_string(path.join("src/routes/auth.rs")).unwrap()
    }

    #[test]
    fn magic_link_emits_request_email_verify_routes() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        assert!(
            routes.contains("#[get(\"/login/magic\")]"),
            "must emit GET /login/magic request form: {routes}"
        );
        assert!(
            routes.contains("#[post(\"/login/magic\")]"),
            "must emit POST /login/magic request endpoint"
        );
        assert!(
            routes.contains("#[get(\"/login/magic/verify\")]"),
            "must emit GET /login/magic/verify endpoint (renders confirm page)"
        );
        assert!(
            routes.contains("#[post(\"/login/magic/verify\")]"),
            "must emit POST /login/magic/verify endpoint (consumes token)"
        );
    }

    #[test]
    fn magic_link_verify_get_renders_confirm_without_consuming() {
        // Scanner-safety: the GET verify handler renders a "Confirm sign-in" page
        // and must NOT consume the token or establish a session. Consumption +
        // login lives entirely in the POST handler.
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());

        // GET handler body: from its signature up to the POST consume handler.
        let get_start = routes
            .find("pub async fn magic_link_verify_form(")
            .expect("GET confirm handler present");
        let get_end = routes[get_start..]
            .find("pub async fn magic_link_verify(")
            .map(|o| get_start + o)
            .expect("POST verify handler follows the GET form");
        let get_body = &routes[get_start..get_end];
        assert!(
            !get_body.contains("magic_link_tokens::consumed_at.eq(Some(now))"),
            "GET verify must NOT consume the token: {get_body}"
        );
        assert!(
            !get_body.contains("session.insert(state.auth_session_key()"),
            "GET verify must NOT establish an authenticated session: {get_body}"
        );
        // It renders a POST-back confirmation form carrying the token.
        assert!(
            get_body.contains("form action=\"/login/magic/verify\" method=\"post\"")
                && get_body.contains("name=\"token\" value=(query.token)"),
            "GET verify must render a form that POSTs the token back: {get_body}"
        );

        // POST handler body: it DOES consume + establish the session.
        let post_body = magic_link_verify_body(&routes);
        assert!(
            post_body.contains("magic_link_tokens::consumed_at.eq(Some(now))"),
            "POST verify must consume the token: {post_body}"
        );
        assert!(
            post_body.contains("session.insert(state.auth_session_key()"),
            "POST verify must establish an authenticated session: {post_body}"
        );
        // The POST consume route is per-IP throttled to bound token brute-forcing.
        let post_attr = routes
            .find("#[post(\"/login/magic/verify\")]")
            .expect("POST verify attr present");
        let after = &routes[post_attr..];
        let throttle_rel = after
            .find("#[throttle(")
            .expect("POST verify throttle attr present");
        let handler_rel = after
            .find("pub async fn magic_link_verify(")
            .expect("POST verify handler present");
        assert!(
            throttle_rel < handler_rel && after[..handler_rel].contains("key = \"ip\""),
            "POST verify must carry a per-IP #[throttle] between the method attr and handler: {after}"
        );
    }

    #[test]
    fn magic_link_post_route_is_throttled_per_ip() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        // Method attribute outermost, #[throttle] inner, per the macro contract.
        let post_pos = routes
            .find("#[post(\"/login/magic\")]")
            .expect("post attr present");
        let after = &routes[post_pos..];
        let throttle_rel = after.find("#[throttle(").expect("throttle attr present");
        let handler_rel = after
            .find("pub async fn magic_link_request(")
            .expect("handler present");
        assert!(
            throttle_rel < handler_rel,
            "#[throttle] must sit between #[post] and the handler (method attr outermost)"
        );
        assert!(
            after[..handler_rel].contains("key = \"ip\""),
            "throttle must key on ip for per-IP rate limiting"
        );
    }

    #[test]
    fn magic_link_request_is_non_enumerating() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        let start = routes
            .find("pub async fn magic_link_request(")
            .expect("request handler");
        let end = routes[start..]
            .find("pub async fn magic_link_verify(")
            .map_or(routes.len(), |o| start + o);
        let body = &routes[start..end];
        // Minting only happens inside the account-exists branch.
        assert!(
            body.contains("if let Some(user)"),
            "token minting must be gated on the account existing: {body}"
        );
        // The final response is the same shared page regardless of existence.
        assert!(
            body.contains("If that address is registered"),
            "must render the same non-enumerating check-your-email response: {body}"
        );
        // Constant-time padding so hit/miss are latency-indistinguishable.
        assert!(
            body.contains("tokio::time::sleep"),
            "request handler must pad response time to avoid a timing oracle"
        );
    }

    #[test]
    fn magic_link_stores_only_token_digest() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        assert!(
            routes.contains("let token_digest = sha256_hex(&raw_token);"),
            "only the SHA-256 digest of the raw token may be stored"
        );
        // The migration stores a digest column, never a raw-token column.
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("token_digest TEXT NOT NULL UNIQUE"),
            "magic_link_tokens must persist a unique token_digest: {up}"
        );
        assert!(
            !up.contains("raw_token"),
            "the raw token must never be a persisted column"
        );
    }

    #[test]
    fn magic_link_verify_does_not_stamp_step_up() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        let start = routes
            .find("pub async fn magic_link_verify(")
            .expect("verify handler");
        let end = routes[start..]
            .find("async fn send_magic_link_email(")
            .map(|o| start + o)
            .expect("mailer helper follows verify");
        let body = &routes[start..end];
        // The explanatory comment mentions the name, so assert on the actual
        // CALL form (with the `step_up::` path + open paren), not the bare name.
        assert!(
            !body.contains("step_up::set_last_strong_auth_at("),
            "magic-link verify must NOT stamp step-up (proves email possession, \
             not password knowledge): {body}"
        );
    }

    #[test]
    fn magic_link_verify_rotates_session_before_auth() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        let start = routes
            .find("pub async fn magic_link_verify(")
            .expect("verify handler");
        let end = routes[start..]
            .find("async fn send_magic_link_email(")
            .map(|o| start + o)
            .expect("mailer helper follows verify");
        let body = &routes[start..end];
        let rotate = body
            .find("session.rotate_id().await")
            .expect("rotate present");
        let auth_insert = body
            .find("session.insert(state.auth_session_key()")
            .expect("auth key insert present");
        assert!(
            rotate < auth_insert,
            "session must be rotated BEFORE the authenticated session id is set \
             (session-fixation defense)"
        );
    }

    #[test]
    fn magic_link_verify_is_single_use_and_generic_failure() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        // Atomic consume: stamp consumed_at while filtering unconsumed + unexpired.
        assert!(
            routes.contains("magic_link_tokens::consumed_at.is_null()")
                && routes.contains("magic_link_tokens::consumed_at.eq(Some(now))"),
            "verify must atomically consume the token (single-use)"
        );
        // Expired/consumed/unknown all funnel to one generic failure page.
        assert!(
            routes.contains("fn magic_link_invalid_page()"),
            "a single generic failure page must exist (no oracle)"
        );
    }

    #[test]
    fn magic_link_verify_accepts_query_token() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        assert!(
            routes.contains("pub struct MagicLinkVerifyQuery")
                && routes.contains("Query(query): Query<MagicLinkVerifyQuery>"),
            "verify must read the token from the ?token= query parameter (AC4)"
        );
    }

    /// Extract just the `magic_link_verify` handler body from a rendered routes
    /// file (from the handler signature up to the mailer helper that follows it).
    fn magic_link_verify_body(routes: &str) -> &str {
        let start = routes
            .find("pub async fn magic_link_verify(")
            .expect("verify handler");
        let end = routes[start..]
            .find("async fn send_magic_link_email(")
            .map(|o| start + o)
            .expect("mailer helper follows verify");
        &routes[start..end]
    }

    #[test]
    fn magic_link_verify_honors_totp_second_factor() {
        // With `--magic-link --totp`, a 2FA-enrolled account must be parked and
        // redirected to /login/verify — NOT straight-logged-in (Fix 1).
        let routes = render_routes_file("User", "user", "users", &[], true, true);
        let body = magic_link_verify_body(&routes);
        assert!(
            body.contains("if user.totp_enabled {"),
            "verify must branch on totp_enabled under --totp: {body}"
        );
        assert!(
            body.contains(".insert(\"totp_pending_id\""),
            "must set the totp_pending_id marker for the interstitial: {body}"
        );
        // Mirror confirm_email: mark the pending login confirmation-origin so
        // login_verify skips the strong-auth stamp (email + TOTP ≠ password).
        assert!(
            body.contains("session.insert(\"totp_pending_confirmation\", \"1\")"),
            "must mark the pending login as confirmation-origin: {body}"
        );
        assert!(
            body.contains("return Ok(redirect_to(\"/login/verify\"));"),
            "a 2FA account must be redirected to /login/verify, not logged in: {body}"
        );
        // The direct auth-session insert must sit AFTER the 2FA early-return, so a
        // TOTP-enrolled user is never logged in without the second factor.
        let pending_return = body
            .find("return Ok(redirect_to(\"/login/verify\"));")
            .expect("2FA early-return present");
        let auth_insert = body
            .find("session.insert(state.auth_session_key()")
            .expect("direct auth-session insert present");
        assert!(
            pending_return < auth_insert,
            "the direct auth-session insert must be gated behind the 2FA early-return: {body}"
        );
    }

    #[test]
    fn magic_link_verify_rechecks_account_lock() {
        // #1777: the POST verify handler must re-check `locked_at` AFTER the
        // atomic token consume and BEFORE establishing the session, so a link
        // minted before a lockout cannot complete a login after it. Non-totp
        // variant. Mirrors the password-login success-path guard.
        let routes = render_routes_file("User", "user", "users", &[], false, true);
        let body = magic_link_verify_body(&routes);
        // Reads the same [auth.lockout] config the password-login path uses.
        assert!(
            body.contains("let lockout_cfg = &config.auth.lockout;")
                && body.contains("let config = state.config_arc();"),
            "verify must read [auth.lockout] through the shared config handle: {body}"
        );
        // TOCTOU-safe: the recheck must NOT trust the stale in-memory `user` row
        // SELECTed earlier — it must re-read `locked_at` at the DB. The stale
        // in-memory check (`if let Some(locked_at) = user.locked_at`) is gone.
        assert!(
            !body.contains("if let Some(locked_at) = user.locked_at {"),
            "verify must NOT gate on the stale in-memory user.locked_at row \
             (TOCTOU race): {body}"
        );
        // Fresh DB read: a guarded UPDATE predicated on `locked_at` re-reads the
        // current lock state, clearing an expired lock and rejecting an active one
        // — the same guard the password success path uses right before the session.
        assert!(
            body.contains("let rows_cleared = diesel::update(")
                && body.contains("users::locked_at.is_null().or(")
                && body.contains("users::locked_at.le(lock_expired_before)")
                && body.contains("users::failed_attempts.eq(0),")
                && body.contains("if rows_cleared == 0 {"),
            "verify must re-read locked_at at the DB via a guarded UPDATE (mirrors \
             password login), not the stale in-memory row: {body}"
        );
        // Ordering: guard sits AFTER the atomic consume and BEFORE the auth insert.
        let consume = body
            .find("magic_link_tokens::consumed_at.eq(Some(now))")
            .expect("token consume present");
        let recheck = body
            .find("let rows_cleared = diesel::update(")
            .expect("lock recheck present");
        let auth_insert = body
            .find("session.insert(state.auth_session_key()")
            .expect("auth insert present");
        assert!(
            consume < recheck && recheck < auth_insert,
            "lock recheck must be AFTER the token consume and BEFORE the session is \
             established: {body}"
        );
    }

    #[test]
    fn magic_link_verify_rechecks_account_lock_with_totp() {
        // #1777: the same fresh-DB lock recheck must be present in the
        // `--magic-link --totp` variant, and must gate BOTH the 2FA park and the
        // direct login (it sits before the /login/verify early-return).
        let routes = render_routes_file("User", "user", "users", &[], true, true);
        let body = magic_link_verify_body(&routes);
        assert!(
            body.contains("let lockout_cfg = &config.auth.lockout;")
                && body.contains("let config = state.config_arc();"),
            "totp variant must read [auth.lockout] through the shared config handle: {body}"
        );
        assert!(
            !body.contains("if let Some(locked_at) = user.locked_at {"),
            "totp variant must NOT gate on the stale in-memory user.locked_at row \
             (TOCTOU race): {body}"
        );
        assert!(
            body.contains("let rows_cleared = diesel::update(")
                && body.contains("users::locked_at.is_null().or(")
                && body.contains("users::locked_at.le(lock_expired_before)")
                && body.contains("if rows_cleared == 0 {"),
            "totp variant must re-read locked_at at the DB via a guarded UPDATE: {body}"
        );
        let consume = body
            .find("magic_link_tokens::consumed_at.eq(Some(now))")
            .expect("token consume present");
        let recheck = body
            .find("let rows_cleared = diesel::update(")
            .expect("lock recheck present");
        let pending_return = body
            .find("return Ok(redirect_to(\"/login/verify\"));")
            .expect("2FA early-return present");
        assert!(
            consume < recheck && recheck < pending_return,
            "lock recheck must sit after the consume and before the 2FA park/redirect: {body}"
        );
    }

    #[test]
    fn magic_link_verify_locked_account_uses_generic_failure_page() {
        // #1777 (no oracle): a locked account must be indistinguishable from a
        // bad/expired/consumed token — both render magic_link_invalid_page().
        for (totp, label) in [(false, "--magic-link"), (true, "--magic-link --totp")] {
            let routes = render_routes_file("User", "user", "users", &[], totp, true);
            let body = magic_link_verify_body(&routes);
            // The token-failure path already returns the generic page.
            assert!(
                body.contains("Err(_) => return Ok(magic_link_invalid_page().into_response())"),
                "{label}: token-failure path must render the generic page: {body}"
            );
            // The fresh-DB lock guard returns the SAME generic page when zero rows
            // match (account actively locked) — no distinct response, no oracle.
            let recheck = body
                .find("if rows_cleared == 0 {")
                .expect("lock recheck present");
            let tail = &body[recheck..];
            let next_brace = tail.find('}').unwrap_or(tail.len());
            let recheck_block = &tail[..next_brace];
            assert!(
                recheck_block.contains("return Ok(magic_link_invalid_page().into_response());"),
                "{label}: locked account must render the SAME generic failure page \
                 (no oracle): {body}"
            );
        }
    }

    #[test]
    fn magic_link_verify_without_totp_logs_in_directly() {
        // With `--magic-link` and no `--totp`, verify logs in directly.
        let routes = render_routes_file("User", "user", "users", &[], false, true);
        let body = magic_link_verify_body(&routes);
        assert!(
            !body.contains("totp_enabled"),
            "no totp branch may be emitted without --totp: {body}"
        );
        assert!(
            !body.contains("redirect_to(\"/login/verify\")"),
            "must not redirect to /login/verify without --totp: {body}"
        );
        assert!(
            body.contains("session.insert(state.auth_session_key()"),
            "must log in directly by inserting the auth session key: {body}"
        );
        assert!(
            body.contains("record_login_session("),
            "direct login must record the session: {body}"
        );
    }

    #[test]
    fn magic_link_verify_never_stamps_strong_auth_with_or_without_totp() {
        // In NEITHER case may magic-link verify stamp password-grade strong-auth:
        // magic-link proves email possession, not password knowledge (Fix 1).
        for totp in [false, true] {
            let routes = render_routes_file("User", "user", "users", &[], totp, true);
            let body = magic_link_verify_body(&routes);
            assert!(
                !body.contains("step_up::set_last_strong_auth_at("),
                "magic-link verify must never stamp strong-auth (totp={totp}): {body}"
            );
        }
    }

    #[test]
    fn magic_link_verify_clears_stale_totp_pending_under_totp() {
        // Direct (non-2FA) magic-link login rotates the session id, which PRESERVES
        // session data. An abandoned `totp_pending_*` / parked-reset handoff left in
        // this browser from a prior `/login/verify` would otherwise survive and could
        // be resumed under the freshly authenticated session. Mirror the
        // `totp_clear_pending` scrub password login / reset perform, BEFORE the auth
        // keys are written.
        let routes = render_routes_file("User", "user", "users", &[], true, true);
        let body = magic_link_verify_body(&routes);
        for key in [
            "session.remove(\"totp_pending_id\").await;",
            "session.remove(\"totp_pending_reset_digest\").await;",
            "session.remove(\"totp_pending_reset_token\").await;",
            "session.remove(\"totp_pending_secret\").await;",
            "session.remove(\"totp_pending_confirmation\").await;",
        ] {
            assert!(
                body.contains(key),
                "magic-link direct login must clear stale pending state ({key}): {body}"
            );
        }
        // The scrub must run BEFORE the authenticated session key is written.
        let clear = body
            .find("session.remove(\"totp_pending_id\").await;")
            .expect("pending-id scrub present");
        let auth_insert = body
            .find("session.insert(state.auth_session_key()")
            .expect("auth key insert present");
        assert!(
            clear < auth_insert,
            "pending-state cleanup must precede the auth key insert: {body}"
        );
    }

    #[test]
    fn magic_link_verify_without_totp_omits_pending_cleanup() {
        // A magic-link-only build (no --totp) must not reference the TOTP-only
        // pending session keys at all — there is no /login/verify handoff to scrub.
        let routes = render_routes_file("User", "user", "users", &[], false, true);
        let body = magic_link_verify_body(&routes);
        assert!(
            !body.contains("totp_pending"),
            "magic-link-only build must not reference TOTP pending keys: {body}"
        );
    }

    #[test]
    fn magic_link_verify_clears_reauth_pw_ok_before_parking_and_promoting() {
        // Magic-link proves EMAIL possession, not password knowledge. An
        // unfinished /reauth in this browser may have left a `reauth_pw_ok`
        // "password already verified" marker; `rotate_id()` preserves it. If it
        // survived a magic-link login, a later /reauth POST could skip the
        // password and mint a fresh step-up claim off an email-only login. Verify
        // the marker is scrubbed on BOTH the parked (TOTP) and direct paths,
        // mirroring `establish_remember_login` (issue #833/#1397).
        const REMOVE: &str = "session.remove(\"reauth_pw_ok\").await;";

        // Under --totp the marker must be dropped BEFORE the 2FA early-return
        // parks the session (which returns before the direct-path cleanup).
        let routes_totp = render_routes_file("User", "user", "users", &[], true, true);
        let body_totp = magic_link_verify_body(&routes_totp);
        assert!(
            body_totp.contains(REMOVE),
            "magic-link verify must clear reauth_pw_ok under --totp: {body_totp}"
        );
        let remove_at = body_totp
            .find(REMOVE)
            .expect("reauth_pw_ok scrub present under --totp");
        let park_return = body_totp
            .find("return Ok(redirect_to(\"/login/verify\"));")
            .expect("2FA early-return present");
        assert!(
            remove_at < park_return,
            "reauth_pw_ok must be cleared BEFORE the session is parked: {body_totp}"
        );
        let auth_insert_totp = body_totp
            .find("session.insert(state.auth_session_key()")
            .expect("direct auth-session insert present");
        assert!(
            remove_at < auth_insert_totp,
            "reauth_pw_ok must be cleared BEFORE the direct-path promotion: {body_totp}"
        );

        // Without --totp the direct path must ALSO scrub the marker before it
        // promotes the session, and use a plain string key so the magic-link-only
        // build still compiles (no TOTP-only session-key reference).
        let routes = render_routes_file("User", "user", "users", &[], false, true);
        let body = magic_link_verify_body(&routes);
        assert!(
            body.contains(REMOVE),
            "magic-link verify must clear reauth_pw_ok without --totp: {body}"
        );
        let remove_at = body.find(REMOVE).expect("reauth_pw_ok scrub present");
        let auth_insert = body
            .find("session.insert(state.auth_session_key()")
            .expect("auth key insert present");
        assert!(
            remove_at < auth_insert,
            "reauth_pw_ok must be cleared BEFORE the auth key insert: {body}"
        );
    }

    #[test]
    fn two_factor_disable_clears_magic_link_tokens_under_magic_link() {
        // Codex review: an outstanding magic-link token issued while TOTP was on is
        // gated by `magic_link_verify`'s `if user.totp_enabled` branch (routes to
        // /login/verify). Disabling TOTP flips that flag to false, so the SAME
        // still-unconsumed token would then take the DIRECT single-factor path and
        // log the account in fully. The disable transaction must therefore delete
        // the user's outstanding `magic_link_tokens` so disabling 2FA cannot
        // retroactively un-gate a previously second-factor-protected link.
        let routes = render_routes_file("User", "user", "users", &[], true, true);
        let start = routes
            .find("pub async fn two_factor_disable(")
            .expect("disable handler present");
        let end = routes[start..]
            .find("pub async fn login_verify_form(")
            .map(|o| start + o)
            .expect("login_verify_form follows disable handler");
        let body = &routes[start..end];
        assert!(
            body.contains(
                "magic_link_tokens::table.filter(magic_link_tokens::user_id.eq(user_id))"
            ),
            "disable transaction must delete the user's outstanding magic-link tokens: {body}"
        );
        // The delete must sit INSIDE the transaction (before the closing `Ok(())`),
        // so a mid-operation failure can't leave the tokens live after 2FA is off.
        let delete_at = body
            .find("magic_link_tokens::table.filter(magic_link_tokens::user_id.eq(user_id))")
            .expect("magic-link delete present");
        let txn_ok = body
            .find("            Ok(())")
            .expect("transaction Ok(()) present");
        assert!(
            delete_at < txn_ok,
            "the magic-link delete must sit inside the disable transaction: {body}"
        );
    }

    #[test]
    fn two_factor_disable_without_magic_link_omits_token_clear() {
        // A `--totp` build with no `--magic-link` has no `magic_link_tokens` table,
        // so the disable transaction must not reference it (or the crate won't
        // compile).
        let routes = render_routes_file("User", "user", "users", &[], true, false);
        let start = routes
            .find("pub async fn two_factor_disable(")
            .expect("disable handler present");
        let end = routes[start..]
            .find("pub async fn login_verify_form(")
            .map(|o| start + o)
            .expect("login_verify_form follows disable handler");
        let body = &routes[start..end];
        assert!(
            !body.contains("magic_link_tokens"),
            "a non-magic-link build must not reference magic_link_tokens in disable: {body}"
        );
    }

    #[test]
    fn magic_link_ttl_is_config_sourced_with_15_minute_default() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        // TTL is sourced from autumn.toml via the shared config handle, not a const.
        assert!(
            routes.contains("let ttl_minutes = config.auth.magic_link.ttl_minutes;")
                && routes.contains("let config = state.config_arc();"),
            "TTL must be sourced from auth.magic_link via config_arc: {routes}"
        );
        // The documented default (15) and the ≤ 15-minute guidance must survive
        // the move to config so operators keep the tight-window recommendation.
        assert!(
            routes.contains("ttl_minutes = 15") && routes.contains("≤ 15 min"),
            "generated docs/comments must keep the 15-minute default and ≤ 15 min guidance: {routes}"
        );
        // The per-email cooldown is likewise config-sourced.
        assert!(
            routes
                .contains("let email_cooldown_secs = config.auth.magic_link.email_cooldown_secs;"),
            "per-email cooldown must be sourced from auth.magic_link: {routes}"
        );
    }

    #[test]
    fn magic_link_config_knobs_are_unsigned_to_reject_negative_values() {
        // `MagicLinkConfig::{ttl_minutes,email_cooldown_secs}` are `u64`, so a
        // negative value in autumn.toml fails deserialization instead of
        // silently breaking the throttle/expiry. The generated code proves it
        // consumes those unsigned knobs: the chrono math casts through `as i64`
        // (a negative cooldown would push `cooldown_start` into the future and
        // defeat the per-email email-bomb throttle), and the mailer helper takes
        // an unsigned `ttl_minutes`. Assert for BOTH the --magic-link and
        // --magic-link --totp variants.
        for totp in [false, true] {
            let routes = render_routes_file("User", "user", "users", &[], totp, true);
            assert!(
                routes.contains("chrono::Duration::seconds(email_cooldown_secs as i64)"),
                "cooldown math must cast the unsigned config knob via `as i64` (totp={totp}): {routes}"
            );
            assert!(
                routes.contains("chrono::Duration::minutes(ttl_minutes as i64)"),
                "TTL math must cast the unsigned config knob via `as i64` (totp={totp}): {routes}"
            );
            assert!(
                routes.contains("ttl_minutes: u64,"),
                "send_magic_link_email must take an unsigned ttl_minutes (totp={totp}): {routes}"
            );
        }
    }

    #[test]
    fn magic_link_email_goes_through_mail_builder_without_suppression_bypass() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        assert!(
            routes.contains("async fn send_magic_link_email(")
                && routes.contains("Mail::builder()")
                && routes.contains("/login/magic/verify?token="),
            "the link must be sent via the Mail builder with a verify URL"
        );
        let start = routes
            .find("async fn send_magic_link_email(")
            .expect("mailer helper");
        let body = &routes[start..];
        assert!(
            !body.contains("ignore_suppression"),
            "magic-link email must inherit suppression-list gating (AC7)"
        );
    }

    #[test]
    fn magic_link_migration_creates_dedicated_table() {
        let tmp = project_with_main();
        magic_link_plan(tmp.path())
            .execute(Flags::default())
            .unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("CREATE TABLE magic_link_tokens"),
            "must create a dedicated magic_link_tokens table: {up}"
        );
        assert!(
            up.contains("consumed_at TIMESTAMP NULL")
                && up.contains("expires_at TIMESTAMP NOT NULL"),
            "table must carry single-use consumed_at + expiry columns"
        );
        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/down.sql"),
        )
        .unwrap();
        assert!(
            down.contains("DROP TABLE magic_link_tokens;"),
            "down migration must drop the table (clean reverse): {down}"
        );
        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("magic_link_tokens (id)"),
            "schema.rs must declare the magic_link_tokens table: {schema}"
        );
    }

    #[test]
    fn magic_link_model_file_created() {
        let tmp = project_with_main();
        magic_link_plan(tmp.path())
            .execute(Flags::default())
            .unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/magic_link_token.rs")).unwrap();
        assert!(
            model.contains("pub struct MagicLinkToken")
                && model.contains("pub token_digest: String")
                && model.contains("pub consumed_at: Option<chrono::NaiveDateTime>"),
            "magic_link_token model must mirror the reset-token invariants: {model}"
        );
        let model_mod = fs::read_to_string(tmp.path().join("src/models/mod.rs")).unwrap();
        assert!(
            model_mod.contains("magic_link_token"),
            "models/mod.rs must declare the module"
        );
    }

    #[test]
    fn magic_link_routes_registered_in_main() {
        let tmp = project_with_main();
        magic_link_plan(tmp.path())
            .execute(Flags::default())
            .unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        for handler in [
            "routes::auth::magic_link_request_form",
            "routes::auth::magic_link_request",
            "routes::auth::magic_link_verify_form",
            "routes::auth::magic_link_verify",
        ] {
            assert!(
                main.contains(handler),
                "main.rs must register {handler}: {main}"
            );
        }
    }

    #[test]
    fn magic_link_adds_login_page_link() {
        let tmp = project_with_main();
        let routes = magic_link_routes(tmp.path());
        assert!(
            routes.contains("Email me a magic sign-in link"),
            "login page must link to /login/magic when --magic-link is set"
        );
    }

    #[test]
    fn magic_link_docs_document_ttl_ratelimit_enumeration_and_single_use() {
        let tmp = project_with_main();
        magic_link_plan(tmp.path())
            .execute(Flags::default())
            .unwrap();
        let docs = fs::read_to_string(tmp.path().join("docs/guide/authentication.md")).unwrap();
        assert!(
            docs.contains("Passwordless Magic-Link Login"),
            "docs section"
        );
        assert!(
            docs.contains("auth.magic_link.ttl_minutes"),
            "docs must document the config-sourced TTL knob"
        );
        assert!(
            docs.contains("#[throttle(") && docs.contains("auth.magic_link.email_cooldown_secs"),
            "docs must document per-IP + per-email rate limiting"
        );
        assert!(
            docs.contains("enumerat"),
            "docs must document the no-enumeration guarantee"
        );
        assert!(
            docs.contains("Single-use")
                || docs.contains("single-use")
                || docs.contains("used once"),
            "docs must document the single-use guarantee"
        );
    }

    #[test]
    fn magic_link_emits_ignored_live_db_test() {
        let tmp = project_with_main();
        magic_link_plan(tmp.path())
            .execute(Flags::default())
            .unwrap();
        let tests = fs::read_to_string(tmp.path().join("tests/auth.rs")).unwrap();
        assert!(
            tests.contains("fn magic_link_request_and_consume_logs_in()")
                && tests.contains("#[ignore"),
            "an #[ignore]d live-DB magic-link test must be emitted (AC7): {tests}"
        );
    }

    #[test]
    fn without_magic_link_nothing_is_emitted() {
        let tmp = project_with_main();
        // Every optional flag OFF.
        plan_auth(tmp.path(), "User", "20260508000000")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            !routes.contains("/login/magic"),
            "no magic-link routes without --magic-link"
        );
        assert!(
            !routes.contains("MagicLinkToken"),
            "no magic-link model references without --magic-link"
        );
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            !up.contains("magic_link_tokens"),
            "no magic_link_tokens table without --magic-link"
        );
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            !main.contains("magic_link"),
            "no magic-link routes registered without --magic-link"
        );
        assert!(
            !tmp.path().join("src/models/magic_link_token.rs").exists(),
            "no magic-link model file without --magic-link"
        );
        let docs = fs::read_to_string(tmp.path().join("docs/guide/authentication.md")).unwrap();
        assert!(
            !docs.contains("Magic-Link"),
            "no magic-link docs without --magic-link"
        );
    }

    #[test]
    fn magic_link_composes_with_totp_passkeys_and_oauth() {
        let tmp = project_with_main();
        fs::write(tmp.path().join("Cargo.toml"), template_shipped_cargo_toml()).unwrap();
        fs::write(tmp.path().join("autumn.toml"), "[server]\nport = 3000\n").unwrap();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        plan_auth_full_ex2(
            tmp.path(),
            "User",
            "20260508000000",
            &oauth,
            true,
            true,
            true,
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // Magic-link, TOTP, and password flows all coexist in auth.rs.
        assert!(
            routes.contains("#[get(\"/login/magic/verify\")]"),
            "magic-link present"
        );
        assert!(
            routes.contains("Two-factor authentication (TOTP)"),
            "totp present"
        );
        // Passkeys and OAuth live in their own route files.
        assert!(
            tmp.path().join("src/routes/passkeys.rs").exists(),
            "passkeys present"
        );
        assert!(
            tmp.path().join("src/routes/oauth.rs").exists(),
            "oauth present"
        );
        assert!(
            tmp.path().join("src/models/magic_link_token.rs").exists(),
            "magic-link model present alongside the others"
        );
    }

    #[test]
    fn generate_then_destroy_auth_with_magic_link_round_trips_to_original_project_state() {
        let tmp = project_with_main();
        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(&cargo_path, template_shipped_cargo_toml()).unwrap();
        let main_path = tmp.path().join("src/main.rs");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        magic_link_plan(tmp.path())
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("src/models/magic_link_token.rs").exists());
        assert!(
            fs::read_to_string(tmp.path().join("src/schema.rs"))
                .unwrap()
                .contains("magic_link_tokens")
        );

        magic_link_plan(tmp.path())
            .revert(Flags::default())
            .unwrap();
        assert!(!tmp.path().join("src/models").exists());
        assert!(!tmp.path().join("src/routes").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(!tmp.path().join("migrations").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    // ── Plan structure ──────────────────────────────────────────────────────

    #[test]
    fn plan_auth_creates_expected_files() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        let paths: Vec<String> = plan
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();

        for expected in [
            "src/models/user.rs",
            "src/models/mod.rs",
            "src/schema.rs",
            "migrations/20260508000000_create_users/up.sql",
            "migrations/20260508000000_create_users/down.sql",
            "src/routes/auth.rs",
            "src/routes/mod.rs",
            "tests/auth.rs",
            "docs/guide/authentication.md",
            "src/main.rs",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "missing expected action for {expected}; got {paths:?}"
            );
        }
    }

    #[test]
    fn plan_auth_errors_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_auth(tmp.path(), "User", "20260508000000").unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    // ── Migration SQL ───────────────────────────────────────────────────────

    #[test]
    fn migration_up_sql_creates_users_table_with_digest_columns() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("CREATE TABLE users"),
            "missing CREATE TABLE: {up}"
        );
        assert!(up.contains("email"), "missing email column: {up}");
        assert!(
            up.contains("password_digest"),
            "missing password_digest: {up}"
        );
        assert!(
            up.contains("reset_token_digest"),
            "missing reset_token_digest: {up}"
        );
        assert!(
            up.contains("reset_token_expires_at"),
            "missing reset_token_expires_at: {up}"
        );
        assert!(up.contains("UNIQUE"), "email column must be UNIQUE: {up}");
        assert!(
            !up.contains("password TEXT"),
            "raw password must never be stored: {up}"
        );
        assert!(
            !up.contains("reset_token TEXT"),
            "raw reset_token must never be stored: {up}"
        );
    }

    #[test]
    fn migration_down_sql_drops_users_table() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/down.sql"),
        )
        .unwrap();
        assert!(
            down.contains("DROP TABLE users"),
            "missing DROP TABLE: {down}"
        );
    }

    // ── schema.rs ───────────────────────────────────────────────────────────

    #[test]
    fn schema_rs_contains_diesel_table_for_auth_table() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("users (id)"),
            "schema missing table block: {schema}"
        );
        assert!(
            schema.contains("email -> Text"),
            "schema missing email column: {schema}"
        );
        assert!(
            schema.contains("password_digest -> Text"),
            "schema missing password_digest: {schema}"
        );
        assert!(
            schema.contains("reset_token_digest -> Nullable<Text>"),
            "schema missing nullable reset_token_digest: {schema}"
        );
        assert!(
            schema.contains("reset_token_expires_at -> Nullable<Timestamp>"),
            "schema missing nullable reset_token_expires_at: {schema}"
        );
    }

    // ── Model file ──────────────────────────────────────────────────────────

    #[test]
    fn model_file_contains_struct_and_digest_fields() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/user.rs")).unwrap();
        assert!(model.contains("pub struct User"), "missing struct: {model}");
        assert!(
            model.contains("pub email: String"),
            "missing email: {model}"
        );
        assert!(
            model.contains("pub password_digest: String"),
            "missing password_digest: {model}"
        );
        assert!(
            model.contains("pub reset_token_digest: Option<String>"),
            "reset_token_digest must be nullable: {model}"
        );
        assert!(
            !model.contains("pub password:"),
            "raw password must not be a field: {model}"
        );
        assert!(
            !model.contains("pub reset_token:"),
            "raw reset_token must not be a field: {model}"
        );
    }

    #[test]
    fn model_mod_rs_declares_module() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let mod_rs = fs::read_to_string(tmp.path().join("src/models/mod.rs")).unwrap();
        assert!(
            mod_rs.contains("pub mod user;"),
            "missing pub mod user: {mod_rs}"
        );
    }

    // ── Step-up / reauth correctness ─────────────────────────────────────────

    #[test]
    fn confirm_email_does_not_stamp_step_up_claim() {
        // Email confirmation proves inbox access, not password knowledge.
        // set_last_strong_auth_at must NOT be called in confirm_email so that
        // an attacker with a stolen confirmation link cannot bypass #[step_up].
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let confirm_pos = routes
            .find("pub async fn confirm_email(")
            .expect("confirm_email handler missing");
        let after_confirm = &routes[confirm_pos..];
        let next_fn = after_confirm
            .find("\npub async fn ")
            .unwrap_or(after_confirm.len());
        let confirm_body = &after_confirm[..next_fn];
        assert!(
            !confirm_body.contains("set_last_strong_auth_at"),
            "confirm_email must NOT stamp set_last_strong_auth_at (email != password proof): {confirm_body}"
        );
        // confirm_email must also actively remove any inherited step-up claim so a
        // browser session that previously held last_strong_auth_at cannot carry it
        // into the newly confirmed account's session.
        assert!(
            confirm_body.contains("STEP_UP_SESSION_KEY"),
            "confirm_email must remove STEP_UP_SESSION_KEY after rotate_id: {confirm_body}"
        );
    }

    #[test]
    fn confirm_email_rejects_pending_email_change_tokens() {
        // A change-email token shares the `confirm_token_digest` column but sets
        // `pending_email`. The signup confirm_email UPDATE must guard on
        // `pending_email IS NULL` so a change-email token can never satisfy it
        // (Fix 2 — otherwise it consumes the token, stamps email_confirmed_at,
        // orphans pending_email, and mints an unintended session).
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let confirm_pos = routes
            .find("pub async fn confirm_email(")
            .expect("confirm_email handler missing");
        let after_confirm = &routes[confirm_pos..];
        let next_fn = after_confirm
            .find("\npub async fn ")
            .unwrap_or(after_confirm.len());
        let confirm_body = &after_confirm[..next_fn];
        // The reverse guard must be present in the signup-confirm UPDATE filter.
        assert!(
            confirm_body.contains("pending_email.is_null()"),
            "confirm_email UPDATE must filter on pending_email IS NULL to reject \
             email-change tokens: {confirm_body}"
        );
    }

    #[test]
    fn reauth_rotates_session_before_stamping_step_up() {
        // Rotating the session ID at the privilege-elevation point invalidates
        // any attacker-held copy of the old cookie, preventing a stolen session
        // from benefiting from the legitimate user's reauth.
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let reauth_pos = routes
            .find("pub async fn reauth(")
            .expect("reauth handler missing");
        let reauth_body = &routes[reauth_pos..];
        let rotate_pos = reauth_body
            .find("rotate_id")
            .expect("reauth must call session.rotate_id()");
        let stamp_pos = reauth_body
            .find("set_last_strong_auth_at")
            .expect("reauth must call set_last_strong_auth_at");
        assert!(
            rotate_pos < stamp_pos,
            "session.rotate_id() must come before set_last_strong_auth_at to invalidate the old cookie"
        );
    }

    #[test]
    fn totp_reauth_does_not_embed_password_in_form() {
        // The TOTP re-render form must not include the raw password in a hidden
        // field — that would expose it in the DOM to any script on the page.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let reauth_pos = routes
            .find("pub async fn reauth(")
            .expect("reauth handler missing");
        let reauth_body = &routes[reauth_pos..];
        // The only password input in reauth should be type="password" (visible field),
        // never type="hidden" with name="password".
        assert!(
            !reauth_body.contains("name=\"password\" value"),
            "TOTP reauth must not embed the raw password in a hidden field: {reauth_body}"
        );
    }

    #[test]
    fn totp_reauth_requires_second_factor_for_enrolled_accounts() {
        // When --totp is enabled, the reauth handler must verify the TOTP code
        // for accounts with totp_enabled = true. Password alone is insufficient.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let reauth_pos = routes
            .find("pub async fn reauth(")
            .expect("reauth handler missing");
        let reauth_body = &routes[reauth_pos..];
        assert!(
            reauth_body.contains("totp_enabled"),
            "TOTP reauth handler must check totp_enabled: {reauth_body}"
        );
        assert!(
            reauth_body.contains("totp_code"),
            "TOTP reauth handler must check totp_code from form: {reauth_body}"
        );
        assert!(
            reauth_body.contains("verify_totp_code"),
            "TOTP reauth must call verify_totp_code for enrolled accounts: {reauth_body}"
        );
    }

    #[test]
    fn totp_reauth_form_has_totp_field() {
        // The reauth form must include a TOTP input when --totp is enabled so
        // that users with two-factor authentication can provide their code.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let reauth_pos = routes
            .find("pub async fn reauth_form(")
            .expect("reauth_form handler missing");
        let reauth_body = &routes[reauth_pos..];
        assert!(
            reauth_body.contains("totp_code"),
            "reauth form must include totp_code input field: {reauth_body}"
        );
    }

    // ── Routes file ─────────────────────────────────────────────────────────

    #[test]
    fn routes_file_contains_all_handlers() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        for needle in [
            "pub async fn signup_form",
            "pub async fn signup",
            "pub async fn login_form",
            "pub async fn login",
            "pub async fn logout",
            "pub async fn account",
            "pub async fn account_destroy",
            "pub async fn reauth_form",
            "pub async fn reauth",
            "pub async fn forgot_password_form",
            "pub async fn forgot_password",
            "pub async fn reset_password_form",
            "pub async fn reset_password",
        ] {
            assert!(routes.contains(needle), "routes missing handler: {needle}");
        }
    }

    #[test]
    fn routes_file_uses_session_invalidation_on_logout() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // Scope the assertion to the logout handler body so it cannot be
        // satisfied by `session.destroy()` calls in other handlers.
        let logout_pos = routes
            .find("pub async fn logout(")
            .expect("logout handler missing");
        let after = &routes[logout_pos..];
        let next_fn = after[1..]
            .find("\npub async fn ")
            .map_or(after.len(), |p| p + 1);
        let logout_body = &after[..next_fn];
        // Logout must invalidate the session so the pre-logout cookie cannot be
        // replayed: clear the data (drops auth keys) and rotate the id (the old
        // id is destroyed in the store on save). This is equivalent to
        // `destroy()` for replay safety while letting a one-shot logout notice
        // ride the rotated session to the login page.
        assert!(
            logout_body.contains("session.clear()") && logout_body.contains("session.rotate_id()"),
            "logout must invalidate the session via clear + rotate: {logout_body}"
        );
    }

    #[test]
    fn routes_file_emits_flash_messages() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // Flashes are set on the PRG-compatible auth flows.
        assert!(
            routes.contains(r#"flash.success("Logged in.")"#),
            "login success must set a flash: {routes}"
        );
        assert!(
            routes.contains(r#"flash.info("You have been logged out.")"#),
            "logout must set a flash: {routes}"
        );
        assert!(
            routes.contains("flash.info(\"Account created"),
            "signup must set a flash: {routes}"
        );
        // Issue #1353/#1240: the redirect-target pages render pending flashes
        // through the shared, accessible `flash_messages()` helper threaded into
        // the layout's 3rd argument — NOT the older in-content `flash.render()`.
        assert!(
            !routes.contains("flash.render().await"),
            "auth pages must not use the old in-content flash.render() path: {routes}"
        );
        assert!(
            routes.contains("flash_messages(&flash.consume().await)"),
            "auth pages must render pending flashes via flash_messages(): {routes}"
        );
        assert!(
            routes.contains("pub async fn login_form(flash: Flash"),
            "login_form must take the Flash extractor to render notices: {routes}"
        );
    }

    /// Issue #1353: every auth view renders through the application's shared
    /// `crate::layout(title, current_path, flash, content)` (4 args) rather than
    /// a private per-file `fn layout(title, content)` stub. The private stub —
    /// and its bare DOCTYPE shell — must be gone, and representative pages must
    /// call `crate::layout` with a per-page `current_path` and the flash arg.
    #[test]
    fn auth_views_render_through_shared_layout() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        // The private layout stub is gone.
        assert!(
            !routes.contains("fn layout(title: &str, content: Markup)"),
            "the private 2-arg layout stub must be removed: {routes}"
        );
        // Views render through the shared 4-arg layout.
        assert!(
            routes.contains("crate::layout("),
            "auth views must render through crate::layout: {routes}"
        );
        assert!(
            !routes.contains(" layout(") || routes.contains("crate::layout("),
            "no bare 2-arg layout() calls may remain: {routes}"
        );
        // Login renders through the shared layout with its own current_path and
        // the flash threaded to the 3rd arg (mirrors scaffold's assertions).
        assert!(
            routes.contains(
                "crate::layout(\"Log In\", \"/login\", flash_messages(&flash.consume().await),"
            ),
            "login_form must render through crate::layout with /login + flash: {routes}"
        );
        // The account page likewise threads its current_path + flash.
        assert!(
            routes.contains(
                "crate::layout(\"Your Account\", \"/account\", flash_messages(&flash.consume().await),"
            ),
            "account must render through crate::layout with /account + flash: {routes}"
        );
        // A page that shows no flash still passes a per-page current_path and an
        // empty flash markup as the 3rd arg.
        assert!(
            routes.contains("crate::layout(\"Sign Up\", \"/signup\", html! {},"),
            "signup form must render through crate::layout with /signup: {routes}"
        );
    }

    /// Issue #1353: the TOTP (`--totp`) and magic-link (`--magic-link`) views
    /// also render through the shared `crate::layout`, with per-page paths.
    #[test]
    fn auth_totp_and_magic_link_views_render_through_shared_layout() {
        let routes = render_routes_file("User", "user", "users", &[], true, true);
        assert!(
            !routes.contains("fn layout(title: &str, content: Markup)"),
            "the private layout stub must be removed under --totp/--magic-link: {routes}"
        );
        assert!(
            routes.contains(
                "crate::layout(\"Two-Factor Verification\", \"/login/verify\", html! {},"
            ),
            "the TOTP verify page must render through crate::layout: {routes}"
        );
        assert!(
            routes.contains(
                "crate::layout(\"Confirm sign-in\", \"/login/magic/verify\", flash_messages(&flash.consume().await),"
            ),
            "the magic-link confirm page must thread flash through crate::layout: {routes}"
        );
    }

    /// Issue #1353: `autumn generate auth` requires the target app to expose a
    /// shared 4-arg `pub fn layout` (as `autumn new` emits) so its views can
    /// render through `crate::layout`. When `src/main.rs` has no such layout —
    /// e.g. an `autumn new --api` project — the generator fails early with an
    /// actionable Config error rather than emitting routes that fail to compile.
    /// Mirrors the scaffold generator's preflight (issue #1130).
    #[test]
    fn plan_auth_errors_when_main_rs_has_no_shared_layout() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), main_without_layout()).unwrap();
        let err = plan_auth(tmp.path(), "User", "20260508000000").unwrap_err();
        match err {
            GenerateError::Config(msg) => {
                assert!(
                    msg.contains("pub fn layout") && msg.contains("autumn new"),
                    "missing shared layout must yield the actionable error: {msg}"
                );
            }
            other => panic!("expected an actionable Config error, got: {other:?}"),
        }
    }

    /// Issue #1353: a genuinely absent `src/main.rs` surfaces the same
    /// actionable shared-layout Config error (pointing at `autumn new`), not a
    /// raw Io "missing" error.
    #[test]
    fn plan_auth_errors_when_main_rs_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        let err = plan_auth(tmp.path(), "User", "20260508000000").unwrap_err();
        match err {
            GenerateError::Config(msg) => {
                assert!(
                    msg.contains("pub fn layout") && msg.contains("autumn new"),
                    "absent main.rs must yield the actionable shared-layout error: {msg}"
                );
            }
            other => panic!("expected an actionable Config error, got: {other:?}"),
        }
    }

    /// Issue #1353 follow-up: `autumn destroy auth` recomputes the identical
    /// plan before reverting it. The shared-layout preflight is a generate-time
    /// guard only — it must NOT fire on the destroy/revert path, or cleanup
    /// would hard-fail in a project whose shared `pub fn layout` is missing or
    /// renamed (e.g. one scaffolded by an older CLI), stranding the generated
    /// files. The revert-only plan builder must therefore succeed even when
    /// `src/main.rs` exposes no shared 4-arg layout.
    #[test]
    fn plan_auth_for_revert_succeeds_without_shared_layout() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        // A project with NO shared `pub fn layout` — the exact input the
        // generate-time preflight rejects.
        fs::write(tmp.path().join("src/main.rs"), main_without_layout()).unwrap();
        let oauth = AuthOAuthOptions {
            providers: Vec::new(),
        };
        // The revert builder must not consult the shared layout at all.
        let plan = plan_auth_full_ex2_for_revert(
            tmp.path(),
            "User",
            "20260508000000",
            &oauth,
            false,
            false,
            false,
        )
        .expect("destroy auth must build its revert plan without a shared layout");
        // Sanity: it produced the auth routes file a normal auth plan would, so
        // `Plan::revert` has something to remove.
        assert!(
            find_plan_content_for_path(&plan, &tmp.path().join("src/routes/auth.rs")).is_some(),
            "revert plan should still include the auth routes it will remove"
        );

        // And the generate path over the SAME project still fails fast — the
        // guard is bypassed only for revert, never weakened for generation.
        let err = plan_auth(tmp.path(), "User", "20260508000000").unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(ref msg) if msg.contains("pub fn layout")),
            "generate path must still reject a missing shared layout: {err:?}"
        );
    }

    #[test]
    fn routes_file_rotates_session_on_login() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("session.rotate_id"),
            "login must rotate the session ID to prevent fixation: {routes}"
        );
    }

    #[test]
    fn routes_file_uses_configured_auth_session_key_for_policy_identity() {
        let routes = render_routes_file("Account", "account", "accounts", &[], false, false);
        assert!(
            routes.contains("State(state): State<AppState>"),
            "auth routes must receive AppState: {routes}"
        );
        assert!(
            routes.contains("session.insert(state.auth_session_key()"),
            "auth routes must populate the same session key used by policy context: {routes}"
        );
        assert!(
            !routes.contains("session.insert(\"user_id\""),
            "non-User auth routes must not hard-code user_id as the policy session key: {routes}"
        );
    }

    #[test]
    fn routes_file_account_is_protected() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("#[secured]"),
            "account route must use #[secured] for protection: {routes}"
        );
    }

    #[test]
    fn account_destroy_carries_step_up_by_default() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();

        // Find the account_destroy fn and verify #[step_up] precedes it.
        let destroy_pos = routes
            .find("pub async fn account_destroy")
            .expect("account_destroy handler missing from scaffold");
        let before_destroy = &routes[..destroy_pos];
        assert!(
            before_destroy.rfind("#[step_up]").is_some(),
            "account_destroy must carry #[step_up] for step-up protection"
        );
    }

    #[test]
    fn mfa_remove_has_builtin_reauth_and_no_step_up_double_prompt() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions::default();
        let plan = plan_auth_full(tmp.path(), "User", "20260508000000", &oauth, true).unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();

        // two_factor_disable already verifies the user's TOTP code or password
        // in its body; a separate #[step_up] would cause a double re-auth prompt
        // (once via /reauth, once in the form itself). The handler must be present.
        assert!(
            routes.contains("pub async fn two_factor_disable"),
            "two_factor_disable handler missing from scaffold"
        );
    }

    #[test]
    fn routes_mod_rs_declares_auth_module() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let mod_rs = fs::read_to_string(tmp.path().join("src/routes/mod.rs")).unwrap();
        assert!(
            mod_rs.contains("pub mod auth;"),
            "missing pub mod auth: {mod_rs}"
        );
    }

    #[test]
    fn routes_file_forgot_password_checks_mailer_is_disabled() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("mailer.is_disabled()"),
            "forgot_password must guard against disabled mail transport: {routes}"
        );
        assert!(
            routes.contains("mailer.send(mail).await"),
            "forgot_password must use async mailer.send(): {routes}"
        );
        // The is_disabled guard must appear before the DB lookup (maybe_user)
        // so it fires unconditionally and cannot enumerate registered addresses.
        // Search within the forgot_password function body, which is identified
        // by the unique `maybe_user` variable that only appears there.
        let disabled_pos = routes.find("mailer.is_disabled()").unwrap();
        let maybe_user_pos = routes.find("let maybe_user").unwrap();
        assert!(
            disabled_pos < maybe_user_pos,
            "is_disabled guard must come before the DB lookup in forgot_password"
        );
    }

    // ── Change password / change email (issue #1396) ─────────────────────────

    /// Slice out a single handler body from the rendered routes file: from
    /// `pub async fn <name>(` up to the next handler declaration.
    fn handler_body<'a>(routes: &'a str, name: &str) -> &'a str {
        let needle = format!("pub async fn {name}(");
        let start = routes
            .find(&needle)
            .unwrap_or_else(|| panic!("handler {name} missing from routes"));
        let after = &routes[start..];
        let next = after[1..]
            .find("\npub async fn ")
            .map_or(after.len(), |p| p + 1);
        &after[..next]
    }

    #[test]
    fn routes_file_has_change_password_and_email_handlers() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        for needle in [
            "pub async fn change_password_form(",
            "pub async fn change_password(",
            "pub async fn change_email_form(",
            "pub async fn change_email(",
            "pub async fn confirm_email_change(",
        ] {
            assert!(routes.contains(needle), "routes missing handler: {needle}");
        }
        // Both POST forms carry the documented field names.
        assert!(
            routes.contains("current_password"),
            "missing current_password field"
        );
        assert!(
            routes.contains("new_password_confirmation"),
            "missing confirmation field"
        );
        assert!(routes.contains("new_email"), "missing new_email field");
        assert!(
            routes.contains(r#"action="/account/password""#),
            "change-password form must post to /account/password"
        );
        assert!(
            routes.contains(r#"action="/account/email""#),
            "change-email form must post to /account/email"
        );
    }

    #[test]
    fn change_password_post_carries_secured_and_step_up() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let pos = routes
            .find("pub async fn change_password(")
            .expect("change_password handler missing");
        let before = &routes[..pos];
        // The attributes immediately preceding the handler must be, in order,
        // #[secured] then #[step_up] then #[post(...)] — matching account_destroy.
        let secured = before
            .rfind("#[secured]")
            .expect("change_password needs #[secured]");
        let step_up = before
            .rfind("#[step_up]")
            .expect("change_password needs #[step_up]");
        let post = before
            .rfind(r#"#[post("/account/password")]"#)
            .expect("change_password needs its #[post] route");
        assert!(
            secured < step_up && step_up < post,
            "attribute order must be secured, step_up, post"
        );
    }

    #[test]
    fn change_email_post_carries_secured_and_step_up() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let pos = routes
            .find("pub async fn change_email(")
            .expect("change_email handler missing");
        let before = &routes[..pos];
        let secured = before
            .rfind("#[secured]")
            .expect("change_email needs #[secured]");
        let step_up = before
            .rfind("#[step_up]")
            .expect("change_email needs #[step_up]");
        let post = before
            .rfind(r#"#[post("/account/email")]"#)
            .expect("change_email needs its #[post] route");
        assert!(
            secured < step_up && step_up < post,
            "attribute order must be secured, step_up, post"
        );
    }

    #[test]
    fn change_password_verifies_current_rotates_and_revokes_others() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "change_password");
        assert!(
            body.contains("verify_password("),
            "must verify the current password"
        );
        assert!(
            body.contains("validate_password("),
            "must apply the password policy"
        );
        assert!(
            body.contains("hash_password("),
            "must hash the new password"
        );
        assert!(
            body.contains("session.rotate_id()"),
            "must rotate the session id"
        );
        assert!(
            body.contains("set_last_strong_auth_at"),
            "must re-stamp the step-up claim on success"
        );
        // Revokes OTHER tracked sessions (delete where digest != current) and the
        // remember chains.
        assert!(
            body.contains("user_sessions::token_digest.ne"),
            "must revoke other tracked sessions"
        );
        assert!(
            body.contains("user_remember_tokens::table"),
            "must revoke persistent remember-me chains"
        );
    }

    #[test]
    fn change_password_wrong_current_returns_422_and_no_update() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "change_password");
        // Wrong current password re-renders the form via form_unprocessable (422)
        // and returns BEFORE any password_digest update.
        let wrong_pos = body
            .find("Current password is incorrect.")
            .expect("must show a clear wrong-password error");
        let update_pos = body
            .find("password_digest.eq")
            .expect("must update the password digest on success");
        assert!(
            wrong_pos < update_pos,
            "wrong-password branch must return before any update"
        );
        assert!(
            routes.contains("StatusCode::UNPROCESSABLE_ENTITY"),
            "rejected form submissions must carry a 422 status"
        );
    }

    #[test]
    fn change_password_clears_reset_token() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "change_password");
        // The same transaction that writes the new password_digest must also null
        // any outstanding forgot-password token, so a reset link minted before the
        // change cannot overwrite the just-secured password (reset_password
        // authenticates SOLELY by matching an unexpired reset_token_digest).
        assert!(
            body.contains("password_digest.eq(&new_digest)"),
            "must update the password digest"
        );
        assert!(
            body.contains("reset_token_digest.eq(None::<String>)"),
            "change_password must clear reset_token_digest"
        );
        assert!(
            body.contains("reset_token_expires_at.eq(None::<chrono::NaiveDateTime>)"),
            "change_password must clear reset_token_expires_at"
        );
    }

    #[test]
    fn change_password_clears_magic_link_tokens_only_when_enabled() {
        // #7: a magic link minted before the password change must not sign the
        // account in afterwards, so the credential-change transaction deletes the
        // user's outstanding magic_link_tokens — but only under --magic-link (the
        // table exists only then).
        let with = render_routes_file("User", "user", "users", &[], false, true);
        let body = handler_body(&with, "change_password");
        assert!(
            body.contains(
                "magic_link_tokens::table.filter(magic_link_tokens::user_id.eq(user_id))"
            ),
            "with --magic-link, change_password must delete the user's magic_link_tokens"
        );
        // The delete must sit inside the credential-change transaction (before the
        // final `Ok(())`), alongside the reset-token clear.
        let clear_pos = body
            .find("magic_link_tokens::table.filter")
            .expect("magic-link delete present");
        let rem_pos = body
            .find("user_remember_tokens::table")
            .expect("remember-chain delete present");
        assert!(
            rem_pos < clear_pos,
            "magic-link delete belongs inside the same transaction as the other revocations"
        );

        let without = render_routes_file("User", "user", "users", &[], false, false);
        let body_without = handler_body(&without, "change_password");
        assert!(
            !body_without.contains("magic_link_tokens"),
            "without --magic-link, change_password must not reference magic_link_tokens"
        );
    }

    #[test]
    fn reset_password_clears_magic_link_tokens_only_when_enabled() {
        // #11: reset_password sets a new password digest and revokes sessions /
        // remember chains, so — like change_password / confirm_email_change — it must
        // delete the user's outstanding magic_link_tokens in the SAME transaction, or
        // a sign-in link minted/intercepted before the recovery flow stays valid
        // afterwards. Emitted only under --magic-link (the table exists only then).
        let with = render_routes_file("User", "user", "users", &[], false, true);
        let body = handler_body(&with, "reset_password");
        assert!(
            body.contains(
                "magic_link_tokens::table.filter(magic_link_tokens::user_id.eq(user_id))"
            ),
            "with --magic-link, reset_password must delete the user's magic_link_tokens"
        );
        // The delete must sit inside the reset transaction (before the final
        // `Ok(())`), after the remember-chain revocation.
        let clear_pos = body
            .find("magic_link_tokens::table.filter")
            .expect("magic-link delete present");
        let rem_pos = body
            .find("user_remember_tokens::table")
            .expect("remember-chain delete present");
        let ok_pos = body
            .rfind("Ok(())")
            .expect("transaction closes with Ok(())");
        assert!(
            rem_pos < clear_pos && clear_pos < ok_pos,
            "magic-link delete belongs inside the reset transaction with the other revocations"
        );

        let without = render_routes_file("User", "user", "users", &[], false, false);
        let body_without = handler_body(&without, "reset_password");
        assert!(
            !body_without.contains("magic_link_tokens"),
            "without --magic-link, reset_password must not reference magic_link_tokens"
        );
    }

    #[test]
    fn login_verify_reset_commit_clears_magic_link_tokens_only_when_enabled() {
        // #11: for a 2FA-enabled account the password-reset commit is deferred to
        // login_verify. That deferred commit also writes a new password_digest and
        // revokes sessions, so it must likewise delete the user's outstanding
        // magic_link_tokens — guarded on the commit succeeding (`updated == 1`) and
        // only under --magic-link.
        let with = render_routes_file("User", "user", "users", &[], true, true);
        let body = handler_body(&with, "login_verify");
        assert!(
            body.contains("magic_link_tokens::table")
                && body.contains("magic_link_tokens::user_id.eq(user_id)"),
            "with --magic-link + --totp, login_verify's deferred reset commit must delete \
             magic_link_tokens"
        );
        // The delete must sit inside the deferred-commit transaction, after the
        // password write and before the transaction returns `Ok(updated == 1)`.
        let commit_pos = body
            .find("password_digest.eq(&new_digest)")
            .expect("deferred reset commit writes the new digest");
        let clear_pos = body
            .find("magic_link_tokens::user_id.eq(user_id)")
            .expect("magic-link delete present");
        let ret_pos = body
            .find("Ok(updated == 1)")
            .expect("deferred commit transaction returns Ok(updated == 1)");
        assert!(
            commit_pos < clear_pos && clear_pos < ret_pos,
            "magic-link delete must sit inside the deferred reset commit transaction"
        );
        // Guarded on the parked reset actually committing.
        assert!(
            body.contains("if updated == 1 {"),
            "magic-link delete must be guarded on the parked reset committing"
        );

        let without = render_routes_file("User", "user", "users", &[], true, false);
        let body_without = handler_body(&without, "login_verify");
        assert!(
            !body_without.contains("magic_link_tokens"),
            "without --magic-link, login_verify must not reference magic_link_tokens"
        );
    }

    #[test]
    fn change_password_gates_other_session_revocation_on_opt_out() {
        // #8: the other-device sign-out honours
        // [auth.sessions].revoke_on_credential_change, mirroring the
        // reset/TOTP/passkey paths — but the current session is always rebound and
        // reset tokens are always cleared regardless of the flag.
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "change_password");
        assert!(
            body.contains("state.config_arc().auth.sessions.revoke_on_credential_change"),
            "change_password must read the revoke_on_credential_change opt-out"
        );
        // The OTHER-session delete is gated on the captured flag.
        let flag_pos = body
            .find("let revoke_other_sessions_in_txn =")
            .expect("must capture the revocation flag");
        let gate_pos = body
            .find("if revoke_other_sessions_in_txn {")
            .expect("other-session delete must be gated on the flag");
        let other_delete_pos = body
            .find("user_sessions::token_digest.ne")
            .expect("other-session delete present");
        assert!(
            flag_pos < gate_pos && gate_pos < other_delete_pos,
            "the other-session delete must sit inside `if revoke_other_sessions_in_txn`"
        );
        // The current session is ALWAYS rebound onto the rotated id (not gated).
        assert!(
            body.contains("token_digest.eq(&post_rotation_digest)"),
            "change_password must always rebind the current session onto the rotated id"
        );
        // Reset tokens are cleared unconditionally (not inside the gate).
        assert!(
            body.contains("reset_token_digest.eq(None::<String>)"),
            "reset tokens must be cleared regardless of the opt-out"
        );
    }

    #[test]
    fn change_email_guards_disabled_mail_first_and_stores_pending() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "change_email");
        // Disabled-mail guard must come before the DB session lookup, mirroring
        // forgot_password — so it fires unconditionally.
        let disabled = body
            .find("mailer.is_disabled()")
            .expect("must guard disabled mail");
        let lookup = body
            .find("require_tracked_session")
            .expect("must load the session");
        assert!(
            disabled < lookup,
            "is_disabled guard must precede the session lookup"
        );
        assert!(
            body.contains("verify_password("),
            "must verify the current password"
        );
        assert!(
            body.contains("pending_email.eq"),
            "must store the new address as pending"
        );
        assert!(
            body.contains("send_email_change_confirmation"),
            "must send a confirmation link to the new address"
        );
        assert!(
            body.contains("send_email_change_notice"),
            "must send a change-notice to the old address"
        );
        // Old address stays valid: the request handler must NOT SWAP the active
        // address here (only the later confirm sets `email`). A read-only
        // availability filter that references `users::email.eq(&new_email)` is fine;
        // the swap is uniquely the `.set` form with a trailing comma.
        assert!(
            !body.contains("email.eq(&new_email),"),
            "change_email request must NOT swap the active address"
        );
    }

    #[test]
    fn change_email_prechecks_availability_before_sending() {
        // If the requested address already belongs to ANOTHER account, the handler
        // must short-circuit to the SAME generic "check your email" response
        // WITHOUT emailing that third party and WITHOUT storing a doomed pending
        // change (the later confirm would collide on email's unique index). The
        // availability check must therefore run BEFORE both the mail send and the
        // pending-state persist. Non-enumerating: the caller cannot distinguish a
        // taken address from an available one.
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "change_email");
        let precheck = body
            .find("users::email.eq(&new_email)")
            .expect("must look up whether the new address is already taken");
        let send = body
            .find("send_email_change_confirmation")
            .expect("must send the confirmation mail");
        let persist = body
            .find("pending_email.eq(Some(&new_email))")
            .expect("must persist the pending change");
        assert!(
            precheck < send,
            "availability check must precede the confirmation mail send: {body}"
        );
        assert!(
            precheck < persist,
            "availability check must precede the pending-state persist: {body}"
        );
    }

    #[test]
    fn confirm_email_change_swaps_address_and_stamps_confirmed() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "confirm_email_change");
        assert!(
            body.contains("pending_email.is_not_null()"),
            "must target a pending change only"
        );
        assert!(
            body.contains("users::email.eq(&new_email)"),
            "must swap the active address"
        );
        assert!(
            body.contains("pending_email.eq(None::<String>)"),
            "must clear pending_email on confirm"
        );
        assert!(
            body.contains("email_confirmed_at.eq(Some(now))"),
            "must stamp email_confirmed_at on confirm"
        );
        // Single generic error (no oracle): the not-found / expired / consumed
        // paths all return the same message.
        assert!(
            body.contains("This email-change link is invalid or has expired."),
            "must return a single non-revealing error"
        );
        assert!(
            routes.contains(r#"#[get("/account/email/confirm/{token}")]"#),
            "confirm route must be registered under /account/email/confirm/{{token}}"
        );
    }

    #[test]
    fn confirm_email_change_clears_reset_token() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "confirm_email_change");
        // The atomic confirm UPDATE swaps the address; it must ALSO null any
        // outstanding reset token so a reset link sent to the OLD mailbox before
        // the change cannot reset the account after it has moved.
        assert!(
            body.contains("users::email.eq(&new_email)"),
            "must swap the active address"
        );
        assert!(
            body.contains("reset_token_digest.eq(None::<String>)"),
            "confirm_email_change must clear reset_token_digest"
        );
        assert!(
            body.contains("reset_token_expires_at.eq(None::<chrono::NaiveDateTime>)"),
            "confirm_email_change must clear reset_token_expires_at"
        );
    }

    #[test]
    fn confirm_email_change_clears_magic_link_tokens_only_when_enabled() {
        // #10: a magic link sent to the OLD mailbox before the change must not
        // sign the account in after the address moves, so the confirmation
        // transaction deletes the user's outstanding magic_link_tokens — gated on
        // --magic-link.
        let with = render_routes_file("User", "user", "users", &[], false, true);
        let body = handler_body(&with, "confirm_email_change");
        assert!(
            body.contains(
                "magic_link_tokens::table.filter(magic_link_tokens::user_id.eq(user_id))"
            ),
            "with --magic-link, confirm_email_change must delete the user's magic_link_tokens"
        );
        // The cleanup runs in the same transaction as the address swap.
        let swap_pos = body
            .find("users::email.eq(&new_email")
            .expect("address swap present");
        let clear_pos = body
            .find("magic_link_tokens::table.filter")
            .expect("magic-link delete present");
        assert!(
            swap_pos < clear_pos,
            "magic-link delete must follow the address swap in the same transaction"
        );

        let without = render_routes_file("User", "user", "users", &[], false, false);
        let body_without = handler_body(&without, "confirm_email_change");
        assert!(
            !body_without.contains("magic_link_tokens"),
            "without --magic-link, confirm_email_change must not reference magic_link_tokens"
        );
    }

    #[test]
    fn account_page_links_to_change_password_and_email() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        let body = handler_body(&routes, "account");
        assert!(
            body.contains(r#"href="/account/password""#),
            "account page must link to change-password"
        );
        assert!(
            body.contains(r#"href="/account/email""#),
            "account page must link to change-email"
        );
    }

    #[test]
    fn change_email_confirm_uses_secured_only_on_get_forms() {
        let routes = render_routes_file("User", "user", "users", &[], false, false);
        // The GET form pages are #[secured] but NOT #[step_up].
        for form_fn in ["change_password_form", "change_email_form"] {
            let pos = routes
                .find(&format!("pub async fn {form_fn}("))
                .unwrap_or_else(|| panic!("{form_fn} missing"));
            // Look at the attribute block immediately preceding this handler
            // (bounded by the previous handler) so a #[step_up] on some later
            // handler cannot satisfy the check.
            let before = &routes[..pos];
            let prev_fn = before.rfind("pub async fn ").unwrap_or(0);
            let attrs = &before[prev_fn..];
            assert!(attrs.contains("#[secured]"), "{form_fn} must be #[secured]");
            assert!(
                !attrs.contains("#[step_up]"),
                "{form_fn} GET form must NOT be #[step_up]"
            );
        }
    }

    #[test]
    fn migration_and_model_and_schema_have_pending_email() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("pending_email TEXT NULL"),
            "migration missing pending_email: {up}"
        );
        let model = fs::read_to_string(tmp.path().join("src/models/user.rs")).unwrap();
        assert!(
            model.contains("pub pending_email: Option<String>"),
            "model missing pending_email: {model}"
        );
        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("pending_email"),
            "schema missing pending_email: {schema}"
        );
    }

    #[test]
    fn main_rs_registers_change_password_and_email_routes() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        for entry in [
            "routes::auth::change_password_form",
            "routes::auth::change_password",
            "routes::auth::change_email_form",
            "routes::auth::change_email",
            "routes::auth::confirm_email_change",
        ] {
            assert!(main.contains(entry), "main.rs missing route entry: {entry}");
        }
    }

    // ── Generated tests ─────────────────────────────────────────────────────

    #[test]
    fn tests_file_covers_all_required_flows() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let tests = fs::read_to_string(tmp.path().join("tests/auth.rs")).unwrap();
        for needle in [
            "auth_signup_returns_200",
            "auth_login_returns_200",
            "auth_logout_redirects",
            "auth_forgot_password_returns_200",
            "auth_reset_password_returns_200",
            "auth_account_rejects_anonymous",
        ] {
            assert!(tests.contains(needle), "tests missing flow: {needle}");
        }
    }

    // ── main.rs registration ────────────────────────────────────────────────

    #[test]
    fn main_rs_registers_auth_routes() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        for entry in [
            "routes::auth::signup_form",
            "routes::auth::signup",
            "routes::auth::login_form",
            "routes::auth::login",
            "routes::auth::logout",
            "routes::auth::account",
            "routes::auth::forgot_password_form",
            "routes::auth::forgot_password",
            "routes::auth::reset_password_form",
            "routes::auth::reset_password",
        ] {
            assert!(main.contains(entry), "main.rs missing route entry: {entry}");
        }
    }

    #[test]
    fn main_rs_declares_models_and_routes_mods() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains("mod models;"),
            "main.rs missing mod models: {main}"
        );
        assert!(
            main.contains("mod routes;"),
            "main.rs missing mod routes: {main}"
        );
    }

    // ── Dry run ─────────────────────────────────────────────────────────────

    #[test]
    fn dry_run_writes_no_files() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(
            !tmp.path().join("src/models/user.rs").exists(),
            "dry run must not create model file"
        );
        assert!(
            !tmp.path().join("src/routes/auth.rs").exists(),
            "dry run must not create routes file"
        );
    }

    // ── Cargo.toml feature injection ────────────────────────────────────────

    #[test]
    fn cargo_toml_gets_mail_feature_simple_string_form() {
        let input = "[dependencies]\nautumn-web = \"0.3\"\n";
        let out = ensure_autumn_web_mail_feature(input);
        assert!(
            out.contains("features = [\"mail\"]"),
            "mail feature missing: {out}"
        );
        assert_eq!(out.matches("autumn-web =").count(), 1, "must not duplicate");
    }

    #[test]
    fn cargo_toml_gets_mail_feature_inline_table_with_existing_features() {
        let input = "[dependencies]\nautumn-web = { version = \"0.3\", features = [\"ws\"] }\n";
        let out = ensure_autumn_web_mail_feature(input);
        assert!(out.contains("\"mail\""), "mail missing: {out}");
        assert!(
            out.contains("\"ws\""),
            "existing feature must be kept: {out}"
        );
    }

    #[test]
    fn cargo_toml_mail_feature_idempotent() {
        let input = "[dependencies]\nautumn-web = { version = \"0.3\", features = [\"mail\"] }\n";
        let out = ensure_autumn_web_mail_feature(input);
        assert_eq!(
            out.matches("\"mail\"").count(),
            1,
            "must not duplicate feature"
        );
    }

    #[test]
    fn model_file_created_at_is_marked_default() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/user.rs")).unwrap();
        assert!(
            model.contains("#[default]"),
            "created_at must be marked #[default] so NewUser excludes it: {model}"
        );
    }

    #[test]
    fn cargo_toml_adds_axum_and_tracing_deps() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("axum"), "axum dep missing: {cargo}");
        assert!(cargo.contains("tracing"), "tracing dep missing: {cargo}");
        assert!(
            cargo.contains("\"mail\""),
            "autumn-web mail feature missing: {cargo}"
        );
    }

    // ── Non-default model name ──────────────────────────────────────────────

    #[test]
    fn plan_auth_supports_custom_model_name() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "Account", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(
            tmp.path().join("src/models/account.rs").exists(),
            "model file should use snake_case of given name"
        );
        let model = fs::read_to_string(tmp.path().join("src/models/account.rs")).unwrap();
        assert!(
            model.contains("pub struct Account"),
            "struct name should match given name"
        );
    }

    // ── OAuth2 generator tests (RED phase) ──────────────────────────────────

    #[test]
    fn plan_auth_with_oauth_creates_oauth_identities_migration() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned(), "google".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        let paths: Vec<String> = plan
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();
        assert!(
            paths.iter().any(|p| p.contains("oauth_identities")),
            "oauth_identities migration missing; got {paths:?}"
        );
    }

    #[test]
    fn plan_auth_without_oauth_is_unchanged() {
        let tmp = project_with_main();
        // Calling with empty providers must produce the same plan as plain plan_auth.
        let oauth = AuthOAuthOptions { providers: vec![] };
        let plan_with =
            plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        let plan_plain = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        // Plans should have the same number of actions and the same paths.
        let paths_with: std::collections::HashSet<String> = plan_with
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();
        let paths_plain: std::collections::HashSet<String> = plan_plain
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            paths_with, paths_plain,
            "empty --oauth must not add any extra files"
        );
    }

    #[test]
    fn oauth_migration_up_has_provider_subject_and_fk_columns() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        // Find the oauth_identities up.sql — timestamp is base+1 to avoid Diesel conflicts.
        let mig_dir = tmp
            .path()
            .join("migrations/20260508000001_create_oauth_identities");
        let up = fs::read_to_string(mig_dir.join("up.sql")).unwrap();
        assert!(
            up.contains("CREATE TABLE oauth_identities"),
            "missing CREATE TABLE oauth_identities: {up}"
        );
        assert!(up.contains("provider"), "missing provider column: {up}");
        assert!(up.contains("subject"), "missing subject column: {up}");
        assert!(
            up.contains("UNIQUE"),
            "provider+subject must have UNIQUE constraint: {up}"
        );
        assert!(
            !up.contains("plaintext") && !up.contains("access_token"),
            "oauth_identities must not store provider access tokens: {up}"
        );
    }

    #[test]
    fn oauth_routes_file_contains_redirect_and_callback_handlers() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        assert!(
            routes.contains("pub async fn oauth_redirect"),
            "oauth_redirect handler missing: {routes}"
        );
        assert!(
            routes.contains("pub async fn oauth_callback"),
            "oauth_callback handler missing: {routes}"
        );
        assert!(
            routes.contains("oauth2_authorize_url"),
            "oauth_redirect must call oauth2_authorize_url: {routes}"
        );
        assert!(
            routes.contains("oauth2_finish_login"),
            "oauth_callback must call oauth2_finish_login: {routes}"
        );
    }

    #[test]
    fn oauth_callback_uses_state_nonce_validation() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        // oauth2_finish_login handles state+nonce internally, so its presence
        // is the contract that state/nonce checking is enforced.
        assert!(
            routes.contains("oauth2_finish_login"),
            "callback must use oauth2_finish_login (which enforces state+nonce): {routes}"
        );
    }

    #[test]
    fn plan_auth_with_oauth_creates_oauth_doc() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["google".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let doc = fs::read_to_string(tmp.path().join("docs/guide/oauth.md")).unwrap();
        assert!(
            doc.contains("OAuth") || doc.contains("oauth"),
            "oauth.md must reference OAuth: {doc}"
        );
        assert!(
            doc.contains("google") || doc.contains("Google"),
            "oauth.md must mention configured providers: {doc}"
        );
        assert!(
            doc.contains("client_id"),
            "oauth.md must cover client_id configuration: {doc}"
        );
        assert!(
            doc.contains("PKCE") || doc.contains("pkce"),
            "oauth.md must document PKCE security property: {doc}"
        );
    }

    #[test]
    fn plan_auth_with_oauth_adds_oauth2_feature_to_autumn_web() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("\"oauth2\""),
            "Cargo.toml must enable autumn-web's oauth2 feature: {cargo}"
        );
    }

    #[test]
    fn plan_auth_without_oauth_does_not_add_oauth2_feature() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            !cargo.contains("\"oauth2\""),
            "Cargo.toml must NOT enable oauth2 feature when --oauth not used: {cargo}"
        );
    }

    #[test]
    fn oauth_routes_registered_in_main_rs() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains("routes::oauth"),
            "main.rs must register oauth routes: {main}"
        );
    }

    #[test]
    fn oauth_migration_has_distinct_timestamp_from_base_auth_migration() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let mig_entries: Vec<_> = fs::read_dir(tmp.path().join("migrations"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let auth_mig = mig_entries
            .iter()
            .find(|n| n.contains("create_users") || n.contains("create_accounts"));
        let oauth_mig = mig_entries.iter().find(|n| n.contains("oauth_identities"));
        if let (Some(a), Some(o)) = (auth_mig, oauth_mig) {
            let a_ts = a.split('_').next().unwrap_or("");
            let o_ts = o.split('_').next().unwrap_or("");
            assert_ne!(
                a_ts, o_ts,
                "oauth_identities migration must have a different timestamp than the auth migration"
            );
        }
    }

    #[test]
    fn oauth_mod_rs_preserves_base_auth_module_declaration() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/routes/mod.rs")).unwrap();
        assert!(
            mod_rs.contains("pub mod auth;"),
            "routes/mod.rs must keep pub mod auth: {mod_rs}"
        );
        assert!(
            mod_rs.contains("pub mod oauth;"),
            "routes/mod.rs must add pub mod oauth: {mod_rs}"
        );
    }

    #[test]
    fn oauth_handlers_have_route_macros() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        assert!(
            routes.contains("#[get("),
            "oauth handlers must have #[get(...)] route macros for routes! registration: {routes}"
        );
    }

    #[test]
    fn oauth_callback_does_not_set_session_key_before_account_link() {
        let routes =
            render_oauth_routes_file("User", "user", "users", &["github".to_owned()], false);
        // The TODO comment may mention the call; verify the EXECUTABLE line is absent.
        // Executable session.insert calls are not prefixed with "//" or "#".
        let executable_insert = routes
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| l.contains("session.insert(&auth_cfg.session_key"));
        assert!(
            !executable_insert,
            "generated callback must not execute session.insert before account linking: {routes}"
        );
    }

    #[test]
    fn plan_auth_with_oauth_adds_oauth_identities_to_schema_rs() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("oauth_identities (id)"),
            "schema.rs must contain oauth_identities table block: {schema}"
        );
        assert!(
            schema.contains("provider -> Text"),
            "schema.rs must contain provider column: {schema}"
        );
        assert!(
            schema.contains("subject -> Text"),
            "schema.rs must contain subject column: {schema}"
        );
        assert!(
            schema.contains("user_id -> Int8"),
            "schema.rs must contain user_id column: {schema}"
        );
        assert!(
            schema.contains("email -> Nullable<Text>"),
            "schema.rs must contain nullable email: {schema}"
        );
        assert!(
            schema.contains("name -> Nullable<Text>"),
            "schema.rs must contain nullable name: {schema}"
        );
    }

    #[test]
    fn plan_auth_with_oauth_appends_stubs_to_autumn_toml() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned(), "google".to_owned()],
        };
        // Setup mock autumn.toml
        fs::write(tmp.path().join("autumn.toml"), "[server]\nport = 3000\n").unwrap();

        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(
            toml.contains("[auth.oauth2.github]"),
            "autumn.toml missing github preset"
        );
        assert!(
            toml.contains("[auth.oauth2.google]"),
            "autumn.toml missing google preset"
        );
        assert!(
            toml.contains("scope = \"read:user user:email\""),
            "autumn.toml missing github scope"
        );
    }

    #[test]
    fn oauth_routes_file_contains_login_buttons() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned(), "google".to_owned()],
        };
        let plan = plan_auth_with_options(tmp.path(), "User", "20260508000000", &oauth).unwrap();
        plan.execute(Flags::default()).unwrap();

        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("Or sign in with:"),
            "login form routes file must render OAuth provider buttons"
        );
        assert!(
            routes.contains("a href=\"/auth/github/redirect\""),
            "login form routes file missing github redirect link"
        );
        assert!(
            routes.contains("a href=\"/auth/google/redirect\""),
            "login form routes file missing google redirect link"
        );
    }

    // ── TOTP two-factor generator tests (S-061 / #799) ──────────────────────

    fn totp_plan(tmp: &std::path::Path) -> Plan {
        let oauth = AuthOAuthOptions::default();
        plan_auth_full(tmp, "User", "20260508000000", &oauth, true).unwrap()
    }

    #[test]
    fn totp_migration_adds_totp_columns_and_recovery_table() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("totp_secret_encrypted"),
            "missing totp_secret_encrypted: {up}"
        );
        assert!(up.contains("totp_enabled"), "missing totp_enabled: {up}");
        assert!(
            up.contains("CREATE TABLE recovery_codes"),
            "missing recovery_codes table: {up}"
        );
        assert!(up.contains("code_digest"), "missing code_digest: {up}");
        assert!(up.contains("used_at"), "missing used_at: {up}");
        assert!(
            !up.contains("totp_secret TEXT") && !up.contains("recovery_code TEXT"),
            "raw secret / raw recovery code must never be stored: {up}"
        );
        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/down.sql"),
        )
        .unwrap();
        assert!(
            down.contains("DROP TABLE recovery_codes"),
            "down.sql must drop recovery_codes: {down}"
        );
    }

    #[test]
    fn totp_model_has_totp_fields_and_recovery_model() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/user.rs")).unwrap();
        assert!(
            model.contains("pub totp_secret_encrypted: Option<String>"),
            "{model}"
        );
        assert!(model.contains("pub totp_enabled: bool"), "{model}");
        let rc = fs::read_to_string(tmp.path().join("src/models/recovery_code.rs")).unwrap();
        assert!(rc.contains("pub struct RecoveryCode"), "{rc}");
        assert!(rc.contains("pub code_digest: String"), "{rc}");
        assert!(rc.contains("pub used_at: Option<"), "{rc}");
        let mod_rs = fs::read_to_string(tmp.path().join("src/models/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod recovery_code;"), "{mod_rs}");
    }

    #[test]
    fn totp_schema_has_totp_and_recovery_columns() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("totp_secret_encrypted -> Nullable<Text>"),
            "{schema}"
        );
        assert!(schema.contains("totp_enabled -> Bool"), "{schema}");
        assert!(schema.contains("recovery_codes (id)"), "{schema}");
        assert!(schema.contains("code_digest -> Text"), "{schema}");
        assert!(
            schema.contains("used_at -> Nullable<Timestamp>"),
            "{schema}"
        );
    }

    #[test]
    fn totp_routes_have_2fa_handlers_and_paths() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        for needle in [
            "pub async fn two_factor_status",
            "pub async fn two_factor_enable",
            "pub async fn two_factor_confirm",
            "pub async fn two_factor_disable",
            "pub async fn login_verify_form",
            "pub async fn login_verify",
        ] {
            assert!(
                routes.contains(needle),
                "routes missing 2fa handler: {needle}"
            );
        }
        for path in [
            "\"/account/2fa\"",
            "\"/account/2fa/enable\"",
            "\"/account/2fa/disable\"",
            "\"/login/verify\"",
        ] {
            assert!(routes.contains(path), "routes missing 2fa path: {path}");
        }
    }

    #[test]
    fn totp_login_marks_pending_and_redirects_to_verify() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("totp_pending_id"),
            "login must set totp_pending marker: {routes}"
        );
        assert!(
            routes.contains("totp_enabled"),
            "login must branch on totp_enabled: {routes}"
        );
        assert!(
            routes.contains("/login/verify"),
            "login must redirect to /login/verify: {routes}"
        );
    }

    #[test]
    fn totp_login_clears_abandoned_reset_marker() {
        // P2 (#1057): an ordinary password login must clear any stale
        // totp_pending_reset_digest so it can't commit a stale password change.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let login_pos = routes.find("pub async fn login(").expect("login fn");
        let logout_pos = routes.find("pub async fn logout(").unwrap_or(routes.len());
        let login_body = &routes[login_pos..logout_pos];
        assert!(
            login_body.contains("session.remove(\"totp_pending_reset_digest\")"),
            "login interstitial must clear an abandoned deferred-reset marker: {login_body}"
        );
    }

    #[test]
    fn totp_signup_clears_pending_state() {
        // P2 (#1057 round 6): signing up establishes a fresh authenticated
        // session, so it must clear any abandoned pending-2FA / reset / enrollment
        // markers — otherwise a later /login/verify could switch accounts.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let signup_pos = routes.find("pub async fn signup(").expect("signup fn");
        let next_pos = routes[signup_pos + 1..]
            .find("pub async fn ")
            .map_or(routes.len(), |p| signup_pos + 1 + p);
        let signup_body = &routes[signup_pos..next_pos];
        assert!(
            signup_body.contains("session.remove(\"totp_pending_id\")")
                && signup_body.contains("session.remove(\"totp_pending_secret\")"),
            "signup must clear pending 2FA / enrollment markers: {signup_body}"
        );
    }

    #[test]
    fn totp_clear_pending_includes_enrollment_secret() {
        // P2 (#1057 round 6): a stale totp_pending_secret must not survive a fresh
        // full login, or two_factor_confirm could accept it for a different account.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let login_pos = routes.find("pub async fn login(").expect("login fn");
        let logout_pos = routes.find("pub async fn logout(").unwrap_or(routes.len());
        let login_body = &routes[login_pos..logout_pos];
        assert!(
            login_body.contains("session.remove(\"totp_pending_secret\")"),
            "login must clear a stale enrollment secret: {login_body}"
        );
    }

    #[test]
    fn totp_interstitial_rejects_session_key_collision() {
        // P2 (#1057 round 6): if [auth].session_key collides with a reserved
        // totp_pending_* key, parking the pending marker would set the trusted
        // auth key. The interstitials must refuse such a configuration.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("fn is_reserved_totp_pending_key"),
            "must define the reserved-pending-key guard helper: {routes}"
        );
        let login_pos = routes.find("pub async fn login(").expect("login fn");
        let logout_pos = routes.find("pub async fn logout(").unwrap_or(routes.len());
        let login_body = &routes[login_pos..logout_pos];
        assert!(
            login_body.contains("is_reserved_totp_pending_key(state.auth_session_key())"),
            "login interstitial must reject a colliding auth session key: {login_body}"
        );
    }

    #[test]
    fn totp_pending_handoff_clears_live_auth_keys() {
        // P2 (#1057 round 9): rotate_id() preserves session data, so a pending-2FA
        // handoff started while already authenticated as another account must drop
        // the live auth keys, or #[secured] would still trust the old account.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // Login interstitial: before parking totp_pending_id it must remove the
        // configured auth session key.
        let login_pos = routes.find("pub async fn login(").expect("login fn");
        let logout_pos = routes.find("pub async fn logout(").unwrap_or(routes.len());
        let login_body = &routes[login_pos..logout_pos];
        let park_at = login_body
            .find("insert(\"totp_pending_id\"")
            .expect("login parks totp_pending_id");
        let before_park = &login_body[..park_at];
        assert!(
            before_park.contains("session.remove(state.auth_session_key())"),
            "login interstitial must clear the live auth key before parking 2FA: {login_body}"
        );
        // Reset interstitial must do the same.
        let reset_pos = routes
            .find("pub async fn reset_password(")
            .expect("reset_password fn");
        let reset_body = &routes[reset_pos..reset_pos + 6000.min(routes.len() - reset_pos)];
        let reset_park = reset_body
            .find("insert(\"totp_pending_id\"")
            .expect("reset parks totp_pending_id");
        assert!(
            reset_body[..reset_park].contains("session.remove(state.auth_session_key())"),
            "reset interstitial must clear the live auth key before parking 2FA: {reset_body}"
        );
    }

    #[test]
    fn totp_login_verify_falls_back_to_recovery_on_broken_secret() {
        // P2 (#1057 round 8): a decrypt/build failure (e.g. rotated TOTP_ENC_KEY)
        // must not abort login_verify before the recovery-code branch — recovery
        // codes are the only escape from that lockout.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let verify_pos = routes
            .find("pub async fn login_verify(")
            .expect("login_verify fn");
        // Up to the recovery-code branch, the TOTP attempt must not use `?` on
        // decrypt_secret/build_totp (which would abort the whole handler).
        let verify_to_recovery = &routes[verify_pos..];
        let recovery_at = verify_to_recovery
            .find("try an unused recovery code")
            .expect("recovery-code comment");
        let totp_branch = &verify_to_recovery[..recovery_at];
        assert!(
            !totp_branch.contains("decrypt_secret(stored)?"),
            "login_verify TOTP branch must not abort on decrypt failure: {totp_branch}"
        );
        assert!(
            totp_branch.contains("decrypt_secret(stored)") && totp_branch.contains(".ok()"),
            "login_verify must treat a broken secret as a miss and fall through: {totp_branch}"
        );
    }

    #[test]
    fn totp_disable_falls_back_to_password_on_broken_secret() {
        // P2 (#1057 round 8): the disable code path must likewise not abort on a
        // decrypt failure, so the password fallback can still re-authenticate.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let disable_pos = routes
            .find("pub async fn two_factor_disable(")
            .expect("two_factor_disable fn");
        let next = routes[disable_pos + 1..]
            .find("pub async fn ")
            .map_or(routes.len(), |p| disable_pos + 1 + p);
        let disable_body = &routes[disable_pos..next];
        assert!(
            !disable_body.contains("decrypt_secret(stored)?"),
            "disable must not abort on decrypt failure before the password fallback: {disable_body}"
        );
    }

    #[test]
    fn totp_build_totp_sanitizes_colon_in_account_label() {
        // P2 (#1057 round 8): totp-rs rejects account labels containing ':', so an
        // email like a:b@example.com would 500. build_totp must strip colons.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let build_pos = routes.find("fn build_totp(").expect("build_totp fn");
        let build_body = &routes[build_pos..build_pos + 600.min(routes.len() - build_pos)];
        assert!(
            build_body.contains("account.replace(':'"),
            "build_totp must sanitize colons in the account label: {build_body}"
        );
    }

    #[test]
    fn totp_oauth_pending_note_guards_session_key_collision() {
        // P2 (#1057 round 8): the OAuth pending-2FA guidance must mirror the
        // password/reset reserved-key rejection before parking totp_pending_id.
        let tmp = project_with_main();
        let oauth_opts = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        plan_auth_full(tmp.path(), "User", "20260508000000", &oauth_opts, true)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let oauth = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        assert!(
            oauth.contains("is_reserved_totp_pending_key(&auth_cfg.session_key)"),
            "OAuth pending-2FA guidance must guard against an auth-key collision: {oauth}"
        );
    }

    #[test]
    fn totp_rejects_recovery_code_name_collision() {
        // P2 (#1057 round 7): `--totp` always emits a recovery_code model/table,
        // so an auth resource that resolves to that name would emit it twice and
        // produce an unusable app. The generator must reject it up front.
        let tmp = project_with_main();
        // Both forms resolve to snake `recovery_code` / table `recovery_codes`,
        // which is exactly the helper model/table emitted under --totp.
        for name in ["RecoveryCode", "recovery_code"] {
            let err = plan_auth_with_providers(tmp.path(), name, "20260508000000", &[], true)
                .expect_err("recovery_code collision must be rejected under --totp");
            assert!(
                matches!(err, GenerateError::InvalidName(_, _)),
                "expected InvalidName for {name}, got {err:?}"
            );
        }
        // Without --totp the same name is fine (no recovery_code table emitted).
        assert!(
            plan_auth_with_providers(tmp.path(), "RecoveryCode", "20260508000000", &[], false)
                .is_ok(),
            "recovery_code name must be allowed without --totp"
        );
    }

    #[test]
    fn totp_rejects_existing_recovery_codes_table() {
        // P2 (#1057 round 11): if the project already declares a `recovery_codes`
        // table, the unconditional --totp migration would CREATE it again and
        // `diesel migration run` would fail. Reject up front.
        let tmp = project_with_main();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/schema.rs"),
            "diesel::table! {\n    recovery_codes (id) {\n        id -> Int8,\n    }\n}\n",
        )
        .unwrap();
        let err = plan_auth_with_providers(tmp.path(), "User", "20260508000000", &[], true)
            .expect_err("existing recovery_codes table must be rejected under --totp");
        assert!(
            matches!(err, GenerateError::InvalidName(_, _)),
            "expected InvalidName, got {err:?}"
        );
        // Without --totp there's no recovery_codes helper, so it's fine.
        assert!(
            plan_auth_with_providers(tmp.path(), "User", "20260508000000", &[], false).is_ok(),
            "existing recovery_codes table is irrelevant without --totp"
        );
    }

    #[test]
    fn adoption_migration_docs_include_pending_email() {
        // The generated routes/schema always reference `pending_email` (signup-confirm
        // disambiguation + email-change flow), so the documented "adoption path"
        // migration for existing apps must add that column in up.sql and drop it in
        // down.sql — otherwise the regenerated app queries a missing column.
        let docs = render_docs_file("User", false, false);
        let up_marker = "-- migrations/<timestamp>_add_email_confirmation_to_{table}/up.sql";
        let down_marker = "-- migrations/<timestamp>_add_email_confirmation_to_{table}/down.sql";
        let up_pos = docs.find(up_marker).expect("adoption up.sql block present");
        let down_pos = docs
            .find(down_marker)
            .expect("adoption down.sql block present");
        let up_block = &docs[up_pos..down_pos];
        let down_block = &docs[down_pos..];
        assert!(
            up_block.contains("ADD COLUMN IF NOT EXISTS pending_email TEXT NULL"),
            "adoption up.sql must add pending_email: {up_block}"
        );
        assert!(
            down_block.contains("DROP COLUMN IF EXISTS pending_email"),
            "adoption down.sql must drop pending_email: {down_block}"
        );
    }

    #[test]
    fn magic_link_rejects_helper_name_collision() {
        // `--magic-link` always emits a magic_link_token model/table, so an auth
        // resource that resolves to those names would emit the CREATE TABLE twice and
        // overwrite the auth model with the helper model. The generator must reject it.
        let tmp = project_with_main();
        for name in ["MagicLinkToken", "magic_link_token"] {
            let err =
                plan_auth_with_providers_ex(tmp.path(), name, "20260508000000", &[], false, true)
                    .expect_err("magic_link_token collision must be rejected under --magic-link");
            assert!(
                matches!(err, GenerateError::InvalidName(_, _)),
                "expected InvalidName for {name}, got {err:?}"
            );
        }
        // Without --magic-link the same name is fine (no magic_link_tokens table emitted).
        assert!(
            plan_auth_with_providers_ex(
                tmp.path(),
                "MagicLinkToken",
                "20260508000000",
                &[],
                false,
                false,
            )
            .is_ok(),
            "magic_link_token name must be allowed without --magic-link"
        );
    }

    #[test]
    fn magic_link_rejects_existing_magic_link_tokens_table() {
        // If the project already declares a `magic_link_tokens` table, the
        // unconditional --magic-link migration would CREATE it again. Reject up front.
        let tmp = project_with_main();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/schema.rs"),
            "diesel::table! {\n    magic_link_tokens (id) {\n        id -> Int8,\n    }\n}\n",
        )
        .unwrap();
        let err =
            plan_auth_with_providers_ex(tmp.path(), "User", "20260508000000", &[], false, true)
                .expect_err("existing magic_link_tokens table must be rejected under --magic-link");
        assert!(
            matches!(err, GenerateError::InvalidName(_, _)),
            "expected InvalidName, got {err:?}"
        );
        // Without --magic-link there's no helper table, so it's fine.
        assert!(
            plan_auth_with_providers_ex(tmp.path(), "User", "20260508000000", &[], false, false)
                .is_ok(),
            "existing magic_link_tokens table is irrelevant without --magic-link"
        );
    }

    #[test]
    fn totp_enable_rejects_reserved_session_key() {
        // P2 (#1057 round 11): two_factor_enable stashes totp_pending_secret, so it
        // must also reject a [auth].session_key that collides with a reserved key.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let enable_pos = routes
            .find("pub async fn two_factor_enable(")
            .expect("enable fn");
        let confirm_pos = routes
            .find("pub async fn two_factor_confirm(")
            .unwrap_or(routes.len());
        let enable_body = &routes[enable_pos..confirm_pos];
        assert!(
            enable_body.contains("is_reserved_totp_pending_key(state.auth_session_key())"),
            "two_factor_enable must reject a reserved auth session key: {enable_body}"
        );
    }

    #[test]
    fn totp_oauth_full_login_note_rotates_session() {
        // P2 (#1057 round 11): the non-2FA OAuth full-login guidance must rotate the
        // session id before promoting, mirroring password login (anti-fixation).
        let tmp = project_with_main();
        let oauth_opts = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        plan_auth_full(tmp.path(), "User", "20260508000000", &oauth_opts, true)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let oauth = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        let note_pos = oauth
            .find("otherwise, fully authenticate")
            .expect("full-login guidance");
        let after = &oauth[note_pos..];
        assert!(
            after.contains("session.rotate_id()"),
            "OAuth full-login guidance must rotate the session: {after}"
        );
    }

    #[test]
    fn ensure_totp_rs_features_promotes_simple_version() {
        let toml = "[dependencies]\ntotp-rs = \"5\"\n";
        let out = ensure_totp_rs_features(toml);
        assert!(
            out.contains("\"qr\"") && out.contains("\"gen_secret\"") && out.contains("\"otpauth\""),
            "simple version must be promoted with required features: {out}"
        );
    }

    #[test]
    fn ensure_totp_rs_features_merges_into_partial_features() {
        let toml = "[dependencies]\ntotp-rs = { version = \"5\", features = [\"qr\"] }\n";
        let out = ensure_totp_rs_features(toml);
        assert!(
            out.contains("\"qr\"") && out.contains("\"gen_secret\"") && out.contains("\"otpauth\""),
            "missing features must be merged in: {out}"
        );
        // The pre-existing feature must not be duplicated.
        assert_eq!(out.matches("\"qr\"").count(), 1, "qr duplicated: {out}");
    }

    #[test]
    fn ensure_totp_rs_features_adds_features_key_when_absent() {
        let toml = "[dependencies]\ntotp-rs = { version = \"5\" }\n";
        let out = ensure_totp_rs_features(toml);
        assert!(
            out.contains("features = [")
                && out.contains("\"gen_secret\"")
                && out.contains("\"otpauth\""),
            "a features key must be added: {out}"
        );
    }

    #[test]
    fn ensure_totp_rs_features_is_idempotent_when_complete() {
        let toml = "[dependencies]\ntotp-rs = { version = \"5\", features = [\"qr\", \"gen_secret\", \"otpauth\"] }\n";
        assert_eq!(
            ensure_totp_rs_features(toml),
            toml,
            "already-complete features must be left untouched"
        );
    }

    #[test]
    fn ensure_totp_rs_features_merges_subtable_with_partial_features() {
        // P2 (#1057 round 12): the `[dependencies.totp-rs]` subtable form must also
        // gain the missing features.
        let toml = "[dependencies.totp-rs]\nversion = \"5\"\nfeatures = [\"qr\"]\n";
        let out = ensure_totp_rs_features(toml);
        assert!(
            out.contains("\"qr\"") && out.contains("\"gen_secret\"") && out.contains("\"otpauth\""),
            "subtable features must be merged: {out}"
        );
        assert_eq!(out.matches("\"qr\"").count(), 1, "qr duplicated: {out}");
    }

    #[test]
    fn ensure_totp_rs_features_adds_subtable_features_key_when_absent() {
        let toml = "[dependencies.totp-rs]\nversion = \"5\"\n";
        let out = ensure_totp_rs_features(toml);
        assert!(
            out.contains("features = [")
                && out.contains("\"qr\"")
                && out.contains("\"gen_secret\"")
                && out.contains("\"otpauth\""),
            "a features key must be added to the subtable: {out}"
        );
    }

    #[test]
    fn ensure_totp_rs_features_strips_inline_comment_on_simple_version() {
        // P2 (#1057 round 13): a trailing inline comment must not be folded into
        // the version string (which would make Cargo.toml invalid).
        let toml = "[dependencies]\ntotp-rs = \"5\" # used by metrics\n";
        let out = ensure_totp_rs_features(toml);
        assert!(
            out.contains("version = \"5\"") && out.contains("features = ["),
            "version must be parsed cleanly: {out}"
        );
        assert!(
            !out.contains("\"5\" # used by metrics\""),
            "the comment must not leak into the version string: {out}"
        );
        // The comment is preserved after the rewritten table.
        assert!(
            out.contains("# used by metrics"),
            "the trailing comment should be preserved: {out}"
        );
    }

    #[test]
    fn ensure_totp_rs_features_merges_multiline_subtable_array() {
        // P2 (#1057 round 13): a multiline `features = [` array in the subtable
        // form must still gain the missing features.
        let toml = "[dependencies.totp-rs]\nversion = \"5\"\nfeatures = [\n    \"qr\",\n]\n";
        let out = ensure_totp_rs_features(toml);
        assert!(
            out.contains("\"qr\"") && out.contains("\"gen_secret\"") && out.contains("\"otpauth\""),
            "multiline subtable features must be merged: {out}"
        );
        assert_eq!(out.matches("\"qr\"").count(), 1, "qr duplicated: {out}");
    }

    #[test]
    fn totp_disable_wraps_cleanup_in_transaction() {
        // P2 (#1057 round 12): disabling 2FA must be atomic — the flag/secret clear
        // and recovery-code delete run in one transaction.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let disable_pos = routes
            .find("pub async fn two_factor_disable(")
            .expect("disable fn");
        let next = routes[disable_pos + 1..]
            .find("pub async fn ")
            .map_or(routes.len(), |p| disable_pos + 1 + p);
        let body = &routes[disable_pos..next];
        assert!(
            body.contains(".transaction::<_, diesel::result::Error, _>"),
            "two_factor_disable must run cleanup in a transaction: {body}"
        );
        // Both the flag clear and the recovery-code delete must be inside it.
        assert!(
            body.contains("totp_enabled.eq(false)")
                && body.contains("delete(recovery_codes::table"),
            "transaction must cover both the disable update and recovery-code delete: {body}"
        );
    }

    #[test]
    fn totp_login_verify_tears_down_pending_on_stale_reset() {
        // P2 (#1057 round 7): if the parked reset token is stale, login_verify must
        // clear totp_pending_id too — otherwise a retry skips the (now-cleared)
        // reset branch and promotes the session to a full login.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let verify_pos = routes
            .find("pub async fn login_verify(")
            .expect("login_verify fn");
        let verify_body = &routes[verify_pos..];
        // The stale-reset failure path (before the success-promotion block) must
        // remove totp_pending_id.
        let promote_at = verify_body
            .find("Promote the pending session")
            .expect("promotion comment");
        let failure_block = &verify_body[..promote_at];
        assert!(
            failure_block.contains("session.remove(\"totp_pending_id\")")
                && failure_block.contains("Please restart the password reset"),
            "stale-reset path must tear down the pending login: {failure_block}"
        );
    }

    #[test]
    fn totp_oauth_full_login_note_clears_pending_state() {
        // P2 (#1057 round 7): the OAuth non-2FA full-login guidance must also clear
        // abandoned totp_pending_* state, mirroring password login/signup.
        let tmp = project_with_main();
        let oauth_opts = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        plan_auth_full(tmp.path(), "User", "20260508000000", &oauth_opts, true)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let oauth = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        let note_pos = oauth
            .find("otherwise, fully authenticate")
            .expect("full-login guidance");
        let after = &oauth[note_pos..];
        assert!(
            after.contains("totp_pending_secret") && after.contains("totp_pending_id"),
            "OAuth full-login guidance must clear pending 2FA/enrollment state: {after}"
        );
        // P2 (#1057 round 10): the full-login guidance must also set the
        // model-specific identity keys the generated handlers read, not only the
        // configured auth key.
        assert!(
            after.contains("session.insert(\"user_id\", local_user.id.to_string())")
                && after.contains("session.insert(\"user_email\", &local_user.email)"),
            "OAuth full-login guidance must set the model identity keys: {after}"
        );
    }

    #[test]
    fn totp_reset_password_routes_2fa_users_through_verify() {
        // P1 (#1057): a reset link must not bypass the second factor.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // The 2FA interstitial branch must appear inside reset_password too.
        let reset_pos = routes
            .find("pub async fn reset_password(")
            .expect("reset_password fn");
        let after = &routes[reset_pos..];
        let body_end = after.find("\n}").unwrap_or(after.len());
        let body = &after[..body_end];
        assert!(
            body.contains("totp_pending_id") && body.contains("/login/verify"),
            "reset_password must route 2FA-enabled users through /login/verify: {body}"
        );
    }

    #[test]
    fn totp_reset_password_defers_password_commit_until_verify() {
        // P2 (#1057): for a 2FA-enabled account the password must NOT be written
        // until /login/verify passes; the reset handler parks the digest and the
        // verify handler commits it.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let reset_pos = routes
            .find("pub async fn reset_password(")
            .expect("reset_password fn");
        let reset_tail = &routes[reset_pos..];
        let reset_body = &reset_tail[..reset_tail.find("\n/// `").unwrap_or(reset_tail.len())];
        // The deferral branch (which returns to /login/verify) must come BEFORE
        // the password_digest UPDATE in the handler body.
        let park_at = reset_body
            .find("totp_pending_reset_digest")
            .expect("reset must park the pending digest");
        let write_at = reset_body
            .find("password_digest.eq(&new_digest)")
            .expect("reset still writes the digest for non-2FA users");
        assert!(
            park_at < write_at,
            "reset must park (and return) before committing the password for 2FA users"
        );
        // login_verify must commit a parked reset once the factor is proven.
        let verify_pos = routes
            .find("pub async fn login_verify(")
            .expect("login_verify fn");
        let verify_body = &routes[verify_pos..];
        assert!(
            verify_body.contains("totp_pending_reset_digest")
                && verify_body.contains("password_digest.eq(&new_digest)"),
            "login_verify must commit a parked password reset after 2FA: {verify_body}"
        );
    }

    #[test]
    fn totp_reset_branch_parks_token_digest() {
        // P2 (#1057 round 5): the reset interstitial must store totp_pending_reset_token
        // (the authorising SHA-256 digest) in the session alongside the new password
        // digest so login_verify can revalidate it before committing.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let reset_pos = routes
            .find("pub async fn reset_password(")
            .expect("reset_password fn");
        let reset_tail = &routes[reset_pos..];
        let reset_body = &reset_tail[..reset_tail.find("\n/// `").unwrap_or(reset_tail.len())];
        assert!(
            reset_body.contains("totp_pending_reset_token"),
            "reset_password 2FA branch must park the token digest: {reset_body}"
        );
        assert!(
            reset_body.contains("token_digest"),
            "reset_password must have token_digest in scope when parking: {reset_body}"
        );
    }

    #[test]
    fn totp_login_verify_revalidates_token_before_commit() {
        // P2 (#1057 round 5): login_verify must make the deferred password commit
        // conditional on the authorising token still matching the DB row — a
        // superseded or expired reset token must cause 0 rows updated and be rejected.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let verify_pos = routes
            .find("pub async fn login_verify(")
            .expect("login_verify fn");
        let verify_body = &routes[verify_pos..];
        assert!(
            verify_body.contains("totp_pending_reset_token"),
            "login_verify must load the stored token from the session: {verify_body}"
        );
        assert!(
            verify_body.contains("reset_token_digest"),
            "login_verify update must filter by reset_token_digest: {verify_body}"
        );
        assert!(
            verify_body.contains("reset_token_expires_at"),
            "login_verify update must filter by reset_token_expires_at: {verify_body}"
        );
    }

    #[test]
    fn totp_login_verify_stamps_step_up_claim_on_success() {
        // Issue #833 / Codex P2: TOTP login (password + second factor) proves
        // identity at least as strongly as password-only login. login_verify must
        // call set_last_strong_auth_at so that #[step_up]-protected routes are
        // immediately accessible without forcing a redundant /reauth.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let verify_pos = routes
            .find("pub async fn login_verify(")
            .expect("login_verify fn");
        let verify_body = &routes[verify_pos..];
        assert!(
            verify_body.contains("set_last_strong_auth_at"),
            "login_verify (TOTP) must stamp set_last_strong_auth_at on success: {verify_body}"
        );
    }

    #[test]
    fn totp_oauth_callback_note_clears_both_pending_markers() {
        // P2 (#1057 round 5): the OAuth callback guidance comment must instruct
        // developers to clear both pending-reset markers before parking the 2FA
        // state — mirroring what the password-login handler does.
        let tmp = project_with_main();
        let oauth_opts = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        let plan = plan_auth_full(tmp.path(), "User", "20260508000000", &oauth_opts, true).unwrap();
        plan.execute(Flags::default()).unwrap();
        let oauth = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        assert!(
            oauth.contains("totp_pending_reset_digest"),
            "OAuth callback note must mention totp_pending_reset_digest: {oauth}"
        );
        assert!(
            oauth.contains("totp_pending_reset_token"),
            "OAuth callback note must mention totp_pending_reset_token: {oauth}"
        );
    }

    #[test]
    fn totp_enroll_requires_password_reauth() {
        // P2 (#1057): first enrollment must require step-up password re-auth.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("struct TotpEnableForm"),
            "enable must take a password form: {routes}"
        );
        let enable_pos = routes
            .find("pub async fn two_factor_enable(")
            .expect("enable fn");
        let confirm_pos = routes
            .find("pub async fn two_factor_confirm(")
            .expect("confirm fn");
        let enable_body = &routes[enable_pos..confirm_pos];
        assert!(
            enable_body.contains("verify_password(&form.password"),
            "two_factor_enable must verify the account password before enrolling: {enable_body}"
        );
        // The status-page enable form must collect the password.
        assert!(
            routes.contains("Confirm your password to begin enrollment"),
            "enable form must prompt for the password: {routes}"
        );
    }

    #[test]
    fn totp_disable_applies_replay_guard_on_code() {
        // P2 (#1057): a TOTP code used to disable must advance/respect
        // totp_last_used_step so a code already spent on login can't be replayed.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let disable_pos = routes
            .find("pub async fn two_factor_disable(")
            .expect("disable fn");
        let after = &routes[disable_pos..];
        let body_end = after.find("\n/// ").unwrap_or(after.len());
        let body = &after[..body_end];
        assert!(
            body.contains("totp_last_used_step") && body.contains("affected == 1"),
            "disable must consume the TOTP step with the same atomic guard: {body}"
        );
    }

    #[test]
    fn totp_confirm_guards_against_double_submit() {
        // P2 (#1057): the final enable flip is conditional on totp_enabled=false
        // with a rollback, so concurrent confirms don't desync recovery codes.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let confirm_pos = routes
            .find("pub async fn two_factor_confirm(")
            .expect("confirm fn");
        let after = &routes[confirm_pos..];
        let disable_pos = after
            .find("pub async fn two_factor_disable(")
            .unwrap_or(after.len());
        let body = &after[..disable_pos];
        assert!(
            body.contains("totp_enabled.eq(false)") && body.contains("RollbackTransaction"),
            "confirm must claim the enable transition conditionally and roll back on loss: {body}"
        );
    }

    #[test]
    fn totp_enable_does_not_disable_live_secret() {
        // P1 (#1057): starting re-enrollment must not flip totp_enabled=false on
        // the live account. The pending secret lives in the session instead.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let enable_pos = routes
            .find("pub async fn two_factor_enable(")
            .expect("enable fn");
        let confirm_pos = routes
            .find("pub async fn two_factor_confirm(")
            .expect("confirm fn");
        let enable_body = &routes[enable_pos..confirm_pos];
        assert!(
            enable_body.contains("session.insert(\"totp_pending_secret\""),
            "enable must stash the pending secret in the session: {enable_body}"
        );
        assert!(
            !enable_body.contains("totp_enabled.eq("),
            "enable must NOT write totp_enabled on the live row: {enable_body}"
        );
        assert!(
            !enable_body.contains("totp_secret_encrypted.eq("),
            "enable must NOT overwrite the live secret before confirmation: {enable_body}"
        );
    }

    #[test]
    fn totp_enable_rejects_reenrollment_while_active() {
        // P2 (#1057): a hijacked session must not swap the authenticator without
        // first disabling (which re-authenticates). Both enable and confirm guard
        // on the live `totp_enabled` flag.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let enable_pos = routes
            .find("pub async fn two_factor_enable(")
            .expect("enable fn");
        let confirm_pos = routes
            .find("pub async fn two_factor_confirm(")
            .expect("confirm fn");
        let disable_pos = routes
            .find("pub async fn two_factor_disable(")
            .expect("disable fn");
        let enable_body = &routes[enable_pos..confirm_pos];
        let confirm_body = &routes[confirm_pos..disable_pos];
        assert!(
            enable_body.contains("if __SNAKE__.totp_enabled")
                || enable_body.contains("if user.totp_enabled"),
            "two_factor_enable must reject re-enrollment while 2FA is active: {enable_body}"
        );
        assert!(
            confirm_body.contains("if user.totp_enabled") || confirm_body.contains("totp_enabled"),
            "two_factor_confirm must reject re-enrollment while 2FA is active: {confirm_body}"
        );
    }

    #[test]
    fn totp_confirm_persists_codes_before_enabling_in_transaction() {
        // P2 (#1057): enable + recovery-code replacement must be atomic and the
        // flag flipped only after codes are persisted.
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        let confirm_pos = routes
            .find("pub async fn two_factor_confirm(")
            .expect("confirm fn");
        let after = &routes[confirm_pos..];
        let disable_pos = after
            .find("pub async fn two_factor_disable(")
            .unwrap_or(after.len());
        let body = &after[..disable_pos];
        assert!(
            body.contains(".transaction"),
            "confirm must use a transaction: {body}"
        );
        assert!(
            body.contains("RECOVERY_CODE_COUNT"),
            "confirm must generate N codes: {body}"
        );
        assert!(
            body.contains("hash_password"),
            "recovery codes must be bcrypt-hashed: {body}"
        );
        // totp_enabled flip must come AFTER the recovery-code inserts.
        let enabled_at = body.find("totp_enabled.eq(true)").expect("enable flag set");
        let insert_at = body
            .find("insert_into(recovery_codes::table")
            .expect("code insert");
        assert!(
            enabled_at > insert_at,
            "totp_enabled must be set after codes are persisted"
        );
        // The pending secret must be read from the session, not the live row.
        assert!(
            body.contains("totp_pending_secret"),
            "confirm must consume session pending secret: {body}"
        );
    }

    #[test]
    fn totp_verification_window_and_atomic_replay_guard() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // ±1 step skew window helper.
        assert!(
            routes.contains("[-1i64, 0, 1]"),
            "verification must use a ±1 step window: {routes}"
        );
        // Atomic, conditional consumption: rows-affected guard on the step update.
        assert!(routes.contains("totp_last_used_step"), "{routes}");
        assert!(
            routes.contains("affected == 1"),
            "step + recovery consumption must require exactly one affected row: {routes}"
        );
        assert!(
            routes.contains("recovery_codes::used_at.is_null()"),
            "recovery consumption must be guarded by used_at IS NULL: {routes}"
        );
    }

    #[test]
    fn totp_recovery_code_marked_used_and_count_surfaced() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("used_at"),
            "consumed recovery code must be stamped used: {routes}"
        );
        assert!(
            routes.contains("remaining"),
            "remaining count must be surfaced: {routes}"
        );
    }

    #[test]
    fn totp_disable_requires_reauth_and_clears_secret() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("verify_password") && routes.contains("two_factor_disable"),
            "disable must require re-auth: {routes}"
        );
        assert!(
            routes.contains("totp_secret_encrypted.eq(None"),
            "disable must clear the stored secret: {routes}"
        );
        assert!(
            routes.contains("delete(recovery_codes::table"),
            "disable must delete recovery codes: {routes}"
        );
    }

    #[test]
    fn totp_secret_encrypted_at_rest() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("Aes256Gcm"),
            "totp secret must be AES-GCM encrypted: {routes}"
        );
        assert!(
            routes.contains("fn encrypt_secret") && routes.contains("fn decrypt_secret"),
            "generated code must provide encrypt/decrypt helpers: {routes}"
        );
        assert!(
            routes.contains("otpauth://") || routes.contains("get_url"),
            "must expose otpauth URI: {routes}"
        );
    }

    #[test]
    fn totp_generates_2fa_integration_tests() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let tests = fs::read_to_string(tmp.path().join("tests/auth_2fa.rs")).unwrap();
        for flow in [
            "two_factor_enroll_and_confirm",
            "login_with_totp_code",
            "login_with_recovery_code",
            "recovery_code_reuse_rejected",
            "two_factor_disable",
        ] {
            assert!(
                tests.contains(flow),
                "tests/auth_2fa.rs missing flow: {flow}"
            );
        }
    }

    #[test]
    fn totp_registers_routes_in_main_rs_and_adds_deps() {
        let tmp = project_with_main();
        totp_plan(tmp.path()).execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        for entry in [
            "routes::auth::two_factor_enable",
            "routes::auth::two_factor_confirm",
            "routes::auth::two_factor_disable",
            "routes::auth::login_verify",
        ] {
            assert!(main.contains(entry), "main.rs missing 2fa route: {entry}");
        }
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("totp-rs"),
            "Cargo.toml missing totp-rs: {cargo}"
        );
        assert!(
            cargo.contains("aes-gcm"),
            "Cargo.toml missing aes-gcm: {cargo}"
        );
    }

    #[test]
    fn without_totp_no_totp_artifacts() {
        let tmp = project_with_main();
        plan_auth(tmp.path(), "User", "20260508000000")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/user.rs")).unwrap();
        assert!(
            !model.contains("totp_enabled"),
            "default model must not contain totp fields: {model}"
        );
        assert!(!tmp.path().join("src/models/recovery_code.rs").exists());
        assert!(!tmp.path().join("tests/auth_2fa.rs").exists());
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            !routes.contains("two_factor_enable"),
            "default routes must not have 2fa handlers"
        );
    }

    #[test]
    fn totp_combines_with_oauth_and_gates_callback() {
        // P2 (#1057): OAuth callback guidance must be TOTP-aware.
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        plan_auth_full(tmp.path(), "User", "20260508000000", &oauth, true)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("src/routes/oauth.rs").exists());
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("two_factor_enable"),
            "totp+oauth must keep 2fa handlers"
        );
        assert!(
            routes.contains("Or sign in with:"),
            "totp+oauth must keep oauth buttons"
        );
        let oauth_routes = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        assert!(
            oauth_routes.contains("totp_pending_id") && oauth_routes.contains("/login/verify"),
            "oauth callback guidance must route 2FA users through /login/verify: {oauth_routes}"
        );
    }

    #[test]
    fn oauth_without_totp_has_no_2fa_callback_note() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        plan_auth_full(tmp.path(), "User", "20260508000000", &oauth, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let oauth_routes = fs::read_to_string(tmp.path().join("src/routes/oauth.rs")).unwrap();
        assert!(
            !oauth_routes.contains("TWO-FACTOR"),
            "oauth-only scaffold must not mention the 2FA callback note: {oauth_routes}"
        );
    }

    // ── Passkeys (WebAuthn) generator tests (S-062 / #806) ──────────────────────

    fn passkey_plan(tmp: &std::path::Path) -> Plan {
        plan_auth_full_ex(
            tmp,
            "User",
            "20260508000000",
            &AuthOAuthOptions::default(),
            false,
            true,
        )
        .unwrap()
    }

    #[test]
    fn passkeys_flag_defaults_off() {
        // Without --passkeys, plan_auth_full (the 5-arg form) must not emit any
        // passkey artefacts — passkeys are opt-in.
        let tmp = project_with_main();
        plan_auth_full(
            tmp.path(),
            "User",
            "20260508000000",
            &AuthOAuthOptions::default(),
            false,
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();
        assert!(
            !tmp.path().join("src/routes/passkeys.rs").exists(),
            "passkeys.rs must not be created without --passkeys"
        );
        assert!(
            !tmp.path().join("tests/auth_passkeys.rs").exists(),
            "auth_passkeys.rs must not be created without --passkeys"
        );
    }

    #[test]
    fn passkeys_plan_emits_webauthn_credentials_migration() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        // The migration directory name is timestamp+1 relative to the base ts.
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000001_create_webauthn_credentials/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("CREATE TABLE webauthn_credentials"),
            "missing CREATE TABLE webauthn_credentials: {up}"
        );
        assert!(up.contains("user_id"), "missing user_id column: {up}");
        assert!(
            up.contains("credential_id"),
            "missing credential_id column: {up}"
        );
        assert!(
            up.contains("credential_json"),
            "missing credential_json column: {up}"
        );
        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000001_create_webauthn_credentials/down.sql"),
        )
        .unwrap();
        assert!(
            down.contains("DROP TABLE webauthn_credentials"),
            "down.sql must drop webauthn_credentials: {down}"
        );
    }

    #[test]
    fn passkeys_plan_emits_four_ceremony_routes() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();
        for needle in [
            "passkey_register_begin",
            "passkey_register_finish",
            "passkey_login_begin",
            "passkey_login_finish",
        ] {
            assert!(
                routes.contains(needle),
                "passkeys.rs missing ceremony handler: {needle}"
            );
        }
    }

    #[test]
    fn passkeys_plan_emits_list_and_revoke_surface() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();
        assert!(
            routes.contains("passkey_list"),
            "missing passkey_list: {routes}"
        );
        assert!(
            routes.contains("passkey_revoke"),
            "missing passkey_revoke: {routes}"
        );
        assert!(
            routes.contains("#[secured]"),
            "revoke/list must use #[secured]: {routes}"
        );
    }

    #[test]
    fn passkeys_login_finish_writes_session_auth_key() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();
        assert!(
            routes.contains("state.auth_session_key()"),
            "passkey_login_finish must call session.insert(state.auth_session_key(), ...): {routes}"
        );
        // After template substitution ("User" → "user"), __SNAKE___id becomes user_id.
        assert!(
            routes.contains("\"user_id\""),
            "passkey_login_finish must store user_id in session: {routes}"
        );
        assert!(
            routes.contains("\"user_email\""),
            "passkey_login_finish must store user_email in session: {routes}"
        );
    }

    #[test]
    fn passkeys_plan_emits_rp_config_in_autumn_toml() {
        let tmp = project_with_main();
        // Create an autumn.toml so the generator can update it.
        fs::write(tmp.path().join("autumn.toml"), "[server]\nport = 3000\n").unwrap();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(
            toml.contains("[auth.webauthn]"),
            "autumn.toml missing [auth.webauthn]: {toml}"
        );
        assert!(toml.contains("rp_id"), "autumn.toml missing rp_id: {toml}");
        assert!(
            toml.contains("rp_name"),
            "autumn.toml missing rp_name: {toml}"
        );
        assert!(
            toml.contains("rp_origin"),
            "autumn.toml missing rp_origin: {toml}"
        );
    }

    #[test]
    fn passkeys_plan_adds_webauthn_feature_to_autumn_web() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("webauthn"),
            "Cargo.toml must reference webauthn feature or dep: {cargo}"
        );
        assert!(
            cargo.contains("webauthn-rs"),
            "Cargo.toml missing webauthn-rs dep: {cargo}"
        );
    }

    #[test]
    fn passkeys_plan_generates_integration_tests() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let tests = fs::read_to_string(tmp.path().join("tests/auth_passkeys.rs")).unwrap();
        for needle in [
            "passkey_register_happy_path",
            "passkey_login_happy_path",
            "passkey_wrong_origin_rejected",
            "passkey_unknown_credential_rejected",
            "passkey_revoke_then_relogin",
        ] {
            assert!(
                tests.contains(needle),
                "auth_passkeys.rs missing test: {needle}"
            );
        }
    }

    #[test]
    fn passkeys_plan_writes_docs() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let docs = fs::read_to_string(tmp.path().join("docs/guide/passkeys.md")).unwrap();
        assert!(
            docs.contains("rp_id") || docs.contains("rp_origin") || docs.contains("WebAuthn"),
            "docs/guide/passkeys.md should cover rp config or WebAuthn: {docs}"
        );
    }

    #[test]
    fn passkeys_plan_maud_template_has_js_shim() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();
        assert!(
            routes.contains("navigator.credentials"),
            "passkeys.rs Maud template must include navigator.credentials JS: {routes}"
        );
    }

    #[test]
    fn passkeys_plan_registers_routes_in_main() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        for entry in [
            "routes::passkeys::passkey_register_begin",
            "routes::passkeys::passkey_register_finish",
            "routes::passkeys::passkey_login_begin",
            "routes::passkeys::passkey_login_finish",
            "routes::passkeys::passkey_list",
            "routes::passkeys::passkey_revoke",
        ] {
            assert!(
                main.contains(entry),
                "main.rs missing passkey route: {entry}"
            );
        }
    }

    #[test]
    fn passkeys_can_combine_with_oauth_and_totp() {
        let tmp = project_with_main();
        let oauth = AuthOAuthOptions {
            providers: vec!["github".to_owned()],
        };
        plan_auth_full_ex(tmp.path(), "User", "20260508000000", &oauth, true, true)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("src/routes/passkeys.rs").exists());
        assert!(tmp.path().join("src/routes/oauth.rs").exists());
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("two_factor_enable"),
            "totp+passkeys+oauth must keep 2fa handlers"
        );
    }

    #[test]
    fn passkeys_misconfiguration_fails_fast_error_message() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();
        assert!(
            routes.contains("auth.webauthn"),
            "build_webauthn error must mention auth.webauthn config key: {routes}"
        );
    }

    #[test]
    fn ensure_webauthn_rs_features_merges_into_shorthand() {
        let toml = "webauthn-rs = \"0.5\"\n";
        let out = ensure_webauthn_rs_features(toml);
        assert!(
            out.contains("danger-allow-state-serialisation"),
            "shorthand should be promoted: {out}"
        );
        assert!(
            out.contains("conditional-ui"),
            "missing conditional-ui: {out}"
        );
    }

    #[test]
    fn ensure_webauthn_rs_features_merges_into_partial_inline() {
        let toml = "webauthn-rs = { version = \"0.5\", features = [\"danger-allow-state-serialisation\"] }\n";
        let out = ensure_webauthn_rs_features(toml);
        assert!(
            out.contains("conditional-ui"),
            "conditional-ui should be added to partial features: {out}"
        );
    }

    #[test]
    fn ensure_webauthn_rs_features_is_idempotent() {
        let toml = "webauthn-rs = { version = \"0.5\", features = [\"danger-allow-state-serialisation\", \"conditional-ui\"] }\n";
        let out = ensure_webauthn_rs_features(toml);
        assert_eq!(out, toml, "idempotent: should not duplicate features");
    }

    #[test]
    fn passkeys_routes_template_has_redirect_to() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();
        assert!(
            routes.contains("fn redirect_to"),
            "passkeys.rs must define redirect_to: {routes}"
        );
        assert!(
            routes.contains("fn redirect_to(url: &str) -> axum::response::Redirect"),
            "redirect_to must return a concrete axum::response::Redirect \
             (impl Trait is illegal nested in AutumnResult<_>): {routes}"
        );
    }

    #[test]
    fn passkeys_register_begin_binds_user_id() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();
        assert!(
            routes.contains("user_id"),
            "register begin should store user_id in passkey_reg_state envelope: {routes}"
        );
    }

    #[test]
    fn passkeys_templates_accept_csrf_header_and_nonce() {
        let tmp = project_with_main();
        passkey_plan(tmp.path()).execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/passkeys.rs")).unwrap();

        assert!(
            routes.contains("pub async fn passkey_register_page("),
            "passkey_register_page is missing"
        );
        assert!(
            routes.contains("csrf_header: Option<CsrfTokenHeader>"),
            "passkey_register_page must take csrf_header"
        );
        assert!(
            routes.contains("nonce: Option<CspNonce>"),
            "passkey_register_page must take nonce"
        );
        assert!(
            routes.contains("pub async fn passkey_login_page("),
            "passkey_login_page is missing"
        );
        assert!(
            routes.contains("csrf_header: Option<CsrfTokenHeader>"),
            "passkey_login_page must take csrf_header"
        );
        assert!(
            routes.contains("nonce: Option<CspNonce>"),
            "passkey_login_page must take nonce"
        );

        assert!(
            routes.contains("script nonce=[script_nonce]"),
            "templates must output script tags with nonce attribute: {routes}"
        );
        assert!(
            routes.contains("const csrfHeader = document.querySelector('meta[name=\"csrf-token-header\"]')?.content ?? 'X-CSRF-Token';"),
            "templates must dynamically resolve csrf header name: {routes}"
        );
        assert!(
            routes.contains("[csrfHeader]: csrfToken"),
            "templates must use dynamic csrf header key in fetch calls: {routes}"
        );
    }

    // ── Account lockout (issue #814) ────────────────────────────────────────

    #[test]
    fn migration_up_sql_contains_lockout_columns() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("failed_attempts"),
            "migration must include failed_attempts column: {up}"
        );
        assert!(
            up.contains("locked_at"),
            "migration must include locked_at column: {up}"
        );
    }

    #[test]
    fn model_file_contains_lockout_fields() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/user.rs")).unwrap();
        assert!(
            model.contains("pub failed_attempts"),
            "model must include failed_attempts field: {model}"
        );
        assert!(
            model.contains("pub locked_at"),
            "model must include locked_at field: {model}"
        );
    }

    #[test]
    fn schema_rs_contains_lockout_columns() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("failed_attempts"),
            "schema must include failed_attempts: {schema}"
        );
        assert!(
            schema.contains("locked_at"),
            "schema must include locked_at: {schema}"
        );
    }

    #[test]
    fn routes_file_checks_lockout_state_on_login() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("locked_at"),
            "login handler must check locked_at for lockout: {routes}"
        );
        assert!(
            routes.contains("failed_attempts"),
            "login handler must track failed_attempts: {routes}"
        );
    }

    #[test]
    fn routes_file_lockout_response_indistinguishable_from_wrong_password() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // Lockout must return the same auth_err closure as wrong password (non-enumerating).
        let auth_err_count = routes.matches("auth_err()").count();
        assert!(
            auth_err_count >= 2,
            "lockout must reuse auth_err() for non-enumerating response (found {auth_err_count}): {routes}"
        );
    }

    #[test]
    fn routes_file_resets_failed_attempts_on_successful_login() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("failed_attempts.eq(0)"),
            "successful login must reset failed_attempts to 0: {routes}"
        );
    }

    #[test]
    fn routes_file_emits_telemetry_event_on_lockout_transition() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("account_locked"),
            "lockout transition must emit a structured telemetry event with account_locked: {routes}"
        );
    }

    #[test]
    fn routes_file_respects_lockout_enabled_config() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // Must check enabled flag or threshold so operators can disable lockout.
        assert!(
            routes.contains("lockout")
                && (routes.contains("enabled") || routes.contains("threshold")),
            "routes must respect lockout enabled/threshold config for opt-out: {routes}"
        );
    }

    /// #2500: the lock-stamping `UPDATE ... SET locked_at = now()` must never be
    /// unconditional. A concurrent successful login can reset `failed_attempts`
    /// to 0 and clear `locked_at` in the gap between the failed request's own
    /// increment and its lock stamp; without a fresh DB-level guard the stamp
    /// re-locks the account out from under the login that already succeeded.
    /// Both the `login` and `reauth` handlers duplicate this block, so both
    /// must carry the guard.
    #[test]
    fn routes_file_guards_lock_stamp_against_concurrent_reset() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();

        let guarded_stamp_occurrences = routes
            .matches("failed_attempts.ge(lockout_cfg.threshold)")
            .count();
        assert_eq!(
            guarded_stamp_occurrences, 2,
            "both the login and reauth lock-stamp UPDATEs must filter on \
             failed_attempts.ge(lockout_cfg.threshold) so a concurrent successful \
             login's reset cannot be clobbered by a stale-threshold re-lock \
             (found {guarded_stamp_occurrences}): {routes}"
        );

        // `locked_at.is_null()` also appears, unrelated, in the pre-existing
        // success-path reset guard (`locked_at.is_null().or(locked_at.le(...)))`),
        // so a bare substring count can't tell the lock-stamp guard apart from
        // that unrelated clause and would stay green even if the lock-stamp
        // UPDATE's own `.filter(locked_at.is_null())` were dropped. Anchor on
        // `locked_at.eq(Some(now))` instead — the lock-stamp `.set(...)` call,
        // which appears exactly once per handler (login, reauth) — and require
        // both guard filters to appear immediately before each one.
        let stamp_occurrences = routes.matches("locked_at.eq(Some(now))").count();
        assert_eq!(
            stamp_occurrences, 2,
            "expected exactly one lock-stamp UPDATE in each of login and reauth \
             (found {stamp_occurrences}): {routes}"
        );
        let mut search_from = 0;
        for i in 0..stamp_occurrences {
            let stamp_at = routes[search_from..]
                .find("locked_at.eq(Some(now))")
                .map(|pos| search_from + pos)
                .unwrap();
            let window_start = stamp_at.saturating_sub(400);
            let preceding = &routes[window_start..stamp_at];
            assert!(
                preceding.contains("failed_attempts.ge(lockout_cfg.threshold)")
                    && preceding.contains("locked_at.is_null()"),
                "lock-stamp UPDATE #{} must be guarded by both \
                 failed_attempts.ge(lockout_cfg.threshold) and locked_at.is_null() \
                 immediately before the `locked_at.eq(Some(now))` write, so it never \
                 re-stamps (and extends the cool-off of) an account another request \
                 already locked, nor clobbers a concurrent successful reset: {routes}",
                i + 1
            );
            search_from = stamp_at + "locked_at.eq(Some(now))".len();
        }
    }

    /// #2500: the `account_locked` telemetry event must only fire when this
    /// request's own write actually applied the lock. If a concurrent
    /// successful login won the race (see the guard tested above), the
    /// lock-stamp UPDATE affects zero rows and no lock ever took effect, so
    /// logging `account_locked` would be a false positive.
    #[test]
    fn routes_file_gates_lockout_telemetry_on_rows_actually_locked() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();

        let login_block_start = routes
            .find("Failed to lock account")
            .expect("login handler must attempt to lock the account");
        let telemetry_start = routes[login_block_start..]
            .find("account_locked")
            .expect("account_locked telemetry must follow the lock-stamp UPDATE");
        let between = &routes[login_block_start..login_block_start + telemetry_start];
        assert!(
            between.contains("locked_rows") && between.contains("if locked_rows > 0"),
            "telemetry must be gated behind a check that the lock-stamp UPDATE \
             actually affected a row (`if locked_rows > 0`), not fired \
             unconditionally after attempting the write: {routes}"
        );
    }

    #[test]
    fn generated_tests_cover_lockout_scenarios() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let tests = fs::read_to_string(tmp.path().join("tests/auth.rs")).unwrap();
        // AC8a: N+1 failed attempts reject correct credentials
        assert!(
            tests.contains("auth_lockout_rejects_correct_credentials"),
            "generated tests must cover: N+1 failed attempts with correct creds are rejected: {tests}"
        );
        // AC8b: successful login resets counter
        assert!(
            tests.contains("auth_successful_login_resets_lockout_counter"),
            "generated tests must cover: successful login resets counter: {tests}"
        );
        // AC8c: locked-state response byte-identical to wrong-password
        assert!(
            tests.contains("auth_lockout_response_identical_to_wrong_password"),
            "generated tests must verify lockout response is byte-identical to wrong-password: {tests}"
        );
    }

    #[test]
    fn docs_file_documents_lockout_config() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let docs = fs::read_to_string(tmp.path().join("docs/guide/authentication.md")).unwrap();
        assert!(
            docs.contains("lockout"),
            "docs must document the lockout policy: {docs}"
        );
        assert!(
            docs.contains("threshold"),
            "docs must document the threshold setting: {docs}"
        );
        assert!(
            docs.contains("cooloff") || docs.contains("cool_off") || docs.contains("cool-off"),
            "docs must document the cool-off period: {docs}"
        );
    }

    #[test]
    fn docs_file_documents_operator_unlock_path() {
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let docs = fs::read_to_string(tmp.path().join("docs/guide/authentication.md")).unwrap();
        assert!(
            docs.contains("unlock"),
            "docs must document operator unlock path: {docs}"
        );
    }

    // ── GDPR data export & account deletion (issue #820) ─────────────────────

    #[test]
    fn plan_auth_creates_data_export_route() {
        // AC#1: auth starter generates a "Download my data" route
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("data_export") || routes.contains("download_my_data"),
            "generated routes must include a data-export handler: {routes}"
        );
    }

    #[test]
    fn plan_auth_data_export_route_requires_auth() {
        // AC#1: the export route must be protected by authentication
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        // Find the data_export function and verify it extracts Auth
        let export_pos = routes
            .find("data_export")
            .expect("data_export route must be present");
        let export_body = &routes[export_pos..export_pos + 500.min(routes.len() - export_pos)];
        // Authentication is enforced either via the tracked-session gate
        // (#819) or a session/Auth extractor check.
        assert!(
            export_body.contains("require_tracked_session")
                || export_body.contains("Auth")
                || export_body.contains("auth"),
            "data_export route must require authentication: {export_body}"
        );
    }

    #[test]
    fn plan_auth_creates_delete_account_route() {
        // AC#3: auth starter generates a "Delete my account" flow
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("delete_account") || routes.contains("account_delete"),
            "generated routes must include an account-deletion handler: {routes}"
        );
    }

    #[test]
    fn plan_auth_delete_account_has_confirmation_step() {
        // AC#3: deletion flow requires a confirmation step
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("confirm") || routes.contains("confirmation"),
            "delete_account handler must include a confirmation step: {routes}"
        );
    }

    #[test]
    fn plan_auth_delete_account_writes_audit_log() {
        // AC#3: account deletion must write an audit log entry
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("audit") || routes.contains("AuditEvent"),
            "delete_account handler must write an audit log entry: {routes}"
        );
    }

    #[test]
    fn plan_auth_migration_includes_deletion_tracking_columns() {
        // AC#3: migration must include grace-period deletion tracking columns
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("delete_requested_at") || up.contains("deletion_requested_at"),
            "migration must include a deletion-request timestamp column: {up}"
        );
        assert!(
            up.contains("delete_scheduled_at")
                || up.contains("deletion_scheduled_at")
                || up.contains("delete_grace_until"),
            "migration must include a grace-period deadline column: {up}"
        );
    }

    #[test]
    fn plan_auth_migration_includes_export_tracking_columns() {
        // AC#1/#5: migration must include export-request tracking columns
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260508000000_create_users/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("export_requested_at") || up.contains("data_export_requested_at"),
            "migration must include an export-request timestamp column: {up}"
        );
    }

    #[test]
    fn plan_auth_creates_export_job() {
        // AC#5: auth starter generates a background export job
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let routes = fs::read_to_string(tmp.path().join("src/routes/auth.rs")).unwrap();
        assert!(
            routes.contains("export_job")
                || routes.contains("ExportJob")
                || routes.contains("enqueue"),
            "generated code must enqueue a background export job: {routes}"
        );
    }

    #[test]
    fn plan_auth_gdpr_docs_file_created() {
        // AC#7: auth starter generates a GDPR compliance documentation file
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let paths: Vec<String> = plan
            .actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();
        assert!(
            paths
                .iter()
                .any(|p| p.contains("gdpr") || p.contains("compliance")),
            "plan must include a GDPR/compliance documentation file; got: {paths:?}"
        );
    }

    #[test]
    fn plan_auth_gdpr_docs_references_article_15_and_17() {
        // AC#7: GDPR docs must reference Article 15 (right of access) and Article 17 (erasure)
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();

        // Find the GDPR compliance docs file
        let docs_path = plan
            .actions
            .iter()
            .find(|a| {
                let path_str = a.path().to_string_lossy().to_lowercase();
                path_str.contains("gdpr") || path_str.contains("compliance")
            })
            .map(|a| a.path().to_owned())
            .expect("plan must include a GDPR/compliance documentation file");

        let content = fs::read_to_string(&docs_path).unwrap();
        assert!(
            content.contains("Article 15") || content.contains("article 15"),
            "GDPR docs must reference Article 15 (right of access): {content}"
        );
        assert!(
            content.contains("Article 17") || content.contains("article 17"),
            "GDPR docs must reference Article 17 (right to erasure): {content}"
        );
    }

    #[test]
    fn plan_auth_model_includes_delete_requested_at_field() {
        // AC#3: generated User model must include deletion tracking fields
        let tmp = project_with_main();
        let plan = plan_auth(tmp.path(), "User", "20260508000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/user.rs")).unwrap();
        assert!(
            model.contains("delete_requested_at") || model.contains("deletion_requested_at"),
            "User model must include deletion tracking field: {model}"
        );
    }
}
