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
    // Once a paired-escape scan proves the unscanned tail holds no `]` at
    // all — quoted or otherwise — no later scan can find a close either (a
    // shorter suffix of a `]`-free suffix is still `]`-free), so the `[[`
    // fast path stops rescanning the tail. Without this, a run of `[` with
    // no `]` anywhere costs a full tail scan per opener — quadratic in the
    // input length — on a function that renders public pages and feeds.
    let mut no_close_ahead = false;
    // A scan that fails at a newline with no quotes met proves every later
    // `[[` before that newline fails identically (a subsegment of a
    // quote-free, close-free segment is too), so those scans are skipped.
    // Without this, a run of `[` ending in a newline costs a full
    // line-scan per opener — quadratic — just like the unpaired-close run
    // that `collapse_bracket_run` handles.
    let mut dead_line_end: Option<usize> = None;

    while i < bytes.len() {
        if bytes[i] != '[' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // `[[` is an escaped literal bracket, as in WordPress: `[[tag]]`
        // renders the literal `[tag]`, with the shortcode left unexpanded.
        // The paired close is consumed too — emitting `[` for the escape
        // and then copying the rest verbatim would leave a stray `]` (#2678).
        if bytes.get(i + 1) == Some(&'[') {
            // A paired escape needs the inner tag's close. Unsuccessful
            // scans must not rescan the tail on every later opener
            // (quadratic): a scan that proves the tail has no `]` at all
            // arms `no_close_ahead`, a scan that fails at a newline with no
            // quotes met arms `dead_line_end`, and a scan that finds an
            // unpaired close in a quote-free region is consumed outright by
            // `collapse_bracket_run`.
            if !no_close_ahead {
                if dead_line_end.is_some_and(|end| i + 1 < end) {
                    out.push('[');
                    i += 2;
                    continue;
                }
                let scan = scan_close(&bytes, i + 1);
                if let Some(close) = scan.close {
                    if scan.paired {
                        let inner: String = bytes[i + 2..close].iter().collect();
                        out.push('[');
                        out.push_str(&inner);
                        out.push(']');
                        i = close + 2;
                        continue;
                    }
                    if scan.clean {
                        i = collapse_bracket_run(&bytes, &registry, &mut out, i, close);
                        continue;
                    }
                } else if let Some(end) = scan.newline {
                    if scan.clean {
                        dead_line_end = Some(end);
                    }
                } else if !scan.saw_bracket {
                    no_close_ahead = true;
                }
            }
            out.push('[');
            i += 2;
            continue;
        }
        let Some(close) = find_close(&bytes, i) else {
            out.push(bytes[i]);
            i += 1;
            continue;
        };
        i = emit_tag(&mut out, &registry, &bytes, i, close);
    }
    out
}

/// Find the `]` closing the tag opened at `open`, ignoring brackets inside a
/// quoted attribute value.
fn find_close(chars: &[char], open: usize) -> Option<usize> {
    scan_close(chars, open).close
}

/// The outcome of scanning for the `]` closing the tag opened at `open`,
/// with the same quote/newline rules as [`find_close`].
struct CloseScan {
    /// The first unquoted `]`, if the scan found one before a newline or the
    /// end of input.
    close: Option<usize>,
    /// Whether the char after `close` is another `]` — the `[[…]]` paired
    /// escape. Only meaningful when `close` is `Some`.
    paired: bool,
    /// The scan met no `"` and no `\n` before resolving. A clean scan that
    /// found an unpaired close (or failed at a newline) proves the same for
    /// every opener inside its segment: a subsegment of a quote-free,
    /// newline-free segment is too, and the first unquoted `]` cannot move.
    clean: bool,
    /// When no close was found: the newline that stopped the scan, if any.
    newline: Option<usize>,
    /// Whether the scan met any `]` at all, quoted or otherwise. A scan that
    /// reaches end-of-input without meeting one proves no later scan can find
    /// a close either (a shorter suffix of a `]`-free suffix is still
    /// `]`-free). A quoted `]` earlier in the tail can mask a real close for
    /// a later scan start, so only a completely `]`-free tail is conclusive.
    saw_bracket: bool,
}

fn scan_close(chars: &[char], open: usize) -> CloseScan {
    let mut in_quotes = false;
    let mut scan = CloseScan {
        close: None,
        paired: false,
        clean: true,
        newline: None,
        saw_bracket: false,
    };
    for (offset, ch) in chars.iter().enumerate().skip(open + 1) {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                scan.clean = false;
            }
            ']' => {
                scan.saw_bracket = true;
                if !in_quotes {
                    scan.close = Some(offset);
                    scan.paired = chars.get(offset + 1) == Some(&']');
                    return scan;
                }
            }
            // A newline inside a tag means it was never a tag.
            '\n' => {
                scan.newline = Some(offset);
                return scan;
            }
            _ => {}
        }
    }
    scan
}

/// Emit the tag `[bytes[open + 1..close]]`: expand the handler when its name
/// is registered, otherwise emit the tag verbatim. Returns the index to
/// continue scanning from. A `raw` that is not a tag emits just `[` and
/// reports `open + 1`, so the caller retries after the bracket.
fn emit_tag(
    out: &mut String,
    registry: &std::collections::BTreeMap<String, Handler>,
    bytes: &[char],
    open: usize,
    close: usize,
) -> usize {
    let raw: String = bytes[open + 1..close].iter().collect();
    match parse_tag(&raw) {
        Some((name, attrs)) => match registry.get(&name) {
            Some(handler) => {
                out.push_str(&handler(&attrs));
            }
            None => {
                // Unregistered: emit the original text verbatim.
                out.push('[');
                out.push_str(&raw);
                out.push(']');
            }
        },
        None => {
            out.push('[');
            return open + 1;
        }
    }
    close + 1
}

/// Collapse a bracket run without rescanning: `i` is a `[[`, `close` is the
/// first `]` at or after `i` (not paired), and `[i, close]` holds no `"` or
/// `\n`.
///
/// Every `[[` in the region is known to find the same `close`, unpaired, so
/// each collapses to `[`; every single `[` is known to close at `close`, so
/// it is processed as a tag with that close. Anything else is copied
/// verbatim. Returns the index just past `close`. Each character is visited
/// once, so a run of openers stays linear instead of rescanning the tail per
/// opener.
///
/// The shared `close` is emitted literally only if no inner tag consumed it:
/// a tag that parses (registered or verbatim) consumes the close exactly as
/// the ordinary path would, so re-emitting it would duplicate the `]`.
fn collapse_bracket_run(
    bytes: &[char],
    registry: &std::collections::BTreeMap<String, Handler>,
    out: &mut String,
    mut i: usize,
    close: usize,
) -> usize {
    // The `[[` at `i` collapses to `[`.
    out.push('[');
    i += 2;
    while i < close {
        if bytes[i] == '[' && bytes.get(i + 1) == Some(&'[') {
            out.push('[');
            i += 2;
        } else if bytes[i] == '[' {
            i = emit_tag(out, registry, bytes, i, close);
            if i == close + 1 {
                // The tag consumed the shared close; nothing left to emit.
                return i;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out.push(']');
    close + 1
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
    fn paired_double_brackets_strip_one_bracket_from_each_side() {
        // WordPress parity: `[[tag]]` renders the literal `[tag]`.
        assert_eq!(expand("[[x]]"), "[x]");
        assert_eq!(expand("[[note attr=\"v\"]]"), "[note attr=\"v\"]");
        assert_eq!(expand("[[unregistered]]"), "[unregistered]");
        assert_eq!(
            expand("a [[x]] b [[y]] c"),
            "a [x] b [y] c",
            "the escape is uniform across the whole string"
        );
    }

    #[test]
    fn paired_double_brackets_suppress_expansion() {
        add_shortcode("pairednote", |_| "<aside>expanded</aside>".to_string());
        // The escape exists to *mention* a shortcode, so a registered name
        // must render literally, not expand.
        assert_eq!(expand("[[pairednote]]"), "[pairednote]");
    }

    #[test]
    fn a_triple_opening_keeps_one_extra_bracket() {
        // `[[[x]]]` strips one pair, leaving the outermost literal pair.
        assert_eq!(expand("[[[x]]]"), "[[x]]");
    }

    #[test]
    fn brackets_with_no_close_anywhere_collapse_pairwise() {
        // The no-rescan fast path must not change output: with no `]` in the
        // tail at all, every opener is literal.
        assert_eq!(expand("[[[["), "[[");
        assert_eq!(expand("a [[ b"), "a [ b");
        assert_eq!(expand("[["), "[");
    }

    #[test]
    fn quoted_brackets_do_not_poison_later_scans() {
        // `]`s seen only inside quotes must NOT arm the no-rescan fast path:
        // a scan started later, past the quotes, can still find a real close,
        // and the shortcode there must expand.
        add_shortcode("quotegal", |_| "<em>q</em>".to_string());
        // The first scan (from the `[[`) drowns in the quoted `]`s and fails;
        // the later scan finds `[quotegal]`'s close and expands it.
        assert_eq!(expand("[[\" ][quotegal]"), "[\" ]<em>q</em>");
        assert_eq!(expand("[[\"\"][quotegal]"), "[\"\"]<em>q</em>");
    }

    #[test]
    fn a_bracket_run_with_no_close_stays_linear() {
        // P2 review: each `[[` rescanning the whole tail makes `expand`
        // quadratic on `[` x N. 100k openers hold no `]` at all; the old code
        // needed ~10^10 char inspections here, the single-scan path ~10^5.
        let input = "[".repeat(100_000);
        let start = std::time::Instant::now();
        let out = expand(&input);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "bracket run took {:?}",
            start.elapsed()
        );
        assert_eq!(out, "[".repeat(50_000));
    }

    #[test]
    fn many_open_brackets_with_single_unpaired_close_stay_linear() {
        // P2 review follow-up: the run above proved the `]`-free tail, but a
        // run ending in ONE `]` rescanned the whole tail per opener — each
        // `[[` found the same non-paired close and advanced only two chars.
        // The collapse path visits each char once instead.
        let input = format!("{}]", "[".repeat(100_000));
        let start = std::time::Instant::now();
        let out = expand(&input);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "bracket run with single close took {:?}",
            start.elapsed()
        );
        assert_eq!(out, format!("{}]", "[".repeat(50_000)));
    }

    #[test]
    fn bracket_run_then_newline_then_close_stays_linear() {
        // Same as above, but the scan first fails at the newline: the tail
        // after it is re-scanned, but each opener advances, so the whole
        // input is still visited once.
        let input = format!("{}\n]", "[".repeat(100_000));
        let start = std::time::Instant::now();
        let out = expand(&input);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "bracket run with newline and close took {:?}",
            start.elapsed()
        );
        assert_eq!(out, format!("{}\n]", "[".repeat(50_000)));
    }

    #[test]
    fn unpaired_close_run_collapses_with_correct_output() {
        // The collapse path must render exactly what the old per-opener
        // rescan produced: each `[[` becomes `[`, each lone `[` emits
        // literally when its close makes it a non-tag, and `]` copies over.
        assert_eq!(expand("[[[a]"), "[[a]");
        assert_eq!(expand("[[[a] b"), "[[a] b");
        assert_eq!(expand("[x] [y]"), "[x] [y]");
        assert_eq!(expand("[[[[x]"), "[[x]");
        assert_eq!(expand("[[a][b]"), "[a][b]");
    }

    #[test]
    fn bracket_run_unpaired_close_then_valid_shortcode() {
        // After collapsing a bracket run, later content keeps scanning
        // normally — a real shortcode past the run still expands.
        add_shortcode("late", |_| "<em>late</em>".to_string());
        assert_eq!(expand("[[[a] [late]"), "[[a] <em>late</em>");
    }

    #[test]
    fn paired_escape_prose_with_quoted_escapes() {
        // `"` or `\n` in the region disables the collapse fast path (they
        // can mask a real close), but paired escapes still render as before.
        assert_eq!(expand("[[x\"]]"), "[x\"]]");
        assert_eq!(expand("[[\"x]]"), "[\"x]]");
        add_shortcode("quotecollapse", |_| "<em>q</em>".to_string());
        assert_eq!(
            expand("[[\"[quotecollapse]"),
            "[\"<em>q</em>",
            "quotes block the collapse fast path but not later expansion"
        );
    }

    #[test]
    fn single_brackets_with_single_close_do_not_rescan() {
        // The `[[` collapse path also pairs up a plain run of `[`: one close
        // lookup, no rescans.
        let input = format!("{}]", "[".repeat(50_000));
        let start = std::time::Instant::now();
        let out = expand(&input);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "single-bracket run took {:?}",
            start.elapsed()
        );
        assert_eq!(out, format!("{}]", "[".repeat(25_000)));
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
