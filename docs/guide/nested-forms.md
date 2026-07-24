# Nested `has_many` Forms

A **nested form** binds a parent record *and* a repeating collection of child
records in a single HTML `<form>`, then decodes, validates, and saves them
together — the classic "master–detail" edit page (an order with its line items,
a survey with its questions, a collection with its links). Autumn's
[`NestedChangesetForm<P, C>`](../../autumn/src/nested_form.rs) is the
`has_many` counterpart of [`ChangesetForm<T>`](../../autumn/src/form.rs): one
extractor decodes the whole submission, runs `validator::Validate` on the parent
and on every non-empty child row, and — when anything fails — hands the whole
changeset back so you can re-render the form with each field preserved and its
error inline.

It works **without any JavaScript**: an extra blank template row is rendered so a
user can add a child, and a `_destroy` checkbox removes one. htmx is an optional
progressive enhancement layered on top.

> A complete runnable version of everything below lives in
> [`examples/wiki`](../../examples/wiki) — the **Collections** feature
> (`src/routes/collections.rs`, `src/models.rs`), where a *collection* (parent)
> owns many *links* (children).

## The model

You need a parent type and a child type. Both derive `serde::Deserialize` and
`validator::Validate`; the child additionally implements
[`NestedChild`](../../autumn/src/nested_form.rs), whose `COLLECTION` constant
names the field group.

```rust
use autumn_web::nested_form::NestedChild;

/// Parent side of the form.
#[derive(Default, serde::Deserialize, serde::Serialize, validator::Validate)]
pub struct CollectionForm {
    #[validate(length(min = 1, message = "Title is required"))]
    pub title: String,
}

/// One repeated child row.
#[derive(serde::Deserialize, serde::Serialize, validator::Validate)]
pub struct LinkForm {
    #[validate(length(min = 1, message = "Label is required"))]
    pub label: String,
    #[validate(url(message = "Must be a valid URL"))]
    pub url: String,
}

impl NestedChild for LinkForm {
    const COLLECTION: &'static str = "links";
}
```

`serde::Serialize` on the child is only needed if you seed an **edit** form (see
[Editing](#editing)); the create-only path can drop it.

These are the *form* shapes — the fields the browser submits. Your persisted
Diesel models (`Collection`, `CollectionLink`, `NewCollection`,
`NewCollectionLink`) are separate structs, exactly as in any other Autumn form.

## The wire format

The child collection is a set of indexed, bracketed input names. `COLLECTION`
(`"links"`) is the group; the number is the row; the last segment is the
subfield:

```text
title=Rust reading list        // parent field
links[0][label]=The Book       // row 0
links[0][url]=https://doc.rust-lang.org/book/
links[1][label]=Rustonomicon   // row 1
links[1][url]=https://doc.rust-lang.org/nomicon/
links[1][_destroy]=1           // optional: mark row 1 for removal
```

Row indices need not be contiguous — a client that removes the middle row can
leave a gap (`links[0]`, `links[2]`), and the decoder compacts them into
sequential rows in ascending order. Per-row validation errors are addressable
with combined keys of the shape `links[1].url`.

## Rendering the form

With the `maud` feature, [`inputs_for`](../../autumn/src/nested_form.rs) renders
the repeating child block and the row-scoped input helpers produce the correct
nested `name`s and per-row-unique ids. The surrounding `<form>` is emitted by
`form.form_tag`, which injects the CSRF (and one-time submit-token) hidden fields
under the app-configured names.

```rust
use autumn_web::form::{required_text_input, submit_button};
use autumn_web::nested_form::{inputs_for, InputsForOptions, NestedChangesetForm};
use autumn_web::prelude::*;

fn collection_form(
    form: &NestedChangesetForm<CollectionForm, LinkForm>,
    action: &str,
    submit_label: &str,
) -> Markup {
    let opts = InputsForOptions::default();
    form.form_tag(action, "post", html! {
        // Parent field — `form` derefs to its inner `NestedChangeset`, whose
        // `parent` is a plain `Changeset<CollectionForm>`.
        (required_text_input(&form.parent, "title", "Collection title"))

        // Repeating child rows. `inputs_for` re-emits every submitted row (with
        // its values + inline errors) then appends a blank template row.
        (inputs_for(form, &opts, |row| html! {
            (row.required_text_input("label", "Label"))
            (row.required_text_input("url", "URL"))
            (row.destroy_checkbox("Remove"))
        }))

        (submit_button(submit_label))
    })
}
```

The closure receives a [`RowScope`](../../autumn/src/nested_form.rs) with
row-scoped builders — `text_input`, `required_text_input`, `number_input`,
`textarea_input`, `hidden_input`, and `destroy_checkbox` — each emitting a
`links[{i}][{sub}]` name and a unique id so repeated rows never collide.

### The blank template row and no-JS "add a child"

`inputs_for` always appends **at least one** blank template row. Because the
browser submits that row's empty inputs, the decoder applies Rails-style
`reject_if: :all_blank`: a child row whose every non-`_destroy` subfield is blank
is **ignored** entirely — never decoded, validated, or saved. So the blank
template row is safe with no JavaScript and never becomes a phantom child.

To keep the no-JS path submittable, `required_text_input` deliberately omits the
client-side `required` attribute **on the blank template row** (and on a row
marked for destruction), so the browser's native validation does not block
submitting a form that still carries an empty trailing row. Required-ness is
still enforced **server-side** for any row the user actually fills.

### Optional: an htmx "Add row" button

Set [`InputsForOptions::add_row_url`](../../autumn/src/nested_form.rs) to an
endpoint that returns a single row via
[`nested_row_fragment`](../../autumn/src/nested_form.rs), and `inputs_for` also
renders an "Add row" button (`hx-get` + `hx-swap="beforeend"`). This is a
progressive enhancement — the no-JS blank-row path keeps working unchanged.

## Decoding and validating in the handler

Extract `NestedChangesetForm<P, C>` and call `into_valid()`. On success you get
`(P, Vec<C>)` — the validated parent plus the non-destroyed children in order.
On failure you get the whole form back, ready to re-render at `422` with values
and errors preserved:

```rust
use autumn_web::nested_form::NestedChangesetForm;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;

#[post("/collections")]
pub async fn create(
    mut db: Db,
    form: NestedChangesetForm<CollectionForm, LinkForm>,
) -> AutumnResult<Response> {
    match form.into_valid() {
        Err(form) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            collection_form(&form, "/collections", "Create collection"),
        )
            .into_response()),
        Ok((collection, links)) => {
            let id = save_collection(&mut db, collection, links).await?;
            Ok(Redirect::to(&format!("/collections/{id}")).into_response())
        }
    }
}
```

Like `ChangesetForm`, the extractor does **not** reject an invalid submission
with `422` itself — the errors live in the changeset and *you* decide the
response. (A malformed parent value is still a hard `400`, matching
`ChangesetForm`; a bad child row is a soft, re-renderable error.)

## Saving atomically

A parent and its children must be persisted **atomically** — a half-saved
collection with only some of its links is never acceptable. Do it inside a
single [`Db::tx`](../../autumn/src/db.rs): insert the parent, read back its
generated `id`, stamp each child's foreign key, and insert the children — all on
the one `conn` the closure is handed. Returning `Err` from anywhere in the
closure rolls the **whole** transaction back.

```rust
use scoped_futures::ScopedFutureExt;

async fn save_collection(
    db: &mut Db,
    collection: CollectionForm,
    links: Vec<LinkForm>,
) -> AutumnResult<i64> {
    db.tx(move |conn| async move {
        let created: Collection = diesel::insert_into(collections::table)
            .values(&NewCollection { title: collection.title })
            .returning(Collection::as_returning())
            .get_result(conn)
            .await?;

        for (i, link) in links.into_iter().enumerate() {
            diesel::insert_into(collection_links::table)
                .values(&NewCollectionLink {
                    collection_id: created.id, // FK from the freshly-read parent id
                    label: link.label,
                    url: link.url,
                    position: i as i32,
                })
                .execute(conn)
                .await?; // any Err here rolls back the parent insert too
        }

        Ok::<_, AutumnError>(created.id)
    }
    .scope_boxed())
    .await
}
```

> **Use raw diesel inserts on `conn`, not a generated
> [`#[repository]`](../../autumn/src/lib.rs) `create`.** That `create` opens its
> *own* `Db::tx`, and `Db::tx` cannot be re-entered on the same connection — the
> nested call trips the nested-transaction guard and returns a `400`. Keep the
> whole parent-plus-children unit of work in the one outer `tx`.

## Editing

For an **edit** page, build the form with
[`seeded`](../../autumn/src/nested_form.rs) instead of `blank`, passing the
existing children as child-form values. `seeded` pre-renders one row per existing
child (pre-filling its inputs) so the page shows and preserves current rows, and
the no-JS `_destroy` removal works before the first submit:

```rust
#[get("/collections/{id}/edit")]
pub async fn edit_form(Path(id): Path<i64>, mut db: Db) -> AutumnResult<Markup> {
    let collection = load_collection(&mut db, id).await?;
    let links = load_links(&mut db, id).await?;

    let form = NestedChangesetForm::<CollectionForm, LinkForm>::seeded(
        CollectionForm { title: collection.title.clone() },
        links.iter().map(|l| LinkForm { label: l.label.clone(), url: l.url.clone() }).collect(),
        None, // CSRF token, when middleware is active
    );
    Ok(collection_form(&form, &format!("/collections/{id}"), "Save changes"))
}
```

On the update `POST`, decode and validate the same way, then persist the new
child set. The simplest correct strategy — and the one the wiki example uses — is
to **replace the whole set inside one transaction**: update the parent, delete
its existing children, and re-insert the submitted (non-destroyed) ones. This
sidesteps per-row reconciliation entirely while staying atomic. Deeper
reconciliation (matching submitted rows back to persisted ids to compute
per-row inserts / updates / deletes, or reordering) is intentionally out of
scope for the built-in helpers; carry each row's primary key as a
`row.hidden_input("id", …)` if you want to build it yourself.

## Removing a child (`_destroy`)

`row.destroy_checkbox("Remove")` renders a durable, no-JS removal control. When
its box is ticked and the form submitted, the decoder honours the truthy
`_destroy` marker: the row is **retained for re-render** (so the checkbox state
survives a round-trip) but is **excluded** from the `Vec<C>` that `into_valid()`
returns — so a "replace the whole set" update simply never re-inserts it. An
htmx/JS row removal (swapping the row node out of the DOM) is an optional
enhancement on top; this checkbox is the required mechanism.

## CSRF and one-time submit tokens

`form.form_tag(...)` injects the CSRF hidden field under the app-configured
`security.csrf.form_field` name automatically on the re-render (POST) path. On
the initial GET render, pass the token into `blank` / `seeded` (its third
argument) so the field is present, and call `.with_submit_token(Some(token))` if
you also use one-time [submit tokens](../../autumn/src/security/submit_token.rs)
to protect the first submission against double-submit. Prefer `form.form_tag`
over the standalone `form_tag` helper for nested re-renders: it emits the CSRF
field under the captured field name, so a customized `form_field` survives the
round-trip.

## Quick reference

| Piece | Purpose |
|-------|---------|
| `NestedChild::COLLECTION` | Names the child field group (`links[i][field]`) |
| `NestedChangesetForm::<P, C>::blank(parent, csrf)` | Clean create render (no rows) |
| `NestedChangesetForm::<P, C>::seeded(parent, children, csrf)` | Edit render (one row per child) |
| `inputs_for(&form, &opts, render_row)` | Renders the repeating child block + blank row |
| `RowScope` builders | `text_input`, `required_text_input`, `number_input`, `textarea_input`, `hidden_input`, `destroy_checkbox` |
| `form.into_valid()` | `Ok((P, Vec<C>))` or `Err(form)` for re-render |
| `Db::tx(...)` | Save parent + children atomically |

## See also

- [`examples/wiki`](../../examples/wiki) — the Collections feature is a complete,
  runnable master–detail form.
- [Forms & validation](../../autumn/src/form.rs) — the single-record
  `ChangesetForm<T>` this builds on.
- [One-time submit tokens](../../autumn/src/security/submit_token.rs) — protect
  the first submit against double-submit.
