# The Agent Authority Envelope

`#[agent_operable(grant = ...)]` makes an agent-callable handler's **blast
radius** a build-time constant. Declare what the action is allowed to do, and
the build fails if the handler can do anything else — on every branch, whether
or not a test exercises it.

```rust
use autumn_web::prelude::*;

authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        outbound: ["https://api.stripe.com/v1/refunds"],
        jobs: [NotifyFinanceJob],
        rate: "10/min",
        spend: "500.00 USD",
        reversibility: compensable,
    }
}

#[post("/api/refunds")]
#[api_doc(mcp, summary = "Draft a refund")]
#[agent_operable(grant = RefundDrafter)]
pub async fn draft_refund(
    repo: PgRefundRepository,
    client: Client,
    Json(body): Json<NewRefund>,
) -> AutumnResult<Json<Refund>> {
    // allowed: writes: [Refund]
    let refund = repo.create(&body).await?;
    // allowed: outbound: ["https://api.stripe.com/v1/refunds"]
    client.post("https://api.stripe.com/v1/refunds").json(&refund).send().await?;
    // allowed: jobs: [NotifyFinanceJob]
    NotifyFinanceJob::enqueue(NotifyFinanceArgs { refund_id: refund.id }).await?;
    Ok(Json(refund))
}
```

An [MCP tool](mcp.md) is an action an autonomous agent can take with no human
in the loop. The tool's *description* says what it is for; nothing until now
said what it is **allowed to do**. That fact — which models it writes, whether
it may erase a table, whether it may leave the tenant it was invoked for, which
hosts it may reach, which jobs it may start, and how hard the whole thing is to
undo — lived in the reviewer's head, and drifted the first time someone added a
line to the body.

This turns it into a declared value the compiler checks. `authority_grant!`
declares a named `const Grant`. `#[agent_operable]` walks the handler body,
derives the effect set it can prove, and emits one
`const _: () = assert!(GRANT.allows_…(…), "…")` per proved effect, **respanned
onto the offending call**. A write the grant does not list is a compile error
at the write. Because the check is const-evaluated against a linked `Grant`
rather than against tokens, it holds when the grant is declared in another
crate.

## How this differs from the runtime guardrails

Autumn already has ways to constrain what a request may do, and every one of
them is decided while the request runs:

| Tool | When it decides | Coverage |
|---|---|---|
| [`#[secured]` / `#[authorize]`](authorization.md) | per request | *who* may call it, never *what the call may do* |
| [Rate limiting](rate-limiting.md) and throttling | per request | call volume, not blast radius |
| [Audit logging](audit-logging.md) | after the fact | what happened, once it has happened |
| **`#[agent_operable(grant = G)]`** | **`cargo build`** | **every effect on every reachable path, tested or not** |

They compose rather than replace each other. A grant is a statement about the
code; a policy is a statement about the caller. Nothing about `#[agent_operable]`
runs during a request — the *envelope* is compile-time, and the runtime half of
this feature is the audit record (below), not an enforcement point.

---

## The worked example: an agent-operable handler that grows a new effect

### Red build

The support agent's refund tool is granted `writes: [Refund]`. Someone adds a
payout alongside the refund — a reasonable-looking two-line change, and a
materially different authority:

```rust
authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = RefundDrafter)]
async fn draft_payout(payouts: PgPayoutRepository) -> Result<Payout, ()> {
    payouts.create(&NewPayout).await
}
```

`cargo build` fails:

```text
error[E0080]: evaluation panicked: agent authority: `draft_payout` writes
              `Payout`, which grant `RefundDrafter` does not allow.

              Add `Payout` to the grant's `writes: [...]`, or move the effect
              out of the agent-operable handler.
              See docs/guide/agent-authority.md.
  --> tests/compile-fail/agent_authority_unlisted_write.rs:32:13
   |
32 |     payouts.create(&NewPayout).await
   |             ^^^^^^ evaluation of `_` failed here
```

Both sides are named — what the handler does, and which grant refused it — and
the span is the offending call, not the handler. A grant violation is one call
site; "handler `draft_payout` violates its grant" on a 200-line body would be
unusable.

Note what did *not* have to happen: no test ran, no tool was called, and no
agent was pointed at the deployment.

### Green build

Either widen the envelope deliberately — a diff a reviewer can see, and a
manifest row that changes:

```rust
authority_grant! {
    pub RefundDrafter {
        writes: [Refund, Payout],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}
```

…or keep the envelope and move the write out of the agent-operable handler.
The point of the gate is that widening is a decision, not a side effect.

Both halves are compiled in CI as trybuild fixtures — see
`autumn/tests/compile-fail/agent_authority_unlisted_write.rs` and
`autumn/tests/compile-pass/agent_authority_valid.rs`.

---

## The grant grammar

```rust
authority_grant! {
    /// Doc comments are carried onto the generated `const`.
    pub RefundDrafter {
        writes: [Refund, RefundNote],
        unbounded_writes: [],
        tenant_scope: scoped,
        outbound: ["https://api.stripe.com/v1/refunds", "alias:stripe"],
        webhooks: ["refund.drafted"],
        jobs: [NotifyFinanceJob, "audit_export"],
        rate: "10/min",
        spend: "500.00 USD",
        reversibility: compensable,
    }
}
```

Keys may appear in any order. Every key except `reversibility` is optional.

| Key | Accepts | Default | Checked how |
|---|---|---|---|
| `writes` | model idents or `"table"` strings | `[]` | exact match against the effect's subject |
| `unbounded_writes` | same | `[]` | exact match. The subsumption runs one way only: an entry here also permits the bounded write (an action allowed to delete the whole table may delete one row of it), but listing a model under `writes` **never** grants the unbounded form — one row and all of them are different authorities |
| `tenant_scope` | `scoped` \| `cross_tenant` \| `none` | `scoped` | any cross-tenant effect fails under `scoped`; `none` declares a single-tenant app, where the dimension does not apply |
| `outbound` | absolute URL prefixes, and `"alias:<name>"` for a named client | `[]` | prefix match that must end at a path boundary (`/`, `?`, `#`, end), so `…/v1/refunds` never authorises `…/v1/refunds-evil` |
| `webhooks` | topic strings | `[]` | exact match on the topic — a fan-out delivers to subscriber-supplied URLs, so it can only be granted by topic |
| `jobs` | job type idents or `"job_name"` strings | `[]` | exact match |
| `rate` | `"<n>/<s\|sec\|min\|hour\|day>"` | none | **grammar** validated at compile time; **declared only**, nothing meters it |
| `spend` | `"<decimal> <ISO 4217>"`, e.g. `"500.00 USD"` | none | **grammar** validated at compile time; **declared only** |
| `reversibility` | `reversible` \| `compensable` \| `irreversible` | **required** | must sit at or above the floor the proved effects impose (below) |

Anything the declaration cannot defend is refused at the declaration rather
than recorded: an unknown or duplicated key, a missing `reversibility`, a
`rate`/`spend` no reader could interpret. A key the macro silently dropped
would be an allowance the author believes they declared and the manifest never
carries.

Every declared grant reaches the manifest, including one nothing uses — a dead
envelope is either dead code or a handler that lost its annotation, and both
are worth seeing.

## What counts as an effect

Effects are a **set**, not a count: branches and loops are unioned, and at every
join the *unsafe* side wins (unbounded beats bounded, cross-tenant beats
scoped). Reads are not effects and are never mentioned — a read-only tool
proves an empty effect set rather than an unprovable one.

| Effect | Recognised as | Subject | Provenance |
|---|---|---|---|
| **Write** | `save`, `insert`, `create`, `update`, `upsert`, `delete_by_id`, `save_many`, `update_many`, `restore`, `soft_delete`, `transition_*`, … on a handle named in the signature; `diesel::insert_into(t)…execute(conn)` | the model (or table) | `type_resolved` when the handle is a generated `Pg…Repository` (its `__AUTUMN_MODEL_IDENT` is read by const-eval, so a rename cannot desync it); `syntactic` otherwise |
| **UnboundedWrite** | `delete_all`, `update_all`, `truncate`, `purge_all`, `delete_where`, `update_where`, and `delete_by_<x>` / `update_by_<x>` for an `x` that is not the id; an unfiltered `diesel::delete`/`diesel::update` reaching an executor | the model or table | as above |
| **CrossTenant** | `across_tenants`, `unscoped`, `preload_across_tenants`, `each_shard`, `fan_out_shards` — **and** `for_tenant(..)`, `with_tenant(..)`, `db_for`, `db_on`, `read_for`, `for_shard`, `from_shard`, `with_shard`, whose argument may itself be agent-chosen; **and** a raw diesel `SELECT`/`UPDATE`/`DELETE` on a `Db` or connection handle, which carries no repository tenant predicate | the method name, or `raw_query:<table>` | `syntactic` |
| **Outbound** | `get`/`post`/`put`/`patch`/`delete`/`head`/`request`/`get_ssrf_safe` on an outbound root: a `Client`/`HttpClient` parameter, **any** associated function on `Client` (`Client::new()`, `from_state(..)`, `from_config(..)`, `named(..)`, `builder()…build()`), or `<expr>.http_client()`. `request(method, url)` is read from whichever argument is the URL | the absolute literal URL, or `alias:<name>` | `syntactic` for an absolute literal (or `concat!` of literals); `declared` for an alias, whose host comes from config |
| **Webhook** | `.dispatch(&state, "<topic>", ..)` on `state.webhook_outbound()` or a `WebhookOutboundManager` | `<topic>` | `syntactic` |
| **Job** | `enqueue`, `enqueue_in`, `enqueue_at`, `enqueue_tracked*`, `enqueue_on_conn`, `enqueue_in_tx`, `enqueue_after_commit`, … — as a method **or** a free function, with any receiver | the literal job name, or the job type's own registered name | `syntactic` for a literal; `type_resolved` for a `#[job]` type, whose `__AUTUMN_JOB_NAME` const-eval reads, so renaming the job cannot desync the grant |

Subjects are recorded bare — a webhook effect's subject is the topic itself,
not a prefixed `webhook:<topic>` — because the `kind` column beside it already
says which dimension the subject belongs to, in the manifest row and in the
audit metadata alike. That also means a proved dispatch and one declared with
`#[agent_effect(webhooks("…"))]` record the same subject, so a `--check` diff
never reads as a changed topic.

Three of those dimensions have no signature chokepoint: `job::enqueue` reaches
a global client, `Client::new()` is constructible from nothing, and a webhook
dispatch fans out to URLs nobody in this codebase wrote down. So the analysis
runs a second, fail-closed pass over those verbs wherever they appear, and
requires their subject to be a literal. The default for them is *unprovable*,
never zero.

The analysis also sees through the shapes real handler code is written in: a
handle behind a wrapper (`Arc`, `Box`, `Option`, `Vec`, `State<_>`,
`Extension<_>`, …), a UFCS call (`Repository::save(&repo, …)`), a binding
introduced by a `match` / `if let` / `while let` pattern, a `.await?` chain,
and a handle captured by an `Option`/`Result` combinator closure are all
tracked exactly like the plain form.

### Outbound precedence

Three rules decide what an outbound call proves, in this order:

1. **An absolute literal wins.** `client.named("stripe").post("https://evil.example/x")`
   records `https://evil.example/x`, not `alias:stripe` — the URL at the call
   site is what the request actually reaches, and reading the alias first would
   let an alias launder any host past the allowlist.
2. **An alias only names a *relative* literal.** `client.named("stripe").post("/v1/refunds")`
   records `alias:stripe`, because the host comes from the client's configured
   base URL. A non-literal `named(..)` is refused: an alias chosen at runtime
   names no host either.
3. **A relative literal with no alias is refused.** `client.post("/v1/refunds")`
   takes its host from a base URL nothing at the call site pins down, so it
   proves nothing — pass an absolute URL or route it through an alias.

An absolute literal is refused outright when the URL itself defeats the
grant's prefix check: `user:pass@host` userinfo (which would be copied verbatim
into the committed manifest and every audit row), a `%2e` percent-encoded dot,
or a `..` path segment that resolves above the prefix the grant allows. Spell
the URL as the host and path it actually reaches.

### Boundedness is tracked on the binding

A diesel write builder carries its boundedness through `let`s and branches:

```rust
let q = diesel::update(refunds::table);                    // unbounded so far
let q = if scoped { q.filter(refunds::id.eq(id)) } else { q };  // one arm is not
q.set(refunds::state.eq("void")).execute(&mut *db).await?; // → UnboundedWrite
```

A conditional produces a bounded builder only if **every** arm is bounded.

### Tenant scope, and raw queries

Under the default `tenant_scope: scoped`, a repository call carries the tenant
predicate the repository codegen applies, so it is scoped by construction. A
raw diesel query handed a `Db` or connection handle carries no such predicate.
That is not treated as unreadable — it is a **proved cross-tenant effect**,
recorded with the subject `raw_query:<table>` and checked against the grant
like any other:

```rust
let all: Vec<Refund> = refunds::table.load(&mut *db).await?;
```

Under `tenant_scope: scoped` that fails the build:

```text
error[E0080]: evaluation panicked: agent authority: `list_refunds` runs a raw
              query (`load`) that carries no repository tenant predicate, which
              grant `RefundDrafter` does not allow (its `tenant_scope` is not
              `cross_tenant`).

              Route it through a tenant-scoped repository, declare the
              statement scoped with `#[agent_effect(scoped, reason = "...")]`,
              or declare `tenant_scope: cross_tenant` on the grant.
              See docs/guide/agent-authority.md.
```

Three ways out, in the order you should reach for them:

1. **Route it through a tenant-scoped repository** — the predicate then exists,
   and the effect disappears rather than being excused.
2. **Declare the statement scoped**, when it is scoped by something the
   analysis cannot see (a tenant-partitioned view, a shard-local table):

   ```rust
   #[agent_effect(scoped, reason = "the view is already tenant-partitioned")]
   let all: Vec<Refund> = refunds_scoped::table.load(&mut *db).await?;
   ```

3. **Declare `tenant_scope: cross_tenant`** (or `none` for a single-tenant
   app) on the grant, when leaving the tenant is the point. Under either, a raw
   query needs no annotation at all: the effect is still recorded, and the
   grant allows it.

`INSERT` is exempt — `diesel::insert_into(refunds::table)` has no `WHERE` to
scope, so it is analysed as a bounded write and nothing else. `SELECT`,
`UPDATE` and `DELETE` all carry the effect.

### Jobs inside a transaction

A plain `enqueue` / `enqueue_in` / `enqueue_at` inside a `tx` / `tx_with` /
`transaction` callback fires even when the transaction rolls back, so it is
refused with the fix named:

```rust
db.tx(move |conn| async move {
    diesel::insert_into(refunds::table).values(&body).execute(conn).await?;
    autumn_web::job::enqueue_on_conn("notify_finance", args, conn).await?;   // ✓
    Ok(())
}.scope_boxed()).await?;
```

### Detached effects

`tokio::spawn`, `spawn_blocking`, `spawn_local` and `JoinSet::spawn` anywhere
in the body are refused: an agent-operable action must not detach effects from
the request it is audited under. The work outlives the correlation id that
would have recorded it. Move the work into a job (which the envelope names) or
declare the statement with `#[agent_effect(...)]` if it genuinely performs no
effect.

## The reversibility floor

`reversibility` is a claim about how hard the action is to take back. There are
three answers, and every grant picks one:

| Value | What it claims |
|---|---|
| `reversible` | Undoable by writing the previous rows back; nothing left the process. |
| `compensable` | Undoable only by a compensating action — a refund for a charge, a retraction for a webhook. |
| `irreversible` | Not undoable at all. |

The proved effects then put a floor under that claim:

| Proved effects | Lowest `reversibility` the grant may declare |
|---|---|
| nothing, bounded `Write`s, `CrossTenant` reads, or both | `reversible` |
| any `UnboundedWrite`, `Outbound` (non-`GET`/`HEAD`), `Webhook`, or `Job` | `compensable` |
| any `#[agent_effect(none, …)]` site | `compensable` — nothing *proved* there is nothing to undo |

Declaring *above* the floor is always allowed — an `irreversible` grant on a
handler that only writes one row is a legitimate, conservative claim. Declaring
below it is a compile error, checked in const-eval like everything else. The
floor exists because none of the effects in the second row can be undone by
writing the previous rows back: a webhook has been delivered, a job has been
picked up, a charge has left the building.

`CrossTenant` is deliberately **not** in that row. Reading another tenant's
rows is a serious authority question — which is why `tenant_scope` gates it —
but it is not an irreversible one, and a raw `SELECT` is the commonest way the
effect appears. A cross-tenant *write* still carries the floor its own write
effect imposes, so nothing is loosened: `across_tenants().delete_all()` is an
`UnboundedWrite` and floors at `compensable` on that basis.

The declared reversibility also feeds MCP's `destructiveHint`, but only ever
upward: the HTTP verb is a **floor**, not a guess to be overridden. A grant can
*raise* a `POST` or `PATCH` that the verb alone says nothing about; it cannot
clear the warning a `DELETE` already carries. `reversible` means the compiler
proved the effect set is bounded writes only — it does not mean the application
can put the row back, and nothing checks for soft-delete or versioning. Since
an MCP client skips its confirmation prompt on `destructiveHint: false`, one
unproved adjective is not allowed to trade away a real signal. See
[Exposing Your API as MCP Tools](mcp.md).

## What the analysis refuses to guess

Anything the analysis cannot read is **reported**, never assumed effect-free.
A false positive costs one annotation; a false negative ships an unbounded
authority under a manifest that says otherwise.

- **A helper handed a tracked handle** — `issue_refund(&mut db, id)`, and
  equally the associated form `Billing::wipe(&repo)` or
  `<Billing as Janitor>::wipe(&repo)`. Its body is another function; the macro
  sees only the call. An uppercase path segment is a *shape*, not evidence
  that the callee is framework surface — a generated static finder
  (`Post::find_published(&mut db)`) is a read, and is spelled exactly like a
  helper that erases the table, so both are refused and the read is discharged
  with `#[agent_effect(none, reason = "...")]`.
- **A non-inert macro naming a handle** — `refund_pipeline!(repo, 7)`. Note
  that `format!` and `vec!` are *not* inert here even though they are for
  [query budgets](query-budgets.md): `vec!` can carry a handle and `format!`
  launders URLs. Only pure logging, assertion and template macros are inert.
- **A body-local `macro_rules!` naming a handle** — it is hygienic at its
  *definition* site, so it performs that handle's effects wherever it is
  invoked, and the invocation (`wipe!()`) mentions nothing to see.
- **A URL that is not a literal** — a `format!`-built URL or
  `with_base_url(x)` proves nothing about the host that will be reached; nor
  does a relative literal with no alias, or a `named(..)` alias chosen at
  runtime.
- **A non-literal job name or webhook topic** — the subject is the whole claim,
  and it is read from the argument that *holds* it: the job name is the first
  argument of every `autumn_web::job` enqueue API, and the topic is the second
  argument of `dispatch(&state, topic, payload)`. A literal somewhere else in
  the call is not the subject. `enqueue_after_commit(&chosen, "notify_finance")`
  is refused, not recorded as the job `notify_finance`.
- **A `dyn Trait`, `impl Trait` or fn-generic parameter** — any method call on
  one is opaque. (`Json<T>`, `Path<T>`, `Query<T>`, `Form<T>` and friends are
  never handles, so they never trigger this.)
- **A detached task** — `tokio::spawn` and its relatives, as above.
- **An awaited call the analysis cannot read** — `start_finance_job().await`,
  `svc.kick_off().await`, or a future bound to a name and awaited later. This
  is the one refusal that does not need a handle to be in sight, and it is the
  counterpart of the [effect verb sweep](#what-counts-as-an-effect): a
  *synchronous* call handed no tracked handle cannot enqueue, write or call out
  (and `spawn` is refused outright), but an awaited one can reach the global job
  client or construct its own `Client`. So the rule is inverted for `.await`:
  readable, or refused.

  An awaited call is **readable** when it is rooted at a tracked handle
  (`repo.find_all().await`), carries one as an argument
  (`refunds::table.load(&mut *db).await`), is a verb the sweep already speaks
  for (`enqueue*`, `spawn*`), is a constructor, or is on the **inert-async
  allowlist**, which is exactly:

  | Allowed | Why |
  |---|---|
  | `sleep`, `sleep_until`, `yield_now`, `timeout` | scheduling, not work. `timeout` *awaits the future it is handed*, so that future is judged at the `await` instead — `timeout(d, start_finance_job()).await` is still refused |
  | a chain whose root binding is named `session`, `flash`, `cache`, `cookies`, `cookie_jar` or `csrf` | request-local plumbing; it stores no rows a grant governs |
  | a chain whose root parameter is typed `Session`, `Flash`, `CookieJar`, `PrivateCookieJar`, `SignedCookieJar`, `Csrf`, `CsrfToken` or `Cache…` — through any extractor wrapping it | the same allowance, for a parameter that is named something else |
  | `.commit()` / `.rollback()` | these end a transaction rather than acting through it |
  | a call rooted at `autumn_web` whose function is `__`-prefixed | the guard prologue `#[secured]`, `#[authorize]`, `#[step_up]` and `#[throttle]` prepend when they stack with this macro — the framework refusing the request, not the handler acting. A `Self::__autumn_…` call is *not* covered: that is a generated repository method |

  Everything else is discharged the usual way: declare what the call does with
  `#[agent_effect(...)]`, or — when it is verified to do nothing an agent's
  grant governs — with `#[agent_effect(none, reason = "...")]`.

Each fails with a diagnostic naming the call site, both sides of the
disagreement, and the annotation that resolves it.

## Escape hatches

There is exactly one, and it works at **statement** level:

```rust
#[agent_effect(writes(Refund), outbound("https://api.stripe.com/v1/refunds"),
               reason = "finalize() performs the row write and the capture call")]
let refund = finalize(&repo, &client, id).await?;

#[agent_effect(none, reason = "pure formatting helper; verified effect-free")]
let summary = render(&rows);

#[agent_effect(scoped, reason = "the view is already tenant-partitioned")]
let all: Vec<Refund> = refunds_scoped::table.load(&mut *db).await?;
```

These are all the keys, and the only ones — anything else is a compile error
that lists them back at you:

| Key | Form | Meaning |
|---|---|---|
| `writes` | `writes(Refund, "refund_notes")` | the statement performs bounded row writes to these models |
| `unbounded_writes` | `unbounded_writes(Refund)` | …with no proven row bound |
| `cross_tenant` | `cross_tenant` (bare — no arguments) | the statement leaves the tenant |
| `outbound` | `outbound("https://api.stripe.com/v1/refunds")` | the statement calls these hosts |
| `webhooks` | `webhooks("refund.drafted")` | the statement dispatches these topics |
| `jobs` | `jobs(NotifyFinanceJob, "audit_export")` | the statement enqueues these jobs |
| `scoped` | `scoped` (bare) | the statement's raw query stays inside the tenant |
| `none` | `none` (bare) | the statement performs no effect at all |
| `reason` | `reason = "…"` | **mandatory**, and non-blank |

Subjects are model/job idents or string literals; anything else in the
parentheses is refused. A statement carries at most one `#[agent_effect]` —
declare every effect of a site in the one annotation.

`none` replaces the statement's analysis entirely — that is what discharging an
opaque site means. Declared effects do **not**: they are *unioned* with what
the walk proves, so the hatch can only ever discharge unprovability, never hide
an effect the analysis could see on its own. `scoped` answers the tenant
question for a raw query and nothing else, leaving the rest of the statement
analysed as usual.

**The hatch declares; it never grants.** A declared effect is checked against
the grant exactly like a proved one, with provenance `declared` — otherwise it
would be a grant bypass with better ergonomics than deleting the grant. Three
further limits keep it a statement hatch rather than a licence:

- It is only meaningful inside an `#[agent_operable]` function, is consumed by
  that macro and never reaches rustc.
- On the **function** it is an error: it would read as covering the whole body,
  and the handler's envelope is the grant and nothing else.
- On a **block-like statement** — a block, `for`, `while`, `loop`, `async`,
  `unsafe` or `try` — it is also an error. Every effect inside such a region
  would leave the ledger, including the ones that set the reversibility floor.
  Annotate the individual statement that performs the effect, or move the block
  into a function and annotate the call.

Because a `none` site is a human's word rather than a proof, it is recorded
rather than forgotten: the expansion keeps one `AssertedEffectFree { location,
reason }` per site — so a reviewer sees not just *that* a hatch was used but
where and why — the row's manifest provenance drops from `provable` to
`declared`, **and the handler is floored at `compensable`**. Nothing proved
there is nothing to undo, so the grant may not also claim `reversible`.

## What the gate refuses

Every row below is a trybuild fixture in `autumn/tests/compile-fail/`, run in
CI. `E0080` is a failing const assertion, from one of two places: the grant
declaration itself could not be defended, or a proved effect fell outside the
envelope. `macro` is the macro refusing outright — an unreadable site, a
malformed annotation, or a grant key it will not silently drop.

| Fixture | Violation | Diagnostic |
|---|---|---|
| `agent_authority_unlisted_write` | writes a model the grant never names | `E0080` — "writes `Payout`, which grant `RefundDrafter` does not allow" |
| `agent_authority_unbounded_write` | `delete_all()` under `writes: [Refund]` | `E0080` — `writes` never implies `unbounded_writes` |
| `agent_authority_cross_tenant` | `across_tenants()` under `tenant_scope: scoped` | `E0080` — needs `tenant_scope: cross_tenant` |
| `agent_authority_outbound_not_allowlisted` | literal URL outside `outbound: [...]` | `E0080` — the exfiltration shape |
| `agent_authority_outbound_dynamic_url` | `format!`-built URL | `macro` — the host cannot be proven; names `#[agent_effect(outbound("…"))]` |
| `agent_authority_outbound_alias_dynamic_url` | `named("stripe").post(&url)` with a URL the analysis cannot read | `macro` — an alias names the *host*, so it cannot rescue an unreadable path; refused with or without one |
| `agent_authority_job_not_listed` | free-function `enqueue("wire_transfer", …)` | `E0080` — a job outlives its request, so it is part of the envelope |
| `agent_authority_opaque_helper` | helper handed a tracked handle | `macro` — never assumed effect-free; names the hatch |
| `agent_authority_opaque_associated_helper` | `Billing::wipe(&repo)` | `macro` — an uppercase path is not framework surface; the helper can write what the grant refuses |
| `agent_authority_bad_attr` | `#[agent_operable]` with no `grant = ...` | `macro` — one diagnostic, and no marker for a grant never named |
| `agent_authority_blank_effect_reason` | `#[agent_effect(none, reason = "   ")]` | `macro` — the reason is what makes the assertion reviewable |
| `agent_authority_stray_effect_on_fn` | `#[agent_effect]` on the handler | `macro` — the hatch is per statement, not a handler-wide licence |
| `agent_authority_missing_reversibility` | grant with no `reversibility` | `E0080` — the one required key; never defaulted to the permissive answer |
| `agent_authority_bad_rate` | `rate: "ten per minute"` | `E0080` — a cap no reader can interpret is not a cap |
| `agent_authority_declared_effect_outside_grant` | `#[agent_effect(writes(Payout))]` under a grant that does not allow it | `E0080` — the hatch declares, it never grants |
| `agent_authority_edge_with_agent_operable` | `#[edge]` + `#[agent_operable]` | `macro` — the edge lane is read-only, with no session, auth state or audit sink |
| `agent_authority_unknown_grant_key` | an invented grant key | `macro` — refused, never silently dropped |
| `agent_authority_repository_unlisted_write` | the unlisted write against a real `#[repository]` | `E0080` — subject resolved through `__AUTUMN_MODEL_IDENT`, so it holds across crates |

The macro also carries a seeded corpus of 26+ escape shapes as unit tests
(handles laundered through tuples, context structs and conditionals; writes
inside `#[secured]`- and `#[cached]`-rewritten bodies; trait-shaped handles;
detached tasks), plus a clean corpus of conforming handlers that must expand
without a diagnostic. A false positive is what pushes a team toward the widest
grant in the codebase, which would make the whole envelope a rubber stamp.

---

## The manifest

```bash
autumn agents manifest --manifest agent-authority.json
```

A bare `cargo build` does not write the manifest, and cannot: which handlers
are agent-operable is a whole-binary fact. An action declared in a plugin the
app merely depends on is still an action an agent can take, and link-time
`inventory` collection is the only place all of those registrations exist
together. `autumn agents manifest` builds the app, runs it in dump mode, and
reads the document back — the same shape as
[`autumn data-flow`](data-classification.md),
[`autumn cache audit`](cache-coherence.md) and
[`autumn routes audit`](security-posture-manifest.md).

```json
{
  "schema_version": 1,
  "provenance": "provable",
  "audit": { "sink_configured": true },
  "actions": [
    {
      "action": "draft_refund",
      "module_path": "shop::billing::refunds",
      "location": "src/billing/refunds.rs:28",
      "exposure": "mcp-tool",
      "provenance": "provable",
      "route": { "method": "POST", "path": "/api/refunds", "mcp_tool": true },
      "grant": {
        "name": "RefundDrafter",
        "reversibility": "compensable",
        "tenant_scope": "scoped",
        "rate": "10/min",
        "spend": "500.00 USD"
      },
      "effects": [
        { "kind": "write", "subject": "Refund", "provenance": "type_resolved", "location": "src/billing/refunds.rs:30" },
        { "kind": "outbound", "subject": "https://api.stripe.com/v1/refunds", "provenance": "syntactic", "location": "src/billing/refunds.rs:33" },
        { "kind": "job", "subject": "NotifyFinanceJob", "provenance": "syntactic", "location": "src/billing/refunds.rs:34" }
      ],
      "unused_grant_entries": ["writes: RefundNote"],
      "asserted_effect_free": []
    }
  ],
  "grants": [
    {
      "name": "RefundDrafter",
      "location": "src/billing/refunds.rs:10",
      "reversibility": "compensable",
      "tenant_scope": "scoped",
      "writes": ["Refund", "RefundNote"],
      "unbounded_writes": [],
      "outbound": ["https://api.stripe.com/v1/refunds"],
      "webhooks": [],
      "jobs": ["NotifyFinanceJob"],
      "rate": "10/min",
      "spend": "500.00 USD",
      "used": true
    }
  ],
  "ungoverned_tools": [
    {
      "tool": "destroy_widget",
      "handler": "destroy_widget",
      "method": "DELETE",
      "path": "/api/widgets/{id}",
      "module_path": "shop::widgets",
      "mutating": true,
      "exposed_by": "attribute"
    }
  ],
  "unregistered_authorities": [],
  "excluded": [
    {
      "dimension": "rate",
      "eventual_provenance": "runtime-only",
      "runtime_caveat": "declared, not enforced in this slice: the cap is checked for grammar at compile time and recorded here, but nothing meters calls at runtime."
    },
    {
      "dimension": "outbound",
      "eventual_provenance": "provable",
      "runtime_caveat": "literal URL prefixes are proven at compile time; a host resolved at runtime, a named-client `alias:` entry and any `#[agent_effect]` declaration are `declared` provenance, not proven."
    }
    // … and 4 more: spend, jobs, cascading_deletes, generated_repository_tools
  ]
}
```

### Reading the provenance labels

The document follows the rubric in
[Security Posture Manifest — Provenance Classes](security-posture-manifest.md)
at the row level; effect rows use a finer vocabulary (`type_resolved` /
`syntactic` / `declared`) that refines the rubric's `provable`. The rubric is
what keeps a row from claiming more than it knows:

- **A row** is `provable` when every effect in it was derived by the analyser,
  and `declared` when the author wrote any of them down by hand — including a
  single `#[agent_effect(none, …)]` site.
- **An effect** is `type_resolved` (the subject came from the handle's type,
  through the generated repository's model constant), `syntactic` (recovered
  from the source text — a literal URL, a job ident, a stripped type name), or
  `declared`.
- **Outbound is provable per literal entry, with a dimension caveat.** A
  literal absolute URL is proven at the call site; a named client
  (`alias:stripe`) resolves its host from `[http.client.base_urls]` at runtime,
  so the alias entry is `declared` and the dimension carries that caveat in
  `excluded`. This is a *stronger* answer than the security posture manifest's
  app-wide `outbound_http: declared`, and for a reason worth stating: that
  dimension is `declared` because nothing proves every outbound call in an app
  routes through a named client. Inside an agent-operable handler something
  does — every client root is tracked and a non-literal URL is a build error —
  so the literal half earns `provable` here that it cannot earn app-wide.
- **`rate` and `spend` are declared, full stop.** Their grammar is checked and
  they are recorded; nothing meters them.

`excluded` is this document's **dimension-caveat** list rather than the posture
manifest's excluded-dimension list: it names each dimension the manifest
records but this slice does not fully enforce, and each entry carries an
`eventual_provenance` (what the dimension could eventually claim) and a
`runtime_caveat` (what is and is not true about it today). The caveat lives
**in the document** rather than in this guide, so a reader hits it in the same
object as the claim it qualifies. Six entries ship in this slice: `rate` and
`spend` (`runtime-only` — declared, nothing meters them), `outbound` and `jobs`
(`provable` with a caveat — the literal URL and the enqueue are proven; a
runtime-resolved host and whatever the job itself goes on to do are not),
`cascading_deletes` and `generated_repository_tools` (both `provable` with a
caveat — a `dependent(...)` cascade is not folded into the write set, so
deleting a parent may write child models the grant does not list; and
`#[repository(api, mcp)]` / `expose_all_as_mcp` tools have no annotation site,
so they surface under `ungoverned_tools` rather than being gated).

### `ungoverned_tools`, and the CI gate

`ungoverned_tools` is the completeness half of the document: every MCP-exposed
route with **no** envelope, which is the one thing the compiler cannot catch —
a tool with no grant has no assertion to fail. An ordinary HTTP route that is
not an MCP tool is not an agent's to call, so it is not listed.

There is no gate in the build for that. What the manifest gives you is a
**diff**: commit it and check it in CI, so a widened envelope is reviewed
rather than merged silently.

```bash
autumn agents manifest --release --manifest agent-authority.json   # to record
autumn agents manifest --release --check agent-authority.json      # in CI
```

`--check` fails on drift **and** on any MCP-exposed *mutating* tool (anything
but `GET`/`HEAD`) with no envelope. A read-only ungoverned tool is warned
about, never failed on.

Two `exposure` values say the action is not an agent's to call, and neither is
a problem. An action reachable only over plain HTTP is `exposure:
"http-route"` — an envelope on a route no agent can reach is legal, and
recorded rather than flagged, because the grant still describes the handler.
An action registered but reachable from no route in this binary at all is
`exposure: "not-exposed"` and shown in the report — a plugin may legitimately
register an action the host does not mount, so it never fails.

`--check` also fails when the binary has **no audit sink configured** *and*
can still take an action nothing can undo — a non-`reversible` action **an
agent can reach**, or a mutating ungoverned tool. Reachability is read from the
same `exposure` field described above: only `mcp-tool` rows count, so a
compensable action a linked plugin registers but this binary never mounts, or
mounts only as an `http-route`, does not trip the gate and never costs you an
`--allow-unaudited` for a dependency's registration. The two halves matter
together: a missing sink is
survivable when everything is reversible, and a non-reversible action is
survivable when it is recorded. Only the conjunction is the state nothing
catches at runtime, because with no sink installed the audit write trivially
*succeeds*, so the fail-closed refusal never fires — that refusal protects a
configured sink that is failing, not a missing one. Install a sink with
`AppBuilder::with_audit_sink(..)`, make the actions `reversible`, or accept it
with `--allow-unaudited`.

While adopting incrementally, `--allow-ungoverned` lets `--check` pass with
mutating tools that carry no envelope. They are still listed. Both flags are
flags, never defaults: a mutating tool an agent can call with nothing declared
about it is precisely what this command exists to surface.

Two more rows exist so the document cannot be read as more than it is.
`ActionRow.asserted_effect_free` lists every `#[agent_effect(none, …)]` site
with its `location` and the author's `reason` verbatim — a count would tell a
reviewer that a hatch was used but not where or why, which is exactly the
question a hatch raises. `unregistered_authorities` catches a route pointing at
an authority nothing registered: it cannot arise from the macros (which always
emit the static and its submission together), but a hand-written static plus a
hand-written marker produces it, and such a tool would otherwise appear in
*neither* `actions` nor `ungoverned_tools` — invisible to the gate. It is
always fatal under `--check`, with no allow-flag twin.

An `ungoverned_tools` row names both the `tool` (the `operationId` an agent
calls) and the `handler` behind it, which differ when a route overrides
`#[api_doc(operation_id = "...")]`; and `exposed_by` says whether an author
opted the route in (`attribute`) or the whole-API
[`expose_all_as_mcp`](mcp.md) hatch swept it up (`hatch`).

`autumn routes --format json` also carries `agent_grant` per route, if you
want the envelope's name alongside the route table rather than in its own
document.

**Audit under the profile and features you deploy.** The manifest describes the
binary that produced it. An action or a grant behind
`#[cfg(not(debug_assertions))]` or behind a feature flag exists only in the
build that enables it, so a `--check` against a debug manifest can pass while
the shipped binary carries envelopes nobody reviewed. `--release`,
`--features`, `--all-features` and `--no-default-features` all select the
binary being inspected.

---

## The audit record

The envelope is compile-time. The record of an agent *using* it is not: every
MCP `tools/call` writes **two** [audit events](audit-logging.md), with zero
per-handler wiring.

| Event | When | Status | Carries |
|---|---|---|---|
| `agent.tool.<name>.attempt` | **before** dispatch | Success | the invocation's whole compile-known context |
| `agent.tool.<name>` | **after** dispatch | Success on 2xx, else Failure | the same, plus `http_status` and `request_id` |
| `agent.tool.<name>.refused` | instead of dispatch | Failure | the same, plus `refused_reason`; **no** `http_status`, since nothing was dispatched |

Two events for a served call rather than one, because an invocation that
crashes the process mid-flight must still leave a record that it was attempted.
The third is written **best-effort** when the call is turned away before
dispatch (see the fail-closed rule below): the thing that refused it was, by
definition, a sink that had just failed, but the write is retried anyway
because a healthy sink alongside a broken one should still get the single most
interesting thing that happened. Its own failure is ignored — there is nowhere
left to report it. It gets its own action rather than the outcome action with a
different `phase`, so an operator filtering for completed calls never has to
parse metadata to exclude refusals.

Metadata on all three:

| Key | Value |
|---|---|
| `correlation_id` | minted before dispatch; **the join key for the pair** |
| `transport` | `"mcp"` |
| `tool` | the MCP tool name |
| `grant` | the grant's name — **absent** when the tool is ungoverned |
| `reversibility` | `reversible` / `compensable` / `irreversible`, or `"unknown"` when ungoverned |
| `effects` | the proved effect set as `kind:subject`, comma-joined and capped — **absent** when the tool is ungoverned |
| `argument_names` | the argument keys the tool declares — **names only**, with any others counted (`body,+2 unknown`) |
| `phase` | exactly `attempt`, `outcome` or `refused` — which of the three records this is, so a sink can filter without parsing the action name |

…and on the outcome event alone:

| Key | Value |
|---|---|
| `http_status` | the replayed request's status (neither the attempt nor a refusal can know it — nothing was dispatched yet, or ever) |
| `request_id` | the pipeline's own `x-request-id` |
| `stream_state` | **streaming tools only**: `completed`, `aborted` or `errored` |
| `result` | **only when the body never reached the agent**: `body_overflow` or `body_error` |

The outcome is recorded once the tool result actually exists, never from the
status line alone. For a `#[api_doc(mcp, stream)]` tool that means when the
*stream* ends — a streaming handler returns `200` before it has produced
anything, so a record written then would durably claim success for a stream
that later errored or was cut off by a client disconnect, and `stream_state`
distinguishes the three endings (an abandoned stream is a `Failure` with
`stream_state = "aborted"`). For an ordinary buffered tool it means after the
handler's body has been read back and packaged: a body that overflows the
10 MiB tool-result cap, or that errors mid-read, reaches the agent as a tool
error, so the outcome is a `Failure` carrying `result` rather than a success
nobody received.

The `actor_id` is the authenticated identity's subject, or `agent:anonymous` —
never empty. The `target_resource_id` is the route template, not the tool name.
**Metadata never contains argument values**: an audit sink is not a place to
spill request payloads, and only the *shape* of a call is recorded — and only
argument names the tool itself declares (`body`, `query`, its path params). Any
other key the caller sends is counted, not quoted (`body,+2 unknown`), because
the caller chooses those names and an audit row is not the place to discover
that one of them was a newline or someone's national insurance number.

> **Where `actor_id` comes from, and what it cannot see.** It is resolved on the
> `/mcp` envelope, *before* the tool is dispatched. A principal that the route's
> own guard resolves — `#[secured]` or `RequireApiToken` on the handler — is
> published into the dispatched request's task-local scope, which is torn down
> before the dispatcher regains control, so neither event can see it. A tool
> guarded only at the route therefore records `agent:anonymous` even though a
> named principal performed the action. Read `agent:anonymous` as *"no principal
> was established at the endpoint"*, never as *"nobody"* — and when you need the
> trail to name who acted, authenticate the endpoint itself (see
> [Secure the endpoint, not just the handler](#secure-the-endpoint-not-just-the-handler)).
> Either way the outcome event's `request_id` joins the row to the dispatched
> request's own log lines, which do carry its resolved `user_id`.

### `correlation_id` vs `request_id`

They answer different questions, and both are needed. The `correlation_id`
joins the attempt to its outcome — it exists before any HTTP request does, and
survives a call that never produced a response. The `request_id` is the
`x-request-id` the MCP dispatcher's replayed request carried through the normal
pipeline, which is what joins the audit row to the **access log** and to
everything else that request touched.

### Reading the invocation inside the handler

The dispatcher inserts an `AgentInvocation` request extension before dispatch,
so an application can thread the same correlation id into its own records:

```rust
use autumn_web::agent_authority::AgentInvocation;

#[post("/api/refunds")]
#[api_doc(mcp, summary = "Draft a refund")]
#[agent_operable(grant = RefundDrafter)]
pub async fn draft_refund(
    Extension(invocation): Extension<AgentInvocation>,
    repo: PgRefundRepository,
) -> AutumnResult<Json<Refund>> {
    tracing::info!(correlation_id = %invocation.correlation_id, tool = %invocation.tool);
    // …
}
```

It carries `correlation_id`, `tool`, `grant` and `reversibility`.

### An ungoverned tool is still audited

A tool with no `#[agent_operable]` is recorded exactly like a governed one,
with no `grant` key and `reversibility = "unknown"`. The trail never silently
omits an agent action just because nobody annotated it, and "unknown" is never
rounded down to "reversible".

### No sink, and the fail-closed rule

When MCP is mounted with no `AuditLogger` installed, Autumn logs one
`tracing::warn!` at boot naming `AppBuilder::with_audit_sink(..)`, and the
manifest carries `audit.sink_configured` so the gap shows up in the document
rather than in a startup line nobody reads. Both the attempt and the outcome
event are mirrored to `tracing::info!(target: "autumn.agent", …)`, so an app
with no sink still has a trace of every agent action it served — including the
attempt, which is the half that matters when a call never returns.

When the **attempt** record cannot be written and the action is not
`reversible` — including the `unknown` of an ungoverned tool — the tool returns
an error and **the handler never runs**:

```text
audit attempt record could not be written; refusing a non-reversible action
```

A `reversible` action proceeds, and the failed write is warned about. Every one
of the three writes — attempt, refusal and outcome — is bounded by a **2 second
`AUDIT_WRITE_TIMEOUT`**, and an expiry is treated as a write failure. The
attempt write is the one that matters: it sits inline ahead of the handler, so
a saturated pool or a collector that accepted the connection and went quiet
would otherwise stall every `tools/call` until the request envelope's own
timeout fired. Under the deadline a wedged sink fails closed for a
non-reversible action and open for a reversible one, exactly as an error
would.

An audit failure *after* dispatch is always warned about and never alters the
tool result: by then the effect has happened, and losing the record is not
improved by losing the response too.

### Secure the endpoint, not just the handler

The attempt record is written **before** the request is dispatched, which is
before route-level authorization runs — that is the point (an action that is
about to be refused by a policy is still an action that was attempted, and the
trail should say so). The consequence is that an unauthenticated caller who can
reach `/mcp` can make your audit sink do work. Once a sink is installed, gate
the endpoint itself: mount it with `secure_mcp(..)` so a bearer token is
required before any of this runs, and put a rate limit in front of it. See
[Exposing Your API as MCP Tools](mcp.md) for both.

---

## Scope of the first slice

Deliberately out of scope for now:

- **Runtime enforcement.** `rate` and `spend` are validated for grammar and
  recorded; nothing meters them at request time. Treat them as reviewable
  declarations, not as limits. Rate limiting is still
  [`[rate_limit]`](rate-limiting.md)'s job.
- **Generated repository CRUD tools.** `#[repository(api, mcp)]` and
  `expose_all_as_mcp` generate tools with no annotation site, so they surface
  in `ungoverned_tools` rather than being gated. A `#[repository(.., grant = X)]`
  key is the follow-up.
- **Cascading deletes.** A `dependent(...)` cascade is not folded into the
  write set: `writes: [Post]` does not imply the comments the delete takes with
  it. Named in `excluded`.
- **Effects reached outside the handler body** — a `#[job]`'s own body, a
  `#[scheduled]` task, an interceptor, or plugin code. The envelope describes
  one action's body.
- **Non-MCP callers.** The audit record is written by the MCP dispatcher. The
  compile-time envelope applies to the handler however it is reached; the two
  events do not.
- **Proving the subject of fully dynamic effects.** A `format!`-built URL or a
  computed job name is refused, not inferred. The hatch makes the claim
  explicit and visible in review.

The threat model throughout is **drift detection, not an adversarial author** —
the same posture the [security posture manifest](security-posture-manifest.md)
states. Someone determined to escape the envelope can write
`#[agent_effect(none, reason = "trust me")]`; what they cannot do is widen the
blast radius *without it showing up in a diff*.

## How this composes

`#[agent_operable]` reads the handler and emits it unchanged, so it composes
with the route macro in either order — pinned by
`autumn/tests/compile-pass/agent_authority_route.rs`, which builds the same
handler both ways:

```rust
#[post("/api/refunds")]          // route macro outermost — preferred
#[api_doc(mcp, summary = "Draft a refund")]
#[secured("support")]
#[agent_operable(grant = RefundDrafter)]
pub async fn draft_refund(/* … */) -> AutumnResult<Json<Refund>> { /* … */ }
```

The order-independence is not luck: `#[agent_operable]` leaves a marker const
inside the body, so the route macro fills `ApiDoc::agent_authority` even when
it expands first and never sees the attribute. `#[secured]`, `#[step_up]`,
`#[authorize]` and `#[throttle]` rewrite the body into an `async` block and
`#[cached]` into an immediately-invoked closure; the analysis walks through
both, so it never blames you for a closure you did not write.

One combination is refused: **`#[edge]` + `#[agent_operable]`**. The
[edge lane](edge.md) is read-path only — no session, no auth state, no audit
sink — and an agent-operable action is a mutating, audited call by
construction.

---

## See also

- [Exposing Your API as MCP Tools](mcp.md) — `#[api_doc(mcp)]`, `mount_mcp`,
  and the tool surface this governs
- [Security Posture Manifest](security-posture-manifest.md) — the provenance
  rubric this manifest follows
- [Audit Logging](audit-logging.md) — `AuditEvent`, sinks, and the archive
- [Compile-Time Query Budgets](query-budgets.md) — the same shape of gate, for
  query counts
- [Data Classification](data-classification.md) — `#[classified]` and
  `autumn data-flow`, the taint-tracking counterpart
