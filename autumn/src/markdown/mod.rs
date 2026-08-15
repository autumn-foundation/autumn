//! First-class Markdown rendering with frontmatter parsing and SSG integration.
//!
//! Enable with the Cargo feature `markdown`.
//!
//! ## Trusted content vs. user content
//!
//! This module has two entry points, and picking the wrong one is a security
//! bug:
//!
//! | Source of the Markdown | Use |
//! |---|---|
//! | Files you authored and committed (docs, marketing pages, a `content/` tree) | [`render`](crate::markdown::render) / [`MarkdownRegistry`](crate::markdown::MarkdownRegistry) |
//! | Anything a request body carried in (posts, comments, wiki bodies, bios) | `render_user_content` |
//!
//! [`render`](crate::markdown::render) is built for *trusted, build-time* content: it injects heading
//! anchors and applies no allowlist. `render_user_content` disables raw-HTML
//! passthrough, restricts link schemes, and runs the output through an
//! allowlist sanitizer — see [`render_user_content_html`](crate::markdown::render_user_content_html) and
//! [`docs/guide/rich-text.md`] for the exact guarantee.
//!
//! [`docs/guide/rich-text.md`]: https://github.com/autumn-foundation/autumn/blob/main/docs/guide/rich-text.md
//!
//! ## Quick start
//!
//! ### 1. Embed Markdown files at compile time
//!
//! ```rust,ignore
//! use std::sync::OnceLock;
//! use autumn_web::markdown::{MarkdownRegistry, MarkdownSource, RenderOptions, render};
//!
//! static DOCS: OnceLock<MarkdownRegistry> = OnceLock::new();
//!
//! fn docs() -> &'static MarkdownRegistry {
//!     DOCS.get_or_init(|| {
//!         MarkdownRegistry::from_embedded(&[
//!             MarkdownSource { slug: "intro", content: include_str!("../content/intro.md") },
//!             MarkdownSource { slug: "api",   content: include_str!("../content/api.md") },
//!         ]).expect("embedded docs are valid")
//!     })
//! }
//! ```
//!
//! ### 2. Render a page dynamically
//!
//! ```rust,ignore
//! #[get("/docs/{slug}")]
//! async fn show_doc(Path(slug): Path<String>) -> AutumnResult<Markup> {
//!     let page = docs().get(&slug)
//!         .ok_or_else(|| AutumnError::not_found())?;
//!     let out = render(&page.body, RenderOptions::default());
//!     Ok(layout(&page.frontmatter.title, html! {
//!         (PreEscaped(&out.html))
//!     }))
//! }
//! ```
//!
//! Heading anchors are unique per document: a heading repeated within a page
//! keeps the plain slug on its first occurrence (`#example`) and later ones are
//! suffixed (`#example-1`), so every entry in [`RenderedMarkdown::toc`] links to
//! its own heading.
//!
//! ### 3. Wire up static pre-rendering
//!
//! ```rust,ignore
//! async fn doc_params(_router: axum::Router) -> Vec<StaticParams> {
//!     docs().static_params()
//! }
//!
//! #[static_get("/docs/{slug}", params = doc_params)]
//! async fn show_doc_static(Path(slug): Path<String>) -> AutumnResult<Markup> {
//!     // same as the dynamic handler above
//! }
//!
//! // In main():
//! autumn_web::app()
//!     .routes(routes![show_doc_static, ...])
//!     .static_routes(static_routes![show_doc_static])
//!     .run()
//!     .await;
//! ```
//!
//! ## Frontmatter format
//!
//! Each `.md` file must begin with a TOML block enclosed in `+++` delimiters:
//!
//! ```text
//! +++
//! title = "Getting Started"
//! description = "Set up your app in minutes."
//! order = 1
//! +++
//!
//! # Getting Started
//!
//! ...
//! ```
//!
//! The `title` field is required; `description` and `order` are optional
//! (defaulting to `""` and `0` respectively).

mod registry;
mod renderer;
mod types;
mod user_content;

pub use registry::MarkdownRegistry;
pub use renderer::{heading_id, render};
pub use types::{
    MarkdownError, MarkdownFrontmatter, MarkdownPage, MarkdownSource, RenderOptions,
    RenderedMarkdown, TocItem,
};
#[cfg(feature = "maud")]
pub use user_content::render_user_content;
pub use user_content::{
    RICH_TEXT_ALLOWED_TAGS, RICH_TEXT_ALLOWED_URL_SCHEMES, render_user_content_html,
    sanitize_user_html,
};
