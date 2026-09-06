//! Signup, login, and logout.
//!
//! Composes shipped primitives: `Session` for the cookie-backed session,
//! `hash_password`/`verify_password` (bcrypt) for credentials, and plain
//! Diesel for the user row.
//!
//! Signing up with no pending invitation creates a personal `Organization`
//! and makes the new user its `Owner` (issue #1261 AC3). Signing up *via* an
//! invite link instead joins the invited organization — see
//! `routes::invitations::accept_invitation`.
//!
//! On success we store `user_id`, `organization_id` (the active org — the
//! tenant, per `[tenancy]` in `autumn.toml`), and `role` (the caller's role in
//! that org) in the session. The `role` key is the same one
//! `#[secured("...")]`/`PolicyContext::has_role` already read (issue #496),
//! so no second authorization mechanism is introduced.

use autumn_web::auth::{hash_password, verify_password};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::models::{LoginForm, Membership, NewUser, Organization, SignupForm, User};
use crate::repositories::{MembershipRepository, PgMembershipRepository};
use crate::role::Role;
use crate::schema::{memberships, organizations, users};

use super::layout::{csrf_value, layout};

// bcrypt hash used as a dummy target when the email is not found, so the
// login handler takes the same wall time whether or not the account exists.
const DUMMY_HASH: &str = "$2b$12$Ro0CUfOqk6cXEKf3dyaM7OhSCvnwM9s1Aw6lfLP2.GvpAfNXwi.2K";

/// `email` re-fills the address the user already typed so a rejected
/// password doesn't also cost them re-typing an unrelated field. `messages`
/// renders as a list rather than a single blob of text: joined into one
/// `<p>` they used to collapse into a run-on sentence (HTML collapses the
/// `\n` join to a single space), which defeated the point of reporting every
/// failure at once.
fn signup_page(min_len: usize, csrf_token: &str, email: &str, messages: &[String]) -> Markup {
    layout(
        "Sign up",
        false,
        csrf_token,
        html! {
            h1 class="text-2xl font-bold mb-6" { "Create your account" }
            @if !messages.is_empty() {
                ul class="mb-4 text-sm text-red-600 list-disc pl-5" role="alert" {
                    @for message in messages {
                        li { (message) }
                    }
                }
            }
            form action="/signup" method="post" class="space-y-4 bg-white rounded-lg shadow p-6 max-w-md" {
                input type="hidden" name="_csrf" value=(csrf_token);
                div {
                    label for="email" class="block text-sm font-medium mb-1" { "Email" }
                    input #email type="email" name="email" value=(email) required autocomplete="email"
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
pub async fn signup_form(State(state): State<AppState>, csrf: Option<CsrfToken>) -> Markup {
    signup_page(
        state.config_arc().auth.password.min_length,
        csrf_value(&csrf),
        "",
        &[],
    )
}

#[post("/signup")]
pub async fn signup(
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
    csrf: Option<CsrfToken>,
    Form(form): Form<SignupForm>,
) -> AutumnResult<Response> {
    let email = form.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return Ok(signup_page(
            state.config_arc().auth.password.min_length,
            csrf_value(&csrf),
            &email,
            &["Enter a valid email address (max 254 characters)".to_owned()],
        )
        .into_response());
    }
    if form.password.len() > 128 {
        return Ok(signup_page(
            state.config_arc().auth.password.min_length,
            csrf_value(&csrf),
            &email,
            &["Password must be at most 128 characters".to_owned()],
        )
        .into_response());
    }
    // `config_arc` shares the resolved config behind an `Arc`; `config()` would
    // deep-clone every section just to read `[auth.password]` on a request path.
    let config = state.config_arc();
    let policy = config.auth.password.policy();
    let validation =
        autumn_web::auth::validate_password(&form.password, &policy, &[email.as_str()]).await;
    if !validation.is_valid() {
        let messages = validation.messages();
        return Ok(signup_page(
            config.auth.password.min_length,
            csrf_value(&csrf),
            &email,
            &messages,
        )
        .into_response());
    }

    let password_hash = hash_password(&form.password).await?;

    // No invite in play: give the new user their own organization as Owner
    // (issue #1261 AC3 — "creating an organization makes the creator an
    // Owner member"). All three inserts run in one transaction: the
    // generated `tenant_scoped` repositories each acquire their own pooled
    // connection (which can't share this one), and raw queries on a single
    // `conn` are what make this atomic — otherwise a failure between the
    // user insert and the org/membership inserts would strand a
    // zero-membership account that can never log in (login rejects those)
    // and can never retry signup (`users.email` is `UNIQUE`).
    let organization_name = format!("{email}'s Organization");
    let role = Role::Owner;
    let (user, org): (User, Organization) = db
        .tx(move |conn| {
            async move {
                let user: User = diesel::insert_into(users::table)
                    .values(&NewUser {
                        email: email.clone(),
                        password_hash,
                    })
                    .returning(User::as_returning())
                    .get_result(conn)
                    .await
                    .map_err(|_| AutumnError::unprocessable_msg("Could not create account"))?;

                let org: Organization = diesel::insert_into(organizations::table)
                    .values(&InsertOrganization {
                        name: organization_name,
                    })
                    .returning(Organization::as_returning())
                    .get_result(conn)
                    .await?;

                diesel::insert_into(memberships::table)
                    .values(&InsertMembership {
                        tenant_id: org.id.to_string(),
                        user_id: user.id,
                        role: role.as_str().to_owned(),
                    })
                    .execute(conn)
                    .await?;

                Ok::<_, AutumnError>((user, org))
            }
            .scope_boxed()
        })
        .await?;

    establish_session(&session, user.id, &org.id.to_string(), role).await;
    Ok(Redirect::to("/members").into_response())
}

#[derive(diesel::Insertable)]
#[diesel(table_name = organizations)]
struct InsertOrganization {
    name: String,
}

#[derive(diesel::Insertable)]
#[diesel(table_name = memberships)]
struct InsertMembership {
    tenant_id: String,
    user_id: i64,
    role: String,
}

/// Only ever redirect to a same-origin path after login — an unvalidated
/// `next` would be an open-redirect vector.
///
/// Rejects a leading `//` (a protocol-relative URL browsers resolve against
/// the current scheme but an arbitrary host) *and* any backslash anywhere in
/// the path: browsers parsing an HTTP(S) URL treat `\` the same as `/`, so
/// `/\evil.com` (which this app never legitimately needs — no route here
/// uses one) would otherwise slip past the `//` check while still resolving
/// as `//evil.com` in the browser, redirecting off-site (Codex review
/// finding).
fn safe_next(next: Option<&str>) -> &str {
    match next {
        Some(path) if path.starts_with('/') && !path.starts_with("//") && !path.contains('\\') => {
            path
        }
        _ => "/members",
    }
}

/// Render the login form, optionally re-filling `email` / `next` and showing
/// an authentication `error` inline.
///
/// Mirrors `signup_page`: a failed login used to `Err(...)` straight to the
/// framework's generic full-page error screen (`ErrorPageFilter`), which
/// threw the user off the login page entirely — losing the email they'd
/// typed and the post-login `next` destination — instead of keeping them on
/// the form the way a rejected signup already does.
fn login_page(email: &str, next: &str, csrf_token: &str, error: Option<&str>) -> Markup {
    layout(
        "Log in",
        false,
        csrf_token,
        html! {
            h1 class="text-2xl font-bold mb-6" { "Log in" }
            @if let Some(error) = error {
                p class="mb-4 text-sm text-red-600" role="alert" { (error) }
            }
            form action="/login" method="post" class="space-y-4 bg-white rounded-lg shadow p-6 max-w-md" {
                input type="hidden" name="_csrf" value=(csrf_token);
                @if !next.is_empty() {
                    input type="hidden" name="next" value=(next);
                }
                div {
                    label for="email" class="block text-sm font-medium mb-1" { "Email" }
                    input #email type="email" name="email" value=(email) required autocomplete="email"
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

#[get("/login")]
pub async fn login_form(
    Query(query): Query<crate::models::NextQuery>,
    csrf: Option<CsrfToken>,
) -> Markup {
    let next = query.next.unwrap_or_default();
    login_page("", &next, csrf_value(&csrf), None)
}

#[post("/login")]
pub async fn login(
    session: Session,
    mut db: Db,
    membership_repo: PgMembershipRepository,
    csrf: Option<CsrfToken>,
    Form(form): Form<LoginForm>,
) -> AutumnResult<Response> {
    let email = form.email.trim().to_lowercase();
    let next = form.next.clone().unwrap_or_default();
    if email.len() > 254 || form.password.len() > 128 {
        return Ok(login_page(
            &email,
            &next,
            csrf_value(&csrf),
            Some("Invalid email or password"),
        )
        .into_response());
    }

    let user: Option<User> = users::table
        .filter(users::email.eq(&email))
        .select(User::as_select())
        .first(&mut *db)
        .await
        .optional()?;

    let user = match user {
        Some(u) => u,
        None => {
            let _ = verify_password(&form.password, DUMMY_HASH).await;
            return Ok(login_page(
                &email,
                &next,
                csrf_value(&csrf),
                Some("Invalid email or password"),
            )
            .into_response());
        }
    };
    if !verify_password(&form.password, &user.password_hash).await? {
        return Ok(login_page(
            &email,
            &next,
            csrf_value(&csrf),
            Some("Invalid email or password"),
        )
        .into_response());
    }

    let memberships: Vec<Membership> = membership_repo
        .across_tenants()
        .find_by_user_id(user.id)
        .await?;

    let Some(active) = memberships.into_iter().min_by_key(|m| m.id) else {
        // A user with zero memberships (shouldn't happen via this app's own
        // signup flow, but is reachable if the account row is seeded by
        // hand) has nowhere to land — send them to sign up a fresh org
        // rather than 500.
        return Err(AutumnError::forbidden_msg(
            "This account does not belong to any organization",
        ));
    };
    let Some(role) = Role::parse(&active.role) else {
        return Err(AutumnError::internal_server_error_msg(
            "Corrupt membership role",
        ));
    };

    establish_session(&session, user.id, &active.tenant_id, role).await;
    Ok(Redirect::to(safe_next(form.next.as_deref())).into_response())
}

#[post("/logout")]
pub async fn logout(session: Session) -> Response {
    session.clear().await;
    session.rotate_id().await;
    Redirect::to("/").into_response()
}

/// Log a user in: rotate the session id (prevents fixation) and record the
/// account + active organization + role the rest of the app scopes to.
///
/// `tenant_id` is the active `Organization.id` in its string form — the same
/// value the `tenant_scoped` `Membership`/`Invitation` repositories filter by
/// (see `models.rs`'s doc comment on `Membership::tenant_id`).
pub async fn establish_session(session: &Session, user_id: i64, tenant_id: &str, role: Role) {
    session.rotate_id().await;
    session.insert("user_id", user_id.to_string()).await;
    session.insert("organization_id", tenant_id).await;
    session.insert("role", role.as_str()).await;
}

#[cfg(test)]
mod tests {
    use super::safe_next;

    #[test]
    fn accepts_a_same_origin_path() {
        assert_eq!(safe_next(Some("/members")), "/members");
    }

    #[test]
    fn rejects_missing_or_empty_next() {
        assert_eq!(safe_next(None), "/members");
        assert_eq!(safe_next(Some("")), "/members");
    }

    #[test]
    fn rejects_a_protocol_relative_url() {
        assert_eq!(safe_next(Some("//evil.test")), "/members");
    }

    #[test]
    fn rejects_a_scheme_qualified_url() {
        assert_eq!(safe_next(Some("https://evil.test")), "/members");
    }

    /// Browsers parsing an HTTP(S) URL treat `\` the same as `/`, so a path
    /// starting with a single `/` followed by `\` resolves as a
    /// protocol-relative URL in the browser even though it isn't literally
    /// `//`-prefixed (Codex review finding).
    #[test]
    fn rejects_a_backslash_disguised_protocol_relative_url() {
        assert_eq!(safe_next(Some("/\\evil.test")), "/members");
        assert_eq!(safe_next(Some("/\\/evil.test")), "/members");
    }

    #[test]
    fn rejects_a_backslash_anywhere_in_the_path() {
        assert_eq!(safe_next(Some("/members\\..\\evil")), "/members");
    }
}
