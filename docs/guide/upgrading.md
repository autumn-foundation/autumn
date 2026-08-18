# Upgrading with `autumn upgrade`

Autumn ships every 2–4 weeks and, pre-1.0, most releases can break existing
apps ([Stability Policy](../../STABILITY.md)). Every breaking release ships a
[migration guide](../migrations/README.md) — and for the mechanical part of the
upgrade, a codemod you can run instead of hand-editing call sites out of prose.

```bash
autumn upgrade            # preview: per-file diff, nothing written
autumn upgrade --apply    # take the rewrites
```

## What it does

For each release between the `autumn-web` version your `Cargo.toml` records and
the version you are upgrading to, `autumn upgrade` applies that release's
machine-applyable migrations to **your own Rust source** — `src/`, `tests/`,
`examples/`, `benches/`, and every workspace member. Build output (`target/`)
and vendored sources are never touched.

It is deliberately narrow. Today the shipped rewrites are API **renames**:
`0.6.0`'s `with_pool` → `with_pool_untracked`, for instance. Configuration
files, dependency versions, and framework-owned scaffold files are out of
scope — `autumn doctor` and the migration guides cover those.

## Preview first, always

A bare `autumn upgrade` writes nothing. It prints the diff it *would* apply,
per file, plus a count of affected sites:

```text
autumn upgrade - app-code migrations 0.5.0 -> 0.6.0

Migrations in range (2):
  manual  0.6.0-tenancy-jwt-secret-secretstring  `TenancyConfig::jwt_secret` is now a `secrecy::SecretString`
          https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/0.6.0.md#security-tenancyconfigjwt_secret-is-now-a-secrecysecretstring
  auto    0.6.0-repository-with-pool-untracked  repository constructor `with_pool` is renamed to `with_pool_untracked`
          https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/0.6.0.md#repository-with_pool-is-renamed-to-with_pool_untracked

Preview (nothing is written without --apply):

src/repositories.rs (2 sites)
@@ line 12 @@
-    let repo = PgPostRepository::with_pool(pool.clone());
+    let repo = PgPostRepository::with_pool_untracked(pool.clone());
@@ line 18 @@
-    let repo = PgCommentRepository::with_pool(pool.clone());
+    let repo = PgCommentRepository::with_pool_untracked(pool.clone());

Manual - not rewritten; read the guide section:
  (whole change)  0.6.0-tenancy-jwt-secret-secretstring (no machine-applyable rewrite)
      https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/0.6.0.md#security-tenancyconfigjwt_secret-is-now-a-secrecysecretstring

2 sites in 1 file would be rewritten; 14 file(s) scanned.
Nothing was written. Re-run with `--apply` to write these changes.
```

Migrations are listed in release order, so a `manual` change from earlier in the
release can appear above an `auto` one.

Running it twice is a no-op — the rewrites match whole identifiers, so an
already-migrated call site does not match again. An app that never used the
affected APIs reports **nothing to change**.

## Confidence labels

Every documented breaking change is classified in its migration guide, and the
same label appears in the upgrade summary:

| Label | What it means |
|-------|---------------|
| `auto` | Safe by construction — a rename or an import move. Rewritten in full. |
| `review` | Rewritten, and **every** rewritten site is listed for you to read. |
| `manual` | No mechanical rewrite. The summary links the exact guide section. |

## Nothing is silently skipped

A call site the tool cannot safely rewrite is reported, not guessed at. That
means a call inside a macro invocation or an attribute, where the tokens that
look like a call may never become one:

```text
Manual - not rewritten; read the guide section:
  src/repositories.rs:40  0.6.0-repository-with-pool-untracked (inside a macro invocation)
      https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/0.6.0.md#repository-with_pool-is-renamed-to-with_pool_untracked
```

A file that is not valid Rust is reported under **Skipped** and left exactly as
it was; one unparsable file never stops the rest of the migration.

## Flags

| Flag | Effect |
|------|--------|
| `PATH` | Project directory to migrate (positional, defaults to `.`). |
| `--apply` | Write the rewrites. Without it the command only previews. |
| `--from VERSION` | Override the recorded `autumn-web` version. Needed when you already bumped the dependency, or when any manifest declares a requirement with no single floor — a git pin, a bare `*`, a multi-comparator range, or an upper bound like `"<0.6"`. A wildcard in a later position does have a floor and is read: `"0.5.*"` is `0.5.0`, the same as `"0.5"`. The root and every workspace member are read together (including `[target.'cfg(…)'.dependencies]`), the oldest floor wins, and one ambiguous declaration anywhere makes the whole answer a guess rather than being ignored. |
| `--to VERSION` | Upgrade to this release instead of the CLI's own version. |
| `--json` | Machine-readable report — the same content, for CI. |
| `--list-migrations` | Print the shipped codemods and exit, without scanning. |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | The scan completed. This includes a run that reported `manual` sites or skipped an unparsable file — both are in the report, and neither is a failure of the command. |
| `1` | The apply step failed partway through. The report names the file it died on; the ones listed before it were already written. |
| `2` | A bad argument, a `PATH` that is not a readable directory, or a version this command cannot parse. Nothing was scanned. |

There is no "found something" exit code: a preview that finds work is the
command working. Gate on the `--json` report's `manual`, `skipped`, and
`rewritten_sites` fields instead.

## Before you run it

**Run it before you bump the dependency.** The release it migrates *from* is
the one your `Cargo.toml` records, so bumping `autumn-web` first leaves nothing
in range and the command reports "nothing to change". Install the new
`autumn-cli`, run `autumn upgrade --apply`, *then* bump the dependency and
`cargo check`. (If you bumped first, `--from <previous-version>` gets you back
on track — the command says so when the range comes out empty.)

Commit or stash first. `autumn upgrade --apply` edits files in place, and the
diff you reviewed in the preview is the only record of what changed — `git
diff` afterwards is how you check its work.

Every file is read before any file is written, so a rewrite is computed from a
snapshot. If something else changes one of those files in between — a formatter,
a code generator, an editor saving — that file is refused rather than
overwritten with the rewrite of stale contents, and the run reports it. Re-run
to migrate it against what is now on disk.

## Adding a codemod (contributors)

The registry is `autumn-cli/src/upgrade/migrations.rs`, one `AppMigration` per
documented breaking change — including the ones with no mechanical form, whose
entries are what make the summary link the guide section. See
[`docs/migrations/README.md`](../migrations/README.md), *Classifying a breaking
change*, for the label convention and the release gate that enforces it.

## What it can get wrong

The tool matches call sites by name, call form, and argument count; it does not
resolve types.

Form and arity carry more weight than they might look like. Autumn itself has
same-named APIs that are *not* being renamed: `AppState::with_pool` and
`AuthzContext::with_pool` are current builder methods. They survive the 0.6.0
codemod because the renamed repository constructor takes no `self` and exactly
one argument, so only `Repo::with_pool(pool)` matches — `state.with_pool(pool)`
and the UFCS `AppState::with_pool(state, pool)` are provably different
functions and are left alone.

The receiver narrows it further. `#[repository]` names its concrete type `Pg` +
the trait name, and the scaffold names every trait `{Model}Repository`, so only
a `PgSomethingRepository::with_pool(pool)` call is rewritten — your own
`Cache::with_pool(pool)` and `PgCache::with_pool(pool)` are not. A receiver that
does not match is *reported* rather than dropped, because an aliased import
(`use PgPostRepository as Repo;`) or a hand-named trait (`PostStore` →
`PgPostStore`) would look the same from the outside:

```text
Manual - not rewritten; read the guide section:
  src/cache.rs:18  0.6.0-repository-with-pool-untracked (receiver is not a generated repository)
```

The name alone is not enough, though, because an app is free to write its own
`PgAuditRepository` with its own one-argument `with_pool`. So the shape is only
the first test: `autumn upgrade` also collects every `#[repository]` trait in
the source it scans — under any spelling of the attribute path, including the
`#[autumn_web::repository(...)]` the scaffold emits — and derives the types they
generate. A receiver has to be
one of those to be rewritten. One that looks right but is not — because no
`#[repository]` trait in the app accounts for it, or because the trait lives in
a crate outside the scan — is reported rather than guessed at:

```text
Manual - not rewritten; read the guide section:
  src/audit.rs:10  0.6.0-repository-with-pool-untracked (no `#[repository]` trait in this app generates this receiver)
```

A receiver written with a module in front of it is checked against that module
too, so a real `repositories::PgAuditRepository` does not vouch for an unrelated
`custom::PgAuditRepository`. An unqualified receiver is accepted on its name
alone — with one guard: because `#[repository]` produces its type from a macro,
that type never appears in your source, so a `struct PgAuditRepository` written
out anywhere in the scan is proof of a *different*, hand-written type. When both
exist, an unqualified call could mean either and is reported rather than
rewritten. Write the module in front of it to say which you mean — unless the
two sit at the same module path in different crates, in which case even that
does not distinguish them and the call is still reported.

What remains unresolved is an alias: `use custom::PgAuditRepository;` followed by
an unqualified call, where nothing in the scan spells out a competing
definition. Following `use` declarations is name resolution, which this command
does not do.

Preview is still the default and the diff still names every file and line —
read it before you `--apply`, and `git diff` after. This is also the line the
`auto` label draws: a change that needs to know a receiver's *type* rather than
what generates it is labelled `review` or `manual`, never `auto`.

Symlinked source files and directories are not followed. Rewriting through a
link could write outside the project, so a symlink is reported and left alone;
if your app keeps real source behind one, migrate it in its own checkout.

`target/`, `vendor/`, `node_modules/`, `dist/` and `tmp/` are skipped where a
crate begins — a directory holding a `Cargo.toml`. Beneath that they are
ordinary module names, so `src/vendor/mod.rs` is migrated like any other file.

If Cargo's output directory has been moved — `CARGO_TARGET_DIR`, or
`build.target-dir` in `.cargo/config.toml` — that directory is skipped too,
resolved as a path rather than matched by name. With the output in `out/`, an
unrelated `src/out/mod.rs` is still migrated.

Every `.cargo/config.toml` in the scan is read, not just the one at the root: a
nested standalone crate redirects its own output, and the path it names need not
sit under that crate. `CARGO_TARGET_DIR` is the exception — it overrides every
config file, so when it is set no config redirect applies.

Hidden directories are skipped by name — `.git`, `.github`, `.cargo`, `.vscode`
and the like — not because they start with a dot. A dot-directory that holds
compiled source, say a `#[path = ".generated/repositories.rs"]` module, is
migrated.

The same exclusions decide which `Cargo.toml` files the version floor is read
from, so a crate whose sources are rewritten always gets a vote on which
migrations run.
