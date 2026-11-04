# Plan: widget plugin seam (issue #3400)

Part of #3393, Phase 0. Depends on #3395 (passthrough), #3396 (tokens),
#3397 (keyboard core) and #3398 (browser gate). All four are merged.

## Goal

Let a crate ship a widget pack as a plugin. The pack adds stories, styles
itself with the public tokens, reuses the keyboard core, binds to
`Changeset<T>` and `form_for`, and passes the same axe and keyboard gate
as core.

## Brainstorming

Ideas we examined:

1. `with_story_gallery` merges into the gallery that is already there. The
   duplicate-slug panic stays in `StoryRegistry::new`.
2. A `WidgetPack` value: a name, stories and an optional `PluginAssets`
   bundle. One `AppBuilder::widget_pack` call installs all three.
3. The gallery links the pack stylesheets and scripts, so a pack story
   renders as an app shows it.
4. `FieldControl::Custom(CustomControl)` holds an `Arc<dyn FieldRenderer>`.
   `FieldContext` gives the renderer the id, name, value, errors and
   options. The framework writes the label, hint, error and ARIA links.
5. `FieldContext::control` writes the control tag with the owned
   attributes (`id`, `name`, `aria-*`, `required`) and the passthrough.
6. A grouped control (radios) gets a `<fieldset>` and a `<legend>`.
7. `custom_input_with` binds a custom control to a `Changeset<T>` without
   `form_for`.
8. Static conformance: name, stories, class prefix, token use, CSP-safe
   markup and script.
9. Browser conformance: the core axe check (shared through
   `Page::axe_violations`), console errors, Tab reachability and roving
   arrow keys.
10. The runtime fires `autumn:init` on each new root, after core widget
    init and before roving init, so plugin JS needs no `htmx:afterSwap`
    code of its own.
11. The reference plugin ships a small `swatch` pack that uses every new
    surface.

## Reverse brainstorming

Question: "How can a widget pack break an app?" Each answer gives a guard.

| Way to fail | Guard |
|---|---|
| A second `with_story_gallery` call drops the first gallery. | Merge. A unit test registers two galleries. |
| Two packs register one slug. | The registry panics and names both stories. Conformance requires the `<pack>-` slug prefix. |
| A pack class collides with a core class. | Conformance fails on a class that is not `autumn-<pack>-*` and not a core class, and on a pack name that is a core class root. |
| A pack sets `--primary` and re-themes the whole app. | Conformance fails on a custom property that is not `--autumn-<pack>-*`. |
| A pack hard-codes `#fff` and breaks the dark scheme. | Conformance fails on a raw color in pack CSS. |
| A renderer overrides `id`, `name` or `aria-describedby`. | `FieldContext::control` drops those names. A test proves it. |
| A renderer adds `onclick` or `style`. | `control` drops `on*` and `style`. Conformance fails on them in story markup. |
| Pack JS uses `eval` and breaks `script-src 'self'`. | Conformance fails on `eval(` and `new Function(`. The browser check fails on a console error. |
| Pack JS misses widgets that htmx swaps in. | `autumn:init` fires on each swapped root. |
| A pack loads before the runtime and its init runs too early. | The runtime boots on `DOMContentLoaded`, after all deferred scripts. |
| The plugin gate drifts from the core gate. | Both call `Page::axe_violations`. |
| A custom control breaks `multipart` detection. | `FieldRenderer::multipart` feeds `FormFor::render`. |
| A conformance check cannot fail. | Canary tests with seeded bad packs. |

## Six thinking hats

- **White (facts).** `with_story_gallery` replaces. `FieldControl` is
  `#[non_exhaustive]` and closed. `PluginAssets` serves hashed plugin
  files. axe-core is a test fixture and is not published. The keyboard core
  inits only core widgets.
- **Red (feeling).** Plugin authors want one call to install a pack, and one
  call to prove it is correct.
- **Black (risk).** Merge changes behaviour for an app that called
  `with_story_gallery` two times. The new surfaces are stable, so a later
  change needs a migration guide. A JS change alters the script
  fingerprint and the posture goldens. The browser check needs Chromium.
- **Yellow (value).** Five first-party packs (#3413–#3417) and third-party
  packs get one documented seam. Core stays small.
- **Green (alternatives).** A `WidgetPack` trait (rejected: a value is
  simpler to test and to pass around). A JS API object on `window`
  (rejected in ADR 0027). Shipping axe in the crate (rejected: 652 KB; the
  caller passes the source).
- **Blue (process).** RED: tests for each gap, run and see them fail.
  GREEN: the smallest code. REFACTOR: share the axe check. Then docs, an
  ADR, a changelog fragment, review agents, fixes.

## Scope

In scope: the five gaps, `AppBuilder::widget_pack`, `SystemTest::plugin_assets`,
`Page::axe_violations`, the reference `swatch` pack, CI, docs.

Out of scope: the packs themselves (#3413–#3417), feature-gated core
families (#3408), a `#[model]` attribute that selects a custom control.
