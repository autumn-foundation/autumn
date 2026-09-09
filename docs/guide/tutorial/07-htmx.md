# Chapter 7: Interactivity with htmx

**Goal:** By the end of this chapter, you will be able to toggle a todo's
completion status and delete a todo without a full page reload, using htmx
attributes and server-rendered HTML fragments.

---

## Sections

### What htmx Does

htmx sends AJAX requests triggered by HTML attributes and swaps the response
(an HTML fragment) into the DOM. No JavaScript to write. Autumn bundles htmx
and serves it at `/static/js/htmx.min.js`.

### Why Autumn Chose htmx

Server-rendered HTML with sprinkles of interactivity. No client-side
framework, no build step for JS, no JSON serialization/deserialization for
UI interactions. The server stays the source of truth.

### Toggle: `hx-post` and Partial Responses

Adding `hx-post="/todos/{id}/toggle"` to the checkbox button. The server
handler toggles the `completed` flag, re-renders the single `<li>` item, and
returns it. htmx swaps the old `<li>` with the new one via `hx-target` and
`hx-swap="outerHTML"`.

### Delete: `hx-delete` and Empty Responses

Adding `hx-delete="/todos/{id}"` to the delete button. The server deletes
the record and returns an empty string. htmx replaces the element with
nothing, removing it from the page.

### Falling Back to Plain HTML Forms

htmx is the enhancement, not the requirement. Native browser forms can
only submit `GET` or `POST`, so to reach a `#[delete]` route without
JavaScript the form submits a hidden `_method` field that Autumn rewrites
into the declared HTTP method before route matching:

```rust,ignore
use autumn_web::form::method_input;
use autumn_web::security::CsrfToken;

#[get("/todos/{id}/edit")]
async fn edit_form(csrf: CsrfToken) -> Markup {
    html! {
        form method="post" action="/todos/42" {
            (method_input("DELETE"))
            input type="hidden" name="_csrf" value=(csrf.token());
            button { "Delete" }
        }
    }
}
```

A few things to know:

- The override is honoured only on same-origin form submissions
  (content-type `application/x-www-form-urlencoded`). `X-HTTP-Method-Override`
  headers are intentionally not enabled by default — the convention is for
  browser HTML, not REST tunneling.
- An unrecognised override value (anything other than `PUT`, `PATCH`,
  `DELETE`, case-insensitive) is rejected with `400 Bad Request` before
  reaching the handler.
- CSRF protection still treats the transport `POST` as unsafe. An
  overridden `DELETE` without a valid `_csrf` token is rejected with
  `403 Forbidden`, exactly like any other mutating POST.
- `autumn routes` and `/actuator/graph` continue to list the declared
  method (`PUT`, `PATCH`, `DELETE`) — route listings stay semantically
  honest regardless of the transport browsers used.

For `ChangesetForm` users, the bundled `form_tag` helper does this
automatically when given a non-GET/POST method:

```rust,ignore
form.form_tag("/todos/42", "delete", html! { button { "Delete" } })
```

renders `<form method="post">` with the hidden `_method` and `_csrf`
inputs already in place.

### Understanding `hx-target` and `hx-swap`

How htmx knows which element to update and how to update it. The `outerHTML`
swap strategy vs. `innerHTML`.

### The Fragment Pattern

Returning partial HTML (a single `<li>`, not a full page) from htmx
endpoints. Why this works and how it differs from SPA patterns.

### Checkpoint

Expected project state with interactive toggle and delete.

---

*Content coming in Sprint 12.*

---

Previous: [Chapter 6 — Styling with Tailwind CSS](06-tailwind.md) | Next: [Chapter 8 — JSON API](08-json-api.md)
