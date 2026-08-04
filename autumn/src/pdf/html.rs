//! A small, tolerant HTML-subset parser.
//!
//! This is **not** a general HTML5 parser: it is just enough tag/text/entity
//! handling to walk a Maud-rendered (or hand-written) HTML string into a tree
//! [`crate::pdf::layout`] can lay out as PDF text. It never panics on
//! malformed input — unmatched closing tags are ignored, unclosed tags are
//! auto-closed at end of input, and it parses iteratively (no recursion) so
//! adversarially deep nesting can't blow the stack.

/// A parsed node: either a run of text or an element with children.
#[derive(Debug, PartialEq)]
pub(super) enum Node {
    Text(String),
    Element { tag: String, children: Vec<Self> },
}

impl Drop for Node {
    /// Drop a (possibly very deep) tree iteratively.
    ///
    /// The compiler-generated `Drop` glue for a recursive type like this one
    /// would drop each level's children by recursing — for a pathologically
    /// deep tree (see the parser's own stack-safety test) that overflows the
    /// stack even though *parsing* never recurses. This flattens the tree
    /// into an explicit heap-allocated work list instead: each popped node's
    /// children are moved out onto the list before the (now childless, so
    /// trivially-dropped) node itself goes out of scope.
    fn drop(&mut self) {
        let mut stack: Vec<Self> = if let Self::Element { children, .. } = self {
            std::mem::take(children)
        } else {
            return;
        };
        while let Some(mut node) = stack.pop() {
            if let Self::Element { children, .. } = &mut node {
                stack.append(children);
            }
        }
    }
}

/// HTML elements with no content and no closing tag.
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "br" | "hr"
            | "img"
            | "input"
            | "meta"
            | "link"
            | "area"
            | "base"
            | "col"
            | "embed"
            | "source"
            | "track"
            | "wbr"
    )
}

/// "Raw text" elements per the HTML5 parsing spec: their content is never
/// tokenized as markup at all, even if it contains characters that look
/// like tags (e.g. a JS comparison `a<b` inside `<script>`, or a CSS `>`
/// combinator inside `<style>`) — only the literal closing tag ends them.
/// Without this, such a `<` can be parsed as a bogus opening tag that
/// swallows the real `</script>`, leaving the element unclosed and nesting
/// (and, since `is_non_rendered` in `layout.rs` skips its subtree, hiding)
/// everything that follows.
fn is_raw_text_element(tag: &str) -> bool {
    matches!(tag, "script" | "style")
}

/// Bound on how far [`consume_raw_text`] scans past a candidate
/// `</script`/`</style` prefix looking for the closing `>` — the same
/// defense as [`MAX_TAG_SCAN`] for the same reason: a legitimate closing
/// tag has no attributes and essentially no whitespace before `>`, but an
/// unbounded `after_tag.find('>')` would cost O(remaining input) on every
/// failed candidate — a raw-text body containing many `"</script "`
/// fragments with no `>` anywhere would make this O(n^2) overall, the same
/// shape of bug [`MAX_TAG_SCAN`]/[`MAX_CLOSE_SCAN`] were fixed for.
const MAX_RAW_CLOSE_SCAN: usize = 64;

/// Scan `input[pos..]` for the literal, case-insensitive closing tag for a
/// [raw text element](is_raw_text_element) (e.g. `</script>`, allowing
/// whitespace before the `>`), returning the text before it and the
/// position just past the closing tag. If no closing tag is found, all of
/// `input[pos..]` is returned as text with the position at end of input —
/// matching this parser's usual "auto-close at EOF" tolerance for
/// unterminated tags.
fn consume_raw_text<'a>(input: &'a str, pos: usize, tag: &str) -> (&'a str, usize) {
    let rest = &input[pos..];
    for (i, _) in rest.match_indices('<') {
        let Some(after_slash) = rest[i + 1..].strip_prefix('/') else {
            continue;
        };
        if after_slash.len() < tag.len()
            || !after_slash.as_bytes()[..tag.len()].eq_ignore_ascii_case(tag.as_bytes())
        {
            continue;
        }
        let after_tag = &after_slash[tag.len()..];
        // Must be immediately followed by whitespace or '>' — not e.g.
        // "</scripty>" merely starting with "script".
        let is_boundary = after_tag
            .chars()
            .next()
            .is_none_or(|c| c == '>' || c.is_whitespace());
        if !is_boundary {
            continue;
        }
        let Some(gt) = bounded_prefix(after_tag, MAX_RAW_CLOSE_SCAN).find('>') else {
            continue;
        };
        let consumed = i + 2 + tag.len() + gt + 1;
        return (&rest[..i], pos + consumed);
    }
    (rest, input.len())
}

/// Bound on how many frames back [`parse`]'s closing-tag matcher scans
/// looking for the nearest open tag with a given name. Well-formed HTML
/// (even deeply nested) closes tags in the order they were opened, so a
/// match is almost always found in the last frame or two; scanning the
/// *entire* stack on every close only matters for malformed input that
/// closes an ancestor while many descendants are still open. Without a
/// bound, a long run of opens followed by a long run of non-matching closes
/// (e.g. `"<a>".repeat(n) + "</x>".repeat(n)`, where "x" never appears on
/// the stack so no close ever pops it) costs O(n) per close for O(n^2)
/// overall — the same shape of bug already fixed for `decode_entities` and
/// `parse_open_tag`. A close whose matching opener is further back than
/// this is treated the same as a close with no matching opener at all:
/// ignored, rather than auto-closing a deep run of intervening tags.
const MAX_CLOSE_SCAN: usize = 512;

/// Parse `input` into a forest of top-level [`Node`]s.
pub(super) fn parse(input: &str) -> Vec<Node> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    // Stack of (tag_name, children-so-far). The implicit root is index 0 with
    // an empty tag name; it is never popped.
    let mut stack: Vec<(String, Vec<Node>)> = vec![(String::new(), Vec::new())];

    while pos < len {
        if bytes[pos] == b'<' {
            if input[pos..].starts_with("<!--") {
                let end = input[pos..].find("-->").map_or(len, |i| pos + i + 3);
                pos = end;
                continue;
            }
            if input[pos..].starts_with("<!") || input[pos..].starts_with("<?") {
                // Doctype / processing-instruction-like: skip to the next '>'.
                let end = input[pos..].find('>').map_or(len, |i| pos + i + 1);
                pos = end;
                continue;
            }
            if let Some(rest) = input[pos..].strip_prefix("</") {
                let name_end = rest.find('>').unwrap_or(rest.len());
                let name = rest[..name_end].trim().to_ascii_lowercase();
                pos += 2 + name_end + usize::from(name_end < rest.len());

                // Close frames up to and including the matching open tag, if
                // any exists within the last MAX_CLOSE_SCAN frames of the
                // stack. A stray/mismatched close tag with no matching
                // opener nearby is ignored rather than corrupting the tree.
                // An empty tag name (`</>`) must never match: the implicit
                // root frame is *also* keyed by an empty string (it has no
                // tag), and popping it would leave `stack` empty.
                let matching_depth = if name.is_empty() {
                    None
                } else {
                    let window_start = stack.len().saturating_sub(MAX_CLOSE_SCAN);
                    stack[window_start..]
                        .iter()
                        .rposition(|(tag, _)| *tag == name)
                        .map(|i| window_start + i)
                };
                if let Some(depth) = matching_depth {
                    while stack.len() > depth {
                        let (tag, children) = stack.pop().expect("depth <= stack.len()");
                        stack
                            .last_mut()
                            .expect("root frame is never popped")
                            .1
                            .push(Node::Element { tag, children });
                    }
                }
                continue;
            }
            if let Some((tag, self_closing, tag_end)) = parse_open_tag(&input[pos..]) {
                pos += tag_end;
                if !self_closing && is_raw_text_element(&tag) {
                    // `<script>`/`<style>` content is never tokenized as
                    // markup — see `consume_raw_text` — so a `<` that
                    // merely *looks* like the start of a tag (a JS
                    // comparison, a CSS selector combinator) can't swallow
                    // the real closing tag.
                    let (text, new_pos) = consume_raw_text(input, pos, &tag);
                    pos = new_pos;
                    let mut children = Vec::new();
                    if !text.is_empty() {
                        children.push(Node::Text(text.to_owned()));
                    }
                    stack
                        .last_mut()
                        .expect("root frame is never popped")
                        .1
                        .push(Node::Element { tag, children });
                    continue;
                }
                if self_closing || is_void_element(&tag) {
                    stack
                        .last_mut()
                        .expect("root frame is never popped")
                        .1
                        .push(Node::Element {
                            tag,
                            children: Vec::new(),
                        });
                } else {
                    stack.push((tag, Vec::new()));
                }
                continue;
            }
            // A lone '<' that isn't a recognizable tag: treat as literal text.
            push_text(&mut stack, "<");
            pos += 1;
            continue;
        }

        let next_lt = input[pos..].find('<').map_or(len, |i| pos + i);
        let raw = &input[pos..next_lt];
        if !raw.is_empty() {
            push_text(&mut stack, &decode_entities(raw));
        }
        pos = next_lt;
    }

    // Auto-close any still-open tags at end of input.
    while stack.len() > 1 {
        let (tag, children) = stack.pop().expect("stack.len() > 1");
        stack
            .last_mut()
            .expect("root frame is never popped")
            .1
            .push(Node::Element { tag, children });
    }

    stack.pop().expect("root frame always present").1
}

fn push_text(stack: &mut [(String, Vec<Node>)], text: &str) {
    if text.is_empty() {
        return;
    }
    let top = stack.last_mut().expect("root frame is never popped");
    if let Some(Node::Text(prev)) = top.1.last_mut() {
        prev.push_str(text);
    } else {
        top.1.push(Node::Text(text.to_owned()));
    }
}

/// Parse an opening tag starting at `s[0] == '<'`.
///
/// Returns `(tag_name, self_closing, total_consumed_len)`, or `None` if `s`
/// doesn't start with a well-formed tag (e.g. `< foo>` with a space, or an
/// unterminated `<foo`).
///
/// Attributes are ignored entirely (no CSS support), including their
/// contents — so a literal `>` inside a quoted attribute value is not
/// specially handled and will end the tag early. Developer-authored template
/// markup essentially never does this in practice; see the module docs.
/// Bound on how far [`parse_open_tag`] scans looking for the closing `>`. A
/// long run of unterminated `<tag` fragments with no `>` anywhere
/// (adversarial or just malformed input, e.g. `"<a".repeat(n)`) would
/// otherwise make that scan cover the *entire remainder* of the document —
/// and because a failed parse doesn't consume any input (the caller falls
/// back to treating just the `<` as literal text and retries at the very
/// next byte), that unbounded cost gets paid again at every subsequent `<` —
/// O(n^2) overall, the same shape of bug `decode_entities` was fixed for.
/// This bound must stay in place (an earlier draft that dropped it entirely
/// reintroduced the O(n^2) scan cost) but doesn't need to be tight: the
/// tag-name allocation below only happens *after* `>` is found, so a large
/// window costs nothing extra on the (bounded-scan, no-allocation) failure
/// path — only real matches pay for the window size, and a real match is
/// one allocation per tag in the document, not per byte scanned. Sized
/// generously (4 KiB) so a long but genuine attribute list — a Tailwind
/// utility-class soup, several `data-*`/`aria-*` attributes — still parses
/// instead of being rejected and leaked into the PDF as literal text.
const MAX_TAG_SCAN: usize = 4096;

/// Take a byte-length-bounded, UTF-8-safe prefix of `s` — cheap regardless
/// of `max_len`'s size (no per-char iterator overhead, unlike walking
/// `s.char_indices()` up to the bound), nudged back to the nearest char
/// boundary so the result can't split a multi-byte character. Used to cap
/// a scan for a delimiter (`>`) at a fixed cost instead of the full
/// remaining input length — see [`MAX_TAG_SCAN`] and
/// [`MAX_RAW_CLOSE_SCAN`] for why that bound matters.
fn bounded_prefix(s: &str, max_len: usize) -> &str {
    let mut end = s.len().min(max_len);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn parse_open_tag(s: &str) -> Option<(String, bool, usize)> {
    debug_assert!(s.starts_with('<'));
    let rest = &s[1..];
    let first = rest.chars().next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }

    let window = bounded_prefix(rest, MAX_TAG_SCAN);

    // Find `>` first and bail before doing any allocation if it's not in the
    // window — the common failure case (a malformed/unterminated `<`) then
    // costs only the bounded scan above, never a string allocation.
    let gt = window.find('>')?;

    let name_end = window[..gt]
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(gt);
    let tag = window[..name_end].to_ascii_lowercase();
    let self_closing = window[..gt].trim_end().ends_with('/');
    Some((tag, self_closing, 1 + gt + 1))
}

/// Decode the small set of entities likely to appear in developer-authored
/// (Maud-escaped) HTML: the five predefined XML entities, `&nbsp;` and a
/// handful of common typographic entities, and numeric character references.
/// Anything unrecognized is passed through unchanged (including the leading
/// `&`) rather than dropped, so malformed input never loses data.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices();
    while let Some((i, ch)) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let rest = &raw[i..];
        // Bound the semicolon search to a small *character* window before
        // scanning, not after: searching the unbounded remainder for a `;`
        // and only checking the offset afterward means a long run of `&`
        // with no nearby `;` rescans the whole rest of `raw` for every `&`
        // — O(n^2) on adversarial input (e.g. thousands of bare `&`
        // characters). All supported entity names are ASCII and at most 6
        // characters, so an 11-character window (`&` + up to 10 name chars)
        // is ample headroom while keeping each `&` O(1) to resolve.
        let window_end = rest
            .char_indices()
            .nth(11)
            .map_or(rest.len(), |(off, _)| off);
        let Some(semi) = rest[..window_end].find(';') else {
            out.push('&');
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = decode_one_entity(entity);
        match decoded {
            Some(c) => {
                out.push(c);
                // Advance the outer iterator past the consumed entity body.
                for _ in 0..semi {
                    chars.next();
                }
            }
            None => out.push('&'),
        }
    }
    out
}

fn decode_one_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "copy" => Some('©'),
        _ => {
            let dec = entity.strip_prefix('#')?;
            let value = if let Some(hex) = dec.strip_prefix('x').or_else(|| dec.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                dec.parse::<u32>().ok()?
            };
            char::from_u32(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(children: &[Node]) -> String {
        children
            .iter()
            .map(|n| match n {
                Node::Text(t) => t.clone(),
                Node::Element { children, .. } => text(children),
            })
            .collect()
    }

    #[test]
    fn plain_text_round_trips() {
        let nodes = parse("hello world");
        assert_eq!(nodes, vec![Node::Text("hello world".to_owned())]);
    }

    #[test]
    fn nested_elements_build_a_tree() {
        let nodes = parse("<p>Hello <strong>bold</strong> world</p>");
        assert_eq!(nodes.len(), 1);
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Hello bold world");
        assert!(matches!(&children[1], Node::Element { tag, .. } if tag == "strong"));
    }

    #[test]
    fn script_content_with_a_stray_angle_bracket_does_not_swallow_later_siblings() {
        // Regression: `<script>`/`<style>` content used to be tokenized as
        // ordinary markup, so a `<` that merely *looks* like the start of a
        // tag (e.g. a JS comparison `a<b`) got parsed as a bogus opening
        // tag — which then consumed the *real* `</script>` as part of its
        // own (malformed) closing, leaving `script` unclosed and nesting
        // everything that followed (here, the `<p>`) inside it instead of
        // as a sibling.
        let nodes = parse("<script>if(a<b){}</script><p>Visible</p>");
        assert_eq!(
            nodes.len(),
            2,
            "the <p> must be a sibling of <script>, not swallowed into it"
        );
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn style_content_with_a_stray_angle_bracket_does_not_swallow_later_siblings() {
        let nodes = parse("<style>/* a<b */</style><p>Visible</p>");
        assert_eq!(nodes.len(), 2);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "style"));
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn void_elements_have_no_children_and_need_no_close() {
        let nodes = parse("a<br>b<hr/>c");
        assert_eq!(nodes.len(), 5);
        assert!(
            matches!(&nodes[1], Node::Element { tag, children } if tag == "br" && children.is_empty())
        );
        assert!(
            matches!(&nodes[3], Node::Element { tag, children } if tag == "hr" && children.is_empty())
        );
    }

    #[test]
    fn unclosed_tags_are_auto_closed_at_eof() {
        let nodes = parse("<div><p>oops");
        assert_eq!(nodes.len(), 1);
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected div")
        };
        assert_eq!(tag, "div");
        assert!(matches!(&children[0], Node::Element { tag, .. } if tag == "p"));
    }

    #[test]
    fn stray_closing_tag_is_ignored() {
        let nodes = parse("hello</p>world");
        assert_eq!(text(&nodes), "helloworld");
    }

    #[test]
    fn empty_closing_tag_does_not_panic() {
        // Regression: `</>` has an empty tag name, which used to collide
        // with the implicit root frame's own empty-string sentinel and pop
        // it, panicking on the next `stack.last_mut()`.
        assert_eq!(text(&parse("hello</>world")), "helloworld");
        assert_eq!(text(&parse("<div></></div>")), "");
        assert_eq!(text(&parse("</>")), "");
    }

    #[test]
    fn entities_are_decoded() {
        let nodes = parse("Fish &amp; Chips &mdash; &pound;5 &#65;&#x42;");
        // `&pound;` is not in the supported set, so it (and its `&`) survives
        // literally rather than being dropped.
        assert_eq!(text(&nodes), "Fish & Chips — &pound;5 AB");
    }

    #[test]
    fn long_run_of_unterminated_ampersands_is_linear_not_quadratic() {
        // Regression: the semicolon search used to scan the *entire*
        // remainder of the string per `&` before checking how far away it
        // was, making a long run of unterminated `&` (no nearby `;`) O(n^2).
        // 200k chars comfortably reproduced multi-second blowups before the
        // fix; this should now complete near-instantly.
        let html = "&".repeat(200_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(
            text(&nodes),
            html,
            "unterminated `&` passes through unchanged"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "decode_entities took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_unterminated_open_tags_is_linear_not_quadratic() {
        // Regression: `parse_open_tag`'s search for the closing `>` (and the
        // tag-name search before it) scanned the entire remainder of the
        // string, and on failure returned `None` *without consuming any
        // input* — so the outer loop retried at the very next byte and paid
        // that same unbounded scan again, for every `<` in a long run of
        // unterminated fragments (e.g. `"<a".repeat(n)`, with no `>`
        // anywhere) — O(n^2) overall.
        let html = "<a".repeat(100_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(
            text(&nodes),
            html,
            "unterminated `<a` fragments pass through unchanged as literal text"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse_open_tag took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_unmatched_closing_tags_is_linear_not_quadratic() {
        // Regression: the closing-tag matcher scanned the *entire* open-tag
        // stack (`stack.iter().rposition(...)`) looking for a matching
        // opener, on every `</tag>` encountered. For a deep run of opens
        // followed by a long run of closes that never match anything on the
        // stack (e.g. `"<a>".repeat(n) + "</x>".repeat(n)` — "x" never
        // appears, so no close ever pops the stack), each of the n closes
        // pays the full O(n) scan for O(n^2) overall.
        let html = format!("{}{}", "<a>".repeat(50_000), "</x>".repeat(50_000));
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        // None of the "</x>" closes match anything, so they're all ignored
        // — the tree is just 50,000 nested (empty) <a> elements.
        assert_eq!(nodes.len(), 1);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_unterminated_raw_text_closes_is_linear_not_quadratic() {
        // Regression: `consume_raw_text`'s search for the closing `>` past
        // a candidate `</script`/`</style` prefix scanned the *entire*
        // remaining suffix on every failed candidate. A `<script>` body
        // containing many `"</script "` fragments with no `>` anywhere
        // (each one looks like it could be the real closing tag, right up
        // until the bound where a `>` should be) pays that full scan once
        // per candidate — O(n^2) overall, the same shape of bug
        // `MAX_TAG_SCAN`/`MAX_CLOSE_SCAN` were fixed for.
        let html = format!("<script>{}", "</script ".repeat(50_000));
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn closing_tag_deeper_than_the_scan_window_is_ignored_not_matched() {
        // Documents the accepted precision loss from bounding the
        // closing-tag scan (MAX_CLOSE_SCAN): a close whose matching opener
        // sits further back than the window no longer auto-closes the many
        // intervening tags — it's treated the same as a close with no
        // matching opener at all (ignored), so trailing content ends up
        // nested inside the still-open tags instead of becoming a sibling
        // at the root. Real templates essentially never nest hundreds of
        // levels deep, let alone rely on this specific deep-ancestor-close
        // pattern, so this only affects pathological input.
        let html = format!("<outer>{}</outer>tail", "<a>".repeat(600));
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            1,
            "the out-of-window close must not split off a sibling at the root"
        );
        assert_eq!(text(&nodes), "tail");
    }

    #[test]
    fn valid_tag_with_long_attribute_list_still_parses() {
        // Regression: a real Tailwind-style `class` attribute easily runs
        // past a couple hundred bytes on its own — MAX_TAG_SCAN must stay
        // generous enough that a genuine (if verbose) opening tag doesn't
        // get rejected and leaked into the PDF as literal `<div ...>` text.
        let long_class = "flex items-center justify-between px-4 py-2 bg-white \
            dark:bg-gray-900 border border-gray-200 rounded-lg shadow-sm \
            hover:shadow-md transition-shadow duration-200 text-sm font-medium \
            text-gray-700 dark:text-gray-300 focus:outline-none focus:ring-2 \
            focus:ring-offset-2 data-controller=\"dropdown\" aria-label=\"menu\"";
        assert!(
            long_class.len() > 256,
            "test fixture must exceed the old, too-tight window"
        );
        let html = format!("<div class=\"{long_class}\">Hello</div>");
        let nodes = parse(&html);
        assert_eq!(
            text(&nodes),
            "Hello",
            "a long but well-formed opening tag must parse, not leak as literal text"
        );
    }

    #[test]
    fn attributes_are_ignored_but_dont_break_parsing() {
        let nodes = parse(r#"<p class="total" data-x='1'>Total</p>"#);
        assert_eq!(text(&nodes), "Total");
    }

    #[test]
    fn deeply_nested_input_does_not_overflow_the_stack() {
        let mut html = String::new();
        for _ in 0..50_000 {
            html.push_str("<div>");
        }
        html.push('x');
        for _ in 0..50_000 {
            html.push_str("</div>");
        }
        // `parse` is iterative, so it must not overflow. Deliberately does
        // *not* walk the resulting tree with a recursive helper like `text`
        // here — that would just move the overflow risk into the test
        // itself. Dropping `nodes` at the end of this test exercises
        // `Node`'s custom iterative `Drop` the same way.
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn malformed_lone_angle_bracket_is_literal_text() {
        let nodes = parse("a < b");
        assert_eq!(text(&nodes), "a < b");
    }
}
