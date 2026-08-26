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
//! `/login?next=/invite/{token}` — completing the round trip converts
//! them into case (b).
//!
//! Accepting is idempotent (AC5): a second click never creates a second
//! `Membership` row or 500s. Expired/revoked/already-consumed tokens render a
//! clear error page (AC6), never a panic.
//!
//! The invitee-facing routes live under `/invite/...` — a *different* path
//! prefix than the admin-only `/invitations` routes below (`create`/`revoke`/
//! `resend`) — deliberately. `[tenancy] public_paths` matches by path
//! *prefix* (see `autumn.toml`), so an anonymous visitor following an accept
//! link needs `/invite` exempted from the tenancy gate, but the admin routes
//! must stay gated (they need the ambient tenant the gate establishes). A
//! single shared `/invitations` prefix would have exempted the admin routes
//! too — this split keeps the public surface exactly as small as it needs to
//! be instead of relying on a fragile allowlist.

use autumn_web::auth::{generate_raw_token, hash_api_token, hash_password, verify_password};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;
use serde::Deserialize;

use crate::mailers::invitation_mailer::InvitationMailer;
use crate::models::{Invitation, InviteForm, Membership, NewUser, User};
use crate::repositories::{
    InvitationRepository, OrganizationRepository, PgInvitationRepository, PgMembershipRepository,
    PgOrganizationRepository,
};
use crate::role::{Role, require_role};
use crate::schema::{invitations, memberships, users};

use super::auth::establish_session;
use super::layout::{csrf_value, invitation_error_page, layout};

const INVITATION_TTL_DAYS: i64 = 7;

// Raw-insert structs for the two places that write `memberships`/`invitations`
// inside a hand-rolled `db.tx` transaction rather than through the
// `tenant_scoped` repository (whose CRUD methods each acquire their own
// pooled connection, which can't share an outer transaction's connection) —
// see `accept_invitation` and `resend_invitation`.

#[derive(diesel::Insertable)]
#[diesel(table_name = memberships)]
struct InsertMembership {
    tenant_id: String,
    user_id: i64,
    role: String,
}

#[derive(diesel::Insertable)]
#[diesel(table_name = invitations)]
struct InsertInvitation {
    tenant_id: String,
    email: String,
    role: String,
    token_hash: String,
    status: String,
    invited_by_user_id: i64,
    expires_at: chrono::NaiveDateTime,
}

// ── Create ───────────────────────────────────────────────────────────────────

/// Absolute base URL for links embedded in emails, matching the convention
/// `autumn generate auth`'s scaffolded email flows use.
fn app_base_url() -> String {
    std::env::var("APP_BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_owned())
}

/// Send an invitation into the *active* organization. Gated `Admin` or
/// higher (issue #1261 AC7); inviting someone as `Owner` itself requires the
/// caller to already be an `Owner` — otherwise an Admin could mint a fresh
/// Owner account of their own choosing.
#[post("/invitations")]
pub async fn create_invitation(
    session: Session,
    Tenant(tenant_id): Tenant,
    mut db: Db,
    org_repo: PgOrganizationRepository,
    membership_repo: PgMembershipRepository,
    mailer: Mailer,
    Form(form): Form<InviteForm>,
) -> AutumnResult<Response> {
    let caller_role = require_role(&session, &membership_repo, Role::Admin).await?;
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
    if role == Role::Owner && caller_role != Role::Owner {
        return Err(AutumnError::forbidden_msg(
            "Only an owner can invite someone as owner",
        ));
    }

    let org_id: i64 = tenant_id.parse().map_err(|_| {
        AutumnError::internal_server_error_msg("Corrupt organization id in session")
    })?;
    let Some(organization) = org_repo.find_by_id(org_id).await? else {
        return Err(AutumnError::not_found_msg("Organization not found"));
    };

    let raw_token = generate_raw_token();
    let token_hash = hash_api_token(&raw_token);
    let insert_email = email.clone();
    let insert_role = role.as_str().to_owned();
    db.tx(move |conn| {
        async move {
            // Revalidate the inviter's own membership inside the
            // transaction, instead of trusting the `require_role` result
            // computed before it — another request could have demoted or
            // removed the inviter while this one was still building the
            // invitation (Codex review finding).
            let inviter_membership: Option<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .filter(memberships::user_id.eq(inviter_id))
                .select(Membership::as_select())
                .for_update()
                .first(conn)
                .await
                .optional()?;
            let Some(inviter_membership) = inviter_membership else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(current_caller_role) = Role::parse(&inviter_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !current_caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }
            if role == Role::Owner && current_caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can invite someone as owner",
                ));
            }

            // Revoke any other still-pending invitation to this email in
            // this organization before creating the new one — otherwise
            // both tokens would stay independently valid, and accepting
            // one would leave the other pending indefinitely (the same
            // lingering-token issue `accept_invitation` guards against for
            // an already-a-member accept, but here at creation time for
            // two never-yet-accepted invitations to the same address).
            diesel::update(
                invitations::table
                    .filter(invitations::tenant_id.eq(&tenant_id))
                    .filter(invitations::email.eq(&insert_email))
                    .filter(invitations::status.eq("pending")),
            )
            .set(invitations::status.eq("revoked"))
            .execute(conn)
            .await?;

            diesel::insert_into(invitations::table)
                .values(&InsertInvitation {
                    tenant_id,
                    email: insert_email,
                    role: insert_role,
                    token_hash,
                    status: "pending".to_owned(),
                    invited_by_user_id: inviter_id,
                    expires_at: chrono::Utc::now().naive_utc()
                        + chrono::Duration::days(INVITATION_TTL_DAYS),
                })
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
    .map_err(|err| {
        // Two concurrent requests for the same (tenant_id, email) can both
        // pass the revoke step (a no-op when no prior pending row exists)
        // and then race to insert here — the transaction alone doesn't
        // serialize them, since there's no existing row for either to lock.
        // `idx_invitations_pending_email` (a partial unique index on
        // `(tenant_id, email) WHERE status = 'pending'`, see the migration)
        // is the backstop: the loser's INSERT fails closed instead of
        // leaving two live pending tokens for the same invitee.
        if autumn_web::error::unique_violation_field(
            &err,
            &[(
                "idx_invitations_pending_email",
                "email",
                "An invitation to this email is already pending",
            )],
        )
        .is_some()
        {
            AutumnError::conflict_msg(
                "An invitation to this email is already pending for this organization",
            )
        } else {
            err
        }
    })?;

    let accept_url = format!("{}/invite/{raw_token}", app_base_url());
    // Sent synchronously (not `deliver_later_invite`): the success metric is
    // "an invite email is delivered to the dev mailbox within the same
    // request cycle" (issue #1261), which a fire-and-forget background send
    // can't guarantee.
    InvitationMailer
        .send_invite(
            &mailer,
            email,
            organization.name,
            role.as_str().to_owned(),
            accept_url,
        )
        .await?;

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

/// Who is accepting the invitation: an already-authenticated user, or the
/// not-yet-created account for case (a), whose insert is deferred into the
/// membership transaction (see `accept_invitation`).
enum Joiner {
    Existing(i64),
    New {
        email: String,
        /// The plaintext password, kept alongside `password_hash` so a
        /// concurrently-created account (see `accept_invitation`) can be
        /// verified against the *submitted* password rather than blindly
        /// adopted.
        password: String,
        password_hash: String,
    },
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

#[get("/invite/{token}")]
pub async fn show_invitation(
    session: Session,
    mut db: Db,
    invitation_repo: PgInvitationRepository,
    org_repo: PgOrganizationRepository,
    csrf: Option<CsrfToken>,
    Path(raw_token): Path<String>,
) -> AutumnResult<Response> {
    let Some(invitation) = load_pending_invitation(&invitation_repo, &raw_token).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page(csrf_value(&csrf), "This invitation link is invalid."),
        )
            .into_response());
    };
    if let Some(message) = invitation_status_error(&invitation) {
        return Ok((
            StatusCode::GONE,
            invitation_error_page(csrf_value(&csrf), message),
        )
            .into_response());
    }
    let Some(organization) = org_repo
        .find_by_id(parse_tenant_id(&invitation.tenant_id)?)
        .await?
    else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page(
                csrf_value(&csrf),
                "This invitation's organization no longer exists.",
            ),
        )
            .into_response());
    };

    let signed_in = session.get("user_id").await.is_some();

    let page = if signed_in {
        layout(
            "Accept invitation",
            true,
            csrf_value(&csrf),
            html! {
                div class="bg-white rounded-lg shadow p-6 max-w-md" {
                    h1 class="text-xl font-bold mb-2" { "Join " (organization.name) }
                    p class="text-gray-600 mb-4" { "You've been invited as " (invitation.role) "." }
                    form action={"/invite/" (raw_token) "/accept"} method="post" {
                        input type="hidden" name="_csrf" value=(csrf_value(&csrf));
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
                csrf_value(&csrf),
                html! {
                    div class="bg-white rounded-lg shadow p-6 max-w-md" {
                        h1 class="text-xl font-bold mb-2" { "Join " (organization.name) }
                        p class="text-gray-600 mb-4" {
                            "You've been invited as " (invitation.role) ". Log in as "
                            strong { (invitation.email) } " to accept."
                        }
                        a href={"/login?next=/invite/" (raw_token)}
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
                csrf_value(&csrf),
                html! {
                    div class="bg-white rounded-lg shadow p-6 max-w-md" {
                        h1 class="text-xl font-bold mb-2" { "Join " (organization.name) }
                        p class="text-gray-600 mb-4" {
                            "You've been invited as " (invitation.role)
                            ". Create an account to accept."
                        }
                        form action={"/invite/" (raw_token) "/accept"} method="post" class="space-y-4" {
                            input type="hidden" name="_csrf" value=(csrf_value(&csrf));
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

#[post("/invite/{token}/accept")]
pub async fn accept_invitation(
    session: Session,
    mut db: Db,
    invitation_repo: PgInvitationRepository,
    State(state): State<AppState>,
    csrf: Option<CsrfToken>,
    Path(raw_token): Path<String>,
    Form(form): Form<AcceptForm>,
) -> AutumnResult<Response> {
    let Some(invitation) = load_pending_invitation(&invitation_repo, &raw_token).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            invitation_error_page(csrf_value(&csrf), "This invitation link is invalid."),
        )
            .into_response());
    };

    let existing_session_user: Option<i64> =
        session.get("user_id").await.and_then(|s| s.parse().ok());

    // Resolve *who* is joining, but don't create anything yet: case (a)'s
    // account creation happens inside the transaction below (alongside the
    // membership insert), so a failure partway through the accept can never
    // leave an orphaned account with no membership.
    //
    // Security-critical: this must never silently authenticate the *caller*
    // as an existing account. A visitor with no session is only ever allowed
    // to join by proving they own the invited email — either by creating a
    // fresh account with a password (case a), or by actually logging in
    // first (redirected below) — never by the mere possession of the token.
    // An already-authenticated caller may only redeem an invite addressed to
    // their own email; otherwise anyone who obtains a token not meant for
    // them (a forwarded link, a mail-scanner fetch, a shared clipboard)
    // could join — or silently log in as — an account that isn't theirs.
    let joiner = if let Some(uid) = existing_session_user {
        let caller: User = users::table
            .filter(users::id.eq(uid))
            .select(User::as_select())
            .first(&mut *db)
            .await?;
        if normalize_email(&caller.email) != normalize_email(&invitation.email) {
            return Err(AutumnError::forbidden_msg(
                "This invitation was sent to a different email address",
            ));
        }
        Joiner::Existing(uid)
    } else {
        let email = normalize_email(&invitation.email);
        let existing_user: Option<User> = users::table
            .filter(users::email.eq(&email))
            .select(User::as_select())
            .first(&mut *db)
            .await
            .optional()?;
        match existing_user {
            Some(_) => {
                // An account already exists for this email but the visitor
                // isn't authenticated as it — never adopt it on their behalf.
                // The `show_invitation` GET page already points them at this
                // same login-first flow; this POST must enforce it too.
                return Err(AutumnError::unauthorized_msg(
                    "An account already exists for this email — log in to accept",
                ));
            }
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
                // Read through the shared `Arc`: `config()` would deep-clone
                // every config section to reach `[auth.password]`.
                let config = state.config_arc();
                let policy = config.auth.password.policy();
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
                Joiner::New {
                    email,
                    password: password.to_owned(),
                    password_hash,
                }
            }
        }
    };

    let invitation_id = invitation.id;
    let tenant_id = invitation.tenant_id.clone();
    let role = invitation.role.clone();

    // Raw queries (not the generated `tenant_scoped` repository) inside one
    // transaction: the repository's CRUD methods each acquire their own
    // pooled connection, which can't share this transaction's connection —
    // and atomicity across the account creation + row-lock + insert + status
    // update is exactly what makes double-click acceptance idempotent
    // instead of racy, and what stops a mid-flight failure from leaving a
    // freshly-created account with no membership.
    let (membership, target_user_id): (Membership, i64) = db
        .tx(move |conn| {
            async move {
                // Lock the invitation row FIRST — before touching `users` at
                // all — so two concurrent accepts of the same signup-and-join
                // form serialize on this lock rather than racing each other
                // to the `users.email` unique constraint. Locking after the
                // account insert (the prior order) meant the loser blocked on
                // that constraint instead, then failed with a raw 422 even
                // though the winner's request was about to complete the very
                // same accept for it (Codex review finding).
                let invitation: Invitation = invitations::table
                    .filter(invitations::id.eq(invitation_id))
                    .for_update()
                    .select(Invitation::as_select())
                    .first(conn)
                    .await?;

                let target_user_id = match joiner {
                    Joiner::Existing(uid) => uid,
                    Joiner::New {
                        email,
                        password,
                        password_hash,
                    } => {
                        // Now that concurrent accepts are serialized on the
                        // invitation lock above, check whether the account
                        // was already created by an earlier request for this
                        // exact accept (the pre-transaction existence check
                        // only ruled out a *pre-existing* account, not one
                        // just created by a concurrent twin of this same
                        // request) before inserting a new one.
                        let existing_user: Option<User> = users::table
                            .filter(users::email.eq(&email))
                            .select(User::as_select())
                            .first(conn)
                            .await
                            .optional()?;
                        if let Some(user) = existing_user {
                            // Security-critical: this account may have just
                            // been created a moment ago by an unrelated
                            // request racing the *same* invitation link
                            // (e.g. an attacker who obtained a forwarded or
                            // leaked token) with a *different* password.
                            // Blindly adopting it here without checking
                            // would authenticate this caller into an
                            // account whose credentials they never actually
                            // supplied — verify the submitted password
                            // against the stored hash first, exactly like
                            // `login` does, and reject rather than log in
                            // on a mismatch (Codex review finding).
                            if !verify_password(&password, &user.password_hash).await? {
                                return Err(AutumnError::unauthorized_msg(
                                    "An account already exists for this email — log in to accept",
                                ));
                            }
                            user.id
                        } else {
                            let user: User = diesel::insert_into(users::table)
                                .values(&NewUser {
                                    email,
                                    password_hash,
                                })
                                .returning(User::as_returning())
                                .get_result(conn)
                                .await
                                .map_err(|_| {
                                    AutumnError::unprocessable_msg("Could not create account")
                                })?;
                            user.id
                        }
                    }
                };

                // Reject a revoked, or expired-while-still-pending, token up
                // front — before the existing-membership shortcut below.
                // This check must NOT run for an already-`accepted` token:
                // that's the idempotent double-click case, whose
                // `expires_at` (fixed at creation) may well have passed by
                // now without that being a problem, since no new grant is
                // happening. Checked before the `existing` lookup so a
                // revoked/expired-pending invitation can never silently
                // reuse someone's unrelated existing membership to switch
                // their active organization and return success (Codex
                // review finding).
                if invitation.status == "revoked"
                    || (invitation.status == "pending"
                        && invitation.expires_at <= chrono::Utc::now().naive_utc())
                {
                    return Err(AutumnError::gone_msg(
                        "This invitation is no longer available",
                    ));
                }

                let existing: Option<Membership> = memberships::table
                    .filter(memberships::tenant_id.eq(&tenant_id))
                    .filter(memberships::user_id.eq(target_user_id))
                    .select(Membership::as_select())
                    .first(conn)
                    .await
                    .optional()?;
                if let Some(existing) = existing {
                    // The user already belongs to this organization — either
                    // this exact token was already redeemed (idempotent
                    // double-click: `invitation.status` is already
                    // "accepted"), or they joined some other way while this
                    // invitation sat pending (and, per the check above, not
                    // expired). Either way there's no new membership to
                    // insert, but a still-pending invitation must be
                    // consumed here too — otherwise its token stays valid
                    // indefinitely and, if this membership is later removed,
                    // redeeming the lingering token again would silently
                    // regrant it.
                    if invitation.status == "pending" {
                        diesel::update(
                            invitations::table.filter(invitations::id.eq(invitation_id)),
                        )
                        .set(invitations::status.eq("accepted"))
                        .execute(conn)
                        .await?;
                    }
                    return Ok::<_, AutumnError>((existing, target_user_id));
                }

                if invitation.status != "pending" {
                    // Already "accepted" (checked for "revoked"/expired
                    // above) but no existing membership: the token was
                    // consumed by an earlier accept whose membership has
                    // since been removed. Nothing left to (re)grant.
                    return Err(AutumnError::gone_msg(
                        "This invitation is no longer available",
                    ));
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

                Ok((membership, target_user_id))
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
    mut db: Db,
    invitation_repo: PgInvitationRepository,
    membership_repo: PgMembershipRepository,
    Path(invitation_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(caller_id) = session
        .get("user_id")
        .await
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };
    // `find_by_id` is tenant-scoped, so this also proves `invitation_id`
    // belongs to the caller's active organization before anything is written.
    let Some(old) = invitation_repo.find_by_id(invitation_id).await? else {
        return Err(AutumnError::not_found_msg("Invitation not found"));
    };
    let tenant_id = old.tenant_id.clone();

    db.tx(move |conn| {
        async move {
            // Revalidate the caller's own membership inside the
            // transaction, instead of trusting the pre-transaction
            // `require_role` result — see `create_invitation`'s identical
            // fix for the full rationale (Codex review finding).
            let caller_membership: Option<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .filter(memberships::user_id.eq(caller_id))
                .select(Membership::as_select())
                .for_update()
                .first(conn)
                .await
                .optional()?;
            let Some(caller_membership) = caller_membership else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(current_caller_role) = Role::parse(&caller_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !current_caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }

            // Lock and re-check status inside the transaction: a stale
            // revoke form submitted after the invitee already accepted — or
            // an accept that commits while this transaction waits for the
            // row lock above — must not stomp the terminal `accepted` row
            // back to `revoked`. That would leave the granted membership in
            // place while a later revisit of the (already-consumed) link
            // reports "revoked" instead of following the accepted-token
            // idempotency path, corrupting the invitation's audit trail
            // (Codex review finding).
            let current: Invitation = invitations::table
                .filter(invitations::id.eq(invitation_id))
                .for_update()
                .select(Invitation::as_select())
                .first(conn)
                .await?;
            if current.status != "pending" {
                return Err(AutumnError::conflict_msg(
                    "This invitation is no longer pending",
                ));
            }

            diesel::update(invitations::table.filter(invitations::id.eq(invitation_id)))
                .set(invitations::status.eq("revoked"))
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Redirect::to("/members").into_response())
}

/// Re-send a pending invitation: revoke the old token and mint a fresh one
/// with a new expiry (issue #1261 AC6 — "new token, old token invalidated").
/// The revoke-then-create pair runs in one transaction so a failure between
/// the two steps can never leave the organization with neither a valid old
/// nor a new invitation.
#[post("/invitations/{id}/resend")]
pub async fn resend_invitation(
    session: Session,
    mut db: Db,
    org_repo: PgOrganizationRepository,
    invitation_repo: PgInvitationRepository,
    membership_repo: PgMembershipRepository,
    mailer: Mailer,
    Path(invitation_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(inviter_id) = session.get("user_id").await.and_then(|s| s.parse().ok()) else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };
    // `find_by_id` is tenant-scoped, so this also proves `invitation_id`
    // belongs to the caller's active organization before anything is written.
    let Some(old) = invitation_repo.find_by_id(invitation_id).await? else {
        return Err(AutumnError::not_found_msg("Invitation not found"));
    };

    let tenant_id = old.tenant_id.clone();
    let email = old.email.clone();
    let role = old.role.clone();
    let raw_token = generate_raw_token();
    let token_hash = hash_api_token(&raw_token);
    let new: Invitation = db
        .tx(move |conn| {
            async move {
                // Revalidate the caller's own membership inside the
                // transaction — see `create_invitation`'s identical fix for
                // the full rationale (Codex review finding).
                let inviter_membership: Option<Membership> = memberships::table
                    .filter(memberships::tenant_id.eq(&tenant_id))
                    .filter(memberships::user_id.eq(inviter_id))
                    .select(Membership::as_select())
                    .for_update()
                    .first(conn)
                    .await
                    .optional()?;
                let Some(inviter_membership) = inviter_membership else {
                    return Err(AutumnError::unauthorized_msg("no active organization"));
                };
                let Some(current_caller_role) = Role::parse(&inviter_membership.role) else {
                    return Err(AutumnError::forbidden_msg("insufficient permissions"));
                };
                if !current_caller_role.at_least(Role::Admin) {
                    return Err(AutumnError::forbidden_msg("insufficient permissions"));
                }

                // Lock and re-check status inside the transaction: a
                // double-clicked resend (or two concurrent ones) must not
                // both pass a stale "it's pending" read from before the
                // transaction and each mint their own replacement, which
                // would leave two live pending invitations instead of the
                // single refreshed one this endpoint promises.
                let current: Invitation = invitations::table
                    .filter(invitations::id.eq(invitation_id))
                    .for_update()
                    .select(Invitation::as_select())
                    .first(conn)
                    .await?;
                if current.status != "pending" {
                    return Err(AutumnError::conflict_msg(
                        "This invitation is no longer pending",
                    ));
                }
                // Same rule `create_invitation` enforces for a fresh Owner
                // invitation: resending must not become a back door for an
                // Admin to indefinitely renew an Owner grant they could
                // never have created themselves (Codex review finding).
                if current.role == Role::Owner.as_str() && current_caller_role != Role::Owner {
                    return Err(AutumnError::forbidden_msg(
                        "Only an owner can resend an invitation to join as owner",
                    ));
                }

                diesel::update(invitations::table.filter(invitations::id.eq(invitation_id)))
                    .set(invitations::status.eq("revoked"))
                    .execute(conn)
                    .await?;

                let new: Invitation = diesel::insert_into(invitations::table)
                    .values(&InsertInvitation {
                        tenant_id,
                        email,
                        role,
                        token_hash,
                        status: "pending".to_owned(),
                        invited_by_user_id: inviter_id,
                        expires_at: chrono::Utc::now().naive_utc()
                            + chrono::Duration::days(INVITATION_TTL_DAYS),
                    })
                    .returning(Invitation::as_returning())
                    .get_result(conn)
                    .await?;
                Ok::<_, AutumnError>(new)
            }
            .scope_boxed()
        })
        .await?;

    let Some(organization) = org_repo
        .find_by_id(parse_tenant_id(&new.tenant_id)?)
        .await?
    else {
        return Err(AutumnError::not_found_msg("Organization not found"));
    };
    let accept_url = format!("{}/invite/{raw_token}", app_base_url());
    InvitationMailer
        .send_invite(&mailer, new.email, organization.name, new.role, accept_url)
        .await?;

    Ok(Redirect::to("/members").into_response())
}
