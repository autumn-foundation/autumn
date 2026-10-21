# Headless Accessible Primitives, WAI-ARIA APG Patterns, and Native HTML Platform Primitives (as of Oct 2026)

Scope note: research was run on 2026-10-21. Component lists were pulled live from each library's docs or GitHub. Browser-support figures for features that changed in 2025–2026 were checked against caniuse / MDN / web.dev / WebKit release notes. Older, settled support data (dialog, inert, `<search>`, `:user-invalid`, `<details name>`, popover) is given from MDN/caniuse as I know it and was not re-fetched this session. These items are marked "(not re-fetched)". The per-pattern APG keyboard summaries come from the APG pattern pages linked on each row. Only the APG index was fetched live. The pattern pages themselves have not changed in substance since APG 1.2, so treat the summaries as the standard APG content and spot-check before quoting them verbatim.

---

## 1. The full APG pattern list and the keyboard/focus contract for each

### Takeaway
The W3C APG lists **30 patterns**. No pattern on the index is marked new or changed as of Oct 2026. The real "behavioral contract" comes down to a few cross-cutting mechanics:
- **Roving focus / `aria-activedescendant`**: one Tab stop, then arrow keys inside the widget. Used by listbox, menu, menubar, radio group, tabs, toolbar, tree, grid, treegrid, and the combobox popup.
- **Home/End**
- **Typeahead**: listbox, menu, tree.
- **Escape closes and returns focus to the invoker**: dialog, menu button, combobox, tooltip.
- **Focus trap plus focus restore**: modal dialog and alertdialog.
- **ARIA state sync**: `aria-expanded`, `aria-selected`, `aria-checked`, `aria-pressed`, `aria-valuenow`.

### Cited Findings
- The APG index lists exactly 30 patterns, in this order: Accordion, Alert, Alert and Message Dialogs (`alertdialog`), Breadcrumb, Button, Carousel, Checkbox, Combobox, Dialog (Modal) (`dialog-modal`), Disclosure, Feed, Grid, Landmarks, Link, Listbox, Menu and Menubar (`menubar`), Menu Button, Meter, Radio Group (`radio`), Slider, Slider (Multi-Thumb), Spinbutton, Switch, Table, Tabs, Toolbar, Tooltip, Tree View (`treeview`), Treegrid, Window Splitter (`windowsplitter`). None is flagged new or changed. — [APG Patterns](https://www.w3.org/WAI/ARIA/apg/patterns/)

Per-pattern contract. URLs are `https://www.w3.org/WAI/ARIA/apg/patterns/<slug>/`.

| Pattern | Roles/ARIA | Keyboard + focus requirements |
|---|---|---|
| [Accordion](https://www.w3.org/WAI/ARIA/apg/patterns/accordion/) | header buttons with `aria-expanded`, `aria-controls`; optional `region` panels | Enter/Space toggles. Tab moves through headers and panel content. Optional: Down/Up move between headers, Home/End go to first/last header. |
| [Alert](https://www.w3.org/WAI/ARIA/apg/patterns/alert/) | `role="alert"` (live region) | No keyboard interaction. It must not move focus. |
| [Alertdialog](https://www.w3.org/WAI/ARIA/apg/patterns/alertdialog/) | `role="alertdialog"`, `aria-modal`, `aria-describedby` | Same as modal dialog: trap focus, Escape closes, focus returns to the invoker. Initial focus usually goes to the least-destructive action. |
| [Breadcrumb](https://www.w3.org/WAI/ARIA/apg/patterns/breadcrumb/) | `nav` + `aria-label`, `aria-current="page"` | None beyond links. |
| [Button](https://www.w3.org/WAI/ARIA/apg/patterns/button/) | `button`; toggle uses `aria-pressed`; menu button uses `aria-haspopup` | Enter/Space activate. Focus after activation depends on context, e.g. it moves into an opened dialog. |
| [Carousel](https://www.w3.org/WAI/ARIA/apg/patterns/carousel/) | `region`/`group` with `aria-roledescription="carousel"`/`"slide"`; rotation control; optional tablist picker | Auto-rotation must stop when keyboard focus or hover is inside. Provides Previous/Next/Rotation buttons. A tabbed variant uses tabs-style arrows. |
| [Checkbox](https://www.w3.org/WAI/ARIA/apg/patterns/checkbox/) | `checkbox`, `aria-checked` true/false/mixed | Space toggles. A mixed (tri-state) parent controls a group. |
| [Combobox](https://www.w3.org/WAI/ARIA/apg/patterns/combobox/) | input `role="combobox"`, `aria-expanded`, `aria-controls`, `aria-activedescendant`, `aria-autocomplete` (none/list/both); popup is listbox/grid/tree/dialog | Down/Alt+Down open the popup. Down/Up move the active option while DOM focus stays in the input. Enter accepts. Escape closes, and a second Escape optionally clears. Printable keys filter. Home/End may move the caret. |
| [Dialog (Modal)](https://www.w3.org/WAI/ARIA/apg/patterns/dialog-modal/) | `dialog`, `aria-modal="true"`, `aria-labelledby` | On open, focus moves inside. Tab/Shift+Tab wrap inside the dialog (trap). Escape closes. On close, focus returns to the invoker. Background is inert. |
| [Disclosure](https://www.w3.org/WAI/ARIA/apg/patterns/disclosure/) | button with `aria-expanded`, `aria-controls` | Enter/Space toggle. |
| [Feed](https://www.w3.org/WAI/ARIA/apg/patterns/feed/) | `feed` > `article` with `aria-posinset`/`aria-setsize`, `aria-busy` while loading | PageDown/PageUp move between articles. Ctrl+End/Ctrl+Home exit the feed. Content loads as the user scrolls. |
| [Grid](https://www.w3.org/WAI/ARIA/apg/patterns/grid/) | `grid`/`row`/`gridcell`, `aria-rowcount`/`aria-colindex` | One Tab stop with 2-D roving focus: arrows, Home/End (row), Ctrl+Home/End (grid), PageUp/Down. Optional selection with Shift/Ctrl+Space. Enter/F2 enter cell-edit mode and Escape exits. Layout grids follow the same model. |
| [Landmarks](https://www.w3.org/WAI/ARIA/apg/patterns/landmarks/) | `banner`, `main`, `nav`, `complementary`, `contentinfo`, `region`, `search`, `form` | No widget keyboard behaviour. Screen readers navigate by landmark. |
| [Link](https://www.w3.org/WAI/ARIA/apg/patterns/link/) | native `a[href]` preferred | Enter activates. |
| [Listbox](https://www.w3.org/WAI/ARIA/apg/patterns/listbox/) | `listbox`/`option`, `aria-selected`, `aria-multiselectable`, `aria-activedescendant` or roving tabindex | Up/Down move between options and Home/End jump to the ends. Typeahead applies. Multi-select adds Space, Shift+Up/Down, Ctrl+A, Ctrl+Shift+Home/End. |
| [Menu and Menubar](https://www.w3.org/WAI/ARIA/apg/patterns/menubar/) | `menu`/`menubar`/`menuitem`/`menuitemcheckbox`/`menuitemradio`, `aria-haspopup`, `aria-expanded` | Roving focus with one Tab stop. Arrows move along the menu axis and the cross axis opens or closes submenus. Home/End and typeahead apply. Enter/Space activate. Escape closes and returns focus to the parent item or button. Tab leaves and closes the menu. |
| [Menu Button](https://www.w3.org/WAI/ARIA/apg/patterns/menu-button/) | button `aria-haspopup="menu"`, `aria-expanded` | Enter/Space/Down open the menu and focus the first item. Up opens it and focuses the last item. Escape returns focus to the button. |
| [Meter](https://www.w3.org/WAI/ARIA/apg/patterns/meter/) | `meter`, `aria-valuenow/min/max` | Not interactive. |
| [Radio Group](https://www.w3.org/WAI/ARIA/apg/patterns/radio/) | `radiogroup`/`radio`, `aria-checked` | One Tab stop, on the checked radio. Arrows move focus and check the next radio, wrapping at the ends. Space checks the focused radio. |
| [Slider](https://www.w3.org/WAI/ARIA/apg/patterns/slider/) | `slider`, `aria-valuenow/min/max/valuetext`, `aria-orientation` | Right/Up increase and Left/Down decrease. Home/End set min/max. PageUp/PageDown take larger steps. |
| [Slider (Multi-Thumb)](https://www.w3.org/WAI/ARIA/apg/patterns/slider-multithumb/) | each thumb is a `slider` | Same keys per thumb, and each thumb is its own Tab stop. Thumbs cannot cross: each thumb's min/max updates dynamically. |
| [Spinbutton](https://www.w3.org/WAI/ARIA/apg/patterns/spinbutton/) | `spinbutton`, `aria-valuenow/min/max/valuetext` | Up/Down step. PageUp/PageDown take larger steps. Home/End set min/max. Text entry is allowed. |
| [Switch](https://www.w3.org/WAI/ARIA/apg/patterns/switch/) | `switch`, `aria-checked` | Space toggles, and Enter optionally toggles too. |
| [Table](https://www.w3.org/WAI/ARIA/apg/patterns/table/) | native `table` or `table`/`row`/`cell`; sortable headers use `aria-sort` | Not a composite widget: there is no grid navigation. |
| [Tabs](https://www.w3.org/WAI/ARIA/apg/patterns/tabs/) | `tablist`/`tab`/`tabpanel`, `aria-selected`, `aria-controls` | One Tab stop on the active tab. Left/Right (or Up/Down if vertical) move between tabs, wrapping at the ends. Home/End jump to the ends. Activation is automatic on focus or manual with Enter/Space. The next Tab goes into the panel. Delete optionally closes a tab. |
| [Toolbar](https://www.w3.org/WAI/ARIA/apg/patterns/toolbar/) | `toolbar`, `aria-orientation` | One Tab stop. Arrows move between controls and Home/End jump to the ends. Focus memory is optional. |
| [Tooltip](https://www.w3.org/WAI/ARIA/apg/patterns/tooltip/) | `tooltip`, trigger references it with `aria-describedby` | Appears on focus and on hover. Escape dismisses. The tooltip itself never receives focus. APG still marks this pattern as work-in-progress. |
| [Tree View](https://www.w3.org/WAI/ARIA/apg/patterns/treeview/) | `tree`/`treeitem`/`group`, `aria-expanded`, `aria-selected`, `aria-level/setsize/posinset` | Roving focus. Up/Down move to the visible previous/next node. Right expands, or moves to the first child if already open. Left collapses, or moves to the parent. Home/End jump to the ends. Typeahead applies. `*` expands all siblings. Multi-select modifiers are supported. |
| [Treegrid](https://www.w3.org/WAI/ARIA/apg/patterns/treegrid/) | `treegrid`/`row`/`gridcell`, `aria-expanded` on rows | Combines the grid's 2-D navigation with tree expand/collapse. Focus can sit on a row or a cell, and Right/Left expand or collapse a row when it has focus. |
| [Window Splitter](https://www.w3.org/WAI/ARIA/apg/patterns/windowsplitter/) | `separator` (focusable), `aria-valuenow/min/max`, `aria-controls` | Left/Right (or Up/Down) move the splitter. Enter toggles collapse. Home/End are optional. F6 optionally cycles between panes. |

### Inferences
- About half the patterns need no custom keyboard code when built on the right native element: alert, breadcrumb, button, checkbox, landmarks, link, meter, switch-as-checkbox, table, and disclosure via `<details>`. The hard tier is the composite widgets that need roving focus or active-descendant: combobox, listbox, menu/menubar, radio group (native radios already do this), tabs, toolbar, tree, grid, treegrid, and feed.
- A server-rendered library can treat "single Tab stop + arrow navigation + Home/End + typeahead" as one shared JS behavior (a roving-focus helper). Such a helper covers about 8 of the 30 patterns. Radix uses exactly this factoring (`roving-focus`, `focus-scope`, `dismissable-layer` packages, see §2).

### Gaps
- The APG pattern detail pages were not individually fetched in this session, so the keyboard summaries above are standard APG content and were not re-verified live. The "Tooltip is work-in-progress" note should be re-checked on the tooltip page.

---

## 2. Component inventory per headless library, and the cross-library baseline

### Takeaway
Only nine patterns appear in all five libraries:
- dialog (and alert-dialog)
- popover
- menu / menu button
- tabs
- checkbox
- radio group
- switch
- select/listbox
- disclosure/collapsible

Three more sit at 4 of 5:
- **Combobox**: missing only from Radix.
- **Tooltip, slider, accordion, toast**: missing only from Headless UI.

Rich widgets are the differentiators, held by only some libraries:
- **Date/time, color, drag-and-drop, virtualized grid/table/tree**: React Aria; Ark/Zag covers date picker, color picker and tree view.
- **Splitter, carousel, tags input, file upload, steps**: Ark/Zag.
- **OTP, number field, navigation menu, menubar, context menu**: Radix and Base UI, plus Ark for most of these.

### Cited Findings

**Radix UI Primitives**
- The recommended install is now the unified tree-shakeable `radix-ui` package, e.g. `import { Dialog, DropdownMenu, Tooltip } from "radix-ui"`. Individual `@radix-ui/react-*` packages still exist. — [Radix intro](https://www.radix-ui.com/primitives/docs/overview/introduction)
- `packages/react` on GitHub contains these component packages: accessible-icon, accordion, alert-dialog, aspect-ratio, avatar, checkbox, collapsible, context-menu, dialog, dropdown-menu, form, hover-card, label, menubar, navigation-menu, one-time-password-field, password-toggle-field, popover, progress, radio-group, scroll-area, select, separator, slider, switch, tabs, toast, toggle, toggle-group, toolbar, tooltip, visually-hidden.
  - Behavior utilities: announce, collection, dismissable-layer, focus-guards, focus-scope, popper, portal, presence, roving-focus, direction, slot.
  - There is **no combobox, date picker, tree, grid/table, number field, or calendar** package. — [radix-ui/primitives packages/react](https://github.com/radix-ui/primitives/tree/main/packages/react)
- The Radix Accordion keyboard model: Space/Enter toggle, Tab/Shift+Tab move, Down/Up move between triggers when vertical, Left/Right when horizontal, Home/End jump to the ends. This is the full APG optional set. — [Radix Accordion](https://www.radix-ui.com/primitives/docs/components/accordion)

**React Aria Components (Adobe)**
- Docs moved to `react-aria.adobe.com` (301 from `react-spectrum.adobe.com/react-aria/components.html`). The site says "Over 50 components". — [React Aria home](https://react-aria.adobe.com/)
- Source files in `packages/react-aria-components/src`, grouped here by function:
  - Buttons and actions: Button, ToggleButton, ToggleButtonGroup, Link, Breadcrumbs, Toolbar, FileTrigger, DropZone
  - Forms: Checkbox, RadioGroup, Switch, TextField, TextArea, SearchField, NumberField, Input, Label, FieldError, Form, Group
  - Pickers: Select, ComboBox, Autocomplete, ListBox, Menu, TagGroup, TokenField
  - Date and time: DateField, DatePicker, Calendar, HiddenDateInput
  - Color: ColorArea, ColorField, ColorPicker, ColorSlider, ColorSwatch, ColorSwatchPicker, ColorThumb, ColorWheel
  - Overlays: Dialog, Modal, Popover, Tooltip, OverlayArrow, Sheet, PreviewTrigger
  - Collections and status: Disclosure, GridList, Table, Tree, NavigationTree, Tabs, Meter, ProgressBar, Slider, Toast, Separator, Heading, Text
  - Drag and drop: DragAndDrop, useDragAndDrop, TreeDropTargetDelegate
  - Virtualization and layout: Virtualizer, GridLayout, TableLayout
  - Other: Keyboard, SelectionIndicator, SharedElementTransition
  - — [react-spectrum GitHub src](https://github.com/adobe/react-spectrum/tree/main/packages/react-aria-components/src)
- React Aria is the only library with a full **Table/GridList/Tree with keyboard grid navigation, drag and drop, and virtualization**, a full **date/time family** (DateField segments, DatePicker, DateRangePicker, Calendar, RangeCalendar), and a **color family**. — same source
- The Calendar exposes `focusedValue`, `onFocusChange`, `autoFocus`, and `pageBehavior` (page by visible duration or by single unit). — [React Aria Calendar](https://react-aria.adobe.com/Calendar)

**Headless UI (Tailwind Labs)**
- React docs show **v2.1**. The React components are Menu, Disclosure, Dialog, Popover, Tabs, Transition, Button, Checkbox, Combobox, Fieldset, Input, Listbox, Radio Group, Select (a styled native select), Switch, Textarea. The Vue docs link to `/v1/vue`. There is **no tooltip, slider, accordion, toast, date picker, or tree**. — [headlessui.com](https://headlessui.com/)

**Base UI (MUI)**
- npm badge shows **1.9.0** (the 1.x line is stable).
- 37 components: Accordion, Alert Dialog, Autocomplete, Avatar, Button, Checkbox, Checkbox Group, Collapsible, Combobox, Context Menu, Dialog, Drawer, Field, Fieldset, Form, Input, Menu, Menubar, Meter, Navigation Menu, Number Field, OTP Field, Popover, Preview Card (hover card), Progress, Radio Group, Scroll Area, Select, Separator, Slider, Switch, Tabs, Toast, Toggle, Toggle Group, Toolbar, Tooltip.
- Utilities: CSP Provider, Direction Provider, mergeProps, useRender.
- There is **no date picker, tree, or table/grid**.
- — [Base UI quick start](https://base-ui.com/react/overview/quick-start)

**Ark UI / Zag.js**
- Ark UI is a headless library with "45+ accessible components", "Driven by State Machines", and supports React, Solid, Vue, and Svelte. A new **Navigation Menu** is announced. — [ark-ui.com](https://ark-ui.com/)
- Zag.js (the framework-agnostic state machines under Ark) lists 54 components:
  - Accordion, Angle Slider, Avatar, Carousel, Cascade Select (Beta), Checkbox, Clipboard, Collapsible, ColorPicker, Combobox, Date Input (Beta), Date Picker, Dialog, Drawer (Beta), Editable
  - File Upload, Floating Panel, Hover Card, Image Cropper, Listbox, Marquee, Menu, Nested Menu, Context Menu, Navigation Menu, Number Input, Pagination, Password Input, Pin Input, Popover, Presence
  - Progress (Linear/Circular), QR Code, Radio Group, Rating Group, Scroll Area, Segmented Control, Select, Signature Pad, Slider, Range Slider, Splitter, Steps, Switch, Tabs, Tags Input, Timer, Toast, Toggle, Toggle Group, Tooltip, Tour, Tree View
  - Utilities: Async List, Focus Trap, Hotkeys
  - Zag describes itself as "part of the next evolution of Chakra UI". — [zagjs.com](https://zagjs.com/overview/introduction)
- Gap note: the Ark docs path `ark-ui.com/docs/overview/introduction` returned 404 and the Ark homepage did not name Zag. The Ark↔Zag relationship (Ark wraps Zag machines) is well established but was not confirmed from a page fetched this session.

**APG pattern × library coverage matrix** (✓ = dedicated component; ~ = achievable via a related component; – = none)

| APG pattern | Radix | React Aria | Headless UI | Base UI | Ark/Zag | All 5? |
|---|---|---|---|---|---|---|
| Accordion | ✓ | ~ (Disclosure/DisclosureGroup) | ~ (Disclosure) | ✓ | ✓ | 4 dedicated |
| Alert dialog | ✓ | ~ (Dialog role) | ~ (Dialog) | ✓ | ~ (Dialog role) | ✓ (by config) |
| Breadcrumb | – | ✓ | – | – | – | 1 |
| Button / toggle button | ✓ Toggle | ✓ | ✓ | ✓ Button/Toggle | ✓ Toggle | ✓ |
| Carousel | – | – | – | – | ✓ | 1 |
| Checkbox | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Combobox | – | ✓ (+Autocomplete) | ✓ | ✓ (+Autocomplete) | ✓ | 4 |
| Dialog (modal) | ✓ | ✓ | ✓ | ✓ (+Drawer) | ✓ (+Drawer) | ✓ |
| Disclosure | ✓ Collapsible | ✓ | ✓ | ✓ Collapsible | ✓ Collapsible | ✓ |
| Feed | – | – | – | – | – | 0 |
| Grid / table (interactive) | – | ✓ Table/GridList | – | – | – | 1 |
| Listbox / Select | ✓ Select | ✓ | ✓ Listbox | ✓ Select | ✓ Listbox/Select | ✓ |
| Menu / Menu button | ✓ DropdownMenu | ✓ | ✓ | ✓ | ✓ | ✓ |
| Menubar | ✓ | – (not found in src) | – | ✓ | – | 2 |
| Meter / progress | ✓ Progress | ✓ both | – | ✓ both | ✓ Progress | 4 |
| Radio group | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Slider | ✓ (multi-thumb) | ✓ (multi-thumb) | – | ✓ | ✓ (+Range, Angle) | 4 |
| Spinbutton (number field) | – | ✓ | – | ✓ | ✓ | 3 |
| Switch | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Tabs | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Toolbar | ✓ | ✓ | – | ✓ | – | 3 |
| Tooltip | ✓ | ✓ | – | ✓ | ✓ | 4 |
| Tree view | – | ✓ | – | – | ✓ | 2 |
| Treegrid | – | ~ (Table/Tree; unconfirmed) | – | – | – | ≤1 |
| Window splitter | – | – | – | – | ✓ Splitter | 1 |
| Popover (non-APG) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Toast (non-APG) | ✓ | ✓ | – | ✓ | ✓ | 4 |
| Hover card / preview (non-APG) | ✓ | ~ PreviewTrigger | – | ✓ | ✓ | 3–4 |
| Date/calendar (non-APG; uses grid) | – | ✓ | – | – | ✓ | 2 |
| OTP / pin input | ✓ (preview) | – | – | ✓ | ✓ | 3 |
| Tags / token input | – | ✓ TagGroup/TokenField | – | – | ✓ | 2 |
| Navigation menu | ✓ | – | – | ✓ | ✓ | 3 |
| Context menu | ✓ | – | – | ✓ | ✓ | 3 |

(Matrix compiled from the sources cited above in this section.)

### Inferences
- **Baseline bar** (what a "serious" library is expected to have, since all five ship it): dialog/alertdialog, popover, menu button + menu, select/listbox, tabs, checkbox, radio group, switch, disclosure. Close to baseline (4 of 5): combobox, tooltip, slider, accordion, toast, progress/meter.
- **Second tier** (2–3 libs): number field/spinbutton, toolbar, menubar, navigation menu, context menu, hover card, OTP input, tree view, date picker.
- **Rare** (React Aria or Zag only): interactive grid/table, treegrid, drag and drop, virtualization, color pickers, carousel, window splitter, breadcrumb (trivial anyway). **Feed** has no headless implementation in any of the five.
- The shared low-level utilities these libraries factor out are the behaviors a server-rendered framework would need as small JS modules: roving focus, focus scope/trap, dismissable layer (outside-click + Escape), presence (exit animations), popper/anchor positioning, collection/typeahead, and live-region announce. Radix exposes them as named packages; Zag exposes Focus Trap and Hotkeys.

### Gaps
- I could not get React Aria's rendered sidebar, so the category labels and stability flags (alpha/beta/RC) for Toast, Tree, Virtualizer, Autocomplete, Sheet, NavigationTree, TokenField, PreviewTrigger and SharedElementTransition are unconfirmed. Some may be unreleased or unstable files in `src`.
- The Radix doc sidebar would not render. Preview status of `one-time-password-field` and `password-toggle-field` is from prior knowledge and was not re-verified.
- The Headless UI page shows v2.1. A newer 2.2.x may exist. Unverified.
- Whether React Aria supports treegrid (expandable Table rows) was not verified.

---

## 3. Native HTML/CSS primitives: browser support (Oct 2026) and the widget patterns they satisfy

### Takeaway
By late 2026 a server-rendered framework can ship these with zero JS on Baseline features:
- **Modal dialogs**: `<dialog>` + `commandfor="id" command="show-modal"`, which gives native focus trap/inert, Escape, and top layer.
- **Non-modal popovers/menus/dropdown panels** with light dismiss: `popover` + `popovertarget`/`commandfor`.
- **Anchored positioning**: CSS anchor positioning, Baseline Jan 2026.
- **Exclusive accordions**: `<details name>`.
- **Plus**: `inert`, `<search>`, `:user-invalid`, `field-sizing` (all three engines since mid-2026), same-document view transitions.

Still not Baseline in Oct 2026:
- **Customizable `<select>`** (`appearance: base-select`): Chrome 135+ and Safari 27 (Sept 2026); Firefox not shipped.
- **Interest invokers** (`interestfor`, hover/focus tooltips): Chromium-only since 142.
- **`popover="hint"`**: Chrome + Firefox 149; Safari blocked.
- **`<dialog closedby>`**: Chrome 134 + Firefox 141; Safari only in Technology Preview.

### Cited Findings

**Invoker commands (`commandfor` / `command`)**
- Baseline 2025 "newly available" as of Dec 2025. Supported in Chrome/Edge 135, Firefox 144, Safari 26.2 (`command`, `commandfor`, `CommandEvent`). Built-in commands open and close dialogs and popovers without JS (`show-modal`, `close`, `request-close`, `show-popover`, `hide-popover`, `toggle-popover`), and custom `--commands` dispatch a `command` event to the target. — [InfoQ, Jan 2026](https://www.infoq.com/news/2026/01/html-invoker-commands/); [MDN Invoker Commands API](https://developer.mozilla.org/en-US/docs/Web/API/Invoker_Commands_API); [web.dev Dec 2025](https://web.dev/blog/web-platform-12-2025); [caniuse](https://caniuse.com/wf-invoker-commands)
- The web.dev October 2025 roundup lists "Safari 18" in one place and 26.2 in another. MDN and the December roundup both say Safari 26.2. — [web.dev Oct 2025](https://web.dev/blog/web-platform-10-2025)

**CSS anchor positioning**
- Became Baseline Newly available in January 2026 with Firefox 147. Versions: Chrome/Edge 125, Safari 26, Firefox 147. — [web.dev Jan 2026](https://web.dev/blog/web-platform-01-2026); [caniuse](https://caniuse.com/css-anchor-positioning)
- Caveat: one article notes that Web Platform Status still tags parts of the group as limited, because `position-visibility` and `anchor-size()` landed later. — [buildmvpfast](https://www.buildmvpfast.com/blog/css-anchor-positioning-baseline-delete-floating-ui-2026) (secondary)
- Safari 27 (Sept 17, 2026) changed `position-anchor` to default to `normal` (was `auto`) and follows transformed anchors. It also renamed `anchors-valid`/`anchors-visible` to `anchor-valid`/`anchor-visible`, with the old names temporarily supported. — [WebKit: Safari 27.0 features](https://webkit.org/blog/18325/webkit-features-for-safari-27-0/)

**Customizable `<select>` (`appearance: base-select`)**
- Safari 27.0 (Sept 17, 2026) ships it:
  - `appearance: base-select` on `select` and `::picker(select)`
  - `::picker-icon` and `::checkmark`
  - `<selectedcontent>`
  - arbitrary HTML inside `<option>`
  - — [WebKit Safari 27.0](https://webkit.org/blog/18325/webkit-features-for-safari-27-0/); [WebKit "golden rule of customizable select"](https://webkit.org/blog/18117/the-golden-rule-of-customizable-select/)
- Chromium has supported it since 135. The listbox (in-page, `multiple`/`size`) variant shipped in Chrome 145 and is not supported in Safari or Firefox. — [web-features issue #4551](https://github.com/web-platform-dx/web-features/issues/4551)
- Firefox: experimental, behind a flag in Nightly. **Not Baseline.** — [CODERCOPS](https://blog.codercops.com/blog/css-customizable-select-appearance-base-select-2026); [DEV Community](https://dev.to/olivierleplus/customizable-select-the-dropdown-finally-catches-up-with-css-1c60)
- Sources conflict: CODERCOPS (Sept 2026) said Safari had it only in Technology Preview, but WebKit's own Safari 27.0 release post lists it as shipped. Trust WebKit.
- It degrades to a normal `<select>`, so progressive enhancement is safe. — [WebKit golden rule](https://webkit.org/blog/18117/the-golden-rule-of-customizable-select/)

**Interest invokers (`interestfor`)**
- Not Baseline. Chrome/Edge 142 (Oct 2025), and Chrome Android listed from 149. **Unsupported in Firefox and Safari** through Firefox 154 and Safari 26.5 on caniuse.
- `:interest-target` and `InterestEvent` are flagged limited-availability on MDN.
- A polyfill exists (npm `interestfor`) but cannot fully replicate touchscreen long-press or the CSS side.
- — [caniuse](https://caniuse.com/wf-interest-invokers); [Chromium Intent to Ship](https://groups.google.com/a/chromium.org/g/blink-dev/c/bX1G_yDt6W4); [GoogleChrome modern-web-guidance](https://github.com/GoogleChrome/modern-web-guidance/blob/main/skills/modern-web-guidance/guides/ui-behaviors/interest-triggered-tooltips.md); [MDN :interest-target](https://developer.mozilla.org/en-US/docs/Web/CSS/Reference/Selectors/:interest-target)

**`popover="hint"`**
- Chrome supported. Firefox shipped it in **149** (March 2026). Safari not supported through 26.6 / 27.x previews. The web-features explorer says Baseline has been "blocked since March 2026 by Safari".
- One DEV post says it appeared in Safari Technology Preview 249.
- — [web-features explorer](https://web-platform-dx.github.io/web-features-explorer/features/popover-hint/); [caniuse](https://caniuse.com/wf-popover-hint); [Firefox 149 notes](https://web-standards.dev/news/2026/03/firefox-149/); [DEV STP 249](https://dev.to/leobaniak/safari-technology-preview-249-drops-attr-if-function-popoverhint-and-temporal-in-one-build-4n4e)

**`<dialog closedby>`** (`any` = light dismiss, `closerequest`, `none`)
- Chrome/Edge 134, Firefox 141. Safari: not supported through 26.4, with Technology Preview status inconsistent across tables. **Not Baseline.** — [MDN closedBy](https://developer.mozilla.org/docs/Web/API/HTMLDialogElement/closedBy); [caniuse](https://caniuse.com/mdn-html_elements_dialog_closedby)
- Safari 27 fixed `requestClose()` so it no longer fires duplicate `cancel` events. Popover light dismiss now accounts for input buttons and disabled command buttons. — [WebKit Safari 27.0](https://webkit.org/blog/18325/webkit-features-for-safari-27-0/)

**`field-sizing: content`** (auto-growing inputs/textareas)
- Chrome 123, Safari 26.2, Firefox 152 (≈June 2026). This makes it cross-engine as of mid-2026, i.e. Baseline Newly available.
- Sources differ slightly on the Firefox version; caniuse ranges are most precise.
- Safari 27 fixed placeholder clipping on empty number inputs.
- — [caniuse field-sizing](https://caniuse.com/mdn-css_properties_field-sizing_content); [CODERCOPS](https://blog.codercops.com/blog/css-field-sizing-auto-resize-textarea-guide); [WebKit Safari 27.0](https://webkit.org/blog/18325/webkit-features-for-safari-27-0/)

**View transitions**
- Same-document (`document.startViewTransition`, `view-transition-name`/`-class`, `match-element`, `:active-view-transition`) became Baseline Newly available when Firefox 144 shipped (Oct 14, 2025).
- Cross-document (`@view-transition { navigation: auto }`) is not part of that Baseline.
- — [web.dev Oct 16, 2025](https://web.dev/blog/same-document-view-transitions-are-now-baseline-newly-available); [Chrome: view transitions in 2025](https://developer.chrome.com/blog/view-transitions-in-2025)

**Settled Baseline features** (not re-fetched; MDN/caniuse values as known)
- `<dialog>` + `showModal()`: Baseline March 2022. Chrome 37, Firefox 98, Safari 15.4. Gives top layer, `::backdrop`, inert background, Escape → `cancel`, and focus restore to the previously focused element on close. — [MDN dialog](https://developer.mozilla.org/en-US/docs/Web/HTML/Element/dialog)
- `popover` attribute + `popovertarget`: Baseline 2024. Chrome 114, Firefox 125, Safari 17. Gives top layer, light dismiss and Escape for `popover="auto"`, auto-wiring of `aria-expanded`/`aria-details` on the invoker, and focus-return behavior. — [MDN Popover API](https://developer.mozilla.org/en-US/docs/Web/API/Popover_API)
- `<details name>` (exclusive accordion): Baseline 2024. Chrome 120, Firefox 130, Safari 17.2. — [MDN details](https://developer.mozilla.org/en-US/docs/Web/HTML/Element/details)
- `inert` attribute: Baseline 2023. Chrome 102, Firefox 112, Safari 15.5. — [MDN inert](https://developer.mozilla.org/en-US/docs/Web/HTML/Global_attributes/inert)
- `<search>` element (`role="search"` landmark): Baseline 2023. Chrome 118, Firefox 118, Safari 17. — [MDN search](https://developer.mozilla.org/en-US/docs/Web/HTML/Element/search)
- `:user-invalid` / `:user-valid`: Baseline 2023. Firefox 88, Safari 16.5, Chrome 119. — [MDN :user-invalid](https://developer.mozilla.org/en-US/docs/Web/CSS/:user-invalid)

**Native primitive → APG pattern coverage (zero-JS)**

| Native feature | Status Oct 2026 | Satisfies (fully / partially) |
|---|---|---|
| `<dialog>` + `commandfor command="show-modal"` | Baseline | **Fully:** Dialog (Modal), Alertdialog (add `role="alertdialog"`). Native trap, inert background, Escape, focus restore. Light-dismiss on backdrop click needs `closedby="any"` (not in Safari) or a few lines of JS. |
| `popover` + `popovertarget`/`commandfor` | Baseline | **Mostly:** disclosure-style popovers and non-modal dialogs, with light dismiss, Escape, and top layer. **Partially:** Menu Button. The open/close and `aria-expanded` wiring is native, but the arrow-key roving focus, typeahead, and focus-first-item inside the menu still need JS. |
| CSS anchor positioning (`anchor-name`, `position-anchor`, `position-area`, `position-try`) | Baseline (Jan 2026) | Replaces Floating UI/Popper for placement and collision flipping of tooltips, popovers, menus, and combobox popups. Pure layout, no ARIA. |
| `<details>`/`<summary>` + `name` | Baseline | **Fully:** Disclosure and Accordion (single-open via `name`). Enter/Space work natively. The optional APG Up/Down/Home/End header navigation is absent. |
| Customizable `<select>` (`base-select`) | Chrome + Safari 27; Firefox no | **Fully** (where supported): a styled Select-only combobox / listbox-picker with native keyboard and typeahead, and a fallback to the classic select elsewhere. Does **not** cover an editable/filtering combobox. |
| `interestfor` | Chromium only | **Tooltip and hover card** (hover/focus/long-press show with delay). Needs a polyfill or JS fallback for Firefox and Safari. |
| `popover="hint"` | Chrome + Firefox; Safari no | Tooltip semantics: a hint does not close `auto` popovers. Pair with `interestfor`. |
| `<dialog closedby>` | Chrome + Firefox; Safari no | Light-dismiss modal (sheet/drawer) without JS. |
| `inert` | Baseline | Makes the background inert for custom modal/drawer layers and off-screen carousel slides. |
| `<search>` | Baseline | Landmarks pattern (search landmark). |
| `:user-invalid` | Baseline | Form validation styling after interaction (no APG pattern; supports accessible error UX). |
| `field-sizing: content` | All three engines (mid-2026) | Auto-growing textarea/input. Replaces the JS autosize behavior. |
| Same-document view transitions | Baseline (Oct 2025) | Animated state swaps, e.g. after htmx fragment swaps (htmx supports `transition:true`). No ARIA role. |
| `<input type="range">` | Baseline (long-standing) | **Fully:** single-thumb Slider. **Not** multi-thumb. |
| `<input type="number">` | Baseline | Spinbutton (native up/down; styling limited). |
| `<input type="checkbox" role="switch">` | Baseline (`switch` role on checkbox) | Switch. Safari 17.4+ also has a native `<input type="checkbox" switch>` (not re-fetched). |
| `<meter>`, `<progress>` | Baseline | Meter and progress. |
| `<table>` + `aria-sort` | Baseline | Table pattern (sortable via links or server round-trips). |

### Inferences
- **Fully server-renderable today with zero JS on all engines:** alert, breadcrumb, button, checkbox, disclosure, accordion (single or multi), modal dialog and alertdialog (via invoker commands), non-modal popover, landmarks, link, meter/progress, single-thumb slider, switch, table, spinbutton (native number), and anchored placement of any popup.
- **Zero-JS on Chromium/Safari with graceful fallback in Firefox:** a styled single-select listbox (`base-select`).
- **Zero-JS on Chromium only:** tooltip/hover card (`interestfor`). For a framework targeting all three engines this still needs a small JS fallback, or a focus/hover CSS tooltip (`:hover`/`:focus-within` + anchor positioning), which loses the APG Escape-to-dismiss rule.
- Invoker commands plus `popover` remove most of the "toggle open state" JS that htmx apps traditionally delegated to hyperscript or Alpine.

### Gaps
- No primary-source (MDN/caniuse) fetch this session for `<details name>`, `inert`, `<search>`, `:user-invalid`, base `popover`, base `<dialog>`, or `<input switch>`. Their versions are given from known MDN data and should be spot-checked.
- Firefox's exact timeline for customizable select (Nightly flag only, as of Sept 2026) is from secondary blogs.
- Whether `closedby` shipped in Safari 27.0 stable is unclear. The WebKit 27.0 excerpt I read did not mention it, and the second half of the page (offset 100000+) was not read.
- Whether Safari 27 added `interestfor` was not found in the portion of the WebKit 27.0 notes I read. caniuse says unsupported through 26.5.

---

## 4. Patterns that still fundamentally require JavaScript

### Takeaway
Native HTML now covers the **open/close/layer/dismiss/position** half of the contract. It does **not** cover the **in-widget keyboard model**: roving tabindex or `aria-activedescendant` with arrows, Home/End, typeahead, 2-D grid navigation, tree expand/collapse by arrow keys, editable-combobox filtering, multi-thumb sliders, and date-grid navigation. These remain JS. Each needs a small focused script (a few hundred bytes to a few KB), not a framework.

### Cited Findings
- Combobox requires DOM focus to stay in the input while `aria-activedescendant` tracks the highlighted option. Down/Up move the active option, printable characters filter, and Escape closes or clears. There is no native element for an editable, filtering combobox. `<datalist>` exists, but its popup is UA-controlled and unstyleable (not re-fetched). — [APG Combobox](https://www.w3.org/WAI/ARIA/apg/patterns/combobox/)
- Customizable select covers only a **select-only** picker. WebKit's "golden rule" is that it must remain a select. — [WebKit golden rule](https://webkit.org/blog/18117/the-golden-rule-of-customizable-select/)
- Menu, menubar, tree, grid, treegrid, toolbar, tabs, and listbox all require a single Tab stop plus arrow-key roving and typeahead. — [APG Menubar](https://www.w3.org/WAI/ARIA/apg/patterns/menubar/), [Tree View](https://www.w3.org/WAI/ARIA/apg/patterns/treeview/), [Grid](https://www.w3.org/WAI/ARIA/apg/patterns/grid/), [Tabs](https://www.w3.org/WAI/ARIA/apg/patterns/tabs/), [Toolbar](https://www.w3.org/WAI/ARIA/apg/patterns/toolbar/)
- Every headless library ships these as reusable internals:
  - Radix: `roving-focus`, `focus-scope`, `focus-guards`, `dismissable-layer`, `collection`, `announce` — [Radix packages](https://github.com/radix-ui/primitives/tree/main/packages/react)
  - Zag: "Focus Trap", "Hotkeys", "Async List" — [Zag](https://zagjs.com/overview/introduction)
- Multi-thumb sliders need thumbs that cannot cross, with dynamic `aria-valuemin`/`aria-valuemax` per thumb. Native range is single-thumb. — [APG Multi-thumb Slider](https://www.w3.org/WAI/ARIA/apg/patterns/slider-multithumb/)
- Date pickers, calendars, and color pickers exist only in the JS libraries (React Aria, Zag). — [react-aria src](https://github.com/adobe/react-spectrum/tree/main/packages/react-aria-components/src); [Zag](https://zagjs.com/overview/introduction)
- Tooltip on hover/focus with Escape dismissal is native only via `interestfor` (Chromium-only). — [caniuse interest invokers](https://caniuse.com/wf-interest-invokers)

### Inferences
JS-required matrix for a server-rendered library:

| Pattern | Native coverage | Remaining JS |
|---|---|---|
| Combobox (editable/autocomplete) | anchor positioning, popover for the popup | `aria-activedescendant` tracking, arrow keys, filtering. htmx can do the filtering server-side (`hx-trigger="input changed delay:200ms"`), but the keyboard model is still JS. |
| Listbox (standalone, multi-select) | `<select multiple>` (unstyled; base-select listbox is Chrome 145 only) | roving/active-descendant, Shift/Ctrl range select, typeahead |
| Menu / menubar / context menu | popover + commandfor + anchor | roving focus, typeahead, submenu open/close by arrows, focus-first-item on open. Context menu needs a `contextmenu` event handler. |
| Tabs | none (CSS `:target` / radio hacks are not APG-compliant) | arrow roving and `aria-selected` sync. With htmx, tab panels can be server-fetched, but the tablist keyboard still needs JS. |
| Toolbar, radio-like segmented control | native radios cover the radio-group arrows | toolbar roving |
| Tree view / treegrid / grid | none | 2-D/hierarchical roving, expand/collapse, typeahead, selection |
| Feed | none | PageUp/PageDown article navigation, `aria-busy`. Infinite load is doable via htmx `revealed`. |
| Slider (multi-thumb), angle slider | `<input type=range>` single only | full slider |
| Window splitter | none | drag and keyboard resize |
| Carousel | scroll-snap + CSS `::scroll-button()`/`::scroll-marker()` (Chromium-only; not researched this session) | rotation control, pause-on-focus, slide `inert` toggling |
| Tooltip | `interestfor` (Chromium only) | cross-browser hover/focus delay and Escape |
| Toast | `popover="manual"` gives top layer | timers, pause on hover/focus, live-region announce, stacking |
| Date picker / calendar | `<input type=date>` (unstyleable) | grid navigation, segments, locale |
| Focus trap for non-modal layers (drawers not using `<dialog>`) | `inert` on siblings | wiring inert/focus restore |
| Modal dialog light dismiss in Safari | `closedby` (no Safari) | ~5 lines of backdrop-click JS |

- Many JS pieces can share one ~2–4 KB roving-focus/typeahead module plus one dismiss/focus-restore module. This is the main architectural takeaway from how Radix and Zag factor their internals.

### Gaps
- CSS carousel primitives (`::scroll-button`, `::scroll-marker`, `scroll-target-group`) were not researched this session, and their cross-browser status is unknown here.
- No published byte-size benchmarks were found for minimal vanilla roving-focus implementations.

---

## 5. How server-driven / htmx ecosystems handle these widgets (brief)

### Takeaway
The htmx-world answer has converged on three approaches:
1. **CSS + native HTML first**: `<dialog>`, `<details>`, popover. Examples: DaisyUI, Oat, Basecoat.
2. **Tiny vanilla JS for the keyboard-model widgets**: Basecoat's small scripts for dropdowns and modals.
3. **Web Components for the hard widgets**: Oat's dropdowns/tooltips; Shoelace/Web Awesome. htmx 4 explicitly argues web components "work great" with htmx.

No htmx-ecosystem library was found that claims full APG coverage for tree, grid, combobox, or date picker.

### Cited Findings
- **Basecoat:**
  - It is "a Tailwind CSS, vanilla HTML/CSS/JavaScript implementation of the shadcn/ui design system … without React, Radix, or framework runtime dependencies". It is built "specifically for use with server-rendered templates, htmx, vanilla js, alpine". — [Basecoat GitHub](https://github.com/hunvreus/basecoat); [Basecoat intro](https://basecoatui.com/introduction/); [hx-pod episode](https://hx-pod.transistor.fm/episodes/ronan-berder-basecoat-ui-shadcn-without-react)
  - Most Basecoat components need no JS. "Some components (like modals or dropdowns) use a tiny bit of vanilla JavaScript." It ships Nunjucks/Jinja macros, a model similar to Rust maud functions, and is MIT-licensed. — [Basecoat README](https://cdn.jsdelivr.net/gh/hunvreus/basecoat@main/README.md)
- **Oat:**
  - Oat (knadh/oat) is an "ultra-lightweight HTML + CSS, semantic UI component library" (~6–10 KB) that styles native elements (`<button>`, `<input>`, `<dialog>`, `role="alert"`) directly. Dropdowns, tooltips, and sidebars are implemented as native Web Components. It is pre-v1. — [Oat components](https://oat.ink/components); [unpkg README](https://unpkg.com/@knadh/oat@0.6.2/README.md)
- **DaisyUI:**
  - Its classic CSS-only dropdown relies on a `tabindex="0"` focus trick. Third-party commentary notes production apps may need JS for accessibility. Newer variants use `<details>` and the popover API. — [DaisyUI roadmap](https://daisyui.com/docs/roadmap/); [studyraid dropdown article](https://app.studyraid.com/en/read/12415/400835/creating-dropdown-menus) (secondary)
- **htmx and Web Components:**
  - htmx's essay "web components work great" (on the htmx 4 site) argues they pair well. htmx 2.0 integrates with Web Components but doesn't see into shadow DOM automatically, so `htmx.process(shadowRoot)` is needed. — [htmx 4 essay](https://four.htmx.org/essays/webcomponents-work-great); [OpenReplay htmx 2.0](https://blog.openreplay.com/htmx-2-0-is-here/); [binaryigor](https://binaryigor.com/htmx-and-web-components-a-perfect-match.html)

### Inferences
- For a Rust maud + htmx framework, the closest prior art is **Basecoat**: server-template macros emitting shadcn-style markup with a tiny opt-in vanilla JS file per interactive widget. Oat's web-component approach is the alternative for the keyboard-heavy widgets.
- A credible gap analysis should check each widget function against the APG contract in §1. "Opens and closes" is not enough. The usual failure points in CSS-only kits are the focus model (roving, typeahead, Escape-returns-focus) and ARIA state sync (`aria-expanded`, `aria-selected`, `aria-activedescendant`). DaisyUI's `tabindex` dropdown is an example.

### Gaps
- Web Awesome 3 (the Shoelace successor) and Alpine.js (`x-trap` focus plugin, community "Alpine UI components"): searches returned nothing usable, so their current component lists and accessibility claims are unverified.
- htmx + hyperscript and htmx-ext accessibility tooling: not researched within the tool budget.
- No sourced accessibility audits of Basecoat, Oat, or DaisyUI against APG were found.
