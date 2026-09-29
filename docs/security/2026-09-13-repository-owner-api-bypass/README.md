# `#[repository(api = ..., owner = column)]` auto-API ignores `owner` entirely (2026-09-13)

**Class:** cross-principal (per-user) read and write through an authenticated
surface, via a macro that generates a CRUD API without the guard its own
declared attribute implies
**Surface:** `autumn_macros::repository` (`repository_macro`'s `owner_column`
handling) × `#[repository(api = "...", owner = <column>)]`'s five
auto-generated HTTP handlers
**Entry point:** the real, macro-generated `GET <api>`, `GET <api>/{id}`,
`PUT <api>/{id}` and `DELETE <api>/{id}` routes, mounted through
`AppBuilder`/`routes![]` exactly as `#[repository(api = "...")]` documents
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped
`owner = <column>` (issue #1841) — including projects scaffolded by
`autumn-cli generate scaffold`, which generated this exact declaration by
default (see Blast radius)
**Status:** fixed — `autumn-macros/src/repository.rs` (compile-time rejection)
and `autumn-cli/src/generate/scaffold.rs` (generator now wires the policy it
already creates into the same attribute)

## 🎯 Surface

`#[repository(Model, table = "...", api = "/api/...", owner = <column>)]` is
documented and tested (`autumn-macros/src/repository.rs`'s
`repository_owner_scoped_list_filters_owner_in_count_and_page` /
`repository_owner_scoped_search_filters_all_three_phases`, both written
against exactly this `api` + `owner` combination) as the lightweight
alternative to a full `Policy`/`Scope` for "each user only sees/touches their
own rows." The five HTTP handlers `api = "..."` generates are:

| Route | Handler | Backing call |
| --- | --- | --- |
| `GET <api>` | `{prefix}_api_list` | `scope_list_body` → `repo.page()` / `repo.find_all()` (no `scope`/`policy`) |
| `GET <api>/{id}` | `{prefix}_api_get` | `repo.find_by_id(id)` |
| `POST <api>` | `{prefix}_api_create` | `repo.save(new)` |
| `PUT <api>/{id}` | `{prefix}_api_update` | `repo.update(id, changes)` |
| `DELETE <api>/{id}` | `{prefix}_api_delete` | `repo.delete_by_id(id)` |

## 🕵️ Threat model

> Against an app that follows Autumn's own documented, macro-tested pattern —
> declares `#[repository(Note, table = "notes", api = "/api/notes", owner =
> author_id)]` to get a per-owner-scoped REST CRUD API, the exact combination
> `autumn-macros/src/repository.rs`'s own unit tests exercise for issue #1841
> — an attacker who is any other ordinary authenticated user of the same app
> (not elevated, not cross-tenant — just a different principal) can read
> **every** user's rows via `GET /api/notes`, and read, overwrite, or delete
> **any single row by id** via `GET`/`PUT`/`DELETE /api/notes/{id}`,
> regardless of who owns it — because none of the five generated handlers
> ever consult `owner_column`. The app author did nothing the documentation
> told them not to do: they declared `owner = author_id` on the exact
> attribute the framework advertises for this purpose.

## 🔎 Root cause

`owner = <column>` (`RepoConfig::owner_column`, `autumn-macros/src/repository.rs`)
is threaded into exactly two places, both **opt-in repository trait methods**
that a hand-written handler must call explicitly with an owner id it resolves
itself:

- `list_scoped(owner_id, ..)` (`~line 13642`, gated on `owner_col_ident` alone)
- `search_page_scoped(owner_id, ..)` (`~line 17491`, gated on `owner_col_ident`
  **and** `searchable`)

The auto-generated `api = "..."` HTTP handlers never call either method.
`scope_list_body` (`~line 15043`), which is the actual body of the generated
`GET <api>` handler, states its own precedence in a doc comment: "1. `scope =
SomeScope` … 2. `policy = SomePolicy` without `scope` … 3. Neither: plain
`repo.find_all()`, a public list." `owner_column` is not one of the three
cases — it is invisible to this function. The single-record handlers are
worse: `#get_fn`'s body is `repo.find_by_id(id)` unconditionally (only
`policy_check_show`, gated on `has_policy`, runs afterward), and
`find_by_id_impl`/`update`/`delete_by_id` (`~line 16514` onward) branch only
on `config.tenant_scoped`; `config.owner_column` never appears in any of the
three. A `grep -n "owner"` over the entire auto-API-handler-generation block
(`~lines 14846–16600`, covering list/get/create/update/delete) returns zero
matches.

The routes-audit classifier (`autumn/src/route_listing.rs::classify`) also
never consulted `owner_column` — only `api_doc.secured`, `api_doc.has_policy`,
`repo_has_policy`, and `repo_has_scope` (`repository.scope_check.is_some()`).
So `#[repository(api = "...", owner = ...)]` with no `policy`/`scope` fell
through to `RouteClassification::Unclassified` on all five routes, which
*would* fail `autumn routes audit`'s exit code (`audit_exit_code` fails
closed on any unclassified route, of any HTTP method) — but only for an app
that runs the audit gate in CI. It gives no diagnostic explaining *why*
(nothing points at `owner_column` specifically), so a developer chasing an
`Unclassified` failure on a route they believe `owner = ...` already protects
has no signal telling them the attribute they read is inert here.

## 🧪 Reproduction

Test (scratch, run against trunk to capture the red state — see
`trunk-failure.txt` below; **not part of the committed suite**, since the fix
makes the exact model declaration below a compile error):

```rust
#[autumn_web::model(table = "test_owner_docs")]
pub struct OwnerDoc { #[id] pub id: i64, pub title: String, pub author_id: i64 }

#[autumn_web::repository(
    OwnerDoc, table = "test_owner_docs", api = "/api/owner-docs", owner = author_id,
)]
pub trait OwnerDocRepository {}
```

Seeded two users' rows (`tenant-a-sentinel-owner-bypass` owned by user 1,
a second row owned by user 2), authenticated as user 999 (a stranger to
both), and drove the real HTTP routes through `TestApp`/`TestClient`:

```
WARDEN_SCRATCH_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
  cargo test -p autumn-web --test integration_tests --features "db,test-support" \
  owner_scoped_api_leaks -- --nocapture --test-threads=1
```

Result on trunk (fix reverted via `git stash`): **PASS** — i.e. the bad
outcome (leak) reproduces. See `trunk-failure.txt`: the stranger's `GET
/api/owner-docs/{sentinel_id}` returns `200` with the sentinel title, `GET
/api/owner-docs` returns both users' rows, `PUT` returns `200` and overwrites
the title, and `DELETE` returns `204` — all as a session belonging to neither
owner.

Permanent regression test (the fix's actual "green"): a `trybuild`
compile-fail fixture,
`autumn/tests/compile-fail/repository_owner_api_without_policy_or_scope.rs`,
proving the exact same declaration above is now rejected at compile time —
registered in `autumn/tests/integration/compile_fail.rs`. See `after.txt`.

## 🩹 Fix

`autumn-macros/src/repository.rs`'s `parse_repo_args` now rejects
`api_path.is_some() && owner_column.is_some() && policy_type.is_none()` with
a `syn::Error` naming the exact gap (`owner = <column> has no effect on the
generated api = "..." CRUD routes`) and the ways out (add `policy`, or drop
`api` and call `list_scoped`/`search_page_scoped` from a hand-written
route). `policy_type` is required unconditionally — `scope_type` may
additionally be present (it still gives the list endpoint a cheaper
SQL-level filter) but is never accepted as a substitute, since it only ever
filters `GET <api>`'s SQL query and has no effect on
`_api_get`/`_api_update`/`_api_delete` (only `has_policy` drives
`policy_check_show`/`policy_check_update_pre`/`policy_check_delete_pre`;
there is no `scope`-driven equivalent for the single-record handlers). Fixed
at the layer that generates the routes — the parse-time config validation
every `#[repository(...)]` expansion goes through — not by patching one
app's handler, so every app on the next release is covered without a code
change on their part (their build breaks, with a message telling them
exactly what to add).

**Review caught a gap before merge.** The first version of this fix accepted
`policy_type.is_none() && scope_type.is_none()` (i.e. treated `scope` as an
alternative to `policy`), which still left `owner = <column>` + `api =
"..."` + `scope = Type` (no `policy`) compiling successfully with
`_api_get`/`_api_update`/`_api_delete` completely unguarded — the same
underlying vulnerability, just gated behind one extra declared attribute.
Codex's automated review on PR #2770 flagged this
(`autumn-macros/src/repository.rs:1253`) before the PR merged; the condition
was tightened to require `policy_type` unconditionally, the error message
and a test were corrected, and a second `trybuild` fixture
(`repository_owner_api_with_scope_but_no_policy.rs`) was added to pin the
narrower case shut too.

Three pre-existing `autumn-macros` unit tests
(`parse_repo_args_with_owner`, `repository_owner_scoped_list_filters_owner_in_count_and_page`,
`repository_owner_scoped_search_filters_all_three_phases`) exercised `owner
= author_id` together with `api = "/api/posts"` purely as input flavor —
none of them asserted anything about the auto-API surface, only about
`list_scoped`/`search_page_scoped` generation, which is gated on
`owner_column` alone and completely independent of `api_path`. Updated to
drop the now-invalid `api = "/api/posts"` from their input so they keep
testing exactly what they tested before. Five new unit tests added
(`repository_owner_api_without_policy_or_scope_is_rejected`,
`repository_owner_api_with_policy_is_accepted`,
`repository_owner_api_with_scope_but_no_policy_is_rejected`,
`repository_owner_api_with_policy_and_scope_is_accepted`,
`repository_owner_without_api_is_accepted`) pin the gate in every direction,
plus two `trybuild` compile-fail fixtures for the two rejected shapes.

## ✅ Verification

- Reproduction: PASS on trunk (bad outcome reproduces — `trunk-failure.txt`),
  and both compile-fail fixtures are REJECTED after the fix (`after.txt`).
- `cargo test -p autumn-macros --lib repository::` — see `after.txt`.
- `cargo test -p autumn-web --test integration_tests --features "db,test-support" compile_fail` — see `after.txt`.
- `cargo fmt --all` — clean.
- `cargo clippy -p autumn-macros --lib -- -D warnings` — clean.
- `./scripts/check-migration-guides.sh` and `./scripts/check-docs-links.sh` — clean.
- Re-attack: confirmed `owner = <column>` alongside `policy = Type` (with or
  without `scope = Type` too) still compiles, that `owner = <column>` +
  `scope = Type` with **no** `policy` is rejected, and that `owner =
  <column>` with **no** `api = "..."` at all (the pure hand-written-handler
  usage) still compiles unchanged.

## 📡 Blast radius

- **`autumn-cli`'s own `generate scaffold` command shipped this exact
  pattern.** `render_repository_file` (`autumn-cli/src/generate/scaffold.rs`)
  emits `owner = <col>` on the standard owner-scoped path (`authorize_wiring`)
  but never wired the `{Pascal}Policy` it *also* generates and registers on
  the app into the same `#[repository(...)]` attribute — so `autumn generate
  scaffold Post title:String author_id:i64` (and the `--searchable` /
  attachment variants) produced a project with exactly the vulnerable
  declaration this ledger describes, before a single line was hand-written.
  Discovered when the fix landed and CI's generator-conformance suite
  (`owner-searchable`, `nullable-owner-searchable`, `attachment-owner`,
  `policy-scaffold`) started failing to compile. Fixed alongside the macro:
  `render_repository_file` now calls a new `owner_policy_wiring` helper that
  wires `policy = {Pascal}Policy` (and its `use` import) into the generated
  repository attribute whenever `owner_column` is set — completing wiring the
  scaffold's own doc comment already told developers to do by hand, using the
  policy the scaffold was already generating and registering. Three
  `autumn-cli` tests updated to assert the new wiring
  (`autumn-cli/src/generate/scaffold.rs`'s
  `searchable_with_owner_scoped_model_emits_scoped_wiring`,
  `autumn-cli/tests/generate.rs`'s
  `generated_owner_searchable_scaffold_cargo_checks` and
  `generated_nullable_owner_searchable_scaffold_cargo_checks`), and the four
  previously-failing generator-conformance tests were re-run locally and
  pass: the two `--searchable` ones above (assertions updated), plus
  `generated_attachment_owner_scaffold_cargo_checks` and
  `generated_policy_scaffold_cargo_checks`, which needed no test changes —
  they don't assert on the repository attribute's exact content, only that
  the generated project compiles and its routes authorize correctly.
- The first cut of `owner_policy_wiring` fixed the macro-level gate but
  reintroduced all four failures with a different error: `E0405: cannot find
  trait "PostDraftExt"`. `#[repository]`'s `_api_update` handler expands an
  unqualified `{Model}DraftExt::from_patch(...)` whenever `has_policy` (issue
  #1801), and this scaffold's template always emits `api = "/api/{plural}"`
  regardless of the CLI's own `--api` flag (that flag only controls the doc
  comment and which routes `main.rs` mounts, never the attribute string) — so
  `has_policy` goes live on every owner-scoped scaffold once `policy = Type`
  is wired in, not only the `--live` (broadcasts) case the existing
  `draft_ext_import` gate covered. Fixed by widening that gate from `live` to
  `live || owner_column.is_some()`. See `after.txt` for the verification.
- Codex also flagged (P1, `autumn-cli/src/generate/scaffold.rs:3030`) that a
  default non-`--api` owner-scoped scaffold's HTML `show`/index routes stay
  open because the generated policy's `can_show` returns `true`
  unconditionally. Investigated and **not a regression from this PR**: for a
  non-`--api` scaffold, `main_route_entries` mounts the hand-written
  `routes::{plural}::show` (not the macro's `_api_get`), whose authorization
  is driven entirely by `render_routes_file`'s own template — a function this
  PR never touches and which receives its own independent `owner`/`authorize`
  parameters, not a re-parse of the repository attribute string `policy_attr`
  lands in. "Public reads by default" is `autumn-cli/src/generate/policy.rs`'s
  documented, pre-existing design for the generated policy (module doc:
  "public reads, authenticated create, owner-or-admin update/delete";
  `can_show`'s own comment: "Reads are public by default. Tighten this if
  shows should be gated.") — unchanged by, and unreachable from, this PR's
  diff. The list endpoint (which IS privacy-sensitive — enumeration) already
  uses the owner-filtered `list_scoped` on every owner-scoped scaffold,
  `--api` or not (issue #1830/#1841), independent of this fix too.
- Single fix point: every `#[repository(...)]` expansion goes through the
  same `parse_repo_args`, so there is no second code path that could declare
  this combination another way.
- Swept the full auto-API handler-generation block (list/get/create/update/
  delete, `~lines 14846–16600` pre-fix) for any other `owner_column`
  reference: none exists outside `list_scoped`/`search_page_scoped`.
- Checked `create` (`_api_create`) separately: it neither reads nor writes
  `owner_column` either (an app must still set the owner column itself on
  create, e.g. via a `before_create` hook or a session-derived default) —
  unaffected by, and not fixed by, this change; out of scope since `create`
  has no existing row to leak and the declaration-site promise this bug
  breaks is about *reading/mutating another user's existing row*, not about
  who a new row is stamped with.
- Related but distinct from `docs/security/2026-09-05-cached-tenant-key/`
  (a macro-generated cache key missing an ambient scope component) — same
  general family ("a macro silently fails to apply a scoping guarantee its
  own attribute implies"), but a different mechanism (a missing branch in
  the auto-API's handler-body generation, not a cache-key hash) and a
  separate fix.
- Not a tenancy bug: `tenant_scoped` is unaffected and continues to be
  applied ambiently via `CURRENT_TENANT` to every finder, `owner` included;
  the two compose independently (see `list_scoped`'s tenant + owner filter
  setup).
- Feature-independent: `owner_column` and the auto-API are both
  default-feature-set code (`db` only), so the fix applies uniformly across
  every feature combination that compiles `#[repository(api = ...)]` at all.

## 📜 Compatibility

- **Breaking, at compile time only.** `#[repository(api = "...", owner =
  column)]` with no `policy` now fails to compile with a message naming the
  fix — this includes `owner` + `api` + `scope` alone, which is also
  rejected. Any app that had either combination in production was already
  fully exposed (this closes the hole, it does not narrow a working
  guarantee), so the break trades a silent vulnerability for a loud build
  failure. Recorded in `CHANGELOG.md` under `## [Unreleased]` → `### Security`,
  and in `docs/migrations/next.md` (`repository: owner = column next to api =
  "..." now requires policy`).
- No runtime behavior changes for any repository that already declares
  `policy` (with or without `scope`), or that declares `owner` with no `api`
  at all.
- No config default changed.

## 🗂 Ledger

This directory. `trunk-failure.txt` is the scratch reproduction's red run
against trunk (fix reverted); `after.txt` is the green run (fix applied):
the same declaration now fails to compile via the `trybuild` fixture, and the
`autumn-macros` unit tests (updated + four new ones) pass.
