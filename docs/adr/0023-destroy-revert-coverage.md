# ADR 0023: `destroy` Checks That Every Recorded Revert Is Still Planned

- Status: Accepted
- Date: 2026-10-22
- Deciders: Autumn maintainers
- Tags: generators, destroy, provenance

## Context

`autumn destroy` builds the plan again from the current inputs. It accepts an
owned file by digest when the identity of the `generate` run matches (issue
#1835). The identity holds the arguments, the `autumn.generate.toml`
fingerprint, and the database backend.

A generator can read other project config. The scaffold reads `[i18n]`. If
`dir` changes, the identity still matches. The owned files go. The
`I18nFtlKeys` revert now targets the new bundle path, so the old bundle keeps
its keys. This is a partial destroy (issue #2560).

Each fix so far added one more input to the identity. Four sub-shapes are
known:

| Sub-shape | Direction | Example |
| --- | --- | --- |
| Input the arguments cannot see | too narrow | `[i18n]` locale and dir |
| Argument with no effect on the plan | too wide | `--import` on `--live` |
| Identity depends on uncommitted state | unstable | `.env`, environment |
| Argument order | too wide | `--api --soft-delete` |

## Options

Brainstorm:

1. Fingerprint all of `autumn.toml`. Closes the class. Any port edit drops
   every baseline.
2. Fingerprint the sections generators read. A list that goes stale in
   silence.
3. Each generator declares its inputs. Precise. A missed generator is silent.
4. Check the consequence: a revert that no longer reaches an edit that
   `generate` made.
5. Replace the identity with the parsed options. Every new option field
   drops every old baseline.

Reverse brainstorm (how does option 4 fail?):

- A revert that does nothing is normal after an earlier `destroy` removed the
  shared entry (`destroy model` then `destroy admin`). A "matched nothing"
  test cannot tell this from "never matched". → Do not test matches. Compare
  what the plan targets with what `generate` targeted.
- Reverts depend on disk state (a locale bundle that a translator added). →
  Allow extra reverts. Block only a lost one.
- A recorded target file is gone (the user moved `i18n/`). → Nothing is left
  to strand. Allow it.
- A template change alters a revert parameter. → The old edit still sits in
  the shared file. Refuse. `--force` overrides.
- An old manifest has no record. → Fall back to the identity. No new refusal.

Six hats:

- White (facts): reverts are typed data. `Revert` has 21 variants.
- Red (feel): users distrust a destroy that stops half way.
- Black (risk): a false refusal costs `--force`. A false accept costs lost
  data. Prefer the refusal.
- Yellow (gain): no list of inputs. No generator work. A newer CLI can add
  reverts without dropping old baselines.
- Green (new idea): record the revert targets, not the inputs.
- Blue (process): keep the identity. It still blocks another command from
  claiming a file. Add one check beside it.

## Decision

`generate` records the revert **anchors** of the run in
`.autumn/generated.toml` (`[[runs.<id>.reverts]]`). An anchor is the project
path and a stable description of the revert. It leaves out text that comes
from templates, such as `expected_block`.

A revert with a list parameter gives one anchor for each item. `destroy` plans
more route entries than `generate` wrote, and that must not count as a change.

`destroy` refuses when a recorded anchor meets both conditions:

1. Its target file still exists.
2. The current plan has no equal anchor.

`--force` overrides. The refusal names each anchor. Only a plan that claims to
be the sole resource of its process records or checks anchors (see ADR 0021).
The CLI dispatcher always claims. Two plans in one test process share one
invocation, and their edits must not count against each other.

`Revert::anchor` is an exhaustive `match`. A new variant does not compile
until it is described. No list can go stale in silence.

### Rule: identity uses only state that travels with the repository

Neither the identity nor the anchors may read uncommitted state. The anchors
come from the plan, and the plan comes from committed files. The `.env` and
environment input stays in the backend part of the identity. It is lost
tolerance, never an unsafe delete.

## Consequences

- The `[i18n]` case is closed. So is every later input of that shape, with no
  change to the identity.
- `destroy model` then `destroy admin` still works. Anchors compare plans, not
  file content.
- A translator who adds a locale bundle does not block `destroy`.
- Too-wide identities (`--import`, argument order) are not fixed. They lose
  tolerance only. They are out of scope, and a later change can relax the
  identity now that the anchor check covers the safety job.
- A manifest from an older CLI has no anchors. It keeps the old behaviour
  until the next `generate` of that resource.
- A later `generate` with the same invocation adds its anchors to the record.
  It does not replace them. An earlier run made its edits, and they stay in the
  files. After a setting change and a new `generate`, the edits at the old
  path still need `--force` or a manual check. The record goes when the last
  file of the run goes.
