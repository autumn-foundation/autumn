//! [`MarkdownRegistry`]: a keyed collection of parsed Markdown pages.

use std::path::Path;

use crate::markdown::types::{MarkdownError, MarkdownFrontmatter, MarkdownPage, MarkdownSource};
use crate::static_gen::StaticParams;

/// A registry of Markdown pages keyed by slug.
///
/// Build from embedded sources via [`MarkdownRegistry::from_embedded`] or
/// from a directory of `.md` files via [`MarkdownRegistry::from_dir`].
pub struct MarkdownRegistry {
    pages: indexmap::IndexMap<String, MarkdownPage>,
}

impl MarkdownRegistry {
    /// Build a registry from a slice of embedded [`MarkdownSource`] values.
    ///
    /// Each source must contain a valid `+++` TOML frontmatter block followed
    /// by a Markdown body.
    ///
    /// # Errors
    ///
    /// Returns [`MarkdownError::FrontmatterMissing`] or
    /// [`MarkdownError::FrontmatterInvalid`] if any source cannot be parsed,
    /// or [`MarkdownError::DuplicateSlug`] if two sources share a slug.
    pub fn from_embedded(sources: &[MarkdownSource]) -> Result<Self, MarkdownError> {
        let mut pages = indexmap::IndexMap::new();
        for source in sources {
            let page = parse_page(source.slug.to_owned(), source.content)?;
            if pages.insert(page.slug.clone(), page).is_some() {
                return Err(MarkdownError::DuplicateSlug {
                    slug: source.slug.to_owned(),
                });
            }
        }
        pages.sort_by(|_, a, _, b| {
            (a.frontmatter.order, a.slug.as_str()).cmp(&(b.frontmatter.order, b.slug.as_str()))
        });
        Ok(Self { pages })
    }

    /// Load all `.md` files from `dir` and parse them into pages.
    ///
    /// The slug for each page is derived from the file stem
    /// (e.g. `getting-started.md` → `"getting-started"`).
    /// Non-`.md` entries and non-file entries (subdirectories, symlinks) are
    /// silently ignored.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be read, any file cannot be
    /// opened, frontmatter is missing or invalid, or two files share a slug.
    pub fn from_dir(dir: &Path) -> Result<Self, MarkdownError> {
        let mut pages = indexmap::IndexMap::new();

        let read_dir = std::fs::read_dir(dir).map_err(|source| MarkdownError::Io {
            path: dir.to_owned(),
            source,
        })?;

        for entry in read_dir {
            let entry = entry.map_err(|source| MarkdownError::Io {
                path: dir.to_owned(),
                source,
            })?;
            // Skip non-regular files (symlinks, subdirectories, …).
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let slug = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| MarkdownError::InvalidFileName {
                    name: path.display().to_string(),
                })?
                .to_owned();
            let content = std::fs::read_to_string(&path).map_err(|source| MarkdownError::Io {
                path: path.clone(),
                source,
            })?;
            let page = parse_page(slug.clone(), &content)?;
            if pages.insert(slug.clone(), page).is_some() {
                return Err(MarkdownError::DuplicateSlug { slug });
            }
        }
        pages.sort_by(|_, a, _, b| {
            (a.frontmatter.order, a.slug.as_str()).cmp(&(b.frontmatter.order, b.slug.as_str()))
        });
        Ok(Self { pages })
    }

    /// Retrieve a page by slug, returning `None` if no such page exists.
    #[must_use]
    pub fn get(&self, slug: &str) -> Option<&MarkdownPage> {
        self.pages.get(slug)
    }

    /// All pages in sort order: `frontmatter.order` ascending, then slug as
    /// a tiebreaker.  The registry is pre-sorted at construction so this is a
    /// simple iterator collect with no runtime sorting overhead.
    #[must_use]
    pub fn all_sorted(&self) -> Vec<&MarkdownPage> {
        self.pages.values().collect()
    }

    /// Derive one [`StaticParams`] per page for use with `#[static_get]`
    /// parameterized routes.
    ///
    /// Pages are returned in sorted order (same as [`MarkdownRegistry::all_sorted`]).
    /// Each entry maps `"slug"` to the page's slug.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn doc_params(_router: axum::Router) -> Vec<StaticParams> {
    ///     docs_registry().static_params()
    /// }
    ///
    /// #[static_get("/docs/{slug}", params = doc_params)]
    /// async fn show_doc(Path(slug): Path<String>) -> AutumnResult<Markup> { ... }
    /// ```
    #[must_use]
    pub fn static_params(&self) -> Vec<StaticParams> {
        self.static_params_for("slug")
    }

    /// Derive one [`StaticParams`] per page, binding each page's slug to the
    /// route parameter named `param`.
    ///
    /// Use this when the `#[static_get]` route names its parameter something
    /// other than `slug` — e.g. `#[static_get("/docs/{page}", …)]` needs
    /// `static_params_for("page")`, because a `StaticParams` entry keyed
    /// `"slug"` would leave `{page}` unsubstituted.
    ///
    /// Pages are returned in sorted order (same as [`MarkdownRegistry::all_sorted`]).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn doc_params(_router: axum::Router) -> Vec<StaticParams> {
    ///     docs_registry().static_params_for("page")
    /// }
    ///
    /// #[static_get("/docs/{page}", params = doc_params)]
    /// async fn show_doc(Path(page): Path<String>) -> AutumnResult<Markup> { ... }
    /// ```
    #[must_use]
    pub fn static_params_for(&self, param: &str) -> Vec<StaticParams> {
        self.pages
            .values()
            .map(|p| {
                let mut params = StaticParams::new();
                params.insert(param.to_owned(), p.slug.clone());
                params
            })
            .collect()
    }

    /// Number of pages in the registry.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// Returns `true` if the registry contains no pages.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// Parse a Markdown document (with `+++` TOML frontmatter) into a [`MarkdownPage`].
fn parse_page(slug: String, content: &str) -> Result<MarkdownPage, MarkdownError> {
    let (frontmatter, body) = split_frontmatter(&slug, content)?;
    Ok(MarkdownPage {
        slug,
        frontmatter,
        body: body.trim_start().to_owned(),
    })
}

/// Split a document into its TOML frontmatter and raw Markdown body.
///
/// Expects the content to start with `+++\n`, followed by TOML, followed
/// by `\n+++` on its own line. The body is everything after the closing
/// delimiter.
fn split_frontmatter<'a>(
    slug: &str,
    content: &'a str,
) -> Result<(MarkdownFrontmatter, &'a str), MarkdownError> {
    let content = content.trim_start();

    let after_open = content
        .strip_prefix("+++\n")
        .or_else(|| content.strip_prefix("+++\r\n"))
        .ok_or_else(|| MarkdownError::FrontmatterMissing {
            slug: slug.to_owned(),
        })?;

    let close_pos = after_open
        .find("\n+++")
        .ok_or_else(|| MarkdownError::FrontmatterMissing {
            slug: slug.to_owned(),
        })?;

    let toml_str = &after_open[..close_pos];
    let after_close = &after_open[close_pos + 4..]; // skip "\n+++"

    // Skip the optional newline(s) immediately after the closing +++
    let body = after_close
        .strip_prefix("\r\n")
        .or_else(|| after_close.strip_prefix('\n'))
        .unwrap_or(after_close);

    // Normalize CRLF → LF before TOML parsing so that embedded files checked
    // out on Windows (where git autocrlf converts line endings) parse correctly.
    // The toml crate rejects bare \r that results when find("\n+++") lands on
    // the \n of a \r\n sequence, leaving a trailing \r in the TOML slice.
    let toml_normalized;
    let toml_for_parse: &str = if toml_str.contains('\r') {
        toml_normalized = toml_str.replace('\r', "");
        &toml_normalized
    } else {
        toml_str
    };

    let frontmatter: MarkdownFrontmatter =
        toml::from_str(toml_for_parse).map_err(|source| MarkdownError::FrontmatterInvalid {
            slug: slug.to_owned(),
            source,
        })?;

    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GETTING_STARTED: MarkdownSource = MarkdownSource {
        slug: "getting-started",
        content: "+++\ntitle = \"Getting Started\"\ndescription = \"How to get started\"\norder = 1\n+++\n\n# Getting Started\n\nWelcome!\n",
    };

    const API_REFERENCE: MarkdownSource = MarkdownSource {
        slug: "api-reference",
        content: "+++\ntitle = \"API Reference\"\ndescription = \"API docs\"\norder = 2\n+++\n\n# API Reference\n\nRoutes and types.\n",
    };

    // ---- Unit tests: parsing ----

    #[test]
    fn parses_frontmatter_correctly() {
        let page = parse_page(
            "test".to_owned(),
            "+++\ntitle = \"My Title\"\ndescription = \"A desc\"\norder = 3\n+++\n\n# Body\n",
        )
        .unwrap();
        assert_eq!(page.frontmatter.title, "My Title");
        assert_eq!(page.frontmatter.description, "A desc");
        assert_eq!(page.frontmatter.order, 3);
        assert_eq!(page.body, "# Body\n");
    }

    #[test]
    fn description_defaults_to_empty_string() {
        let page = parse_page(
            "no-desc".to_owned(),
            "+++\ntitle = \"No Desc\"\norder = 5\n+++\n\n# Body\n",
        )
        .unwrap();
        assert_eq!(page.frontmatter.description, "");
    }

    #[test]
    fn order_defaults_to_zero() {
        let page = parse_page(
            "no-order".to_owned(),
            "+++\ntitle = \"No Order\"\n+++\n\n# Body\n",
        )
        .unwrap();
        assert_eq!(page.frontmatter.order, 0);
    }

    #[test]
    fn parses_crlf_frontmatter() {
        // Simulate a file checked out on Windows with git autocrlf=true.
        let page = parse_page(
            "crlf".to_owned(),
            "+++\r\ntitle = \"CRLF Page\"\r\norder = 7\r\n+++\r\n\r\n# Body\r\n",
        )
        .unwrap();
        assert_eq!(page.frontmatter.title, "CRLF Page");
        assert_eq!(page.frontmatter.order, 7);
    }

    #[test]
    fn missing_opening_delimiter_returns_error() {
        let result = parse_page("bad".to_owned(), "# No frontmatter\n");
        assert!(matches!(
            result,
            Err(MarkdownError::FrontmatterMissing { .. })
        ));
    }

    #[test]
    fn missing_closing_delimiter_returns_error() {
        let result = parse_page("bad".to_owned(), "+++\ntitle = \"Test\"\n# No closing\n");
        assert!(matches!(
            result,
            Err(MarkdownError::FrontmatterMissing { .. })
        ));
    }

    #[test]
    fn invalid_toml_returns_error() {
        let result = parse_page(
            "bad".to_owned(),
            "+++\nnot valid toml = [[[\n+++\n\n# Body\n",
        );
        assert!(matches!(
            result,
            Err(MarkdownError::FrontmatterInvalid { .. })
        ));
    }

    #[test]
    fn body_leading_whitespace_stripped() {
        let page =
            parse_page("test".to_owned(), "+++\ntitle = \"T\"\n+++\n\n\n\n# Body\n").unwrap();
        assert!(page.body.starts_with("# Body"));
    }

    // ---- Unit tests: registry operations ----

    #[test]
    fn builds_registry_from_embedded() {
        let registry = MarkdownRegistry::from_embedded(&[GETTING_STARTED, API_REFERENCE]).unwrap();
        assert_eq!(registry.len(), 2);
        assert!(registry.get("getting-started").is_some());
        assert!(registry.get("api-reference").is_some());
    }

    #[test]
    fn get_returns_correct_page() {
        let registry = MarkdownRegistry::from_embedded(&[GETTING_STARTED]).unwrap();
        let page = registry.get("getting-started").unwrap();
        assert_eq!(page.frontmatter.title, "Getting Started");
        assert_eq!(page.frontmatter.order, 1);
        assert_eq!(page.slug, "getting-started");
    }

    #[test]
    fn get_unknown_slug_returns_none() {
        let registry = MarkdownRegistry::from_embedded(&[GETTING_STARTED]).unwrap();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn all_sorted_orders_by_order_field() {
        // Sources given in reverse order — sorted output must be by `order`.
        let registry = MarkdownRegistry::from_embedded(&[API_REFERENCE, GETTING_STARTED]).unwrap();
        let pages = registry.all_sorted();
        assert_eq!(pages[0].slug, "getting-started"); // order = 1
        assert_eq!(pages[1].slug, "api-reference"); // order = 2
    }

    #[test]
    fn all_sorted_tiebreaks_by_slug() {
        let a = MarkdownSource {
            slug: "zebra",
            content: "+++\ntitle = \"Zebra\"\norder = 1\n+++\n\n# Z\n",
        };
        let b = MarkdownSource {
            slug: "apple",
            content: "+++\ntitle = \"Apple\"\norder = 1\n+++\n\n# A\n",
        };
        let registry = MarkdownRegistry::from_embedded(&[a, b]).unwrap();
        let pages = registry.all_sorted();
        assert_eq!(pages[0].slug, "apple");
        assert_eq!(pages[1].slug, "zebra");
    }

    #[test]
    fn static_params_returns_one_entry_per_page() {
        let registry = MarkdownRegistry::from_embedded(&[GETTING_STARTED, API_REFERENCE]).unwrap();
        let params = registry.static_params();
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn static_params_uses_slug_key() {
        let registry = MarkdownRegistry::from_embedded(&[GETTING_STARTED]).unwrap();
        let params = registry.static_params();
        assert_eq!(params[0].get("slug").unwrap(), "getting-started");
    }

    #[test]
    fn static_params_sorted_by_order() {
        let registry = MarkdownRegistry::from_embedded(&[API_REFERENCE, GETTING_STARTED]).unwrap();
        let params = registry.static_params();
        assert_eq!(params[0].get("slug").unwrap(), "getting-started");
        assert_eq!(params[1].get("slug").unwrap(), "api-reference");
    }

    #[test]
    fn static_params_for_uses_custom_param_name() {
        // `#[static_get("/docs/{page}")]` binds a param named `page`, not
        // `slug`. Without a keyed helper, SSG for such a route gets params it
        // cannot substitute.
        let registry = MarkdownRegistry::from_embedded(&[GETTING_STARTED, API_REFERENCE]).unwrap();
        let params = registry.static_params_for("page");
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].get("page").unwrap(), "getting-started");
        assert_eq!(params[1].get("page").unwrap(), "api-reference");
        assert!(!params[0].contains_key("slug"));
    }

    #[test]
    fn static_params_delegates_to_static_params_for_slug() {
        let registry = MarkdownRegistry::from_embedded(&[GETTING_STARTED, API_REFERENCE]).unwrap();
        assert_eq!(registry.static_params(), registry.static_params_for("slug"));
    }

    #[test]
    fn empty_registry_is_empty() {
        let registry = MarkdownRegistry::from_embedded(&[]).unwrap();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.all_sorted().is_empty());
        assert!(registry.static_params().is_empty());
    }

    #[test]
    fn missing_frontmatter_propagates_error_from_embedded() {
        let bad = MarkdownSource {
            slug: "bad",
            content: "# No frontmatter\n",
        };
        let result = MarkdownRegistry::from_embedded(&[bad]);
        assert!(matches!(
            result,
            Err(MarkdownError::FrontmatterMissing { .. })
        ));
    }

    #[test]
    fn duplicate_slug_returns_error_from_embedded() {
        let dup = MarkdownSource {
            slug: "getting-started",
            content: "+++\ntitle = \"Dup\"\norder = 99\n+++\n\n# Dup\n",
        };
        let result = MarkdownRegistry::from_embedded(&[GETTING_STARTED, dup]);
        assert!(matches!(result, Err(MarkdownError::DuplicateSlug { .. })));
    }

    #[test]
    fn loads_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("page-one.md"),
            "+++\ntitle = \"Page One\"\norder = 1\n+++\n\n# Page One\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("page-two.md"),
            "+++\ntitle = \"Page Two\"\norder = 2\n+++\n\n# Page Two\n",
        )
        .unwrap();
        // Non-md file must be ignored
        std::fs::write(dir.path().join("README.txt"), "ignore me").unwrap();

        let registry = MarkdownRegistry::from_dir(dir.path()).unwrap();
        assert_eq!(registry.len(), 2);
        assert!(registry.get("page-one").is_some());
        assert!(registry.get("page-two").is_some());
    }

    #[test]
    fn directory_not_found_returns_io_error() {
        let result = MarkdownRegistry::from_dir(std::path::Path::new(
            "/nonexistent/path/that/does/not/exist",
        ));
        assert!(matches!(result, Err(MarkdownError::Io { .. })));
    }

    #[test]
    fn from_dir_skips_subdirectory_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("real-page.md"),
            "+++\ntitle = \"Real\"\norder = 1\n+++\n\n# Real\n",
        )
        .unwrap();
        // A subdirectory whose name ends in .md must be skipped.
        std::fs::create_dir(dir.path().join("subdir.md")).unwrap();

        let registry = MarkdownRegistry::from_dir(dir.path()).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.get("real-page").is_some());
    }

    #[test]
    fn from_dir_duplicate_slug_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // Two files with the same stem produce a duplicate slug.
        std::fs::write(
            dir.path().join("page.md"),
            "+++\ntitle = \"Page A\"\norder = 1\n+++\n\n# A\n",
        )
        .unwrap();
        // On most filesystems a second `page.md` cannot exist; simulate the
        // duplicate by writing a file whose stem matches an already-inserted
        // slug.  We achieve this via a wrapper that inserts the same slug twice
        // using from_embedded instead, since from_dir cannot have two files with
        // the same name in the same directory.
        // Test the embedded path only (the from_dir duplicate path is identical code).
        let dup = MarkdownSource {
            slug: "page",
            content: "+++\ntitle = \"Page B\"\norder = 2\n+++\n\n# B\n",
        };
        let base = MarkdownSource {
            slug: "page",
            content: "+++\ntitle = \"Page A\"\norder = 1\n+++\n\n# A\n",
        };
        let result = MarkdownRegistry::from_embedded(&[base, dup]);
        assert!(matches!(result, Err(MarkdownError::DuplicateSlug { .. })));
        // Also verify from_dir returns one page cleanly (no duplicate possible
        // in a real filesystem with identical filenames).
        let registry = MarkdownRegistry::from_dir(dir.path()).unwrap();
        assert_eq!(registry.len(), 1);
    }

    // ---- Integration test: statically renders at least two Markdown pages ----

    #[tokio::test]
    async fn integration_statically_renders_two_markdown_pages() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc;

        use crate::markdown::RenderOptions;
        use crate::markdown::render;
        use crate::static_gen::{StaticRouteMeta, render_static_routes};

        // Defined before any `let` statements to satisfy `items_after_statements`.
        fn doc_params(_: axum::Router) -> Pin<Box<dyn Future<Output = Vec<StaticParams>> + Send>> {
            Box::pin(async {
                vec![
                    crate::static_params! { "slug" => "getting-started" },
                    crate::static_params! { "slug" => "api-reference" },
                ]
            })
        }

        let registry =
            Arc::new(MarkdownRegistry::from_embedded(&[GETTING_STARTED, API_REFERENCE]).unwrap());

        // Build a router that renders pages from the registry.
        let reg = registry.clone();
        let router = axum::Router::new().route(
            "/docs/{slug}",
            axum::routing::get({
                let r = reg.clone();
                move |axum::extract::Path(slug): axum::extract::Path<String>| {
                    let r = r.clone();
                    async move {
                        r.get(&slug).map_or_else(
                            || (axum::http::StatusCode::NOT_FOUND, "not found".to_owned()),
                            |page| {
                                let rendered = render(&page.body, RenderOptions::default());
                                (axum::http::StatusCode::OK, rendered.html)
                            },
                        )
                    }
                }
            }),
        );

        let meta = StaticRouteMeta {
            path: "/docs/{slug}",
            name: "test_show_doc",
            revalidate: None,
            params_fn: Some(doc_params),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        };

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");

        render_static_routes(router, &[meta], &dist).await.unwrap();

        // Both pages must be pre-rendered.
        let html_a = std::fs::read_to_string(dist.join("docs/getting-started/index.html")).unwrap();
        assert!(html_a.contains("Getting Started"));

        let html_b = std::fs::read_to_string(dist.join("docs/api-reference/index.html")).unwrap();
        assert!(html_b.contains("API Reference"));
    }
}
