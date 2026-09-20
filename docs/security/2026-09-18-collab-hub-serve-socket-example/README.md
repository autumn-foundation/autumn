# 2026-09-18 — `collab::hub::serve_socket`'s rustdoc example taught the IDOR the guide warns against (negative result + doc fix)

## 🎯 Surface

`autumn/src/collab/hub.rs` — the `#[collaborative]` live-editing hub (issue
#1806), specifically the `CollabHub::document` / `CollabHub::open_with` /
`serve_socket` seam an app wires a `#[ws("/…/collab")]` handler through. This
feature landed a few commits before this session (`b88f78b`) and had not had
a security pass yet.

## 🕵️ Threat model attempted

Against an app that follows the framework's own documented wiring for a
`#[collaborative]` field, can an authenticated user who owns none of a
resource read or write another tenant's/user's live document by guessing or
enumerating its id?

`CollabHub` is deliberately record-authorization-agnostic — `doc_key(table,
pk, column)` carries no tenant or ownership predicate, and
`CollabHub::document`/`open_with` open whatever key they're given. That
*looks* like Warden's row 2 ("the framework's default is unsafe") until you
check what the framework actually tells an app to do with it.

## 🔎 What I found

`docs/guide/collaboration.md`'s "Serve a live session" section already:

- shows the correct pattern — authorize the record via `session: Session` and
  `load_note_for(&session, note_id)` *before* calling `hub.open_with(...)`;
- carries an explicit `[!IMPORTANT]` callout: *"Route auth is not record
  auth. The hub applies no ownership check of its own: anyone who reaches the
  socket can edit the document the handler opens. Authorize the record in the
  handler, as above, and mark the route `#[public]` only when it genuinely
  is."*

That is exactly the framework being honest about a primitive it intentionally
leaves ungated — Warden's row 4 ("the app can hold it wrong, and the
framework offers no gate"), which the table marks **not a vulnerability**
precisely because the risk is named and the safe pattern is shown. There is
no record-level ACL a `#[collaborative]` field could check at compile time —
the same reason `#[authorize]`'s binding is resolved at runtime rather than
proven — so there is nothing to fix at the enforcement layer here.

But `autumn/src/collab/hub.rs`'s own rustdoc example on `serve_socket` (the
one visible on hover / docs.rs — more discoverable than the guide prose)
disagreed with the guide it is supposed to summarize. Before this change it
read:

```rust,ignore
#[ws("/notes/{id}/collab")]
async fn collaborate(state: AppState, hub: CollabHub, id: Path<i64>) -> impl WsHandler {
    let doc = hub.document(&doc_key("notes", *id, "body"));
    let actor = state.entropy().uuid_v4().to_string();
    move |socket| async move { serve_socket(&doc, actor, "Guest", socket).await }
}
```

No `Session` extractor, no authorization call, and `hub.document(...)`
(which seeds a **fresh empty** document — it never reads the row) keyed
directly off the raw path parameter. Copied verbatim, this is precisely the
IDOR the guide's callout exists to prevent: any caller who can reach the
route can open, read, and write the live document for any `id`, and if an
app's own close/persist path later writes `doc.document()` back to that
note's `body` column (the pattern the guide shows two sections later), a
cross-tenant *write* follows on eviction.

This is a documentation inconsistency, not an exploitable framework control
failure — `serve_socket` and `CollabHub` behave exactly as both docs describe
(no ownership check, by design), and the authoritative guide already carries
the warning and the fix. Per the ban on "fixing" a documentation gap by
inventing a code-level gate, and because the primitive has no static
authorization surface to gate, there is no enforcement-layer change to make.

## 🩹 Fix

Rewrote the `serve_socket` rustdoc example (`autumn/src/collab/hub.rs`) to
match the guide: adds the `session: Session` extractor, the
`load_note_for(&session, note_id)` authorization step, switches
`hub.document(...)` to `hub.open_with(...)` seeded from the loaded row, and
carries the same "route auth is not record auth" warning inline so the two
places a developer is likely to read this (rustdoc/docs.rs and the guide) say
the same thing. No behavior changed — `CollabHub`/`serve_socket` are
unmodified.

## ✅ Verification

- `cargo check -p autumn-web --features collab,presence,offline-sync,ws` —
  green (the edited block is a doc comment marked `rust,ignore`, so it is
  intentionally not doc-tested, matching the guide's own `rust,ignore`
  fences for the same reason: `Path`, `WsHandler`, `Session`, `AppState`, and
  `load_note_for` are illustrative, not in scope here).
- No test suite change: there is no framework-enforced behavior to regress-
  guard, since the "vulnerability" was in prose, not code. `CollabHub`'s
  existing tests already cover `document`/`open_with`/registry-full
  behavior unchanged.

## 📡 Blast radius

Swept every other rustdoc/doc-comment example in `autumn/src/collab/` (`hub.rs`,
`resolver.rs`, `mod.rs`) and `docs/guide/collaboration.md` itself for the same
gap:

- `mod.rs`'s field-declaration example only shows `#[collaborative] pub body:
  CollabText` — no socket wiring, nothing to authorize.
- `resolver.rs`'s example wires `CollabResolver` into `sync::server::router`,
  which is a `ConflictResolver` invoked by the sync engine on rows it has
  already resolved via its own tenant/session-scoped sync protocol — no
  client-suppliable key reaches `CollabResolver::resolve` directly (see the
  code-reading notes below), so no analogous example fix was needed there.
  Confirmed by reading `resolve()`: it operates on a `Change`/`RemoteRow` pair
  the sync engine already matched to one row; there is no id parameter for an
  example to get wrong.
- `docs/guide/collaboration.md`'s own example was already correct (it is what
  the rustdoc now mirrors).
- No other `#[ws(...)]`-adjacent rustdoc example in the crate references
  `CollabHub`/`serve_socket`/`doc_key` (checked via `grep -rn "doc_key\|CollabHub"
  autumn/src autumn-*/src`).

Not a released-version concern: `#[collaborative]`/`CollabHub` shipped in
`b88f78b`, a handful of commits before this session, and this is the first
security pass over it.

## 📜 Compatibility

Doc-comment-only change. No public signature, config default, or macro
expansion touched. No CHANGELOG entry — nothing user-observable changed.

## 🗂 Ledger

This directory. No red/trunk-failure or query-witness files: there is no
code-level reproduction, because there is no code-level vulnerability — see
"What I found" above.
