//! Authentication routes — register, login, logout.
//!
//! Demonstrates: Session extractor, password hashing (bcrypt),
//! session.insert / clear / rotate_id, flash messages, `CsrfToken`, form handling.

use autumn_web::auth::{hash_password, verify_password};
use autumn_web::extract::Path;
use autumn_web::extract::State;
use autumn_web::prelude::*;
use autumn_web::storage::{Transform, VariantBudget};
use autumn_web::webhook_outbound::WebhookOutboundManager;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::jobs::{UserOnboardingArgs, UserOnboardingJob};
use crate::models::{NewUser, User};
use crate::schema::users;

use super::layout::layout;

struct AccountMailer;

#[mailer]
impl AccountMailer {
    fn welcome(&self, to: String, username: String) -> Mail {
        Mail::builder()
            .to(to)
            .subject("Welcome to Autumn Reddit")
            .html(html! {
                p { "Welcome, " strong { (username) } "!" }
                p { "Your account is ready. Go find something worth arguing about." }
            })
            .text(format!(
                "Welcome, {username}! Your account is ready. Go find something worth arguing about."
            ))
            .build()
            .expect("static welcome template should be valid")
    }
}

#[mailer_preview]
impl AccountMailer {
    fn welcome_preview() -> Mail {
        AccountMailer.welcome(
            "preview@example.com".to_owned(),
            "cool_rustacean".to_owned(),
        )
    }
}

pub fn mail_previews() -> Vec<MailPreview> {
    mail_previews![AccountMailer]
}

// ── Register ───────────────────────────────────────────────────

#[get("/register")]
pub async fn register_form(csrf: CsrfToken) -> Markup {
    layout(
        "Sign Up",
        None,
        Some(csrf.token()),
        html! {
            div class="max-w-md mx-auto" {
                h1 class="text-2xl font-bold mb-6" { "Create an Account" }
                form action="/register" method="post" class="space-y-4 bg-white rounded-lg shadow p-6" {
                    input type="hidden" name="_csrf" value=(csrf.token());
                    div {
                        label for="username" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Username"
                        }
                        input type="text" id="username" name="username" required
                              autocomplete="username"
                              placeholder="cool_rustacean"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="email" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Email"
                        }
                        input type="email" id="email" name="email" required
                              autocomplete="email"
                              placeholder="you@example.com"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="password" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Password"
                        }
                        input type="password" id="password" name="password" required
                              autocomplete="new-password"
                              minlength="6"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    button type="submit"
                           class="w-full bg-orange-500 text-white py-2 rounded font-medium \
                                  hover:bg-orange-600 transition-colors" {
                        "Sign Up"
                    }
                    p class="text-center text-sm text-gray-500" {
                        "Already have an account? "
                        a href="/login" class="text-orange-600 hover:underline" { "Log in" }
                    }
                }
            }
        },
    )
}

#[derive(serde::Deserialize)]
pub struct RegisterForm {
    pub username: String,
    pub email: String,
    pub password: String,
}

#[post("/register")]
pub async fn register(
    State(state): State<AppState>,
    mut db: Db,
    mailer: Mailer,
    session: Session,
    events: autumn_web::events::Events,
    flash: Flash,
    form: Form<RegisterForm>,
) -> AutumnResult<Redirect> {
    let open = crate::config_svc()
        .get("registration_open")
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !open {
        return Err(AutumnError::unprocessable_msg(
            "Registrations are currently closed",
        ));
    }

    let username = form.0.username.trim().to_lowercase();
    let email = form.0.email.trim().to_owned();
    let password = form.0.password;

    if username.len() < 2 || username.len() > 32 {
        return Err(AutumnError::unprocessable_msg(
            "Username must be 2-32 characters",
        ));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(AutumnError::unprocessable_msg(
            "Username may only contain letters, numbers, and underscores",
        ));
    }
    if password.len() < 6 {
        return Err(AutumnError::unprocessable_msg(
            "Password must be at least 6 characters",
        ));
    }
    if !email.contains('@') {
        return Err(AutumnError::unprocessable_msg("Email address is invalid"));
    }

    // Check if username already taken
    let existing: i64 = users::table
        .filter(users::username.eq(&username))
        .count()
        .get_result(&mut *db)
        .await?;

    if existing > 0 {
        return Err(AutumnError::unprocessable_msg("Username already taken"));
    }

    let hashed = hash_password(&password).await?;
    let new_user = NewUser {
        username: username.clone(),
        password_hash: hashed,
    };

    let user = db
        .tx(move |conn| {
            async move {
                let user: User = diesel::insert_into(users::table)
                    .values(&new_user)
                    .returning(User::as_returning())
                    .get_result(conn)
                    .await
                    .map_err(|_| AutumnError::unprocessable_msg("Username already taken"))?;

                // The default profile uses the Postgres job backend, so the
                // job row is inserted on this transaction connection.
                autumn_web::job::enqueue_on_conn(
                    UserOnboardingJob::NAME,
                    UserOnboardingArgs::from_user(&user),
                    conn,
                )
                .await?;

                AccountMailer.deliver_later_welcome(&mailer, email, user.username.clone());

                Ok::<_, AutumnError>(user)
            }
            .scope_boxed()
        })
        .await?;

    // Publish a typed domain event. Listeners (see `crate::listeners`) react
    // independently — adding a new reaction needs zero edits to this handler.
    // Use the injected `Events` extractor so dispatch is scoped to this app.
    events
        .publish(crate::events::UserSignedUp {
            user_id: user.id,
            username: user.username.clone(),
        })
        .await?;

    // Dispatch the outbound webhook event on "user.created"
    if let Some(manager) = state.extension::<WebhookOutboundManager>() {
        let dispatch_result = manager.dispatch(&state, "user.created", &user).await;
        if let Err(e) = dispatch_result {
            tracing::error!(error = %e, "Failed to dispatch user.created webhook event");
        }
    }

    // Log in immediately after registration
    session.rotate_id().await;
    session.insert("user_id", user.id.to_string()).await;
    session.insert("username", &user.username).await;
    session.insert("role", &user.role).await;

    flash
        .success(format!("Welcome to autumn/reddit, u/{}!", user.username))
        .await;
    Ok(Redirect::to("/"))
}

// ── Login ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn welcome_email_is_captured_as_eml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mailer = Mailer::builder()
            .transport(Transport::File)
            .from("Autumn <noreply@example.com>")
            .file_dir(dir.path())
            .build()
            .expect("file mailer should build");

        AccountMailer
            .send_welcome(
                &mailer,
                "new-user@example.com".to_owned(),
                "cool_rustacean".to_owned(),
            )
            .await
            .expect("send should succeed");

        let entry = std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .next()
            .expect("one email should be captured")
            .expect("dir entry");
        let eml = std::fs::read_to_string(entry.path()).expect("eml readable");
        assert!(eml.contains("To:"), "missing To header: {eml}");
        assert!(
            eml.contains("new-user@example.com"),
            "missing recipient address: {eml}"
        );
        assert!(eml.contains("Subject: Welcome to Autumn Reddit"));
        assert!(eml.contains("cool_rustacean"));
    }

    #[tokio::test]
    async fn onboarding_enqueue_failure_is_returned_to_registration() {
        let user = User {
            id: 42,
            username: "ferris".to_owned(),
            password_hash: "hashed".to_owned(),
            karma: 0,
            role: "user".to_owned(),
            created_at: chrono::DateTime::UNIX_EPOCH.naive_utc(),
            avatar: None,
        };

        let error = UserOnboardingJob::enqueue(UserOnboardingArgs::from_user(&user))
            .await
            .expect_err("missing job runtime should fail registration");

        assert!(
            error.to_string().contains("job runtime is not initialized"),
            "unexpected error: {error}"
        );
    }
}

#[get("/login")]
pub async fn login_form(csrf: CsrfToken) -> Markup {
    layout(
        "Log In",
        None,
        Some(csrf.token()),
        html! {
            div class="max-w-md mx-auto" {
                h1 class="text-2xl font-bold mb-6" { "Log In" }
                form action="/login" method="post" class="space-y-4 bg-white rounded-lg shadow p-6" {
                    input type="hidden" name="_csrf" value=(csrf.token());
                    div {
                        label for="username" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Username"
                        }
                        input type="text" id="username" name="username" required
                              autocomplete="username"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="password" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Password"
                        }
                        input type="password" id="password" name="password" required
                              autocomplete="current-password"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    button type="submit"
                           class="w-full bg-orange-500 text-white py-2 rounded font-medium \
                                  hover:bg-orange-600 transition-colors" {
                        "Log In"
                    }
                    p class="text-center text-sm text-gray-500" {
                        "New here? "
                        a href="/register" class="text-orange-600 hover:underline" { "Create an account" }
                    }
                }
            }
        },
    )
}

#[derive(serde::Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

#[post("/login")]
pub async fn login(
    mut db: Db,
    session: Session,
    flash: Flash,
    form: Form<LoginForm>,
) -> AutumnResult<Redirect> {
    let username = form.0.username.trim().to_lowercase();

    let user: User = users::table
        .filter(users::username.eq(&username))
        .select(User::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::bad_request_msg("Invalid username or password"))?;

    if !verify_password(&form.0.password, &user.password_hash).await? {
        return Err(AutumnError::bad_request_msg("Invalid username or password"));
    }

    // Rotate session ID to prevent session fixation
    session.rotate_id().await;
    session.insert("user_id", user.id.to_string()).await;
    session.insert("username", &user.username).await;
    session.insert("role", &user.role).await;

    flash
        .success(format!("Welcome back, u/{}!", user.username))
        .await;
    Ok(Redirect::to("/"))
}

// ── Logout ─────────────────────────────────────────────────────

#[post("/logout")]
pub async fn logout(
    session: Session,
    flash: Flash,
) -> autumn_web::reexports::axum::response::Response {
    // Clear the session data and rotate the id so the old cookie can't be
    // replayed, while letting a one-shot "signed out" notice ride the rotated
    // session through to the front page.
    session.clear().await;
    session.rotate_id().await;
    flash.info("You have been signed out.").await;
    super::layout::hx_redirect_to("/")
}

// ── Profile ────────────────────────────────────────────────────

#[get("/u/{username}")]
pub async fn profile(
    State(state): State<AppState>,
    Path(name): Path<String>,
    session: Session,
    csrf: CsrfToken,
    mut db: Db,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;

    let user: User = users::table
        .filter(users::username.eq(&name))
        .select(User::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg(format!("User u/{name} not found")))?;

    // Serve the avatar as a lazily-generated 64×64 thumbnail with EXIF
    // stripped.  The variant is content-addressed and generated at most
    // once; subsequent requests are a single head() cache hit.  In dev
    // the URL is an HMAC-signed `/_blobs/…` link; in prod it's a real
    // S3 presigned URL.  The expiry is long because the variant key is
    // immutable — same source + same spec always produces the same bytes.
    let avatar_url = match (
        user.avatar.as_ref(),
        state.extension::<autumn_web::storage::BlobStoreState>(),
    ) {
        (Some(blob), Some(blobs)) => {
            let store = blobs.store();
            let budget = VariantBudget::default();
            blob.variant(
                "thumb",
                &[
                    Transform::resize_to_limit(64, 64),
                    Transform::strip_metadata(),
                ],
            )
            .url(&**store, &budget, std::time::Duration::from_secs(3600))
            .await
            .ok()
        }
        _ => None,
    };
    let is_self = current_user.as_deref() == Some(user.username.as_str());

    Ok(layout(
        &format!("u/{}", user.username),
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            div class="bg-white rounded-lg shadow p-6" {
                div class="flex items-center gap-4 mb-4" {
                    @if let Some(url) = &avatar_url {
                        img src=(url) alt=(format!("u/{} avatar", user.username))
                            class="w-16 h-16 rounded-full object-cover";
                    } @else {
                        div class="w-16 h-16 bg-orange-100 text-orange-600 rounded-full \
                                   flex items-center justify-center text-2xl font-bold" {
                            (user.username.chars().next().unwrap_or('?').to_uppercase().to_string())
                        }
                    }
                    div {
                        h1 class="text-2xl font-bold" { "u/" (user.username) }
                        p class="text-sm text-gray-500" {
                            (user.karma) " karma"
                            " \u{2022} joined "
                            (user.created_at.format("%b %d, %Y"))
                        }
                        @if is_self {
                            p class="text-xs mt-1" {
                                a href="/settings/avatar"
                                  class="text-orange-600 hover:underline" { "Change picture" }
                            }
                        }
                    }
                }
            }
        },
    ))
}

autumn_web::paths![register_form, register, login_form, login, logout, profile];
