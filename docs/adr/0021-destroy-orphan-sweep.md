# ADR 0021: `destroy` Sweeps Files the Templates No Longer Name

- Status: Accepted
- Date: 2026-10-21
- Deciders: Autumn maintainers
- Tags: generators, destroy, provenance, security

## Context

`autumn destroy` builds the plan again from the current templates. It deletes
only paths that plan names. When a CLI upgrade renames or drops a generated
file (`old.rs` → `new.rs`), `destroy` skips `new.rs` (it does not exist) and
does not see `old.rs`. The file and its `.autumn/generated.toml` entry stay
(issue #2557).

The manifest records a digest and the invocation for each file. A sweep can
use it. But a sweep deletes paths that come only from the manifest, which is
committed project content. Also, the invocation identifies one resource only
when one process applies one plan. Some tests generate two resources in one
process with one argv.

## Options

Brainstorm of the resource identity:

1. Invocation only. Wrong when one process applies many plans.
2. A name that each generator sets (`job:Foo`). About 25 call sites, and a
   missed generator loses the sweep.
3. A nonce for each run, anchored by an overlap with the current plan. Fails
   when the upgrade renames every file.
4. **The CLI declares that its plan is the sole resource of its invocation.**
   One claim in each process. Other callers do not claim.

Reverse brainstorm (how can the sweep delete the wrong file?):

- A sibling resource with the same argv. → Only a claimed plan sweeps, and
  only entries that a claimed plan wrote.
- A shared `CreateIfAbsent` file. → Recorded as `shared`. Never swept.
- A migration. → Never swept. The applied-migration guard stays the only
  path to removal.
- A developer edit. → Digest mismatch. Warn and keep, also with `--force`.
- A crafted key: `../x`, `/etc/x`, `C:x`, `a\b`, `x.`, `PROGRA~1`, a symlinked
  directory. → Refused with a warning.
- A forged entry for a real file (`README.md`, `.env`, `Cargo.toml`). → A dot
  segment, `target/` and the project files are protected. An owned file is
  deleted only in a directory where the current plan also creates a file.
- A renamed module whose `pub mod foo;` line stays (the reverts come from the
  current templates). → A `.rs` file that any other Rust file still names
  (its module name as a whole word, or its file name) is kept. No parser:
  comments, attributes and macros can stand between `mod` and the name.
- Another spelling of one file (case, Windows aliases). → Each segment must be
  the exact name on disk. A case-only match counts as the plan's own file
  only when both paths reach one directory entry (see Decision 5).
- An entry from 0.8.0 (no ownership). → Cannot tell owned from shared. Warn
  and keep.

Six hats:

- White: the CLI applies one plan for each process. 0.8.0 shipped entries
  with no ownership.
- Red: deletion from manifest data is a risk. Delete only what is proven
  safe. Name every kept file in a warning.
- Black: the cases above.
- Yellow: `destroy` reverses `generate` completely, and it reports what it
  cannot remove.
- Green: record `scope = "owned" | "shared"` only under a claim.
- Blue: test first. Unit tests on the sweep, engine tests, CLI tests.

## Decision

1. `Entry` gets an optional `scope`. A claimed plan writes `owned` for
   `Create`/`CreateBytes` and `shared` for `CreateIfAbsent`. Other plans
   write no scope. An unknown value reads as no scope.
2. `Plan::claim_sole_resource` marks the plan as the sole resource of its
   invocation. A process-wide flag allows one claim. The CLI dispatcher
   calls it.
3. A claimed `revert` sweeps entries with the same invocation that no action
   or revert of the plan names:
   - file missing: drop the entry.
   - migration that the plan matches by suffix: leave it to the migration
     pass.
   - other migration, protected file, unsafe path: warn and keep.
   - `shared` or no scope: warn and keep.
   - `owned`: delete when the digest matches, the plan creates a file in the
     same directory, and no other Rust file names it. Otherwise warn and keep.
4. When `destroy` refuses (diverged files, no `--force`), the sweep removes
   nothing. `--dry-run` lists the sweep. Before it changes anything,
   `destroy` checks each swept file again. If one changed, it refuses, because
   the dependency reverts assumed that file was gone.

5. Case-only spellings (issue #3382). Two keys that differ only in ASCII
   case are one file when both paths reach one file and the directory does
   not list both names (`spelling::one_entry`). This is true on a
   case-insensitive file system. A hard link or a symlink is two entries.
   The rule applies to:
   - the sweep's check that the plan names a file;
   - the sweep's check that the plan creates a file in the orphan's
     directory (`src/widgets` and `src/Widgets`);
   - the manifest lookups `is_ours`, `was_written` and `forget`. A template
     that changes `Foo.rs` to `foo.rs` then finds the `Foo.rs` entry.
     `destroy` reads the keys before it deletes a file, and drops them after,
     because the rule needs the file on disk.

   A case-sensitive host keeps exact matching. Non-ASCII case stays exact.

## Consequences

- Projects generated by 0.8.0 get a warning, not a deletion, for a stranded
  file. The warning names the file, so the developer can delete it.
- A rename into a new directory, or a renamed module, gives a warning, not a
  deletion.
- The manifest cannot be authenticated. A forged `owned` entry can still
  name an unedited file in a directory where the plan creates files, and
  that no other Rust file names. `destroy` prints each swept file as
  `Removed <path> (no longer generated)`, and git can restore it. Review
  changes to `.autumn/generated.toml` like code.
- Tests that build many plans in one process do not sweep. A sweep test sets
  `sole_resource` on its plan directly.
- A new caller that applies one plan in its own process can call
  `claim_sole_resource`. A caller that applies many must not.
