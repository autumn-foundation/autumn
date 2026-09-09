//! Categories, tags, and any custom taxonomy — one screen for all of them.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::content;
use crate::content_types::{self, Taxonomy};
use crate::models::{NewTerm, Term, UpdateTerm};
use crate::repositories::TermRepository as _;
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::layout;

/// How many terms one page of the taxonomy screen shows.
const TERMS_PER_PAGE: i64 = 50;

/// How many terms the hierarchical parent selector offers.
///
/// Terms accumulate through ordinary category creation *and* the post editor's
/// find-or-create box, so this control grows on its own; the same bound the
/// media and page pickers now carry.
const TERM_PARENT_LIMIT: i64 = 100;

#[derive(Debug, Default, Deserialize)]
pub struct TermsFilter {
    #[serde(default)]
    pub page: Option<usize>,
}

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
    Query(filter): Query<TermsFilter>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ManageCategories);
    let registered = resolve(&taxonomy)?;

    // Ordered and bounded in SQL, and the parent selector below is bounded
    // separately. This screen loaded and sorted the whole taxonomy and then
    // rendered every term a second time as an `<option>`, so the only screen
    // for managing terms was the one a large taxonomy broke first.
    let page = i64::try_from(filter.page.unwrap_or(1).clamp(1, 100_000)).unwrap_or(1);
    let (terms, total, parents, parent_names, parents_truncated) = {
        let mut conn = repos.conn().await?;
        let (terms, total) = content::terms_page_with_total(
            &mut conn,
            &taxonomy,
            (page - 1) * TERMS_PER_PAGE,
            TERMS_PER_PAGE,
        )
        .await?;
        // The parent names this page refers to, fetched by id. Resolving them
        // from the loaded set was correct only while the set was the whole
        // taxonomy: a term whose parent sits on another page would have
        // rendered as if it had none.
        let parent_ids: Vec<i64> = terms.iter().filter_map(|term| term.parent_id).collect();
        let parent_names = content::terms_by_ids(&mut conn, &parent_ids).await?;
        let (parents, parents_truncated) = if registered.hierarchical {
            let rows = content::terms_page(&mut conn, &taxonomy, 0, TERM_PARENT_LIMIT).await?;
            (rows, total > TERM_PARENT_LIMIT)
        } else {
            (Vec::new(), false)
        };
        (terms, total, parents, parent_names, parents_truncated)
    };
    let last_page = ((total + TERMS_PER_PAGE - 1) / TERMS_PER_PAGE).max(1);

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
                                        @if let Some(p) = parent_names.get(&parent) {
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
                                @if page > 1 {
                                    "Nothing on this page."
                                } @else {
                                    "No " (registered.plural.to_lowercase()) " yet."
                                }
                            } }
                        }
                    }
                }

                @if last_page > 1 {
                    nav aria-label="Term pages"
                        class="flex items-center justify-between p-4 border-t \
                               border-gray-100 text-sm" {
                        @if page > 1 {
                            a href=(format!("/admin/terms/{taxonomy}?page={}", page - 1))
                              class="text-indigo-700 hover:underline" { "← Previous" }
                        } @else {
                            span {}
                        }
                        span class="text-gray-500" { "Page " (page) " of " (last_page) }
                        @if page < last_page {
                            a href=(format!("/admin/terms/{taxonomy}?page={}", page + 1))
                              class="text-indigo-700 hover:underline" { "Next →" }
                        } @else {
                            span {}
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
                            @for term in &parents {
                                option value=(term.id) { (term.name) }
                            }
                        }
                        @if parents_truncated {
                            p class="text-xs text-gray-400 mt-1" {
                                "Showing the first " (TERM_PARENT_LIMIT) " by name."
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
