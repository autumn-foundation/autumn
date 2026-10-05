# Plan: the `escape-hatches` example

Written in ASD-STE100 style: short sentences, active voice, one word for one
meaning.

## 1. Goal

Show each Autumn escape hatch in one small, real app. Each hatch starts from
the convention. The app uses a hatch only where the convention cannot do the
job. A test proves each limit and each fix.

The domain is a stockroom: products, stock counts, and orders.

## 2. Terms

| Term | Meaning |
|---|---|
| Convention | The framework default: `#[model]`, `#[repository]`, `#[get]`, `autumn.toml`. |
| Escape hatch | A supported API that goes below the convention. |
| Limit | A thing the convention cannot do, or cannot do safely. |

## 3. Brainstorming

Question: "Where does a stockroom app need more than a model and a repository?"
We wrote all ideas first and judged them later.

1. Reserve the last unit of a product. Two buyers. One unit.
2. Check out a cart of several products. All lines or none.
3. Add stock to a whole category in one action.
4. Rank the most valuable products in each category.
5. Stop the ranking query from running many times at once.
6. Let scanner devices call a JSON API with a bearer token.
7. Scanner devices send POST without a CSRF cookie.
8. Stock counts change all the time. Proxies must not cache pages.
9. A nightly job writes CSV files. Serve them.
10. The supplier team owns a plain Axum router. Mount it.
11. Show a 404 page that links back to the product list.
12. Tell clients when to retry after a database timeout.
13. A sidecar rotates the database password into a file.
14. Return `201 Created` with a `Location` header.
15. Load config from a remote service.
16. Store sessions in Redis.
17. Wrap outgoing mail.
18. Tune the Tokio runtime.

## 4. Reverse brainstorming

Question: "How do we make this example bad?" Each answer gives a rule.

| How to fail | Rule we keep |
|---|---|
| Use raw SQL where a repository finder works. | Each hatch names the convention first, and the exact limit. |
| Invent a need ("use Diesel because we can"). | Each limit has a failing test or a cited framework fact. |
| Copy a built-in feature with a hatch. | Check the framework first. We dropped ideas that duplicate built-ins (see 4.1). |
| Hide the hazard. | A test shows the hazard (a lost update) before the fix. |
| Write long, vague comments. | Comments follow ASD-STE100: short, active, one meaning. |
| Mount a router that the route audit cannot see. | Declare plugin routes. |
| Leave Docker tests dark in CI. | Name the test target in the CI Docker step. |
| Use process-global state in shared tests. | Put the report gate test in its own test binary. |

### 4.1 Ideas we dropped, and why

| Idea | Why we dropped it |
|---|---|
| Per-route timeout layer | Built in: `#[get(..., timeout_ms = N)]`. |
| `Deprecation`/`Sunset` layer on `/v1` | Built in: API version lifecycles. |
| `statement_timeout` pool provider | Built in: `database.statement_timeout`. |
| Custom rate-limit layer | Built in: `#[throttle]`. |
| Ideas 15 to 18 | This app has no need. A hatch without a need is noise. Section 7 lists where each one is shown. |

## 5. Six thinking hats

| Hat | Finding |
|---|---|
| White (facts) | `with_lock` locks one row. Repository writes set absolute values. Diesel's DSL has no window functions. The framework sets `Cache-Control` only on `/static`. Autumn serves files only from `static/`. tokio-postgres reads no password file. The query-timeout `503` has no `Retry-After`. |
| Red (feelings) | Readers distrust examples with invented needs. A test that shows the hazard builds trust. |
| Black (risks) | Concurrency tests can be flaky. Use a deterministic interleave for the lost update. Use a semaphore that the test holds for the gate. Process-global state needs its own binary. |
| Yellow (benefits) | One app shows the full ladder: convention, then hatch, with the reason in the code. |
| Green (ideas) | One end-to-end test boots the real binary. It proves the wiring that `TestApp` cannot reach: the pool provider, error pages, exception filter. |
| Blue (process) | Red, green, refactor for each hatch. Then a multi-angle review. Then the AC table. |

## 6. Hatches in this app

| # | Convention first | Limit | Hatch | Proof |
|---|---|---|---|---|
| H1 | `with_lock` per product | Locks one row. A cart needs all lines or none. | `Db::tx` with a guarded Diesel `UPDATE` per line | `checkout_*` tests |
| H2 | Repository `update` per row | N round trips. Absolute values lose concurrent decrements. | One set-based Diesel `UPDATE` | `restock_*` tests |
| H3 | Repository finders and aggregates | No window functions | `diesel::sql_query` + `QueryableByName` | `report_*` tests |
| H4 | `#[throttle]`, `timeout_ms` | Per caller rate, not a global limit on concurrent runs | `#[intercept(ReportGate)]` | `report_gate` test binary |
| H5 | Session auth on browser routes | Scanners are machines with bearer tokens | `scoped("/api", RequireApiToken, ..)` | `api_*` tests |
| H6 | CSRF on for all POSTs in `prod` | Bearer clients have no CSRF cookie | `security.csrf.exempt_paths` | `csrf_*` test |
| H7 | No app-wide header | Live stock must not sit in a shared cache | `.layer(SetResponseHeaderLayer)` | `cache_control_*` tests |
| H8 | `static/` file serving | Build assets only. Runtime files need another mount. | `.nest(Router)` with `ServeDir` + `declare_plugin_routes` | `exports_*` tests |
| H9 | `#[get]` handlers on `AppState` | Supplier code is plain Axum with its own state | `Plugin` + `nest` + `declare_plugin_routes` | `supplier_*` tests |
| H10 | Default error pages | The 404 page knows only the status. A scanner that reads an unknown SKU needs a link to order it. | `.error_pages(..)` | `not_found_*` tests |
| H11 | Default problem response | Query-timeout `503` has no `Retry-After` | `.exception_filter(..)` | `retry_after_*` tests |
| H12 | Default pool factory | Password rotates in a file | `.with_pool_provider(..)` | `password_file_*` tests |
| H13 | `Markup` / `Json` return | No `201` + `Location` helper | `impl IntoResponse` tuple | `checkout_returns_201_*` test |

## 7. Hatches not used here

| Hatch | Why not here | Where it is shown |
|---|---|---|
| `with_config_loader` | `autumn.toml` and env vars cover this app. | `docs/guide/custom-subsystems.md` |
| `with_session_store`, `with_cache_backend` | One process. The built-in stores are enough. | `autumn-cache-redis` |
| `with_blob_store` | No uploads. | `autumn-storage-s3` |
| `with_telemetry_provider`, `with_channels_backend` | No need. | `docs/guide/custom-subsystems.md` |
| `with_*_interceptor` (job, mail, DB, HTTP) | No jobs, mail, or outbound HTTP. | `docs/guide/middleware.md` |
| `static_gate` | No static page cache. | `docs/guide/middleware.md` |
| `.merge(Router)` | `merge` hides routes from `autumn routes audit`. `nest` + `declare_plugin_routes` does not. A test mounts the supplier router with `merge` to show that raw routes get the app middleware. | `docs/guide/getting-started.md` |
| `on_startup` + raw SQL seed | No seed data. | `examples/react-graphql` |
| `state_initializer` + extensions | No shared service. | `examples/media-room` |
| Custom `FromRequestParts` | Built-in extractors are enough. | `docs/guide/extractors.md` |
| `#[autumn_web::main(configure = ..)]` | Default runtime is enough. | `docs/guide/getting-started.md` |

## 8. Acceptance criteria

No GitHub issue exists for this example. The criteria come from the request
and from the PRD (FR-041, FR-042, FR-045).

1. AC1: An example app shows each escape hatch kind, each with a real reason.
2. AC2: Each hatch starts from the convention (a model and a repository).
3. AC3: Work follows red, green, refactor. Commits show each phase.
4. AC4: Planning uses brainstorming, reverse brainstorming, and six hats.
5. AC5: Comments and docs are short and use ASD-STE100.
6. AC6 (FR-041): Raw Axum routes share state and middleware.
7. AC7 (FR-042): The example replaces at least one subsystem.
8. AC8 (FR-045): The example overrides an error page.
9. AC9: Tests run in CI, Docker tests included.
10. AC10: The example is in the catalog (`EXAMPLES.md`, `README.md`).
