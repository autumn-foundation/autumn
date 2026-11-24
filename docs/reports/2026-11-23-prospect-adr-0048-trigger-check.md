# ⛏️ Prospect: has ADR 0048's revisit trigger fired? (kill: +0 IDL-solvable limits vs ≥2 line)

## 🎯 Question

**Falsifiable:** as of 2026-11-23, has either "When to revisit" trigger in
`docs/adr/0048-wire-contracts-idl-at-the-edge.md` fired?
1. Most extracted services move to separate repos with separate owners.
2. The "Limits of the first slice" list in `docs/guide/wire-contracts.md` keeps growing with
   items an IDL already solves (nested types, wire representation, query parameters).

**Why now:** the guide's limits section was edited by #3716 (2026-11-13) and #3737
(2026-11-16), both after the ADR merged (#3703, 2026-11-12). Those edits are nested-type
work, one of the ADR's named examples.

**Decision fed:** whether Keystone reopens ADR 0048. **Decider:** Keystone / maintainers.

## ⚖️ Pre-registration

`docs/reports/assay-prereg/2026-11-23-adr-0048-revisit-trigger-prereg.md`, commit `b9937c51`,
written before the baseline-to-now diff. Pursue: ≥ 2 IDL-solvable items added and a net rise.
Kill: no net rise. One added with a net rise: undetermined. The rule for "IDL-solvable" was
fixed in the same commit: what a schema can express (type shape, query parameters, wire
representation, streaming) counts. Call-site or build visibility (read-sets, write-sets, how a
client is reached) does not count, because an IDL does not see what the Rust code reads.
Disclosure: I had read the current list before writing the rule, but not the baseline or the diff.

## 🔍 Prior art

There is no earlier check of ADR 0048 in the ledger. This is its first.

## 🧪 Apparatus

`git show 928f6948:docs/guide/wire-contracts.md`, an awk that pulls out the section, and
`diff`. **Stubs/limits:** this compares the guide text only. A limit that exists in the code
but is not written in the guide is not seen.

## 📊 Assay

| Measure | At 928f6948 (ADR merge) | trunk-dev tip |
|---|---|---|
| Bullets in "Limits of the first slice" | 32 | 37 |
| Hunks changed | | 1 (#3716, #3737) |

The diff, classified under the pre-registered rule:

| Change | Item | IDL-solvable? |
|---|---|---|
| removed | read-set is known at the top level only (`item.details.color` reads `details`) | no: call-site read-set |
| added | nested read seen through field access, patterns, bindings, maud `@let`/`@match`/`@for`, `Option`/`Result` callbacks | no: call-site read-set |
| added | a loop or index over a nested list keeps the path | no: call-site read-set |
| added | another method call (`first()`, `iter().map`) stops the path | no: call-site read-set |
| added | macro-body bindings hold the value only for `x`, `Some(x)`, `Ok(x)` | no: call-site read-set |
| added | a field with more than one nested type (`Envelope<Payload>`) gives each type the same reads | no: read attribution. Protobuf and Avro have no generics either. |
| added | nested write-set known only for an inline literal or `Some(literal)` | no: call-site write-set |

IDL-solvable items: **+0 added, −0 removed, net 0**. The control is the baseline list under
the same rule. It holds about 9 IDL-solvable items (streaming, query parameters, `any` from
custom serde, opaque `#[wire(type)]`, decimal form, integer width, `Option` alias, `ty`
alias, last-name lookup), and none of them changed.

The list grew by 5 bullets net, but the growth comes from the nested-type work removing
one coarse limit and writing down the exact edges of its replacement. Nested types are now
handled better. What remains are code-analysis edges that an IDL would not see either.

**Trigger 1:** `[workspace] members` is the same at 928f6948 and at the tip. The two
workspace apps that declare contracts (`examples/mesh-catalog`, `examples/reddit-clone`)
are still in the tree. 0 services have moved out, so trigger 1 is false.

## 🏁 Verdict

**Kill (the deferral stands).** Trigger 2: +0 IDL-solvable items against a line of ≥ 2.
Trigger 1: 0 services moved. The ADR stands as written.

**Watch item, not a trigger:** the nested work removed a limit an IDL *does* frame (nested
types) by solving it in Rust. If later wire-contract PRs add representation items (enum
variants, path parameter types, query parameters) instead, trigger 2 moves. Recheck when the
limits section next changes, or at the next Prospect run after 2026-12-07.

## 💰 Cost to productionize

N/A (not a pursue verdict).

## 🔬 Reproduce

```bash
ext(){ awk '/^## Limits of the first slice/{f=1;next} f&&/^## /{exit} f'; }
diff <(git show 928f6948:docs/guide/wire-contracts.md | ext) <(ext < docs/guide/wire-contracts.md)
diff <(git show 928f6948:Cargo.toml | awk '/^members/,/\]/') <(awk '/^members/,/\]/' Cargo.toml)
git log --format='%h %ad %s' --date=short 928f6948..HEAD -- docs/guide/wire-contracts.md
```
