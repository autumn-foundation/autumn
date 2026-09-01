# SSG: record the intended `Content-Type` in the static manifest (#1832)

## Problem restated

`autumn/src/router.rs`'s static-first middleware derives each cached response's
`Content-Type` at *request* time, from the request route plus the served on-disk
file name. `static_gen::url_to_file_path` collapses every non-root route to
`<route>/index.html`, so the route's semantic type is thrown away at generation
time and reverse-engineered at serve time. Review of #1819 needed three
consecutive corrections to that heuristic (fonts, generated `.txt`/`.xml`
routes, dotted-slug HTML pages) — three symptoms of one root cause.

## Planning

### Brainstorming — candidate solutions

| # | Idea | Verdict |
|---|------|---------|
| A | Record `content_type` per route in `ManifestEntry`; serve reads it. | **Chosen.** Removes the guess entirely; O(1) at serve time. |
| B | Change `url_to_file_path` to keep the route's real extension (`/robots.txt` → `robots.txt`). | Rejected. Breaks the `dir/index.html` convention every static host expects, and still cannot classify `/posts/release.v1` (dot, but HTML). |
| C | Sidecar `.meta` file per generated page. | Rejected. One extra `stat`+read per request for data that fits in the manifest we already load once at startup. |
| D | Record the whole response header map per route (Cache-Control, Link, …). | Deferred. A strict superset of A; larger surface and a header-forwarding security question. A is the slice #1832 asks for; D can build on the same field later. |
| E | Precompute the heuristic once at layer construction instead of per request. | Rejected. Still the same heuristic — moves the bug, does not remove it. |
| F | Sniff the file bytes at serve time. | Rejected. Content sniffing is exactly what `X-Content-Type-Options: nosniff` exists to stop. |

### Reverse brainstorming — how would we *break* this?

| Failure mode we invented | Countermeasure in the design |
|---|---|
| Hand-edited/corrupt manifest carries a header-illegal value (`"text/html\r\nX: y"`, non-visible-ASCII) and the serve path's `.expect("infallible response builder")` **panics** — a request-path panic. | Serve path parses the recorded value with `HeaderValue::from_str` and falls back to the derivation on failure. It builds a `HeaderValue`, never a raw `&str`, so the builder cannot fail. |
| Header injection via CRLF in a recorded type. | Same: `HeaderValue::from_str` rejects CR/LF. Generation only records values that were already valid `HeaderValue`s. |
| Empty string recorded → empty `Content-Type` header. | Empty is treated as "not recorded" and falls back. |
| An existing `dist/` built by an older Autumn has no field → every route 500s or serves octet-stream. | `#[serde(default)]`: the field is `Option<String>`, absent ⇒ `None` ⇒ the existing heuristic runs, byte-for-byte as today. The six #1819 serve-time tests are kept and become the legacy-manifest regression suite. |
| A new manifest read by an older Autumn binary. | serde ignores unknown fields by default; older runtimes keep deriving. Forward-compatible. |
| A handler that declares no `Content-Type` at all gets a *guess* baked into the manifest permanently. | Generation records `None` in that case rather than guessing, leaving serve-time fallback untouched. Only a type the handler actually declared is stored. |
| ISR regenerates a route whose handler now declares a different type; the manifest still says the old one. | The type is a build-time property and ISR re-runs the same handler, so drift means the app changed without a rebuild. `regenerate_page` compares and `warn!`s so it is visible instead of silent. |
| Adding a public field breaks downstream struct literals. | Pre-1.0, and called out in the changelog; `ManifestEntry::new` / `with_revalidate` / `with_content_type` are added so downstream code has a non-breaking construction path going forward. |
| Recorded type silently changes compression behaviour. | Tests assert the compression outcome (`Content-Encoding`) alongside the type for both recorded and derived paths. |

### Six hats

- **White (facts).** The manifest is JSON, loaded once at startup into an `Arc`.
  `assets::content_type_for_opt` already distinguishes "recognized asset
  extension" from "no idea". Six serve-time tests in `router.rs` pin the current
  heuristic. `render_static_routes` holds the full `Response` — headers included
  — before it reads the body, so the declared type is free to capture.
- **Red (instinct).** A 40-line comment block justifying a heuristic is the
  smell. Storing the answer where it is known feels obviously right; the only
  discomfort is that routes whose handler returns a bare `String` will now be
  served as the `text/plain` axum actually declared instead of the `text/html`
  the heuristic assumed. That is the dynamic path's behaviour, so agreement is
  the improvement, but it belongs in the changelog.
- **Black (risks).** Request-path panic on a bad value; silent breakage of
  already-built `dist/` dirs; public-API break; ISR drift. All addressed above.
- **Yellow (upside).** The three #1819 edge cases become impossible by
  construction. Types the heuristic could *never* produce — `application/rss+xml`,
  `application/manifest+json`, `text/calendar` — now round-trip. One place to
  reason about instead of two.
- **Green (creative).** Put the decision in one named, directly unit-testable
  function next to the manifest (`resolved_content_type`) rather than inline in
  the router closure, and return a `HeaderValue` so the "cannot panic" property
  is structural rather than a comment.
- **Blue (process).** Strict red → green → refactor, in four slices: manifest
  type, generation, serve-time selection, wiring. Every slice gets a test that
  fails first.

## Design

```rust
pub struct ManifestEntry {
    pub file: String,
    pub revalidate: Option<u64>,
    /// `Content-Type` the handler declared when this page was generated.
    /// `None` for legacy manifests and handlers that declared none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}
```

- `render_static_routes` reads `response.headers()[CONTENT_TYPE]`, keeps it when
  it is valid visible ASCII and non-empty, and stores it on the entry.
- `StaticFileLayer::resolve_entry` returns `ResolvedStatic { file_path,
  content_type }`; `resolve` stays as the file-path-only shorthand.
- `static_gen::resolved_content_type(recorded, route, file) -> HeaderValue`
  is the single decision point: recorded-and-valid wins, otherwise the existing
  route-extension → file-name derivation, otherwise `application/octet-stream`.
- `router.rs` loses its heuristic block and calls that function.

## TDD slices

1. **Red:** manifest round-trip preserves `content_type`; a legacy JSON blob with
   no field deserializes to `None`. **Green:** add the field.
2. **Red:** `render_static_routes` over a handler declaring
   `application/xml` records it; a handler declaring nothing records `None`.
   **Green:** capture the response header.
3. **Red:** `resolved_content_type` prefers the recorded value, falls back for
   `None`/empty/header-illegal input, and reproduces all three #1819 cases.
   **Green:** implement it.
4. **Red:** end-to-end through the router — a recorded `application/rss+xml`
   route is served as such (unreachable for the heuristic), and a manifest whose
   recorded value is header-illegal falls back instead of panicking.
   **Green:** wire the router to `resolve_entry` + `resolved_content_type`.
5. **Refactor:** delete the heuristic comment block from `router.rs`, document
   the new field in `docs/design/hybrid-rendering.md`, add the ISR drift warning,
   changelog.
