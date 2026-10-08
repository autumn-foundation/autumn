# Edge Path Helpers and Embed Builds (issue #1790, third pass)

> **Status: executed.** Prior work: `2026-08-19-edge-capsule-first-slice.md`,
> `2026-10-05-edge-node-and-ttfb.md`, PR #3130 and PR #3152.

## Gap audit

| AC | State before this plan | Gap |
| --- | --- | --- |
| AC-1 one `autumn build`, no separate codebase | `autumn build --embed` stops when the app has `#[edge]` routes. | An embed build needs a second command. |
| AC-2 byte-identical, in CI | Done. Tiers A to E. | None. |
| AC-3 fallthrough, no glue | Done. `autumn edge serve`. | None. |
| AC-4 identical handler source at origin and edge | A `paths::*` helper of an `#[edge]` route calls `::autumn_web`. The capsule does not compile it. | The author must change the handler for the edge. |
| AC-5 actionable refusal | Done. | None. |

## Brainstorming

1. Emit the path helper of an `#[edge]` route with `::autumn_edge::paths`
   encoders. Do not gate it off wasm.
2. Move the encoders to a new shared crate.
3. Inline the encoder in each macro expansion.
4. Re-export `paths![]` and `PathExt` from `autumn-edge`.
5. Build the capsule after the embed build.
6. Pass `embed-assets` to the capsule build too.
7. Add an edge route to the example that uses a path helper.

Selected: 1, 4, 5, 6, 7. Item 2 adds a crate for two functions. Item 3 makes
each expansion larger.

## Reverse brainstorming ("how do we make this fail?")

| Failure we could cause | Prevention |
| --- | --- |
| The edge encoder and the origin encoder differ. A link changes by lane. | One test compares both encoders on the same corpus. |
| A non-edge route expansion changes. | The codegen test for a plain route stays as it is. |
| The helper of a non-edge route goes to `autumn_edge`. Apps without that crate break. | Only an `#[edge]` route gets the new helper. |
| The `name = "..."` alias stays gated. `paths![fn_name]` fails on wasm. | The alias gets the same gate as its helper. |
| The embed build passes features the capsule does not get. A gated route is lost. | Both builds get `embed-assets` plus the user features. |
| `embed-assets` names a native-only dependency. The wasm build fails. | Resolver 2 drops it for wasm. CI builds it. |
| The embed build skips the capsule with no message. | The capsule step prints its artifact line. |
| A debug `--embed` build makes a capsule with no request. | The plan is the same as without `--embed`: debug needs `--edge`. |

## Six thinking hats

- **White (facts):** The helper calls `::autumn_web::paths::encode_*`. The
  edge macro companion already names `::autumn_edge` on all targets.
  `build_embedded` returns before the edge step.
- **Red (feeling):** "Same source" is false while a link helper breaks the
  capsule.
- **Black (risks):** Two copies of the encoder can drift. An embed build of a
  large app takes longer.
- **Yellow (value):** The handler source does not change for the edge. One
  command makes the embed binary and the capsule.
- **Green (ideas):** Exercise the helper in the conformance corpus. Add a
  CI step for `autumn build --embed`.
- **Blue (process):** Red tests first, then green, then refactor. Update the
  guide, the plan of record and a changelog fragment.

## Design

- `autumn_edge::paths`: `encode_path_segment`, `encode_catch_all_param`,
  `PathExt`. The same algorithm as `autumn_web::paths`.
- `autumn_edge::paths!`: a re-export of the `paths![]` macro.
- The route macro: for an `#[edge]` route, the helper and its alias are not
  gated. They call `::autumn_edge::paths`.
- `autumn build --embed`: build the capsule after the embed build, with
  `embed-assets` added to the features.
- `examples/edge-greeting`: a `/link/{name}` edge route that renders a path
  helper with a query. It enters the corpus and Tier D.

## TDD order

1. Red: codegen tests, `autumn-edge` encoder tests, an encoder parity test,
   `plan_edge_step` tests, the CLI embed test, the example route.
2. Green: `autumn_edge::paths`, the macro change, the build change.
3. Refactor: docs, changelog fragment, CI step.
