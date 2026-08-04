//! Email invitations: create, accept, revoke, resend (issue #1261 AC4-AC6).
//!
//! Accepting a tokened link has two shapes:
//! - **(a)** logged-out visitor, no account for the invited email yet: the
//!   accept page embeds a signup form; posting it creates the account *and*
//!   the membership in one step (no separate detour through `/signup`, which
//!   would otherwise hand the new user an unwanted default organization).
//! - **(b)** already-authenticated visitor: the accept page just shows an
//!   "Accept" button that joins under the current session.
//!
//! A logged-out visitor whose email already has an account is sent to
//! `/login?next=/invitations/{token}` — completing the round trip converts
//! them into case (b).
//!
//! Accepting is idempotent (AC5): a second click never creates a second
//! `Membership` row or 500s. Expired/revoked/already-consumed tokens render a
//! clear error page (AC6), never a panic.

use autumn_web::auth::{generate_raw_token, hash_api_token, hash_password};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;
use serde::Deserialize;

use crate::mailers::invitation_mailer::InvitationMailer;
use crate::models::{
    Invitation, InviteForm, Membership, NewInvitation, NewUser, UpdateInvitation, User,
};
use crate::repositories::{
    InvitationRepository, OrganizationRepository, PgInvitationRepository, PgOrganizationRepository,
};
use crate::role::{Role, require_role};
use crate::schema::{invitations, memberships, users};

use super::auth::establish_session;
use super::layout::{invitation_error_page, layout};

const INVITATION_TTL_DAYS: i64 = 7;

// ── Create ───────────────────────────────────────────────────────────────────

/// Absolute base URL for links embedded in emails, matching the convention
/// `autumn generate auth`'s scaffolded email flows use.
fn app_base_url() -> String {
    std::env::var("APP_BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_owned())
}

/// Send an invitation into the *active* organization. Gated `Admin` or
/// higher (issue #1261 AC7).
#[post("/invitations")]
pub async fn create_invitation(
    session: Session,
    Tenant(org_id): Tenant,
    org_repo: PgOrganizationRepository,
    invitation_repo: PgInvitationRepository,
    mailer: Mailer,
    Form(form): Form<InviteForm>,
) -> AutumnResult<Response> {
    require_role(&session, Role::Admin).await?;
    let Some(inviter_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    let email = form.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return Err(AutumnError::unprocessable_msg(
            "Enter a valid email address",
        ));
    }
    let Some(role) = Role::parse(&form.role) else {
        return Err(AutumnError::unprocessable_msg("Unknown role"));
    };

    let org_id: i64 = org_id.parse().map_err(|_| {
        AutumnError::internal_server_error_msg("Corrupt organization id in session")
    })?;
    let Some(organization) = org_repo.find_by_id(org_id).await? else {
        return Err(AutumnError::not_found_msg("Organization not found"));
    };

    let raw_token = generate_raw_token();
    invitation_repo
        .save(&NewInvitation {
            email: email.clone(),
            role: role.as_str().to_owned(),
            token_hash: hash_api_token(&raw_token),
            status: "pending".to_owned(),
            invited_by_user_id: inviter_id,
            expires_at: chrono::Utc::now().naive_utc()
                + chrono::Duration::days(INVITATION_TTL_DAYS),
        })
        .await?;

    let accept_url = format!("{}/invitations/{raw_token}", app_base_url());
    InvitationMailer.deliver_later_invite(
        &mailer,
        email,
        organization.name,
        role.as_str().to_owned(),
        accept_url,
    );

    Ok(Redirect::to("/members").into_response())
}

// ── Accept (show) ────────────────────────────────────────────────────────────

fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// `Invitation`/`Membership::tenant_id` is a `String` (the `tenant_scoped`
/// macro's convention — see `models.rs`); parse it back to the
/// `Organization.id` it names when a lookup on the `Organization` row itself
/// is needed.
fn parse_tenant_id(tenant_id: &str) -> AutumnResult<i64> {
    tenant_id
        .parse()
        .map_err(|_| AutumnError::internal_server_error_msg("Corrupt tenant id"))
}

#[derive(Deserialize)]
pub struct AcceptForm {
    /// Present only on the signup-then-join path (case a); absent when an
    /// already-authenticated session is joining directly (case b).
    #[serde(default)]
    pub password: Option<String>,
}

async fn load_pending_invitation(
    invitation_repo: &PgInvitationRepository,
    raw_token: &str,
) -> AutumnResult<Option<Invitation>> {
    let token_hash = hash_api_token(raw_token);
    let mut matches = invitation_repo
        .across_tenants()
        .find_by_token_hash(token_hash)
        .await?;
    Ok(matches.pop())
}

fn invitation_status_error(invitation: &Invitation) -> Option<&'static str> {
    match invitation.status.as_str() {
        "accepted" => Some("This invitation has already been used."),
        "revoked" => Some("This invitation was revoked."),
        _ if invitation.expires_at <= chrono::Utc::now().naive_utc() => {
            Some("This invitation has expired.")
        }
        _ => None,
    }
}

#[get("/invitations/{token}")]
pub async fn show_invitation(
    session: Session,
    mut db: Db,
    invitation_repo: PgInvitationRepository,
    org_repo: PgOrganizationRepository,
    Path(raw_token): Path<String>,
) -> AutumnResult<Response> {
    let Some(invitation) = load_pending_invitation(&invitation_repo, &raw_token).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page("This invitation link is invalid."),
        )
            .into_response());
    };
    if let Some(message) = invitation_status_error(&invitation) {
        return Ok((StatusCode::GONE, invitation_error_page(message)).into_response());
    }
    let Some(organization) = org_repo
        .find_by_id(parse_tenant_id(&invitation.tenant_id)?)
        .await?
    else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page("This invitation's organization no longer exists."),
        )
            .into_response());
    };

    let signed_in = session.get("user_id").await.is_some();

    let page = if signed_in {
        layout(
            "Accept invitation",
            true,
            html! {
                div class="bg-white rounded-lg shadow p-6 max-w-md" {
                    h1 class="text-xl font-bold mb-2" { "Join " (organization.name) }
                    p class="text-gray-600 mb-4" { "You've been invited as " (invitation.role) "." }
                    form action={"/invitations/" (raw_token) "/accept"} method="post" {
                        button type="submit"
                               class="w-full bg-indigo-600 text-white py-2 rounded hover:bg-indigo-700" {
                            "Accept invitation"
                        }
                    }
                }
            },
        )
    } else {
        let email = normalize_email(&invitation.email);
        let existing_user: Option<User> = users::table
            .filter(users::email.eq(&email))
            .select(User::as_select())
            .first(&mut *db)
            .await
            .optional()?;

        if existing_user.is_some() {
            layout(
                "Accept invitation",
                false,
                html! {
                    div class="bg-white rounded-lg shadow p-6 max-w-md" {
                        h1 class="text-xl font-bold mb-2" { "Join " (organization.name) }
                        p class="text-gray-600 mb-4" {
                            "You've been invited as " (invitation.role) ". Log in as "
                            strong { (invitation.email) } " to accept."
                        }
                        a href={"/login?next=/invitations/" (raw_token)}
                          class="inline-block bg-indigo-600 text-white py-2 px-4 rounded hover:bg-indigo-700" {
                            "Log in to accept"
                        }
                    }
                },
            )
        } else {
            layout(
                "Accept invitation",
                false,
                html! {
                    div class="bg-white rounded-lg shadow p-6 max-w-md" {
                        h1 class="text-xl font-bold mb-2" { "Join " (organization.name) }
                        p class="text-gray-600 mb-4" {
                            "You've been invited as " (invitation.role)
                            ". Create an account to accept."
                        }
                        form action={"/invitations/" (raw_token) "/accept"} method="post" class="space-y-4" {
                            div {
                                label for="email" class="block text-sm font-medium mb-1" { "Email" }
                                input #email type="email" value=(invitation.email) readonly disabled
                                      class="w-full border rounded px-3 py-2 bg-gray-100";
                            }
                            div {
                                label for="password" class="block text-sm font-medium mb-1" { "Password" }
                                input #password type="password" name="password" required
                                      autocomplete="new-password" class="w-full border rounded px-3 py-2";
                            }
                            button type="submit"
                                   class="w-full bg-indigo-600 text-white py-2 rounded hover:bg-indigo-700" {
                                "Create account and join"
                            }
                        }
                    }
                },
            )
        }
    };
    Ok(page.into_response())
}

// ── Accept (confirm) ─────────────────────────────────────────────────────────

#[post("/invitations/{token}/accept")]
pub async fn accept_invitation(
    session: Session,
    mut db: Db,
    invitation_repo: PgInvitationRepository,
    State(state): State<AppState>,
    Path(raw_token): Path<String>,
    Form(form): Form<AcceptForm>,
) -> AutumnResult<Response> {
    let Some(invitation) = load_pending_invitation(&invitation_repo, &raw_token).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page("This invitation link is invalid."),
        )
            .into_response());
    };

    let existing_session_user: Option<i64> =
        session.get("user_id").await.and_then(|s| s.parse().ok());

    // Resolve (or create) the joining user *before* the membership
    // transaction: case (a) creates the account here rather than via
    // `/signup`, which would otherwise saddle the new user with an unwanted
    // default organization.
    let target_user_id = if let Some(uid) = existing_session_user {
        uid
    } else {
        let email = normalize_email(&invitation.email);
        let existing_user: Option<User> = users::table
            .filter(users::email.eq(&email))
            .select(User::as_select())
            .first(&mut *db)
            .await
            .optional()?;
        match existing_user {
            Some(u) => u.id,
            None => {
                let Some(password) = form.password.as_deref().filter(|p| !p.is_empty()) else {
                    return Err(AutumnError::unprocessable_msg(
                        "A password is required to create your account",
                    ));
                };
                if password.len() > 128 {
                    return Err(AutumnError::unprocessable_msg(
                        "Password must be at most 128 characters",
                    ));
                }
                let password_cfg = state.config().auth.password;
                let policy = password_cfg.policy();
                let validation =
                    autumn_web::auth::validate_password(password, &policy, &[email.as_str()]).await;
                if !validation.is_valid() {
                    let messages = validation.messages();
                    let message = if messages.is_empty() {
                        "Invalid password".to_owned()
                    } else {
                        messages.join("\n")
                    };
                    return Err(AutumnError::unprocessable_msg(message));
                }
                let password_hash = hash_password(password).await?;
                let user: User = diesel::insert_into(users::table)
                    .values(&NewUser {
                        email: email.clone(),
                        password_hash,
                    })
                    .returning(User::as_returning())
                    .get_result(&mut *db)
                    .await
                    .map_err(|_| AutumnError::unprocessable_msg("Could not create account"))?;
                user.id
            }
        }
    };

    let invitation_id = invitation.id;
    let tenant_id = invitation.tenant_id.clone();
    let role = invitation.role.clone();

    // Raw queries (not the generated `tenant_scoped` repository) inside one
    // transaction: the repository's CRUD methods each acquire their own
    // pooled connection, which can't share this transaction's connection —
    // and atomicity across the row-lock + insert + status update is exactly
    // what makes double-click acceptance idempotent instead of racy.
    let membership: Membership = db
        .tx(move |conn| {
            async move {
                // Lock the invitation row so two concurrent accepts serialize
                // rather than race past the pending/expiry check together.
                let invitation: Invitation = invitations::table
                    .filter(invitations::id.eq(invitation_id))
                    .for_update()
                    .select(Invitation::as_select())
                    .first(conn)
                    .await?;

                let existing: Option<Membership> = memberships::table
                    .filter(memberships::tenant_id.eq(&tenant_id))
                    .filter(memberships::user_id.eq(target_user_id))
                    .select(Membership::as_select())
                    .first(conn)
                    .await
                    .optional()?;
                if let Some(existing) = existing {
                    // Idempotent double-click: the membership this accept
                    // would create already exists (from an earlier accept of
                    // this same token). Succeed as a no-op rather than
                    // erroring or inserting a duplicate.
                    return Ok::<_, AutumnError>(existing);
                }

                if invitation.status != "pending"
                    || invitation.expires_at <= chrono::Utc::now().naive_utc()
                {
                    return Err(AutumnError::gone_msg(
                        "This invitation is no longer available",
                    ));
                }

                #[derive(diesel::Insertable)]
                #[diesel(table_name = memberships)]
                struct InsertMembership {
                    tenant_id: String,
                    user_id: i64,
                    role: String,
                }
                let membership: Membership = diesel::insert_into(memberships::table)
                    .values(&InsertMembership {
                        tenant_id: tenant_id.clone(),
                        user_id: target_user_id,
                        role: role.clone(),
                    })
                    .returning(Membership::as_returning())
                    .get_result(conn)
                    .await?;

                diesel::update(invitations::table.filter(invitations::id.eq(invitation_id)))
                    .set(invitations::status.eq("accepted"))
                    .execute(conn)
                    .await?;

                Ok(membership)
            }
            .scope_boxed()
        })
        .await?;

    let Some(resolved_role) = Role::parse(&membership.role) else {
        return Err(AutumnError::internal_server_error_msg(
            "Corrupt membership role",
        ));
    };
    establish_session(
        &session,
        target_user_id,
        &membership.tenant_id,
        resolved_role,
    )
    .await;
    Ok(Redirect::to("/members").into_response())
}

// ── Revoke / resend ──────────────────────────────────────────────────────────

/// Revoke a pending invitation. Gated `Admin` or higher.
#[post("/invitations/{id}/revoke")]
pub async fn revoke_invitation(
    session: Session,
    invitation_repo: PgInvitationRepository,
    Path(invitation_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, Role::Admin).await?;
    invitation_repo
        .update(
            invitation_id,
            &UpdateInvitation {
                status: Patch::Set("revoked".to_owned()),
                ..Default::default()
            },
        )
        .await?;
    Ok(Redirect::to("/members").into_response())
}

/// Re-send a pending invitation: revoke the old token and mint a fresh one
/// with a new expiry (issue #1261 AC6 — "new token, old token invalidated").
#[post("/invitations/{id}/resend")]
pub async fn resend_invitation(
    session: Session,
    org_repo: PgOrganizationRepository,
    invitation_repo: PgInvitationRepository,
    mailer: Mailer,
    Path(invitation_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, Role::Admin).await?;
    let Some(inviter_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };
    let Some(old) = invitation_repo.find_by_id(invitation_id).await? else {
        return Err(AutumnError::not_found_msg("Invitation not found"));
    };
    invitation_repo
        .update(
            invitation_id,
            &UpdateInvitation {
                status: Patch::Set("revoked".to_owned()),
                ..Default::default()
            },
        )
        .await?;

    let raw_token = generate_raw_token();
    invitation_repo
        .save(&NewInvitation {
            email: old.email.clone(),
            role: old.role.clone(),
            token_hash: hash_api_token(&raw_token),
            status: "pending".to_owned(),
            invited_by_user_id: inviter_id,
            expires_at: chrono::Utc::now().naive_utc()
                + chrono::Duration::days(INVITATION_TTL_DAYS),
        })
        .await?;

    let Some(organization) = org_repo
        .find_by_id(parse_tenant_id(&old.tenant_id)?)
        .await?
    else {
        return Err(AutumnError::not_found_msg("Organization not found"));
    };
    let accept_url = format!("{}/invitations/{raw_token}", app_base_url());
    InvitationMailer.deliver_later_invite(
        &mailer,
        old.email,
        organization.name,
        old.role,
        accept_url,
    );

    Ok(Redirect::to("/members").into_response())
}
