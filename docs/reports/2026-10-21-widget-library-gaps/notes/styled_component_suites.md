# Styled React/JS component suites: normalized component catalog (as of 2026-10-21)

Scope: shadcn/ui, MUI (Material UI core + MUI X), Ant Design, Mantine, Chakra UI, Bootstrap (5.3 stable, plus the 6.0 alpha). Research date: 2026-10-21. Where possible, lists come from the live docs navigation or from each project's machine-readable index (`llms.txt`, shadcn `r/index.json`). They are not from blog summaries.

Legend used in tables:
- **Y / name**: a first-class documented component (it has its own docs page or nav entry). The library's own name is given.
- **(v: X)**: a documented variant, prop mode or sub-component of another component, not its own nav entry. Note: the (v) mappings come from knowledge of each library's component APIs. They were **not** each re-checked against docs this session (see Gaps).
- **—**: not shipped.

---

## Q1. Current component list for each library, with versions

### Takeaway
Six versions were confirmed live on 2026-10-21:
- **Material UI 9.5.0** with **MUI X 9.16.0**.
- **Ant Design 6.6.7**.
- **Mantine 9.7.0**.
- **Chakra UI 3.37.0**.
- **Bootstrap 5.3.8** (stable), plus **Bootstrap 6.0.0-alpha.1**, released 2026-10-08.

shadcn/ui has no version number. Its registry holds about 65 to 66 UI items. Since mid-2026 it has added a cluster of AI/agent-chat primitives: Message, Bubble, Message Scroller, Attachment, Marker, Questionnaire, Trace and Code Block, plus 21 "AI Elements" patterns.

### Cited Findings

**shadcn/ui** (no version; current registry, 2026-10):
- The docs component index lists 66 components: Accordion, Alert, Alert Dialog, Aspect Ratio, Attachment, Avatar, Badge, Breadcrumb, Bubble, Button, Button Group, Calendar, Card, Carousel, Chart, Checkbox, Code Block, Collapsible, Combobox, Command, Context Menu, Data Table, Date Picker, Dialog, Direction, Drawer, Dropdown Menu, Empty, Field, Hover Card, Input, Input Group, Input OTP, Item, Kbd, Label, Marker, Menubar, Message, Message Scroller, Native Select, Navigation Menu, Pagination, Popover, Progress, Questionnaire, Radio Group, Resizable, Scroll Area, Select, Separator, Sheet, Sidebar, Skeleton, Slider, Spinner, Switch, Table, Tabs, Textarea, Toast, Toggle, Toggle Group, Tooltip, Trace, Typography. Code Block and Trace are flagged "New". — [shadcn/ui Components](https://ui.shadcn.com/docs/components)
- The machine-readable registry (`/r/index.json`) has 65 `registry:ui` items. It also contains **`sonner`** (depends on `sonner` and `next-themes`) and **`form`**, which are absent from the docs nav. It contains a separate **`toast`** item built on `@base-ui/react`. Data Table, Date Picker and Typography are docs-only compositions, not registry items. — [shadcn registry index](https://ui.shadcn.com/r/index.json)
- Every component is documented in three "bases": `/docs/components/base/*` (Base UI), `/docs/components/radix/*` and `/docs/components/aria/*` (React Aria). The October 2026 release says the new components "are available for Base UI, React Aria and Radix across all eight styles." — [shadcn changelog](https://ui.shadcn.com/docs/changelog)
- Changelog entries, newest first:
  - **Oct 2026**: Trace, Code Block, AI Elements.
  - **Sep 2026**: "cn".
  - **Aug 2026**: Private GitHub Registries, Human in the Loop, Questionnaire.
  - **Jun 2026**: "components for chat interfaces" (Message Scroller, Message, Bubble, Marker, Attachment).
  - Earlier: multiple entries for Jan to Jul 2026.
  
  Source: [shadcn changelog](https://ui.shadcn.com/docs/changelog)
- AI Elements is a set of 21 copy-in patterns: Chain of Thought, Chat, Checkpoint, Confirmation, Context, File Tree, Inline Citation, Message, Model Selector, Plan, Prompt Input, Queue, Reasoning, Sources, Stack Trace, Subagent, Suggestion, Task, Terminal, Test Results, Tool. It "replaces AI SDK Elements". — [shadcn changelog](https://ui.shadcn.com/docs/changelog)
- The docs nav documents form integrations for React Hook Form, TanStack Form and Formisch, plus the utilities `scroll-fade` and `shimmer`. — [shadcn changelog page nav](https://ui.shadcn.com/docs/changelog)

**MUI: Material UI v9.5.0** ("Material UI and MUI X v9 are out"):
- **Inputs:** Autocomplete, Button, Button Group, Checkbox, Floating Action Button, Radio Group, Rating, Select, Slider, Switch, Text Field, Transfer List, Toggle Button. The sidebar also has **Number Field (New)**.
- **Data display:** Avatar, Badge, Chip, Divider, Icons, Material Icons, List, Table, Tooltip, Typography.
- **Feedback:** Alert, Backdrop, Dialog, Progress, Skeleton, Snackbar.
- **Surfaces:** Accordion, App Bar, Card, Paper.
- **Navigation:** Bottom Navigation, Breadcrumbs, Drawer, Link, Menu, Pagination, Speed Dial, Stepper, Tabs. The sidebar also has **Menubar (New)**.
- **Layout:** Box, Container, Grid, Stack, Image List.
- **Lab:** Masonry, Timeline.
- **Utils:** Click-Away Listener, CSS Baseline, Modal, No SSR, Popover, Popper, Portal, Textarea Autosize, Transitions, useMediaQuery, InitColorSchemeScript.

Source: [MUI all components](https://mui.com/material-ui/all-components/)
- **MUI X v9.16.0** includes Data Grid, Date and Time Pickers, Charts, Tree View, **Scheduler (Preview)** and **Chat (Preview)**. It is "open core": Community packages are MIT, and Pro/Premium are commercial. — [MUI X overview](https://mui.com/x/introduction/)
- The commercial packages are `@mui/x-data-grid-pro`, `@mui/x-date-pickers-pro`, `@mui/x-charts-pro`, `@mui/x-tree-view-pro`, `@mui/x-data-grid-premium`, `@mui/x-charts-premium` and `@mui/x-scheduler-premium`. — [MUI X licensing](https://mui.com/x/introduction/licensing/)

**Ant Design v6.6.7:**
- **General:** Button, FloatButton, Icon, Typography.
- **Layout:** Divider, Flex, Grid, Layout, Masonry (since 6.0.0), Space, Splitter.
- **Navigation:** Anchor, Breadcrumb, Dropdown, Menu, Pagination, Steps, Tabs.
- **Data Entry:** AutoComplete, Cascader, Checkbox, ColorPicker, DatePicker, Form, Input, InputNumber, Mentions, Radio, Rate, Select, Slider, Switch, TimePicker, Transfer, TreeSelect, Upload.
- **Data Display:** Avatar, Badge, Calendar, Card, Carousel, Collapse, Descriptions, Empty, Image, List (**Deprecated**), **Listy** (since 6.6.0), Popover, QRCode, Segmented, Statistic, Table, Tag, Timeline, Tooltip, Tour, Tree.
- **Feedback:** Alert, Drawer, Message, Modal, Notification, Popconfirm, Progress, Result, Skeleton, Spin, Watermark.
- **Other:** Affix, App, **BorderBeam** (since 6.4.0), ConfigProvider, Util.
- A separate "Pro" family is also listed: ProLayout, ProForm, ProTable, ProDescriptions, ProList, EditableProTable.

Source: [Ant Design overview](https://ant.design/components/overview)

**Mantine v9.7.0** (the changelog index runs v7.0 to v9.7.0):
- **@mantine/core** (about 120 entries): Accordion, ActionBar, ActionIcon, Affix, Alert, AlphaSlider, Anchor, AngleSlider, AppShell, AspectRatio, Autocomplete, Avatar, BackgroundImage, Badge, Blockquote, Box, Breadcrumbs, Burger, Button, Card, Cascader, Center, Checkbox, Chip, CloseButton, Code, Collapse, ColorInput, ColorPicker, ColorSwatch, Combobox, ComboboxPopover, Container, CopyButton, DataList, Dialog, Divider, Drawer, EmptyState, Fieldset, FileButton, FileInput, Flex, FloatingIndicator, FloatingWindow, FocusTrap, Grid, Group, Highlight, HoverCard, HueSlider, Image, Indicator, Input, JsonInput, Kbd, List, Loader, LoadingOverlay, Mark, Marquee, MaskInput, Menu, Menubar, Modal, MultiSelect, NativeSelect, NavLink, Notification, NumberFormatter, NumberInput, OverflowList, Overlay, Pagination, Paper, PasswordInput, Pill, PillsInput, PinInput, Popover, Portal, Progress, Radio, RangeSlider, Rating, RingProgress, RollingNumber, ScrollArea, Scroller, SegmentedControl, Select, SemiCircleProgress, SimpleGrid, Skeleton, Slider, Space, Splitter, Spoiler, Stack, Stepper, Switch, Table, TableOfContents, Tabs, TagsInput, Text, Textarea, TextInput, ThemeIcon, Timeline, Title, Toggle, Toolbar, Tooltip, Tour, Transition, Tree, TreeSelect, Typography, UnstyledButton, VisuallyHidden.
- **@mantine/dates:** Calendar, DateInput, DatePicker, DatePickerInput, DateTimePicker, InlineDateTimePicker, MiniCalendar, MonthPicker, MonthPickerInput, TimeGrid, TimeInput, TimePicker, TimeValue, YearPicker, YearPickerInput.
- **@mantine/charts:** AreaChart, BarChart, BarsList, BubbleChart, BulletChart, CandlestickChart, CompositeChart, DonutChart, FunnelChart, GaugeChart, Heatmap, LineChart, MatrixChart, PieChart, RadarChart, RadialBarChart, SankeyChart, ScatterChart, Sparkline, SunburstChart, Treemap, WaffleChart.
- **@mantine/schedule:** Schedule, DayView, WeekView, MonthView, YearView, AgendaView, MobileMonthView, ResourcesDayView, ResourcesWeekView, ResourcesMonthView, ResourcesSchedule, plus recurring events.
- **@mantine/code-highlight:** CodeHighlight, JsonViewer.
- **"X" extensions:** Carousel, Dropzone, Lightbox, Modals manager, NavigationProgress, Notifications system, Rich text editor (Tiptap), Spotlight.
- **@mantine/form:** use-form, use-field, schema validation.

Source: [Mantine llms.txt](https://mantine.dev/llms.txt)
- The v9.0.0 release added:
  - The `@mantine/schedule` package.
  - The components FloatingWindow, OverflowList, Marquee, Scroller and BarsList.
  - Combobox virtualization, use-form async validation and Standard Schema support.
  - An `@mantine/mcp-server` package.
  
  Source: [Mantine v9.0.0 changelog](https://mantine.dev/llms/changelog-9-0-0.md)

**Chakra UI v3.37.0** (docs sidebar):
- **Layout:** Aspect Ratio, Bleed, Box, Center (Absolute), Center, Container, Flex, Float, Grid, Group, Scroll Area, Separator, SimpleGrid, Splitter, Stack, Wrap.
- **Typography:** Blockquote, Code, Code Block, Em, Heading, Highlight, Kbd, Link, Link Overlay, List, Mark, Prose, Rich Text Editor, Text.
- **Buttons:** Button, Close Button, Icon Button, Download Trigger.
- **Date and Time:** Date Input (new), Date Picker, Calendar.
- **Forms:** Checkbox, Checkbox Card, Color Picker, Color Swatch, Editable, Field, Fieldset, File Upload, Input, Number Input, Password Input, Pin Input, Radio Card, Radio, Rating, Segmented Control, Select (Native), Switch, Slider, Textarea, Tags Input.
- **Collections:** Combobox, Listbox, Select, Tree View.
- **Overlays:** Action Bar, Dialog, Drawer, Floating Panel, Hover Card, Menu, Overlay Manager, Popover, Toggle Tip, Tooltip.
- **Disclosure:** Accordion, Breadcrumb, Carousel, Collapsible, Pagination, Steps, Tabs.
- **Feedback:** Alert, Empty State, Progress Circle, Progress, Skeleton, Spinner, Status, Toast.
- **Data Display:** Avatar, Badge, Card, Clipboard, Image, Data List, Icon, Marquee, QR Code, Stat, Table, Tag, Timeline.
- **Internationalization:** LocaleProvider, FormatNumber, FormatByte.
- **Utilities:** Checkmark, ClientOnly, EnvironmentProvider, For, Presence, Portal, Radiomark, Show, Skip Nav, Visually Hidden, Theme.

Source: [Chakra UI docs](https://chakra-ui.com/docs/components/concepts/overview)

**Bootstrap 5.3.8** (stable docs nav):
- **Components:** Accordion, Alerts, Badge, Breadcrumb, Buttons, Button group, Card, Carousel, Close button, Collapse, Dropdowns, List group, Modal, Navbar, Navs & tabs, Offcanvas, Pagination, Placeholders, Popovers, Progress, Scrollspy, Spinners, Toasts, Tooltips.
- **Forms:** Form control, Select, Checks & radios (incl. switches), Range, Input group, Floating labels, Layout, Validation.
- **Content:** Reboot, Typography, Images, Tables, Figures.
- **Helpers:** ratio, stacks, visually-hidden, focus-ring, stretched-link, text-truncation, vertical-rule and others.

Source: [Bootstrap 5.3 docs](https://getbootstrap.com/docs/5.3/getting-started/introduction/)

**Bootstrap 6.0.0-alpha.1** (released 2026-10-08):
- The release brings ESM-only JS, Sass modules with CSS "token maps", and a browser floor of Chrome/Edge 130, Firefox 132 and Safari 18.
- It uses `light-dark()`, `:has()`, container queries, `oklch()`, `color-mix()` and native `<dialog>`.
- Bootstrap 5 stays supported. v5.4.0 is still to come before an EOL date is announced.

Source: [Bootstrap 6 Alpha blog post](https://blog.getbootstrap.com/2026/10/08/bootstrap-6-alpha/)
- The v6.0 docs nav adds or renames:
  - **Components:** avatar, dialog (replaces modal), drawer (replaces offcanvas), menu (replaces dropdowns), nav-overflow, stepper, toggler, placeholder, spinner, tab.
  - **Forms:** chips, combobox, datepicker, field, form-adorn, otp-input, password-strength, switch.
  - **Content:** prose.
  - **Customize:** rtl, theme, color-modes.
  - **Utilities:** container-queries, divide, color-scheme.
  
  Source: [Bootstrap 6.0 docs: Alert page nav](https://getbootstrap.com/docs/6.0/components/alert)

### Inferences
- The libraries mean different things by "component". Bootstrap ships CSS classes plus a few vanilla JS plugins. shadcn ships copy-in source. The other four ship npm React components. Bootstrap's count (~24 components + ~8 form pages on 5.3) is therefore structurally smaller, which deflates any "≥5 of 6" score for widgets that need JS.
- Bootstrap 6 alpha closes many of Bootstrap's long-standing gaps: avatar, combobox, datepicker, OTP, stepper, chips and password strength. If the diff targets "where the ecosystem is heading", it is reasonable to score Bootstrap on 6.0-alpha as a secondary column.
- The newest shadcn additions are mostly AI/agent-chat primitives (Jun to Oct 2026). Those are a new category that no other suite except MUI X Chat (Preview) covers.

### Gaps
- The shadcn docs nav lists "Toast" and does not list "Sonner" or "Form", yet both `sonner` and `form` remain in the registry index. I could not confirm whether Sonner is formally deprecated in favour of the new Base UI-backed Toast; the toast page's text did not extract.
- I did not fetch Bootstrap 6's "New and improved components" blog entry (part of the series). The component additions above come only from the v6.0 docs nav slugs.

---

## Q2. Tiering: universal (≥5 of 6), common (3-4) and niche (1-2)

### Takeaway
About 30 widgets are universal across the six suites. They are the baseline every styled kit ships:
- **Inputs:** button, text input, textarea, checkbox, radio, switch, slider, select.
- **Overlays:** dialog, drawer, popover, tooltip, dropdown menu.
- **Feedback and display:** alert, toast, progress, spinner, skeleton, badge, avatar, card, table, list, typography, separator.
- **Navigation:** tabs, accordion, breadcrumb, pagination.
- **Rich widgets:** carousel, combobox, date picker and calendar.
- **Layout:** grid and stack.

The "common" tier is where suites differ:
- number, OTP and tags inputs
- rating and color picker
- file upload
- stepper and timeline
- tree view
- empty state
- data/description list
- hover card
- splitter
- data grid
- charts
- time picker

The niche tier is mostly AntD and Mantine specialities (cascader, tree-select, transfer, mentions, watermark, tour, QR), plus shadcn's AI-chat set.

### Cited Findings
The matrix below is built from the docs navigation lists cited in Q1:
- [shadcn](https://ui.shadcn.com/docs/components)
- [MUI](https://mui.com/material-ui/all-components/) and [MUI X](https://mui.com/x/introduction/)
- [AntD](https://ant.design/components/overview)
- [Mantine](https://mantine.dev/llms.txt)
- [Chakra](https://chakra-ui.com/docs/components/concepts/overview)
- [Bootstrap 5.3](https://getbootstrap.com/docs/5.3/getting-started/introduction/) and [Bootstrap 6.0α](https://getbootstrap.com/docs/6.0/components/alert)

The "Score" column counts Y plus (v) across the 6 libraries, with Bootstrap scored on 5.3 stable. The BS6α column is informational only.

#### Tier 1: Universal (≥5 of 6)

| Canonical widget | shadcn | MUI (+X) | AntD 6 | Mantine 9 | Chakra 3 | BS 5.3 | BS6α | Score |
|---|---|---|---|---|---|---|---|---|
| Button | Button | Button | Button | Button | Button | Buttons | button | 6 |
| Text input | Input | Text Field | Input | TextInput | Input | Form control | field | 6 |
| Textarea | Textarea | (v: TextField multiline) / Textarea Autosize | (v: Input.TextArea) | Textarea | Textarea | (v: form-control) | Y | 6 |
| Checkbox | Checkbox | Checkbox | Checkbox | Checkbox | Checkbox (+Checkbox Card) | Checks & radios | checkbox | 6 |
| Radio group | Radio Group | Radio Group | Radio | Radio | Radio (+Radio Card) | Checks & radios | radio | 6 |
| Switch | Switch | Switch | Switch | Switch | Switch | (v: form-switch) | switch | 6 |
| Slider / range | Slider | Slider | Slider | Slider, RangeSlider | Slider | Range | Y | 6 |
| Select (styled listbox) | Select | Select | Select | Select | Select (Collections) | — (native only) | (v: combobox) | 5 |
| Native select | Native Select | (v: NativeSelect) | — | NativeSelect | Select (Native) | Select | Y | 5 |
| Combobox / autocomplete | Combobox | Autocomplete | AutoComplete | Autocomplete, Combobox | Combobox | — | combobox | 5 |
| Input group / adornments | Input Group | (v: InputAdornment) | (v: addonBefore/Space.Compact) | (v: left/rightSection) | (v: InputGroup / Group) | Input group | form-adorn | 6 |
| Field wrapper (label + help + error) | Field (+Label, Form) | (v: FormControl/FormHelperText) | Form (Form.Item) | (v: Input.Wrapper) + Fieldset | Field, Fieldset | Forms: validation, floating labels | field | 6 |
| Dialog / modal | Dialog | Dialog (+Modal util) | Modal | Modal (+Dialog) | Dialog | Modal | dialog | 6 |
| Drawer / sheet / offcanvas | Drawer, Sheet | Drawer | Drawer | Drawer | Drawer | Offcanvas | drawer | 6 |
| Popover | Popover | Popover (util) | Popover | Popover | Popover | Popovers | popover | 6 |
| Tooltip | Tooltip | Tooltip | Tooltip | Tooltip | Tooltip | Tooltips | tooltip | 6 |
| Dropdown / action menu | Dropdown Menu | Menu | Dropdown | Menu | Menu | Dropdowns | menu | 6 |
| Alert / callout | Alert | Alert | Alert | Alert | Alert | Alerts | alert | 6 |
| Toast / snackbar / notification | Toast (+Sonner in registry) | Snackbar | Message, Notification | Notification + Notifications system | Toast | Toasts | toasts | 6 |
| Progress (linear) | Progress | Progress | Progress | Progress | Progress | Progress | progress | 6 |
| Spinner / loader | Spinner | (v: CircularProgress) | Spin | Loader | Spinner | Spinners | spinner | 6 |
| Skeleton / placeholder | Skeleton | Skeleton | Skeleton | Skeleton | Skeleton | Placeholders | placeholder | 6 |
| Badge | Badge | Badge | Badge | Badge | Badge | Badge | Y | 6 |
| Avatar | Avatar | Avatar | Avatar | Avatar | Avatar | — | avatar | 5 |
| Card | Card | Card (+Paper) | Card | Card (+Paper) | Card | Card | card | 6 |
| Table (styled, static) | Table | Table | Table | Table | Table | Tables | tables | 6 |
| List / list group | Item | List | Listy (List deprecated) | List | List | List group | list-group | 6 |
| Typography | Typography | Typography | Typography | Text, Title, Typography | Heading, Text, Prose | Typography | typography, prose | 6 |
| Separator / divider | Separator | Divider | Divider | Divider | Separator | (v: hr, vertical-rule) | (v: divide utility) | 6 |
| Tabs | Tabs | Tabs | Tabs | Tabs | Tabs | Navs & tabs | tab | 6 |
| Accordion / collapsible | Accordion, Collapsible | Accordion | Collapse | Accordion, Collapse | Accordion, Collapsible | Accordion, Collapse | accordion, collapse | 6 |
| Breadcrumb | Breadcrumb | Breadcrumbs | Breadcrumb | Breadcrumbs | Breadcrumb | Breadcrumb | breadcrumb | 6 |
| Pagination | Pagination | Pagination | Pagination | Pagination | Pagination | Pagination | pagination | 6 |
| Carousel | Carousel | — | Carousel | Carousel (x) | Carousel | Carousel | carousel | 5 |
| Date picker | Date Picker (composition) | Date Picker (X) | DatePicker | DatePicker / DatePickerInput | Date Picker | — | datepicker | 5 |
| Calendar (inline) | Calendar | Date Calendar (X) | Calendar | Calendar | Calendar | — | (v: datepicker) | 5 |
| Grid layout | — (Tailwind) | Grid | Grid | Grid, SimpleGrid | Grid, SimpleGrid | Grid | grid | 5 |
| Stack / flex layout | — (Tailwind) | Stack | Flex, Space | Stack, Group, Flex | Stack, Flex, Wrap | (v: stacks helper, flex utils) | stacks | 5 |
| Button group | Button Group | Button Group | (v: Space.Compact) | (v: Button.Group) | (v: Group attached) | Button group | button-group | 6 |
| Image | — | (v: Image List) | Image (with preview) | Image, BackgroundImage | Image | Images | images | 5 |
| Navbar / app header | Navigation Menu | App Bar | (v: Layout.Header + Menu) | (v: AppShell.Header) | — | Navbar | navbar | 5 |

#### Tier 2: Common (3-4 of 6)

| Canonical widget | shadcn | MUI (+X) | AntD 6 | Mantine 9 | Chakra 3 | BS 5.3 | BS6α | Score |
|---|---|---|---|---|---|---|---|---|
| Number input / stepper field | — | Number Field (new) | InputNumber | NumberInput | Number Input | — | — | 4 |
| OTP / PIN input | Input OTP | — | (v: Input.OTP) | PinInput | Pin Input | — | otp-input | 4 |
| Password input | — | — | (v: Input.Password) | PasswordInput | Password Input | — | password-strength | 3 |
| Tags / chips input | — | (v: Autocomplete multiple) | (v: Select mode=tags) | TagsInput, PillsInput | Tags Input | — | chips | 4 |
| Multi-select | — | (v: Select multiple) | (v: Select mode=multiple) | MultiSelect | (v: Select multiple) | — | — | 4 |
| Rating | — | Rating | Rate | Rating | Rating | — | — | 4 |
| Color picker | — | — | ColorPicker | ColorPicker, ColorInput, Hue/AlphaSlider | Color Picker, Color Swatch | — | — | 3 |
| File upload / dropzone | — | — | Upload | FileInput, FileButton, Dropzone (x) | File Upload | (v: input type=file styled) | — | 4 |
| Toggle (pressed button) / toggle group / segmented | Toggle, Toggle Group | Toggle Button | Segmented | Toggle, SegmentedControl | Segmented Control | (v: btn-check) | — | 6* |
| Time picker | — | Time Picker (X) | TimePicker | TimePicker, TimeInput | — | — | — | 3 |
| Date-range picker | (v: Calendar range) | Date Range Picker (X **Pro**) | (v: RangePicker) | (v: DatePicker type=range) | (v: Date Picker range) | — | ? | 5* |
| Date-time picker | — | Date Time Picker (X) | (v: DatePicker showTime) | DateTimePicker | — | — | — | 3 |
| Stepper / steps | — | Stepper | Steps | Stepper | Steps | — | stepper | 4 |
| Timeline | — | Timeline (Lab) | Timeline | Timeline | Timeline | — | — | 4 |
| Tree view | — | Tree View (X) | Tree | Tree | Tree View | — | — | 4 |
| Empty state | Empty | — | Empty | EmptyState | Empty State | — | — | 4 |
| Description / data list (key-value) | — | — | Descriptions | DataList | Data List | — | — | 3 |
| Statistic / KPI stat | — | — | Statistic | (v: NumberFormatter / RollingNumber) | Stat | — | — | 3 |
| Hover card | Hover Card | — | — | HoverCard | Hover Card | — | — | 3 |
| Context menu | Context Menu | (v: Menu at cursor) | (v: Dropdown trigger=contextMenu) | — | (v: Menu context trigger) | — | — | 4 |
| Menubar | Menubar | Menubar (new) | — | Menubar | — | — | — | 3 |
| Stepper/circular progress (ring) | — | (v: CircularProgress determinate) | (v: Progress type=circle) | RingProgress, SemiCircleProgress | Progress Circle | — | — | 4 |
| Alert dialog / confirm | Alert Dialog | — (pattern) | Popconfirm, (v: Modal.confirm) | (v: modals.openConfirmModal) | (v: Dialog role=alertdialog) | — | — | 4 |
| Backdrop / loading overlay | — | Backdrop | (v: Spin wrapping) | Overlay, LoadingOverlay | — | — | — | 3 |
| Close button | — | — | — | CloseButton | Close Button | Close button | close-button | 3 |
| Kbd (key hint) | Kbd | — | — | Kbd | Kbd | (v: `<kbd>` reboot style) | — | 4 |
| Code / code block | Code Block (Oct 2026) | — | (v: Typography code) | Code, CodeHighlight | Code, Code Block | (v: `<code>`/`<pre>` reboot) | — | 5* |
| Blockquote | — | — | — | Blockquote | Blockquote | (v: typography blockquote) | — | 3 |
| Highlight / mark | — | — | (v: Typography mark) | Highlight, Mark | Highlight, Mark | (v: `<mark>`) | — | 4* |
| Copy-to-clipboard | — | — | (v: Typography copyable) | CopyButton | Clipboard | — | — | 3 |
| Chip / tag | — (Badge) | Chip | Tag | Chip, Pill | Tag | — | chips | 4 |
| Scroll area (styled scrollbars) | Scroll Area | — | — | ScrollArea, Scroller | Scroll Area | — | — | 3 |
| Resizable panels / splitter | Resizable | — | Splitter | Splitter | Splitter | — | — | 4 |
| Aspect ratio | Aspect Ratio | — | — | AspectRatio | Aspect Ratio | (v: ratio helper) | Y (utility) | 4 |
| Container | — | Container | — | Container | Container | Containers | containers | 4 |
| Box (primitive) | — | Box | — | Box | Box | — | — | 3 |
| App shell / sidebar layout | Sidebar | — | Layout (Sider/Header/Content) | AppShell (+NavLink) | — | — | — | 3 |
| Nav link / vertical nav | (v: Sidebar menu) | Link | Menu (inline mode) | NavLink | Link | Navs | nav, nav-overflow | 6* |
| Anchor / TOC / scrollspy | — | — | Anchor | TableOfContents | — | Scrollspy | scrollspy | 3 |
| Data grid (sort, filter, paginate) | Data Table (TanStack pattern) | Data Grid (X) | Table (full featured) | — (third-party) | — | — | — | 3 |
| Charts | Chart (recharts wrapper) | Charts (X) | — (separate AntV) | @mantine/charts (22 types) | — (see Gaps) | — | — | 3 |
| Visually hidden / skip link | — | — | — | VisuallyHidden | Visually Hidden, Skip Nav | Visually hidden | visually-hidden | 3 |
| Rich text editor | — | — | — | Rich text editor (Tiptap, x) | Rich Text Editor | — | — | 2 |

\* Score inflated by counting (v) variants. These are borderline universal, but their shape differs a lot across suites.

#### Tier 3: Niche (1-2 of 6)

| Widget | Libraries |
|---|---|
| Cascader (hierarchical select) | AntD Cascader, Mantine Cascader |
| Tree select | AntD TreeSelect, Mantine TreeSelect |
| Transfer list (dual list) | MUI Transfer List, AntD Transfer |
| Mentions (@-autocomplete in textarea) | AntD Mentions |
| Mask input | Mantine MaskInput |
| JSON input / JSON viewer | Mantine JsonInput, JsonViewer |
| Angle slider | Mantine AngleSlider |
| Editable (inline click-to-edit text) | Chakra Editable |
| Listbox (standalone) | Chakra Listbox |
| Download trigger | Chakra Download Trigger |
| Command palette | shadcn Command, Mantine Spotlight |
| Action bar (bulk-selection floating toolbar) | Chakra Action Bar, Mantine ActionBar |
| Floating panel / window | Chakra Floating Panel, Mantine FloatingWindow |
| Toggle tip | Chakra Toggle Tip |
| Overlay manager / modals manager | Chakra Overlay Manager, Mantine Modals manager |
| Tour / onboarding | AntD Tour, Mantine Tour |
| Popconfirm (inline confirm bubble) | AntD Popconfirm |
| Result (full-page success/error/403/404) | AntD Result |
| Watermark | AntD Watermark |
| QR code | AntD QRCode, Chakra QR Code |
| Marquee | Mantine Marquee, Chakra Marquee |
| Spoiler (show more) | Mantine Spoiler |
| Overflow list ("+N more") | Mantine OverflowList, Bootstrap 6α nav-overflow |
| Indicator / status dot | Mantine Indicator, Chakra Status (plus badge-dot variants elsewhere) |
| Number formatting / rolling number | Mantine NumberFormatter, RollingNumber; Chakra FormatNumber, FormatByte |
| Masonry | MUI Masonry (Lab), AntD Masonry |
| Image list / gallery | MUI Image List |
| Lightbox | Mantine Lightbox (x) (AntD Image has built-in preview) |
| Speed dial | MUI Speed Dial |
| Floating action button | MUI FAB, AntD FloatButton |
| Bottom navigation | MUI Bottom Navigation |
| Affix (sticky on scroll) | AntD Affix, Mantine Affix |
| Burger / navbar toggler | Mantine Burger, Bootstrap 6α toggler |
| Navigation progress (top loading bar) | Mantine NavigationProgress |
| Scheduler / event calendar | MUI X Scheduler (Preview, Premium package), Mantine @mantine/schedule |
| Border beam (animated border) | AntD BorderBeam |
| Link overlay (whole-card link) | Chakra Link Overlay, Bootstrap stretched-link helper |
| Bleed, Float, Center | Chakra (Center also in Mantine) |
| Theme icon | Mantine ThemeIcon |
| Floating indicator (animated tab underline) | Mantine FloatingIndicator |
| Toolbar | Mantine Toolbar |
| AI chat primitives: Message, Bubble, Message Scroller, Attachment, Marker, Questionnaire, Trace | shadcn only (MUI X Chat is Preview) |
| Direction provider (RTL) as a component | shadcn Direction, Chakra LocaleProvider (AntD via ConfigProvider) |

### Inferences
- Breadth ranking by count: Mantine (~120 core + 15 dates + 22 charts + schedule + extensions) is the widest. Then Chakra (~110 entries incl. utilities), then AntD (~70 + Pro family), then shadcn (~66) and MUI core (~55 + X). Bootstrap 5.3 is the narrowest (~32).
- For a server-rendered Rust framework, the universal tier is the must-have list. Many items on it are pure CSS plus light JS: card, badge, alert, table, list, breadcrumb, pagination, typography, separator, skeleton, spinner, progress, accordion (`<details>`), dialog (`<dialog>`), popover (Popover API), tooltip. Five items need real client behaviour with ARIA patterns: combobox/autocomplete, select listbox, date picker/calendar, dropdown menu and toast.
- Bootstrap 6 itself moved to native `<dialog>`. That is evidence that the platform-primitive approach is now mainstream (see the [Bootstrap 6 Alpha post](https://blog.getbootstrap.com/2026/10/08/bootstrap-6-alpha/)).
- The highest-value "common" gaps are the ones that appear in the 4 rich React suites (MUI, AntD, Mantine, Chakra) but not in shadcn or Bootstrap: number input, rating, stepper, timeline, tree view, tags input and time picker.

### Gaps
- The (v) cells (e.g., AntD Input.OTP/Input.Password/Space.Compact, Mantine Button.Group, Chakra InputGroup, MUI IconButton/NativeSelect/InputAdornment, Chakra Date Picker range mode) were not individually re-verified against docs pages this session.
- The tier boundaries depend on whether variants count. A strict "own nav entry only" count would demote textarea, switch (Bootstrap), button group, spinner (MUI), native select and multi-select by one or two points.

---

## Q3. Heavyweight data grids and tables: feature comparison

### Takeaway
MUI X Data Grid is the reference full-featured grid, but its advanced features are licence-tiered:
- **Pro:** multi-sort/filter, column resizing and column pinning.
- **Premium:** row grouping, aggregation, pivoting and Excel export.

AntD Table is the most featureful free table: sorting, filtering, selection, fixed columns and headers, tree data, editable rows, virtual list and summary rows. It has no built-in export, and column resizing is not among its documented demos. shadcn ships only a TanStack Table recipe (sort, filter, paginate, visibility, selection). Mantine and Chakra ship no data grid. Bootstrap ships table styles only.

### Cited Findings
- The **MUI X Data Grid** docs list two groups:
  - **Main features:** Layout, Columns, Rows, Cells, Editing, Sorting, Filtering, Pagination, Selection, Virtualization, Accessibility, Localization.
  - **Advanced features:** Tree data, Row grouping, Aggregation, Formulas (New), Pivoting (New), Export, Copy and paste, Undo and redo (New), Scrolling, List view, Server-side data (New), Charts integration (New), AI Assistant (New).
  - Toolbar components: Export, Quick Filter, Prompt Field, Formula Bar (New). Columns/Filter/Pivot/Charts/AI Assistant panels are marked "Planned".
  
  Source: [MUI X Data Grid quickstart nav](https://mui.com/x/react-data-grid/getting-started/)
- "The Data Grid Pro comes with multi-filtering, multi-sorting, column resizing, and column pinning." "MUI X Premium unlocks the most advanced features of the Data Grid, including row grouping and Excel exporting." — [MUI X licensing](https://mui.com/x/introduction/licensing/)
- **AntD Table** demo index:
  - **Selection:** checkbox/radio, selection operations, custom selections.
  - **Filter and sorter:** filter in tree mode, filter search, multiple sorter, reset filters/sorters, customized filter panel.
  - **Data and layout:** Ajax (remote data), size (density), border/title/footer, expandable rows, colSpan/rowSpan, tree data.
  - **Fixed and sticky:** fixed header, auto height, fixed columns, stacked fixed columns, hidden columns, grouped table head, "Fixed header and scroll bar with the page" (sticky).
  - **Editing:** editable cells, editable rows, nested tables.
  - **Reordering:** drag-sort rows, drag-sort columns, drag handle.
  - **Display:** ellipsis column with tooltip, custom empty, **summary** row, **virtual list**, responsive, pagination settings.
  
  Source: [AntD Table docs](https://ant.design/components/table.md)
- AntD's own "Pro" family includes ProTable and EditableProTable. — [Ant Design overview](https://ant.design/components/overview)
- The **shadcn Data Table** is a guide built on `@tanstack/react-table`. It covers filtering ("filter emails"), pagination controls "including page size and selection count", column visibility toggling and row selection. The guide states: "Every data table or datagrid I've created has been unique… so a guide rather than a component." — [shadcn Data Table](https://ui.shadcn.com/docs/components/base/data-table)
- Mantine core ships only `Table` (plus `Table` docs). It has no data-grid entry in any first-party package list. Mantine 9.0 added **Combobox virtualization**. — [Mantine llms.txt](https://mantine.dev/llms.txt); [Mantine v9.0.0 changelog](https://mantine.dev/llms/changelog-9-0-0.md)

**Feature matrix** (Y = documented, P = Pro, Pr = Premium, R = recipe/you build it, — = absent):

| Feature | MUI X DataGrid | AntD Table | shadcn Data Table | Mantine | Chakra | Bootstrap |
|---|---|---|---|---|---|---|
| Sorting | Y (multi = P) | Y (multi-sorter) | R (TanStack) | — | — | — |
| Filtering | Y (multi = P), quick filter | Y (menu/tree/search, custom panel) | R | — | — | — |
| Pagination | Y | Y | R | Pagination component only | Pagination only | Pagination CSS only |
| Row selection | Y | Y (checkbox/radio/custom) | R | — | — | — |
| Column resizing | P | — (not in demo list) | R (TanStack supports) | — | — | — |
| Column pinning / fixed | P | Y (fixed columns + header) | R | — | — | — |
| Column visibility | Y | Y (hidden columns) | R | — | — | — |
| Virtualization | Y | Y (virtual list) | — | (Combobox only) | — | — |
| Inline editing | Y | Y (cells/rows demos) | — | — | — | — |
| Tree data / nested rows | Y (Advanced) | Y (tree data, expandable, nested) | — | — | — | — |
| Row grouping / aggregation / pivot | Pr | — (summary row only) | — | — | — | — |
| Export | Y (CSV/print; Excel = Pr) | — | — | — | — | — |
| Copy/paste, undo/redo, formulas | Y (Advanced section) | — | — | — | — | — |
| Server-side data source | Y | Y (Ajax demo) | R | — | — | — |
| Row/column drag reorder | Y | Y | — | — | — | — |
| Density / size | Y | Y (size demo) | — | — | — | Y (`.table-sm`) |

### Inferences
- A Rust SSR framework's data-table offering is best benchmarked against **AntD Table**, which is free and full-featured, and the **shadcn/TanStack recipe**. MUI X Pro/Premium features represent a paid tier that most open-source kits do not try to match.
- Sorting, filtering and pagination map naturally to server round-trips (query params plus htmx-style partial swaps). Virtualization, column resizing and pinning are client-side concerns that need JS.

### Gaps
- I did not verify, against a source fetched this session, the exact MUI X tier assignment for tree data, row reordering and lazy loading (believed to be Pro). Nor did I verify the Community plan's page-size cap.
- I did not verify the current status of third-party Mantine grids (mantine-react-table, mantine-datatable) for Mantine 9.
- Chakra v3 has no data grid in its nav. I did not check whether it has a documented TanStack recipe.

---

## Q4. Date and time pickers and calendars

### Takeaway
MUI X and Mantine have the most complete pickers:
- **MUI X:** separate Field, Calendar, Clock and Picker variants. Range pickers are Pro. Adds timezone and non-Gregorian calendar support.
- **Mantine:** date/month/year pickers and inputs, time grid, mini calendar. Since v9.0 it also has a full **@mantine/schedule** event calendar.

AntD covers DatePicker, TimePicker and Calendar. Chakra 3 has Date Input (new), Date Picker and Calendar. shadcn composes Calendar plus Popover. Bootstrap 5 has nothing, while Bootstrap 6α adds a datepicker.

### Cited Findings
- **MUI X Pickers:**
  - **Single-value components:** Date Picker, Date Field, Date Calendar, Time Picker, Time Field, Time Clock, Digital Clock, Date Time Picker, Date Time Field.
  - **Range components:** Date Range Picker/Field/Calendar, Time Range Picker/Field, Date Time Range Picker/Field.
  - **Supporting docs:** Shortcuts, Validation, Accessibility, Translated components, Date format and localization, UTC and timezones, **Calendar systems** (non-Gregorian), custom layout/field/opening button.
  - The DateRangeCalendar and DateRangePicker API pages are marked "(pro)".
  
  Source: [MUI X llms.txt](https://mui.com/x/llms.txt). The Pro picker package is `@mui/x-date-pickers-pro` — [MUI X licensing](https://mui.com/x/introduction/licensing/)
- **MUI X Scheduler** is in "Preview", and `@mui/x-scheduler-premium` exists. — [MUI X overview](https://mui.com/x/introduction/); [licensing](https://mui.com/x/introduction/licensing/)
- **Mantine Dates:** Calendar, DateInput, DatePicker, DatePickerInput, DateTimePicker, InlineDateTimePicker, MiniCalendar, MonthPicker(+Input), TimeGrid, TimeInput, TimePicker, TimeValue, YearPicker(+Input). — [Mantine llms.txt](https://mantine.dev/llms.txt)
- **Mantine Schedule** provides Schedule, DayView, WeekView, MonthView, YearView, AgendaView, MobileMonthView, Resources{Day,Week,Month}View, ResourcesSchedule, recurring events and events data. It is new in v9.0.0. — [Mantine llms.txt](https://mantine.dev/llms.txt); [v9.0.0 changelog](https://mantine.dev/llms/changelog-9-0-0.md)
- **AntD:** DatePicker and TimePicker (Data Entry), and Calendar (Data Display). — [AntD overview](https://ant.design/components/overview)
- **Chakra 3.37:** a "Date and Time" category with Date Input (new), Date Picker and Calendar. — [Chakra docs](https://chakra-ui.com/docs/components/concepts/overview)
- **shadcn:** Calendar (a registry component) and Date Picker (a docs composition, not in the registry index). — [shadcn components](https://ui.shadcn.com/docs/components); [registry index](https://ui.shadcn.com/r/index.json)
- **Bootstrap 6.0α:** `forms/datepicker` in the docs nav. Bootstrap 5.3 has no date picker. — [BS6 docs](https://getbootstrap.com/docs/6.0/components/alert); [BS5.3 docs](https://getbootstrap.com/docs/5.3/getting-started/introduction/)

| Capability | shadcn | MUI X | AntD | Mantine | Chakra | BS5.3 / BS6α |
|---|---|---|---|---|---|---|
| Inline calendar | Calendar | DateCalendar | Calendar | Calendar, MiniCalendar | Calendar | — / ? |
| Popup date picker | Date Picker (recipe) | DatePicker | DatePicker | DatePickerInput, DatePicker | Date Picker | — / datepicker |
| Typed/segmented date field | — | DateField | — | DateInput | Date Input (new) | — |
| Time picker | — | TimePicker, TimeClock, DigitalClock | TimePicker | TimePicker, TimeInput, TimeGrid | — | — |
| Date-time picker | — | DateTimePicker | (v: showTime) | DateTimePicker, InlineDateTimePicker | — | — |
| Date range | (v: Calendar mode) | Pro | (v: RangePicker) | (v: type="range") | ? | — |
| Month / year pickers | — | (v: views) | (v: picker="month/year") | MonthPicker, YearPicker | ? | — |
| Timezone / calendar systems | — | Y (docs pages) | ? | ? | ? | — |
| Event scheduler | — | Scheduler (Preview) | — | @mantine/schedule | — | — |

### Inferences
- Across the five suites that have a picker, the stable common core is an inline calendar plus a popup date picker. Time pickers and range pickers are a step up, and a scheduler is a niche heavyweight.
- A server-rendered framework could ship calendar/date picker with a native `<input type="date">` fallback. Bootstrap 5 relied entirely on native inputs until 6α.

### Gaps
- I did not verify AntD's RangePicker / `picker` modes or Chakra's range support against fetched pages this session (marked "(v)" or "?").
- I did not check the shadcn Calendar's underlying engine for 2026, historically react-day-picker.

---

## Q5. Theming, tokens, dark mode, density, RTL and i18n

### Takeaway
All six suites are now CSS-variable based:
- **MUI:** CSS theme variables with "native color".
- **AntD:** Design Tokens (Seed → Map → Alias) with algorithm presets for default, **dark** and **compact (density)**.
- **Mantine:** CSS variables plus a CSS-variables resolver.
- **Chakra:** a tokens/recipes system.
- **shadcn:** Tailwind CSS variables and 8 styles over 3 headless bases.
- **Bootstrap:** CSS variables and color modes. In 6α, every Sass variable becomes a runtime CSS variable via "token maps", using `light-dark()`.

AntD is the only suite with a first-class density algorithm. MUI and AntD expose theme-level per-component overrides.

### Cited Findings
- **MUI:**
  - Docs sections: "CSS theme variables" (overview, usage, **native color**, configuration), "Dark mode" ("two palette modes: light (the default) and dark"), "Right-to-left support", "Themed components" (customize a component's styles and default props in the theme), and InitColorSchemeScript ("eliminates dark mode flicker").
  
  Source: [MUI llms.txt](https://mui.com/material-ui/llms.txt)
- **AntD:**
  - "We call the smallest element that affects the theme **Design Token**", passed via `ConfigProvider`'s `theme`.
  - Seed Tokens cascade into other tokens.
  - There are three preset algorithms: `theme.defaultAlgorithm`, `theme.darkAlgorithm` and `theme.compactAlgorithm`.
  - Since 5.0 the theme uses CSS-in-JS, replacing 4.x Less variables.
  
  Source: [AntD customize theme](https://ant.design/docs/react/customize-theme.md)
- **AntD** "Other" category contents: ConfigProvider and App. — [AntD overview](https://ant.design/components/overview)
- **Mantine:**
  - FAQ topics include a CSS variables resolver ("change component color prop value depending on the color scheme"), virtual colors for different primary per scheme, `useMantineColorScheme`/`useComputedColorScheme`, and the Styles API.
  - "Can I have color schemes other than light and dark?" is answered as a theme-vs-scheme distinction.
  
  Source: [Mantine llms.txt](https://mantine.dev/llms.txt)
- **Chakra:** Concepts include "Color Mode" and "Server Component". The Internationalization category has LocaleProvider, FormatNumber and FormatByte, and Utilities includes "Theme". — [Chakra docs](https://chakra-ui.com/docs/components/concepts/overview)
- **shadcn:** the docs nav has Theming, Dark Mode and "Direction" (an RTL component). Components exist for Base UI, React Aria and Radix bases "across all eight styles". — [shadcn changelog](https://ui.shadcn.com/docs/changelog); [components](https://ui.shadcn.com/docs/components)
- **Bootstrap 5.3:** customize pages for color, **color-modes**, **css-variables**, sass, options and components, plus getting-started/**rtl** and getting-started/accessibility. — [BS5.3 docs](https://getbootstrap.com/docs/5.3/getting-started/introduction/)
- **Bootstrap 6α:**
  - "customization now happens via token maps". "Nearly every Sass variable from v5 is now a CSS variable in a token map… customize Bootstrap's CSS variables at runtime".
  - It uses `light-dark()`, `oklch()` and `color-mix()`.
  - Docs pages include customize/theme, customize/rtl, customize/color-modes and utilities/color-scheme.
  
  Source: [Bootstrap 6 Alpha](https://blog.getbootstrap.com/2026/10/08/bootstrap-6-alpha/); [BS6 docs nav](https://getbootstrap.com/docs/6.0/components/alert)

**Cross-cutting capability matrix:**

| Capability | shadcn | MUI | AntD | Mantine | Chakra | Bootstrap |
|---|---|---|---|---|---|---|
| Design tokens / CSS vars | CSS vars (Tailwind), 8 styles | CSS theme variables, native color | Design Token (Seed/Map/Alias), ConfigProvider | CSS vars + resolver, Styles API | Tokens + Theme | CSS vars; 6α token maps |
| Dark mode | Dark Mode doc | light/dark palette modes, InitColorSchemeScript | darkAlgorithm | color schemes + hooks | Color Mode | color modes (5.3); `light-dark()` (6α) |
| Density | — | ? | compactAlgorithm; Table size | (v: size props) | (v: size props) | `.table-sm`, `-sm`/`-lg` classes |
| RTL | Direction component | RTL guide | ? (see Gaps) | ? | LocaleProvider | RTL guide (5.3), customize/rtl (6α) |
| i18n of built-in strings | — | (Data Grid, Pickers localization pages) | ? (see Gaps) | (Dates locale) | LocaleProvider, FormatNumber, FormatByte | n/a (no built-in strings) |
| Per-component overrides | source is yours | theme `components` styleOverrides/defaultProps | component tokens | Styles API / classNames | recipes | component CSS vars / token maps |
| Form integration | Field + React Hook Form / TanStack Form / Formisch | (FormControl) | Form | @mantine/form (use-form, Standard Schema, async validation) | Field, Fieldset | validation styles |
| Toast system | Toast (+Sonner) | Snackbar | message / notification APIs | Notifications system | Toast | Toasts (JS plugin) |
| Command palette | Command | — | — | Spotlight | — | — |
| Charts | Chart | MUI X Charts | — (AntV separate) | @mantine/charts | ? | — |
| Virtualization | — | Data Grid | Table virtual | Combobox (v9) | ? | — |

### Inferences
- The industry baseline for theming is CSS custom properties with light/dark switching. Server-rendered tokens map cleanly onto this, and Bootstrap 6's `light-dark()` approach is SSR-friendly because it needs no JS for scheme switching.
- Density as a first-class theme axis is rare (AntD compact). Most suites use per-component `size` props instead.

### Gaps
- I did not fetch AntD's RTL (`ConfigProvider direction`) or locale docs, Mantine's RTL docs, MUI's core localization page or Chakra's theming/recipes page this session. Those cells are marked "?".
- I could not confirm whether Chakra 3 ships an official charts package; it is absent from the component nav fetched.

---

## Q6. Accessibility claims

### Takeaway
Mantine makes the most explicit, testable claim: WAI-ARIA conformance, axe and jest-axe tests, keyboard and focus unit tests, and manual VoiceOver testing. MUI X documents accessibility as a named feature page for both Data Grid and Pickers. shadcn inherits accessibility from its headless bases (Radix, Base UI, React Aria). Bootstrap has a dedicated accessibility page. AntD's claims were not captured.

### Cited Findings
- "Mantine components follow WAI-ARIA accessibility guidelines. All components have proper roles, aria-\* attributes… provide full keyboard support, manage focus correctly, and support screen readers."
- "All components that have interactive elements are tested with axe (jest-axe)… tested for keyboard support and focus management with unit tests… more than 10,000 unit tests… manually tested with screen readers (VoiceOver)."

Source: [Mantine a11y FAQ](https://mantine.dev/llms/q-are-mantine-components-accessible.md)
- MUI X Data Grid lists "Accessibility" and "Localization" among its main features. — [Data Grid nav](https://mui.com/x/react-data-grid/getting-started/)
- MUI X Pickers have an "Accessibility" page: "Learn how the Date and Time Pickers implement accessibility". — [MUI X llms.txt](https://mui.com/x/llms.txt)
- shadcn's Trace component offers "Keyboard and screen reader support from the trigger that wraps each line". Components ship on Base UI, React Aria or Radix bases. — [shadcn changelog](https://ui.shadcn.com/docs/changelog)
- Chakra's tagline is "an accessible component system". — [Chakra llms.txt](https://chakra-ui.com/llms.txt)
- Bootstrap 5.3 and 6.0 both have a `getting-started/accessibility` page. 6.0 also adds `helpers/focus-ring` and `visually-hidden`. — [BS5.3 docs](https://getbootstrap.com/docs/5.3/getting-started/introduction/); [BS6 docs](https://getbootstrap.com/docs/6.0/components/alert)

### Inferences
- The suites built on headless state-machine or ARIA libraries (shadcn on Radix, Base UI or React Aria; Chakra v3, believed to be built on Ark UI/Zag; see Gaps) push APG keyboard patterns into the base layer. A Rust SSR framework would need to supply equivalent behaviour for its composite widgets: combobox, listbox, menu, tabs, dialog focus trap and tree. Mantine's explicit FocusTrap and VisuallyHidden components, and MUI's Click-Away Listener and FocusTrap utilities, show what those suites treat as primitives.

### Gaps
- I did not fetch AntD's accessibility statement or Chakra's Ark UI/Zag foundation (from training knowledge, not verified this session). Nor did I fetch the per-component WAI-ARIA APG pattern mappings for any library.
- None of the suites' docs fetched here claim a formal WCAG conformance level (e.g., 2.2 AA). I found no source for such a claim.
