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
pub(crate) fn owner_count(memberships: &[Membership]) -> usize {
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
        members_content(
            can_manage,
            owners,
            caller_role,
            &memberships,
            &emails,
            &pending_invitations,
            csrf_value(&csrf),
            None,
            None,
            "",
            "member",
        ),
    );
    Ok(page.into_response())
}

/// The members list + management page body: roster, pending invitations,
/// and (for an Admin+) the "Send Invitation" form. Shared between
/// [`list_members`]'s clean GET render and
/// `routes::invitations::create_invitation`'s redisplay on a rejected
/// email/role/duplicate invitation, so the roster and pending-invitations
/// list never have to be reconstructed differently between the two call
/// sites (Wayfinder: error-path inventory — the same anti-pattern already
/// fixed on `/signup` and `/invite/{token}/accept`, but reached through this
/// form's `create_invitation` handler instead).
///
/// `email_error`/`role_error` are separate (rather than one shared
/// `invite_error`) so each rejection marks `aria-invalid`/shows its message
/// next to the field that actually caused it: an unrecognized role used to
/// attach its message to the email input while the role `<select>` quietly
/// fell back to its first option ("Member") with nothing marked invalid,
/// misidentifying the field to fix and risking a resubmission that silently
/// changes the intended role to Member (Codex review finding).
#[allow(clippy::too_many_arguments)]
pub(crate) fn members_content(
    can_manage: bool,
    owners: usize,
    caller_role: Role,
    memberships: &[Membership],
    emails: &std::collections::HashMap<i64, String>,
    pending_invitations: &[Invitation],
    csrf_token: &str,
    email_error: Option<&str>,
    role_error: Option<&str>,
    invite_email: &str,
    invite_role: &str,
) -> Markup {
    html! {
            h1 class="text-2xl font-bold mb-6" { "Members" }

            @if can_manage {
                form action="/invitations" method="post"
                     class="flex gap-2 items-start mb-6 bg-white rounded-lg shadow p-4" {
                    input type="hidden" name="_csrf" value=(csrf_token);
                    div class="flex-1" {
                        input name="email" type="email" required value=(invite_email)
                              placeholder="teammate@example.com"
                              aria-label="Email to invite"
                              aria-invalid=(if email_error.is_some() { "true" } else { "false" })
                              class="w-full border rounded px-3 py-2";
                        @if let Some(error) = email_error {
                            p class="mt-1 text-sm text-red-600" role="alert" { (error) }
                        }
                    }
                    div {
                        select name="role" aria-label="Role"
                                aria-invalid=(if role_error.is_some() { "true" } else { "false" })
                                class="border rounded px-3 py-2" {
                            option value="member" selected[invite_role == "member"] { "Member" }
                            option value="admin" selected[invite_role == "admin"] { "Admin" }
                            option value="owner" selected[invite_role == "owner"] { "Owner" }
                        }
                        @if let Some(error) = role_error {
                            p class="mt-1 text-sm text-red-600" role="alert" { (error) }
                        }
                    }
                    button type="submit"
                           class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                        "Invite"
                    }
                }
            }

            ul class="space-y-2 mb-8" {
                @for membership in memberships {
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
                                    input type="hidden" name="_csrf" value=(csrf_token);
                                    select name="role" aria-label="Change role" disabled[locked] {
                                        option value="member" selected[membership.role == "member"] { "Member" }
                                        option value="admin" selected[membership.role == "admin"] { "Admin" }
                                        option value="owner" selected[membership.role == "owner"] { "Owner" }
                                    }
                                    button type="submit" disabled[locked]
                                           class="text-xs px-2 py-1 border rounded hover:bg-gray-50" { "Update" }
                                }
                                form action={"/members/" (membership.id) "/remove"} method="post" {
                                    input type="hidden" name="_csrf" value=(csrf_token);
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
                    @for invitation in pending_invitations {
                        li class="bg-white rounded-lg shadow p-4 flex items-center justify-between" {
                            div {
                                span class="font-medium" { (invitation.email) }
                                span class="ml-2 text-xs uppercase text-gray-400" { (invitation.role) }
                            }
                            div class="flex items-center gap-2" {
                                form action={"/invitations/" (invitation.id) "/resend"} method="post" {
                                    input type="hidden" name="_csrf" value=(csrf_token);
                                    button type="submit" class="text-xs px-2 py-1 border rounded hover:bg-gray-50" {
                                        "Resend"
                                    }
                                }
                                form action={"/invitations/" (invitation.id) "/revoke"} method="post" {
                                    input type="hidden" name="_csrf" value=(csrf_token);
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
    }
}

pub(crate) async fn load_emails(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_emails() -> std::collections::HashMap<i64, String> {
        std::collections::HashMap::new()
    }

    /// Baseline (no error): the invite form is present with an empty email
    /// and no alert — this is what `list_members`'s own GET renders.
    #[test]
    fn members_content_invite_form_clean_when_no_error() {
        let html = members_content(
            true,
            1,
            Role::Owner,
            &[],
            &empty_emails(),
            &[],
            "csrf-abc",
            None,
            None,
            "",
            "member",
        )
        .into_string();
        assert!(html.contains(r#"action="/invitations""#), "{html}");
        assert!(html.contains(r#"value="csrf-abc""#), "{html}");
        assert!(html.contains(r#"value="""#), "{html}");
        assert_eq!(html.matches(r#"aria-invalid="false""#).count(), 2, "{html}");
        assert!(!html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains(r#"option value="member" selected"#), "{html}");
    }

    /// A rejected email (Wayfinder: error-path inventory) shows the message
    /// next to the email field, flags only that field `aria-invalid`, and
    /// preserves both the typed email and the selected role — the admin
    /// only has to fix the one bad field, not re-enter the whole form.
    #[test]
    fn members_content_invite_form_shows_email_error_and_preserves_input() {
        let html = members_content(
            true,
            1,
            Role::Owner,
            &[],
            &empty_emails(),
            &[],
            "csrf-abc",
            Some("Enter a valid email address"),
            None,
            "not-an-email",
            "admin",
        )
        .into_string();
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("Enter a valid email address"), "{html}");
        assert!(html.contains(r#"value="not-an-email""#), "{html}");
        assert!(html.contains(r#"option value="admin" selected"#), "{html}");
        assert!(
            !html.contains(r#"option value="member" selected"#),
            "{html}"
        );
        // Only the email field is flagged invalid — the role select isn't.
        let select_start = html.find("<select").expect("role select");
        assert!(
            html[..select_start].contains(r#"aria-invalid="true""#),
            "{html}"
        );
        assert!(
            html[select_start..].contains(r#"aria-invalid="false""#),
            "{html}"
        );
    }

    /// A rejected role (Codex review finding on this PR: an unrecognized
    /// role used to attach its error to the email field while the role
    /// `<select>` silently fell back to "Member" with nothing marked
    /// invalid) must flag the *role* field, not the email field, and leave
    /// the email untouched.
    #[test]
    fn members_content_invite_form_shows_role_error_on_role_field() {
        let html = members_content(
            true,
            1,
            Role::Owner,
            &[],
            &empty_emails(),
            &[],
            "csrf-abc",
            None,
            Some("Unknown role"),
            "newbie@acme.test",
            "super-admin",
        )
        .into_string();
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("Unknown role"), "{html}");
        assert!(html.contains(r#"value="newbie@acme.test""#), "{html}");
        // The email field must not be flagged invalid — only the role is.
        let email_input_start = html.find(r#"name="email""#).expect("email input");
        let select_start = html.find("<select").expect("role select");
        assert!(
            html[email_input_start..select_start].contains(r#"aria-invalid="false""#),
            "{html}"
        );
        assert!(
            html[select_start..].contains(r#"aria-invalid="true""#),
            "{html}"
        );
        // No option matches "super-admin"; none should render as selected.
        assert!(!html.contains("selected"), "{html}");
    }

    /// A non-Admin viewer never sees the invite form at all, error or not —
    /// `can_manage` gates it independently of the error fields.
    #[test]
    fn members_content_hides_invite_form_when_caller_cannot_manage() {
        let html = members_content(
            false,
            1,
            Role::Member,
            &[],
            &empty_emails(),
            &[],
            "csrf-abc",
            None,
            None,
            "",
            "member",
        )
        .into_string();
        assert!(!html.contains(r#"action="/invitations""#), "{html}");
    }
}
