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
//! every public render fn in `src/widgets.rs`, `src/ui/pagination.rs`,
//! `src/flash.rs` and `src/wizard.rs`, and every `src/a11y.rs` primitive, is
//! used by at least one story's source. When you add a widget, add (or extend)
//! a story here and add the new top-level widget's slug to
//! `EXPECTED_STORY_SLUGS` in that test.

use super::{Story, story};

/// Every built-in story: the ungated set, then the feature-gated set.
pub(super) fn all_stories() -> Vec<Story> {
    let mut stories = builtin_stories();
    stories.extend(gated_stories());
    stories
}

/// The ungated story set, in gallery display order.
#[allow(clippy::too_many_lines)]
fn builtin_stories() -> Vec<Story> {
    vec![
        story! {
            "Forms",
            "Active search",
            {
                use autumn_web::widgets::{
                    ActiveSearchConfig, active_search, active_search_empty_state,
                    active_search_input, active_search_results,
                };

                // A real, live-wired demo — unlike most story action URLs,
                // which stay synthetic 404s on purpose (see e.g. the Confirm
                // action or Bulk actions stories): a search box that visibly
                // does nothing while typing reads as broken. The gallery
                // itself serves this path (see `stories::demo_search`).
                let config = ActiveSearchConfig::new("/_stories/demo/search", "#post-search-results")
                    .placeholder("Search posts…");
                maud::html! {
                    // The claim below only holds when htmx is actually on
                    // the page — story_page only loads it under the `htmx`
                    // feature (review follow-up: a maud-only, no-htmx build
                    // still compiles this story, and its hx-* attributes
                    // stay inert without htmx there to process them).
                    @if cfg!(feature = "htmx") {
                        p { em { "Try it — this box is live, searching a small demo post list." } }
                    } @else {
                        p { em { "This box would be live with the htmx feature enabled — hx-* attributes need htmx on the page to do anything." } }
                    }
                    (active_search("post-search", "Search posts", &config))
                    p { em { "For reference — not live — what your handler returns when nothing matches:" } }
                    (active_search_empty_state("No posts matched your search."))
                    // Compose the pieces yourself when you need custom layout
                    // between the input and the results container: this needs
                    // its own config, targeting its own results container —
                    // reusing `config` above would target
                    // "#post-search-results" (the first demo's), leaving this
                    // one empty on a successful response (review follow-up).
                    div {
                        (active_search_input(
                            "post-search-split",
                            "Search posts",
                            &ActiveSearchConfig::new("/_stories/demo/search", "#post-search-split-results")
                                .placeholder("Search posts…"),
                        ))
                        (active_search_results("post-search-split-results"))
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

                // Real, live-wired demo — see the Active search story above
                // for why (and `stories::demo_tag_search` for the handler).
                let config = AutocompleteConfig::new("/_stories/demo/tags/search", "tag_id")
                    .placeholder("Start typing a tag…");
                maud::html! {
                    // See the Active search story above for why this is
                    // conditional (review follow-up).
                    @if cfg!(feature = "htmx") {
                        p { em { "Try it — this box is live, matching against a small demo tag list." } }
                    } @else {
                        p { em { "This box would be live with the htmx feature enabled — hx-* attributes need htmx on the page to do anything." } }
                    }
                    (autocomplete_input("tag-picker", "Tag", &config))
                    p { em { "For reference — not live — the two shapes your handler returns:" } }
                    // One matching option, as your handler would render it. An
                    // option needs a listbox parent; the live box above has one.
                    div role="listbox" aria-label="Example options" {
                        (autocomplete_option("42", "rust"))
                    }
                    // And the empty state when nothing matches:
                    (autocomplete_empty_state("No matching tags."))
                }
            }
        },
        story! {
            "Forms",
            "Field",
            {
                use autumn_web::form::{
                    Changeset, FieldOptions, checkbox_input_with, required_password_input,
                    select_input_with, text_input_with,
                };

                // Field wrapper v2 (#3395): hint, required marker, typed
                // attributes, and the error state.
                let mut errors = std::collections::HashMap::new();
                errors.insert("handle".to_string(), vec!["is taken".to_string()]);
                let changeset = Changeset::from_errors(
                    serde_json::json!({
                        "email": "",
                        "handle": "ada",
                        "password": "",
                        "plan": "pro",
                        "terms": false,
                    }),
                    errors,
                );

                maud::html! {
                    (text_input_with(&changeset, "email", "Email", &FieldOptions::new()
                        .hint("We never share your address.")
                        .required()
                        .autocomplete("email")
                        .inputmode("email")
                        .placeholder("you@example.com")))
                    (text_input_with(&changeset, "handle", "Handle", &FieldOptions::new()
                        .hint("Letters and digits only.")
                        .pattern("[a-z0-9]+")))
                    (required_password_input(&changeset, "password", "Password"))
                    // A disabled control does not submit its value.
                    (select_input_with(
                        &changeset,
                        "plan",
                        "Plan",
                        &[("free", "Free"), ("pro", "Pro")],
                        &FieldOptions::new().disabled().hint("Change your plan in billing."),
                    ))
                    (checkbox_input_with(&changeset, "terms", "I accept the terms", &FieldOptions::new().required()))
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
            "Accordion",
            {
                use autumn_web::widgets::{
                    AccordionConfig, CollapsibleConfig, HeadingLevel, accordion, collapsible,
                    collapsible_with,
                };

                let items = [
                    ("Shipping", maud::html! { p { "Orders ship in two days." } }),
                    ("Returns", maud::html! { p { "Return within 30 days." } }),
                    ("Support", maud::html! { p { "Mail us any time." } }),
                ];
                maud::html! {
                    (accordion(&items, &AccordionConfig::new("demo-faq").level(HeadingLevel::H2).open(0)))
                    (accordion(&items, &AccordionConfig::new("demo-faq-multi").level(HeadingLevel::H2).multiple().open(0).open(1)))
                    (collapsible("Advanced options", &maud::html! { p { "Hidden until opened." } }))
                    (collapsible_with(
                        "Open by default",
                        &maud::html! { p { "Shown on load." } },
                        &CollapsibleConfig::new().open(true),
                    ))
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
            "Navigation",
            "Locale switcher",
            {
                use autumn_web::widgets::{locale_switcher, localized_path};

                maud::html! {
                    p { "Current page: " code { "/posts" } }
                    (locale_switcher(
                        "/posts",
                        "en",
                        &["en".to_owned(), "es".to_owned(), "fr".to_owned()],
                    ))
                    p class="text-xs text-stone-500" {
                        "Each link is built with " code { "localized_path" }
                        " — e.g. " code { (localized_path("/posts", "es")) }
                    }
                }
            }
        },
        story! {
            "Navigation",
            "Theme toggle",
            {
                use autumn_web::ui::theme::Theme;
                use autumn_web::widgets::{BadgeVariant, ThemeToggleConfig, badge, theme_toggle};

                // With autumn-widgets.js the choice applies in place. Without
                // JS the form posts to THEME_PATH; mount ui::theme::router().
                maud::html! {
                    (theme_toggle(&ThemeToggleConfig::new(Theme::System)))
                    div data-theme="dark" class="story-preview" {
                        p { "data-theme=\"dark\" on a subtree: " (badge("Dark", BadgeVariant::Info)) }
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
            "Popover",
            {
                use autumn_web::widgets::{PopoverAlign, PopoverConfig, PopoverSide, popover};

                // `command`/`commandfor` open the panel with no JavaScript.
                // The browser gives light dismiss and Escape.
                maud::html! {
                    (popover(
                        "Help",
                        &maud::html! { p { "Press ? to see the keyboard shortcuts." } },
                        &PopoverConfig::new("help-popover"),
                    ))
                    (popover(
                        "Above, centered",
                        &maud::html! { p { "Opens above the trigger." } },
                        &PopoverConfig::new("tip-popover")
                            .side(PopoverSide::Top)
                            .align(PopoverAlign::Center),
                    ))
                }
            }
        },
        story! {
            "Overlays",
            "Dropdown menu",
            {
                use autumn_web::widgets::{MenuEntry, PopoverConfig, dropdown_menu};
                use http::Method;

                // An APG menu button. Items are links or CSRF forms, so they
                // work with JavaScript off. Only the arrow keys need it.
                dropdown_menu(
                    "Actions",
                    &[
                        MenuEntry::link("Edit", "/posts/1/edit"),
                        MenuEntry::group(
                            "Share",
                            vec![
                                MenuEntry::link("Copy link", "/posts/1/link"),
                                MenuEntry::link("Email", "/posts/1/email"),
                            ],
                        ),
                        MenuEntry::separator(),
                        MenuEntry::action("Delete", "/posts/1", Method::DELETE, "demo-token")
                            .destructive(),
                    ],
                    &PopoverConfig::new("post-actions"),
                )
            }
        },
        story! {
            "Overlays",
            "Modal",
            {
                use autumn_web::widgets::{
                    DialogSize, ModalConfig, modal, modal_close_button, modal_trigger,
                };

                let body = maud::html! { p { "This action cannot be undone." } };
                let config = ModalConfig::new()
                    .footer(maud::html! {
                        (modal_close_button("Cancel", "delete-confirm", None))
                    })
                    .close_button("Close dialog")
                    .light_dismiss(true);
                let large = ModalConfig::new()
                    .size(DialogSize::Lg)
                    .close_button("Close dialog")
                    .light_dismiss(true);
                let large_body = maud::html! { p { "A large dialog has room for a form." } };
                maud::html! {
                    (modal_trigger("Delete post", "delete-confirm", None))
                    (modal("delete-confirm", "Delete this post?", &body, &config))
                    (modal_trigger("Large modal", "large-modal", None))
                    (modal("large-modal", "Large modal", &large_body, &large))
                }
            }
        },
        story! {
            "Overlays",
            "Drawer",
            {
                use autumn_web::widgets::{
                    DialogSize, DrawerConfig, DrawerSide, drawer, drawer_trigger,
                    modal_close_button,
                };

                let body = maud::html! { p { "Filters and details sit at the edge." } };
                let right = DrawerConfig::new()
                    .close_button("Close filters")
                    .footer(maud::html! { (modal_close_button("Done", "filters", None)) });
                let left = DrawerConfig::new()
                    .side(DrawerSide::Left)
                    .size(DialogSize::Sm)
                    .close_button("Close menu");
                let bottom = DrawerConfig::new()
                    .side(DrawerSide::Bottom)
                    .close_button("Close sheet");
                let panel = DrawerConfig::new()
                    .modal(false)
                    .close_button("Close panel");
                maud::html! {
                    (drawer_trigger("Filters", "filters", &right, None))
                    (drawer("filters", "Filters", &body, &right))
                    (drawer_trigger("Menu", "menu-drawer", &left, None))
                    (drawer("menu-drawer", "Menu", &body, &left))
                    (drawer_trigger("Bottom sheet", "bottom-sheet", &bottom, None))
                    (drawer("bottom-sheet", "Bottom sheet", &body, &bottom))
                    (drawer_trigger("Side panel", "side-panel", &panel, None))
                    (drawer("side-panel", "Side panel", &body, &panel))
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
            "Overlays",
            "Tooltip",
            {
                use autumn_web::widgets::{TooltipConfig, TooltipPlacement, tooltip, tooltip_with};

                maud::html! {
                    p {
                        // The closure gets the text's id. Hover or keyboard
                        // focus shows the text. Escape hides it.
                        (tooltip("copy-tip", "Copy the link to the clipboard", |tip| maud::html! {
                            button type="button" aria-describedby=(tip) { "Copy link" }
                        }))
                        " "
                        // A link trigger, with the text below it.
                        (tooltip_with(
                            "docs-tip",
                            "Opens the tooltip guide",
                            &TooltipConfig::new().placement(TooltipPlacement::Bottom),
                            |tip| maud::html! { a href="#docs" aria-describedby=(tip) { "Docs" } },
                        ))
                        " "
                        // An icon-only trigger: the text is its name.
                        (tooltip_with(
                            "delete-tip",
                            "Delete",
                            &TooltipConfig::new().placement(TooltipPlacement::End),
                            |tip| maud::html! {
                                button type="button" aria-labelledby=(tip) {
                                    span aria-hidden="true" { "×" }
                                }
                            },
                        ))
                    }
                }
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
                    // With an image. A data: URI, so the served gallery has
                    // no broken image:
                    (avatar("Ada Lovelace", &AvatarConfig::new()
                        .image("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'%3E%3Crect width='1' height='1' fill='%23c2410c'/%3E%3C/svg%3E")
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
                use std::time::Duration;

                use autumn_web::widgets::{
                    AlertVariant, DEFAULT_TOAST_REGION_ID, ToastConfig, toast, toast_in,
                    toast_region, toast_with,
                };

                maud::html! {
                    // Drop the region once in your base layout. It opens in
                    // the top layer, above a modal <dialog>.
                    (toast_region(DEFAULT_TOAST_REGION_ID))
                    // Return toasts from htmx handlers. htmx appends them OOB.
                    // `Error` announces assertively, others politely. Hover or
                    // focus pauses the timer.
                    (toast("Saved successfully", AlertVariant::Success))
                    // `toast_in` takes the region id. Use it when the page
                    // has more than one region.
                    (toast_in(DEFAULT_TOAST_REGION_ID, "Upload finished", AlertVariant::Info))
                    // Set a longer timer.
                    (toast_with(
                        "Report export started",
                        AlertVariant::Info,
                        &ToastConfig::new().duration(Duration::from_secs(10)),
                    ))
                    // A message the user must act on: no timer.
                    (toast_with(
                        "Could not save — please retry",
                        AlertVariant::Error,
                        &ToastConfig::new().persistent(true),
                    ))
                    // `ToastConfig::region` does the same as `toast_in`.
                    // This toast is also persistent.
                    (toast_with(
                        "Heads up: trial ends soon",
                        AlertVariant::Warning,
                        &ToastConfig::new().region(DEFAULT_TOAST_REGION_ID).persistent(true),
                    ))
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
                // Reveal mode's sentinel fires on scroll-into-view with no
                // click needed (`hx-trigger="revealed, click"`), so this
                // points at a real demo backend rather than the synthetic
                // 404 URLs most other stories use — otherwise the sentinel
                // below would 404-swap the instant this page loads, not on
                // any visitor action. See `stories::demo_infinite_feed`.
                let config = FeedConfig::new("/_stories/demo/posts/feed").mode(FeedMode::Reveal);
                maud::html! {
                    // Initial view: the feed container + an auto-loading sentinel.
                    (infinite_feed(items, Some("eyJpZCI6Mn0"), &config))
                    // The fragment a handler returns for each append (here the
                    // last page, so no further sentinel is emitted):
                    (feed_page(next, None, &FeedConfig::new("/_stories/demo/posts/feed").button()))
                }
            }
        },
        story! {
            "Forms",
            "Transition controls",
            {
                use autumn_web::widgets::transition_controls;

                // A `#[state_machine]` field emits a `(from, to, Option<guard>)`
                // transitions constant and a `can_transition_<field>_to`
                // predicate; pass those two straight through. Here they are
                // inlined so the story is self-contained.
                let transitions: &[(&str, &str, Option<&str>)] = &[
                    ("draft", "published", None),
                    ("published", "archived", None),
                ];
                // Only edges leaving `current` ("draft") render, so this shows
                // the single `draft -> published` action button.
                transition_controls(
                    "/orders/1/transitions/status",
                    "status",
                    "draft",
                    transitions,
                    |_to| true, // |to| order.can_transition_status_to(to)
                    None,
                    None,
                )
            }
        },
        story! {
            "Forms",
            "Translated transition controls",
            {
                use autumn_web::widgets::{TransitionLabels, transition_controls_with_labels};

                // Same shape as "Transition controls", with the group label and
                // one button text overridden. A target state left out of
                // `.buttons(...)` keeps the default "Mark as {state}" text —
                // here that is the untranslated "archived" edge.
                let transitions: &[(&str, &str, Option<&str>)] = &[
                    ("draft", "published", None),
                    ("published", "archived", None),
                ];
                let labels = TransitionLabels::new()
                    .group("Estado del pedido")
                    .buttons(&[("published", "Publicar")]);
                transition_controls_with_labels(
                    "/orders/1/transitions/status",
                    "status",
                    "draft",
                    transitions,
                    |_to| true, // |to| order.can_transition_status_to(to)
                    None,
                    None,
                    &labels,
                )
            }
        },
        story! {
            "Display",
            "Comment thread",
            {
                use autumn_web::widgets::{CommentThread, CommentView, comment_dom_id, comment_thread};

                // The view half of `#[commentable]`. The router hands back a
                // nested thread; this renders it with an inline reply form per
                // node. Plain POST forms, swapped in place by htmx when it is
                // present and a full page load when it is not.
                let thread = vec![
                    CommentView {
                        id: 7,
                        author: "ada".to_owned(),
                        body: "Does this nest?".to_owned(),
                        datetime: Some("2026-08-21T09:30:00Z".to_owned()),
                        timestamp: "2026-08-21 09:30".to_owned(),
                        replies: vec![CommentView {
                            id: 8,
                            author: "grace".to_owned(),
                            body: "It does — up to `max_depth`,\nafter which the reply form stops rendering.".to_owned(),
                            datetime: Some("2026-08-21T09:41:00Z".to_owned()),
                            timestamp: "2026-08-21 09:41".to_owned(),
                            replies: Vec::new(),
                        }],
                    },
                    CommentView {
                        id: 9,
                        author: "linus".to_owned(),
                        body: "A second top-level comment.".to_owned(),
                        datetime: None,
                        timestamp: String::new(),
                        replies: Vec::new(),
                    },
                ];

                let signed_in = CommentThread::new("comments-post-42", "/comments/Post/42")
                    .label("Post comments")
                    .csrf_token("story-token")
                    .return_to("/posts/42");

                // Anything linking to one comment — a notification, an email —
                // needs the id the widget actually renders, so the convention
                // has one definition rather than a copy per caller.
                let deep_link = comment_dom_id("comments-post-42", 8);

                maud::html! {
                    (comment_thread(&signed_in, &thread))

                    // Signed out: the thread still reads, the form is replaced
                    // by a prompt rather than a control that would 401.
                    (comment_thread(
                        &CommentThread::new("comments-post-43", "/comments/Post/43")
                            .label("Signed-out view")
                            .read_only()
                            .sign_in_prompt("Log in to comment."),
                        &thread,
                    ))

                    // Empty, and a rejected submission re-rendered inline —
                    // a bare 422 would be swallowed by htmx.
                    (comment_thread(
                        &CommentThread::new("comments-post-44", "/comments/Post/44")
                            .label("Empty and errored")
                            .empty_text("No comments yet. Start the conversation!")
                            .csrf_token("story-token")
                            .error("Comment cannot be empty."),
                        &[],
                    ))

                    p class="text-sm text-gray-500" {
                        "Deep link to the nested reply: #" (deep_link)
                    }
                }
            }
        },
        story! {
            "Display",
            "Reaction controls",
            {
                use autumn_web::widgets::{ReactionControls, reaction_controls};

                // The view half of `#[votable]`: `react()` returns the new
                // aggregate and the viewer's own reaction, which is all this
                // widget needs. Plain POST forms, upgraded in place by htmx.
                maud::html! {
                    // Sum mode -- signed up/down votes. The viewer has already
                    // upvoted, so the up arrow renders as a pressed toggle.
                    (reaction_controls(
                        &ReactionControls::votes(
                            "votes-42",
                            "/posts/42/upvote",
                            "/posts/42/downvote",
                        )
                        .aggregate(7)
                        .current(Some(1))
                        .label("Post score"),
                    ))
                    // Count mode -- one membership toggle, not yet pressed.
                    (reaction_controls(
                        &ReactionControls::likes("likes-42", "/posts/42/like")
                            .aggregate(3)
                            .label("Likes"),
                    ))
                }
            }
        },
        story! {
            "Display",
            "Bulk actions",
            {
                use autumn_web::widgets::{
                    BulkActionsConfig, Column, DataTableConfig, bulk_actions_form,
                    bulk_actions_toolbar, bulk_select_checkbox, data_table,
                };

                struct Post {
                    id: i64,
                    title: &'static str,
                }
                let rows = vec![
                    Post { id: 1, title: "First post" },
                    Post { id: 2, title: "Second post" },
                ];

                // `action` is the bulk endpoint -- `paths::bulk_delete()` in a
                // scaffolded app. The flow is entirely unscripted, so it works
                // with JavaScript disabled.
                let config = BulkActionsConfig::new("/posts/bulk_delete");

                // The checkbox goes in the row's first cell, so every row can be
                // selected. Each one submits as a repeated `ids=<id>` pair.
                let columns: Vec<Column<Post>> = vec![
                    Column::new("", |row: &Post| {
                        maud::html! { (bulk_select_checkbox(row.id, &config)) }
                    }),
                    Column::new("Title", |row: &Post| maud::html! { (row.title) }),
                ];

                // A second bulk action, driving a hand-rolled list instead of a
                // `data_table`. `bulk_actions_form` already appends a toolbar, so
                // `bulk_actions_toolbar` is called directly only here, where the
                // surrounding <form> is hand-built -- a bare toolbar outside any
                // form would render a submit button that does nothing.
                let archive = BulkActionsConfig::new("/posts/bulk_archive")
                    .submit_label("Archive selected");

                maud::html! {
                    // Page furniture stays OUTSIDE the form -- anything inside is
                    // submitted along with the selection.
                    a href="/posts/new" { "New Post" }
                    (bulk_actions_form(
                        &config,
                        Some("demo-csrf-token"),
                        None,
                        Some("demo-submit-token"),
                        None,
                        maud::html! {
                            (data_table(&rows, &columns, &DataTableConfig::new("No posts yet.")))
                        },
                    ))
                    form method="post" action="/posts/bulk_archive" class="autumn-bulk-form" {
                        // Hand-built forms carry the same hidden fields
                        // `bulk_actions_form` emits, and for the same reason: both
                        // lead the body, ahead of the checkboxes, because
                        // `SubmitTokenLayer` only scans its first chunk.
                        input type="hidden" name="_submit_token" value="demo-archive-token";
                        input type="hidden" name="_csrf" value="demo-csrf-token";
                        ul {
                            @for row in &rows {
                                li {
                                    (bulk_select_checkbox(row.id, &archive))
                                    " " (row.title)
                                }
                            }
                        }
                        (bulk_actions_toolbar(&archive))
                    }
                }
            }
        },
        story! {
            "Navigation",
            "Pagination",
            {
                use autumn_web::pagination::{CursorPage, CursorRequest, Page, PageRequest};
                use autumn_web::ui::pagination::{
                    PagerOptions, cursor_pagination_nav, pagination_nav,
                };

                // Page 5 of 20. The pager keeps the `q` filter on every link.
                let page: Page<u32> = Page::new((41..=50).collect(), 200, &PageRequest::new(5, 10));
                let offset = PagerOptions::new("/posts").query("q=rust");

                // A cursor page has no total: only Previous and Next.
                let request = CursorRequest::new(None, 20);
                let feed: CursorPage<u32> =
                    CursorPage::from_overfetched((1..=21).collect(), &request, |n| *n);
                let cursor = PagerOptions::new("/feed").aria_label("Feed pages");

                maud::html! {
                    (pagination_nav(&page, &offset))
                    (cursor_pagination_nav(&feed, &cursor))
                }
            }
        },
        story! {
            "Primitives",
            "Accessible primitives",
            {
                use autumn_web::a11y::{
                    Button, Checkbox, FileField, Img, Link, MenuItem, RadioGroup, RadioOption,
                    Select, SelectOption, TextArea, TextField,
                };

                // Each primitive needs an accessible name before it renders.
                let icon = maud::html! { span aria-hidden="true" { "✎" } };
                maud::html! {
                    (Img::new(
                        "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'%3E%3Crect width='1' height='1' fill='%23c2410c'/%3E%3C/svg%3E",
                        "Orange square",
                    )
                    .width(32)
                    .height(32))
                    (Img::decorative("data:image/gif;base64,R0lGODlhAQABAAAAACw="))
                    p {
                        (Button::new("Save").submit())
                        " "
                        (Button::icon(icon.clone(), "Edit post"))
                    }
                    p {
                        (Link::new("/posts", "All posts"))
                        " "
                        (Link::icon("/posts/1/edit", icon, "Edit post 1"))
                    }
                    // A menu item needs a `role="menu"` parent. The data-autumn-*
                    // attributes give the menu its arrow keys (autumn-widgets.js).
                    ul role="menu" aria-label="Post actions" data-autumn-roving
                        data-autumn-orientation="vertical" data-autumn-typeahead {
                        li role="none" { (MenuItem::new("Archive")) }
                        li role="none" { (MenuItem::new("View").href("/posts/1")) }
                    }
                    (TextField::new("email").input_type("email").label("Email address").required())
                    (TextArea::new("bio").label("Bio").rows(3))
                    (Select::new("plan").label("Plan")
                        .options([SelectOption::new("free", "Free"), SelectOption::new("pro", "Pro")])
                        .selected_value("pro"))
                    (Checkbox::new("terms").label("I accept the terms"))
                    (FileField::new("avatar").label("Avatar").accept("image/*"))
                    (RadioGroup::new("speed", RadioOption::new("standard", "Standard"))
                        .option(RadioOption::new("express", "Express"))
                        .label("Delivery speed"))
                }
            }
        },
    ]
}

/// Stories for feature-gated modules (#3398). `vec!` cannot hold a
/// `#[cfg]` element, so these are pushed one by one.
fn gated_stories() -> Vec<Story> {
    #[allow(
        unused_mut,
        clippy::vec_init_then_push,
        reason = "each push has its own #[cfg]; vec! cannot hold one"
    )]
    let mut stories = Vec::new();
    #[cfg(feature = "flash")]
    stories.push(story! {
        "Feedback",
        "Flash",
        {
            use autumn_web::flash::{
                FlashLevel, FlashMessage, FlashMessagesConfig, flash_message_divs,
                flash_messages, flash_messages_with,
            };

            let messages = vec![
                FlashMessage { level: FlashLevel::Success, message: "Post saved.".into() },
                FlashMessage { level: FlashLevel::Error, message: "Title is required.".into() },
            ];
            maud::html! {
                // An error or a warning interrupts the screen reader. A success
                // or an info message waits until the screen reader is idle.
                (flash_messages(&messages))
                p { em { "With a no-JS dismiss control (Space or click):" } }
                (flash_messages_with(&messages, &FlashMessagesConfig::new().dismissible(true)))
                p { em { "The markup inside Flash::render:" } }
                (flash_message_divs(&messages[..1]))
            }
        }
    });
    #[cfg(feature = "wizard")]
    stories.push(story! {
        "Forms",
        "Wizard progress",
        {
            use autumn_web::wizard::{StepState, WizardProgressStep, wizard_progress_from_steps};

            // In a handler, `wizard_progress(&ctx, "payment").await` reads the
            // step states from the session. Here they are fixed.
            let step = |name: &str, label: &str, state| WizardProgressStep {
                name: name.into(),
                label: label.into(),
                state,
            };
            wizard_progress_from_steps(&[
                step("shipping", "Shipping", StepState::Completed),
                step("payment", "Payment", StepState::Current),
                step("review", "Review", StepState::Upcoming),
            ])
        }
    });
    stories
}
