# Chapter 6: Styling with Tailwind CSS

**Goal:** By the end of this chapter, your todo app will have a polished visual
design using Tailwind utility classes, and you will understand how the
`build.rs` CSS pipeline works.

---

## Sections

### How the Build Pipeline Works

The compile-time flow: `build.rs` runs the Tailwind CLI, which scans
`src/**/*.rs` for class names in string literals, generates only the CSS you
use, and writes `static/css/autumn.css`. Autumn serves this file from
`/static/css/autumn.css`.

### Adding Tailwind Classes to Maud Templates

Classes in Maud are just string attributes: `class="bg-gray-100 min-h-screen"`.
Styling the layout, the todo list, form inputs, and buttons.

### Responsive Design Basics

Using Tailwind's responsive prefixes (`sm:`, `md:`, `lg:`) to make the
layout work on different screen sizes.

### The Static File Serving Pipeline

How Autumn auto-mounts the `static/` directory at `/static/`. The CSS link
in the layout, the htmx script tag, and adding custom assets.

### Custom CSS

Adding your own styles below the `@tailwind` directives in `input.css` for
anything Tailwind does not cover.

### Checkpoint

Expected project state with styled templates.

---

> **Not written yet.** This chapter's narrative doesn't exist yet. For the
> actual Tailwind config and classes this chapter's goal describes, see
> [`examples/todo-app/tailwind.config.js`](../../../examples/todo-app/tailwind.config.js)
> and the class names used throughout
> [`examples/todo-app/src/routes/todos.rs`](../../../examples/todo-app/src/routes/todos.rs)
> — that's the reference implementation this tutorial builds toward. The
> Getting Started guide's
> ["Style with Tailwind CSS"](../getting-started.md#style-with-tailwind-css)
> section explains how the `build.rs` pipeline works in general, which
> applies here without adaptation. Continue to
> [Chapter 7 — Interactivity with htmx](07-htmx.md) once yours is styled.

---

Previous: [Chapter 5 — HTML Templates with Maud](05-templates.md) | Next: [Chapter 7 — Interactivity with htmx](07-htmx.md)
