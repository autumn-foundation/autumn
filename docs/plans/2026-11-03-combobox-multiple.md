# Plan: combobox v2, multi-value tags mode (issue #3410)

Part of #3393, Phase 2. It extends `autocomplete_input`, so it stays in
the `widgets-search` family. It uses #3394 (repeated keys decode into
`Vec<T>`) and #3397 (keyboard core).

## Goal

Add a `multiple` mode to `autocomplete_input`. Each selected value shows
as a chip that the user can remove. Each chip submits one hidden
`name=value` input.

## Brainstorming

Ideas we examined:

1. `AutocompleteConfig::multiple()`. One config, one function. The single
   mode does not change.
2. The server renders the chips from `AutocompleteConfig::selected` pairs
   `(value, label)`. A chip needs a label, and an ID is not a label.
3. `autocomplete_input_for(changeset, …)` reads
   `Changeset::field_values` (#3394). It finds each label in
   `fallback_options`. Else the label is the value (tags mode).
4. A `<template>` holds the chip markup. The JS clones it. Thus the chip
   markup has one source: Rust.
5. A "create new" entry: `autocomplete_create_option(value, label)`. The
   handler renders it, because only the server knows if the tag exists.
   `free_text()` in multiple mode also adds the typed text on Enter.
6. The chip list is a roving-tabindex list (`data-autumn-roving`). It is
   one tab stop. The arrow keys move between the remove buttons.
7. Backspace in an empty input removes the last chip. ArrowLeft at the
   start of the input moves to the last chip. ArrowRight on the last chip
   moves back to the input.
8. The listbox hides an option whose value is already a chip.
9. No-JS fallback: `<noscript><select multiple>`. The selected values are
   `selected`. A value that is not in `fallback_options` gets its own
   option, so a resubmit keeps it.

## Reverse brainstorming

Question: "How can we make this fail?" Each answer gives a guard.

| Way to fail | Guard |
|---|---|
| With JS off, the chip inputs and the `<select>` both submit. Each value comes twice. | The server renders the chip inputs `disabled` and the chip list `hidden`. The JS enables them. A browser test with JS off checks it. |
| A JS-off user sees remove buttons that do nothing. | The chip list is `hidden` until the JS runs. |
| A label with `<script>` runs as code. | Maud escapes the server chips. The JS sets `textContent`, not `innerHTML`. A unit test checks the escape. |
| Enter on typed text submits the form in tags mode. | With `free_text`, Enter with no active option adds a chip and calls `preventDefault`. |
| The same tag is added two times. | `addChip` skips a value that is already a chip. A browser test checks it. |
| Focus goes to `<body>` when a chip is removed. | Keyboard removal moves focus to the next chip, else the previous chip, else the input. Pointer removal moves focus to the input. |
| A late htmx response shows an option that is already a chip. | `sync()` hides chosen options on each swap. |
| The "create" option puts "Create “x”" in the chip. | The option has `data-label`. The chip uses it. |
| RTL pages invert ArrowLeft and ArrowRight. | The JS reads `direction` and swaps the keys. |
| The JS grows past its budget. | The size test. The budget increases with the reason in the docs. |
| An empty selection posts nothing, and a strict struct fails. | The docs say `#[serde(default)]` on the `Vec<T>` field. |
| An `on*` handler breaks `script-src 'self'`. | All wiring is in `autumn-widgets.js`. The CSP browser test drives the new widget. |

## Six thinking hats

- **White (facts).** `autocomplete_input` holds one value. `free_text` is
  one value. `Changeset::field_values` and repeated-key decoding exist
  (#3394). The keyboard core has roving tabindex and active descendant
  (#3397). The JS budget has about 1 KB free.
- **Red (feeling).** Users expect tag inputs to feel like GitHub labels
  or Slack recipients: type, Enter, Backspace. A chip that does not go away
  causes frustration.
- **Black (risk).** Duplicate values with JS off. Focus loss on removal.
  Two views of one selection with JS off. A remove button below 24 px
  fails WCAG 2.5.8. The budget increase must be stated.
- **Yellow (value).** A free multi-select and tags input. Peers sell it
  as a paid add-on. It reuses the keyboard core and #3394, so the new code
  is small.
- **Green (alternatives).** The JS adds the "create" entry from a
  template. We did not do this: the client cannot know if a tag exists.
  `aria-multiselectable` on the listbox with the popup open after each
  pick. We did not do this: the server filters, and the query is cleared
  after each pick. A live region that says "Removed x". We keep this for a
  later issue (it needs new strings for each locale).
- **Blue (process).** RED: unit tests in `search.rs`, browser tests in
  `widgets_keyboard_browser.rs` and `stories_browser.rs`. See them fail.
  GREEN: Rust markup, JS, CSS, story and demo route. REFACTOR: share the
  option code of the two modes. Then docs, review and fixes.

## Scope

In scope: `multiple()`, `selected()`, `remove_label()`,
`autocomplete_input_for`, `autocomplete_create_option`, chip keyboard and
pointer removal, no-JS `<select multiple>`, CSS, a "Tags input" story,
unit and browser tests, docs, changelog fragment.

Out of scope: a live region that announces changes, a maximum chip
count, and field errors on the widget. Each can be a later issue.
