# Buttons, loading states and small display widgets

`autumn_web::widgets` has a set of small widgets (issue #3406). Each one
renders native HTML with `autumn-*` classes. The stylesheet uses only the
design tokens. None of them needs JavaScript, except the htmx busy state.

| Widget | Element |
|---|---|
| `button` | `<button>` with a variant, a size, an icon and a loading state |
| `button_group` | `role="group"` of buttons that touch |
| `spinner` | `role="status"` ring with a visually hidden label |
| `skeleton` | `aria-hidden` placeholder: line, block or circle |
| `progress` | native `<progress>` |
| `meter` | native `<meter>` |
| `separator` | `<hr>`, or a vertical `role="separator"` |
| `kbd`, `kbd_combo` | `<kbd>`, and nested `<kbd>` for a key combination |
| `empty_state` | title, text and action for a list with no data |
| `avatar_group` | avatars that overlap, with a `+N` item |

A textarea can also grow with its text: see [Auto-grow textarea](#auto-grow-textarea).

The `/_stories` gallery shows each widget. See [Widget Stories](stories.md).

## Button

```rust
use autumn_web::widgets::{ButtonConfig, ButtonSize, ButtonVariant, button};

let save = button("Save", &ButtonConfig::new().submit());
let delete = button(
    "Delete",
    &ButtonConfig::new()
        .variant(ButtonVariant::Destructive)
        .size(ButtonSize::Small),
);
```

- **Variants:** `Primary` (the default), `Secondary`, `Ghost`,
  `Destructive` and `Link`.
- **Sizes:** `Small`, `Medium` (the default) and `Large`. With a compact
  density, a small button is 24 px high: the WCAG 2.5.8 minimum.
- **Type:** the default is `type="button"`. Use `.submit()` or
  `.kind(autumn_web::a11y::ButtonType::Reset)`.
- **Icon:** `.icon(markup)` puts the icon before the label. The icon is
  `aria-hidden`.
- **Icon only:** `.icon_only()` hides the label visually. The label stays
  in the button, so it is still the accessible name.
- **Attributes:** `.attrs(&[("hx-post", "/save")])` adds attributes. The
  widget drops `on*` and `hx-on*` handlers and `style`.

The label is required. A debug build panics on an empty label.

The classes also style a link. Use them with `link_to_with` or
`button_to_with`:

```rust
use autumn_web::links::{LinkToOptions, link_to_with};

let docs = link_to_with(
    "Read the docs",
    "/docs",
    &LinkToOptions::new().class("autumn-button autumn-button--secondary autumn-button--md"),
);
```

### Loading state

There are two loading states:

- `.loading()`: the server knows that work is in progress. The button gets
  `aria-busy="true"`, `disabled` and a spinner.
- `.loading_on_request()`: the button is busy during an htmx request. It
  gets `data-autumn-busy` and a hidden spinner.

```rust
use autumn_web::widgets::{ButtonConfig, button};

let save = button(
    "Save",
    &ButtonConfig::new()
        .loading_on_request()
        .attrs(&[("hx-post", "/posts/7"), ("hx-target", "#post")]),
);
```

htmx adds the `htmx-request` class to the element that sends a request,
or to the elements that `hx-indicator` names. The spinner shows while the
button, or an element around it, has that class. Thus it shows in these
cases:

- The button sends the request.
- The form of the button sends the request.
- `hx-indicator` names the button, for example `hx-indicator="#save"`.
  Set the id with `.id("save")`.

`autumn-widgets.js` sets `aria-busy="true"` on each `[data-autumn-busy]`
element for the same time. htmx 2 does not set `aria-busy`. The runtime
removes it after the request and after the settle step.

The spinner covers the label, so the width does not change. CSS hides the
icon and the label with `opacity: 0` only. They stay in the accessibility
tree, so the button keeps its name.

To stop a second click, add `hx-disabled-elt="this"` to the button.

## Button group

```rust
use autumn_web::widgets::{ButtonConfig, ButtonGroupConfig, ButtonVariant, button, button_group_with};

let secondary = ButtonConfig::new().variant(ButtonVariant::Secondary);
let zoom = button_group_with(
    "Zoom",
    &[button("Zoom in", &secondary), button("Zoom out", &secondary)],
    &ButtonGroupConfig::new().vertical(),
);
```

The label names the group (`aria-label`). It is required.

## Spinner

```rust
use autumn_web::widgets::{SpinnerConfig, SpinnerSize, spinner, spinner_with};

let loading = spinner("Loading results");
let small = spinner_with("Saving", &SpinnerConfig::new().size(SpinnerSize::Small));
```

The label is the text that a screen reader reads. With
`prefers-reduced-motion: reduce`, the ring does not turn. It fades in and
out slowly. In forced colors mode, the ring uses system colors.

Do not put a spinner in a button. Use `ButtonConfig::loading_on_request`.

## Skeleton

```rust
use autumn_web::widgets::{SkeletonConfig, SkeletonShape, skeleton, skeleton_with};

let avatar = skeleton(SkeletonShape::Circle);
let text = skeleton_with(SkeletonShape::Line, &SkeletonConfig::new().count(3));
```

- `Line` fills the width. With more than one line, the last line is
  shorter.
- `Block` is for an image or a card.
- `Circle` is avatar size. Circles go in a row.

A skeleton is `aria-hidden`. Tell screen readers about the wait in a
different way, for example with a spinner. With
`prefers-reduced-motion: reduce`, the skeleton does not pulse.

## htmx indicators

A spinner or a skeleton can be an htmx indicator. `.indicator(id)` sets
the id and the `autumn-indicator` class. The element is hidden until htmx
gives it, or an element around it, the `htmx-request` class:

```rust
use autumn_web::widgets::{SkeletonConfig, SkeletonShape, skeleton_with};
use maud::html;

let page = html! {
    button type="button" hx-get="/posts" hx-target="#posts" hx-indicator="#posts-loading" {
        "Reload"
    }
    (skeleton_with(
        SkeletonShape::Line,
        &SkeletonConfig::new().count(3).indicator("posts-loading"),
    ))
    div id="posts" {}
};
```

The `autumn-indicator` class does not use the htmx indicator styles. Those
styles are an inline `<style>`, which a strict CSP can block.

## Progress and meter

Use `progress` for a task that completes. Use `meter` for a measurement in
a known range.

```rust
use autumn_web::widgets::{MeterConfig, ProgressConfig, meter_with, progress, progress_with};

let upload = progress("Upload", Some(40.0), 100.0);
let loading = progress("Loading", None, 1.0); // indeterminate
let import = progress_with(
    "Import",
    Some(3.0),
    8.0,
    &ProgressConfig::new().visible_label().value_text("3 of 8 files"),
);
let disk = meter_with(
    "Disk use",
    7.0,
    0.0..=10.0,
    &MeterConfig::new().low(5.0).high(8.0).optimum(0.0).visible_label(),
);
```

- The label is the accessible name (`aria-label`). It is required.
- `.visible_label()` shows the label and the value above the bar. That
  text is `aria-hidden`, because the bar has the same name and value.
- `.value_text(text)` shows `text` in place of the percent. It is also the
  `aria-valuetext`.
- The widget clamps each value to its range. A `NaN` progress value gives
  an indeterminate bar.
- The meter fills use `--success`, `--warning-text` and `--danger`. Each
  has 3:1 contrast with the track in both schemes.

## Separator

```rust
use autumn_web::widgets::{SeparatorConfig, separator, separator_with};

let line = separator(); // <hr class="autumn-separator">
let bar = separator_with(&SeparatorConfig::new().vertical());
let look = separator_with(&SeparatorConfig::new().decorative());
```

A vertical separator is `role="separator"` with
`aria-orientation="vertical"`. A decorative one is `role="none"`.

## Kbd

```rust
use autumn_web::widgets::{kbd, kbd_combo};

let esc = kbd("Esc");
let search = kbd_combo(&["Ctrl", "K"]);
```

`kbd_combo` puts one `<kbd>` for each key in an outer `<kbd>`. This is the
HTML pattern for keys that you press together.

## Empty state

```rust
use autumn_web::links::link_to;
use autumn_web::widgets::{EmptyStateConfig, HeadingLevel, empty_state_with};

let action = link_to("New post", "/posts/new");
let empty = empty_state_with(
    "No posts yet",
    "Write your first post.",
    Some(&action),
    &EmptyStateConfig::new().level(HeadingLevel::H3),
);
```

The title is a heading. The default is `<h2>`. Set the level one below the
heading that holds the empty state. An empty body gives no paragraph.

## Avatar group

```rust
use autumn_web::widgets::{AvatarConfig, AvatarGroupConfig, avatar, avatar_group_with};

let names = ["Ada", "Grace", "Linus"];
let avatars: Vec<_> = names.iter().map(|n| avatar(n, &AvatarConfig::new())).collect();
let likes = avatar_group_with(
    "Liked by",
    &avatars,
    &AvatarGroupConfig::new().max(3).total(120).more_label("{count} more"),
);
```

- `max` is the most items that show, the `+N` item included. The default
  is 5. With more avatars, the group shows `max - 1` avatars and `+N`.
- `total` sets the number of people when you give fewer avatars.
- The `+N` item is `role="img"`. Its name comes from `more_label`, where
  `{count}` becomes the number. Translate it there.

## Auto-grow textarea

`FieldOptions::auto_grow()` lets a textarea grow with its text. It uses
`field-sizing: content`:

```rust
use autumn_web::form::{Changeset, FieldOptions, textarea_input_with};

#[derive(serde::Serialize)]
struct Profile { bio: String }

let changeset = Changeset::new(Profile { bio: String::new() });
let bio = textarea_input_with(&changeset, "bio", "Bio", &FieldOptions::new().auto_grow());
```

It also works on `rich_text_area_with`, the nested-form textarea and
`a11y::TextArea::auto_grow()`. The textarea is at least three lines high
and at most half the screen. A browser with no `field-sizing` keeps the
`rows` height.

## CSS hooks

| Selector | Element |
|---|---|
| `.autumn-button`, `--primary` … `--link`, `--sm` … `--lg`, `--icon` | Button and its modifiers |
| `.autumn-button__icon`, `__label`, `__spinner` | Button parts |
| `.autumn-button-group`, `--vertical` | Button group |
| `.autumn-spinner`, `--sm` … `--lg`, `__icon` | Spinner |
| `.autumn-skeleton`, `--line`, `--block`, `--circle`, `__item` | Skeleton |
| `.autumn-indicator` | Hidden until an htmx request |
| `.autumn-progress`, `.autumn-meter` | The bars |
| `.autumn-progress-field`, `__header`, `__label`, `__value` | The visible label of a bar |
| `.autumn-separator`, `--vertical` | Separator |
| `.autumn-kbd`, `.autumn-kbd-combo` | Keys |
| `.autumn-empty-state`, `__icon`, `__title`, `__body`, `__action` | Empty state |
| `.autumn-avatar-group`, `__more` | Avatar group |
| `.autumn-auto-grow` | Auto-grow textarea |

To change a color, set a token. See [Widget Styling](widget-styling.md).
