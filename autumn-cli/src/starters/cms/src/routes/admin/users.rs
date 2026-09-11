//! The Users screen — accounts and role assignment.

use autumn_web::AutumnResult;
use autumn_web::auth::{hash_password, validate_password};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::{Capability, Role};
use crate::content;
use crate::models::{NewUser, User};
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::{layout, role_options};

/// How many accounts one page of the users screen shows.
const USERS_PER_PAGE: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct UsersFilter {
    #[serde(default)]
    pub page: Option<usize>,
}

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

/// The "Add user" card's field values, carried through a failed submission so
/// the administrator does not have to retype them. The password is
/// deliberately not part of this tuple — same convention as `/register`'s
/// `redisplay`, which never echoes a submitted password back into a form.
type AddUserValues<'a> = (&'a str, &'a str, Role);

/// The "Add user" card. `error`, when present, renders adjacent to the fields
/// it applies to and is announced via `role="alert"` — the same pattern
/// `/register`'s `register_form_markup` already uses for this example.
fn add_user_form_markup(csrf: &Csrf, values: AddUserValues<'_>, error: Option<&str>) -> Markup {
    let (username, email, role) = values;
    html! {
        form action="/admin/users" method="post"
             class="bg-white rounded-lg shadow p-5 space-y-3 h-fit" {
                 (csrf.input())
            h2 class="font-semibold text-sm" { "Add user" }
            @if let Some(error) = error {
                p class="text-sm text-red-700 whitespace-pre-line" role="alert" { (error) }
            }
            div {
                label for="new-username" class="block text-sm font-medium mb-1" {
                    "Username"
                }
                input #new-username type="text" name="username" value=(username) required
                      maxlength="60" class="w-full border rounded px-3 py-2";
            }
            div {
                label for="new-email" class="block text-sm font-medium mb-1" { "Email" }
                input #new-email type="email" name="email" value=(email) required
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
                    (role_options(role))
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

/// Renders the whole Users screen — the account table, its pagination, and
/// the "Add user" card — parameterized by what the card should show. `list`
/// calls this with a blank card; `create` calls it again, with the
/// administrator's own submission and failure message, whenever that
/// submission cannot be saved. Re-querying the table on a failed submission
/// costs one extra page of accounts; the alternative is the account being
/// added ending up on a page the admin never sees, mid-correction.
async fn users_page(
    repos: &Repos,
    user: &User,
    csrf: &Csrf,
    filter: &UsersFilter,
    add_user: AddUserValues<'_>,
    add_user_error: Option<&str>,
) -> AutumnResult<Markup> {
    let may_edit = user.role().can(Capability::EditUsers);

    // Ordered and bounded in SQL. `find_all` loaded every account and this
    // screen sorted the whole set in memory and rendered one or two forms per
    // row — so on a site with open registration, the screen an administrator
    // would use to clear a signup flood was the one the flood broke first.
    let page = i64::try_from(filter.page.unwrap_or(1).clamp(1, 100_000)).unwrap_or(1);
    let (users, total) = {
        let mut conn = repos.conn().await?;
        let rows =
            content::users_page(&mut conn, (page - 1) * USERS_PER_PAGE, USERS_PER_PAGE).await?;
        let total = content::user_count(&mut conn).await?;
        (rows, total)
    };
    let last_page = ((total + USERS_PER_PAGE - 1) / USERS_PER_PAGE).max(1);

    Ok(html! {
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

                @if last_page > 1 {
                    nav aria-label="User pages"
                        class="flex items-center justify-between p-4 border-t \
                               border-gray-100 text-sm" {
                        @if page > 1 {
                            a href=(format!("/admin/users?page={}", page - 1))
                              class="text-indigo-700 hover:underline" { "← Previous" }
                        } @else {
                            span {}
                        }
                        span class="text-gray-500" { "Page " (page) " of " (last_page) }
                        @if page < last_page {
                            a href=(format!("/admin/users?page={}", page + 1))
                              class="text-indigo-700 hover:underline" { "Next →" }
                        } @else {
                            span {}
                        }
                    }
                }
            }

            @if may_edit {
                (add_user_form_markup(csrf, add_user, add_user_error))
            }
        }
    })
}

#[get("/admin/users")]
pub async fn list(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Query(filter): Query<UsersFilter>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ListUsers);
    let body = users_page(
        &repos,
        &user,
        &csrf,
        &filter,
        ("", "", Role::Subscriber),
        None,
    )
    .await?;
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
    let actor = require_capability!(repos, session, csrf, Capability::EditUsers);
    // Parse rather than trust: a hand-crafted POST could otherwise put any
    // string in the column, and an unrecognised role degrades to Subscriber —
    // silently granting *less*, never more.
    let role = Role::parse(&form.role);

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
        return redisplay_add_user(
            &repos,
            &actor,
            &csrf,
            (&form.username, &form.email, role),
            &validation.messages().join("\n"),
        )
        .await;
    }

    let draft = NewUser {
        username: form.username.trim().to_lowercase(),
        email: form.email.trim().to_lowercase(),
        password_hash: hash_password(&form.password).await?,
        display_name: if form.display_name.trim().is_empty() {
            form.username.trim().to_owned()
        } else {
            form.display_name.trim().to_owned()
        },
        role: role.slug().to_owned(),
        bio: String::new(),
        website: String::new(),
    };

    // Re-authorized against the actor's current row, under the same lock every
    // other change to the administrator set takes. The check above ran before a
    // password hash that takes hundreds of milliseconds by design, and the
    // account being created can carry any role — so an administrator demoted in
    // that window could otherwise still mint a fresh administrator and keep
    // privileged access through it.
    let created = repos
        .with_conn(async |conn| content::create_user_as(conn, actor.id, draft).await)
        .await;

    // A duplicate username/email (a `UNIQUE` violation) or a `normalize_new_user`
    // rejection (bad email, empty or non-slug username — the pre-check above
    // only covers the password) both land here. Either is the administrator's
    // to fix by resubmitting, so redisplay rather than let it fall through to
    // the framework's generic error page — the same distinction `/register`
    // already draws between "fix the form" and "something else broke".
    if let Err(error) = created {
        let message =
            if error.status() == StatusCode::CONFLICT || error.to_string().contains("unique") {
                "That username or email is already taken".to_owned()
            } else if error.status() == StatusCode::UNPROCESSABLE_ENTITY {
                error.to_string()
            } else {
                return Err(error);
            };
        return redisplay_add_user(
            &repos,
            &actor,
            &csrf,
            (&form.username, &form.email, role),
            &message,
        )
        .await;
    }

    Ok(Redirect::to("/admin/users").into_response())
}

/// Redisplays the Users screen at 422 with the "Add user" card filled back in
/// and `message` shown against it, instead of discarding what the
/// administrator typed. See [`users_page`].
async fn redisplay_add_user(
    repos: &Repos,
    actor: &User,
    csrf: &Csrf,
    add_user: AddUserValues<'_>,
    message: &str,
) -> AutumnResult<Response> {
    let body = users_page(
        repos,
        actor,
        csrf,
        &UsersFilter::default(),
        add_user,
        Some(message),
    )
    .await?;
    Ok((
        StatusCode::UNPROCESSABLE_ENTITY,
        layout(actor, csrf, "/admin/users", "Users", body),
    )
        .into_response())
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
    // The last-administrator check and the write share one transaction under an
    // advisory lock — see `content::update_user`. Checking here and writing
    // afterwards lets two administrators demote each other concurrently, each
    // having seen the other still in post.
    //
    // Credentials are deliberately not reachable from this screen: a role
    // change and a password change are different operations, and conflating
    // them is how an admin screen becomes an account-takeover primitive.
    repos
        .with_conn(async |conn| {
            crate::content::update_user(
                conn,
                actor.id,
                id,
                crate::content::UserEdit {
                    role: Role::parse(&form.role),
                    email: form.email.clone(),
                    display_name: form.display_name.trim().to_owned(),
                    bio: form.bio.clone(),
                    website: form.website.trim().to_owned(),
                },
            )
            .await
        })
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
    // `posts.author_id` is `ON DELETE CASCADE`, so deleting an account removes
    // its content too — the same choice WordPress offers as "delete all
    // content". WordPress also offers "attribute to another user"; that would
    // be a reassignment step here, and is deliberately left out rather than
    // implemented halfway.
    //
    // The delete runs under the same administrator-set lock as a role change,
    // and hands back the terms its cascaded posts were filed under: the cascade
    // reaches `post_terms`, and nothing in it maintains `terms.post_count`, so
    // without the rebuild every archive those posts appeared in keeps counting
    // them forever.
    // The delete and the recounts share one transaction. Recounting afterwards
    // means a transient failure leaves the account and its posts permanently
    // gone with the counts stale — and a retry finds no user to delete, so
    // nothing ever repairs them.
    repos
        .with_conn(async |conn| crate::content::delete_user(conn, actor.id, id).await)
        .await?;

    Ok(Redirect::to("/admin/users").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline (pre-fix) reproduction: a failed `POST /admin/users` returned
    /// `Err(AutumnError::unprocessable_msg(..))` straight from the handler.
    /// `AutumnError::into_response` (autumn/src/error.rs) always answers
    /// `application/problem+json` — see its own `into_response_has_json_body`
    /// test — so that path never rendered a `<form>` and never carried
    /// forward the username, email or role the administrator had already
    /// typed. Error-path booleans, all four failing: not adjacent to its
    /// cause (a different content type, not the Users screen at all), does
    /// not persist until resolved (there is nothing left to resolve), does
    /// not say how to recover (states what was wrong, not what to do next),
    /// and does not preserve entered data. This module's fix replaces that
    /// `Err(..)` with `redisplay_add_user`, asserted by the tests below.
    #[test]
    fn baseline_unprocessable_msg_is_422_not_html() {
        let err = AutumnError::unprocessable_msg("Password is too short");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// The redisplayed "Add user" card keeps the username, email and role
    /// the administrator already picked — only the password is dropped, the
    /// same convention `/register`'s form uses.
    #[test]
    fn add_user_form_preserves_submitted_values_on_failure() {
        let csrf = Csrf::disabled();
        let markup = add_user_form_markup(
            &csrf,
            ("cross-post", "person@example.com", Role::Editor),
            Some("A username can only contain lowercase letters, numbers and hyphens"),
        )
        .into_string();

        assert!(
            markup.contains(r#"value="cross-post""#),
            "username not preserved: {markup}"
        );
        assert!(
            markup.contains(r#"value="person@example.com""#),
            "email not preserved: {markup}"
        );
        assert!(
            markup.contains(&format!(r#"value="{}" selected"#, Role::Editor.slug())),
            "the submitted role is not reselected: {markup}"
        );
        assert!(
            !markup.contains(r#"name="password" value"#),
            "the password field must never echo a submitted password back: {markup}"
        );
    }

    /// The failure message renders inside the same card the fields are in
    /// (adjacent to its cause) and is announced to assistive tech.
    #[test]
    fn add_user_form_announces_the_error_next_to_the_fields() {
        let csrf = Csrf::disabled();
        let markup = add_user_form_markup(
            &csrf,
            ("", "", Role::Subscriber),
            Some("Username is required"),
        )
        .into_string();

        assert!(
            markup.contains(r#"role="alert""#) && markup.contains("Username is required"),
            "the error is not announced next to the form: {markup}"
        );
    }

    /// No failure, no alert — the blank card `list` renders carries nothing
    /// for a screen reader to announce.
    #[test]
    fn add_user_form_has_no_alert_when_nothing_failed() {
        let csrf = Csrf::disabled();
        let markup = add_user_form_markup(&csrf, ("", "", Role::Subscriber), None).into_string();

        assert!(
            !markup.contains(r#"role="alert""#),
            "an alert rendered with no error to report: {markup}"
        );
    }
}
