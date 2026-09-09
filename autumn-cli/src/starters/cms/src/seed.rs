//! Demo content.
//!
//! Run it with `autumn task seed_demo_content`. It is a one-off task rather
//! than something the server does at boot: seeding on startup means a
//! production deploy that briefly loses its database comes back up and writes
//! sample posts into a live site.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{NewMenu, NewMenuItem, NewPost, NewSiteOption, NewTerm, NewWidget};
use crate::schema::{menu_items, menus, options, posts, terms, users, widgets};
use crate::settings::Settings;

/// Populate a fresh site with representative content.
///
/// Idempotent: every insert is guarded on the natural key it would collide on,
/// so running it twice changes nothing the second time. A seed that cannot be
/// re-run is a seed nobody dares run.
#[autumn_web::task(name = "seed-demo")]
pub async fn seed_demo_content(mut db: Db) -> AutumnResult<()> {
    let conn = &mut *db;

    // The default settings, written out so the Settings screen shows real rows
    // rather than implicit fallbacks.
    for (name, value) in Settings::default().to_rows() {
        let exists: i64 = options::table
            .filter(options::name.eq(name))
            .count()
            .get_result(conn)
            .await?;
        if exists == 0 {
            diesel::insert_into(options::table)
                .values(&NewSiteOption {
                    name: name.to_owned(),
                    value,
                    autoload: true,
                })
                .execute(conn)
                .await?;
        }
    }

    // Seeding needs an author, and the seed deliberately does not create an
    // account: a committed default password is a security defect, and the
    // first-registration-owns-the-site flow already covers the empty case.
    let Some(author_id): Option<i64> = users::table
        .order(users::id.asc())
        .select(users::id)
        .first(conn)
        .await
        .optional()?
    else {
        autumn_web::reexports::tracing::warn!(
            "no accounts yet — register at /register first, then re-run this task"
        );
        return Ok(());
    };

    // Terms.
    for (taxonomy, name, slug) in [
        ("category", "Announcements", "announcements"),
        ("category", "Engineering", "engineering"),
        ("post_tag", "rust", "rust"),
        ("post_tag", "cms", "cms"),
    ] {
        let exists: i64 = terms::table
            .filter(terms::taxonomy.eq(taxonomy))
            .filter(terms::slug.eq(slug))
            .count()
            .get_result(conn)
            .await?;
        if exists == 0 {
            diesel::insert_into(terms::table)
                .values(&NewTerm {
                    taxonomy: taxonomy.to_owned(),
                    name: name.to_owned(),
                    slug: slug.to_owned(),
                    description: String::new(),
                    parent_id: None,
                })
                .execute(conn)
                .await?;
        }
    }

    // Content.
    let now = chrono::Utc::now().naive_utc();
    seed_posts(conn, author_id, now).await?;

    // A primary menu pointing at the About page.
    let menu_exists: i64 = menus::table
        .filter(menus::slug.eq("primary"))
        .count()
        .get_result(conn)
        .await?;
    if menu_exists == 0 {
        let menu_id: i64 = diesel::insert_into(menus::table)
            .values(&NewMenu {
                name: "Primary".to_owned(),
                slug: "primary".to_owned(),
                location: "primary".to_owned(),
            })
            .returning(menus::id)
            .get_result(conn)
            .await?;

        let about_id: Option<i64> = posts::table
            .filter(posts::post_type.eq("page"))
            .filter(posts::slug.eq("about"))
            .select(posts::id)
            .first(conn)
            .await
            .optional()?;

        diesel::insert_into(menu_items::table)
            .values(&NewMenuItem {
                menu_id,
                parent_id: None,
                label: "Home".to_owned(),
                url: "/".to_owned(),
                post_id: None,
                term_id: None,
                position: 0,
            })
            .execute(conn)
            .await?;
        if let Some(about_id) = about_id {
            diesel::insert_into(menu_items::table)
                .values(&NewMenuItem {
                    menu_id,
                    parent_id: None,
                    label: "About".to_owned(),
                    url: String::new(),
                    post_id: Some(about_id),
                    term_id: None,
                    position: 1,
                })
                .execute(conn)
                .await?;
        }
    }

    // A sidebar.
    let widget_count: i64 = widgets::table.count().get_result(conn).await?;
    if widget_count == 0 {
        for (position, kind, title, settings) in [
            (0, "search", "Search", serde_json::json!({})),
            (
                1,
                "recent_posts",
                "Recent posts",
                serde_json::json!({ "count": 5 }),
            ),
            (2, "categories", "Categories", serde_json::json!({})),
        ] {
            diesel::insert_into(widgets::table)
                .values(&NewWidget {
                    sidebar: "primary".to_owned(),
                    kind: kind.to_owned(),
                    title: title.to_owned(),
                    settings,
                    position,
                })
                .execute(conn)
                .await?;
        }
    }

    autumn_web::reexports::tracing::info!("demo content seeded");
    Ok(())
}

/// Seed the demo posts and pages.
///
/// Extracted so a test can drive it directly: the task takes a `Db`
/// extractor, which a test has no way to build, and the skip rule below is
/// exactly the part that needed covering.
pub async fn seed_posts(
    conn: &mut autumn_web::reexports::diesel_async::AsyncPgConnection,
    author_id: i64,
    now: chrono::NaiveDateTime,
) -> AutumnResult<()> {
    let content = [
        (
            "post",
            "Hello, world",
            "hello-world",
            "The first post on a brand new site.",
            "# Hello\n\nThis site runs on **Autumn CMS**. Edit or delete this post, then start \
         writing.\n\nEverything you would reach for in WordPress is here: categories and \
         tags, pages, a media library, threaded comments with a moderation queue, \
         revisions, menus, widgets, and a REST API.",
        ),
        (
            "page",
            "About",
            "about",
            "",
            "This is a page. Pages are hierarchical and undated — they sit outside the blog and \
         are addressed by their path.",
        ),
    ];
    for (post_type, title, slug, excerpt, body) in content {
        // Asked of the whole bare-path namespace, not just this type. `post`
        // and top-level `page` both mint `/about`, and `idx_posts_bare_path_slug`
        // enforces that — so a same-type check reported "not present" for a
        // *page* named `about` and the insert then failed on the constraint,
        // taking the rest of the seed with it after the settings and terms had
        // already committed.
        //
        // Skipping rather than allocating a suffix is deliberate: this is demo
        // content, and `/about-2` beside somebody's real `/about` is worse than
        // not seeding it.
        // The types this slug actually competes with. A bare-path type competes
        // with every other bare-path type; a custom type is addressed under its
        // own prefix and competes only with itself, so widening the check for it
        // would skip content nothing was blocking.
        let competing: Vec<&str> = if crate::content::BARE_PATH_TYPES.contains(&post_type) {
            crate::content::BARE_PATH_TYPES.to_vec()
        } else {
            vec![post_type]
        };
        let exists: i64 = posts::table
            .filter(posts::slug.eq(slug))
            .filter(posts::post_type.eq_any(competing))
            .count()
            .get_result(conn)
            .await?;
        if exists > 0 {
            continue;
        }
        diesel::insert_into(posts::table)
            .values(&NewPost {
                post_type: post_type.to_owned(),
                title: title.to_owned(),
                slug: slug.to_owned(),
                excerpt: excerpt.to_owned(),
                body: body.to_owned(),
                status: "publish".to_owned(),
                author_id,
                parent_id: None,
                featured_media_id: None,
                menu_order: 0,
                comment_status: "open".to_owned(),
                password: String::new(),
                sticky: false,
                published_at: Some(now),
            })
            .execute(conn)
            .await?;
    }
    Ok(())
}
