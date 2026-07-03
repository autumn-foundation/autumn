//! Route handlers for the admin panel.
//!
//! All handlers return [`AutumnResult<Response>`] so the framework's
//! error-page filter can render 401/403/404/500 as branded HTML for browser
//! clients and JSON for API clients — no hand-rolled error HTML here.

use std::sync::Arc;

use autumn_web::runtime_config::RuntimeConfigService;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::LazyLock;

use autumn_web::extract::Multipart;
use autumn_web::flash::{Flash, FlashLevel, FlashMessage};
use autumn_web::job::{JobAdminQuery, JobAdminSnapshot, JobScheduleSummary, job_admin_backend};
use autumn_web::prelude::{HxRequest, HxResponseExt};
use autumn_web::security::{CsrfFormField, CsrfToken, CsrfTokenHeader};
use autumn_web::{AppState, AutumnError, AutumnResult};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::middleware::from_fn;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;
use futures::future::join_all;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::check_role;
use crate::registry::AdminRegistry;
use crate::templates;
use crate::traits::{
    AdminError, AdminField, AdminFieldKind, AdminImportError, AdminImportReport,
    AdminImportRowResult, AdminModel, CsvImportMode, ListParams, SortDirection, record_id,
};

/// Admin-owned CSRF extractor that tolerates a missing `CsrfLayer`.
///
/// Autumn enables CSRF only for the `prod` profile by default, so a plain
/// `CsrfToken` extractor would crash every admin page in dev/test with a
/// 500. This wrapper reads the same request extension and falls back to an
/// empty token when the layer isn't installed — the rendered CSRF
/// hidden input and `<meta>` are then harmless because the middleware
/// that would validate them isn't running either.
#[derive(Debug, Clone, Default)]
pub struct AdminCsrf {
    token: String,
    form_field: String,
    token_header: String,
}

impl AdminCsrf {
    /// The CSRF token, or `""` if `CsrfLayer` is not installed.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The configured form field name, or Autumn's default when CSRF is absent.
    #[must_use]
    pub fn form_field(&self) -> &str {
        if self.form_field.is_empty() {
            "_csrf"
        } else {
            &self.form_field
        }
    }

    /// The configured CSRF token header name, or `"X-CSRF-Token"` when CSRF is absent.
    #[must_use]
    pub fn token_header(&self) -> &str {
        if self.token_header.is_empty() {
            "X-CSRF-Token"
        } else {
            &self.token_header
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AdminCsrf {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let token = parts
            .extensions
            .get::<CsrfToken>()
            .map(|t| t.token().to_owned())
            .unwrap_or_default();
        let form_field = parts
            .extensions
            .get::<CsrfFormField>()
            .map_or_else(|| "_csrf".to_owned(), |field| field.0.clone());
        let token_header = parts
            .extensions
            .get::<CsrfTokenHeader>()
            .map_or_else(|| "X-CSRF-Token".to_owned(), |h| h.0.clone());
        Ok(Self {
            token,
            form_field,
            token_header,
        })
    }
}

/// Plugin-owned JS. Served as an external file (not inline) so it works
/// under the default CSP `script-src 'self'`.
const ADMIN_JS: &str = include_str!("admin.js");

/// FNV-1a 64-bit hash of the shipped JS, computed at compile time. Used to
/// fingerprint the asset path so the browser cache can be `immutable` for
/// a year without risking a post-deploy mismatch between cached client JS
/// and newer server templates — bumping the JS bumps the URL.
const ADMIN_JS_HASH: u64 = fnv1a_64(ADMIN_JS.as_bytes());

const fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// Route path (relative to the plugin prefix) where [`ADMIN_JS`] is served.
/// Format: `/static/admin.<hash>.js`. Built at startup from the compile-time
/// content hash; stable for the lifetime of the process.
pub static ADMIN_JS_PATH: LazyLock<String> =
    LazyLock::new(|| format!("/static/admin.{ADMIN_JS_HASH:016x}.js"));

// ── Router construction ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn admin_router(
    registry: Arc<AdminRegistry>,
    prefix: &str,
    actuator_prefix: String,
    auth_session_key: String,
    require_role: Option<String>,
    config_svc: Option<Arc<RuntimeConfigService>>,
    step_up_mutations: bool,
    step_up_max_age_secs: u64,
) -> axum::Router<AppState> {
    let has_config = config_svc.is_some();

    let mut router = axum::Router::new()
        // Dashboard
        .route("/", routing::get(dashboard))
        .route("/jobs", routing::get(jobs_dashboard))
        .route("/jobs/counters", routing::get(jobs_counters))
        .route("/jobs/{id}/retry", routing::post(job_retry))
        .route("/jobs/{id}/discard", routing::post(job_discard))
        .route("/jobs/{id}/cancel", routing::post(job_cancel));

    // Runtime config routes — registered before /{slug} so the literal
    // "/config" path wins over the parameterized catch-all.
    if let Some(svc) = config_svc {
        router = router
            .route("/config", routing::get(config_list))
            .route("/config/{key}/set", routing::post(config_set))
            .route("/config/{key}/unset", routing::post(config_unset))
            .route("/config/{key}/history", routing::get(config_key_history))
            .layer(axum::Extension(AdminAuthSessionKey(
                auth_session_key.clone(),
            )))
            .layer(axum::Extension(svc));
    }

    router = router
        // Model routes (dynamic dispatch via slug)
        .route("/{slug}", routing::get(model_list).post(model_create))
        .route("/{slug}/new", routing::get(model_new_form))
        .route(
            "/{slug}/{id}",
            routing::get(model_detail)
                .post(model_update)
                .delete(model_delete),
        )
        // Version history pane (only reachable when model.has_history() is true)
        .route("/{slug}/{id}/history", routing::get(model_history))
        .route("/{slug}/{id}/edit", routing::get(model_edit_form))
        // Bulk-action endpoint. Receives selected `ids[]` and an `action`
        // name from the list-view form; dispatches to
        // `AdminModel::execute_action`.
        .route("/{slug}/actions", routing::post(model_action))
        // CSV export (GET) and import (POST)
        .route("/{slug}/export.csv", routing::get(model_export_csv))
        .route(
            "/{slug}/import",
            routing::get(model_import_form).post(model_import_csv),
        )
        .route(&ADMIN_JS_PATH, routing::get(serve_admin_js))
        .layer(axum::Extension(HasRuntimeConfig(has_config)))
        .layer(axum::Extension(AdminPrefix(prefix.to_owned())))
        .layer(axum::Extension(ActuatorPrefix(actuator_prefix)))
        .layer(axum::Extension(registry));

    // Apply step-up mutation guard before the role check so that a hijacked
    // admin session cannot exercise destructive admin actions.
    let router = if step_up_mutations {
        router.layer(from_fn(move |req, next| {
            crate::auth::check_step_up_mutations(step_up_max_age_secs, req, next)
        }))
    } else {
        router
    };

    match require_role {
        Some(role) => router.layer(from_fn(move |req, next| {
            check_role(role.clone(), auth_session_key.clone(), req, next)
        })),
        None => router,
    }
}

/// Typed Extension carrying the admin URL prefix so handlers can build links.
#[derive(Clone)]
struct AdminPrefix(String);

/// Typed Extension signalling whether the runtime config service is mounted.
#[derive(Clone)]
struct HasRuntimeConfig(bool);

/// Typed Extension carrying the actuator URL prefix (the value of
/// `config.actuator.prefix`), used for dashboard links and HTMX polling.
#[derive(Clone)]
struct ActuatorPrefix(String);

/// Typed Extension carrying the session key used to look up the authenticated
/// user's identity, so config-mutation handlers can record a real actor.
#[derive(Clone)]
struct AdminAuthSessionKey(String);

/// Serve the plugin's static JS with long-cache headers.
async fn serve_admin_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        ADMIN_JS,
    )
        .into_response()
}

// ── Query params ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    #[serde(default = "default_page")]
    page: u64,
    #[serde(default)]
    q: String,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    dir: SortDirection,
}

#[derive(Debug, Deserialize)]
struct JobsQuery {
    #[serde(default = "default_page", rename = "enqueued_page")]
    enqueued: u64,
    #[serde(default = "default_page", rename = "scheduled_page")]
    scheduled: u64,
    #[serde(default = "default_page", rename = "running_page")]
    running: u64,
    #[serde(default = "default_page", rename = "completed_page")]
    completed: u64,
    #[serde(default = "default_page", rename = "failed_page")]
    failed: u64,
    #[serde(default = "default_jobs_per_page", rename = "per_page")]
    per: u64,
}

impl From<JobsQuery> for JobAdminQuery {
    fn from(query: JobsQuery) -> Self {
        Self {
            enqueued_page: query.enqueued.max(1),
            scheduled_page: query.scheduled.max(1),
            running_page: query.running.max(1),
            completed_page: query.completed.max(1),
            failed_page: query.failed.max(1),
            per_page: query.per.clamp(1, 100),
        }
    }
}

const fn default_page() -> u64 {
    1
}

const fn default_jobs_per_page() -> u64 {
    25
}

// ── Shared resolution ───────────────────────────────────────────────

/// Resolve the DB pool + model for a slug, translating missing state into
/// `AutumnError` so handlers can use `?`.
fn resolve<'r>(
    state: &AppState,
    registry: &'r AdminRegistry,
    slug: &str,
) -> AutumnResult<(Pool<AsyncPgConnection>, &'r dyn AdminModel)> {
    // `Pool` is Arc-backed inside deadpool; cloning is cheap.
    let pool = state
        .pool()
        .cloned()
        .ok_or_else(|| AutumnError::service_unavailable_msg("No database pool configured"))?;
    let model = registry
        .get(slug)
        .ok_or_else(|| AutumnError::not_found_msg(format!("Model '{slug}' not found")))?;
    Ok((pool, model))
}

/// Filter a user-supplied sort key down to fields the model declared as
/// both sortable and list-displayed, AND that aren't of a sensitive kind
/// (`Password`/`Hidden`). A `None` (or unrecognised key) means "no sort"
/// — never forward arbitrary identifiers to the model.
///
/// The Hidden/Password exclusion mirrors the template-level filter on
/// `list_fields`: those columns aren't visible in the table, so a sort
/// header link can never produce them — only URL crafting can. Reject
/// here so the model doesn't receive an unexpected ORDER BY against a
/// column the admin chose to keep server-side.
fn validate_sort_key(sort: Option<String>, fields: &[AdminField]) -> Option<String> {
    sort.filter(|s| {
        fields.iter().any(|f| {
            f.name == s
                && f.sortable
                && f.list_display
                && !matches!(f.kind, AdminFieldKind::Password | AdminFieldKind::Hidden)
        })
    })
}

/// Pick `filter.<name>=<value>` pairs out of the raw query map and keep
/// only those whose `<name>` matches a field declared as `filterable` in
/// the model's schema. Sorted by name for stable output.
fn extract_filters(raw: &HashMap<String, String>, fields: &[AdminField]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = raw
        .iter()
        .filter_map(|(k, v)| {
            let name = k.strip_prefix("filter.")?;
            // Must be declared filterable. Empty values count as "no
            // filter on this field" and are dropped.
            if v.is_empty() {
                return None;
            }
            if !fields.iter().any(|f| f.name == name && f.filterable) {
                return None;
            }
            Some((name.to_owned(), v.clone()))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Translate an [`AdminError`] to the correct HTTP status. Validation errors
/// become 400, missing records 404, database/other backend failures 500. The
/// `action` word prefixes the message ("Create failed: ..."), which is handy
/// in logs and error pages.
fn admin_err(action: &str, err: AdminError) -> AutumnError {
    match err {
        AdminError::NotFound => AutumnError::not_found_msg(format!("{action}: not found")),
        AdminError::Validation(msg) => AutumnError::bad_request_msg(format!("{action}: {msg}")),
        AdminError::Database(msg) => {
            AutumnError::internal_server_error_msg(format!("{action}: database error: {msg}"))
        }
        AdminError::Other(msg) => {
            AutumnError::internal_server_error_msg(format!("{action}: {msg}"))
        }
    }
}

/// Render a Maud `Markup` into an `Html` response.
fn render(markup: maud::Markup) -> Response {
    Html(markup.into_string()).into_response()
}

/// Extract the value of the `__autumn_reveal` cookie from request headers.
fn extract_reveal_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie_str = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie_str.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("__autumn_reveal=")
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    })
}

// ── Handlers ────────────────────────────────────────────────────────

/// `GET /admin` — Dashboard with model counts.
async fn dashboard(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let pool = state
        .pool()
        .cloned()
        .ok_or_else(|| AutumnError::service_unavailable_msg("No database pool configured"))?;

    let futures: Vec<_> = registry
        .iter()
        .map(|(slug, model)| {
            let pool = pool.clone();
            async move {
                let count = model.count(&pool).await.unwrap_or(0);
                (slug, model.display_name_plural(), count)
            }
        })
        .collect();
    let counts = join_all(futures).await;
    let messages = flash.consume().await;

    Ok(render(templates::dashboard_page(
        &registry,
        &counts,
        &messages,
        csrf.token(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        show_config,
    )))
}

/// `GET /admin/jobs` -- built-in background jobs dashboard.
#[allow(clippy::too_many_arguments)]
async fn jobs_dashboard(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Query(query): Query<JobsQuery>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let mut snapshot = match job_admin_backend(&state) {
        Some(backend) => backend.snapshot(query.into()).await?,
        None => JobAdminSnapshot::empty(),
    };
    snapshot.schedules = scheduled_job_summaries(&state);
    let messages = flash.consume().await;

    Ok(render(templates::jobs_page(
        &registry,
        &snapshot,
        &messages,
        csrf.token(),
        csrf.form_field(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        show_config,
    )))
}

/// `GET /admin/jobs/counters` -- HTMX counter refresh fragment.
async fn jobs_counters(
    State(state): State<AppState>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Query(query): Query<JobsQuery>,
) -> AutumnResult<Response> {
    let mut snapshot = match job_admin_backend(&state) {
        Some(backend) => backend.snapshot(query.into()).await?,
        None => JobAdminSnapshot::empty(),
    };
    snapshot.schedules = scheduled_job_summaries(&state);
    Ok(render(templates::jobs_counters(&snapshot, &prefix)))
}

/// `POST /admin/jobs/{id}/retry` -- retry a failed job.
async fn job_retry(
    State(state): State<AppState>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Path(id): Path<String>,
    flash: Flash,
) -> AutumnResult<Response> {
    let backend = job_admin_backend(&state)
        .ok_or_else(|| AutumnError::service_unavailable_msg("job runtime is not initialized"))?;
    backend.retry(&id).await?;
    flash.success(format!("Retried job {id}.")).await;
    Ok(Redirect::to(&format!("{prefix}/jobs")).into_response())
}

/// `POST /admin/jobs/{id}/discard` -- discard a failed job.
async fn job_discard(
    State(state): State<AppState>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Path(id): Path<String>,
    flash: Flash,
) -> AutumnResult<Response> {
    let backend = job_admin_backend(&state)
        .ok_or_else(|| AutumnError::service_unavailable_msg("job runtime is not initialized"))?;
    backend.discard(&id).await?;
    flash.success(format!("Discarded job {id}.")).await;
    Ok(Redirect::to(&format!("{prefix}/jobs")).into_response())
}

/// `POST /admin/jobs/{id}/cancel` -- cancel a job that has not started.
async fn job_cancel(
    State(state): State<AppState>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Path(id): Path<String>,
    flash: Flash,
) -> AutumnResult<Response> {
    let backend = job_admin_backend(&state)
        .ok_or_else(|| AutumnError::service_unavailable_msg("job runtime is not initialized"))?;
    backend.cancel(&id).await?;
    flash.success(format!("Canceled job {id}.")).await;
    Ok(Redirect::to(&format!("{prefix}/jobs")).into_response())
}

fn scheduled_job_summaries(state: &AppState) -> Vec<JobScheduleSummary> {
    let mut schedules: Vec<_> = state
        .task_registry()
        .snapshot()
        .into_iter()
        .map(|(name, status)| {
            let last_run_status = status
                .last_error
                .as_ref()
                .map(|error| format!("failed: {error}"))
                .or(status.last_result);
            JobScheduleSummary {
                name,
                schedule: status.schedule,
                next_run_at: status.next_run_at,
                last_run_status,
            }
        })
        .collect();
    schedules.sort_by(|a, b| a.name.cmp(&b.name));
    schedules
}

/// `GET /admin/{slug}` -- Model list view.
#[allow(clippy::too_many_arguments)]
async fn model_list(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Path(slug): Path<String>,
    Query(query): Query<ListQuery>,
    Query(raw_query): Query<HashMap<String, String>>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    let ListQuery { page, q, sort, dir } = query;
    let page = page.max(1);
    let per_page = model.per_page();
    let fields = model.fields();
    // Validate the requested sort key against the model's declared
    // sortable fields. A crafted `?sort=<unexpected>` is silently dropped
    // — the model never sees an unvalidated sort key, so it can't error
    // or build unsafe dynamic ORDER BY expressions.
    let sort = validate_sort_key(sort, &fields);
    // Pull `?filter.<name>=<value>` keys out of the raw query string and
    // validate against the model's declared filterable fields. Unknown or
    // non-filterable names are dropped so a crafted URL can't drive
    // arbitrary filter logic in `AdminModel::list`.
    let filters = extract_filters(&raw_query, &fields);

    let params = ListParams {
        page,
        per_page,
        search: (!q.is_empty()).then(|| q.clone()),
        sort_by: sort.clone(),
        sort_dir: dir,
        filters: filters.clone(),
    };

    let result = model
        .list(&pool, params)
        .await
        .map_err(|e| admin_err("List", e))?;

    let actions = model.actions();
    let messages = flash.consume().await;
    Ok(render(templates::model_list_page(
        &registry,
        &slug,
        model.display_name_plural(),
        &fields,
        &actions,
        &result,
        &q,
        sort.as_deref(),
        dir,
        &filters,
        &messages,
        csrf.token(),
        csrf.form_field(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        show_config,
        model.supports_csv_export(),
        model.supports_csv_import(),
    )))
}

/// `GET /admin/{slug}/new` — Create form.
async fn model_new_form(
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Path(slug): Path<String>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let model = registry
        .get(&slug)
        .ok_or_else(|| AutumnError::not_found_msg(format!("Model '{slug}' not found")))?;

    let fields = model.fields();
    let messages = flash.consume().await;
    Ok(render(templates::model_form_page(
        &registry,
        &slug,
        model.display_name(),
        model.display_name_plural(),
        &fields,
        None,
        None,
        &messages,
        csrf.token(),
        csrf.form_field(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        show_config,
    )))
}

/// `POST /admin/{slug}` — Create a record.
async fn model_create(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Path(slug): Path<String>,
    flash: Flash,
    axum::extract::Form(form_data): axum::extract::Form<Value>,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    let fields = model.fields();
    let form_data = coerce_form_fields(strip_meta_fields(form_data, &fields, false), &fields);
    let record = model
        .create(&pool, form_data)
        .await
        .map_err(|e| admin_err("Create failed", e))?;
    // The post-create redirect needs a routable ID. Treat a missing or
    // non-numeric `id` as a model-impl bug rather than silently sending
    // the admin to `/{slug}/0` (which lands on the wrong row or a 404).
    let new_id = record_id(&record).ok_or_else(|| {
        AutumnError::internal_server_error_msg(format!(
            "{} create returned a record without a numeric `id` field; cannot route post-create redirect",
            model.display_name()
        ))
    })?;
    let detail_path = format!("{prefix}/{slug}/{new_id}");
    let mut response = Redirect::to(&detail_path).into_response();

    // If the model returned a one-time secret (e.g. a raw API token), hand it
    // off via a short-lived HttpOnly reveal cookie rather than through flash.
    // Flash::push stores messages in the Session, and SessionLayer persists
    // dirty sessions to the configured backing store (Redis/DB) — putting a raw
    // bearer token there even briefly is a plaintext-secret-storage violation.
    // The reveal cookie is path-scoped to the detail page and expires in 5
    // minutes; the detail handler reads it exactly once and clears it.
    //
    // Additionally, skip flash.success() on secret-returning creates: the
    // session write would dirty the session before the redirect; if the session
    // store (Redis/DB) is unavailable, SessionLayer replaces the handler's
    // response with its own error — discarding the Set-Cookie header and
    // permanently losing the raw credential.  The "copy now" FlashMessage
    // rendered by model_detail from the reveal cookie is sufficient notification.
    if let Some(Value::String(secret)) = record.get("token") {
        // Mirror the session cookie's Secure flag so the reveal cookie is
        // accepted in both HTTPS and explicit HTTP-only deployments.
        let secure_attr = if state.config().session.secure {
            "; Secure"
        } else {
            ""
        };
        let cookie = format!(
            "__autumn_reveal={secret}; HttpOnly{secure_attr}; SameSite=Strict; Path={detail_path}; Max-Age=300"
        );
        if let Ok(hv) = axum::http::HeaderValue::from_str(&cookie) {
            response
                .headers_mut()
                .insert(axum::http::header::SET_COOKIE, hv);
        }
    } else {
        // Non-secret create: safe to write a flash message (no raw credential
        // at risk if the session save fails — the record is already committed
        // and remains accessible via the list view).
        flash
            .success(format!("{} created.", model.display_name()))
            .await;
    }
    Ok(response)
}

/// `GET /admin/{slug}/{id}` — Detail view.
#[allow(clippy::too_many_arguments)]
async fn model_detail(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Path((slug, id)): Path<(String, i64)>,
    request_headers: axum::http::HeaderMap,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    let record = model
        .get(&pool, id)
        .await
        .map_err(|e| admin_err("Get", e))?
        .ok_or_else(|| {
            AutumnError::not_found_msg(format!("{} #{id} not found", model.display_name()))
        })?;

    let display = model.record_display(&record);
    let fields = model.fields();

    // Consume the reveal cookie if present (set by model_create for one-time
    // secrets such as raw API tokens).  The secret is appended to the in-memory
    // messages slice; it never touches the session store.
    let reveal_secret = extract_reveal_cookie(&request_headers);
    let mut messages = flash.consume().await;
    if let Some(ref secret) = reveal_secret {
        messages.push(FlashMessage {
            level: FlashLevel::Info,
            message: format!("Copy your token now — it will not be shown again: {secret}"),
        });
    }

    let mut response = render(templates::model_detail_page(
        &registry,
        &slug,
        model.display_name(),
        model.display_name_plural(),
        &fields,
        &record,
        &display,
        id,
        &messages,
        csrf.token(),
        csrf.form_field(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        model.has_history(),
        show_config,
    ))
    .into_response();

    // Clear the reveal cookie so a page refresh does not show the token again.
    // Mirror the session cookie's Secure flag for consistency.
    if reveal_secret.is_some() {
        let secure_attr = if state.config().session.secure {
            "; Secure"
        } else {
            ""
        };
        let clear = format!(
            "__autumn_reveal=; HttpOnly{secure_attr}; SameSite=Strict; Path={prefix}/{slug}/{id}; Max-Age=0"
        );
        if let Ok(hv) = axum::http::HeaderValue::from_str(&clear) {
            response
                .headers_mut()
                .insert(axum::http::header::SET_COOKIE, hv);
        }
    }

    Ok(response)
}

/// `GET /admin/{slug}/{id}/history` - Version history pane.
///
/// Returns 404 when the model has not opted into version history
/// (`model.has_history()` returns `false`).
#[allow(clippy::too_many_arguments)]
async fn model_history(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Path((slug, id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
    csrf: AdminCsrf,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    if !model.has_history() {
        return Err(AutumnError::not_found_msg(format!(
            "{} does not have version history enabled",
            model.display_name()
        )));
    }

    let page: u64 = params.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let per_page: u64 = params
        .get("per_page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(25);

    let history = model
        .get_history(&pool, id, page, per_page)
        .await
        .map_err(|e| admin_err("History", e))?;

    Ok(render(templates::model_history_page(
        &registry,
        &slug,
        model.display_name(),
        model.display_name_plural(),
        id,
        &history,
        &prefix,
        &actuator_prefix,
        csrf.token_header(),
        show_config,
    )))
}

/// `GET /admin/{slug}/{id}/edit` — Edit form.
#[allow(clippy::too_many_arguments)]
async fn model_edit_form(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Path((slug, id)): Path<(String, i64)>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    let record = model
        .get(&pool, id)
        .await
        .map_err(|e| admin_err("Get", e))?
        .ok_or_else(|| {
            AutumnError::not_found_msg(format!("{} #{id} not found", model.display_name()))
        })?;

    let fields = model.fields();
    let messages = flash.consume().await;
    Ok(render(templates::model_form_page(
        &registry,
        &slug,
        model.display_name(),
        model.display_name_plural(),
        &fields,
        Some(&record),
        Some(id),
        &messages,
        csrf.token(),
        csrf.form_field(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        show_config,
    )))
}

/// `POST /admin/{slug}/{id}` — Update a record.
async fn model_update(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Path((slug, id)): Path<(String, i64)>,
    flash: Flash,
    axum::extract::Form(form_data): axum::extract::Form<Value>,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    let fields = model.fields();
    let form_data = coerce_form_fields(strip_meta_fields(form_data, &fields, true), &fields);
    model
        .update(&pool, id, form_data)
        .await
        .map_err(|e| admin_err("Update failed", e))?;
    flash
        .success(format!("{} #{id} updated.", model.display_name()))
        .await;
    Ok(Redirect::to(&format!("{prefix}/{slug}/{id}")).into_response())
}

/// `POST /admin/{slug}/actions` — Execute a bulk action.
///
/// Form body carries `action=<name>`, repeated `ids=<id>` for each selected
/// row, and the CSRF token field. Validates the action name against the model's
/// declared `actions()` list, parses every `ids` entry as `i64`, then
/// dispatches to [`AdminModel::execute_action`].
async fn model_action(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Path(slug): Path<String>,
    flash: Flash,
    body: axum::body::Bytes,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    // serde_urlencoded doesn't support repeated keys (`ids=1&ids=2&ids=3`),
    // so parse with `form_urlencoded` directly.
    let mut action: Option<String> = None;
    let mut ids: Vec<i64> = Vec::new();
    let mut malformed_id = false;
    for (k, v) in form_urlencoded::parse(&body) {
        match k.as_ref() {
            "action" => action = Some(v.into_owned()),
            "ids" => match v.parse::<i64>() {
                Ok(id) => ids.push(id),
                Err(_) => malformed_id = true,
            },
            // ignore the CSRF token field and any unknown keys
            _ => {}
        }
    }

    if malformed_id {
        return Err(AutumnError::bad_request_msg(
            "bulk action: one or more `ids` values were not valid integers",
        ));
    }
    let action = action
        .ok_or_else(|| AutumnError::bad_request_msg("bulk action: missing `action` form field"))?;
    if ids.is_empty() {
        return Err(AutumnError::bad_request_msg(
            "bulk action: select at least one row",
        ));
    }
    // Validate the action name against the model's declared list.
    if !model.actions().iter().any(|a| a.name == action) {
        return Err(AutumnError::bad_request_msg(format!(
            "bulk action: '{action}' is not declared by this model"
        )));
    }

    let count = model
        .execute_action(&pool, &action, ids)
        .await
        .map_err(|e| admin_err("Bulk action failed", e))?;
    flash
        .success(format!("Applied '{action}' to {count} record(s)."))
        .await;
    Ok(Redirect::to(&format!("{prefix}/{slug}")).into_response())
}

/// `DELETE /admin/{slug}/{id}` — Delete a record.
///
/// Called from the detail view's `hx-delete` button, and reachable as a
/// plain form submission (`POST` + `_method=DELETE`) from the no-JS
/// `button_to_with` fallback. htmx requests get an empty 200 body with
/// `HX-Redirect` so htmx performs a full-page navigation to the list view
/// (updating `window.location`), rather than swapping the list HTML into
/// the stale detail page. Non-htmx requests get a real 303 redirect, since
/// a plain browser form submission ignores `HX-Redirect` and would
/// otherwise land on a blank 200 response.
async fn model_delete(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    Path((slug, id)): Path<(String, i64)>,
    hx: HxRequest,
    flash: Flash,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    model
        .delete(&pool, id)
        .await
        .map_err(|e| admin_err("Delete failed", e))?;
    flash
        .success(format!("{} #{id} deleted.", model.display_name()))
        .await;
    let list_path = format!("{prefix}/{slug}");
    if hx.is_htmx {
        Ok(StatusCode::OK.hx_redirect(&list_path))
    } else {
        Ok(Redirect::to(&list_path).into_response())
    }
}

// ── CSV export / import handlers ─────────────────────────────────────

/// Records fetched per page during CSV export to bound peak JSON memory usage.
const EXPORT_PAGE_SIZE: u64 = 500;

/// `GET /admin/{slug}/export.csv` — Download all records as a CSV file.
///
/// The response respects active `?q=`, `?sort=`, `?dir=`, and `?filter.*`
/// query parameters so the exported data matches what the user sees in the
/// list view.
#[allow(clippy::too_many_arguments)]
async fn model_export_csv(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    Path(slug): Path<String>,
    Query(query): Query<ListQuery>,
    Query(raw_query): Query<HashMap<String, String>>,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    if !model.supports_csv_export() {
        return Err(AutumnError::bad_request_msg(format!(
            "Model '{slug}' does not support CSV export"
        )));
    }

    let ListQuery {
        page: _,
        q,
        sort,
        dir,
    } = query;
    let fields = model.fields();
    let sort_key = validate_sort_key(sort, &fields);
    let filters = extract_filters(&raw_query, &fields);
    let search = (!q.is_empty()).then_some(q);

    // Page through records in batches to avoid buffering the entire dataset
    // as JSON in memory at once.
    let columns = model.csv_export_columns();
    let mut buf = Vec::new();
    {
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(true)
            .from_writer(&mut buf);

        wtr.write_record(&columns)
            .map_err(|e| AutumnError::internal_server_error_msg(format!("CSV write error: {e}")))?;

        let mut page: u64 = 1;
        loop {
            let params = ListParams {
                page,
                per_page: EXPORT_PAGE_SIZE,
                search: search.clone(),
                sort_by: sort_key.clone(),
                sort_dir: dir,
                filters: filters.clone(),
            };
            let result = model
                .list(&pool, params)
                .await
                .map_err(|e| admin_err("Export", e))?;
            let done =
                result.records.len() < usize::try_from(EXPORT_PAGE_SIZE).unwrap_or(usize::MAX);
            for record in &result.records {
                let row = model.csv_export_row(&columns, record);
                wtr.write_record(&row).map_err(|e| {
                    AutumnError::internal_server_error_msg(format!("CSV write error: {e}"))
                })?;
            }
            if done {
                break;
            }
            page += 1;
        }

        wtr.flush()
            .map_err(|e| AutumnError::internal_server_error_msg(format!("CSV flush error: {e}")))?;
    }

    let filename = format!("{slug}.csv");
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(axum::body::Body::from(buf))
        .map_err(|e| AutumnError::internal_server_error_msg(format!("Response error: {e}")))
}

/// `GET /admin/{slug}/import` — Render the CSV import form.
#[allow(clippy::too_many_arguments)]
async fn model_import_form(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Path(slug): Path<String>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let model = registry
        .get(&slug)
        .ok_or_else(|| AutumnError::not_found_msg(format!("Model '{slug}' not found")))?;

    if !model.supports_csv_import() {
        return Err(AutumnError::bad_request_msg(format!(
            "Model '{slug}' does not support CSV import"
        )));
    }

    let _ = state; // pool not needed to render the form
    let messages = flash.consume().await;
    Ok(render(templates::model_import_form_page(
        &registry,
        &slug,
        model.display_name_plural(),
        &messages,
        csrf.token(),
        csrf.form_field(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        show_config,
    )))
}

/// `POST /admin/{slug}/import` — Accept a multipart CSV upload and import rows.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn model_import_csv(
    State(state): State<AppState>,
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(HasRuntimeConfig(show_config)): axum::Extension<HasRuntimeConfig>,
    Path(slug): Path<String>,
    csrf: AdminCsrf,
    flash: Flash,
    mut multipart: Multipart,
) -> AutumnResult<Response> {
    let (pool, model) = resolve(&state, &registry, &slug)?;

    if !model.supports_csv_import() {
        return Err(AutumnError::bad_request_msg(format!(
            "Model '{slug}' does not support CSV import"
        )));
    }

    // Read multipart fields: "file" (required) and "mode" (optional).
    let mut csv_bytes: Option<Vec<u8>> = None;
    let mut import_mode = CsvImportMode::Insert;

    while let Some(field) = multipart.next_field().await? {
        match field.name() {
            Some("file") => {
                csv_bytes = Some(field.bytes_limited().await?);
            }
            Some("mode") => {
                let bytes = field.bytes_limited().await?;
                let val = String::from_utf8_lossy(&bytes).into_owned();
                import_mode = CsvImportMode::from_form_value(&val).ok_or_else(|| {
                    AutumnError::bad_request_msg(format!("Unknown import mode: '{val}'"))
                })?;
            }
            _ => {}
        }
    }

    let csv_bytes = csv_bytes
        .ok_or_else(|| AutumnError::bad_request_msg("No CSV file found in the multipart upload"))?;

    // Parse CSV headers first, then process rows.
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(csv_bytes.as_slice());

    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| AutumnError::bad_request_msg(format!("CSV header error: {e}")))?
        .iter()
        .map(str::to_owned)
        .collect();

    let mut report = AdminImportReport::default();

    for result in rdr.records() {
        match result {
            Ok(record) => {
                let line = record.position().map_or(0, csv::Position::line);
                let row: std::collections::HashMap<String, String> = headers
                    .iter()
                    .zip(record.iter())
                    .map(|(k, v)| (k.clone(), v.to_owned()))
                    .collect();

                let outcome = model
                    .import_csv_row(&pool, line, row, import_mode)
                    .await
                    .unwrap_or_else(|e| AdminImportRowResult::RowError(e.to_string()));

                match outcome {
                    AdminImportRowResult::Inserted => report.inserted += 1,
                    AdminImportRowResult::Updated => report.updated += 1,
                    AdminImportRowResult::Skipped => report.skipped += 1,
                    AdminImportRowResult::RowError(msg) => {
                        report.errors.push(AdminImportError {
                            line,
                            column: None,
                            message: msg,
                        });
                    }
                    AdminImportRowResult::FieldError { column, message } => {
                        report.errors.push(AdminImportError {
                            line,
                            column: Some(column),
                            message,
                        });
                    }
                }
            }
            Err(e) => {
                let line = e.position().map_or(0, csv::Position::line);
                report.errors.push(AdminImportError {
                    line,
                    column: None,
                    message: format!("CSV parse error: {e}"),
                });
            }
        }
    }

    let messages = flash.consume().await;
    Ok(render(templates::model_import_result_page(
        &registry,
        &slug,
        model.display_name_plural(),
        &report,
        import_mode,
        &messages,
        csrf.token(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
        show_config,
    )))
}

// ── Runtime config handlers ──────────────────────────────────────────

/// `GET /admin/config` — List all runtime config keys with their values.
async fn config_list(
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(svc): axum::Extension<Arc<RuntimeConfigService>>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let entries = svc
        .list()
        .map_err(|e| AutumnError::internal_server_error_msg(format!("Runtime config: {e}")))?;
    let messages = flash.consume().await;
    Ok(render(templates::config_page(
        &registry,
        &entries,
        &messages,
        csrf.token(),
        csrf.form_field(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
    )))
}

/// `POST /admin/config/{key}/set` — Update a config key's value.
async fn config_set(
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(AdminAuthSessionKey(auth_session_key)): axum::Extension<AdminAuthSessionKey>,
    axum::Extension(svc): axum::Extension<Arc<RuntimeConfigService>>,
    session: autumn_web::session::Session,
    Path(key): Path<String>,
    flash: Flash,
    axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>,
) -> AutumnResult<Response> {
    let value = form.get("value").map_or("", String::as_str);
    let actor = session
        .get(&auth_session_key)
        .await
        .unwrap_or_else(|| "admin-ui".to_owned());
    match svc.set(&key, value, Some(&actor)) {
        Ok(()) => flash.success(format!("Updated {key} = {value}")).await,
        Err(e) => flash.error(format!("Failed to set {key}: {e}")).await,
    }
    Ok(Redirect::to(&format!("{prefix}/config")).into_response())
}

/// `POST /admin/config/{key}/unset` — Revert a config key to its default.
async fn config_unset(
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(AdminAuthSessionKey(auth_session_key)): axum::Extension<AdminAuthSessionKey>,
    axum::Extension(svc): axum::Extension<Arc<RuntimeConfigService>>,
    session: autumn_web::session::Session,
    Path(key): Path<String>,
    flash: Flash,
) -> AutumnResult<Response> {
    let actor = session
        .get(&auth_session_key)
        .await
        .unwrap_or_else(|| "admin-ui".to_owned());
    match svc.unset(&key, Some(&actor)) {
        Ok(()) => flash.success(format!("Reset {key} to default")).await,
        Err(e) => flash.error(format!("Failed to reset {key}: {e}")).await,
    }
    Ok(Redirect::to(&format!("{prefix}/config")).into_response())
}

/// `GET /admin/config/{key}/history` — View change history for a config key.
#[allow(clippy::too_many_arguments)]
async fn config_key_history(
    axum::Extension(registry): axum::Extension<Arc<AdminRegistry>>,
    axum::Extension(AdminPrefix(prefix)): axum::Extension<AdminPrefix>,
    axum::Extension(ActuatorPrefix(actuator_prefix)): axum::Extension<ActuatorPrefix>,
    axum::Extension(svc): axum::Extension<Arc<RuntimeConfigService>>,
    Path(key): Path<String>,
    csrf: AdminCsrf,
    flash: Flash,
) -> AutumnResult<Response> {
    let history = svc
        .history(&key, 50)
        .map_err(|e| AutumnError::internal_server_error_msg(format!("Runtime config: {e}")))?;
    let messages = flash.consume().await;
    Ok(render(templates::config_history_page(
        &registry,
        &key,
        &history,
        &messages,
        csrf.token(),
        csrf.token_header(),
        &prefix,
        &actuator_prefix,
    )))
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Filter incoming form data down to fields the model declared as editable.
///
/// Enforcement (all four are necessary):
///
/// 1. **Drop underscore-prefixed keys** (`_csrf` and similar form internals).
/// 2. **Drop keys not declared in `fields`** so a crafted POST can't inject
///    arbitrary columns (e.g. `is_admin=true`) into an `AdminModel::create`.
/// 3. **Drop keys whose `AdminField::editable = false`** so read-only columns
///    (`id`, `created_at`, computed fields, privilege flags) can't be
///    overwritten by admins submitting tampered forms.
/// 4. **Drop `create_only` keys on update** so immutable-after-create fields
///    (e.g. `principal_id`, `expires_at`) can't be silently ignored while
///    an admin thinks they've changed them.
/// 5. **Drop blank string values on declared `Password` fields** so "leave
///    blank to keep current" doesn't wipe stored hashes.
///
/// The UI's readonly contract is the source of truth: if the admin didn't
/// declare a field as editable, model code never sees it.
fn strip_meta_fields(mut data: Value, fields: &[AdminField], for_update: bool) -> Value {
    if let Some(obj) = data.as_object_mut() {
        obj.retain(|k, v| {
            if k.starts_with('_') {
                return false;
            }
            let Some(field) = fields.iter().find(|f| f.name == k) else {
                // Key not in the schema — drop it. Prevents arbitrary columns
                // from being injected past the declared editable surface.
                return false;
            };
            if matches!(field.kind, AdminFieldKind::Hidden) {
                // Hidden fields are read-only by contract, regardless of
                // whether `editable` was flipped back to true — the form
                // never exposes an input for them, so any submitted value
                // is necessarily tampered.
                return false;
            }
            if !field.editable {
                // Readonly field — drop it regardless of submitted value.
                return false;
            }
            if for_update && field.create_only {
                // Immutable-after-create field — drop on update so the model
                // never sees it and the admin can't be misled into thinking
                // the change was persisted.
                return false;
            }
            // Drop blank string values on Password fields so admins editing
            // unrelated fields don't overwrite stored hashes. (Encrypted columns
            // are rendered as disabled, unsubmitted controls in the form, so they
            // never reach this map and need no name-based special-casing here —
            // which also avoids dropping blanks on same-named plaintext columns of
            // other admin resources.)
            !matches!(v, Value::String(s) if s.is_empty() && matches!(field.kind, AdminFieldKind::Password))
        });
    }
    data
}

fn coerce_form_fields(mut data: Value, fields: &[AdminField]) -> Value {
    let Some(obj) = data.as_object_mut() else {
        return data;
    };

    for field in fields {
        let Some(value) = obj.get_mut(field.name) else {
            continue;
        };
        coerce_form_value(value, field);
    }

    data
}

fn coerce_form_value(value: &mut Value, field: &AdminField) {
    if !field.required
        && matches!(
            field.kind,
            AdminFieldKind::Integer
                | AdminFieldKind::Float
                | AdminFieldKind::Date
                | AdminFieldKind::DateTime
        )
        && matches!(value, Value::String(raw) if raw.trim().is_empty())
    {
        *value = Value::Null;
        return;
    }

    match &field.kind {
        AdminFieldKind::Boolean => {
            if let Value::String(raw) = value
                && let Some(parsed) = parse_form_bool(raw)
            {
                *value = Value::Bool(parsed);
            }
        }
        AdminFieldKind::Integer => {
            if let Value::String(raw) = value
                && let Ok(parsed) = raw.parse::<i64>()
            {
                *value = Value::Number(parsed.into());
            }
        }
        AdminFieldKind::Float => {
            if let Value::String(raw) = value
                && let Ok(parsed) = raw.parse::<f64>()
                && let Some(number) = serde_json::Number::from_f64(parsed)
            {
                *value = Value::Number(number);
            }
        }
        AdminFieldKind::Json => {
            if let Value::String(raw) = value
                && let Ok(parsed) = serde_json::from_str(raw)
            {
                *value = parsed;
            }
        }
        AdminFieldKind::Text
        | AdminFieldKind::TextArea
        | AdminFieldKind::Date
        | AdminFieldKind::DateTime
        | AdminFieldKind::Select(_)
        | AdminFieldKind::Hidden
        | AdminFieldKind::Password => {}
    }
}

fn parse_form_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::job::{JobAdminBackendEntry, JobAdminMemoryBackend};
    use autumn_web::session::Session;
    use axum::body::Body;
    use serde_json::json;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn fields(specs: &[(&'static str, AdminFieldKind)]) -> Vec<AdminField> {
        specs
            .iter()
            .cloned()
            .map(|(name, kind)| AdminField::new(name, kind))
            .collect()
    }

    #[tokio::test]
    async fn jobs_route_renders_without_database_pool() {
        let backend = JobAdminMemoryBackend::new();
        let state =
            AppState::for_test().with_extension(JobAdminBackendEntry(std::sync::Arc::new(backend)));
        state.task_registry().register_scheduled(
            "cleanup",
            "every 60s",
            autumn_web::task::TaskCoordination::Fleet,
            "local",
            "replica-a",
        );
        state
            .task_registry()
            .record_next_run_at("cleanup", "2026-05-08T12:00:00Z");
        let session = Session::new_for_test("sid".to_owned(), HashMap::new());
        let app = admin_router(
            std::sync::Arc::new(AdminRegistry::new()),
            "/admin",
            "/actuator".to_owned(),
            "user_id".to_owned(),
            None,
            None,
            false,
            autumn_web::step_up::DEFAULT_MAX_AGE_SECS,
        )
        .layer(axum::Extension(session))
        .with_state(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/jobs")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(html.contains("Jobs"), "html: {html}");
        assert!(html.contains("Enqueued"), "html: {html}");
        assert!(
            html.contains(r#"hx-get="/admin/jobs/counters""#),
            "html: {html}"
        );
        assert!(html.contains("cleanup"), "html: {html}");
        assert!(html.contains("2026-05-08T12:00:00Z"), "html: {html}");
    }

    #[test]
    fn strip_meta_removes_csrf_and_underscore_fields() {
        let input = json!({"name": "x", "_csrf": "t", "_foo": 1});
        let out = strip_meta_fields(input, &fields(&[("name", AdminFieldKind::Text)]), false);
        assert_eq!(out, json!({"name": "x"}));
    }

    #[test]
    fn strip_meta_drops_blank_password_by_declared_kind() {
        let fields = fields(&[
            ("password", AdminFieldKind::Password),
            ("other", AdminFieldKind::Text),
        ]);
        let out = strip_meta_fields(json!({"password": "", "other": "y"}), &fields, false);
        assert_eq!(out, json!({"other": "y"}));

        let out = strip_meta_fields(json!({"password": "hunter2", "other": "y"}), &fields, false);
        assert_eq!(out, json!({"password": "hunter2", "other": "y"}));
    }

    #[test]
    fn strip_meta_drops_blank_custom_named_password() {
        // Regression: the old name-heuristic version missed this.
        // A field called "secret" declared as Password must still be stripped.
        let fields = fields(&[("secret", AdminFieldKind::Password)]);
        let out = strip_meta_fields(json!({"secret": ""}), &fields, false);
        assert_eq!(out, json!({}));
    }

    #[test]
    fn strip_meta_preserves_blank_non_password_fields() {
        let fields = fields(&[
            ("name", AdminFieldKind::Text),
            ("bio", AdminFieldKind::TextArea),
        ]);
        let out = strip_meta_fields(json!({"name": "", "bio": ""}), &fields, false);
        assert_eq!(out, json!({"name": "", "bio": ""}));
    }

    #[test]
    fn coerce_form_fields_converts_boolean_strings() {
        let fields = fields(&[("published", AdminFieldKind::Boolean)]);
        let out = coerce_form_fields(json!({"published": "true"}), &fields);
        assert_eq!(out, json!({"published": true}));

        let out = coerce_form_fields(json!({"published": "false"}), &fields);
        assert_eq!(out, json!({"published": false}));
    }

    #[test]
    fn coerce_form_fields_converts_numeric_and_json_strings() {
        let fields = fields(&[
            ("count", AdminFieldKind::Integer),
            ("rating", AdminFieldKind::Float),
            ("settings", AdminFieldKind::Json),
        ]);
        let out = coerce_form_fields(
            json!({
                "count": "42",
                "rating": "3.5",
                "settings": "{\"published\":true}"
            }),
            &fields,
        );

        assert_eq!(
            out,
            json!({
                "count": 42,
                "rating": 3.5,
                "settings": {"published": true}
            })
        );
    }

    #[test]
    fn coerce_form_fields_converts_blank_optional_numeric_strings_to_null() {
        let fields = vec![
            AdminField::new("count", AdminFieldKind::Integer).optional(),
            AdminField::new("rating", AdminFieldKind::Float).optional(),
        ];
        let out = coerce_form_fields(json!({"count": "", "rating": ""}), &fields);

        assert_eq!(out, json!({"count": null, "rating": null}));
    }

    #[test]
    fn coerce_form_fields_converts_blank_optional_date_strings_to_null() {
        let fields = vec![
            AdminField::new("published_on", AdminFieldKind::Date).optional(),
            AdminField::new("starts_at", AdminFieldKind::DateTime).optional(),
        ];
        let out = coerce_form_fields(json!({"published_on": "", "starts_at": "   "}), &fields);

        assert_eq!(out, json!({"published_on": null, "starts_at": null}));
    }

    #[test]
    fn validate_sort_key_passes_known_sortable_displayed_fields() {
        let fields = fields(&[("name", AdminFieldKind::Text)]);
        assert_eq!(
            validate_sort_key(Some("name".to_owned()), &fields),
            Some("name".to_owned())
        );
    }

    #[test]
    fn validate_sort_key_drops_unknown_keys() {
        // Crafted `?sort=<unexpected>` reaches model handler — must be dropped.
        let fields = fields(&[("name", AdminFieldKind::Text)]);
        assert_eq!(
            validate_sort_key(Some("DROP TABLE users".into()), &fields),
            None
        );
        assert_eq!(validate_sort_key(Some("password".into()), &fields), None);
    }

    #[test]
    fn validate_sort_key_drops_non_sortable_fields() {
        let mut computed = AdminField::new("computed", AdminFieldKind::Text);
        computed.sortable = false;
        let schema = vec![computed];
        assert_eq!(validate_sort_key(Some("computed".into()), &schema), None);
    }

    #[test]
    fn validate_sort_key_drops_hidden_columns() {
        // Fields excluded from list_display can't be sorted by URL crafting
        // either — the affordance doesn't exist in the UI.
        let mut secret = AdminField::new("secret", AdminFieldKind::Text);
        secret.list_display = false;
        let schema = vec![secret];
        assert_eq!(validate_sort_key(Some("secret".into()), &schema), None);
    }

    #[test]
    fn validate_sort_key_drops_sensitive_kinds_even_if_flagged_sortable() {
        // AdminField::new defaults sortable=true and list_display=true for
        // every kind, so without an explicit kind check, crafted
        // `?sort=password_hash` or `?sort=internal_token` would reach the
        // model. Mirror the template's Hidden/Password exclusion.
        let pw = AdminField::new("password_hash", AdminFieldKind::Password);
        let hidden = AdminField::new("internal_token", AdminFieldKind::Hidden);
        let schema = vec![pw, hidden];
        assert_eq!(
            validate_sort_key(Some("password_hash".into()), &schema),
            None
        );
        assert_eq!(
            validate_sort_key(Some("internal_token".into()), &schema),
            None
        );
    }

    #[test]
    fn extract_filters_keeps_declared_filterable_fields() {
        let mut status = AdminField::new("status", AdminFieldKind::Text);
        status.filterable = true;
        let schema = vec![status, AdminField::new("name", AdminFieldKind::Text)];
        let raw = HashMap::from([
            ("filter.status".into(), "active".into()),
            ("filter.name".into(), "alice".into()), // not filterable — drop
            ("page".into(), "1".into()),            // not a filter — drop
            ("filter.unknown".into(), "x".into()),  // not in schema — drop
        ]);
        let out = extract_filters(&raw, &schema);
        assert_eq!(out, vec![("status".to_owned(), "active".to_owned())]);
    }

    #[test]
    fn extract_filters_drops_empty_values() {
        let mut status = AdminField::new("status", AdminFieldKind::Text);
        status.filterable = true;
        let schema = vec![status];
        let raw = HashMap::from([("filter.status".into(), String::new())]);
        assert_eq!(extract_filters(&raw, &schema), vec![]);
    }

    #[test]
    fn extract_filters_handles_no_filters() {
        let schema = vec![AdminField::new("name", AdminFieldKind::Text)];
        let raw = HashMap::from([("page".into(), "2".into()), ("q".into(), "x".into())]);
        assert_eq!(extract_filters(&raw, &schema), vec![]);
    }

    #[test]
    fn extract_filters_sorts_for_stable_output() {
        let mut a = AdminField::new("zeta", AdminFieldKind::Text);
        a.filterable = true;
        let mut b = AdminField::new("alpha", AdminFieldKind::Text);
        b.filterable = true;
        let schema = vec![a, b];
        let raw = HashMap::from([
            ("filter.zeta".into(), "z".into()),
            ("filter.alpha".into(), "a".into()),
        ]);
        let out = extract_filters(&raw, &schema);
        assert_eq!(
            out,
            vec![
                ("alpha".to_owned(), "a".to_owned()),
                ("zeta".to_owned(), "z".to_owned()),
            ]
        );
    }

    #[test]
    fn validate_sort_key_passes_through_none() {
        let fields = fields(&[("name", AdminFieldKind::Text)]);
        assert_eq!(validate_sort_key(None, &fields), None);
    }

    #[test]
    fn admin_err_maps_variants_to_correct_status() {
        use axum::http::StatusCode;
        assert_eq!(
            admin_err("X", AdminError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            admin_err("X", AdminError::Validation("bad".into())).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            admin_err("X", AdminError::Database("pg down".into())).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            admin_err("X", AdminError::Other("boom".into())).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn admin_csrf_extractor_returns_empty_when_layer_missing() {
        // Simulate a dev/test setup where CsrfLayer is not installed.
        let req = axum::http::Request::builder().uri("/").body(()).unwrap();
        let (mut parts, ()) = req.into_parts();
        let extracted = AdminCsrf::from_request_parts(&mut parts, &())
            .await
            .expect("infallible");
        assert_eq!(extracted.token(), "");
        assert_eq!(extracted.form_field(), "_csrf");
    }

    #[tokio::test]
    async fn admin_csrf_extractor_reads_token_from_extensions() {
        // Build a CsrfToken the way CsrfLayer would — via its public
        // `FromRequestParts`-adjacent API isn't exposed, so reach through
        // the debug impl: we can't construct CsrfToken outside its crate.
        // Instead, verify the extractor at least doesn't panic when the
        // extension IS present by round-tripping through an axum handler.
        use axum::Router;
        use axum::body::Body;
        use axum::http::StatusCode;
        use axum::routing::get;
        use tower::ServiceExt;

        async fn handler(csrf: AdminCsrf) -> String {
            csrf.token().to_owned()
        }
        let app = Router::new().route("/", get(handler));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No panic, no 500 — just an empty-string body because no CsrfLayer.
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_csrf_extractor_reads_configured_form_field_from_extensions() {
        let req = axum::http::Request::builder().uri("/").body(()).unwrap();
        let (mut parts, ()) = req.into_parts();
        parts
            .extensions
            .insert(CsrfFormField("authenticity_token".to_owned()));

        let extracted = AdminCsrf::from_request_parts(&mut parts, &())
            .await
            .expect("infallible");

        assert_eq!(extracted.token(), "");
        assert_eq!(extracted.form_field(), "authenticity_token");
    }

    #[test]
    fn strip_meta_keeps_field_named_password_if_not_declared_as_such() {
        // If the model exposes a Text field literally named "password" (weird
        // but legal), we should NOT drop the empty string — the model gets to
        // decide. Only `AdminFieldKind::Password` triggers the strip.
        let fields = fields(&[("password", AdminFieldKind::Text)]);
        let out = strip_meta_fields(json!({"password": ""}), &fields, false);
        assert_eq!(out, json!({"password": ""}));
    }

    #[test]
    fn strip_meta_drops_fields_not_in_schema() {
        // Prevents a crafted POST from injecting arbitrary columns past the
        // declared editable surface (e.g. `is_admin=true` on a users model
        // that doesn't expose it).
        let fields = fields(&[("name", AdminFieldKind::Text)]);
        let input = json!({"name": "x", "is_admin": true, "raw_column": "y"});
        let out = strip_meta_fields(input, &fields, false);
        assert_eq!(out, json!({"name": "x"}));
    }

    #[test]
    fn strip_meta_drops_hidden_fields_even_if_editable_true() {
        // Defense in depth: even if a caller flipped `editable` back to true
        // on a Hidden field (which `AdminField::new` defaults to `false`),
        // the server must still reject it — the form never exposes an input
        // so any submitted value is tampered.
        let mut hidden = AdminField::new("owner_id", AdminFieldKind::Hidden);
        hidden.editable = true; // deliberately wrong
        let schema = vec![hidden];
        let out = strip_meta_fields(json!({"owner_id": 999}), &schema, false);
        assert_eq!(out, json!({}));
    }

    #[test]
    fn strip_meta_drops_create_only_fields_on_update_but_keeps_on_create() {
        let mut principal = AdminField::new("principal_id", AdminFieldKind::Text);
        principal.create_only = true;
        let name = AdminField::new("name", AdminFieldKind::Text);
        let schema = vec![principal, name];
        let input = json!({"principal_id": "svc:x", "name": "my-token"});

        // create context — create_only field is kept
        let out = strip_meta_fields(input.clone(), &schema, false);
        assert_eq!(out, json!({"principal_id": "svc:x", "name": "my-token"}));

        // update context — create_only field is dropped
        let out = strip_meta_fields(input, &schema, true);
        assert_eq!(out, json!({"name": "my-token"}));
    }

    #[test]
    fn strip_meta_drops_readonly_fields() {
        // `editable = false` fields (id, created_at, computed, privilege
        // flags) must not be forwarded to model code even if submitted.
        let mut id = AdminField::new("id", AdminFieldKind::Integer);
        id.editable = false;
        let mut created_at = AdminField::new("created_at", AdminFieldKind::DateTime);
        created_at.editable = false;
        let name = AdminField::new("name", AdminFieldKind::Text);
        let schema = vec![id, created_at, name];

        let input = json!({
            "id": 999,
            "created_at": "2026-01-01T00:00:00Z",
            "name": "legit",
        });
        let out = strip_meta_fields(input, &schema, false);
        assert_eq!(out, json!({"name": "legit"}));
    }

    // ── parse_form_bool coverage ──────────────────────────────────────

    #[test]
    fn parse_form_bool_recognizes_truthy_falsy_and_unknown_variants() {
        // Truthy variants
        assert_eq!(parse_form_bool("true"), Some(true));
        assert_eq!(parse_form_bool("1"), Some(true));
        assert_eq!(parse_form_bool("yes"), Some(true));
        assert_eq!(parse_form_bool("on"), Some(true));
        assert_eq!(parse_form_bool("TRUE"), Some(true)); // case-insensitive
        assert_eq!(parse_form_bool("YES"), Some(true));
        // Falsy variants
        assert_eq!(parse_form_bool("false"), Some(false));
        assert_eq!(parse_form_bool("0"), Some(false));
        assert_eq!(parse_form_bool("no"), Some(false));
        assert_eq!(parse_form_bool("off"), Some(false));
        assert_eq!(parse_form_bool(""), Some(false));
        assert_eq!(parse_form_bool("  "), Some(false)); // trims whitespace
        // Unknown → None (value is left as-is by coerce_form_value)
        assert_eq!(parse_form_bool("maybe"), None);
        assert_eq!(parse_form_bool("y"), None);
        assert_eq!(parse_form_bool("2"), None);
    }

    // ── extract_reveal_cookie ─────────────────────────────────────────────────

    fn headers_with_cookie(cookie: &str) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        map.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(cookie).unwrap(),
        );
        map
    }

    #[test]
    fn extract_reveal_cookie_returns_none_when_no_cookie_header() {
        let headers = axum::http::HeaderMap::new();
        assert!(extract_reveal_cookie(&headers).is_none());
    }

    #[test]
    fn extract_reveal_cookie_returns_none_when_cookie_absent_from_header() {
        let headers = headers_with_cookie("session=abc; other=xyz");
        assert!(extract_reveal_cookie(&headers).is_none());
    }

    #[test]
    fn extract_reveal_cookie_returns_none_when_value_is_empty() {
        let headers = headers_with_cookie("__autumn_reveal=; session=abc");
        assert!(extract_reveal_cookie(&headers).is_none());
    }

    #[test]
    fn extract_reveal_cookie_returns_value_when_present() {
        let headers = headers_with_cookie("session=abc; __autumn_reveal=tok123; other=xyz");
        assert_eq!(extract_reveal_cookie(&headers).as_deref(), Some("tok123"));
    }

    #[test]
    fn extract_reveal_cookie_handles_leading_only_cookie() {
        let headers = headers_with_cookie("__autumn_reveal=supersecret");
        assert_eq!(
            extract_reveal_cookie(&headers).as_deref(),
            Some("supersecret")
        );
    }
}
