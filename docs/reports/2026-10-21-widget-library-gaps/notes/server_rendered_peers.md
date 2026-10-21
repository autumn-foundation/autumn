# Server-rendered framework peers: UI component offerings (as of 2026-10-21)

Method note: most component lists below come straight from each project's **default-branch source tree**, read by shallow `git clone` on 2026-10-21. They do not come from marketing pages, which a fetch tool summarised lossily. Each GitHub link points at the repo or path that was read. The date in brackets is the repo's last commit on the default branch. Versions come from repo manifests or `git ls-remote --tags`.

---

## 1. Component list per library (top peers)

### Takeaway
Framework-*shipped* sets are deliberately small. Phoenix 1.8's generated `core_components.ex` has 7 components plus JS helpers. Rails ships no component set, only Tailwind-styled scaffold/auth templates through `tailwindcss-rails`. Laravel's official Flux has about 40 free primitives, and the advanced widgets are paid. Ecosystem "shadcn ports" reach 40–80 components in every stack: Petal (Phoenix), RubyUI (Rails), Basecoat (any), templUI/shadcn-templ (Go), and on Rust Dioxus Components, leptos-shadcn-ui and maud-ui. One maud + htmx library, maud-ui, already exists and is Autumn's closest direct peer.

### Cited Findings

**Phoenix (Elixir)**
- Phoenix 1.8 generated `core_components.ex` (installer template on `main`, installer `@version "1.8.8"` in `installer/mix.exs`; latest tags v1.8.15/v1.8.16; `main` is now the v1.9 changelog). It defines exactly: `flash`, `button` (with `variant="primary"`, and it renders a link when given `href`/`navigate`/`patch`), `input` (with clauses for `hidden`, `checkbox`, `select`, `textarea` and the default text types, all taking a `Phoenix.HTML.FormField` and calling `translate_error`), `header` (`subtitle` and `actions` slots), `table` (`col`/`action` slots, `row_click`, `row_id`, and it supports `phx-update="stream"` for LiveView streams), `list` (`item` slot with `title`) and `icon` (Heroicons through `hero-*` CSS classes). It also has the JS helpers `show/2` and `hide/2` (wrapping `JS.show`/`JS.hide` with transitions) and `translate_error/1`, `translate_errors/2`. — [phoenix installer core_components.ex.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_web/components/core_components.ex.eex); [installer/mix.exs](https://github.com/phoenixframework/phoenix/blob/main/installer/mix.exs); [CHANGELOG (main = v1.9)](https://github.com/phoenixframework/phoenix/blob/main/CHANGELOG.md)
- The generated `layouts.ex` adds `app` (the single app layout), `flash_group` and `theme_toggle`. The toggle is a three-way system/light/dark switch that dispatches `phx:set-theme` and sets `data-theme`. — [layouts.ex.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_web/components/layouts.ex.eex)
- **The 1.7 → 1.8 delta:** Phoenix v1.7.14's `core_components.ex` defined `modal, flash, flash_group, simple_form, button, input, label, error, header, table, list, back`. In 1.8, **`modal`, `simple_form`, `label`, `error` and `back` are gone**: label and error are inlined into `input`, and `flash_group` moved to Layouts. — [v1.7.14 core_components.ex](https://github.com/phoenixframework/phoenix/blob/v1.7.14/installer/templates/phx_web/components/core_components.ex) compared with the main template above.
- The Phoenix 1.8 release post (Chris McCord, 2025-08-05) explains the cut. It says Phoenix 1.8 "extends our built-in tailwindcss support with daisyUI, adding a flexible component and theming system", that "all `phx.new` apps now ship with light and dark themes out of the box, with a toggle built into the layout", and that the old components "would often get in the way of experienced developers, by generating too much opinionated code", which "allows us to trim down the core components to only the main building blocks". It also says "the `phx.gen.*` generators do not depend on daisyUI". — [Phoenix 1.8.0 released](https://www.phoenixframework.org/blog/phoenix-1-8-released)
- The generated `app.css` loads `daisyui` with `themes: false`, then two custom `daisyui-theme` blocks: "dark" with `prefersdark: true`, and "light", inspired by Phoenix colours. It adds a `@custom-variant dark` keyed on `[data-theme=dark]` and a vendored heroicons Tailwind plugin. — [app.css.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_assets/app.css.eex)
- **Petal Components** v4.18.0 [2026-10-21] markets itself as "Shadcn-style Phoenix components that AI assistants can actually use". It uses Tailwind v4, works in live or dead views, and ships a hosted MCP server (`mcp.petal.build`) that exposes component schemas. It has about 75 modules: accordion, alert, alert_dialog, avatar, badge, breadcrumbs, button, button_group, calendar, card, carousel, chart, chat, collapsible, color_scheme_switch, combo_box, command, container, context_menu, **data_table**, date_picker, dropdown, empty, field, file_upload, filters, form, hover_card, icon, input, input_group, input_otp, kbd, language_select, link, loading, local_time, menu, modal, navigation_menu, number_field, pagination, popover, progress, qr_code, rating, resizable, scroll_area, scrollspy, separator, sidebar, skeleton, slide_over, slider, sortable, sparkline, stepper, table, tabs, timeline, toast, toggle_group, tooltip, tree, typography and user_dropdown_menu. It also has decorative modules (aurora, border_beam, confetti, marquee, meteors, etc.). — [petal_components lib/](https://github.com/petalframework/petal_components/tree/main/lib/petal_components); [README](https://github.com/petalframework/petal_components)
- **SaladUI** v1.0.0-beta.6 [2026-08-11] is a shadcn port for Phoenix. Its components are accordion, alert, alert_dialog, avatar, badge, breadcrumb, button, card, chart, checkbox, collapsible, command, dialog, dropdown_menu, form, hover_card, icon, input, label, menu, pagination, popover, progress, radio_group, scroll_area, select, separator, sheet, sidebar, skeleton, slider, switch, table, tabs, textarea, toast, toggle, toggle_group and tooltip. — [salad_ui lib/](https://github.com/bluzky/salad_ui/tree/main/lib/salad_ui)
- **Mishka Chelekom** v0.0.10-alpha.7 [2026-10-17] is a generator that copies components into the app. It has about 80 component templates, including banner, blockquote, carousel, chat, checkbox_card, clipboard, combobox, date_time_field, device_mockup, dock, drawer, gallery, mega_menu, modal, native_select, rating, speed_dial, stat, stepper, table, table_content, timeline and typed fields (email, password, tel, url, number, color, range, search). Its JS directory adds about 70 hooks: calendar, color_picker, context_menu, editors (TipTap, Lexical, Milkdown, CodeMirror), charts (Chart.js, ECharts, Billboard), floating, focus_trap, mask_input, menubar, navigation_menu, otp, roving_tabindex, splitter, tree, toast_region and more. — [mishka_chelekom priv/components](https://github.com/mishka-group/mishka_chelekom/tree/master/priv/components); [priv/assets/js](https://github.com/mishka-group/mishka_chelekom/tree/master/priv/assets/js)
- **Fluxon UI** is a commercial LiveView library by Andriel Nuernberg, distributed from a private Hex repo after licence purchase. It emphasises keyboard navigation, focus management, dropdown positioning and click/esc/blur close behaviour, and it supports stackable sheets/modals. — [Elixir Forum announcement thread](https://elixirforum.com/t/fluxon-ui-a-modern-ui-components-lib-for-phoenix-liveview/67551); [stfx blog, 2025-10-15](https://blog.stfx.eu/2025-10-15-fluxon-ui-first.html)
- **Backpex** v0.22.0 [2026-10-21] is a LiveView admin. Its fields are belongs_to, boolean, checkgroup, currency, date, date_time, email, has_many, has_many_through, inline_crud, multi_select, number, select, text, textarea, time, upload and url. Its filters are boolean, multi_select, range and select. — [backpex lib/backpex/fields](https://github.com/naymspace/backpex/tree/develop/lib/backpex/fields)

**Rails (Ruby)**
- **ViewComponent** v4.17.0 [2026-10-20] — [version.rb](https://github.com/ViewComponent/view_component/blob/main/lib/view_component/version.rb). It is the component *mechanism*, not a component set. It has built-in previews (`ViewComponent::Preview`) — [ViewComponent previews guide](https://viewcomponent.org/guide/previews.html).
- **Primer ViewComponents** (`@primer/view-components` v0.53.5) [2026-10-14] — [package.json](https://github.com/primer/view_components/blob/main/package.json). Components in `app/components/primer/`:
  - **Alpha:** ActionBar, ActionList, ActionMenu, AutoComplete, Banner, ButtonMarketing, CheckBox, CheckBoxGroup, Dialog, Dropdown, FileTreeView, FormButton, FormControl, HellipButton, HiddenTextExpander, Image, IncludeFragment, Layout, Menu, MultiInput, NavList, Navigation::Tab, Overlay, RadioButton, RadioButtonGroup, SegmentedControl, Select, SelectPanel, SkeletonBox, Stack/StackItem, SubmitButton, TabContainer, TabNav, TabPanels, TextArea, TextField, ToggleSwitch, Tooltip, TreeView, UnderlineNav and UnderlinePanels.
  - **Beta:** AutoComplete, Avatar, AvatarStack, Blankslate, BorderBox, Breadcrumbs, Button, ButtonGroup, ClipboardCopy, ClipboardCopyButton, CloseButton, Counter, Details, Flash, Heading, IconButton, Label, Link, Markdown, NavList, Octicon, Popover, ProgressBar, RelativeTime, Spinner, State, Subhead, Text, TimelineItem and Truncate.
  - **Forms framework** (`lib/primer/forms`): ActionMenu, AutoComplete, Button, Caption, CheckBox(Group), FormControl, FormList, Group, HiddenField, Multi, RadioButton(Group), Select, Separator, SubmitButton, TextArea, TextField, ToggleSwitch(Form) and ValidationMessage.
  - Sources: [app/components/primer](https://github.com/primer/view_components/tree/main/app/components/primer); [lib/primer/forms](https://github.com/primer/view_components/tree/main/lib/primer/forms)
- **RubyUI** (formerly PhlexUI) [2026-10-21] is a set of Phlex components that come with Stimulus controllers. 78 `*_controller.js` files cover accordion, alert_dialog, avatar, calendar, carousel, chart, checkbox, clipboard, collapsible, combobox, command, context_menu, **data_table**, dialog, dropdown_menu, form, hover_card, input_otp, masked_input, message_scroller, popover, select, sheet, sidebar, tabs, theme_toggle, toast, toggle, toggle_group and tooltip. — [ruby_ui repo](https://github.com/ruby-ui/ruby_ui)
- **stimulus-components** [2026-10-18] is a set of single-purpose Stimulus controllers (behaviours, not styled widgets): animated-number, auto-submit, carousel, character-counter, chartjs, checkbox-select-all, clipboard, color-picker, confirmation, content-loader, dialog, dropdown, glow, hotkey, lightbox, notification, password-visibility, places-autocomplete, popover, prefetch, rails-nested-form, read-more, remote-rails, reveal, scroll-progress, scroll-reveal, scroll-to, sortable, sound, speech-recognition, textarea-autogrow and timeago. — [stimulus-components/components](https://github.com/stimulus-components/stimulus-components/tree/master/components)
- **Rails 8 built-in UI:** core Rails 8.1.4 ships no component library. With `--css tailwind`, the `tailwindcss-rails` gem (v4.6.0) overrides the generators and provides Tailwind-styled templates for `scaffold` (`_form`, `index`, `show`, `new`, `edit`, `partial`), `authentication` (`sessions/new`, `passwords/new|edit`), `controller` and `mailer`. — [tailwindcss-rails lib/generators/tailwindcss](https://github.com/rails/tailwindcss-rails/tree/main/lib/generators/tailwindcss); [rails authentication generator templates](https://github.com/rails/rails/tree/main/railties/lib/rails/generators/rails/authentication/templates)
- **Avo** v4.3.2 [2026-10-21] is a Rails admin. Its field types are area, array, avatar, badge, belongs_to, boolean, boolean_group, checkbox_list, code, country, date, date_time, easy_mde, external_image, file, files, gravatar, has_and_belongs_to_many, has_many, has_one, heading, hidden, id, key_value, location, number, password, preview, progress_bar, radio, select, stars, status, tags, text, textarea, time, tiptap and trix. — [avo lib/avo/fields](https://github.com/avo-hq/avo/tree/main/lib/avo/fields)

**Laravel (PHP)**
- **Flux** (official Livewire library). The free/open-source package `livewire/flux` [2026-10-21] ships these Blade components in its public stubs: accent, aside, avatar, badge, brand, breadcrumbs, button, callout, card, checkbox, container, description, dropdown, error, field, fieldset, flag, footer, header, heading, icon, input, label, legend, link, main, menu, modal, navbar, navlist, navmenu, otp, pagination, profile, progress, radio, select, separator, sidebar, skeleton, spacer, subheading, switch, table, text, textarea, toast, toggle and tooltip. Its README says "the rest of the Flux components are part of the paid (Pro) tier". — [livewire/flux stubs](https://github.com/livewire/flux/tree/main/stubs/resources/views/flux); [README](https://github.com/livewire/flux)
- The Flux site catalogue (v2.x) lists 52 components. These include Pro widgets that are absent from the free stubs: Accordion, Autocomplete, Calendar, Carousel, Chart, Color picker, Command, Composer, Context, Date picker, Datetime picker, Editor, File upload, Kanban, Phone, Pillbox, Slider, Tabs, Time picker and Timeline. — [fluxui.dev/components](https://fluxui.dev/components)
- Flux Pro pricing names 16 Pro components: Accordion, Autocomplete, Calendar, Charts, Command palette, Context menu, Date picker, Rich text editor, Listbox, Searchable select, Combobox, Tabs, File upload, Time picker, Kanban and Slider. The page also says "Includes 23+ Pro components". Prices are $149 per project, $299 unlimited and $799 for a team, all one-time. — [fluxui.dev/pricing](https://fluxui.dev/pricing)
- The **Laravel Livewire starter kit** [2026-09-21] depends on `livewire/flux ^2.13.1`, `livewire/livewire ^4.1`, `livewire/blaze ^1.0` and `laravel/fortify`. It adds only app-specific components: app-logo, auth-header, auth-session-status, desktop-user-menu, passkey-registration/verify and placeholder-pattern. — [livewire-starter-kit composer.json](https://github.com/laravel/livewire-starter-kit/blob/main/composer.json)
- **Filament** v5.10.3 [2026-10-20] is split into packages: actions, forms, infolists, notifications, panels, query-builder, schemas, tables and widgets. — [filament packages](https://github.com/filamentphp/filament/tree/5.x/packages)
  - **Form fields:** Builder, Checkbox, CheckboxList, CodeEditor, ColorPicker, DatePicker, DateTimePicker, FileUpload, Hidden, KeyValue, MarkdownEditor, ModalTableSelect, MorphToSelect, MultiSelect, OneTimeCodeInput, Radio, Repeater, RichEditor, Select, Slider, TableSelect, TagsInput, TextInput, Textarea, TimePicker, Toggle and ToggleButtons.
  - **Table columns:** Text, Icon, Image, Color, Badge, Boolean, Checkbox, Select, TextInput, Toggle, Tags, View and ColumnGroup.
  - **Table filters:** Filter, SelectFilter, MultiSelectFilter, TernaryFilter, TrashedFilter and QueryBuilder.
  - **Infolist entries:** Text, Icon, Image, Color, Code, KeyValue, Repeatable and View.
  - **Schema layout:** Section, Tabs, Wizard, Grid, Flex, Fieldset, Group, FusedGroup, Callout, EmptyState, EmbeddedTable and Actions.
  - Sources: [forms/src/Components](https://github.com/filamentphp/filament/tree/5.x/packages/forms/src/Components); [tables/src/Columns](https://github.com/filamentphp/filament/tree/5.x/packages/tables/src/Columns); [tables/src/Filters](https://github.com/filamentphp/filament/tree/5.x/packages/tables/src/Filters); [infolists/src/Components](https://github.com/filamentphp/filament/tree/5.x/packages/infolists/src/Components); [schemas/src/Components](https://github.com/filamentphp/filament/tree/5.x/packages/schemas/src/Components)
- **Blade UI Kit** v0.7.0 has a last commit of 2025-05-23, so it looks stagnant. Components: Alert, FormButton, Logout, Carbon, Countdown, EasyMDE, Trix, Error, Form, Checkbox, ColorPicker, Email, FlatPickr, Input, Password, Pikaday, Textarea, Label, Html, SocialMeta, Mapbox, Markdown, ToC, Dropdown, Avatar, Cron and Unsplash. — [blade-ui-kit src/Components](https://github.com/blade-ui-kit/blade-ui-kit/tree/main/src/Components)

**Django (Python)**
- **django-cotton** v0.9.22 [2026-10-17] is a component *syntax*, not a set: `<c-my-component />` tags, named slots, `{{ attrs }}` passthrough and merging, and `<c-vars>`. — [django-cotton README](https://github.com/wrabit/django-cotton)
- **django-components** v0.152.0 [2026-10-19] is a component mechanism. Each component bundles HTML, `js_file` and `css_file`, with `get_template_data`, `get_js_data` and `get_css_data` hooks. — [django-components README](https://github.com/django-components/django-components)
- **django-crispy-forms** 2.7 [2026-07-29] renders forms through swappable template packs (`CRISPY_TEMPLATE_PACK`): Bootstrap (crispy-bootstrap5), Tailwind (crispy-tailwind), Bulma and Foundation. — [crispy-forms README](https://github.com/django-crispy-forms/django-crispy-forms)
- **django-unfold** 0.110.0 [2026-10-21] is an admin theme built on Tailwind, Alpine.js and HTMX. Its template components are button, card, container, flex, icon, layer, link, navigation, progress, separator, table, text, title and tracker. Its contrib integrations cover constance, filters, forms, guardian, hijack, import_export, inlines, location_field, reversion, simple_history and waffle. — [unfold components](https://github.com/unfoldadmin/django-unfold/tree/main/src/unfold/templates/unfold/components); [README](https://github.com/unfoldadmin/django-unfold)

**Stack-agnostic / hypermedia-native**
- **Basecoat** (`basecoat-css` v1.0.2) [2026-07-06] is "a Tailwind CSS, vanilla HTML/CSS/JavaScript implementation of the shadcn/ui design system… without React, Radix, or framework runtime dependencies". It ships Nunjucks and Jinja templates. CSS components: accordion, alert, avatar, badge, breadcrumb, button, button-group, card, chart, checkbox, collapsible, combobox, command, dialog, drawer, dropdown-menu, empty, field, form, input, input-group, item, kbd, label, native-select, popover, progress, radio, range, scrollbar, select, sidebar, skeleton, switch, table, tabs, textarea, toast and tooltip. Only 13 of them need JS: accordion, chart, combobox, command, drawer, dropdown-menu, popover, range, select, sidebar, tabs and toast, plus the `basecoat.js` core. — [basecoat README](https://github.com/hunvreus/basecoat); [src/css/components](https://github.com/hunvreus/basecoat/tree/main/src/css/components); [src/js](https://github.com/hunvreus/basecoat/tree/main/src/js)
- **daisyUI** v5.7.50 [2026-10-13] is CSS-only. It ships 35 built-in themes and about 60 component classes: alert, avatar, badge, breadcrumbs, button, calendar, card, carousel, chat, checkbox, collapse, countdown, diff, divider, dock, drawer, dropdown, fab, fieldset, fileinput, filter, footer, hero, indicator, input, kbd, label, link, list, loading, mask, megamenu, menu, mockup, modal, navbar, otp, progress, radialprogress, radio, range, rating, select, skeleton, stack, stat, status, steps, swap, tab, table, textarea, textrotate, timeline, toast, toggle, tooltip and validator. — [daisyui src/components](https://github.com/saadeghi/daisyui/tree/master/packages/daisyui/src/components)
- **Web Awesome** (successor to Shoelace) v3.14.0 [2026-10-19] is a set of web components.
  - Free components include accordion, animated-image, avatar, badge, breadcrumb, button(-group), callout, card, carousel, checkbox(-group), color-picker, comparison, copy-button, details, dialog, divider, drawer, dropdown, format-bytes/date/number, icon, include, input, markdown, number-input, otp-input, page, pagination, popover, popup, progress-bar/ring, qr-code, radio(-group), rating, relative-time, scroller, select, skeleton, slider, spinner, split-panel, stepper, switch, tab-group, tag, tag-input, textarea, time-input, toast, tooltip, tree and zoomable-frame.
  - **Pro-only:** Combobox (3.1), Date Input and Date Picker (3.8), File Input (3.2), Data Grid (experimental, 3.11), Sparkline and the charts (Bar/Bubble/Doughnut/Line/Pie/Polar/Radar/Scatter, 3.3), and Video/Video Playlist (3.7).
  - Sources: [webawesome src/components](https://github.com/shoelace-style/webawesome/tree/next/packages/webawesome/src/components); [webawesome.com/docs/components](https://webawesome.com/docs/components/)
- **Franken UI** [last commit 2026-06-11, v2.1.2-next.0]: the README now says "Franken is now 0build. Go to https://0build.dev". Treat it as discontinued under that name. — [franken-ui/ui README](https://github.com/franken-ui/ui)
- **Oat** [2026-10-19] describes itself as "ultra-lightweight… semantic UI component library with zero dependencies", about 10KB of CSS and JS. Semantic tags are styled without classes, and "a few dynamic components are WebComponents". Its CSS covers accordion, alert, avatar, badge, button, card, dialog, dropdown, form, grid, progress, sidebar, skeleton, spinner, table, tabs, taginput, toast, tooltip and upload. JS covers dropdown, sidebar, tabs, taginput, toast, tooltip and upload. — [knadh/oat](https://github.com/knadh/oat)
- **Pines** (Alpine + Tailwind, copy-paste) has a last commit of 2025-08-23. Elements: accordion, alert, badge, banner, breadcrumbs, button, card, checkbox, combobox, command, context-menu, copy-to-clipboard, date-picker, dropdown-menu, full-screen-modal, hover-card, image-gallery, menubar, modal, monaco-editor, navigation-menu, pagination, popover, progress, radio-group, range-slider, rating, select, slide-over, switch, table, tabs, text-input, textarea(-auto-resize), toast, tooltip and others. — [thedevdojo/pines elements](https://github.com/thedevdojo/pines/tree/main/elements)
- **HyperUI** [2026-10-10] is copy-paste Tailwind HTML snippets with no JS library.
  - Application sections: accordions, badges, breadcrumbs, button-groups, charts, checkboxes, details-list, dividers, dropdown, empty-states, file-uploaders, filters, grids, inputs, loaders, media, modals, pagination, progress-bars, quantity-inputs, radio-groups, range-inputs, selects, side-menu, skip-links, stats, steps, tables, tabs, textareas, timelines, toasts, toggles and vertical-menu.
  - It also has marketing sections, neobrutalism variants and page templates.
  - Source: [hyperui collection](https://github.com/markmead/hyperui/tree/main/src/content/collection)

**Go**
- **templUI** has been renamed **shadcn-templ** ("an unofficial, community-led port of shadcn/ui for Go and templ"). Its latest tag is v2.0.0-beta.15 [2026-10-15].
  - Components: accordion, alert, alertdialog, aspectratio, attachment, avatar, badge, breadcrumb, bubble, button, buttongroup, calendar, card, carousel, chart, checkbox, collapsible, combobox, command, contextmenu, copybutton, dialog, direction, drawer, dropdownmenu, empty, field, hovercard, input, inputgroup, inputotp, item, kbd, label, marker, menubar, message, messagescroller, nativeselect, navigationmenu, pagination, popover, progress, questionnaire, radiogroup, resizable, scrollarea, select, separator, sheet, sidebar, skeleton, slider, spinner, switch, table, tabs, textarea, toast, toggle, togglegroup and tooltip.
  - Blocks: dashboard, login 01–05, sidebar 01–16 and signup 01–05.
  - An older (v1-era) public list had 42 components, including Date Picker, Time Picker, Rating and Tags Input.
  - Sources: [templui README](https://github.com/templui/templui); [components/](https://github.com/templui/templui/tree/main/components); [templui.io component list](https://templui.io/docs/components)

**Rust**
- **maud-ui** v0.21.0 (2026-09-28, MIT, by hgeldenhuys) has "84 headless, accessible UI components for Rust web apps, built on maud and htmx and styled like shadcn/ui". It is organised in tiers:
  - **Tier 1, pure HTML+CSS:** Alert, Avatar, Badge, Breadcrumb, Button(Group), Card, Chart, Checkbox, Date Range, Empty State, Field, Fieldset, Form, Input, Kbd, Label, Meter, Native Select, Number Field, Pagination, Progress, Radio (Group), Separator, Skeleton, Spinner, Table, Textarea, Typography and others.
  - **Tier 2, JS-enhanced:** Accordion, Collapsible, Hover Card, Input Group, Input OTP, Switch, Tabs, Toast, Toggle (Group), Tooltip and others.
  - **Tier 3, requires JS:** Alert Dialog, Calendar, Carousel, Combobox, Command, Context Menu, **Data Table**, **Date Picker**, Dialog, Drawer, Menu, Menubar, Navigation Menu, Popover, Resizable, Scroll Area, Select, Sheet, Sidebar, Slider and Sonner.
  - Plus 31 blocks and 15 third-party widget shells (Monaco, AG Grid, ECharts, Leaflet, FullCalendar, TipTap and others).
  - Sources: [docs.rs/crate/maud-ui](https://docs.rs/crate/maud-ui/latest); [github.com/hgeldenhuys/maud-ui](https://github.com/hgeldenhuys/maud-ui)
- **Thaw UI** (Leptos, Fluent Design) is at `0.5.0-beta` on Leptos 0.8.5 [2026-05-07].
  - Components: accordion, anchor, auto_complete, avatar, back_top, badge, breadcrumb, button, calendar, card, checkbox, code, color_picker, combobox, config_provider, date_picker, dialog, divider, drawer, field, flex, grid, icon, image, info_label, input, label, layout, link, loading_bar, locale, menu, message_bar, nav, pagination, persona, popover, progress_bar, radio, rating, scrollbar, select, skeleton, slider, space, spin_button, spinner, switch, tab_list, table, tag, tag_picker, text, textarea, theme, time_picker, toast, tooltip, tree and upload.
  - There is an `examples/ssr_axum` example.
  - Sources: [thaw/src](https://github.com/thaw-ui/thaw/tree/main/thaw/src); [Cargo.toml](https://github.com/thaw-ui/thaw/blob/main/thaw/Cargo.toml)
- **Dioxus Components** [2026-09-07] is "a shadcn style component library for Dioxus built on top of the unstyled Dioxus primitives". Components are added through the `dx` CLI. `dioxus-primitives` is at 0.0.1 in the manifest.
  - Primitives: accordion, alert_dialog, aspect_ratio, avatar, calendar, checkbox, collapsible, color_picker, combobox, context_menu, date_picker, dialog, drag_and_drop_list, dropdown_menu, hover_card, label, listbox, menubar, navbar, popover, progress, radio_group, scroll_area, select, separator, slider, switch, tabs, tag_group, toast, toggle, toggle_group, toolbar, tooltip and virtual_list.
  - The styled preview layer adds badge, button, card, form, input, item, pagination, sheet, sidebar, skeleton and textarea.
  - Sources: [DioxusLabs/components](https://github.com/DioxusLabs/components); [primitives/src](https://github.com/DioxusLabs/components/tree/main/primitives/src)
- **Rust/UI** [2026-10-12]: the README now describes it as "A component registry for **Dioxus** — Tailwind CSS, copy-paste ready". It says "Early stage, experimental" and "Not affiliated with… the Dioxus team". It was originally Leptos-based (rust-ui.com).
  - Registry `ui/` includes accordion, alert_dialog, auto_form, bottom_nav, button, calendar, carousel, charts, chips, combobox, command, context_menu, **data_grid**, **data_table**, date_picker, dialog, drag_and_drop, drawer, dropdown_menu, dropzone, form, input_otp, input_phone, multi_select, navigation_menu, popover, select, sheet, sidebar, slider, stepper, table, tabs, theme_toggle, toast, tooltip and others.
  - Source: [rust-ui/ui](https://github.com/rust-ui/ui)
- **leptos-shadcn-ui** v0.9.0 on Leptos 0.8.9 has a last commit of 2026-01-10, so it may be slowing. It has about 45 packages: accordion, alert(-dialog), aspect-ratio, avatar, badge, breadcrumb, button, calendar, card, carousel, checkbox, collapsible, combobox, command, context-menu, date-picker, dialog, drawer, dropdown-menu, form, hover-card, input(-otp), label, menubar, navigation-menu, pagination, popover, progress, radio-group, resizable, scroll-area, select, separator, sheet, skeleton, slider, switch, table, tabs, textarea, toast, toggle and tooltip. — [cloud-shuttle/leptos-shadcn-ui packages/leptos](https://github.com/cloud-shuttle/leptos-shadcn-ui/tree/main/packages/leptos)
- **Loco.rs** v1.2.0 [2026-10-20] ships **no UI component library**. Views use Tera (`src/tera.rs`, `tera_builtins`). The current `loco-gen` scaffold templates have only `api/` and `frontend/` variants: `frontend_list|new|edit|show|api.t`. No htmx or html scaffold templates appear on today's `master`. — [loco-gen/src/templates/scaffold](https://github.com/loco-rs/loco/tree/master/loco-gen/src/templates/scaffold)
- Other maud-adjacent crates: `mash-up` v0.1.0 (2024) is only a re-export bundle for Maud/Axum/SQLx/HTMX, not components. `htmx-components` v0.3.0 is built on rscx, not maud. — [docs.rs/mash-up](https://docs.rs/mash-up/latest/mash_up/); [docs.rs/htmx-components](https://docs.rs/htmx-components)

### Inferences
- Phoenix 1.8 is the closest *framework-level* analogue to Autumn. It ships the components a generator needs: flash, button, form input with error translation, header, table that is stream-aware, description list, and icon. It hands everything else (modal, dropdown, tabs) to daisyUI CSS classes. A batteries-included Autumn would beat Phoenix's built-in set even with a modest library.
- Every ecosystem converged on "shadcn-shaped" naming and inventory: Petal, SaladUI, RubyUI, Basecoat, shadcn-templ, maud-ui, Dioxus Components, leptos-shadcn-ui and Rust/UI. Using shadcn names lowers the learning cost for Autumn users and for AI assistants. Petal now explicitly markets an MCP server for its components.
- maud-ui is an existing, actively released (0.21.0, Sept 2026) maud + htmx component crate. Autumn should benchmark directly against it, and could either learn from it or consider integrating it.

### Gaps
- No authoritative full free-vs-Pro matrix for Flux could be retrieved. The pricing page lists 16 Pro components but says "23+". The free list above is inferred from the open-source stubs.
- Fluxon UI's component list, version and pricing could not be retrieved: fluxon.dev did not resolve from this environment.
- The Wagtail admin UI component system (pattern library / Storybook) was not researched in depth because of the tool-call budget.
- Flowbite's Rails/ViewComponent usage and "Tailwind UI (Tailwind Plus) in Rails" were not verified from primary sources.
- leptos-use (composables, not UI components) was not inspected.
- Yew (yew-bootstrap / PatternFly-yew) was not inspected. Neither is server-rendered.

---

## 2. Implementing overlays, menus, comboboxes and date pickers without an SPA

### Takeaway
There are four patterns:
- **LiveView JS commands + hooks** (Phoenix).
- **Small vanilla-JS or Stimulus/Alpine controllers bound by data-attributes** (Basecoat, RubyUI, shadcn-templ, maud-ui, Pines).
- **Custom elements / web components** (Primer via Catalyst, Flux `<ui-*>`, Web Awesome, Oat).
- **Pure CSS on native elements** (daisyUI: `<dialog>`, `popover`, `details`, `:focus-within`, CSS anchor positioning).

Native `<dialog>` with `showModal()` and the HTML `popover` attribute are now the common substrate for modals and menus. Comboboxes and date pickers always need real JS and are the usual paywall or "Pro" line.

### Cited Findings
- Phoenix core components use `Phoenix.LiveView.JS` (`JS.show`/`JS.hide` with transition classes, `JS.push("lv:clear-flash")`). There is no Alpine. — [core_components.ex.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_web/components/core_components.ex.eex)
- Petal's dropdown uses `JS.toggle`/`JS.hide`. Petal's modal composes `JS.exec`, `JS.push`, `JS.add_class`/`remove_class` and `JS.focus_first`. It also ships `assets/js/petal_components.js`. — [petal dropdown.ex](https://github.com/petalframework/petal_components/blob/main/lib/petal_components/dropdown.ex); [modal.ex](https://github.com/petalframework/petal_components/blob/main/lib/petal_components/modal.ex)
- SaladUI renders `data-component` markup with `phx-hook="SaladUI"`. Its JS layer is a state-machine framework (`state-machine.js`, `focus-trap.js`, `positioner.js`, `portal.js`, `click-outside.js`, `collection.js`) plus per-component modules (dialog, dropdown_menu, popover, select, command, tooltip, toast, etc.). — [salad_ui dialog.ex](https://github.com/bluzky/salad_ui/blob/main/lib/salad_ui/dialog.ex); [salad_ui assets](https://github.com/bluzky/salad_ui/tree/main/assets)
- Mishka Chelekom ships headless JS hooks for floating positioning, focus traps, roving tabindex, combobox, calendar, menubar and splitter. — [mishka priv/assets/js](https://github.com/mishka-group/mishka_chelekom/tree/master/priv/assets/js)
- Primer ViewComponents' interactivity is GitHub's Catalyst web components plus `@github/*` custom elements: auto-complete-element, clipboard-copy-element, details-menu-element, include-fragment-element, relative-time-element and others. Its TS elements include `action_menu_element`, `select_panel_element`, `modal_dialog`, `tool_tip`, `tree_view`, `focus_group` and `anchored_position`. `Alpha::Dialog` renders a native `<dialog>`. — [package.json](https://github.com/primer/view_components/blob/main/package.json); [alpha/dialog.html.erb](https://github.com/primer/view_components/blob/main/app/components/primer/alpha/dialog.html.erb)
- Flux wraps behaviour in its own custom elements: the dropdown renders `<ui-dropdown>`, and the modal renders `<ui-modal>` around a native `<dialog>` with an `x-data` hook. — [flux dropdown.blade.php](https://github.com/livewire/flux/blob/main/stubs/resources/views/flux/dropdown.blade.php); [modal/index.blade.php](https://github.com/livewire/flux/blob/main/stubs/resources/views/flux/modal/index.blade.php)
- Basecoat's dialog CSS targets `:is([open], :popover-open)`, so it works with native `<dialog>` and the Popover API. Its dropdown-menu JS uses the `popover` attribute, `aria-expanded`, `aria-activedescendant` and ArrowDown keyboard handling. Its select uses `aria-activedescendant` and `role`. — [dialog.css](https://github.com/hunvreus/basecoat/blob/main/src/css/components/dialog.css); [dropdown-menu.js](https://github.com/hunvreus/basecoat/blob/main/src/js/dropdown-menu.js); [select.js](https://github.com/hunvreus/basecoat/blob/main/src/js/select.js)
- shadcn-templ ships per-component vanilla JS. It also has a shared floating-UI-like internal layer: `floating_focus_manager.js`, `use_anchor_positioning.js`, `use_dismiss.js`, `use_list_navigation.js`, `use_typeahead.js`, `scroll_lock.js` and `portal.js`. Its dialog emits `aria-haspopup`, `aria-expanded`, `aria-labelledby` and `aria-describedby`. It has a `parity/htmx/server` harness plus `parity/a11y.mjs` and `behavior.mjs` scripts for checking htmx compatibility. — [templui repo](https://github.com/templui/templui); [parity/](https://github.com/templui/templui/tree/main/parity)
- maud-ui's dialog is a "modal dialog using native `<dialog>` element with focus trap and ESC/backdrop close". Triggers carry `data-mui="dialog-trigger"`, `aria-controls` and `aria-haspopup="dialog"`. It has per-component JS files (combobox, date_picker, data_table, menu, popover, select, etc.) bundled into `maud-ui.js`. Its docs say components "render without JavaScript (progressive enhancement)" and recommend htmx for cross-request interactions. — [maud-ui src/primitives/dialog.rs](https://github.com/hgeldenhuys/maud-ui/blob/main/src/primitives/dialog.rs); [docs.rs/crate/maud-ui](https://docs.rs/crate/maud-ui/latest)
- Web Awesome's dialog wraps a native `<dialog>` and calls `showModal()`. — [webawesome dialog.ts](https://github.com/shoelace-style/webawesome/blob/next/packages/webawesome/src/components/dialog/dialog.ts)
- daisyUI 5 is CSS-only. The modal supports `<dialog open>`, `:target` and the legacy `modal-toggle` checkbox. The dropdown supports `<details>`, `:focus-within`, `popover` and CSS `anchor` positioning. — [daisyui modal.css](https://github.com/saadeghi/daisyui/blob/master/packages/daisyui/src/components/modal.css); [dropdown.css](https://github.com/saadeghi/daisyui/blob/master/packages/daisyui/src/components/dropdown.css)
- Pines' modal uses Alpine `x-data`, `x-teleport` and `x-trap` for the focus trap. — [pines elements/modal.html](https://github.com/thedevdojo/pines/blob/main/elements/modal.html)
- Oat makes "a few dynamic components" WebComponents with minimal JS. — [knadh/oat README](https://github.com/knadh/oat)
- RubyUI pairs each interactive Phlex component with a Stimulus controller (78 controllers). — [ruby_ui](https://github.com/ruby-ui/ruby_ui)
- django-unfold is built on Alpine.js + HTMX. — [unfold README](https://github.com/unfoldadmin/django-unfold)
- Comboboxes and date pickers are paywalled or "Pro" in Flux (Combobox, Date picker, Time picker, Calendar) and in Web Awesome (Combobox, Date Picker, Date Input, File Input, Data Grid). — [fluxui.dev/pricing](https://fluxui.dev/pricing); [webawesome.com/docs/components](https://webawesome.com/docs/components/)
- Blade UI Kit delegates date pickers to third-party libraries (FlatPickr, Pikaday). Mishka and maud-ui delegate rich editors, charts and grids to TipTap/Lexical, ECharts/Chart.js and AG Grid. — [blade-ui-kit Components](https://github.com/blade-ui-kit/blade-ui-kit/tree/main/src/Components); [docs.rs/crate/maud-ui](https://docs.rs/crate/maud-ui/latest)

### Inferences
- For a maud + htmx framework, the peer-consistent design is:
  - Native `<dialog>` and `popover` for overlays.
  - A small vanilla-JS controller set bound by `data-*` attributes, following Basecoat, shadcn-templ and maud-ui. Avoid requiring Alpine or Stimulus.
  - A shared positioning and focus layer (anchor positioning, dismiss, focus trap, list navigation, typeahead) that all overlays reuse.
  - Make sure controllers re-initialise after htmx swaps (`htmx:load`). shadcn-templ's parity harness exists precisely to check this.
- Combobox and date picker are where peers spend real engineering or charge money. Shipping good free versions would be a differentiator.

### Gaps
- Whether Flux's `<ui-*>` elements are standalone web components or Alpine-backed internally was not verified beyond the `x-data` seen in the modal.
- How each library re-initialises after htmx/Turbo swaps (MutationObserver vs explicit events) was not checked library by library.

---

## 3. Preview/catalog tools and visual/a11y testing

### Takeaway
Rails (Lookbook + ViewComponent previews) and Phoenix (Phoenix Storybook) have mature, framework-native component catalogs. Primer adds automated axe-style accessibility tests. The newer shadcn ports test behaviour with Playwright (Dioxus Components) or parity scripts (shadcn-templ). There is no equivalent in Rust/maud land beyond maud-ui's own gallery.

### Cited Findings
- ViewComponent has built-in previews (`ViewComponent::Preview`). — [viewcomponent.org/guide/previews](https://viewcomponent.org/guide/previews.html)
- Lookbook v2.3.15 [2026-10-15] is the ViewComponent preview UI. — [lookbook version.rb](https://github.com/lookbook-hq/lookbook/blob/main/lib/lookbook/version.rb)
- Primer uses Lookbook for its demo and docs (`demo/app/views/lookbook/…`, `lib/primer/yard/lookbook_docs_helper.rb`). It runs a dedicated `test-accessibility.yml` workflow (`bundle exec rake test:accessibility`) and ships an accessibility linter config (`lib/primer/view_components/linters/accessibility.yml`). — [primer/view_components](https://github.com/primer/view_components); [test-accessibility.yml](https://github.com/primer/view_components/blob/main/.github/workflows/test-accessibility.yml)
- Phoenix Storybook v1.5.0 [2026-09-22] is the LiveView component catalog. — [phenixdigital/phoenix_storybook](https://github.com/phenixdigital/phoenix_storybook)
- Dioxus Components ships a `preview/` gallery app and per-component Playwright specs (`playwright/accordion.spec.ts`, `alert-dialog.spec.ts`, `calendar.spec.ts`, …). — [DioxusLabs/components playwright](https://github.com/DioxusLabs/components/tree/main/playwright)
- shadcn-templ has a `parity/` suite: `a11y.mjs`, `behavior.mjs`, `compare.mjs`, `escape.mjs` and an htmx server. — [templui parity](https://github.com/templui/templui/tree/main/parity)
- maud-ui has a live gallery and a `/theme` customiser, and its repo contains gallery screenshot captures at 1280 and 390 widths. — [docs.rs/crate/maud-ui](https://docs.rs/crate/maud-ui/latest); [maud-ui docs/](https://github.com/hgeldenhuys/maud-ui/tree/main/docs)
- Petal publishes an MCP server for AI tools, and live docs at petal.build. — [petal_components README](https://github.com/petalframework/petal_components)

### Inferences
- The peer bar for a "batteries-included" framework is a dev-mode component catalog: Lookbook / Phoenix Storybook parity, with each component's variants rendered and an a11y check (axe) in CI. Autumn's existing Rust test infrastructure could host maud render snapshots plus a browser-based axe sweep behind its existing `system-tests` feature.

### Gaps
- Phoenix Storybook's a11y and visual-regression features, and Lookbook's axe integration, were not confirmed from primary docs.
- Whether Flux, Filament or Basecoat publish automated a11y test suites was not verified.

---

## 4. Theming approach

### Takeaway
Tailwind v4 is near-universal. Theming is done with CSS custom properties: shadcn-style tokens (`--background`, `--primary`, …) or daisyUI theme plugins. A light/dark switch through `data-theme` on `<html>` is the default everywhere. Phoenix 1.8 and maud-ui ship the toggle out of the box.

### Cited Findings
- Phoenix 1.8: daisyUI theme plugin with custom light and dark themes, `data-theme` attribute, and a `theme_toggle` component in layouts. — [app.css.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_assets/app.css.eex); [layouts.ex.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_web/components/layouts.ex.eex)
- daisyUI 5 has 35 bundled themes. — [daisyui src/themes](https://github.com/saadeghi/daisyui/tree/master/packages/daisyui/src/themes)
- Basecoat is "compatible with shadcn/ui themes" and supports dark mode. It ships style packs (Vega, Nova, Maia, Lyra, Mira, Luma, Sera, Rhea), each importable as `basecoat-css/<style>`, plus a styleless base. — [basecoat README](https://github.com/hunvreus/basecoat)
- maud-ui uses `--mui-*` CSS tokens, `data-theme` dark/light, `data-brand` and `data-density` (compact, comfortable, spacious), and an `mui-` class prefix that coexists with Tailwind. Its live customiser exports `brand.css`. — [docs.rs/crate/maud-ui](https://docs.rs/crate/maud-ui/latest)
- Flux: colours are set by props using Tailwind colour names, there is an accent system (`accent` stub), and its guides cover theming and dark mode. — [fluxui.dev/components](https://fluxui.dev/components); [flux stubs](https://github.com/livewire/flux/tree/main/stubs/resources/views/flux)
- Petal is built on Tailwind v4 and installs by adding `@source "../deps/petal_components/**/*.*ex"` to `app.css`. — [petal README](https://github.com/petalframework/petal_components)
- Rust/UI uses Tailwind v4 with `tw_merge` for class merging. Dioxus Components ships `dx-components-theme.css`. — [rust-ui/ui README](https://github.com/rust-ui/ui); [DioxusLabs/components preview/assets](https://github.com/DioxusLabs/components/tree/main/preview/assets)
- Thaw follows Fluent Design with a `theme` module and `config_provider`. — [thaw/src](https://github.com/thaw-ui/thaw/tree/main/thaw/src)
- django-unfold: Tailwind with "dark mode, customizable colors". — [unfold README](https://github.com/unfoldadmin/django-unfold)

### Inferences
- The peer default is CSS-variable tokens compatible with shadcn themes, plus `data-theme` light/dark/system and a toggle component. Shipping CSS that does not *require* a Tailwind build step (as Basecoat's CDN bundles and maud-ui do) matters for a Rust framework with no Node toolchain.

### Gaps
- Exact token names and theme APIs for Flux, Web Awesome and shadcn-templ were not extracted.

---

## 5. Form integration

### Takeaway
Form-field binding with error rendering is the single most consistent "framework-owned" component. Phoenix's `input` takes a `FormField` and translates errors. Primer has a Ruby forms DSL. crispy-forms renders Django forms through template packs. Filament has a declarative schema builder. Admin generators (Filament, Backpex, Avo, Unfold) go further, to typed field catalogs.

### Cited Findings
- Phoenix `input/1` pattern-matches `%Phoenix.HTML.FormField{}`, pulls errors, and calls `translate_error/1` (Gettext). It supports `options` for select through `Phoenix.HTML.Form.options_for_select/2`, plus `multiple`, `error_class` and others. — [core_components.ex.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_web/components/core_components.ex.eex)
- Primer has a full forms framework with ValidationMessage, Caption, FormControl and the field types listed in §1. — [lib/primer/forms](https://github.com/primer/view_components/tree/main/lib/primer/forms)
- crispy-forms uses swappable template packs (Bootstrap, Tailwind, Bulma, Foundation). — [crispy-forms README](https://github.com/django-crispy-forms/django-crispy-forms)
- Filament forms offer 27+ field types, including Repeater, Builder, RichEditor, TableSelect and ModalTableSelect, all composed through the schemas package (Section, Tabs, Wizard, Grid). — [filament forms](https://github.com/filamentphp/filament/tree/5.x/packages/forms/src/Components)
- Flux has `field`, `label`, `description`, `error`, `fieldset` and `legend` stubs, plus `with-field` / `with-inline-field` wrappers. Buttons with `wire:click` show a loading state automatically. — [flux stubs](https://github.com/livewire/flux/tree/main/stubs/resources/views/flux); [fluxui.dev/components](https://fluxui.dev/components)
- Avo has 40+ field types. Backpex has 18 field types. — [avo fields](https://github.com/avo-hq/avo/tree/main/lib/avo/fields); [backpex fields](https://github.com/naymspace/backpex/tree/develop/lib/backpex/fields)

### Inferences
- Autumn's peer bar: a `field`/`input` component bound to its form/validation types, with error and label rendering, i18n of error messages, select options helpers and a loading state on htmx-submitted buttons, matching Flux's `wire:click` behaviour.

### Gaps
- Not checked: how Basecoat and shadcn-templ bind to server-side validation. They appear to be markup only.

---

## 6. Data table heavyweights (admin and table builders)

### Takeaway
Filament Tables is the reference heavyweight: typed columns, filters (including a QueryBuilder filter), editable columns, and grouping. Backpex and Avo provide the same for LiveView and Rails. Generic component libraries mostly stop at a styled `table`. Petal, RubyUI, maud-ui and Rust/UI each add a `data_table`, and Web Awesome sells a Pro Data Grid.

### Cited Findings
- Filament table columns include editable `SelectColumn`, `TextInputColumn`, `ToggleColumn` and `CheckboxColumn`, plus `ColumnGroup`. Filters include `QueryBuilder`, `TernaryFilter` and `TrashedFilter`. Filament also ships a separate `query-builder` package. — [filament tables](https://github.com/filamentphp/filament/tree/5.x/packages/tables/src)
- Phoenix core `table` supports LiveView streams (`phx-update="stream"`), `row_click` and an `action` slot, but has no sort, filter or pagination. — [core_components.ex.eex](https://github.com/phoenixframework/phoenix/blob/main/installer/templates/phx_web/components/core_components.ex.eex)
- `data_table` components exist in Petal (`data_table.ex`, `filters.ex`, `pagination.ex`), RubyUI (`data_table` Stimulus controller), maud-ui (Tier-3 Data Table plus an AG Grid shell) and Rust/UI (`data_table`, `data_grid`). Web Awesome's Data Grid is Pro and experimental (3.11). — [petal lib](https://github.com/petalframework/petal_components/tree/main/lib/petal_components); [ruby_ui](https://github.com/ruby-ui/ruby_ui); [docs.rs/crate/maud-ui](https://docs.rs/crate/maud-ui/latest); [rust-ui/ui](https://github.com/rust-ui/ui); [webawesome.com/docs/components](https://webawesome.com/docs/components/)
- Backpex filters are boolean, multi_select, range and select. Avo is a full admin with 40+ fields. — [backpex filters](https://github.com/naymspace/backpex/tree/develop/lib/backpex/filters); [avo](https://github.com/avo-hq/avo)

### Inferences
- The peer bar for "data table" has three levels:
  1. A styled table with column and action slots (Phoenix).
  2. A data table with server-driven sort, filter and paginate (Petal, RubyUI, maud-ui). This maps naturally onto htmx partial swaps.
  3. A resource/admin builder (Filament, Backpex, Avo).
- Autumn's htmx model suits level 2 particularly well, because server-side sort, filter and paginate is just a fragment swap.

### Gaps
- Feature-level detail (column sort/resize, row selection, bulk actions) was not extracted for each data table.

---

## 7. Peer baseline: components standard across server-side peers

### Takeaway
Most major peer libraries share a core of roughly 30 components. Counting across Phoenix core plus daisyUI, Petal, SaladUI, RubyUI, Flux free, Primer, Basecoat, shadcn-templ, maud-ui, Dioxus Components, Thaw and Web Awesome, the baseline is:
- **Actions:** button, button group, link, icon.
- **Feedback:** alert/callout/flash, badge, progress, spinner/loading, skeleton, toast.
- **Forms:** field wrapper (label, description, error), input, textarea, select/native select, checkbox, radio group, switch/toggle, slider/range, OTP input.
- **Layout and navigation:** card, separator, breadcrumb, tabs, pagination, sidebar/nav list, navbar/header, accordion/collapsible.
- **Overlays:** dialog/modal, alert dialog, drawer/sheet, dropdown menu, popover, tooltip.
- **Data display:** table, avatar, empty state, kbd.

The common "advanced" tier is combobox/autocomplete, command palette, date picker/calendar, data table, carousel, chart, file upload, context menu, hover card and theme toggle. Peers routinely charge for this tier (Flux Pro, Web Awesome Pro, Fluxon) or delegate it to third-party JS.

### Cited Findings
- Each item above appears in the source-tree lists in §1 for at least eight of these libraries: [Petal](https://github.com/petalframework/petal_components/tree/main/lib/petal_components), [SaladUI](https://github.com/bluzky/salad_ui/tree/main/lib/salad_ui), [RubyUI](https://github.com/ruby-ui/ruby_ui), [Flux](https://github.com/livewire/flux/tree/main/stubs/resources/views/flux), [Basecoat](https://github.com/hunvreus/basecoat/tree/main/src/css/components), [shadcn-templ](https://github.com/templui/templui/tree/main/components), [maud-ui](https://docs.rs/crate/maud-ui/latest), [Dioxus Components](https://github.com/DioxusLabs/components/tree/main/primitives/src), [Thaw](https://github.com/thaw-ui/thaw/tree/main/thaw/src), [Web Awesome](https://github.com/shoelace-style/webawesome/tree/next/packages/webawesome/src/components), [daisyUI](https://github.com/saadeghi/daisyui/tree/master/packages/daisyui/src/components) and [Primer](https://github.com/primer/view_components/tree/main/app/components/primer)
- Paid or "Pro" advanced tier: Flux Pro (combobox, date/time picker, calendar, command, context menu, editor, file upload, kanban, charts, slider, tabs, accordion) — [fluxui.dev/pricing](https://fluxui.dev/pricing). Web Awesome Pro (combobox, date picker/input, file input, data grid, charts) — [webawesome.com/docs/components](https://webawesome.com/docs/components/). Fluxon UI is commercial — [Elixir Forum](https://elixirforum.com/t/fluxon-ui-a-modern-ui-components-lib-for-phoenix-liveview/67551).

### Inferences
- **Approximate component counts:**

  | Library | Stack | Approx. components | Interactivity | Licence |
  |---|---|---|---|---|
  | Phoenix 1.8 core | LiveView | 7 (+ layouts: app, flash_group, theme_toggle) | LiveView JS | MIT |
  | Petal v4.18 | LiveView | ~75 | LiveView JS + JS file | MIT (Pro tier separate) |
  | SaladUI 1.0-beta.6 | LiveView | ~40 | hook + state machines | MIT |
  | Mishka Chelekom 0.0.10-alpha | LiveView | ~80 (+70 JS hooks) | hooks | MIT |
  | Primer VC 0.53.5 | Rails | ~70 + forms DSL | Catalyst/custom elements | MIT |
  | RubyUI | Rails/Phlex | ~45 | Stimulus | MIT |
  | Flux 2.x | Livewire | ~45 free / 52+ total | `<ui-*>` elements | free + Pro ($149–$799) |
  | Filament 5.10 | Livewire | 27 fields, 13 columns, 6 filters, 8 entries | Livewire/Alpine | MIT |
  | Basecoat 1.0.2 | any | ~38 | vanilla JS (13 modules) | MIT |
  | daisyUI 5.7 | any | ~60 classes, 35 themes | CSS only | MIT |
  | Web Awesome 3.14 | any | ~70 free + Pro | web components | free + Pro |
  | shadcn-templ 2.0-beta.15 | Go/templ | ~60 + blocks | vanilla JS | MIT |
  | maud-ui 0.21 | Rust/maud/htmx | 84 (+31 blocks) | vanilla JS + htmx | MIT |
  | Dioxus Components | Dioxus | ~36 primitives + styled | Rust/WASM | MIT/Apache |
  | Thaw 0.5-beta | Leptos | ~60 | Rust/WASM | MIT |
  | Loco 1.2 | Rust | none | — | — |

  Counts are derived from the source trees cited in §1. Licences are as commonly published for MIT projects; licence files were not individually verified.
- To meet the peer bar, Autumn needs at least the ~30-component baseline, with native-`<dialog>`/`popover` overlays and htmx-safe vanilla controllers. To *lead* among Rust server-rendered frameworks, it needs free, accessible versions of combobox, date picker and data table, which peers paywall. Loco ships nothing, so this would differentiate Autumn among Rust "Rails-like" frameworks. maud-ui is the direct competitor or potential integration candidate.

### Gaps
- Licences for each library (especially Mishka, RubyUI and Dioxus Components) were not individually confirmed from LICENSE files.
- Download and adoption figures (hex.pm, rubygems, crates.io) were not collected.
