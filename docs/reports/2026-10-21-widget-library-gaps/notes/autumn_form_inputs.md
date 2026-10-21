# Autumn form/input control layer and interactive-input widgets (inventory as of HEAD 6104f0c4, 2026-10-21)

All sources are local files in `/home/user/autumn`, cited as `path:line`. "Shipped" means public API at HEAD. "Generated" means markup emitted by `autumn-cli` generators into user projects, not a framework API. "Absent" means a grep of `autumn/src`, `autumn-cli/src`, `autumn-macros*/src`, `autumn-admin-plugin/src`, `autumn/vendor/*.js` found no implementation.

Rendering stack: Maud server-rendered HTML, htmx for enhancement, one small vanilla-JS runtime (`autumn/vendor/autumn-widgets.js`, 267 lines). The JS covers autocomplete, the min-length guard, nav_bar, and the modal fallback ([Source](autumn/vendor/autumn-widgets.js:1-267)).

## Q1. What form-input controls does Autumn ship? (per-control inventory)

### Takeaway
Autumn has three overlapping layers.
- **Changeset-bound helpers** in `autumn_web::form`: about 9 control kinds, each tied to a `Changeset<T>` for value pre-fill and inline errors.
- **Typed accessible primitives** in `autumn_web::a11y`: six form primitives that will not compile without a label.
- **A few htmx widgets** in `autumn_web::widgets` and elsewhere: active search, autocomplete/combobox, bulk-select checkbox, reaction toggles, comment form, direct upload, CAPTCHA.

Every control is a native HTML element. Autumn has no custom-drawn pickers, sliders, switches or tag inputs.

### Cited Findings

**Layer 1: changeset-bound helpers (`autumn/src/form.rs`, all `#[cfg(feature = "maud")]`)**

Common contract of the "full" helpers:
- Wrapper: `<div id="{field}-field" class="autumn-field">`.
- Label: `<label for={field} class="autumn-field__label">`.
- Value pre-filled from `Changeset::field_value` (the data is serialized to JSON and the key looked up).
- `aria-invalid="true|false"`.
- `aria-describedby="{field}-error"` when errors exist. When there are no errors it is emitted as an empty attribute.
- Inline errors in `<div id="{field}-error" role="alert" class="autumn-field__errors">` with one `<p class="autumn-field__error">` per message.
- `--invalid` CSS modifier on the input.
- Sources: [text_input doc + body](autumn/src/form.rs:1438-1532); [CSS hooks](autumn/src/ui/widgets.css:106-164).

| Helper | file:line | HTML rendered | Required variant | Notes |
|---|---|---|---|---|
| `text_input` | autumn/src/form.rs:1449 | `input type="text"` | `required_text_input` (form.rs:2361), which adds `required` + `aria-required="true"` | `required_text_input` drops the `autumn-field*` classes on the wrapper, label and errors: its markup is a bare `div id=…` with `label for=…` ([Source](autumn/src/form.rs:2361-2395)). This styling is inconsistent with the other helpers. |
| `text_input_htmx` / `text_input_htmx_with_token_field` | form.rs:1536 / 1588 | `input type="text"` + `hx-post=validate_url hx-trigger="change" hx-target="closest [data-autumn-field-wrapper]" hx-swap="outerHTML" hx-include="closest form" hx-params="not _submit_token"` | `required_text_input_htmx` (1650), `required_text_input_htmx_with_token_field` (1676) | Server-round-trip inline validation on `change` (not per keystroke). The `_with_token_field` variant handles a custom submit-token name (issue #1843) ([Source](autumn/src/form.rs:1556-1640)). |
| `password_input` | form.rs:1741 | `input type="password"` | **none** | Never echoes the value ([Source](autumn/src/form.rs:1730-1779)). It emits no `autocomplete` attribute, and it has no reveal toggle or strength meter. |
| `textarea_input` | form.rs:1783 | `<textarea>` | **none** | Value is the element body ([Source](autumn/src/form.rs:1777-1820)). The forms guide confirms the gap: "Does not [have required_*]: password_input, textarea_input, checkbox_input" ([Source](docs/guide/forms.md:118-126)). |
| `checkbox_input` | form.rs:2430 | `input type="checkbox" value="true"` | **none** | The label is rendered *before* the box. It deliberately has no hidden `false` sibling, because `serde_urlencoded` rejects duplicate keys, so the target `bool` needs `#[serde(default)]` ([Source](autumn/src/form.rs:2397-2474)). |
| `number_input` | form.rs:2478 | `input type="number" step=…` | `required_number_input` (2524) | Takes only a `step` parameter. There are no `min`/`max` parameters on this helper ([Source](autumn/src/form.rs:2466-2565)). |
| `date_input` | form.rs:2912 | `input type="date"` | `required_date_input` (2956) | Normalizes RFC 3339 or naive values to `YYYY-MM-DD` ([Source](autumn/src/form.rs:2567-2594, 2905-2952)). |
| `datetime_input` | form.rs:3029 | `input type="datetime-local"` | `required_datetime_input` (3070) | Has no time-zone concept. It ships six serde deserializers so that `DateTime<Utc>`, `DateTime<Local>` and `NaiveDateTime` accept the offsetless browser value (`deserialize_datetime_local_utc[_option]`, `_local[_option]`, `deserialize_naive_datetime_local[_option]`) ([Source](autumn/src/form.rs:2720-2905, 3005-3027)). |
| `select_input` | form.rs:3121 | `<select>` with `(value,label)` options | `required_select_input` (3166) | Single-select only: there is no `multiple` and no `<optgroup>`. The placeholder is an empty-value option the caller supplies ([Source](autumn/src/form.rs:3105-3221)). |
| `rich_text_area` (+ `_with_labels`, `_htmx`, `_htmx_with_labels`, `_htmx_with_token_field`, `_htmx_with_token_field_with_labels`) | form.rs:1975-2104 | Markdown `<textarea rows=12>` + a static `role="group"` syntax "toolbar" + a hint `<p id="{field}-hint">`. The optional live preview `div role="region"` updates via `hx-post` on `keyup changed delay:400ms, change`. | `required_rich_text_area*` (2137-2230) | The only helper with a **hint** wired into `aria-describedby` (`{field}-hint`, plus the error id when there are errors). It works without JS. The toolbar is display-only and shows syntax (Bold `**bold**`, Italic, Link, Code, List, Heading, Quote). Toolbar labels are translatable via `RichTextLabels` ([Source](autumn/src/form.rs:1863-1950, 2252-2357)). |
| `submit_button` | form.rs:1724 | `button type="submit" class="autumn-submit"` | n/a | ([Source](autumn/src/form.rs:1722-1728)) |
| `form_tag` / `ChangesetForm::form_tag` | form.rs:1198 / 683 | `<form>` + hidden `_csrf`. PUT, PATCH and DELETE are rewritten to POST with a hidden `_method`. | n/a | `method_input` (form.rs:1288) emits the `_method` field on its own ([Source](autumn/src/form.rs:1195-1300)). |
| `aria_live_region`, `skip_link` | form.rs:3225, 3247 | `div role="status" aria-live="polite"`, plus a skip link | n/a | Page-level a11y helpers ([Source](autumn/src/form.rs:3203-3251)). |

- The `ChangesetForm<T>` extractor exposes method forms of only `text_input`, `text_input_htmx[_with_token_field]`, the `rich_text_area*` family and `submit_button`. Every other control is a free function that takes `&form.changeset` ([Source](autumn/src/form.rs:676-820)).
- The extractor decodes urlencoded or multipart bodies (with the `multipart` feature), runs `validator::Validate`, captures the CSRF token, and drops blank optional fields so they decode as `None` ([Source](autumn/src/form.rs:13-18, 822-1020)).
- Multipart decoding **skips file fields**. The docs warn: "`ChangesetForm` and file uploads do not compose … the upload is silently discarded" ([Source](docs/guide/forms.md:793-817); [Source](autumn/src/form.rs:134-138)).
- None of the changeset helpers accepts a placeholder, a hint/help text (except rich text), `disabled`, `readonly`, `autocomplete`, `inputmode`, `pattern`, `min`/`max`/`minlength`/`maxlength`, or extra attributes. Each signature is only `(changeset, field, label[, step | options])` ([Source](autumn/src/form.rs:1449, 1741, 1783, 2430, 2478, 2912, 3029, 3121)).

**Layer 2: typed accessible primitives (`autumn/src/a11y.rs`, issue #1706)**

These use a type-state `NoLabel`/`Labeled` pattern: only the `Labeled` state implements `maud::Render`, so "an unlabeled field is therefore unrepresentable as markup." The doc spells this out ([Source](autumn/src/a11y.rs:30-73)).

| Primitive | file:line | Renders | Setters |
|---|---|---|---|
| `TextField` | a11y.rs:625 | `<input>` with **arbitrary `input_type(..)`** (a11y.rs:718), e.g. `email` | `label`/`aria_label`/`labelled_by`, `value`, `required`, `aria_required`, `class`, `label_class`, `aria_invalid`, `described_by`, `minlength`, `maxlength`, `min`, `max`, `step`, `hx(name, value)` (a11y.rs:648-882) |
| `TextArea` | a11y.rs:950 | `<textarea>` | the same, minus type/min/max/step (a11y.rs:971-1122) |
| `Select` + `SelectOption` | a11y.rs:1219 / 1173 | `<select>`. Options can be `disabled()` (a11y.rs:1201). | `option`, `options`, `selected_value`, `required`, `aria_required`, `aria_invalid`, `described_by`, `hx` (a11y.rs:1236-1369). **No `multiple`, no optgroup.** |
| `Checkbox` | a11y.rs:1432 | a single boolean checkbox | `value`, `checked`, `required`, `aria_invalid`, `described_by`, `hx` (a11y.rs:1450-1571) |
| `FileField` | a11y.rs:1633 | `input type="file"` with no value | `accept` (1712), `multiple` (1719), `required`, `aria_invalid`, `described_by`, `hx` |
| `RadioGroup` + `RadioOption` | a11y.rs:1902 / 1826 | A visible label gives `<fieldset role="radiogroup"><legend>`; an ARIA label gives `<div role="radiogroup">`. Ids are generated per option, and only the first checked option renders `checked`. Options support `checked`/`disabled`. | `label`/`aria_label`/`labelled_by`, `id_prefix`, `checked_value`, `required`, `aria_required`, `aria_invalid`, `described_by`, `hx` (a11y.rs:1921-2069; render at 2131-2193) |

- Non-form a11y primitives: `Img`, `Button` (with icon-only via `aria-label`), `Link`, `MenuItem` ([Source](autumn/src/a11y.rs:14-29, 102-472)).
- None of the primitives has a placeholder, `disabled` (except on options), `readonly`, `autocomplete`, `pattern` or `inputmode` setter. A grep of a11y.rs for `placeholder|autocomplete|pattern|inputmode` matches only doc comments ([Source](autumn/src/a11y.rs:1199, 1217, 1424)).
- **There is no `radio_input` changeset helper**, and `FieldControl` has no `Radio` variant. Radio groups exist only as the `a11y::RadioGroup` primitive, with manual error wiring ([Source](autumn/src/form.rs:3265-3297)).

**Layer 3: interactive widgets (`autumn/src/widgets.rs` and others)**

| Widget | file:line | Behaviour |
|---|---|---|
| `active_search_input` / `active_search_results` / `active_search` / `active_search_empty_state` | widgets.rs:385 / 420 / 457 / 498 | `input type="search" autocomplete="off" aria-controls` with `hx-get`/`hx-post` and `hx-trigger="input changed delay:{n}ms[, load]"`. The results container is `role="status" aria-live="polite"`. `active_search` adds a `<noscript>` GET/POST form fallback. Config covers debounce (default 300 ms), `min_length`, placeholder, indicator, param name and method ([Source](autumn/src/widgets.rs:105-230, 375-505)). |
| `autocomplete_input` / `autocomplete_option` / `autocomplete_empty_state` | widgets.rs:538 / 626 / 643 | The visible input is `type="search" role="combobox" aria-expanded="false" aria-autocomplete="list" aria-controls`. A hidden value input pairs with a server-filled `div role="listbox" aria-live="polite"`. Options are `div role="option" tabindex="0" data-value`. The `<noscript>` fallback is a `<select>` with static `fallback_options`. Options: `free_text` (tag-style), `initial_value` (422 re-render), `min_length`, `debounce` ([Source](autumn/src/widgets.rs:227-260, 507-650)). |
| autocomplete JS | autumn/vendor/autumn-widgets.js:35-105 | Mouse click or Enter/Space on a focused option selects it. The hidden input gets its `name` only on selection (ID mode) or always (free-text mode). It has **no arrow-key navigation from the input into the listbox, no `aria-activedescendant`, and never toggles `aria-expanded`** (the attribute is set to `"false"` statically). ([Source](autumn/vendor/autumn-widgets.js:37-105); [Source](autumn/src/widgets.rs:567-575)) |
| min-length guard JS | autumn-widgets.js:12-33 | Cancels htmx requests below `data-ac-min-length` ([Source](autumn/vendor/autumn-widgets.js:12-33)) |
| `bulk_select_checkbox` / `bulk_actions_toolbar` / `bulk_actions_form` | widgets.rs:2306 / 2375 / 2481 | A per-row `input type="checkbox" name="ids" value aria-label="Select row {id}"`. The server parses the repeated keys (the scaffold generates `parse_bulk_ids`). Works without JS. **No "select all" checkbox** (a grep for `select.all` in widgets.rs finds nothing) ([Source](autumn/src/widgets.rs:2205-2320)). |
| `reaction_controls` (`ReactionControls::votes` / `likes`) | widgets.rs:1238 | Up/down or like toggle buttons with `aria-pressed`, each in its own POST form with `hx-post` and `hx-sync="#id:replace"`. The count is `aria-live="polite"`. Works without JS ([Source](autumn/src/widgets.rs:899-1312)). |
| `comment_thread` / `CommentThread` | widgets.rs:1707 / 1405 | Comment and reply forms: a labelled `<textarea>`, hidden `reply_to`, CSRF, `hx-post`, a `<details>` reply disclosure, `placeholder`, draft preservation and an error message ([Source](autumn/src/widgets.rs:1405-1660, 1760-1876)). |
| `transition_controls` | widgets.rs:784 | State-machine transition buttons (POST forms) ([Source](autumn/src/widgets.rs:706-900)) |
| `locale_switcher` | widgets.rs:2755 | A `<nav aria-label="Language">` of **links**, not a `<select>`. The current locale is `aria-current="true"` ([Source](autumn/src/widgets.rs:2698-2771)). |
| `error_summary` | widgets.rs:4979 | An error alert listing every changeset message in a `<ul>`, titled "Please fix the following errors". The messages **do not link/anchor to their fields**. Focus management is left to the app (the reddit-clone example moves focus itself) ([Source](autumn/src/widgets.rs:4950-5006); [Source](changelog.d/2454-reddit-clone-form-a11y.md)). |
| `direct_upload_input` | autumn/src/storage/form_helper.rs:66 | `input type="file"` in `div data-controller="direct-upload" data-direct-upload-url`, plus a hidden progress bar `div.autumn-upload-progress > .autumn-upload-bar` and a `<noscript>` notice. The `aria-label` is humanized from `name`, with no visible `<label>`. **The JS controller is not shipped:** the docs say "A JS controller (e.g. a Stimulus controller or custom htmx extension) hooks into these attributes", and a grep for `direct-upload` in the vendor JS finds nothing ([Source](autumn/src/storage/form_helper.rs:1-110); [Source](autumn/vendor/)). |
| `datetime_local_input` (time zone) | autumn/src/time_zone.rs:505 | `input type="datetime-local"` pre-filled from UTC converted into the user's `Tz`. No error/aria wiring ([Source](autumn/src/time_zone.rs:494-519)). |
| `bot_protection_widget` | autumn/src/security/captcha.rs:828 | Turnstile (`div.cf-turnstile` + Cloudflare script) or hCaptcha (`div.h-captcha` + script). Dev-bypass and test providers emit a hidden input ([Source](autumn/src/security/captcha.rs:255-272, 335-445, 828)). |
| `consent_banner_markup` | autumn/src/consent/banner.rs:95 | Two equal-weight `type="submit"` buttons ("Reject non-essential" / "Accept all") plus CSRF. **No per-category toggles or checkboxes** (a grep for `checkbox|fieldset` in consent finds nothing) ([Source](autumn/src/consent/banner.rs:80-120)). |
| `link_to` / `button_to` | autumn/src/links.rs:259 / 383 | `button_to` renders a one-button POST form with method override and CSRF ([Source](autumn/src/links.rs:259-460)) |

**Multi-row and multi-step**

- **Nested forms** (`autumn/src/nested_form.rs`): `NestedChangesetForm<P,C>` decodes `items[i][field]` wire names with a `_destroy` marker and Rails-style `reject_if: all_blank` ([Source](autumn/src/nested_form.rs:1-116)).
  - `RowScope` helpers: `text_input`, `required_text_input`, `number_input`, `textarea_input`, `hidden_input`, `destroy_checkbox` ([Source](autumn/src/nested_form.rs:1387-1575)).
  - `inputs_for` (1713) renders every row plus a blank template row, so rows can be added without JS. An optional htmx "Add row" `<button type="button" hx-get hx-swap="beforeend" hx-params="index">` is served by `nested_row_fragment` (1806) ([Source](autumn/src/nested_form.rs:98-116, 1578-1810)).
  - Row scopes have no select, checkbox (other than destroy), date or radio helpers.
- **Wizard / stepper** (`autumn/src/wizard.rs`): `WizardContext` keeps session-backed per-step data and draft keys.
  - `wizard_progress` renders `<nav aria-label="Progress"><ol role="list" class="wizard-progress">` with `aria-current="step"` on the current `<li>` and an `aria-live="polite"` "step N of M" summary ([Source](autumn/src/wizard.rs:1-60, 82-131, 351-450)).
  - There is no step-navigation component: Back/Next are ordinary buttons the app writes.
- **Submit tokens:** a hidden one-time `_submit_token` field plus `SubmitTokenLayer` give at-most-once POSTs with no JS ([Source](docs/guide/submit-tokens.md:1-30); [Source](autumn/src/security/submit_token.rs:78-87, 443)).

**Constela (feature `constela`):** a JSON UI language for LLM-generated interfaces. Its allowlist permits `input`, `textarea`, `select`, `option`, `optgroup`, `label` and form attributes such as `placeholder`, `pattern`, `multiple`, `autocomplete`, `inputmode` and `list`. It is a sandboxed renderer, not a component library ([Source](autumn/src/constela/mod.rs:1-25); [Source](autumn/src/constela/policy.rs:96, 120-125)).

### Inferences
- Measured against Phoenix `<.input type=…>`, the changeset helpers cover text, password, textarea, checkbox, number, date, datetime-local and select. Phoenix's `<.input>` also passes through `email`, `tel`, `url`, `color`, `range`, `time`, `hidden`, `select multiple` and `checkgroup`-style patterns. Autumn reaches those only through the label-enforcing `a11y::TextField::input_type(..)` escape hatch, which has no changeset binding.
- Autumn's "widgets" are native-element wrappers plus htmx. None is a custom-rendered composite control except the autocomplete listbox.

### Gaps
- I did not render the HTML in a browser, so the visual default styling (`widgets.css`) and real screen-reader behaviour are unverified.
- The `transition_controls` and `comment_thread` internals were read only at the markup level.

## Q2. Which input controls are absent? (checklist against MUI / AntD / Mantine / shadcn / React Aria / Phoenix / simple_form / Django)

### Takeaway
Most "rich" inputs are absent. Autumn ships the native basics, a server-driven single-value combobox, a Markdown textarea with live preview, a radio group, and an unenhanced file input. It has no date picker beyond the native `<input type=date|datetime-local>`, and no switch, slider, multi-select/tag input, OTP, color, rating, phone, currency, mention, transfer, tree or cascader control.

### Cited Findings
Status of each control:

- **Combobox / select-with-search: shipped, partial.** `autocomplete_input` uses `role="combobox"` with server-rendered options, single-value only. It lacks arrow-key navigation and `aria-activedescendant`, and `aria-expanded` is never updated ([Source](autumn/src/widgets.rs:538-611); [Source](autumn/vendor/autumn-widgets.js:65-104)).
- **Multi-select / tag input: absent as a widget.** `select_input`/`a11y::Select` have no `multiple` (only `FileField` has one) ([Source](autumn/src/a11y.rs:1636-1720)). `autocomplete` `free_text` mode submits one text value; its docs call it "tag-style fields" but it holds a single value ([Source](autumn/src/widgets.rs:247-255)). `ChangesetForm` decodes via `serde_urlencoded` (form.rs:993-1018), which the checkbox docs say "rejects duplicate keys" ([Source](autumn/src/form.rs:2400-2410)). Repeated-key arrays therefore do not decode through the changeset path; bulk-select relies on a separate generated `parse_bulk_ids` ([Source](autumn/src/widgets.rs:2262-2267)).
- **Date picker: native only.** `date_input` gives `type="date"` ([Source](autumn/src/form.rs:2912-2945)). There is no calendar popover or custom picker.
- **Time picker: absent.** No `type="time"` anywhere (a grep for `type="time"` across autumn/src, autumn-cli/src and the macros returns nothing).
- **Datetime: native only.** `datetime_input` (form.rs:3029) and `time_zone::datetime_local_input` (time_zone.rs:505).
- **Date-range picker: absent.** No helper.
- **Month/week inputs: absent.** No `type="month"` or `type="week"`.
- **Number stepper: native only.** `number_input` relies on the browser spinner and supports `step` only ([Source](autumn/src/form.rs:2478-2520)).
- **Slider / range: absent.** No `type="range"` anywhere. The raw `a11y::TextField::input_type("range")` would render one but has no helper.
- **Switch / toggle: absent.** No `role="switch"` anywhere. `aria-pressed` toggle buttons exist only inside `reaction_controls` ([Source](autumn/src/widgets.rs:1173, 1289-1303)).
- **Segmented control / button group: absent.**
- **Radio group: shipped as a primitive only.** `a11y::RadioGroup` renders `fieldset/legend`. There is no changeset helper and no `FieldControl::Radio` ([Source](autumn/src/a11y.rs:1879-2193); [Source](autumn/src/form.rs:3265-3297)).
- **Radio cards: absent.**
- **Checkbox group: absent.** There is a single boolean `checkbox_input`/`a11y::Checkbox` (a11y.rs:1424 says Checkbox is bool-only). Bulk-select checkboxes are the only repeated-name checkboxes.
- **Color picker: absent.** No `type="color"`.
- **File input: shipped, basic.** `FieldControl::File`/`a11y::FileField` support `accept` and `multiple`. `form_for` switches the form to `multipart/form-data` ([Source](autumn/src/form.rs:3292-3296, 3631-3634, 3866-3890)).
- **File dropzone with progress: absent.** `direct_upload_input` emits `data-controller` hooks and a hidden progress bar, but **no JS ships**, so there is no drag-and-drop, preview, or working progress ([Source](autumn/src/storage/form_helper.rs:12-40, 66-100)). `autumn-media-plugin` is video/rooms with no upload UI ([Source](autumn-media-plugin/src/)).
- **OTP / PIN input: generated markup only.** `autumn generate auth --totp` emits a single `input type="text" autocomplete="one-time-code" inputmode="numeric" pattern="[0-9 ]*"` ([Source](autumn-cli/src/generate/auth.rs:9048-9049, 9114-9115, 9447, 9701)). There is no segmented OTP widget in the framework.
- **Password reveal / strength meter: absent.** `password_input` is a plain `type=password` ([Source](autumn/src/form.rs:1741-1779)). There is a server-side weak-password list (`autumn/src/auth/weak_passwords.txt`) but no UI meter.
- **Passkey / WebAuthn UI: generated.** `--passkeys` pulls in `webauthn-rs` with `conditional-ui` ([Source](autumn-cli/src/generate/auth.rs:39-50, 166-167)). This is not a framework widget.
- **Phone input: absent.** No `type="tel"` anywhere (`tel` appears only as a Constela URL scheme).
- **Email / URL inputs: generated or primitive only.** DSL constraints `{email}`/`{url}` make the scaffold emit `type="email"`/`type="url"` through `a11y::TextField` ([Source](autumn-cli/src/generate/dsl.rs:18-38)). There is no `email_input` helper in `form.rs`.
- **Currency / money input: absent.** The `money` module has types and arithmetic only, and `format::number_to_currency` is display-only ([Source](autumn/src/money/mod.rs:238-1108); [Source](autumn/src/format.rs:94-148)). Scaffold Decimal columns render `number_input` with a `step` derived from scale ([Source](autumn-cli/src/generate/scaffold.rs:9840-9849)).
- **Rating input: absent.**
- **Rich text (WYSIWYG): absent by policy.** "Block/WYSIWYG editors, collaborative editing, image upload and embedding, @-mentions, slash commands, emoji pickers … are all deliberately out of scope" ([Source](docs/guide/rich-text.md:212-222)).
- **Markdown editor: shipped.** `rich_text_area*` is a textarea with a syntax legend and htmx live preview, sanitized on render ([Source](autumn/src/form.rs:1953-2357)).
- **Code editor: absent.** The JSON column renders a plain `textarea_input` in the scaffold ([Source](autumn-cli/src/generate/scaffold.rs:9858-9866)). The admin plugin uses a "monospace textarea" for JSON ([Source](autumn-cli/src/generate/admin.rs:647)).
- **Mentions: absent by policy** ([Source](docs/guide/rich-text.md:215)).
- **Signature pad: absent.**
- **Transfer list: absent.**
- **Tree select: absent.**
- **Cascader: absent.**
- **Time-zone picker: absent.** `time_zone.rs` resolves the zone from extension, session, cookie or `?tz=` and offers `set_time_zone_cookie`, but has no `<select>` of IANA zones ([Source](autumn/src/time_zone.rs:9-19, 346-364)).
- **Locale switcher: shipped as links, not a select** ([Source](autumn/src/widgets.rs:2755-2771)).
- **Hidden input: shipped in rows only.** `RowScope::hidden_input` exists ([Source](autumn/src/nested_form.rs:1539)). There is no top-level `hidden_input` helper; apps write raw `input type="hidden"`. CSRF, submit-token and `_method` hidden inputs are emitted automatically.
- **CAPTCHA: shipped.** Turnstile and hCaptcha ([Source](autumn/src/security/captcha.rs:255-272, 335-345)).
- **Consent preferences: two buttons only.** No category toggles ([Source](autumn/src/consent/banner.rs:80-120)).
- **Multi-step stepper: shipped.** `wizard_progress` is a display-only progress list ([Source](autumn/src/wizard.rs:403-450)).

### Inferences
- The largest gaps against MUI, AntD and Mantine are the composite pickers: date/time/range calendar, multi-select/tags, switch, slider, color, OTP, rating, file dropzone, transfer, tree and cascader. Several of these could be built on native elements plus htmx while keeping the no-JS contract: switch as a styled checkbox with `role="switch"`, checkbox group as a `fieldset` of checkboxes, segmented control as styled radios, range, color, time.
- Repeated-key decoding (`Vec<T>`) is a precondition for any checkbox-group or multi-select helper. The current `serde_urlencoded` decode path cannot express it (inferred from form.rs:993-1018 and the duplicate-key note at form.rs:2400-2410).

### Gaps
- I found no issue tracker data locally to say whether any of these absent controls is ticketed. `docs/plans/` has no input-control plans (the listing shows only infra and design docs), and `changelog.d/` has no fragments for new input controls (see Q7).

## Q3. How are errors displayed, and how is validation wired (server and client)?

### Takeaway
Validation is server-authoritative, using `validator::Validate` through `ChangesetForm`.
- **Inline errors** sit in a per-field `role="alert"` block tied by `aria-describedby`. An optional **summary** (`widgets::error_summary`) has no field links.
- **Client-side validation** is limited to native HTML5 attributes (`required`, `minlength`/`maxlength`, `min`/`max`, `type=email|url`) plus optional htmx server round-trip validation per field. There is no client-side JS validator.

### Cited Findings
- The `Changeset<T>` API includes `errors_for`, `all_errors`, `add_error`, `is_valid`, `into_valid` and `field_value` ([Source](autumn/src/form.rs:182-340)). The handler pattern is `into_valid()` → `Err(form)` → re-render with 422 ([Source](autumn/src/form.rs:41-121)).
- Inline error markup is `div id="{field}-error" role="alert" class="autumn-field__errors"` with `p.autumn-field__error` per message. The input gets `aria-invalid="true"`, the `--invalid` class and `aria-describedby` ([Source](autumn/src/form.rs:1500-1530)).
- `form_for` wraps a11y primitives in the same skeleton via `wrap_field_control` ([Source](autumn/src/form.rs:3738-3770)).
- `error_summary(&changeset)` returns an Error alert with a sorted `<ul>` of messages, or `None` when the changeset is valid ([Source](autumn/src/widgets.rs:4979-5006)).
- htmx inline validation: `text_input_htmx` posts the whole form on `change` to a validate endpoint that re-runs `ChangesetForm` and returns the single field wrapper to swap via `outerHTML`. It excludes `_submit_token` so the one-time token is not consumed ([Source](autumn/src/form.rs:1480-1530, 1556-1640)).
- The scaffold `--live-validation` flag wires `text_input_htmx`/`required_text_input_htmx` for validated text fields and adds the validate routes to `exempt_paths` ([Source](autumn-cli/src/generate/scaffold.rs:9889-9930)).
- Only *text* fields get htmx inline validation. Numerics, selects, checkboxes and dates do not ([Source](autumn-cli/src/generate/scaffold.rs:9784-9790); [Source](autumn/src/form.rs:1536)).
- A DSL constraint fans out to both sides: `title:String{min=3,max=120}` / `{email}` / `{url}` produce both a `#[validate]` rule and an HTML5 attribute ([Source](autumn-cli/src/generate/dsl.rs:18-38)).
- Nested forms use per-row error keys like `items[1].quantity`, read back with `RowScope::errors_for` ([Source](autumn/src/nested_form.rs:77-87)).
- Other server-side guards:
  - NUL bytes are rejected with `NUL_CHARACTER_FIELD_ERROR`, with an i18n key `common.error.nul_character` ([Source](autumn/src/form.rs:361-369)).
  - `Valid<T>` is the JSON/API path and `Validated<T>` the proof type ([Source](autumn/src/validation.rs:1-42, 135)).
- The forms guide summarizes: "`required_*` only writes the client-side hint, and `#[validate(presence)]` on the model is what actually rejects a blank submission" ([Source](docs/guide/forms.md:118-130)).

### Inferences
- There is no field-level "touched/dirty" state and no client-side async validation beyond htmx `change`. Error placement is fixed (after the input) and not configurable.
- `aria-describedby=""` is emitted as an empty attribute when there are no errors. It is harmless but noisy.

### Gaps
- I did not verify how validator messages are localized end to end beyond the NUL key.

## Q4. Is there a form-builder abstraction like Phoenix `<.simple_form>`/`<.input>` or Rails `form_with`?

### Takeaway
Yes, with limits.
- `form_for(&changeset, action, method)` is a builder over a `FormModel` trait. It dispatches each `FormField`'s `FieldControl` to the matching helper and wraps the result in a CSRF and method-override `<form>`.
- `#[model]` auto-derives `FormModel` from Rust types. There is no standalone `#[derive(Form)]`.
- The control vocabulary is a closed, `#[non_exhaustive]` enum of 9 kinds.

### Cited Findings
- `FieldControl` variants: `Text`, `Textarea`, `Password`, `Number{step}`, `Checkbox`, `Date`, `DateTime`, `Select{options}`, `File` ([Source](autumn/src/form.rs:3256-3297)).
- `FormField { name, label, control, required, value_name }` and `trait FormModel { fn form_fields() }` ([Source](autumn/src/form.rs:3299-3378)).
- `FormFor` builder methods: `.csrf`, `.csrf_field_name`, `.exclude`, `.override_field`, `.override_label`, `.append`, `.prepend`, `.submit_label`, `.multipart`, `.render` ([Source](autumn/src/form.rs:3495-3660)).
  - `render` auto-sets `multipart/form-data` if any field is `File`.
  - The submit button is a bare `button type="submit"` without the `autumn-submit` class ([Source](autumn/src/form.rs:3614-3656)).
- Dispatch is in `render_form_control`. Textarea, checkbox, select and file route through the `a11y` primitives, the rest through the `*_input` helpers. Textarea and Checkbox ignore `required` ([Source](autumn/src/form.rs:3796-3905)).
- `#[model]` type mapping in `form_control_tokens` ([Source](autumn-macros-model/src/model.rs:7083-7149)):

  | Rust type | Control |
  |---|---|
  | `bool` | Checkbox |
  | `Option<bool>` | tri-state Select (Unset/Yes/No) |
  | integer types | `Number{step:"1"}` |
  | `f32`/`f64`/`Decimal` | `Number{step:"any"}` |
  | `NaiveDate` | Date |
  | `NaiveDateTime` | DateTime |
  | `DateTime<Utc \| Local>` | DateTime |
  | other `DateTime<Tz>` | Text |
  | anything else (String, Uuid, user enums, JSON) | Text |

  The required flag is `!Option` ([Source](autumn-macros-model/src/model.rs:7210-7265)).
- The only proc-macro derives in `autumn-macros` are `OpenApiSchema` and `WireShape`, so there is no `#[derive(Form)]` ([Source](autumn-macros/src/lib.rs:516, 1327)). The framework's own comparison table frames the API as Phoenix/Rails/Django-equivalent: "Autumn (Rust) | `Changeset<T>` | `form.text_input("name", "Name")`" ([Source](autumn/src/form.rs:72-79)).
- Scaffold generator mapping (`render_changeset_form_inputs`) ([Source](autumn-cli/src/generate/scaffold.rs:9725-9935)):

  | DSL field kind | Rendered control |
  |---|---|
  | `RichText` | `[required_]rich_text_area_htmx_with_token_field` |
  | `references` (belongs_to) | `<select>` of parent rows |
  | constrained numeric | `a11y::TextField` with min/max |
  | `Attachment` | `a11y::FileField` |
  | `Bool` | checkbox; nullable is a tri-state select |
  | `I32`/`I64` | `number_input` step 1 |
  | `F32`/`F64` | step any |
  | `Decimal` | step from scale |
  | `NaiveDateTime`/`DateTime` | datetime |
  | `Json` | textarea |
  | `Enum` | `[required_]select_input` with a placeholder |
  | String/Text/Uuid | text, htmx text, or a constrained `a11y::TextField` (email/url/textarea) |

- DSL `FieldKind` has **no Date-only kind** and no time/money/phone/color kinds. The kinds are String, Text, RichText, I32, I64, Bool, F32, F64, Uuid, NaiveDateTime, DateTime, Bytea, Attachment, Json, References, Enum, Decimal, Slug, Position, Commentable ([Source](autumn-cli/src/generate/dsl.rs:436-520)).
- Admin plugin `AdminFieldKind`: Text, TextArea, Integer, Float, Boolean, Date, DateTime, Select, Hidden, Password, Json ([Source](autumn-admin-plugin/src/traits.rs:14-36)). Its rendered input types are text, number, checkbox, date, datetime-local, password, file, hidden and search ([Source](autumn-admin-plugin/src/)).

### Inferences
- `form_for` is closer to Django `ModelForm` auto-rendering or simple_form's `f.input` inference than to Phoenix `<.input>`. It infers controls from types but offers no per-field hint, placeholder, wrapper class, or input-HTML attribute passthrough, which simple_form and Phoenix both expose.
- The enum is `#[non_exhaustive]`, which explicitly leaves room for new control kinds without a semver break ([Source](autumn/src/form.rs:3262-3264)).

### Gaps
- There is no field-level `#[model]` attribute to choose a widget (e.g. `#[form(widget = "textarea")]`). Overrides happen only at render time via `FormFor::override_field`. No attribute was found by grepping model.rs for `form(`/`widget`.

## Q5. Progressive enhancement: do inputs work without JS?

### Takeaway
Yes. "Works without JavaScript" is an explicit, repeatedly enforced design contract. There are two exceptions: the direct-upload helper, which depends on JS that is not shipped, and the third-party CAPTCHA scripts.

### Cited Findings
- Inline htmx validation falls back to the full-form POST with a 422 re-render ([Source](autumn/src/form.rs:35-37, 140-145)).
- The rich-text toolbar is intentionally non-interactive: "a scripted control that silently does nothing when scripting is off is worse than no control at all. The editor's contract is that it works with no JavaScript (issue #1255)" ([Source](autumn/src/form.rs:1869-1880)).
- `active_search` renders a `<noscript>` form fallback ([Source](autumn/src/widgets.rs:478-492)).
- `autocomplete_input` renders a `<noscript><select>` fallback, and the hidden input has no `name` until JS assigns it, to avoid duplicate fields ([Source](autumn/src/widgets.rs:548-560, 595-605)).
- Nested forms add rows without JS via a pre-rendered blank row. htmx "Add row" is an enhancement ([Source](autumn/src/nested_form.rs:98-116)).
- PUT/PATCH/DELETE work from plain forms via the `_method` override ([Source](autumn/src/form.rs:1207-1219)).
- Submit tokens provide at-most-once submission without JS ([Source](docs/guide/submit-tokens.md:1-10)).
- Bulk actions and reactions are plain POST forms ([Source](autumn/src/widgets.rs:2258-2267, 1185)).
- Exception: `direct_upload_input` shows only a `<noscript>` notice that "Direct upload requires JavaScript", and the JS controller is left to the app ([Source](autumn/src/storage/form_helper.rs:12-40, 92-100)).
- The JS runtime is a same-origin static file, so it works under the default `script-src 'self'` CSP ([Source](autumn/vendor/autumn-widgets.js:1-4)).

### Inferences
- The no-JS contract is a likely reason rich composite pickers (calendar, slider with tooltip, tag input) are absent. Any additions would need a native-element baseline.

### Gaps
- None material.

## Q6. Accessibility: label association, required indicators, aria-invalid, aria-describedby, fieldset/legend

### Takeaway
Accessibility is strong at the label level. The `a11y` primitives make a missing label a compile error, and every helper emits `<label for>`, `aria-invalid`, `aria-describedby` and `role="alert"` errors.

The weaker spots:
- No visual required indicator (asterisk). `required` and `aria-required` are attribute-only, and three control kinds have no required variant.
- No generic hint/help-text slot.
- Fieldset/legend only for `RadioGroup`.
- The autocomplete combobox is incompletely keyboard-accessible.
- The direct-upload input has no visible label.

### Cited Findings
- The type-state label obligation maps to WCAG 1.3.1, 3.3.2 and 4.1.2 ([Source](autumn/src/a11y.rs:1-73)). `autumn a11y verify` fails builds that regress ([Source](docs/guide/forms.md:756-780)).
- Required signalling: `required_*` adds `required` + `aria-required="true"`. No `required_*` exists for password, textarea or checkbox ([Source](docs/guide/forms.md:118-126)). No helper renders a visual asterisk or "(required)" text: grepping `form.rs` label markup shows the label as just `(label)` ([Source](autumn/src/form.rs:1514, 2380)).
- Hints: only `rich_text_area` has a `{field}-hint` element in `aria-describedby` ([Source](autumn/src/form.rs:2262-2310)). Other helpers have no hint parameter; `a11y` primitives can point `described_by` at a single caller-supplied id ([Source](autumn/src/a11y.rs:814)).
- Fieldset/legend: `RadioGroup` with a visible label renders `<fieldset role="radiogroup"><legend>`. With an ARIA label it renders `<div role="radiogroup">` ([Source](autumn/src/a11y.rs:1879-1900, 2160-2193)). There is no fieldset helper for checkbox groups or date parts.
- Live regions:
  - The search results and autocomplete listbox use `aria-live="polite"`.
  - The rich-text preview is deliberately *not* live, to avoid reading the whole post back to the author ([Source](autumn/src/form.rs:2318-2326)).
  - `aria_live_region` serves page-level announcements ([Source](autumn/src/form.rs:3203-3228)).
- The combobox keyboard model is incomplete:
  - Options are `tabindex="0"` divs selected with Enter/Space.
  - There are no ArrowUp/ArrowDown handlers, no `aria-activedescendant`, and `aria-expanded` stays `"false"`.
  - The listbox itself carries `aria-live="polite"`.
  - ([Source](autumn/src/widgets.rs:567-590, 626-638); [Source](autumn/vendor/autumn-widgets.js:73-83))
- `direct_upload_input` uses an `aria-label` humanized from the field name rather than a visible `<label>` ([Source](autumn/src/storage/form_helper.rs:41-80)).
- The bulk-select checkboxes use `aria-label="Select row {id}"` ([Source](autumn/src/widgets.rs:2306-2317)).
- Forced-colors focus outlines are kept on form fields, the submit button, search and autocomplete ([Source](changelog.d/wayfinder-widget-focus-outline.md)).
- Generated auth forms now bind `<label for>`/`id`, and the 2FA confirm input gained a label ([Source](changelog.d/3170-auth-form-labels.md)).
- `error_summary` has no focus management and no links to fields; the reddit-clone example adds focus-on-summary itself ([Source](autumn/src/widgets.rs:4979-5006); [Source](changelog.d/2454-reddit-clone-form-a11y.md)).

### Inferences
- The a11y layer is ahead of many component libraries on "unlabeled is uncompilable", and behind React Aria and MUI on composite-widget keyboard models (combobox) and on visual required/help-text conventions.

### Gaps
- No automated audit output (e.g. axe results) was available to confirm runtime behaviour.

## Q7. Planned or in-flight input work (changelog.d, docs/plans)

### Takeaway
There is no planned work on new input controls at HEAD. Recent fragments are a11y and robustness fixes to existing controls. Several absences are declared out of scope rather than planned.

### Cited Findings
- `changelog.d/` holds 183 fragments. The form-related ones are:
  - `3170-auth-form-labels.md`: label `for`/`id` binding in generated auth.
  - `wayfinder-widget-focus-outline.md`: forced-colors focus outlines.
  - `3267-form-content-type-case.md`: case-insensitive `Content-Type` matching for `ChangesetForm`/`NestedChangesetForm`.
  - `2454-reddit-clone-form-a11y.md`: error summary and focus in the example.
  - `2439-nul-byte-follow-ups.md`.
  - ([Source](changelog.d/3170-auth-form-labels.md); [Source](changelog.d/wayfinder-widget-focus-outline.md); [Source](changelog.d/3267-form-content-type-case.md); [Source](changelog.d/2454-reddit-clone-form-a11y.md))
- A grep of docs, changelog.d and autumn/src for `date.picker|datepicker|combobox|multi-select|tag input|color picker|slider|toggle switch|checkbox group|otp input|dropzone` matches only `active-search-and-autocomplete.md`, `ledgered-entities.md`, `form.rs` and `widgets.rs`. These are existing-feature mentions, not plans ([Source](docs/guide/active-search-and-autocomplete.md:157-190)).
- `docs/plans/` contains only infrastructure and design plans (actuator, profiles, repository macros, harvest, WASM, webhooks, and others). None covers input controls ([Source](docs/plans/)).
- Explicitly out of scope:
  - WYSIWYG, mentions, emoji pickers, image embed and click-to-insert toolbar ([Source](docs/guide/rich-text.md:212-222)).
  - Search indexing and ranking for active search ([Source](docs/guide/active-search-and-autocomplete.md:19)).
- Signposts for future control kinds:
  - `FieldControl` is `#[non_exhaustive]` "new control kinds will be added without a semver-major bump" ([Source](autumn/src/form.rs:3262-3264)).
  - The `select_input` docs reference enum (#1030) and references (#1026) field kinds as consumers ([Source](autumn/src/form.rs:3105-3118)).

### Inferences
- Any gap-filling (switch, checkbox group, multi-select, range, color, time, OTP, file dropzone JS, combobox keyboard support, hint/placeholder passthrough, visual required marker, linked error summary) would be net-new work with no existing ticket visible in the repo.

### Gaps
- GitHub issues were not queried, so open feature requests for these controls may exist outside the repo.
