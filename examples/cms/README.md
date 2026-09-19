# cms — a WordPress-core-parity content management system

A complete, runnable CMS built from Autumn's shipped primitives. Posts and
pages, categories and tags, a media library, threaded comments with a moderation
queue, revisions, menus, widgets, themes, roles and capabilities, a plugin hook
system, shortcodes, permalinks, feeds, a sitemap, a REST API, and import/export.

## Prerequisites

- Rust 1.88.0+
- PostgreSQL **12 or newer** — the `search_vector` column is a stored generated
  column, which Postgres 11 does not support. A `docker-compose.yml` pinning
  Postgres 16 is included for local development.
- Docker, for the full integration suite (it provisions Postgres itself).

## Quick start

```bash
docker compose up -d   # start Postgres
autumn migrate         # create the content schema
autumn dev             # run the app at http://localhost:3000
```

Register at `/register` — **the first account created owns the site**, so there
are no default credentials to leak — then everything else happens in `/admin`.

```bash
autumn task seed-demo  # optional: a starter post, page, menu and sidebar
```

### Success check

```bash
curl -s http://localhost:3000/register | grep -o "Create an account"
```

## WordPress-core parity

The stopping condition for this starter was parity with WordPress core, so here
is the honest scorecard.

### Covered

| WordPress core | Here |
|---|---|
| Posts, Pages, custom post types | one `posts` table keyed by `post_type`; types are registry entries (`content_types.rs`), not migrations |
| Categories, tags, custom taxonomies | `terms` + `post_terms`; `category` is hierarchical, `post_tag` flat |
| Post statuses | `draft · pending · publish · private · future · trash`, as a `#[state_machine]` — illegal edges are refused, not just discouraged |
| Scheduled publishing | `#[scheduled(every = "1m")]`, a real timer (see the wp-cron note below) |
| Revisions | snapshot before every edit, restore appends rather than rewinds, capped at 25 per post |
| Trash and restore | `→ trash` and `trash → draft/publish` edges |
| Media library | `attachments` + `BlobStore` (local disk or S3); MIME allowlist enforced server-side |
| Featured images | `posts.featured_media_id` |
| Excerpts | authored, or the first 55 words — WordPress's own default length |
| Custom fields | `post_meta` |
| Sticky posts | `posts.sticky`, pinned on the blog index |
| Password-protected posts | `posts.password`; body withheld from the page, the feed **and** the REST API |
| Comments | threaded, depth-capped, guest or account-backed |
| Comment moderation | `approved · pending · spam · trash` queue; `comment_count` counts approved only, as WordPress's does |
| Roles and capabilities | all five core roles, the core capability matrix, derived from the role at check time (`capabilities.rs`) |
| Users admin | list, create, change role, delete — with a guard against removing the last administrator |
| Settings | typed `Settings` struct over the `options` table, one place for defaults and validation |
| Permalinks | all five WordPress structures, and **changing the setting does not 404 existing URLs** |
| Menus | multi-level, assigned to theme locations, targeting posts/terms/URLs |
| Widgets and sidebars | five widget kinds, ordered, with per-instance settings |
| Themes | a `Theme` trait with defaulted methods; two ship, switchable from Settings |
| Plugin API | typed actions and filters (`plugins.rs`) — `do_action` / `apply_filters` with enum hook names |
| Shortcodes | `add_shortcode`, with unregistered codes left verbatim |
| Search | Postgres full-text over a `tsvector` GIN index, not `LIKE '%…%'` |
| Feeds | Atom and RSS 2.0, site-wide and per-term, with conditional GET |
| Sitemap + robots.txt | driven by the live permalink structure |
| REST API | `/api/v1` — posts, terms, comments, authors, site info |
| Import / export | JSON, idempotent on `(post_type, slug)` |
| Archives | date, author, term, and custom post type |

### Deliberately not covered

| WordPress core | Why not |
|---|---|
| Multisite | a different product shape. Autumn ships row-level tenancy (`examples/saas`) which is the better answer here; bolting it on would mean re-plumbing every query in this starter. |
| XML-RPC | deprecated by WordPress itself, disabled by default since 5.x, and a long-running attack surface. The REST API replaces it. |
| The block editor (Gutenberg) | a large React application. The editor here is Markdown with a live-sanitized pipeline; shipping half a block editor would be worse than shipping none. |
| Pingbacks / trackbacks | effectively dead — near-universally spam, and off by default in most installs. |
| Auto-updates and the plugin/theme installer | downloading and executing code at runtime is the single largest source of WordPress compromises. Plugins here are Rust code compiled into the binary. |
| Post format taxonomy (`aside`, `quote`, …) | theme-specific styling hooks almost no theme uses. A custom taxonomy covers it if you want it. |

### Where this is deliberately not WordPress

- **Capabilities are derived, not stored.** WordPress copies a role's capability
  set into usermeta when the role is assigned, so changing a role's definition
  does not affect existing users and a corrupted `wp_capabilities` silently
  strips access. Here the role is the only stored fact.
- **wp-cron is a real timer.** WordPress fires scheduled work from visitor page
  loads, so a site nobody visits never publishes its scheduled posts. This uses
  `#[scheduled]`.
- **One table for terms, not three.** WordPress splits `wp_terms` /
  `wp_term_taxonomy` / `wp_term_relationships` so a term row can be shared
  across taxonomies. Nothing uses that, and it is the source of the
  `term_id` vs `term_taxonomy_id` confusion.
- **Attachments are their own table.** WordPress stores them in `wp_posts` with
  `post_type = 'attachment'`, which every post query then has to exclude.
- **Export is JSON, not WXR.** WXR is an RSS dialect only WordPress reads.
- **Permalink changes are not breaking.** Resolution is independent of the
  configured structure — every dated shape ends in the post slug, so the router
  reads all of them. This is the single most common WordPress permalink
  complaint and it is covered by a test.

## Where to look

| Concern | File |
|---|---|
| The content model | `src/models.rs`, `migrations/` |
| Roles and the capability matrix | `src/capabilities.rs` |
| Post types and taxonomies | `src/content_types.rs` |
| Transactional operations | `src/content.rs` |
| Permalinks and request resolution | `src/permalinks.rs` |
| Actions and filters | `src/plugins.rs` |
| Shortcodes | `src/shortcodes.rs` |
| Themes, menus, widgets | `src/theme.rs` |
| The public site | `src/routes/front.rs` |
| wp-admin | `src/routes/admin/` |
| The REST API | `src/routes/api.rs` |

## Notable framework primitives used

| Piece | Primitive |
|---|---|
| Post lifecycle | `#[state_machine]` with guards |
| Data access | `#[autumn_web::repository]` (pool-backed, so a page holds one connection) |
| Write invariants | `MutationHooks` — one implementation covers the editor, the API and the importer |
| Search | `#[searchable]` + `repository(searchable)` |
| Media | `storage::BlobStore` + `Download::from_blob` |
| Settings cache | `#[cached]` + `invalidates(...)`, checked by the build-time coherence gate |
| Rich text | `markdown::render_user_content_html` (allowlist-sanitized) |
| Feeds | `feed::Feed` with `conditional()` for cheap polling |
| Scheduled publishing | `#[scheduled]` |
| Comment rendering | `widgets::CommentView` |

## Tests

```bash
cargo test -p cms                                        # structural, no Docker
cargo test -p cms -- --include-ignored --test-threads=1   # full flow (needs Docker)
```

`--test-threads=1` is not optional for the Docker suite: the tests share one
Postgres container and each truncates it.

The full-flow tests cover first-account ownership, draft invisibility, the
status state machine refusing an undeclared edge, guest-comment moderation and
counter arithmetic, contributor capability limits, password protection across
page/feed/API, permalink-structure changes not breaking existing URLs, revision
restore, tag creation and archives, full-text search, export/import
idempotence, page ancestry, and a CSRF round trip.
