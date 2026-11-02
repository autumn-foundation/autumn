# Plan: native input helpers (issue #3405)

Part of #3393, Phase 1. Depends on #3394 (multi-value decoding) and #3395
(field wrapper v2). Both are merged.

## Goal

Add changeset helpers and `FieldControl` variants for the native inputs that
are missing. Each one uses the field wrapper v2 (hint, required marker,
passthrough). Map new scaffold DSL kinds onto them.

## Brainstorming

1. Single inputs use the one writer, `render_field`. Each new input is a new
   internal `FieldKind`: `Email`, `Tel`, `Url`, `Time`, `Month`, `Color`,
   `Range`, `Switch`.
2. A switch is a checkbox with `role="switch"`. It has the same `bool`
   binding and the same `#[serde(default)]` rule.
3. A group (radio, checkbox, segmented) uses the `<fieldset>` and `<legend>`
   wrapper that a grouped custom control (#3400) already has. The `aria-*`
   links go on the `<fieldset>`.
4. Each group item has `id="{field}-{index}"`. The index, not the value,
   goes in the id, so a value with spaces or quotes cannot break the id.
5. A checkbox group reads `Changeset::field_values` (#3394), so the boxes
   stay checked after a 422.
6. A segmented control is radios with other classes. The radio is
   visually hidden but stays focusable, so the native arrow keys work.
7. `hidden_input` has no label, no wrapper and no error block.
8. Values are normalized for the browser: `time` to `HH:MM[:SS[.sss]]`,
   `month` to `YYYY-MM`, `color` to lowercase `#rrggbb`.
9. `form_for` gets one arm per new variant. The `#[model]` derive maps
   `NaiveTime` to `FieldControl::Time`.
10. Scaffold DSL: `time` (`NaiveTime`, `TIME`), `phone` (alias `tel`) and
    `color` (both `String`, `TEXT`), and `money` (alias of `decimal{12,2}`).
11. Two stories: "Native inputs" and "Choice controls". The axe sweep over
    `/_stories` checks them in the light and the dark scheme.
12. A Verus spec proves that item ids do not collide.

## Reverse brainstorming

Question: "How can these helpers break a form?" Each answer gives a guard.

| Way to fail | Guard |
|---|---|
| Two radios of one group get one id. | The index is in the id. Verus proves the id is injective. |
| An item id equals `{field}-hint` or `{field}-error`. | The last part of an item id is digits only. Verus proves it. |
| A required checkbox group puts `required` on each box, so the user must check all boxes. | A checkbox group never writes `required` on a box. It shows the marker only. A test proves it. |
| `aria-required` on a radio fails axe (`aria-allowed-attr`). | Radios get `required` only. A test proves it. |
| `required` on `range`, `color` or `hidden` is not valid HTML. | These controls drop `required`. A test proves it. |
| A `time` value with nanoseconds (`chrono`) is not a valid HTML time. | Normalize to at most 3 fraction digits. Add `step` when seconds show. |
| An uppercase or bad color value. | Lowercase a valid `#rrggbb`. Drop any other value. |
| A checked box is lost after a 422. | Prefill tests for each group. |
| `display: none` on the segmented radio removes it from the keyboard and the a11y tree. | Use the clip pattern. A browser keyboard test presses the arrow keys. |
| A switch is not visible in forced-colors mode. | A `forced-colors` rule draws the thumb with system colors. |
| The model `required` flag makes a switch, range, color or checkbox group "must be set". | `form_for` ignores the model flag for them, as it does for a checkbox. |
| A new class has no CSS rule. | `widget_css_coverage` fails. Add the rules. |
| A scaffold sample literal is wrong for a `TIME` column. | Exhaustive matches. Add a literal and a test for each new kind. |

## Six thinking hats

- **White (facts).** `FieldControl` is `#[non_exhaustive]`. `render_field`
  writes every core field. `render_custom_field` writes the grouped
  `<fieldset>`. axe-core 3.50 treats `aria-invalid` as global. chrono parses
  `HH:MM` and `HH:MM:SS` for `NaiveTime`. The HTML spec lets `required`
  apply to no `range`, `color` or `hidden` input.
- **Red (feeling).** An app author wants one call per control, with the same
  shape as `select_input`. Surprises (a dropped attribute) must be in the
  docs.
- **Black (risk).** Many new public functions. A scaffold kind touches many
  exhaustive matches. Verus is not installed in this session, so CI runs the
  proof. The browser gate needs Chromium.
- **Yellow (value).** Switch, slider and radio are universal in the styled
  suites. Scaffolds get correct controls for time, phone, color and money.
- **Green (alternatives).** Format modifiers (`string{tel}`) instead of DSL
  kinds (rejected: the issue asks for kinds). A JS switch (rejected: native
  checkbox is enough). A `Money<C>` scaffold kind (rejected: it needs a
  currency choice; a `decimal{12,2}` alias is enough now). `week` (left out:
  Firefox and Safari do not support it).
- **Blue (process).** SPEC: Verus model of the item ids. RED: tests for each
  helper (happy path, prefill, error wiring), the groups, `form_for`, the
  derive and the scaffold. GREEN: the smallest code that passes. REFACTOR:
  share the group writer. Then a review from several angles, and an
  acceptance check.

## API

| Helper | `_with` | `required_` | `FieldControl` |
|---|---|---|---|
| `switch_input` | yes | yes | `Switch` |
| `range_input(…, step)` | yes | no | `Range { step }` |
| `radio_group(…, choices)` | yes | yes | `Radio { options }` |
| `checkbox_group(…, choices)` | yes | no | `CheckboxGroup { options }` |
| `segmented_control(…, choices)` | yes | yes | `Segmented { options }` |
| `email_input` | yes | yes | `Email` |
| `tel_input` | yes | yes | `Tel` |
| `url_input` | yes | yes | `Url` |
| `time_input` | yes | yes | `Time` |
| `month_input` | yes | yes | `Month` |
| `color_input` | yes | no | `Color` |
| `hidden_input(changeset, field)` | yes | no | `Hidden` |
