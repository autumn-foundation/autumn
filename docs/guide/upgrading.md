# Upgrading with `autumn upgrade`

Autumn ships every 2–4 weeks and, pre-1.0, most releases can break existing
apps ([Stability Policy](../../STABILITY.md)). Every breaking release ships a
[migration guide](../migrations/README.md) — and for the mechanical part of the
upgrade, a codemod you can run instead of hand-editing call sites out of prose.

```bash
autumn upgrade            # preview: per-file diff, nothing written
autumn upgrade --apply    # take the rewrites
autumn upgrade --check    # CI gate: exit 3 if scaffold files have drifted
```

One run covers both halves of an upgrade:

1. **Your own Rust code** — each release's machine-applyable API migrations,
   applied to `src/`, `tests/`, `examples/`, `benches/` and every workspace
   member.
2. **Your project's framework-owned files** — `Dockerfile`, `build.rs`,
   `autumn.toml`, the toolchain and style configs, the CI workflow —
   reconciled against the current release's scaffold. See
   [Scaffold files](#scaffold-files) below.

## What it does

For each release between the `autumn-web` version your `Cargo.toml` records and
the version you are upgrading to, `autumn upgrade` applies that release's
machine-applyable migrations to **your own Rust source** — `src/`, `tests/`,
`examples/`, `benches/`, and every workspace member. Build output (`target/`)
and vendored sources are never touched.

It is deliberately narrow. Today the shipped rewrites are API **renames**:
`0.6.0`'s `with_pool` → `with_pool_untracked`, for instance. Dependency
versions are out of scope, and so is anything under `src/` that is not a call
site a shipped codemod names.

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
it was; one unparsable file never stops the rest of the migration. "Valid" means
it parses as Rust, not merely that its delimiters balance — `let = f();` is
skipped, not rewritten.

The same applies to a receiver the tool cannot pin down to a generated
repository. Two cases are worth naming, because both look rewritable and are
not:

- **`self::` and `super::` receivers.** These are relative to the module the
  call is written in, which this command does not track. `self::PgAuditRepository::with_pool(pool)`
  is reported rather than matched against a repository declared in some other
  module. Spell the path from the crate root — `crate::repositories::PgAuditRepository`
  — and it is rewritten.
- **A `#[repository]` attribute qualified by a renamed dependency.** Only
  Autumn's own attribute is evidence, so `#[autumn_web::repository]`,
  `#[autumn::repository]` and the bare `#[repository]` the scaffold emits all
  count, while another crate's `#[other_macros::repository]` does not. If your
  manifest renames the dependency to something else *and* you use the qualified
  spelling, those call sites are reported rather than rewritten.
- **A `#[repository]` trait declared inside a function body.** The type it
  generates is visible only in that block, so it cannot vouch for a call
  elsewhere in the module; such calls are reported. Declare the trait at module
  level and it is rewritten as usual.
- **`#[cfg]`-gated repository declarations.** A `#[cfg(feature = "postgres")] #[repository] trait AuditRepository`
  generates its type only when that feature is on, so it cannot vouch for a
  call unconditionally; under the other configuration the same name may be an
  unrelated import. Calls to such a type are reported for a human.

## Scaffold files

`autumn new` writes about a dozen framework-owned files into every project, and
those templates keep evolving: the widget CSS restructure, the `nav_bar`
helper, the toolchain and style configs that did not exist before `0.5`.
Bumping `autumn-web` in `Cargo.toml` updates the *library*; it does not touch
your project skeleton. So an app scaffolded on `0.5` keeps `0.5`-vintage
project files forever unless something reconciles them.

That something is the second half of `autumn upgrade`. It renders the current
release's scaffold in memory, compares it against what is on disk, and prints a
per-file verdict. Here is a real run in an app scaffolded before this feature
existed — which is every app that exists today:

```text
Scaffold files (unknown -> 0.7.0)
  This project predates scaffold provenance, so there is no record of
  what Autumn originally wrote. Files it is missing are offered; every
  file that differs is a conflict for you to review.

  3 file(s) differ:
  conflict  Dockerfile           no recorded baseline, so an edit cannot be ruled out
  add       clippy.toml          this release's scaffold has it; your project does not
  add       rust-toolchain.toml  this release's scaffold has it; your project does not

Dockerfile (conflict)
@@ lines 65-66 @@
-
-# our own base image pin

clippy.toml (add)
@@ line 1 @@
+msrv = "1.88.0"

rust-toolchain.toml (add)
@@ lines 1-3 @@
+[toolchain]
+channel = "1.88.0"
+components = ["rustfmt", "clippy"]

2 file(s) would be written; 1 conflict(s) need review.
Nothing was written. Re-run with `--apply` to take the writable ones.
Each file's diff is above; `git status` and `git diff` show what is yours.
Conflicts are never overwritten. Take what you want from this release's
version, or `autumn upgrade --accept <path>` to keep yours for good.
Upgrade guide: https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/0.7.0.md
```

Once the project has a baseline — see [How it knows you edited a
file](#how-it-knows-you-edited-a-file) — the header names the release it was
last reconciled to (`Scaffold files (0.7.0 -> 0.8.0)`), the banner goes away,
and an edited file is reported as `you changed this since it was scaffolded`
rather than as an unprovable one.

In every diff, `-` is what your project has now and `+` is what this release's
scaffold writes. For a `conflict` that is a description of the difference, not
of an impending write — a conflict is never applied.

The scaffold half always reconciles to **this CLI's own release**. `--to`
selects which codemods run against your Rust code; it cannot conjure a
historical scaffold, because the CLI ships exactly one set of templates — its
own. Downgrades and arbitrary historical scaffold versions are out of scope.

That cuts both ways. If the project's manifest records a release **newer** than
the CLI you are running, the scaffold half refuses outright and says so: its
files came from templates this CLI does not have, so every one of them would
look like a stale file to update, and applying that would downgrade your
`Dockerfile`, build script and CI workflow. Install a CLI at least as new as the
recorded release and run it again. `--check` exits `2` there rather than
reporting a clean project it never looked at.

### Which files it owns

| Owned — reconciled | Not owned — never touched |
|---|---|
| `autumn.toml` | `src/**` (all of your application code, including `src/bin/seed.rs`) |
| `Dockerfile`, `.dockerignore` | `Cargo.toml` (your dependencies) |
| `build.rs` | `README.md` (your prose) |
| `.gitignore`, `.env.example` | `tests/**`, `migrations/**`, `i18n/**` |
| `.github/workflows/ci.yml` | `config/credentials/**` (your secrets) |
| `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml` | `static/js/**` (vendored — `autumn assets` owns these) |
| `tailwind.config.js`, `static/css/input.css` (fullstack only) | `deny.toml` (your advisory waivers — scaffolded once, then yours) |

> **`deny.toml` and the advisory gate.** The reconciled `ci.yml` runs
> `cargo deny check advisories` against a `deny.toml` at your project root
> (issue #1600). A project scaffolded before that gate existed does not have
> one, and the upgrade will not write it — the waiver list is yours to grow, and
> a file you edit would come back as a conflict on every later run. Generate a
> throwaway project with the flags you originally scaffolded with — `--bundled-pg`
> ships a waiver the other flavors do not — and copy its `deny.toml` into your
> project root in the same commit; the audit step says so too rather than
> auditing you under an unwaived default. See
> [the advisory gate](supply-chain.md#part-3a--the-advisory-gate-known-vulnerable-dependencies).

**Application source is out of bounds.** The command never reads, writes, or
names a path under `src/`. API migration to your own code is the *first* half
of `autumn upgrade` (above), and anything it cannot rewrite mechanically is
your release's [migration guide](../migrations/README.md) to work through.

### The five verdicts

| Verdict | What it means | Written by `--apply`? |
|---|---|---|
| `current` | Identical to this release's scaffold. Not listed. | – |
| `add` | This release's scaffold has the file and your project does not — including every file a release *after* yours introduced. | yes |
| `update` | The template changed and your copy is provably untouched since Autumn wrote it. | yes |
| `conflict` | The template changed and your copy may be yours now. | **no** |
| `removed` | Autumn wrote this file once and you deleted it. Delete its line from the manifest's `[files]` to be offered it again. | **no** |
| `pinned` | You ran `autumn upgrade --accept <path>` on it. It is yours. | **no** |

A `conflict` says which of four things it is:

| Reason | When |
|---|---|
| `you changed this since it was scaffolded` | The file no longer matches the digest Autumn recorded for it. |
| `no recorded baseline, so an edit cannot be ruled out` | Nothing was recorded for this file — see [Projects older than this feature](#projects-older-than-this-feature). |
| `on disk but unreadable as text, so its contents are unknown` | It is not UTF-8, it is a directory, or the process cannot open it. What it holds is unknown, so it is untouchable. |
| `a symlink; writing through it could write outside the project` | The path is a symbolic link. Writing through it would write wherever it points, which need not be inside the project — so the link is reported and left exactly as it is. |

### Finishing a conflict

Reviewing a conflict has to be able to *conclude*. Sometimes you take this
release's version; sometimes the file is deliberately yours and always will be —
a `Dockerfile` with your own base image, a CI workflow with your own jobs. For
that second case:

```bash
autumn upgrade --accept Dockerfile
```

That records the path in the manifest's `pinned` list and writes nothing else.
From then on the file reports as `pinned`, is never offered or written, and does
not count as drift — so `--check` can go green again. Without it a team that
customises one file has a CI gate that is red forever, which is a gate that gets
deleted.

`pinned` is a plain list of paths, not digests, precisely so you can undo it:
delete a line from `.autumn/scaffold.toml` and the file comes back under
reconciliation. `--accept` refuses a path the current scaffold does not own —
promising to skip a file this command never touches would mean nothing.

**When you resolve a conflict by taking this release's version by hand, run
`autumn upgrade --apply` once more afterwards.** The file is now identical to
the scaffold, so nothing gets written — but the run records its digest, and
that baseline is what lets the *next* release update the file for you instead
of raising it as a conflict again. `--check` deliberately writes nothing, so it
cannot do this for you: it will report the file as current while leaving it
without a baseline.

### How it knows you edited a file

"May I overwrite this?" is really "did you touch it?", and neither the file's
contents nor its timestamp can answer that. So `autumn new` records a digest of
every framework-owned file as it writes it, in `.autumn/scaffold.toml`:

```toml
version = "0.7.0"
written_by = "0.7.0"
flavor = "fullstack"
i18n = false
seed = false
daemon = false
bundled_pg = false

pinned = ["Dockerfile"]

[files]
"clippy.toml" = "<64 hex characters of SHA-256>"
```

**Commit that file.** Its entire value is being the baseline a *later* checkout
compares against. It holds a release, the flags the project was created with,
and one digest per file — no paths outside the project, and nothing secret (the
digests are of Autumn's own template text, never of your content).

`version` means "the release these files were last *fully* reconciled to".
`autumn upgrade --apply` refreshes the file, and moves `version` forward only
once no conflicts are left — so a half-finished upgrade never reads as a
finished one, and a project that has never been fully reconciled simply has no
`version` line rather than a flattering guess.

`written_by` answers a different question: which is the newest release that has
written any digest here. It moves forward on every apply, conflicts or not.
The two come apart exactly when an upgrade leaves a conflict standing — the
digests advance to the new templates while `version` waits — and that gap is
what an older CLI has to see. Without it, rolling back to the older CLI would
find digests it trusts, matching files it cannot render, and downgrade them.

Every write is staged in the same directory and renamed into place, so an
interrupted `--apply` cannot leave you with a half-written `Dockerfile` or a
truncated new file. An updated file keeps its original permissions; an added
one lands with exactly the mode `autumn new` would have given it.

Line endings are normalised before hashing, so a `core.autocrlf` checkout on
Windows is not mistaken for you having personally rewritten every one of them.

### Projects older than this feature

An app scaffolded before `.autumn/scaffold.toml` existed has no baseline, and
that is **not** an error — it just means "untouched" cannot be proven. The
upgrade is best effort:

- files your project is missing entirely are still offered as `add` — there is
  no content to lose;
- every file that differs is a `conflict` for you to review, never an
  overwrite.

The report says so up front. Once you have run `--apply` once, the manifest
exists and every later upgrade gets the sharper answer.

Flavor is inferred the same way when there is no manifest, and deliberately
needs *positive* evidence of the fullstack CSS pipeline: a `tailwind.config.js`,
a `static/css/` directory, or the vendored `static/js/htmx.min.js`. Any one is
enough, so deleting a single file cannot reclassify your project — and a JSON
API that happens to serve its `openapi.json` out of `static/` is not handed a
Tailwind config it has no use for. An `i18n/` directory means the project was
made with `--with-i18n`, and `autumn.toml`'s own daemon markers identify the
daemon flavors.

The project's name comes from `[package] name` in `Cargo.toml` and from nowhere
else — never the directory name. `autumn.toml`, `.env.example`, the CI workflow
and the `Dockerfile`'s `CMD` all interpolate it, so a guessed name would render
a *different* scaffold and could rewrite `COPY --from=builder
/app/target/release/<name>` into an image that cannot start. If your
`Cargo.toml` gives no usable package name, the scaffold half says so and
reconciles nothing rather than comparing against a fiction — and `--check` exits
`2` there, because "we could not look" is not "clean".

### Reverting

`--apply` edits files in place, and your VCS is the undo:

```bash
git status                            # what changed, added files included
git diff                              # exactly what changed, line by line
git checkout -- Dockerfile            # put back a file it UPDATED
rm rust-toolchain.toml                # remove a file it ADDED
```

The distinction matters: `git checkout --` restores a tracked file's contents,
and does nothing at all for a file that was just created — git has never heard
of it. An aged project is mostly `add`s, so `git status` (which lists untracked
files) is the one to start from. A legacy project's first `--apply` also creates
`.autumn/scaffold.toml`; it is new too.

Commit or stash before you apply, the same as for the codemods. The preview is
the review step, `git diff` is the proof, and `git checkout --` is the escape
hatch.

### Inside a Cargo workspace

If the project is a crate inside an enclosing workspace — meaning its own
`Cargo.toml` has no `[workspace]` table of its own — the files that workspace
owns at *its* root are out of scope here, and the report says so:

- `clippy.toml`, `rustfmt.toml` and `rust-toolchain.toml` are resolved from the
  nearest ancestor of the crate being built, so a crate-local copy does not add
  to the workspace's — it **shadows** it, silently dropping its lints and its
  MSRV pin with no diagnostic.
- GitHub only runs workflows from the repository root, so a member's
  `.github/workflows/ci.yml` never runs at all.

Everything genuinely per-crate — `autumn.toml`, `Dockerfile`, `.dockerignore`,
`build.rs`, `.gitignore`, `.env.example`, and the CSS pipeline — is reconciled as
usual. Reconcile the workspace-level files by running the command at the
workspace root, if that root is itself an Autumn project.

`autumn new` writes a bare `[workspace]` table into every generated
`Cargo.toml`, so a scaffolded project is its own workspace root wherever you
drop it, and nothing changes. This applies only once you adopt the app *into* a
workspace by deleting that table — which is exactly when Cargo starts resolving
its lint, format and toolchain config from the root instead.

### Gating CI on scaffold freshness

```bash
autumn upgrade --check
```

`--check` reconciles the scaffold files, writes nothing, prints the verdicts
without the per-file diffs (a build log is a poor place for the working
contents of `autumn.toml`), and exits **3** when anything has drifted — so a CI job can fail the build on a stale skeleton:

```yaml
- name: Scaffold is current
  run: autumn upgrade --check
```

It exits `0` on a clean project. A file you deliberately deleted is reported as
`removed` but does **not** hold the gate red — deleting it was a decision, and
a gate that can never go green again is a gate people delete.

`--check` cannot be combined with `--apply`; that is a usage error (exit `2`).
Run it outside an Autumn project — a directory with neither an `autumn.toml` nor
a `.autumn/scaffold.toml` — and it says so and exits `2` rather than reporting a
spurious pass.

`--json` works with `--check` too, and both emit the scaffold report under the
same `scaffold` key, so one `jq '.scaffold.drift'` works against either:

```json
{
  "scaffold": {
    "baseline": "0.7.0",
    "target": "0.7.0",
    "named": true,
    "has_manifest": true,
    "workspace_member": false,
    "outcome": "preview",
    "drift": true,
    "writable": 2,
    "written": 0,
    "conflicts": 1,
    "pinned": 0,
    "guide": "https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/0.7.0.md",
    "files": [
      { "path": "clippy.toml", "status": "add", "reason": "…", "applied": true }
    ]
  }
}
```

`writable` and `written` are different questions, the same split the app-code
report draws. `writable` is the plan — how many files `--apply` *would* write,
and `applied` on each file means the same thing. `written` is what actually
reached disk: `0` for a preview or a `--check`, the whole plan after a complete
apply, and only the prefix that landed after an interrupted one. Gate on
`written` when you care about the state of the working tree, and on `drift` when
you care whether there is work to do.

A run that could not render the scaffold at all reports `"named": false`, and
`--check` exits `2` rather than reporting `"drift": false` as though it had
looked.

### What it does not report

A file an *older* release generated and the current one no longer does is not
listed. The reconciler compares against the current scaffold, so a retired file
simply falls out of scope and stays on disk untouched. Retiring a scaffold file
is rare; when it happens the release's migration guide is where it is called
out.

### A whole upgrade, end to end

```bash
# 1. Start clean, so `git diff` means something.
git status

# 2. Preview both halves: code rewrites and scaffold drift.
autumn upgrade

# 3. Take them.
autumn upgrade --apply

# 4. Read what it did. `git checkout -- <path>` puts back a file it updated;
#    `rm <path>` removes one it added (git has never heard of that one).
git status
git diff

# 5. Work the conflicts it refused to touch, one file at a time. Each one's
#    diff is in the report above. Take this release's version where you want
#    it; where the file is deliberately yours, say so once and for all:
#      autumn upgrade --accept Dockerfile

# 6. Record what you resolved by hand. This writes no file content — the
#    conflicts you settled are now identical to the scaffold, so there is
#    nothing left to write — but it does add them to your baseline, which is
#    what makes the NEXT release able to update them for you automatically.
autumn upgrade --apply

# 7. Confirm the skeleton is current.
autumn upgrade --check

# 8. Now bump the library and build.
#    (The codemods migrate FROM the version Cargo.toml records, so this is last.)
cargo add autumn-web@0.7.0
cargo check

# 9. Read the release's migration guide for anything mechanical rewriting
#    could not do — the link is at the bottom of every report.
```

Only steps 3 and 6 write anything, and step 4 shows you everything step 3 did.

## Flags

| Flag | Effect |
|------|--------|
| `PATH` | Project directory to migrate (positional, defaults to `.`). |
| `--apply` | Write the rewrites. Without it the command only previews. |
| `--from VERSION` | Override the recorded `autumn-web` version. Needed when you already bumped the dependency, or when any manifest declares a requirement with no single floor — a git pin, a bare `*`, a multi-comparator range, or an upper bound like `"<0.6"`. A wildcard in a later position does have a floor and is read: `"0.5.*"` is `0.5.0`, the same as `"0.5"`. The root and every workspace member are read together (including `[target.'cfg(…)'.dependencies]`), the oldest floor wins, and one ambiguous declaration anywhere makes the whole answer a guess rather than being ignored. |
| `--to VERSION` | Upgrade to this release instead of the CLI's own version. |
| `--json` | Machine-readable report — the same content, for CI. |
| `--list-migrations` | Print the shipped codemods and exit, without scanning. |
| `--check` | Reconcile the scaffold files only, write nothing, print verdicts without diffs, and exit `3` if any have drifted. For CI. Cannot be combined with `--apply`. |
| `--accept PATH` | Record a framework-owned file as yours, so reconciliation leaves it alone. Repeatable. Writes only `.autumn/scaffold.toml`. |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | The scan completed. This includes a run that reported `manual` sites or skipped an unparsable file — both are in the report, and neither is a failure of the command. |
| `1` | The apply step failed partway through, or it wrote the files but could not record their new baseline in `.autumn/scaffold.toml` — in which case the files are correct and the next run will report them as conflicts until the manifest can be written. The report names the file it died on; the ones listed before it were already written. |
| `2` | A bad argument, a `PATH` that is not a readable directory, a version this command cannot parse, or a `--check` that could not look: outside an Autumn project, in one whose `Cargo.toml` gives no usable `[package] name`, or in one scaffolded by a release newer than this CLI. Nothing was scanned. |
| `3` | `--check` found scaffold drift. Its own code rather than `1`, so a CI job can tell "your skeleton is stale" from "the apply step died partway". |

There is no "found something" exit code: a preview that finds work is the
command working. Gate on the `--json` report's `manual`, `skipped`, and site
counts instead.

The report carries two site counts, because "what did this run plan" and "what
is on disk now" are different questions. `rewritten_sites` is the plan;
`written_sites` is the part of it that reached disk — equal after a complete
apply, zero for a preview, and only the files written before the failure after
a partial one. Gate on `written_sites` when you care about the state of the
working tree.

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
config file's `target-dir`, so when it is set no config redirect applies to
build output.

Vendored dependencies are excluded the same way. `cargo vendor third-party`
records the path in `[source.vendored-sources]`, and that directory is skipped
wherever it is — the `vendor` name is only Cargo's default, not the rule.

Hidden directories are skipped by name — `.git`, `.github`, `.cargo`, `.vscode`
and the like — not because they start with a dot. A dot-directory that holds
compiled source, say a `#[path = ".generated/repositories.rs"]` module, is
migrated.

The same exclusions decide which `Cargo.toml` files the version floor is read
from, so a crate whose sources are rewritten always gets a vote on which
migrations run.
