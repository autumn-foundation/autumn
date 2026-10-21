# Autumn display/layout/navigation widget layer — inventory (HEAD 6104f0c, 2026-10-21)

Sources are the local repo at `/home/user/autumn` (read-only). Citations are `path:Lline` links into that repo. "Shipped" = in `autumn/src` at HEAD; "Absent" = not found by grep over `autumn/src/widgets.rs`, `autumn/src/ui/`, `autumn/src/a11y.rs`, `autumn/src/form.rs`, `autumn/src/links.rs`, `autumn/src/format.rs`, `autumn/vendor/autumn-widgets.js`. Every widget in `widgets.rs` is `#[cfg(feature = "maud")]` and returns `maud::Markup`.

## Q1. Full catalog of shipped widgets, by category

### Takeaway
Autumn ships about 35 public render functions in `autumn_web::widgets`, plus about 10 more spread across `ui::pagination`, `flash`, `form`, `wizard`, `presence`, `consent`, `links` and `format`. They are server-rendered and work without JS. The set leans toward admin, CRUD and social-app needs: tables, badges, a modal/confirm, toasts, a nav bar, tabs, SVG charts, comments, votes, bulk actions and an infinite feed. The general-purpose overlay and input primitives that make up most of shadcn/Radix are not there.

### Cited Findings

**Module-level self-description**
- The module doc says every widget returns `maud::Markup`, is composable, CSP-safe (no inline JS) and HTML-escapes caller text. It indexes Display, Feedback, Interactive/search, Bulk actions and Modals groups. — [autumn/src/widgets.rs:L1-L103](../../../../autumn/src/widgets.rs)

**Layout / containers**
- `card(body: &Markup, &CardConfig)`. `CardConfig` setters: `title`, `title_html`, `level(HeadingLevel)`, `header_action(Markup)`, `footer(Markup)`, `class`. Hooks: `.autumn-card`, `__header`, `__title`, `__body`, `__footer`. No JS. — [autumn/src/widgets.rs:L3242-L3320, L3415](../../../../autumn/src/widgets.rs)
- `HeadingLevel` enum (H1–H6, default H2) is shared by card, hero and modal. — [autumn/src/widgets.rs:L3207](../../../../autumn/src/widgets.rs)
- `stat_card(label, value, link: Option<(&str,&str)>)` is a metric tile. It has no config struct, no delta/trend and no icon. — [autumn/src/widgets.rs:L3471-L3488](../../../../autumn/src/widgets.rs)
- `hero(&HeroConfig)`. `HeroConfig::new(title)` plus `title_html`, `level`, `subtitle`, `subtitle_html`, `cta(Cta)` (repeatable) and `class`. `Cta::primary` / `Cta::secondary` (`CtaStyle`) render a `<section class="autumn-hero">` with CTA anchors. — [autumn/src/widgets.rs:L3490-L3700](../../../../autumn/src/widgets.rs)
- `property_list(rows: &[(&str, Markup)])` renders a `<dl class="autumn-property-list">` for record detail views. — [autumn/src/widgets.rs:L695](../../../../autumn/src/widgets.rs)

**Navigation**
- `nav_link(current_path, href, label)` and `nav_link_matched(..., NavLinkMatch::{Exact, Prefix})` emit `class="autumn-active"` plus `aria-current="page"` when active. Prefix matching respects `/` segment boundaries. — [autumn/src/widgets.rs:L2602-L2661](../../../../autumn/src/widgets.rs)
- `nav_bar(current_path, &NavBarConfig)`:
  - `NavBarConfig` setters: `brand`, `brand_html`, `item`, `items`, `trailing`, `aria_label`, `layout(NavBarLayout::{Horizontal, Sidebar})`, `id`, `toggle_label`, `class`.
  - `NavItem::{link, link_matched, plain_link, menu(NavMenu), section(label)}`.
  - `NavMenu` is a dropdown: a `<button aria-expanded aria-controls>` plus a `<ul>`. It is a disclosure, not an ARIA `role="menu"`.
  - It renders a hamburger toggle that stays hidden until JS runs. Without JS, every dropdown renders open.

  — [autumn/src/widgets.rs:L2779-L3195](../../../../autumn/src/widgets.rs)
- The nav bar's mobile collapse is a CSS breakpoint at `max-width: 768px`, applied only to `.autumn-nav--enhanced`. — [autumn/src/ui/widgets.css:L493](../../../../autumn/src/ui/widgets.css)
- `breadcrumb(&[Crumb])`, with `Crumb::link(label, href)` and `Crumb::current(label)`. Renders `<nav aria-label="Breadcrumb">` > `<ol>`, an `aria-hidden` "›" separator, and `aria-current="page"` on the current crumb. — [autumn/src/widgets.rs:L2517-L2592](../../../../autumn/src/widgets.rs)
- `locale_switcher(...)` plus `localized_path(path, locale)` render `<nav aria-label="Language">`, with `aria-current="true"` on the current locale (#1251). — [autumn/src/widgets.rs:L2725-L2770](../../../../autumn/src/widgets.rs)
- `tabs(id, active_id: Option<&str>, panels: &[(&str, &str, Markup)])`:
  - Tabs are `<a href="#panel">` links. Switching is pure CSS `:target` plus `:has()`.
  - ARIA: `role="tablist"`, `role="tab"` with `aria-controls`/`aria-selected`, and `role="tabpanel"` with `aria-labelledby` and `tabindex="0"`.

  — [autumn/src/widgets.rs:L4220-L4375](../../../../autumn/src/widgets.rs)
- `pagination_nav(&Page<T>, &PagerOptions)` and `cursor_pagination_nav(&CursorPage<T>, &PagerOptions)`:
  - `PagerOptions` setters: `new(base_path)`, `query` (preserves filters), `page_param`, `size_param`, `cursor_param`, `window(radius)`, `include_size`, `aria_label`, `hx_target` (htmx opt-in).
  - Links are plain `<a href>` by default.

  — [autumn/src/ui/pagination.rs:L1-L50, L69-L175, L316, L382](../../../../autumn/src/ui/pagination.rs)
- `skip_link(target, label)` renders `<a class="skip-link">`. — [autumn/src/form.rs:L3247](../../../../autumn/src/form.rs)

**Data display**
- `data_table(rows: &[T], columns: &[Column<T>], &DataTableConfig)`:
  - `Column::new(header, cell: Fn(&T) -> Markup)`, with `.sortable(sort_key)`.
  - `DataTableConfig` fields: `caption`, `empty_message`, `base_path`, `query`, `sort_param`/`dir_param`/`page_param`, `active_sort`, `active_dir(SortDir)`, `class`.
  - Sorting is server-side: header `<a>` links with `aria-sort`. Headers use `th scope="col"`. The empty state is `span role="status"`.

  — [autumn/src/widgets.rs:L1880-L2210](../../../../autumn/src/widgets.rs)
- `badge(label, BadgeVariant)`, `badge_with(label, variant, &BadgeConfig{title, aria_label, class})` and `status_tag(label)` (always Neutral).
  - `BadgeVariant::{Neutral, Info, Success, Warning, Danger}`.
  - `BadgeVariant::for_label(&str)` maps common status words to a variant and FNV-hashes any other word to a stable color.
  - Emits an unprefixed `class="badge badge--<variant>"`.

  — [autumn/src/widgets.rs:L4381-L4545](../../../../autumn/src/widgets.rs)
- `avatar(name, &AvatarConfig{image, size, class})`:
  - `AvatarSize::{Small=24, Medium=40, Large=64}` px.
  - With an image: `<img loading=lazy alt=name width/height>`.
  - Without an image: an initials badge with `role="img"` and `aria-label`, colored from a 12-class palette chosen by a deterministic hash.
  - There is no avatar group/stack and no presence dot.

  — [autumn/src/widgets.rs:L4552-L4760](../../../../autumn/src/widgets.rs)
- Charts (#1231):
  - Functions: `sparkline`/`sparkline_with`, `bar_chart`/`bar_chart_with`, `line_chart`/`line_chart_with`. Each takes `&[(&str, f64)]`.
  - `ChartConfig` setters: `class`, `title` (aria-label and `<title>`), `desc`, `min`, `max`, `with_table` (a visually hidden data table).
  - Output is inline `<svg role="img">` with no script and no `style=`.
  - Single series only. There are no axes/ticks, legend, hover tooltips, pie/donut, area or stacked charts.

  — [autumn/src/widgets.rs:L5510-L6040](../../../../autumn/src/widgets.rs)
- `presence_badge(count)` renders `<span id="presence-badge">N online</span>`, swapped by SSE OOB. — [autumn/src/presence.rs:L486-L497](../../../../autumn/src/presence.rs)
- Formatting helpers that return Markup: `number_to_currency`, `number_with_delimiter`, `pluralize`/`pluralize_with`, `truncate`/`truncate_words`, `time_ago_in_words`, `format_datetime`. — [autumn/src/format.rs:L110-L384](../../../../autumn/src/format.rs)
- Job status fragment (`fn status_fragment` is private and rendered by the job-tracking endpoint):
  - `<progress class="autumn-job-status__bar" value max=100>`.
  - It polls with `hx-trigger="every 2s"` until the job reaches a terminal state.

  — [autumn/src/job_tracking.rs:L1405-L1420](../../../../autumn/src/job_tracking.rs)
- `wizard_progress(ctx, current_step)` (async) is a step-progress stepper: `<nav aria-label="Progress">`, `<ol role="list" class="wizard-progress">`, `aria-current="step"`, and an `aria-live="polite"` summary. — [autumn/src/wizard.rs:L403-L446](../../../../autumn/src/wizard.rs)

**Feedback / overlay**
- `alert(variant, body)` and `alert_with(variant, body, &AlertConfig{title, icon, dismissible, class})`:
  - `AlertVariant::{Info, Success, Warning, Error}`. Error and Warning get `role="alert"`; Info and Success get `role="status"`.
  - The icon is inline SVG with `aria-hidden`.
  - Dismiss is a CSS-only `<label>`-wrapped checkbox plus a `:has(:checked)` hide rule. It is not persisted across reloads.

  — [autumn/src/widgets.rs:L4766-L4955](../../../../autumn/src/widgets.rs); [autumn/src/ui/widgets.css:L1152-L1171](../../../../autumn/src/ui/widgets.css)
- `error_summary(&Changeset<T>) -> Option<Markup>` renders an Error alert with a sorted `<ul>` of messages. The messages are plain text, not links to the fields. — [autumn/src/widgets.rs:L4979-L5010](../../../../autumn/src/widgets.rs)
- Toasts:
  - `toast_region(id)`, `toast(msg, AlertVariant)`, `toast_in(region_id, msg, variant)`, with `DEFAULT_TOAST_REGION_ID = "toast-region"`.
  - A toast is appended through `hx-swap-oob="beforeend:#toast-region"`.
  - The region is persistent with `aria-live="polite"` and `aria-atomic="false"`. Error toasts carry their own `role="alert"`.

  — [autumn/src/widgets.rs:L5015-L5200](../../../../autumn/src/widgets.rs)
- Toast auto-dismiss is CSS-only: a fade-out at a fixed 5s (`autumn-toast-out 400ms ease-in 5s`), with a `prefers-reduced-motion` variant. The CSS has no pause-on-hover/focus rule, and the toast has no close button or action slot. — [autumn/src/ui/widgets.css:L1201-L1290](../../../../autumn/src/ui/widgets.css)
- `flash_messages(&[FlashMessage])`, `flash_messages_with(...)` and `flash_message_divs(...)`. `FlashLevel` is success/error/warning/info, with per-severity role/aria-live. `FLASH_CSS` is a separate token-based stylesheet. — [autumn/src/flash.rs:L43, L214-L340](../../../../autumn/src/flash.rs); [autumn/src/ui/tokens.rs:L33-L46](../../../../autumn/src/ui/tokens.rs)
- Modal:
  - `modal(id, title, body, &ModalConfig{footer, level, class, light_dismiss})` renders a native `<dialog role="dialog" aria-modal="true" aria-labelledby>`. `light_dismiss` emits `closedby="any"`.
  - `modal_trigger(label, dialog_id, class)` uses the Invoker Commands API (`command="show-modal" commandfor`) plus a `data-modal-open` fallback. `modal_close_button` does the same with `command="close"`.
  - There is no size variant, no built-in close "×" button and no side/drawer placement.

  — [autumn/src/widgets.rs:L3704-L3890](../../../../autumn/src/widgets.rs)
- `confirm_action(id, trigger_label, action, method, csrf_token, &ConfirmActionConfig)`:
  - Config setters: `title`, `message`, `confirm_label`, `cancel_label`, `trigger_class`, `confirm_class`, `danger` (default true), `csrf_field`, `attrs`, `level`, `modal_class`, `light_dismiss`.
  - The confirm button is a `button_to_with` form (`_method` override plus CSRF).
  - A `<noscript>` fallback renders an unconfirmed submit.
  - It is positioned as the replacement for `hx-confirm` / `window.confirm`.

  — [autumn/src/widgets.rs:L3893-L4228](../../../../autumn/src/widgets.rs)
- `aria_live_region(id, message)` renders `role="status" aria-live="polite" aria-atomic="true"`. — [autumn/src/form.rs:L3225](../../../../autumn/src/form.rs)
- `consent_banner_markup(csrf_token, field)` renders a sticky `<section role="region" aria-label="Cookie consent">` whose Accept and Reject buttons share one class (an anti-dark-pattern choice). — [autumn/src/consent/banner.rs:L76-L97](../../../../autumn/src/consent/banner.rs); [autumn/src/ui/widgets.css:L1489-L1559](../../../../autumn/src/ui/widgets.css)

**Search / selection (htmx)**
- `active_search(id, label, &ActiveSearchConfig)`, `active_search_input`, `active_search_results` and `active_search_empty_state`:
  - Config setters: `new(action, target)`, `post()` (`SearchMethod`), `debounce(ms)`, `min_length`, `indicator`, `initial_load`, `placeholder`, `param_name`.
  - Results go to a `role="status" aria-live="polite"` container. A `<noscript>` GET form is the fallback.

  — [autumn/src/widgets.rs:L105-L500](../../../../autumn/src/widgets.rs)
- `autocomplete_input(id, label, &AutocompleteConfig)`, `autocomplete_option(value, label)` and `autocomplete_empty_state`:
  - Config setters: `new(action, value_name)`, `debounce`, `min_length`, `indicator`, `query_param`, `placeholder`, `fallback_options` (for a noscript `<select>`), `free_text`, `initial_value`.
  - A hidden input carries the selected ID. This is single-select only: there is no multi-select or tags mode.

  — [autumn/src/widgets.rs:L227-L660](../../../../autumn/src/widgets.rs)

**Domain / social / workflow widgets**
- `reaction_controls(&ReactionControls)`:
  - Constructors and setters: `ReactionControls::votes(..)`, `::likes(dom_id, action)`, `aggregate`, `current`, `label`, `up_label`, `down_label`, `like_label`, `hx_target`, `csrf`, `preserve_pressed_state`.
  - Renders no-JS POST forms with `aria-pressed` toggles, `role="group"`, and an aggregate count marked `aria-live="polite"`. It pairs with `#[votable]`.

  — [autumn/src/widgets.rs:L911-L1322](../../../../autumn/src/widgets.rs)
- `comment_thread(&CommentThread, &[CommentView])`, `CommentView::from_thread` and `comment_dom_id`:
  - Renders `<section role="region">` with nested `<ol>` levels (`max_depth`, default 5).
  - Each reply form sits in a `<details>`/`<summary>` disclosure. Setters include `read_only`, `sign_in_prompt` and `draft`. Errors render with `role="alert"`.

  — [autumn/src/widgets.rs:L1324-L1860](../../../../autumn/src/widgets.rs)
- `transition_controls(action, field, current, transitions, can, csrf, csrf_field)` and `transition_controls_with_labels(..., &TransitionLabels)` render `role="group"` with one POST form per legal state-machine edge. Guard-failing edges get `disabled`. — [autumn/src/widgets.rs:L720-L905](../../../../autumn/src/widgets.rs)
- Bulk actions (#1312): `bulk_select_checkbox`, `bulk_actions_toolbar` (`role="group"`), `bulk_actions_form` (CSRF plus submit-token) and `BulkActionsConfig`. There is no framework "select all" control; grep finds none in widgets.rs. — [autumn/src/widgets.rs:L2213-L2515](../../../../autumn/src/widgets.rs)
- Infinite feed (#1372):
  - `infinite_feed(items, next_cursor, &FeedConfig)` and `feed_page(...)`. `FeedConfig` setters: `new(url)`, `mode(FeedMode::{Reveal, Button})`, `button()`, `cursor_param`, `load_more_label`.
  - The sentinel is an `<a href>` that also carries `hx-get` with `hx-trigger="revealed, click"`.

  — [autumn/src/widgets.rs:L5200-L5508](../../../../autumn/src/widgets.rs)

**Form-adjacent (covered here only for presence/absence)**
- `FieldControl` variants: Text, Textarea, Password, Number{step}, Checkbox, Date, DateTime, Select{options}, File. All are native inputs. — [autumn/src/form.rs:L3265-L3293](../../../../autumn/src/form.rs)
- The rich-text editor is a Markdown `<textarea>` with a syntax-hint "toolbar" and an optional htmx live preview. It needs no JS and is not WYSIWYG. — [autumn/src/form.rs:L1954-L1965](../../../../autumn/src/form.rs); [autumn/src/ui/widgets.css:L174-L270](../../../../autumn/src/ui/widgets.css)
- `direct_upload_input(name, presign_url, accept)` renders a file input with a hidden-until-JS progress bar. — [autumn/src/storage/form_helper.rs:L20-L90](../../../../autumn/src/storage/form_helper.rs)
- `link_to`/`link_to_with` and `button_to`/`button_to_with` produce method-override plus CSRF forms. — [autumn/src/links.rs:L259-L452](../../../../autumn/src/links.rs)

### Inferences
- A practical count of shipped display/nav/feedback primitives comparable to a component library:
  - card, stat card, hero, property list (description list)
  - nav link, nav bar (with a dropdown disclosure and sidebar layout), breadcrumb, locale switcher, tabs, pagination (offset and cursor), skip link
  - data table, badge/status tag, avatar, three chart types, presence badge, progress (job status only), stepper (wizard only)
  - alert/callout, error summary, toast, flash, modal, confirm dialog, live region, cookie banner
  - active search, autocomplete (single select)
  - the social/workflow set: reactions, comments, state transitions, bulk actions, infinite feed

  That is about 30 concepts.
- Several widgets are domain-coupled (comments, votes, state transitions, bulk actions). They have no shadcn/MUI equivalent and reflect Autumn's Rails/Phoenix-style full-stack positioning.

### Gaps
- `nav_bar`, `hero` and `property_list` have no dated CHANGELOG entry (grep found none), so their introduction release is unconfirmed.

## Q2. Explicit presence or absence of common component-library primitives

### Takeaway
Modal/dialog, confirm dialog, tabs, toast, alert, badge, avatar, breadcrumb, pagination, a dropdown nav disclosure, combobox-style autocomplete, sparkline/bar/line charts and empty states (search only) are present. Popover, tooltip, accordion, drawer/sheet, menu/menubar/context menu, command palette, date picker/calendar, slider, switch, spinner, skeleton, carousel, tree, kbd, separator, timeline, rating, dropzone, color picker, code block, copy button, resizable, scroll area, virtual list and hover card are all absent as framework widgets.

### Cited Findings
- **Present:**
  - Modal/dialog: `modal`, `modal_trigger`, `modal_close_button`. — [autumn/src/widgets.rs:L3817-L3890](../../../../autumn/src/widgets.rs)
  - Alert dialog: `confirm_action`. — [autumn/src/widgets.rs:L4143](../../../../autumn/src/widgets.rs)
  - Tabs: `tabs`. — [autumn/src/widgets.rs:L4309](../../../../autumn/src/widgets.rs)
  - Toast: `toast`. — [autumn/src/widgets.rs:L5131](../../../../autumn/src/widgets.rs)
  - Alert/callout: `alert_with`. — [autumn/src/widgets.rs:L4919](../../../../autumn/src/widgets.rs)
  - Badge, chip/tag: `badge`, `status_tag`. — [autumn/src/widgets.rs:L4517-L4545](../../../../autumn/src/widgets.rs)
  - Avatar: `avatar`. — [autumn/src/widgets.rs:L4723](../../../../autumn/src/widgets.rs)
  - Breadcrumb: `breadcrumb`. — [autumn/src/widgets.rs:L2569](../../../../autumn/src/widgets.rs)
  - Pagination: `pagination_nav`. — [autumn/src/ui/pagination.rs:L316](../../../../autumn/src/ui/pagination.rs)
  - Navigation menu / sidebar: `nav_bar` with `NavMenu` and `NavBarLayout::Sidebar`. — [autumn/src/widgets.rs:L2779-L2830](../../../../autumn/src/widgets.rs)
  - Combobox (single-select, server-filtered): `autocomplete_input`. — [autumn/src/widgets.rs:L538](../../../../autumn/src/widgets.rs)
  - Select: native `<select>` via `FieldControl::Select` and `a11y::Select`. — [autumn/src/form.rs:L3286](../../../../autumn/src/form.rs); [autumn/src/a11y.rs:L1219](../../../../autumn/src/a11y.rs)
  - Date input: native `<input type=date/datetime-local>` only. — [autumn/src/form.rs:L2903-L3090](../../../../autumn/src/form.rs)
  - Progress: native `<progress>` only, inside the job-status and upload fragments. — [autumn/src/job_tracking.rs:L1420](../../../../autumn/src/job_tracking.rs)
  - Stepper: `wizard_progress`. — [autumn/src/wizard.rs:L418](../../../../autumn/src/wizard.rs)
  - Collapsible: native `<details>` used only inside `comment_thread`. — [autumn/src/widgets.rs:L1802](../../../../autumn/src/widgets.rs)
  - Empty state: only search/autocomplete-specific variants plus the `data_table` `empty_message`. — [autumn/src/widgets.rs:L498, L643](../../../../autumn/src/widgets.rs)
  - Charts: `sparkline`, `bar_chart`, `line_chart`. — [autumn/src/widgets.rs:L5836-L6021](../../../../autumn/src/widgets.rs)
  - Separator: only the breadcrumb `›` glyph. — [autumn/src/widgets.rs:L2569-L2592](../../../../autumn/src/widgets.rs)
  - Tooltip: only via the native `title` attribute on `badge_with`. — [autumn/src/widgets.rs:L4462](../../../../autumn/src/widgets.rs)
- **Absent.** A grep over widgets.rs, ui/, a11y.rs, form.rs, links.rs, format.rs and autumn-widgets.js returned 0 hits for: `popover`, `accordion`, `drawer`, `menubar`, `role="menu"`, `datepicker`, `calendar`, `slider`, `type="range"`, `carousel`, `kbd`, `<hr`, `timeline`, `dropzone`, `resizable`, scroll area, `virtual`, hover card, context menu, command palette, `aria-busy`, code block / `<pre`. Hits for `spinner`, `skeleton`, `rating`, `sheet` and `switch` were all unrelated (test fixture `#spinner`, the word "skeleton" in form.rs comments, `separating`, "stylesheet", locale "switcher"). — grep run at HEAD; e.g. [autumn/src/widgets.rs:L6908](../../../../autumn/src/widgets.rs) (`#spinner` is a test selector)
- **Not a full widget:** `a11y::MenuItem` emits `role="menuitem"` (button or link), but no `role="menu"` container or menu keyboard model exists. — [autumn/src/a11y.rs:L410-L512](../../../../autumn/src/a11y.rs)
- **Not a full widget:** copy-to-clipboard, switch/toggle and segmented control are absent. The only toggle semantics are `aria-pressed` buttons inside `reaction_controls`. — [autumn/src/widgets.rs:L1173](../../../../autumn/src/widgets.rs)
- **Escape hatch for rich client widgets:** the docs present WASM "islands" (Yew) as a hand-wired recipe, not a subsystem ("There is no `autumn generate island`"). — [docs/guide/wasm-islands.md:L1-L22](../../../../docs/guide/wasm-islands.md)

### Inferences
- The biggest gaps against shadcn/Radix/MUI/Ant are the floating-UI family (popover, tooltip, dropdown menu with menu semantics, hover card, context menu), disclosure layouts (accordion, collapsible as a standalone widget, drawer/sheet), rich inputs (date picker, slider, switch, multi-select/tags combobox, file dropzone, color picker), and loading states (spinner, skeleton, standalone progress).
- Phoenix core components (`modal`, `flash`, `table`, `list`, `header`, `button`, `input`, `simple_form`, `back`) are roughly matched or exceeded, because Phoenix's own core set is also small. This is an inference from general knowledge of Phoenix; I did not verify it in this repo.

### Gaps
- I found no roadmap or planned-widget documents. `docs/plans/` (75 files) contains no plan for popover, tooltip, accordion, drawer, date picker, combobox or dark mode; the only grep hits were unrelated uses of "skeleton" and "dropdown". The open-issue scan via the GitHub REST API (3 pages of open issues) found no widget-feature issues. The only UI-related open issue is #2666, "a11y: an empty accessible name satisfies the typestate…". Closed issues and issue labels were not checked.

## Q3. Theming: tokens, dark mode, density, customization hooks

### Takeaway
Theming is a small flat set of about 18 CSS custom properties on `:root`. They are light-mode only, with no dark tokens in the widget stylesheet, no density/size scale and no spacing or typography scale. Customization is BEM-style `autumn-*` class hooks plus an optional extra `class` on most config structs. No slots or headless/unstyled variants exist, and markup cannot be overridden except by not using the widget.

### Cited Findings
- `tokens.css` defines `--bg`, `--surface`, `--text`, `--text-muted`, `--border`, `--primary`, `--primary-hover`, `--primary-light`, `--danger`, `--danger-hover`, `--danger-light`, `--success`, `--success-light`, `--warning`, `--warning-light`, `--radius`, `--shadow` and `--font-family`. There are no spacing, font-size, z-index or breakpoint tokens. — [autumn/src/ui/tokens.css:L1-L20](../../../../autumn/src/ui/tokens.css)
- "The token set is light-mode only — surfaces that need dark mode should add their own `@media (prefers-color-scheme: dark)` overrides." — [autumn/src/ui/tokens.rs:L6-L7](../../../../autumn/src/ui/tokens.rs)
- `widgets.css` (1,868 lines) contains no `prefers-color-scheme` rule. In `autumn/src`, only the error-page fallback styles and the GDPR portability viewer have dark-mode blocks. — [autumn/src/error_pages/defaults.rs:L173-L182](../../../../autumn/src/error_pages/defaults.rs)
- The stylesheet is served at `/static/css/autumn-widgets.css` as `WIDGETS_CSS` (tokens plus components). `WIDGETS_COMPONENT_CSS` is the component rules without tokens. The documented re-theme path is to override tokens on `:root`, "not by forking these rules". — [autumn/src/ui/widgets_css.rs:L1-L46](../../../../autumn/src/ui/widgets_css.rs)
- The guide documents linking the stylesheet once, re-theming via tokens, and overriding a specific widget through its documented class hooks ("there's no ceiling"). No Tailwind build is required. — [docs/guide/widget-styling.md:L12-L58](../../../../docs/guide/widget-styling.md)
- Contrast fix: success and warning chip text `color-mix`es toward a fixed dark, and a dark theme may set `--success-text` / `--warning-text`. The fragment says chips "keep their light tint in dark themes". — [changelog.d/wayfinder-widget-text-contrast.md](../../../../changelog.d/wayfinder-widget-text-contrast.md); [autumn/src/ui/widgets.css:L1020-L1027](../../../../autumn/src/ui/widgets.css)
- Most config structs accept an extra class through `merge_class`: `CardConfig`, `HeroConfig`, `ModalConfig`, `NavBarConfig`, `BadgeConfig`, `AvatarConfig`, `AlertConfig`, `ChartConfig`, `DataTableConfig`, and `ConfirmActionConfig` (`trigger_class`, `confirm_class`, `modal_class`). — [autumn/src/widgets.rs:L3324](../../../../autumn/src/widgets.rs)
- Class naming is inconsistent. Most hooks are `autumn-*` (renamed in 0.8.0, #2354), but alert (`alert alert-<v>`, `alert__*`) and badge (`badge badge--<v>`) roots stay unprefixed, and the wizard uses `wizard-*`. — [CHANGELOG.md:L52-L62](../../../../CHANGELOG.md); [autumn/src/widgets.rs:L4526, L4925](../../../../autumn/src/widgets.rs)
- No inline `style=` is ever emitted. Per-name avatar colors are palette classes so they survive a nonce-based CSP. — [autumn/src/widgets.rs:L4660-L4697](../../../../autumn/src/widgets.rs)
- The admin plugin ships its own duplicated `tokens.css`, which adds `--success-text`, `--warning-text` and `--autumn-danger` aliases. — [autumn-admin-plugin/src/tokens.css:L1-L35](../../../../autumn-admin-plugin/src/tokens.css)
- Avatar has three fixed sizes; no other widget has size or density variants. Badge has five color variants, alert four, and `CtaStyle` two (primary/secondary). There is no general Button component with variants in `widgets.rs`; `a11y::Button` is a semantic primitive. — [autumn/src/widgets.rs:L4552](../../../../autumn/src/widgets.rs); [autumn/src/a11y.rs:L182-L209](../../../../autumn/src/a11y.rs)

### Inferences
- Compared with shadcn (CSS-variable themes, including dark, plus copy-in source), MUI (a theme object with palette, typography, spacing, breakpoints and component overrides) and Primer (multi-mode primitives), Autumn's theming is minimal. It has no dark mode out of the box, no spacing or typography scale, no variant/size API and no slot overrides.

### Gaps
- I did not verify whether `--success-text` is consumed by the badge and alert rules in all states beyond the cited lines.

## Q4. Composition model and API consistency

### Takeaway
Every widget is a free function returning `maud::Markup`. Optional settings go through `XConfig` builder structs, mostly `const fn` chained setters. Children are passed as `Markup` arguments ("slots": body, footer, header_action, title_html, message). The API is broadly consistent but varies in borrowed vs owned config, tuple-slice inputs, and functions that have no config at all.

### Cited Findings
- The module doc says "All widgets return `maud::Markup` and are composable — pass the output of one as the body of another." — [autumn/src/widgets.rs:L3-L5](../../../../autumn/src/widgets.rs)
- Builder pattern with borrowed lifetimes (`CardConfig<'a>`, `ActiveSearchConfig<'a>` with `const fn` setters) versus owned `String` builders (`NavBarConfig`, `HeroConfig`, `ModalConfig`, `ReactionControls`, `CommentThread`). — [autumn/src/widgets.rs:L131-L220, L2915-L3030, L3558-L3640, L934-L1120](../../../../autumn/src/widgets.rs)
- Tuple-slice inputs instead of structs: `tabs(&[(id, label, Markup)])`, `property_list(&[(&str, Markup)])`, charts `&[(&str, f64)]`, and `transition_controls(&[(&str, &str, Option<&str>)])`. — [autumn/src/widgets.rs:L695, L784, L4309, L5836](../../../../autumn/src/widgets.rs)
- Positional-only widgets with no config: `stat_card(label, value, link)` and `modal_trigger(label, id, class: Option<&str>)`. — [autumn/src/widgets.rs:L3471, L3852](../../../../autumn/src/widgets.rs)
- Shorthand plus `_with` pairs: `badge`/`badge_with`, `alert`/`alert_with`, `sparkline`/`sparkline_with`, `toast`/`toast_in`. — [autumn/src/widgets.rs:L4517-L4525, L4890-L4919, L5131-L5152](../../../../autumn/src/widgets.rs)
- Typed a11y primitives use typestate builders implementing `maud::Render` (`TextField<NoLabel>` vs `<Labeled>`), which is a different composition style from widgets.rs. — [autumn/src/a11y.rs:L1-L90, L515-L625](../../../../autumn/src/a11y.rs)
- Several widgets trust caller-supplied `dom_id` values as raw ids (documented "# `dom_id` is trusted"). — [autumn/src/widgets.rs:L911, L1383](../../../../autumn/src/widgets.rs)

### Inferences
- Markup-level customization works through class hooks and the `Markup` slots. There is no render-prop, `asChild` or element-override mechanism, so changing a widget's internal structure means hand-writing the HTML.

### Gaps
- None material.

## Q5. Accessibility: affordances, keyboard, focus, testing

### Takeaway
Autumn's accessibility strengths are landmarks, roles, live regions, labels-by-construction (the typed `a11y` primitives, proven by trybuild), and a CLI linter (`autumn check --a11y`, `autumn a11y verify`). Keyboard interaction models are thin:
- Tabs have no arrow-key roving focus, and `aria-selected` is static.
- Autocomplete has no arrow-key navigation and `aria-expanded` never changes.
- The nav dropdown has only Escape and click-outside.
- The modal relies on native `<dialog>` for its focus trap and focus return.

No axe-core automation runs in CI.

### Cited Findings
- **Modal:** ESC-close, focus trap, focus-into and focus-return come "for free from the browser" via `showModal()`. Opening uses the Invoker Commands API or the JS fallback. — [autumn/src/widgets.rs:L3786-L3800](../../../../autumn/src/widgets.rs); [autumn/vendor/autumn-widgets.js:L200-L240](../../../../autumn/vendor/autumn-widgets.js)
- **Tabs:** `aria-selected` reflects only the server default. The docs call this "a known, accepted limitation… a screen reader always hears the first tab announced as 'selected'". Tabs are `<a>` links with no arrow-key handling. The active-tab highlight syncs only for the first 6 tabs. — [autumn/src/widgets.rs:L4243-L4258](../../../../autumn/src/widgets.rs); [autumn/src/ui/widgets.css:L840-L860](../../../../autumn/src/ui/widgets.css)
- **Autocomplete:**
  - Markup: `role="combobox"`, a hard-coded `aria-expanded="false"`, `aria-autocomplete="list"` and `aria-controls`. The listbox has `aria-live="polite"`. Options are `role="option" tabindex="0"`.
  - JS: handles click plus Enter/Space on a focused option. No `ArrowDown`, `aria-activedescendant` or `aria-expanded` update exists in autumn-widgets.js; the only `aria-expanded` writes are for the nav.

  — [autumn/src/widgets.rs:L538-L640](../../../../autumn/src/widgets.rs); [autumn/vendor/autumn-widgets.js:L37-L105, L125-L186](../../../../autumn/vendor/autumn-widgets.js)
- **Nav bar:** the hamburger and dropdown toggles get `aria-expanded`/`aria-controls`. Escape closes the open dropdown and refocuses its trigger. Click-outside closes. With no JS, everything renders open. — [autumn/vendor/autumn-widgets.js:L107-L198](../../../../autumn/vendor/autumn-widgets.js)
- **Live regions:**
  - The toast region is a persistent `aria-live="polite" aria-atomic="false"` container; error toasts use `role="alert"`.
  - Alerts use `alert` vs `status` by severity.
  - Search results use `role="status"`.
  - Reaction counts use `aria-live`.

  — [autumn/src/widgets.rs:L5017-L5060, L4795-L4801, L420-L430](../../../../autumn/src/widgets.rs)
- **Charts:** `role="img"`, `<title>`/`<desc>`, and an optional visually hidden data table. — [autumn/src/widgets.rs:L5786-L5815](../../../../autumn/src/widgets.rs)
- **Focus visibility:** a CSS test forbids `outline: none` so focus stays visible in forced-colors mode (WCAG 2.4.7). — [autumn/src/ui/widgets_css.rs:L100-L120](../../../../autumn/src/ui/widgets_css.rs); [changelog.d/wayfinder-widget-focus-outline.md](../../../../changelog.d/wayfinder-widget-focus-outline.md)
- **Reduced motion and timing:** toasts honor `prefers-reduced-motion`, but auto-dismiss is a fixed 5s with no pause or close control. — [autumn/src/ui/widgets.css:L1235-L1290](../../../../autumn/src/ui/widgets.css)
- **Typed primitives:** `Img`, `Button`, `Link`, `MenuItem`, `TextField`, `TextArea`, `Select`, `Checkbox`, `FileField` and `RadioGroup` make the accessible name a compile-time obligation. — [autumn/src/a11y.rs:L1-L90](../../../../autumn/src/a11y.rs)
- **Tooling:**
  - `autumn check --a11y` rules: html-has-lang, bypass, landmark-one-main, image-alt, label, button-name.
  - `autumn a11y verify` is a static raw-`html!` scan with route attribution.
  - The `/actuator/a11y` posture endpoint reports three self-declared booleans.
  - Axe is mentioned only as a manual browser-extension step.

  — [docs/guide/accessibility.md:L290-L475, L597-L760](../../../../docs/guide/accessibility.md); [autumn-cli/src/a11y.rs:L1-L60](../../../../autumn-cli/src/a11y.rs)
- **Open issue:** #2666, "a11y: an empty accessible name satisfies the typestate, and four attribution shapes stay unresolved" (GitHub REST open-issues listing, autumn-foundation/autumn).

### Inferences
- Against Radix/React Aria (full WAI-ARIA APG keyboard models), Autumn's interactive widgets fall short on tabs (no APG arrow-key or roving tabindex model), combobox (no APG arrow/Escape/active-descendant model), and dropdown menus (disclosure only, no menu keyboard model). The native `<dialog>` modal is the one interactive widget whose focus management is complete, and that comes from the browser.

### Gaps
- No runtime axe/pa11y run over widget output was found. A grep for `axe`, `pa11y` and `lighthouse` in `.rs`/`.yml`/`.toml` files hit only docs and the CLI. Whether any CI job runs a browser a11y audit was not confirmed beyond that grep.

## Q6. Testing and documentation coverage

### Takeaway
Widgets are well covered by string-assertion tests: about 236 unit tests in widgets.rs plus per-widget integration files. Most public widgets carry runnable rustdoc examples. A built-in story gallery (`/_stories`, 26 stories) has a CI gate that fails if a public widget fn lacks a story. There is no visual-regression or screenshot testing of widgets.

### Cited Findings
- **Story gallery:**
  - `StoryGallery`/`StoryRegistry` plus the `story!` macro. `GET /_stories` gives a grouped index; `/_stories/{slug}` gives the live render plus Source and Rendered-HTML tabs.
  - It is gated by `AppBuilder::with_story_gallery` and `[stories] enabled = true` (default false), and may be enabled in production as a public showcase.

  — [autumn/src/stories/mod.rs:L1-L40, L248-L340](../../../../autumn/src/stories/mod.rs)
- The 26 built-in stories are grouped Forms, Display, Navigation, Marketing, Overlays and Feedback. They cover active search, autocomplete, property list, data table, breadcrumb, card, charts, stat card, tabs, nav bar, nav link, locale switcher, hero, modal, confirm action, badge, avatar, alert, toast, infinite feed, transition controls (plain and translated), comment thread, reaction controls and bulk actions. Pagination, flash, wizard and the a11y primitives are not in widgets.rs and have no built-in story. — [autumn/src/stories/builtin.rs:L22-L631](../../../../autumn/src/stories/builtin.rs)
- **CI anti-rot gate:**
  - Every story must render without panic, produce balanced HTML and have a unique slug.
  - `EXPECTED_STORY_SLUGS` is diffed both ways against the registry.
  - Every top-level `pub fn` in `src/widgets.rs` must appear in some story's source.

  — [autumn/tests/integration/stories.rs:L1-L360](../../../../autumn/tests/integration/stories.rs); [docs/guide/stories.md:L145-L160](../../../../docs/guide/stories.md)
- Widget CSS coverage tests assert that emitted classes have rules (`widget_css_coverage.rs`, `renamed_widget_classes_are_backed_by_component_css`). — [autumn/src/ui/widgets_css.rs:L60-L98](../../../../autumn/src/ui/widgets_css.rs); autumn/tests/integration/widget_css_coverage.rs
- **Test counts:**
  - Integration: `widgets_modal.rs` 43, `widgets_tabs.rs` 22, `widgets_toast.rs` 17, `widgets_infinite_feed.rs` 17, `widgets_reaction_controls.rs` 17, `widgets_avatar.rs` 12, `widgets_alert.rs` 11, `widgets_badge.rs` 9, `widgets_charts.rs` 8, `form_search_widgets.rs` 60.
  - Unit: 236 `#[test]` in `autumn/src/widgets.rs`.

  — `grep -c '#\[test\]'` over autumn/tests/integration/widgets_*.rs and [autumn/src/widgets.rs](../../../../autumn/src/widgets.rs)
- The `test_html` HTML parser is crate-private and lenient, so the stories test carries its own strict balanced-HTML checker. — [autumn/tests/integration/stories.rs:L14-L30](../../../../autumn/tests/integration/stories.rs); [autumn/src/lib.rs:L2420](../../../../autumn/src/lib.rs)
- **Docs:** guide pages exist for widget styling, tabs, stories, active search and autocomplete, accessibility, pagination, rich text, commentable, votable and cookie consent. There is no single "widgets/components" reference page; the module doc tables in widgets.rs serve as the index. — docs/guide/{widget-styling,tabs,stories,active-search-and-autocomplete,accessibility,pagination}.md; [autumn/src/widgets.rs:L7-L54](../../../../autumn/src/widgets.rs)
- **Browser tests:** `autumn/src/system_test.rs` and `browser_detect.rs` provide a Chromium system-test harness behind the `system-tests` feature. Grepping those files for widget names (modal, nav_bar, tabs, autocomplete) found no hits, so there is no evidence of browser or visual tests of widget behavior. — [autumn/src/system_test.rs](../../../../autumn/src/system_test.rs); CLAUDE.md CI notes

### Inferences
- Rendering correctness is strongly tested, but the client-side behavior in `autumn-widgets.js` (dropdown toggling, autocomplete selection, the modal fallback) appears untested in a real browser. That matches the keyboard-model gaps found in Q5.

### Gaps
- I could not confirm whether the JS runtime has unit tests beyond the asset sanity checks in `htmx.rs`; `autumn/src/htmx.rs:L789` reads it as UTF-8.

## Q7. Interactivity runtime: htmx, bundled JS, no-JS fallbacks

### Takeaway
Interactivity is htmx 2.0.4 (vendored) plus a single 267-line, same-origin, CSP-safe `autumn-widgets.js`. It handles autocomplete selection, min-length gating, nav toggles and dropdowns, and modal-open fallbacks. There is no Alpine-like declarative state layer. Native HTML features (`<dialog>`, Invoker Commands, `:target`, `<details>`, `:has()`) carry most behavior, and nearly every widget documents a no-JS fallback.

### Cited Findings
- Vendored assets served at `/static/js/…`: `htmx.min.js` (`HTMX_VERSION = "2.0.4"`), `sse.js`, `autumn-widgets.js`, `idiomorph.min.js`, and an inline `autumn-htmx-csrf.js`. — [autumn/src/htmx.rs:L37-L159](../../../../autumn/src/htmx.rs)
- `autumn-widgets.js` sections:
  - min-length enforcement on `htmx:configRequest` (L12–L33)
  - autocomplete (L35–L105)
  - nav_bar enhancement (L107–L198)
  - modal Invoker-Commands fallback (L200–L224)
  - `closedby="any"` backdrop polyfill (L225–L240)
  - re-init on `htmx:afterSwap` (L252–L267)

  — [autumn/vendor/autumn-widgets.js](../../../../autumn/vendor/autumn-widgets.js)
- No-JS fallbacks:
  - active search uses a `<noscript>` form; autocomplete uses a `<noscript><select>`.
  - confirm_action uses a `<noscript>` unconfirmed submit.
  - nav_bar renders everything open.
  - tabs use CSS `:target`.
  - infinite feed uses an `<a href>` sentinel; pagination uses plain links.
  - alert dismiss is a CSS checkbox; toasts dismiss through CSS animation.

  — [autumn/src/widgets.rs:L96-L103, L4180-L4228, L5452-L5466](../../../../autumn/src/widgets.rs)
- htmx-specific widget wiring:
  - `active_search`/`autocomplete` use `hx-get`/`hx-post`, `hx-trigger` debounce and `hx-indicator`.
  - `toast` uses `hx-swap-oob`; `infinite_feed` uses `hx-trigger="revealed, click"`.
  - `PagerOptions::hx_target`, `ReactionControls::hx_target` and `CommentThread::hx_target`.
  - The job-status fragment polls every 2s; `presence_badge` uses SSE OOB.

  — [autumn/src/widgets.rs:L385-L410, L5152-L5198](../../../../autumn/src/widgets.rs); [autumn/src/job_tracking.rs:L1405](../../../../autumn/src/job_tracking.rs); [autumn/src/presence.rs:L480-L497](../../../../autumn/src/presence.rs)
- `LiveFragment` lets models broadcast `hx-swap-oob` fragments on repository mutations. This is a live-update primitive, not a widget. — [autumn/src/live.rs:L1-L40](../../../../autumn/src/live.rs)
- `HxRequest` extractor, `HxResponseExt`, `OobSwap`/`OobMethod` and `HtmxFragments` support htmx responses. — [autumn/src/htmx.rs:L169-L450](../../../../autumn/src/htmx.rs)

### Inferences
- Because there is no client-side state layer, widgets such as popovers and tooltips (which need positioning), comboboxes with keyboard models, date pickers and sliders would need either an expansion of `autumn-widgets.js`, newer platform features (the `popover` attribute, CSS anchor positioning), or the WASM-island escape hatch.

### Gaps
- None material.

## Q8. Reuse by first-party surfaces (admin plugin, CLI scaffolds) and history

### Takeaway
The admin plugin reuses `card`, `stat_card`, `nav_bar` (sidebar), `modal`, `confirm_action` and `pagination_nav`, but keeps its own table, badges, select-all JS and duplicated tokens. CLI scaffolds and starters generate code using `data_table`, `property_list`, `confirm_action`, `active_search`, bulk actions, `transition_controls`, `comment_thread` and `nav_bar`. Most widgets landed in 0.6.0 (2026-07-18) and 0.7.0 (2026-08-23). 0.8.0 (2026-10-01) namespaced classes.

### Cited Findings
- Admin imports `CardConfig, ConfirmActionConfig, ModalConfig, NavBarConfig, NavBarLayout, NavItem, card, confirm_action, modal, modal_close_button, nav_bar, stat_card` and `ui::pagination::{PagerOptions, pagination_nav}`. — [autumn-admin-plugin/src/templates.rs:L10-L17](../../../../autumn-admin-plugin/src/templates.rs)
- Admin has its own pieces:
  - cell and detail renderers (`render_cell_value`, `render_form_widget`)
  - a select-all checkbox and bulk-confirm flow in `admin.js` (141 lines)
  - its own `tokens.css`

  `status_tag`'s docs name the admin's hand-rolled `badge badge-{op}` spans as a "migration target". — [autumn-admin-plugin/src/templates.rs:L1920-L2160](../../../../autumn-admin-plugin/src/templates.rs); [autumn-admin-plugin/src/admin.js:L1-L60](../../../../autumn-admin-plugin/src/admin.js); [autumn/src/widgets.rs:L4538-L4540](../../../../autumn/src/widgets.rs)
- CLI templates and starters reference `widgets::{Column, data_table, DataTableConfig, property_list, bulk_actions_form, bulk_select_checkbox, BulkActionsConfig, confirm_action, ConfirmActionConfig, active_search, active_search_input, ActiveSearchConfig, transition_controls, TransitionLabels, comment_thread, CommentView, NavBarConfig, NavItem, NavMenu, nav_bar}`, `ui::pagination`, and `autumn-widgets.css`/`.js` in `main.rs.tmpl`. The `widgets::table`/`sidebar` hits in the CMS starter are a Diesel table named `widgets`, not framework widgets. — grep over autumn-cli/src; [autumn-cli/src/starters/cms/src/content.rs:L3371](../../../../autumn-cli/src/starters/cms/src/content.rs)
- **History, by oldest CHANGELOG mention:**

  | Release | Widgets |
  |---|---|
  | 0.5.0 (2026-06-16) | `pagination_nav` |
  | 0.6.0 (2026-07-18) | `data_table`, `stat_card`, breadcrumb, tabs, toast, infinite feed, charts, `confirm_action`/modal, story gallery (#1526), widget stylesheet (#1215), badge (#1259), avatar (#1263), alert (#1314), typed a11y primitives (#1706) |
  | 0.7.0 (2026-08-23) | `locale_switcher` (#1251), bulk actions (#1312), `reaction_controls` (#1362), `comment_thread` (#1367) |
  | 0.8.0 (2026-10-01) | `autumn-*` class namespacing (#2354) |

  — [CHANGELOG.md:L12, L52-L62, L10326-L16215](../../../../CHANGELOG.md)
- Unreleased fragments touching widgets cover only a11y polish: forced-colors focus outline and WCAG AA text contrast. — [changelog.d/wayfinder-widget-focus-outline.md](../../../../changelog.d/wayfinder-widget-focus-outline.md); [changelog.d/wayfinder-widget-text-contrast.md](../../../../changelog.d/wayfinder-widget-text-contrast.md)

### Inferences
- The admin plugin only partly dogfoods the widget set. Its table, badges and select-all are parallel implementations, which suggests the shared widgets do not yet cover admin needs such as a selectable data table with select-all and status badges in cells.

### Gaps
- The CHANGELOG "oldest mention" method may date a widget by a later reference rather than its introduction. `nav_bar`, `hero` and `property_list` had no CHANGELOG hit.
