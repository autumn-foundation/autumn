//! Shortcodes — `[gallery id="3"]` in a post body, expanded at render time.
//!
//! WordPress's shortcode API is how a non-technical author embeds dynamic
//! content in prose without writing HTML, and it predates (and still outlives)
//! the block editor. The parser here accepts the same syntax WordPress does for
//! the self-closing form — `[name]`, `[name attr="value"]`, `[name attr=value]`
//! — which is the form the overwhelming majority of shortcodes take.
//!
//! Enclosing shortcodes (`[caption]…[/caption]`) are deliberately not supported;
//! see the note on [`expand`].

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// The attributes parsed off a shortcode tag.
pub type Attributes = BTreeMap<String, String>;

/// `Arc` rather than `Box` so `expand` can clone the handlers it needs and drop
/// the registry lock before running any of them — the same reason the action
/// and filter dispatchers hold `Arc`s. See `expand`.
type Handler = std::sync::Arc<dyn Fn(&Attributes) -> String + Send + Sync>;

fn registry() -> &'static RwLock<BTreeMap<String, Handler>> {
    static REGISTRY: OnceLock<RwLock<BTreeMap<String, Handler>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register a shortcode. WordPress's `add_shortcode`.
///
/// The handler returns **HTML**, which is inserted into the rendered document
/// verbatim — so a handler is trusted code, exactly as a WordPress shortcode
/// callback is. Anything it interpolates from user input must be escaped by the
/// handler itself; [`escape_html`] is provided for that.
pub fn add_shortcode(name: &str, handler: impl Fn(&Attributes) -> String + Send + Sync + 'static) {
    registry()
        .write()
        .expect("shortcode registry poisoned")
        .insert(name.to_ascii_lowercase(), std::sync::Arc::new(handler));
}

/// The names of every registered shortcode, for the editor's help panel.
#[must_use]
pub fn registered() -> Vec<String> {
    registry()
        .read()
        .expect("shortcode registry poisoned")
        .keys()
        .cloned()
        .collect()
}

/// Expand every registered shortcode in `input`.
///
/// Unregistered shortcodes are left **exactly as written**, which is WordPress's
/// behaviour and the right one: prose containing `[citation needed]` must
/// survive a round trip through the renderer unchanged.
///
/// Only self-closing shortcodes are recognised. An enclosing pair
/// (`[caption]…[/caption]`) needs a nesting-aware scanner and a content
/// argument, and every shortcode this CMS ships is self-closing; supporting the
/// enclosing form badly — matching the first `[/name]` regardless of nesting —
/// would corrupt exactly the documents it was added for.
#[must_use]
pub fn expand(input: &str) -> String {
    // A snapshot, not a held guard. Running a handler while holding the read
    // lock hangs the page the moment that handler calls `add_shortcode`:
    // `RwLock` is not reentrant, so the write blocks on a read the same thread
    // holds. Registering from inside a handler is an ordinary thing for a
    // plugin to do, and this is the same fix `do_action` and `apply_filters`
    // already carry.
    //
    // Cloning the whole map costs one `Arc` bump per registered shortcode —
    // there are a handful — and it is what lets the handlers run unlocked.
    let registry: std::collections::BTreeMap<String, Handler> = registry()
        .read()
        .expect("shortcode registry poisoned")
        .iter()
        .map(|(name, handler)| (name.clone(), std::sync::Arc::clone(handler)))
        .collect();
    let mut out = String::with_capacity(input.len());
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != '[' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // `[[` is an escaped literal bracket, as in WordPress.
        if bytes.get(i + 1) == Some(&'[') {
            out.push('[');
            i += 2;
            continue;
        }
        let Some(close) = find_close(&bytes, i) else {
            out.push(bytes[i]);
            i += 1;
            continue;
        };
        let raw: String = bytes[i + 1..close].iter().collect();
        match parse_tag(&raw) {
            Some((name, attrs)) => match registry.get(&name) {
                Some(handler) => {
                    out.push_str(&handler(&attrs));
                    i = close + 1;
                }
                None => {
                    // Unregistered: emit the original text verbatim.
                    out.push('[');
                    out.push_str(&raw);
                    out.push(']');
                    i = close + 1;
                }
            },
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// Find the `]` closing the tag opened at `open`, ignoring brackets inside a
/// quoted attribute value.
fn find_close(chars: &[char], open: usize) -> Option<usize> {
    let mut in_quotes = false;
    for (offset, ch) in chars.iter().enumerate().skip(open + 1) {
        match ch {
            '"' => in_quotes = !in_quotes,
            ']' if !in_quotes => return Some(offset),
            // A newline inside a tag means it was never a tag.
            '\n' => return None,
            _ => {}
        }
    }
    None
}

/// Parse `name attr="value" other=bare` into its name and attributes.
fn parse_tag(raw: &str) -> Option<(String, Attributes)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut chars = raw.chars().peekable();
    let mut name = String::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            break;
        }
        name.push(ch);
        chars.next();
    }
    // A shortcode name is a slug. Anything else — a markdown link's `[text]`,
    // a `[citation needed]` — is left alone by returning `None`.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }

    let mut attrs = Attributes::new();
    let rest: String = chars.collect();
    let mut cursor = rest.trim();
    while !cursor.is_empty() {
        let Some(eq) = cursor.find('=') else { break };
        let key = cursor[..eq].trim().to_ascii_lowercase();
        let after = cursor[eq + 1..].trim_start();
        let (value, consumed) = if let Some(stripped) = after.strip_prefix('"') {
            match stripped.find('"') {
                Some(end) => (
                    stripped[..end].to_owned(),
                    eq + 1 + (after.len() - stripped.len()) + end + 1,
                ),
                None => break,
            }
        } else {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            (
                after[..end].to_owned(),
                eq + 1 + (cursor[eq + 1..].len() - after.len()) + end,
            )
        };
        if !key.is_empty() {
            attrs.insert(key, value);
        }
        if consumed >= cursor.len() {
            break;
        }
        cursor = cursor[consumed..].trim_start();
    }
    Some((name.to_ascii_lowercase(), attrs))
}

/// Escape text for interpolation into a shortcode's HTML output.
#[must_use]
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unregistered_shortcodes_survive_verbatim() {
        // The property that keeps ordinary prose safe.
        assert_eq!(expand("a [citation needed] b"), "a [citation needed] b");
        assert_eq!(expand("see [1] and [2]"), "see [1] and [2]");
        assert_eq!(
            expand("[link](https://example.com)"),
            "[link](https://example.com)",
            "a markdown link must not be eaten as a shortcode"
        );
    }

    #[test]
    fn a_registered_shortcode_expands_with_its_attributes() {
        add_shortcode("testbox", |attrs| {
            format!(
                "<div class=\"{}\">{}</div>",
                escape_html(attrs.get("class").map_or("", String::as_str)),
                escape_html(attrs.get("text").map_or("", String::as_str)),
            )
        });
        assert_eq!(
            expand(r#"before [testbox class="note" text="Hi there"] after"#),
            r#"before <div class="note">Hi there</div> after"#
        );
    }

    #[test]
    fn bare_and_quoted_attribute_values_both_parse() {
        add_shortcode("attrs", |attrs| {
            attrs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",")
        });
        assert_eq!(
            expand("[attrs id=7 name=\"two words\"]"),
            "id=7,name=two words"
        );
    }

    #[test]
    fn escaped_double_brackets_render_a_literal() {
        assert_eq!(expand("[[testbox]"), "[testbox]");
    }

    #[test]
    fn an_unterminated_tag_is_left_alone() {
        assert_eq!(expand("[testbox class=\"x"), "[testbox class=\"x");
        assert_eq!(expand("a [ b"), "a [ b");
    }

    #[test]
    fn shortcode_output_escapes_interpolated_input() {
        add_shortcode("echo", |attrs| {
            escape_html(attrs.get("v").map_or("", String::as_str))
        });
        assert_eq!(
            expand(r#"[echo v="<script>alert(1)</script>"]"#),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
    }
}
