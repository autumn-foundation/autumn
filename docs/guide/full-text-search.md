# Postgres Full-Text Search (FTS) in Autumn

Autumn provides first-class, high-performance Postgres Full-Text Search (FTS). Through declarative model-level annotations and automated CLI migration generators, Autumn configures database-level stored `tsvector` generated columns, attaches high-throughput `GIN` indexes, and exposes typed, paginated, and relevance-ranked search interfaces directly on your repositories.

---

## FTS vs. External Search Engines

When choosing a search architecture, it is essential to balance infrastructure complexity, transaction guarantees, and scaling needs.

| Dimension | Postgres FTS (Autumn Core) | Meilisearch | Elasticsearch / OpenSearch | Vector / Semantic Search |
| :--- | :--- | :--- | :--- | :--- |
| **Infrastructure** | Zero extra dependencies (Postgres) | Dedicated search server | Large JVM cluster | Vector DB or vector PG extension |
| **Consistency** | Strong (Updates inside ACID transaction) | Eventual (Async index queue) | Eventual (Refresh latency) | Eventual (Embedding/index sync) |
| **Setup Cost** | Low (Annotations + Migration) | Medium (Docker + Sync logic) | High (Mappings + Sync pipeline) | High (Embeddings + Model serving) |
| **Search Types** | Keyphrase, prefix, and wildcard | Typo-tolerant lexical search | Advanced lexical, synonym grids | Conceptual / Semantic matching |
| **Scale Target** | Up to ~10M records / 100GB | Medium-Large datasets | Multi-Terabyte / Enterprise | Neural semantic datasets |

### When to use Postgres FTS:
- You want **ACID transactional consistency**: searches must instantly reflect the most recent database inserts or updates.
- You want **zero operational complexity**: you wish to avoid managing, provisioning, and securing Elasticsearch/Meilisearch clusters.
- Your corpus is **under 10-20 million records** where Postgres GIN indexes can reside comfortably inside RAM.

---

## 1. Declaring FTS on Models

To mark a model as searchable, apply the `#[searchable]` attributes.

```rust
#[autumn_web::model]
#[searchable(language = "english")] // FTS dictionary/configuration (default is "simple")
pub struct Page {
    #[id]
    pub id: i64,
    
    #[searchable(weight = "A")] // High relevance
    pub title: String,
    
    pub slug: String,
    
    #[searchable(weight = "B")] // Medium relevance
    pub body: String,
}
```

### Language Dictionaries
- `simple`: Default. Language-neutral dictionary that parses words into lowercased tokens without stemming. Ideal for general code, tags, IDs, or multi-language columns.
- `english` / `german` / etc.: Applies linguistic dictionaries that perform stemming (e.g. mapping "programming", "programs", and "programmer" to the stem "program") and strip common stop words ("the", "and").

### Field Weights
Relevance weights are mapped from highest (`A`) to lowest (`D`):
- `A` (Weight: 1.0) — Recommended for titles, headings, and short primary fields.
- `B` (Weight: 0.4) — Recommended for summaries, categories, or high-priority descriptions.
- `C` (Weight: 0.2) — Recommended for general content body.
- `D` (Weight: 0.1) — Recommended for auxiliary metadata or comments.

---

## 2. Enabling Repository APIs

To generate search methods on your repository, add the `searchable` parameter:

```rust
#[autumn_web::repository(Page, api = "/api/v1/pages", searchable)]
pub trait PageRepository {}
```

This automatically generates the following asynchronous signatures on the repository trait:

```rust
fn search(&self, query: &str) -> impl Future<Output = AutumnResult<Vec<Page>>> + Send;
fn search_page(
    &self,
    query: &str,
    req: &PageRequest,
) -> impl Future<Output = AutumnResult<Page<Page>>> + Send;
```

### Search Features:
1. **Relevance Ranking**: Results are dynamically sorted using `ts_rank_cd(search_vector, query) DESC` and secondary key `id DESC` for stable pagination.
2. **Websearch Operators**: Queries are parsed through `websearch_to_tsquery`, supporting:
   - Quoted phrases: `"rust programming"` (exact match)
   - Exclusions: `database -mysql` (excludes mysql)
   - Logical OR: `Go OR Postgres`
3. **Tenancy Boundaries**: Scoped automatically when `tenant_scoped` is enabled, matching your multi-tenancy configuration.
4. **Performance Short-Circuiting**: Empty or whitespace-only queries immediately bypass database execution, returning empty vectors.

---

## 3. Idempotent Database Migrations

Autumn’s migration planner detects FTS additions. When you run `autumn generate migration AddSearchToPages`, the scaffolder:
1. Locates `src/models/page.rs`.
2. Inspects your `#[searchable]` weights and language properties.
3. Generates the corresponding stored column and indexing SQL:

### Generated `up.sql`:
```sql
-- autumn-safety: potentially-blocking
-- adding stored generated column will backfill existing rows
ALTER TABLE pages ADD COLUMN search_vector tsvector GENERATED ALWAYS AS (
    setweight(to_tsvector('english'::regconfig, coalesce(title, '')), 'A') || 
    setweight(to_tsvector('english'::regconfig, coalesce(body, '')), 'B')
) STORED;

CREATE INDEX idx_pages_search_vector ON pages USING gin(search_vector);
```

### Generated `down.sql`:
```sql
DROP INDEX IF EXISTS idx_pages_search_vector;
ALTER TABLE pages DROP COLUMN IF EXISTS search_vector;
```

---

## 4. Zero-Downtime Production Deployment

In PostgreSQL, adding a stored generated column via `ALTER TABLE ... ADD COLUMN ... GENERATED ALWAYS AS (...) STORED` performs a full table rewrite and holds a strict `ACCESS EXCLUSIVE` lock on the table. On large, high-traffic production tables, this blocks all read and write queries, causing significant downtime.

To perform a **true zero-downtime deployment** on large datasets, you must decouple column creation, index generation, and backfilling:

### Step 1: Add a Plain Nullable `tsvector` Column
Adding a nullable column with no default values takes a metadata-only lock that executes instantaneously, avoiding a table rewrite:
```sql
ALTER TABLE pages ADD COLUMN search_vector tsvector;
```

### Step 2: Set Up an Automated DB Trigger
Create a trigger to automatically calculate the search vector on all future `INSERT` and `UPDATE` operations:
```sql
CREATE OR REPLACE FUNCTION pages_search_vector_trigger() 
RETURNS trigger AS $$
BEGIN
  new.search_vector := 
    setweight(to_tsvector('english'::regconfig, coalesce(new.title::text, '')), 'A') || 
    setweight(to_tsvector('english'::regconfig, coalesce(new.body::text, '')), 'B');
  RETURN new;
END
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_pages_search_vector_update
  BEFORE INSERT OR UPDATE ON pages
  FOR EACH ROW EXECUTE FUNCTION pages_search_vector_trigger();
```

### Step 3: Create the GIN Index Concurrently
Generate the GIN index in a separate transaction block using `CONCURRENTLY`. This builds the index in the background without blocking reads or writes:
```sql
CREATE INDEX CONCURRENTLY idx_pages_search_vector ON pages USING gin(search_vector);
```

### Step 4: Backfill Pre-Existing Rows in Batches
With the trigger handling new/updated rows and the index in place, safely backfill your historical data in small, controlled batches (e.g., 5,000 rows at a time) to prevent CPU and lock exhaustion:
```sql
-- Run repeatedly in the background until all rows are updated:
UPDATE pages 
SET search_vector = 
  setweight(to_tsvector('english'::regconfig, coalesce(title::text, '')), 'A') || 
  setweight(to_tsvector('english'::regconfig, coalesce(body::text, '')), 'B')
WHERE id IN (
  SELECT id FROM pages 
  WHERE search_vector IS NULL 
  LIMIT 5000
);
```

---

## 5. End-to-End HTMX Active Search Example

Using the framework's default Maud + HTMX capabilities, you can build a debounced, live-updating search input with zero custom JavaScript.

### Maud Template and Search Bar (Page List View)
```rust
pub fn search_bar_markup() -> Markup {
    html! {
        div class="mb-6 bg-white p-4 rounded shadow flex items-center" {
            input type="search" 
                  name="q" 
                  placeholder="Search pages..."
                  hx-get="/search" 
                  hx-trigger="keyup changed delay:300ms, search"
                  hx-target="#search-results" 
                  hx-indicator="#search-indicator"
                  class="flex-grow border rounded px-3 py-2 text-sm focus:ring-emerald-500";
            
            span id="search-indicator" class="htmx-indicator ml-3 text-sm text-gray-400 hidden" {
                "Searching..."
            }
        }
    }
}
```

### HTTP Router and Repository Implementation
```rust
#[derive(serde::Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: String,
}

#[get("/search")]
pub async fn search(
    repo: PgPageRepository,
    Query(params): Query<SearchParams>,
) -> AutumnResult<Markup> {
    let term = params.q.trim();
    let pages = if term.is_empty() {
        repo.find_all().await?
    } else {
        repo.search(term).await?
    };
    
    // Return ONLY the list HTML snippet representing the target element hx-target="#search-results"
    Ok(html! {
        ul id="search-results" class="space-y-3" {
            @for p in pages {
                li class="p-4 bg-white rounded shadow" {
                    a href=(paths::show(p.slug)) { (p.title) }
                }
            }
            @if pages.is_empty() {
                li class="text-gray-400 text-center py-8" { "No pages found." }
            }
        }
    })
}
```

---

## 6. SQLite FTS5

The same `#[searchable]` model annotation and the same generated repository
APIs (`search`, `search_page`, `search_page_scoped`) also work on the **SQLite**
backend (#2047). The `--search` / `#[searchable]` scaffold now generates on both
backends — you write your models the same way, and the generator emits a
backend-appropriate search index:

| | Postgres | SQLite |
| --- | --- | --- |
| Index | `tsvector` generated column + `GIN` index | external-content **FTS5** virtual table kept in sync by AFTER INSERT/DELETE/UPDATE triggers |
| Tokenizer | Postgres text-search config | `unicode61` (full-Unicode case/diacritic folding, so `äpfel` matches `Äpfel`) |
| Ranking | `ts_rank` | `bm25`, with per-column weights derived from `#[searchable(weight=…)]` priorities (A=10, B=5, C=2, else 1) |

Nothing in your application or repository call sites changes — the tenant,
soft-delete, and owner scoping predicates are applied identically on both
backends.

### Query safety (fail-closed)

On SQLite, the free-text query is passed through a **fail-closed,
injection-safe** sanitizer (`repository::sqlite_fts5_match_query`) before it ever
reaches FTS5. It tokenizes the input on Unicode whitespace and turns every token
into a literal quoted phrase — so every FTS5 operator (`AND` / `OR` / `NOT` /
`NEAR` / `col:` / `*` / parentheses / quotes / a leading `-`) is treated as a
**literal search term**, never as syntax. An empty or whitespace-only query (or
one that produces no tokens) yields an **empty result page with no query run** —
it never degrades into an unfiltered table scan.

### FTS5 is a hard boot dependency

FTS5 is not optional at runtime. `libsqlite3-sys` is built bundled with
`SQLITE_ENABLE_FTS5`, and each SQLite connection probes FTS5 availability during
setup. If FTS5 is missing, the app **fails loudly at boot** with an actionable
message rather than silently falling back to a slower `LIKE` scan — so a SQLite
search app is either correct or it does not start.

> SQLite full-text search is part of the single-host [SQLite in
> production](./sqlite-in-production.md) tier. For the scale-out reasons to
> prefer Postgres (read replicas, sharding, multi-replica scheduling), see the
> comparison table at the top of that guide.
