// SEO toolkit integration tests.

use autumn_web::seo::{
    SeoMeta, SitemapChangefreq, SitemapEntry, SitemapSource, robots_txt, sitemap_xml,
};
use std::future::Future;
use std::pin::Pin;

// ── robots_txt() unit tests ──────────────────────────────────────────────────

#[test]
fn robots_txt_dev_disallows_all() {
    let txt = robots_txt("dev", None, &[]);
    assert!(txt.contains("User-agent: *"), "missing User-agent");
    assert!(txt.contains("Disallow: /"), "dev should disallow all");
    assert!(!txt.contains("Allow: /"), "dev must not allow all");
}

#[test]
fn robots_txt_test_disallows_all() {
    let txt = robots_txt("test", None, &[]);
    assert!(txt.contains("Disallow: /"), "test should disallow all");
}

#[test]
fn robots_txt_prod_allows_all() {
    let txt = robots_txt("prod", None, &[]);
    assert!(txt.contains("User-agent: *"), "missing User-agent");
    assert!(txt.contains("Allow: /"), "prod should allow all");
    assert!(!txt.contains("Disallow: /"), "prod must not disallow all");
}

#[test]
fn robots_txt_injects_sitemap_url() {
    let txt = robots_txt("prod", Some("https://example.com/sitemap.xml"), &[]);
    assert!(
        txt.contains("Sitemap: https://example.com/sitemap.xml"),
        "should inject sitemap URL; got:\n{txt}"
    );
}

#[test]
fn robots_txt_no_sitemap_when_not_provided() {
    let txt = robots_txt("prod", None, &[]);
    assert!(
        !txt.contains("Sitemap:"),
        "should not emit Sitemap: when None"
    );
}

#[test]
fn robots_txt_includes_additional_rules() {
    let rules = vec![
        "Disallow: /admin".to_string(),
        "Crawl-delay: 10".to_string(),
    ];
    let txt = robots_txt("prod", None, &rules);
    assert!(
        txt.contains("Disallow: /admin"),
        "should include additional rules"
    );
    assert!(
        txt.contains("Crawl-delay: 10"),
        "should include crawl delay rule"
    );
}

// ── sitemap_xml() unit tests ─────────────────────────────────────────────────

#[test]
fn sitemap_xml_valid_urlset_structure() {
    let entries = vec![SitemapEntry::new("https://example.com/about")];
    let xml = sitemap_xml(&entries, None);
    assert!(
        xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
        "should start with XML declaration"
    );
    assert!(xml.contains("<urlset"), "should have urlset element");
    assert!(
        xml.contains("http://www.sitemaps.org/schemas/sitemap/0.9"),
        "should have sitemap namespace"
    );
    assert!(
        xml.contains("<loc>https://example.com/about</loc>"),
        "should have location URL"
    );
    assert!(xml.contains("</urlset>"), "should close urlset");
}

#[test]
fn sitemap_xml_includes_lastmod() {
    let entries = vec![SitemapEntry::new("https://example.com/").lastmod("2026-01-15")];
    let xml = sitemap_xml(&entries, None);
    assert!(
        xml.contains("<lastmod>2026-01-15</lastmod>"),
        "should include lastmod"
    );
}

#[test]
fn sitemap_xml_includes_changefreq() {
    let entries =
        vec![SitemapEntry::new("https://example.com/").changefreq(SitemapChangefreq::Weekly)];
    let xml = sitemap_xml(&entries, None);
    assert!(
        xml.contains("<changefreq>weekly</changefreq>"),
        "should include changefreq"
    );
}

#[test]
fn sitemap_xml_includes_priority() {
    let entries = vec![SitemapEntry::new("https://example.com/").priority(0.8)];
    let xml = sitemap_xml(&entries, None);
    assert!(
        xml.contains("<priority>0.8</priority>"),
        "should include priority"
    );
}

#[test]
fn sitemap_xml_escapes_special_chars_in_url() {
    let entries = vec![SitemapEntry::new("https://example.com/?q=hello&amp;world")];
    let xml = sitemap_xml(&entries, None);
    // The URL itself doesn't need & escaping if it's already well-formed,
    // but raw & in URLs should be escaped in XML
    let entries2 = vec![SitemapEntry::new("https://example.com/?q=hello&world")];
    let xml2 = sitemap_xml(&entries2, None);
    assert!(xml2.contains("&amp;"), "should escape & in URL");
    let _ = xml;
}

#[test]
fn sitemap_xml_truncates_at_50k_for_large_sites() {
    // Generate more than 50,000 entries; the function caps at 50,000 and
    // always returns a <urlset> (never a <sitemapindex>).
    let entries: Vec<SitemapEntry> = (0..50_001)
        .map(|i| SitemapEntry::new(format!("https://example.com/page/{i}")))
        .collect();
    let xml = sitemap_xml(&entries, Some("https://example.com"));
    assert!(
        xml.contains("<urlset"),
        "large sites must still return a urlset, not a sitemapindex"
    );
    assert!(
        !xml.contains("<sitemapindex"),
        "sitemapindex must not be generated"
    );
    // The last entry (index 50_000) must be absent — only 0..50_000 included.
    assert!(
        !xml.contains("https://example.com/page/50000"),
        "entry beyond the 50k cap must be truncated"
    );
    assert!(
        xml.contains("https://example.com/page/49999"),
        "entry at the 50k cap must be present"
    );
}

#[test]
fn sitemap_xml_multiple_entries() {
    let entries = vec![
        SitemapEntry::new("https://example.com/"),
        SitemapEntry::new("https://example.com/about"),
        SitemapEntry::new("https://example.com/contact"),
    ];
    let xml = sitemap_xml(&entries, None);
    assert_eq!(xml.matches("<url>").count(), 3, "should have 3 url entries");
}

// ── SeoMeta::render() unit tests ─────────────────────────────────────────────

#[cfg(feature = "maud")]
mod meta_tag_tests {
    use super::*;

    #[test]
    fn seometa_renders_title() {
        let meta = SeoMeta::new().title("My Blog Post");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains("<title>My Blog Post</title>"),
            "should render title; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_renders_description() {
        let meta = SeoMeta::new().description("A great post about things");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"name="description""#),
            "should have name=description; got:\n{rendered}"
        );
        assert!(
            rendered.contains("A great post about things"),
            "should have description content; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_renders_canonical() {
        let meta = SeoMeta::new().canonical("https://example.com/posts/my-post");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"rel="canonical""#),
            "should have rel=canonical; got:\n{rendered}"
        );
        assert!(
            rendered.contains("https://example.com/posts/my-post"),
            "should have canonical URL; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_renders_og_tags() {
        let meta = SeoMeta::new()
            .title("Post Title")
            .description("Post Description")
            .og_image("https://example.com/og.jpg");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"property="og:title""#),
            "should have og:title; got:\n{rendered}"
        );
        assert!(
            rendered.contains(r#"property="og:description""#),
            "should have og:description; got:\n{rendered}"
        );
        assert!(
            rendered.contains(r#"property="og:image""#),
            "should have og:image; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_og_title_overrides_title() {
        let meta = SeoMeta::new()
            .title("Generic Title")
            .og_title("OG Specific Title");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains("OG Specific Title"),
            "og_title should override title for OG; got:\n{rendered}"
        );
        assert!(
            rendered.contains("<title>Generic Title</title>"),
            "page title should remain unchanged"
        );
    }

    #[test]
    fn seometa_renders_twitter_card() {
        let meta = SeoMeta::new()
            .title("Post Title")
            .twitter_card("summary_large_image");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"name="twitter:card""#),
            "should have twitter:card; got:\n{rendered}"
        );
        assert!(
            rendered.contains("summary_large_image"),
            "should have card type; got:\n{rendered}"
        );
        assert!(
            rendered.contains(r#"name="twitter:title""#),
            "should have twitter:title when card set; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_no_twitter_title_without_card() {
        let meta = SeoMeta::new().title("Post Title");
        let rendered = meta.render().into_string();
        assert!(
            !rendered.contains(r#"name="twitter:title""#),
            "should not emit twitter:title without twitter:card"
        );
    }

    #[test]
    fn seometa_renders_robots_directive() {
        let meta = SeoMeta::new().robots("noindex");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"name="robots""#),
            "should have robots meta; got:\n{rendered}"
        );
        assert!(
            rendered.contains("noindex"),
            "should have noindex directive; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_og_url_defaults_to_canonical() {
        let meta = SeoMeta::new().canonical("https://example.com/post");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"property="og:url""#),
            "og:url should use canonical as fallback; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_empty_renders_nothing() {
        let meta = SeoMeta::new();
        let rendered = meta.render().into_string();
        // Empty meta should produce empty/minimal markup - no spurious tags
        assert!(
            !rendered.contains("<title>"),
            "empty meta should not render title"
        );
        assert!(
            !rendered.contains(r#"name="description""#),
            "empty meta should not render description"
        );
    }
}

// ── SitemapSource trait tests ────────────────────────────────────────────────

struct TestSitemapSource {
    entries: Vec<SitemapEntry>,
}

impl SitemapSource for TestSitemapSource {
    fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>> {
        let entries = self.entries.clone();
        Box::pin(async move { entries })
    }
}

#[tokio::test]
async fn sitemap_source_provides_entries() {
    let source = TestSitemapSource {
        entries: vec![
            SitemapEntry::new("https://example.com/post/1"),
            SitemapEntry::new("https://example.com/post/2"),
        ],
    };

    let entries = source.entries().await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].loc, "https://example.com/post/1");
    assert_eq!(entries[1].loc, "https://example.com/post/2");
}

// ── SeoConfig deserialization tests ─────────────────────────────────────────

#[test]
fn seo_config_deserializes_from_toml() {
    let toml = r#"
[seo]
base_url = "https://example.com"

[seo.robots]
additional_rules = ["Disallow: /admin"]
"#;
    let config: autumn_web::config::AutumnConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.seo.base_url.as_deref(), Some("https://example.com"));
    assert_eq!(config.seo.robots.additional_rules, vec!["Disallow: /admin"]);
}

#[test]
fn seo_config_defaults_are_empty() {
    let config = autumn_web::config::AutumnConfig::default();
    assert!(config.seo.base_url.is_none());
    assert!(config.seo.robots.additional_rules.is_empty());
    assert!(config.seo.robots.allow_all.is_none());
}

// ── HTTP endpoint tests ──────────────────────────────────────────────────────

mod endpoint_tests {
    use autumn_web::seo::{SitemapEntry, build_seo_router, build_seo_router_with_entries};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Verifies that when SEO routes are enabled, /robots.txt returns 200.
    #[tokio::test]
    async fn robots_txt_endpoint_returns_200() {
        let router: Router = build_seo_router("dev", None, &[]);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn robots_txt_endpoint_returns_text_plain() {
        let router: Router = build_seo_router("prod", None, &[]);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/plain"),
            "robots.txt should have text/plain content-type; got: {content_type}"
        );
    }

    #[tokio::test]
    async fn sitemap_xml_endpoint_returns_200() {
        let entries = vec![SitemapEntry::new("https://example.com/")];
        let router: Router =
            build_seo_router_with_entries("prod", Some("https://example.com"), &[], &entries);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/sitemap.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sitemap_xml_endpoint_returns_application_xml() {
        let entries = vec![SitemapEntry::new("https://example.com/")];
        let router: Router =
            build_seo_router_with_entries("prod", Some("https://example.com"), &[], &entries);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/sitemap.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("application/xml") || content_type.contains("text/xml"),
            "sitemap.xml should have XML content-type; got: {content_type}"
        );
    }
}

// ── Static build SEO output tests ────────────────────────────────────────────

#[tokio::test]
async fn write_seo_files_creates_robots_txt() {
    use autumn_web::seo::write_seo_files;

    let dir = tempfile::tempdir().unwrap();
    write_seo_files(dir.path(), "prod", None, None, &[], &[])
        .await
        .unwrap();
    let robots_path = dir.path().join("robots.txt");
    assert!(robots_path.exists(), "dist/robots.txt should be written");
    let content = std::fs::read_to_string(&robots_path).unwrap();
    assert!(
        content.contains("Allow: /"),
        "prod robots.txt should allow all"
    );
}

#[tokio::test]
async fn write_seo_files_creates_sitemap_xml() {
    use autumn_web::seo::{SitemapEntry, write_seo_files};

    let dir = tempfile::tempdir().unwrap();
    let entries = vec![SitemapEntry::new("https://example.com/about")];
    write_seo_files(
        dir.path(),
        "prod",
        Some("https://example.com"),
        None,
        &[],
        &entries,
    )
    .await
    .unwrap();

    let sitemap_path = dir.path().join("sitemap.xml");
    assert!(sitemap_path.exists(), "dist/sitemap.xml should be written");
    let content = std::fs::read_to_string(&sitemap_path).unwrap();
    assert!(
        content.contains("<urlset"),
        "sitemap.xml should have urlset"
    );
    assert!(
        content.contains("https://example.com/about"),
        "sitemap should include provided entries"
    );
}

#[tokio::test]
async fn write_seo_files_injects_sitemap_url_in_robots() {
    use autumn_web::seo::write_seo_files;

    let dir = tempfile::tempdir().unwrap();
    write_seo_files(
        dir.path(),
        "prod",
        Some("https://example.com"),
        None,
        &[],
        &[],
    )
    .await
    .unwrap();

    let robots = std::fs::read_to_string(dir.path().join("robots.txt")).unwrap();
    assert!(
        robots.contains("Sitemap: https://example.com/sitemap.xml"),
        "robots.txt should auto-inject sitemap URL; got:\n{robots}"
    );
}

#[tokio::test]
async fn write_seo_files_respects_sitemap_url_override() {
    use autumn_web::seo::write_seo_files;

    let dir = tempfile::tempdir().unwrap();
    write_seo_files(
        dir.path(),
        "prod",
        Some("https://example.com"),
        Some("https://cdn.example.com/sitemap.xml"),
        &[],
        &[],
    )
    .await
    .unwrap();

    let robots = std::fs::read_to_string(dir.path().join("robots.txt")).unwrap();
    assert!(
        robots.contains("Sitemap: https://cdn.example.com/sitemap.xml"),
        "should use sitemap_url_override; got:\n{robots}"
    );
    assert!(
        !robots.contains("Sitemap: https://example.com/sitemap.xml"),
        "should not use derived sitemap url; got:\n{robots}"
    );
}

// ── SitemapChangefreq coverage ────────────────────────────────────────────────

#[test]
fn sitemap_changefreq_all_variants_render() {
    let variants = [
        (SitemapChangefreq::Always, "always"),
        (SitemapChangefreq::Hourly, "hourly"),
        (SitemapChangefreq::Daily, "daily"),
        (SitemapChangefreq::Weekly, "weekly"),
        (SitemapChangefreq::Monthly, "monthly"),
        (SitemapChangefreq::Yearly, "yearly"),
        (SitemapChangefreq::Never, "never"),
    ];
    for (freq, expected) in variants {
        let entries = vec![SitemapEntry::new("https://example.com/").changefreq(freq)];
        let xml = sitemap_xml(&entries, None);
        assert!(
            xml.contains(&format!("<changefreq>{expected}</changefreq>")),
            "should render {expected}; got:\n{xml}"
        );
    }
}

// ── build_seo_router_from_bodies endpoint tests ───────────────────────────────

mod router_from_bodies_tests {
    use autumn_web::seo::build_seo_router_from_bodies;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn serves_robots_body() {
        let router: Router =
            build_seo_router_from_bodies("User-agent: *\nAllow: /\n".to_string(), String::new());

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn serves_sitemap_body() {
        let router: Router = build_seo_router_from_bodies(
            String::new(),
            "<?xml version=\"1.0\"?><urlset/>".to_string(),
        );

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/sitemap.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}

// ── SeoMeta additional field coverage ────────────────────────────────────────

#[cfg(feature = "maud")]
mod meta_extra_tests {
    use super::*;

    #[test]
    fn seometa_og_type_renders() {
        let meta = SeoMeta::new().og_type("article");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"property="og:type""#),
            "should have og:type; got:\n{rendered}"
        );
        assert!(rendered.contains("article"), "should have og:type value");
    }

    #[test]
    fn seometa_og_description_overrides_description() {
        let meta = SeoMeta::new()
            .description("Generic desc")
            .og_description("OG specific desc");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains("OG specific desc"),
            "og_description should override description for OG; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_og_url_explicit_override() {
        let meta = SeoMeta::new().og_url("https://example.com/canonical-og");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"property="og:url""#),
            "should have og:url; got:\n{rendered}"
        );
        assert!(
            rendered.contains("https://example.com/canonical-og"),
            "should have explicit og:url; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_twitter_image_renders() {
        let meta = SeoMeta::new()
            .twitter_card("summary_large_image")
            .twitter_image("https://example.com/og.jpg");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains(r#"name="twitter:image""#),
            "should have twitter:image; got:\n{rendered}"
        );
        assert!(
            rendered.contains("https://example.com/og.jpg"),
            "should have twitter:image url; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_twitter_title_override() {
        let meta = SeoMeta::new()
            .title("Generic Title")
            .twitter_card("summary")
            .twitter_title("Twitter-specific Title");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains("Twitter-specific Title"),
            "twitter_title should override for twitter:title; got:\n{rendered}"
        );
    }

    #[test]
    fn seometa_twitter_description_override() {
        let meta = SeoMeta::new()
            .description("Generic desc")
            .twitter_card("summary")
            .twitter_description("Twitter-specific desc");
        let rendered = meta.render().into_string();
        assert!(
            rendered.contains("Twitter-specific desc"),
            "twitter_description should override for twitter:description; got:\n{rendered}"
        );
    }
}
