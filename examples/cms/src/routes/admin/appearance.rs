//! Appearance — navigation menus and sidebar widgets.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::content;
use crate::models::{NewMenuItem, NewWidget, UpdateWidget};
use crate::repositories::{MenuItemRepository as _, WidgetRepository as _};
use crate::require_capability;
use crate::theme::WidgetKind;

use super::super::site::{Csrf, Repos};
use super::layout;

/// The theme locations a menu can be assigned to.
const LOCATIONS: &[(&str, &str)] = &[("primary", "Primary navigation"), ("footer", "Footer")];

#[derive(Deserialize)]
pub struct MenuForm {
    pub name: String,
    #[serde(default)]
    pub location: String,
}

#[derive(Deserialize)]
pub struct MenuItemForm {
    pub label: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub post_id: String,
    #[serde(default)]
    pub term_id: String,
    #[serde(default)]
    pub position: String,
    /// The item this one nests under, if any. The schema and the renderer both
    /// support a second level; without this field every item was forced to the
    /// root and the multi-level menu the theme draws was unreachable.
    #[serde(default)]
    pub parent_id: String,
}

#[derive(Deserialize)]
pub struct WidgetForm {
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub count: String,
    #[serde(default)]
    pub position: String,
}

/// How many categories the menu-item builder offers, matching the bound on the
/// page selector beside it.
const MENU_TERM_LIMIT: i64 = 200;

/// How many menus one page of the Appearance screen shows.
const MENUS_PER_PAGE: i64 = 20;

/// How many items are rendered per menu.
///
/// A menu is navigation: past a couple of dozen entries it has stopped being
/// one, and the screen should not become unusable because a script filled it.
const MENU_ITEMS_SHOWN: i64 = 100;

#[derive(Debug, Default, serde::Deserialize)]
pub struct AppearanceFilter {
    #[serde(default)]
    pub page: Option<usize>,
}

#[get("/admin/appearance")]
pub async fn show(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Query(filter): Query<AppearanceFilter>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);

    // Bounded and batched. Every menu was loaded and then queried for its items
    // one at a time, so the screen's cost was the number of menus times the size
    // of each — and menus are created through the form on this very page, with
    // no deletion route, so that grows through ordinary use and stays grown.
    let page = i64::try_from(filter.page.unwrap_or(1).clamp(1, 100_000)).unwrap_or(1);
    let (menu_blocks, menu_total) = {
        let mut conn = repos.conn().await?;
        let menus =
            content::menus_page(&mut conn, (page - 1) * MENUS_PER_PAGE, MENUS_PER_PAGE).await?;
        let total = content::menu_count(&mut conn).await?;
        let ids: Vec<i64> = menus.iter().map(|menu| menu.id).collect();
        let mut items = content::menu_items_for(&mut conn, &ids, MENU_ITEMS_SHOWN).await?;
        let blocks: Vec<(crate::models::Menu, Vec<crate::models::MenuItem>)> = menus
            .into_iter()
            .map(|menu| {
                let own = items.remove(&menu.id).unwrap_or_default();
                (menu, own)
            })
            .collect();
        (blocks, total)
    };
    let last_menu_page = ((menu_total + MENUS_PER_PAGE - 1) / MENUS_PER_PAGE).max(1);

    let mut widgets = repos.widgets.find_by_sidebar("primary".to_owned()).await?;
    widgets.sort_by_key(|w| (w.position, w.id));

    let pages = repos.published_posts("page", 200).await?;
    // Bounded like `pages` above, and for the same reason: this is a
    // *create* control — every menu on the screen renders the whole set as
    // `<option>`s, so an unbounded finder here made the Appearance screen the
    // one that broke on a large taxonomy while the taxonomy and authoring
    // screens stayed responsive. There is no current selection to retain: the
    // control always starts at "No category".
    let (categories, category_count) = {
        let mut conn = repos.conn().await?;
        crate::content::terms_page_with_total(&mut conn, "category", 0, MENU_TERM_LIMIT).await?
    };

    let body = html! {
        section class="mb-10" {
            h2 class="text-lg font-semibold mb-3" { "Menus" }
            div class="grid grid-cols-1 lg:grid-cols-3 gap-6" {
                div class="lg:col-span-2 space-y-4" {
                    @for (menu, items) in &menu_blocks {
                        div class="bg-white rounded-lg shadow p-5" {
                            div class="flex items-baseline justify-between mb-3" {
                                h3 class="font-medium" { (menu.name) }
                                span class="text-xs text-gray-400" {
                                    @if menu.location.is_empty() {
                                        "unassigned"
                                    } @else {
                                        (menu.location)
                                    }
                                }
                            }
                            @if i64::try_from(items.len()).unwrap_or(i64::MAX)
                                >= MENU_ITEMS_SHOWN
                            {
                                p class="text-xs text-gray-400 mb-2" {
                                    "Showing the first " (MENU_ITEMS_SHOWN) " items."
                                }
                            }
                            ol class="space-y-1 text-sm mb-4" {
                                @for item in items {
                                    li class="flex items-center justify-between gap-2" {
                                        span {
                                            @if item.parent_id.is_some() {
                                                span class="text-gray-400" { "└ " }
                                            }
                                            (item.label)
                                        }
                                        form method="post"
                                             action=(format!("/admin/appearance/menu-items/{}/delete",
                                                              item.id)) {
                                                                  (csrf.input())
                                            button type="submit"
                                                   class="text-red-700 hover:underline text-xs" {
                                                "Remove"
                                            }
                                        }
                                    }
                                }
                                @if items.is_empty() {
                                    li class="text-gray-400" { "No items yet." }
                                }
                            }
                            form method="post"
                                 action=(format!("/admin/appearance/menus/{}/items", menu.id))
                                 class="grid grid-cols-1 sm:grid-cols-5 gap-2 text-sm \
                                        border-t border-gray-100 pt-3" {
                                            (csrf.input())
                                div class="sm:col-span-2" {
                                    label for=(format!("label-{}", menu.id)) class="sr-only" {
                                        "Label"
                                    }
                                    input #(format!("label-{}", menu.id)) type="text" name="label"
                                          required placeholder="Label"
                                          class="w-full border rounded px-2 py-1.5";
                                }
                                div {
                                    label for=(format!("target-{}", menu.id)) class="sr-only" {
                                        "Page target"
                                    }
                                    select #(format!("target-{}", menu.id)) name="post_id"
                                           class="w-full border rounded px-2 py-1.5" {
                                        option value="" { "No page" }
                                        @for page in &pages {
                                            option value=(page.id) { (page.title) }
                                        }
                                    }
                                }
                                div {
                                    label for=(format!("term-{}", menu.id)) class="sr-only" {
                                        "Category target"
                                    }
                                    select #(format!("term-{}", menu.id)) name="term_id"
                                           class="w-full border rounded px-2 py-1.5" {
                                        option value="" { "No category" }
                                        @for term in &categories {
                                            option value=(term.id) { (term.name) }
                                        }
                                    }
                                    @if category_count > MENU_TERM_LIMIT {
                                        p class="text-xs text-gray-400 mt-1" {
                                            "First " (MENU_TERM_LIMIT) " by name."
                                        }
                                    }
                                }
                                div {
                                    label for=(format!("url-{}", menu.id)) class="sr-only" {
                                        "Custom URL"
                                    }
                                    input #(format!("url-{}", menu.id)) type="text" name="url"
                                          placeholder="/custom-url"
                                          class="w-full border rounded px-2 py-1.5";
                                }
                                // Only the menu's own root items are offered:
                                // the renderer draws two levels, and the
                                // handler refuses a parent from another menu.
                                div class="sm:col-span-2" {
                                    label for=(format!("parent-{}", menu.id)) class="sr-only" {
                                        "Nest under"
                                    }
                                    select #(format!("parent-{}", menu.id)) name="parent_id"
                                           class="w-full border rounded px-2 py-1.5" {
                                        option value="" { "Top level" }
                                        @for item in items.iter().filter(|i| i.parent_id.is_none()) {
                                            option value=(item.id) { "Under " (item.label) }
                                        }
                                    }
                                }
                                div class="sm:col-span-5" {
                                    button type="submit"
                                           class="px-3 py-1.5 border rounded bg-white \
                                                  hover:bg-gray-50" {
                                        "Add item"
                                    }
                                }
                            }
                        }
                    }
                    @if menu_blocks.is_empty() {
                        p class="text-gray-400 bg-white rounded-lg shadow p-8 text-center" {
                            @if page > 1 {
                                "No menus on this page."
                            } @else {
                                "No menus yet. Create one to populate the site navigation."
                            }
                        }
                    }
                    @if last_menu_page > 1 {
                        nav aria-label="Menu pages"
                            class="flex items-center justify-between text-sm" {
                            @if page > 1 {
                                a href=(format!("/admin/appearance?page={}", page - 1))
                                  class="text-indigo-700 hover:underline" { "← Previous" }
                            } @else {
                                span {}
                            }
                            span class="text-gray-500" {
                                "Page " (page) " of " (last_menu_page)
                            }
                            @if page < last_menu_page {
                                a href=(format!("/admin/appearance?page={}", page + 1))
                                  class="text-indigo-700 hover:underline" { "Next →" }
                            } @else {
                                span {}
                            }
                        }
                    }
                }

                form action="/admin/appearance/menus" method="post"
                     class="bg-white rounded-lg shadow p-5 space-y-3 h-fit" {
                         (csrf.input())
                    h3 class="font-semibold text-sm" { "New menu" }
                    div {
                        label for="menu-name" class="block text-sm font-medium mb-1" { "Name" }
                        input #menu-name type="text" name="name" required
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="menu-location" class="block text-sm font-medium mb-1" {
                            "Location"
                        }
                        select #menu-location name="location"
                               class="w-full border rounded px-3 py-2" {
                            option value="" { "(unassigned)" }
                            @for (value, label) in LOCATIONS {
                                option value=(value) { (label) }
                            }
                        }
                    }
                    button type="submit"
                           class="w-full px-4 py-2 bg-indigo-600 text-white rounded \
                                  hover:bg-indigo-700" {
                        "Create menu"
                    }
                }
            }
        }

        section {
            h2 class="text-lg font-semibold mb-3" { "Widgets" }
            p class="text-sm text-gray-500 mb-3" {
                "Widgets fill the sidebar beside your content, in this order."
            }
            div class="grid grid-cols-1 lg:grid-cols-3 gap-6" {
                div class="lg:col-span-2 bg-white rounded-lg shadow divide-y divide-gray-100" {
                    @for widget in &widgets {
                        div class="p-4 flex items-center justify-between gap-3" {
                            div {
                                p class="font-medium text-sm" {
                                    (WidgetKind::parse(&widget.kind)
                                        .map_or("Unknown", WidgetKind::label))
                                }
                                @if !widget.title.trim().is_empty() {
                                    p class="text-xs text-gray-500" { (widget.title) }
                                }
                            }
                            div class="flex items-center gap-3 text-xs" {
                                span class="text-gray-400" { "position " (widget.position) }
                                form method="post"
                                     action=(format!("/admin/appearance/widgets/{}/delete",
                                                      widget.id)) {
                                                          (csrf.input())
                                    button type="submit" class="text-red-700 hover:underline" {
                                        "Remove"
                                    }
                                }
                            }
                        }
                    }
                    @if widgets.is_empty() {
                        p class="p-8 text-center text-gray-400" { "The sidebar is empty." }
                    }
                }

                form action="/admin/appearance/widgets" method="post"
                     class="bg-white rounded-lg shadow p-5 space-y-3 h-fit" {
                         (csrf.input())
                    h3 class="font-semibold text-sm" { "Add widget" }
                    div {
                        label for="widget-kind" class="block text-sm font-medium mb-1" { "Type" }
                        select #widget-kind name="kind" class="w-full border rounded px-3 py-2" {
                            @for kind in WidgetKind::all() {
                                option value=(kind.slug()) { (kind.label()) }
                            }
                        }
                    }
                    div {
                        label for="widget-title" class="block text-sm font-medium mb-1" {
                            "Heading " span class="text-gray-400 font-normal" { "(optional)" }
                        }
                        input #widget-title type="text" name="title"
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="widget-count" class="block text-sm font-medium mb-1" {
                            "Item count " span class="text-gray-400 font-normal" {
                                "(Recent Posts)"
                            }
                        }
                        input #widget-count type="number" name="count" min="1" max="20" value="5"
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="widget-text" class="block text-sm font-medium mb-1" {
                            "Text " span class="text-gray-400 font-normal" {
                                "(Text widget, Markdown)"
                            }
                        }
                        textarea #widget-text name="text" rows="3"
                                 class="w-full border rounded px-3 py-2 text-sm" {}
                    }
                    div {
                        label for="widget-position" class="block text-sm font-medium mb-1" {
                            "Position"
                        }
                        input #widget-position type="number" name="position" value="0"
                              class="w-full border rounded px-3 py-2";
                    }
                    button type="submit"
                           class="w-full px-4 py-2 bg-indigo-600 text-white rounded \
                                  hover:bg-indigo-700" {
                        "Add widget"
                    }
                }
            }
        }
    };

    Ok(layout(&user, &csrf, "/admin/appearance", "Appearance", body).into_response())
}

#[post("/admin/appearance/menus")]
pub async fn create_menu(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Form(form): Form<MenuForm>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    let name = form.name.trim().to_owned();

    // Only one menu can hold a given theme location, so assigning this one
    // clears the previous holder rather than leaving two menus both claiming
    // "primary" and the nav picking whichever the query returned first.
    //
    // Clearing and inserting share one transaction. Separately, a failed insert
    // — a duplicate slug is the easy way to get one — would leave the previous
    // menu detached and the site's navigation simply gone, from a request that
    // reported an error.
    let location = form.location.trim().to_owned();
    let mut conn = repos.conn().await?;
    crate::content::replace_menu_at_location(&mut conn, &name, &location).await?;
    Ok(Redirect::to("/admin/appearance").into_response())
}

#[post("/admin/appearance/menus/{id}/items")]
pub async fn create_menu_item(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(menu_id): Path<i64>,
    Form(form): Form<MenuItemForm>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    let parse = |raw: &str| raw.trim().parse::<i64>().ok().filter(|v| *v > 0);

    // A parent has to be an item of *this* menu and itself a root item. The
    // first stops a crafted request grafting one menu's items onto another's —
    // the renderer walks from the roots of one menu, so a foreign parent makes
    // the item render nowhere. The second holds the tree to the two levels the
    // renderer draws, rather than accepting a depth it would silently drop.
    let parent_id = match parse(&form.parent_id) {
        Some(candidate) => {
            let parent = repos
                .menu_items
                .find_by_id(candidate)
                .await?
                .filter(|item| item.menu_id == menu_id && item.parent_id.is_none())
                .ok_or_else(|| {
                    AutumnError::unprocessable_msg(
                        "A menu item can only nest under a top-level item of the same menu",
                    )
                })?;
            Some(parent.id)
        }
        None => None,
    };

    repos
        .menu_items
        .save(&NewMenuItem {
            menu_id,
            parent_id,
            label: form.label.trim().to_owned(),
            url: form.url.trim().to_owned(),
            post_id: parse(&form.post_id),
            term_id: parse(&form.term_id),
            position: form.position.trim().parse::<i32>().unwrap_or(0),
        })
        .await?;
    Ok(Redirect::to("/admin/appearance").into_response())
}

#[post("/admin/appearance/menu-items/{id}/delete")]
pub async fn delete_menu_item(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(id): Path<i64>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    repos.menu_items.delete_by_id(id).await?;
    Ok(Redirect::to("/admin/appearance").into_response())
}

#[post("/admin/appearance/widgets")]
pub async fn create_widget(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Form(form): Form<WidgetForm>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    let kind = WidgetKind::parse(form.kind.trim())
        .ok_or_else(|| AutumnError::unprocessable_msg("Unknown widget type"))?;

    // Only the settings this kind actually reads are stored, so a Text widget
    // does not carry a stale `count` that a later render might pick up.
    let settings = match kind {
        WidgetKind::RecentPosts => serde_json::json!({
            "count": form.count.trim().parse::<u64>().unwrap_or(5).clamp(1, 20)
        }),
        WidgetKind::Text => serde_json::json!({ "text": form.text }),
        _ => serde_json::json!({}),
    };

    repos
        .widgets
        .save(&NewWidget {
            sidebar: "primary".to_owned(),
            kind: kind.slug().to_owned(),
            title: form.title.trim().to_owned(),
            settings,
            position: form.position.trim().parse::<i32>().unwrap_or(0),
        })
        .await?;
    Ok(Redirect::to("/admin/appearance").into_response())
}

#[post("/admin/appearance/widgets/{id}/delete")]
pub async fn delete_widget(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(id): Path<i64>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    repos.widgets.delete_by_id(id).await?;
    Ok(Redirect::to("/admin/appearance").into_response())
}

#[allow(dead_code)]
fn _type_uses(_: UpdateWidget) {}
