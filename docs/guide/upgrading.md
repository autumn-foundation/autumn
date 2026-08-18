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
          docs/migrations/0.6.0.md#security-tenancyconfigjwt_secret-is-now-a-secrecysecretstring
  auto    0.6.0-repository-with-pool-untracked  repository constructor `with_pool` is renamed to `with_pool_untracked`
          docs/migrations/0.6.0.md#repository-with_pool-is-renamed-to-with_pool_untracked

Preview (nothing is written without --apply):

src/repositories.rs (2 sites)
@@ line 12 @@
-    let repo = PostRepository::with_pool(pool.clone());
+    let repo = PostRepository::with_pool_untracked(pool.clone());
@@ line 18 @@
-    let repo = CommentRepository::with_pool(pool.clone());
+    let repo = CommentRepository::with_pool_untracked(pool.clone());

Manual - not rewritten; read the guide section:
  (whole change)  0.6.0-tenancy-jwt-secret-secretstring (no machine-applyable rewrite)
      docs/migrations/0.6.0.md#security-tenancyconfigjwt_secret-is-now-a-secrecysecretstring

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
      docs/migrations/0.6.0.md#repository-with_pool-is-renamed-to-with_pool_untracked
```

A file that is not valid Rust is reported under **Skipped** and left exactly as
it was; one unparsable file never stops the rest of the migration.

## Flags

| Flag | Effect |
|------|--------|
| `PATH` | Project directory to migrate (positional, defaults to `.`). |
| `--apply` | Write the rewrites. Without it the command only previews. |
| `--from VERSION` | Override the recorded `autumn-web` version. Needed when you already bumped the dependency, or when it is a git pin or a range with no single floor. |
| `--to VERSION` | Upgrade to this release instead of the CLI's own version. |
| `--json` | Machine-readable report — the same content, for CI. |
| `--list-migrations` | Print the shipped codemods and exit, without scanning. |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | The scan completed. This includes a run that reported `manual` sites or skipped an unparsable file — both are in the report, and neither is a failure of the command. |
| `1` | The apply step failed partway through. The report names the file it died on; the ones listed before it were already written. |
| `2` | A bad argument, an unreadable `PATH`, or a version this command cannot parse. Nothing was scanned. |

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

What remains is a same-named API in *your* code with the same form and arity:
a one-argument associated `with_pool` on a type of your own would be rewritten.
This is why preview is the default and why the diff names every file and line —
read it before you `--apply`, and `git diff` after. It is also the line the
`auto` label draws: a change that needs to know a receiver's type is labelled
`review` or `manual`, never `auto`.

Symlinked source files are not followed. Rewriting through a link could write
outside the project, so a symlink is left alone; if your app keeps real source
behind one, migrate it in its own checkout.
