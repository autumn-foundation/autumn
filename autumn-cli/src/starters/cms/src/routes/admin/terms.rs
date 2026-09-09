//! Categories, tags, and any custom taxonomy — one screen for all of them.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::content_types::{self, Taxonomy};
use crate::models::{NewTerm, Term, UpdateTerm};
use crate::repositories::TermRepository as _;
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::layout;

#[derive(Deserialize)]
pub struct TermForm {
    pub name: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parent_id: Option<String>,
}

fn resolve(slug: &str) -> AutumnResult<Taxonomy> {
    content_types::find_taxonomy(slug)
        .ok_or_else(|| AutumnError::not_found_msg(format!("Unknown taxonomy `{slug}`")))
}

#[get("/admin/terms/{taxonomy}")]
pub async fn list(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(taxonomy): Path<String>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ManageCategories);
    let registered = resolve(&taxonomy)?;
    let mut terms = repos.terms.find_by_taxonomy(taxonomy.clone()).await?;
    terms.sort_by_key(|term| term.name.to_lowercase());

    let body = html! {
        div class="grid grid-cols-1 lg:grid-cols-3 gap-6" {
            div class="lg:col-span-2 bg-white rounded-lg shadow overflow-hidden" {
                table class="w-full text-sm" {
                    caption class="sr-only" { (registered.plural) }
                    thead class="bg-gray-50 text-left text-xs uppercase tracking-wide \
                                 text-gray-500" {
                        tr {
                            th scope="col" class="px-4 py-3" { "Name" }
                            th scope="col" class="px-4 py-3" { "Slug" }
                            th scope="col" class="px-4 py-3" { "Posts" }
                            th scope="col" class="px-4 py-3" { span class="sr-only" { "Actions" } }
                        }
                    }
                    tbody {
                        @for term in &terms {
                            tr class="border-t border-gray-100" {
                                td class="px-4 py-3 font-medium" {
                                    @if let Some(parent) = term.parent_id {
                                        @if let Some(p) = terms.iter().find(|t| t.id == parent) {
                                            span class="text-gray-400" { (p.name) " — " }
                                        }
                                    }
                                    (term.name)
                                }
                                td class="px-4 py-3 font-mono text-xs text-gray-500" {
                                    (term.slug)
                                }
                                td class="px-4 py-3 text-gray-500" { (term.post_count) }
                                td class="px-4 py-3 text-right" {
                                    form method="post"
                                         action=(format!("/admin/terms/{taxonomy}/{}/delete",
                                                          term.id))
                                         class="inline" {
                                        (csrf.input())
                                        button type="submit"
                                               class="text-red-700 hover:underline text-xs" {
                                            "Delete"
                                        }
                                    }
                                }
                            }
                        }
                        @if terms.is_empty() {
                            tr { td colspan="4" class="px-4 py-10 text-center text-gray-400" {
                                "No " (registered.plural.to_lowercase()) " yet."
                            } }
                        }
                    }
                }
            }

            form action=(format!("/admin/terms/{taxonomy}")) method="post"
                 class="bg-white rounded-lg shadow p-5 space-y-3 h-fit" {
                     (csrf.input())
                h2 class="font-semibold text-sm" { "Add " (registered.singular.to_lowercase()) }
                div {
                    label for="name" class="block text-sm font-medium mb-1" { "Name" }
                    input #name type="text" name="name" required maxlength="200"
                          class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="slug" class="block text-sm font-medium mb-1" {
                        "Slug " span class="text-gray-400 font-normal" { "(optional)" }
                    }
                    input #slug type="text" name="slug"
                          class="w-full border rounded px-3 py-2 font-mono text-sm";
                }
                @if registered.hierarchical {
                    div {
                        label for="parent_id" class="block text-sm font-medium mb-1" { "Parent" }
                        select #parent_id name="parent_id"
                               class="w-full border rounded px-3 py-2 text-sm" {
                            option value="" { "(none)" }
                            @for term in &terms {
                                option value=(term.id) { (term.name) }
                            }
                        }
                    }
                }
                div {
                    label for="description" class="block text-sm font-medium mb-1" {
                        "Description"
                    }
                    textarea #description name="description" rows="3"
                             class="w-full border rounded px-3 py-2 text-sm" {}
                }
                button type="submit"
                       class="w-full px-4 py-2 bg-indigo-600 text-white rounded \
                              hover:bg-indigo-700" {
                    "Add"
                }
            }
        }
    };

    Ok(layout(
        &user,
        &csrf,
        &format!("/admin/terms/{taxonomy}"),
        registered.plural,
        body,
    )
    .into_response())
}

#[post("/admin/terms/{taxonomy}")]
pub async fn create(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(taxonomy): Path<String>,
    Form(form): Form<TermForm>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::ManageCategories);
    resolve(&taxonomy)?;

    let parent_id = form
        .parent_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<i64>().ok());

    // The `<select>` only ever offers this taxonomy's own terms, but the id is
    // a number in a form body and nothing downstream re-derives it: the foreign
    // key accepts any `terms.id`, and `TermHooks::before_create` can only clear
    // the parent for a *flat* taxonomy because a hook has no database of its
    // own to look the candidate up in. So a crafted submission could file a
    // category under a tag. The result is a row no screen can render — the list
    // resolves parent names only within the taxonomy it loaded — and one the
    // exporter drops on the floor for the same reason, silently flattening the
    // tree on the next restore. Resolve it here, where there is a connection,
    // and require the match.
    if let Some(parent_id) = parent_id {
        let parent = repos
            .terms
            .find_by_id(parent_id)
            .await?
            .ok_or_else(|| AutumnError::unprocessable_msg("No such parent"))?;
        if parent.taxonomy != taxonomy {
            return Err(AutumnError::unprocessable_msg(
                "A term's parent must belong to the same taxonomy",
            ));
        }
    }

    // Slugging, taxonomy validation and the flat-taxonomy parent rule all live
    // in `TermHooks`, so the importer and the REST API get them too.
    repos
        .terms
        .save(&NewTerm {
            taxonomy: taxonomy.clone(),
            name: form.name.trim().to_owned(),
            slug: form.slug.trim().to_owned(),
            description: form.description.trim().to_owned(),
            parent_id,
        })
        .await?;

    Ok(Redirect::to(&format!("/admin/terms/{taxonomy}")).into_response())
}

#[post("/admin/terms/{taxonomy}/{id}/delete")]
pub async fn delete(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path((taxonomy, id)): Path<(String, i64)>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::ManageCategories);

    // The `post_terms` rows go with it through `ON DELETE CASCADE`, so a
    // deleted category unfiles its posts rather than orphaning join rows.
    // The posts themselves are untouched — deleting a category must never
    // delete content, which is the mistake that makes this button scary.
    repos.terms.delete_by_id(id).await?;
    Ok(Redirect::to(&format!("/admin/terms/{taxonomy}")).into_response())
}

/// Silence the unused-import warning for types referenced only in signatures
/// the compiler already checks.
#[allow(dead_code)]
fn _type_uses(_: Term, _: UpdateTerm) {}
