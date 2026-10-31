//! A document path that costs nothing until a diagnostic needs it.
//!
//! A [`Path`] is a parent pointer and one segment. Both are borrowed, so a
//! child path copies no bytes, whatever the length of a name. The text form is
//! built by [`Display`], and only when a [`Diagnostic`](super::error::Diagnostic)
//! is made. See issue #3445.

// autumn-panic-gate: request-path module — it runs on a document written by a
// language model, so every panic in it is reachable by hostile input. Production
// code must be panic-free. See CONTRIBUTING.md "Request-path panic gate".
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt::{self, Display, Write as _};

/// One step of a [`Path`].
#[derive(Debug, Clone, Copy)]
enum Segment<'p> {
    /// The start of a path. Written as is.
    Root(&'p str),
    /// A fixed field name: `.key`.
    Key(&'static str),
    /// A name from the document: `.name`.
    Name(&'p str),
    /// A list position: `[i]`.
    Index(usize),
    /// A component call: `<name>`.
    Component(&'p str),
}

/// Where a node or an expression sits in the submitted document.
///
/// `Copy`, and as small as two pointers. Render it with `to_string` only to
/// make a diagnostic.
#[derive(Debug, Clone, Copy)]
pub struct Path<'p> {
    parent: Option<&'p Self>,
    segment: Segment<'p>,
}

impl<'p> Path<'p> {
    /// A path that starts at `root`.
    pub const fn root(root: &'p str) -> Self {
        Self {
            parent: None,
            segment: Segment::Root(root),
        }
    }

    const fn push(&'p self, segment: Segment<'p>) -> Self {
        Self {
            parent: Some(self),
            segment,
        }
    }

    /// This path, then `.key`.
    #[must_use]
    pub const fn key(&'p self, key: &'static str) -> Self {
        self.push(Segment::Key(key))
    }

    /// This path, then `.name`, where `name` comes from the document.
    #[must_use]
    pub const fn name(&'p self, name: &'p str) -> Self {
        self.push(Segment::Name(name))
    }

    /// This path, then `[index]`.
    #[must_use]
    pub const fn index(&'p self, index: usize) -> Self {
        self.push(Segment::Index(index))
    }

    /// This path, then `<name>`.
    #[must_use]
    pub const fn component(&'p self, name: &'p str) -> Self {
        self.push(Segment::Component(name))
    }
}

/// A text sink that keeps at most `max` bytes and then refuses more.
struct Clipped {
    text: String,
    max: usize,
}

impl fmt::Write for Clipped {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        let room = self.max.saturating_sub(self.text.len());
        let mut end = piece.len().min(room);
        while !piece.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        self.text.push_str(piece.get(..end).unwrap_or_default());
        if end < piece.len() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

impl Path<'_> {
    /// The text form, cut to `max` bytes at a character boundary.
    ///
    /// For a path that is kept after the call. The copy is bounded by `max`,
    /// whatever the length of a name.
    #[must_use]
    pub fn render_clipped(self, max: usize) -> String {
        let mut sink = Clipped {
            text: String::new(),
            max,
        };
        // An `Err` here only means the text was cut.
        let _ = write!(sink, "{self}");
        sink.text
    }
}

impl Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Parent first, without recursion: the chain is as deep as the
        // document, and the document can be deep.
        let mut chain = Vec::new();
        let mut current = Some(self);
        while let Some(path) = current {
            chain.push(path.segment);
            current = path.parent;
        }
        for segment in chain.into_iter().rev() {
            match segment {
                Segment::Root(text) => f.write_str(text)?,
                Segment::Key(text) | Segment::Name(text) => {
                    f.write_char('.')?;
                    f.write_str(text)?;
                }
                Segment::Index(index) => write!(f, "[{index}]")?,
                Segment::Component(text) => {
                    f.write_char('<')?;
                    f.write_str(text)?;
                    f.write_char('>')?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_renders_like_the_old_format_strings() {
        let root = Path::root("view");
        let text = root
            .key("children")
            .index(2)
            .key("props")
            .name("href")
            .to_string();
        assert_eq!(text, "view.children[2].props.href");
        assert_eq!(
            root.component("Card").key("body").to_string(),
            "view<Card>.body"
        );
        let meta = Path::root("route.meta");
        assert_eq!(meta.name("og:title").to_string(), "route.meta.og:title");
    }

    #[test]
    fn a_clipped_path_keeps_at_most_max_bytes() {
        let name = "é".repeat(1000);
        let root = Path::root("view");
        let text = root.name(&name).render_clipped(11);
        assert!(text.len() <= 11);
        assert!(text.starts_with("view.é"));
        assert_eq!(Path::root("view").render_clipped(100), "view");
    }

    #[test]
    fn a_child_path_leaves_its_parent_unchanged() {
        let root = Path::root("view");
        let _left = root.key("left");
        assert_eq!(root.to_string(), "view");
    }

    #[test]
    fn a_path_holds_a_name_by_reference() {
        let name = "a".repeat(1 << 20);
        let root = Path::root("components");
        let path = root.name(&name);
        assert!(std::mem::size_of_val(&path) <= 48);
        assert_eq!(path.to_string().len(), "components.".len() + name.len());
    }
}
