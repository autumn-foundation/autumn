# Generated UI (Constela)

You want a page a model wrote. Not a chat bubble containing a description of a
page — the actual interface: a filter panel shaped like the data it filters, a
dashboard laid out for the question someone just asked, a form with the fields
this particular record needs.

The obvious way to get one is to ask for HTML. That is remote code execution
with extra steps, and every mitigation for it costs the thing you wanted:
sanitize the output and the interactivity goes; sandbox an iframe and the page
is no longer part of your app.

[Constela](https://github.com/yuuichieguchi/constela) takes the other route. The
model does not emit code. It emits a **description**, in a constrained JSON
language with no escape hatch — no expression form that calls a function, no
attribute that holds script, no way to spell `eval`. What a document can express
is exactly what Autumn can check, and Autumn checks all of it before anything
renders.

```bash
cargo add autumn-web --features constela
```

## The shortest possible example

```rust
use autumn_web::constela::{Document, Limits, RenderContext};
use autumn_web::prelude::*;

#[get("/generated")]
async fn generated() -> AutumnResult<Markup> {
    // In a real app this string came back from a model.
    let source = r#"{
      "version": "1.0",
      "state": { "greeting": { "type": "string", "initial": "Hello, Autumn!" } },
      "actions": [],
      "view": {
        "kind": "element", "tag": "p",
        "props": { "class": { "expr": "lit", "value": "greeting" } },
        "children": [{ "kind": "text", "value": { "expr": "state", "name": "greeting" } }]
      }
    }"#;

    let document = Document::parse(source, &Limits::default())?;
    let ctx = RenderContext {
        state: document.initial_state(),
        ..RenderContext::default()
    };
    Ok(document.render(&ctx)?.body)
}
```

`?` on a Constela error produces a **422**, not a 500: a document that does not
describe a renderable UI is malformed input, the same class of fault a
`#[validate(...)]` rejection is.

## The pipeline

```text
 JSON  ──parse──▶  Program  ──validate──▶  Document  ──render──▶  Markup
        limits              references,               escaping,
        depth               allowlists,               id prefixing,
        size                shapes                    render limits
```

`Document::parse` runs the first two stages. Both of its constructors — `parse`
and `from_program`, for a `Program` you already have — go through validation,
and there is no third, so holding a `Document` is a proof that the checks ran. A
function taking one never has to ask whether the names in it resolve or whether
its tags are allowed.

## The safety guarantee

A Constela document is as trustworthy as the request body in
[the rich-text path](rich-text.md): the author is an attacker until proven
otherwise. Four **independent** controls are applied, all of them:

1. **Tags are allowlisted.** A curated set of structural, text, media and form
   elements. Every script-bearing, style-bearing and document-structure element
   — `script`, `style`, `iframe`, `object`, `embed`, `base`, `link`, `meta`,
   `svg`, `math`, `template`, `noscript`, `html`, `head`, `body`, `title` — is
   rejected at validation time and never reaches the renderer.
2. **Attributes are allowlisted.** Presentation, form, table and
   `aria-*`/`data-*` attributes. Every `on*` handler attribute is rejected
   first and explicitly, so it cannot be readmitted by a later rule. `style` is
   not on the list, and neither is anything under `data-constela-*` — that
   prefix is the renderer's own, and a document has a first-class way (an event
   binding, a `ref`, an `island` node) to say everything it carries.
3. **URL schemes are allowlisted.** Relative URLs plus `http`, `https`,
   `mailto` and `tel`, checked after stripping the ASCII whitespace and control
   characters a browser ignores when it resolves a scheme — so
   `java&#9;script:alert(1)` is rejected along with the plain spelling.
   Literal URLs are checked during validation, and **every computed URL is
   checked again at render time**, where its value is finally known.
4. **Text is escaped, never interpolated.** Every byte of document-derived
   output goes through one of the renderer's two escape functions — one for
   text, one for attribute values, which are always written inside double
   quotes. Tag and attribute *names* are only ever written after passing the
   allowlists above, so they are fixed strings from a fixed set. There is no
   other path from a document to the page: the one node that yields markup
   rather than text, `markdown`, is rendered through
   `markdown::render_user_content` — the same allowlist sanitizer the
   user-submitted rich-text path uses.

The lists are fixed, with no per-app configuration, for the same reason
`RICH_TEXT_ALLOWED_TAGS` has none: a guarantee that varies per call site is a
guarantee nobody can state. They live in `autumn_web::constela::policy`, and
the adversarial corpus that locks them down is
`autumn/tests/integration/constela.rs`.

### A panic is a vulnerability here, so it is a build failure

Not every attack on a page that renders generated input is an injection. An
index one past the end, or an add that overflows, is a 500 that anyone who can
influence the document can trigger at will. Every module in this path is
therefore enrolled in the [request-path panic gate](../../CONTRIBUTING.md):
`unwrap`, `expect`, `panic!`, indexing, string slicing and unchecked arithmetic
are all `deny`-level in production code, so a panic reachable from a document is
caught by `cargo clippy`, not by an incident.

### Element ids are prefixed, not banned

The rich-text path bans `id` outright, because a user-chosen id can shadow
`document.getElementById("login")` in the host page's own scripts — DOM
clobbering. A generated *interface* needs ids: `<label for>` and
`aria-labelledby` are how a form is accessible at all.

So Autumn prefixes them instead. Every id a document writes, and every attribute
that references one (`for`, `form`, `list`, `headers`, `aria-labelledby`,
`aria-describedby`, `aria-controls`, …), is rewritten with
`RenderContext::id_prefix` — `c-` by default:

```json
{ "kind": "element", "tag": "label", "props": { "for": { "expr": "lit", "value": "email" } } }
```

renders as `<label for="c-email">`. A same-document fragment link is rewritten
to match, so `href="#section"` becomes `href="#c-section"` and still reaches the
element the document called `id="section"` — prefixing one half and not the
other would break in-fragment navigation and let `#section` resolve against the
host page instead. A cross-document fragment (`/other#section`) is left alone;
it points at ids this render did not write.

Association inside the fragment keeps working; collision with the host page
becomes impossible. If a page embeds more
than one document, give each its own prefix:

```rust
let ctx = RenderContext {
    state: document.initial_state(),
    id_prefix: format!("panel-{panel_id}-"),
    ..RenderContext::default()
};
```

`target="_blank"` links also get `rel="noopener noreferrer"` added
automatically, whether or not the document remembered to ask for it.

## Diagnostics are a repair prompt

Validation collects **every** violation, not the first one, and each carries a
path into the JSON that was submitted plus a stable machine-readable code. A
generator that gets that list back converges in one round instead of six:

```rust
match Document::parse(&source, &Limits::default()) {
    Ok(document) => render(&document),
    Err(err) => {
        // A JSON array of { path, code, message } — hand it straight back.
        let repair_prompt = err.to_json();
        regenerate_with(&source, &repair_prompt).await?
    }
}
```

A typical report:

```json
[
  { "path": "view.children[0]",
    "code": "constela.tag_not_allowed",
    "message": "tag \"iframe\" is not on the allowlist; see `constela::policy::ALLOWED_TAGS`" },
  { "path": "view.children[1].props.href",
    "code": "constela.url_scheme",
    "message": "URL \"javascript:x\" uses a scheme that is not allowed; write a relative URL or one of: http, https, mailto, tel" },
  { "path": "view.children[2].value",
    "code": "constela.unknown_ref",
    "message": "no state field named \"nope\" is declared" }
]
```

The codes are part of the public surface (`autumn_web::constela::codes`) and
change with the same care as a function signature.

## Making it interactive

**Autumn ships no Constela client runtime, and generates no JavaScript.** What
it does instead fits the framework it is in — the interaction loop runs on the
server, over htmx.

An event binding renders as data attributes describing what the document asked
for:

```json
{ "kind": "element", "tag": "button",
  "props": { "onClick": { "event": "click", "action": "increment" } } }
```

```html
<button data-constela-on-click="increment"></button>
```

`Document::dispatch` runs an action's **pure** state steps — `set`, `update`,
`setPath`, `if` — on the server:

```rust
let mut state = session_state(&session);
let outcome = document.dispatch("increment", &mut state, &Map::new())?;
```

Put the two together and you have a working UI with no client-side interpreter
at all: post the action name, dispatch it, re-render, swap the fragment.

The renderer puts no `hx-*` attributes on the document's own elements — those
elements belong to the document, not to your app. Your app owns the wrapper, and
one delegated listener turns any `data-constela-on-click` into a post. That
listener is the only client-side code involved, and you write it once:

```js
// static/js/constela-bridge.js — served from your own origin. No eval, CSP-safe.
document.addEventListener("click", (event) => {
  const el = event.target.closest("[data-constela-on-click]");
  if (!el) return;
  const values = { action: el.dataset.constelaOnClick };
  if (el.dataset.constelaPayloadClick) values.payload = el.dataset.constelaPayloadClick;
  htmx.ajax("POST", "/panel/action", { target: "#panel", swap: "outerHTML", values });
});
```

```rust
use autumn_web::constela::{Document, Limits, RenderContext};
use autumn_web::prelude::*;
use serde_json::{Map, Value};

/// The htmx target. Re-rendered on every interaction.
fn panel(document: &Document, state: &Map<String, Value>) -> AutumnResult<Markup> {
    let ctx = RenderContext {
        state: state.clone(),
        ..RenderContext::default()
    };
    let body = document.render(&ctx)?.body;
    Ok(html! { div id="panel" { (body) } })
}

#[post("/panel/action")]
async fn act(session: Session, Form(form): Form<ActionForm>) -> AutumnResult<Markup> {
    let document = load_document(&session)?;
    let mut state = session_state(&session);

    // `dispatch` refuses an action the document does not declare, so the
    // action name arriving from the browser needs no allowlist of its own.
    document.dispatch(&form.action, &mut state, &form.payload())?;

    save_state(&session, &state);
    panel(&document, &state)
}
```

State lives wherever your app wants it — a session, a row, a cache entry. It is
a plain `serde_json::Map`, and a document never gets to touch anything else.

### Effects are reported, not performed

The browser-side steps — `fetch`, `storage`, `navigate`, `delay`, `interval`,
`focus` — are **not** executed by `dispatch`. They come back on
`Dispatched::effects` with their operands already evaluated, and the app decides
what, if anything, to do with them.

`fetch` is the clear reason why. Running a model-authored URL from inside your
app would give a prompt injection your app's own network position: SSRF by
construction, reaching whatever your app can reach. The app is the only party
that knows which hosts a generated document should be allowed to talk to, so the
app makes that call:

```rust
for effect in outcome.effects {
    match effect {
        Effect::Fetch { url, .. } if my_allowlist.permits(&url) => proxy(&url).await?,
        Effect::Navigate { url, .. } => return Ok(Redirect::to(&url).into_response()),
        other => tracing::debug!(?other, "declined effect from a generated document"),
    }
}
```

When an effectful step is reached, **execution stops** — neither its nested
`onSuccess`/`onError` branches nor any later step in the action is run, and
`Dispatched::suspended_at` names where it stopped.

That is a correctness requirement, not caution. An effect can bind a `result`
that later steps read; the server did not perform the effect, so it has no value
to bind, and running on would evaluate those reads as `null` and commit the
answer to state:

```json
{ "do": "fetch", "url": {"expr": "lit", "value": "/api"}, "result": "res" },
{ "do": "set", "target": "data", "value": {"expr": "var", "name": "res"} }
```

Continuing past the `fetch` would write `null` over whatever `data` held. So
dispatch stops, hands back the effects it reached, and leaves the rest of the
action to whoever performs them.

## Limits

Two independent sets, because they bound different things.

`Limits` bounds the **document**, and applies before and around parsing:

| Field | Default | Why |
| --- | --- | --- |
| `max_bytes` | 512 KiB | Rejected before the JSON parser runs at all. |
| `max_depth` | 64 | Checked on the `Value` tree, *before* the typed deserialization that recurses once per level — so a document engineered to overflow the stack is rejected before it can. |
| `max_nodes` | 20 000 | Bounds the walk itself. |

`RenderLimits` bounds the **expansion** of that document against runtime state.
A 2 KB document that loops over a 100 000-element list is small and expensive:

| Field | Default |
| --- | --- |
| `max_depth` | 128 |
| `max_nodes` | 50 000 |
| `max_each_items` | 5 000 |
| `max_output_bytes` | 4 MiB |

`max_output_bytes` is not implied by the node or iteration counts, which is why
it is its own budget: a *single* text node can emit as much as
`RenderContext::state` holds, and the app owns that. A 5 000-iteration `each`
over a large string is a few dozen nodes, inside every other bound, and
gigabytes of markup. Counting nodes does not see that; counting bytes does. The
budget spans the body **and** every portal together, and it also caps what a
single expression may *build* — `concat`, `array`, `obj` and string `+` assemble
a finished value inside the evaluator before any of it reaches the output
buffer, so bounding only the output would leave the allocation unbounded.

`max_depth` caps the shape of state as well as the depth of the view. Every
state mutation is checked, not only `setPath`: state persists between
dispatches, so an action as ordinary as `set x = array(state x)` adds a nesting
level *per request*, and `serde_json::Value` drops recursively — an unbounded
version overflows the stack eventually, on a request that did nothing unusual.

`max_depth` also caps how deep a `setPath` step may write. That one is worth
knowing about, because the reason is not the obvious one: the walk over a long
path is iterative and would cope fine, but the *structure* it creates would not.
`serde_json::Value` drops recursively, so a document that wrote two hundred
thousand levels down would overflow the stack whenever that state was next
freed — a crash with no visible connection to the request that built it.
Refusing the write is the only fix that holds, and no real UI addresses a field
that deep.

Tighten any of these for a document arriving on an untrusted public endpoint.

## The supported subset

Autumn implements the part of the upstream AST a *server* can be faithful to.

**Expressions:** `lit`, `state`, `var`, `param`, `bin`, `not`, `cond`, `get`,
`index`, `concat`, `array`, `obj`, `route`, `style`.

**View nodes:** all twelve — `element`, `text`, `if`, `each`, `component`,
`slot`, `markdown`, `code`, `portal`, `island`, `suspense`, `errorBoundary`.

**State types:** `number`, `string`, `list`, `boolean`, `object`.

**Action steps:** `set`, `update`, `setPath`, `if` (run on the server), plus
`fetch`, `storage`, `navigate`, `delay`, `interval`, `focus` (reported as
effects).

Deliberately absent, and rejected at parse time with a message naming what *is*
supported: `call` and `lambda` (need a function registry), `ref` and `validity`
(need a live DOM), `import` and `data` (need a build-time loader), and `local`
(component-local state). A document using one fails loudly rather than rendering
something subtly different from what it asked for.

Some node kinds are rendered but cannot mean server-side quite what they mean in
a browser, and it is worth knowing which:

- `suspense` and `errorBoundary` render their **content**. Server-side nothing
  is pending and nothing has thrown. The fallbacks are validated and carried, for
  a client runtime to use.
- `island` renders its content inside a wrapper carrying the requested hydration
  strategy as data attributes. Hydrating it is the host app's business.
- `portal` content is **not** spliced into the body. It names a destination in
  the host page, which only your layout knows how to reach, so it comes back on
  `RenderedUi::portals` for you to place — or drop.
- `markdown` requires the `markdown` feature. Without it, a document containing
  one fails validation with `constela.feature_required` rather than silently
  rendering nothing.
- `code` renders an escaped `<pre><code class="language-…">`. There is no
  server-side syntax highlighting; the language hint is reduced to characters
  that are safe in a class name and emitted for a highlighter of your choosing
  to pick up.
- An `each`'s `key` is parsed and validated but unused. A key exists to let a
  client runtime reconcile a list across re-renders; a server render has
  nothing to reconcile against.
- A component may contain **at most one** `slot`. The AST gives no way to say
  which children fill which named slot, so rather than pick a rule — duplicate
  the children into every slot, or silently fill only the first — a second slot
  is rejected.

### Evaluation semantics

Expressions follow upstream's JavaScript semantics exactly, so a document
rendered here and the same document rendered by the upstream client runtime
agree: `"a" + 1` is `"a1"`, `[1,2] + ""` is `"1,2"`, `==` is `===` (no type
juggling), `&&` and `||` yield the *operand* rather than a boolean, `[]` and `{}`
are truthy, and `<` on non-numbers compares stringified operands.

Number formatting matches too, thresholds included: ECMAScript switches to
exponential notation at `|x| >= 1e21` and again below `1e-6`, so `1e21` renders
as `1e+21` and `1e-7` as `1e-7` rather than as long decimal expansions.

Two things differ, both forced by JSON rather than chosen:

- **Division and remainder by zero yield `null`**, which renders as the empty
  string. JavaScript yields `NaN` and `±Infinity`, and JSON has no way to spell
  either.
- **`==` compares objects and arrays structurally**, where JavaScript's `===`
  compares them by reference. A value-based evaluator has no references to
  compare, and structural equality is the only total answer available. Scalars,
  which is what almost every document compares, behave identically — including
  numbers, where a computed `1 + 2` equals the literal `3`.

A `state`, `var`, `param` or `route` reference that resolves to nothing yields
`null` — JavaScript's `undefined` — rather than failing the render. By the time a
document renders, validation has already rejected every reference that
*statically* cannot resolve, so a null there means a value genuinely absent at
runtime, such as an optional query parameter.

One asymmetry is worth spelling out. In a **view**, a `var` naming nothing an
enclosing `each` binds is an error: nothing else can bind one there, so it is a
typo. In an **action**, it is not: an action's `var`s come from the payload it
was dispatched with, and that payload is chosen by the caller. The exception is
a name the action itself binds with a *later* step's `result` — that is
unambiguously an ordering mistake, and it is caught.

## See also

- [Rich text](rich-text.md) — the other untrusted-content path, and the
  allowlist this one is modelled on.
- `autumn_web::constela::policy` — the tag, attribute and URL allowlists.
- `autumn/tests/integration/constela.rs` — the adversarial corpus.
