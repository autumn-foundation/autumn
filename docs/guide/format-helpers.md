# View Formatting Helpers

Autumn's widget lane (`card`, `data_table`, `property_list`, `hero`,
`breadcrumb`) renders *containers*. `autumn_web::format` renders the
**scalar values** that go inside them — money, counts, and timestamps —
so apps stop hand-rolling `format!("${:.2}", price)` and ad-hoc relative-time
math in every template.

All helpers are pure functions that return HTML-escaped [`maud::Markup`],
so they're safe to interpolate directly into `html! { ... }` blocks. They
ship in the `autumn_web::prelude` (behind the default `maud` feature), so
no extra imports are needed beyond the usual `use autumn_web::prelude::*;`.

## Helpers at a glance

| Helper | Example output |
|--------|-----------------|
| `number_to_currency(price)` | `$1,234.50` |
| `CurrencyOptions::new().symbol("€")...format(price)` | `€1.234,50` |
| `number_with_delimiter(count)` | `1,234,567` |
| `pluralize(count, "comment")` | `1 comment` / `2 comments` |
| `pluralize_with(count, "octopus", "octopi")` | `1 octopus` / `2 octopi` |
| `truncate(text, 30)` | `The quick brown fox jumps ov…` |
| `truncate_words(text, 5)` | `The quick brown fox jumps…` |
| `time_ago_in_words(dt, &clock)` | `3 minutes ago` / `in 2 days` |
| `format_datetime(dt, "%Y-%m-%d")` | `2026-06-07` |

`number_to_currency` takes a [`rust_decimal::Decimal`](https://docs.rs/rust_decimal)
— the same exact-precision type used by decimal model fields — so money never
rounds through a float on its way to the page.

## Copy-paste example: money, count, and timestamp in a `data_table`

```rust
use autumn_web::prelude::*;
use autumn_web::widgets::{Column, DataTableConfig, data_table};

struct Order {
    id: i64,
    total: Decimal,
    item_count: i64,
    placed_at: chrono::DateTime<chrono::Utc>,
}

#[get("/orders")]
async fn index(clock: Clock, orders: Vec<Order>) -> Markup {
    let cols: Vec<Column<Order>> = vec![
        Column::new("Total", |row: &Order| html! { (number_to_currency(row.total)) }),
        Column::new("Items", |row: &Order| html! { (pluralize(row.item_count, "item")) }),
        Column::new("Placed", |row: &Order| {
            html! { (time_ago_in_words(row.placed_at, &clock)) }
        }),
    ];

    html! {
        (data_table(&orders, &cols, &DataTableConfig::new("No orders yet.")))
    }
}
```

This renders each row's `total` as `$1,234.50`, `item_count` as `3 items`
(or `1 item`), and `placed_at` as `2 hours ago` — three helper calls, zero
hand-written `format!`s.

## Customizing currency output

`number_to_currency` uses [`CurrencyOptions`] defaults (`$`, 2 decimal
places, `,`/`.` separators). Build a [`CurrencyOptions`] directly for a
different symbol, precision, or separators:

```rust
use autumn_web::prelude::*;

let opts = CurrencyOptions::new()
    .symbol("€")
    .precision(2)
    .thousands_separator('.')
    .decimal_separator(',');

let price: Decimal = "1234.5".parse().unwrap();
assert_eq!(opts.format(price).into_string(), "€1.234,50");
```

## Pluralize and truncate escape hatches

`pluralize` covers common English rules (sibilant endings, consonant+`y`,
and a handful of irregulars like `person`/`people`). For irregulars it
misses, use `pluralize_with` with an explicit plural:

```rust
pluralize_with(count, "octopus", "octopi")
```

`truncate`/`truncate_words` default to an `"…"` ellipsis and never split a
UTF-8 character mid-byte; use `truncate_with`/`truncate_words_with` for a
custom marker (e.g. `" [read more]"`).

## Deterministic relative time in tests

`time_ago_in_words` takes any [`autumn_web::time::ClockSource`], so tests
get exact, non-flaky output with [`autumn_web::time::FixedClock`]:

```rust
use autumn_web::prelude::*;
use autumn_web::time::FixedClock;
use chrono::{TimeZone, Utc};

let posted_at = Utc.with_ymd_and_hms(2026, 1, 1, 11, 58, 0).unwrap();
let clock = FixedClock::at(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
assert_eq!(time_ago_in_words(posted_at, &clock).into_string(), "2 minutes ago");
```

## Out of scope

Single-locale only (configurable separators/symbol, not full CLDR/ICU
locale catalogs) — see the [i18n guide](i18n.md) for message translation.
No number-to-words, ordinalize, or file-size humanizers yet; no
auto-formatting wired into scaffolded views (scaffolds still emit the raw
value — call these helpers explicitly in the generated template).
