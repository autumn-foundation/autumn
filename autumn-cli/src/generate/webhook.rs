//! `autumn generate webhook` — scaffold a signed, replay-safe inbound webhook
//! endpoint over the shipped `autumn_web::webhook` surface (issue #1366).
//!
//! For `autumn generate webhook stripe Payments`, the generator produces:
//! - `src/webhooks/payments.rs` — a `#[post("/webhooks/stripe")]` handler taking
//!   the shipped [`SignedWebhook`](autumn_web::webhook::SignedWebhook) extractor,
//!   an `event_type()` dispatch skeleton with clearly-marked stub arms, and a
//!   `#[cfg(test)]` module covering the valid / missing / invalid / replayed
//!   delivery cases.
//! - `src/webhooks/mod.rs` — created or updated with `pub mod payments;`.
//! - `src/main.rs` — `mod webhooks;` plus the route registered in `routes![…]`.
//! - `autumn.toml` — a `[[security.webhooks.endpoints]]` stub (path, provider,
//!   `secret_env`, replay protection on), the `[security.webhooks.replay]`
//!   backend declaration, and the CSRF/CAPTCHA path exemptions the endpoint
//!   needs in production.
//! - `Cargo.toml` — `serde_json` plus the tokio dev-dependency features
//!   `#[tokio::test]` needs.
//!
//! Nothing here hand-rolls signature verification: raw-body capture,
//! constant-time HMAC comparison, timestamp tolerance, replay windows, and
//! secret rotation all live in the framework already. The generator's job is to
//! wire them up correctly, which is exactly the part that is easy to get wrong
//! by hand.

use std::fmt::Write as _;
use std::path::Path;

use super::emit::Plan;
use super::model::{ensure_cargo_dependencies, validate_resource_name};
use super::naming::snake;
use super::schema_edit::{
    add_mod_declaration, ensure_dev_dependency_tokio_test_features, update_main_rs,
};
use super::{GenerateError, ensure_project_root, read_or_empty};

/// Cargo dependencies the generated handler and its test module need.
///
/// Neither is in `autumn new`'s template, so a fresh project really does need
/// them added: the handler decodes the verified body into a
/// `serde_json::Value` and logs the delivery envelope with `tracing`.
const WEBHOOK_DEPS: &[(&str, &str)] = &[("serde_json", "\"1\""), ("tracing", "\"0.1\"")];

/// A provider preset supported by `autumn generate webhook`, mapping 1:1 onto
/// [`autumn_web::webhook::WebhookProvider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// `Stripe-Signature: t=…,v1=…` over `{timestamp}.{raw_body}`.
    Stripe,
    /// `X-Hub-Signature-256: sha256=…` over the raw body.
    Github,
    /// `X-Slack-Signature: v0=…` over `v0:{timestamp}:{raw_body}`.
    Slack,
    /// Generic HMAC-SHA256 over the raw body.
    Generic,
}

/// Every provider preset, in the order `--help` and error messages list them.
const PROVIDERS: &[Provider] = &[
    Provider::Stripe,
    Provider::Github,
    Provider::Slack,
    Provider::Generic,
];

impl Provider {
    /// Parse a CLI provider argument (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns [`GenerateError::Config`] naming the supported presets when
    /// `value` is not one of them.
    pub fn parse(value: &str) -> Result<Self, GenerateError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stripe" => Ok(Self::Stripe),
            "github" => Ok(Self::Github),
            "slack" => Ok(Self::Slack),
            "generic" | "hmac" => Ok(Self::Generic),
            other => Err(GenerateError::Config(format!(
                "unsupported webhook provider {other:?} — supported presets are {}. Use \
                 `generic` for any provider that signs the raw body with HMAC-SHA256, then \
                 adjust the header names under `[[security.webhooks.endpoints]]` in \
                 autumn.toml.",
                PROVIDERS
                    .iter()
                    .map(|provider| format!("`{}`", provider.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Lower-case preset name, matching `WebhookProvider`'s serde representation.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stripe => "stripe",
            Self::Github => "github",
            Self::Slack => "slack",
            Self::Generic => "generic",
        }
    }

    /// The `WebhookProvider` variant this preset maps to.
    const fn variant(self) -> &'static str {
        match self {
            Self::Stripe => "Stripe",
            Self::Github => "Github",
            Self::Slack => "Slack",
            Self::Generic => "Generic",
        }
    }

    /// Human-readable provider name used in generated doc comments.
    const fn label(self) -> &'static str {
        match self {
            Self::Stripe => "Stripe",
            Self::Github => "GitHub",
            Self::Slack => "Slack",
            Self::Generic => "a generic HMAC-SHA256 provider",
        }
    }

    /// Default `secret_env` name written into the `autumn.toml` stub.
    fn default_secret_env(self) -> String {
        format!("{}_WEBHOOK_SECRET", self.as_str().to_ascii_uppercase())
    }

    /// Event types the generated `match` gets stub arms for.
    ///
    /// For Slack these are the *inner* `event.type` values: Slack's Events API
    /// wraps them in an `event_callback` envelope, which is what
    /// `SignedWebhook::event_type()` reports (it reads the top-level JSON
    /// `type`), so the generated handler unwraps the envelope before matching.
    const fn stub_events(self) -> &'static [&'static str] {
        match self {
            Self::Stripe => &[
                "payment_intent.succeeded",
                "customer.subscription.updated",
                "customer.subscription.deleted",
            ],
            Self::Github => &["push", "pull_request", "issues"],
            Self::Slack => &["app_mention", "message"],
            Self::Generic => &["example.created", "example.updated"],
        }
    }

    /// Whether this preset's signature covers a request timestamp, so the
    /// generated tests need to bind one.
    const fn is_timestamped(self) -> bool {
        matches!(self, Self::Stripe | Self::Slack)
    }

    /// Where the delivery ID that replay protection keys on comes from.
    const fn delivery_id_source(self) -> &'static str {
        match self {
            Self::Stripe => "the JSON `id` field",
            Self::Github => "the `X-GitHub-Delivery` header",
            Self::Slack => "the JSON `event_id` field",
            Self::Generic => "the `X-Webhook-Delivery` header",
        }
    }

    /// Where the provider's dashboard webhook should be pointed, for the
    /// printed next steps.
    const fn dashboard_hint(self) -> &'static str {
        match self {
            Self::Stripe => "Stripe Dashboard → Developers → Webhooks",
            Self::Github => "GitHub repository/org → Settings → Webhooks",
            Self::Slack => "Slack app config → Event Subscriptions",
            Self::Generic => "your provider's webhook configuration",
        }
    }
}

/// Per-invocation overrides for `autumn generate webhook`.
#[derive(Debug, Clone, Default)]
pub struct WebhookOptions {
    /// Route path override (default: `/webhooks/<provider>`).
    pub path: Option<String>,
    /// Signing-secret environment variable override (default:
    /// `<PROVIDER>_WEBHOOK_SECRET`).
    pub secret_env: Option<String>,
}

/// Everything the renderers need about the endpoint being generated.
struct EndpointSpec {
    provider: Provider,
    /// Endpoint name in `autumn.toml` and in replay keys (snake-cased `<Name>`).
    name: String,
    /// Route path the handler and the endpoint config share.
    path: String,
    /// Environment variable supplying the signing secret.
    secret_env: String,
    /// Generated handler function name.
    handler_fn: String,
}

/// Compute the file actions for `autumn generate webhook <provider> <Name>`.
///
/// # Errors
///
/// Returns [`GenerateError`] when the project layout, resource name, provider
/// preset, or `--path` override is invalid, when `src/main.rs` cannot be read,
/// or when `autumn.toml` already configures a *different* endpoint on the same
/// path (which would fail the framework's own duplicate-path validation at
/// boot).
pub fn plan_webhook(
    project_root: &Path,
    provider: &str,
    name: &str,
    options: &WebhookOptions,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;
    let provider = Provider::parse(provider)?;

    let snake_name = snake(name);
    let path = match options.path.as_deref() {
        Some(path) => validate_route_path(path)?,
        None => format!("/webhooks/{}", provider.as_str()),
    };
    let spec = EndpointSpec {
        provider,
        name: snake_name.clone(),
        path,
        secret_env: options
            .secret_env
            .clone()
            .unwrap_or_else(|| provider.default_secret_env()),
        handler_fn: format!("{snake_name}_webhook"),
    };

    let mut plan = Plan::new(project_root);

    // ── src/webhooks/<snake>.rs ───────────────────────────────────────────
    plan.create(
        project_root
            .join("src")
            .join("webhooks")
            .join(format!("{snake_name}.rs")),
        render_handler_file(&spec),
    );

    // ── src/webhooks/mod.rs ───────────────────────────────────────────────
    let mod_path = project_root.join("src").join("webhooks").join("mod.rs");
    plan.modify(
        mod_path.clone(),
        add_mod_declaration(&read_or_empty(&mod_path), &snake_name),
    );
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: mod_path,
        name: snake_name.clone(),
    });

    // ── src/main.rs — `mod webhooks;` + the route in `routes![…]` ─────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|error| {
        GenerateError::Io(std::io::Error::new(
            error.kind(),
            format!("{}: {error}", main_path.display()),
        ))
    })?;
    let route_entries = vec![format!("webhooks::{snake_name}::{}", spec.handler_fn)];
    plan.modify(
        main_path.clone(),
        update_main_rs(&main_existing, &["webhooks"], &route_entries),
    );
    // `mod webhooks;` is shared infrastructure (see
    // `emit::SHARED_MAIN_MODULE_NAMES`) — only this webhook's own route entry
    // is reverted here.
    plan.push_revert(crate::generate::emit::Revert::RoutesEntries {
        path: main_path,
        entries: route_entries,
    });

    // ── autumn.toml — endpoint stub, replay backend, path exemptions ──────
    plan_autumn_toml(&mut plan, project_root, &spec)?;

    // ── Cargo.toml — serde_json + tokio test features ─────────────────────
    let cargo_path = project_root.join("Cargo.toml");
    let cargo_existing = read_or_empty(&cargo_path);
    let updated_cargo = ensure_dev_dependency_tokio_test_features(&ensure_cargo_dependencies(
        &cargo_existing,
        WEBHOOK_DEPS,
    ));
    if updated_cargo != cargo_existing {
        plan.modify(cargo_path.clone(), updated_cargo);
    }
    // Pushed unconditionally — see `plan_cargo_deps`'s matching comment in
    // model.rs: destroy recomputes this plan against the already-generated
    // Cargo.toml, where these entries are by definition already present.
    plan.push_revert(crate::generate::emit::Revert::CargoDeps {
        path: cargo_path,
        names: WEBHOOK_DEPS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect(),
        owner_dir: project_root.join("src").join("webhooks"),
    });

    // ── Printed next steps (AC #5) ────────────────────────────────────────
    plan.warn(format!(
        "Set the signing secret before starting the app: add {}=… to `.env` (gitignored, \
         auto-loaded in the dev/test profiles) or export it. autumn.toml references the \
         variable via `secret_env`, so the secret itself is never committed — and the app \
         refuses to start while a configured endpoint has no secret.",
        spec.secret_env
    ));
    plan.warn(format!(
        "Point {} at POST {} — the endpoint is registered from autumn.toml's \
         `[[security.webhooks.endpoints]]`, so no builder wiring is needed.",
        spec.provider.dashboard_hint(),
        spec.path
    ));
    plan.warn(
        "Production: replay protection defaults to the process-local `memory` backend, which \
         production config validation rejects. Set [security.webhooks.replay] backend = \
         \"redis\" (with a redis url) before deploying more than one replica."
            .to_owned(),
    );

    Ok(plan)
}

/// Validate a `--path` override: it must be an absolute route path and not the
/// site root, matching `WebhookEndpointConfig::validate`'s own rule so a
/// generated config can never fail at boot.
fn validate_route_path(path: &str) -> Result<String, GenerateError> {
    let trimmed = path.trim();
    if !trimmed.starts_with('/') || trimmed == "/" {
        return Err(GenerateError::Config(format!(
            "invalid webhook path {path:?}: it must start with '/' and must not be the site \
             root (e.g. `--path /webhooks/stripe-billing`)"
        )));
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(GenerateError::Config(format!(
            "invalid webhook path {path:?}: route paths must not contain whitespace"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Add the `autumn.toml` actions: the endpoint stub, the replay-backend
/// declaration, and the CSRF/CAPTCHA exemptions for the route path.
fn plan_autumn_toml(
    plan: &mut Plan,
    project_root: &Path,
    spec: &EndpointSpec,
) -> Result<(), GenerateError> {
    let toml_path = project_root.join("autumn.toml");
    let existing = read_or_empty(&toml_path);
    reject_conflicting_endpoint(&existing, spec)?;

    match upsert_webhook_endpoint(&existing, spec) {
        Some(updated) => {
            plan.modify(toml_path.clone(), updated);
            plan.push_revert(crate::generate::emit::Revert::WebhookEndpointStub {
                path: toml_path,
                name: spec.name.clone(),
                route_path: spec.path.clone(),
            });
        }
        None => {
            // A config we cannot parse is left untouched rather than corrupted;
            // the user gets the exact block to paste instead.
            plan.warn(format!(
                "Could not parse {} — add this endpoint manually:\n{}",
                toml_path.display(),
                manual_endpoint_block(spec)
            ));
        }
    }
    Ok(())
}

/// Reject a second endpoint on a path some *other* endpoint already claims.
///
/// `WebhookRegistry::from_config` fails with `DuplicatePath` when two endpoints
/// share a path, so an app generated over one would refuse to boot. Re-running
/// the generator for the *same* endpoint name is fine — that is a regeneration
/// (or a `destroy` recomputing this plan), not a conflict.
fn reject_conflicting_endpoint(existing: &str, spec: &EndpointSpec) -> Result<(), GenerateError> {
    let Ok(doc) = existing.parse::<toml_edit::DocumentMut>() else {
        return Ok(());
    };
    let Some(endpoints) = doc
        .get("security")
        .and_then(|security| security.get("webhooks"))
        .and_then(|webhooks| webhooks.get("endpoints"))
        .and_then(toml_edit::Item::as_array_of_tables)
    else {
        return Ok(());
    };

    for endpoint in endpoints {
        let name = endpoint.get("name").and_then(|v| v.as_str());
        let path = endpoint.get("path").and_then(|v| v.as_str());
        if path == Some(spec.path.as_str()) && name != Some(spec.name.as_str()) {
            return Err(GenerateError::Config(format!(
                "autumn.toml already configures webhook endpoint {:?} on path {:?}; two \
                 endpoints on one path fail config validation at boot. Re-run with a distinct \
                 path, e.g. `--path {}-{}`.",
                name.unwrap_or("<unnamed>"),
                spec.path,
                spec.path,
                spec.name
            )));
        }
    }
    Ok(())
}

/// The `[[security.webhooks.endpoints]]` block, as text, for the "could not
/// parse autumn.toml" fallback.
fn manual_endpoint_block(spec: &EndpointSpec) -> String {
    format!(
        "[[security.webhooks.endpoints]]\nname = \"{}\"\npath = \"{}\"\nprovider = \"{}\"\n\
         secret_env = \"{}\"\nreplay_protection = true\n",
        spec.name,
        spec.path,
        spec.provider.as_str(),
        spec.secret_env,
    )
}

/// Insert (idempotently) this webhook's `autumn.toml` configuration:
/// `[security.webhooks.replay]`, the `[[security.webhooks.endpoints]]` entry,
/// and the CSRF/CAPTCHA exemptions for its path.
///
/// Edits are made through `toml_edit` so comments, key order, and hand-crafted
/// array layout in the rest of the file survive untouched. Returns `None` when
/// the document does not parse — the caller then leaves the file alone.
fn upsert_webhook_endpoint(existing: &str, spec: &EndpointSpec) -> Option<String> {
    use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value, value};

    let mut doc = existing.parse::<DocumentMut>().ok()?;

    let security_missing = !doc.as_table().contains_key("security");
    let security = doc
        .as_table_mut()
        .entry("security")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()?;
    if security_missing {
        // Render as dotted `[security.webhooks…]` headers rather than emitting
        // a bare, empty `[security]` table of its own.
        security.set_implicit(true);
    }

    let webhooks_missing = !security.contains_key("webhooks");
    let webhooks = security
        .entry("webhooks")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()?;
    if webhooks_missing {
        webhooks.set_implicit(true);
    }

    // ── [security.webhooks.replay] ────────────────────────────────────────
    if !webhooks.contains_key("replay") {
        let mut replay = Table::new();
        replay.decor_mut().set_prefix(
            "\n# Replay-protection storage for signed webhooks. \"memory\" is process-local:\n\
             # fine for tests, development, and single-replica deployments, but production\n\
             # config validation rejects it — switch to redis (or set\n\
             # allow_memory_in_production = true) before running more than one replica.\n\
             #\n\
             # backend = \"redis\"\n\
             # [security.webhooks.replay.redis]\n\
             # url = \"redis://redis:6379/0\"\n",
        );
        replay.insert("backend", value("memory"));
        webhooks.insert("replay", Item::Table(replay));
    }

    // ── [[security.webhooks.endpoints]] ───────────────────────────────────
    let endpoints = webhooks
        .entry("endpoints")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()?;
    let already_present = endpoints
        .iter()
        .any(|endpoint| endpoint.get("name").and_then(|v| v.as_str()) == Some(spec.name.as_str()));
    if !already_present {
        let mut endpoint = Table::new();
        endpoint.decor_mut().set_prefix(format!(
            "\n# Signed webhook intake generated by `autumn generate webhook {} {}`.\n\
             # The secret itself lives in the {} environment variable — never inline it here.\n\
             # During rotation, add the previous value's variable to previous_secret_envs.\n",
            spec.provider.as_str(),
            spec.name,
            spec.secret_env,
        ));
        endpoint.insert("name", value(spec.name.as_str()));
        endpoint.insert("path", value(spec.path.as_str()));
        endpoint.insert("provider", value(spec.provider.as_str()));
        endpoint.insert("secret_env", value(spec.secret_env.as_str()));
        endpoint.insert(
            "previous_secret_envs",
            Item::Value(Value::Array(Array::new())),
        );
        endpoint.insert("replay_protection", value(true));
        endpoints.push(endpoint);
    }

    // ── path exemptions ───────────────────────────────────────────────────
    // The `prod` profile turns CSRF on via smart defaults, and a configured
    // CAPTCHA challenges unauthenticated POSTs — either would reject a provider
    // callback that (correctly) carries no browser session. Signature
    // verification is this endpoint's authentication.
    let security = doc.as_table_mut().get_mut("security")?.as_table_mut()?;
    let csrf_missing = !security.contains_key("csrf");
    let csrf = security
        .entry("csrf")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()?;
    if csrf_missing {
        csrf.decor_mut().set_prefix(
            "\n# Signed webhooks authenticate with a provider signature, not a browser\n\
             # session, so their paths must be exempt from CSRF (the prod profile enables it).\n",
        );
    }
    push_unique_str(
        csrf.entry("exempt_paths")
            .or_insert_with(|| Item::Value(Value::Array(Array::new())))
            .as_array_mut()?,
        &spec.path,
    );

    push_unique_str(
        security
            .entry("captcha_exempt_paths")
            .or_insert_with(|| Item::Value(Value::Array(Array::new())))
            .as_array_mut()?,
        &spec.path,
    );

    Some(doc.to_string())
}

/// Append `value` to a TOML array unless it is already there.
fn push_unique_str(array: &mut toml_edit::Array, value: &str) {
    if array.iter().any(|item| item.as_str() == Some(value)) {
        return;
    }
    array.push(value);
}

/// Inverse of [`upsert_webhook_endpoint`] (`autumn destroy`, issue #1048).
///
/// Removes the `[[security.webhooks.endpoints]]` entry this generator added
/// (matched on both name and path, so a hand-added endpoint that merely shares
/// a name is never touched), its CSRF/CAPTCHA path exemptions, and — once no
/// endpoint is left — the `[security.webhooks.replay]` block, collapsing any
/// table the removals leave empty. A no-op when the document does not parse or
/// the entry is already gone.
pub(super) fn remove_webhook_endpoint(existing: &str, name: &str, route_path: &str) -> String {
    let Ok(mut doc) = existing.parse::<toml_edit::DocumentMut>() else {
        return existing.to_owned();
    };

    let Some(security) = doc
        .as_table_mut()
        .get_mut("security")
        .and_then(toml_edit::Item::as_table_mut)
    else {
        return existing.to_owned();
    };

    let mut removed = false;
    if let Some(webhooks) = security
        .get_mut("webhooks")
        .and_then(toml_edit::Item::as_table_mut)
    {
        if let Some(endpoints) = webhooks
            .get_mut("endpoints")
            .and_then(toml_edit::Item::as_array_of_tables_mut)
        {
            let before = endpoints.len();
            endpoints.retain(|endpoint| {
                let matches_name = endpoint.get("name").and_then(|v| v.as_str()) == Some(name);
                let matches_path =
                    endpoint.get("path").and_then(|v| v.as_str()) == Some(route_path);
                !(matches_name && matches_path)
            });
            removed = endpoints.len() < before;
            if endpoints.is_empty() {
                webhooks.remove("endpoints");
            }
        }
        if !removed {
            return existing.to_owned();
        }
        // The shared replay backend only exists to serve endpoints; with the
        // last one gone it has nothing left to configure.
        if !webhooks.contains_key("endpoints") {
            webhooks.remove("replay");
        }
        if webhooks.is_empty() {
            security.remove("webhooks");
        }
    }

    if !removed {
        return existing.to_owned();
    }

    if let Some(csrf) = security
        .get_mut("csrf")
        .and_then(toml_edit::Item::as_table_mut)
    {
        let emptied = remove_from_array(csrf.get_mut("exempt_paths"), route_path);
        if emptied {
            csrf.remove("exempt_paths");
        }
        if csrf.is_empty() {
            security.remove("csrf");
        }
    }
    if remove_from_array(security.get_mut("captcha_exempt_paths"), route_path) {
        security.remove("captcha_exempt_paths");
    }
    if security.is_empty() {
        doc.as_table_mut().remove("security");
    }

    doc.to_string()
}

/// Remove `value` from a TOML array item, reporting whether that left it empty.
fn remove_from_array(item: Option<&mut toml_edit::Item>, value: &str) -> bool {
    let Some(array) = item.and_then(toml_edit::Item::as_array_mut) else {
        return false;
    };
    array.retain(|entry| entry.as_str() != Some(value));
    array.is_empty()
}

// ── Template rendering ───────────────────────────────────────────────────────

/// Render `src/webhooks/<snake>.rs`: the handler, its event-dispatch skeleton,
/// the per-event stub functions, and the generated test module.
fn render_handler_file(spec: &EndpointSpec) -> String {
    let EndpointSpec {
        provider,
        name,
        path,
        secret_env,
        handler_fn,
    } = spec;
    let label = provider.label();
    let dispatch = render_dispatch(spec);
    let stubs = render_event_stubs(spec);
    let tests = render_tests(spec);

    format!(
        r#"//! Signed inbound webhook intake for {label} — generated by
//! `autumn generate webhook {provider_name} {name}`. Edit freely; once
//! generated this is ordinary application code.
//!
//! The `SignedWebhook` extractor verifies the provider signature against the
//! exact request bytes *before* this handler runs, applies the configured
//! timestamp tolerance, and rejects replayed deliveries. The endpoint —
//! path, provider preset, and the `{secret_env}` environment variable holding
//! the signing secret — is configured in `autumn.toml` under
//! `[[security.webhooks.endpoints]]`, and installed automatically at startup.
//! Nothing here hand-rolls HMAC verification.
//!
//! Rejections happen before the handler body (see
//! `docs/guide/signed-webhooks.md`):
//!
//! | Failure | Status |
//! |---------|--------|
//! | missing/malformed signature, timestamp, or delivery id | `400` |
//! | stale timestamp or signature mismatch | `401` |
//! | duplicate delivery inside the replay window | `409` |
//! | replay backend unavailable | `503` |
//!
//! Replay protection keys on {delivery_source}.
//!
//! Keep this handler fast and idempotent: webhook delivery is at-least-once, so
//! the same event can arrive more than once. Enqueue a job for slow work
//! (mail, external API calls, long transactions) and return promptly.

use autumn_web::prelude::*;

/// `POST {path}` — verified {label} intake.
#[post("{path}")]
pub async fn {handler_fn}(webhook: SignedWebhook) -> AutumnResult<Json<serde_json::Value>> {{
    // The bytes are already verified; this only decodes them. Swap
    // `serde_json::Value` for a typed provider struct when you have one.
    let event: serde_json::Value = webhook.json::<serde_json::Value>().map_err(|error| {{
        AutumnError::bad_request_msg(format!("invalid {provider_name} webhook payload: {{error}}"))
    }})?;

    let event_type = webhook.event_type().unwrap_or("unknown");
    // Log the envelope, never the payload — provider callbacks routinely carry
    // personal and financial data.
    tracing::info!(
        provider = webhook.provider(),
        endpoint = webhook.endpoint(),
        delivery_id = webhook.delivery_id().unwrap_or("-"),
        event_type,
        "signed webhook accepted"
    );

{dispatch}
    Ok(Json(serde_json::json!({{ "received": true }})))
}}

{stubs}{tests}"#,
        provider_name = provider.as_str(),
        delivery_source = provider.delivery_id_source(),
    )
}

/// Render the `match webhook.event_type()` dispatch skeleton.
fn render_dispatch(spec: &EndpointSpec) -> String {
    let arms = spec
        .provider
        .stub_events()
        .iter()
        .fold(String::new(), |mut out, event| {
            let _ = writeln!(
                out,
                "        {event:?} => {}(&event).await?,",
                stub_fn(event)
            );
            out
        });

    // Slack wraps every Events API callback in an `event_callback` envelope, and
    // `event_type()` reports that envelope's type — so the inner event type is
    // unwrapped here rather than matched directly.
    if spec.provider == Provider::Slack {
        return format!(
            r#"    match event_type {{
        // Slack's one-time endpoint handshake: echo the challenge verbatim.
        "url_verification" => {{
            let challenge = event
                .get("challenge")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            return Ok(Json(serde_json::json!({{ "challenge": challenge }})));
        }}
        "event_callback" => {{
            let inner = event
                .get("event")
                .and_then(|inner| inner.get("type"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            match inner {{
{arms}                // Acknowledge and ignore: a 2xx tells Slack not to retry an
                // event this app does not handle.
                other => tracing::debug!(event_type = other, "unhandled Slack event — acknowledged"),
            }}
        }}
        // Acknowledge and ignore every other envelope type.
        _ => tracing::debug!(event_type, "unhandled Slack callback — acknowledged"),
    }}
"#,
            arms = arms.lines().fold(String::new(), |mut out, line| {
                let _ = writeln!(out, "    {line}");
                out
            }),
        );
    }

    format!(
        r#"    match event_type {{
{arms}        // Acknowledge and ignore: returning 2xx stops the provider from
        // retrying an event this app does not handle.
        _ => tracing::debug!(event_type, "unhandled webhook event — acknowledged and ignored"),
    }}
"#
    )
}

/// Render one clearly-marked stub function per dispatched event type.
fn render_event_stubs(spec: &EndpointSpec) -> String {
    spec.provider
        .stub_events()
        .iter()
        .fold(String::new(), |mut out, event| {
            let _ = write!(
                out,
                r#"/// TODO: handle the `{event}` event.
///
/// `event` is the verified payload. Keep this idempotent — the same delivery
/// can arrive twice — and enqueue a job for anything slow.
async fn {fn_name}(event: &serde_json::Value) -> AutumnResult<()> {{
    let _ = event; // TODO: remove once the payload is used.
    tracing::info!("TODO: handle {event}");
    Ok(())
}}

"#,
                fn_name = stub_fn(event),
            );
            out
        })
}

/// The stub function name for an event type (`payment_intent.succeeded` →
/// `on_payment_intent_succeeded`).
fn stub_fn(event: &str) -> String {
    let mut out = String::with_capacity(event.len() + 3);
    out.push_str("on_");
    for ch in event.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out
}

/// Render the generated `#[cfg(test)]` module: valid, missing-header, invalid,
/// and replayed deliveries against the real extractor.
///
/// Split into the module preamble (fixtures, config, client, signature helper)
/// and [`render_test_cases`] (the four `#[tokio::test]`s) so neither half is a
/// wall of interleaved template and logic.
fn render_tests(spec: &EndpointSpec) -> String {
    let EndpointSpec {
        provider,
        name,
        path,
        handler_fn,
        ..
    } = spec;
    let provider = *provider;
    let fixture = fixture_body(provider);
    let signature_helper = signature_helper(provider);
    // Only the timestamped presets (Stripe, Slack) bind a request timestamp;
    // emitting an unused one for the others would warn on every build.
    let timestamp_fn = if provider.is_timestamped() {
        "    fn now_secs() -> u64 {\n        \
         std::time::SystemTime::now()\n            \
         .duration_since(std::time::UNIX_EPOCH)\n            \
         .expect(\"system clock is after the Unix epoch\")\n            \
         .as_secs()\n    }\n\n"
    } else {
        ""
    };
    let cases = render_test_cases(spec);

    format!(
        r#"#[cfg(test)]
mod tests {{
    use super::*;
    use autumn_web::config::AutumnConfig;
    use autumn_web::test::{{TestApp, TestClient}};
    use autumn_web::webhook::{{
        WebhookConfig, WebhookEndpointConfig, WebhookProvider, hmac_sha256_hex,
    }};

    /// Fixture-only secret. Real deployments read the secret from the
    /// `{secret_env}` environment variable via `autumn.toml`'s `secret_env`.
    const TEST_SECRET: &str = "test-{provider_name}-webhook-secret-32-bytes";

    /// A delivery shaped like the real thing. Replace it with a captured
    /// payload from your provider dashboard as you fill the handlers in.
    const BODY: &str = {fixture};

    fn test_config() -> AutumnConfig {{
        AutumnConfig {{
            security: autumn_web::security::SecurityConfig {{
                webhooks: WebhookConfig {{
                    endpoints: vec![WebhookEndpointConfig::new(
                        "{name}",
                        "{path}",
                        WebhookProvider::{variant},
                        TEST_SECRET,
                    )],
                    ..Default::default()
                }},
                ..Default::default()
            }},
            ..Default::default()
        }}
    }}

    fn client() -> TestClient {{
        TestApp::new()
            .config(test_config())
            .routes(routes![{handler_fn}])
            .build()
    }}

{timestamp_fn}{signature_helper}{cases}}}
"#,
        provider_name = provider.as_str(),
        variant = provider.variant(),
        secret_env = spec.secret_env,
    )
}

/// Render the four `#[tokio::test]` cases: accepted, missing signature,
/// wrong signature, replayed delivery.
fn render_test_cases(spec: &EndpointSpec) -> String {
    let provider = spec.provider;
    let path = spec.path.as_str();
    let valid_headers = request_headers(provider, valid_signature_expr(provider));
    let invalid_headers = request_headers(provider, invalid_signature_expr(provider));
    let timestamp_binding = if provider.is_timestamped() {
        "        let timestamp = now_secs();\n"
    } else {
        ""
    };

    format!(
        r#"    #[tokio::test]
    async fn valid_signature_is_accepted() {{
{timestamp_binding}        client()
            .post("{path}")
{valid_headers}            .body(BODY)
            .send()
            .await
            .assert_status(200);
    }}

    #[tokio::test]
    async fn missing_signature_header_is_rejected() {{
        // A delivery with no signature at all cannot be verified: it is
        // rejected as a malformed request before the handler runs.
        client()
            .post("{path}")
            .body(BODY)
            .send()
            .await
            .assert_status(400);
    }}

    #[tokio::test]
    async fn invalid_signature_is_rejected() {{
        // Well-formed but wrong: this is the unauthorized case, not a
        // malformed-request one.
{timestamp_binding}        client()
            .post("{path}")
{invalid_headers}            .body(BODY)
            .send()
            .await
            .assert_status(401);
    }}

    #[tokio::test]
    async fn replayed_delivery_is_rejected() {{
        // Replay protection keys on {delivery_source}, so re-sending the same
        // delivery is rejected with 409 inside the replay window.
        let app = client();
{timestamp_binding}        app.post("{path}")
{valid_headers}            .body(BODY)
            .send()
            .await
            .assert_status(200);
        app.post("{path}")
{valid_headers}            .body(BODY)
            .send()
            .await
            .assert_status(409);
    }}
"#,
        delivery_source = provider.delivery_id_source(),
    )
}

/// The fixture request body used by every generated test.
const fn fixture_body(provider: Provider) -> &'static str {
    match provider {
        // Stripe's replay key is the JSON `id`; its event type is the JSON `type`.
        Provider::Stripe => {
            r##"r#"{"id":"evt_test_1","type":"payment_intent.succeeded","data":{"object":{}}}"#"##
        }
        // GitHub carries both the delivery id and the event type in headers.
        Provider::Github => {
            r##"r#"{"ref":"refs/heads/main","repository":{"full_name":"acme/app"}}"#"##
        }
        // Slack's replay key is the JSON `event_id`; the envelope wraps the event.
        Provider::Slack => {
            r##"r#"{"event_id":"Ev00000001","type":"event_callback","event":{"type":"app_mention"}}"#"##
        }
        Provider::Generic => r##"r#"{"data":{"id":"1"}}"#"##,
    }
}

/// The per-provider signature helper the generated tests use.
const fn signature_helper(provider: Provider) -> &'static str {
    match provider {
        Provider::Stripe => {
            "    /// Stripe signs `{timestamp}.{raw_body}` and sends `t=…,v1=…`.\n\
             \x20   fn signature(body: &str, timestamp: u64) -> String {\n\
             \x20       let signed = format!(\"{timestamp}.{body}\");\n\
             \x20       let digest = hmac_sha256_hex(TEST_SECRET.as_bytes(), signed.as_bytes());\n\
             \x20       format!(\"t={timestamp},v1={digest}\")\n\
             \x20   }\n\n"
        }
        Provider::Github | Provider::Generic => {
            "    /// Both presets sign the raw body and send `sha256=<hex>`.\n\
             \x20   fn signature(body: &str) -> String {\n\
             \x20       format!(\n\
             \x20           \"sha256={}\",\n\
             \x20           hmac_sha256_hex(TEST_SECRET.as_bytes(), body.as_bytes())\n\
             \x20       )\n\
             \x20   }\n\n"
        }
        Provider::Slack => {
            "    /// Slack signs `v0:{timestamp}:{raw_body}` and sends `v0=<hex>`.\n\
             \x20   fn signature(body: &str, timestamp: u64) -> String {\n\
             \x20       let signed = format!(\"v0:{timestamp}:{body}\");\n\
             \x20       format!(\n\
             \x20           \"v0={}\",\n\
             \x20           hmac_sha256_hex(TEST_SECRET.as_bytes(), signed.as_bytes())\n\
             \x20       )\n\
             \x20   }\n\n"
        }
    }
}

/// The expression a generated test uses for a *valid* signature header value.
const fn valid_signature_expr(provider: Provider) -> &'static str {
    if provider.is_timestamped() {
        "&signature(BODY, timestamp)"
    } else {
        "&signature(BODY)"
    }
}

/// The expression for a well-formed but wrong signature — the 401 case (a
/// malformed one is a 400 instead).
const fn invalid_signature_expr(provider: Provider) -> &'static str {
    match provider {
        Provider::Stripe => "&format!(\"t={timestamp},v1={}\", \"0\".repeat(64))",
        Provider::Github | Provider::Generic => "&format!(\"sha256={}\", \"0\".repeat(64))",
        Provider::Slack => "&format!(\"v0={}\", \"0\".repeat(64))",
    }
}

/// The `.header(…)` lines a signed request needs, with `signature_expr`
/// supplying the signature header's value.
fn request_headers(provider: Provider, signature_expr: &str) -> String {
    match provider {
        Provider::Stripe => {
            format!("            .header(\"stripe-signature\", {signature_expr})\n")
        }
        Provider::Github => format!(
            "            .header(\"x-hub-signature-256\", {signature_expr})\n\
             \x20           .header(\"x-github-event\", \"push\")\n\
             \x20           .header(\"x-github-delivery\", \"00000000-0000-0000-0000-000000000001\")\n"
        ),
        Provider::Slack => format!(
            "            .header(\"x-slack-signature\", {signature_expr})\n\
             \x20           .header(\"x-slack-request-timestamp\", &timestamp.to_string())\n"
        ),
        Provider::Generic => format!(
            "            .header(\"x-webhook-signature\", {signature_expr})\n\
             \x20           .header(\"x-webhook-event\", \"example.created\")\n\
             \x20           .header(\"x-webhook-delivery\", \"delivery-1\")\n"
        ),
    }
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

    fn project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n\n\
             [dev-dependencies]\ntokio = { version = \"1\", features = [\"macros\"] }\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("autumn.toml"),
            "[server]\nhost = \"127.0.0.1\"\nport = 3000\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), default_main()).unwrap();
        tmp
    }

    fn generated(tmp: &TempDir, provider: &str, name: &str) -> String {
        let plan = plan_webhook(tmp.path(), provider, name, &WebhookOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        fs::read_to_string(
            tmp.path()
                .join("src/webhooks")
                .join(format!("{}.rs", crate::generate::naming::snake(name))),
        )
        .unwrap()
    }

    // ── AC #1: post route + shipped extractor ────────────────────────────

    #[test]
    fn generated_handler_uses_post_route_and_signed_webhook_extractor() {
        let tmp = project();
        let src = generated(&tmp, "stripe", "Payments");

        assert!(
            src.contains("#[post(\"/webhooks/stripe\")]"),
            "handler must expose a #[post] route at the provider path:\n{src}"
        );
        assert!(
            src.contains("webhook: SignedWebhook"),
            "handler must take the shipped SignedWebhook extractor:\n{src}"
        );
        assert!(
            !src.contains("hmac_sha256_hex(") || src.contains("mod tests"),
            "no hand-rolled HMAC outside the test module:\n{src}"
        );
        let handler_body = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !handler_body.contains("Hmac") && !handler_body.contains("hmac"),
            "handler must not hand-roll signature verification:\n{handler_body}"
        );
    }

    // ── AC #2: event dispatch with stub arms + default ack-and-ignore ────

    #[test]
    fn generated_handler_dispatches_on_event_type_with_stubs_and_default_arm() {
        let tmp = project();
        let src = generated(&tmp, "stripe", "Payments");

        assert!(
            src.contains("webhook.event_type()"),
            "handler must dispatch on event_type():\n{src}"
        );
        assert!(
            src.contains("\"payment_intent.succeeded\" =>"),
            "expected a Stripe stub arm:\n{src}"
        );
        assert!(src.contains("TODO"), "stub arms must be clearly marked");
        assert!(
            src.contains("_ =>"),
            "expected a default acknowledge-and-ignore arm:\n{src}"
        );
    }

    // ── AC #3: provider presets ──────────────────────────────────────────

    #[test]
    fn every_supported_provider_preset_generates_its_own_shape() {
        for (provider, variant, arm) in [
            ("stripe", "Stripe", "\"payment_intent.succeeded\" =>"),
            ("github", "Github", "\"push\" =>"),
            ("slack", "Slack", "\"app_mention\" =>"),
            ("generic", "Generic", "\"example.created\" =>"),
        ] {
            let tmp = project();
            let src = generated(&tmp, provider, "Intake");
            assert!(
                src.contains(&format!("#[post(\"/webhooks/{provider}\")]")),
                "{provider}: wrong route path:\n{src}"
            );
            assert!(
                src.contains(&format!("WebhookProvider::{variant}")),
                "{provider}: generated test config must use WebhookProvider::{variant}:\n{src}"
            );
            assert!(
                src.contains(arm),
                "{provider}: missing preset stub arm:\n{src}"
            );

            let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
            assert!(
                toml.contains(&format!("provider = \"{provider}\"")),
                "{provider}: autumn.toml endpoint must name the provider preset:\n{toml}"
            );
        }
    }

    #[test]
    fn unknown_provider_is_rejected_with_the_supported_list() {
        let tmp = project();
        let err = plan_webhook(tmp.path(), "twilio", "Sms", &WebhookOptions::default())
            .expect_err("unknown provider must be rejected");
        let message = err.to_string();
        assert!(message.contains("twilio"), "{message}");
        assert!(message.contains("generic"), "{message}");
    }

    // ── AC #4: autumn.toml endpoint stub ─────────────────────────────────

    #[test]
    fn autumn_toml_gets_a_secret_env_endpoint_stub_with_replay_protection() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();

        assert!(
            toml.contains("[[security.webhooks.endpoints]]"),
            "expected an endpoint stub:\n{toml}"
        );
        assert!(toml.contains("name = \"payments\""), "{toml}");
        assert!(toml.contains("path = \"/webhooks/stripe\""), "{toml}");
        assert!(toml.contains("provider = \"stripe\""), "{toml}");
        assert!(
            toml.contains("secret_env = \"STRIPE_WEBHOOK_SECRET\""),
            "{toml}"
        );
        assert!(toml.contains("replay_protection = true"), "{toml}");
        assert!(
            !toml.contains("\nsecret ="),
            "a plaintext secret must never be written:\n{toml}"
        );
    }

    #[test]
    fn autumn_toml_exempts_the_webhook_path_from_csrf() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(
            toml.contains("[security.csrf]") && toml.contains("\"/webhooks/stripe\""),
            "the prod profile enables CSRF, so the webhook path must be exempt:\n{toml}"
        );
        assert!(
            toml.contains("captcha_exempt_paths"),
            "a configured CAPTCHA would challenge provider callbacks too:\n{toml}"
        );
    }

    #[test]
    fn autumn_toml_declares_the_replay_backend_so_prod_boot_validation_passes() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(
            toml.contains("[security.webhooks.replay]"),
            "expected a replay backend section:\n{toml}"
        );
        assert!(
            toml.contains("redis"),
            "prod guidance for the redis backend must be present:\n{toml}"
        );
    }

    #[test]
    fn regenerating_the_same_webhook_does_not_duplicate_the_toml_stub() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let plan =
            plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default()).unwrap();
        plan.execute(Flags {
            force: true,
            ..Flags::default()
        })
        .unwrap();

        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert_eq!(
            toml.matches("[[security.webhooks.endpoints]]").count(),
            1,
            "endpoint stub must be idempotent:\n{toml}"
        );
        assert_eq!(
            toml.matches("\"/webhooks/stripe\"").count(),
            3,
            "one endpoint path + the csrf and captcha exemptions, no duplicates:\n{toml}"
        );
    }

    #[test]
    fn a_second_webhook_on_the_same_path_is_rejected_with_a_path_hint() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let err = plan_webhook(tmp.path(), "stripe", "Billing", &WebhookOptions::default())
            .expect_err("a duplicate endpoint path must be rejected");
        let message = err.to_string();
        assert!(message.contains("/webhooks/stripe"), "{message}");
        assert!(message.contains("--path"), "{message}");
    }

    #[test]
    fn missing_autumn_toml_is_created_with_the_endpoint_stub() {
        let tmp = project();
        fs::remove_file(tmp.path().join("autumn.toml")).unwrap();
        generated(&tmp, "github", "Repo");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(toml.contains("[[security.webhooks.endpoints]]"), "{toml}");
    }

    #[test]
    fn path_and_secret_env_overrides_are_honored_everywhere() {
        let tmp = project();
        let options = WebhookOptions {
            path: Some("/hooks/billing".to_owned()),
            secret_env: Some("BILLING_SECRET".to_owned()),
        };
        let plan = plan_webhook(tmp.path(), "stripe", "Billing", &options).unwrap();
        plan.execute(Flags::default()).unwrap();

        let src = fs::read_to_string(tmp.path().join("src/webhooks/billing.rs")).unwrap();
        assert!(src.contains("#[post(\"/hooks/billing\")]"), "{src}");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(toml.contains("path = \"/hooks/billing\""), "{toml}");
        assert!(toml.contains("secret_env = \"BILLING_SECRET\""), "{toml}");
    }

    #[test]
    fn an_invalid_path_override_is_rejected() {
        let tmp = project();
        for bad in ["hooks/billing", "/"] {
            let options = WebhookOptions {
                path: Some(bad.to_owned()),
                secret_env: None,
            };
            assert!(
                plan_webhook(tmp.path(), "stripe", "Billing", &options).is_err(),
                "path {bad:?} must be rejected"
            );
        }
    }

    // ── AC #5: app wiring ────────────────────────────────────────────────

    #[test]
    fn the_route_is_registered_in_main_rs_and_next_steps_are_printed() {
        let tmp = project();
        let plan =
            plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default()).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("STRIPE_WEBHOOK_SECRET")),
            "the secret env var must be part of the printed next steps: {:?}",
            plan.warnings
        );
        plan.execute(Flags::default()).unwrap();

        let main_rs = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main_rs.contains("mod webhooks;"), "{main_rs}");
        assert!(
            main_rs.contains("webhooks::payments::payments_webhook"),
            "the generated route must be registered in routes![...]:\n{main_rs}"
        );
        let mod_rs = fs::read_to_string(tmp.path().join("src/webhooks/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod payments;"), "{mod_rs}");
    }

    // ── AC #6: generated tests ───────────────────────────────────────────

    #[test]
    fn generated_test_module_covers_valid_invalid_and_replayed_deliveries() {
        let tmp = project();
        let src = generated(&tmp, "stripe", "Payments");
        let tests = src
            .split_once("#[cfg(test)]")
            .expect("a generated test module")
            .1;

        assert!(tests.contains("assert_status(200)"), "{tests}");
        assert!(
            tests.contains("assert_status(401)"),
            "an invalid signature must be asserted as 401:\n{tests}"
        );
        assert!(
            tests.contains("assert_status(400)"),
            "a missing signature header must be asserted as 400:\n{tests}"
        );
        assert!(
            tests.contains("assert_status(409)"),
            "a replayed delivery id must be asserted as 409:\n{tests}"
        );
        assert!(
            tests.contains("replay"),
            "the replay case must be named:\n{tests}"
        );
        assert!(
            tests.contains("\"id\""),
            "the fixture body needs an `id` — Stripe's replay key comes from it:\n{tests}"
        );
    }

    // ── Cargo wiring ─────────────────────────────────────────────────────

    #[test]
    fn cargo_toml_gets_serde_json_and_tokio_test_features() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("serde_json"), "{cargo}");
        assert!(cargo.contains("tracing"), "{cargo}");
        assert!(
            cargo.contains("\"rt\"") && cargo.contains("\"macros\""),
            "#[tokio::test] needs the rt + macros dev features:\n{cargo}"
        );
    }

    // ── destroy round trip ───────────────────────────────────────────────

    #[test]
    fn generate_then_destroy_round_trips_to_the_original_project_state() {
        let tmp = project();
        let main_path = tmp.path().join("src/main.rs");
        let toml_path = tmp.path().join("autumn.toml");
        let original_main = fs::read_to_string(&main_path).unwrap();
        let original_toml = fs::read_to_string(&toml_path).unwrap();

        let plan =
            plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();

        let destroy =
            plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default()).unwrap();
        destroy.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/webhooks").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&toml_path).unwrap(), original_toml);
    }
}
