//! Signup, login, and logout.
//!
//! Composes shipped primitives: `Session` for the cookie-backed session,
//! `hash_password`/`verify_password` (bcrypt) for credentials, and plain Diesel
//! for the user row. On success we store both `user_id` and `tenant_id` in the
//! session; the dashboard reads `tenant_id` back to scope every query.

use autumn_web::auth::{hash_password, validate_password, verify_password};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use serde::Deserialize;

use crate::models::{NewUser, User};
use crate::schema::users;

use super::layout::layout;

#[derive(Deserialize)]
pub struct SignupForm {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub email: String,
    pub password: String,
}

// bcrypt hash used as a dummy target when the email is not found, so the
// login handler takes the same wall time whether or not the account exists.
const DUMMY_HASH: &str = "$2b$12$Ro0CUfOqk6cXEKf3dyaM7OhSCvnwM9s1Aw6lfLP2.GvpAfNXwi.2K";

// ── Signup ───────────────────────────────────────────────────────────────────

/// Render the signup form, optionally with a validation error. The minimum
/// length reflects the active `[auth.password]` policy so the `minlength`
/// attribute always matches what the handler enforces.
fn signup_page(min_len: usize, error: Option<&str>) -> Markup {
    layout(
        "Sign up",
        false,
        html! {
            h1 class="text-2xl font-bold mb-6" { "Create your account" }
            @if let Some(error) = error {
                p class="mb-4 text-sm text-red-600" role="alert" { (error) }
            }
            form action="/signup" method="post" class="space-y-4 bg-white rounded-lg shadow p-6 max-w-md" {
                div {
                    label for="email" class="block text-sm font-medium mb-1" { "Email" }
                    input #email type="email" name="email" required autocomplete="email"
                          class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="password" class="block text-sm font-medium mb-1" { "Password" }
                    input #password type="password" name="password" required minlength=(min_len)
                          autocomplete="new-password" class="w-full border rounded px-3 py-2";
                }
                button type="submit"
                       class="w-full bg-indigo-600 text-white py-2 rounded hover:bg-indigo-700" {
                    "Sign up"
                }
                p class="text-sm text-gray-500 text-center" {
                    "Already have an account? " a href="/login" class="text-indigo-600 hover:underline" { "Log in" }
                }
            }
        },
    )
}

#[get("/signup")]
pub async fn signup_form(State(state): State<AppState>) -> Markup {
    signup_page(state.config().auth.password.min_length, None)
}

#[post("/signup")]
pub async fn signup(
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
    Form(form): Form<SignupForm>,
) -> AutumnResult<Response> {
    let email = form.email.trim().to_lowercase();
    // Cap input lengths so an attacker cannot drive bcrypt/DB work with huge
    // payloads (254 is the RFC-5321 email maximum; 128 is a generous password cap).
    if !email.contains('@') || email.len() > 254 {
        return Err(AutumnError::unprocessable_msg(
            "Enter a valid email address (max 254 characters)",
        ));
    }
    if form.password.len() > 128 {
        return Err(AutumnError::unprocessable_msg(
            "Password must be at most 128 characters",
        ));
    }
    // Enforce the configured password policy (length, weak-list, similarity to
    // the email, and optional HIBP breach check). On failure, re-render the form
    // with the specific message at HTTP 200 rather than accepting a weak
    // credential.
    let password_cfg = state.config().auth.password;
    let mut policy = password_cfg.policy();
    if password_cfg.breach_check != autumn_web::auth::BreachCheck::Off {
        // Breach checking needs an HTTP client for the HIBP k-anonymity lookup;
        // the default-off path never constructs one.
        policy = policy.with_client(autumn_web::http_client::Client::new());
    }
    let validation = validate_password(&form.password, &policy, &[email.as_str()]).await;
    if !validation.is_valid() {
        let message = validation
            .first_message()
            .unwrap_or_else(|| "Invalid password".to_owned());
        return Ok(signup_page(password_cfg.min_length, Some(&message)).into_response());
    }

    // Each account gets its own isolated tenant; email is the unique identifier
    // (already enforced by the UNIQUE constraint on users.email).
    let tenant_id = email.clone();
    let password_hash = hash_password(&form.password).await?;

    let user: User = diesel::insert_into(users::table)
        .values(&NewUser {
            email,
            password_hash,
            tenant_id,
        })
        .returning(User::as_returning())
        .get_result(&mut *db)
        .await
        // A duplicate email hits the UNIQUE constraint; surface the same generic
        // message a failed login does so the form does not enumerate accounts.
        .map_err(|_| AutumnError::unprocessable_msg("Could not create account"))?;

    establish_session(&session, &user).await;
    Ok(Redirect::to("/dashboard").into_response())
}

// ── Login ────────────────────────────────────────────────────────────────────

#[get("/login")]
pub async fn login_form() -> Markup {
    layout(
        "Log in",
        false,
        html! {
            h1 class="text-2xl font-bold mb-6" { "Log in" }
            form action="/login" method="post" class="space-y-4 bg-white rounded-lg shadow p-6 max-w-md" {
                div {
                    label for="email" class="block text-sm font-medium mb-1" { "Email" }
                    input #email type="email" name="email" required autocomplete="email"
                          class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="password" class="block text-sm font-medium mb-1" { "Password" }
                    input #password type="password" name="password" required
                          autocomplete="current-password" class="w-full border rounded px-3 py-2";
                }
                button type="submit"
                       class="w-full bg-indigo-600 text-white py-2 rounded hover:bg-indigo-700" {
                    "Log in"
                }
                p class="text-sm text-gray-500 text-center" {
                    "Need an account? " a href="/signup" class="text-indigo-600 hover:underline" { "Sign up" }
                }
            }
        },
    )
}

#[post("/login")]
pub async fn login(
    session: Session,
    mut db: Db,
    Form(form): Form<LoginForm>,
) -> AutumnResult<Redirect> {
    let email = form.email.trim().to_lowercase();
    // Reject over-long inputs before any DB query or bcrypt work — they can never
    // match a stored account and only waste CPU.
    if email.len() > 254 || form.password.len() > 128 {
        return Err(AutumnError::unauthorized_msg("Invalid email or password"));
    }

    let user: Option<User> = users::table
        .filter(users::email.eq(&email))
        .select(User::as_select())
        .first(&mut *db)
        .await
        .optional()?;

    let invalid = || AutumnError::unauthorized_msg("Invalid email or password");
    // Always run a bcrypt verification so the response time is constant whether
    // or not the email exists — prevents a timing side-channel that reveals
    // which accounts are registered.
    let user = match user {
        Some(u) => u,
        None => {
            let _ = verify_password(&form.password, DUMMY_HASH).await;
            return Err(invalid());
        }
    };
    if !verify_password(&form.password, &user.password_hash).await? {
        return Err(invalid());
    }

    establish_session(&session, &user).await;
    Ok(Redirect::to("/dashboard"))
}

// ── Logout ───────────────────────────────────────────────────────────────────

#[post("/logout")]
pub async fn logout(session: Session) -> Redirect {
    // Clear the session contents and rotate the id so the old cookie cannot be
    // replayed.
    session.clear().await;
    session.rotate_id().await;
    Redirect::to("/")
}

/// Log a user in: rotate the session id (prevents fixation) and record the
/// account + tenant the rest of the app scopes to.
async fn establish_session(session: &Session, user: &User) {
    session.rotate_id().await;
    session.insert("user_id", user.id.to_string()).await;
    session.insert("tenant_id", &user.tenant_id).await;
}
