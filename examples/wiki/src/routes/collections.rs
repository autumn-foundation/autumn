//! Curated link collections — a worked example of nested (`has_many`) form
//! binding with [`NestedChangesetForm`](autumn_web::nested_form::NestedChangesetForm).
//!
//! A **collection** (the parent) owns many **links** (the children). One HTML
//! form edits both at once: the parent's `title` plus a repeating block of
//! `label` / `url` rows. The same `NestedChangesetForm<CollectionForm, LinkForm>`
//! type decodes the submission, validates the parent and every non-blank child,
//! and — on a failed submit — re-renders the whole form at `422` with each field
//! preserved and its error inline. On success the parent and its children are
//! inserted in a **single transaction**, so a collection is never half-saved.
//!
//! The child block is rendered with [`inputs_for`](autumn_web::nested_form::inputs_for),
//! which appends a blank template row so a user can add a link with **no
//! JavaScript**; the decoder drops that row when it is left empty. Ticking a
//! row's Remove checkbox (`_destroy`) drops it from the saved set. See
//! `docs/guide/nested-forms.md` for the full walkthrough.
//!
//! The wiki example runs without CSRF (like its other forms), so `None` is
//! passed for the CSRF token throughout; an app with CSRF middleware would
//! thread the token from a `CsrfToken` extractor into `blank` / `seeded`.

use autumn_web::extract::Path;
use autumn_web::form::{required_text_input, submit_button};
use autumn_web::nested_form::{InputsForOptions, NestedChangesetForm, inputs_for};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::models::{
    Collection, CollectionForm, CollectionLink, LinkForm, NewCollection, NewCollectionLink,
};
use crate::routes::pages::layout;
use crate::schema::{collection_links, collections};

/// Render the shared new/edit form for a collection and its links.
///
/// `form` is a [`NestedChangesetForm`]: on the initial GET it is a
/// [`blank`](NestedChangesetForm::blank) (create) or
/// [`seeded`](NestedChangesetForm::seeded) (edit) changeset; on a failed POST it
/// is the invalid changeset returned by `into_valid`, so values and per-field
/// errors pre-fill automatically.
fn collection_form(
    form: &NestedChangesetForm<CollectionForm, LinkForm>,
    action: &str,
    heading: &str,
    submit_label: &str,
) -> Markup {
    let opts = InputsForOptions::default();
    layout(
        heading,
        html! {
            (breadcrumb(&[
                Crumb::link("Wiki", "/"),
                Crumb::link("Collections", &paths::list()),
                Crumb::current(heading),
            ]))
            h1 class="text-2xl font-bold mb-6" { (heading) }
            // `form.form_tag` injects the CSRF / submit-token hidden fields under
            // the app-configured names when present. The wiki runs without CSRF,
            // so none are emitted here — but preferring it over a bare `<form>`
            // keeps the example correct for an app that adds CSRF later.
            (form.form_tag(action, "post", html! {
                div class="space-y-4 bg-white rounded shadow p-6" {
                    // Parent field. `form` derefs to its inner `NestedChangeset`,
                    // whose `parent` is a plain `Changeset<CollectionForm>`.
                    (required_text_input(&form.parent, "title", "Collection title"))

                    h2 class="text-lg font-semibold pt-2 border-t" { "Links" }
                    p class="text-sm text-gray-500" {
                        "Add one or more related links. The trailing blank row is "
                        "optional — leave it empty and it is ignored on submit."
                    }

                    // The repeating child block. `inputs_for` re-emits every
                    // submitted row (with values + inline errors) then appends a
                    // blank template row for the no-JS "add a link" path.
                    (inputs_for(form, &opts, |row| html! {
                        div class="flex flex-wrap gap-3 items-start border-t pt-3" {
                            div class="flex-1 min-w-[12rem]" { (row.required_text_input("label", "Label")) }
                            div class="flex-1 min-w-[12rem]" { (row.required_text_input("url", "URL")) }
                            (row.destroy_checkbox("Remove"))
                        }
                    }))

                    (submit_button(submit_label))
                }
            }))
        },
    )
}

/// Map persisted link rows into the child form shape for an edit render.
fn seed_links(links: &[CollectionLink]) -> Vec<LinkForm> {
    links
        .iter()
        .map(|l| LinkForm {
            label: l.label.clone(),
            url: l.url.clone(),
        })
        .collect()
}

async fn load_collection(db: &mut Db, id: i64) -> AutumnResult<Collection> {
    collections::table
        .find(id)
        .select(Collection::as_select())
        .first(&mut **db)
        .await
        .map_err(|_| AutumnError::not_found_msg(format!("Collection #{id} not found")))
}

async fn load_links(db: &mut Db, collection_id: i64) -> AutumnResult<Vec<CollectionLink>> {
    Ok(collection_links::table
        .filter(collection_links::collection_id.eq(collection_id))
        .order(collection_links::position.asc())
        .select(CollectionLink::as_select())
        .load(&mut **db)
        .await?)
}

#[get("/collections")]
pub async fn list(mut db: Db) -> AutumnResult<Markup> {
    let all: Vec<Collection> = collections::table
        .order(collections::created_at.desc())
        .select(Collection::as_select())
        .load(&mut *db)
        .await?;

    Ok(layout(
        "Collections",
        html! {
            (breadcrumb(&[
                Crumb::link("Wiki", "/"),
                Crumb::current("Collections"),
            ]))
            div class="flex justify-between items-center mb-6" {
                h1 class="text-2xl font-bold" { "Link Collections" }
                a href=(paths::new_form())
                  class="bg-emerald-600 text-white px-4 py-2 rounded hover:bg-emerald-700" {
                    "+ New Collection"
                }
            }
            @if all.is_empty() {
                p class="text-gray-400 text-center py-8" {
                    "No collections yet. Create one to group related links."
                }
            } @else {
                ul class="space-y-3" {
                    @for c in &all {
                        li class="p-4 bg-white rounded shadow flex justify-between items-center" {
                            a href=(paths::show(c.id))
                              class="text-emerald-700 font-medium hover:underline" { (c.title) }
                            span class="text-sm text-gray-400" {
                                (c.created_at.format("%Y-%m-%d %H:%M"))
                            }
                        }
                    }
                }
            }
        },
    ))
}

#[get("/collections/new")]
pub async fn new_form() -> Markup {
    // A blank, non-validating changeset: the create page renders clean (no
    // premature "required" error before the user has typed) with zero child
    // rows plus one blank template row from `inputs_for`.
    let form =
        NestedChangesetForm::<CollectionForm, LinkForm>::blank(CollectionForm::default(), None);
    collection_form(
        &form,
        &paths::create(),
        "New Collection",
        "Create collection",
    )
}

#[post("/collections")]
pub async fn create(
    mut db: Db,
    form: NestedChangesetForm<CollectionForm, LinkForm>,
) -> AutumnResult<Response> {
    match form.into_valid() {
        // Invalid: re-render the same form at 422 with values + inline errors.
        Err(form) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            collection_form(
                &form,
                &paths::create(),
                "New Collection",
                "Create collection",
            ),
        )
            .into_response()),
        // Valid: insert the parent and its children atomically. Raw diesel
        // inserts on the one `conn` (not a generated repository `create`, which
        // opens its own transaction) — an Err anywhere rolls the whole unit back.
        Ok((collection, links)) => {
            let new_id = db
                .tx(move |conn| {
                    async move {
                        let created: Collection = diesel::insert_into(collections::table)
                            .values(&NewCollection {
                                title: collection.title,
                            })
                            .returning(Collection::as_returning())
                            .get_result(conn)
                            .await?;

                        for (i, link) in links.into_iter().enumerate() {
                            diesel::insert_into(collection_links::table)
                                .values(&NewCollectionLink {
                                    collection_id: created.id,
                                    label: link.label,
                                    url: link.url,
                                    position: i as i32,
                                })
                                .execute(conn)
                                .await?;
                        }

                        Ok::<_, AutumnError>(created.id)
                    }
                    .scope_boxed()
                })
                .await?;

            Ok(Redirect::to(&paths::show(new_id)).into_response())
        }
    }
}

#[get("/collections/{id}")]
pub async fn show(Path(id): Path<i64>, mut db: Db) -> AutumnResult<Markup> {
    let collection = load_collection(&mut db, id).await?;
    let links = load_links(&mut db, id).await?;

    Ok(layout(
        &collection.title,
        html! {
            (breadcrumb(&[
                Crumb::link("Wiki", "/"),
                Crumb::link("Collections", &paths::list()),
                Crumb::current(&collection.title),
            ]))
            div class="flex justify-between items-center mb-4" {
                h1 class="text-3xl font-bold" { (collection.title) }
                a href=(paths::edit_form(collection.id))
                  class="text-sm text-emerald-600 hover:underline" { "Edit" }
            }
            @if links.is_empty() {
                p class="text-gray-400 italic" { "This collection has no links yet." }
            } @else {
                ul class="space-y-2 bg-white rounded shadow p-6" {
                    @for l in &links {
                        li {
                            a href=(l.url) rel="noopener noreferrer"
                              class="text-emerald-700 font-medium hover:underline" { (l.label) }
                            span class="text-sm text-gray-400" { " — " (l.url) }
                        }
                    }
                }
            }
        },
    ))
}

#[get("/collections/{id}/edit")]
pub async fn edit_form(Path(id): Path<i64>, mut db: Db) -> AutumnResult<Markup> {
    let collection = load_collection(&mut db, id).await?;
    let links = load_links(&mut db, id).await?;

    // `seeded` pre-renders one row per existing link so the edit page shows and
    // preserves current links, and the no-JS Remove (`_destroy`) works before
    // the first submit.
    let form = NestedChangesetForm::<CollectionForm, LinkForm>::seeded(
        CollectionForm {
            title: collection.title.clone(),
        },
        seed_links(&links),
        None,
    );

    Ok(collection_form(
        &form,
        &paths::update(id),
        &format!("Edit: {}", collection.title),
        "Save changes",
    ))
}

#[post("/collections/{id}")]
pub async fn update(
    Path(id): Path<i64>,
    mut db: Db,
    form: NestedChangesetForm<CollectionForm, LinkForm>,
) -> AutumnResult<Response> {
    match form.into_valid() {
        Err(form) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            collection_form(&form, &paths::update(id), "Edit collection", "Save changes"),
        )
            .into_response()),
        // Valid: update the title, then replace the child set — delete the old
        // links and re-insert the submitted (non-destroyed) ones — all in one
        // transaction. Replacing the whole set keeps the example focused on the
        // nested-form mechanics; per-row diffing is intentionally out of scope
        // (see the guide's "Editing" section).
        Ok((collection, links)) => {
            db.tx(move |conn| {
                async move {
                    let rows = diesel::update(collections::table.find(id))
                        .set(collections::title.eq(&collection.title))
                        .execute(conn)
                        .await?;
                    if rows == 0 {
                        return Err(AutumnError::not_found_msg(format!(
                            "Collection #{id} not found"
                        )));
                    }

                    diesel::delete(
                        collection_links::table.filter(collection_links::collection_id.eq(id)),
                    )
                    .execute(conn)
                    .await?;

                    for (i, link) in links.into_iter().enumerate() {
                        diesel::insert_into(collection_links::table)
                            .values(&NewCollectionLink {
                                collection_id: id,
                                label: link.label,
                                url: link.url,
                                position: i as i32,
                            })
                            .execute(conn)
                            .await?;
                    }

                    Ok::<_, AutumnError>(())
                }
                .scope_boxed()
            })
            .await?;

            Ok(Redirect::to(&paths::show(id)).into_response())
        }
    }
}

autumn_web::paths![list, new_form, create, show, edit_form, update];
