# Migrating to the next Autumn release (rolling draft)

> **Rolling draft.** This is the in-flight guide for the changes that are not
> released yet. Every PR that lands a breaking change appends a section here
> and links this file from its changelog note — which is a file under
> [`changelog.d/`](../../changelog.d/README.md), not a line in
> [`CHANGELOG.md`](../../CHANGELOG.md). At release time the file is renamed to
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

### HTTP client: `retries(n)` no longer retries `POST` and `PATCH`

**Why:** `.retries(n)` also turned on retries for non-idempotent methods. A
request for more retries could then send a `POST` two times with no
`Idempotency-Key` (issue #3054).

**Before (`0.8`):**

```rust
// Retried the POST up to 2 times.
client.post(url).retries(2).send().await?;
```

**After (`0.9`):**

```rust
// Retries the POST up to 2 times. Sends an `Idempotency-Key` header
// (the same value on each attempt) if the request has none.
client.post(url).retries(2).retry_non_idempotent().send().await?;
```

If you build a `RetryPolicy`, `HttpClientConfig` or `JobConfig` with a
struct literal, add the new field (`max_backoff`, `max_backoff_ms`), or end
the literal with `..Default::default()`.

**Automation:** `manual` — the fix adds a call only where you want `POST` or
`PATCH` retried. A codemod cannot know which calls are safe to repeat.

---

## Plugin authors

Everything here is addressed to someone maintaining an `autumn-plugin-*` /
`autumn-*-plugin` crate, not to an application author. See
[`docs/plugins.md`](../plugins.md#the-plugin-api-contract) for the tiers.

- **Stable surface changed:** none.
- **Experimental surface changed:** none.
- **New stable surface:** none.
- **Declared range to move to:** each release, bump the literal in
  `Plugin::contract`'s `.autumn_web("…")` to the new series (or write it with
  `lockstep_range(env!("CARGO_PKG_VERSION"))` if the plugin releases in
  lockstep with the framework) and re-run
  `autumn plugin-check --plugin-name <your-plugin>`. A range that excludes the
  host makes `AppBuilder::plugin` panic at registration unless
  `AUTUMN_PLUGIN_CONTRACT=warn` demotes it. The examples in the `Plugin::contract`
  and `plugin_contract` rustdoc and the reference plugin's note name the series
  under development; copy the shape, not the literal.

## Compiler error cheat sheet

Paste the most common errors a user will hit and the fix. This is the
single most valuable section of the guide — keep it factual and short.

| Error message (truncated) | Where you see it | Fix |
|---------------------------|------------------|-----|
| `error[E0432]: unresolved import \`autumn_web::foo\`` | module reorganized | `use autumn_web::<new path>;` |
| `error[E0061]: this function takes 2 arguments but 1 was supplied` | `App::run` added a parameter | see [Breaking changes › {Area}] |
| `error[E0063]: missing field \`max_backoff\`` (or `max_backoff_ms`) | a `RetryPolicy`, `HttpClientConfig` or `JobConfig` literal | add the field, or `..Default::default()` |

## Configuration changes

- `autumn.toml` keys that were renamed, removed, or have new defaults.
- New `AUTUMN_*` environment variables.
- Default profile changes.

If nothing changed, delete this section.

- New: `[http.client] max_backoff_ms` (default `20000`). The cap on the
  HTTP client's retry backoff.
- New: `[jobs] max_backoff_ms` and `AUTUMN_JOBS__MAX_BACKOFF_MS` (default
  `3600000`, 1 h). The cap on job retry backoff for every backend.

## Behavior changes

Changes that still compile but behave differently at runtime. Examples:

- Error responses adopted a new JSON shape.
- A default middleware is now ordered differently.
- A scheduled task now runs on a different worker.

If nothing changed, delete this section.

- Retries use full jitter (issue #3054). The delay before retry `n` is a
  random value in `[0, min(cap, base * 2^n)]`. Before, the HTTP client and the
  `postgres`, `redis` and `sqlite` job backends used the exact value
  `base * 2^n`, and the `local` backend used `[base/2, base]`. A test that
  expects an exact retry time must allow the range.
- The HTTP client now obeys `Retry-After` on a `503`, as on a `429`. The wait
  is `backoff + min(hint, 5 s)`. Before, a `429` waited the full hint (up to
  `max_retry_after`, default 10 s) plus the backoff. A `429` with
  `Retry-After: 10` now retries after about 5 s. Against a strict rate
  limiter, raise `max_retries` or handle the `429` yourself.
- A `POST` or `PATCH` with `.retries(n)` and no `.retry_non_idempotent()`
  now makes one attempt. This compiles with no warning, so search your code
  for `.post(` and `.patch(` calls that use `.retries(`.

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
