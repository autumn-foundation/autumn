//! Sign in, sign out, and registration.

use autumn_web::auth::{hash_password, validate_password, verify_password};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Role;
use crate::models::NewUser;
use crate::repositories::UserRepository as _;
use crate::theme::{self, Chrome};

use super::site::{Csrf, Repos, Submit};

/// A bcrypt hash used as a dummy verification target when the username is not
/// found, so a failed login takes the same wall time whether or not the account
/// exists. Without it, response timing enumerates the site's usernames.
const DUMMY_HASH: &str = "$2b$12$Ro0CUfOqk6cXEKf3dyaM7OhSCvnwM9s1Aw6lfLP2.GvpAfNXwi.2K";

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct RegisterForm {
    pub username: String,
    pub email: String,
    pub password: String,
}

/// Render an auth screen inside the site chrome, so signing in does not drop
/// the visitor onto an unstyled page that looks like a different site.
async fn auth_page(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    title: &str,
    content: Markup,
) -> AutumnResult<Markup> {
    let settings = repos.settings().await?;
    let chrome = Chrome {
        nav: Vec::new(),
        // Nor a footer menu: an auth screen is deliberately a dead end.
        footer_nav: Vec::new(),
        csrf: csrf.input(),
        // No sidebar on an auth screen: nothing there helps, and a "recent
        // posts" list beside a password field is noise.
        sidebar: None,
        current_user: repos.current_user(session).await?,
        settings: settings.clone(),
    };
    Ok(theme::active_theme(&settings).layout(&chrome, title, content))
}

fn login_form_markup(csrf: &Csrf, username: &str, error: Option<&str>) -> Markup {
    html! {
        h1 class="text-2xl font-bold mb-6" { "Log in" }
        @if let Some(error) = error {
            p class="mb-4 text-sm text-red-700" role="alert" { (error) }
        }
        form action="/login" method="post" class="space-y-4 max-w-sm" {
            (csrf.input())
            div {
                label for="username" class="block text-sm font-medium mb-1" { "Username" }
                input #username type="text" name="username" value=(username) required
                      autocomplete="username" class="w-full border rounded px-3 py-2";
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
            p class="text-sm text-gray-500" {
                "No account? " a href="/register" class="text-indigo-700 hover:underline" { "Register" }
            }
        }
    }
}

#[get("/login")]
pub async fn login_form(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Markup> {
    let form = login_form_markup(&csrf, "", None);
    auth_page(&repos, &session, &csrf, "Log in", form).await
}

// Every failed attempt runs a bcrypt verification — deliberately, so the
// response time does not reveal whether an account exists (see `DUMMY_HASH`).
// That makes the endpoint expensive by design, and this starter ships with the
// global limiter off, so without a per-route bound an unauthenticated client
// can drive arbitrary concurrent cost-12 hashes: credential stuffing and CPU
// exhaustion from the same request loop. A per-IP throttle belongs on the
// route rather than in an operator's checklist.
#[post("/login")]
#[throttle(limit = 10, per = "1m", key = "ip")]
pub async fn login(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Form(form): Form<LoginForm>,
) -> AutumnResult<Response> {
    let username = form.username.trim().to_lowercase();

    // Reject over-long input before any query or bcrypt work — it can never
    // match a stored account and only burns CPU.
    let candidate = if username.len() > 60 || form.password.len() > 128 {
        None
    } else {
        repos
            .users
            .find_by_username(username.clone())
            .await?
            .into_iter()
            .next()
    };

    let Some(user) = candidate else {
        // Always verify against *something*, so the response time does not
        // reveal whether the account exists.
        let _ = verify_password(&form.password, DUMMY_HASH).await;
        let page = auth_page(
            &repos,
            &session,
            &csrf,
            "Log in",
            login_form_markup(&csrf, &form.username, Some("Invalid username or password")),
        )
        .await?;
        return Ok(page.into_response());
    };

    if !verify_password(&form.password, &user.password_hash).await? {
        let page = auth_page(
            &repos,
            &session,
            &csrf,
            "Log in",
            login_form_markup(&csrf, &form.username, Some("Invalid username or password")),
        )
        .await?;
        return Ok(page.into_response());
    }

    // Rotate the session id on privilege change — the standard fixation
    // defence: a session id an attacker planted before login is not the one
    // that ends up authenticated.
    session.rotate_id().await;
    session.insert("user_id", user.id.to_string()).await;

    // Subscribers have no admin screens to land on, so they go to the site.
    let destination = if user.role().can_access_admin() {
        "/admin"
    } else {
        "/"
    };
    Ok(Redirect::to(destination).into_response())
}

#[post("/logout")]
pub async fn logout(session: Session) -> Redirect {
    session.clear().await;
    session.rotate_id().await;
    Redirect::to("/")
}

fn register_form_markup(
    csrf: &Csrf,
    submit_token: &str,
    min_length: usize,
    form: (&str, &str),
    error: Option<&str>,
) -> Markup {
    let (username, email) = form;
    html! {
        h1 class="text-2xl font-bold mb-6" { "Create an account" }
        @if let Some(error) = error {
            p class="mb-4 text-sm text-red-700 whitespace-pre-line" role="alert" { (error) }
        }
        form action="/register" method="post" class="space-y-4 max-w-sm" {
            (csrf.input())
            input type="hidden" name="_submit_token" value=(submit_token);
            div {
                label for="username" class="block text-sm font-medium mb-1" { "Username" }
                input #username type="text" name="username" value=(username) required
                      maxlength="60" autocomplete="username"
                      class="w-full border rounded px-3 py-2";
            }
            div {
                label for="email" class="block text-sm font-medium mb-1" { "Email" }
                input #email type="email" name="email" value=(email) required
                      autocomplete="email" class="w-full border rounded px-3 py-2";
            }
            div {
                label for="password" class="block text-sm font-medium mb-1" { "Password" }
                input #password type="password" name="password" required
                      minlength=(min_length) autocomplete="new-password"
                      class="w-full border rounded px-3 py-2";
            }
            button type="submit"
                   class="w-full bg-indigo-600 text-white py-2 rounded hover:bg-indigo-700" {
                "Register"
            }
        }
    }
}

#[get("/register")]
pub async fn register_form(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    submit_token: Submit,
    State(state): State<AppState>,
) -> AutumnResult<Markup> {
    let min_length = state.config_arc().auth.password.min_length;
    let form = register_form_markup(&csrf, submit_token.token(), min_length, ("", ""), None);
    auth_page(&repos, &session, &csrf, "Register", form).await
}

// Registration hashes a password on every request, so it is expensive for the
// same reason as `login` and takes the same per-IP bound. It is also the
// account-creation endpoint, which is worth rate-limiting on its own.
#[post("/register")]
#[throttle(limit = 5, per = "1m", key = "ip")]
pub async fn register(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    // The token that guarded *this* request was consumed by the framework's
    // `SubmitTokenLayer` before the handler ran, so an error re-render needs a
    // fresh one or the corrected resubmit would be rejected as a replay.
    submit_token: Submit,
    State(state): State<AppState>,
    Form(form): Form<RegisterForm>,
) -> AutumnResult<Response> {
    let password_cfg = state.config_arc().auth.password.clone();
    let username = form.username.trim().to_lowercase();
    let email = form.email.trim().to_lowercase();

    let redisplay = |message: String, token: &str| {
        register_form_markup(
            &csrf,
            token,
            password_cfg.min_length,
            (&form.username, &form.email),
            Some(&message),
        )
    };

    if username.is_empty() || username.len() > 60 {
        let page = auth_page(
            &repos,
            &session,
            &csrf,
            "Register",
            redisplay(
                "Username must be between 1 and 60 characters".to_owned(),
                submit_token.token(),
            ),
        )
        .await?;
        return Ok(page.into_response());
    }
    // A cheap pre-check so the form can redisplay with its own message;
    // `normalize_new_user` applies the model's declared validator, which is
    // what actually decides. Approximating it here *instead* accepted `user@`.
    if !autumn_web::reexports::validator::ValidateEmail::validate_email(&email)
        || email.len() > crate::hooks::MAX_EMAIL_BYTES
    {
        let page = auth_page(
            &repos,
            &session,
            &csrf,
            "Register",
            redisplay(
                "Enter a valid email address".to_owned(),
                submit_token.token(),
            ),
        )
        .await?;
        return Ok(page.into_response());
    }

    // Enforce the configured password policy — length, weak-list, and
    // similarity to the identifiers the account is being created with.
    let validation = validate_password(
        &form.password,
        &password_cfg.policy(),
        &[username.as_str(), email.as_str()],
    )
    .await;
    if !validation.is_valid() {
        // Show every failure at once rather than making the user discover them
        // one resubmission at a time.
        let messages = validation.messages();
        let message = if messages.is_empty() {
            "Invalid password".to_owned()
        } else {
            messages.join("\n")
        };
        let page = auth_page(
            &repos,
            &session,
            &csrf,
            "Register",
            redisplay(message, submit_token.token()),
        )
        .await?;
        return Ok(page.into_response());
    }

    let password_hash = hash_password(&form.password).await?;

    // The first account to exist owns the site. WordPress asks for this during
    // its five-minute install; doing it on first registration means a freshly
    // migrated database is usable with no seed step and no default credentials
    // committed anywhere.
    //
    // The election happens inside `register_user`'s transaction rather than
    // here: a `count() == 0` read followed by an insert lets two concurrent
    // signups both see an empty table and both become administrators, and the
    // bcrypt hash above sits right inside that window.
    // The connection is checked out here, after every repository read is
    // done, and released when the handler returns. Taking it as a `Db`
    // extractor instead held it across those reads — and each repository
    // call acquires a *second* connection from the same pool, so enough
    // concurrent requests could each hold one slot while waiting for a
    // second that only another of them could release. One connection at a
    // time, acquired last, cannot deadlock that way.
    let mut conn = repos.conn().await?;
    let created = crate::content::register_user(
        &mut conn,
        NewUser {
            username: username.clone(),
            email,
            password_hash,
            display_name: form.username.trim().to_owned(),
            role: Role::Subscriber.slug().to_owned(),
            bio: String::new(),
            website: String::new(),
        },
    )
    .await;

    let user = match created {
        Ok(user) => user,
        Err(error) => {
            // A duplicate username or email is the expected failure here.
            // Redisplay it on the form rather than sending the visitor to a
            // generic error page, but do not distinguish which field collided —
            // that would turn the form into an account-enumeration oracle.
            if matches!(error.status(), StatusCode::CONFLICT)
                || error.to_string().contains("unique")
            {
                let page = auth_page(
                    &repos,
                    &session,
                    &csrf,
                    "Register",
                    redisplay(
                        "That username or email is already taken".to_owned(),
                        submit_token.token(),
                    ),
                )
                .await?;
                return Ok(page.into_response());
            }
            return Err(error);
        }
    };

    session.rotate_id().await;
    session.insert("user_id", user.id.to_string()).await;
    // The role was decided inside `register_user`'s transaction, so read it
    // back off the row rather than from anything submitted.
    let destination = if user.role().can_access_admin() {
        "/admin"
    } else {
        "/"
    };
    Ok(Redirect::to(destination).into_response())
}
