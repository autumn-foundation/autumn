# Autumn Wiki Example

A small wiki showing how Autumn's mutation hooks, generated repositories, and
Markdown documentation primitives fit together when you need lifecycle logic,
revision history, and a JSON API without hand-writing the CRUD boilerplate
twice.

## What it demonstrates

- `#[model]` for `Page` and `Revision`
- `#[repository(Page, hooks = PageHooks, api = "/api/v1/pages")]`
- Mutation hooks for slug generation and revision auditing
- **`#[state_machine]` with transition effects** — `Page::status`
  (`draft → published → archived`) declares a `can_publish` guard plus per-edge
  `on = "..."` effects that append the audit `Revision` inside the transition's
  transaction; the `POST /pages/{slug}/transitions/status` handler drives the
  effectful `transition_status_to_on_conn` under one `Db::tx_with` (see
  `docs/guide/state-machines.md` and `docs/guide/transition-effects.md`)
- Maud templates for a server-rendered editing flow
- Embedded migrations on startup
- Framework health and actuator endpoints
- **Markdown docs with SSG** — `autumn_web::markdown` registry, `#[static_get]`
  pre-rendering, and embedded `.md` content files (see `src/routes/docs.rs`)
- **Nested `has_many` forms** — the Collections feature edits a parent record
  and its child links in one master–detail form via `NestedChangesetForm`, saved
  atomically (see `src/routes/collections.rs` and `docs/guide/nested-forms.md`)

## Prerequisites

- Rust 1.88.0+
- PostgreSQL (via Docker Compose below)

## Quick start

From the workspace root:

```bash
# 1. Download Tailwind CSS
cargo run -p autumn-cli -- setup

# 2. Start Postgres
docker compose -f examples/wiki/docker-compose.yml up -d

# 3. Run the app
cargo run -p wiki
```

Open <http://localhost:3000>.

## Hook behavior

`PageHooks` keeps the interesting invariants in one place:

- `before_create` slugifies the title and fills in a default `"draft"` status
- `before_update` re-slugifies when the title changes

That means the UI routes and the generated REST API both get the same lifecycle
behavior automatically.

## Routes

### HTML

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | List all pages |
| GET | `/new` | New page form |
| POST | `/pages` | Create a page |
| GET | `/pages/{slug}` | View a page |
| GET | `/pages/{slug}/edit` | Edit form |
| POST | `/pages/{slug}` | Update a page |
| POST | `/pages/{slug}/transitions/status` | Apply a `#[state_machine]` status transition (fires the audit-`Revision` effect) |
| GET | `/pages/{slug}/history` | Full revision history |
| GET | `/collections` | List link collections |
| GET | `/collections/new` | New collection form (nested `has_many`) |
| POST | `/collections` | Create a collection + its links atomically |
| GET | `/collections/{id}` | View a collection |
| GET | `/collections/{id}/edit` | Edit form (seeded child rows) |
| POST | `/collections/{id}` | Update a collection + replace its links |

### JSON API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/pages` | List pages |
| GET | `/api/v1/pages/{id}` | Fetch one page |
| POST | `/api/v1/pages` | Create a page |
| PUT | `/api/v1/pages/{id}` | Update a page |
| DELETE | `/api/v1/pages/{id}` | Delete a page |

### Docs (Markdown + SSG)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/docs` | Documentation index (all pages sorted by `order`) |
| GET | `/docs/{slug}` | Rendered Markdown page with TOC |

The docs routes use `autumn_web::markdown`:

```rust
// ~10 lines of glue; layout markup excluded
static REGISTRY: OnceLock<MarkdownRegistry> = OnceLock::new();

fn docs() -> &'static MarkdownRegistry {
    REGISTRY.get_or_init(|| {
        MarkdownRegistry::from_embedded(&[
            MarkdownSource { slug: "getting-started", content: include_str!("../../content/getting-started.md") },
            MarkdownSource { slug: "configuration",   content: include_str!("../../content/configuration.md") },
        ]).expect("valid embedded docs")
    })
}

pub async fn doc_params(_router: axum::Router) -> Vec<StaticParams> {
    docs().static_params()
}

#[static_get("/docs/{slug}", params = doc_params)]
pub async fn show(Path(slug): Path<String>) -> AutumnResult<Markup> { ... }
```

Pre-render to `dist/` with:

```bash
cargo run -p autumn-cli -- build -p wiki
```

### Framework

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health probe |
| GET | `/actuator/health` | Detailed health view |
| GET | `/actuator/info` | Build and runtime metadata |
| GET | `/actuator/metrics` | Request and pool metrics |
| GET | `/static/js/htmx.min.js` | Bundled htmx |
| GET | `/static/css/autumn.css` | Compiled Tailwind output |
