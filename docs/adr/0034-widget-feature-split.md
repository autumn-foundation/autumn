# ADR 0034: Split Widgets Into Feature-Gated Modules

- Status: Accepted
- Date: 2026-11-03
- Deciders: Autumn maintainers
- Tags: build-time, features, widgets

## Context

Issue #3408. `autumn/src/widgets.rs` was one file of about 13,000 lines. Only
`maud` gated it. An app that used `card` and `nav_bar` compiled every widget.
Every page got all of `autumn-widgets.css` and `autumn-widgets.js`.

ADR 0027 gates rarely used modules behind default-on features. This ADR uses
the same rule for widget families.

## Decision

- `widgets` is a directory module. The modules are `layout`, `nav`,
  `feedback`, `data`, `search`, `social`, `workflow` and `charts`.
- The modules are private. `widgets/mod.rs` re-exports each one with a glob,
  so every `autumn_web::widgets::*` path stays the same.
- Four families are opt-in:

  | Feature | Module | Widgets |
  |---|---|---|
  | `widgets-search` | `search` | active search, autocomplete |
  | `widgets-social` | `social` | reaction controls, comment thread, infinite feed |
  | `widgets-workflow` | `workflow` | transition controls, bulk actions |
  | `widgets-charts` | `charts` | sparkline, bar chart, line chart |

- Each feature enables `maud` and is in `default`.
- The `#[commentable]` router renders `comment_thread`. It needs
  `widgets-social`.
- CSS: core rules stay in `ui/widgets.css`. Each family has a file in
  `ui/widgets/`. `WIDGETS_CSS` joins the enabled files.
- JS: the search runtime is `vendor/autumn-widgets-search.js`. It binds on
  `autumn:init`. The served bundle puts it before the core. The URL does not
  change.
- The story gallery registers a family's stories and demo routes only when
  the family is on.

## Consequences

- A default build does not change.
- An app with `default-features = false` must name the families it uses.
  The migration guide lists them.
- The `widgets` lane of the `test-features` CI job checks all 16 family
  combinations.
- The story gate reads every file in `src/widgets/`. A new module is seen
  without a gate edit.
- A plugin `autumn:init` listener on an element must not stop propagation.
  If it does, the search runtime does not see that root.
