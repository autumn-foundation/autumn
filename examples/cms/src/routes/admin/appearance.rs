//! Appearance — navigation menus and sidebar widgets.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::content;
use crate::models::{NewMenuItem, NewWidget, UpdateWidget, User};
use crate::repositories::{MenuItemRepository as _, WidgetRepository as _};
use crate::require_capability;
use crate::theme::WidgetKind;

use super::super::site::{Csrf, Repos};
use super::layout;

/// The longest widget title accepted, matching the cap the `Menu` and
/// `MenuItem` models declare for the same kind of label.
const MAX_WIDGET_TITLE: usize = 200;

/// The bound `Menu::name` declares (`#[validate(length(min = 1, max = 200))]`).
///
/// `replace_menu_at_location` writes the row via a raw `diesel::insert_into`,
/// bypassing the model's generated `validator::Validate` entirely — so
/// nothing enforced this bound before `create_menu` checked it explicitly. An
/// empty (or whitespace-only, which HTML5 `required` does not reject) name
/// slugified to a fallback hash token and was inserted with a blank display
/// name; an overlong one was inserted uncapped.
const MAX_MENU_NAME: usize = 200;

/// The longest body a text widget may carry.
///
/// The widget renders in the sidebar of *every* public page, so its size is
/// paid on every request rather than on the one page it belongs to. Generous
/// enough for a real "about this site" blurb and nowhere near the request
/// limit, which is what the field was otherwise bounded by.
const MAX_WIDGET_TEXT: usize = 10_000;

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
    /// The page of menus the form was rendered on, so a rejected submission
    /// redisplays the same page and the message lands next to the menu that
    /// caused it rather than on a page that may not list that menu at all.
    #[serde(default)]
    pub page: String,
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
pub const MENU_ITEMS_SHOWN: i64 = 100;

#[derive(Debug, Default, serde::Deserialize)]
pub struct AppearanceFilter {
    #[serde(default)]
    pub page: Option<usize>,
}

/// The "New menu" card's field values, carried through a failed submission so
/// the administrator does not have to retype them — same convention as
/// `users.rs`'s `AddUserValues`.
type NewMenuValues<'a> = (&'a str, &'a str);

/// A menu-item or widget submission the Appearance screen could not save,
/// carried back into the card it came from together with the reason.
enum Rejected<'a> {
    MenuItem {
        menu_id: i64,
        form: &'a MenuItemForm,
        message: &'a str,
    },
    Widget {
        form: &'a WidgetForm,
        message: &'a str,
    },
}

/// The message to show for a refused write, or the error itself when it is not
/// one the administrator can act on by resubmitting — the same distinction
/// `create_menu` draws between "fix the form" and "something else broke".
fn actionable(error: AutumnError) -> Result<String, AutumnError> {
    match error.status() {
        StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => Ok(error.to_string()),
        _ => Err(error),
    }
}

#[get("/admin/appearance")]
pub async fn show(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Query(filter): Query<AppearanceFilter>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    let body = appearance_page(&repos, &csrf, &filter, ("", ""), None, None).await?;
    Ok(layout(&user, &csrf, "/admin/appearance", "Appearance", body).into_response())
}

/// Renders the whole Appearance screen — menus, the "New menu" card, and
/// widgets — parameterized by what the "New menu" card should show. `show`
/// calls this with a blank card; `create_menu` calls it again, with the
/// administrator's own submission and failure message, whenever that
/// submission cannot be saved (same pattern as `users.rs`'s `users_page`).
async fn appearance_page(
    repos: &Repos,
    csrf: &Csrf,
    filter: &AppearanceFilter,
    new_menu: NewMenuValues<'_>,
    new_menu_error: Option<&str>,
    rejected: Option<&Rejected<'_>>,
) -> AutumnResult<Markup> {
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

    // The same bound the public sidebar renders under, so what an administrator
    // manages here is what visitors actually see.
    let widgets = repos
        .with_conn(async |conn| {
            content::sidebar_widgets(conn, "primary", crate::routes::site::MAX_SIDEBAR_WIDGETS)
                .await
        })
        .await?;

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
                        @let refused = match rejected {
                            Some(Rejected::MenuItem { menu_id, form, message })
                                if *menu_id == menu.id => Some((*form, *message)),
                            _ => None,
                        };
                        @let kept = refused.map(|(form, _)| form);
                        @let kept_target = |value: &str, field: fn(&MenuItemForm) -> &str| {
                            kept.is_some_and(|form| field(form).trim() == value)
                        };
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
                                input type="hidden" name="page" value=(page);
                                @if let Some((_, message)) = refused {
                                    p class="sm:col-span-5 text-red-700 whitespace-pre-line"
                                      id=(format!("item-error-{}", menu.id)) role="alert" {
                                        (message)
                                    }
                                }
                                div class="sm:col-span-2" {
                                    label for=(format!("label-{}", menu.id)) class="sr-only" {
                                        "Label"
                                    }
                                    input #(format!("label-{}", menu.id)) type="text" name="label"
                                          value=[kept.map(|form| form.label.as_str())]
                                          required placeholder="Label"
                                          maxlength=(crate::hooks::MAX_MENU_ITEM_LABEL)
                                          autofocus[refused.is_some()]
                                          aria-describedby=[refused.map(|_| format!("item-error-{}", menu.id))]
                                          class="w-full border rounded px-2 py-1.5";
                                }
                                div {
                                    label for=(format!("target-{}", menu.id)) class="sr-only" {
                                        "Page target"
                                    }
                                    select #(format!("target-{}", menu.id)) name="post_id"
                                           class="w-full border rounded px-2 py-1.5" {
                                        option value="" { "No page" }
                                        @for target in &pages {
                                            option value=(target.id)
                                                   selected[kept_target(&target.id.to_string(), |f| &f.post_id)] {
                                                (target.title)
                                            }
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
                                            option value=(term.id)
                                                   selected[kept_target(&term.id.to_string(), |f| &f.term_id)] {
                                                (term.name)
                                            }
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
                                          value=[kept.map(|form| form.url.as_str())]
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
                                            option value=(item.id)
                                                   selected[kept_target(&item.id.to_string(), |f| &f.parent_id)] {
                                                "Under " (item.label)
                                            }
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
                    @if let Some(error) = new_menu_error {
                        p class="text-sm text-red-700 whitespace-pre-line" role="alert" {
                            (error)
                        }
                    }
                    div {
                        label for="menu-name" class="block text-sm font-medium mb-1" { "Name" }
                        input #menu-name type="text" name="name" value=(new_menu.0) required
                              maxlength=(MAX_MENU_NAME) class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="menu-location" class="block text-sm font-medium mb-1" {
                            "Location"
                        }
                        select #menu-location name="location"
                               class="w-full border rounded px-3 py-2" {
                            option value="" selected[new_menu.1.is_empty()] { "(unassigned)" }
                            @for (value, label) in LOCATIONS {
                                option value=(value) selected[new_menu.1 == *value] { (label) }
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

        @let refused_widget = match rejected {
            Some(Rejected::Widget { form, message }) => Some((*form, *message)),
            _ => None,
        };
        @let kept_widget = refused_widget.map(|(form, _)| form);
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
                    @if let Some((_, message)) = refused_widget {
                        p class="text-sm text-red-700 whitespace-pre-line" id="widget-error"
                          role="alert" {
                            (message)
                        }
                    }
                    div {
                        label for="widget-kind" class="block text-sm font-medium mb-1" { "Type" }
                        select #widget-kind name="kind" class="w-full border rounded px-3 py-2"
                               autofocus[refused_widget.is_some()]
                               aria-describedby=[refused_widget.map(|_| "widget-error")] {
                            @for kind in WidgetKind::all() {
                                option value=(kind.slug())
                                       selected[kept_widget.is_some_and(|f| f.kind.trim() == kind.slug())] {
                                    (kind.label())
                                }
                            }
                        }
                    }
                    div {
                        label for="widget-title" class="block text-sm font-medium mb-1" {
                            "Heading " span class="text-gray-400 font-normal" { "(optional)" }
                        }
                        input #widget-title type="text" name="title"
                              value=[kept_widget.map(|form| form.title.as_str())]
                              maxlength=(MAX_WIDGET_TITLE)
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="widget-count" class="block text-sm font-medium mb-1" {
                            "Item count " span class="text-gray-400 font-normal" {
                                "(Recent Posts)"
                            }
                        }
                        input #widget-count type="number" name="count" min="1" max="20"
                              value=(kept_widget.map_or("5", |form| form.count.as_str()))
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="widget-text" class="block text-sm font-medium mb-1" {
                            "Text " span class="text-gray-400 font-normal" {
                                "(Text widget, Markdown)"
                            }
                        }
                        textarea #widget-text name="text" rows="3"
                                 maxlength=(MAX_WIDGET_TEXT)
                                 class="w-full border rounded px-3 py-2 text-sm" {
                            (kept_widget.map_or("", |form| form.text.as_str()))
                        }
                    }
                    div {
                        label for="widget-position" class="block text-sm font-medium mb-1" {
                            "Position"
                        }
                        input #widget-position type="number" name="position"
                              value=(kept_widget.map_or("0", |form| form.position.as_str()))
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

    Ok(body)
}

/// Redisplays the Appearance screen at 422 with the "New menu" card filled
/// back in and `message` shown against it, instead of discarding what the
/// administrator typed or falling through to the generic error page — the
/// same pattern `users.rs`'s `redisplay_add_user` uses.
async fn redisplay_new_menu(
    repos: &Repos,
    user: &User,
    csrf: &Csrf,
    new_menu: NewMenuValues<'_>,
    message: &str,
) -> AutumnResult<Response> {
    let body = appearance_page(
        repos,
        csrf,
        &AppearanceFilter::default(),
        new_menu,
        Some(message),
        None,
    )
    .await?;
    Ok((
        StatusCode::UNPROCESSABLE_ENTITY,
        layout(user, csrf, "/admin/appearance", "Appearance", body),
    )
        .into_response())
}

/// Redisplays the Appearance screen at 422 with the rejected menu-item or
/// widget submission filled back in and the reason shown inside its own card,
/// instead of replacing the screen with the generic error page and discarding
/// everything the administrator had typed.
async fn redisplay_rejected(
    repos: &Repos,
    user: &User,
    csrf: &Csrf,
    page: Option<usize>,
    rejected: &Rejected<'_>,
) -> AutumnResult<Response> {
    let body = appearance_page(
        repos,
        csrf,
        &AppearanceFilter { page },
        ("", ""),
        None,
        Some(rejected),
    )
    .await?;
    Ok((
        StatusCode::UNPROCESSABLE_ENTITY,
        layout(user, csrf, "/admin/appearance", "Appearance", body),
    )
        .into_response())
}

#[post("/admin/appearance/menus")]
pub async fn create_menu(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Form(form): Form<MenuForm>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    let name = form.name.trim().to_owned();
    let location = form.location.trim().to_owned();

    // `Menu::name` declares `#[validate(length(min = 1, max = 200))]`, but
    // `replace_menu_at_location` writes the row through a raw
    // `diesel::insert_into` that never runs the model's generated
    // `validator::Validate` — so this was the only place that bound could be
    // enforced at all. An empty (or whitespace-only — HTML5 `required` does
    // not reject that) name previously slugified to a fallback hash token and
    // was inserted with a blank display name, silently, with no feedback.
    if name.is_empty() || name.chars().count() > MAX_MENU_NAME {
        return redisplay_new_menu(
            &repos,
            &user,
            &csrf,
            (&form.name, &form.location),
            &format!("A menu name must be between 1 and {MAX_MENU_NAME} characters"),
        )
        .await;
    }

    // Only one menu can hold a given theme location, so assigning this one
    // clears the previous holder rather than leaving two menus both claiming
    // "primary" and the nav picking whichever the query returned first.
    //
    // Clearing and inserting share one transaction. Separately, a failed insert
    // — a duplicate slug is the easy way to get one — would leave the previous
    // menu detached and the site's navigation simply gone, from a request that
    // reported an error.
    // `with_conn` scopes the checkout to this call, so the connection is
    // returned to the pool before `redisplay_new_menu` below checks any more
    // out — a `let conn = repos.conn().await?` held open across that awaited
    // call would otherwise sit on a pool slot through the whole redisplay,
    // and a small pool serializes every rejected submission behind it.
    let result = repos
        .with_conn(async |conn| {
            crate::content::replace_menu_at_location(conn, &name, &location).await
        })
        .await;
    if let Err(error) = result {
        // A location race (`idx_menus_location`) or the slug allocator's own
        // "too many menus share that name" both land here as a message the
        // administrator can act on by resubmitting — same distinction the
        // Users screen draws between "fix the form" and "something else
        // broke".
        let message = match error.status() {
            StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => error.to_string(),
            _ => return Err(error),
        };
        return redisplay_new_menu(&repos, &user, &csrf, (&form.name, &form.location), &message)
            .await;
    }
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
    let user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    let parse = |raw: &str| raw.trim().parse::<i64>().ok().filter(|v| *v > 0);
    let page = form.page.trim().parse::<usize>().ok();
    let reject = async |message: &str| {
        redisplay_rejected(
            &repos,
            &user,
            &csrf,
            page,
            &Rejected::MenuItem {
                menu_id,
                form: &form,
                message,
            },
        )
        .await
    };

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
                .filter(|item| item.menu_id == menu_id && item.parent_id.is_none());
            match parent {
                Some(parent) => Some(parent.id),
                None => {
                    return reject(
                        "A menu item can only nest under a top-level item of the same menu",
                    )
                    .await;
                }
            }
        }
        None => None,
    };

    // Bounded, because the read is. Both this screen and the navigation show
    // the first `MENU_ITEMS_SHOWN` items, so accepting more produced an item
    // that appears nowhere and has no delete control — reachable only by
    // removing a visible one first.
    let saved = repos
        .with_conn(async |conn| {
            content::insert_menu_item(
                conn,
                NewMenuItem {
                    menu_id,
                    parent_id,
                    label: form.label.trim().to_owned(),
                    url: form.url.trim().to_owned(),
                    post_id: parse(&form.post_id),
                    term_id: parse(&form.term_id),
                    position: form.position.trim().parse::<i32>().unwrap_or(0),
                },
                MENU_ITEMS_SHOWN,
            )
            .await
        })
        .await;
    if let Err(error) = saved {
        return reject(&actionable(error)?).await;
    }
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
    let user = require_capability!(repos, session, csrf, Capability::EditThemeOptions);
    let reject = async |message: &str| {
        redisplay_rejected(
            &repos,
            &user,
            &csrf,
            None,
            &Rejected::Widget {
                form: &form,
                message,
            },
        )
        .await
    };
    let Some(kind) = WidgetKind::parse(form.kind.trim()) else {
        return reject("Unknown widget type").await;
    };

    // Only the settings this kind actually reads are stored, so a Text widget
    // does not carry a stale `count` that a later render might pick up.
    let settings = match kind {
        WidgetKind::RecentPosts => serde_json::json!({
            "count": form.count.trim().parse::<u64>().unwrap_or(5).clamp(1, 20)
        }),
        WidgetKind::Text => serde_json::json!({ "text": form.text }),
        _ => serde_json::json!({}),
    };

    // The model declares no length rule for these, and a sidebar widget is the
    // one piece of content that renders on every page — so an unbounded title
    // or body is paid site-wide, not on the page that carries it. The form's
    // `maxlength` is a browser convenience a crafted POST ignores.
    let title = form.title.trim();
    if title.chars().count() > MAX_WIDGET_TITLE {
        return reject(&format!(
            "A widget title must be at most {MAX_WIDGET_TITLE} characters"
        ))
        .await;
    }
    if form.text.chars().count() > MAX_WIDGET_TEXT {
        return reject(&format!(
            "A text widget must be at most {MAX_WIDGET_TEXT} characters"
        ))
        .await;
    }

    // Bounded, for the same reason `create_menu_item` is: the sidebar renders
    // its first `MAX_SIDEBAR_WIDGETS` and so does this screen, so a widget past
    // that is invisible and undeletable.
    let saved = repos
        .with_conn(async |conn| {
            content::insert_widget(
                conn,
                NewWidget {
                    sidebar: "primary".to_owned(),
                    kind: kind.slug().to_owned(),
                    title: title.to_owned(),
                    settings,
                    position: form.position.trim().parse::<i32>().unwrap_or(0),
                },
                crate::routes::site::MAX_SIDEBAR_WIDGETS,
            )
            .await
        })
        .await;
    if let Err(error) = saved {
        return reject(&actionable(error)?).await;
    }
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
