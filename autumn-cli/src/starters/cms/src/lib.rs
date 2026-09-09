//! Autumn CMS — a WordPress-core-parity content management system.
//!
//! The module map, for a reader who knows WordPress:
//!
//! | Module | WordPress equivalent |
//! |---|---|
//! | [`models`] | `wp_posts`, `wp_terms`, `wp_options`, `wp_comments`, … |
//! | [`capabilities`] | roles and `current_user_can()` |
//! | [`content_types`] | `register_post_type` / `register_taxonomy` |
//! | [`settings`] | `get_option` / `update_option` |
//! | [`permalinks`] | the rewrite rules and Settings → Permalinks |
//! | [`plugins`] | `do_action` / `apply_filters` |
//! | [`shortcodes`] | `add_shortcode` |
//! | [`theme`] | the template hierarchy, menus and widgets |
//! | [`content`] | `wp_insert_post`, `wp_set_object_terms`, comment moderation |
//! | [`routes::admin`] | wp-admin |
//! | [`routes::api`] | the REST API (`/wp-json`) |

pub mod capabilities;
pub mod content;
pub mod content_types;
pub mod hooks;
pub mod models;
pub mod permalinks;
pub mod plugins;
pub mod repositories;
pub mod routes;
pub mod schema;
pub mod seed;
pub mod seo;

#[doc(hidden)]
pub use seo as cms_seo;
pub mod settings;
pub mod shortcodes;
pub mod tasks;
pub mod taxonomy;
pub mod theme;

/// Every route the application serves.
///
/// Defined here rather than inline in `main` so the binary and the integration
/// tests mount the *same* table. When they were two lists, the test suite
/// asserted properties of a route set the server never served — which is how a
/// reserved prefix can go missing in production and still pass its own test.
///
/// Ordering note: `front::dispatch` is the `/{*path}` catch-all that serves
/// every permalink. axum prefers a literal path over a wildcard regardless of
/// registration order, so a new reserved prefix needs a literal route — see
/// `every_reserved_prefix_has_a_literal_route` in `tests/integration_test.rs`,
/// which fails if one is forgotten.
#[must_use]
pub fn all_routes() -> Vec<autumn_web::Route> {
    autumn_web::routes![
        // ── Auth ────────────────────────────────────────────────
        routes::auth::login_form,
        routes::auth::login,
        routes::auth::logout,
        routes::auth::register_form,
        routes::auth::register,
        // ── Admin ───────────────────────────────────────────────
        routes::admin::dashboard,
        routes::admin::posts::list,
        routes::admin::posts::new_form,
        routes::admin::posts::create,
        routes::admin::posts::edit_form,
        routes::admin::posts::update,
        routes::admin::posts::transition,
        routes::admin::posts::revisions,
        routes::admin::posts::restore,
        routes::admin::terms::list,
        routes::admin::terms::create,
        routes::admin::terms::delete,
        routes::admin::comments::list,
        routes::admin::comments::moderate,
        routes::admin::comments::delete,
        routes::admin::media::list,
        routes::admin::media::upload,
        routes::admin::media::delete,
        routes::admin::users::list,
        routes::admin::users::create,
        routes::admin::users::update,
        routes::admin::users::delete,
        routes::admin::settings::show,
        routes::admin::settings::save,
        routes::admin::appearance::show,
        routes::admin::appearance::create_menu,
        routes::admin::appearance::create_menu_item,
        routes::admin::appearance::delete_menu_item,
        routes::admin::appearance::create_widget,
        routes::admin::appearance::delete_widget,
        routes::admin::tools::show,
        routes::admin::tools::export,
        routes::admin::tools::import,
        // ── REST API ────────────────────────────────────────────
        routes::api::site_info,
        routes::api::list_posts,
        routes::api::create_post,
        routes::api::get_post,
        routes::api::list_comments,
        routes::api::list_terms,
        routes::api::list_authors,
        // ── Public site ─────────────────────────────────────────
        routes::admin::media::serve,
        cms_seo::sitemap,
        cms_seo::robots,
        routes::feed::atom,
        routes::feed::rss,
        routes::comments::post_comment,
        routes::front::favicon,
        routes::front::unlock,
        routes::front::search,
        routes::front::front_page,
        routes::front::dispatch,
    ]
}

/// Register everything that must exist before the router is built.
///
/// This is where a plugin would hook in. Called from `main` — and from the
/// integration tests, so the test app and the real one see the same registry.
pub fn bootstrap() {
    use plugins::{Action, DEFAULT_PRIORITY, Filter, add_action, add_filter};

    // A shortcode: `[note]Text[/note]` is the enclosing form this parser does
    // not support, so the self-closing spelling carries its text as an
    // attribute — `[note text="Heads up"]`.
    shortcodes::add_shortcode("note", |attrs| {
        let text = attrs.get("text").map_or("", String::as_str);
        format!(
            r#"<aside class="border-l-4 border-amber-400 bg-amber-50 px-4 py-2 my-4">{}</aside>"#,
            shortcodes::escape_html(text)
        )
    });

    // The current year, so a footer or a post can say `[year]` and stay right.
    shortcodes::add_shortcode("year", |_| chrono::Utc::now().format("%Y").to_string());

    // A filter, demonstrating the `the_content` hook: turn a bare `--` into an
    // em dash, the way a typographic plugin would.
    add_filter(Filter::TheContent, DEFAULT_PRIORITY, |html| {
        html.replace(" -- ", " \u{2014} ")
    });

    // An action listener. Real work that can fail belongs in a `#[job]`; a
    // listener is synchronous and infallible by design, so this logs.
    add_action(Action::PostTransitioned, DEFAULT_PRIORITY, |post_id| {
        reexports::tracing::debug!(post_id, "post status changed");
    });
}

pub use autumn_web::reexports;
