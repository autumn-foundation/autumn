//! `sitemap.xml` and `robots.txt`, served from the database.
//!
//! These are **not** registered through `AppBuilder::seo_source`. That trait is
//! evaluated once, at router-build time — `assemble_seo_bodies` renders the
//! entries into a string and the framework serves that string thereafter. It is
//! the right shape for a fixed set of app-authored URLs (see
//! `examples/blog`), and the wrong one for a CMS, where the whole point is that
//! an editor publishes a post at 3pm and it appears in the sitemap at 3pm.
//!
//! Registering `/sitemap.xml` here makes the framework's SEO router stand down
//! (it yields on collision rather than panicking), and it yields *both* routes
//! as a pair — so `robots.txt` is served here too, from the same
//! `[seo]` configuration it would otherwise have used.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;

use crate::models::Post;
use crate::routes::site::Repos;

/// How many URLs one sitemap carries.
///
/// The sitemap protocol caps a single file at 50,000 URLs and 50 MB. A site
/// past that needs a sitemap *index*, which is a different document; capping
/// here keeps the single-file response valid rather than silently emitting an
/// oversized one crawlers reject.
const MAX_URLS: usize = 50_000;

#[get("/sitemap.xml")]
pub async fn sitemap(repos: Repos, State(state): State<AppState>) -> AutumnResult<Response> {
    let settings = repos.settings().await?;
    let base = crate::routes::feed::base_url(&state);

    let mut urls: Vec<(String, Option<String>)> = vec![(format!("{base}/"), None)];

    // Every published post and page, at the permalink the site is currently
    // configured to mint — read from settings rather than assumed, because a
    // sitemap full of URLs that 404 is worse than no sitemap at all.
    for post_type in crate::content_types::all_post_types() {
        if !post_type.public {
            continue;
        }
        // Ask for at most what is still needed. Loading every published row and
        // truncating afterwards bounds the response but not the database,
        // memory or per-post ancestry work — on an unauthenticated endpoint.
        let remaining = MAX_URLS.saturating_sub(urls.len());
        if remaining == 0 {
            break;
        }
        let batch = repos
            .published_posts(post_type.slug, i64::try_from(remaining).unwrap_or(i64::MAX))
            .await?;
        // Ancestors for the whole batch in one query per level, rather than one
        // per ancestor per page — `Repos::permalink` walks the chain a row at a
        // time, and this endpoint is unauthenticated with a 50,000-URL budget,
        // so a page-heavy site turned one crawler request into tens of
        // thousands of round trips. Only a hierarchical type has ancestry at
        // all; for the rest the map is never consulted.
        let ancestors = if post_type.hierarchical {
            let ids: Vec<i64> = batch.iter().map(|post| post.id).collect();
            repos
                .with_conn(async move |conn| crate::content::posts_with_ancestors(conn, &ids).await)
                .await?
        } else {
            std::collections::HashMap::new()
        };
        for post in batch {
            // Emitted whatever shape it has, query string included. The
            // `plain` structure makes every post's canonical URL `/?p=<id>` —
            // which `front_page` serves — so skipping query-string paths
            // dropped the *entire* post corpus from the sitemap on a site that
            // chose that structure. A query string is valid in `<loc>`; the
            // `&` that a multi-parameter one would carry is escaped by
            // `escape_xml` below.
            let path = crate::routes::site::permalink_from(&post, &ancestors, &settings);
            let lastmod = post.published_at.map(|published| {
                post.updated_at
                    .max(published)
                    .and_utc()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            });
            urls.push((format!("{base}{path}"), lastmod));
        }
    }

    // Term archives that actually have published content. Listing empty ones is
    // how a sitemap earns a reputation for noise — and the `post_count > 0`
    // filter and the remaining budget are both applied in SQL, so a site whose
    // posts already filled the cap does not load every term just to discard
    // them.
    for taxonomy in crate::content_types::all_taxonomies() {
        let remaining = MAX_URLS.saturating_sub(urls.len());
        if remaining == 0 {
            break;
        }
        let mut conn = repos.conn().await?;
        for term in crate::content::populated_terms(
            &mut conn,
            taxonomy.slug,
            i64::try_from(remaining).unwrap_or(i64::MAX),
        )
        .await?
        {
            urls.push((format!("{base}{}", crate::theme::term_url(&term)), None));
        }
    }

    // A belt to the per-type budget above: term archives are appended after
    // the posts and have no budget of their own.
    urls.truncate(MAX_URLS);

    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    for (url, lastmod) in &urls {
        body.push_str("  <url>\n    <loc>");
        body.push_str(&escape_xml(url));
        body.push_str("</loc>\n");
        if let Some(lastmod) = lastmod {
            body.push_str("    <lastmod>");
            body.push_str(&escape_xml(lastmod));
            body.push_str("</lastmod>\n");
        }
        body.push_str("  </url>\n");
    }
    body.push_str("</urlset>\n");

    Ok((
        [(
            autumn_web::reexports::http::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

#[get("/robots.txt")]
pub async fn robots(State(state): State<AppState>) -> Response {
    let config = state.config_arc();
    let base = crate::routes::feed::base_url(&state);

    // Anything that is not a production profile is disallowed wholesale. A
    // staging copy of a site indexed alongside the real one splits its ranking
    // and leaks unreleased content, and that mistake is made once per company.
    let production = config
        .profile
        .as_deref()
        .is_some_and(|profile| matches!(profile, "prod" | "production"));

    let body = if production {
        format!(
            "User-agent: *\nAllow: /\nDisallow: /admin\nDisallow: /api/\n\nSitemap: {base}/sitemap.xml\n"
        )
    } else {
        "User-agent: *\nDisallow: /\n".to_owned()
    };

    (
        [(
            autumn_web::reexports::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// Escape the five XML metacharacters.
///
/// A slug is ASCII by construction, but a `base_url` is operator-supplied and a
/// query string can reach these values — an unescaped `&` is a malformed
/// document, which crawlers reject outright.
fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Silence the unused-import warning when no query in this module names `Post`
/// directly; it is the type `published_posts` returns.
#[allow(dead_code)]
fn _type_uses(_: Post) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_metacharacters_are_escaped() {
        assert_eq!(
            escape_xml("https://x.test/?a=1&b=2"),
            "https://x.test/?a=1&amp;b=2"
        );
        assert_eq!(escape_xml("<script>"), "&lt;script&gt;");
    }
}
