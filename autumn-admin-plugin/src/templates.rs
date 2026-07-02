//! Maud templates for the admin panel.
//!
//! All templates are server-rendered with HTMX for interactivity.
//! The design mirrors the actuator UI: system-ui font, Tailwind-ish
//! color palette, clean cards with subtle shadows.

use autumn_web::flash::{FlashMessage, flash_message_divs};
use autumn_web::job::{
    JobAdminPage, JobAdminRecord, JobAdminSnapshot, JobAdminStatus, JobScheduleSummary,
};
use autumn_web::pagination::Page;
use autumn_web::runtime_config::{ConfigChangeRecord, ConfigEntry};
use autumn_web::ui::pagination::{PagerOptions, pagination_nav};
use autumn_web::widgets::{
    CardConfig, NavBarConfig, NavBarLayout, NavItem, card, nav_bar, stat_card,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde_json::Value;

use crate::registry::{AdminRegistry, JOBS_NAV_SLUG, RUNTIME_CONFIG_NAV_SLUG};
use crate::routes::ADMIN_JS_PATH;
use crate::traits::{
    AdminAction, AdminField, AdminFieldKind, AdminHistoryPage, AdminImportReport, CsvImportMode,
    ListResult, SortDirection, record_id,
};

const HTMX_JS_PATH: &str = "/static/js/htmx.min.js";
const HTMX_CSRF_JS_PATH: &str = "/static/js/autumn-htmx-csrf.js";
const TOKENS_CSS: &str = include_str!("tokens.css");
const FLASH_CSS: &str = "\
.flash {
    padding: 0.75rem 1rem;
    border-radius: 0.375rem;
    margin-bottom: 1rem;
    font-size: 0.875rem;
}
.flash-success { background: var(--success-light); color: var(--success); border: 1px solid var(--success); }
.flash-error { background: var(--danger-light); color: var(--danger); border: 1px solid var(--danger); }
.flash-warning { background: var(--warning-light); color: var(--warning); border: 1px solid var(--warning); }
.flash-info { background: var(--primary-light); color: var(--primary); border: 1px solid var(--primary); }
";

// ── CSS ─────────────────────────────────────────────────────────────

/// Admin-specific styles that build on the plugin's shared tokens
/// ([`TOKENS_CSS`]) and flash styles ([`FLASH_CSS`]).
const ADMIN_CSS: &str = "
    /* Skip-to-content link: visually hidden at rest, revealed on keyboard focus. */
    .admin-skip-link {
        position: absolute;
        top: -9999px;
        left: 0;
        z-index: 9999;
        padding: 0.5rem 1rem;
        background: var(--primary);
        color: #fff;
        border-radius: 0 0 0.25rem 0.25rem;
        font-size: 0.875rem;
        text-decoration: none;
    }
    .admin-skip-link:focus {
        top: 0;
        outline: 3px solid var(--primary);
        outline-offset: 2px;
    }
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
        font-family: var(--font-family);
        background: var(--bg);
        color: var(--text);
        line-height: 1.5;
    }
    a { color: var(--primary); text-decoration: none; }
    a:hover { text-decoration: underline; }

    /* Layout */
    .admin-layout { display: flex; min-height: 100vh; }
    .admin-sidebar {
        width: 240px;
        background: var(--surface);
        border-right: 1px solid var(--border);
        padding: 1.5rem 0;
        position: fixed;
        top: 0;
        left: 0;
        bottom: 0;
        overflow-y: auto;
    }
    .admin-main {
        margin-left: 240px;
        flex: 1;
        padding: 2rem;
        min-width: 0;
    }
    .admin-sidebar .autumn-nav__brand {
        display: block;
        font-size: 1.125rem;
        font-weight: 700;
        padding: 0 1.5rem 1rem;
        border-bottom: 1px solid var(--border);
        margin-bottom: 1rem;
        color: var(--text);
    }
    .admin-sidebar .autumn-nav__items { list-style: none; }
    .admin-sidebar .autumn-nav__item a {
        display: block;
        padding: 0.5rem 1.5rem;
        color: var(--text-muted);
        font-size: 0.875rem;
        font-weight: 500;
        border-left: 3px solid transparent;
        transition: all 0.15s;
    }
    .admin-sidebar .autumn-nav__item a:hover {
        background: var(--bg);
        color: var(--text);
        text-decoration: none;
    }
    .admin-sidebar .autumn-nav__item a.active {
        background: var(--primary-light);
        color: var(--primary);
        border-left-color: var(--primary);
    }
    .admin-sidebar .autumn-nav__section {
        font-size: 0.7rem;
        text-transform: uppercase;
        letter-spacing: 0.05em;
        color: var(--text-muted);
        padding: 1rem 1.5rem 0.375rem;
        font-weight: 600;
    }
    /* The sidebar hides itself entirely below 768px (see the .admin-sidebar
       rule in the Responsive section) instead of collapsing behind nav_bar's
       own hamburger toggle, so the toggle stays hidden at every width. */
    .admin-sidebar .autumn-nav__toggle {
        display: none;
    }

    /* Cards */
    .card {
        background: var(--surface);
        border-radius: var(--radius);
        box-shadow: var(--shadow);
        margin-bottom: 1.5rem;
    }
    .card-header {
        display: flex;
        justify-content: space-between;
        align-items: center;
        padding: 1rem 1.5rem 0.75rem;
        border-bottom: 1px solid var(--border);
    }
    .card-title {
        font-size: 1.125rem;
        font-weight: 600;
        margin: 0;
    }
    .card-body {
        padding: 1.5rem;
    }

    /* Buttons */
    .btn {
        display: inline-flex;
        align-items: center;
        gap: 0.375rem;
        padding: 0.5rem 1rem;
        border-radius: 0.375rem;
        font-size: 0.875rem;
        font-weight: 500;
        border: 1px solid var(--border);
        background: var(--surface);
        color: var(--text);
        cursor: pointer;
        transition: all 0.15s;
    }
    .btn:hover { background: var(--bg); text-decoration: none; }
    .btn-primary {
        background: var(--primary);
        color: white;
        border-color: var(--primary);
    }
    .btn-primary:hover { background: var(--primary-hover); }
    .btn-danger {
        background: var(--danger);
        color: white;
        border-color: var(--danger);
    }
    .btn-danger:hover { background: var(--danger-hover); }
    .btn-sm { padding: 0.25rem 0.625rem; font-size: 0.8125rem; }

    /* Tables */
    .table-wrap { overflow-x: auto; }
    table {
        width: 100%;
        border-collapse: collapse;
        font-size: 0.875rem;
    }
    th {
        text-align: left;
        padding: 0.75rem;
        font-weight: 600;
        color: var(--text-muted);
        font-size: 0.75rem;
        text-transform: uppercase;
        letter-spacing: 0.05em;
        border-bottom: 2px solid var(--border);
        white-space: nowrap;
        user-select: none;
    }
    th a { cursor: pointer; }
    th a:hover { color: var(--text); }
    th .sort-icon { font-size: 0.625rem; margin-left: 0.25rem; }
    td {
        padding: 0.75rem;
        border-bottom: 1px solid var(--border);
        max-width: 300px;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
    }
    tr:hover td { background: var(--bg); }
    .checkbox-cell { width: 40px; text-align: center; }

    /* Forms */
    .form-group { margin-bottom: 1rem; }
    .form-label {
        display: block;
        font-size: 0.875rem;
        font-weight: 500;
        margin-bottom: 0.375rem;
        color: var(--text);
    }
    .form-label .required { color: var(--danger); margin-left: 0.125rem; }
    .form-input {
        width: 100%;
        padding: 0.5rem 0.75rem;
        border: 1px solid var(--border);
        border-radius: 0.375rem;
        font-size: 0.875rem;
        line-height: 1.5;
        background: var(--surface);
        color: var(--text);
        transition: border-color 0.15s;
    }
    .form-input:focus {
        outline: none;
        border-color: var(--primary);
        box-shadow: 0 0 0 3px var(--primary-light);
    }
    textarea.form-input { min-height: 100px; resize: vertical; }
    select.form-input { appearance: auto; }

    /* Action bar (bulk actions) */
    .action-bar {
        display: flex;
        gap: 0.5rem;
        align-items: center;
        margin-top: 0.75rem;
        padding-top: 0.75rem;
        border-top: 1px solid var(--border);
        font-size: 0.875rem;
        color: var(--text-muted);
    }

    /* Search bar */
    .search-bar {
        display: flex;
        gap: 0.75rem;
        margin-bottom: 1rem;
        align-items: center;
    }
    .search-bar input {
        flex: 1;
        padding: 0.5rem 0.75rem;
        border: 1px solid var(--border);
        border-radius: 0.375rem;
        font-size: 0.875rem;
    }
    .search-bar input:focus {
        outline: none;
        border-color: var(--primary);
        box-shadow: 0 0 0 3px var(--primary-light);
    }

    /* Pagination */
    .pagination {
        display: flex;
        justify-content: space-between;
        align-items: center;
        margin-top: 1rem;
        font-size: 0.875rem;
        color: var(--text-muted);
    }
    .autumn-pager {
        display: flex;
        gap: 0.25rem;
    }
    .autumn-pager a, .autumn-pager span {
        padding: 0.375rem 0.75rem;
        border: 1px solid var(--border);
        border-radius: 0.375rem;
        font-size: 0.8125rem;
        color: var(--text);
    }
    .autumn-pager a:hover { background: var(--bg); text-decoration: none; }
    .autumn-pager .autumn-pager__current {
        background: var(--primary);
        color: white;
        border-color: var(--primary);
    }
    .autumn-pager .autumn-pager__ellipsis { border: none; color: var(--text-muted); }
    .autumn-pager .autumn-pager__disabled { color: var(--text-muted); opacity: 0.5; }

    /* Dashboard stats */
    .stats-grid {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
        gap: 1rem;
        margin-bottom: 1.5rem;
    }
    .stat-card {
        background: var(--surface);
        border-radius: var(--radius);
        box-shadow: var(--shadow);
        padding: 1.25rem;
    }
    .stat-label { font-size: 0.8125rem; color: var(--text-muted); font-weight: 500; }
    .stat-value { font-size: 1.75rem; font-weight: 700; margin-top: 0.25rem; }
    .stat-link { font-size: 0.8125rem; margin-top: 0.375rem; }
    .jobs-counter-grid {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
        gap: 0.75rem;
        margin-bottom: 1rem;
    }
    .jobs-counter {
        border: 1px solid var(--border);
        border-radius: var(--radius);
        padding: 0.875rem;
        background: var(--bg);
    }
    .jobs-counter strong {
        display: block;
        font-size: 1.35rem;
        line-height: 1.1;
        margin-top: 0.2rem;
    }
    .job-error summary {
        cursor: pointer;
        color: var(--danger);
    }
    .job-error pre {
        margin-top: 0.5rem;
        white-space: pre-wrap;
        word-break: break-word;
        background: var(--danger-light);
        border-radius: 0.375rem;
        padding: 0.5rem;
        max-width: 32rem;
    }
    .job-actions {
        display: flex;
        gap: 0.375rem;
        flex-wrap: wrap;
    }
    .job-actions form { display: inline; }

    /* Breadcrumbs */
    .breadcrumbs {
        font-size: 0.875rem;
        color: var(--text-muted);
        margin-bottom: 1rem;
    }
    .breadcrumbs a { color: var(--text-muted); }
    .breadcrumbs a:hover { color: var(--primary); }
    .breadcrumbs .sep { margin: 0 0.5rem; }

    /* Detail view */
    .detail-grid {
        display: grid;
        grid-template-columns: 160px 1fr;
        gap: 0;
    }
    .detail-label {
        padding: 0.75rem;
        font-weight: 500;
        color: var(--text-muted);
        font-size: 0.875rem;
        border-bottom: 1px solid var(--border);
        background: var(--bg);
    }
    .detail-value {
        padding: 0.75rem;
        font-size: 0.875rem;
        border-bottom: 1px solid var(--border);
        word-break: break-word;
    }

    /* Responsive */
    @media (max-width: 768px) {
        .admin-sidebar { display: none; }
        .admin-main { margin-left: 0; }
        .stats-grid { grid-template-columns: 1fr 1fr; }
    }
    ";

// ── Layout ──────────────────────────────────────────────────────────

/// Render the full admin page layout with sidebar navigation.
#[allow(clippy::too_many_arguments)]
pub fn admin_layout(
    registry: &AdminRegistry,
    active_slug: Option<&str>,
    title: &str,
    prefix: &str,
    actuator_prefix: &str,
    csrf_token: &str,
    csrf_token_header: &str,
    messages: &[FlashMessage],
    show_config: bool,
    content: &Markup,
) -> Markup {
    // Each nav item's href is computed once here and reused both to
    // synthesize current_path (so nav_link's own path-comparison logic
    // decides which sidebar item is active) and as the anchor's href below —
    // so the "/jobs" and "/config" route suffixes each appear as a literal
    // exactly once.
    let jobs_href = format!("{prefix}/jobs");
    let config_href = format!("{prefix}/config");
    let current_path = match active_slug {
        None => prefix.to_owned(),
        Some(JOBS_NAV_SLUG) => jobs_href.clone(),
        Some(RUNTIME_CONFIG_NAV_SLUG) => config_href.clone(),
        Some(slug) => format!("{prefix}/{slug}"),
    };

    let mut nav_items = vec![NavItem::link(prefix, "Dashboard")];
    if registry.model_count() > 0 {
        nav_items.push(NavItem::section("Models"));
        nav_items.extend(registry.iter().map(|(slug, model)| {
            NavItem::link(format!("{prefix}/{slug}"), model.display_name_plural())
        }));
    }
    nav_items.push(NavItem::section("System"));
    nav_items.push(NavItem::link(jobs_href, "Jobs"));
    if show_config {
        nav_items.push(NavItem::link(config_href, "Runtime Config"));
    }
    nav_items.push(NavItem::plain_link(
        format!("{actuator_prefix}/ui"),
        "Actuator",
    ));
    let sidebar_nav = NavBarConfig::new()
        .brand_html(html! { "🍂 Autumn Admin" }, None)
        .items(nav_items)
        .aria_label("Admin navigation")
        .layout(NavBarLayout::Sidebar)
        .class("admin-sidebar");

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                // CSRF token for HTMX requests (hx-delete, hx-post). The
                // companion script at HTMX_CSRF_JS_PATH reads this meta tag
                // and attaches the configured header to outgoing htmx requests.
                // The admin JS multipart handler uses data-header to send the
                // right header name when security.csrf.token_header is customised.
                meta name="csrf-token" content=(csrf_token) data-header=(csrf_token_header);
                title { (title) " — Autumn Admin" }
                script src=(HTMX_JS_PATH) {}
                script src=(HTMX_CSRF_JS_PATH) {}
                // Reveals/wires up nav_bar's hamburger toggle and any future
                // dropdown menu; the sidebar itself stays fully visible/hidden
                // via the .admin-sidebar media-query rule below, not the
                // toggle, so its own toggle button is kept CSS-hidden always.
                script src=(autumn_web::htmx::AUTUMN_WIDGETS_JS_PATH) defer {}
                // External so it runs under the default CSP `script-src 'self'`.
                script src={ (prefix) (&**ADMIN_JS_PATH) } {}
                style {
                    (PreEscaped(TOKENS_CSS))
                    (PreEscaped(FLASH_CSS))
                    (PreEscaped(ADMIN_CSS))
                }
            }
            body {
                // Skip-to-content link — first focusable element for keyboard users.
                a href="#admin-main" class="admin-skip-link" { "Skip to main content" }
                div class="admin-layout" {
                    // Sidebar navigation landmark
                    header role="banner" {
                        (nav_bar(&current_path, &sidebar_nav))
                    }
                    // Main content landmark
                    main id="admin-main" class="admin-main" {
                        (flash_message_divs(messages))
                        (content)
                    }
                }
            }
        }
    }
}

// ── Jobs dashboard ──────────────────────────────────────────────────

fn csrf_hidden_input(csrf_token: &str, csrf_form_field: &str) -> Markup {
    html! {
        input type="hidden" name=(csrf_form_field) value=(csrf_token);
    }
}

/// Render the built-in jobs admin dashboard.
#[allow(clippy::too_many_arguments)]
pub fn jobs_page(
    registry: &AdminRegistry,
    snapshot: &JobAdminSnapshot,
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_form_field: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
    show_config: bool,
) -> Markup {
    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            span { "Jobs" }
        }

        h1 style="font-size: 1.5rem; font-weight: 700; margin-bottom: 1rem;" {
            "Jobs"
        }

        (jobs_counters(snapshot, prefix))

        (job_list_card(
            "Enqueued",
            "Work waiting for a worker.",
            &snapshot.enqueued,
            "enqueued_page",
            csrf_token,
            csrf_form_field,
            prefix,
        ))
        (job_list_card(
            "Scheduled",
            "Delayed one-shot jobs waiting for their due time. Cancel before they run.",
            &snapshot.scheduled,
            "scheduled_page",
            csrf_token,
            csrf_form_field,
            prefix,
        ))
        (job_list_card(
            "Running",
            "Work currently executing in this runtime.",
            &snapshot.running,
            "running_page",
            csrf_token,
            csrf_form_field,
            prefix,
        ))
        (job_list_card(
            "Completed (last 24h)",
            "Recently completed work retained by the bounded dashboard history.",
            &snapshot.completed,
            "completed_page",
            csrf_token,
            csrf_form_field,
            prefix,
        ))
        (job_list_card(
            "Failed (last 7d)",
            "Terminal failures available for retry or discard.",
            &snapshot.failed,
            "failed_page",
            csrf_token,
            csrf_form_field,
            prefix,
        ))
        (job_schedules_card(&snapshot.schedules))

        p style="font-size: 0.8125rem; color: var(--text-muted); margin-top: 1rem;" {
            "Default backend history is bounded to " (snapshot.bounded_history_limit)
            " lifecycle entries; counter refreshes use bounded in-memory reads."
        }
    };

    admin_layout(
        registry,
        Some(JOBS_NAV_SLUG),
        "Jobs",
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        show_config,
        &content,
    )
}

/// Render the HTMX-refreshable job counter fragment.
pub fn jobs_counters(snapshot: &JobAdminSnapshot, prefix: &str) -> Markup {
    html! {
        div id="jobs-counters"
            class="jobs-counter-grid"
            hx-get={ (prefix) "/jobs/counters" }
            hx-trigger="load, every 2s"
            hx-swap="outerHTML" {
            (job_counter("Enqueued", snapshot.enqueued.total))
            (job_counter("Scheduled", snapshot.scheduled.total))
            (job_counter("Running", snapshot.running.total))
            (job_counter("Completed 24h", snapshot.completed.total))
            (job_counter("Failed 7d", snapshot.failed.total))
        }
    }
}

fn job_counter(label: &str, value: u64) -> Markup {
    html! {
        div class="jobs-counter" {
            span class="stat-label" { (label) }
            strong { (value) }
        }
    }
}

fn job_list_card(
    title: &str,
    description: &str,
    page: &JobAdminPage,
    page_param: &str,
    csrf_token: &str,
    csrf_form_field: &str,
    prefix: &str,
) -> Markup {
    let header_action = html! {
        span style="font-size: 0.875rem; color: var(--text-muted);" {
            (page.total) " total"
        }
    };
    let title_markup = html! {
        (title)
        span style="display: block; font-size: 0.8125rem; color: var(--text-muted); margin-top: 0.25rem; font-weight: 400;" {
            (description)
        }
    };
    let body = html! {
        div class="table-wrap" {
            table {
                thead {
                    tr {
                        th { "Job" }
                        th { "Enqueued At" }
                        th { "Started At" }
                        th { "Finished At" }
                        th { "Attempts" }
                        th { "Principal" }
                        th { "Correlation" }
                        th { "Last Error" }
                        th { "Actions" }
                    }
                }
                tbody {
                    @if page.records.is_empty() {
                        tr {
                            td colspan="9" style="text-align: center; padding: 1.5rem; color: var(--text-muted);" {
                                "No jobs."
                            }
                        }
                    }
                    @for record in &page.records {
                        (job_row(record, csrf_token, csrf_form_field, prefix))
                    }
                }
            }
        }
        (jobs_pagination(page, page_param, prefix))
    };
    card(
        &body,
        &CardConfig::new()
            .title_html(title_markup)
            .header_action(header_action),
    )
}

fn job_row(
    record: &JobAdminRecord,
    csrf_token: &str,
    csrf_form_field: &str,
    prefix: &str,
) -> Markup {
    html! {
        tr {
            td {
                strong { (record.name) }
                div style="font-size: 0.75rem; color: var(--text-muted);" {
                    (record.status.label()) " · queue " (record.queue) " · " (record.id)
                    @if let Some(due) = record.scheduled_for.as_deref() {
                        " · due " (due)
                    }
                }
            }
            td { (optional_text(record.enqueued_at.as_deref())) }
            td { (optional_text(record.started_at.as_deref())) }
            td { (optional_text(record.finished_at.as_deref())) }
            td { (record.attempt) "/" (record.max_attempts) }
            td { (optional_text(record.principal_id.as_deref())) }
            td { (optional_text(record.correlation_id.as_deref())) }
            td { (job_error(record)) }
            td { (job_actions(record, csrf_token, csrf_form_field, prefix)) }
        }
    }
}

fn job_error(record: &JobAdminRecord) -> Markup {
    let Some(error) = record.last_error.as_deref() else {
        return html! { span style="color: var(--text-muted);" { "—" } };
    };
    if record.status == JobAdminStatus::Failed {
        html! {
            details class="job-error" {
                summary { (truncate_display(error, 80)) }
                pre { (error) }
            }
        }
    } else {
        html! { (truncate_display(error, 80)) }
    }
}

fn job_actions(
    record: &JobAdminRecord,
    csrf_token: &str,
    csrf_form_field: &str,
    prefix: &str,
) -> Markup {
    html! {
        div class="job-actions" {
            @if record.status == JobAdminStatus::Failed {
                (job_action_form(prefix, &record.id, "retry", "Retry", "btn btn-sm btn-primary", csrf_token, csrf_form_field))
                (job_action_form(prefix, &record.id, "discard", "Discard", "btn btn-sm btn-danger", csrf_token, csrf_form_field))
            } @else if record.status == JobAdminStatus::Enqueued || record.status == JobAdminStatus::Scheduled {
                (job_action_form(prefix, &record.id, "cancel", "Cancel", "btn btn-sm btn-danger", csrf_token, csrf_form_field))
            } @else {
                span style="color: var(--text-muted);" { "—" }
            }
        }
    }
}

fn job_action_form(
    prefix: &str,
    id: &str,
    action: &str,
    label: &str,
    class_name: &str,
    csrf_token: &str,
    csrf_form_field: &str,
) -> Markup {
    html! {
        form method="post" action={ (prefix) "/jobs/" (id) "/" (action) } {
            (csrf_hidden_input(csrf_token, csrf_form_field))
            button type="submit" class=(class_name) {
                (label)
            }
        }
    }
}

fn jobs_pagination(page: &JobAdminPage, page_param: &str, prefix: &str) -> Markup {
    if page.total_pages() <= 1 {
        return html! {};
    }
    let meta = page_meta(page.page, page.per_page, page.total, page.total_pages());
    let base = format!("{prefix}/jobs");
    let opts = PagerOptions::new(&base).page_param(page_param).window(1);
    html! {
        div class="pagination" {
            div {
                "Page " (page.page) " of " (page.total_pages())
            }
            (pagination_nav(&meta, &opts))
        }
    }
}

fn job_schedules_card(schedules: &[JobScheduleSummary]) -> Markup {
    let body = html! {
        div class="table-wrap" {
            table {
                thead {
                    tr {
                        th { "Name" }
                        th { "Schedule" }
                        th { "Next Run At" }
                        th { "Last Run Status" }
                    }
                }
                tbody {
                    @if schedules.is_empty() {
                        tr {
                            td colspan="4" style="text-align: center; padding: 1.5rem; color: var(--text-muted);" {
                                "No scheduled tasks registered."
                            }
                        }
                    }
                    @for schedule in schedules {
                        tr {
                            td { (schedule.name) }
                            td { (schedule.schedule) }
                            td { (optional_text(schedule.next_run_at.as_deref())) }
                            td { (optional_text(schedule.last_run_status.as_deref())) }
                        }
                    }
                }
            }
        }
    };
    card(&body, &CardConfig::new().title("Recurring Schedules"))
}

fn optional_text(value: Option<&str>) -> Markup {
    value.filter(|value| !value.is_empty()).map_or_else(
        || html! { span style="color: var(--text-muted);" { "—" } },
        |value| html! { (value) },
    )
}

// ── Dashboard ───────────────────────────────────────────────────────

/// Render the admin dashboard with model counts.
#[allow(clippy::too_many_arguments)]
pub fn dashboard_page(
    registry: &AdminRegistry,
    model_counts: &[(&str, &str, u64)], // (slug, display_name_plural, count)
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
    show_config: bool,
) -> Markup {
    let content = html! {
        h1 style="font-size: 1.5rem; font-weight: 700; margin-bottom: 1.5rem;" {
            "Dashboard"
        }

        div class="stats-grid" {
            @for (slug, name, count) in model_counts {
                (stat_card(name, &count.to_string(), Some((&format!("{prefix}/{slug}"), "View all →"))))
            }
        }

        // Actuator summary (loaded via HTMX)
        ({
            let action = html! {
                a href={ (actuator_prefix) "/ui" } class="btn btn-sm" { "Full Dashboard →" }
            };
            let body = html! {
                div hx-get={ (actuator_prefix) "/ui/metrics" } hx-trigger="load, every 5s" {
                    "Loading metrics…"
                }
            };
            card(&body, &CardConfig::new().title("System Health").header_action(action))
        })
    };
    admin_layout(
        registry,
        None,
        "Dashboard",
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        show_config,
        &content,
    )
}

// ── Model list view ─────────────────────────────────────────────────

/// Render the list view for a model (table + search + pagination).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn model_list_page(
    registry: &AdminRegistry,
    model_slug: &str,
    model_name_plural: &str,
    fields: &[AdminField],
    actions: &[AdminAction],
    result: &ListResult,
    search_query: &str,
    sort_by: Option<&str>,
    sort_dir: SortDirection,
    // Active filters (already validated by the handler). Carried into
    // every generated sort/pagination URL so navigation doesn't silently
    // revert to unfiltered results.
    filters: &[(String, String)],
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_form_field: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
    show_config: bool,
    supports_csv_export: bool,
    supports_csv_import: bool,
) -> Markup {
    // Password fields are documented as write-only — never surface their
    // values (raw or hashed) in the index view. Hidden fields are
    // documented as "shown in detail, not editable" — so they should
    // also stay out of the list view, regardless of `list_display`.
    let list_fields: Vec<_> = fields
        .iter()
        .filter(|f| {
            f.list_display && !matches!(f.kind, AdminFieldKind::Password | AdminFieldKind::Hidden)
        })
        .collect();
    let search_enc = url_encode(search_query);
    // Pre-encode active filters into a `&filter.<k>=<v>` suffix so
    // sort/pagination links carry filter state forward without rebuilding it.
    let filters_enc = encode_filter_suffix(filters);
    // Export URL preserves the current search/sort/filter state so "Download CSV"
    // exports exactly the rows shown on the page, not the whole table.
    let export_csv_url = {
        let mut params: Vec<String> = Vec::new();
        if !search_enc.is_empty() {
            params.push(format!("q={search_enc}"));
        }
        if let Some(sort) = sort_by {
            params.push(format!("sort={}", url_encode(sort)));
            params.push(format!("dir={}", sort_dir.as_str()));
        }
        for (k, v) in filters {
            params.push(format!("filter.{}={}", url_encode(k), url_encode(v)));
        }
        if params.is_empty() {
            format!("{prefix}/{model_slug}/export.csv")
        } else {
            format!("{prefix}/{model_slug}/export.csv?{}", params.join("&"))
        }
    };

    let content = html! {
        // Breadcrumbs
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            span { (model_name_plural) }
        }

        ({
            let title = html! {
                (model_name_plural)
                span style="font-weight: 400; color: var(--text-muted); margin-left: 0.5rem;" {
                    "(" (result.total) ")"
                }
            };
            let header_action = html! {
                div style="display: flex; gap: 0.5rem; align-items: center;" {
                    @if supports_csv_export {
                        a href=(export_csv_url) class="btn btn-sm"
                            title="Download all matching records as CSV" {
                            "⬇ Download CSV"
                        }
                    }
                    @if supports_csv_import {
                        a href={ (prefix) "/" (model_slug) "/import" } class="btn btn-sm"
                            title="Upload a CSV file to import records" {
                            "⬆ Import CSV"
                        }
                    }
                    a href={ (prefix) "/" (model_slug) "/new" } class="btn btn-primary" {
                        "+ Add " (model_slug.trim_end_matches('s'))
                    }
                }
            };
            let body = html! {
                // Search. Hidden inputs preserve any active filters so both
                // full-form GET submits AND live-search HTMX requests carry
                // the filter set forward (htmx only includes the triggering
                // element by default — `hx-include="closest form"` pulls in
                // every input in the form, including the filter hiddens).
                form class="search-bar" method="get" {
                    input type="search" name="q" placeholder="Search…"
                        value=(search_query)
                        hx-get={ (prefix) "/" (model_slug) }
                        hx-trigger="input changed delay:300ms"
                        hx-include="closest form"
                        hx-target="closest .card"
                        hx-select=".card > *"
                        hx-push-url="true" {}
                    @for (k, v) in filters {
                        input type="hidden" name={ "filter." (k) } value=(v);
                    }
                }

                // Bulk-action form wraps the table so the row checkboxes
                // submit alongside the action selector.
                form method="post" action={ (prefix) "/" (model_slug) "/actions" } {
                    (csrf_hidden_input(csrf_token, csrf_form_field))

                // Table
                div class="table-wrap" {
                    table {
                        thead {
                            tr {
                                th class="checkbox-cell" {
                                    // Wired up by admin.js via event delegation on #select-all.
                                    input type="checkbox" id="select-all";
                                }
                                @for field in &list_fields {
                                    @let is_sorted = sort_by == Some(field.name);
                                    @let next_dir = if is_sorted { sort_dir.flipped() } else { SortDirection::Asc };
                                    th {
                                        @if field.sortable {
                                            a href={ (prefix) "/" (model_slug) "?sort=" (field.name) "&dir=" (next_dir.as_str())
                                                @if !search_enc.is_empty() { "&q=" (search_enc) }
                                                (filters_enc)
                                            }
                                            style="color: inherit; text-decoration: none;" {
                                                (field.label)
                                                @if is_sorted {
                                                    span class="sort-icon" {
                                                        @if matches!(sort_dir, SortDirection::Asc) { "▲" } @else { "▼" }
                                                    }
                                                }
                                            }
                                        } @else {
                                            (field.label)
                                        }
                                    }
                                }
                                th { "Actions" }
                            }
                        }
                        tbody {
                            @if result.records.is_empty() {
                                tr {
                                    td colspan=(list_fields.len() + 2)
                                        style="text-align: center; padding: 2rem; color: var(--text-muted);" {
                                        "No records found."
                                    }
                                }
                            }
                            @for record in &result.records {
                                @let row_id = record_id(record);
                                tr {
                                    td class="checkbox-cell" {
                                        // Only emit a bulk-action checkbox for rows with a
                                        // routable id — otherwise the form would post id="" or
                                        // the wrong record.
                                        @if let Some(id) = row_id {
                                            input type="checkbox" class="row-check"
                                                name="ids" value=(id);
                                        }
                                    }
                                    @for field in &list_fields {
                                        td { (render_cell_value(record, field)) }
                                    }
                                    td {
                                        @if let Some(id) = row_id {
                                            a href={ (prefix) "/" (model_slug) "/" (id) }
                                                class="btn btn-sm" { "View" }
                                            " "
                                            a href={ (prefix) "/" (model_slug) "/" (id) "/edit" }
                                                class="btn btn-sm" { "Edit" }
                                        } @else {
                                            // Surface the issue rather than rendering links to /0.
                                            span style="color: var(--text-muted); font-size: 0.75rem;" {
                                                "no id"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                    // Bulk-action bar — only rendered when the model declares
                    // at least one action. Sits below the table inside the
                    // wrapping form.
                    @if !actions.is_empty() {
                        div class="action-bar" {
                            label for="bulk-action" { "With selected:" }
                            select name="action" id="bulk-action" class="form-input"
                                style="width: auto; display: inline-block;" {
                                @for a in actions {
                                    option value=(a.name) data-confirm=[a.confirm.then_some("1")] {
                                        (a.label)
                                    }
                                }
                            }
                            button type="submit" class="btn" data-bulk-submit="1" {
                                "Apply"
                            }
                        }
                    }

                } // /form

                // Pagination
                @if result.total_pages() > 1 {
                    (render_pagination(result, model_slug, &search_enc, sort_by, sort_dir, &filters_enc, prefix))
                }
            };
            card(&body, &CardConfig::new().title_html(title).header_action(header_action))
        })
    };
    admin_layout(
        registry,
        Some(model_slug),
        model_name_plural,
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        show_config,
        &content,
    )
}

// ── CSV import form ──────────────────────────────────────────────────

/// Render the CSV import upload form.
#[allow(clippy::too_many_arguments)]
pub fn model_import_form_page(
    registry: &AdminRegistry,
    model_slug: &str,
    model_name_plural: &str,
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_form_field: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
    show_config: bool,
) -> Markup {
    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            a href={ (prefix) "/" (model_slug) } { (model_name_plural) }
            span class="sep" { "›" }
            span { "Import CSV" }
        }

        ({
            let title = html! { "Import " (model_name_plural) " from CSV" };
            let body = html! {
                p style="color: var(--text-muted); margin-bottom: 1rem;" {
                    "Upload a CSV file with a header row. Column names must match the model's field names."
                }

                form id="autumn-csv-import-form"
                    method="post"
                    action={ (prefix) "/" (model_slug) "/import?" (csrf_form_field) "=" (csrf_token) }
                    enctype="multipart/form-data" {

                    (csrf_hidden_input(csrf_token, csrf_form_field))

                    div style="margin-bottom: 1rem;" {
                        label for="csv-file" style="display: block; margin-bottom: 0.25rem; font-weight: 500;" {
                            "CSV File"
                        }
                        input type="file" id="csv-file" name="file"
                            accept=".csv,text/csv"
                            required
                            class="form-input" {}
                    }

                    div style="margin-bottom: 1.5rem;" {
                        label for="import-mode" style="display: block; margin-bottom: 0.25rem; font-weight: 500;" {
                            "Import Mode"
                        }
                        select id="import-mode" name="mode" class="form-input"
                            style="width: auto; display: inline-block;" {
                            option value="insert" selected { "Insert (add as new records)" }
                            option value="dry_run" { "Dry Run (validate only, no writes)" }
                        }
                    }

                    div style="display: flex; gap: 0.75rem; align-items: center;" {
                        button type="submit" class="btn btn-primary" { "Upload and Import" }
                        a href={ (prefix) "/" (model_slug) } class="btn" { "Cancel" }
                    }
                }

                div style="margin-top: 2rem; padding-top: 1rem; border-top: 1px solid var(--border);" {
                    h3 style="font-size: 0.875rem; font-weight: 600; margin-bottom: 0.5rem;" {
                        "Tips"
                    }
                    ul style="color: var(--text-muted); font-size: 0.875rem; padding-left: 1.25rem;" {
                        li { "The first row must be a header row with column names." }
                        li { "Column names must match the model's field names." }
                        li { "Use Dry Run to preview the import and catch errors before writing." }
                        li {
                            "Download a template: "
                            a href={ (prefix) "/" (model_slug) "/export.csv" } { "export.csv" }
                        }
                    }
                }
            };
            card(&body, &CardConfig::new().title_html(title))
        })
    };

    admin_layout(
        registry,
        Some(model_slug),
        model_name_plural,
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        show_config,
        &content,
    )
}

/// Render the result page after a CSV import.
#[allow(clippy::too_many_arguments)]
pub fn model_import_result_page(
    registry: &AdminRegistry,
    model_slug: &str,
    model_name_plural: &str,
    report: &AdminImportReport,
    mode: CsvImportMode,
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
    show_config: bool,
) -> Markup {
    let mode_label = match mode {
        CsvImportMode::DryRun => "Dry Run",
        CsvImportMode::Insert => "Insert",
    };
    let total = report.inserted + report.updated + report.skipped + report.errors.len() as u64;

    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            a href={ (prefix) "/" (model_slug) } { (model_name_plural) }
            span class="sep" { "›" }
            span { "Import Result" }
        }

        ({
            let title = html! { "Import Report — " (mode_label) };
            let body = html! {
                div style="display: grid; grid-template-columns: repeat(4, 1fr); gap: 1rem; margin-bottom: 1.5rem;" {
                    div style="text-align: center; padding: 1rem; background: var(--success-light); border-radius: 0.375rem;" {
                        div style="font-size: 1.5rem; font-weight: 700; color: var(--success);" { (report.inserted) }
                        div style="font-size: 0.75rem; color: var(--text-muted);" { "Inserted" }
                    }
                    div style="text-align: center; padding: 1rem; background: var(--primary-light); border-radius: 0.375rem;" {
                        div style="font-size: 1.5rem; font-weight: 700; color: var(--primary);" { (report.updated) }
                        div style="font-size: 0.75rem; color: var(--text-muted);" { "Updated" }
                    }
                    div style="text-align: center; padding: 1rem; background: var(--border); border-radius: 0.375rem;" {
                        div style="font-size: 1.5rem; font-weight: 700;" { (report.skipped) }
                        div style="font-size: 0.75rem; color: var(--text-muted);" { "Skipped" }
                    }
                    div style="text-align: center; padding: 1rem; background: var(--danger-light); border-radius: 0.375rem;" {
                        div style="font-size: 1.5rem; font-weight: 700; color: var(--danger);" { (report.errors.len()) }
                        div style="font-size: 0.75rem; color: var(--text-muted);" { "Errors" }
                    }
                }

                p style="color: var(--text-muted); font-size: 0.875rem; margin-bottom: 1.5rem;" {
                    "Processed " (total) " data rows."
                    @if matches!(mode, CsvImportMode::DryRun) {
                        " (Dry run — no records were written.)"
                    }
                }

                @if !report.errors.is_empty() {
                    h3 style="font-size: 0.875rem; font-weight: 600; margin-bottom: 0.75rem; color: var(--danger);" {
                        "Row Errors"
                    }
                    div class="table-wrap" {
                        table {
                            thead {
                                tr {
                                    th { "Line" }
                                    th { "Column" }
                                    th { "Message" }
                                }
                            }
                            tbody {
                                @for err in &report.errors {
                                    tr {
                                        td { (err.line) }
                                        td {
                                            @if let Some(col) = &err.column {
                                                code { (col) }
                                            } @else {
                                                span style="color: var(--text-muted);" { "—" }
                                            }
                                        }
                                        td { (err.message) }
                                    }
                                }
                            }
                        }
                    }
                }

                div style="display: flex; gap: 0.75rem; margin-top: 1.5rem;" {
                    a href={ (prefix) "/" (model_slug) } class="btn btn-primary" { "Back to list" }
                    a href={ (prefix) "/" (model_slug) "/import" } class="btn" { "Import another file" }
                }
            };
            card(&body, &CardConfig::new().title_html(title))
        })
    };

    admin_layout(
        registry,
        Some(model_slug),
        model_name_plural,
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        show_config,
        &content,
    )
}

// ── Detail view ─────────────────────────────────────────────────────

/// Render the detail view for a single record.
#[allow(clippy::too_many_arguments)]
pub fn model_detail_page(
    registry: &AdminRegistry,
    model_slug: &str,
    model_name: &str,
    model_name_plural: &str,
    fields: &[AdminField],
    record: &Value,
    record_display: &str,
    // Path-based ID from the handler. Authoritative — edit/delete links
    // must route to the same record the URL addressed, not whatever ID
    // happens to appear in the JSON payload.
    id: i64,
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
    has_history: bool,
    show_config: bool,
) -> Markup {
    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            a href={ (prefix) "/" (model_slug) } { (model_name_plural) }
            span class="sep" { "›" }
            span { (record_display) }
        }

        ({
            let header_action = html! {
                div {
                    @if has_history {
                        a href={ (prefix) "/" (model_slug) "/" (id) "/history" }
                            class="btn btn-secondary" { "History" }
                        " "
                    }
                    a href={ (prefix) "/" (model_slug) "/" (id) "/edit" }
                        class="btn btn-primary" { "Edit" }
                    " "
                    button class="btn btn-danger"
                        hx-delete={ (prefix) "/" (model_slug) "/" (id) }
                        hx-confirm={ "Are you sure you want to delete this " (model_name) "?" }
                        hx-target="body" {
                        "Delete"
                    }
                }
            };
            let body = html! {
                div class="detail-grid" {
                    @for field in fields {
                        div class="detail-label" { (field.label) }
                        div class="detail-value" {
                            (render_detail_value(record, field))
                        }
                    }
                }
            };
            card(&body, &CardConfig::new().title(record_display).header_action(header_action))
        })
    };
    admin_layout(
        registry,
        Some(model_slug),
        record_display,
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        show_config,
        &content,
    )
}

// ── Create / Edit form ──────────────────────────────────────────────

/// Render the create or edit form for a model.
#[allow(clippy::too_many_arguments)]
pub fn model_form_page(
    registry: &AdminRegistry,
    model_slug: &str,
    model_name: &str,
    model_name_plural: &str,
    fields: &[AdminField],
    record: Option<&Value>,
    // Path-based ID from the handler on edit pages (`None` when rendering
    // the "new" form). Never trust the JSON payload for mutation routing.
    id: Option<i64>,
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_form_field: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
    show_config: bool,
) -> Markup {
    let is_edit = id.is_some();
    let title = if is_edit {
        format!("Edit {model_name}")
    } else {
        format!("New {model_name}")
    };

    let editable_fields: Vec<_> = fields.iter().filter(|f| f.editable).collect();

    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            a href={ (prefix) "/" (model_slug) } { (model_name_plural) }
            span class="sep" { "›" }
            span { (title) }
        }

        ({
            let body = html! {
                form method="post"
                    action={
                        @if let Some(id) = id {
                            (prefix) "/" (model_slug) "/" (id)
                        } @else {
                            (prefix) "/" (model_slug)
                        }
                    } {
                    (csrf_hidden_input(csrf_token, csrf_form_field))

                    @for field in &editable_fields {
                        div class="form-group" {
                            label class="form-label" for=(field.name) {
                                (field.label)
                                @if field.required && !(is_edit && field.create_only) {
                                    span class="required" { "*" }
                                }
                            }
                            @if is_edit && field.create_only {
                                // Immutable-after-create: show current value as read-only text
                                // so the admin can see it but cannot change it.
                                (render_readonly_display(field, record))
                            } @else {
                                (render_form_widget(field, record))
                            }
                        }
                    }

                    div style="display: flex; gap: 0.75rem; margin-top: 1.5rem;" {
                        button type="submit" class="btn btn-primary" {
                            @if is_edit { "Save Changes" } @else { "Create" }
                        }
                        a href={ (prefix) "/" (model_slug) } class="btn" {
                            "Cancel"
                        }
                    }
                }
            };
            card(&body, &CardConfig::new().title(&title))
        })
    };
    admin_layout(
        registry,
        Some(model_slug),
        &title,
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        show_config,
        &content,
    )
}

// ── Runtime config page ─────────────────────────────────────────────

/// Render the runtime config management page.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn config_page(
    registry: &AdminRegistry,
    entries: &[ConfigEntry],
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_form_field: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
) -> Markup {
    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            span { "Runtime Config" }
        }

        h1 style="font-size: 1.5rem; font-weight: 700; margin-bottom: 0.5rem;" {
            "Runtime Config"
        }
        p style="color: var(--text-muted); margin-bottom: 1.5rem; font-size: 0.875rem;" {
            "Live-tunable operational knobs. Changes take effect immediately without a restart."
        }

        @if entries.is_empty() {
            (card(&html! {
                p style="color: var(--text-muted); padding: 1rem;" {
                    "No config keys have been registered. Declare keys with "
                    code { "ConfigRegistry::define" }
                    " and pass the service via "
                    code { "AdminPlugin::with_runtime_config" }
                    "."
                }
            }, &CardConfig::new()))
        } @else {
            (card(&html! {
                table class="table" {
                    thead {
                        tr {
                            th { "Key" }
                            th { "Type" }
                            th { "Current Value" }
                            th { "Default" }
                            th { "Status" }
                            th { "Actions" }
                        }
                    }
                    tbody {
                        @for entry in entries {
                            tr {
                                td {
                                    strong { (entry.name) }
                                    @if let Some(desc) = &entry.description {
                                        br;
                                        span style="color: var(--text-muted); font-size: 0.8125rem;" {
                                            (desc)
                                        }
                                    }
                                }
                                td { code style="font-size: 0.8125rem;" { (entry.value_type) } }
                                td { code style="font-size: 0.8125rem;" { (entry.current.to_raw()) } }
                                td {
                                    code style="font-size: 0.8125rem; color: var(--text-muted);" {
                                        (entry.default.to_raw())
                                    }
                                }
                                td {
                                    @if entry.is_overridden {
                                        span style="color: var(--warning); font-size: 0.8125rem; font-weight: 500;" {
                                            "overridden"
                                        }
                                    } @else {
                                        span style="color: var(--text-muted); font-size: 0.8125rem;" {
                                            "default"
                                        }
                                    }
                                }
                                td {
                                    div style="display: flex; gap: 0.5rem; flex-wrap: wrap; align-items: center;" {
                                        form method="post"
                                            action={ (prefix) "/config/" (entry.name) "/set" }
                                            style="display: flex; gap: 0.25rem; align-items: center;" {
                                            (csrf_hidden_input(csrf_token, csrf_form_field))
                                            input type="text" name="value"
                                                value=(entry.current.to_raw())
                                                style="width: 11rem; font-size: 0.8125rem; padding: 0.25rem 0.5rem; border: 1px solid var(--border); border-radius: 0.25rem;" {}
                                            button type="submit" class="btn btn-sm btn-primary" { "Save" }
                                        }
                                        @if entry.is_overridden {
                                            form method="post"
                                                action={ (prefix) "/config/" (entry.name) "/unset" } {
                                                (csrf_hidden_input(csrf_token, csrf_form_field))
                                                button type="submit" class="btn btn-sm" { "Reset" }
                                            }
                                        }
                                        a href={ (prefix) "/config/" (entry.name) "/history" }
                                            class="btn btn-sm" { "History" }
                                    }
                                }
                            }
                        }
                    }
                }
            }, &CardConfig::new()))
        }
    };
    admin_layout(
        registry,
        Some(RUNTIME_CONFIG_NAV_SLUG),
        "Runtime Config",
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        true,
        &content,
    )
}

/// Render the change history page for a single config key.
#[allow(clippy::too_many_arguments)]
pub fn config_history_page(
    registry: &AdminRegistry,
    key: &str,
    history: &[ConfigChangeRecord],
    messages: &[FlashMessage],
    csrf_token: &str,
    csrf_token_header: &str,
    prefix: &str,
    actuator_prefix: &str,
) -> Markup {
    let title = format!("History: {key}");
    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            a href={ (prefix) "/config" } { "Runtime Config" }
            span class="sep" { "›" }
            span { (key) }
        }

        h1 style="font-size: 1.5rem; font-weight: 700; margin-bottom: 1rem;" {
            "History: " (key)
        }

        (card(&html! {
            @if history.is_empty() {
                p style="color: var(--text-muted); padding: 1rem;" {
                    "No changes recorded for this key yet."
                }
            } @else {
                table class="table" {
                    thead {
                        tr {
                            th { "Timestamp (UTC)" }
                            th { "Actor" }
                            th { "Old Value" }
                            th { "New Value" }
                        }
                    }
                    tbody {
                        @for record in history {
                            tr {
                                td {
                                    code style="font-size: 0.8125rem;" {
                                        (format_timestamp(record.timestamp_secs))
                                    }
                                }
                                td { (record.actor.as_deref().unwrap_or("—")) }
                                td {
                                    @match &record.old_value {
                                        Some(v) => {
                                            code style="font-size: 0.8125rem;" { (v.to_raw()) }
                                        }
                                        None => {
                                            span style="color: var(--text-muted);" { "—" }
                                        }
                                    }
                                }
                                td {
                                    @match &record.new_value {
                                        Some(v) => {
                                            code style="font-size: 0.8125rem;" { (v.to_raw()) }
                                        }
                                        None => {
                                            span style="color: var(--text-muted); font-style: italic;" {
                                                "reset to default"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }, &CardConfig::new()))

        a href={ (prefix) "/config" } class="btn" style="margin-top: 1rem;" {
            "← Back to Runtime Config"
        }
    };
    admin_layout(
        registry,
        Some(RUNTIME_CONFIG_NAV_SLUG),
        &title,
        prefix,
        actuator_prefix,
        csrf_token,
        csrf_token_header,
        messages,
        true,
        &content,
    )
}

fn format_timestamp(ts: u64) -> String {
    use chrono::{DateTime, Utc};
    let secs = i64::try_from(ts).unwrap_or(i64::MAX);
    DateTime::from_timestamp(secs, 0).map_or_else(
        || ts.to_string(),
        |dt: DateTime<Utc>| dt.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

// ── Rendering helpers ───────────────────────────────────────────────

/// Render a cell value in the list table.
fn render_cell_value(record: &Value, field: &AdminField) -> Markup {
    // Defense in depth: the list-view field filter already excludes
    // `AdminFieldKind::Password`, but mask here too so we can never leak
    // a hash if a caller slips one through.
    if matches!(field.kind, AdminFieldKind::Password) {
        return html! { "••••••••" };
    }
    // At-rest encrypted columns (#805) are redacted in admin views by default.
    // Rendering decrypted plaintext is a per-field opt-in (`encrypted_visible`,
    // from `#[encrypted(admin_visible)]`) gated through the admin policy
    // machinery (#496). The flag is per-field (not a global column-name lookup)
    // so an unrelated same-named plaintext column is unaffected.
    if field.encrypted && !field.encrypted_visible {
        return html! { span title="encrypted at rest" { "••••••••" } };
    }
    let val = record.get(field.name);
    match val {
        None | Some(Value::Null) => html! {
            span style="color: var(--text-muted);" { "—" }
        },
        Some(Value::Bool(b)) => html! {
            @if *b {
                span style="color: var(--success);" { "✓" }
            } @else {
                span style="color: var(--text-muted);" { "✗" }
            }
        },
        Some(Value::String(s)) => html! { (truncate_display(s, 80)) },
        Some(v) => html! { (v) },
    }
}

/// Percent-encode a query-component value.
///
/// Conservative: escapes everything that isn't an unreserved URL character
/// (RFC 3986 `unreserved`: `A-Z a-z 0-9 - . _ ~`). Safe for both path and
/// query contexts.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            other => {
                use std::fmt::Write;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// Encode active filters as a URL suffix, e.g.
/// `&filter.status=active&filter.tier=premium`. Empty when no filters are
/// active. Both keys and values are percent-encoded so values containing
/// `&`, `=`, or non-ASCII characters round-trip correctly.
fn encode_filter_suffix(filters: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in filters {
        out.push_str("&filter.");
        out.push_str(&url_encode(k));
        out.push('=');
        out.push_str(&url_encode(v));
    }
    out
}

/// UTF-8-safe truncation by character count. Appends `…` if truncated.
fn truncate_display(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Normalize a stored date string into `YYYY-MM-DD`, the only format the
/// HTML `<input type="date">` control accepts. Leaves the input untouched
/// if it can't be parsed — the user sees whatever the backend sent rather
/// than a silently-empty field.
fn normalize_date_input(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    // Fast path: already in the right shape.
    if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() {
        return s.to_owned();
    }
    // Fall back to full RFC 3339 (which includes the `T` + time).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.format("%Y-%m-%d").to_string();
    }
    // Finally try a naive datetime.
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return ndt.format("%Y-%m-%d").to_string();
    }
    s.to_owned()
}

/// Normalize a stored datetime string into `YYYY-MM-DDTHH:MM`, the only
/// format the HTML `<input type="datetime-local">` control accepts.
/// Browsers silently reject RFC 3339 with `Z`/offset; this maps common
/// backend representations onto the local-time shape the input expects.
///
/// **Wall-time preserved.** For RFC 3339 inputs with an explicit offset,
/// the offset is dropped but the local clock components are kept as-is
/// (we use `naive_local()`, not `naive_utc()`). That way an unchanged
/// edit-save round trip doesn't shift the timestamp — `12:34+05:30`
/// renders as `12:34`, posts back unchanged as `12:34`, and the model can
/// re-attach whatever offset it wants.
///
/// If parsing fails, the original string is returned unchanged — better
/// to show the server's value than silently blank the field on edit.
fn normalize_datetime_local_input(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    // Already local-shaped.
    if chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").is_ok() {
        return s.to_owned();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return ndt.format("%Y-%m-%dT%H:%M").to_string();
    }
    // RFC 3339 with timezone — keep the local wall-clock components and
    // drop the offset (don't shift to UTC; that would mutate the value
    // on a no-op save).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.naive_local().format("%Y-%m-%dT%H:%M").to_string();
    }
    s.to_owned()
}

/// Render a field value in the detail view.
fn render_detail_value(record: &Value, field: &AdminField) -> Markup {
    // Encrypted columns (#805) are redacted by default in admin detail views; the
    // `encrypted_visible` opt-in shows plaintext. Per-field, not a name lookup.
    if field.encrypted && !field.encrypted_visible {
        return html! { span title="encrypted at rest" { "••••••••" } };
    }
    let val = record.get(field.name);
    match val {
        None | Some(Value::Null) => html! {
            span style="color: var(--text-muted);" { "—" }
        },
        Some(Value::Bool(b)) => html! {
            @if *b { "Yes" } @else { "No" }
        },
        Some(Value::String(s)) => {
            if matches!(field.kind, AdminFieldKind::Password) {
                html! { "••••••••" }
            } else if matches!(field.kind, AdminFieldKind::TextArea | AdminFieldKind::Json) {
                html! {
                    pre style="white-space: pre-wrap; font-size: 0.8125rem; background: var(--bg); padding: 0.75rem; border-radius: 0.375rem;" {
                        (s)
                    }
                }
            } else {
                html! { (s) }
            }
        }
        // Objects and arrays pretty-printed inside <pre>; plain text so
        // Maud HTML-escapes attacker-controlled content. PreEscaped here
        // would be a stored-XSS sink.
        Some(v) => html! {
            pre style="white-space: pre-wrap; font-size: 0.8125rem; background: var(--bg); padding: 0.75rem; border-radius: 0.375rem;" {
                (serde_json::to_string_pretty(v).unwrap_or_default())
            }
        },
    }
}

/// Render a read-only display for a create-only field on the edit form.
///
/// Shows the current value as static text with no form control so the admin
/// can see it but cannot alter it (and it is never submitted to the server).
fn render_readonly_display(field: &AdminField, record: Option<&Value>) -> Markup {
    let value = record
        .and_then(|r| r.get(field.name))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    html! {
        p class="form-static-value" style="margin: 0; padding: 0.375rem 0; color: #555;" {
            (value)
        }
        small class="form-help" style="color: #888;" {
            "This field cannot be changed after creation."
        }
    }
}

/// Render a form widget for a field.
fn render_form_widget(field: &AdminField, record: Option<&Value>) -> Markup {
    // Encrypted columns (#805). On EDIT (`record` is `Some`) we must never reveal
    // or overwrite the stored ciphertext, so render a disabled, redacted control
    // with no `name`: the plaintext never reaches the HTML and a save never
    // submits (and thus never overwrites) it. On CREATE (`record` is `None`) there
    // is no stored secret to protect and the generated `New*` DTO requires the
    // value, so fall through to a normal editable input that captures the initial
    // plaintext (the wrapper encrypts it on insert). The flag is per-field, so an
    // unrelated same-named plaintext column stays editable.
    if field.encrypted && record.is_some() {
        return html! {
            input type="text" class="form-input" value="••••••••" disabled
                title="Encrypted at rest — managed outside the admin";
        };
    }
    let current_value = record
        .and_then(|r| r.get(field.name))
        .cloned()
        .unwrap_or(Value::Null);
    let str_val = match &current_value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };

    match &field.kind {
        AdminFieldKind::Text => html! {
            input type="text" class="form-input" name=(field.name) id=(field.name)
                value=(str_val)
                required[field.required];
        },
        AdminFieldKind::TextArea => html! {
            textarea class="form-input" name=(field.name) id=(field.name)
                required[field.required] {
                (str_val)
            }
        },
        AdminFieldKind::Integer => html! {
            input type="number" class="form-input" name=(field.name) id=(field.name)
                value=(str_val) step="1"
                required[field.required];
        },
        AdminFieldKind::Float => html! {
            input type="number" class="form-input" name=(field.name) id=(field.name)
                value=(str_val) step="any"
                required[field.required];
        },
        AdminFieldKind::Boolean => {
            let checked = matches!(current_value, Value::Bool(true));
            html! {
                input type="hidden" name=(field.name) value="false";
                input type="checkbox" name=(field.name) id=(field.name)
                    value="true" checked[checked]
                    style="width: auto;";
            }
        }
        AdminFieldKind::Date => {
            let v = normalize_date_input(&str_val);
            html! {
                input type="date" class="form-input" name=(field.name) id=(field.name)
                    value=(v)
                    required[field.required];
            }
        }
        AdminFieldKind::DateTime => {
            let v = normalize_datetime_local_input(&str_val);
            html! {
                input type="datetime-local" class="form-input" name=(field.name) id=(field.name)
                    value=(v)
                    required[field.required];
            }
        }
        AdminFieldKind::Select(options) => html! {
            select class="form-input" name=(field.name) id=(field.name)
                required[field.required] {
                option value="" { "— Select —" }
                @for opt in options {
                    option value=(opt.value)
                        selected[str_val == opt.value] {
                        (opt.label)
                    }
                }
            }
        },
        AdminFieldKind::Hidden => html! {
            input type="hidden" name=(field.name) value=(str_val);
        },
        AdminFieldKind::Password => html! {
            input type="password" class="form-input" name=(field.name) id=(field.name)
                placeholder="Leave blank to keep current"
                autocomplete="new-password";
        },
        AdminFieldKind::Json => html! {
            textarea class="form-input" name=(field.name) id=(field.name)
                style="font-family: monospace; min-height: 150px;"
                required[field.required] {
                (str_val)
            }
        },
    }
}

/// Render pagination controls.
///
/// `search_enc` and `filters_enc` are expected to be already URL-encoded;
/// callers pass the raw form for rendering and the encoded form for link
/// building.
#[allow(clippy::too_many_arguments)]
fn render_pagination(
    result: &ListResult,
    model_slug: &str,
    search_enc: &str,
    sort_by: Option<&str>,
    sort_dir: SortDirection,
    filters_enc: &str,
    prefix: &str,
) -> Markup {
    let current = result.page.max(1);

    // The fixed portion of the query string — filters, sort, and search to
    // preserve across page clicks. The shared pager appends the `page` param.
    let query = {
        let mut s = String::new();
        if !search_enc.is_empty() {
            s.push_str("q=");
            s.push_str(search_enc);
        }
        if let Some(sort) = sort_by {
            if !s.is_empty() {
                s.push('&');
            }
            s.push_str("sort=");
            s.push_str(&url_encode(sort));
            s.push_str("&dir=");
            s.push_str(sort_dir.as_str());
        }
        if !filters_enc.is_empty() {
            // filters_enc is pre-encoded as `&filter.k=v&…`; merge it in.
            let trimmed = filters_enc.strip_prefix('&').unwrap_or(filters_enc);
            if !s.is_empty() {
                s.push('&');
            }
            s.push_str(trimmed);
        }
        s
    };

    let start = if result.total == 0 {
        0
    } else {
        result
            .per_page
            .saturating_mul(current.saturating_sub(1))
            .saturating_add(1)
    };
    let end = start
        .saturating_add(result.per_page)
        .saturating_sub(1)
        .min(result.total);

    let meta = page_meta(current, result.per_page, result.total, result.total_pages());
    let base = format!("{prefix}/{model_slug}");
    let opts = PagerOptions::new(&base)
        .query(&query)
        .window(1)
        .prev_label("← Prev")
        .next_label("Next →");

    html! {
        div class="pagination" {
            span {
                "Showing " (start) "–" (end) " of " (result.total)
            }
            (pagination_nav(&meta, &opts))
        }
    }
}

fn page_meta(page: u64, per_page: u64, total: u64, total_pages: u64) -> Page<()> {
    let to_u32 = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
    Page::from_raw(to_u32(page), to_u32(per_page), total, to_u32(total_pages))
}

// -- Version history pane ----------------------------------------------------

/// Render the version history pane for an opted-in model record.
///
/// Called by `GET /admin/{slug}/{id}/history`. Lists entries in
/// chronological order with actor, timestamp, and column-level diff.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn model_history_page(
    registry: &AdminRegistry,
    model_slug: &str,
    model_name: &str,
    model_name_plural: &str,
    record_id_val: i64,
    history: &AdminHistoryPage,
    prefix: &str,
    actuator_prefix: &str,
    csrf_token_header: &str,
    show_config: bool,
) -> Markup {
    let record_display = format!("{model_name} #{record_id_val}");
    let history_page_href = |page: u64| {
        format!(
            "{prefix}/{model_slug}/{record_id_val}/history?page={page}&per_page={}",
            history.per_page
        )
    };
    let empty_messages: &[FlashMessage] = &[];
    let content = html! {
        div class="breadcrumbs" {
            a href=(prefix) { "Admin" }
            span class="sep" { "›" }
            a href={ (prefix) "/" (model_slug) } { (model_name_plural) }
            span class="sep" { "›" }
            a href={ (prefix) "/" (model_slug) "/" (record_id_val) } { (record_display) }
            span class="sep" { "›" }
            span { "History" }
        }

        ({
            let header_action = html! { small { " " (history.total) " entries" } };
            let body = html! {
                @if history.entries.is_empty() {
                    p class="text-muted" style="padding:1rem" { "No history entries yet." }
                } @else {
                    table class="admin-table" {
                        thead {
                            tr {
                                th { "#" }
                                th { "Operation" }
                                th { "Actor" }
                                th { "Request ID" }
                                th { "Changes" }
                                th { "Recorded At" }
                            }
                        }
                        tbody {
                            @for entry in &history.entries {
                                tr {
                                    td { (entry.id) }
                                    td {
                                        span class={ "badge badge-" (entry.op) } { (entry.op) }
                                    }
                                    td { code { (entry.actor) } }
                                    td {
                                        @if let Some(ref req_id) = entry.request_id {
                                            code class="text-muted" { (req_id) }
                                        } @else {
                                            span class="text-muted" { "—" }
                                        }
                                    }
                                    td {
                                        @if entry.changes.is_empty() {
                                            span class="text-muted" { "no changes" }
                                        } @else {
                                            details {
                                                summary { (entry.changes.len()) " column(s)" }
                                                ul class="change-list" {
                                                    @for change in &entry.changes {
                                                        li {
                                                            @if let Some(col) = change.get("column").and_then(Value::as_str) {
                                                                code { (col) }
                                                            }
                                                            @if change.get("sensitive").and_then(Value::as_bool).unwrap_or(false) {
                                                                span class="badge-sensitive" { " [sensitive]" }
                                                            } @else {
                                                                " "
                                                                span class="text-muted" { "before: " }
                                                                @if let Some(before) = change.get("before") {
                                                                    code { (before) }
                                                                } @else {
                                                                    em { "null" }
                                                                }
                                                                " → "
                                                                span class="text-muted" { "after: " }
                                                                @if let Some(after) = change.get("after") {
                                                                    code { (after) }
                                                                } @else {
                                                                    em { "null" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    td {
                                        time datetime=(entry.recorded_at.to_rfc3339()) {
                                            (entry.recorded_at.format("%Y-%m-%d %H:%M:%S UTC"))
                                        }
                                    }
                                }
                            }
                        }
                    }

                    @if history.total_pages() > 1 {
                        div class="pagination" {
                            @if history.page > 1 {
                                a href=(history_page_href(history.page - 1))
                                    class="btn btn-secondary btn-sm" { "← Prev" }
                            }
                            span { " Page " (history.page) " of " (history.total_pages()) " " }
                            @if history.has_next_page() {
                                a href=(history_page_href(history.page + 1))
                                    class="btn btn-secondary btn-sm" { "Next →" }
                            }
                        }
                    }
                }
            };
            card(&body, &CardConfig::new().title("Version History").header_action(header_action))
        })
    };
    admin_layout(
        registry,
        Some(model_slug),
        &record_display,
        prefix,
        actuator_prefix,
        "",
        csrf_token_header,
        empty_messages,
        show_config,
        &content,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // A model with at-rest encrypted columns (#805): `ssn` is redacted by default;
    // `audit_note` opts into `admin_visible` (shown in read views). The per-field
    // flag on `AdminField` carries this — no global column-name lookup.

    #[test]
    fn encrypted_columns_are_redacted_in_admin_views() {
        let record = serde_json::json!({ "id": 1, "ssn": "123-45-6789" });
        let field = AdminField::new("ssn", AdminFieldKind::Text).encrypted();
        let cell = render_cell_value(&record, &field).into_string();
        let detail = render_detail_value(&record, &field).into_string();
        assert!(
            !cell.contains("123-45-6789"),
            "list cell must redact: {cell}"
        );
        assert!(
            !detail.contains("123-45-6789"),
            "detail must redact: {detail}"
        );
        assert!(cell.contains("••••••••"));
        assert!(detail.contains("••••••••"));
    }

    #[test]
    fn admin_visible_encrypted_column_renders_plaintext_in_views() {
        // The decrypted record (admin loads it through the model) is shown for
        // an `admin_visible` column in list/detail views.
        let record = serde_json::json!({ "id": 1, "audit_note": "visible-note" });
        let field = AdminField::new("audit_note", AdminFieldKind::Text).encrypted_visible();
        let cell = render_cell_value(&record, &field).into_string();
        let detail = render_detail_value(&record, &field).into_string();
        assert!(cell.contains("visible-note"), "admin_visible cell: {cell}");
        assert!(
            detail.contains("visible-note"),
            "admin_visible detail: {detail}"
        );
    }

    #[test]
    fn edit_form_never_prefills_encrypted_plaintext() {
        // Even an admin_visible column must not pre-fill its secret into the
        // editable form control.
        let record = serde_json::json!({ "ssn": "123-45-6789", "audit_note": "visible-note" });
        for field in [
            AdminField::new("ssn", AdminFieldKind::Text).encrypted(),
            AdminField::new("audit_note", AdminFieldKind::Text).encrypted_visible(),
        ] {
            let col = field.name;
            let form = render_form_widget(&field, Some(&record)).into_string();
            assert!(
                !form.contains("123-45-6789") && !form.contains("visible-note"),
                "edit form must not pre-fill encrypted plaintext for {col}: {form}"
            );
            // The edit control is disabled and carries no `name`, so it is not
            // submitted (and cannot overwrite the stored ciphertext).
            assert!(form.contains("disabled"), "edit control disabled: {form}");
            assert!(
                !form.contains("name="),
                "edit control must not submit: {form}"
            );
        }
    }

    #[test]
    fn create_form_allows_setting_initial_encrypted_value() {
        // On CREATE (`record` is None) there is no stored secret to protect and the
        // New* DTO needs the value, so the encrypted field must be an editable,
        // submittable, empty input — otherwise the default "New" flow can't create
        // a record with a required encrypted column (#805).
        let field = AdminField::new("ssn", AdminFieldKind::Text).encrypted();
        let form = render_form_widget(&field, None).into_string();
        assert!(
            form.contains("name=\"ssn\""),
            "create control must submit the value: {form}"
        );
        assert!(
            !form.contains("disabled"),
            "create control editable: {form}"
        );
        assert!(
            !form.contains("••••••••"),
            "create control is an empty input, not the redaction mask: {form}"
        );
    }

    fn list_result(page: u64, per_page: u64, total: u64) -> crate::traits::ListResult {
        crate::traits::ListResult {
            total,
            per_page,
            page,
            records: vec![],
        }
    }

    #[test]
    fn render_pagination_shows_summary_and_nav() {
        let result = list_result(2, 10, 100);
        let html = render_pagination(
            &result,
            "users",
            "",
            None,
            crate::traits::SortDirection::Asc,
            "",
            "/admin",
        )
        .into_string();
        assert!(html.contains("Showing 11–20 of 100"), "{html}");
        assert!(html.contains("autumn-pager"), "{html}");
        // Page links target the model list path.
        assert!(html.contains("/admin/users?"), "{html}");
    }

    #[test]
    fn render_pagination_preserves_search_and_sort() {
        let result = list_result(5, 10, 200); // 20 pages
        let html = render_pagination(
            &result,
            "users",
            "foo",
            Some("name"),
            crate::traits::SortDirection::Asc,
            "",
            "/admin",
        )
        .into_string();
        // Every emitted page link must keep the active filter and sort.
        for (i, _) in html.match_indices("href=\"") {
            let rest = &html[i + 6..];
            let end = rest.find('"').unwrap_or(rest.len());
            let href = &rest[..end];
            assert!(href.contains("q=foo"), "missing q in {href}");
            assert!(href.contains("sort=name"), "missing sort in {href}");
        }
        // Middle page of a 20-page set must window with an ellipsis.
        assert!(html.contains('…'), "{html}");
    }

    #[test]
    fn render_pagination_marks_active_page() {
        let result = list_result(3, 10, 100);
        let html = render_pagination(
            &result,
            "users",
            "",
            None,
            crate::traits::SortDirection::Asc,
            "",
            "/admin",
        )
        .into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn truncate_display_ascii() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello world!", 6), "hello…");
    }

    #[test]
    fn truncate_display_utf8_boundary_safe() {
        // 4 multi-byte chars, each 3 bytes. Byte slicing at 7 would panic.
        let s = "日本語日";
        assert_eq!(truncate_display(s, 3), "日本…");
        // Under the limit by char count, no truncation.
        assert_eq!(truncate_display(s, 10), s);
    }

    #[test]
    fn url_encode_handles_reserved_chars() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(url_encode("safe-._~"), "safe-._~");
    }

    #[test]
    fn url_encode_handles_utf8() {
        // "é" is 0xC3 0xA9 in UTF-8.
        assert_eq!(url_encode("é"), "%C3%A9");
    }

    #[test]
    fn normalize_datetime_local_accepts_expected_shape() {
        assert_eq!(
            normalize_datetime_local_input("2026-04-24T12:34"),
            "2026-04-24T12:34"
        );
    }

    #[test]
    fn normalize_datetime_local_strips_seconds() {
        assert_eq!(
            normalize_datetime_local_input("2026-04-24T12:34:56"),
            "2026-04-24T12:34"
        );
    }

    #[test]
    fn normalize_datetime_local_strips_rfc3339_zulu() {
        // The browser refuses the `Z` suffix; we emit UTC without offset.
        assert_eq!(
            normalize_datetime_local_input("2026-04-24T12:34:56Z"),
            "2026-04-24T12:34"
        );
    }

    #[test]
    fn normalize_datetime_local_preserves_wall_time_across_offsets() {
        // Regression: previously this used naive_utc() which shifted
        // 12:34+05:30 to 07:04 UTC. That mutated the value on a no-op
        // edit-save round trip. Now we preserve the local wall clock —
        // the offset is dropped, but 12:34 stays 12:34 so re-saving
        // produces the same logical timestamp.
        assert_eq!(
            normalize_datetime_local_input("2026-04-24T12:34:56+05:30"),
            "2026-04-24T12:34"
        );
        // Negative offset, end-of-day boundary — verify the date doesn't
        // flip either.
        assert_eq!(
            normalize_datetime_local_input("2026-04-24T23:30:00-04:00"),
            "2026-04-24T23:30"
        );
    }

    #[test]
    fn normalize_datetime_local_empty_stays_empty() {
        assert_eq!(normalize_datetime_local_input(""), "");
    }

    #[test]
    fn normalize_datetime_local_leaves_garbage_untouched() {
        // Better to show the raw value than silently blank the field.
        assert_eq!(normalize_datetime_local_input("not-a-date"), "not-a-date");
    }

    #[test]
    fn normalize_date_accepts_expected_shape() {
        assert_eq!(normalize_date_input("2026-04-24"), "2026-04-24");
    }

    #[test]
    fn normalize_date_extracts_from_rfc3339() {
        assert_eq!(normalize_date_input("2026-04-24T12:34:56Z"), "2026-04-24");
    }

    // ── End-to-end render checks (CSRF / XSS / actuator prefix wiring) ──

    fn dummy_registry() -> AdminRegistry {
        AdminRegistry::new()
    }

    #[test]
    fn history_page_pagination_preserves_per_page() {
        let r = dummy_registry();
        let history = AdminHistoryPage {
            entries: vec![crate::traits::AdminHistoryEntry {
                id: 1,
                actor: "system".to_owned(),
                op: "insert".to_owned(),
                request_id: None,
                changes: vec![],
                recorded_at: chrono::Utc::now(),
            }],
            total: 250,
            page: 2,
            per_page: 100,
        };

        let html = model_history_page(
            &r,
            "posts",
            "Post",
            "Posts",
            42,
            &history,
            "/admin",
            "/ops",
            "X-CSRF-Token",
            false,
        )
        .into_string();

        assert!(
            html.contains("/admin/posts/42/history?page=1&amp;per_page=100"),
            "previous history page link must preserve per_page: {html}"
        );
        assert!(
            html.contains("/admin/posts/42/history?page=3&amp;per_page=100"),
            "next history page link must preserve per_page: {html}"
        );
    }

    #[test]
    fn dashboard_emits_csrf_meta_and_script() {
        let r = dummy_registry();
        let html = dashboard_page(
            &r,
            &[],
            &[],
            "tok-123",
            "X-CSRF-Token",
            "/admin",
            "/ops",
            false,
        )
        .into_string();
        assert!(
            html.contains(r#"<meta name="csrf-token" content="tok-123""#),
            "CSRF meta tag missing: {html}"
        );
        assert!(
            html.contains("/static/js/autumn-htmx-csrf.js"),
            "HTMX CSRF helper script not loaded: {html}"
        );
    }

    #[test]
    fn dashboard_uses_configured_actuator_prefix() {
        let r = dummy_registry();
        let html = dashboard_page(&r, &[], &[], "tok", "X-CSRF-Token", "/admin", "/ops", false)
            .into_string();
        assert!(
            html.contains(r#"href="/ops/ui""#),
            "sidebar link wrong: {html}"
        );
        assert!(
            html.contains(r#"hx-get="/ops/ui/metrics""#),
            "metrics polling URL wrong: {html}"
        );
        assert!(
            !html.contains("/actuator/"),
            "must not hardcode /actuator when prefix is /ops: {html}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn jobs_page_renders_lists_actions_polling_and_csrf() {
        use autumn_web::job::{
            JobAdminPage, JobAdminRecord, JobAdminSnapshot, JobAdminStatus, JobScheduleSummary,
        };

        let r = dummy_registry();
        let snapshot = JobAdminSnapshot {
            enqueued: JobAdminPage::new(
                vec![JobAdminRecord {
                    id: "job-enqueued".to_owned(),
                    name: "send_email".to_owned(),
                    queue: "default".to_owned(),
                    status: JobAdminStatus::Enqueued,
                    enqueued_at: Some("2026-05-07T10:00:00Z".to_owned()),
                    scheduled_for: None,
                    started_at: None,
                    finished_at: None,
                    attempt: 1,
                    max_attempts: 5,
                    last_error: None,
                    principal_id: Some("42".to_owned()),
                    correlation_id: Some("req-123".to_owned()),
                }],
                1,
                1,
                25,
            ),
            scheduled: JobAdminPage::new(
                vec![JobAdminRecord {
                    id: "job-scheduled".to_owned(),
                    name: "reminder".to_owned(),
                    queue: "default".to_owned(),
                    status: JobAdminStatus::Scheduled,
                    enqueued_at: Some("2026-05-07T10:00:00Z".to_owned()),
                    scheduled_for: Some("2026-05-08T10:00:00Z".to_owned()),
                    started_at: None,
                    finished_at: None,
                    attempt: 1,
                    max_attempts: 5,
                    last_error: None,
                    principal_id: None,
                    correlation_id: None,
                }],
                1,
                1,
                25,
            ),
            running: JobAdminPage::new(
                vec![JobAdminRecord {
                    id: "job-running".to_owned(),
                    name: "reindex".to_owned(),
                    queue: "default".to_owned(),
                    status: JobAdminStatus::Running,
                    enqueued_at: Some("2026-05-07T10:01:00Z".to_owned()),
                    scheduled_for: None,
                    started_at: Some("2026-05-07T10:02:00Z".to_owned()),
                    finished_at: None,
                    attempt: 1,
                    max_attempts: 3,
                    last_error: None,
                    principal_id: None,
                    correlation_id: None,
                }],
                1,
                1,
                25,
            ),
            completed: JobAdminPage::new(
                vec![JobAdminRecord {
                    id: "job-complete".to_owned(),
                    name: "digest".to_owned(),
                    queue: "default".to_owned(),
                    status: JobAdminStatus::Completed,
                    enqueued_at: Some("2026-05-07T09:00:00Z".to_owned()),
                    scheduled_for: None,
                    started_at: Some("2026-05-07T09:01:00Z".to_owned()),
                    finished_at: Some("2026-05-07T09:02:00Z".to_owned()),
                    attempt: 1,
                    max_attempts: 3,
                    last_error: None,
                    principal_id: None,
                    correlation_id: None,
                }],
                1,
                1,
                25,
            ),
            failed: JobAdminPage::new(
                vec![JobAdminRecord {
                    id: "job-failed".to_owned(),
                    name: "send_email".to_owned(),
                    queue: "default".to_owned(),
                    status: JobAdminStatus::Failed,
                    enqueued_at: Some("2026-05-07T08:00:00Z".to_owned()),
                    scheduled_for: None,
                    started_at: Some("2026-05-07T08:01:00Z".to_owned()),
                    finished_at: Some("2026-05-07T08:02:00Z".to_owned()),
                    attempt: 5,
                    max_attempts: 5,
                    last_error: Some("smtp refused recipient".repeat(6)),
                    principal_id: Some("7".to_owned()),
                    correlation_id: None,
                }],
                1,
                1,
                25,
            ),
            schedules: vec![JobScheduleSummary {
                name: "send-digest".to_owned(),
                schedule: "every 1h".to_owned(),
                next_run_at: None,
                last_run_status: Some("ok".to_owned()),
            }],
            bounded_history_limit: 1_000,
        };

        let html = jobs_page(
            &r,
            &snapshot,
            &[],
            "tok-job",
            "authenticity_token",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
        )
        .into_string();
        assert!(html.contains("Jobs"));
        assert!(html.contains("Enqueued"));
        assert!(html.contains("Running"));
        assert!(html.contains("Completed (last 24h)"));
        assert!(html.contains("Failed (last 7d)"));
        assert!(html.contains("send_email"));
        assert!(html.contains("req-123"));
        assert!(html.contains(r#"action="/admin/jobs/job-failed/retry""#));
        assert!(html.contains(r#"action="/admin/jobs/job-failed/discard""#));
        assert!(html.contains(r#"action="/admin/jobs/job-enqueued/cancel""#));
        // Scheduled (delayed) jobs render their own list, show the due time, and
        // can be canceled before they run.
        assert!(html.contains("Scheduled"));
        assert!(html.contains("due 2026-05-08T10:00:00Z"));
        assert!(html.contains(r#"action="/admin/jobs/job-scheduled/cancel""#));
        assert!(html.contains(r#"name="authenticity_token" value="tok-job""#));
        assert!(!html.contains(r#"name="_csrf" value="tok-job""#));
        assert!(html.contains(r#"hx-get="/admin/jobs/counters""#));
        assert!(html.contains(r#"hx-trigger="load, every 2s""#));
        assert!(html.contains("send-digest"));
    }

    #[test]
    fn jobs_counters_fragment_preserves_polling_after_outer_swap() {
        use autumn_web::job::JobAdminSnapshot;

        let html = jobs_counters(&JobAdminSnapshot::empty(), "/admin").into_string();
        assert!(html.contains(r#"id="jobs-counters""#));
        assert!(html.contains(r#"hx-get="/admin/jobs/counters""#));
        assert!(html.contains(r#"hx-trigger="load, every 2s""#));
        assert!(html.contains(r#"hx-swap="outerHTML""#));
    }

    #[test]
    fn form_page_renders_hidden_csrf_input() {
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let html = model_form_page(
            &r,
            "widgets",
            "Widget",
            "Widgets",
            &fields,
            None,
            None,
            &[],
            "tok-xyz",
            "authenticity_token",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="authenticity_token" value="tok-xyz""#),
            "custom CSRF hidden field missing: {html}"
        );
        assert!(!html.contains(r#"name="_csrf" value="tok-xyz""#));
    }

    #[test]
    fn form_page_normalizes_datetime_for_browser_input() {
        let r = dummy_registry();
        let fields = vec![AdminField::new("created_at", AdminFieldKind::DateTime)];
        // RFC 3339 with `Z` — would render as empty without normalization.
        let record = serde_json::json!({"id": 1, "created_at": "2026-04-24T12:34:56Z"});
        let html = model_form_page(
            &r,
            "widgets",
            "Widget",
            "Widgets",
            &fields,
            Some(&record),
            Some(1),
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
        )
        .into_string();
        assert!(
            html.contains(r#"value="2026-04-24T12:34""#),
            "datetime-local input should carry browser-friendly value: {html}"
        );
        assert!(
            !html.contains(r#"value="2026-04-24T12:34:56Z""#),
            "raw RFC3339 must not reach datetime-local input: {html}"
        );
    }

    #[test]
    fn form_page_action_uses_path_id_not_payload_id() {
        // Regression: mutation target must come from the URL path, not the
        // record payload. Payload says id=99, path says 42 — form posts to 42.
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let record = serde_json::json!({"id": 99, "name": "x"});
        let html = model_form_page(
            &r,
            "widgets",
            "Widget",
            "Widgets",
            &fields,
            Some(&record),
            Some(42),
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
        )
        .into_string();
        assert!(
            html.contains(r#"action="/admin/widgets/42""#),
            "form action should use path-based id 42, not payload id 99: {html}"
        );
        assert!(
            !html.contains(r#"action="/admin/widgets/99""#),
            "payload-derived id must not appear in form action: {html}"
        );
    }

    #[test]
    fn detail_page_edit_delete_links_use_path_id() {
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let record = serde_json::json!({"id": 99, "name": "x"});
        let html = model_detail_page(
            &r,
            "widgets",
            "Widget",
            "Widgets",
            &fields,
            &record,
            "#42",
            42,
            &[],
            "t",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
        )
        .into_string();
        assert!(
            html.contains(r#"href="/admin/widgets/42/edit""#),
            "Edit link must use path id 42: {html}"
        );
        assert!(
            html.contains(r#"hx-delete="/admin/widgets/42""#),
            "Delete must target path id 42: {html}"
        );
        assert!(
            !html.contains("widgets/99"),
            "payload id 99 must not route mutations: {html}"
        );
    }

    #[test]
    fn detail_view_escapes_malicious_json() {
        let r = dummy_registry();
        let fields = vec![AdminField::new("meta", AdminFieldKind::Json)];
        // Pretty-printed JSON of a nested object contains attacker-controlled
        // angle brackets. Pre-fix this rendered as live HTML.
        let record = serde_json::json!({
            "id": 1,
            "meta": {"xss": "<script>alert(1)</script>"},
        });
        let html = model_detail_page(
            &r,
            "widgets",
            "Widget",
            "Widgets",
            &fields,
            &record,
            "#1",
            1,
            &[],
            "t",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
        )
        .into_string();
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "raw <script> must be escaped: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "escaped form expected: {html}"
        );
    }

    #[test]
    fn layout_loads_external_admin_js_not_inline() {
        // The layout must NOT ship an inline <script>{js}</script> block —
        // that would be blocked by the default CSP (`script-src 'self'`).
        // Instead it must load the plugin-owned asset at a fingerprinted
        // `/{prefix}/static/admin.<hash>.js` URL.
        let r = dummy_registry();
        let html = dashboard_page(
            &r,
            &[],
            &[],
            "t",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
        )
        .into_string();
        let expected = format!(r#"src="/admin{}""#, &**ADMIN_JS_PATH);
        assert!(
            html.contains(&expected),
            "admin.js must be referenced as an external script at {expected}: {html}"
        );
        // The URL must be content-fingerprinted so immutable caching is safe.
        assert!(
            html.contains("/admin/static/admin.") && html.contains(".js\""),
            "admin.js URL should be fingerprinted (admin.<hash>.js): {html}"
        );
        assert!(
            !html.contains(r#"src="/admin/static/admin.js""#),
            "unfingerprinted URL would invalidate immutable caching: {html}"
        );
        // No inline onclick on the select-all checkbox either — it's
        // wired via event delegation in admin.js now.
        assert!(
            !html.contains("onclick=\""),
            "no inline event handlers allowed under default CSP: {html}"
        );
    }

    #[test]
    fn list_page_hides_hidden_fields_even_if_list_display_true() {
        use crate::traits::ListResult;
        let r = dummy_registry();
        // Hidden contract: shown in detail, not in list. Even with
        // list_display=true (the default), the column must not appear.
        let fields = vec![
            AdminField::new("name", AdminFieldKind::Text),
            AdminField::new("internal_token", AdminFieldKind::Hidden),
        ];
        let result = ListResult {
            records: vec![serde_json::json!({
                "id": 1,
                "name": "alice",
                "internal_token": "INT-9999",
            })],
            total: 1,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "users",
            "Users",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        assert!(
            !html.contains("INT-9999"),
            "hidden field value must not surface in list view: {html}"
        );
        assert!(
            !html.contains("internal_token") && !html.contains("Internal Token"),
            "hidden field column header must not appear: {html}"
        );
    }

    #[test]
    fn list_page_hides_password_fields_even_if_list_display_true() {
        use crate::traits::ListResult;
        let r = dummy_registry();
        // A model that (incorrectly) marks a password field list_display=true.
        // The admin plugin must drop it from the index table anyway.
        let fields = vec![
            AdminField::new("name", AdminFieldKind::Text),
            AdminField::new("password_hash", AdminFieldKind::Password),
        ];
        let result = ListResult {
            records: vec![serde_json::json!({
                "id": 1,
                "name": "alice",
                "password_hash": "$argon2id$leaked",
            })],
            total: 1,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "users",
            "Users",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        assert!(
            !html.contains("$argon2id$leaked"),
            "raw password hash must not appear in list view: {html}"
        );
        assert!(
            !html.contains("password_hash"),
            "password column must not have a header in list view: {html}"
        );
    }

    #[test]
    fn list_page_handles_records_without_numeric_id() {
        // A model that returns a row whose `id` is missing (or non-numeric)
        // must not render `/admin/widgets/0` action links — those would
        // route mutations to the wrong row. Show "no id" instead.
        use crate::traits::ListResult;
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let result = ListResult {
            records: vec![
                serde_json::json!({"id": 7, "name": "with id"}),
                serde_json::json!({"name": "no id"}),
            ],
            total: 2,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "widgets",
            "Widgets",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        // Row with id renders working links and a checkbox.
        assert!(
            html.contains(r#"href="/admin/widgets/7""#),
            "row with id should have working View link: {html}"
        );
        // Row without id shows the placeholder, never `/0` links.
        assert!(
            html.contains(r#"<span style="color: var(--text-muted); font-size: 0.75rem;">no id"#)
                || html.contains("no id</span>"),
            "row without id should show 'no id' placeholder: {html}"
        );
        assert!(
            !html.contains("/admin/widgets/0"),
            "must not generate /0 links for rows missing id: {html}"
        );
    }

    #[test]
    fn list_page_carries_filters_into_sort_and_pagination_links() {
        // Active filter must round-trip through every navigation URL the
        // list view generates — sort header links AND pagination links.
        // Otherwise a user with `?filter.status=active` who clicks a
        // column header silently reverts to unfiltered results.
        use crate::traits::ListResult;
        let r = dummy_registry();
        let mut name = AdminField::new("name", AdminFieldKind::Text);
        name.sortable = true;
        let fields = vec![name];
        // 60 records over per_page=25 → 3 pages, so pagination renders.
        let result = ListResult {
            records: vec![serde_json::json!({"id": 1, "name": "alice"})],
            total: 60,
            page: 1,
            per_page: 25,
        };
        let active_filters = vec![
            ("status".to_owned(), "active".to_owned()),
            ("tier".to_owned(), "premium".to_owned()),
        ];
        let html = model_list_page(
            &r,
            "users",
            "Users",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &active_filters,
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        // Sort header link carries both filters.
        assert!(
            html.contains("filter.status=active"),
            "sort link must preserve filter.status: {html}"
        );
        assert!(
            html.contains("filter.tier=premium"),
            "sort link must preserve filter.tier: {html}"
        );
        // Pagination link to page 2 carries both filters too.
        assert!(
            html.contains("page=2") && html.contains("filter.status=active"),
            "pagination link must preserve filter.status: {html}"
        );
    }

    #[test]
    fn search_form_carries_filters_as_hidden_inputs() {
        // Regression: search submit (method=get) drops anything not in
        // the form. Active filters must round-trip via hidden inputs so
        // typing a search query doesn't reset the dataset.
        use crate::traits::ListResult;
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let result = ListResult {
            records: vec![],
            total: 0,
            page: 1,
            per_page: 25,
        };
        let active_filters = vec![
            ("status".to_owned(), "active".to_owned()),
            ("tier".to_owned(), "premium".to_owned()),
        ];
        let html = model_list_page(
            &r,
            "users",
            "Users",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &active_filters,
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="filter.status" value="active""#),
            "search form should preserve filter.status: {html}"
        );
        assert!(
            html.contains(r#"<input type="hidden" name="filter.tier" value="premium""#),
            "search form should preserve filter.tier: {html}"
        );
        // Live-search must include the hidden filter inputs in the
        // HTMX request, otherwise typing in the search box silently
        // resets to unfiltered results. `hx-include="closest form"`
        // pulls every form input (including the filter hiddens) into
        // the request, matching the full-form GET-submit behaviour.
        assert!(
            html.contains(r#"hx-include="closest form""#),
            "search input must hx-include the form so live-search carries filters: {html}"
        );
    }

    #[test]
    fn list_page_url_encodes_filter_values() {
        // Filter values containing reserved chars must be percent-encoded
        // so they round-trip through the URL parser cleanly.
        use crate::traits::ListResult;
        let r = dummy_registry();
        let mut name = AdminField::new("name", AdminFieldKind::Text);
        name.sortable = true;
        let fields = vec![name];
        let result = ListResult {
            records: vec![],
            total: 0,
            page: 1,
            per_page: 25,
        };
        let active_filters = vec![("q".to_owned(), "a&b=c".to_owned())];
        let html = model_list_page(
            &r,
            "users",
            "Users",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &active_filters,
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        assert!(
            html.contains("filter.q=a%26b%3Dc"),
            "filter values must be percent-encoded in generated links: {html}"
        );
    }

    #[test]
    fn list_page_renders_bulk_action_form() {
        use crate::traits::{ActionStyle, ListResult};
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let actions = vec![
            AdminAction {
                name: "delete",
                label: "Delete selected".to_owned(),
                style: ActionStyle::Danger,
                confirm: true,
            },
            AdminAction {
                name: "archive",
                label: "Archive".to_owned(),
                style: ActionStyle::Default,
                confirm: false,
            },
        ];
        let result = ListResult {
            records: vec![serde_json::json!({"id": 1, "name": "x"})],
            total: 1,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "widgets",
            "Widgets",
            &fields,
            &actions,
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "tok",
            "admin_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        // Form posts to the bulk-action endpoint with the CSRF token.
        assert!(
            html.contains(r#"action="/admin/widgets/actions""#),
            "list view must wrap table in a form posting to /actions: {html}"
        );
        assert!(
            html.contains(r#"name="admin_csrf" value="tok""#),
            "configured CSRF token field must be in the bulk-action form: {html}"
        );
        assert!(!html.contains(r#"name="_csrf" value="tok""#));
        // Both action options appear, with the dangerous one tagged for
        // client-side confirm.
        assert!(html.contains(r#"value="delete""#));
        assert!(html.contains(r#"value="archive""#));
        assert!(
            html.contains(r#"data-confirm="1""#),
            "destructive action should set data-confirm: {html}"
        );
    }

    #[test]
    fn list_page_skips_action_bar_when_no_actions_declared() {
        use crate::traits::ListResult;
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let result = ListResult {
            records: vec![],
            total: 0,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "widgets",
            "Widgets",
            &fields,
            &[], // no actions
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        assert!(
            !html.contains("class=\"action-bar\""),
            "no action-bar should render when actions is empty: {html}"
        );
    }

    #[test]
    fn list_page_omits_sort_link_for_unsortable_fields() {
        use crate::traits::ListResult;
        let r = dummy_registry();
        // One sortable, one non-sortable.
        let mut computed = AdminField::new("computed", AdminFieldKind::Text).label("Computed");
        computed.sortable = false;
        let fields = vec![AdminField::new("name", AdminFieldKind::Text), computed];
        let result = ListResult {
            records: vec![],
            total: 0,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "widgets",
            "Widgets",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "tok",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        // Sortable field gets a sort link.
        assert!(
            html.contains(r#"href="/admin/widgets?sort=name"#),
            "sortable field should have a sort link: {html}"
        );
        // Non-sortable field must NOT get a sort link.
        assert!(
            !html.contains("sort=computed"),
            "non-sortable field must not emit a sort link: {html}"
        );
        // But its label is still rendered.
        assert!(
            html.contains("Computed"),
            "label should still render: {html}"
        );
    }

    #[test]
    fn list_page_shows_csv_download_link_when_export_enabled() {
        use crate::traits::ListResult;
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let result = ListResult {
            records: vec![],
            total: 0,
            page: 1,
            per_page: 25,
        };
        // supports_csv_export = true, supports_csv_import = false
        let html = model_list_page(
            &r,
            "widgets",
            "Widgets",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false, // show_config
            true,  // supports_csv_export
            false, // supports_csv_import
        )
        .into_string();
        assert!(
            html.contains(r#"href="/admin/widgets/export.csv""#),
            "Download CSV link must appear when supports_csv_export=true: {html}"
        );
        assert!(
            !html.contains("/import"),
            "Import CSV link must not appear when supports_csv_import=false: {html}"
        );
    }

    #[test]
    fn list_page_shows_import_link_when_import_enabled() {
        use crate::traits::ListResult;
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let result = ListResult {
            records: vec![],
            total: 0,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "widgets",
            "Widgets",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false, // show_config
            false, // supports_csv_export
            true,  // supports_csv_import
        )
        .into_string();
        assert!(
            html.contains(r#"href="/admin/widgets/import""#),
            "Import CSV link must appear when supports_csv_import=true: {html}"
        );
        assert!(
            !html.contains("export.csv"),
            "Download CSV link must not appear when supports_csv_export=false: {html}"
        );
    }

    #[test]
    fn list_page_hides_csv_buttons_when_both_disabled() {
        use crate::traits::ListResult;
        let r = dummy_registry();
        let fields = vec![AdminField::new("name", AdminFieldKind::Text)];
        let result = ListResult {
            records: vec![],
            total: 0,
            page: 1,
            per_page: 25,
        };
        let html = model_list_page(
            &r,
            "widgets",
            "Widgets",
            &fields,
            &[],
            &result,
            "",
            None,
            SortDirection::Asc,
            &[],
            &[],
            "t",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
            false,
            false,
            false,
        )
        .into_string();
        assert!(
            !html.contains("export.csv"),
            "no export link when disabled: {html}"
        );
        assert!(
            !html.contains("/import"),
            "no import link when disabled: {html}"
        );
    }

    #[test]
    fn admin_js_does_not_contain_inline_event_handlers() {
        // Sanity-check the shipped JS: has the two behaviours we expect.
        let js = include_str!("admin.js");
        assert!(
            js.contains("select-all"),
            "admin.js should wire the select-all checkbox"
        );
        assert!(
            js.contains("removeAttribute(\"name\")"),
            "admin.js should strip blank password input names"
        );
    }

    #[test]
    fn pagination_range_start_underflow_protection() {
        // The start calculation could previously panic in debug mode if current was 0.
        let result = crate::traits::ListResult {
            total: 10,
            per_page: 5,
            page: 0,
            records: vec![],
        };
        // render_pagination itself expects the request page, which is usually clamped to >=1,
        // but just to verify it won't panic if it somehow gets 0:
        let _ = render_pagination(
            &result,
            "y",
            "x",
            None,
            crate::traits::SortDirection::Asc,
            "",
            "",
        );
    }

    // ── Runtime config page tests ────────────────────────────────────────────

    #[test]
    fn config_page_empty_shows_no_keys_registered_message() {
        let r = dummy_registry();
        let html = config_page(
            &r,
            &[],
            &[],
            "tok",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(
            html.contains("No config keys have been registered"),
            "empty state message missing: {html}"
        );
        assert!(
            html.contains("Runtime Config"),
            "page title missing: {html}"
        );
    }

    #[test]
    fn config_page_renders_key_name_type_and_value() {
        use autumn_web::runtime_config::{ConfigEntry, ConfigValue, ConfigValueType};

        let r = dummy_registry();
        let entries = vec![ConfigEntry {
            name: "max_upload_mb".to_owned(),
            value_type: ConfigValueType::Int,
            current: ConfigValue::Int(50),
            default: ConfigValue::Int(50),
            is_overridden: false,
            description: Some("Max upload in MB".to_owned()),
        }];
        let html = config_page(
            &r,
            &entries,
            &[],
            "tok",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(html.contains("max_upload_mb"), "key name missing: {html}");
        assert!(
            html.contains("Max upload in MB"),
            "description missing: {html}"
        );
        assert!(
            html.contains(r#"action="/admin/config/max_upload_mb/set""#),
            "set form action missing: {html}"
        );
        assert!(
            html.contains(r#"href="/admin/config/max_upload_mb/history""#),
            "history link missing: {html}"
        );
    }

    #[test]
    fn config_page_overridden_key_shows_unset_form() {
        use autumn_web::runtime_config::{ConfigEntry, ConfigValue, ConfigValueType};

        let r = dummy_registry();
        let entries = vec![ConfigEntry {
            name: "rate_limit".to_owned(),
            value_type: ConfigValueType::Int,
            current: ConfigValue::Int(200),
            default: ConfigValue::Int(100),
            is_overridden: true,
            description: None,
        }];
        let html = config_page(
            &r,
            &entries,
            &[],
            "tok",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(
            html.contains(r#"action="/admin/config/rate_limit/unset""#),
            "unset form should appear for overridden key: {html}"
        );
    }

    #[test]
    fn config_page_shows_overridden_status() {
        use autumn_web::runtime_config::{ConfigEntry, ConfigValue, ConfigValueType};

        let r = dummy_registry();
        let entries = vec![ConfigEntry {
            name: "rate_limit".to_owned(),
            value_type: ConfigValueType::Int,
            current: ConfigValue::Int(200),
            default: ConfigValue::Int(100),
            is_overridden: true,
            description: None,
        }];
        let html = config_page(
            &r,
            &entries,
            &[],
            "tok",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(
            html.to_lowercase().contains("overridden"),
            "overridden status missing: {html}"
        );
    }

    #[test]
    fn config_page_shows_default_status_for_unoverridden_key() {
        use autumn_web::runtime_config::{ConfigEntry, ConfigValue, ConfigValueType};

        let r = dummy_registry();
        let entries = vec![ConfigEntry {
            name: "feature_flag".to_owned(),
            value_type: ConfigValueType::Bool,
            current: ConfigValue::Bool(false),
            default: ConfigValue::Bool(false),
            is_overridden: false,
            description: None,
        }];
        let html = config_page(
            &r,
            &entries,
            &[],
            "tok",
            "_csrf",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(
            html.to_lowercase().contains("default"),
            "default status missing: {html}"
        );
    }

    #[test]
    fn config_page_embeds_csrf_token_in_forms() {
        use autumn_web::runtime_config::{ConfigEntry, ConfigValue, ConfigValueType};

        let r = dummy_registry();
        let entries = vec![ConfigEntry {
            name: "timeout_secs".to_owned(),
            value_type: ConfigValueType::Int,
            current: ConfigValue::Int(30),
            default: ConfigValue::Int(30),
            is_overridden: false,
            description: None,
        }];
        let html = config_page(
            &r,
            &entries,
            &[],
            "csrf-tok-789",
            "authenticity_token",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(
            html.contains(r#"name="authenticity_token" value="csrf-tok-789""#),
            "CSRF token not embedded in config forms: {html}"
        );
    }

    #[test]
    fn config_history_page_shows_empty_state() {
        let r = dummy_registry();
        let html = config_history_page(
            &r,
            "rate_limit",
            &[],
            &[],
            "tok",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(
            html.contains("No changes recorded"),
            "empty history message missing: {html}"
        );
        assert!(html.contains("rate_limit"), "key name missing: {html}");
    }

    #[test]
    fn config_history_page_renders_change_records() {
        use autumn_web::runtime_config::{ConfigChangeRecord, ConfigValue};

        let r = dummy_registry();
        let history = vec![ConfigChangeRecord {
            key: "rate_limit".to_owned(),
            old_value: Some(ConfigValue::Int(100)),
            new_value: Some(ConfigValue::Int(200)),
            actor: Some("ops@example.com".to_owned()),
            timestamp_secs: 1_700_000_000,
        }];
        let html = config_history_page(
            &r,
            "rate_limit",
            &history,
            &[],
            "tok",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(html.contains("rate_limit"), "key name missing: {html}");
        assert!(html.contains("ops@example.com"), "actor missing: {html}");
        assert!(html.contains("100"), "old value missing: {html}");
        assert!(html.contains("200"), "new value missing: {html}");
    }

    #[test]
    fn config_history_page_handles_unset_record() {
        use autumn_web::runtime_config::{ConfigChangeRecord, ConfigValue};

        let r = dummy_registry();
        let history = vec![ConfigChangeRecord {
            key: "flag".to_owned(),
            old_value: Some(ConfigValue::Bool(true)),
            new_value: None,
            actor: None,
            timestamp_secs: 0,
        }];
        let html = config_history_page(
            &r,
            "flag",
            &history,
            &[],
            "tok",
            "X-CSRF-Token",
            "/admin",
            "/actuator",
        )
        .into_string();
        assert!(html.contains("flag"), "key name missing: {html}");
        // The null actor should render as a dash placeholder.
        assert!(html.contains("—"), "null actor placeholder missing: {html}");
    }

    #[test]
    fn format_timestamp_formats_unix_epoch() {
        let s = format_timestamp(0);
        assert!(s.contains("1970"), "epoch should format as 1970: {s}");
    }

    #[test]
    fn format_timestamp_formats_known_instant() {
        // 2023-11-14 22:13:20 UTC
        let s = format_timestamp(1_700_000_000);
        assert!(s.contains("2023"), "expected 2023 in formatted output: {s}");
    }

    // ── admin_layout nav (#1134) ─────────────────────────────────────────

    fn render_layout(active_slug: Option<&str>) -> String {
        let registry = AdminRegistry::new();
        admin_layout(
            &registry,
            active_slug,
            "Title",
            "/admin",
            "/actuator",
            "tok",
            "X-CSRF-Token",
            &[],
            true,
            &html! {},
        )
        .into_string()
    }

    #[test]
    fn admin_layout_dashboard_active_has_aria_current() {
        let html = render_layout(None);
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
        assert!(
            html.contains(r#"href="/admin" class="active" aria-current="page""#),
            "{html}"
        );
    }

    #[test]
    fn admin_layout_jobs_active_has_aria_current() {
        let html = render_layout(Some(JOBS_NAV_SLUG));
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
        assert!(
            html.contains(r#"href="/admin/jobs" class="active" aria-current="page""#),
            "{html}"
        );
    }

    #[test]
    fn admin_layout_runtime_config_active_has_aria_current() {
        let html = render_layout(Some(RUNTIME_CONFIG_NAV_SLUG));
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
        assert!(
            html.contains(r#"href="/admin/config" class="active" aria-current="page""#),
            "{html}"
        );
    }

    #[test]
    fn admin_layout_no_active_slug_has_no_dashboard_aria_current() {
        // active_slug = Some(...) for an unrelated slug: Dashboard must not
        // claim the active state, and nothing else should either since the
        // registry is empty (no model nav items).
        let html = render_layout(Some(JOBS_NAV_SLUG));
        assert!(
            !html.contains(r#"href="/admin" class="active""#),
            "dashboard must not be active: {html}"
        );
    }

    // ── admin_layout nav_bar (#1137) ──────────────────────────────────────

    use crate::registry::tests::DummyModel;

    fn render_layout_with_registry(registry: &AdminRegistry, active_slug: Option<&str>) -> String {
        admin_layout(
            registry,
            active_slug,
            "Title",
            "/admin",
            "/actuator",
            "tok",
            "X-CSRF-Token",
            &[],
            true,
            &html! {},
        )
        .into_string()
    }

    #[test]
    fn admin_layout_renders_nav_bar_sidebar() {
        let html = render_layout(None);
        assert!(html.contains("autumn-nav--sidebar"), "{html}");
        assert!(
            html.contains(r#"class="autumn-nav autumn-nav--sidebar admin-sidebar""#),
            "{html}"
        );
        assert!(html.contains(r#"aria-label="Admin navigation""#), "{html}");
    }

    #[test]
    fn admin_layout_brand_uses_nav_brand_class() {
        let html = render_layout(None);
        assert!(html.contains("autumn-nav__brand"), "{html}");
        assert!(html.contains("🍂 Autumn Admin"), "{html}");
        assert!(!html.contains(r#"class="admin-logo""#), "{html}");
    }

    #[test]
    fn admin_layout_sections_render_as_nav_sections_without_models() {
        let html = render_layout(None);
        assert!(
            html.contains(r#"<li class="autumn-nav__section">System</li>"#),
            "{html}"
        );
        assert!(!html.contains("Models"), "{html}");
    }

    #[test]
    fn admin_layout_sections_render_as_nav_sections_with_models() {
        let mut registry = AdminRegistry::new();
        registry.register(DummyModel {
            slug: "projects",
            name: "Projects",
        });
        let html = render_layout_with_registry(&registry, None);
        assert!(
            html.contains(r#"<li class="autumn-nav__section">Models</li>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<li class="autumn-nav__section">System</li>"#),
            "{html}"
        );
        assert!(html.contains(r#"href="/admin/projects""#), "{html}");
    }

    #[test]
    fn admin_layout_actuator_link_is_plain() {
        let html = render_layout(None);
        assert!(html.contains(r#"href="/actuator/ui""#), "{html}");
        // Only one aria-current in the whole page: Dashboard's. The Actuator
        // link must never claim active state even though it's the last item.
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
    }

    #[test]
    fn admin_layout_has_no_hand_rolled_nav_markup() {
        let html = render_layout(None);
        assert!(!html.contains(r#"class="admin-nav""#), "{html}");
        assert!(!html.contains("admin-logo"), "{html}");
    }

    #[test]
    fn admin_layout_loads_nav_bar_widget_runtime() {
        // nav_bar's hamburger toggle and any future dropdown depend on
        // autumn-widgets.js to reveal/wire them up; without it the toggle
        // stays permanently hidden and dead.
        let html = render_layout(None);
        assert!(
            html.contains(autumn_web::htmx::AUTUMN_WIDGETS_JS_PATH),
            "{html}"
        );
    }

    #[test]
    fn admin_sidebar_toggle_stays_hidden() {
        // The admin sidebar hides itself entirely below 768px (see the
        // .admin-sidebar { display: none } media rule) rather than using
        // nav_bar's own collapse-behind-a-hamburger UX, so the toggle must
        // never become visible even once autumn-widgets.js unhides it.
        assert!(ADMIN_CSS.contains(".autumn-nav__toggle"), "{ADMIN_CSS}");
    }
}
