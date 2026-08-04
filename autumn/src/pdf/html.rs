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
                // any exists on the stack. A stray/mismatched close tag with
                // no matching opener is ignored rather than corrupting the
                // tree.
                if let Some(depth) = stack.iter().rposition(|(tag, _)| *tag == name) {
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
fn parse_open_tag(s: &str) -> Option<(String, bool, usize)> {
    debug_assert!(s.starts_with('<'));
    let rest = &s[1..];
    let first = rest.chars().next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let tag = rest[..name_end].to_ascii_lowercase();

    let gt = rest.find('>')?;
    let self_closing = rest[..gt].trim_end().ends_with('/');
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
        let Some(semi) = rest.find(';').filter(|&p| p <= 10) else {
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
    fn entities_are_decoded() {
        let nodes = parse("Fish &amp; Chips &mdash; &pound;5 &#65;&#x42;");
        // `&pound;` is not in the supported set, so it (and its `&`) survives
        // literally rather than being dropped.
        assert_eq!(text(&nodes), "Fish & Chips — &pound;5 AB");
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
