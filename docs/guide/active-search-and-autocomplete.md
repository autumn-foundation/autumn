# Active Search & Autocomplete

Autumn provides first-class helpers for two common query-driven UI patterns:
**active search** (results appear as the user types) and
**autocomplete/lookup** (pick a related record and store its ID).
Both are built on htmx and server-rendered Maud fragments — no client-side
libraries required.

## When to use what

| Situation | Recommended approach |
|-----------|---------------------|
| Keyword search over a list, rendered server-side | `active_search` / `active_search_input` |
| Select a single related record, store its ID | `autocomplete_input` |
| Simple filter that works fine as a plain `GET` form | `axum::extract::Query` |
| The htmx wiring needs unusual attributes | Hand-write `hx-*` attributes |
| Full-text ranking, stemming, or GIN index search | [`Repository#search`](./full-text-search.md) paired with `active_search_input` |

Out of scope: search indexing, ranking, stemming, typo tolerance, vector
search, infinite scrolling, client-side autocomplete, or command palettes.

---

## Active search

### Page template

```rust
use autumn_web::prelude::*;
use autumn_web::widgets::{ActiveSearchConfig, active_search};

#[get("/posts")]
async fn index() -> Markup {
    let config = ActiveSearchConfig::new("/posts/search", "#post-results")
        .placeholder("Search posts…")
        .debounce(400)       // ms before request fires (default: 300)
        .min_length(2);      // minimum characters (default: 1)

    html! {
        (active_search("post-search", "Search posts", &config))
        // ^ emits input + results container + noscript fallback
    }
}
```

`active_search` emits:
- A `<div id="post-search-wrapper">` containing:
  - A `<label>` + `<input type="search">` with htmx attributes
  - A `<div id="post-search-results" role="status" aria-live="polite">` results container
  - A `<noscript>` fallback `<form method="get">` for non-JavaScript browsers

### htmx attributes emitted

The search input receives:

```html
<input
  type="search"
  id="post-search"
  name="q"
  autocomplete="off"
  aria-controls="post-results"
  hx-get="/posts/search"
  hx-trigger="input changed delay:400ms"
  hx-target="#post-results"
>
```

### Search handler

Your handler is an ordinary Autumn route that returns a `Markup` partial:

```rust
use autumn_web::prelude::*;
use autumn_web::widgets::active_search_empty_state;
use serde::Deserialize;

#[derive(Deserialize)]
struct SearchQuery { q: String }

#[get("/posts/search")]
async fn search(
    Query(params): Query<SearchQuery>,
    repo: PgPostRepository,        // your repository
) -> AutumnResult<Markup> {
    let q = params.q.trim();
    if q.is_empty() {
        return Ok(active_search_empty_state("Enter a search term"));
    }

    // Works with the full-text search repository feature:
    let results = repo.search(q).await?;

    Ok(html! {
        @if results.is_empty() {
            (active_search_empty_state("No results found"))
        } @else {
            ul {
                @for post in &results {
                    li { (post.title) }
                }
            }
        }
    })
}
```

### Integration with full-text search

If your model uses `#[repository(..., searchable)]`, the generated
`repo.search(q)` method returns results ranked by relevance. Pass the query
string straight through:

```rust
// The handler stays the same — no special plumbing needed.
let results = repo.search(params.q.trim()).await?;
```

See [Full-Text Search](./full-text-search.md) for repository configuration.

### Configuration options

| Builder method | Default | Effect |
|----------------|---------|--------|
| `.debounce(ms)` | `300` | Debounce delay before the request fires |
| `.min_length(n)` | `1` | Minimum characters required (enforced server-side) |
| `.indicator(selector)` | *(none)* | CSS selector for an `htmx-indicator` element |
| `.initial_load()` | `false` | Fire the search immediately on page load |
| `.placeholder(text)` | *(none)* | Placeholder text for the input |
| `.param_name(name)` | `"q"` | Query parameter name |
| `.post()` | *(GET)* | Use `hx-post` instead of the default `hx-get` |

### POST opt-in

`GET` is the default because search queries are idempotent, cacheable, and
bookmarkable. Use `.post()` only when the handler genuinely needs a request
body (e.g. large filter payloads or CSRF-protected endpoints):

```rust
let config = ActiveSearchConfig::new("/posts/search", "#post-results").post();
```

---

## Autocomplete / lookup

Autocomplete lets a user type into a visible search field and pick a related
record. The selected record's ID is stored in a hidden field and submitted with
the form.

### Page template

```rust
use autumn_web::widgets::{AutocompleteConfig, autocomplete_input};

let config = AutocompleteConfig::new(
    "/tags/autocomplete", // handler URL
    "tag_id",             // name of the hidden input (stores the selected ID)
)
.placeholder("Search tags…");

html! {
    form action="/posts" method="post" {
        // … other fields …
        (autocomplete_input("tag-picker", "Tag", &config))
        button type="submit" { "Save" }
    }
}
```

Rendered HTML (abbreviated):

```html
<div id="tag-picker-wrapper"
     data-ac-value-id="tag-picker-value"
     data-ac-value-name="tag_id">
  <label for="tag-picker-query">Tag</label>
  <input type="search" id="tag-picker-query" name="q"
         role="combobox" aria-expanded="false" aria-autocomplete="list"
         aria-controls="tag-picker-options"
         data-ac-query data-ac-min-length="1"
         hx-get="/tags/autocomplete"
         hx-trigger="input changed delay:300ms"
         hx-target="#tag-picker-options">
  <input type="hidden" id="tag-picker-value" value="">
  <div id="tag-picker-options" role="listbox" aria-live="polite"></div>
  <noscript>
    <select name="tag_id">…</select>
  </noscript>
</div>
```

Option selection and input sync are handled by the `autumn-widgets.js` runtime (no inline JS, CSP-compatible). Include it once in your layout:

```html
<script src="/static/js/autumn-widgets.js" defer></script>
```

### Autocomplete handler

Your handler returns option partials using `autocomplete_option` and
`autocomplete_empty_state`:

```rust
use autumn_web::widgets::{autocomplete_option, autocomplete_empty_state};

#[get("/tags/autocomplete")]
async fn tags_autocomplete(
    Query(params): Query<SearchQuery>,
    mut db: Db,
) -> AutumnResult<Markup> {
    let q = params.q.trim();
    if q.is_empty() {
        return Ok(autocomplete_empty_state("Type to search tags."));
    }

    let tags: Vec<Tag> = /* your Diesel query */ ...;

    Ok(html! {
        @if tags.is_empty() {
            (autocomplete_empty_state("No matching tags found."))
        } @else {
            @for tag in &tags {
                (autocomplete_option(&tag.id.to_string(), &tag.name))
            }
        }
    })
}
```

`autocomplete_option(value, label)` renders:

```html
<div role="option" tabindex="-1" data-value="42">Tag Name</div>
```

The `autumn-widgets.js` runtime handles click and keyboard selection
on the listbox. It copies the option's `textContent` into the visible input and
its `data-value` into the hidden field, then clears the listbox. No additional
wiring is needed beyond including the script in your layout.

### Keyboard

The widget follows the APG combobox pattern. Focus stays on the input.

| Key | Effect |
|---|---|
| ArrowDown / ArrowUp | Move the active option. Wraps. Opens a closed popup. |
| Enter | Selects the active option. It does not submit the form. |
| Escape | Closes the popup. |
| Tab, or a click outside | Closes the popup. |

`aria-expanded` is `"true"` only while the popup shows an option.
`aria-activedescendant` names the active option. See
[Widget Keyboard Core](keyboard.md).

### Free-text mode (tag-style fields)

By default, the hidden field is **only** set when an option is selected from the
list — typing a value and submitting without selecting leaves the field empty.
This is the safe default for foreign-key ID fields (submitting unvalidated text
as an ID would break form deserialization).

For tag-style fields where users should be able to submit typed text directly
(e.g. creating a new tag), enable free-text mode:

```rust
let config = AutocompleteConfig::new("/tags/autocomplete", "tag")
    .free_text(); // keep hidden field in sync with typed text
```

---

## No-JavaScript fallback

Both `active_search` and `autocomplete_input` emit a `<noscript>` block that
works without JavaScript:

- **Active search** — a plain `<form method="get">` that submits the query.
  Your handler already returns a correct response; wrap it in your layout
  for the full-page no-JS case.
- **Autocomplete** — a `<select name="...">` that submits the value directly.
  Pass `.fallback_options(&[("val", "Label"), …])` to `AutocompleteConfig` to
  populate its options server-side for the fallback path.

For a seamless no-JS experience, detect whether the request is an htmx
request using `HxRequest` and wrap the response in a full layout if not:

```rust
#[get("/posts/search")]
async fn search(hx: HxRequest, Query(params): Query<SearchQuery>, ...) -> AutumnResult<impl IntoResponse> {
    let partial = /* render results */;
    if hx.is_htmx {
        Ok(partial.into_response())
    } else {
        Ok(layout("Search results", partial).into_response())
    }
}
```

---

## Accessibility

All widgets include:

- `<label>` associated with the input via `for`/`id`
- `aria-controls` pointing at the results container
- `role="status"` and `aria-live="polite"` on the results container (so screen
  readers announce updates without moving focus)
- `aria-atomic="true"` on the active search results container
- `role="combobox"` + `aria-expanded` + `aria-autocomplete="list"` on the
  autocomplete visible input
- `role="listbox"` on the autocomplete options container
- `role="option"` + `tabindex="-1"` on each autocomplete option (focus stays
  on the input; `aria-activedescendant` names the active option)
- `role="status"` + `aria-live="polite"` on empty-state partials

---

## Example: bookmarks app

The `examples/bookmarks` app demonstrates both primitives:

- **Active search** on `GET /bookmarks` — searches across `title`, `url`, and
  `tag` fields using an ILIKE query.
- **Tag autocomplete** on `GET /bookmarks/new` — lists existing tags matching
  the typed prefix via `GET /bookmarks/tags/autocomplete`.

See `examples/bookmarks/src/routes/bookmarks.rs` for the full implementation.
