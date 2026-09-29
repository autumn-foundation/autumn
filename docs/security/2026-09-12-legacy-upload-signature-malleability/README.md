# `sign_upload_legacy` signature malleability (2026-09-12, negative result)

**Class investigated:** privilege escalation / scope-escape — a presigned
upload token authorizing a write to a different object than the one it was
granted for, via HMAC input-concatenation ambiguity (classic
length-extension-adjacent "delimiter injection" in a hand-rolled MAC scheme)
**Surface:** `autumn_web::storage::local::{sign_upload_legacy,
verify_upload_rotation_with_now}` × `LocalBlobStore::serve_router`'s `PUT
{mount}/{*key}?upload=1&ct=…&exp=…&sig=…` route
**Entry point investigated:** the real direct-upload HTTP route mounted by
`serve_router` (not a private helper call)
**Result:** **no working exploit** — `storage::validate_key`'s rejection of
`:` in any blob key (an unrelated, Windows-portability check) happens to
block every key this ambiguity can produce. Two regression tests committed
as tripwires.

## 🎯 Surface

`autumn/src/storage/local.rs` signs direct-upload tokens by HMAC-SHA256 over
`(blob_key, content_type, expires_at)`. Two implementations exist:

- `sign_upload` — the current, safe scheme. Length-prefixes `blob_key` and
  `content_type` with big-endian `u64` lengths before hashing, so there is
  exactly one way to parse the MAC input back into its three fields.
- `sign_upload_legacy` — HMACs `"upload:" + blob_key + ":" + content_type +
  ":" + expires_at` with bare `:` delimiters and no length prefixes.

`sign_upload_legacy` is doc-commented "Compute the legacy upload signature
for backwards compatibility" and is checked — never minted — by
`verify_upload_rotation_with_now`, which tries `sign_upload` first and falls
back to `sign_upload_legacy` against the **current** signing key and every
key in `previous_signing_keys`. This function is called directly from the
production upload route (`local.rs:1199`, inside `serve_router`'s
`upload_handler`), not gated behind any feature or test-only cfg.

`docs/plans/2026-06-05-feedback-bugfixes.md` ("Task 19: Length-Delimit
Upload Signature Fields", Issue 22) documents the original vulnerability
this was fixed for: `sign_upload("a:b", "c", exp)` and `sign_upload("a",
"b:c", exp)` used to hash identically. The plan's own fix task modified
`sign_upload` in place. Sometime after that fix, `sign_upload_legacy` was
added back as a compatibility fallback — with no corresponding CHANGELOG
entry, no `docs/guide/storage.md` mention of a signing-format migration or
deprecation window, and no expiry on how long the fallback stays wired in.
The net effect: the exact ambiguity Issue 22 eliminated for newly-minted
tokens is still accepted by the verifier, unconditionally, against the
current key.

## 🕵️ Threat model

> Against an app using Autumn's documented direct-upload flow
> (`BlobStore::presign_put`, `docs/guide/storage.md`'s "presigned direct
> upload" path) with a caller-chosen `expires_in`, where the app (or an
> earlier deployed version of it, or any integration that calls the
> still-`pub` `sign_upload_legacy` directly) has ever handed out a
> legacy-format upload token that has not yet expired — a **holder of that
> one token** (its intended recipient, or anyone it reached) can, if their
> granted `content_type` contains a `:`, replay its signature against a
> *different* `(key, content_type)` pair of the form `granted_key + ":" +
> prefix_of(content_type)` and have the verifier accept it as authentic. The
> app author did nothing wrong: `presign_put` never validates or restricts
> `content_type` format (nor should it — it's why the ambiguity is
> reachable at all), and the compatibility fallback is entirely the
> framework's own doing, not something the app opted into.

This clears the severity floor as stated ("a scoped API token exceeding its
scopes") *if* it composes into an actual write. It does not, which is the
rest of this writeup.

## 🧪 Reproduction attempt

Two tests added to `autumn/tests/integration/storage_local_integration.rs`
(consolidated binary, `#[cfg(feature = "storage")]`, already how this file
is registered in `mod.rs` — no wiring change needed):

```
cargo test -p autumn-web --test integration_tests --features storage,test-support \
  -- storage_local_integration --nocapture
```

- `legacy_upload_signature_collides_across_the_key_content_type_boundary` —
  primitive-level proof. `sign_upload_legacy(key, "reports/mine.txt",
  "text/plain:evil", exp)` and `sign_upload_legacy(key,
  "reports/mine.txt:text/plain", "evil", exp)` produce byte-identical
  signatures; `sign_upload` (current) does not collide for the same inputs.
- `legacy_signature_replay_cannot_retarget_an_upload` — the actual attack,
  through the real router. Computes a legacy signature for `("reports/mine.txt",
  "text/plain:evil")`, then sends `PUT
  /_blobs/reports/mine.txt:text/plain?upload=1&ct=evil&exp=…&sig=<that sig>`
  — the re-sliced pair — to `serve_router`'s live `oneshot` service.

**Result: both tests pass on trunk as written** — this is a negative result,
not a red-then-green fix. The second test's core assertion
(`response.status() == StatusCode::BAD_REQUEST`, not `200 OK`) is the
finding: the signature check *does* accept the forged pair (confirmed by
instrumenting it during investigation — see `queries.txt`... no DB queries
are involved here, so instead see "Root cause" below for the exact call
trace), but the subsequent `store.put_stream` call rejects the re-sliced key
before anything is written. See `after.txt` for the full test run.

## 🔎 Root cause (of both the bug and why it doesn't bite)

**The bug:** `verify_upload_rotation_with_now` (`autumn/src/storage/local.rs:696`)
tries `sign_upload` then falls back to `sign_upload_legacy`
(`autumn/src/storage/local.rs:637,713,723`) — the latter is a straight
`:`-joined concatenation with no length prefixes, so `blob_key + ":" +
content_type` is ambiguous whenever either field contains `:`. This is
reachable from the live route: `local.rs:1158-1233`'s `upload_handler`
takes `blob_key` from the URL path (`Path(blob_key)`) and `content_type`
from the query string (`Query(q).ct`) — both are the literal attacker
request, not values pinned at issuance — and calls
`verify_upload_rotation_with_now(&blob_key, &q.ct, ...)` directly.

**Why it doesn't compose into a write:** every re-sliced key this ambiguity
can produce is of the form `original_key + ":" + something` (a `:` can only
appear at the point the fields get re-split, and `original_key` cannot
already contain one — see next paragraph) — i.e. it always contains a
literal `:`. `store.put_stream` (called immediately after the signature
check passes, `local.rs:1230`) resolves the key via `safe_path_for_key` →
`storage::validate_key` (`autumn/src/storage/mod.rs:363`), which rejects any
key containing `<`, `>`, `:`, `"`, `|`, `?`, or `*` — a check written for
Windows filename portability (`check_windows_paths`/
`validate_key_rejects_windows_reserved_chars`), with no awareness of this
signature scheme at all. `validate_key` is also called at **issuance**
(`presign_put`, `local.rs:526`), so a legitimately-granted `blob_key` can
never itself contain `:` — the only source of `:` in the ambiguity is
`content_type`, which `presign_put` never validates. Both shipped
`BlobStore` implementations (`LocalBlobStore` and `autumn-storage-s3`'s
`S3BlobStore`) call the same shared `storage::validate_key`, so this holds
across backends.

Net: the auth check is broken, but an unrelated, incidental control blocks
every payload the break can produce. Nobody designed `validate_key`'s
colon rejection as a defense for this; it is not documented as one anywhere
in `docs/guide/storage.md`, `storage/mod.rs`, or the plan doc that
introduced `sign_upload`'s fix.

## 🩹 Fix

None shipped. Two options exist for a maintainer, neither taken here:

1. Remove `sign_upload_legacy` and its call sites entirely. This is a
   behavior change (a still-live pre-fix token would start failing
   verification) and needs an "Ask before" decision plus a CHANGELOG /
   compatibility note, same as the profile-conditional-surfaces follow-up
   flagged in `docs/security/2026-09-11-profile-conditional-dev-overlay/`.
2. Keep it, but bound it — e.g. tag legacy tokens with a hard sunset date at
   the point they're deprecated, or log a warning every time the legacy
   branch is the one that matched, so an operator can tell when it's safe to
   remove. Also a behavior/observability change requiring sign-off.

This PR does neither — it only adds the two regression tests above, so the
ambiguity and its current containment are both pinned and visible instead
of undiscovered.

## ✅ Verification

```
cargo fmt --all -- --check
cargo test -p autumn-web --test integration_tests --features storage,test-support \
  -- storage_local_integration --nocapture
cargo clippy -p autumn-web --test integration_tests --features storage,test-support -- -D warnings
```

All green — see `after.txt`. `./scripts/pre-push-check.sh` (the full
workspace compile-only mirror) was not run to completion in this remote
session; see the note in `docs/security/2026-09-11-profile-conditional-dev-overlay/README.md`
for the same resource constraint (bounded disk/RAM for a from-scratch
full-workspace build in this environment). CI's own `lint` + `test` jobs are
the authoritative full-workspace gate for the pushed branch.

## 📡 Blast radius

- **`autumn-storage-s3`** (`autumn-storage-s3/src/lib.rs`): calls the same
  shared `storage::validate_key` at both issuance and any key-taking
  operation. Its presigned URLs are AWS SigV4 (delegated to the S3 API),
  not this crate's HMAC scheme, so `sign_upload_legacy` doesn't even apply
  there — checked, not affected.
- **Download tokens** (`sign`/`verify`/`verify_with_rotation_with_now`):
  structurally safe from this class regardless of `validate_key`. Their MAC
  input is only two fields (`blob_key`, `expires_at`), and `expires_at` is
  always rendered as decimal digits — a value that can never itself
  contain a `:` — so the boundary between the two fields is never
  ambiguous. No `sign_legacy` counterpart exists for downloads; this
  bare-`:` delimiter pattern was only ever duplicated for uploads.
  Swept every other `Hmac<Sha256>`/`Hmac<Sha512>` construction in
  `autumn/src` and `autumn-storage-s3/src`: `mail.rs`'s unsubscribe-token
  `plaintext()` joins fields with `.` too, but base64-encodes each field
  first, so the delimiter can never appear inside a field — safe by
  construction, unlike the raw-string join here. `pagination.rs`,
  `security/config.rs`'s `hmac_sha256_hex`, `inbound_mail.rs`,
  `webhook_outbound.rs`, `alerts.rs`, and `cluster/wire.rs` all sign one
  pre-formed `message`/`payload` byte string rather than concatenating
  several independently-suppliable request fields at verify time, so there
  is no analogous re-parsing ambiguity. `sigv4.rs` and `acme/dns/route53.rs`
  implement AWS's own SigV4 key-derivation chain (`HMAC(HMAC(HMAC(secret,
  date), region), service)`), a standard, unrelated construction.
- **Feature matrix:** `storage` is the only feature gate involved; the
  vulnerable code has no interaction with `redis`/`ws`/`mail`/`i18n`/`sqlite`.
- **`verify_upload_with_rotation`** (the `#[cfg(test)]` sibling of
  `verify_upload_rotation_with_now`) is test-only and not reachable from any
  route; not a separate finding.

## 📜 Compatibility

No behavior change — test-only addition. No CHANGELOG entry, matching this
repo's convention for negative-result commits (e.g.
`docs/security/2026-09-06-idempotency-token-principal/`,
`docs/security/2026-09-10-mcp-secured-guard-dispatch/`).

## 🗂 Ledger

This directory. `after.txt` has the full green test run.
