# Escape hatches: a stockroom

This example shows each kind of Autumn escape hatch in one small app. Each
hatch has a real reason.

The rule: **start from the convention.** The app uses `#[model]`,
`#[repository]`, `#[get]`, and `autumn.toml` first. It goes below them only
where they cannot do the job. A comment at each hatch names the limit. A
test proves the limit and the fix.

The domain is a stockroom: products, stock counts, and orders. Browser
pages list products. Scanner devices call a JSON API with a bearer token.

`PLAN.md` has the planning notes: brainstorming, reverse brainstorming, and
six thinking hats.

## The hatches

| # | Convention first | Why it is not enough | Escape hatch | Where |
|---|---|---|---|---|
| — | `with_lock` per product | (It is enough for one product. This is the baseline.) | — | `src/api.rs` `reserve` |
| H1 | `with_lock` | It locks one row. A cart needs all lines or none. | `Db::tx` + a guarded Diesel `UPDATE` per line | `src/api.rs` `checkout` |
| H2 | Repository `update` per row | It writes absolute values. A concurrent sale is lost. | One set-based Diesel `UPDATE` | `src/api.rs` `restock` |
| H3 | Finders and aggregates | No window functions | `diesel::sql_query` + `QueryableByName` | `src/reports.rs` |
| H4 | `#[throttle]`, `timeout_ms` | They limit rate and time, not concurrent runs | `#[intercept(ReportGate)]` (custom tower layer) | `src/hatches/report_gate.rs` |
| H5 | Browser routes with no login | Scanners are machines with bearer tokens | `.scoped("/api", RequireApiToken, ..)` | `src/lib.rs` |
| H6 | CSRF on for every POST in `prod` | Bearer clients send no CSRF cookie | `[security.csrf] exempt_paths` | `autumn.toml` |
| H7 | `Cache-Control` only on `/static` | Live stock must not sit in a shared cache | `.layer(SetResponseHeaderLayer)` | `src/hatches/cache_control.rs` |
| H8 | `static/` file serving | Build assets only. Runtime files need another mount. | `.nest(..)` + `ServeDir` + `declare_plugin_routes` | `src/hatches/exports.rs` |
| H9 | `#[get]` handlers on `AppState` | The supplier code is plain Axum with its own state | `Plugin` + `nest` + `declare_plugin_routes` | `src/hatches/supplier_plugin.rs`, `src/supplier.rs` |
| H10 | Default error pages | An unknown SKU needs a link to order it | `.error_pages(..)` | `src/hatches/error_pages.rs` |
| H11 | Default problem responses | The query-timeout 503 has no `Retry-After` | `.exception_filter(..)` | `src/hatches/retry_after.rs` |
| H12 | Default pool factory | A sidecar rotates the password in a file | `.with_pool_provider(..)` | `src/hatches/password_file.rs` |
| H13 | `Markup` / `Json` returns | No `201 Created` + `Location` helper | `impl IntoResponse` tuple | `src/api.rs` `checkout` |

`PLAN.md` section 7 lists the hatches that this app does not use, and why.
A hatch with no need is noise.

## Prerequisites

- Rust 1.88.0+
- PostgreSQL. Or Docker: `docker compose up -d` in this folder starts one on
  `localhost:5432` that matches `autumn.toml`.

## Quick start

From the workspace root:

```bash
docker compose -f examples/escape-hatches/docker-compose.yml up -d
STOCKROOM_SCANNER_TOKEN=dev-scanner-token cargo run -p escape-hatches
```

In `development`, the app applies its migration at boot. The tables are
empty. Add three products:

```bash
docker compose -f examples/escape-hatches/docker-compose.yml exec db \
  psql -U autumn stockroom -c "INSERT INTO products (sku, name, category, stock, price_cents) VALUES
    ('HAM-1', 'Claw hammer', 'tools', 5, 1500),
    ('SAW-1', 'Hand saw', 'tools', 2, 2400),
    ('PNT-RED', 'Red paint, 1 L', 'paint', 8, 1100);"
```

Open <http://127.0.0.1:3000>.

### Prove it works

```bash
T='authorization: Bearer dev-scanner-token'
J='content-type: application/json'
A='accept: application/json'

# H1 + H13: a cart. 201, with a Location header.
curl -si http://127.0.0.1:3000/api/checkout -H "$T" -H "$J" -H "$A" \
  -d '{"order_ref":"o-100","lines":[{"sku":"HAM-1","quantity":2},{"sku":"SAW-1","quantity":1}]}'
# => HTTP/1.1 201 Created
#    location: /api/orders/o-100

# H1: a short line rolls back the whole cart. HAM-1 keeps its stock.
curl -s http://127.0.0.1:3000/api/checkout -H "$T" -H "$J" -H "$A" \
  -d '{"order_ref":"o-101","lines":[{"sku":"HAM-1","quantity":1},{"sku":"SAW-1","quantity":50}]}'
# => {"type":".../conflict","status":409,"detail":"not enough stock for SAW-1",...}

# H2: add 10 to each tool, in one statement.
curl -s http://127.0.0.1:3000/api/restock -H "$T" -H "$J" -H "$A" -d '{"category":"tools","add":10}'
# => {"category":"tools","updated":2}

# H3 + H4: the top three products by stock value in each category.
curl -s http://127.0.0.1:3000/reports/stock-value
# => [{"category":"paint","sku":"PNT-RED",...,"rank":1},{"category":"tools","sku":"SAW-1",...},...]

# H5: no token, no access.
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:3000/api/restock
# => 401

# H7: every page says no-store.
curl -si http://127.0.0.1:3000/ | grep -i cache-control
# => cache-control: no-store

# H9: the supplier's plain Axum router, mounted by a plugin.
curl -s http://127.0.0.1:3000/supplier/items/ZZ-9
# => {"sku":"ZZ-9","name":"Torque wrench","unit_cost_cents":4200,"lead_time_days":10}

# H10: an unknown SKU links to the supplier catalog.
curl -s -H 'accept: text/html' http://127.0.0.1:3000/products/ZZ-9 | grep -o 'href="/supplier/items/ZZ-9"'

# H8: a file that a job wrote at runtime.
mkdir -p examples/escape-hatches/exports && echo 'sku,stock' > examples/escape-hatches/exports/stock.csv
curl -s http://127.0.0.1:3000/exports/stock.csv
# => sku,stock
```

The app reads `exports/` in the project folder, as it finds `static/`. Set
`STOCKROOM_EXPORTS_DIR` to use another folder.

### H12: the password from a file

```bash
echo autumn > /tmp/db-password
STOCKROOM_DB_PASSWORD_FILE=/tmp/db-password \
AUTUMN_DATABASE__URL=postgres://autumn@localhost:5432/stockroom \
STOCKROOM_SCANNER_TOKEN=dev-scanner-token \
  cargo run -p escape-hatches
```

The URL has no password. Each new connection reads the file. Write a new
password to the file, and new connections use it with no restart.

## Environment

| Variable | Use |
|---|---|
| `STOCKROOM_SCANNER_TOKEN` | The bearer token for `/api`. With no value, `/api` refuses every call. |
| `STOCKROOM_DB_PASSWORD_FILE` | The database password file (H12). With no value, the default pool is used. |
| `STOCKROOM_EXPORTS_DIR` | The exports folder (H8). The default is `exports/` in the project folder. |

## Tests

| Command | What it runs | Needs |
|---|---|---|
| `cargo test -p escape-hatches` | Unit tests, the no-database tier of `hatches`, and the report gate | — |
| `cargo test -p escape-hatches --test hatches -- --include-ignored --test-threads=1` | All hatch tests, the Postgres tier too | Docker |
| `cargo test -p escape-hatches --test boot -- --ignored` | Boots the real binary: H7, H10, H11, H12 wiring | Docker |
| `cargo test -p escape-hatches --features system-tests --test smoke -- --include-ignored` | Chromium smoke | Docker, Chromium |

Read the tests in this order:

1. `hazard_repository_read_modify_write_loses_an_update`: the problem. Two
   reads, two absolute writes, and one sale is lost.
2. `convention_reserve_with_lock_never_oversells`: the convention fixes it
   for one row.
3. `checkout_*` and `restock_*`: the hatches fix it where `with_lock` cannot.
