//! Widget packs: the plugin seam for widgets (issue #3400).
//!
//! Core keeps the small widgets that all apps use. Put a large widget (date
//! picker, data table, charts, command palette) in a plugin crate
//! `autumn-<name>-plugin`. An app compiles only the packs it uses.
//!
//! # The widget contract
//!
//! A plugin widget obeys these rules. The rules are stable: a change is a
//! breaking change.
//!
//! 1. **Render.** A widget value implements [`maud::Render`], or a widget
//!    function returns [`maud::Markup`]. The markup is correct with
//!    JavaScript off.
//! 2. **Passthrough.** Each widget takes
//!    [`&FieldOptions`](crate::form::FieldOptions). A form input binds with
//!    [`FieldRenderer`](crate::form::FieldRenderer): the app sets the hint,
//!    `required`, the typed attributes and `data-*`/`hx-*`. Another widget
//!    writes its root with [`element`](crate::widget_pack::element), which adds the `data-*`/`hx-*`
//!    passthrough. The app cannot set `id`, `name` or `aria-*` through the
//!    options.
//! 3. **Tokens.** Pack CSS uses only the public tokens
//!    ([`crate::ui::tokens`]) and its own `--autumn-<pack>-*` properties. It
//!    has no raw color, and it does not set a public token.
//! 4. **Class names.** A new class is `autumn-<pack>-*`. A widget can also
//!    use a core class (for example `autumn-field`).
//! 5. **Keyboard.** Pack JS uses the `data-*` contract of the keyboard core
//!    and the `autumn:init` event. It does not write its own arrow-key code.
//!    See `docs/guide/keyboard.md`.
//! 6. **CSP.** No inline `<script>` or `<style>`, no `on*` or `hx-on*`
//!    attribute, no `style` attribute, no `eval`. Assets come from a
//!    [`PluginAssets`](crate::assets::PluginAssets) bundle.
//! 7. **Stories.** Each widget has a story. Each story slug starts with
//!    `<pack>-`.
//!
//! [`plugin_conformance::widgets`](crate::plugin_conformance::widgets)
//! checks these rules: a static check, and a browser check that runs the
//! same axe gate as core.
//!
//! # Example
//!
//! ```rust
//! use autumn_web::assets::PluginAssets;
//! use autumn_web::widget_pack::WidgetPack;
//!
//! static ASSETS: PluginAssets = PluginAssets::from_files(
//!     "swatch",
//!     &[("swatch.css", b".autumn-swatch-chip { color: var(--text); }")],
//! );
//!
//! fn pack() -> WidgetPack {
//!     WidgetPack::new("swatch")
//!         .extend([autumn_web::stories::story! {
//!             "Swatch",
//!             "Swatch chip",
//!             { maud::html! { span class="autumn-swatch-chip" { "Primary" } } }
//!         }])
//!         .assets(&ASSETS)
//! }
//!
//! // In `Plugin::build`: `app.widget_pack(pack())`.
//! assert_eq!(pack().class_prefix(), "autumn-swatch-");
//! assert_eq!(pack().stories()[0].slug(), "swatch-chip");
//! ```

use crate::assets::PluginAssets;
use crate::form::FieldOptions;
use crate::stories::{Story, StoryGallery};

/// Write the root element of a widget that is not a form input.
///
/// The element gets `attrs`, then the app `data-*` and `hx-*` passthrough
/// from `options`. Values are escaped. When a widget attribute and a
/// passthrough attribute have one name, the widget attribute applies. The
/// function removes a malformed name, an `on*` or `hx-on*` handler and
/// `style` from `attrs`. A void tag ignores `body`.
///
/// ```rust
/// use autumn_web::form::FieldOptions;
/// use autumn_web::widget_pack::element;
///
/// let html = element(
///     "div",
///     &[("class", "autumn-swatch-toolbar"), ("role", "toolbar")],
///     &FieldOptions::new().data("test-id", "colors"),
///     maud::html! { "…" },
/// )
/// .into_string();
/// assert!(html.starts_with(
///     r#"<div class="autumn-swatch-toolbar" role="toolbar" data-test-id="colors">"#
/// ));
/// ```
///
/// # Panics
///
/// Panics when `tag` is not `[a-z][a-z0-9]*`, or is a tag that loads code
/// (`script`, `style`, `iframe`, `object`, …).
#[must_use]
pub fn element(
    tag: &'static str,
    attrs: &[(&str, &str)],
    options: &FieldOptions<'_>,
    body: maud::Markup,
) -> maud::Markup {
    crate::form::widget_element(tag, attrs, options, body)
}

/// The longest pack name.
const MAX_NAME_LEN: usize = 32;

/// A widget pack: a name, its stories and its asset bundle.
///
/// Install it with
/// [`AppBuilder::widget_pack`](crate::app::AppBuilder::widget_pack). Check it
/// with [`plugin_conformance::widgets`](crate::plugin_conformance::widgets).
#[derive(Debug, Clone)]
pub struct WidgetPack {
    name: &'static str,
    stories: Vec<Story>,
    assets: Option<&'static PluginAssets>,
}

impl WidgetPack {
    /// A pack with no stories and no assets.
    ///
    /// # Panics
    ///
    /// Panics when `name` is not `[a-z][a-z0-9]*` words joined by single
    /// `-`, or is longer than 32 bytes. The name goes into class names
    /// (`autumn-<name>-*`) and story slugs.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        assert!(
            is_valid_pack_name(name),
            "widget pack name {name:?} is not valid: use lowercase words of \
             [a-z0-9] joined by single `-`, starting with a letter, at most \
             {MAX_NAME_LEN} bytes"
        );
        Self {
            name,
            stories: Vec::new(),
            assets: None,
        }
    }

    /// Add stories. Each slug must start with `<name>-`.
    #[must_use]
    pub fn extend(mut self, stories: impl IntoIterator<Item = Story>) -> Self {
        self.stories.extend(stories);
        self
    }

    /// Set the asset bundle. Its `.css` files are the pack stylesheets and
    /// its `.js` files are the pack scripts.
    #[must_use]
    pub const fn assets(mut self, assets: &'static PluginAssets) -> Self {
        self.assets = Some(assets);
        self
    }

    /// The pack name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The class prefix of the pack: `autumn-<name>-`.
    #[must_use]
    pub fn class_prefix(&self) -> String {
        format!("autumn-{}-", self.name)
    }

    /// The pack stories, in registration order.
    #[must_use]
    pub fn stories(&self) -> &[Story] {
        &self.stories
    }

    /// The asset bundle, if the pack has one.
    #[must_use]
    pub const fn asset_bundle(&self) -> Option<&'static PluginAssets> {
        self.assets
    }

    /// The content-hashed URLs of the pack stylesheets, by logical path.
    #[must_use]
    pub fn stylesheet_urls(&self) -> Vec<String> {
        self.asset_urls(AssetKind::Stylesheet)
    }

    /// The content-hashed URLs of the pack scripts, by logical path.
    #[must_use]
    pub fn script_urls(&self) -> Vec<String> {
        self.asset_urls(AssetKind::Script)
    }

    /// The `<link>` and `<script defer>` tags for the pack, with SRI.
    ///
    /// Put
    /// them in the layout `<head>`, after the core widget stylesheet and
    /// `autumn-widgets.js`.
    #[must_use]
    pub fn head(&self) -> maud::Markup {
        let Some(assets) = self.assets else {
            return maud::html! {};
        };
        maud::html! {
            @for path in self.asset_paths(AssetKind::Stylesheet) {
                (assets.stylesheet_tag(path))
            }
            @for path in self.asset_paths(AssetKind::Script) {
                (assets.deferred_script_tag(path))
            }
        }
    }

    /// A story gallery with the pack stories and asset links.
    #[must_use]
    pub fn gallery(&self) -> StoryGallery {
        self.clone().into_gallery()
    }

    /// Like [`gallery`](Self::gallery), without a copy of the stories.
    #[must_use]
    pub fn into_gallery(self) -> StoryGallery {
        let stylesheets = self.stylesheet_urls();
        let scripts = self.script_urls();
        let mut gallery = StoryGallery::new().extend(self.stories);
        for href in stylesheets {
            gallery = gallery.stylesheet(href);
        }
        for src in scripts {
            gallery = gallery.script(src);
        }
        gallery
    }

    /// The logical paths of one kind of asset, sorted.
    fn asset_paths(&self, kind: AssetKind) -> Vec<&'static str> {
        self.assets.map_or_else(Vec::new, |assets| {
            assets
                .iter()
                .map(crate::assets::PluginAsset::logical_path)
                .filter(|path| kind.matches(path))
                .collect()
        })
    }

    fn asset_urls(&self, kind: AssetKind) -> Vec<String> {
        self.assets.map_or_else(Vec::new, |assets| {
            self.asset_paths(kind)
                .into_iter()
                .map(|path| assets.url(path))
                .collect()
        })
    }
}

/// The asset kinds a pack links.
#[derive(Clone, Copy)]
pub(crate) enum AssetKind {
    Stylesheet,
    Script,
}

impl AssetKind {
    /// `true` when `path` is an asset of this kind.
    pub(crate) fn matches(self, path: &str) -> bool {
        let extension = match self {
            Self::Stylesheet => ".css",
            Self::Script => ".js",
        };
        path.ends_with(extension)
    }
}

/// `true` when `name` is `[a-z][a-z0-9]*` words joined by single `-`, at
/// most [`MAX_NAME_LEN`] bytes.
fn is_valid_pack_name(name: &str) -> bool {
    name.len() <= MAX_NAME_LEN
        && name.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    static ASSETS: PluginAssets = PluginAssets::from_files(
        "wpacktest",
        &[
            ("b.css", b".autumn-wpacktest-b { color: var(--text); }"),
            ("a.css", b".autumn-wpacktest-a { color: var(--text); }"),
            ("pack.js", b"void 0;"),
            ("logo.svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
        ],
    );

    fn demo() -> maud::Markup {
        maud::html! { span class="autumn-wpacktest-a" { "demo" } }
    }

    fn pack() -> WidgetPack {
        WidgetPack::new("wpacktest")
            .extend([Story::new("Pack", "Wpacktest demo", demo, "demo()")])
            .assets(&ASSETS)
    }

    #[test]
    fn valid_names_are_accepted() {
        for name in ["datepicker", "command-palette", "charts2", "a", "x-1"] {
            assert_eq!(WidgetPack::new(name).name(), name);
        }
    }

    #[test]
    fn invalid_names_panic() {
        for name in [
            "",
            "Date",
            "1date",
            "-date",
            "date-",
            "date--picker",
            "date_picker",
            "date picker",
            "dåte",
            "abcdefghijklmnopqrstuvwxyzabcdefg",
        ] {
            let result = std::panic::catch_unwind(|| WidgetPack::new(name));
            assert!(result.is_err(), "{name:?} must be refused");
        }
    }

    #[test]
    fn class_prefix_uses_the_name() {
        assert_eq!(pack().class_prefix(), "autumn-wpacktest-");
    }

    #[test]
    fn asset_urls_come_from_the_bundle_by_kind() {
        let pack = pack();
        let css = pack.stylesheet_urls();
        assert_eq!(css.len(), 2, "{css:?}");
        assert!(
            css[0].starts_with("/static/_plugins/wpacktest/a."),
            "{css:?}"
        );
        assert!(
            css[1].starts_with("/static/_plugins/wpacktest/b."),
            "{css:?}"
        );
        let js = pack.script_urls();
        assert_eq!(js.len(), 1, "{js:?}");
        assert!(
            js[0].starts_with("/static/_plugins/wpacktest/pack."),
            "{js:?}"
        );
        assert!(WidgetPack::new("bare").stylesheet_urls().is_empty());
    }

    #[test]
    fn head_links_stylesheets_then_deferred_scripts_with_sri() {
        let head = pack().head().into_string();
        let a = head.find("/static/_plugins/wpacktest/a.").expect("a.css");
        let b = head.find("/static/_plugins/wpacktest/b.").expect("b.css");
        let js = head
            .find("/static/_plugins/wpacktest/pack.")
            .expect("pack.js");
        assert!(a < b && b < js, "{head}");
        assert_eq!(
            head.matches(r#"<link rel="stylesheet""#).count(),
            2,
            "{head}"
        );
        assert_eq!(head.matches("<script").count(), 1, "{head}");
        assert!(head.contains(" defer"), "{head}");
        assert_eq!(head.matches(r#"integrity="sha384-"#).count(), 3, "{head}");
        assert!(!head.contains("logo"), "{head}");
        assert_eq!(WidgetPack::new("bare").head().into_string(), "");
    }

    #[test]
    fn gallery_carries_stories_and_asset_links() {
        let pack = pack();
        let gallery = pack.gallery();
        assert_eq!(gallery.stories().len(), 1);
        assert_eq!(gallery.stories()[0].slug(), "wpacktest-demo");
        assert_eq!(gallery.stylesheets(), pack.stylesheet_urls().as_slice());
        assert_eq!(gallery.scripts(), pack.script_urls().as_slice());
    }
}
