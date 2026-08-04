//! Member-management surface: list members + pending invitations, invite,
//! change role, revoke invite, remove member (issue #1261 AC7).
//!
//! Every mutating action is gated `require_role(&session, Role::Admin)`, and
//! the last `Owner` in an organization can neither be demoted nor removed —
//! otherwise an organization could be left with no one able to manage it.

use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;

use crate::models::{ChangeRoleForm, Invitation, Membership};
use crate::repositories::{
    InvitationRepository, MembershipRepository, PgInvitationRepository, PgMembershipRepository,
};
use crate::role::{Role, require_role};
use crate::schema::users;

use super::layout::layout;

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
) -> AutumnResult<Response> {
    // Any member (not just Admin+) can view the roster; only Admin+ sees the
    // management controls below.
    let caller_role = require_role(&session, Role::Member).await?;

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
        html! {
            h1 class="text-2xl font-bold mb-6" { "Members" }

            @if can_manage {
                form action="/invitations" method="post"
                     class="flex gap-2 mb-6 bg-white rounded-lg shadow p-4" {
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
                                @let last_owner = membership.role == Role::Owner.as_str() && owners <= 1;
                                form action={"/members/" (membership.id) "/role"} method="post" class="flex items-center gap-1" {
                                    select name="role" aria-label="Change role" disabled[last_owner] {
                                        option value="member" selected[membership.role == "member"] { "Member" }
                                        option value="admin" selected[membership.role == "admin"] { "Admin" }
                                        option value="owner" selected[membership.role == "owner"] { "Owner" }
                                    }
                                    button type="submit" disabled[last_owner]
                                           class="text-xs px-2 py-1 border rounded hover:bg-gray-50" { "Update" }
                                }
                                form action={"/members/" (membership.id) "/remove"} method="post" {
                                    button type="submit" disabled[last_owner]
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
                                    button type="submit" class="text-xs px-2 py-1 border rounded hover:bg-gray-50" {
                                        "Resend"
                                    }
                                }
                                form action={"/invitations/" (invitation.id) "/revoke"} method="post" {
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
/// higher; refuses to demote the last `Owner`.
#[post("/members/{id}/role")]
pub async fn change_role(
    session: Session,
    membership_repo: PgMembershipRepository,
    Path(membership_id): Path<i64>,
    Form(form): Form<ChangeRoleForm>,
) -> AutumnResult<Response> {
    require_role(&session, Role::Admin).await?;
    let Some(new_role) = Role::parse(&form.role) else {
        return Err(AutumnError::unprocessable_msg("Unknown role"));
    };

    let memberships = membership_repo.find_all().await?;
    let Some(target) = memberships.iter().find(|m| m.id == membership_id) else {
        return Err(AutumnError::not_found_msg("Member not found"));
    };
    if target.role == Role::Owner.as_str()
        && new_role != Role::Owner
        && owner_count(&memberships) <= 1
    {
        return Err(AutumnError::conflict_msg(
            "Cannot demote the last owner of an organization",
        ));
    }

    membership_repo
        .update(
            membership_id,
            &crate::models::UpdateMembership {
                role: Patch::Set(new_role.as_str().to_owned()),
                ..Default::default()
            },
        )
        .await?;
    Ok(Redirect::to("/members").into_response())
}

/// Remove a member from the active organization. Gated `Admin` or higher;
/// refuses to remove the last `Owner`.
#[post("/members/{id}/remove")]
pub async fn remove_member(
    session: Session,
    membership_repo: PgMembershipRepository,
    Path(membership_id): Path<i64>,
) -> AutumnResult<Response> {
    require_role(&session, Role::Admin).await?;

    let memberships = membership_repo.find_all().await?;
    let Some(target) = memberships.iter().find(|m| m.id == membership_id) else {
        return Err(AutumnError::not_found_msg("Member not found"));
    };
    if target.role == Role::Owner.as_str() && owner_count(&memberships) <= 1 {
        return Err(AutumnError::conflict_msg(
            "Cannot remove the last owner of an organization",
        ));
    }

    membership_repo.delete_by_id(membership_id).await?;
    Ok(Redirect::to("/members").into_response())
}
