# 🛣️ Onramp: the documented CRUD scaffold no longer fails its own generated CI (clippy -D warnings: red → green)

## 🎯 Journey

**First real integration** (hello world → their use case) — specifically
`docs/guide/generators.md`'s "Five commands to a working CRUD app", the
guide the README's own "Quickstart → scaffold a resource" line points at.
This is the second of Onramp's three tracked journeys (first run / first
real integration / upgrade), and the *exact* command in that guide —
`autumn generate scaffold Post title:String body:Text published:bool` — is
reproduced verbatim below.

Reproduce: see **🔬 Reproduce** below.

## 📈 Evidence

**Tier 1, deterministic, 100% reproducible.** A clean-room run of the
existing `scripts/check-quickstart.sh` harness (install → new → setup →
build → serve → scaffold → scaffold-build, against the published
`autumn-cli`/`autumn-web` 0.7.0 crates.io release) completed successfully
through `cargo build` — no build failure. But `cargo build` is not the bar a
real user's project holds itself to: `autumn new` ships every project with
its own `.github/workflows/ci.yml` (rendered from
`autumn-cli/src/templates/.github/workflows/ci.yml.tmpl`), whose lint step
is `cargo clippy --all-targets -- -D warnings`. Running that exact command
against the freshly scaffolded project:

```
error: unused import: `autumn_web::reexports::serde_json`
error: unused import: `UpdatePost`
error: unused variable: `name`
error: using `clone` on type `bool` which implements the `Copy` trait
error: use of `default` to create a unit struct
error: use of `default` to create a unit struct
error: could not compile `my-app` (bin "my-app") due to 6 previous errors
```

Six errors, from code the generator itself wrote — not from anything the
tutorial asked the user to type. A search of open and closed issues/PRs
found no prior report of this (see **Prior work** below); it does not close
a question-log ≥3× pattern, it clears the impact floor on its own:

> The clean-room run fails today and passes after — any hard failure on the
> documented path clears the floor.

The generated project's own CI red-ing on the user's first push, immediately
after following the getting-started tutorial verbatim and committing, is
exactly that kind of hard failure — it just surfaces one layer downstream of
`cargo build`, in the generated project's own gate rather than in the
tutorial's own steps.

**Weight of the journey:** `docs/guide/generators.md` is the CRUD/database
half of the getting-started flow the README's own Quickstart section links
to, and the scaffold generator is the only command in the base tutorial that
writes substantial hand-generated Rust source a user will actually look at
(every other quickstart step touches config/TOML or scaffolding-free
boilerplate).

## 💡 Hypothesis

The scaffold generator's routes.rs template (`autumn-cli/src/generate/
scaffold.rs`, `render_routes_file`) was written to always emit certain
imports/constructs regardless of which optional flags (`--i18n`, `--live`)
or field shapes (nullable vs. required, `bool`/numeric vs. other kinds) were
actually used — because each one IS needed under *some* combination, nobody
noticed it goes unused under the *default* combination, which happens to be
exactly the combination the documented example exercises (no flags, every
field required, one `bool` field):

1. `use autumn_web::reexports::serde_json;` is unconditional, but the only
   place it's referenced in the whole `render_routes_file` output is inside
   the i18n-only `delete_confirm_js` binding (`labels.enabled()`) — unused
   whenever `--i18n` is off (the default).
2. `Update{Model}` is imported unconditionally, but only *constructed* by
   `update_stmt`'s `--live` branch (`render_update_changeset_expr`) — the
   default, non-`--live` path writes updates through a raw
   `diesel::update(...).set((...))` column tuple
   (`render_update_columns`) that never names the type.
3. `is_nullable_form_field(name: &str)` always names its parameter `name`,
   but `render_nullable_field_match` renders a bare `false` (never reading
   `name`) whenever the model has no nullable or synthetic
   constrained-required-numeric field — true for every field in the
   documented example, since a first scaffold's fields are usually all
   required.
4. `render_update_columns` always emits `.clone()` on every field being
   moved out of the owned `new: New{Model}` local, even for a `Copy` field
   (`bool`/`i32`/`i64`/`f32`/`f64`) where the clone is a same-cost copy
   through a different name — `clippy::clone_on_copy`.
5. `policy_registration_call`/`scope_registration_call` always emit
   `{Model}Policy::default()` / `{Model}Scope::default()`, but both are
   generated as true unit structs (`pub struct {Model}Policy;`) — clippy's
   `default_constructed_unit_structs` prefers the bare literal.

**Falsifiable question:** does the documented scaffold command, run with no
optional flags, build with zero warnings after gating each of these five
emissions on the condition that actually uses them?

## 🔧 Change

Layer: **the API shape** (generator-template output only — no change to
`autumn-web`'s public API, no renamed identifier, no new config option; the
generated project's own template output is additive/corrective, not a
published crate's semver surface). All five fixes are conditional emission
at generation time, in `autumn-cli/src/generate/scaffold.rs` and
`autumn-cli/src/generate/schema_edit.rs`:

1. `serde_json_import` — gated on `labels.enabled()` (mirrors the existing
   `i18n_imports` pattern already in the same function).
2. `update_model_import` — gated on `live` (mirrors the existing
   `enum_import_suffix` pattern).
3. `nullable_form_field_param` — `"name"` when any field is nullable or a
   constrained-required-numeric, `"_name"` otherwise (same predicate
   `render_nullable_field_match` already uses to decide its own body).
4. `is_copy_field_kind` — a new, deliberately narrow predicate (`Bool | I32
   | I64 | F32 | F64` only — no attempt to reason about `Uuid`/chrono/
   `Decimal`'s `Copy`-ness); `render_update_columns` skips `.clone()` only
   for those kinds.
5. `policy_registration_call`/`scope_registration_call` and their
   `is_policy_registration_line`/`is_scope_registration_line` matcher
   counterparts (used by `autumn destroy` to find and remove the
   registration) drop `::default()` consistently on both sides.

Every branch this doesn't take (`--i18n`, `--live`, a nullable field, a
`String`/`Uuid`/etc. field) is unchanged — verified by compiling all three
branches (see **📊 Measurement**).

## 📊 Measurement

**Before → after, `cargo clippy --all-targets -- -D warnings` on the
documented example** (`Post title:String body:Text published:bool`, no
flags), patched to build against this change:

| | Before | After |
|---|---|---|
| Errors | 6 | 0 |
| `cargo build` warnings on the generated crate itself | 3 | 0 |
| Generated project's own CI (`ci.yml.tmpl` lint job) | red | green |

**Branch coverage** (every conditional this change adds, compiled for real,
not just unit-string-matched):

| Scaffold | `serde_json` import | `Update{Model}` import | `is_nullable_form_field` param |
|---|---|---|---|
| documented example (no flags) | absent (correct — unused) | absent (correct — unused) | `_name` (correct — unused) |
| `--i18n` | present (correct — used by `delete_confirm_js`) | — | — |
| `--live` | — | present (correct — `UpdatePost { .. }` constructed) | — |
| a nullable field added | — | — | `name` (correct — `matches!` reads it) |

Each of the four rows above was scaffolded fresh (against the in-tree
`autumn-web` via `[patch.crates-io]`) and `cargo check`ed to a clean pass —
the documented-example and `--live` rows are also now regression-guarded by
`autumn-cli` unit tests (`execute_writes_a_routes_file_referencing_model`,
`live_scaffold_routes_file_imports_update_model`,
`scaffold_with_nullable_field_names_the_form_field_param`).

**Compatibility:** `cargo test -p autumn-cli --bin autumn` — 7532 passed, 0
failed. `cargo test -p autumn-cli --test cli_tests` — 971 passed, 0 failed,
127 ignored (pre-existing Docker/slow-generator set). `cargo check
--workspace` clean. `cargo fmt -p autumn-cli -- --check` clean. No
`autumn-web` public API touched — this is entirely `autumn-cli`'s generator
output.

**New harness test**
(`integration::scaffold_validation::documented_scaffold_example_builds_without_warnings`,
`#[ignore]`d, wired into `.github/workflows/generator-conformance.yml`
alongside the other named `cli_tests` generator checks): scaffolds the
literal documented example against the in-tree `autumn-web` and asserts the
three rustc-lint warning strings this report's mechanism names are absent.
Confirmed RED against the pre-fix generator (`git stash` the three source
files, re-run — fails with exactly the three warnings above) and GREEN
after (`git stash pop`, re-run — passes). The two clippy-only lints
(`clone_on_copy`, `default_constructed_unit_structs`) are not harnessed by
this test — see the test's own doc comment for why (a blanket `RUSTFLAGS=-D
warnings` or `cargo clippy` run against a `[patch.crates-io]`-path-patched
`autumn-web` also fails on `autumn-web`'s own pre-existing, unrelated
warnings, which a real user building against the published crate never
sees, since path dependencies don't get cap-lints treatment) — they were
verified with a real `cargo clippy --all-targets -- -D warnings` run
instead (**📊 Measurement** table above; command in **🔬 Reproduce**).

## Prior work

Searched open and closed issues/PRs for "scaffold generator warnings",
"unused import" in generated code, `is_nullable_form_field`, `UpdatePost`
unused, and generator-template-cleanliness. No prior report found — this
looks like the first time this specific defect was surfaced. The closest
neighboring open issues (#2328, #2186, #2555) are unrelated scaffold
generator defects (Cargo.toml feature drift on regeneration, destroy
stripping unrelated features, SQLite generator deferrals) — none about
build warnings.

## 🔬 Reproduce

```bash
# From the repository root.
cargo build -p autumn-cli

rm -rf /tmp/onramp-repro && mkdir -p /tmp/onramp-repro && cd /tmp/onramp-repro
/path/to/repo/target/debug/autumn new my-app
cd my-app
printf '\n[patch.crates-io]\nautumn-web = { path = "/path/to/repo/autumn" }\n' >> Cargo.toml
/path/to/repo/target/debug/autumn generate scaffold Post title:String body:Text published:bool

# The generated project's own CI runs exactly this:
cargo clippy --all-targets -- -D warnings
# Before this change: 6 errors. After: 0 (only pre-existing, unrelated
# autumn-web warnings surface, and only because of the local path-patch —
# see the harness test's doc comment above).

# Branch coverage (run each against a fresh `autumn new`):
autumn generate scaffold Post title:String body:Text published:bool --i18n   # serde_json import present
autumn generate scaffold Post title:String body:Text published:bool --live  # UpdatePost import present
autumn generate scaffold Post title:String subtitle:Option<String>          # is_nullable_form_field(name: &str)

# The regression-guarding harness test:
cargo test -p autumn-cli --test cli_tests \
  integration::scaffold_validation::documented_scaffold_example_builds_without_warnings \
  -- --ignored --exact
```
