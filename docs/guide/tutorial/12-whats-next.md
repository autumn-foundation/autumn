# Chapter 12: What's Next

**Goal:** By the end of this chapter, you will have a complete, working todo
application and a clear picture of where to go from here.

This tutorial tracks Rust 1.88.0+ and whatever `autumn-cli` version
[Chapter 1](01-project-setup.md) told you to install. If you are following
`trunk` from a checkout instead, confirm the workspace version before
copying dependency snippets.

---

## Sections

### What You Built

Recap of the full application: project scaffolding, route macros, Postgres
with Diesel, Maud templates, Tailwind CSS styling, htmx interactivity, a
JSON API, structured error handling, and production configuration. All from
one framework dependency.

### Comparing to the Reference Implementation

Your code vs. `examples/todo-app/`. Any polish the example includes that the
tutorial omitted. Then branch out into the newer example apps:

- `examples/blog/` for hybrid rendering and a richer admin UI
- `examples/bookmarks/` for repository macros, generated CRUD APIs, and scheduled tasks
- `examples/wiki/` for mutation hooks and revision history
- `examples/reddit-clone/src/routes/webhooks.rs` for signed third-party callback intake and replay fixtures

### Ideas for Extending the App

- **Authentication** -- add user accounts with session cookies
- **Actuator hardening** -- tune which operational endpoints stay visible in prod
- **Background work** -- add a `#[scheduled]` task for cleanup or polling
- **Third-party callbacks** -- add signed Stripe/GitHub/Slack-style webhooks
- **Categories** -- associate todos with categories (a second table, foreign keys)
- **Search** -- add a search bar with `ILIKE` queries
- **Pagination** -- limit the list with `.limit()` and `.offset()`
- **Testing** — see [Chapter 11](11-testing.md) for the integration testing walkthrough

### Further Reading

- [API Reference](https://docs.rs/autumn-web) -- generated Rust docs for every public type
- [Getting Started Guide](../getting-started.md) -- quick overview of all features
- [Signed Webhook Intake](../signed-webhooks.md) -- raw-body HMAC verification and replay protection
- [Example App](../../../examples/todo-app/) -- the reference implementation
- [Autumn on crates.io](https://crates.io/crates/autumn-web) -- versioned releases

### Community

This project's home is its GitHub repository. Open an issue for bugs or
questions, and see [CONTRIBUTING.md](../../../CONTRIBUTING.md) before
sending a pull request.

---

> **Chapters 2, 3, 5, 6, and 8, and part of Chapter 7, aren't written yet** —
> see those chapters for pointers to the equivalent, already-working ground
> in the [Getting Started guide](../getting-started.md).

---

Previous: [Chapter 11 — Writing Integration Tests](11-testing.md) | Back to [Tutorial Index](index.md)
