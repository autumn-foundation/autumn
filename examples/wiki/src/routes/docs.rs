//! Markdown documentation routes.
//!
//! Demonstrates Autumn's `markdown` feature: embedded `.md` files rendered
//! dynamically during development and pre-rendered via `#[static_get]` for
//! production.
//!
//! The entire docs section — registry setup, a dynamic index, and a
//! statically pre-renderable detail page — fits in under 30 lines of app
//! glue (excluding layout markup).

use std::sync::OnceLock;

use autumn_web::extract::Path;
use autumn_web::markdown::{MarkdownRegistry, MarkdownSource, RenderOptions, render};
use autumn_web::prelude::*;
use autumn_web::static_gen::StaticParams;

// ---------------------------------------------------------------------------
// Registry (initialised once at startup from embedded .md files)
// ---------------------------------------------------------------------------

static REGISTRY: OnceLock<MarkdownRegistry> = OnceLock::new();

fn docs() -> &'static MarkdownRegistry {
    REGISTRY.get_or_init(|| {
        MarkdownRegistry::from_embedded(&[
            MarkdownSource {
                slug: "getting-started",
                content: include_str!("../../content/getting-started.md"),
            },
            MarkdownSource {
                slug: "configuration",
                content: include_str!("../../content/configuration.md"),
            },
        ])
        .expect("embedded docs content is valid")
    })
}

// ---------------------------------------------------------------------------
// Params function for SSG pre-rendering
// ---------------------------------------------------------------------------

/// Called by `autumn build` to enumerate every slug that must be pre-rendered.
pub async fn doc_params(_router: autumn_web::reexports::axum::Router) -> Vec<StaticParams> {
    docs().static_params()
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Statically pre-renderable doc page.  Annotated with `#[static_get]` so
/// `autumn build` writes `dist/docs/{slug}/index.html` for every page in the
/// registry.  The same handler also serves live requests during development.
#[static_get("/docs/{slug}", params = doc_params)]
pub async fn show(Path(slug): Path<String>) -> AutumnResult<Markup> {
    let page = docs()
        .get(&slug)
        .ok_or_else(|| AutumnError::not_found_msg(format!("Doc '{slug}' not found")))?;

    let rendered = render(&page.body, RenderOptions::default());

    Ok(crate::routes::pages::layout(
        &page.frontmatter.title,
        html! {
            nav class="mb-4 text-sm" {
                a href="/docs" class="text-emerald-600 hover:underline" { "← Docs" }
            }
            article class="bg-white rounded shadow p-6" {
                h1 class="text-3xl font-bold mb-2" { (page.frontmatter.title) }
                @if !page.frontmatter.description.is_empty() {
                    p class="text-gray-500 mb-6 text-sm" { (page.frontmatter.description) }
                }
                @if !rendered.toc.is_empty() {
                    nav class="mb-6 p-4 bg-gray-50 rounded border text-sm" {
                        p class="font-semibold mb-2" { "Contents" }
                        ul class="space-y-1" {
                            @for item in &rendered.toc {
                                li style=(format!("margin-left: {}rem", (item.level - 1) as f32 * 1.0)) {
                                    a href=(format!("#{}", item.id))
                                      class="text-emerald-600 hover:underline" {
                                        (item.text)
                                    }
                                }
                            }
                        }
                    }
                }
                div class="prose max-w-none" {
                    (PreEscaped(&rendered.html))
                }
            }
        },
    ))
}

/// Documentation index — lists all pages sorted by `order`.
#[get("/docs")]
pub async fn index() -> Markup {
    let pages = docs().all_sorted();
    crate::routes::pages::layout(
        "Documentation",
        html! {
            h1 class="text-2xl font-bold mb-6" { "Documentation" }
            ul class="space-y-3" {
                @for page in &pages {
                    li class="p-4 bg-white rounded shadow" {
                        a href=(format!("/docs/{}", page.slug))
                          class="text-emerald-700 font-medium hover:underline text-lg" {
                            (page.frontmatter.title)
                        }
                        @if !page.frontmatter.description.is_empty() {
                            p class="text-sm text-gray-500 mt-1" {
                                (page.frontmatter.description)
                            }
                        }
                    }
                }
                @if pages.is_empty() {
                    li class="text-gray-400 text-center py-8" { "No docs pages found." }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_loads_two_embedded_pages() {
        let registry = docs();
        assert_eq!(registry.len(), 2);
        assert!(registry.get("getting-started").is_some());
        assert!(registry.get("configuration").is_some());
    }

    #[test]
    fn registry_sorted_by_order() {
        let pages = docs().all_sorted();
        assert_eq!(pages[0].slug, "getting-started");
        assert_eq!(pages[1].slug, "configuration");
    }

    #[test]
    fn static_params_has_slug_keys() {
        let params = docs().static_params();
        assert_eq!(params.len(), 2);
        let slugs: Vec<_> = params.iter().map(|p| p["slug"].as_str()).collect();
        assert!(slugs.contains(&"getting-started"));
        assert!(slugs.contains(&"configuration"));
    }

    #[tokio::test]
    async fn show_doc_renders_html() {
        let result = show(Path("getting-started".to_owned())).await;
        let markup = result.expect("route should succeed");
        let html = markup.into_string();
        assert!(html.contains("Getting Started"));
    }

    #[tokio::test]
    async fn show_doc_returns_not_found_for_unknown_slug() {
        let result = show(Path("nonexistent".to_owned())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn index_renders_both_pages() {
        let markup = index().await;
        let html = markup.into_string();
        assert!(html.contains("Getting Started"));
        assert!(html.contains("Configuration"));
    }

    #[tokio::test]
    async fn test_doc_params_returns_non_empty_vec() {
        let dummy_router = autumn_web::reexports::axum::Router::new();
        let params = doc_params(dummy_router).await;
        assert!(
            !params.is_empty(),
            "doc_params should return a non-empty list of static params"
        );
        assert_eq!(params.len(), docs().len());
    }
}
