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
//!   `secret_env`, replay protection on) plus the `[security.webhooks.replay]`
//!   backend declaration. Nothing else: the framework derives the endpoint's
//!   CSRF/submit-token/CAPTCHA exemptions from that block on every boot.
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
            "generic" => Ok(Self::Generic),
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

    /// A minimal payload for the `autumn webhook sim` line in the printed next
    /// steps — shaped so the simulated delivery actually reaches a stub arm
    /// (Stripe and Slack carry their event type and replay id in the body).
    const fn sim_payload(self) -> &'static str {
        match self {
            Self::Stripe => r#"{"id":"evt_1","type":"payment_intent.succeeded"}"#,
            Self::Github => r#"{"ref":"refs/heads/main"}"#,
            Self::Slack => {
                r#"{"event_id":"Ev1","type":"event_callback","event":{"type":"app_mention"}}"#
            }
            Self::Generic => r#"{"data":{}}"#,
        }
    }

    /// The `--event` argument the printed `autumn webhook sim` line needs, for
    /// the presets whose event type travels in a header.
    ///
    /// Without it the simulator announces its default `sim.event`, which no
    /// generated arm dispatches on — so the delivery would fall through to
    /// acknowledge-and-ignore and prove nothing about the user's handler. Stripe
    /// and Slack carry the type in the payload instead, so they need no flag.
    const fn sim_event(self) -> Option<&'static str> {
        match self {
            Self::Github => Some("push"),
            Self::Generic => Some("example.created"),
            Self::Stripe | Self::Slack => None,
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
    /// The `<Name>` argument exactly as the user typed it, so the generated
    /// module can echo back the command that produced it.
    display_name: String,
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
/// preset, `--path`, or `--secret-env` is invalid, when `src/main.rs` cannot be
/// read, or when `autumn.toml` already configures a *different* endpoint on the
/// same path (which would fail the framework's own duplicate-path validation at
/// boot).
pub fn plan_webhook(
    project_root: &Path,
    provider: &str,
    name: &str,
    options: &WebhookOptions,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    let spec = resolve_spec(provider, name, options)?;
    let snake_name = spec.name.clone();

    // `src/webhooks.rs` and `src/webhooks/mod.rs` are two spellings of the same
    // module: emitting the directory alongside an existing file is rustc E0761
    // ("file for module found at both …"), which stops the whole project
    // compiling. Say so instead of breaking the build.
    if project_root.join("src").join("webhooks.rs").is_file() {
        return Err(GenerateError::Config(
            "src/webhooks.rs already exists, and this generator needs src/webhooks/ — rustc              rejects both spellings of one module (E0761). Move it to              src/webhooks/mod.rs first, then re-run."
                .to_owned(),
        ));
    }

    let mut plan = Plan::new(project_root);
    // Keyed on the RESOLVED endpoint, not the raw arguments: `destroy` is
    // documented not to need a `--path`/`--secret-env` repeated, because
    // `plan_webhook_for_revert` reads both back out of `autumn.toml`. Keyed on
    // the arguments, the recovered run would look like a different command and
    // be refused its own baseline (issue #1835).
    plan.invocation = crate::generate::provenance::resolved_invocation(
        project_root,
        &[
            "webhook",
            spec.provider.as_str(),
            &spec.name,
            &spec.path,
            &spec.secret_env,
        ],
    );

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
    let updated_main = update_main_rs(&main_existing, &["webhooks"], &route_entries);
    // `update_main_rs` can only splice into an existing `routes![…]`. A main.rs
    // without one (hand-written router, or a builder that never called
    // `.routes(…)`) would otherwise get the module declaration and no route —
    // silently unreachable, and the signature checks would never run.
    if !updated_main.contains(&route_entries[0]) {
        plan.warn(format!(
            "src/main.rs has no `routes![…]` list to register the handler in — add              `{}` to your router by hand, or the endpoint will not be mounted.",
            route_entries[0]
        ));
    }
    plan.modify(main_path.clone(), updated_main);
    // `mod webhooks;` is shared infrastructure (see
    // `emit::SHARED_MAIN_MODULE_NAMES`) — only this webhook's own route entry
    // is reverted here.
    plan.push_revert(crate::generate::emit::Revert::RoutesEntries {
        path: main_path,
        entries: route_entries,
    });

    // ── autumn.toml — endpoint stub + replay backend ──────────────────────
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

    Ok(plan)
}

/// Plan builder for `autumn destroy webhook <provider> <Name>` (`autumn destroy`,
/// issue #1048).
///
/// `destroy` mirrors `generate` argument-for-argument across this CLI, but the
/// two overrides here are *recoverable*: the generated endpoint block records its
/// own `path` and `secret_env` under the endpoint name, which is derived from
/// `<Name>` alone. So rather than making the user remember flags from days ago,
/// this adopts the recorded values whenever they are not passed again — without
/// them, a webhook generated with `--path` would leave its `autumn.toml` block
/// behind (the revert searches for the default route) and its handler would fail
/// the divergence guard (the rendered content embeds the path).
///
/// An explicitly passed `--path`/`--secret-env` still wins, and a project with
/// no recorded endpoint falls back to the same defaults `generate` uses.
///
/// # Errors
///
/// Same as [`plan_webhook`].
pub fn plan_webhook_for_revert(
    project_root: &Path,
    provider: &str,
    name: &str,
    options: &WebhookOptions,
) -> Result<Plan, GenerateError> {
    plan_webhook(
        project_root,
        provider,
        name,
        &adopt_recorded_overrides(project_root, name, options),
    )
}

/// Fill in `--path`/`--secret-env` from the `autumn.toml` endpoint recorded under
/// this webhook's name, for any the caller did not pass.
fn adopt_recorded_overrides(
    project_root: &Path,
    name: &str,
    options: &WebhookOptions,
) -> WebhookOptions {
    if options.path.is_some() && options.secret_env.is_some() {
        return options.clone();
    }
    let endpoint_name = snake(name);
    let existing = read_or_empty(&project_root.join("autumn.toml"));
    let Ok(doc) = existing.parse::<toml_edit::DocumentMut>() else {
        return options.clone();
    };
    let recorded = doc
        .get("security")
        .and_then(|security| security.get("webhooks"))
        .and_then(|webhooks| webhooks.get("endpoints"))
        .and_then(toml_edit::Item::as_array_of_tables)
        .into_iter()
        .flatten()
        .find(|endpoint| {
            endpoint.get("name").and_then(|value| value.as_str()) == Some(endpoint_name.as_str())
        });
    let Some(recorded) = recorded else {
        return options.clone();
    };

    WebhookOptions {
        path: options.path.clone().or_else(|| {
            recorded
                .get("path")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        }),
        secret_env: options.secret_env.clone().or_else(|| {
            recorded
                .get("secret_env")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        }),
    }
}

/// Resolve one invocation's arguments into the [`EndpointSpec`] every renderer
/// works from, validating each of them.
///
/// Shared by [`plan_webhook`] and [`next_steps`] so the plan and the printed
/// instructions can never disagree about the path or the secret variable.
///
/// # Errors
///
/// Returns [`GenerateError`] when the name, provider preset, `--path`, or
/// `--secret-env` is invalid.
fn resolve_spec(
    provider: &str,
    name: &str,
    options: &WebhookOptions,
) -> Result<EndpointSpec, GenerateError> {
    validate_resource_name(name)?;
    let provider = Provider::parse(provider)?;
    let snake_name = snake(name);
    let path = match options.path.as_deref() {
        Some(path) => validate_route_path(path)?,
        None => format!("/webhooks/{}", provider.as_str()),
    };
    let secret_env = match options.secret_env.as_deref() {
        Some(variable) => validate_secret_env(variable)?,
        None => provider.default_secret_env(),
    };
    Ok(EndpointSpec {
        provider,
        name: snake_name.clone(),
        display_name: name.to_owned(),
        path,
        secret_env,
        handler_fn: format!("{snake_name}_webhook"),
    })
}

/// The post-generation next steps, printed to stdout by `autumn generate
/// webhook` after a successful, non-dry run (issue #1366 AC #5).
///
/// Deliberately not `plan.warn`: across the generator family that is for
/// *conditional* advisories, and a clean run printing three stderr `Warning:`
/// lines reads as three problems. The one genuine advisory here — an
/// `autumn.toml` that could not be parsed — stays a warning.
///
/// `None` when the arguments do not resolve, in which case `plan_webhook`
/// already failed with the reason.
#[must_use]
pub fn next_steps(provider: &str, name: &str, options: &WebhookOptions) -> Option<String> {
    let spec = resolve_spec(provider, name, options).ok()?;
    let EndpointSpec {
        provider,
        name,
        path,
        secret_env,
        ..
    } = &spec;
    Some(format!(
        "\nNext steps:\n\
         \x20 1. Set the signing secret — the app refuses to start while a configured\n\
         \x20    endpoint has none. Add {secret_env}=… to `.env` (gitignored, auto-loaded\n\
         \x20    in dev/test) or export it; autumn.toml only names the variable, so the\n\
         \x20    secret itself is never committed.\n\
         \x20 2. Point {dashboard} at POST {path}. The endpoint is\n\
         \x20    installed from autumn.toml's `[[security.webhooks.endpoints]]`, so there\n\
         \x20    is no builder wiring to add.\n\
         \x20 3. Try it locally without the provider — this reaches a stub arm, so a\n\
         \x20    filled-in handler actually runs:\n\
         \x20      autumn webhook sim {slug} http://localhost:3000{path} \\\n\
         \x20        --secret \"${secret_env}\" --payload '{payload}'{event_flag}\n\
         \x20 4. Fill in the `on_*` stub functions in src/webhooks/{name}.rs.\n\
         \x20 5. Before deploying: replay protection is on, and its default `memory`\n\
         \x20    backend is process-local — production config validation rejects it. Set\n\
         \x20    [security.webhooks.replay] backend = \"redis\" with a redis url (needs\n\
         \x20    autumn-web's `redis` feature).\n",
        dashboard = provider.dashboard_hint(),
        slug = provider.as_str(),
        payload = provider.sim_payload(),
        event_flag = provider
            .sim_event()
            .map_or_else(String::new, |event| format!(
                " \\\n           --event {event}"
            )),
    ))
}

/// Validate a `--path` override.
///
/// Three separate concerns, all of them load-bearing:
///
/// 1. `WebhookEndpointConfig::validate` requires an absolute, non-root path, so
///    anything else would fail the framework's own config validation at boot.
/// 2. The value is interpolated into generated Rust source (`#[post("…")]` and
///    the generated tests' `.post("…")`), so a quote, backslash, or control
///    character would emit code that means something other than a path.
/// 3. The webhook registry looks endpoints up by an **exact** path match
///    (`WebhookRegistry::endpoint_for_path`), so an axum path parameter
///    (`/hooks/{tenant}`) or wildcard would route requests to a handler whose
///    extractor then cannot find its endpoint — a 500 on every real delivery,
///    while the generated tests (which post the literal template) still pass.
///    Rejecting it here is the only place that trap is visible.
fn validate_route_path(path: &str) -> Result<String, GenerateError> {
    let trimmed = path.trim();
    if !trimmed.starts_with('/') || trimmed == "/" {
        return Err(GenerateError::Config(format!(
            "invalid webhook path {path:?}: it must start with '/' and must not be the site \
             root (e.g. `--path /webhooks/stripe-billing`)"
        )));
    }
    if let Some(bad) = trimmed.chars().find(|ch| !is_allowed_path_char(*ch)) {
        return Err(GenerateError::Config(format!(
            "invalid webhook path {path:?}: character {bad:?} is not allowed. Webhook endpoint \
             paths are matched exactly — no path parameters (`{{tenant}}`) or wildcards — and \
             the path is emitted into generated Rust source, so only unreserved URL path \
             characters are accepted (e.g. `--path /webhooks/stripe-billing`)."
        )));
    }
    Ok(trimmed.to_owned())
}

/// Whether `ch` is safe in a generated webhook route path: the RFC 3986
/// unreserved and sub-delimiter path characters, minus the ones that would make
/// the path a template (`{`, `}`, `*`) rather than a literal.
fn is_allowed_path_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || "-._~/%:@!$&'()+,;=".contains(ch)
}

/// Validate a `--secret-env` override.
///
/// The value names an environment variable, and it is interpolated into
/// generated Rust doc comments and into printed next steps. Restricting it to a
/// C-identifier — what a POSIX shell can `export` in the first place — keeps it
/// from carrying a newline or a quote into either sink.
fn validate_secret_env(name: &str) -> Result<String, GenerateError> {
    let trimmed = name.trim();
    let valid = !trimmed.is_empty()
        && trimmed
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if !valid {
        return Err(GenerateError::Config(format!(
            "invalid secret environment variable name {name:?}: it must be a non-empty \
             identifier of ASCII letters, digits, and underscores, not starting with a digit \
             (e.g. `--secret-env PARTNER_WEBHOOK_SECRET`)"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Add the `autumn.toml` actions: the endpoint stub and the replay-backend
/// declaration.
fn plan_autumn_toml(
    plan: &mut Plan,
    project_root: &Path,
    spec: &EndpointSpec,
) -> Result<(), GenerateError> {
    let toml_path = project_root.join("autumn.toml");
    let existing = read_or_empty(&toml_path);
    reject_conflicting_endpoint(&existing, spec)?;

    // A same-named endpoint the user has since tuned (extra keys, or filled-in
    // rotation variables) is only partially owned by the generator from here on:
    // regeneration refreshes the three keys it owns, and `destroy` will not
    // delete the block at all. Say so once rather than surprising anyone.
    if endpoint_diverges_from_stub(&existing, spec) {
        plan.warn(format!(
            "autumn.toml's {:?} webhook endpoint carries hand edits: generating refreshes only              its path/provider/secret_env, and `autumn destroy webhook` will leave the block in              place rather than delete your changes.",
            spec.name
        ));
    }

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
            // Either the file does not parse, or `security.webhooks` uses an
            // inline form this editing path cannot extend safely. Both leave the
            // file untouched rather than risk corrupting it; the user gets the
            // exact blocks to paste, replay backend included, so a hand-finished
            // config still boots in production.
            plan.warn(format!(
                "Could not edit {} (it does not parse, or `security.webhooks` uses an inline \
                 form — convert `endpoints` to `[[security.webhooks.endpoints]]` table form). \
                 Add this by hand:\n{}",
                toml_path.display(),
                manual_config_block(spec)
            ));
        }
    }
    Ok(())
}

/// Whether `autumn.toml` already holds a same-named endpoint that the generator
/// no longer fully owns — it carries a key outside the set this generator emits,
/// a filled-in `previous_secret_envs`, or `replay_protection = false`.
fn endpoint_diverges_from_stub(existing: &str, spec: &EndpointSpec) -> bool {
    let Ok(doc) = existing.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("security")
        .and_then(|security| security.get("webhooks"))
        .and_then(|webhooks| webhooks.get("endpoints"))
        .and_then(toml_edit::Item::as_array_of_tables)
        .into_iter()
        .flatten()
        .find(|endpoint| endpoint.get("name").and_then(|v| v.as_str()) == Some(spec.name.as_str()))
        .is_some_and(|endpoint| !is_generated_endpoint_stub(endpoint))
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
    else {
        return Ok(());
    };

    // Read both spellings: `[[security.webhooks.endpoints]]` table form (what
    // this generator writes) and an inline `endpoints = [{ … }]` array, which the
    // editing path below cannot extend but which still claims paths at boot.
    let table_form = endpoints
        .as_array_of_tables()
        .into_iter()
        .flatten()
        .map(|endpoint| {
            (
                endpoint.get("name").and_then(|v| v.as_str()),
                endpoint.get("path").and_then(|v| v.as_str()),
            )
        });
    let inline_form = endpoints
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(toml_edit::Value::as_inline_table)
        .map(|endpoint| {
            (
                endpoint.get("name").and_then(|v| v.as_str()),
                endpoint.get("path").and_then(|v| v.as_str()),
            )
        });

    for (name, path) in table_form.chain(inline_form) {
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

/// The config blocks, as text, for the "could not edit autumn.toml" fallback.
///
/// Includes the replay backend as well as the endpoint: an endpoint pasted
/// without it would boot in dev and then fail production config validation.
fn manual_config_block(spec: &EndpointSpec) -> String {
    format!(
        "\n[security.webhooks.replay]\nbackend = \"memory\"  # production must use redis\n\n\
         [[security.webhooks.endpoints]]\nname = \"{}\"\npath = \"{}\"\nprovider = \"{}\"\n\
         secret_env = \"{}\"\nreplay_protection = true\n",
        spec.name,
        spec.path,
        spec.provider.as_str(),
        spec.secret_env,
    )
}

/// The first line of the comment block [`upsert_webhook_endpoint`] writes above
/// `[security.webhooks.replay]`. Removal uses it to tell this generator's own
/// decor apart from document trivia parked in front of it.
const REPLAY_COMMENT_MARKER: &str = "\n# Replay-protection storage for signed webhooks.";

/// The same, for the comment block above `[[security.webhooks.endpoints]]`.
const ENDPOINT_COMMENT_MARKER: &str = "\n# Signed webhook intake generated by";

/// Insert (idempotently) this webhook's `autumn.toml` configuration: the
/// `[security.webhooks.replay]` backend declaration and the
/// `[[security.webhooks.endpoints]]` entry.
///
/// Edits are made through `toml_edit` so comments, key order, and hand-crafted
/// array layout in the rest of the file survive untouched. Returns `None` when
/// the document does not parse — the caller then leaves the file alone.
fn upsert_webhook_endpoint(existing: &str, spec: &EndpointSpec) -> Option<String> {
    use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value, value};

    let mut doc = existing.parse::<DocumentMut>().ok()?;

    // A new root table is appended AFTER the document's trailing trivia, so a
    // file that ends in comments — `autumn new`'s own autumn.toml ends with the
    // commented-out `[health]` probe paths and `[session]` block — would have
    // them re-parented under our last inserted header. Uncommenting one would
    // then silently set a key in the wrong table. Detach the trivia here and
    // prepend it to the FIRST table this function inserts, which puts it back
    // above everything generated and so still inside the table it documents.
    // (Re-attaching it as trailing trivia instead would only move the problem to
    // the last generated table.)
    let mut pending_trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
    if !pending_trailing.is_empty() {
        doc.set_trailing("");
    }

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
        // Deliberately points at redis only. There *is* an
        // `allow_memory_in_production` escape hatch, but a developer meeting a
        // hard boot failure at deploy time would take the one-line bool and
        // silently lose cross-replica replay protection — so that caveat stays
        // in the guide, where its single-replica precondition can be stated.
        replay.decor_mut().set_prefix(
            std::mem::take(&mut pending_trailing)
                + REPLAY_COMMENT_MARKER
                + " \"memory\" is process-local:\n\
             # fine for tests and development, but production config validation rejects it,\n\
             # so a deployed app must use redis (shared across every replica). The redis\n\
             # backend needs autumn-web built with its `redis` feature:\n\
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
    if let Some(existing_entry) = endpoints
        .iter_mut()
        .find(|entry| entry.get("name").and_then(|v| v.as_str()) == Some(spec.name.as_str()))
    {
        // Regeneration (`--force`) with a changed `--path`, `--secret-env`, or
        // provider must not leave the old values behind: the handler would move
        // while the config stayed put, and since the registry matches paths
        // exactly, every real delivery would 500 with no failing test to show it.
        // Only these three keys are refreshed — anything the user tuned
        // (`timestamp_tolerance_secs`, extra headers, rotation variables) is
        // theirs and is left alone.
        existing_entry.insert("path", value(spec.path.as_str()));
        existing_entry.insert("provider", value(spec.provider.as_str()));
        existing_entry.insert("secret_env", value(spec.secret_env.as_str()));
        if !existing_entry.contains_key("replay_protection") {
            existing_entry.insert("replay_protection", value(true));
        }
    } else {
        let mut endpoint = Table::new();
        // Static text: every dynamic value belongs in an escaped `value(…)` key
        // below, never in decor, which `toml_edit` emits verbatim.
        endpoint.decor_mut().set_prefix(
            std::mem::take(&mut pending_trailing)
                + "\n# Signed webhook intake generated by `autumn generate webhook`. The signing\n\
             # secret lives in the secret_env variable below — never inline it here. During\n\
             # rotation, add the previous value's variable to previous_secret_envs.\n\
             #\n\
             # This block is all the wiring the endpoint needs: Autumn installs the webhook\n\
             # registry from it at startup, and derives the endpoint's CSRF, submit-token,\n\
             # and CAPTCHA path exemptions from it on every boot (a provider callback\n\
             # carries no browser session; its signature is its authentication).\n",
        );
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

    // Nothing was inserted (a fully idempotent re-run): put the trivia back
    // exactly where it was.
    if !pending_trailing.is_empty() {
        doc.set_trailing(&pending_trailing);
    }

    // No CSRF/CAPTCHA `exempt_paths` entries are written here on purpose. The
    // framework already derives both from `security.webhooks.endpoints` on every
    // boot — `build_csrf_layer`, `build_submit_token_layer`, and
    // `build_bot_protection_layer` each call `with_exempt_path(&endpoint.path)`
    // for every configured endpoint — so a copy in `[security.csrf]
    // exempt_paths` would add no protection while introducing a second source of
    // truth that goes stale: change this block's `path` (or delete the block) and
    // the derived exemption follows, but a literal copy would keep exempting the
    // old path *and its whole subtree* from CSRF and CAPTCHA forever, with no
    // signature check behind it (the registry matches paths exactly).
    Some(doc.to_string())
}

/// The keys [`upsert_webhook_endpoint`] emits into an endpoint table. An entry
/// carrying anything else has been tuned by hand.
const GENERATED_ENDPOINT_KEYS: &[&str] = &[
    "name",
    "path",
    "provider",
    "secret_env",
    "previous_secret_envs",
    "replay_protection",
];

/// Whether an endpoint table still looks exactly like a generated stub — no key
/// outside [`GENERATED_ENDPOINT_KEYS`], no rotation variables filled in, and
/// replay protection still on.
///
/// `autumn destroy` uses this to refuse to delete config the user has since made
/// their own: `Plan::revert`'s divergence guard only covers whole `Create`d
/// files, so an in-place TOML edit has to check for itself (the same posture
/// `generate auth`'s stub removal takes).
fn is_generated_endpoint_stub(endpoint: &toml_edit::Table) -> bool {
    let unknown_key = endpoint
        .iter()
        .any(|(key, _)| !GENERATED_ENDPOINT_KEYS.contains(&key));
    let rotation_filled = endpoint
        .get("previous_secret_envs")
        .and_then(toml_edit::Item::as_array)
        .is_some_and(|envs| !envs.is_empty());
    let replay_disabled = endpoint
        .get("replay_protection")
        .and_then(toml_edit::Item::as_bool)
        == Some(false);
    !unknown_key && !rotation_filled && !replay_disabled
}

/// Whether the replay table is still the untouched `backend = "memory"` stub
/// this generator writes — so `destroy` never deletes a hand-configured redis
/// backend on its way out.
fn is_generated_replay_stub(replay: &toml_edit::Table) -> bool {
    replay.len() == 1 && replay.get("backend").and_then(toml_edit::Item::as_str) == Some("memory")
}

/// Document trivia parked in a generated table's decor prefix by
/// [`upsert_webhook_endpoint`] — the comments that trailed the file before this
/// generator inserted anything.
///
/// Removing a `toml_edit` table takes its decor with it, so removal has to hand
/// this text back to the document instead of deleting the user's comments along
/// with the block.
fn parked_trivia(prefix: &str) -> &str {
    for marker in [REPLAY_COMMENT_MARKER, ENDPOINT_COMMENT_MARKER] {
        if let Some(index) = prefix.find(marker) {
            return &prefix[..index];
        }
    }
    ""
}

/// The trivia parked in front of one table, as an owned `String`.
fn parked_trivia_of(table: &toml_edit::Table) -> String {
    parked_trivia(
        table
            .decor()
            .prefix()
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default(),
    )
    .to_owned()
}

/// Inverse of [`upsert_webhook_endpoint`] (`autumn destroy`, issue #1048).
///
/// Removes the `[[security.webhooks.endpoints]]` entry this generator added —
/// matched on name *and* path, and only while it still looks like an untouched
/// stub ([`is_generated_endpoint_stub`]) — and, once no endpoint is left, the
/// shared `[security.webhooks.replay]` block if that is still an untouched stub
/// too, collapsing any table the removals leave empty and restoring any document
/// trivia that was parked in front of them.
///
/// A no-op when the document does not parse or `[security]`/`webhooks` is
/// absent. When the endpoint is already gone but an orphaned generated replay
/// block remains, that block is still cleaned up.
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
    let Some(webhooks) = security
        .get_mut("webhooks")
        .and_then(toml_edit::Item::as_table_mut)
    else {
        return existing.to_owned();
    };

    // Trivia the removals would otherwise take with them, in document order.
    let mut salvaged = String::new();

    if let Some(endpoints) = webhooks
        .get_mut("endpoints")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
    {
        let ours = endpoints.iter().position(|endpoint| {
            endpoint.get("name").and_then(|v| v.as_str()) == Some(name)
                && endpoint.get("path").and_then(|v| v.as_str()) == Some(route_path)
                && is_generated_endpoint_stub(endpoint)
        });
        match ours {
            Some(index) => {
                if let Some(endpoint) = endpoints.get(index) {
                    salvaged.push_str(&parked_trivia_of(endpoint));
                }
                endpoints.remove(index);
                if endpoints.is_empty() {
                    webhooks.remove("endpoints");
                }
            }
            // Not ours, hand-edited, or already destroyed: leave the endpoints
            // alone. An orphaned replay stub below is still cleaned up.
            None if !endpoints.is_empty() => return existing.to_owned(),
            None => {
                webhooks.remove("endpoints");
            }
        }
    }

    // With no endpoint left, the shared replay backend has nothing to configure.
    // Reached both after removing the last endpoint and when the user removed it
    // by hand, so an orphaned stub never lingers.
    if !webhooks.contains_key("endpoints") {
        let generated_replay = webhooks
            .get("replay")
            .and_then(toml_edit::Item::as_table)
            .filter(|replay| is_generated_replay_stub(replay))
            .map(parked_trivia_of);
        if let Some(trivia) = generated_replay {
            salvaged.insert_str(0, &trivia);
            webhooks.remove("replay");
        }
    }
    if webhooks.is_empty() {
        security.remove("webhooks");
    }
    if security.is_empty() {
        doc.as_table_mut().remove("security");
    }

    if !salvaged.is_empty() {
        let trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
        doc.set_trailing(salvaged + &trailing);
    }

    doc.to_string()
}

// ── Template rendering ───────────────────────────────────────────────────────

/// Render `src/webhooks/<snake>.rs`: the handler, its event-dispatch skeleton,
/// the per-event stub functions, and the generated test module.
fn render_handler_file(spec: &EndpointSpec) -> String {
    let EndpointSpec {
        provider,
        display_name,
        path,
        secret_env,
        handler_fn,
        ..
    } = spec;
    let label = provider.label();
    let dispatch = render_dispatch(spec);
    let stubs = render_event_stubs(spec);
    let tests = render_tests(spec);

    format!(
        r#"//! Signed inbound webhook intake for {label} — generated by
//! `autumn generate webhook {provider_name} {display_name}`. Edit freely; once
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
///
/// Each stub arm carries its own `// TODO` marker so the work to do is visible
/// at the dispatch site, not only in the stub function it delegates to.
fn render_dispatch(spec: &EndpointSpec) -> String {
    // Slack wraps every Events API callback in an `event_callback` envelope, and
    // `event_type()` reports that envelope's type — so the inner event type is
    // unwrapped and matched here, and the stubs receive the inner event object
    // rather than the envelope.
    if spec.provider == Provider::Slack {
        let arms = dispatch_arms(spec, 16, "inner_event");
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
            // The envelope's `event` object is what the handlers care about.
            let inner_event = event.get("event").unwrap_or(&event);
            let inner_type = inner_event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            match inner_type {{
{arms}                // Acknowledge and ignore: a 2xx tells Slack not to retry
                // an event this app does not handle.
                other => {{
                    tracing::debug!(event_type = other, "unhandled Slack event — acknowledged")
                }}
            }}
        }}
        // Acknowledge and ignore every other envelope type.
        _ => tracing::debug!(event_type, "unhandled Slack callback — acknowledged"),
    }}
"#
        );
    }

    let arms = dispatch_arms(spec, 8, "&event");
    format!(
        r#"    match event_type {{
{arms}        // Acknowledge and ignore: returning 2xx stops the provider from
        // retrying an event this app does not handle.
        _ => tracing::debug!(
            event_type,
            "unhandled webhook event — acknowledged and ignored"
        ),
    }}
"#
    )
}

/// One `"<event>" => on_<event>(&<payload>).await?, // TODO` arm per stub event,
/// indented by `indent` spaces (8 at the top level, 16 inside Slack's nested
/// envelope match) so the emitted code is `rustfmt`-clean as written.
fn dispatch_arms(spec: &EndpointSpec, indent: usize, payload: &str) -> String {
    let pad = " ".repeat(indent);
    spec.provider
        .stub_events()
        .iter()
        .fold(String::new(), |mut out, event| {
            // The marker goes ABOVE the arm, not trailing: `rustfmt` aligns
            // trailing comments across adjacent arms, so the emitted file would
            // not be format-clean as written.
            let _ = writeln!(out, "{pad}// TODO: fill this in");
            let _ = writeln!(
                out,
                "{pad}{event:?} => {}({payload}).await?,",
                stub_fn(event),
            );
            out
        })
}

/// Render one clearly-marked stub function per dispatched event type.
fn render_event_stubs(spec: &EndpointSpec) -> String {
    // Slack's stubs are handed the unwrapped inner event (see `render_dispatch`);
    // every other preset's receive the whole verified payload.
    let payload_doc = if spec.provider == Provider::Slack {
        "`event` is the verified callback's inner event object"
    } else {
        "`event` is the verified payload"
    };
    spec.provider
        .stub_events()
        .iter()
        .fold(String::new(), |mut out, event| {
            let _ = write!(
                out,
                r#"/// TODO: handle the `{event}` event.
///
/// {payload_doc}. Keep this idempotent — the same delivery
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
    let body_const = render_body_const(fixture_body(provider));
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
{body_const}

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

/// The `const BODY` declaration for the generated test module, wrapped onto a
/// second line when the single-line form would pass 100 columns — so the emitted
/// file is `rustfmt`-clean as written.
fn render_body_const(fixture: &str) -> String {
    const DECL: &str = "    const BODY: &str = ";
    // +1 for the trailing `;`.
    if DECL.len() + fixture.len() < 100 {
        format!("{DECL}{fixture};")
    } else {
        format!("    const BODY: &str =\n        {fixture};")
    }
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
    let signature = match provider {
        Provider::Stripe => header_call("stripe-signature", signature_expr),
        Provider::Github => header_call("x-hub-signature-256", signature_expr),
        Provider::Slack => header_call("x-slack-signature", signature_expr),
        Provider::Generic => header_call("x-webhook-signature", signature_expr),
    };
    let metadata = match provider {
        // Stripe carries its event type and delivery id in the JSON body.
        Provider::Stripe => String::new(),
        Provider::Github => {
            header_call("x-github-event", "\"push\"")
                + &header_call(
                    "x-github-delivery",
                    "\"00000000-0000-0000-0000-000000000001\"",
                )
        }
        Provider::Slack => header_call("x-slack-request-timestamp", "&timestamp.to_string()"),
        Provider::Generic => {
            header_call("x-webhook-event", "\"example.created\"")
                + &header_call("x-webhook-delivery", "\"delivery-1\"")
        }
    };
    signature + &metadata
}

/// One `.header(name, value)` line in a generated test's request builder,
/// wrapped across lines when `rustfmt` would wrap it anyway.
///
/// The trigger is `rustfmt`'s `fn_call_width` (60 by default, and this workspace
/// does not override it): a call whose argument list is wider than that gets one
/// argument per line, whatever the total column count. Emitting that shape up
/// front is what keeps generated files format-clean as written.
fn header_call(name: &str, value_expr: &str) -> String {
    const FN_CALL_WIDTH: usize = 60;
    let arguments = format!("\"{name}\", {value_expr}");
    if arguments.chars().count() <= FN_CALL_WIDTH {
        return format!("            .header({arguments})\n");
    }
    format!(
        "            .header(\n                \"{name}\",\n                {value_expr},\n            )\n"
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

    fn project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n\n\
             [dev-dependencies]\ntokio = { version = \"1\", features = [\"macros\"] }\n",
        )
        .unwrap();
        // Ends in a comment block, like `autumn new`'s own autumn.toml: the
        // generator has to keep those comments attached to `[health]` rather
        // than re-parenting them under a generated table, and `destroy` has to
        // put them back rather than delete them with the block.
        fs::write(
            tmp.path().join("autumn.toml"),
            "[server]\nhost = \"127.0.0.1\"\nport = 3000\n\n[health]\npath = \"/health\"\n\
             # live_path = \"/live\"\n# ready_path = \"/ready\"\n",
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

    // ── #1366 AC #1: post route + shipped extractor ────────────────────────────

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

    // ── #1366 AC #2: event dispatch with stub arms + default ack-and-ignore ────

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

    // ── #1366 AC #3: provider presets ──────────────────────────────────────────

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

    // ── #1366 AC #4: autumn.toml endpoint stub ─────────────────────────────────

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
    fn autumn_toml_writes_no_redundant_csrf_or_captcha_exemptions() {
        // The framework derives the endpoint's CSRF / submit-token / CAPTCHA
        // exemptions from `security.webhooks.endpoints` on every boot
        // (`build_csrf_layer` and friends call `with_exempt_path` per endpoint),
        // so a copy in `[security.csrf] exempt_paths` would add no protection
        // while creating a second source of truth that goes stale — and a stale
        // entry exempts the old path *and its whole subtree* forever.
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();

        assert!(
            !toml.contains("exempt_paths"),
            "path exemptions are derived from the endpoint block, not copied:\n{toml}"
        );
        assert!(
            !toml.contains("captcha_exempt_paths"),
            "path exemptions are derived from the endpoint block, not copied:\n{toml}"
        );
        assert_eq!(
            toml.matches("\"/webhooks/stripe\"").count(),
            1,
            "the route path must appear exactly once — in the endpoint block:\n{toml}"
        );
        assert!(
            toml.contains("CAPTCHA path exemptions from it on every boot"),
            "the generated block should say the exemptions are automatic:\n{toml}"
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
            1,
            "the endpoint path must not be duplicated:\n{toml}"
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
        for bad in [
            "hooks/billing", // not absolute
            "/",             // site root
            "/hooks billing",
            // Would break out of the generated `#[post("…")]` attribute.
            "/a\")]pub fn evil(){}//",
            "/hooks\\billing",
            "/hooks\nbilling",
            // Matched exactly by the registry: a path template would 500 on
            // every real delivery while the generated tests still passed.
            "/hooks/{tenant}/in",
            "/hooks/*rest",
        ] {
            let options = WebhookOptions {
                path: Some(bad.to_owned()),
                secret_env: None,
            };
            assert!(
                plan_webhook(tmp.path(), "stripe", "Billing", &options).is_err(),
                "path {bad:?} must be rejected"
            );
        }
        assert!(
            !tmp.path().join("src/webhooks").exists(),
            "a rejected invocation must write nothing"
        );
    }

    #[test]
    fn an_invalid_secret_env_override_is_rejected() {
        // `--secret-env` reaches generated Rust doc comments and printed next
        // steps, and named a TOML comment before this was locked down: a newline
        // could smuggle in a whole `[[security.webhooks.endpoints]]` block with a
        // plaintext `secret = "…"` and replay protection off.
        let tmp = project();
        for bad in [
            "",
            "  ",
            "1BAD",
            "BAD-NAME",
            "BAD NAME",
            "BAD\"NAME",
            "X\n\n[[security.webhooks.endpoints]]\nname = \"evil\"\nsecret = \"known\"\n# ",
        ] {
            let options = WebhookOptions {
                path: None,
                secret_env: Some(bad.to_owned()),
            };
            let Err(error) = plan_webhook(tmp.path(), "stripe", "Payments", &options) else {
                panic!("secret env {bad:?} must be rejected");
            };
            assert!(
                error.to_string().contains("secret environment variable"),
                "unexpected error for {bad:?}: {error}"
            );
        }
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(
            !toml.contains("secret ="),
            "no plaintext secret may ever reach autumn.toml:\n{toml}"
        );
    }

    // ── #1366 AC #5: app wiring ────────────────────────────────────────────────

    #[test]
    fn the_route_is_registered_in_main_rs_and_next_steps_are_printed() {
        let tmp = project();
        let plan =
            plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default()).unwrap();
        assert!(
            plan.warnings.is_empty(),
            "a clean run must print no warnings — next steps go to stdout: {:?}",
            plan.warnings
        );
        plan.execute(Flags::default()).unwrap();

        let steps = next_steps("stripe", "Payments", &WebhookOptions::default())
            .expect("resolvable arguments have next steps");
        assert!(
            steps.contains("STRIPE_WEBHOOK_SECRET"),
            "the secret env var must be named in the next steps:\n{steps}"
        );
        assert!(
            steps.contains("autumn webhook sim stripe"),
            "the next steps should show how to fire a test delivery:\n{steps}"
        );
        assert!(
            steps.contains("payment_intent.succeeded"),
            "the simulated delivery must reach a generated stub arm:\n{steps}"
        );
        assert!(
            !steps.contains("--event"),
            "stripe carries its event type in the payload:\n{steps}"
        );

        // The header-based presets need an explicit --event, or the simulator
        // announces its default `sim.event` and the delivery falls through to
        // the acknowledge-and-ignore arm, proving nothing about the handler.
        for (provider, event) in [("github", "push"), ("generic", "example.created")] {
            let steps = next_steps(provider, "Intake", &WebhookOptions::default())
                .expect("resolvable arguments have next steps");
            assert!(
                steps.contains(&format!("--event {event}")),
                "{provider}: the printed sim must target a generated arm:\n{steps}"
            );
        }
        assert!(
            steps.contains("POST /webhooks/stripe"),
            "the next steps should name the endpoint to register:\n{steps}"
        );

        let main_rs = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main_rs.contains("mod webhooks;"), "{main_rs}");
        assert!(
            main_rs.contains("webhooks::payments::payments_webhook"),
            "the generated route must be registered in routes![...]:\n{main_rs}"
        );
        let mod_rs = fs::read_to_string(tmp.path().join("src/webhooks/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod payments;"), "{mod_rs}");
    }

    // ── #1366 AC #6: generated tests ───────────────────────────────────────────

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

    // ── config the framework can actually load ───────────────────────────

    #[test]
    fn the_generated_autumn_toml_deserializes_into_a_usable_webhook_config() {
        // Nothing else proves the emitted keys are the ones the runtime reads:
        // the generated Rust tests build their config inline, and no config
        // struct in the tree uses `deny_unknown_fields`, so a typo'd key would
        // deserialize to a default and silently never register the endpoint.
        #[derive(serde::Deserialize)]
        struct Root {
            security: Security,
        }
        #[derive(serde::Deserialize)]
        struct Security {
            webhooks: autumn_web::webhook::WebhookConfig,
        }

        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        let root: Root = toml::from_str(&toml).expect("generated autumn.toml must deserialize");
        let webhooks = root.security.webhooks;
        assert_eq!(webhooks.endpoints.len(), 1, "endpoint must be registered");
        let endpoint = &webhooks.endpoints[0];
        assert_eq!(endpoint.name, "payments");
        assert_eq!(endpoint.path, "/webhooks/stripe");
        assert_eq!(
            endpoint.provider,
            autumn_web::webhook::WebhookProvider::Stripe
        );
        assert_eq!(
            endpoint.secret_env.as_deref(),
            Some("STRIPE_WEBHOOK_SECRET")
        );
        assert!(endpoint.replay_protection, "replay protection must be on");
        assert!(endpoint.secret.is_none(), "no inline secret may be written");
        assert_eq!(
            webhooks.replay.backend,
            autumn_web::webhook::WebhookReplayBackend::Memory
        );

        // …and with the secret supplied, the registry the app installs at boot
        // builds from exactly this config.
        let mut with_secret = webhooks;
        with_secret.endpoints[0].secret = Some("test-secret-32-bytes-long-enough".to_owned());
        autumn_web::webhook::WebhookRegistry::from_config(&with_secret)
            .expect("the generated endpoint must build a registry");
    }

    #[test]
    fn generating_preserves_a_trailing_comment_block_in_autumn_toml() {
        // `autumn new`'s autumn.toml ends in commented-out `[health]`/`[session]`
        // keys. A new root table appended after them would re-parent those
        // comments under the generated header, so uncommenting one would land the
        // key in the wrong table.
        let tmp = project();
        let with_trailer = "[server]\nport = 3000\n\n[health]\npath = \"/health\"\n\
                            # live_path = \"/live\"\n# ready_path = \"/ready\"\n";
        fs::write(tmp.path().join("autumn.toml"), with_trailer).unwrap();
        generated(&tmp, "stripe", "Payments");

        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        let health = toml.find("[health]").expect("health table survives");
        let live = toml.find("# live_path").expect("comment survives");
        let endpoint = toml
            .find("[[security.webhooks.endpoints]]")
            .expect("endpoint added");
        assert!(
            live > health && live < endpoint,
            "the [health] comment trailer must stay under [health]:\n{toml}"
        );

        // Uncommenting the trailing key must still parse as a health key.
        let uncommented = toml.replace("# live_path", "live_path");
        let parsed: toml::Value = toml::from_str(&uncommented).unwrap();
        assert!(
            parsed["health"].get("live_path").is_some(),
            "uncommenting must set health.live_path, not a webhook key:\n{uncommented}"
        );
    }

    #[test]
    fn generating_over_existing_security_tables_leaves_them_alone() {
        let tmp = project();
        let existing = "[security]\ncaptcha_exempt_paths = [\"/hook\"]\n\n\
                        [security.csrf]\nenabled = true\nexempt_paths = [\"/api/\"]\n\n\
                        [security.webhooks.replay]\nbackend = \"redis\"\n\n\
                        [security.webhooks.replay.redis]\nurl = \"redis://localhost:6379\"\n";
        fs::write(tmp.path().join("autumn.toml"), existing).unwrap();
        generated(&tmp, "github", "Repo");

        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(toml.contains("enabled = true"), "{toml}");
        assert!(toml.contains("exempt_paths = [\"/api/\"]"), "{toml}");
        assert!(
            toml.contains("backend = \"redis\""),
            "a configured replay backend must survive:\n{toml}"
        );
        assert!(
            !toml.contains("backend = \"memory\""),
            "the replay stub must not be added over an existing one:\n{toml}"
        );
        assert!(toml.contains("[[security.webhooks.endpoints]]"), "{toml}");
    }

    // ── regeneration and destroy edge cases ──────────────────────────────

    #[test]
    fn regenerating_with_a_changed_path_updates_the_endpoint_block() {
        // Otherwise the handler moves while the config stays put, and since the
        // registry matches paths exactly, every real delivery 500s — with the
        // generated tests (which build their own config) still green.
        let tmp = project();
        generated(&tmp, "stripe", "Payments");

        let moved = WebhookOptions {
            path: Some("/hooks/pay".to_owned()),
            secret_env: Some("PAY_SECRET".to_owned()),
        };
        plan_webhook(tmp.path(), "stripe", "Payments", &moved)
            .unwrap()
            .execute(Flags {
                force: true,
                ..Flags::default()
            })
            .unwrap();

        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(toml.contains("path = \"/hooks/pay\""), "{toml}");
        assert!(toml.contains("secret_env = \"PAY_SECRET\""), "{toml}");
        assert!(
            !toml.contains("/webhooks/stripe"),
            "the stale path must be gone:\n{toml}"
        );
        assert_eq!(
            toml.matches("[[security.webhooks.endpoints]]").count(),
            1,
            "the entry must be updated, not duplicated:\n{toml}"
        );

        // …and destroy still finds it at its new path.
        plan_webhook(tmp.path(), "stripe", "Payments", &moved)
            .unwrap()
            .revert(Flags::default())
            .unwrap();
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(!toml.contains("[[security.webhooks.endpoints]]"), "{toml}");
    }

    #[test]
    fn destroy_recovers_a_custom_path_and_secret_env_without_repeating_the_flags() {
        // `autumn destroy webhook stripe Payments` is the documented invocation.
        // Without recovering the recorded overrides it would look for the default
        // route, leave the endpoint block behind, and refuse to remove a handler
        // whose rendered content (which embeds the path) no longer matched.
        let tmp = project();
        let options = WebhookOptions {
            path: Some("/hooks/pay".to_owned()),
            secret_env: Some("PAY_SECRET".to_owned()),
        };
        plan_webhook(tmp.path(), "stripe", "Payments", &options)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        // …destroyed with no flags at all.
        plan_webhook_for_revert(tmp.path(), "stripe", "Payments", &WebhookOptions::default())
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(
            !toml.contains("[[security.webhooks.endpoints]]"),
            "the endpoint block must be removed:\n{toml}"
        );
        assert!(
            !toml.contains("/hooks/pay") && !toml.contains("PAY_SECRET"),
            "no trace of the endpoint may remain:\n{toml}"
        );
        assert!(
            !tmp.path().join("src/webhooks").exists(),
            "the handler must be removed without --force"
        );
        let main_rs = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(!main_rs.contains("payments_webhook"), "{main_rs}");
    }

    #[test]
    fn an_explicit_flag_still_wins_over_the_recorded_endpoint() {
        let tmp = project();
        plan_webhook(
            tmp.path(),
            "stripe",
            "Payments",
            &WebhookOptions {
                path: Some("/hooks/pay".to_owned()),
                secret_env: None,
            },
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();

        // A path that matches nothing recorded must not be silently replaced by
        // the recorded one — destroy then correctly finds nothing to remove.
        let adopted = adopt_recorded_overrides(
            tmp.path(),
            "Payments",
            &WebhookOptions {
                path: Some("/elsewhere".to_owned()),
                secret_env: None,
            },
        );
        assert_eq!(adopted.path.as_deref(), Some("/elsewhere"));
        assert_eq!(
            adopted.secret_env.as_deref(),
            Some("STRIPE_WEBHOOK_SECRET"),
            "the unspecified flag is still recovered"
        );
    }

    #[test]
    fn destroying_one_of_two_webhooks_keeps_the_other_working() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        generated(&tmp, "github", "Repo");

        plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default())
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(!toml.contains("/webhooks/stripe"), "{toml}");
        assert!(
            toml.contains("name = \"repo\""),
            "the sibling must survive:\n{toml}"
        );
        assert!(
            toml.contains("[security.webhooks.replay]"),
            "the shared replay block is still needed:\n{toml}"
        );
        assert!(tmp.path().join("src/webhooks/repo.rs").exists());
        assert!(!tmp.path().join("src/webhooks/payments.rs").exists());
        let main_rs = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main_rs.contains("mod webhooks;"), "{main_rs}");
        assert!(
            main_rs.contains("webhooks::repo::repo_webhook"),
            "{main_rs}"
        );
        assert!(!main_rs.contains("payments_webhook"), "{main_rs}");
    }

    #[test]
    fn destroy_leaves_a_hand_edited_endpoint_and_a_configured_replay_backend_alone() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");

        // The two edits a real deployment makes: a rotation variable and a redis
        // replay backend. Neither may be silently deleted by `destroy`.
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        let edited = toml
            .replace(
                "previous_secret_envs = []",
                "previous_secret_envs = [\"STRIPE_WEBHOOK_SECRET_PREVIOUS\"]",
            )
            .replace("backend = \"memory\"", "backend = \"redis\"");
        fs::write(tmp.path().join("autumn.toml"), &edited).unwrap();

        plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default())
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        let after = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert_eq!(
            after, edited,
            "hand-edited webhook config must survive destroy untouched"
        );
        assert!(
            !tmp.path().join("src/webhooks").exists(),
            "the generated code is still removed"
        );
    }

    #[test]
    fn destroy_cleans_up_a_replay_stub_orphaned_by_a_hand_removed_endpoint() {
        let tmp = project();
        generated(&tmp, "stripe", "Payments");
        let toml = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        let endpoint_start = toml.find("# Signed webhook intake generated").unwrap();
        fs::write(tmp.path().join("autumn.toml"), &toml[..endpoint_start]).unwrap();

        plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default())
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        let after = fs::read_to_string(tmp.path().join("autumn.toml")).unwrap();
        assert!(
            !after.contains("[security.webhooks.replay]"),
            "an orphaned replay stub must not linger:\n{after}"
        );
    }

    #[test]
    fn a_conflicting_src_webhooks_module_file_is_rejected() {
        let tmp = project();
        fs::write(tmp.path().join("src/webhooks.rs"), "// hand-written\n").unwrap();
        let error = plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default())
            .expect_err("E0761 would break the whole build");
        assert!(error.to_string().contains("src/webhooks.rs"), "{error}");
    }

    #[test]
    fn a_main_rs_without_a_routes_list_warns_instead_of_silently_not_registering() {
        let tmp = project();
        fs::write(
            tmp.path().join("src/main.rs"),
            "#[autumn_web::main]\nasync fn main() {\n    autumn_web::app().run().await;\n}\n",
        )
        .unwrap();
        let plan =
            plan_webhook(tmp.path(), "stripe", "Payments", &WebhookOptions::default()).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("routes![")),
            "an unroutable handler must be called out: {:?}",
            plan.warnings
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
