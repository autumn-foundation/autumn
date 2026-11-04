### Added

- **widgets:** widget packs (issue #3400). A plugin crate puts widgets in a
  `WidgetPack`: a name, stories and a `PluginAssets` bundle.
  `AppBuilder::widget_pack` serves the assets and adds the stories to the
  `/_stories` gallery. `WidgetPack::head` links the CSS and JS with SRI. See
  `docs/guide/widget-packs.md`. `widget_pack::element` writes the root of
  a widget that is not a form input, with the `data-*`/`hx-*` passthrough.
- **forms:** `FieldControl::Custom` and the `FieldRenderer` trait. A plugin
  input binds to `Changeset<T>` with `custom_input_with` and to `form_for`
  with `FieldControl::custom`. The framework writes the wrapper, label,
  hint, errors, `aria-*` links and passthrough. A grouped control gets a
  `<fieldset>`, and `FieldContext::group_item` writes each input.
  `FieldOptions` has `is_required`, `is_disabled`, `is_readonly` and
  `hint_text`.
- **plugin conformance:** `plugin_conformance::widgets` checks a pack: name,
  stories, class names, CSP-safe markup, tokens-only CSS and scripts. With
  `system-tests`, `browser::run_widget_browser_conformance` runs the core
  axe gate, a console check and a keyboard check in Chromium.
- **system tests:** `Page::axe_violations(axe_source)` runs axe-core and
  returns the serious and critical violations. `SystemTest::plugin_assets`
  serves a `PluginAssets` bundle.
- **stories:** `StoryGallery::stylesheet`, `StoryGallery::script` and
  `StoryGallery::merge`. Each gallery page links the extra assets.
- **widgets:** `autumn-widgets.js` fires `autumn:init` on the page and on
  each htmx swap, before roving set-up. Plugin JS uses it for set-up.

### Changed

- **stories:** `AppBuilder::with_story_gallery` adds to the gallery. Before,
  a second call replaced it. Duplicate slugs still panic at startup.
- **widgets:** a deferred `autumn-widgets.js` starts on `DOMContentLoaded`,
  after all deferred scripts.
