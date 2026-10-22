# ADR 0023: `destroy` Checks That Recorded Edits Are Still Reverted

- Status: Accepted
- Date: 2026-10-22
- Deciders: Autumn maintainers
- Tags: generators, destroy, provenance

## Terms

- **Edit**: a change that `generate` makes to a shared file.
- **Revert**: the planned action that undoes an edit.
- **Anchor**: the stored description of an edit. It has a path and a revert
  text.

## Context

`autumn destroy` builds the plan again from the current inputs. It accepts an
owned file by digest when the identity of the `generate` run matches (issue
#1835). The identity holds the arguments, the `autumn.generate.toml`
fingerprint, and the database backend.

A generator can read other project config. The scaffold reads `[i18n]`. If
`dir` changes, the identity still matches. The owned files go. The
`I18nFtlKeys` revert now targets the new bundle path, so the old bundle keeps
its keys. This is a partial destroy (issue #2560).

Each earlier fix added one more input to the identity. The issue lists four
sub-shapes:

| Sub-shape | Direction | Example |
| --- | --- | --- |
| Input the arguments do not show | too narrow | `[i18n]` locale and dir |
| Argument with no effect on the plan | too wide | `--import` on `--live` |
| Identity depends on uncommitted state | unstable | `.env`, environment |
| Argument order | too wide | `--api --soft-delete` |

## Options

1. Fingerprint all of `autumn.toml`. Any edit to it makes every baseline
   invalid.
2. Fingerprint the sections that generators read. The list can become wrong
   with no warning.
3. Each generator declares its inputs. A missing declaration gives no error.
4. Check the result: `destroy` must not leave a recorded edit behind.
5. Replace the identity with the parsed options. Every new option field makes
   every old baseline invalid.

## Decision

Option 4. The identity stays. It still stops one command from claiming the
output of another.

`generate` stores the anchors of the run in `.autumn/generated.toml`, under
`[[runs.<id>]]`. An anchor holds the target path and the values that name the
edit. It holds no template text, such as `expected_block`, because template
text changes with the CLI version. A list value gives one anchor for each item,
because `destroy` plans more route entries than `generate` wrote.

`destroy` refuses when an anchor meets all three conditions:

1. Its target file still exists.
2. The new plan has no equal anchor.
3. The new plan has an anchor of the same revert kind.

Condition 3 separates a **moved** revert from a **vanished** one. A moved
revert means an input changed (`[i18n] dir`). A vanished revert means the
generator skipped it because of file state. Example: the append of a
submit-token exemption is a no-op when the exemption exists. A skip is normal
and is not a refusal.

`--force` overrides. A dry run reports the refusal and exits with success.
Only a plan that claims to be the sole resource of its process records or
checks anchors (see `0021-destroy-orphan-sweep.md`). The CLI always claims. Two
plans in one test process share one invocation, and their edits must not count
against each other. After a `destroy` finishes, the run is forgotten.

`Revert::anchors` is an exhaustive `match`. A new variant does not compile
until it has an anchor.

### Rule: use only state that travels with the repository

Neither the identity nor the anchors read uncommitted state. The plan reads
files on disk, and extra reverts are allowed, so a file that exists only on one
machine can add a revert but cannot remove one. The `.env` input stays in the
backend part of the identity. A changed `.env` can only cause a refusal. It
cannot cause an unsafe delete.

## Consequences

- The `[i18n]` case is fixed. Any later input of the same kind is also covered,
  if the edit has a `Revert`.
- **Limit:** an edit with no `Revert` variant is not covered. Examples: the
  `Dockerfile` copy lines and the `[i18n]` table in `autumn.toml` from the
  scaffold, the `autumn.toml` blocks from `auth`, and all edits from
  `introspect`. `destroy` never reverts them. A new `plan.modify` without a
  `Revert` gives no warning.
- **Limit:** an anchor holds no template text. A setting that changes only the
  content of an edit, not its path or name, is not seen.
- `destroy model` then `destroy admin` still works. Anchors compare plans, not
  file content.
- A translator who adds a locale bundle does not block `destroy`. The plan
  reverts more, not less.
- A moved target file does not block `destroy`. If the old file is gone, there
  is no edit to leave.
- Identities that are too wide (`--import`, argument order) only cause extra
  refusals. This ADR does not change them.
- A manifest from an older CLI has no anchors. It keeps the old behaviour until
  the next `generate` of that resource.
- A later `generate` with the same invocation adds its anchors to the run. After
  a setting change and a new `generate`, the edits at the old path still need
  `--force` or a manual check. The run goes when `destroy` finishes or when its
  last file goes.
