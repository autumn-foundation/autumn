//! The Users screen — accounts and role assignment.

use autumn_web::AutumnResult;
use autumn_web::auth::{hash_password, validate_password};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::{Capability, Role};
use crate::models::{NewUser, UpdateUser};
use crate::repositories::UserRepository as _;
use crate::require_capability;
use autumn_web::hooks::Patch;

use super::super::site::{Csrf, Repos};
use super::{layout, role_options};

#[derive(Deserialize)]
pub struct NewUserForm {
    pub username: String,
    pub email: String,
    pub password: String,
    pub role: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Deserialize)]
pub struct UpdateUserForm {
    pub role: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub bio: String,
    #[serde(default)]
    pub website: String,
}

#[get("/admin/users")]
pub async fn list(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ListUsers);
    let may_edit = user.role().can(Capability::EditUsers);
    let mut users = repos.users.find_all().await?;
    users.sort_by(|a, b| a.username.cmp(&b.username));

    let body = html! {
        div class="grid grid-cols-1 lg:grid-cols-3 gap-6" {
            div class="lg:col-span-2 bg-white rounded-lg shadow overflow-hidden" {
                table class="w-full text-sm" {
                    caption class="sr-only" { "Users" }
                    thead class="bg-gray-50 text-left text-xs uppercase tracking-wide \
                                 text-gray-500" {
                        tr {
                            th scope="col" class="px-4 py-3" { "Username" }
                            th scope="col" class="px-4 py-3" { "Email" }
                            th scope="col" class="px-4 py-3" { "Role" }
                            th scope="col" class="px-4 py-3" { span class="sr-only" { "Actions" } }
                        }
                    }
                    tbody {
                        @for row in &users {
                            tr class="border-t border-gray-100" {
                                td class="px-4 py-3 font-medium" {
                                    (row.username)
                                    @if row.id == user.id {
                                        span class="ml-2 text-xs text-gray-400" { "(you)" }
                                    }
                                }
                                td class="px-4 py-3 text-gray-500" { (row.email) }
                                td class="px-4 py-3" {
                                    @if may_edit && row.id != user.id {
                                        form method="post"
                                             action=(format!("/admin/users/{}", row.id))
                                             class="flex gap-2 items-center" {
                                                 (csrf.input())
                                            label for=(format!("role-{}", row.id))
                                                  class="sr-only" { "Role" }
                                            select #(format!("role-{}", row.id)) name="role"
                                                   class="border rounded px-2 py-1 text-xs" {
                                                (role_options(row.role()))
                                            }
                                            input type="hidden" name="display_name"
                                                  value=(row.display_name);
                                            input type="hidden" name="email" value=(row.email);
                                            input type="hidden" name="bio" value=(row.bio);
                                            input type="hidden" name="website" value=(row.website);
                                            button type="submit"
                                                   class="text-indigo-700 hover:underline text-xs" {
                                                "Save"
                                            }
                                        }
                                    } @else {
                                        span class="text-gray-600" { (row.role().label()) }
                                    }
                                }
                                td class="px-4 py-3 text-right" {
                                    // An administrator cannot delete their own
                                    // account from this screen. Locking yourself
                                    // out of the only administrator account is
                                    // not a recoverable mistake through the UI.
                                    @if may_edit && row.id != user.id {
                                        form method="post"
                                             action=(format!("/admin/users/{}/delete", row.id)) {
                                                 (csrf.input())
                                            button type="submit"
                                                   class="text-red-700 hover:underline text-xs" {
                                                "Delete"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            @if may_edit {
                form action="/admin/users" method="post"
                     class="bg-white rounded-lg shadow p-5 space-y-3 h-fit" {
                         (csrf.input())
                    h2 class="font-semibold text-sm" { "Add user" }
                    div {
                        label for="new-username" class="block text-sm font-medium mb-1" {
                            "Username"
                        }
                        input #new-username type="text" name="username" required maxlength="60"
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="new-email" class="block text-sm font-medium mb-1" { "Email" }
                        input #new-email type="email" name="email" required
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="new-password" class="block text-sm font-medium mb-1" {
                            "Password"
                        }
                        input #new-password type="password" name="password" required
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="new-role" class="block text-sm font-medium mb-1" { "Role" }
                        select #new-role name="role" class="w-full border rounded px-3 py-2" {
                            (role_options(Role::Subscriber))
                        }
                    }
                    button type="submit"
                           class="w-full px-4 py-2 bg-indigo-600 text-white rounded \
                                  hover:bg-indigo-700" {
                        "Add user"
                    }
                }
            }
        }
    };

    Ok(layout(&user, &csrf, "/admin/users", "Users", body).into_response())
}

#[post("/admin/users")]
pub async fn create(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    State(state): State<AppState>,
    Form(form): Form<NewUserForm>,
) -> AutumnResult<Response> {
    let _actor = require_capability!(repos, session, csrf, Capability::EditUsers);

    // The configured `[auth.password]` policy, not a hard-coded one, so an
    // administrator creating an account is held to the same rules as a visitor
    // registering one.
    let validation = validate_password(
        &form.password,
        &state.config_arc().auth.password.policy(),
        &[form.username.as_str(), form.email.as_str()],
    )
    .await;
    if !validation.is_valid() {
        return Err(AutumnError::unprocessable_msg(
            validation.messages().join("\n"),
        ));
    }

    repos
        .users
        .save(&NewUser {
            username: form.username.trim().to_lowercase(),
            email: form.email.trim().to_lowercase(),
            password_hash: hash_password(&form.password).await?,
            display_name: if form.display_name.trim().is_empty() {
                form.username.trim().to_owned()
            } else {
                form.display_name.trim().to_owned()
            },
            // Parse rather than trust: a hand-crafted POST could otherwise put
            // any string in the column, and an unrecognised role degrades to
            // Subscriber — silently granting *less*, never more.
            role: Role::parse(&form.role).slug().to_owned(),
            bio: String::new(),
            website: String::new(),
        })
        .await?;

    Ok(Redirect::to("/admin/users").into_response())
}

#[post("/admin/users/{id}")]
pub async fn update(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(id): Path<i64>,
    Form(form): Form<UpdateUserForm>,
) -> AutumnResult<Response> {
    let actor = require_capability!(repos, session, csrf, Capability::EditUsers);

    // Changing your own role is how an administrator accidentally demotes the
    // only administrator. The screen hides the control; this is what enforces it.
    if actor.id == id {
        return Err(AutumnError::forbidden_msg(
            "You cannot change your own role",
        ));
    }
    guard_last_administrator(&repos, id, Role::parse(&form.role)).await?;

    repos
        .users
        .update(
            id,
            // `Patch::Unchanged` on `password_hash` is what keeps this screen
            // from being able to touch a credential at all: a role change and a
            // password change are different operations, and conflating them is
            // how an admin screen becomes an account-takeover primitive.
            &UpdateUser {
                email: Patch::Set(form.email.trim().to_lowercase()),
                display_name: Patch::Set(form.display_name.trim().to_owned()),
                role: Patch::Set(Role::parse(&form.role).slug().to_owned()),
                bio: Patch::Set(form.bio.clone()),
                website: Patch::Set(form.website.trim().to_owned()),
                password_hash: Patch::Unchanged,
                username: Patch::Unchanged,
            },
        )
        .await?;

    Ok(Redirect::to("/admin/users").into_response())
}

#[post("/admin/users/{id}/delete")]
pub async fn delete(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(id): Path<i64>,
) -> AutumnResult<Response> {
    let actor = require_capability!(repos, session, csrf, Capability::EditUsers);
    if actor.id == id {
        return Err(AutumnError::forbidden_msg(
            "You cannot delete your own account",
        ));
    }
    guard_last_administrator(&repos, id, Role::Subscriber).await?;

    // `posts.author_id` is `ON DELETE CASCADE`, so deleting an account removes
    // its content too — the same choice WordPress offers as "delete all
    // content". WordPress also offers "attribute to another user"; that would
    // be a reassignment step here, and is deliberately left out rather than
    // implemented halfway.
    repos.users.delete_by_id(id).await?;
    Ok(Redirect::to("/admin/users").into_response())
}

/// Refuse a change that would leave the site with no administrator.
///
/// Without this, demoting or deleting the last administrator locks everyone out
/// of Settings, Users and Appearance permanently — recoverable only with
/// database access.
async fn guard_last_administrator(
    repos: &Repos,
    target_id: i64,
    new_role: Role,
) -> AutumnResult<()> {
    if new_role == Role::Administrator {
        return Ok(());
    }
    let target = repos
        .users
        .find_by_id(target_id)
        .await?
        .ok_or_else(|| AutumnError::not_found_msg("No such user"))?;
    if target.role() != Role::Administrator {
        return Ok(());
    }
    let administrators = repos
        .users
        .find_by_role(Role::Administrator.slug().to_owned())
        .await?
        .len();
    if administrators <= 1 {
        return Err(AutumnError::unprocessable_msg(
            "This is the only administrator account; promote another user first",
        ));
    }
    Ok(())
}
