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
                tabs("settings-tabs", &panels)
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
                confirm_action(
                    "delete-post",
                    "Delete…",
                    "/posts/42",
                    http::Method::DELETE,
                    "demo-csrf-token",
                    &config,
                )
            }
        },
    ]
}
