# Tutorial: Build a Todo App with Autumn

This tutorial walks you through building a complete todo application from an
empty directory to a working full-stack app. You will use every major feature
of the Autumn framework: route macros, Diesel database access, Maud HTML
templates, Tailwind CSS styling, htmx interactivity, a JSON API, and
structured error handling.

The app you build here is the same one in `examples/todo-app/`. The tutorial
is the narrative; the example is the reference implementation. If you get
stuck, compare your code against the example.

## Prerequisites

- **Rust** (edition 2024) with `cargo` on your PATH
- **Docker** and **Docker Compose** (for Postgres, starting in Chapter 3)
- **~2 hours** for the full tutorial, or work through chapters at your own pace

## Chapters

1. [Project Setup](01-project-setup.md) — scaffold a project, run hello-world
2. [Routes and Handlers](02-routes.md) — define endpoints with macros
3. [Database Setup](03-database.md) — Postgres, Docker, Diesel migrations
4. [Models and Queries](04-models.md) — Diesel models, the `Db` extractor, CRUD
5. [HTML Templates with Maud](05-templates.md) — type-safe HTML rendering
6. [Styling with Tailwind CSS](06-tailwind.md) — the `build.rs` CSS pipeline
7. [Interactivity with htmx](07-htmx.md) — toggle, delete, partial responses
8. [JSON API](08-json-api.md) — `Json<T>` for request and response
9. [Error Handling](09-errors.md) — `AutumnResult`, status codes, validation
10. [Configuration and Production Defaults](10-configuration.md) — `autumn.toml`, env vars, logging
11. [Writing Integration Tests](11-testing.md) — `TestApp`, `TestDb`, smoke tests and DB round-trips
12. [What's Next](12-whats-next.md) — extending the app, further reading

> **Going multilingual?** When you finish the tutorial, see the
> [i18n guide](../i18n.md) for the opt-in Project Fluent integration —
> file convention, `Locale` extractor, and the `t!()` macro.

> **Accepting third-party callbacks?** See the
> [signed webhook guide](../signed-webhooks.md) for Stripe/GitHub/Slack-style
> HMAC verification and replay protection.

## How to Use This Tutorial

Each chapter builds on the previous one. Start from Chapter 1 and work
forward. Every chapter opens with a goal statement telling you what you will
have by the end, and closes with a checkpoint showing the expected project
state.

Chapters 2, 3, 5, 6, and 8, and part of Chapter 7, don't have their own
narrative yet — each one links to the equivalent, already-working section of
the [Getting Started guide](../getting-started.md) instead, so you can still
go end to end without a dead stop.

Code listings show only the new or changed code. When you need to see the
full file at any point, check the corresponding checkpoint or refer to
`examples/todo-app/`.

If you have already read the [Getting Started guide](../getting-started.md),
you can skim Chapter 1 — it covers similar ground but establishes the project
you will build on for the rest of the tutorial.

## Want the short version?

If you're already comfortable with Rust web frameworks and just want a
working CRUD app, the [Code Generators guide](../generators.md) collapses
this whole tutorial into five commands:

```bash
autumn new my-app
cd my-app
autumn generate scaffold Post title:String body:Text published:bool
autumn migrate
autumn dev
```

The tutorial explains *why* each piece exists; the generators guide
shows *how* to skip the typing once you know.
