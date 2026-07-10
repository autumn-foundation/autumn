//! Persistent "remember-me" login with rotating, revocable tokens (issue #1397).
//!
//! Composes the shipped [`autumn_web::auth`] remember primitives (the Barry
//! Jaspan "improved persistent login" scheme) into this app:
//!
//! - On login, when the user ticks "Remember me", [`issue_remember_cookie`]
//!   mints a `(series, token)` credential, persists a `remember_tokens` row
//!   (storing only the SHA-256 hash of the rotating token), and returns the
//!   `Set-Cookie` header to attach to the response.
//! - On every request that arrives WITHOUT a valid session but WITH a remember
//!   cookie, [`remember_me_middleware`] looks the row up by `series` and calls
//!   [`autumn_web::auth::evaluate_remember`]:
//!     - `Rotate`  → mint a fresh token, extend the sliding expiry, establish a
//!       real session, and set a fresh remember cookie (rotation).
//!     - `Theft`   → a replayed / rotated-out token for a known series: delete
//!       the whole chain and stay unauthenticated (theft detection).
//!     - `Reject`  → unknown or expired series: clear the cookie, unauthenticated.
//!
//! The middleware runs as a plain Tower layer OUTSIDE the tenancy middleware
//! (see the ingress order in `autumn_web`'s router), so a rotated-in session is
//! already established by the time tenancy resolves the tenant — an
//! unauthenticated visitor with a valid remember cookie reaches the dashboard
//! instead of being bounced to `/login`.

use std::sync::OnceLock;

use autumn_web::auth::{
    RememberConfig, RememberDecision, RememberRecord, build_remember_clear_cookie,
    build_remember_cookie, default_rotation_grace, evaluate_remember, generate_remember_credential,
    generate_token, hash_remember_token, parse_remember_cookie_value,
};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::extract::Request;
use autumn_web::reexports::axum::http::{HeaderMap, HeaderValue, header};
use autumn_web::reexports::axum::middleware::Next;
use autumn_web::reexports::axum::response::Response;
use diesel::prelude::*;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::schema::remember_tokens;
use crate::schema::users;

/// Process-wide handle to the database pool, populated at startup (`serve`/
/// `main`) or by the integration-test harness. The remember middleware runs as
/// a plain Tower layer with no access to `AppState`, so it checks out its
/// connection from here.
static REMEMBER_STATE: OnceLock<RememberMiddlewareState> = OnceLock::new();

/// The process-wide state the remember middleware needs: the pool it checks
/// connections out of plus the resolved `[auth.remember]` config, so
/// `cookie_name`/`duration_secs`/`enabled` overrides in `autumn.toml` (or
/// `AUTUMN_AUTH__REMEMBER__*`) are honoured instead of the compiled defaults
/// (issue #1397.2).
struct RememberMiddlewareState {
    pool: Pool<AsyncPgConnection>,
    config: RememberConfig,
}

/// Install the shared pool and resolved config the remember middleware uses.
///
/// Idempotent: a second call is ignored, so a test that builds several clients
/// against one shared database does not panic.
pub fn init_remember_pool(pool: Pool<AsyncPgConnection>, config: RememberConfig) {
    let _ = REMEMBER_STATE.set(RememberMiddlewareState { pool, config });
}

// ── Row types ────────────────────────────────────────────────────────────────

/// A stored remember-me chain. Only the hash of the current token is kept, so a
/// database leak cannot be replayed as a cookie.
#[derive(Queryable, Selectable)]
#[diesel(table_name = remember_tokens)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct RememberToken {
    pub series: String,
    pub user_id: i64,
    pub tenant_id: String,
    pub token_hash: String,
    /// SHA-256 of the token that was just rotated out, accepted during the
    /// grace window so benign concurrent requests do not false-fire theft
    /// detection (issue #1397).
    pub previous_token_hash: Option<String>,
    /// When the current token was minted (the last rotation). Bounds how long
    /// `previous_token_hash` stays acceptable.
    pub rotated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub ip: String,
    pub user_agent: String,
    pub ua_family: String,
    pub ua_os: String,
    pub ua_device: String,
    pub label: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Insert shape for a new remember chain. `created_at`/`last_used_at` fall back
/// to their column defaults.
#[derive(Insertable)]
#[diesel(table_name = remember_tokens)]
pub struct NewRememberToken {
    pub series: String,
    pub user_id: i64,
    pub tenant_id: String,
    pub token_hash: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub ip: String,
    pub user_agent: String,
    pub ua_family: String,
    pub ua_os: String,
    pub ua_device: String,
}

// ── Row CRUD ─────────────────────────────────────────────────────────────────

/// Persist a new remember chain.
pub async fn insert_remember_token(
    conn: &mut AsyncPgConnection,
    row: &NewRememberToken,
) -> Result<(), diesel::result::Error> {
    diesel::insert_into(remember_tokens::table)
        .values(row)
        .execute(conn)
        .await?;
    Ok(())
}

/// Look a chain up by its stable series id.
pub async fn find_remember_token(
    conn: &mut AsyncPgConnection,
    series: &str,
) -> Result<Option<RememberToken>, diesel::result::Error> {
    remember_tokens::table
        .find(series)
        .select(RememberToken::as_select())
        .first(conn)
        .await
        .optional()
}

/// Atomically rotate a chain: move the current `token_hash` into
/// `previous_token_hash`, install `new_hash`, stamp `rotated_at`/`last_used_at`,
/// and extend the sliding expiry — but ONLY if the row still carries `old_hash`
/// (compare-and-swap). Returns the number of rows affected (0 when a sibling
/// request already rotated the chain, 1 on success), so the caller can detect a
/// lost race and re-evaluate rather than silently double-rotating (issue #1397).
pub async fn rotate_remember_token(
    conn: &mut AsyncPgConnection,
    series: &str,
    old_hash: &str,
    new_hash: &str,
    new_expiry: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<usize, diesel::result::Error> {
    diesel::update(
        remember_tokens::table
            .filter(remember_tokens::series.eq(series))
            .filter(remember_tokens::token_hash.eq(old_hash)),
    )
    .set((
        remember_tokens::previous_token_hash.eq(old_hash),
        remember_tokens::token_hash.eq(new_hash),
        remember_tokens::rotated_at.eq(now),
        remember_tokens::expires_at.eq(new_expiry),
        remember_tokens::last_used_at.eq(now),
    ))
    .execute(conn)
    .await
}

/// Delete a single chain by series (the theft/logout revocation of one device).
pub async fn delete_remember_series(
    conn: &mut AsyncPgConnection,
    series: &str,
) -> Result<usize, diesel::result::Error> {
    diesel::delete(remember_tokens::table.find(series))
        .execute(conn)
        .await
}

/// Delete every remember chain owned by a user ("sign out everywhere" /
/// credential-change revocation).
pub async fn delete_remember_for_user(
    conn: &mut AsyncPgConnection,
    user_id: i64,
) -> Result<usize, diesel::result::Error> {
    diesel::delete(remember_tokens::table.filter(remember_tokens::user_id.eq(user_id)))
        .execute(conn)
        .await
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parsed device attribution for a remember row.
pub struct DeviceInfo {
    pub ip: String,
    pub user_agent: String,
    pub ua_family: String,
    pub ua_os: String,
    pub ua_device: String,
}

/// Derive device attribution from the request headers, mirroring the fields the
/// active-sessions row records.
#[must_use]
pub fn device_info(headers: &HeaderMap) -> DeviceInfo {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(512)
        .collect::<String>();
    let parsed = autumn_web::user_agent::parse_user_agent(&user_agent);
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    DeviceInfo {
        ip,
        user_agent,
        ua_family: parsed.family,
        ua_os: parsed.os,
        ua_device: parsed.device_class.to_string(),
    }
}

/// Whether to set the `Secure` cookie attribute — mirrors the session cookie by
/// keying off the forwarded scheme (HTTPS in production behind a TLS proxy).
fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
}

/// Read a single cookie value by name out of the request `Cookie` header.
fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        if let Some((k, v)) = pair.trim().split_once('=')
            && k.trim() == name
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// Append a `Set-Cookie` header to the response (never replaces an existing one,
/// so the rotated session cookie and the rotated remember cookie coexist).
fn append_set_cookie(response: &mut Response, value: &str) {
    if let Ok(header_value) = HeaderValue::from_str(value) {
        response
            .headers_mut()
            .append(header::SET_COOKIE, header_value);
    }
}

/// Sliding expiry `duration_secs` in the future, guarding the `u64 → i64` cast
/// and saturating (rather than panicking) on an absurd configured duration.
fn sliding_expiry(
    config: &RememberConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    let secs = i64::try_from(config.duration_secs).unwrap_or(2_592_000);
    now.checked_add_signed(chrono::Duration::seconds(secs))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
}

// ── Issue on login ───────────────────────────────────────────────────────────

/// Mint and persist a remember chain for a just-authenticated user, returning
/// the `Set-Cookie` header the login handler attaches to its response. Only the
/// hash of the rotating token is stored.
pub async fn issue_remember_cookie(
    conn: &mut AsyncPgConnection,
    config: &RememberConfig,
    user_id: i64,
    tenant_id: &str,
    headers: &HeaderMap,
) -> AutumnResult<String> {
    let cred = generate_remember_credential();
    let dev = device_info(headers);
    let row = NewRememberToken {
        series: cred.series.clone(),
        user_id,
        tenant_id: tenant_id.to_owned(),
        token_hash: hash_remember_token(&cred.token),
        expires_at: sliding_expiry(config, chrono::Utc::now()),
        ip: dev.ip,
        user_agent: dev.user_agent,
        ua_family: dev.ua_family,
        ua_os: dev.ua_os,
        ua_device: dev.ua_device,
    };
    insert_remember_token(conn, &row).await.map_err(|e| {
        AutumnError::internal_server_error_msg(format!("Failed to persist remember token: {e}"))
    })?;
    Ok(build_remember_cookie(
        config,
        &cred.series,
        &cred.token,
        request_is_secure(headers),
    ))
}

/// Revoke the remember chain identified by the request's remember cookie (used
/// by logout). No-op when the cookie is absent or malformed.
pub async fn revoke_current_chain(
    conn: &mut AsyncPgConnection,
    config: &RememberConfig,
    headers: &HeaderMap,
) {
    if let Some(value) = read_cookie(headers, &config.cookie_name)
        && let Some((series, _token)) = parse_remember_cookie_value(&value)
    {
        let _ = delete_remember_series(conn, &series).await;
    }
}

/// The `Set-Cookie` value that clears the remember cookie in the browser.
#[must_use]
pub fn clear_remember_cookie(config: &RememberConfig) -> String {
    build_remember_clear_cookie(config)
}

// ── Consume on request (rotation + theft detection) ──────────────────────────

/// Project a stored row into the pure [`RememberRecord`] the decision function
/// evaluates.
fn to_remember_record(r: &RememberToken) -> RememberRecord {
    RememberRecord {
        series: r.series.clone(),
        token_hash: r.token_hash.clone(),
        previous_token_hash: r.previous_token_hash.clone(),
        rotated_at: r.rotated_at,
        expires_at: r.expires_at,
    }
}

/// Confirm the owning account still exists (a replayed cookie after an account
/// deletion must not resurrect a session) and return its tenant.
async fn confirm_account(conn: &mut AsyncPgConnection, user_id: i64) -> Option<String> {
    users::table
        .find(user_id)
        .select(users::tenant_id)
        .first(conn)
        .await
        .optional()
        .ok()
        .flatten()
}

/// Establish a real session for the chain's owner — the same shape the login
/// handler writes, so the tenancy gate resolves the tenant.
async fn establish_session(session: &Session, user_id: i64, tenant_id: &str) {
    session.rotate_id().await;
    session.insert("user_id", user_id.to_string()).await;
    session.insert("tenant_id", tenant_id).await;
}

/// Proceed unauthenticated, clearing the remember cookie on the way out.
async fn unauthenticated_clearing(
    config: &RememberConfig,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    append_set_cookie(&mut response, &clear_remember_cookie(config));
    response
}

/// Global middleware: upgrade a request that has a valid remember cookie but no
/// session into an authenticated one (rotating the token), or trip theft
/// detection on a replayed cookie. Registered via `.layer(from_fn(..))` so it
/// runs before the tenancy gate.
pub async fn remember_me_middleware(session: Session, request: Request, next: Next) -> Response {
    // The resolved config + pool are stashed at startup; without them the
    // middleware is inert.
    let Some(mw_state) = REMEMBER_STATE.get() else {
        return next.run(request).await;
    };
    let config = &mw_state.config;

    // Nothing to do when remember is disabled or the request is already
    // authenticated — the existing session wins.
    if !config.enabled || session.get("user_id").await.is_some() {
        return next.run(request).await;
    }

    let Some(cookie_value) = read_cookie(request.headers(), &config.cookie_name) else {
        return next.run(request).await;
    };
    // A malformed cookie is ignored (proceed unauthenticated) rather than 400.
    let Some((series, token)) = parse_remember_cookie_value(&cookie_value) else {
        return next.run(request).await;
    };

    let Ok(mut conn) = mw_state.pool.get().await else {
        return next.run(request).await;
    };

    let record = match find_remember_token(&mut conn, &series).await {
        Ok(record) => record,
        // A transient DB error must not authenticate — proceed unauthenticated.
        // Release the checked-out connection first so a downstream `Db` route
        // can't self-starve a small/exhausted pool while `next.run` awaits.
        Err(_) => {
            drop(conn);
            return next.run(request).await;
        }
    };

    let now = chrono::Utc::now();
    let secure = request_is_secure(request.headers());
    let grace = default_rotation_grace();
    let evaluated = record.as_ref().map(to_remember_record);

    match evaluate_remember(&token, evaluated.as_ref(), now, grace) {
        RememberDecision::Rotate => {
            let row = record.expect("Rotate is only returned for a present record");

            // Atomic compare-and-swap rotation: only the request that still sees
            // the current token wins. Extend the sliding expiry at the same time,
            // BEFORE establishing the session, so the old cookie value can never
            // be replayed.
            let new_token = generate_token();
            let new_hash = hash_remember_token(&new_token);
            let new_expiry = sliding_expiry(config, now);
            let affected = rotate_remember_token(
                &mut conn,
                &series,
                &row.token_hash,
                &new_hash,
                new_expiry,
                now,
            )
            .await
            .unwrap_or(0);

            if affected == 1 {
                let Some(tenant_id) = confirm_account(&mut conn, row.user_id).await else {
                    let _ = delete_remember_series(&mut conn, &series).await;
                    drop(conn);
                    return unauthenticated_clearing(config, request, next).await;
                };
                drop(conn);
                establish_session(&session, row.user_id, &tenant_id).await;

                let mut response = next.run(request).await;
                append_set_cookie(
                    &mut response,
                    &build_remember_cookie(config, &series, &new_token, secure),
                );
                response
            } else {
                // Lost the race: a sibling request already rotated the chain.
                // Re-select and re-evaluate the presented token against the new
                // state before deciding.
                let record2 = find_remember_token(&mut conn, &series).await.ok().flatten();
                let evaluated2 = record2.as_ref().map(to_remember_record);
                match evaluate_remember(&token, evaluated2.as_ref(), now, grace) {
                    RememberDecision::Accept | RememberDecision::Rotate => {
                        // Within grace (or somehow still current): authenticate
                        // WITHOUT rotating again or re-cookieing — the browser
                        // keeps its token, valid within the window.
                        let Some(tenant_id) = confirm_account(&mut conn, row.user_id).await else {
                            drop(conn);
                            return unauthenticated_clearing(config, request, next).await;
                        };
                        drop(conn);
                        establish_session(&session, row.user_id, &tenant_id).await;
                        next.run(request).await
                    }
                    RememberDecision::Theft => {
                        let _ = delete_remember_series(&mut conn, &series).await;
                        drop(conn);
                        unauthenticated_clearing(config, request, next).await
                    }
                    RememberDecision::Reject => {
                        drop(conn);
                        unauthenticated_clearing(config, request, next).await
                    }
                }
            }
        }
        RememberDecision::Accept => {
            // A benign concurrent request presenting the just-rotated-out token
            // within the grace window: authenticate without rotating or setting a
            // new cookie.
            let row = record.expect("Accept is only returned for a present record");
            let Some(tenant_id) = confirm_account(&mut conn, row.user_id).await else {
                drop(conn);
                return unauthenticated_clearing(config, request, next).await;
            };
            drop(conn);
            establish_session(&session, row.user_id, &tenant_id).await;
            next.run(request).await
        }
        RememberDecision::Theft => {
            // Replayed, rotated-out token for a known series: an attacker holds a
            // stale cookie. Nuke the whole chain and stay unauthenticated.
            let _ = delete_remember_series(&mut conn, &series).await;
            drop(conn);
            unauthenticated_clearing(config, request, next).await
        }
        RememberDecision::Reject => {
            drop(conn);
            unauthenticated_clearing(config, request, next).await
        }
    }
}
