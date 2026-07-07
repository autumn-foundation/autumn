# Widget stories

Autumn ships a Storybook-equivalent for its built-in maud widgets: a browsable
gallery at `/_stories` where every widget renders live above the exact source
snippet that produced it, plus a CI harness that fails the framework build when
a widget loses its example or stops rendering. Apps can serve the built-in
gallery as-is, extend it with their own widgets, or run an app-only gallery.

The index page lists stories in a grouped sidebar; each detail page at
`/_stories/{slug}` shows the live render, a **Source** tab with a copyable
snippet, and a **Rendered HTML** tab with the escaped output — styled by the
same framework widget stylesheet (`autumn-widgets.css`) your app already
serves.

## Enabling the gallery

Two switches are required — registering the gallery on the builder, and
enabling the routes in config:

```rust
use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_story_gallery(StoryGallery::builtin())
        .routes(routes![/* ... */])
        .run()
        .await;
}
```

```toml
[stories]
enabled = true
```

Without the config flag the routes are absent (`/_stories` → 404) even when a
gallery is registered; without the registration the routes serve an empty-state
page pointing at `with_story_gallery`. The environment override is
`AUTUMN_STORIES__ENABLED=true`.

`/_stories` is a reserved framework path while the gallery is enabled, like
`/_autumn/mail` for the mail preview.

## Dev-only vs public showcase (profiles)

Unlike the dev-only mail preview, stories may be enabled in **any** profile —
serving the gallery publicly is a supported use, and it is safe because stories
only ever render synthetic demo data defined inside their own blocks. Profile
gating uses the standard config layering:

```toml
# Private app: gallery under `autumn dev`, 404 in prod.
[stories]
enabled = false

[profile.dev.stories]
enabled = true
```

```toml
# Public showcase: gallery served in production.
[stories]
enabled = false

[profile.prod.stories]
enabled = true
```

Wherever the resolved flag is false, the routes are not mounted at all — the
gallery is absent, not merely hidden.

## Authoring stories with `story!`

A story is a group, a name, and a brace-delimited block:

```rust
use autumn_web::prelude::*;

let badge = story! {
    "App",
    "Team badge",
    {
        maud::html! { span class="team-badge" { "Platform" } }
    }
};
```

The block is **both** executed for the live render **and** captured
byte-for-byte — comments and formatting included — as the displayed snippet,
so the code shown in the Source tab is provably the code that rendered.
That imposes two rules:

- **Blocks are self-contained.** Define demo data inside the block (a local
  struct, a `vec!` of sample rows) so the snippet is a complete, copyable
  example.
- **Blocks are zero-arg pure functions.** The block is coerced to a plain
  `fn() -> Markup` pointer, so capturing anything from the surrounding
  environment — a `Db` handle, `AppState`, request data, any local — is a
  compile error. No extractors, no async, no I/O.

The URL slug derives from the name (`"Team badge"` → `team-badge`):
lowercase, alphanumeric runs joined by `-`. Two stories whose names produce
the same slug panic at startup with a message naming both — rename one. A
name with no alphanumeric characters panics at construction.

A story that panics while rendering does not take the gallery down: the index
never executes render functions, and the detail page returns a 500 error page
naming the story. Note that the caught panic still runs the process panic hook,
so each view of a broken story's detail page emits a panic report to the logs —
worth keeping in mind when exposing a gallery with custom stories publicly.

## Registering custom app widgets

Extend the built-in set, or start empty with the builtin-free constructor:

```rust
use autumn_web::prelude::*;

fn app_stories() -> Vec<Story> {
    vec![
        story! {
            "App",
            "Team badge",
            {
                maud::html! { span class="team-badge" { "Platform" } }
            }
        },
    ]
}

// Built-ins plus your own — your stories appear under their group ("App")
// in the same sidebar:
autumn_web::app().with_story_gallery(StoryGallery::builtin().extend(app_stories()));

// Or an app-only gallery with no framework stories:
autumn_web::app().with_story_gallery(StoryGallery::new().extend(app_stories()));
```

`StoryGallery::routes()` returns the underlying sub-router if you need to
mount it manually; note the handlers read the registry from the `AppState`
extension, so `with_story_gallery` (which installs it) is the normal path.

## What CI checks

For framework contributors, `autumn/tests/integration/stories.rs` is the
anti-rot harness. On every `cargo test` it renders each built-in story and
asserts no panic, non-empty output, balanced/well-formed HTML, unique
well-formed slugs, and non-empty groups and names. A two-layer coverage gate
then enforces that widgets cannot ship without examples:

1. an `EXPECTED_STORY_SLUGS` inventory is diffed both ways against
   `stories::builtin()`, and
2. every public widget fn in `src/widgets.rs` must appear in at least one
   builtin story's source.

Adding a widget without a story fails CI with instructions: add a `story!`
block in `autumn/src/stories/builtin.rs` and, for a new top-level widget, add
its slug to the inventory.

## Reference

Routes (mounted iff `stories.enabled` resolves true; listed by
`autumn routes` under the same condition):

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/_stories` | Grouped story index |
| GET | `/_stories/{slug}` | Live render + Source + Rendered HTML tabs |

Configuration:

| Key | Default | Purpose |
|-----|---------|---------|
| `stories.enabled` | `false` | Mount the gallery routes (any profile; `AUTUMN_STORIES__ENABLED` overrides) |

Related: the dev mail preview (`/_autumn/mail`, see the [mail guide](mail.md)) follows
the same registry-plus-config pattern but is hardwired dev-only; the story
gallery is profile-agnostic by design.
