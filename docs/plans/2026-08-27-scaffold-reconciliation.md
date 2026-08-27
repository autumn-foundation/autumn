# `autumn upgrade` scaffold reconciliation (issue #1593)

Plan for reconciling framework-owned project files across releases. Written
before any code, in the order the thinking actually happened: divergent
options, then a deliberate hunt for failure modes, then Six Hats to settle the
trade-offs, then the design and the TDD slices.

---

## 1. Brainstorming — how could this work at all?

Options generated, no filtering:

1. **Regenerate a throwaway project and diff it.** Shell out to `autumn new` in
   a temp dir, diff the tree. Zero new template plumbing.
2. **Render the current templates in memory and compare.** Same idea without
   the temp dir; needs `new.rs` to expose per-file rendering.
3. **Ship every historical scaffold inside the CLI**, reconstruct the app's
   original files from its recorded version, and do a real three-way merge.
4. **Record a digest per scaffolded file at `autumn new` time** and use it as
   the merge base: unchanged digest ⇒ safe to overwrite, changed ⇒ conflict.
5. **Git-native**: require a clean tree, write everything, let `git diff` be the
   conflict UI.
6. **Interactive per-file prompt** like `rails app:update` (`Y/n/d/q`).
7. **Emit a patch file** the user applies with `git apply --3way`.
8. **`.autumn/scaffold.toml` manifest** carrying version + flavor + per-file
   digests, committed to the repo.
9. **Marker comments** in generated files (`# autumn:managed`) so edited
   regions are detectable inline.
10. **A `--check` CI gate** that exits nonzero on drift.
11. **Fold it into the existing `autumn upgrade`** so file reconciliation and
    codemods are one run.
12. **A separate `autumn scaffold sync` command.**

Combining: **2 + 4 + 8 + 10 + 11** is the shortest path to every acceptance
criterion. (3) is the only option that gives true three-way merges, but the CLI
does not carry historical templates and adding them means a permanently growing
binary; (4)'s digest baseline answers the only question that actually matters —
"did the developer touch this?" — for a fraction of the cost. (5) and (6) are
rejected below.

## 2. Reverse brainstorming — how do we make this a disaster?

Deliberately listing the ways this feature could hurt someone, then the
mitigation that goes into the design.

| How to ruin it | Mitigation in the design |
|---|---|
| Silently clobber a hand-tuned `Dockerfile` | Overwrite only when the on-disk bytes still match the digest Autumn recorded. Anything else is a **conflict**: reported, never written. |
| Clobber files in a project that predates the manifest | No baseline ⇒ cannot prove "untouched" ⇒ every differing file is a conflict. Best-effort still adds files that are simply missing. |
| Rewrite the developer's application code | The framework-owned set is a fixed allowlist with **no `src/` entry**, enforced by an assertion and a test. |
| Write on a bare invocation | Preview is the default; `--apply` is the only writer. |
| Half-write the tree and die | Plan everything in memory first; re-read and compare each file immediately before writing so a file changed underneath us is refused, not overwritten. |
| Restore a file the developer deleted on purpose | A path recorded in the manifest but absent on disk is **removed**, reported and skipped, not re-added. |
| Two sources of truth for "what the scaffold is" | `autumn new` and `autumn upgrade` render from **one** function. If they could disagree, they eventually would. |
| Manifest becomes a liability (merge conflicts, drift, secrets) | Deterministic key order, digests only, no paths outside the project, and the file is optional — deleting it degrades precision, never correctness. |
| Silent success in CI | `--check` exits `3` on drift, with the drift listed. |
| Break the existing `autumn upgrade` | Scaffold reconciliation activates only in a directory that is actually an Autumn project (`autumn.toml` or `.autumn/scaffold.toml`); existing codemod behaviour and exit codes are unchanged. |
| Dead-end the user after the report | Summary links the release's migration guide and states the revert path (`git diff`, `git checkout --`). |
| An interactive prompt in CI | No prompts at all: preview → review → `--apply`. The terminal is not the conflict-resolution UI; the VCS is. |

## 3. Six Thinking Hats

**White (facts).** `autumn new` writes ~12 framework-owned files outside `src/`
plus user-owned ones (`Cargo.toml`, `README.md`, `src/**`, tests, credentials,
vendored JS). Template selection depends on exactly two flags for this file
set: `--api` (drops `tailwind.config.js` + `static/css/input.css`, swaps
`Dockerfile`/`build.rs`/CI) and `--with-i18n` (injects into `autumn.toml` and
the `Dockerfile`); `--daemon`/`--bundled-pg` append to `autumn.toml`. `autumn
upgrade` already exists for codemods, already resolves from/to versions from
`Cargo.toml`, and its module doc already reserves this slot for #1593.

**Red (gut).** The scary part is the write. A tool that touches `Dockerfile`
must feel *conservative*; a single unexpected overwrite ends its adoption. The
"conflict" outcome should feel like the normal one, not a failure.

**Black (risks).** Digests are a weak baseline: reformatting on save, CRLF
checkouts, or an `.editorconfig` trailing-newline rule flip every digest and
turn a clean upgrade into an all-conflict wall. Mitigated by normalising CRLF
before hashing (the renderer already normalises), and by the fact that a
conflict is only ever *more* work, never data loss. `autumn.toml` will be a
perpetual conflict for most real apps because it is a config file people edit —
acceptable and honest. Flavor inference for legacy projects can be wrong; since
legacy projects can never reach the `update` state, a wrong guess costs an
unwanted `add` offer, which the preview shows before anything is written.

**Yellow (upside).** The reconciler and the generator share one renderer, so
this is also a de-duplication of `new.rs`. `--check` makes scaffold freshness a
CI-gateable property — nothing in the Rust web ecosystem has that. The manifest
is a foundation later work can build on (per-file opt-out, three-way merge).

**Green (creative).** The manifest can be updated on a successful apply so the
next upgrade's baseline is exact. Diff rendering is already written
(`upgrade::diff`), and unlike the codemods these diffs *do* change line counts —
so the existing "line counts differ" branch finally earns its keep, and the
renderer becomes worth extracting properly.

**Blue (process).** Ship it in TDD slices, each red → green: file set, manifest,
classification, reporting, CLI wiring, docs. Then a multi-angle review, then
the AC evidence table.

---

## 4. Design

### Command surface

```
autumn upgrade                 # preview: codemods + scaffold drift
autumn upgrade --apply         # write both
autumn upgrade --check         # scaffold drift only; exit 3 if any
autumn upgrade --json          # both reports, machine-readable
```

Scaffold reconciliation runs only when the root is an Autumn project — it has
`autumn.toml` or `.autumn/scaffold.toml`. Elsewhere `autumn upgrade` behaves
exactly as it did.

### Framework-owned set (the allowlist)

Common: `autumn.toml`, `Dockerfile`, `.dockerignore`, `build.rs`, `.gitignore`,
`.env.example`, `.github/workflows/ci.yml`, `rust-toolchain.toml`,
`rustfmt.toml`, `clippy.toml`.
Fullstack only: `tailwind.config.js`, `static/css/input.css`.

Deliberately **not** owned: `src/**` (out of bounds per the issue),
`Cargo.toml`, `README.md`, `tests/**`, `migrations/**`, `i18n/**`,
`config/credentials/**`, vendored `static/js/**`.

### Provenance — `.autumn/scaffold.toml`

```toml
version = "0.7.0"
flavor = "fullstack"
i18n = false
daemon = false
bundled_pg = false

[files]
"clippy.toml" = "sha256 hex"
```

Written by `autumn new`, refreshed by `autumn upgrade --apply`.

### Classification

| Status | Condition | Applied? |
|---|---|---|
| `up-to-date` | on disk, bytes equal the current template | – |
| `add` | not on disk | yes |
| `update` | differs, and bytes still match the recorded digest | yes |
| `conflict` (edited) | differs, and the recorded digest does not match | **no** |
| `conflict` (no baseline) | differs, and nothing was recorded | **no** |
| `removed` | recorded, absent on disk | **no** |

### Exit codes

`0` fine · `1` apply failed partway · `2` bad usage · `3` `--check` found drift.

## 5. TDD slices

1. **Red:** `new::framework_owned_files` unit tests (per-flavor sets, no `src/`
   path, injections applied). **Green:** extract from `generate_inner`.
2. **Red:** manifest round-trip + digest tests. **Green:** `Provenance`.
3. **Red:** classification matrix. **Green:** `classify`.
4. **Red:** report rendering + JSON. **Green:** renderers.
5. **Red:** end-to-end integration tests against the real binary (preview,
   apply, conflict, legacy project, `--check`, `src/` untouched).
   **Green:** CLI wiring.
6. **Refactor:** de-duplicate, tighten names, document.
7. Docs guide + docs-smoke step + CHANGELOG + README.
