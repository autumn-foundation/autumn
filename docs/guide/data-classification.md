# Data Classification

Autumn's other protections for sensitive data are *name*-based and run at
runtime. The [log scrubber](logging-pii.md) matches a key denylist
(`password`, `ssn`, `credit_card`, …), the HTTP client redacts three header
names, and the GDPR registry tracks erasure by table-name strings. Every one of
them is one rename away from silently letting personal data through, because the
classification lives in the developer's memory rather than in a type.

`#[classified]` moves one tier — **personal data** — into the type system, and
gates one sink — the `Json` response — on it. A leak stops being
a production incident and becomes a build failure.

```rust,ignore
#[autumn_web::model(table = "customers")]
pub struct Customer {
    pub id: i32,
    pub name: String,
    /// Personal data: released only through a declared boundary.
    #[classified]
    pub email: String,
}
```

That one attribute changes three things:

1. the generated field is `Classified<String, CustomerEmailClassified>`, not `String`;
2. `Customer` no longer implements `Serialize`, so it cannot be handed to a sink;
3. the column and the boundaries that release it appear in the
   **data-flow manifest** `autumn data-flow` emits.

## What stops compiling

Handing the whole record to the JSON sink:

```rust,ignore
async fn show(Path(id): Path<i32>) -> AutumnResult<impl IntoResponse> {
    let customer = CustomerRepository::find(id).await?;
    Ok(Json(customer)) // ❌
}
```

```text
error[E0277]: `Customer` cannot be released into autumn's `Json` response sink
  = note: an autumn `#[model]` with `#[classified]` columns has no `Serialize`
          impl: the classification is carried by the type, so the whole record
          cannot reach a sink
  = note: release each classified column at a declared boundary and respond with
          the released view
```

Lifting the column into a response DTO — the leak a rename or a new endpoint
would otherwise reopen:

```rust,ignore
#[derive(Serialize)]
struct SupportView { email: String }

SupportView { email: customer.email } // ❌ expected `String`,
                                      //    found `Classified<String, CustomerEmailClassified>`
```

And putting the wrapper itself in a serializable struct:

```text
error[E0277]: `CustomerEmailClassified` is classified personal data and cannot be
              serialized into a sink
  = note: declare a boundary with `autumn_web::declassify!` and release the value
          first: `value.declassify(&YOUR_BOUNDARY)`
```

The wrapper has no `Serialize`, no `Display`, no `Deref`, no `Hash` and no
`into_inner`, so no expression hands the value — or a serializable view of it —
to a sink. That is the guarantee: it is a property of the type, not a list of
field names.

Two pieces of surface survive on purpose, and both are boolean: `PartialEq`,
which the generated repository code needs to correlate records and to compute an
`UpdateDraft`'s changed fields, and the `validator` rules below, which answer
`true`/`false` over the real value. An author who writes a loop around
`==` or `validate_regex` can narrow a value down without a boundary. That is
accepted: the threat model here is **drift detection, not an author attacking
their own application** — the same posture
[the security posture manifest](security-posture-manifest.md) states — and such
an author could simply declare a boundary instead. What is closed is every path
that leaks the value *by accident*.

## Declassifying at a boundary

A release is *declared*, not incidental. `declassify!` names the column, the
sink, the purpose and the reason:

```rust,ignore
autumn_web::declassify! {
    /// Support agents need the customer's email address to answer the ticket.
    pub SUPPORT_LOOKUP: CustomerEmailClassified => JsonResponse,
    purpose = "support_lookup",
    reason = "Support agents need the email address to answer the ticket.",
}
```

`CustomerEmailClassified` is the marker `#[model]` generated for the column
(`{Model}{Column}Classified`). It types the boundary, so one column's approved purpose
cannot release another's:

```rust,ignore
customer.phone.declassify(&SUPPORT_LOOKUP) // ❌ expected Declassification<CustomerPhoneClassified>
```

Both `purpose` and `reason` must be non-blank string literals — a boundary whose
justification is three spaces is the one nobody can review, so the macro rejects
it at compile time.

The approved path:

```rust,ignore
#[derive(Serialize)]
struct SupportView {
    name: String,
    email: String,
}

async fn show(Path(id): Path<i32>) -> AutumnResult<impl IntoResponse> {
    let customer = CustomerRepository::find(id).await?;
    Ok(Json(SupportView {
        name: customer.name,
        email: customer.email.declassify(&SUPPORT_LOOKUP),
    }))
}
```

`declassify` takes the value **by move**, so a release is a single event rather
than a permanent widening. When the record is borrowed, `declassify_cloned`
releases a copy and records identically.

## The auditable record

Every release emits a `tracing` event on the `autumn::declassification` target
carrying the model, field, tier, purpose, sink and reason. It deliberately does
**not** carry the released value — recording the plaintext would reintroduce the
leak the type system just closed.

Applications that persist the record install an observer:

```rust,ignore
let _guard = autumn_web::classify::capture_releases(|record| {
    tracing::warn!(?record, "personal data released");
});
```

The guard removes the observer when it drops.

## The data-flow manifest

```bash
autumn data-flow --manifest target/autumn/data-flow-manifest.json
```

A bare `cargo build` does not write the manifest, and cannot: reachability is a
whole-app fact that only exists once everything is linked. `autumn data-flow`
builds the app, runs it in dump mode, and emits one row per classified column
listing every sink it is proven reachable to:

```json
{
  "schema_version": 1,
  "gated_sinks": ["json_response"],
  "fields": [
    {
      "model": "Customer",
      "model_path": "shop::models::customer::Customer",
      "field": "email",
      "classification": "personal_data",
      "reachable_sinks": [
        {
          "sink": "json_response",
          "purpose": "support_lookup",
          "reason": "Support agents need the email address to answer the ticket."
        }
      ]
    },
    {
      "model": "Order",
      "model_path": "shop::models::order::Order",
      "field": "card_number",
      "classification": "personal_data",
      "reachable_sinks": []
    }
  ]
}
```

Rows are keyed on `model_path`, not the display name: two crates can each define
a `Customer` with a classified `email`, and merging them into one row would hide
one model's column behind the other's. The report prints the short name unless
two rows share it.

An **empty `reachable_sinks` means no leak**: nothing in the binary declares a
boundary for that column, and a boundary is the only way to a gated sink.

The reachable set is an *over*-approximation in the safe direction: a row lists
every sink a declared boundary could release the column to, not the call sites
that actually do. A boundary declared and never used still shows up. That is
deliberate — a manifest that under-reports a reachable sink would be worse than
no manifest.

Why it runs the binary rather than parsing sources: reachability is a whole-app
fact. A column declared in one crate can be released by a boundary declared in
another, or in a plugin the app merely depends on, and link-time collection is
the only place all of those registrations exist together. This is the same shape
as [`autumn routes audit`](security-posture-manifest.md) and
[`autumn cache audit`](cache-coherence.md).

There is no gate in `autumn data-flow`, on purpose: the compiler is the gate.
What the manifest gives you is a **diff**. Commit it and check it in CI, so a new
release edge has to be approved rather than merged silently:

```bash
autumn data-flow --check data-flow-manifest.json
```

```text
✗ The data-flow manifest has drifted from the committed copy:
  ~ Customer.email reaches json_response for support_lookup
      -> json_response for marketing_export, json_response for support_lookup
```

## What `#[classified]` does not change

- **The name-based redaction still runs.** `log/filter.rs` and the HTTP client's
  header denylist are untouched; classification composes with them rather than
  replacing them.
- **The write path keeps the plain type.** `NewCustomer`, `UpdateCustomer` and
  the changeset hold a `String`, because taking personal data *in* is what an
  application does; forms, `#[validate]` rules and deserialization are unchanged.
  Those structs simply never serialize the column.
- **`Debug` is redacted** on the model and on the write structs — the value
  renders as `<classified>`, so it cannot reach a panic message or an error page.
- **`#[validate]` still runs** on the classified column. The wrapper forwards
  `validator`'s string rules (`email`, `length`, `contains`, `does_not_contain`,
  `url`, `regex`, `ip`) to the inner value, answering each with a `bool`.
  `ValidateEmail::as_email_string` and `ValidateUrl::as_url_string` — the two
  rules whose trait shape hands the *value* back rather than a verdict — return
  `None` on the wrapper, so the rule still evaluates on the real value while
  nothing can read it back out.
- **The database sees a plain `Text` column.** Classification says where a value
  may *go*, not how it is stored. For at-rest protection, that is
  [`#[encrypted]`](attribute-encryption.md). The column round-trips through
  `ClassifiedText<CustomerEmailClassified>`, which carries the same field marker
  as the value: an `F`-erasing column type would have been a way to convert a
  value in as one classified column and back out as another, releasing it through
  the wrong boundary and recording it against the wrong column.
- **The column drops out of the client-controlled list surface.** A repository's
  `list()` allowlists every scalar column for `?filter[col]=` and `?sort=col`. A
  classified column is excluded from both: the filter would be an
  unauthenticated equality oracle over the personal data and the sort would order
  the result set by it, neither through a boundary nor visible in the manifest.
  Filtering or sorting on it is simply ignored, exactly as for any unlisted key.

## Restrictions in this slice

`#[classified]` applies to non-null `String` columns and rejects, with a
diagnostic that says why:

| Rejected with | Because |
|---|---|
| a non-`String` type | v1 has one Diesel column representation |
| `#[encrypted]` | both own the column's Diesel representation |
| `#[searchable]` | the search vector is not a gated sink |
| `#[normalize]` | normalizing needs the plaintext with no boundary to record it |
| `#[translatable]` | a per-locale container is a document, not a value |
| `#[id]`, `#[lock_version]`, `#[position]`, `#[state_machine]` | framework-managed columns |
| `#[serde(rename)]` / `#[serde(rename_all)]` | the manifest is keyed on the Rust name |
| `#[serde(with/serialize_with/deserialize_with)]` | the adapter is written against the declared type, and a `serialize_with` serializes the withheld value |
| the `tenant_id` column | the tenancy codegen reads it directly as an isolation key |

Two further limitations are worth knowing before you classify a column:

- A `#[repository]` that records **version history** or is **ledgered**
  serializes the whole model into its snapshot, so it does not compile against a
  classified model (the snapshot needs the `Serialize` impl the classification
  withholds). A repository with **durable commit hooks** compiles, but building
  the payload returns an error at runtime naming the model and the columns.
  Both of those payloads are second, unclassified copies of the record — gating
  them is a follow-up slice. For now, do not classify a column on a versioned,
  ledgered, or durable-commit-hook model.
- Only the JSON response sink is gated. Log/tracing events, outbound HTTP bodies
  and analytics emission are follow-up slices — for those, the runtime
  name-based scrubbing is still what protects you.
- Anything else in the framework that needs the model to serialize needs a
  released view instead, and says so at build time: `form_for::<Customer>` and
  `#[repository(..., api = "/customers")]` are the two you are most likely to
  meet. The generated REST handlers assert the sink bound directly, so the
  failure names the model and points here rather than surfacing as an axum
  `Handler` bound.
- A `#[repository(..., cursor_key = <classified column>)]` does not compile: a
  cursor is echoed back to the client verbatim, which is a release with no
  boundary. Page on a non-classified column.

## Adding a second tier or a second sink

Both are additive by construction. A tier is a variant of
`classify::Classification` plus a spelling accepted by `#[classified(...)]`. A
sink is a variant of `classify::Sink` plus a marker trait alongside
`classify::JsonSink`, bounded on the sink's entry point the same way
`Json`'s `IntoResponse` is.
