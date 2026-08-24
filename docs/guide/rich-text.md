# Rich Text

Blogs, forums, comment threads, and wikis all need the same thing: let a user
write formatted text, then show it to other users. Doing that by hand means
picking an editor, picking a Markdown renderer, and — the part everyone
forgets — picking and correctly configuring an HTML sanitizer. Skip the last
step and you have shipped stored XSS.

Autumn gives you the whole path in one scaffold token:

```bash
autumn generate scaffold Post title:String body:richtext
```

That produces a `TEXT` column holding the Markdown source, a form with a
Markdown editor and a live preview, and a show view that renders the stored
text through a sanitizer. No JavaScript of your own, no sanitizer to configure.

## The sanitization guarantee

`autumn_web::markdown::render_user_content` is the only function you should
use to render text a request body carried in. It applies **two independent
controls**:

1. **Raw-HTML passthrough is disabled.** Every raw-HTML event the Markdown
   parser produces is rewritten into a text event, so `<script>alert(1)</script>`
   in the source renders as the visible characters `<script>alert(1)</script>`,
   not as a script tag. Link destinations are checked against the URL scheme
   allowlist *before* the HTML writer runs; a link with a rejected scheme is
   degraded to its own text. Image destinations are never rendered at all, so
   they are dropped rather than scheme-checked.
2. **The output is run through an allowlist sanitizer.** The resulting HTML
   string is passed through [`ammonia`](https://crates.io/crates/ammonia),
   configured with the curated tag set, a per-tag attribute allowlist, and the
   same URL-scheme allowlist.

Either control alone blocks the canonical payloads. Both are applied so that a
bypass of one is not a bypass of the feature.

```rust
use autumn_web::markdown::render_user_content;

// In a handler or template:
html! {
    article class="post-body" { (render_user_content(&post.body)) }
}
```

The guarantee is locked down by an adversarial payload corpus in
`autumn/tests/integration/rich_text.rs`, which parses the rendered output and
asserts that no non-allowlisted element, no event-handler attribute, and no URL
outside the scheme allowlist survives.

## The allowlist

Both lists are public constants, so the guarantee is inspectable rather than
folklore:

| Constant | Contents |
| --- | --- |
| `markdown::RICH_TEXT_ALLOWED_TAGS` | `p` `br` `hr` `blockquote` `h1`–`h6` `em` `strong` `del` `sub` `sup` `a` `ul` `ol` `li` `code` `pre` `table` `thead` `tbody` `tr` `th` `td` |
| `markdown::RICH_TEXT_ALLOWED_URL_SCHEMES` | `http` `https` `mailto` `tel` |

`sub` and `sup` have no CommonMark syntax, so they are unreachable from
`render_user_content` — they are allowlisted for `sanitize_user_html` callers
whose source is already HTML.

Attributes are allowlisted per tag and narrowed further by value:

| Tag | Allowed attributes |
| --- | --- |
| `a` | `href`, `title` (`rel` is *forced* to `noopener noreferrer nofollow`) |
| `code`, `pre` | `class`, and only in the `language-{lang}` shape a fenced code block produces |
| `th`, `td` | `style`, and only a `text-align: left\|right\|center` rule |
| `ol` | `start` |

A URL destination with **no** scheme — relative (`/about`), fragment
(`#section`), or protocol-relative (`//host/path`) — is allowed. The scheme is
read with the same tolerances a browser applies, so `JaVaScRiPt:`,
`java<TAB>script:`, and a leading-NUL-padded `javascript:` are all recognised
and rejected.

### What is deliberately excluded

- **`<img>`.** Image embedding is out of scope for this field. A Markdown image
  degrades to its alt text, so a post cannot beacon a reader's IP address to a
  third-party host.
- **`id` and `name` attributes.** User-controlled ids enable DOM clobbering —
  a heading called "Login" shadowing `document.getElementById("login")`. Unlike
  the trusted-content renderer, this path injects no heading anchors.
- **`style` attributes**, except the table-alignment rule above.
- **`class` attributes**, except the code-fence language hint.
- **`<script>`, `<style>`, `<iframe>`, `<object>`, `<embed>`, `<form>`,
  `<input>`, `<svg>`, `<math>`, `<base>`, `<link>`, `<meta>`** and every
  event-handler (`on*`) attribute.
- **Per-field allowlist configuration.** The allowlist is fixed on purpose: one
  guarantee, reasoned about once, identical in every app.

## Trusted content is a different function

The `markdown` module has two entry points, and picking the wrong one is a
security bug:

| Source of the Markdown | Use |
| --- | --- |
| Files you authored and committed (docs, marketing pages, a `content/` tree) | `markdown::render` / `MarkdownRegistry` |
| Anything a request body carried in (posts, comments, wiki bodies, bios) | `markdown::render_user_content` |

`markdown::render` is built for build-time content: it injects heading anchors
and applies **no** URL-scheme allowlist, so `[x](javascript:alert(1))` renders
as written. Never point it at user input.

If your untrusted rich text arrives as HTML rather than Markdown — a legacy
column, an imported feed — use `markdown::sanitize_user_html`, which applies
the same allowlist to an HTML string.

## The editor widget

`autumn_web::form::rich_text_area` renders a labeled `<textarea>` carrying the
Markdown source, a minimal formatting toolbar, and a hint. It needs no
JavaScript and degrades to an ordinary textarea everywhere.

The toolbar shows the syntax for each supported construct (`**bold**`,
`[text](url)`, `- item`, …) rather than inserting it on click. That is
deliberate: inserting text into a `<textarea>` is impossible in HTML alone, so a
click-to-insert toolbar would require a script — and a control that silently
does nothing when scripting is off is worse than no control. The editor's
contract is that it works with no JavaScript, so the toolbar holds to the same
bar.

```rust
form.rich_text_area("body", "Body")
```

For a **non-nullable** column use `required_rich_text_area` (and
`required_rich_text_area_htmx` for the preview variant), which adds the HTML
`required` attribute and `aria-required="true"`. This matters more than it
looks: both `String` deserialization and a `TEXT NOT NULL` column accept the
empty string, so without that signal an empty editor would silently persist a
blank body unless the field also declared a `{min=N}` constraint. The scaffold
picks the right variant from the column's nullability.

`rich_text_area_htmx` adds a live preview pane:

```rust
form.rich_text_area_htmx("body", "Body", &paths::preview_body())
```

The textarea POSTs the form to the preview URL a short moment after typing
stops and swaps the response into `#body-preview` — htmx attributes only, no
script of your own. The preview handler renders through the *same* sanitizer as
the show view, so what the author previews is byte-for-byte what a reader gets,
including anything the allowlist strips:

```rust
#[post("/posts/preview/body")]
pub async fn preview_body(body: Bytes) -> Markup {
    let source = autumn_web::form::field_from_urlencoded(&body, "body")
        .unwrap_or_default();
    autumn_web::markdown::render_user_content(&source)
}
```

The scaffold generates exactly this handler for you.

Note that it reads the one field it needs with
`form::field_from_urlencoded` rather than deserializing the whole form.
`hx-include="closest form"` posts **every** field, so on a freshly-opened "new"
page a required `i64` column arrives as `count=`. Decoding the form struct would
fail on that empty string and the preview would stay blank until the author had
filled in every unrelated column. A preview validates nothing, so it has no
reason to depend on the rest of the form being valid — unlike an inline
*validation* fragment, which decodes the whole form precisely because checking
`#[validate]` rules is its job.

### Submit tokens

The preview POST `hx-include`s the whole form, which would otherwise spend the
one-time `_submit_token` that the real create/update submit needs. Two guards
cover this:

- the widget emits `hx-params="not _submit_token"`, filtering the token out
  client-side. If your app customizes `[security.submit_token].field_name`, use
  `rich_text_area_htmx_with_token_field` and pass the configured name from the
  `SubmitFormField` extractor;
- the scaffold adds `/{plural}/preview` to `[security.submit_token]
  exempt_paths` in `autumn.toml`, which is field-name-agnostic.

## Storing and rendering

The column stores the Markdown **source**, never rendered HTML. That means:

- an author can edit what they wrote, exactly as they wrote it;
- tightening the allowlist takes effect on the next page render, with no
  migration and no re-sanitization pass over historical rows;
- a bug in the sanitizer is fixed by upgrading, not by rewriting your data.

The index view renders the source as escaped text — a table cell has no
business carrying block-level markup, and escaped source is inherently safe.
The rendered form appears on the show page.

## Feature flag

All of this lives behind `autumn-web`'s `markdown` feature:

```toml
autumn-web = { version = "0.7", features = ["markdown"] }
```

A `richtext` scaffold enables it on your project automatically.

## Not in scope

Block/WYSIWYG editors, collaborative editing, image upload and embedding,
@-mentions, slash commands, emoji pickers, storing pre-rendered HTML, and
per-field allowlist configuration are all deliberately out of scope. The goal
is one token to safe formatted text — not an editor product.

A **click-to-insert** toolbar is also out of scope for the same reason: it
cannot be built without JavaScript, and the widget's contract is that it works
without any. If you want one, `rich_text_area`'s markup is stable enough to
enhance from your own script — the editor is `#{field}` and the toolbar is
`.autumn-rich-text__toolbar`.

### Known limitation: field order

A `richtext` column is rendered through `form_for`'s `.exclude()` + `.append()`
escape hatch, and appended markup lands just before the submit button. So in a
generated form a rich-text column always appears **last**, regardless of where
it sits in the field list. The attachment and DSL-constrained columns share this
limitation. Reorder by editing the generated `{resource}_form_for` helper.
