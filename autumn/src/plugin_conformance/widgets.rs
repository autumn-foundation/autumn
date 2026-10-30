//! Widget pack conformance (issue #3400).
//!
//! Checks a [`WidgetPack`] against the widget contract in
//! [`crate::widget_pack`]. Run it in the pack crate test suite:
//!
//! ```rust
//! use autumn_web::plugin_conformance::widgets::run_widget_conformance;
//! use autumn_web::widget_pack::WidgetPack;
//!
//! let pack = WidgetPack::new("swatch").extend([autumn_web::stories::story! {
//!     "Swatch",
//!     "Swatch chip",
//!     { maud::html! { span class="autumn-swatch-chip" { "Primary" } } }
//! }]);
//! let report = run_widget_conformance(&pack);
//! assert!(report.passed(), "{}", report.to_text_report());
//! ```
//!
//! The static checks need no browser:
//!
//! | Check | Fails when |
//! |---|---|
//! | `widget-pack-name` | `autumn-<pack>` is a core class or class root, or the bundle namespace is not the pack name. |
//! | `widget-stories` | The pack has no story, a slug does not start with `<pack>-`, or a story panics or renders nothing. |
//! | `widget-class-names` | A story class is not `autumn-<pack>-*` and not a core class. |
//! | `widget-markup-csp` | A story has an inline `<script>` or `<style>`, an `on*` or `hx-on*` attribute, a `style` attribute or a `javascript:` URL. |
//! | `widget-tokens` | Pack CSS uses `@import`, reads an unknown custom property, sets a property that is not `--autumn-<pack>-*`, or has a raw color. |
//! | `widget-scripts` | Pack JS uses `eval(`, `Function(`, a string timer or `document.write(`. |
//!
//! With the `system-tests` feature, [`browser::run_widget_browser_conformance`]
//! adds the axe, console and keyboard checks in Chromium.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use crate::plugin_conformance::{CheckResult, CheckStatus, ConformanceReport};
use crate::widget_pack::{AssetKind, WidgetPack};

#[cfg(all(feature = "system-tests", feature = "htmx"))]
pub mod browser;

/// `widget-pack-name`: `autumn-<pack>` must not be a core class or the root
/// of a core class family (`autumn-<pack>-…`, `__…`, `--…`).
#[must_use]
pub fn check_widget_pack_name(pack: &WidgetPack) -> CheckResult {
    let root = format!("autumn-{}", pack.name());
    let clashes: Vec<String> = core_classes()
        .iter()
        .filter(|class| {
            class.strip_prefix(root.as_str()).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with('-') || rest.starts_with("__")
            })
        })
        .map(|class| format!("`{class}` is a core class; rename the pack"))
        .chain(
            pack.asset_bundle()
                .filter(|assets| assets.namespace() != pack.name())
                .map(|assets| {
                    format!(
                        "the asset bundle namespace `{}` is not the pack name `{}`",
                        assets.namespace(),
                        pack.name()
                    )
                }),
        )
        .collect();
    result(
        "widget-pack-name",
        clashes,
        format!("`{root}-*` does not collide with a core class"),
        "the pack name is not usable",
    )
}

/// `widget-stories`: the pack has stories, each slug starts with
/// `<pack>-`, and each story renders non-empty markup.
#[must_use]
pub fn check_widget_stories(pack: &WidgetPack) -> CheckResult {
    if pack.stories().is_empty() {
        return fail(
            "widget-stories",
            "the pack has no stories",
            vec!["the pack has no stories; add one story per widget".to_owned()],
        );
    }
    let prefix = format!("{}-", pack.name());
    let mut problems = Vec::new();
    for story in pack.stories() {
        if !story.slug().starts_with(&prefix) {
            problems.push(format!(
                "story `{}` (group `{}`): the slug must start with `{prefix}`",
                story.slug(),
                story.group()
            ));
        }
        match story.render() {
            Err(_) => problems.push(format!("story `{}` panicked", story.slug())),
            Ok(markup) if markup.0.trim().is_empty() => {
                problems.push(format!("story `{}` rendered no markup", story.slug()));
            }
            Ok(_) => {}
        }
    }
    result(
        "widget-stories",
        problems,
        format!("{} story(ies) render", pack.stories().len()),
        "story problems",
    )
}

/// `widget-class-names`: each class in the story markup is
/// `autumn-<pack>-*` or a core class.
#[must_use]
pub fn check_widget_class_names(pack: &WidgetPack) -> CheckResult {
    let prefix = pack.class_prefix();
    let mut problems = Vec::new();
    for (slug, html) in rendered_stories(pack) {
        let foreign: BTreeSet<String> = tags(&html)
            .into_iter()
            .flat_map(|tag| tag.attrs)
            .filter(|(name, _)| name == "class")
            .flat_map(|(_, value)| {
                value
                    .split_ascii_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|class| !class.starts_with(&prefix) && !core_classes().contains(class))
            .collect();
        problems.extend(foreign.into_iter().map(|class| {
            format!("story `{slug}`: class `{class}` is not `{prefix}*` or a core class")
        }));
    }
    result(
        "widget-class-names",
        problems,
        format!("every class is `{prefix}*` or a core class"),
        "foreign class names",
    )
}

/// `widget-markup-csp`: the story markup is CSP-safe.
///
/// No inline `<script>` or `<style>`, no `on*` or
/// `hx-on*` handler, no `style` attribute and no `javascript:` URL in the
/// story markup.
#[must_use]
pub fn check_widget_markup_csp(pack: &WidgetPack) -> CheckResult {
    let mut problems = Vec::new();
    for (slug, html) in rendered_stories(pack) {
        for tag in tags(&html) {
            if tag.name == "script" && !tag.attrs.iter().any(|(name, _)| name == "src") {
                problems.push(format!("story `{slug}`: inline <script> element"));
            }
            if tag.name == "style" {
                problems.push(format!(
                    "story `{slug}`: inline <style> element; use the pack CSS"
                ));
            }
            for (name, value) in &tag.attrs {
                if name.starts_with("on")
                    || name.starts_with("hx-on")
                    || name.starts_with("data-hx-on")
                {
                    problems.push(format!(
                        "story `{slug}`: `{name}` attribute on <{}>",
                        tag.name
                    ));
                } else if name == "style" {
                    problems.push(format!(
                        "story `{slug}`: `style` attribute on <{}>; use a class",
                        tag.name
                    ));
                } else if value
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("javascript:")
                {
                    problems.push(format!("story `{slug}`: javascript: URL in `{name}`"));
                }
            }
        }
    }
    result(
        "widget-markup-csp",
        problems,
        "no inline script, handler or style".to_owned(),
        "markup breaks a strict CSP",
    )
}

/// `widget-tokens`: pack CSS uses only the token vocabulary.
///
/// The CSS reads only public tokens and its own
/// `--autumn-<pack>-*` properties, sets only `--autumn-<pack>-*`, and has
/// no raw color.
#[must_use]
pub fn check_widget_tokens(pack: &WidgetPack) -> CheckResult {
    let files = asset_texts(pack, AssetKind::Stylesheet);
    if files.is_empty() {
        return skip("widget-tokens", "the pack has no stylesheet");
    }
    let own = format!("--autumn-{}-", pack.name());
    let mut problems = Vec::new();
    for (path, css) in files {
        let Some(css) = css else {
            problems.push(format!("`{path}` is not UTF-8"));
            continue;
        };
        if strip_comments(&css)
            .to_ascii_lowercase()
            .contains("@import")
        {
            problems.push(format!(
                "`{path}` uses `@import`; put all pack CSS in the bundle"
            ));
        }
        for (property, value) in declarations(&css) {
            if property.starts_with("--") && !property.starts_with(&own) {
                problems.push(format!(
                    "`{path}` sets `{property}`; a pack sets only `{own}*`"
                ));
            }
            for name in var_reads(&value.to_ascii_lowercase()) {
                if !name.starts_with(&own) && !public_tokens().contains(name) {
                    problems.push(format!("`{path}` reads unknown `{name}`"));
                }
            }
            for color in raw_colors(&property, &value) {
                problems.push(format!(
                    "`{path}`: raw color `{color}` in `{property}`; use a token"
                ));
            }
        }
    }
    result(
        "widget-tokens",
        problems,
        "the CSS uses only the public tokens".to_owned(),
        "the CSS leaves the token vocabulary",
    )
}

/// `widget-scripts`: pack JS has no `eval(`, `Function(`, string
/// `setTimeout`/`setInterval`, or `document.write(`.
#[must_use]
pub fn check_widget_scripts(pack: &WidgetPack) -> CheckResult {
    let files = asset_texts(pack, AssetKind::Script);
    if files.is_empty() {
        return skip("widget-scripts", "the pack has no script");
    }
    let mut problems = Vec::new();
    for (path, js) in files {
        let Some(js) = js else {
            problems.push(format!("`{path}` is not UTF-8"));
            continue;
        };
        for banned in script_problems(&js) {
            problems.push(format!("`{path}` uses `{banned}`; a strict CSP blocks it"));
        }
    }
    result(
        "widget-scripts",
        problems,
        "the scripts run under `script-src 'self'`".to_owned(),
        "a script needs `unsafe-eval`",
    )
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn result(name: &str, problems: Vec<String>, ok: String, bad: &str) -> CheckResult {
    if problems.is_empty() {
        CheckResult {
            name: name.to_owned(),
            status: CheckStatus::Pass,
            message: ok,
            diagnostics: Vec::new(),
        }
    } else {
        fail(name, bad, problems)
    }
}

fn fail(name: &str, message: &str, diagnostics: Vec<String>) -> CheckResult {
    CheckResult {
        name: name.to_owned(),
        status: CheckStatus::Fail,
        message: message.to_owned(),
        diagnostics,
    }
}

fn skip(name: &str, message: &str) -> CheckResult {
    CheckResult {
        name: name.to_owned(),
        status: CheckStatus::Skip,
        message: message.to_owned(),
        diagnostics: Vec::new(),
    }
}

/// `(slug, html)` of each story that renders. `widget-stories` reports
/// the ones that do not.
fn rendered_stories(pack: &WidgetPack) -> Vec<(String, String)> {
    pack.stories()
        .iter()
        .filter_map(|story| {
            story
                .render()
                .ok()
                .map(|markup| (story.slug().to_owned(), markup.into_string()))
        })
        .collect()
}

/// `(logical path, text)` of each pack asset of `kind`. The text is `None`
/// when the file is not UTF-8.
fn asset_texts(pack: &WidgetPack, kind: AssetKind) -> Vec<(String, Option<String>)> {
    pack.asset_bundle().map_or_else(Vec::new, |assets| {
        assets
            .iter()
            .filter(|asset| kind.matches(asset.logical_path()))
            .map(|asset| {
                (
                    asset.logical_path().to_owned(),
                    std::str::from_utf8(asset.bytes()).ok().map(str::to_owned),
                )
            })
            .collect()
    })
}

/// Every `autumn-*` class the core widget stylesheet styles.
fn core_classes() -> &'static BTreeSet<String> {
    static CLASSES: OnceLock<BTreeSet<String>> = OnceLock::new();
    CLASSES.get_or_init(|| {
        let css = crate::ui::WIDGETS_CSS;
        css.match_indices(".autumn-")
            .map(|(at, _)| {
                let rest = &css[at + 1..];
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                    .unwrap_or(rest.len());
                rest[..end].to_owned()
            })
            .collect()
    })
}

/// Every custom property the public tokens define (`--bg`, `--space-1`, …).
fn public_tokens() -> &'static BTreeSet<String> {
    static TOKENS: OnceLock<BTreeSet<String>> = OnceLock::new();
    TOKENS.get_or_init(|| {
        declarations(crate::ui::tokens::TOKENS_CSS)
            .into_iter()
            .map(|(property, _)| property)
            .filter(|property| property.starts_with("--"))
            .collect()
    })
}

/// The `(property, value)` declarations of `css`, at any nesting depth.
///
/// Comments are removed. A selector or an at-rule prelude is not a
/// declaration.
fn declarations(css: &str) -> Vec<(String, String)> {
    let text = strip_comments(css);
    let mut out = Vec::new();
    let mut segment = String::new();
    for c in text.chars() {
        match c {
            '{' => segment.clear(),
            ';' | '}' => {
                if let Some((property, value)) = segment.split_once(':') {
                    let property = property.trim();
                    if !property.is_empty() {
                        out.push((property.to_ascii_lowercase(), value.trim().to_owned()));
                    }
                }
                segment.clear();
            }
            _ => segment.push(c),
        }
    }
    out
}

/// `css` without its `/* … */` comments.
fn strip_comments(css: &str) -> String {
    let mut text = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        text.push_str(&rest[..start]);
        rest = rest[start + 2..]
            .find("*/")
            .map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    text.push_str(rest);
    text
}

/// The custom property names `value` reads with `var(…)`.
fn var_reads(value: &str) -> Vec<&str> {
    value
        .match_indices("var(")
        .filter_map(|(at, _)| {
            let rest = value[at + 4..].trim_start();
            let rest = rest.strip_prefix("--").map(|_| rest)?;
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .unwrap_or(rest.len());
            Some(&rest[..end])
        })
        .collect()
}

/// Color functions a pack must not call. A token gives the color.
const COLOR_FUNCTIONS: &[&str] = &[
    "rgb(", "rgba(", "hsl(", "hsla(", "hwb(", "lab(", "lch(", "oklab(", "oklch(", "color(",
];

/// The CSS named colors (CSS Color 4), lowercase.
const NAMED_COLORS: &[&str] = &[
    "aliceblue",
    "antiquewhite",
    "aqua",
    "aquamarine",
    "azure",
    "beige",
    "bisque",
    "black",
    "blanchedalmond",
    "blue",
    "blueviolet",
    "brown",
    "burlywood",
    "cadetblue",
    "chartreuse",
    "chocolate",
    "coral",
    "cornflowerblue",
    "cornsilk",
    "crimson",
    "cyan",
    "darkblue",
    "darkcyan",
    "darkgoldenrod",
    "darkgray",
    "darkgreen",
    "darkgrey",
    "darkkhaki",
    "darkmagenta",
    "darkolivegreen",
    "darkorange",
    "darkorchid",
    "darkred",
    "darksalmon",
    "darkseagreen",
    "darkslateblue",
    "darkslategray",
    "darkslategrey",
    "darkturquoise",
    "darkviolet",
    "deeppink",
    "deepskyblue",
    "dimgray",
    "dimgrey",
    "dodgerblue",
    "firebrick",
    "floralwhite",
    "forestgreen",
    "fuchsia",
    "gainsboro",
    "ghostwhite",
    "gold",
    "goldenrod",
    "gray",
    "green",
    "greenyellow",
    "grey",
    "honeydew",
    "hotpink",
    "indianred",
    "indigo",
    "ivory",
    "khaki",
    "lavender",
    "lavenderblush",
    "lawngreen",
    "lemonchiffon",
    "lightblue",
    "lightcoral",
    "lightcyan",
    "lightgoldenrodyellow",
    "lightgray",
    "lightgreen",
    "lightgrey",
    "lightpink",
    "lightsalmon",
    "lightseagreen",
    "lightskyblue",
    "lightslategray",
    "lightslategrey",
    "lightsteelblue",
    "lightyellow",
    "lime",
    "limegreen",
    "linen",
    "magenta",
    "maroon",
    "mediumaquamarine",
    "mediumblue",
    "mediumorchid",
    "mediumpurple",
    "mediumseagreen",
    "mediumslateblue",
    "mediumspringgreen",
    "mediumturquoise",
    "mediumvioletred",
    "midnightblue",
    "mintcream",
    "mistyrose",
    "moccasin",
    "navajowhite",
    "navy",
    "oldlace",
    "olive",
    "olivedrab",
    "orange",
    "orangered",
    "orchid",
    "palegoldenrod",
    "palegreen",
    "paleturquoise",
    "palevioletred",
    "papayawhip",
    "peachpuff",
    "peru",
    "pink",
    "plum",
    "powderblue",
    "purple",
    "rebeccapurple",
    "red",
    "rosybrown",
    "royalblue",
    "saddlebrown",
    "salmon",
    "sandybrown",
    "seagreen",
    "seashell",
    "sienna",
    "silver",
    "skyblue",
    "slateblue",
    "slategray",
    "slategrey",
    "snow",
    "springgreen",
    "steelblue",
    "tan",
    "teal",
    "thistle",
    "tomato",
    "turquoise",
    "violet",
    "wheat",
    "white",
    "whitesmoke",
    "yellow",
    "yellowgreen",
];

/// Properties whose values are names, not colors (`font-family: Black`).
const NAME_PROPERTIES: &[&str] = &[
    "font",
    "grid",
    "animation",
    "transition",
    "counter",
    "content",
    "quotes",
    "will-change",
    "list-style",
];

/// `value` without its `url(…)` arguments and quoted strings: a `#hash` or
/// a file name there is not a color.
fn strip_urls_and_strings(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let url = rest.find("url(");
        let quote = rest.find(['"', '\'']);
        let (at, close) = match (url, quote) {
            (Some(u), Some(q)) if q < u => (q, rest[q..=q].to_owned()),
            (Some(u), _) => (u, ")".to_owned()),
            (None, Some(q)) => (q, rest[q..=q].to_owned()),
            (None, None) => break,
        };
        out.push_str(&rest[..at]);
        let skip = if close == ")" { at + 4 } else { at + 1 };
        rest = rest[skip..]
            .find(close.as_str())
            .map_or("", |end| &rest[skip + end + 1..]);
        out.push(' ');
    }
    out.push_str(rest);
    out
}

/// Raw colors in a declaration: hex, color functions and named colors.
fn raw_colors(property: &str, value: &str) -> Vec<String> {
    let lower = strip_urls_and_strings(&value.to_ascii_lowercase());
    let mut found = Vec::new();
    for (at, _) in lower.match_indices('#') {
        let digits = lower[at + 1..]
            .bytes()
            .take_while(u8::is_ascii_hexdigit)
            .count();
        let after = lower.as_bytes().get(at + 1 + digits);
        let ends = after.is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_'));
        if matches!(digits, 3 | 4 | 6 | 8) && ends {
            found.push(lower[at..at + 1 + digits].to_owned());
        }
    }
    for function in COLOR_FUNCTIONS {
        if find_word(&lower, function).is_some() {
            found.push((*function).to_owned());
        }
    }
    if !NAME_PROPERTIES.iter().any(|p| property.starts_with(p)) {
        for word in lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
            if NAMED_COLORS.binary_search(&word).is_ok() {
                found.push(word.to_owned());
            }
        }
    }
    found
}

/// The JS a strict CSP blocks: `eval(`, `Function(` (also with `new`),
/// `setTimeout`/`setInterval` with a string, and `document.write(`.
fn script_problems(js: &str) -> Vec<&'static str> {
    let call = |needle: &str| {
        js.match_indices(needle).any(|(at, _)| {
            js.as_bytes()[..at]
                .last()
                .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$'))
        })
    };
    let string_timer = |needle: &str| {
        js.match_indices(needle).any(|(at, _)| {
            js[at + needle.len()..]
                .trim_start()
                .starts_with(['"', '\'', '`'])
        })
    };
    let mut found = Vec::new();
    if call("eval(") {
        found.push("eval(");
    }
    if call("Function(") {
        found.push("Function(");
    }
    if string_timer("setTimeout(") {
        found.push("setTimeout(\"…\")");
    }
    if string_timer("setInterval(") {
        found.push("setInterval(\"…\")");
    }
    if js.contains("document.write(") || js.contains("document.writeln(") {
        found.push("document.write(");
    }
    found
}

/// The first index of `needle` in `haystack` where the byte before it is
/// not part of an identifier.
fn find_word(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .match_indices(needle)
        .map(|(at, _)| at)
        .find(|&at| {
            haystack.as_bytes()[..at].last().is_none_or(|b| {
                !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$' || *b == b'-' || *b == b'.')
            })
        })
}

/// One start tag: its lowercase name and its `(name, value)` attributes.
struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
}

/// The start tags of `html`, outside comments. This parser is sufficient
/// for maud markup: quoted values, no `>` in a tag name.
fn tags(html: &str) -> Vec<Tag> {
    let bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if html[i..].starts_with("<!--") {
            i = html[i + 4..]
                .find("-->")
                .map_or(bytes.len(), |end| i + 4 + end + 3);
            continue;
        }
        if bytes[i] != b'<' || !bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
            j += 1;
        }
        let name = html[i + 1..j].to_ascii_lowercase();
        let mut attrs = Vec::new();
        loop {
            while j < bytes.len() && (bytes[j].is_ascii_whitespace() || bytes[j] == b'/') {
                j += 1;
            }
            if j >= bytes.len() || bytes[j] == b'>' {
                break;
            }
            let start = j;
            while j < bytes.len()
                && !bytes[j].is_ascii_whitespace()
                && !matches!(bytes[j], b'=' | b'>' | b'/')
            {
                j += 1;
            }
            let attr = html[start..j].to_ascii_lowercase();
            let mut value = "";
            if bytes.get(j) == Some(&b'=') {
                j += 1;
                if let Some(&quote @ (b'"' | b'\'')) = bytes.get(j) {
                    let end = html[j + 1..]
                        .find(quote as char)
                        .map_or(bytes.len(), |e| j + 1 + e);
                    value = &html[j + 1..end];
                    j = (end + 1).min(bytes.len());
                } else {
                    let start = j;
                    while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' {
                        j += 1;
                    }
                    value = &html[start..j];
                }
            }
            if !attr.is_empty() {
                attrs.push((attr, value.to_owned()));
            }
        }
        out.push(Tag { name, attrs });
        i = j;
    }
    out
}

/// Run every static widget check on `pack`.
#[must_use]
pub fn run_widget_conformance(pack: &WidgetPack) -> ConformanceReport {
    ConformanceReport {
        plugin_name: pack.name().to_owned(),
        checks: vec![
            check_widget_pack_name(pack),
            check_widget_stories(pack),
            check_widget_class_names(pack),
            check_widget_markup_csp(pack),
            check_widget_tokens(pack),
            check_widget_scripts(pack),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::PluginAssets;
    use crate::stories::Story;

    fn good_story() -> maud::Markup {
        maud::html! {
            div class="autumn-cgood-swatch autumn-field" {
                button type="button" class="autumn-cgood-chip" aria-pressed="false" { "Primary" }
            }
        }
    }

    static GOOD_ASSETS: PluginAssets = PluginAssets::from_files(
        "cgood",
        &[
            (
                "cgood.css",
                b"/* #fff in a comment is fine */\n\
                  .autumn-cgood-swatch { --autumn-cgood-gap: var(--space-2); gap: var(--autumn-cgood-gap); }\n\
                  #cgood-root .autumn-cgood-chip:hover { color: var(--on-primary); background: var(--primary); border: 1px solid transparent; }\n\
                  @media (min-width: 768px) { .autumn-cgood-chip { padding: var(--space-1) var(--space-3); } }\n",
            ),
            (
                "cgood.js",
                b"document.addEventListener('autumn:init', function (e) { void e; });",
            ),
        ],
    );

    fn good_pack() -> WidgetPack {
        WidgetPack::new("cgood")
            .extend([Story::new(
                "Good",
                "Cgood swatch",
                good_story,
                "good_story()",
            )])
            .assets(&GOOD_ASSETS)
    }

    fn status(report: &ConformanceReport, name: &str) -> CheckStatus {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}: {report:?}"))
            .status
            .clone()
    }

    fn assert_fails(check: &CheckResult, needle: &str) {
        assert_eq!(check.status, CheckStatus::Fail, "{check:?}");
        assert!(
            check.diagnostics.iter().any(|d| d.contains(needle)),
            "no diagnostic with `{needle}`: {check:?}"
        );
    }

    #[test]
    fn a_good_pack_passes_every_static_check() {
        let report = run_widget_conformance(&good_pack());
        assert!(report.passed(), "{}", report.to_text_report());
        assert_eq!(report.plugin_name, "cgood");
        for name in [
            "widget-pack-name",
            "widget-stories",
            "widget-class-names",
            "widget-markup-csp",
            "widget-tokens",
            "widget-scripts",
        ] {
            assert_eq!(status(&report, name), CheckStatus::Pass, "{name}");
        }
    }

    #[test]
    fn a_pack_named_after_a_core_class_fails() {
        assert_fails(
            &check_widget_pack_name(&WidgetPack::new("field")),
            "autumn-field",
        );
        assert_fails(
            &check_widget_pack_name(&WidgetPack::new("nav")),
            "autumn-nav",
        );
    }

    fn empty_story() -> maud::Markup {
        maud::html! {}
    }

    fn panicking_story() -> maud::Markup {
        panic!("boom")
    }

    #[test]
    fn story_problems_fail() {
        assert_fails(
            &check_widget_stories(&WidgetPack::new("cnone")),
            "no stories",
        );
        assert_fails(
            &check_widget_stories(
                &WidgetPack::new("cslug").extend([Story::new("G", "Chip", good_story, "")]),
            ),
            "`chip`",
        );
        assert_fails(
            &check_widget_stories(&WidgetPack::new("cpanic").extend([Story::new(
                "G",
                "Cpanic x",
                panicking_story,
                "",
            )])),
            "cpanic-x",
        );
        assert_fails(
            &check_widget_stories(&WidgetPack::new("cempty").extend([Story::new(
                "G",
                "Cempty x",
                empty_story,
                "",
            )])),
            "cempty-x",
        );
    }

    fn foreign_classes() -> maud::Markup {
        maud::html! {
            div class="autumn-cbad-ok btn  autumn-other-thing" {
                span class="autumn-field__input" { "x" }
            }
        }
    }

    #[test]
    fn foreign_class_names_fail() {
        let check = check_widget_class_names(&WidgetPack::new("cbad").extend([Story::new(
            "G",
            "Cbad x",
            foreign_classes,
            "",
        )]));
        assert_fails(&check, "`btn`");
        assert_fails(&check, "`autumn-other-thing`");
        assert!(
            !check
                .diagnostics
                .iter()
                .any(|d| d.contains("autumn-field__input") || d.contains("autumn-cbad-ok")),
            "{check:?}"
        );
    }

    fn unsafe_markup() -> maud::Markup {
        maud::PreEscaped(
            "<div class=\"autumn-ccsp-x\" onclick=\"go()\" style=\"color:red\">\
             <a class=\"autumn-ccsp-a\" href=\"javascript:go()\">x</a>\
             <script>go()</script></div>"
                .to_owned(),
        )
    }

    fn external_script() -> maud::Markup {
        maud::PreEscaped(
            "<div class=\"autumn-cext-x\"><script src=\"/x.js\"></script></div>".to_owned(),
        )
    }

    #[test]
    fn inline_script_handlers_and_styles_fail() {
        let check = check_widget_markup_csp(&WidgetPack::new("ccsp").extend([Story::new(
            "G",
            "Ccsp x",
            unsafe_markup,
            "",
        )]));
        for needle in ["onclick", "style", "javascript:", "<script>"] {
            assert_fails(&check, needle);
        }
        let check = check_widget_markup_csp(&WidgetPack::new("cext").extend([Story::new(
            "G",
            "Cext x",
            external_script,
            "",
        )]));
        assert_eq!(check.status, CheckStatus::Pass, "{check:?}");
    }

    static BAD_CSS: PluginAssets = PluginAssets::from_files(
        "ctokens",
        &[(
            "ctokens.css",
            b":root { --primary: red; }\n\
              .autumn-ctokens-a { color: #fff; background: rgb(0 0 0); }\n\
              .autumn-ctokens-b { border-color: var(--not-a-token); outline-color: white; }\n\
              .autumn-ctokens-c { fill: oklch(0.5 0.1 200); }\n",
        )],
    );

    #[test]
    fn raw_colors_unknown_tokens_and_token_overrides_fail() {
        let check = check_widget_tokens(&WidgetPack::new("ctokens").assets(&BAD_CSS));
        for needle in [
            "--primary",
            "#fff",
            "rgb(",
            "--not-a-token",
            "white",
            "oklch(",
        ] {
            assert_fails(&check, needle);
        }
    }

    static BAD_JS: PluginAssets = PluginAssets::from_files(
        "cjs",
        &[(
            "cjs.js",
            b"eval('1'); var f = new Function('return 1'); document.write('x');",
        )],
    );

    #[test]
    fn eval_and_document_write_fail() {
        let check = check_widget_scripts(&WidgetPack::new("cjs").assets(&BAD_JS));
        for needle in ["eval(", "Function(", "document.write("] {
            assert_fails(&check, needle);
        }
    }

    fn more_unsafe_markup() -> maud::Markup {
        maud::PreEscaped(
            "<div class=\"autumn-cmore-x\" hx-on:click=\"go()\" data-hx-on--load=\"go()\">\
             <!-- <script>commented()</script> -->\
             <style>.x { color: red; }</style></div>"
                .to_owned(),
        )
    }

    #[test]
    fn htmx_handlers_and_inline_styles_fail_but_comments_do_not() {
        let check = check_widget_markup_csp(&WidgetPack::new("cmore").extend([Story::new(
            "G",
            "Cmore x",
            more_unsafe_markup,
            "",
        )]));
        for needle in ["hx-on:click", "data-hx-on--load", "<style>"] {
            assert_fails(&check, needle);
        }
        assert!(
            !check.diagnostics.iter().any(|d| d.contains("<script>")),
            "a script in a comment is not a script: {check:?}"
        );
    }

    static MORE_CSS: PluginAssets = PluginAssets::from_files(
        "cmorecss",
        &[(
            "cmorecss.css",
            b"@import url(https://example.com/x.css);\n\
              .autumn-cmorecss-a { color: VAR(--private); --autumn-cmorecss-fg: crimson; }\n\
              .autumn-cmorecss-b { fill: url(#abc); background: url(/img/white.png); }\n\
              .autumn-cmorecss-c::before { content: \"#fff\"; font-family: Black, sans-serif; }\n",
        )],
    );

    #[test]
    fn imports_case_and_named_colors_fail_but_urls_and_strings_do_not() {
        let check = check_widget_tokens(&WidgetPack::new("cmorecss").assets(&MORE_CSS));
        for needle in ["@import", "--private", "crimson"] {
            assert_fails(&check, needle);
        }
        for clean in ["#abc", "`white`", "#fff", "black"] {
            assert!(
                !check.diagnostics.iter().any(|d| d.contains(clean)),
                "`{clean}` is not a raw color here: {check:?}"
            );
        }
    }

    static MORE_JS: PluginAssets = PluginAssets::from_files(
        "cmorejs",
        &[(
            "cmorejs.js",
            b"window.eval('1'); var f = Function('return 1'); setTimeout('go()', 1); \
              setInterval(go, 1); document.writeln('x'); retrieval(1);",
        )],
    );

    #[test]
    fn more_eval_forms_fail() {
        let check = check_widget_scripts(&WidgetPack::new("cmorejs").assets(&MORE_JS));
        for needle in ["eval(", "Function(", "setTimeout(", "document.write("] {
            assert_fails(&check, needle);
        }
        assert!(
            !check.diagnostics.iter().any(|d| d.contains("setInterval")),
            "a function argument is fine: {check:?}"
        );
    }

    static WRONG_NAMESPACE: PluginAssets = PluginAssets::from_files("not-cns", &[("x.css", b"")]);

    #[test]
    fn a_bundle_namespace_that_is_not_the_pack_name_fails() {
        assert_fails(
            &check_widget_pack_name(&WidgetPack::new("cns").assets(&WRONG_NAMESPACE)),
            "`not-cns`",
        );
    }

    #[test]
    fn named_colors_are_sorted_for_binary_search() {
        assert!(NAMED_COLORS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_pack_without_assets_skips_the_asset_checks() {
        let pack = WidgetPack::new("cbare").extend([Story::new("G", "Cbare x", good_story, "")]);
        assert_eq!(check_widget_tokens(&pack).status, CheckStatus::Skip);
        assert_eq!(check_widget_scripts(&pack).status, CheckStatus::Skip);
    }
}
