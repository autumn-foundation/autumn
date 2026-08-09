# Votes, Likes and Reactions — `#[votable]`

Every social feature eventually grows the same hundred lines: a `(user, thing)`
edge table, a route that reads the user's existing row, branches three ways
(toggle off / flip / insert), and then a second, unprotected statement that
recomputes a denormalised `score` or `like_count` on the parent row. It looks
trivial. It is not: the read-then-branch is a race with itself, the aggregate
recompute is a race with *every other voter*, and the two writes are usually
not in the same transaction, so readers can see a vote that the score does not
reflect.

`#[votable]` makes that a declaration. You name the reactor model and the
aggregate mode; the `#[model]` macro generates the edge table's typed
`diesel::table!`, a `react()` that toggles/flips/inserts, and an aggregate
recompute that runs **in the same transaction, under a row lock on the target
row** — so the persisted aggregate always equals ground truth, even across
different reactors hitting the same target at the same instant. The view half
is [`reaction_controls`](../../autumn/src/widgets.rs), a no-JS htmx widget that
renders the buttons and the live total.

The attribute must be written **below** `#[model]` — attribute macros are
consumed top-down, so `#[votable]` above `#[model]` is never seen by anything
and fails with `cannot find attribute votable in this scope`.

> A complete runnable version of everything below lives in
> [`examples/reddit-clone`](../../examples/reddit-clone): `Post` carries
> `#[votable(by = User, aggregate = sum)]` in
> [`src/models.rs`](../../examples/reddit-clone/src/models.rs), the whole vote
> route is [`src/routes/votes.rs`](../../examples/reddit-clone/src/routes/votes.rs),
> and [`tests/votable_pg_integration.rs`](../../examples/reddit-clone/tests/votable_pg_integration.rs)
> exercises it against the example's real migrations — including 50
> simultaneous clicks on one `(user, post)` pair. The framework's own
> [`autumn/tests/integration/model_votable.rs`](../../autumn/tests/integration/model_votable.rs)
> is the *canonical* race-safety evidence: it is the suite CI's ignored-test
> sweep runs on every push.

## Prerequisites

`#[votable]` is part of `#[autumn_web::model]`, so it needs the default
Postgres/Diesel stack any Autumn app with a `#[model]` and a `#[repository]`
already has. Nothing extra to enable:

```toml
[dependencies]
autumn-web = { version = "0.6", features = ["maud"] }
```

The `maud` feature is only needed for the widget half; the `react()` /
`reaction_of()` helpers are part of the core `db` surface. The result types
live in `autumn_web::repository::{Reaction, ReactionOutcome}`.

## The attribute

```rust,ignore
#[autumn_web::model]
#[votable(by = User, aggregate = sum)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub score: i64,      // the aggregate column
}
```

Two modes:

- **`aggregate = sum`** (the default) — signed reactions. The edge table has a
  `value SMALLINT` column and the target's `score` is `SUM(value)`. This is
  up/down voting.
- **`aggregate = count`** — unary reactions. The edge table has *no* value
  column (a row simply exists or does not), and the target's `{name}_count` is
  `COUNT(*)`. This is likes, favourites, bookmarks.

Every name is inferred, and every inference has an override:

| Key | Default | Meaning |
|---|---|---|
| `by` | **required** | the reactor model, e.g. `User` |
| `aggregate` | `sum` | `sum` (signed) \| `count` (unary) |
| `name` | `vote` | reaction name; drives `table` and the count column |
| `table` | `pluralize(name)` → `votes` | the edge table |
| `reactor_fk` | `{snake(by)}_id` → `user_id` | edge column → reactor |
| `target_fk` | `{snake(Model)}_id` → `post_id` | edge column → this model |
| `value_column` | `value` (sum mode only) | the edge's signed value |
| `column` | `score` (sum) / `{name}_count` (count) | the aggregate column |

A likes feature is therefore one line:

```rust,ignore
#[votable(by = User, aggregate = count, name = like)]
// -> table `likes`, columns `user_id` / `article_id`, aggregate `like_count`
```

The defaults are not arbitrary: they are exactly the schema reddit-clone has
shipped since its first migration, which is why adding `#[votable]` to that
example required **no overrides and no migration at all**.

## The required migration

The edge table is yours to create — `#[votable]` declares its Diesel types, not
its DDL. Two constraints are *load-bearing* (the generated code is wrong
without them), and the column types are fixed:

1. **The composite `UNIQUE (reactor_fk, target_fk)` — load-bearing.** It is the
   `ON CONFLICT` arbiter the generated upsert names by column list. Without it
   the insert fails with `42P10 there is no unique or exclusion constraint
   matching the ON CONFLICT specification`; with it, "at most one edge per
   (reactor, target)" is a database guarantee rather than an application
   convention.
2. **A `CHECK` on `value` — load-bearing in sum mode.** `react()` does **not**
   validate `value`: it writes whatever `i16` you pass and then sums the
   column. `CHECK (value IN (-1, 1))` (or whatever set your app considers
   legal) is the only thing standing between a `value=9000` request parameter
   and a permanently inflated score. A violating value is a database error that
   surfaces as a 500, so **never bind `value` straight from a request** —
   branch on the route (`/upvote` → `1`, `/downvote` → `-1`) the way the
   example does.
3. **The aggregate column `BIGINT NOT NULL DEFAULT 0`** and, in sum mode, the
   value column `SMALLINT NOT NULL`. The types are fixed by the generated code:
   the value is `i16`, the aggregate `i64`. A model whose aggregate field is
   not `i64` is a compile error, not a run-time surprise, and so is a model
   whose `#[id]` is not `i64`. The **reactor's** primary key must be `i64`
   too, but that one is documented contract rather than a compile check:
   `by =` accepts hand-written reactor structs (reddit-clone's `User` stays
   out of `#[model]` deliberately, to keep `password_hash` off the generated
   surface), which implement no framework trait the macro could constrain. A
   UUID-keyed reactor fails on first use with a database type error.
4. **Both foreign keys `BIGINT`, and `NOT NULL` strongly recommended.** The
   generated hidden `table!` declares the target FK non-nullable, so nothing
   `react()` writes can ever be `NULL`. A *nullable* target column in the DDL
   is nevertheless tolerated, and is sometimes what you already have:
   reddit-clone's `votes` is an XOR over `post_id` / `comment_id`, so both are
   nullable. That works because a Postgres unique constraint treats `NULL`s as
   distinct, so `UNIQUE (user_id, post_id)` constrains exactly the rows this
   association writes and leaves the comment votes alone. The cost is that the
   constraint no longer protects the rows written by *other* code paths —
   accept it only when every row this association writes is non-`NULL`, which
   the generated code guarantees for its own writes. (Pinned by
   `react_is_exact_when_the_edge_table_has_a_nullable_target_fk` in the
   framework's test suite.)

```sql
CREATE TABLE votes (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id),
    post_id    BIGINT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    value      SMALLINT NOT NULL CHECK (value IN (-1, 1)),
    created_at TIMESTAMP NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, post_id)              -- the ON CONFLICT arbiter
);
CREATE INDEX idx_votes_post_id ON votes (post_id);   -- the aggregate recompute

ALTER TABLE posts ADD COLUMN score BIGINT NOT NULL DEFAULT 0;
```

For `aggregate = count`, drop the `value` column entirely and name the
aggregate `{name}_count`.

## The generated API

`#[model]` emits a `{Model}Reactions` trait, blanket-implemented for any
repository of that model — so your existing `#[repository]` picks it up with no
attribute of its own. Import it as `_`:

```rust,ignore
use crate::models::PostReactions as _;
use autumn_web::repository::{Reaction, ReactionOutcome};

// sum mode
let r: Reaction = posts.react(user_id, post_id, 1).await?;
r.value;      // Option<i16>  — the reactor's reaction AFTER the call
r.aggregate;  // i64          — the newly persisted score, exact at commit
r.outcome;    // ReactionOutcome::{Inserted, Flipped, Removed}

// count mode has no `value` parameter — a like row is pure membership:
// let r = articles.react(user_id, article_id).await?;

// AC4: the viewer's own state, for rendering
let mine: Option<i16> = posts.reaction_of(user_id, post_id).await?;
```

`react()` is a **toggle**: the same value again removes the edge (`Removed`,
`value: None`), a different value replaces it in place (`Flipped`), a new one
inserts it (`Inserted`). `reaction_of` returns `Option<i16>` in *both* modes —
count mode yields `Some(1)` — so view code is mode-independent.

A toggle is not idempotent, and that has one practical consequence: **do not
blindly retry a `react()` call that timed out.** If the original call in fact
committed, the retry toggles the reaction back off — the user's vote silently
disappears. Retry safety here belongs at the HTTP layer, via an idempotency
key on the POST, not in the repository call.

`react()` is a write and always runs on the primary. `reaction_of()` is a read
and acquires its connection through the repository's normal read route, so it
is **replica-eligible** and does not pin read-your-writes: immediately after a
`react()`, a `reaction_of()` served by a lagging replica can still report the
old value. Re-render from the `Reaction` the write already returned instead —
which is the whole point of its shape: **zero follow-up queries** after a vote.

## Before / after

Here is reddit-clone's `cast_vote` before `#[votable]`. This is the code the
attribute deletes — an existence probe, a read, a three-way match, and a raw
`sql_query` score recompute, none of them in a transaction, all of them on the
request's `Db` connection:

```rust,ignore
// BEFORE — examples/reddit-clone/src/routes/votes.rs (~90 lines of mechanics)

// Verify the post exists before touching votes
let post_exists: bool = diesel::dsl::select(diesel::dsl::exists(posts::table.find(post_id)))
    .get_result(&mut **db)
    .await?;
if !post_exists {
    return Err(AutumnError::not_found_msg("Post not found"));
}

// Check if user already voted on this post
let existing_value: Option<i16> = votes::table
    .filter(votes::user_id.eq(user_id))
    .filter(votes::post_id.eq(post_id))
    .select(votes::value)
    .first(&mut **db)
    .await
    .optional()?;

match existing_value {
    Some(old_value) if old_value == value => {
        // Same vote again — toggle off (remove vote)
        diesel::delete(
            votes::table
                .filter(votes::user_id.eq(user_id))
                .filter(votes::post_id.eq(post_id)),
        )
        .execute(&mut **db)
        .await?;
    }
    Some(_) => {
        // Different vote — flip direction
        diesel::update(
            votes::table
                .filter(votes::user_id.eq(user_id))
                .filter(votes::post_id.eq(post_id)),
        )
        .set(votes::value.eq(value))
        .execute(&mut **db)
        .await?;
    }
    None => {
        // New vote
        diesel::insert_into(votes::table)
            .values((
                votes::user_id.eq(user_id),
                votes::post_id.eq(post_id),
                votes::value.eq(value),
            ))
            .on_conflict((votes::user_id, votes::post_id))
            .do_update()
            .set(votes::value.eq(value))
            .execute(&mut **db)
            .await?;
    }
}

// Recompute the score
diesel::sql_query(
    "UPDATE posts SET score = COALESCE((SELECT SUM(value::bigint) FROM votes \
     WHERE post_id = $1), 0) WHERE id = $1",
)
.bind::<diesel::sql_types::BigInt, _>(post_id)
.execute(&mut **db)
.await?;

// Reload the post just to learn its new score
let post: Post = posts::table.find(post_id).first(&mut **db).await?;
let new_score = post.score;
```

That code has three defects, and only one of them is obvious:

- The `SELECT value` → `match` window is unprotected, so two clicks from the
  same user can both take the `None` branch (one gets a `23505`) or both take
  the `Some(v)` branch (one vote silently lost).
- Worse, and *not* fixed by making the edge write a single upsert: two
  **different** users voting on the same post each run the `SELECT SUM(...)`
  subquery against their own snapshot, which does not contain the other's
  uncommitted edge. Both write `score = 1`. The truth is `2`, and the error is
  permanent.
- The edge write and the score write are separate statements outside a
  transaction, so a reader between them sees a vote the score does not include.

After — the example's `cast_vote` body, elided only where noted:

```rust,ignore
// AFTER — auth, one mutation, one fan-out, one render.
let user_id: i64 = session
    .get("user_id")
    .await
    .ok_or_else(|| AutumnError::unauthorized_msg("Login required to vote"))?
    .parse()
    .map_err(|_| AutumnError::bad_request_msg("Invalid session"))?;

let reaction = posts_repo.react(user_id, post_id, value).await?;

broadcast_post_update(post_id, state).await?;

if hx.is_htmx {
    // htmx swaps the fragment in place.
    Ok(vote_controls(post_id, reaction.aggregate, reaction.value, Some(csrf)).into_response())
} else {
    // A plain form POST (JavaScript off) navigates: hand back a full page,
    // not a bare fragment.
    Ok(Redirect::to(&format!("/posts/{post_id}")).into_response())
}
```

The vote mechanics are now a single statement: no transaction management, no
reload to learn the new score, and none of the three defects above. Roughly 90
lines of hand-written vote mechanics are gone, and — the claim worth being
precise about — the file now contains **zero raw SQL**. It is not diesel-free: the `broadcast_post_update`
helper below still loads the post and its relations with diesel to build the
SSE fragment. That is presentation, not vote logic, which is exactly why it
lives in its own helper.

## The race-safety contract

`react()` acquires its own pooled connection and runs everything inside a
single immediate transaction (`BEGIN` on Postgres, `BEGIN IMMEDIATE` on
SQLite). Five statements, in this order:

```text
S1  guard + row lock on the target row
      pg:     SELECT id FROM posts WHERE id = $t [AND deleted_at IS NULL]
                FOR NO KEY UPDATE;
      sqlite: SELECT id FROM posts WHERE id = $t [AND deleted_at IS NULL];
      0 rows -> AutumnError::not_found, transaction rolls back

S2  the reactor's current edge (safe: the lock is held)
      SELECT value FROM votes WHERE user_id = $r AND post_id = $t;

S3  exactly one of
  (a) SELECT returned the same value -> toggle off
      DELETE FROM votes WHERE user_id = $r AND post_id = $t;
  (b) SELECT returned a different value -> flip
      UPDATE votes SET value = $value WHERE user_id = $r AND post_id = $t;
  (c) SELECT returned nothing -> insert
      INSERT INTO votes (user_id, post_id, value) VALUES ($r, $t, $value)
        ON CONFLICT (user_id, post_id) DO UPDATE SET value = EXCLUDED.value;

S4  ground-truth aggregate (safe: the lock is held)
      sum:   SELECT SUM(value) FROM votes WHERE post_id = $t;
      count: SELECT COUNT(*)   FROM votes WHERE post_id = $t;

S5  persist
      UPDATE posts SET score = $agg WHERE id = $t [AND deleted_at IS NULL];

COMMIT
```

**Why this is correct, in one paragraph.** S1 takes a row lock on the target
and holds it to commit, so S2–S5 of any two `react()` calls on the
same target never interleave — a concurrent execution is therefore equivalent
to *some* serial execution, and it suffices to check one call in isolation. For
edge cardinality: the composite `UNIQUE` means the pair has 0 or 1 rows before
the call; branch (a) needs 1 and leaves 0, (b) needs 1 and leaves 1, (c) needs
0 and leaves 1 — so 50 concurrent same-pair clicks are just 50 sequential
toggles, ending (for an even count, from empty) at exactly 0 and never raising
a `23505`. For the aggregate: S4
runs after S3 inside the critical section, and under READ COMMITTED its
per-statement snapshot contains this transaction's own S3 write plus every edge
committed by a transaction that previously held the lock (it released the lock
only at commit) — and no *other* transaction can have an uncommitted edge for
this target, because it would need the lock we are holding. So S4 is the exact
post-state, S5 persists it, and both writes commit together, meaning an outside
reader sees either (edge before, score before) or (edge after, score after) and
never a mixture.

**Why `FOR NO KEY UPDATE` and not `FOR UPDATE`.** The two are equally
exclusive against each other — two `react()` calls on one target still
serialise — but `FOR UPDATE` additionally conflicts with the `FOR KEY SHARE`
lock Postgres takes when another transaction inserts a row that *references*
this one. Under `FOR UPDATE`, inserting a comment on a post would queue behind
every vote on that post. `react()` only writes a non-key column (the
aggregate), so it asks for the weaker mode and leaves referencing inserts
alone.

The lock clause is emitted only in the Postgres arm; on SQLite `BEGIN
IMMEDIATE` takes the database-wide write lock at transaction start, which is
strictly stronger than a per-target lock. (SQLite behaviour is covered by
`autumn/tests/sqlite_votable.rs`.)

**Where the argument is checked.** The claims above are executable, not
rhetorical: [`autumn/tests/integration/model_votable.rs`](../../autumn/tests/integration/model_votable.rs)
runs them against a real Postgres in CI's ignored-test sweep — 50 concurrent
same-pair clicks landing on exactly 0 edges and then 51 more landing on exactly
1, a 32-reactor burst checked against a closed-form expected total, and a
reader sampling `(score, SUM(value))` in one statement throughout a write burst
without ever seeing them disagree. The reddit-clone suite
([`tests/votable_pg_integration.rs`](../../examples/reddit-clone/tests/votable_pg_integration.rs))
repeats the headline cases against the example's shipped migrations; it is
illustrative and is **not** part of CI.

Two caveats worth knowing:

- **Isolation level.** The argument above assumes Postgres' default READ
  COMMITTED. Under `REPEATABLE READ` or `SERIALIZABLE` a contended locking read
  still *blocks* first; when the lock is released it then fails with `40001
  could not serialize access` if the row was modified while it waited. That is
  fail-safe, not corrupting, but the caller has to retry.
- **Deadlocks.** Every `react()` locks exactly one target and always in the
  same order (target row, then that target's edge), so `react()` calls cannot
  deadlock each other. It *can* block behind an unrelated outer transaction
  that has already locked the same target row on another connection.

## `react()` runs on its own connection

**`react()` checks out its own pooled connection. It does not join an enclosing
`Db::tx`, and you must not hold a `Db` extractor across the call.** A handler
that extracts `Db` and then awaits `react()` needs *two* connections at the
same time. Nothing goes wrong in development, where requests arrive one at a
time. Under load it deadlocks: once the number of concurrent requests in that
handler reaches the pool size (`database.pool_size`, default 10), every one of
them holds one connection and waits for a second that no other request can
release.
The failure mode is a hung endpoint under exactly the traffic that makes voting
interesting, so treat it as a hard rule rather than a tuning question.

This is why the example's vote route takes `PgPostRepository` and never `Db`,
and why every other checkout in that path (the reload for the SSE fan-out) is
short-lived and strictly sequential — the request holds at most one connection
at any instant:

```rust,ignore
// NOTE: no `Db` extractor. `react()` checks out its *own* pooled connection,
// so holding one across the call would make this handler need two at once.
async fn cast_vote(
    post_id: i64,
    value: i16,
    session: &Session,
    csrf: &CsrfToken,
    posts_repo: &PgPostRepository,
    state: &AppState,
) -> AutumnResult<Markup> { /* ... */ }
```

The same rule applies to the m2m mutation helpers (`add_*` / `remove_*` /
`set_*`), which acquire their connection the same way.

## Soft-delete semantics

When the target model has a `deleted_at` field, the macro emits
`AND deleted_at IS NULL` on **both** S1 and S5. So:

- Reacting to a soft-deleted target returns `AutumnError::not_found` and writes
  nothing — no edge is created and the aggregate is untouched. Soft-deleted and
  genuinely missing targets are indistinguishable to callers, the same
  behaviour the repository layer's `soft_delete` scoping gives reads. Note that
  the gate keys off the **model's** `deleted_at` field: a model that has the
  field gets the clause whether or not its `#[repository]` declares
  `soft_delete`.
- `reaction_of` deliberately does **not** consult the target: it reports a fact
  about the *edge* (what this reactor chose), which remains true regardless of
  the target's visibility.

Models without a `deleted_at` field get neither clause — it is a compile-time
decision with zero runtime cost.

Edge-level soft delete (a `deleted_at` on the edge table itself, instead of
hard-deleting on toggle-off) and reactor-side soft delete (dropping a
soft-deleted user's votes from every aggregate) are out of scope.

## The aggregate is recomputed, not accumulated — and not via commit hooks

Issue #1362 suggested recomputing the aggregate by "reusing
`repository_commit_hooks`". That is not possible, and the guide records why
rather than quietly diverging.
[`repository_commit_hooks`](../../autumn/src/repository_commit_hooks.rs) is a
**durable post-commit queue**: only the *enqueue* happens in your transaction;
the hook body runs later, on a different connection, with retries and a
dead-letter path. A hook therefore cannot be atomic with the edge mutation, and
between commit and hook execution a reader would observe exactly the
edge/aggregate disagreement the acceptance criterion forbids.

So `react()` enqueues no hook. It does the recompute inline (S4 + S5) inside
the same transaction. Two consequences worth internalising:

- The aggregate is derived from **ground truth** (`SUM`/`COUNT` over the edges)
  on every write, never accumulated as `score = score + delta`. That makes it
  **self-healing**: any historical drift — including drift left behind by a
  hand-rolled route like the "before" code above — is corrected by the next
  reaction on that target. Delta arithmetic can only preserve an error forever.
- Post-commit side effects (SSE fan-out, notifications, moderation queues) are
  *not* `react()`'s job. The durable queue is the right tool for those, and an
  `after_react` hook is a named follow-up below.

## The widget

[`reaction_controls`](../../autumn/src/widgets.rs) is the view half. It takes
pre-extracted data — never a model or a repository — and renders one
`<form method="post">` per direction (CSRF-protected once you thread the
token) plus the live aggregate:

```rust,ignore
use autumn_web::widgets::{ReactionControls, reaction_controls};

pub fn vote_controls(
    post_id: i64,
    score: i64,
    current: Option<i16>,
    csrf: Option<&CsrfToken>,
) -> Markup {
    reaction_controls(
        &ReactionControls::votes(
            format!("votes-{post_id}"),
            super::votes::__autumn_path_upvote(post_id),
            super::votes::__autumn_path_downvote(post_id),
        )
        .aggregate(score)
        .current(current)
        .csrf(csrf, None)
        .label("Post score"),
    )
}
```

- `ReactionControls::votes(dom_id, up_action, down_action)` renders up form /
  aggregate / down form; `ReactionControls::likes(dom_id, action)` renders a
  single toggle plus the count.
- `.current(...)` takes `reaction_of()`'s result and presses the matching
  button (`aria-pressed="true"` plus the `autumn-reaction-active` class);
  `None` presses neither, which is what feeds and signed-out viewers pass.
- Each form carries `hx-post` / `hx-target` / `hx-swap="outerHTML"`, with
  `hx-target` defaulting to `#{dom_id}` — so the control replaces itself in
  place. `dom_id` is interpolated into that selector, so build it yourself
  (`format!("votes-{post_id}")`); never pass a request parameter.
- The glyphs live in `<span aria-hidden="true">`, so each button's accessible
  name comes from an explicit `aria-label` (`up_label` / `down_label` /
  `like_label` to override). The aggregate sits in an `aria-live="polite"`
  span. See the [accessibility guide](accessibility.md).
- CSRF: `.csrf(Some(&csrf_token), Some(&csrf_field))`, or the
  `.csrf_token(...)` / `.csrf_field(...)` primitives. With no token, no hidden
  input is rendered.
- **The no-JS fallback needs the CSRF token.** The widget renders real
  `<form method="post">` elements, but in a CSRF-protected app a plain form
  POST without the hidden `_csrf` input is rejected with a `403`. The htmx path
  survives regardless (the framework's `autumn-htmx-csrf.js` adds the token as
  a header), so an un-threaded control is silently htmx-only. Thread
  `.csrf(...)` on every page a no-JS visitor can reach — the example does, on
  the feed, the subreddit page, the post detail page and the POST responses.
  The one legitimate `None` is a fragment that only reaches JS clients, such as
  reddit-clone's SSE payload.
- The widget emits `<form>` elements, so never nest it inside another form.

The route returns the same widget, so the response *is* the swap payload:

```rust,ignore
let reaction = posts_repo.react(user_id, post_id, value).await?;
Ok(vote_controls(post_id, reaction.aggregate, reaction.value, Some(csrf)))
```

Styling hooks (`.autumn-reaction-controls`, `.autumn-reaction`,
`.autumn-reaction-up`, `.autumn-reaction-down`, `.autumn-reaction-like`,
`.autumn-reaction-button`, `.autumn-reaction-active`,
`.autumn-reaction-count`) ship in the framework stylesheet — see
[widget styling](widget-styling.md).

## Known limits and warnings

- **`react()` does not validate `value`.** It writes the `i16` you hand it.
  Never bind that from a request; put a `CHECK` on the column as well (see
  [the migration](#the-required-migration)).
- **All edge *writes* must go through `react()`.** The aggregate is only
  recomputed by `react()`, so a hand-written `INSERT`/`UPDATE`/`DELETE` on the
  edge table leaves the target's `score` stale until the next reaction on that
  target heals it. *Reads* of the edge table — including through a separate
  `#[model]` over the same table, as reddit-clone's `Vote` does for its
  leaderboard — are entirely fine.
- **`react()` bypasses model hooks and timestamps.** It writes the edge and the
  aggregate column with direct statements: no `before_save` / `after_save`
  model hooks fire, no validation runs, and the target's `updated_at` is left
  alone (a vote is not an edit of the post). Anything that must happen on every
  vote belongs in the calling route.
- **Tenant isolation is enforced, but only when the model has the column.**
  When the target `#[model]` has a `tenant_id` field *and* the repository is
  `#[repository(..., tenant_scoped)]`, `react()`'s target lock (S1) and its
  aggregate `UPDATE` (S5) both carry `tenant_id = <current tenant>`, so a
  caller who guesses another tenant's `target_id` gets `NotFound` before
  anything is written, and `reaction_of()` returns `None` for it rather than
  that tenant's reaction. A `tenant_scoped` repository used with **no** tenant
  context is an error, exactly like its derived finders, and `across_tenants()`
  opts out of the predicate the same way. A model without a `tenant_id` column
  (or a repository that is not `tenant_scoped`) emits and pays for none of
  this. The tenant boundary lives on the *target* row — the edge table needs no
  tenant column.

  The many-to-many `add_*` / `remove_*` / `set_*` helpers are **not** covered by
  this: they are still id-scoped only. That is pre-existing and tracked
  separately.
- **One `#[votable]` per model.** A model that wants both votes *and* bookmarks
  cannot express it yet; a second attribute is a compile error, because
  `{Model}Reactions` / `react` / `reaction_of` would be ambiguous.
- **The recompute is O(edges per target).** `SUM(value) WHERE post_id = $t` is
  an index scan on every single click. On a post with 500k votes that is real
  work. Index the target FK on the edge table; the exactness is deliberate (see
  the self-healing argument above), but it is a genuine scaling ceiling.
- **Writes to one target serialize.** All reactions to one hot target queue on
  its row lock. The aggregate `UPDATE` already took a lock on that same row, so
  the design only extends the critical section by three short statements — it
  changes the constant, not the asymptotics — but a viral target is still
  capped at roughly one reaction per round trip, and under extreme load the
  queue itself can outlast `statement_timeout` (`57014`) for callers at the
  back of it.
- **Seven round trips per call** (`BEGIN`, S1–S5, `COMMIT`). Fine for a button
  press; do not loop `react()` for a bulk import.
- **READ COMMITTED is assumed** (see above).
- **Feed pages cannot cheaply highlight the viewer's own reactions.** One
  `reaction_of` per row is an N+1, which is why the example's feeds pass
  `None`.

## Follow-ups

These are deliberately *not* in the first release:

- **`reaction_of_many(reactor_id, &[target_id])`** — one batched lookup so feed
  pages can highlight the viewer's reactions without an N+1.
- **`aggregate = sum(delta)` fast mode** — `score = score + :delta` with a
  periodic reconciliation job, trading exactness-per-write for O(1) writes on
  very high-cardinality targets.
- **Multiple `#[votable]` per model**, disambiguated by `name`, generating
  `{Model}{Name}Reactions` / `react_{name}`.
- **An `after_react` commit hook** — enqueued inside the reaction transaction
  (the enqueue *is* transactional) so SSE fan-out and notifications become
  durable instead of fire-and-forget.

## See also

- [Repositories](repositories.md) — the generated CRUD surface `react()` and
  `reaction_of()` attach to.
- [Aggregate queries](aggregates.md) — `GROUP BY` roll-ups over the edge table
  itself, e.g. reddit-clone's "Top posts by votes" leaderboard.
- [Soft delete](soft-delete.md) — the repository-layer scoping `react()`
  mirrors.
- [`docs/adr/0008-associations-and-eager-loading.md`](../adr/0008-associations-and-eager-loading.md)
  — the design record, including the rejected lock-free and CTE designs.
