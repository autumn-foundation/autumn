# ADR 0028: Widget Packs Are Plugins with One Contract and One Gate

- Status: Accepted
- Date: 2026-10-29
- Deciders: Autumn maintainers
- Tags: widgets, plugins, forms, accessibility

## Context

`autumn/src/widgets.rs` has about 9,000 lines. The next widgets (date
picker, data table v2, charts v2, command palette) are large. In core they
would add compile time, script size and stable API (issue #3400). The
decision in #3393: core stays small, and large widgets go in plugin crates.

A plugin could not add stories, bind an input to a form, or show that its
widgets pass the core axe and keyboard gate.

## Options

1. Keep all widgets in core, behind Cargo features.
2. A `WidgetPack` trait that a plugin implements.
3. A `WidgetPack` value, a documented contract, and conformance checks that
   reuse the core gate.

## Decision

Option 3.

- `WidgetPack` holds a name, stories and a `PluginAssets` bundle.
  `AppBuilder::widget_pack` installs all three. A value is simpler to test
  and to pass to the checks than a trait.
- `with_story_gallery` adds. It does not replace. Duplicate slugs still
  panic.
- `FieldControl::Custom` holds an `Arc<dyn FieldRenderer>`. The framework
  writes the wrapper, label, hint, errors and the owned attributes.
  `FieldContext::control` drops the owned names, `on*` and `style`.
- The keyboard core fires `autumn:init` on each new root, before roving
  set-up. It starts on `DOMContentLoaded`, after all deferred scripts.
- A widget that is not a form input writes its root with
  `widget_pack::element`, which adds the `data-*`/`hx-*` passthrough.
- `plugin_conformance::widgets` has static checks (name, stories, classes,
  CSP, tokens, scripts) and browser checks (axe, console, keyboard). The core
  gate and the pack gate call one `Page::axe_violations`.
- Autumn does not ship axe-core (652 KB). The caller passes its source.

## Consequences

- First-party packs (#3413–#3417) and third-party packs use one seam.
- The contract is stable surface. A change needs a migration guide.
- An app that called `with_story_gallery` two times with the same stories
  now panics at startup.
- The keyboard check finds common faults only. A pack still needs its own
  `Page::press` tests for each interactive widget.
