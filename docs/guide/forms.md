# Forms, Validation and Normalization

A form submission is the most common way a user changes data, and it is the one
place where "the happy path" is the *rare* path. Someone will submit a blank
title, an email with a trailing space, a date that will not parse, or the same
form twice. Autumn's answer has three layers, and knowing which layer owns which
problem is most of the work:

| Layer | Owns | Primary type |
|---|---|---|
| **The form** | re-rendering the page with the user's input still in the fields | `ChangesetForm<T>` |
| **Validation** | deciding whether a value is acceptable, and saying why | `#[validate(...)]`, `Valid<T>` |
| **Normalization** | making an acceptable value *canonical* before it is stored | `#[normalize(...)]` |

Those three layers run in two distinct places, and conflating them is the
single easiest way to write a validator that rejects input you meant to accept:

- **The form layer validates first, on raw input.** `ChangesetForm` extraction
  calls `IntoChangeset`, which runs `validator::Validate` on the value exactly
  as it was decoded from the body — before any handler code, and before any
  repository sees it.
- **The repository layer then normalizes, validates, and saves**, in that order.
  This is the pipeline `#[normalize(...)]` belongs to.

So `normalize, then validate, then save` is true of the **model write path**,
not of the form. A form-level `#[validate(email)]` sees `" Alice@example.com "`
with its spaces intact and rejects it, even though the model carries
`#[normalize(trim)]` and would have canonicalized it a moment later.

Design around it: keep form-level validators tolerant of what a human actually
types (shape and length, not exact canonical form), and put the rules that
assume a canonical value on the model, where normalization has already run.

---

## The shape of a form round-trip

Server-rendered forms have one hard requirement that API endpoints do not: on
failure you must render the form **again**, with the values the user typed and
the errors next to the right fields. Everything else follows from that.

A `Changeset<T>` is the value that makes it possible — the submitted data
*plus* the per-field errors, together. `ChangesetForm<T>` is the extractor that
produces one:

```rust,ignore
use autumn_web::prelude::*;
use autumn_web::form::ChangesetForm;
use serde::{Deserialize, Serialize};
use validator::Validate;

// `Default` is here for the blank-form constructor further down
// (`ChangesetForm::blank(TodoForm::default(), ..)`): a form struct that a GET
// route renders empty needs it, and every field type already has one.
#[derive(Deserialize, Serialize, Validate, Clone, Default)]
pub struct TodoForm {
    #[validate(length(min = 1, max = 255, message = "Title must be 1–255 characters"))]
    title: String,
}

#[get("/todos/new")]
async fn new_todo(csrf: CsrfToken) -> Markup {
    // Correct as written under the default CSRF field name. If your app sets
    // `security.csrf.form_field`, this needs one more parameter — see
    // "CSRF is not optional, and not your job" below.
    let blank = ChangesetForm::blank(TodoForm { title: String::new() }, csrf.token());
    layout(render_form(&blank))
}

#[post("/todos")]
async fn create(mut db: Db, form: ChangesetForm<TodoForm>) -> AutumnResult<impl IntoResponse> {
    match form.into_valid() {
        Ok(valid) => {
            insert(&mut db, valid).await?;
            Ok(Redirect::to("/todos").into_response())
        }
        Err(form) => {
            // `form` still holds the submitted values *and* the errors.
            Ok((StatusCode::UNPROCESSABLE_ENTITY, render_form(&form)).into_response())
        }
    }
}
```

`into_valid` is the whole control flow: `Ok(T)` on success, `Err(Self)` on
failure — and the `Err` arm gives the form back, ready to re-render. There is no
flash-carrying, no session round-trip, and no second deserialization.

`examples/todo-app/src/routes/todos.rs` is this pattern end to end, including
the list page it re-renders on failure.

### Rendering the fields

`ChangesetForm` renders each control with its current value, its errors, and the
ARIA wiring that ties the two together:

```rust,ignore
fn render_form(form: &ChangesetForm<TodoForm>) -> Markup {
    form.form_tag("/todos", "post", html! {
        (form.text_input("title", "Title"))
        (form.submit_button("Add"))
    })
}
```

`form_tag` emits the hidden `_csrf` input automatically — see
[CSRF](#csrf-is-not-optional-and-not-your-job) below. For fields the built-in
helpers do not cover, read the state yourself:

| Method | Returns |
|---|---|
| `form.field_value("title")` | the submitted value, for the `value=` attribute |
| `form.errors_for("title")` | this field's error messages |
| `form.is_valid()` | whether the changeset has any errors at all |

The free functions in `autumn_web::form` cover the rest of the control types:
`password_input`, `textarea_input`, `checkbox_input`, `number_input`,
`date_input`, `datetime_input`, and `select_input`.

A `required_*` variant — one that adds the HTML5 constraint attributes for you —
exists for **some but not all** of them:

| Has a `required_*` variant | Does not |
|---|---|
| `text_input`, `number_input`, `date_input`, `datetime_input`, `select_input`, `rich_text_area` | `password_input`, `textarea_input`, `checkbox_input` |

For the three without one, set the attribute yourself in the markup. Server-side
validation is unaffected either way: `required_*` only writes the client-side
hint, and `#[validate(presence)]` on the model is what actually rejects a blank
submission.

### Rendering a whole form in one call

A `#[model]` implements `FormModel`, so `form_for` can derive one control per
editable column:

```rust,ignore
use autumn_web::form::form_for;

form_for(&changeset, "/posts", "post")
    .csrf(csrf.token())
    .exclude("author_id")
    .override_label("body", "Post text")
    .render()
```

This is what the scaffold generator emits. Reach for the individual helpers when
the form's layout matters; reach for `form_for` when it does not.

---

## Two validation paths, and how to choose

Autumn validates with the [`validator`](https://docs.rs/validator) crate:
`#[validate(...)]` attributes on the struct, `#[derive(Validate)]` to run them.
Where the *result* goes is what differs.

### `ChangesetForm<T>` — for pages a human is looking at

Invalid input is **data**, not an error: the extractor does not fail on a
validation error, the handler decides what to render, and the user gets their
input back. This is the right choice for every server-rendered HTML form.

**It can still reject a body it cannot decode.** `decode_form_body` returns 415
for the wrong `Content-Type` and 400 when a field will not deserialize into its
declared type — before the handler runs, so there is nothing to re-render with.
The round-trip covers *invalid* values, not *unparseable* ones.

That distinction has a practical consequence: **type a form field as the string
the browser actually submits, and convert after validation.** A field typed
`i64` turns "abc" — or an empty select — into a hard 400 that discards the whole
submission, title and body included. A `String` field with a validator that
parses it turns the same input into an inline message next to the offending
field:

```rust,ignore
#[derive(Deserialize, Serialize, Validate, Clone, Default)]
struct SubmitPostForm {
    // Not `i64`: a form field's job is to round-trip whatever the browser
    // sent, so the page can be re-rendered with it. The conversion happens
    // after validation proves it parses.
    #[validate(custom(function = "validate_subreddit_choice"))]
    subreddit_id: String,
    #[validate(length(min = 1, max = 300))]
    title: String,
}

fn validate_subreddit_choice(value: &str) -> Result<(), ValidationError> {
    match value.trim().parse::<i64>() {
        Ok(id) if id > 0 => Ok(()),
        _ => Err(ValidationError::new("subreddit_id")
            .with_message("Choose a community".into())),
    }
}
```

Reserve non-string field types for values a browser cannot get wrong, or for
endpoints where a 400 is the right answer. `examples/reddit-clone`'s submit form
is built this way, for exactly this reason.

One custom validator is worth spelling out, because it is easy to write as dead
code. To reject a title that carries no text — `"***"`, `"🎉🔥💯"` — ask
`autumn_web::contains_letter_or_number`, **not** `slugify(value).is_empty()`:
`slugify` never returns an empty string (input with nothing to slugify gets a
stable hash fallback), so the `is_empty()` form is always `false` and rejects
nothing. See [Generators](./generators.md#human-readable-urls-with-slugslugfrom)
for the full contract.

```rust,ignore
fn validate_sluggable_title(value: &str) -> Result<(), ValidationError> {
    if autumn_web::contains_letter_or_number(value) {
        return Ok(());
    }
    Err(ValidationError::new("title")
        .with_message("Title must contain at least one letter or number".into()))
}
```

A route-local validator only runs on the route. If the same model is also
written through generated API routes or a repository, put the rule in a
mutation hook as well — the hooks are the choke point those paths share.

### `Valid<T>` — for endpoints a program is calling

Invalid input is an **error**. `Valid` wraps another extractor, and a validation
failure short-circuits into a 422 with a field-level error map before the
handler body runs:

```rust,ignore
use autumn_web::Valid;

#[post("/api/posts")]
async fn create(Valid(Json(post)): Valid<Json<NewPost>>) -> impl IntoResponse {
    // `post` is guaranteed valid; there is no invalid branch to write.
}
```

It wraps `Json`, `Form`, or `Query` alike.

### `Validated<T>` and `.validate()` — for values that did not arrive in a request

Sometimes the value comes from a job payload, a CSV row, or another service.
`ValidateExt::validate` runs the same rules and returns
`AutumnResult<Validated<T>>`, so `?` works:

```rust,ignore
use autumn_web::ValidateExt;

let row: Validated<ImportRow> = raw_row.validate()?;
```

`Validated<T>` is *proof* that validation ran: it cannot be constructed outside
the framework, it derefs to `T` for reading, and it deliberately does **not**
implement `DerefMut`, so it cannot be mutated back into an invalid state.

### Reading the failure back

Off the HTTP path nothing renders the 422 for you, so read the field map off
the error instead of re-running `validator` yourself:

```rust,ignore
for (index, raw_row) in rows.into_iter().enumerate() {
    match raw_row.validate() {
        Ok(row) => import(row),
        // "Validation failed: email: Must be a valid email address"
        Err(error) => tracing::warn!(row = index, error = %error, "skipped invalid row"),
    }
}
```

Three accessors, on any `AutumnError`:

- `details()` returns the field map for a validation failure, `None`
  otherwise. It is a `HashMap`, so sort the keys before you render them.
- `code()` returns the stable code the `problem+json` body carries —
  `autumn.validation_failed`, `autumn.not_found`, and so on.
- `message()` returns the wrapped error's message alone, which is what the
  body's `detail` shows.

`Display` appends the failing fields to `message()`, sorted by field name.
Keep untrusted text out of your `#[validate(message = "...")]` strings: this
output reaches your logs.

The HTTP response does not change. Its `detail` still reads `Validation
failed`, with the fields in `errors`.

### Which one?

| The caller is… | Use | Failure looks like |
|---|---|---|
| a browser rendering HTML | `ChangesetForm<T>` | the same page, 422, errors inline (a body that will not *decode* is still 400/415) |
| an API client, an MCP tool, htmx expecting JSON | `Valid<T>` | 422 Problem Details with a field map |
| not a request at all | `.validate()` → `Validated<T>` | an `AutumnError` you handle |

---

## Model-level validation

Putting `#[validate(...)]` on the `#[model]` itself moves the rule to where the
data lives, so **every** write path is covered — a form, an API endpoint, a
seed, a job, a CSV import:

```rust,ignore
#[model]
pub struct User {
    #[validate(email(message = "Must be a valid email address"))]
    pub email: String,

    #[validate(length(min = 2, max = 60))]
    pub display_name: String,
}
```

The generated repository runs these rules on **insert**, before the
`before_create` hook, always.

**On update, merged-model validation is opt-in.** A repository with hooks, or
one declared `#[repository(Post, validate_on_update = fetch)]`, loads the
existing row, applies the patch, and validates the *merged* model — so a
`must_match` or a `#[validate(custom = ...)]` comparing two columns sees real
values on both sides, and a partial update that would break the invariant is
rejected with the same 422 field-error map an insert produces.

A plain generated repository with no hooks and no knob takes the **blind
update path**: it emits no merged-model check at all, deliberately, to avoid an
unconditional extra `SELECT` on every update.

**On that path the repository validates nothing at all** — not cross-field
rules, and not the field-level rules on the columns being written. The blind
branch goes straight from `changes.__to_changeset()` to the `UPDATE`. So a
direct `repo.update(id, &UpdateUser { email: Patch::Set("nope".into()), .. })`
persists a value your `#[validate(email)]` would have rejected.

Validation on an update comes from the *caller*, not the repository: a generated
`--api` handler validates the patch before calling it, and a `ChangesetForm`
handler validates the submitted form struct. If you call the repository directly
and want the model's rules enforced, give it hooks, turn on
`validate_on_update = fetch`, or validate before building the patch — do not
assume it.

Turning it on changes what is *validated*, not what is *stored*: only the hooked
path persists normalized values. See [Updates are not the same as
inserts](#updates-are-not-the-same-as-inserts) for the three paths side by
side.

> Even with the knob, that check is point-in-time against a non-locked
> snapshot. It reliably rejects a single request that is invalid once merged,
> but two concurrent partial writes to different fields can still interleave
> into an invalid row. A cross-field invariant you cannot afford to lose wants
> a database `CHECK` constraint underneath it.

**Model rules and form rules are not redundant.** Put the invariant on the
model, and put anything the *form* alone knows about on the form struct:
`confirm_password`, an "I agree" checkbox, a field that exists only in the UI.
The form struct is also where you put rules that need a different message for
end users than for API clients.

---

## Normalization: `#[normalize(...)]`

Validation decides *whether* to accept a value. Normalization decides *what
exactly gets stored*. They are different problems, and conflating them produces
the classic bug: `"  Alice@Example.COM "` passes an email validator, is stored
verbatim, and then never matches a lookup for `alice@example.com`.

```rust,ignore
#[model]
pub struct User {
    #[normalize(trim, downcase)]
    #[validate(email)]
    pub email: String,

    #[normalize(squish)]
    pub display_name: String,
}
```

Built-in normalizers, applied **left to right in the order you write them**:

| Normalizer | Effect |
|---|---|
| `trim` | strip leading and trailing whitespace |
| `downcase` | lowercase |
| `upcase` | uppercase |
| `squish` | trim, and collapse every internal run of whitespace to one space |
| `strip_nul` | remove every NUL (`U+0000`) — see [Unstorable bytes](#unstorable-bytes-nul) |
| `with = path::to::fn` | your own `fn(&str) -> String` |

All the built-ins are **idempotent** — normalizing an already-normal value
changes nothing — which is what lets the same function run on the write path and
on lookups and still agree.

### Where it runs

Normalization happens at the head of the repository save flow: `save` /
`save_many` normalize the `New*` insert struct, **before** validation and
before the `before_create` hook, so:

```text
submitted value
  → #[normalize] chain          "  Alice@Example.COM " → "alice@example.com"
    → #[validate] rules         validators see the canonical value
      → before_create hook      your code sees the canonical value
        → INSERT                the database stores the canonical value
```

A validator that would have rejected the raw value for its whitespace never
sees the whitespace. That is the intent: `#[normalize(trim)]` on a
`#[validate(length(min = 1))]` field means "a title of only spaces is blank",
which is almost always what you wanted.

### Updates are not the same as inserts

The insert path above stores the canonical value. **Two of the three update
paths do not.** They differ in what validation *sees* and in what actually gets
written, and those are not the same question:

| Repository | Merged model built? | Validation sees | Persisted |
|---|---|---|---|
| No hooks, no knob (**blind**) | no — no `from_patch` at all | **nothing** — the repository runs no validation | **raw patch** |
| `validate_on_update = fetch` | yes, and it is normalized | the normalized merged model | **raw patch** |
| Has hooks | yes | the normalized draft | **normalized draft** |

So on either of the first two, this stores the untrimmed, uncased string even on
a `#[normalize(trim, downcase)]` column:

```rust,ignore
repo.update(id, &UpdateUser { email: Patch::Set("  FOO@X.com ".into()), ..Default::default() }).await?;
// stored: "  FOO@X.com "   — not "foo@x.com"
```

The middle row is the surprising one, and it is deliberate: `from_patch`
normalizes the merged model *in order to validate it*, but that draft is
side-effect-only — the generated code keeps its 422 and throws the value away,
persisting `changes.__to_changeset()` unchanged. A value that passes validation
only because normalization cleaned it up is therefore stored dirty.
`autumn-macros/src/repository.rs` calls this the "normalize-vs-persist
asymmetry" at the site that causes it.

It is a trap worth knowing about, because the stored value then no longer
matches the normalized finders below: `find_by_email("foo@x.com")` will not find
that row, and a uniqueness assumption built on the canonical form is broken. If
you need updates to store canonical values, the hooked path is the one that
does it.

The hooked update path persists the normalized draft and does not have this
problem. So: if a normalized column is ever written through `update`, give the
repository hooks (or normalize the value yourself before building the patch)
rather than assuming the attribute covers it. A `CITEXT` column or a functional
unique index is the durable backstop.

### Lookups normalize too

The derived `#[repository]` finders canonicalize their argument the same way, so
`find_by_email("  FOO@X.com ")` matches the stored `foo@x.com` row. You do not
have to remember to normalize at every call site — which is the failure mode
this attribute exists to remove.

### Constraints

- **`String` only.** `#[normalize]` on `Option<String>` or any non-`String`
  field is a compile error.
- **At least one normalizer.** A bare `#[normalize]` or an empty
  `#[normalize()]` is a compile error rather than a silent no-op.
- **Not compatible with `#[translatable]`.** Normalizers rewrite one string;
  they cannot see inside a per-locale column set.
- **It does not rewrite history.** Adding `#[normalize]` to an existing column
  canonicalizes future writes only. Backfill existing rows with a migration.

---

## Unstorable bytes: NUL

A Postgres `TEXT` or `VARCHAR` column cannot hold a NUL byte (`0x00`). This is
not a length or a format rule that `#[validate(...)]` could express — the value
is a perfectly good Rust `String`, and every validator you can write on it
passes. It fails much later, when the driver hands the byte to the server, which
answers `invalid byte sequence for encoding "UTF8": 0x00`.

A real user can produce one without trying: a paste from a binary source, a
misbehaving input method, a clipboard glitch. So this is a robustness question,
not only an adversarial one.

The framework handles it at the form boundary. The two extractors built for
form round-trips — `ChangesetForm` and `NestedChangesetForm` — sweep every
submitted **text** value before it is deserialized, and a field that carried a
NUL gets an ordinary error:

```text
Cannot contain the NUL character (0x00)
```

The message is available as `autumn_web::form::NUL_CHARACTER_FIELD_ERROR`.
Nothing in your handler changes — it is a field error like any other, so the
form re-renders inline with your existing `errors_for(...)` markup and whatever
4xx you already return for a rejected changeset. The retained value is the
author's text with the byte removed, so the re-rendered form keeps their work
(and never echoes a raw `0x00` back into the HTML), and their next submission
succeeds.

Framework plumbing fields are deliberately exempt — the CSRF token, the submit
token, and the `_method` override, under both their default names and whatever
name you configured. None is a form field any template renders, so an error
keyed under one would leave the form invalid with nothing on screen to explain
why, and none is ever written to a column. Nothing about their own handling
changes either: each is read from the raw body by its middleware, before any
extractor runs, so a NUL-mangled `_csrf` is still a 403 and a NUL-mangled
`_method` is still a 400. (A mangled `_submit_token` is not rejected — it simply
matches no stored token and loses its at-most-once protection, exactly as it
would have before.)

`_destroy` on a nested child row is exempt for a different reason: it is read
only for truthiness, and cleaning it would *change* it — `1\0` is falsy today
and would become truthy — so the sweep leaves it alone entirely.

An *unknown* key is not exempt. A submitted name that is not a field of your
form type is normally ignored, but if it carries a NUL it is still reported —
under that name. Since no template renders that name, the submission is
rejected with no message next to any input; `error_summary` will show the
message, but without saying which input it belongs to. In practice that takes a
hand-crafted request: the values your own templates emit for non-form fields
are server-generated, not pasted.

File parts of a `multipart/form-data` body are untouched — binary uploads are
supposed to contain arbitrary bytes.

### The paths no form extractor sees

A JSON API body, a hand-written query, a background job argument, an
`autumn_web::extract::Form` or `Valid<T>` extraction: these reach the database
without passing one of the two round-trip extractors, so the byte still gets
there. It is classified rather than blanket-500'd — the resulting `AutumnError`
carries `422 Unprocessable Entity`, matching the status a validator would have
produced, and `autumn_web::error::is_nul_byte_violation` recognizes it if you
want to fold it back into a form:

```rust,ignore
if let Err(err) = repo.save(&new_post).await
    && is_nul_byte_violation(&err)
{
    changeset.add_error("body", NUL_CHARACTER_FIELD_ERROR);
    return Ok(render_rejected(changeset));
}
```

This is a backstop, not the mechanism: rejecting at the form boundary is what
gives the author something actionable. Two limits come with that. The 422 names
no field — nothing at the database boundary knows which one the byte came from,
so `errors` is empty unless you fold it in yourself as above. And classification
is by server message, anchored on the `: 0x00` the encoding rejection ends with;
a NUL smuggled into a `JSONB` column fails with a different message entirely
(`unsupported Unicode escape sequence`, SQLSTATE `22P05`) and is not classified.

### When you would rather clean than reject

On a column fed by paste-prone input, where dropping the byte silently is
better than refusing the write, use the normalizer:

```rust,ignore
#[model]
pub struct Profile {
    #[normalize(strip_nul)]
    pub bio: String,
}
```

Prefer rejecting wherever the author can see and fix the value — they get told
what happened instead of having their input quietly altered. `strip_nul` removes
only NUL; every other control character is storable and is left exactly as
written.

**It covers inserts and hooked updates only.** Per [Updates are not the same as
inserts](#updates-are-not-the-same-as-inserts), a repository with no hooks, or
one using `validate_on_update = fetch`, persists the raw patch and throws the
normalized draft away — so an `update` through either of those still sends the
NUL to Postgres and still fails the write, now as a 422 rather than a 500.

The message is a fixed English string. Unlike a scaffold's `common.error.taken`,
it is recorded inside the extractor, before any handler holds a `Locale`, so
there is nowhere to look up a translation. A localized app that needs a
localized message can rewrite it from the handler by matching
`NUL_CHARACTER_FIELD_ERROR` in `changeset.errors()`.

---

## CSRF is not optional, and not your job

`ChangesetForm` captures the CSRF token from the request extensions during
extraction, and `form_tag` emits the hidden input. In a POST handler that is the
whole story — no parameter, no wiring. It picks up the configured field *name*
from the extensions too, so a customized `security.csrf.form_field` is handled
for you.

GET handlers that render a blank form get neither, because there is no request
body to have captured them from. The token is the obvious half:

```rust,ignore
#[get("/todos/new")]
async fn new_todo(csrf: CsrfToken) -> Markup {
    let blank = ChangesetForm::blank(TodoForm::default(), csrf.token());
    render_form(&blank)
}
```

**That is correct only while you use the default field name.** `blank` is a
plain constructor with no access to configuration, so it hardcodes `_csrf`,
while `CsrfLayer` scans an incoming body for the *configured* name only. Set
`security.csrf.form_field` to anything else and this form renders happily and
`403`s on its first submission — a failure that appears at the far end of the
flow, on an app that "just changed a config value".

If your app configures the field name, extract it and pass it through. It costs
one parameter and is inert under the default:

```rust,ignore
// `CsrfFormField` is in the prelude, so this import is only for clarity.
use autumn_web::security::CsrfFormField;

#[get("/todos/new")]
async fn new_todo(csrf: CsrfToken, csrf_field: CsrfFormField) -> Markup {
    let blank = ChangesetForm::blank(TodoForm::default(), csrf.token())
        .with_csrf_field(csrf_field.0);
    render_form(&blank)
}
```

`NestedChangesetForm::blank` has the same constraint and the same
`with_csrf_field` remedy. So does any hand-written `<input type="hidden">`: name
it from `CsrfFormField`, not from the literal `_csrf`.

`ChangesetForm::without_csrf` exists for tests and for apps that have disabled
the CSRF layer. If CSRF is enabled and you use it in a real handler, the form
will render and every submission will be rejected.

---

## Inline validation with htmx, and the no-JavaScript path

`text_input_htmx` renders a field that re-validates itself as the user leaves
it. The field POSTs to a validation endpoint and htmx swaps the returned wrapper
in place with `outerHTML`:

```rust,ignore
fn title_field(form: &ChangesetForm<TodoForm>) -> Markup {
    form.text_input_htmx("title", "Title", "/todos/validate/title")
}

#[post("/todos/validate/title")]
async fn validate_title(form: ChangesetForm<TodoForm>) -> Markup {
    title_field(&form)     // the same partial, now carrying errors
}
```

The endpoint is three lines because it re-renders the *same partial* the full
page render uses. That is the property to preserve when you hand-roll a field
wrapper: one function, called from both places.

**The no-JavaScript fallback is automatic, and it is not a second code path.**
With htmx absent, the `hx-*` attributes are inert, the browser performs an
ordinary full-form POST, and the `#[post("/todos")]` handler returns 422 with
the same wrapper rendered inline. The user sees the same errors in the same
places. Nothing extra is written to make that work — it works because the
failure branch of the main handler re-renders the form, which is the pattern
this whole guide is built on.

Two rules keep it that way:

1. **Never make the validation endpoint the only way to see an error.** The full
   POST handler must render errors too.
2. **Keep the CSRF token in the form.** With JavaScript on, the framework's
   `autumn-htmx-csrf.js` shim can send the token as a header; with JavaScript
   off, the hidden `_csrf` input is the only thing that gets a plain form POST
   through. `form_tag` emits it, so this costs nothing — but a hand-rolled
   `<form>` that omits it breaks for exactly the visitors least able to work
   around it.

`examples/reddit-clone` builds its submit and edit forms this way. Neither
carries an `hx-*` attribute, so the no-JavaScript path *is* the only path there,
and unit tests in `src/routes/posts.rs` assert it: no htmx attributes, a
`method="post"` form, and the hidden `_csrf` input that gets a plain POST past
the CSRF layer.

---

## Accessible fields by construction

The form helpers emit labels and error wiring for you. When you build a field by
hand, use the typed primitives in `autumn_web::a11y` rather than raw markup:
`TextField`, `TextArea`, `Select`, `Checkbox`, and `FileField` **do not
implement `Render` until a label is attached**, so an unlabeled field is not
merely discouraged — it does not compile.

```rust,ignore
use autumn_web::a11y::TextField;

TextField::new("title")
    .label("Title")                       // required before it can render
    .value(form.field_value("title").unwrap_or_default())
    .required()                           // emits the HTML5 constraint
    .aria_invalid(!form.errors_for("title").is_empty())
    .described_by("title-error")          // ties the field to its message
```

Pair `described_by` with an element carrying that `id` and `role="alert"`, and a
screen-reader user hears the error when focus lands on the field. The
[accessibility guide](./accessibility.md) covers the full set, and
`autumn a11y verify` fails a build that regresses.

---

## Multi-row and multi-step forms

- **Repeated rows** — a `has_many` edited inline, with add/remove — are their own
  binding problem. See [nested forms](./nested-forms.md).
- **Multi-step flows** that must survive a refresh belong in a
  [wizard](./wizards.md).
- **At-most-once submission** (payments, anything with an external side effect)
  needs a [submit token](./submit-tokens.md), not a disabled button.

---

## Uploads

`Multipart` applies the app's `security.upload` policy — MIME allowlist and
per-file size caps — before your handler sees a byte. See
[storage](./storage.md) for where the bytes then go.

**`ChangesetForm` and file uploads do not compose.** It does decode
`multipart/form-data` bodies when the `multipart` feature is on, but
`decode_multipart` **skips file fields while consuming the stream** — so the
upload is silently discarded, and a second `Multipart` extractor cannot recover
it, because the body is already gone. A handler taking `ChangesetForm<T>` on a
form that contains a file input validates the text fields correctly and loses
the file with no error at all.

So pick one per route:

| The route needs | Use |
|---|---|
| validated text fields, re-rendered on failure | `ChangesetForm<T>` — and no file inputs on that form |
| a file upload | `Multipart`, validating the text fields yourself |

If you need both, split them: take the upload on its own route or its own
request, and keep the changeset form for the fields that have to round-trip.

---

## Testing forms

`TestResponse` has HTML assertions, so a form test reads like the form:

```rust,ignore
let res = app.post("/todos").form("title=").send().await;
res.assert_status(422)
   .assert_selector("input[name=title][aria-invalid=true]")
   .assert_text_contains("[role=alert]", "Title must be 1–255 characters");
```

Test the *failure* path first. The success path is one insert; the failure path
is the one with the round-trip, the ARIA wiring, and the CSRF token in it.

Building a `ChangesetForm` by hand in a unit test has one trap:
`ChangesetForm::without_csrf` and `ChangesetForm::blank` wrap the data in a
*fresh, error-free* changeset — they never call `validate`. Only
`IntoChangeset::into_changeset` runs the rules, so a test that wants to exercise
them has to go through it:

```rust,ignore
// Renders, but proves nothing about the rules — `into_valid()` always succeeds.
let form = ChangesetForm::without_csrf(SubmitPostForm { .. });

// Runs the rules.
let form = ChangesetForm::from_changeset(SubmitPostForm { .. }.into_changeset());
```

For normalization, assert on what was **stored**, not on what was accepted:

```rust,ignore
// "  Alice@Example.COM " — `+` is a space in a form-urlencoded body.
app.post("/users").form("email=++Alice%40Example.COM+").send().await;

let user = users.find_by_email("alice@example.com").await?.unwrap();
assert_eq!(user.email, "alice@example.com");
```

See the [testing guide](./testing.md).

---

## See also

- [Extractors](./extractors.md) — where `ChangesetForm` and `Valid` sit among
  the rest
- [Nested forms](./nested-forms.md) — `has_many` rows in one submission
- [Accessibility](./accessibility.md) — the typed primitives, and
  `autumn a11y verify`
- [Rich text](./rich-text.md) — accepting formatted text safely
- [Generators](./generators.md) — what `autumn generate scaffold` emits, and its
  no-JS fallbacks
- [Repositories](./repositories.md) — where model validation and normalization
  run
- [Submit tokens](./submit-tokens.md) — at-most-once form submission
- [Wizards](./wizards.md) — multi-step flows
