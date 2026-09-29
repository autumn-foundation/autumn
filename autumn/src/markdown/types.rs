//! Public types for the Markdown module.

use std::path::PathBuf;

/// A named Markdown document to embed at compile time via `include_str!`.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownSource {
    /// URL-safe slug used as a route parameter (e.g. `"getting-started"`).
    pub slug: &'static str,
    /// Full Markdown content including the `+++` frontmatter block.
    pub content: &'static str,
}

/// Parsed frontmatter from a Markdown document.
///
/// Uses TOML between `+++` delimiters at the start of the file.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MarkdownFrontmatter {
    /// Display title of the page.
    pub title: String,
    /// Short description used in listings and meta tags.
    #[serde(default)]
    pub description: String,
    /// Sort order for navigation listings (lower numbers appear first).
    #[serde(default)]
    pub order: u32,
}

/// A parsed Markdown page ready for rendering.
#[derive(Debug, Clone)]
pub struct MarkdownPage {
    /// URL-safe identifier, e.g. `"getting-started"`.
    pub slug: String,
    /// Parsed frontmatter metadata.
    pub frontmatter: MarkdownFrontmatter,
    /// Raw Markdown body with frontmatter stripped.
    pub body: String,
}

/// A single entry in the rendered table of contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocItem {
    /// Heading level 1–6.
    pub level: u8,
    /// Stable anchor ID derived from the heading text.
    pub id: String,
    /// Plain-text heading content.
    pub text: String,
}

/// Options controlling the Markdown renderer behaviour.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Render GFM tables (default: `true`).
    pub enable_tables: bool,
    /// Render GFM strikethrough (default: `true`).
    pub enable_strikethrough: bool,
    /// Render GFM task lists (default: `true`).
    pub enable_tasklists: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            enable_tables: true,
            enable_strikethrough: true,
            enable_tasklists: true,
        }
    }
}

/// The output from rendering a [`MarkdownPage`].
#[derive(Debug, Clone)]
pub struct RenderedMarkdown {
    /// HTML produced from the Markdown body.
    ///
    /// Raw HTML in the source is escaped rather than emitted, but this is
    /// **not** sanitized output — see
    /// [`render`](crate::markdown::render), which is for trusted build-time
    /// content only. For user-submitted Markdown use
    /// [`render_user_content_html`](crate::markdown::render_user_content_html).
    pub html: String,
    /// Table of contents extracted from headings, in document order.
    pub toc: Vec<TocItem>,
}

/// Errors from the Markdown module.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MarkdownError {
    /// The document is missing the required `+++` frontmatter delimiters.
    #[error("frontmatter delimiters `+++` not found in '{slug}'")]
    FrontmatterMissing {
        /// The slug that failed to parse.
        slug: String,
    },

    /// The frontmatter TOML could not be parsed.
    #[error("frontmatter TOML parse error in '{slug}': {source}")]
    FrontmatterInvalid {
        /// The slug that failed.
        slug: String,
        /// Underlying TOML error.
        #[source]
        source: toml::de::Error,
    },

    /// A filesystem I/O error occurred while loading from a directory.
    #[error("I/O error reading '{path}': {source}")]
    Io {
        /// The path that caused the error.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A file name could not be converted to a valid slug.
    #[error("file name cannot be converted to a slug: '{name}'")]
    InvalidFileName {
        /// The problematic file name.
        name: String,
    },

    /// Two pages share the same slug in the registry.
    #[error("duplicate slug '{slug}' in registry")]
    DuplicateSlug {
        /// The slug that appeared more than once.
        slug: String,
    },
}
