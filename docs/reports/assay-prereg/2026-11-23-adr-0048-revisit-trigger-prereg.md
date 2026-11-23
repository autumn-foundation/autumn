# Pre-registration: ADR 0048 revisit-trigger check (wire contracts, IDL at the edge)

Committed BEFORE the baseline-versus-now comparison. Read-only record check; no apparatus
beyond `git show`/`git log`. Non-production by construction.

**Question:** As of 2026-11-23, has either "When to revisit" trigger in
`docs/adr/0048-wire-contracts-idl-at-the-edge.md` fired?
1. Most extracted services move to separate repos with separate owners.
2. The "Limits of the first slice" list in `docs/guide/wire-contracts.md` keeps growing
   with items an IDL already solves (the ADR's examples: nested types, wire representation,
   query parameters).

**Decision / decider:** whether Keystone reopens ADR 0048 (IDL as source of truth at the
workspace edge). Decider: Keystone / maintainers.

**Prior art:** none in the assay ledger for ADR 0048 (ledger has 0012, 0013, 0014 checks).
ADR 0048 merged 2026-11-12 in #3703; this is its first check.

**Classification rule (fixed now):** a limits item counts as *IDL-solvable* if it is about
what a schema can express: type shape (nested/recursive types, enums/variants, path
parameter types, query parameters), wire representation (numeric width, decimal/number
form, custom serde forms, nullability, aliases), or streaming. Items about call-site or
build visibility (write-set detection, how a client is reached, which callers the bundle
sees, how many releases are compared, bundle format versions) do NOT count: an IDL does not
solve those either.

**Lines (fixed now):**
- Trigger 2 fires (pursue = recommend Keystone reopen) if, between the limits section at
  `928f6948` (ADR merge) and trunk-dev tip, **≥ 2 IDL-solvable items were added AND the net
  IDL-solvable count rose**.
- Kill (deferral stands) if the net IDL-solvable count did not rise.
- Exactly 1 added with a net rise: undetermined, recheck at next run.
- Trigger 1 fires if ≥ 1 workspace service that uses `#[endpoint]`/`#[event]` contracts has
  been moved to another repository since 928f6948 (evidence: removal from the workspace with a
  stated move). Otherwise false.

**Disclosure:** before writing this, I read the *current* limits list (not the baseline and
not the diff). The classification rule above was written to the ADR's own examples, not
tuned to that list.

**Time box:** this session (~1h). **Riskiest assumption:** that the list's edits are
attributable item-by-item (an item reworded vs added). Rule: a reworded item with the same
subject is not an addition. **Control:** the baseline list at 928f6948 under the same rule.
