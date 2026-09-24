# 2026-09-24 — `policy_registration` × `#[repository(policy = ...)]` HTTP dispatch (negative result)

## 🎯 Surface

`autumn_web::authorization::{authorize, authorize_with_scopes}` (the
runtime resolver behind `#[repository(policy = ...)]`'s generated
`_api_get`/`_api_list`/`_api_create`/`_api_update`/`_api_delete` handlers,
and the `#[authorize]` attribute macro) × the boot-time registration a
`.policy::<R, _>(SomePolicy)` call on `AppBuilder` performs. Entry point
investigated: `GET /api/notes/{id}` and `GET /api/notes` served by a real
`TestApp`/`TestClient` router, on a model whose repository declares
`policy = NotePolicy` but whose app builder never calls
`.policy::<Note, _>(NotePolicy)`.

## 🕵️ Threat model (hypothesis)

`docs/guide/security-posture-manifest.md` names `policy_registration` a
`runtime-only` dimension in the manifest's own `excluded` list: "the
route→action→resource binding is proven from macro-expanded code; which
`impl Policy<Resource>` serves it is resolved from the `PolicyRegistry` at
boot (`AppBuilder::policy::<R, _>(...)`). A missing registration is not
visible at build time." That is the framework naming its own blind spot —
exactly the kind of self-declared gap this audit exists to chase down.

Against an app that follows the documented, ordinary pattern —
`#[repository(Note, api = "/api/notes", policy = NotePolicy)]`, exactly as
`docs/guide/authorization.md` and this same file's own `ac_9*` tests show —
and whose author simply forgets the paired
`.policy::<Note, _>(NotePolicy)` call on the app builder (a real, plausible
slip: the macro argument and the builder call are two different places in
two different files, and nothing at the call site itself enforces they
stay in sync), an authenticated principal with **no relationship to the
record at all** could read or mutate another tenant's row through the
auto-generated CRUD endpoint, if the generated handler's missing-policy
path silently treated "no policy" as "no restriction" instead of failing
closed. That would be the framework's own contract failing exactly the way
row 5 of the classification table describes: a documented primitive
(`policy = ...`, sold as gating every generated CRUD verb) weaker than its
contract on the one path the manifest itself admits it cannot prove at
build time. It would also clear the severity floor directly — authz bypass
reaching a gated surface, or a default-deny gate that can be made to pass
while a route is unguarded.

Reading `authorization.rs` shows `authorize`/`authorize_with_scopes` — the
functions both `__check_policy` (`#[authorize]`) and
`__check_policy_scoped` (`#[repository(policy = ...)]`'s generated CRUD
handlers) call — already resolve the policy with
`.ok_or_else(...).with_status(INTERNAL_SERVER_ERROR)`, i.e. fail closed
with a `500` rather than falling through. Separately, `app.rs`'s
`validate_repository_policies_registered` walks every `policy = ...` route
against the live registry after boot-time registrations are applied, and
in `prod`/`production` profiles refuses to start at all
(`std::process::exit(1)`) when one is missing; in every other profile it
only warns. Both of those were independently plausible, but neither had
ever been proven through a real HTTP request: `app.rs`'s own unit tests
(`registry_check_flags_routes_missing_their_policy_registration` and
neighbors) exercise only the pure `collect_unregistered_repository_handlers`
detection helper against a synthetic `Vec<Route>` — never a router, never a
request. And `TestApp::build` (`autumn/src/test.rs`) never calls
`validate_repository_policies_registered` at all — it only replays
`policy_registrations` onto the live registry — so nothing in the test
suite had ever driven a real request through a `policy = ...` route left
in exactly the state a forgotten builder call (in any profile, including a
`dev`-profile boot that only warns and keeps running) produces.

## 🧪 Reproduction attempt → negative result

Test: `autumn/tests/integration/repository_authorization.rs`:
- `get_via_repository_endpoint_fails_closed_when_policy_is_not_registered`
- `list_via_repository_endpoint_fails_closed_when_policy_is_not_registered`

The first builds a `TestApp` mounting the file's existing `Note` model/route
(`#[repository(Note, api = "/api/notes", policy = NotePolicy, scope =
NoteScope)]`) but deliberately omits the `.policy::<Note, _>(NotePolicy)`
builder call the file's other tests all make — reproducing, byte for byte,
the state `validate_repository_policies_registered` exists to catch at
boot and that a `dev`-profile boot would silently carry into serving real
traffic. It seeds a real row (`"Nobody should ever see this title"`) and
requests it *as the row's own owner* — a session that would pass
`NotePolicy::can_show` if the policy ever ran — via `GET /api/notes/{id}`.

The second uses the file's existing policy-only `SecretNote` fixture
(`policy = SecretNotePolicy`, no `scope`) instead of `Note`, and seeds two
other tenants' rows before requesting the list via `GET /api/secret-notes`
as a third, unrelated session. A first version of this test used `Note` —
a Codex review round on this PR (`chatgpt-codex-connector[bot]`) caught
that `Note` also declares `scope = NoteScope`, and `_api_list`'s generated
body picks its scope-vs-policy branch from which attribute was declared on
the macro, not from what is registered at runtime (`autumn-macros-repository/
src/api.rs`'s `scope_list_body`: `if config.scope_type.is_some() { .. }
else if has_policy { .. }`). Since that first version never registered
`.scope::<Note, _>(...)` either, the request 500'd on "missing scope
registration" *before* the list handler ever reached the policy branch —
the test was green, but proved nothing about the policy path at all.
Switching to `SecretNote`, which declares no `scope`, forces the generated
handler onto the `has_policy` per-row `can_show` branch — the one this
finding is actually about.

```
cargo test -p autumn-web --test integration_tests --features db \
  repository_authorization -- --ignored --nocapture
```

Result: **pass, both** — no bypass, no leak. See `after.txt` for the full
13-test run (the file's 11 pre-existing tests plus these 2, all green).
Both new tests assert the response status is `500 Internal Server Error`
*and* that the response body never contains the seeded record text, so a
future change that returns a differently-shaped "policy missing" error
that still happens to leak the record on its way to the error path would
still fail the second assertion even if it kept the status code fail-closed.

Docker was not available in this sandbox by default (`docker info` failed,
`service docker start` failed with a permission error under the harness's
default sandboxing); starting `dockerd` directly with
`dangerouslyDisableSandbox: true` brought the daemon up and let both tests
run against a real `testcontainers` Postgres instance — not a mock, not a
hand-built extractor call. The first attempt hit Docker Hub's anonymous
pull rate limit (`429 Too Many Requests` on `postgres:11-alpine`, which
also failed 3 of the file's own pre-existing tests in the same run,
confirming the failure was the image pull and not test logic); a
`docker pull postgres:11-alpine` to warm the local cache before re-running
fixed it, and the full file — pre-existing tests included — passed
13/13 clean.

## 🔎 Root cause of the fail-safe behavior

1. `authorization.rs::authorize` / `authorize_with_scopes`, the shared
   resolver every `#[repository(policy = ...)]`-generated handler and
   `#[authorize]`-annotated hand-written handler calls, resolve the policy
   as `state.policy_registry().policy::<R>()` — an `Option` — and turn
   `None` into `AutumnError::...with_status(StatusCode::INTERNAL_SERVER_ERROR)`
   via `.ok_or_else(...)` *before* ever constructing a `PolicyContext` or
   touching the record. There is no code path in either function that
   treats a missing policy as "allow."
2. `_api_get`/`_api_update`/`_api_delete`'s generated bodies call
   `__check_policy_scoped` (which forwards to `authorize_with_scopes`)
   with `?` immediately after loading the record (or, for get, folded into
   the same expression) — the `500` short-circuits the function before a
   `Json(record)` response is ever built, so the record body cannot reach
   the client on this path even though it was already fetched from the
   database into the handler's local state.
3. `_api_list`'s generated body (`autumn-macros-repository/src/api.rs`,
   the `has_policy` branch) resolves `__autumn_state.policy::<Model>()`
   directly with the same `.ok_or_else(...INTERNAL_SERVER_ERROR)` pattern,
   before the `find_all()` call that would otherwise load every row — so
   an unregistered policy on the list endpoint fails before a single row
   is even queried, not merely before the response is serialized.
4. This is independent of, and in addition to, `app.rs`'s
   `validate_repository_policies_registered`, which is a *defense in
   depth* / operator-UX layer (an actionable boot-time error naming the
   exact missing `.policy::<R, _>(...)` call), not the thing actually
   preventing a data leak. Even in a profile where that check only warns
   (anything other than `prod`/`production`), the request-time resolver in
   (1)-(3) is what makes the route safe.

## 🩹 Fix

None — no bug found. Regression test added at
`autumn/tests/integration/repository_authorization.rs`, already registered
in `autumn/tests/integration/mod.rs` (`mod repository_authorization;`,
unconditional — this crate compiles the file under the existing `#![cfg(feature
= "db")]` module gate). The two new tests are
`#[ignore = "requires Docker (testcontainers)"]`, matching every other test
in this file, so CI's existing "Run Docker-dependent tests" step sweeps
them automatically with no workflow edit (`CLAUDE.md`'s documented
guarantee for this house pattern). They pin the actual mechanism — a `500`
that never leaks the record body, proven against both the single-record
`_api_get` path and the `_api_list` path's per-row loop — so they fail
loudly, not vacuously, if a future change to `authorize`/
`authorize_with_scopes`, or to either generated handler body, ever lets a
missing policy registration fall through to an unguarded response.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo check -p autumn-web --test integration_tests --features db` — clean.
- `cargo test -p autumn-web --test integration_tests --features db repository_authorization -- --ignored --nocapture` — 13/13 pass (`after.txt`); first attempt hit a Docker Hub pull rate limit unrelated to this change (see above), resolved by warming the local image cache.
- `cargo clippy -p autumn-web --test integration_tests --features db -- -D warnings` — clean (the one printed warning, `unknown lint: clippy::unused_async_trait_impl`, is the same pre-existing workspace lint-config noise the 2026-09-20 finding already noted, unrelated to this file).
- `./scripts/check-panic-gate.sh` — 35/35 self-tests pass, 83 request-path modules gated; unaffected by this change (test-only, no request-path module touched).
- Re-attack: tried the mutating verbs (`PUT`/`DELETE`) the same way by
  inspection of the generated code in step 3 of "Root cause" above — both
  route through `policy_check_update_pre`/`policy_check_delete_pre`, the
  same `__check_policy_scoped` call the two tested verbs use, so they fail
  closed by the identical mechanism; not added as separate tests since
  `ac_9a`/`ac_9c`/`ac_9d` in this same file already exercise `PUT`/`DELETE`
  *with* the policy registered, and the missing-registration state is
  verb-independent (the `.ok_or_else` resolution happens before any
  verb-specific logic runs).
- One Codex review round (`chatgpt-codex-connector[bot]`) on the PR caught
  a real gap in the *list* test's rigor (not in the framework): the first
  version used `Note`, whose declared `scope = NoteScope` made the
  generated list handler 500 on a missing *scope* registration before the
  missing-*policy* path was ever reached (see "Reproduction" above) — the
  test passed, but for the wrong reason, and would have stayed green even
  if a real bypass existed. Fixed by switching to the file's existing
  policy-only `SecretNote` fixture, which declares no `scope` and so
  forces the `has_policy` branch. Re-ran after the fix — 13/13 still pass
  (`after.txt`, which reflects the post-fix run).

## 📡 Blast radius

- `#[authorize]`'s own generated call site (`__check_policy` /
  `__check_policy_scoped`, `autumn-macros/src/authorize.rs`) resolves
  through the exact same `authorize`/`authorize_with_scopes` functions
  this test exercises via the repository macro's call site — same
  resolver, so a hand-written handler guarded by `#[authorize]` fails
  closed identically. Not a separate finding; same root cause, same fix
  location, already covered by testing the shared function.
- `authorize_create` / `__check_policy_create` (the pre-insert `POST`
  path) was read, not tested here: it resolves the policy with the same
  `.ok_or_else(...INTERNAL_SERVER_ERROR)` pattern before calling
  `can_create`, so it fails closed by inspection for the same reason: it
  shares the code, not just the shape.
- `scope = ...` (the sibling `Scope<R>` registration, used by the list
  endpoint's SQL-level filtering path) has its own, structurally identical
  `.ok_or_else(...INTERNAL_SERVER_ERROR)` in the `scope_list_body` branch
  of `autumn-macros-repository/src/api.rs` — same pattern, same
  conclusion; not tested separately here since it is a different dimension
  of the same manifest-documented `runtime-only` caveat and the mechanism
  (resolve-or-500, before any row is touched) is identical.
- Feature-independent: `authorization.rs`, the `#[repository(policy =
  ...)]` code generation, and `#[authorize]` are all default-feature-set
  code (gated only on `db`, which every backend enables), so this
  reproduces (or, here, fails to reproduce) identically under every
  feature combination that compiles `db`.

## 📜 Compatibility

No behavior change, no CHANGELOG entry (test-only addition, matching this
repo's convention for negative-result commits — see
`docs/security/2026-09-10-mcp-secured-guard-dispatch/`).

## 🗂 Ledger

This directory. `after.txt` has the full green 13-test run.
