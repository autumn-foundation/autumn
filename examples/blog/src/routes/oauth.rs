//! OAuth2/OIDC sign-in handlers for the blog example.
//!
//! Demonstrates the full Authorization Code + PKCE flow using autumn-web helpers.
//! Configure credentials in autumn.toml or via environment variables:
//!   AUTUMN_AUTH__OAUTH2__GITHUB__CLIENT_SECRET=ghp_...
//!   AUTUMN_AUTH__OAUTH2__GOOGLE__CLIENT_SECRET=...
//!
//! Routes:
//!   GET  /auth/oauth/:provider/redirect  → redirects to provider authorization URL
//!   GET  /auth/oauth/:provider/callback  → exchanges the code and logs the user in

use autumn_web::AppState;
use autumn_web::auth::{OAuth2Callback, oauth2_authorize_url, oauth2_finish_login};
use autumn_web::extract::{Path, Query};
use autumn_web::session::Session;
use autumn_web::{AutumnError, AutumnResult, Redirect, State, get, oauth2_callback};

/// Supported providers — must match keys in `[auth.oauth2.*]` in autumn.toml.
const SUPPORTED_PROVIDERS: &[&str] = &["github", "google"];

/// Redirect the browser to the provider's authorization endpoint.
///
/// PKCE (S256) code_challenge, state, and nonce are stored in the session.
#[get("/auth/{provider}/redirect")]
pub async fn oauth_redirect(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    session: Session,
) -> AutumnResult<Redirect> {
    if !SUPPORTED_PROVIDERS.contains(&provider_name.as_str()) {
        return Err(AutumnError::bad_request_msg(format!(
            "unknown provider: {provider_name}"
        )));
    }

    // `config_arc` shares the resolved config behind an `Arc`; `config()` would
    // deep-clone every section (including the OAuth provider map) per request.
    let config = state.config_arc();
    let auth_cfg = &config.auth;
    let provider = auth_cfg
        .oauth2
        .providers
        .get(&provider_name)
        .ok_or_else(|| {
            AutumnError::not_found_msg(format!(
                "provider '{provider_name}' is not configured in autumn.toml"
            ))
        })?
        .clone();

    let url = oauth2_authorize_url(&session, &provider_name, &provider).await?;
    Ok(Redirect::to(&url))
}

/// Handle the OAuth2 callback: exchange the code, create or link a local account.
///
/// State is validated with constant-time comparison against the session value.
/// On success the user is redirected to the home page.
#[oauth2_callback("/auth/{provider}/callback")]
pub async fn oauth_callback(
    Path(provider_name): Path<String>,
    Query(callback): Query<OAuth2Callback>,
    State(state): State<AppState>,
    session: Session,
) -> AutumnResult<Redirect> {
    if !SUPPORTED_PROVIDERS.contains(&provider_name.as_str()) {
        return Err(AutumnError::bad_request_msg(format!(
            "unknown provider: {provider_name}"
        )));
    }

    // `config_arc` shares the resolved config behind an `Arc`; `config()` would
    // deep-clone every section (including the OAuth provider map) per request.
    let config = state.config_arc();
    let auth_cfg = &config.auth;
    let provider = auth_cfg
        .oauth2
        .providers
        .get(&provider_name)
        .ok_or_else(|| {
            AutumnError::not_found_msg(format!(
                "provider '{provider_name}' is not configured in autumn.toml"
            ))
        })?
        .clone();

    let identity = oauth2_finish_login(&session, &provider_name, &provider, &callback)
        .await
        .map_err(|_| {
            // Do not surface the underlying error to avoid leaking sensitive state values.
            AutumnError::bad_request_msg("OAuth2 login failed — check provider configuration")
        })?;

    // TODO: look up or create user in `oauth_identities` + users tables, then set
    // the application session key before redirecting so the user is logged in.
    // Example:
    //   let local_user_id = link_or_create_user(&mut db, &identity, &provider_name).await?;
    //   session.insert(&auth_cfg.session_key, local_user_id).await;
    // See docs/guide/oauth.md for the full account-linking implementation guide.
    let _ = identity;

    Ok(Redirect::to("/"))
}
