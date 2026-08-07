//! Member-management surface: list members + pending invitations, invite,
//! change role, revoke invite, remove member (issue #1261 AC7).
//!
//! Every mutating action is gated `require_role(&session, Role::Admin)`, but
//! an `Admin` may only change or remove `Member`/`Admin` memberships — never
//! an `Owner`'s, no matter how many owners the organization has (`Admin`+
//! can still invite/remove/change non-Owner roles regardless). Without that,
//! `Owner > Admin` wouldn't hold: an Admin could demote or eject any owner
//! as long as a second one existed. Only an `Owner` may touch another
//! `Owner`'s membership, and even an `Owner` can neither demote nor remove
//! the organization's *last* `Owner` — otherwise no one could ever grant
//! `Owner` again. Granting the `Owner` role itself additionally requires the
//! *caller* to already be an `Owner` — an `Admin` gate alone would let any
//! Admin promote themselves (or an ally) to `Owner` and from there demote/
//! remove the organization's real owners.
//!
//! `change_role`/`remove_member` read every membership row in the active
//! organization with `SELECT ... FOR UPDATE` and perform the last-owner
//! count check and the mutation inside that same locked transaction
//! (mirroring `routes::invitations::accept_invitation`'s row-locked
//! pattern) — without that, two concurrent requests demoting/removing each
//! of exactly two remaining owners could both read the same two-owner
//! snapshot, both pass the check, and leave the organization with none
//! (Codex review finding; previously a documented gap here, on the mistaken
//! assumption it could only cost the ability to grant a *new* Owner rather
//! than leave zero).

use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::models::{ChangeRoleForm, Invitation, Membership};
use crate::repositories::{
    InvitationRepository, MembershipRepository, PgInvitationRepository, PgMembershipRepository,
};
use crate::role::{Role, require_role};
use crate::schema::{memberships, users};

use super::layout::{csrf_value, layout};

/// Count how many `owner` members remain in the active organization. Used to
/// block demoting/removing the last one.
fn owner_count(memberships: &[Membership]) -> usize {
    memberships
        .iter()
        .filter(|m| m.role == Role::Owner.as_str())
        .count()
}

#[get("/members")]
pub async fn list_members(
    mut db: Db,
    session: Session,
    membership_repo: PgMembershipRepository,
    invitation_repo: PgInvitationRepository,
    csrf: Option<CsrfToken>,
) -> AutumnResult<Response> {
    // Any member (not just Admin+) can view the roster; only Admin+ sees the
    // management controls below.
    let caller_role = require_role(&session, &membership_repo, Role::Member).await?;

    let memberships = membership_repo.find_all().await?;
    let pending_invitations: Vec<Invitation> = invitation_repo
        .find_all()
        .await?
        .into_iter()
        .filter(|i| i.status == "pending")
        .collect();

    // One extra query to label each membership row with the member's email;
    // `Membership` deliberately doesn't join `users` at the model layer so a
    // repository read never has to reach across a schema boundary.
    let user_ids: Vec<i64> = memberships.iter().map(|m| m.user_id).collect();
    let emails = load_emails(&mut db, &user_ids).await?;

    let can_manage = caller_role.at_least(Role::Admin);
    let owners = owner_count(&memberships);

    let page = layout(
        "Members",
        true,
        csrf_value(&csrf),
        html! {
            h1 class="text-2xl font-bold mb-6" { "Members" }

            @if can_manage {
                form action="/invitations" method="post"
                     class="flex gap-2 mb-6 bg-white rounded-lg shadow p-4" {
                    input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                    input name="email" type="email" required placeholder="teammate@example.com"
                          aria-label="Email to invite" class="flex-1 border rounded px-3 py-2";
                    select name="role" aria-label="Role" class="border rounded px-3 py-2" {
                        option value="member" { "Member" }
                        option value="admin" { "Admin" }
                        option value="owner" { "Owner" }
                    }
                    button type="submit"
                           class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                        "Invite"
                    }
                }
            }

            ul class="space-y-2 mb-8" {
                @for membership in &memberships {
                    li class="bg-white rounded-lg shadow p-4 flex items-center justify-between" {
                        div {
                            span class="font-medium" {
                                (emails.get(&membership.user_id).map(String::as_str).unwrap_or("(unknown)"))
                            }
                            span class="ml-2 text-xs uppercase text-gray-400" { (membership.role) }
                        }
                        @if can_manage {
                            div class="flex items-center gap-2" {
                                // Locked when the target is an Owner and either
                                // the caller isn't one too (only an Owner may
                                // touch another Owner's membership) or it's the
                                // organization's last Owner (never demotable/
                                // removable by anyone) — mirrors the server-side
                                // checks in `change_role`/`remove_member`.
                                @let locked = membership.role == Role::Owner.as_str()
                                    && (caller_role != Role::Owner || owners <= 1);
                                form action={"/members/" (membership.id) "/role"} method="post" class="flex items-center gap-1" {
                                    input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                    select name="role" aria-label="Change role" disabled[locked] {
                                        option value="member" selected[membership.role == "member"] { "Member" }
                                        option value="admin" selected[membership.role == "admin"] { "Admin" }
                                        option value="owner" selected[membership.role == "owner"] { "Owner" }
                                    }
                                    button type="submit" disabled[locked]
                                           class="text-xs px-2 py-1 border rounded hover:bg-gray-50" { "Update" }
                                }
                                form action={"/members/" (membership.id) "/remove"} method="post" {
                                    input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                    button type="submit" disabled[locked]
                                           class="text-xs px-2 py-1 border rounded text-red-600 hover:bg-red-50 \
                                                  disabled:text-gray-300 disabled:hover:bg-transparent" {
                                        "Remove"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            @if can_manage {
                h2 class="text-lg font-bold mb-3" { "Pending invitations" }
                ul class="space-y-2" {
                    @for invitation in &pending_invitations {
                        li class="bg-white rounded-lg shadow p-4 flex items-center justify-between" {
                            div {
                                span class="font-medium" { (invitation.email) }
                                span class="ml-2 text-xs uppercase text-gray-400" { (invitation.role) }
                            }
                            div class="flex items-center gap-2" {
                                form action={"/invitations/" (invitation.id) "/resend"} method="post" {
                                    input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                    button type="submit" class="text-xs px-2 py-1 border rounded hover:bg-gray-50" {
                                        "Resend"
                                    }
                                }
                                form action={"/invitations/" (invitation.id) "/revoke"} method="post" {
                                    input type="hidden" name="_csrf" value=(csrf_value(&csrf));
                                    button type="submit"
                                           class="text-xs px-2 py-1 border rounded text-red-600 hover:bg-red-50" {
                                        "Revoke"
                                    }
                                }
                            }
                        }
                    }
                    @if pending_invitations.is_empty() {
                        li class="text-gray-400 text-center py-4" { "No pending invitations." }
                    }
                }
            }
        },
    );
    Ok(page.into_response())
}

async fn load_emails(
    db: &mut Db,
    user_ids: &[i64],
) -> AutumnResult<std::collections::HashMap<i64, String>> {
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;

    if user_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<(i64, String)> = users::table
        .filter(users::id.eq_any(user_ids))
        .select((users::id, users::email))
        .load(&mut **db)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Change a member's role within the active organization. Gated `Admin` or
/// higher — but an `Admin` may only change a `Member`'s or `Admin`'s role,
/// never an existing `Owner`'s (granting `Owner`, same as touching one,
/// requires the caller to already be an `Owner`); refuses to demote the
/// last `Owner`.
#[post("/members/{id}/role")]
pub async fn change_role(
    session: Session,
    mut db: Db,
    Tenant(tenant_id): Tenant,
    membership_repo: PgMembershipRepository,
    Path(membership_id): Path<i64>,
    Form(form): Form<ChangeRoleForm>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(caller_id) = session
        .get("user_id")
        .await
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };
    let Some(new_role) = Role::parse(&form.role) else {
        return Err(AutumnError::unprocessable_msg("Unknown role"));
    };

    // Raw query (not the `tenant_scoped` repository's plain `find_all()`)
    // inside one locked transaction: reading the owner count and demoting
    // the target must be atomic, or two concurrent requests each demoting a
    // *different* one of exactly two remaining owners could both read the
    // same two-owner snapshot, both pass the last-owner check below, and
    // leave the organization with none (Codex review finding — this was
    // previously a documented, accepted gap, but it can leave zero owners
    // outright, not merely block granting a *new* one).
    db.tx(move |conn| {
        async move {
            let memberships: Vec<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .for_update()
                .select(Membership::as_select())
                .load(conn)
                .await?;

            // Revalidate the caller's own role from this same locked
            // snapshot instead of the `require_role` result computed
            // before the transaction — another request could have
            // demoted or removed the caller while this one waited to
            // acquire the lock above (Codex review finding).
            let Some(caller_membership) = memberships.iter().find(|m| m.user_id == caller_id)
            else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(caller_role) = Role::parse(&caller_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }
            if new_role == Role::Owner && caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can grant the owner role",
                ));
            }

            let Some(target) = memberships.iter().find(|m| m.id == membership_id) else {
                return Err(AutumnError::not_found_msg("Member not found"));
            };
            // The `Owner > Admin` hierarchy must hold regardless of how many
            // owners exist: an Admin changing an Owner's role — even with
            // other owners still in place — is a demotion of a higher role
            // by a lower one.
            if target.role == Role::Owner.as_str() && caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can change another owner's role",
                ));
            }
            if target.role == Role::Owner.as_str()
                && new_role != Role::Owner
                && owner_count(&memberships) <= 1
            {
                return Err(AutumnError::conflict_msg(
                    "Cannot demote the last owner of an organization",
                ));
            }

            diesel::update(memberships::table.filter(memberships::id.eq(membership_id)))
                .set(memberships::role.eq(new_role.as_str()))
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Redirect::to("/members").into_response())
}

/// Remove a member from the active organization. Gated `Admin` or higher —
/// but an `Admin` may never remove an `Owner` (regardless of how many
/// owners exist); refuses to remove the last `Owner` even for a caller who
/// is themselves an `Owner`.
#[post("/members/{id}/remove")]
pub async fn remove_member(
    session: Session,
    mut db: Db,
    Tenant(tenant_id): Tenant,
    membership_repo: PgMembershipRepository,
    Path(membership_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, &membership_repo, Role::Admin).await?;
    let Some(caller_id) = session
        .get("user_id")
        .await
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Err(AutumnError::unauthorized_msg("authentication required"));
    };

    // Same race as `change_role` (see its comment): the owner-count check
    // and the removal must run inside one locked transaction, or two
    // concurrent requests removing two different owners could both pass
    // the last-owner check and leave the organization with none (Codex
    // review finding).
    db.tx(move |conn| {
        async move {
            let memberships: Vec<Membership> = memberships::table
                .filter(memberships::tenant_id.eq(&tenant_id))
                .for_update()
                .select(Membership::as_select())
                .load(conn)
                .await?;

            // Revalidate the caller's own role from this same locked
            // snapshot instead of the `require_role` result computed
            // before the transaction (Codex review finding — see
            // `change_role`'s identical fix for the full rationale).
            let Some(caller_membership) = memberships.iter().find(|m| m.user_id == caller_id)
            else {
                return Err(AutumnError::unauthorized_msg("no active organization"));
            };
            let Some(caller_role) = Role::parse(&caller_membership.role) else {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            };
            if !caller_role.at_least(Role::Admin) {
                return Err(AutumnError::forbidden_msg("insufficient permissions"));
            }

            let Some(target) = memberships.iter().find(|m| m.id == membership_id) else {
                return Err(AutumnError::not_found_msg("Member not found"));
            };
            if target.role == Role::Owner.as_str() && caller_role != Role::Owner {
                return Err(AutumnError::forbidden_msg(
                    "Only an owner can remove another owner",
                ));
            }
            if target.role == Role::Owner.as_str() && owner_count(&memberships) <= 1 {
                return Err(AutumnError::conflict_msg(
                    "Cannot remove the last owner of an organization",
                ));
            }

            diesel::delete(memberships::table.filter(memberships::id.eq(membership_id)))
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Redirect::to("/members").into_response())
}
