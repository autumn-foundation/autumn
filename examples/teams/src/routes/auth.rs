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
use autumn_web::tenancy::with_tenant;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{
    LoginForm, Membership, NewMembership, NewOrganization, NewUser, SignupForm, User,
};
use crate::repositories::{
    MembershipRepository, OrganizationRepository, PgMembershipRepository,
    PgOrganizationRepository,
};
use crate::role::Role;
use crate::schema::users;

use super::layout::layout;

// bcrypt hash used as a dummy target when the email is not found, so the
// login handler takes the same wall time whether or not the account exists.
const DUMMY_HASH: &str = "$2b$12$Ro0CUfOqk6cXEKf3dyaM7OhSCvnwM9s1Aw6lfLP2.GvpAfNXwi.2K";

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
    org_repo: PgOrganizationRepository,
    membership_repo: PgMembershipRepository,
    Form(form): Form<SignupForm>,
) -> AutumnResult<Response> {
    let email = form.email.trim().to_lowercase();
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
    let password_cfg = state.config().auth.password;
    let policy = password_cfg.policy();
    let validation =
        autumn_web::auth::validate_password(&form.password, &policy, &[email.as_str()]).await;
    if !validation.is_valid() {
        let messages = validation.messages();
        let message = if messages.is_empty() {
            "Invalid password".to_owned()
        } else {
            messages.join("\n")
        };
        return Ok(signup_page(password_cfg.min_length, Some(&message)).into_response());
    }

    let password_hash = hash_password(&form.password).await?;

    let user: User = diesel::insert_into(users::table)
        .values(&NewUser {
            email: email.clone(),
            password_hash,
        })
        .returning(User::as_returning())
        .get_result(&mut *db)
        .await
        .map_err(|_| AutumnError::unprocessable_msg("Could not create account"))?;

    // No invite in play: give the new user their own organization as Owner
    // (issue #1261 AC3 — "creating an organization makes the creator an
    // Owner member"). `Organization` is not tenant-scoped (it *is* the
    // tenant), so this insert needs no tenant context.
    let organization_name = format!("{email}'s Organization");
    let org = org_repo
        .save(&NewOrganization {
            name: organization_name,
        })
        .await?;
    let role = Role::Owner;

    // The membership row is tenant-scoped by `organization_id`, but no
    // organization is active in this (pre-login) session yet — supply the
    // brand-new org explicitly via `with_tenant` rather than relying on the
    // session/middleware-derived ambient tenant.
    with_tenant(org.id.to_string(), async {
        membership_repo
            .save(&NewMembership {
                user_id: user.id,
                role: role.as_str().to_owned(),
            })
            .await
    })
    .await?;

    establish_session(&session, user.id, &org.id.to_string(), role).await;
    Ok(Redirect::to("/members").into_response())
}

/// Only ever redirect to a same-origin path after login — an unvalidated
/// `next` would be an open-redirect vector.
fn safe_next(next: Option<&str>) -> &str {
    match next {
        Some(path) if path.starts_with('/') && !path.starts_with("//") => path,
        _ => "/members",
    }
}

#[get("/login")]
pub async fn login_form(Query(query): Query<crate::models::NextQuery>) -> Markup {
    let next = query.next.unwrap_or_default();
    layout(
        "Log in",
        false,
        html! {
            h1 class="text-2xl font-bold mb-6" { "Log in" }
            form action="/login" method="post" class="space-y-4 bg-white rounded-lg shadow p-6 max-w-md" {
                @if !next.is_empty() {
                    input type="hidden" name="next" value=(next);
                }
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
    membership_repo: PgMembershipRepository,
    Form(form): Form<LoginForm>,
) -> AutumnResult<Response> {
    let email = form.email.trim().to_lowercase();
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
