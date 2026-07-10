//! Built-in widget stories: one story per gallery-visible widget in
//! [`crate::widgets`] (issue #1526).
//!
//! Every story is authored with the [`story!`](crate::stories::story) macro,
//! so the snippet shown in the gallery is byte-for-byte the code that
//! rendered. Blocks are self-contained (demo data is defined inside), because
//! they are coerced to zero-arg `fn() -> Markup` pointers — no environment
//! capture compiles.
//!
//! The CI coverage gate (`autumn/tests/integration/stories.rs`) enforces that
//! every public widget fn in `src/widgets.rs` is exercised by at least one
//! story's source. When you add a widget, add (or extend) a story here and
//! add the new top-level widget's slug to `EXPECTED_STORY_SLUGS` in that test.

use super::{Story, story};

/// The built-in story set, in gallery display order.
#[allow(clippy::too_many_lines)]
pub(super) fn builtin_stories() -> Vec<Story> {
    vec![
        story! {
            "Forms",
            "Active search",
            {
                use autumn_web::widgets::{
                    ActiveSearchConfig, active_search, active_search_empty_state,
                    active_search_input, active_search_results,
                };

                let config = ActiveSearchConfig::new("/search", "#post-search-results")
                    .placeholder("Search posts…");
                maud::html! {
                    (active_search("post-search", "Search posts", &config))
                    // What your handler returns when nothing matches:
                    (active_search_empty_state("No posts matched your search."))
                    // Compose the pieces yourself when you need custom layout
                    // between the input and the results container:
                    div {
                        (active_search_input("post-search-split", "Search posts", &config))
                        (active_search_results("post-search-split"))
                    }
                }
            }
        },
        story! {
            "Forms",
            "Autocomplete",
            {
                use autumn_web::widgets::{
                    AutocompleteConfig, autocomplete_empty_state, autocomplete_input,
                    autocomplete_option,
                };

                let config = AutocompleteConfig::new("/tags/search", "tag_id")
                    .placeholder("Start typing a tag…");
                maud::html! {
                    (autocomplete_input("tag-picker", "Tag", &config))
                    // One matching option, as your handler would render it:
                    (autocomplete_option("42", "rust"))
                    // And the empty state when nothing matches:
                    (autocomplete_empty_state("No matching tags."))
                }
            }
        },
        story! {
            "Display",
            "Property list",
            {
                use autumn_web::widgets::property_list;

                property_list(&[
                    ("Title", maud::html! { "Autumn in Practice" }),
                    ("Status", maud::html! { strong { "Published" } }),
                    ("Views", maud::html! { "1,024" }),
                ])
            }
        },
        story! {
            "Display",
            "Data table",
            {
                use autumn_web::widgets::{Column, DataTableConfig, data_table};

                struct Book {
                    title: &'static str,
                    author: &'static str,
                    year: u16,
                }
                let books = [
                    Book { title: "The Long Autumn", author: "R. Ellis", year: 2021 },
                    Book { title: "Falling Leaves", author: "M. Okafor", year: 2023 },
                ];
                let columns = [
                    Column::new("Title", |b: &Book| maud::html! { (b.title) })
                        .sortable("title"),
                    Column::new("Author", |b: &Book| maud::html! { (b.author) }),
                    Column::new("Year", |b: &Book| maud::html! { (b.year) }),
                ];
                let config = DataTableConfig::new("No books yet.")
                    .caption("Books")
                    .base_path("/books");
                data_table(&books, &columns, &config)
            }
        },
        story! {
            "Display",
            "Breadcrumb",
            {
                use autumn_web::widgets::{Crumb, breadcrumb};

                breadcrumb(&[
                    Crumb::link("Home", "/"),
                    Crumb::link("Posts", "/posts"),
                    Crumb::current("My Post"),
                ])
            }
        },
        story! {
            "Display",
            "Card",
            {
                use autumn_web::widgets::{CardConfig, card};

                let body = maud::html! { p { "Ship features, not boilerplate." } };
                let config = CardConfig::new()
                    .title("Release notes")
                    .footer(maud::html! { a href="/changelog" { "Read the changelog" } });
                card(&body, &config)
            }
        },
        story! {
            "Display",
            "Charts",
            {
                use autumn_web::widgets::{
                    ChartConfig, bar_chart, bar_chart_with, line_chart, line_chart_with,
                    sparkline, sparkline_with,
                };

                // A 30-point trend series, built with a loop (no env capture).
                let owned: Vec<(String, f64)> = (0..30)
                    .map(|day| {
                        let label = format!("Day {}", day + 1);
                        let swing = (f64::from(day) * 0.4).sin() * 10.0;
                        let value = 20.0 + swing;
                        (label, value)
                    })
                    .collect();
                let trend: Vec<(&str, f64)> =
                    owned.iter().map(|(l, v)| (l.as_str(), *v)).collect();

                let weekly = [
                    ("Mon", 3.0), ("Tue", 5.0), ("Wed", 4.0),
                    ("Thu", 7.0), ("Fri", 6.0),
                ];

                maud::html! {
                    // Compact inline trend — plain and configured variants:
                    (sparkline(&weekly))
                    (sparkline_with(&weekly, &ChartConfig::new().title("Weekly visits")))
                    // Bars, with a caller axis override + table fallback:
                    (bar_chart(&weekly))
                    (bar_chart_with(
                        &weekly,
                        &ChartConfig::new().min(0.0).max(10.0).with_table(),
                    ))
                    // A 30-point line chart, auto-scaled and configured:
                    (line_chart(&trend))
                    (line_chart_with(&trend, &ChartConfig::new().title("30-day trend")))
                }
            }
        },
        story! {
            "Display",
            "Stat card",
            {
                use autumn_web::widgets::stat_card;

                maud::html! {
                    (stat_card("Subscribers", "1,024", Some(("/subscribers", "View all"))))
                    (stat_card("Open rate", "62%", None))
                }
            }
        },
        story! {
            "Display",
            "Tabs",
            {
                use autumn_web::widgets::tabs;

                let panels = [
                    ("demo-profile", "Profile", maud::html! { p { "Profile settings" } }),
                    ("demo-security", "Security", maud::html! { p { "Security settings" } }),
                    ("demo-billing", "Billing", maud::html! { p { "Billing settings" } }),
                ];
                tabs("settings-tabs", None, &panels)
            }
        },
        story! {
            "Navigation",
            "Nav bar",
            {
                use autumn_web::widgets::{NavBarConfig, NavItem, NavMenu, nav_bar};

                let config = NavBarConfig::new()
                    .brand("Acme", "/")
                    .item(NavItem::link("/", "Home"))
                    .item(NavItem::link("/posts", "Posts"))
                    .item(NavItem::menu(
                        NavMenu::new("More")
                            .link("/about", "About")
                            .plain_link("https://docs.example.com", "Docs"),
                    ))
                    .trailing(NavItem::plain_link("/login", "Sign in"));
                // "/posts" is the current request path, so that link is active.
                nav_bar("/posts", &config)
            }
        },
        story! {
            "Navigation",
            "Nav link",
            {
                use autumn_web::widgets::{NavLinkMatch, nav_link, nav_link_matched};

                maud::html! {
                    nav {
                        // Exact match: active only on "/posts" itself.
                        (nav_link("/posts", "/posts", "Posts"))
                        // Prefix match: "/posts/3/edit" keeps "All posts" active.
                        (nav_link_matched("/posts/3/edit", "/posts", "All posts", NavLinkMatch::Prefix))
                        (nav_link("/posts", "/about", "About"))
                    }
                }
            }
        },
        story! {
            "Marketing",
            "Hero",
            {
                use autumn_web::widgets::{Cta, HeroConfig, hero};

                let config = HeroConfig::new("Welcome to the Blog")
                    .subtitle("Thoughts, tutorials, and stories.")
                    .cta(Cta::primary("New post", "/admin/new"))
                    .cta(Cta::secondary("About", "/about"));
                hero(&config)
            }
        },
        story! {
            "Overlays",
            "Modal",
            {
                use autumn_web::widgets::{
                    ModalConfig, modal, modal_close_button, modal_trigger,
                };

                let body = maud::html! { p { "This action cannot be undone." } };
                let config = ModalConfig::new()
                    .footer(maud::html! {
                        (modal_close_button("Cancel", "delete-confirm", None))
                    })
                    .light_dismiss(true);
                maud::html! {
                    (modal_trigger("Delete post", "delete-confirm", None))
                    (modal("delete-confirm", "Delete this post?", &body, &config))
                }
            }
        },
        story! {
            "Overlays",
            "Confirm action",
            {
                use autumn_web::widgets::{ConfirmActionConfig, confirm_action};

                let config = ConfirmActionConfig::new()
                    .title("Delete this post?")
                    .message(maud::html! { p { "This permanently removes the post." } })
                    .confirm_label("Delete post");
                // The rendered form is live, so this demo points it at a
                // synthetic target (nothing is mounted there — submitting on
                // a served gallery is a 404 no-op) with a fake CSRF token.
                confirm_action(
                    "delete-post",
                    "Delete…",
                    "/_stories/demo/delete-post",
                    http::Method::DELETE,
                    "demo-csrf-token",
                    &config,
                )
            }
        },
        story! {
            "Display",
            "Badge",
            {
                use autumn_web::widgets::{
                    BadgeConfig, BadgeVariant, badge, badge_with, status_tag,
                };

                maud::html! {
                    // Explicit variant, and a deterministic color from a label:
                    (badge("Published", BadgeVariant::Success))
                    (badge("Draft", BadgeVariant::for_label("draft")))
                    // With a tooltip / accessible name for an abbreviated label:
                    (badge_with(
                        "WIP",
                        BadgeVariant::Info,
                        &BadgeConfig::new().title("Work in progress"),
                    ))
                    // Neutral one-liner:
                    (status_tag("Archived"))
                }
            }
        },
        story! {
            "Display",
            "Avatar",
            {
                use autumn_web::widgets::{AvatarConfig, AvatarSize, avatar};

                maud::html! {
                    // With an image (the demo URL is synthetic — a served
                    // gallery has nothing mounted there):
                    (avatar("Ada Lovelace", &AvatarConfig::new()
                        .image("/_stories/demo/ada.png")
                        .size(AvatarSize::Large)))
                    // Deterministic colored-initials fallback, no image:
                    (avatar("Ada Lovelace", &AvatarConfig::new()))
                    (avatar("Grace Hopper", &AvatarConfig::new().size(AvatarSize::Small)))
                }
            }
        },
        story! {
            "Feedback",
            "Alert",
            {
                use autumn_web::form::Changeset;
                use autumn_web::widgets::{
                    AlertConfig, AlertVariant, alert, alert_with, error_summary,
                };

                // A form error summary from an invalid changeset.
                let mut errors = std::collections::HashMap::new();
                errors.insert("email".to_string(), vec!["is invalid".to_string()]);
                let changeset = Changeset::from_errors((), errors);

                maud::html! {
                    (alert(
                        AlertVariant::Info,
                        maud::html! { "No posts yet — create your first one." },
                    ))
                    (alert_with(
                        AlertVariant::Warning,
                        maud::html! { "Your trial ends in 3 days." },
                        &AlertConfig::new().title("Heads up").icon(true).dismissible(true),
                    ))
                    @if let Some(summary) = error_summary(&changeset) {
                        (summary)
                    }
                }
            }
        },
        story! {
            "Feedback",
            "Toast",
            {
                use autumn_web::widgets::{
                    AlertVariant, DEFAULT_TOAST_REGION_ID, toast, toast_in, toast_region,
                };

                maud::html! {
                    // Drop the region once in your base layout:
                    (toast_region(DEFAULT_TOAST_REGION_ID))
                    // Then return toasts from htmx handlers — appended OOB into
                    // the region. `Error` announces assertively, others politely.
                    (toast("Saved successfully", AlertVariant::Success))
                    (toast("Could not save — please retry", AlertVariant::Error))
                    // Target a differently-named region with `toast_in`:
                    (toast_in(DEFAULT_TOAST_REGION_ID, "Heads up: trial ends soon", AlertVariant::Warning))
                }
            }
        },
        story! {
            "Feedback",
            "Infinite feed",
            {
                use autumn_web::widgets::{FeedConfig, FeedMode, feed_page, infinite_feed};

                let items = maud::html! {
                    article class="post" { h3 { "First post" } }
                    article class="post" { h3 { "Second post" } }
                };
                let next = maud::html! {
                    article class="post" { h3 { "Third post" } }
                };
                let config = FeedConfig::new("/posts/feed").mode(FeedMode::Reveal);
                maud::html! {
                    // Initial view: the feed container + an auto-loading sentinel.
                    (infinite_feed(items, Some("eyJpZCI6Mn0"), &config))
                    // The fragment a handler returns for each append (here the
                    // last page, so no further sentinel is emitted):
                    (feed_page(next, None, &FeedConfig::new("/posts/feed").button()))
                }
            }
        },
    ]
}
