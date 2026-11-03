# Plan: split widgets into feature-gated modules (issue #3408)

Part of #3393, Phase 0. An app must compile only the widgets it uses.

## Goal

Split `autumn/src/widgets.rs` (about 13,000 lines with tests) into modules.
Put four opt-in families behind Cargo features. Split `widgets.css` and
`autumn-widgets.js` the same way. Keep every `autumn_web::widgets::*` path.

## Brainstorming

1. Make `widgets.rs` a directory module: `widgets/mod.rs` plus `layout`,
   `nav`, `feedback`, `data`, `search`, `social`, `workflow` and `charts`.
2. `mod.rs` re-exports each module with `pub use <module>::*`. The modules
   stay private, so the public API gets no new paths.
3. Helpers that two modules use (`merge_class`, `heading`, `fnv1a_hash`,
   `assert_named`, `selector_to_id`) go in `mod.rs`. Each module has
   `use super::*`.
4. Four features: `widgets-search`, `widgets-social`, `widgets-workflow`,
   `widgets-charts`. Each one enables `maud`, because a widget makes
   `maud::Markup`. Each one is in `default`.
5. Family contents:
   - `widgets-search`: active search and autocomplete.
   - `widgets-social`: reaction controls, comment thread and the infinite feed.
   - `widgets-workflow`: transition controls and bulk actions.
   - `widgets-charts`: sparkline, bar chart and line chart.
6. The core modules (`layout`, `nav`, `feedback`, `data`) always compile with
   `maud`.
7. Each module keeps its own tests. A test goes with the family it tests.
8. CSS: `ui/widgets.css` keeps the core rules. Each family has its own file in
   `ui/widgets/`. `WIDGETS_CSS` joins the enabled files with `concat!`. It
   stays a `const`, and its URL does not change.
9. JS: the search runtime moves to `vendor/autumn-widgets-search.js`. It is a
   separate IIFE that binds on the `autumn:init` event (#3400). The bundle puts
   it before the core, so its listener exists before the core boots. The other
   families have no JS.
10. The story gallery registers a family's stories and demo routes only when
    the family is on.
11. The `#[commentable]` router renders `comment_thread`, so it needs
    `widgets-social`.
12. A new `widgets` lane in the `test-features` job runs `cargo check` for each
    of the 16 feature combinations.

## Reverse brainstorming

Question: "How can this split break an app or let a gap in?" Each answer
gives a guard.

| Way to fail | Guard |
|---|---|
| A path such as `autumn_web::widgets::card` stops compiling. | Glob re-exports. Existing tests and examples use the old paths. |
| A default build loses a widget. | Each family feature is in `default`. A spec test checks it. |
| A family does not compile alone, or without one other family. | The `widgets` CI lane checks all 16 combinations. |
| The story gate stops seeing a gated function. | The gate reads every file in `src/widgets/`. A feature-off file is checked against the story source text. |
| A new module file is not scanned. | The gate lists the directory. It does not name files. |
| A gated class keeps its CSS in the core file, so the split saves nothing. | A test checks that core CSS has no rule for a gated-only class. |
| A gated family's class has no rule in its own CSS file. | A test checks each family against its own file plus the core file. |
| The search JS runs after the core boots, so autocomplete does not start. | The search part is first in the bundle. A test checks the order. |
| The script is included two times, so autocomplete starts two times. | The search part returns when `window.__autumnWidgets` is set. |
| `default-features = false` with `maud` loses a widget the app uses. | Changelog `**Breaking:**` entry and a migration guide entry. |
| `commentable` does not compile without `widgets-social`. | Its render path is gated on `widgets-social`. The CI lane checks it. |
| A feature list in CI (semver, coverage, docs) drops a family. | Add the four features to each list. |

## Six thinking hats

- **White (facts).** `widgets.rs` has 9,000 production lines and 4,000 test
  lines. `widgets.css` has 3,428 lines. The search runtime is about 140 of
  1,071 JS lines. ADR 0027 set the pattern: default-on compile gates.
- **Red (feeling).** App authors want small builds without a new import path.
  Maintainers want small files.
- **Black (risk).** Apps that turn defaults off and use a gated widget break.
  Glob re-exports can hide a name clash. Moving tests can drop a test.
- **Yellow (value).** A minimal app compiles less code and ships less CSS and
  JS. Smaller files are easier to review.
- **Green (alternatives).** Public submodules (rejected: new API surface). A
  separate crate (rejected: ADR 0027 chose features). Separate JS URLs for
  each family (rejected: apps must then add script tags).
- **Blue (process).** SPEC: a spec test for the features, the module gates
  and the asset parts. RED: the spec fails on trunk. GREEN: move the code
  with a script, gate it, fix the build. REFACTOR: shared helpers in
  `mod.rs`. Then review from several angles and check each acceptance item.

There is no runtime invariant to prove, so there is no Verus spec. The spec is
the build matrix and the spec test.

## Acceptance

- Every feature combination in the `widgets` lane passes `cargo check`.
- The story gate sees every public function, also behind a feature.
- The PR has the before and after compile times.
- `widget-styling.md` and the feature list show the new features.
