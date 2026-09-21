# Chapter 5: HTML Templates with Maud

**Goal:** By the end of this chapter, you will have a proper HTML layout, a
styled todo list page, a todo detail page, and a form for creating new todos
— all rendered with Maud's type-safe Rust DSL.

---

## Sections

### Why Maud

Maud uses a Rust DSL instead of a separate template language. You get
compile-time checking, IDE support, and no runtime template parsing. Brief
comparison with Tera/Askama for context.

### Maud Syntax Primer

Tags, attributes, text content, conditionals (`@if`/`@else`), loops
(`@for`), and expressions. The `html!` macro and `Markup` return type.

### Building a Layout Function

Creating a `layout()` helper that wraps page content in a full HTML document
with `<head>`, CSS link, and htmx script tag.

### The Todo List Template

Rendering the list of todos with `@for`. Conditionally showing an empty
state message. The new-todo form with a text input and submit button.

### The Todo Detail Template

Showing a single todo with its status and creation date. A back-link to the
list.

### The `PreEscaped` Escape Hatch

Using `autumn_web::PreEscaped` for raw HTML (like `<!DOCTYPE html>`). When and
why to use it sparingly.

### Checkpoint

Expected project state with full Maud templates rendering database content.

---

> **Not written yet.** This chapter's narrative doesn't exist yet. The same
> ground — the same todo app, the same API — is already covered and tested in
> the Getting Started guide's
> ["Render HTML with Maud"](../getting-started.md#render-html-with-maud)
> section. Read that, then continue to
> [Chapter 6 — Styling with Tailwind CSS](06-tailwind.md). You can also check
> the finished code in [`examples/todo-app/`](../../../examples/todo-app/).

---

Previous: [Chapter 4 — Models and Queries](04-models.md) | Next: [Chapter 6 — Styling with Tailwind CSS](06-tailwind.md)
