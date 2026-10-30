# Widget Packs

A widget pack is a plugin crate that adds widgets. Core keeps the small
widgets that all apps use. Put a large widget (date picker, data table,
charts, command palette) in a pack. An app compiles only the packs it uses.

Name a first-party pack `autumn-<name>-plugin`. Name a third-party pack
`autumn-plugin-<name>`.

## The widget contract

A pack widget obeys these rules. The rules are stable. A change to them is a
breaking change.

| Rule | What to do |
|---|---|
| Render | Implement `maud::Render`, or return `maud::Markup`. The markup works with JavaScript off. |
| Passthrough | Each widget takes `&FieldOptions`. A form input binds with `FieldRenderer`. Another widget writes its root with `widget_pack::element`. |
| Tokens | Pack CSS uses only the public tokens and its own `--autumn-<pack>-*` properties. It has no raw color. It does not set a public token. |
| Class names | A new class is `autumn-<pack>-*`. A widget can also use a core class, for example `autumn-field`. |
| Keyboard | Pack JS uses the keyboard core attributes and the `autumn:init` event. It has no arrow-key code of its own. |
| CSP | No inline `<script>` or `<style>`, no `on*` or `hx-on*` attribute, no `style` attribute, no `eval`. |
| Stories | Each widget has a story. Each story slug starts with `<pack>-`. |

The public tokens are in [Widget styling](widget-styling.md#design-tokens).

## Make a pack

1. Put the CSS and JS in a `PluginAssets` bundle. Use the pack name as the
   namespace:

   ```rust
   use autumn_web::assets::PluginAssets;

   pub static ASSETS: PluginAssets = PluginAssets::from_files(
       "swatch",
       &[
           ("swatch.css", include_bytes!("swatch.css")),
           ("swatch.js", include_bytes!("swatch.js")),
       ],
   );
   ```

2. Make the `WidgetPack`. Add one story for each widget:

   ```rust
   use autumn_web::form::FieldOptions;
   use autumn_web::stories::story;
   use autumn_web::widget_pack::WidgetPack;

   pub fn pack() -> WidgetPack {
       WidgetPack::new("swatch")
           .extend([story! {
               "Swatch",
               "Swatch toolbar",
               { swatch_toolbar("Highlight colors", &FieldOptions::new()) }
           }])
           .assets(&ASSETS)
   }
   ```

3. Install the pack from `Plugin::build`:

   ```rust
   impl Plugin for SwatchPlugin {
       fn build(self, app: AppBuilder) -> AppBuilder {
           app.widget_pack(pack())
       }
   }
   ```

   `AppBuilder::widget_pack` serves the bundle and adds the stories to the
   `/_stories` gallery. The gallery links the pack CSS and JS. If you install
   a pack two times, the second install does nothing.

4. In the app layout, link the pack assets after the core assets:

   ```rust
   head {
       link rel="stylesheet" href=(autumn_web::ui::WIDGETS_CSS_PATH);
       script src="/static/js/autumn-widgets.js" defer {}
       (swatch::pack().head())
   }
   ```

   `WidgetPack::head` writes a `<link>` for each `.css` file and a
   `<script defer>` for each `.js` file, with SRI hashes.

## Stories

`AppBuilder::with_story_gallery` adds to the gallery. Each call adds its
stories. Start a plugin gallery from `StoryGallery::new()`, not
`StoryGallery::builtin()`. If two stories have the same slug, the app
panics at startup.

## Bind a form input

Implement `FieldRenderer`. The framework writes the wrapper, the label, the
hint and the errors. The renderer writes only the control:

```rust
use autumn_web::form::{FieldContext, FieldRenderer};

struct DatePicker;

impl FieldRenderer for DatePicker {
    fn name(&self) -> &'static str {
        "datepicker"
    }

    fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
        field.control(
            "input",
            &[("type", "date"), ("class", "autumn-datepicker-input")],
            maud::html! {},
        )
    }
}
```

`FieldContext::control` writes `id`, `name`, `value`, `required`, the
`aria-*` links and the app passthrough. It removes an attribute that the
framework writes, an `on*` or `hx-on*` handler, and `style`. It does not put
the changeset value on a `password`, `file`, `checkbox` or `radio` input.

Use the control in two ways:

- With a `Changeset<T>`:
  `custom_input_with(&changeset, "due", "Due", &DatePicker, &options)`.
- With `form_for`:
  `.override_field("due", FieldControl::custom(DatePicker))`.

For a group of inputs (for example radios), make `FieldRenderer::grouped`
return `true`. Then the wrapper is a `<fieldset>` with a `<legend>`. Write
each input with `FieldContext::group_item`. For a control that sends a
file, make `FieldRenderer::multipart` return `true`. Then `form_for` sets
`enctype="multipart/form-data"`.

## Other widgets

A widget that is not a form input also takes `&FieldOptions`. Write its
root element with `widget_pack::element`. The function adds the app
`data-*` and `hx-*` passthrough:

```rust
use autumn_web::form::FieldOptions;
use autumn_web::widget_pack::element;

fn swatch_toolbar(label: &str, options: &FieldOptions<'_>) -> maud::Markup {
    element(
        "div",
        &[("class", "autumn-swatch-toolbar"), ("role", "toolbar"), ("aria-label", label)],
        options,
        maud::html! { /* buttons */ },
    )
}
```

## Pack JavaScript

Use the keyboard core. Do not write arrow-key code. See
[Widget keyboard core](keyboard.md#plugin-javascript) for the full pattern.

## Check a pack

Run the static checks in the pack test suite. They need no browser:

```rust
use autumn_web::plugin_conformance::widgets::run_widget_conformance;

#[test]
fn the_pack_conforms() {
    let report = run_widget_conformance(&pack());
    assert!(report.passed(), "{}", report.to_text_report());
}
```

| Check | Fails when |
|---|---|
| `widget-pack-name` | `autumn-<pack>` is a core class or class root, or the bundle namespace is not the pack name. |
| `widget-stories` | The pack has no story, a slug does not start with `<pack>-`, or a story panics or is empty. |
| `widget-class-names` | A story class is not `autumn-<pack>-*` and not a core class. |
| `widget-markup-csp` | A story has an inline `<script>` or `<style>`, an `on*` or `hx-on*` attribute, a `style` attribute or a `javascript:` URL. |
| `widget-tokens` | Pack CSS uses `@import`, reads an unknown property, sets a property that is not `--autumn-<pack>-*`, or has a raw color. |
| `widget-scripts` | Pack JS uses `eval(`, `Function(`, a string timer or `document.write(`. |

Run the browser checks with the `system-tests` feature. They use the same
axe check as the core gate (`Page::axe_violations`). Autumn does not include
axe-core. Put a copy of axe-core 4.x in your tests. Give its source to the
function:

```rust
use autumn_web::plugin_conformance::widgets::browser::run_widget_browser_conformance;

#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_pack_passes_the_browser_gate() {
    let axe = std::fs::read_to_string("tests/fixtures/axe.min.js").unwrap();
    let report = run_widget_browser_conformance(&pack(), &axe).await.unwrap();
    assert!(report.passed(), "{}", report.to_text_report());
}
```

| Check | Fails when |
|---|---|
| `widget-axe` | axe-core finds a serious or critical violation, in the light or the dark scheme. |
| `widget-console` | A story page logs a console error or an uncaught exception. |
| `widget-keyboard` | Tab does not reach a control, focus cannot leave the story, or an arrow key does not move focus in a visible `data-autumn-roving` list. |

The keyboard check finds common faults only. Also write a `Page::press`
test for each interactive widget.

## Example

`autumn-plugin-reference/src/swatch.rs` is a small pack. It has a radio
group bound with `FieldRenderer`, a toolbar on the keyboard core, CSS that
uses only tokens, and a script that uses `autumn:init`. It passes both
checks.
