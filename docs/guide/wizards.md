# Multi-Step Form Wizards

Wizards are session-backed multi-step forms where each step is validated
independently before the user advances. The final "commit" step assembles
all validated step data and performs the real write.

Autumn ships a `WizardContext` runtime struct plus a generator that
scaffolds the full route file. Start with the generator and edit the
generated TODOs into real field definitions.

## Quick start

```bash
autumn generate wizard checkout shipping payment review
```

This emits three files:

```
src/wizards/checkout.rs        # step structs + GET/POST handlers + confirm/commit/cancel
src/wizards/mod.rs             # pub mod checkout;  (created or appended)
tests/checkout_wizard.rs       # ignored integration test skeletons
```

Mount the routes and add `mod wizards;` in `src/main.rs`:

```rust
mod wizards;

// inside autumn_web::app() builder:
.routes(routes![
    wizards::checkout::show_shipping,
    wizards::checkout::submit_shipping,
    wizards::checkout::show_payment,
    wizards::checkout::submit_payment,
    wizards::checkout::show_review,
    wizards::checkout::submit_review,
    wizards::checkout::show_confirm,
    wizards::checkout::commit,
    wizards::checkout::cancel,
])
```

Then fill in the `// TODO` sections in `src/wizards/checkout.rs`.

## The generated file layout

```
WIZARD_NAME   = "checkout"
STEPS         = ["shipping", "payment", "review"]
wizard_context(session) -> WizardContext

ShippingForm  { /* fill in */ }
PaymentForm   { /* fill in */ }
ReviewForm    { /* fill in */ }

GET  /checkout/shipping  → show_shipping    (guard → form)
POST /checkout/shipping  → submit_shipping  (validate → save → redirect)
GET  /checkout/payment   → show_payment
POST /checkout/payment   → submit_payment
GET  /checkout/review    → show_review
POST /checkout/review    → submit_review
GET  /checkout/confirm   → show_confirm     (summary + CSRF-protected submit button)
POST /checkout/commit    → commit           (assemble all steps → write → clear)
POST /checkout/cancel    → cancel           (clear → redirect)
```

`commit` and `cancel` are POST-only. Commit mutates state; cancel discards
accumulated session data. Using GET for either would make them vulnerable to
CSRF or prefetching.

## `WizardContext`

`WizardContext` is the runtime handle for a wizard session. The generator
produces a `wizard_context(session: Session) -> WizardContext` helper so
you never construct it directly in handler code.

```rust
use autumn_web::wizard::{WizardContext, wizard_progress};

pub fn wizard_context(session: Session) -> WizardContext {
    WizardContext::new(session, WIZARD_NAME, STEPS.iter().map(|s| s.to_string()))
}
```

### `guard_step`

Ensures the user cannot jump to step N without completing steps 1..N-1.
If a prerequisite step is missing, `guard_step` returns an `Err(redirect_url)`
pointing at the first incomplete step.

```rust
if let Err(redirect_url) = wizard.guard_step("payment", "/checkout").await {
    return Redirect::to(&redirect_url).into_response();
}
```

The second argument is the wizard URL prefix. `guard_step` appends
`/{first_incomplete_step}` to it, so passing `/checkout` redirects to
`/checkout/shipping`, `/checkout/payment`, etc. Passing the full step path
(e.g. `/checkout/shipping`) would produce `/checkout/shipping/shipping`.

### `save_step`

Serializes a step struct into the session under a namespaced key. The key
is `{wizard_name}/{step_name}` so multiple wizards in the same session cannot
collide.

```rust
wizard.save_step("shipping", &validated_data).await?;
```

`T` must implement `Serialize + Sync`.

### `step_data`

Deserializes a previously saved step. Returns `None` if the step hasn't been
saved yet or if deserialization fails.

```rust
let shipping: Option<ShippingForm> = wizard.step_data("shipping").await;
let data = shipping.unwrap_or_default(); // safe — generated structs derive Default
```

### `is_step_complete`

Returns `true` if the step has valid JSON saved in the session. Used
internally by `guard_step` and `first_incomplete_step`.

```rust
if wizard.is_step_complete("payment").await {
    // ...
}
```

### `first_incomplete_step`

Returns the name of the first step that is not yet complete, or `None` if
all steps are done. Used in `show_confirm` and `commit` to guard against
direct URL access.

```rust
if let Some(incomplete) = wizard.first_incomplete_step().await {
    return Redirect::to(&format!("/checkout/{incomplete}")).into_response();
}
```

### `clear`

Removes all step data for this wizard from the session. Call at the end of
`commit` (after the write succeeds) and in `cancel`.

```rust
wizard.clear().await;
```

### `wizard_progress`

Renders an accessible `<ol>` progress indicator that marks the active step.
Returns `Markup` so you can embed it directly in a Maud template.

```rust
html! {
    (wizard_progress(&wizard, "payment").await)
    // ... rest of the step form
}
```

Pass `"confirm"` as the step name on the confirm page.

## Step structs

Each step is a plain Rust struct that derives `Serialize`, `Deserialize`,
`Validate`, and `Default`. The generator emits empty structs with a `TODO`
comment; replace the comment with the fields that belong to that step.

```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize, Validate)]
pub struct ShippingForm {
    #[validate(length(min = 1))]
    pub name: String,
    pub address_line_1: String,
    pub city: String,
    pub postcode: String,
}
```

Validation attributes from the `validator` crate (`url`, `email`,
`length(min=N, max=N)`, `range`, custom validators) are all supported.
`ChangesetForm<T>` calls `T::validate()` in `into_valid()` and collects
field-level errors automatically.

## CSRF

The generated GET handlers extract `Option<CsrfToken>` and
`Option<CsrfFormField>` so the wizard works with and without the CSRF
middleware enabled. When the middleware is active, both extractors resolve to
`Some`; when absent, they resolve to `None` and the form omits the hidden
field rather than panicking.

The generated `show_confirm` handler renders the hidden CSRF input manually
(not via `ChangesetForm`) because the confirm page has two forms — commit and
cancel — each needing the token.

## Long-lived drafts (TurboTax-style persistence)

By default, wizard state lives in the session and expires with it (hours to
days). For wizards where the user needs to fill things in over several visits —
tax forms, lengthy onboarding flows, multi-day quote builders — you can persist
step data to a database using three helpers on `WizardContext`:

| Method | What it does |
|---|---|
| `draft_key(user_key)` | Returns a stable `"wizard:{name}:{user_key}"` string to use as a DB column value or cache key |
| `export_draft()` | Serializes all saved steps to a single `serde_json::Value` object keyed by step name |
| `restore_draft(json)` | Loads from that object back into the session; steps already in the session are left unchanged |

The framework provides the serialization format and the session↔draft bridge.
You provide the storage backend and the auth context.

### Saving a draft

Call `export_draft` whenever you want to snapshot progress — on each step
submission, on a "save and exit" button, or in the commit handler if you want
to preserve the draft until the user explicitly discards it:

```rust
#[post("/checkout/shipping")]
async fn submit_shipping(
    session: Session,
    current_user: CurrentUser,
    form: ChangesetForm<ShippingForm>,
    db: Db,
) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());
    // ... validate and save to session as usual ...
    wizard.save_step("shipping", &data).await?;

    // Persist draft so the user can pick up where they left off.
    let key   = wizard.draft_key(&current_user.id.to_string());
    let draft = wizard.export_draft().await.to_string();
    db.upsert_wizard_draft(&key, &draft).await?;

    Redirect::to("/checkout/payment").into_response()
}
```

### Restoring a draft

Restore on the first step's GET handler, before re-populating the form from
the session. `restore_draft` only writes steps that are missing from the
current session — if the user has already filled a step in this session, their
current work is preserved:

```rust
#[get("/checkout/shipping")]
async fn show_shipping(
    session: Session,
    current_user: CurrentUser,
    db: Db,
) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());

    // Restore a saved draft when the session is empty.
    if wizard.first_incomplete_step().await.as_deref() == Some("shipping") {
        let key = wizard.draft_key(&current_user.id.to_string());
        if let Ok(Some(json)) = db.load_wizard_draft(&key).await {
            if let Ok(draft) = serde_json::from_str(&json) {
                wizard.restore_draft(&draft).await;
            }
        }
    }

    // The rest of the handler is unchanged — step_data now returns the
    // restored values if the session was empty.
    let data: ShippingForm = wizard.step_data("shipping").await.unwrap_or_default();
    // ...
}
```

### Cleaning up

Delete the draft row after a successful commit so stale data doesn't
pre-populate the next run:

```rust
#[post("/checkout/commit")]
async fn commit(session: Session, current_user: CurrentUser, db: Db) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());
    // ... assemble data and write ...
    wizard.clear().await;
    db.delete_wizard_draft(&wizard.draft_key(&current_user.id.to_string())).await?;
    Redirect::to("/orders").into_response()
}
```

A minimal draft table looks like:

```sql
CREATE TABLE wizard_drafts (
    key        TEXT PRIMARY KEY,          -- wizard.draft_key(&user_id.to_string())
    data       JSONB NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT NOW()
);
```

## Session storage

Wizard state lives in the session store, namespaced by wizard name. If the
session expires, all step data is lost and the user must restart from step 1
(the guards redirect there automatically). This is intentional: partial
wizard data that outlives its session would be stale.

For wizards where step data is expensive to re-enter, consider increasing the
session TTL in `autumn.toml`. The key is `session.max_age_secs` — the maximum
age of the session cookie in seconds, defaulting to `86400` (24 hours):

```toml
# autumn.toml
[session]
max_age_secs = 259200  # 3 days, up from the 86400 default
```

The full `[session]` table — backend, cookie name, `secure`, `same_site` — is
documented under
[Cookie and backend configuration](authentication.md#cookie-and-backend-configuration).

## Worked example

See [`examples/bookmarks/src/wizards/add_bookmark.rs`](../../examples/bookmarks/src/wizards/add_bookmark.rs)
for a fully filled-in wizard that saves a `Bookmark` through the repository.
The generator skeleton is visible in the commit that added `src/wizards/checkout.rs`;
the diff between skeleton and final is exactly the work left after generation.

## Validation and the 422 pattern

Submit handlers follow the same 422/re-render pattern as scaffold routes:

```rust
match form.into_valid() {
    Ok(data) => {
        wizard.save_step("shipping", &data).await?;
        Redirect::to("/checkout/payment").into_response()
    }
    Err(form) => (
        StatusCode::UNPROCESSABLE_ENTITY,
        html! {
            (wizard_progress(&wizard, "shipping").await)
            (form.form_tag("/checkout/shipping", "post", html! {
                // same fields as the GET handler
            }))
        },
    ).into_response(),
}
```

`ChangesetForm::into_valid()` re-packages the form with errors attached so
`form.text_input("name", "")` automatically renders error messages alongside
the relevant field.

## Name constraints

The wizard name and all step names must be valid Rust identifiers:

- Only ASCII letters, digits, and underscores.
- Must start with an ASCII letter or `_`.
- Must not be a Rust keyword (`type`, `mod`, `crate`, …).
- Step names `confirm`, `commit`, and `cancel` are reserved — the generator
  emits handlers with those names and collisions would produce a compile error.
- Duplicate step names (after snake\_case normalization) are rejected.

Use underscores for multi-word names: `add_bookmark`, `shipping_address`.
The generator converts PascalCase step names to snake\_case automatically
(`ShippingAddress` → `shipping_address`), but the CLI rejects hyphens.
