# Plan: styled select with optgroup and multiple (issue #3409)

Part of #3393, Phase 2, in core. Depends on #3394 (multi-value decoding).
It is merged.

## Goal

Extend `select_input`. Add `<optgroup>`, `multiple`, and an opt-in style
with `appearance: base-select`. Use no JavaScript. The native element keeps
the keyboard and the accessibility tree.

## Brainstorming

1. `multiple` is an HTML attribute, so it goes in `FieldOptions`:
   `FieldOptions::multiple()`. One flag works on `select_input_with`,
   `grouped_select_input_with` and `form_for` (`field_options`). It also
   applies to `<input type="file">`, where HTML allows it.
2. A multiple select reads `Changeset::field_values` (#3394). It marks each
   option whose value is in the list. Bind it to a `#[serde(default)]
   Vec<T>`.
3. A single select marks only the first option that matches. Two options
   with one value do not make two `selected` options.
4. Groups need a new data shape. `grouped_select_input(…, choices, groups)`
   takes top-level `choices` (for example a blank `("", "Choose…")`) and
   `groups` as `(label, &[(value, label)])`. `FieldControl::GroupedSelect`
   carries the same data for `form_for`.
5. The style is opt-in: `FieldOptions::styled_select()` adds the
   `autumn-select` class. The CSS puts `appearance: base-select` inside
   `@supports (appearance: base-select)`. A browser without support (Firefox)
   keeps the native control with the field style.
6. A multiple select keeps the native list box. The base-select rule skips
   `[multiple]`.
7. A Verus spec proves the selection rule. A proptest checks the rendered
   markup.
8. One story, "Select", shows the styled select, the native fallback, the
   groups and the multiple select. The axe sweep checks it in both schemes.

## Reverse brainstorming

Question: "How can this select break a form?" Each answer gives a guard.

| Way to fail | Guard |
|---|---|
| A single select marks two options. The browser shows the last one. | Mark the first match only. Verus proves it. A proptest checks it. |
| A match in one group and a match in a later group are both marked. | One `found` flag for the whole select. Verus proves it for concatenated groups. |
| A multiple select loses the chosen values after a 422. | It reads `field_values`. A prefill test. |
| `multiple` on a `String` field: the second value is a 422. | Docs: bind a `Vec<T>`. The error names the field. |
| A required multiple select from the model forces one or more values on a `Vec` field. | `form_for` ignores the model flag for a multiple select, as for a checkbox group. |
| An optgroup label with `"` breaks the attribute. | Escape it. A test. |
| `::picker(select)` in a selector list drops the whole rule in Firefox. | Each `::picker` rule stands alone, inside `@supports`. |
| `appearance: base-select` on a list box changes nothing, or breaks it. | The rule skips `[multiple]`. A browser test reads the computed style. |
| The picker has light colors in the dark scheme. | The picker sets `color` and `background-color` from tokens. axe runs in both schemes. |
| The styled select needs JS, so it fails with JS off. | Only CSS. The story has no `data-autumn-*` hook. |
| A new class has no CSS rule. | `widget_css_coverage` fails. |
| The picker icon turns with motion for a user who asked for less. | The turn is only in `prefers-reduced-motion: no-preference`, so no `reduce` rule can lose on specificity. A test. |
| A disabled styled select shows a pointer cursor. | The pointer is only on `:enabled`. |
| A widget-pack select drops `.multiple()`. | `FieldContext::control` writes `multiple` on a `select` or a file `input`. A test. |
| The hovered option is not visible in forced colors. | A `forced-colors` rule uses `Highlight`. |

## Six thinking hats

- **White (facts).** `select_input` writes through `render_field`.
  `FieldControl` is `#[non_exhaustive]`. HTML parses a single select with two
  `selected` options and keeps the last. Chrome 135+ and Safari 27 support
  `appearance: base-select` on a drop-down select. A browser that does not
  know `::picker(select)` drops the rule.
- **Red (feeling).** An app author wants one flag to make a select look
  good, and no new mental model. A grouped select must look like a select.
- **Black (risk).** CSS for a new browser feature can change. Verus is not
  installed in this session, so CI checks the proof. A changed attribute
  order can break a test that matches a string.
- **Yellow (value).** The gap analysis in #3393 found a styled select in
  five of six styled suites. This
  one has native keyboard, native a11y and no JS. Optgroup and multiple close
  the "select is single-value only" gap.
- **Green (alternatives).** A JS listbox (rejected: the issue says no JS
  listbox). `multi_select_input` helpers (rejected: one `FieldOptions` flag
  covers every select and `form_for`). A `Select` builder (rejected: the
  other helpers are plain functions). Styled by default (rejected: the issue
  asks for opt-in).
- **Blue (process).** SPEC: a Verus model of the selection rule. RED: tests
  for optgroup, multiple, prefill, the styled class, `form_for`, the story
  and the CSS. GREEN: the smallest code that passes. REFACTOR: one option
  writer for flat and grouped choices. Then a review from several angles and
  an acceptance check.

## API

| Item | Effect |
|---|---|
| `FieldOptions::multiple()` | `multiple` on a `<select>` or a file input. The select marks each value of `field_values`. |
| `FieldOptions::styled_select()` | The `autumn-select` class on a `<select>`. |
| `grouped_select_input(…, choices, groups)` | A select with top-level options, then one `<optgroup>` for each group. |
| `grouped_select_input_with`, `required_grouped_select_input` | As for the other helpers. |
| `FieldControl::GroupedSelect { options, groups }` | The same select in `form_for`. |
