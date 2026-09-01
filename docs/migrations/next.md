# Migrating to the next Autumn release (rolling draft)

> **Rolling draft.** This is the in-flight guide for the changes currently
> under `## [Unreleased]` in [`CHANGELOG.md`](../../CHANGELOG.md). Every PR
> that lands a breaking change appends a section here and links this file from
> its changelog entry. At release time the file is renamed to
> `docs/migrations/<version>.md`, its version placeholders are filled in, and
> the index in [`README.md`](README.md) is updated — see
> [`docs/release-checklist.md`](../release-checklist.md), *Migration Guide
> Gate*.
>
> The `{X.Y.Z}` placeholders below are deliberate: the gate treats `next.md` as
> a draft and accepts them (and empty sections) here, so nothing has to be
> invented for a release that has no changes yet.

## At a glance

- **Old version:** `autumn-web {X.Y.Z}`
- **New version:** `autumn-web {X.Z.0}`
- **Expected upgrade effort:** {S / M / L — one paragraph of context}
- **MSRV delta:** `{old MSRV}` → `{new MSRV}` ({reason, or "unchanged"})
- **Carried dependency majors:** {e.g. `axum 0.8 → 0.9`, `diesel 2 → 3`,
  or "none"}

## Summary

One paragraph describing *why* this release is major. Prefer "we want
these properties, and they required breaking change `X`" over a list of
unrelated removals.

Link to the [CHANGELOG entry](../../CHANGELOG.md) for the release for the
full commit-level picture.

## Before you start

- Pin your existing version (`autumn-web = "={X.Y.Z}"`) and commit.
- Run `cargo update` *before* the upgrade so the subsequent diff is just
  the major bump.
- Make sure your test suite is green on the old version. You will want
  the safety net.

## Step-by-step

1. **Run `autumn upgrade`** — *before* the dependency bump. The release it
   migrates from is the one your `Cargo.toml` still records, so bumping first
   leaves nothing in range. It previews every mechanical change this release
   can apply to your own source — a per-file diff plus a count of affected
   sites — and writes nothing; re-run with `--apply` to take them. Anything it
   cannot safely rewrite is listed with `file:line` and a link to the guide
   section that explains it.

   ```bash
   cargo install autumn-cli --version {X.Z.0}
   autumn upgrade            # preview
   autumn upgrade --apply    # take it
   ```

2. **Bump the dependency.**
   ```toml
   # Cargo.toml
   [dependencies]
   autumn-web = "{(X+1).0}"
   ```

3. **Run `cargo check`.** Work through the compiler errors section by
   section using the cheat sheet below. Only the changes labelled `review` or
   `manual` above should still need you.

4. **Apply configuration changes** (see
   [Configuration changes](#configuration-changes)).

5. **Run the test suite.**

6. **Run the application locally** and exercise each feature at least
   once. Pay attention to the [Behavior changes](#behavior-changes)
   section.

## Breaking changes

Repeat the block below for each breaking change. Keep changes grouped by
area (routing / config / database / …) so readers can skip to what they
care about.

### {Area}: {Short description}

**Why:** One or two sentences on the motivation.

**Before (`{X.Y}`):**

```rust
// paste a minimal, compiling example from the old version
```

**After (`{(X+1).0}`):**

```rust
// paste the equivalent on the new version
```

**Automation:** `manual` — {why no codemod applies: it needs new arguments, it
is a configuration or behaviour change, it is only reachable inside a macro, ….
For a change `autumn upgrade` *does* rewrite, use `auto` (safe by construction:
renames and import moves) or `review` (rewritten, every site flagged for a
human) instead, and name the shipped codemod id from
`autumn-cli/src/upgrade/migrations.rs` in this paragraph.}

Every breaking change carries this label — `scripts/check-migration-guides.sh`
fails without it, and fails an `auto`/`review` label that names no shipped
codemod, or a rename-level change left `manual` with no reason (issue #1629).

### SSG: `ManifestEntry` / `StaticManifest` are `#[non_exhaustive]`, and generated pages carry their declared `Content-Type`

**Why:** The static-first serve path used to reverse-engineer each cached page's
`Content-Type` at request time from the route slug and the served file name.
Because every non-root route is stored as `<route>/index.html`, both clues lie,
and the heuristic needed three consecutive corrections during review of #1819.
`autumn build` now records the type each handler declares into
`dist/manifest.json` and the middleware serves it verbatim (issue #1832). The
manifest types were sealed in the same release so a future field cannot break
callers again.

**Before (`{X.Y}`):**

```rust
use autumn_web::static_gen::{ManifestEntry, StaticManifest};

let entry = ManifestEntry {
    file: "about/index.html".to_owned(),
    revalidate: Some(3600),
};
let manifest = StaticManifest {
    generated_at: timestamp(),
    autumn_version: "{X.Y.Z}".to_owned(),
    routes,
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::static_gen::{ManifestEntry, StaticManifest};

let entry = ManifestEntry::new("about/index.html")
    .with_revalidate(Some(3600))
    .with_content_type(Some("text/html; charset=utf-8".to_owned()));
// `new` stamps `generated_at` and `autumn_version` for you.
let manifest = StaticManifest::new(routes);
```

**Automation:** `manual` — the rewrite is not a rename or an import move. A
struct literal has to become a constructor call plus a variable number of
`with_*` setters chosen from which fields the literal actually set, and
`StaticManifest::new` *drops* two of the fields the old literal supplied
(stamping them itself), so no purely syntactic rewrite is correct. Both types
are niche (only an app that reads or writes `dist/manifest.json` itself touches
them), so `cargo check` pointing at each E0639/E0063 is enough.

#### Behaviour change: extensionless `#[static_get]` routes returning `String`

Nothing is recorded unless the handler *deliberately* declared a type, so
routes that name their own extension are unaffected: `/theme.css` returning a
`String` still serves `text/css`, because axum's `text/plain; charset=utf-8` is
a default from the return type rather than a statement about the page.

An **extensionless** route is the one visible change. `#[static_get("/about")]
async fn about() -> String { html }` has no extension to fall back on, so it is
now served as the `text/plain; charset=utf-8` axum declares, instead of the
`text/html` the old heuristic assumed — the same thing that route already served
on the dynamic path (in dev, or on a manifest miss). Return `Markup` or
`Html<String>` and the page is `text/html` on both paths:

```rust
#[static_get("/about")]
async fn about() -> Markup { html! { h1 { "About" } } }   // text/html
```

For a non-HTML route, declare the type explicitly — this is now the whole
contract, and it is what makes types the old heuristic could never produce
(`application/rss+xml` from `/feed`) work at all:

```rust
#[static_get("/feed")]
async fn feed() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/rss+xml")], build_feed())
}
```

An existing `dist/` records nothing and keeps its previous derived behaviour
until the next `autumn build`.

One operational note: ISR does not rewrite the manifest, so a regeneration whose
handler declares a *different* type than was recorded is refused (logged at
`error`, with the previous page still served) rather than writing bytes the
recorded header would mislabel. Re-run `autumn build` after changing a static
route's `Content-Type`.

### Failure capsules: `capsule::execute` takes `ReplayFixtures`, and the capsule format is version 3

**Why:** Capsules now record every framework effect a failing run produced —
outbound HTTP, job enqueues, cache reads and writes, mail sends, the resolved
tenant, and the random bytes it drew (issue #1634) — and replay serves all of
them from the capsule. A replay is only deterministic if the clock, the entropy
source and the effect tape all come from the *same* capsule, so `execute` takes
one value that bundles them instead of a loose clock.

Two consequences beyond the signature. The document's `format_version` bumps
`2 → 3`, so **capsules recorded by an older Autumn are refused** rather than
replayed with every new seam empty — a reader that tolerated them would report a
verdict on an application shape production never ran. And an outbound HTTP call
during replay is now *served from the capsule* instead of failing closed; a call
the capsule never recorded is still refused, and is reported as a divergence.

**Before:**

```rust
let clock = ReplayClock::new(capsule.clock.clone(), fallback);
let outcome = capsule::execute(router, &capsule, divergences, Some(&clock)).await;
```

**After:**

```rust
let fixtures = ReplayFixtures::from_capsule(&capsule);
// The router is now built *through* the fixtures, so the clock and the
// entropy source it serves come from the same capsule the verdict judges.
let router = TestApp::new()
    .routes(routes![charge])
    .with_clock(fixtures.clock())
    .with_entropy(fixtures.entropy())
    .build()
    .into_router();
let outcome = capsule::execute(router, &capsule, divergences, &fixtures).await;
```

Two smaller source breaks travel with it, both from added fields on
non-`#[non_exhaustive]` types, so only struct-literal construction and
wildcard-less `match`es are affected:

* `ClientError` gains `ReplayedRequestFailure(String)` — a recorded outbound
  transport failure, reproduced as a failure rather than downgraded to a
  status. A `match` on `ClientError` without a `_` arm needs it.
* `Capsule` gains `effects` and `job`, and `ReplayOutcome` gains
  `effect_divergences`. Build capsules with
  `capsule::schema::test_support::capsule(...)` (behind `test-support`) rather
  than a struct literal, or add `..Default::default()`-style fields explicitly.

One behaviour change worth knowing even though it does not break compilation:
during a replay an outbound HTTP call is now **served from the capsule**
instead of failing closed. A call the capsule never recorded is still refused,
and is reported as an effect divergence rather than silently dialling. The
recording answers a call only when its method, URL, caller-set headers *and*
body match — the same endpoint with a different payload is a divergence — and
a mail send is compared against its recorded body as well as its recipients
and subject — and against its sender when the replayed run chose one, since a
message that sets no `from` inherits `[mail] from` at send time and a replay
boots without mail configuration.

Three situations now produce a refusal or an incomplete capsule where an
earlier build would have graded the run. A **job capsule whose payload was
masked** by `[log] filter_parameters` is refused: a handler is handed its
payload verbatim, so it would parse the `[FILTERED]` placeholder rather than
the value production ran on. A run that **enqueued inside its own
transaction** (`enqueue_on_conn`) marks its capsule incomplete, because that
enqueue is also a job-row INSERT on the database tape which replay can serve
but never issue. And an **effect whose future was cancelled** before it
finished — a losing `tokio::select!` branch, a timeout — does the same rather
than persist an outcome the run never had. Each case previously produced a
verdict; each now says why it cannot.

Two API changes come with the sharper failure reproduction. `ClientError` gains
`ReplayedRequestFailure` and `MailError` gains `ReplayedFailure`, both of which
print their recorded text and nothing else; a `match` on either enum without a
`_` arm needs the new variant. Recorded failures are otherwise rebuilt as the
variant production produced, so code branching on
`ClientError::CircuitBreakerOpen` or `MailError::AllRecipientsSuppressed`
behaves during replay as it did live.

A **panicking job** now leaves a capsule on whatever attempt it panicked on.
All three backends dead-letter a panic immediately regardless of remaining
attempts, so a job configured with `max_attempts = 25` that panics on its
first attempt never reaches a "final" one — and under the previous gate never
produced a capsule at all. Ordinary failures are unchanged: still captured on
the final attempt only.

Capsules already on disk are not migrated: replay them with the version that
wrote them, or re-record the failure. A committed regression corpus is
re-recorded and re-converted the same way — `autumn capsule verify` reports
every stale capsule as `UNREADABLE` and exits non-zero, deliberately, so a
corpus cannot quietly stop testing anything. See
[Failure Capsules › Compatibility across Autumn versions](../guide/failure-capsules.md).

**Automation:** `manual` — the new argument is a value the caller has to
construct from the capsule, and there is no textual rewrite that can invent it;
`autumn upgrade` ships no codemod for this. Direct callers of `capsule::execute`
are limited to code that drives replay itself, which is rare outside the
framework.

---

## Compiler error cheat sheet

Paste the most common errors a user will hit and the fix. This is the
single most valuable section of the guide — keep it factual and short.

| Error message (truncated) | Where you see it | Fix |
|---------------------------|------------------|-----|
| `error[E0432]: unresolved import \`autumn_web::foo\`` | module reorganized | `use autumn_web::bar;` |
| `error[E0061]: this function takes 2 arguments but 1 was supplied` | `App::run` added a parameter | see [Breaking changes › {Area}] |

## Configuration changes

- `autumn.toml` keys that were renamed, removed, or have new defaults.
- New `AUTUMN_*` environment variables.
- Default profile changes.

If nothing changed, delete this section.

## Behavior changes

Changes that still compile but behave differently at runtime. Examples:

- Error responses adopted a new JSON shape.
- A default middleware is now ordered differently.
- A scheduled task now runs on a different worker.

If nothing changed, delete this section.

## Deprecations retained from `{X.Y}`

Items that were deprecated during the `{X.Y}` line and have now been
removed. Link each to the release where the deprecation notice first
appeared so users can see how much warning they had.

### Config-key removals

Config keys removed in this major release were registered in
`DEPRECATED_CONFIG_KEYS` (`autumn/src/config.rs`) with `remove_in = "{X+1}.0.0"`.
Startup issued a `WARN` log entry for each deprecated key detected in the config
(via `since = "{X.Y}"`), and `autumn doctor` surfaced them in the
`deprecated_keys` check.

For each removed config key, fill in the table below:

| Removed key (TOML / env var) | Replacement | Deprecated since | References |
|------------------------------|-------------|------------------|------------|
| `section.old_key` / `AUTUMN_SECTION__OLD_KEY` | `section.new_key` | `{X.Y}.0` | (link to changelog) |

If no config keys were removed, delete this subsection.

## Upstream dependency updates

For each major dependency bump carried with this release:

- Link to that project's upstream migration notes.
- Call out any of their changes that leak through Autumn's public API.

If no majors were carried, delete this section.

## How to verify

The reader's proof the upgrade landed. Keep it to concrete, checkable steps —
commands with expected output, not "make sure everything works". Required by
`scripts/check-migration-guides.sh`.

1. `cargo check` — clean, with none of the errors in the cheat sheet above.
2. `cargo test` — the suite is green on the new version.
3. `autumn doctor --strict` — no findings.
4. {one step per breaking change: the observable behaviour that proves the fix
   was applied, e.g. "hit `/x` and confirm the response carries `Y`"}

### Guide-only upgrade walkthrough

(The heading keeps its historical name; the walk-through itself is
codemod-first.) Upgrade an app scaffolded with `autumn new` on the **previous** release
**codemod-first** — `autumn upgrade` before any manual step — using only this
guide for what remains, and record the result here before publishing to
crates.io. See [`docs/release-checklist.md`](../release-checklist.md),
*Migration Guide Gate*.

- **Codemod:** {the `autumn upgrade` invocation the walk-through ran first, and
  what it covered. Required once this release ships any `auto`/`review`
  codemod; the remaining manual steps below must be only the `review`/`manual`
  changes.}
- **Status:** pending
  {the value must *begin* with `performed YYYY-MM-DD` once the walk-through is
  done, or `backfilled` for a guide written after its release shipped;
  `pending` is accepted only while this file is still `next.md`}
- **From → to:** `autumn-cli {X.Y.Z}` app upgraded to `autumn-web {X.Z.0}`
- **Elapsed:** {minutes — the budget is under 30 for a guide-only
  walk-through, and under 10 once `autumn upgrade` covers this release's
  rename-level changes (issue #1629)}
- **Gaps found and fixed in this guide:** {none, or what the walk-through
  exposed}

## Troubleshooting

Known rough edges, workarounds, and known-good version combinations
(e.g. "use `diesel 2.2.5+` — earlier `2.2.x` releases have a known
`pq-sys` linkage issue on macOS").

## Reporting problems

If you hit something not covered here, please open an issue at
<https://github.com/autumn-foundation/autumn/issues> with:

- The error message or unexpected behavior.
- The old version you upgraded from.
- A minimal reproduction if possible.

Migration guides are living documents — we update them based on user
reports for the first few months after a major release.
