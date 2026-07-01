mod admin;
mod models;
mod routes;
mod schema;
mod tasks;

use autumn_admin_plugin::AdminPlugin;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::seo::{SitemapEntry, SitemapSource};
use autumn_web::{jobs, one_off_tasks, routes, static_routes};
use std::future::Future;
use std::pin::Pin;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// Dynamic sitemap source: provides a URL for every published blog post.
///
/// In a real app this would query the database; here we return a
/// representative static list so `autumn build` and the running server
/// produce a valid `/sitemap.xml` without requiring a live database.
struct BlogSitemapSource;

impl SitemapSource for BlogSitemapSource {
    fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>> {
        Box::pin(async {
            vec![
                SitemapEntry::new("https://autumn-demo.example.com/")
                    .changefreq(autumn_web::seo::SitemapChangefreq::Weekly),
                SitemapEntry::new("https://autumn-demo.example.com/about"),
            ]
        })
    }
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        // In-process fragment cache for the post-list view. Rendered post
        // cards are cached by `(post.id, post.updated_at)` (see
        // `routes::posts::post_card`), so unchanged rows skip the `html!{}`
        // work and editing a post re-renders only that card. Swap in the
        // Redis backend to share the cache across replicas.
        .with_cache_backend(autumn_web::cache::MokaCache::new(1_000, None))
        // Auto-load i18n bundle from `i18n/<locale>.ftl` according to the
        // `[i18n]` block in `autumn.toml`. Visit `/greet` to see it work
        // end-to-end with a locale switcher.
        .i18n_auto()
        .plugin(
            AdminPlugin::new()
                .prefix("/backoffice")
                .require_role(None::<String>)
                .register(admin::PostAdmin),
        )
        // Register the sitemap source: mounts /robots.txt and /sitemap.xml.
        // Configure [seo] base_url in autumn.toml for canonical URL injection.
        .seo_source(BlogSitemapSource)
        .routes(routes![
            // Public routes
            routes::about::about, // #[static_get] — pre-rendered
            routes::posts::index,
            routes::posts::show,
            routes::greet::greet, // i18n demo
            // Admin routes
            routes::posts::admin_list,
            routes::posts::new_form,
            routes::posts::create,
            routes::posts::edit_form,
            routes::posts::update,
            routes::posts::delete_post,
            // OAuth2 sign-in (enabled via oauth2 feature in Cargo.toml)
            routes::oauth::oauth_redirect,
            routes::oauth::oauth_callback,
            // JSON API
            routes::api::list_json,
            routes::api::create_json,
            routes::api::enqueue_publish_webhook,
            routes::api::credentials_status,
        ])
        .jobs(jobs![routes::api::publish_webhook])
        .one_off_tasks(one_off_tasks![tasks::cleanup_posts])
        .static_routes(static_routes![routes::about::about,])
        .run()
        .await;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use autumn_admin_plugin::prelude::*;

    const MIGRATION_SQL: &str = include_str!("../migrations/00000000000000_create_posts/up.sql");

    #[test]
    fn migration_uses_bigserial_ids() {
        assert!(
            MIGRATION_SQL.contains("id BIGSERIAL PRIMARY KEY"),
            "post IDs must be 64-bit to match the Int8/i64 application schema",
        );
    }

    #[test]
    fn upgrade_migration_widens_existing_ids() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("migrations/00000000000001_widen_post_ids_to_bigint/up.sql");
        let sql = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("missing upgrade migration at {}: {err}", path.display()));

        assert!(
            sql.contains("ALTER TABLE posts ALTER COLUMN id TYPE BIGINT"),
            "post upgrade migration must widen existing IDs to BIGINT",
        );
        assert!(
            sql.contains("ALTER SEQUENCE posts_id_seq AS BIGINT"),
            "post upgrade migration must widen the backing sequence to BIGINT",
        );
    }

    #[test]
    fn backoffice_admin_fields_match_blog_post_shape() {
        let fields = super::admin::PostAdmin.fields();
        assert!(
            fields.iter().any(|field| {
                field.name == "title"
                    && matches!(field.kind, AdminFieldKind::Text)
                    && field.searchable
            }),
            "expected searchable title field in admin schema"
        );
        assert!(
            fields.iter().any(|field| {
                field.name == "published"
                    && matches!(field.kind, AdminFieldKind::Boolean)
                    && field.filterable
            }),
            "expected filterable published field in admin schema"
        );
        assert!(
            fields.iter().any(|field| {
                field.name == "created_at"
                    && matches!(field.kind, AdminFieldKind::DateTime)
                    && !field.editable
            }),
            "expected readonly created_at field in admin schema"
        );
    }

    #[tokio::test]
    async fn static_about_page_renders_translated_layout_labels() {
        let bundle = autumn_web::i18n::Bundle::load_from_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n"),
            &autumn_web::i18n::I18nConfig {
                supported_locales: vec!["en".to_owned(), "es".to_owned()],
                ..Default::default()
            },
        )
        .expect("blog i18n bundle");
        let locale = autumn_web::i18n::Locale::new("en").with_bundle(std::sync::Arc::new(bundle));

        let html = super::routes::about::about(locale).await.into_string();

        assert!(html.contains("Autumn Blog"), "html: {html}");
        assert!(!html.contains("nav.brand"), "html: {html}");
    }

    #[tokio::test]
    async fn home_hero_i18n_keys_resolve_in_both_locales() {
        let bundle = autumn_web::i18n::Bundle::load_from_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n"),
            &autumn_web::i18n::I18nConfig {
                supported_locales: vec!["en".to_owned(), "es".to_owned()],
                ..Default::default()
            },
        )
        .expect("blog i18n bundle");
        let bundle = std::sync::Arc::new(bundle);

        let en = autumn_web::i18n::Locale::new("en").with_bundle(bundle.clone());
        assert_eq!(en.t("home.hero.title"), "Welcome to the Blog");
        assert_eq!(
            en.t("home.hero.subtitle"),
            "Thoughts, tutorials, and stories — powered by Autumn."
        );

        let es = autumn_web::i18n::Locale::new("es").with_bundle(bundle);
        assert_eq!(es.t("home.hero.title"), "Bienvenido al Blog");
        assert_eq!(
            es.t("home.hero.subtitle"),
            "Reflexiones, tutoriales e historias — con la potencia de Autumn."
        );
    }
}
