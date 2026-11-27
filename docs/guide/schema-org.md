# Schema.org structured data — JSON-LD

Structured data tells a search engine or an AI agent what a page is about:
this page is a recipe, its name is "Pancakes", it takes 30 minutes. The
standard vocabulary is [schema.org](https://schema.org). Pages publish it as
JSON-LD in a `<script type="application/ld+json">` block. Google uses it for
rich results, such as recipe cards, product prices and event dates.

Autumn gives you three parts:

| Part | What it does |
|---|---|
| `autumn_web::json_ld::JsonLd` | Builds a schema.org node and renders it as an escaped script block. |
| `autumn generate scaffold --schema-org <Type>` | Makes a resource a schema.org type. Its show page renders the JSON-LD. |
| `autumn schema-org` | Lists the types, shows a type's properties, and audits the JSON-LD of pages. |

This page builds on [SEO](seo.md): `SeoMeta` renders JSON-LD with the other
head tags.

---

## Build a node by hand

```rust
use autumn_web::json_ld::JsonLd;
use autumn_web::prelude::*;

#[get("/recipes/pancakes")]
async fn pancakes() -> Markup {
    let recipe = JsonLd::new("Recipe")
        .set("name", "Pancakes")
        .set("image", "https://example.com/pancakes.jpg")
        .set("recipeIngredient", vec!["2 eggs", "1 cup flour"])
        .set("author", JsonLd::node("Person").set("name", "Ada"))
        .set_opt("description", None::<String>);
    html! {
        head { (recipe) }
        body { h1 { "Pancakes" } }
    }
}
```

- `JsonLd::new(type)` makes a top-level node with
  `"@context": "https://schema.org"`.
- `JsonLd::node(type)` makes a nested node, without `@context`. A top-level
  node set as a property value loses its `@context` too.
- `set` takes any `Serialize` value: text, numbers, lists, `jiff` dates and
  times (written as ISO 8601), and other nodes. A value that is `null`
  removes the property.
- `set_opt` sets the property only for `Some`.
- `JsonLd::graph([…])` puts several nodes under one `@context`.
- `id(iri)` sets `@id`, so other nodes can refer to this one.
- `has_properties()` tells a nested node with values from an empty one, so you
  can leave the empty one out:
  `.set_opt("offers", Some(offer).filter(JsonLd::has_properties))`.

A `JsonLd` is a `maud::Render`, so `(recipe)` in `html!` writes the whole
`<script>` element. `render()` does the same. Put it in the `<head>` or the
`<body>`: search engines read both.

### With `SeoMeta`

`SeoMeta::json_ld` adds a block that renders after the meta tags:

```rust
use autumn_web::json_ld::JsonLd;
use autumn_web::seo::SeoMeta;

let meta = SeoMeta::new()
    .title("Pancakes")
    .canonical("https://example.com/recipes/pancakes")
    .json_ld(JsonLd::new("Recipe").set("name", "Pancakes"));
// html! { head { (meta.render()) } }
```

Call `json_ld` again to add another block, for example a `BreadcrumbList`.

### Escaping

A JSON string can hold `</script>`. The HTML parser ends the script element
there, whatever the JSON means, and the rest of the value runs as markup.
`to_json()` and `render()` write every `<`, `>` and `&` as a `\u003c`,
`\u003e` or `\u0026` escape, and U+2028 and U+2029 as `\u2028` and `\u2029`.
A JSON parser reads back the same value. The output holds no `<` at all, so no
value can close the element or open an HTML comment. This holds for values
your users typed: a recipe named `</script><script>alert(1)</script>` stays a
name.

`verification/json_ld_escape.rs` proves this for the escaper with Verus, and a
property test checks it for any string. `to_json_unescaped()` is the plain
JSON, for an API response or a file. Do not put it in a page.

---

## Scaffold a schema.org resource

```bash
autumn generate scaffold Recipe --schema-org Recipe
```

With no fields, the type's required and recommended properties become the
fields. `autumn schema-org show Recipe` lists them. Required properties are
`NOT NULL`. The rest are `Option<…>`:

```text
name:String
image:String{url}
author:Option<String>
date_published:Option<Timestamp>
description:Option<Text>
prep_time:Option<String>
…
recipe_ingredient:Option<Text>
recipe_instructions:Option<Text>
nutrition_calories:Option<String>
```

The scaffold writes a `json_ld` function into `src/routes/recipes.rs`, and the
show page renders it after the property list:

```rust
fn json_ld(row: &Recipe) -> autumn_web::json_ld::JsonLd {
    use autumn_web::json_ld::JsonLd;
    let nutrition = JsonLd::node("NutritionInformation")
        .set_opt("calories", row.nutrition_calories.as_ref());
    JsonLd::new("Recipe")
        .set("name", &row.name)
        .set("image", &row.image)
        .set_opt("author", row.author.as_ref().map(|name| JsonLd::node("Person").set("name", name)))
        .set_opt("recipeIngredient", row.recipe_ingredient.as_deref().map(json_ld_lines))
        .set_opt("nutrition", Some(nutrition).filter(JsonLd::has_properties))
        .set("dateCreated", &row.created_at)
        // …
}
```

It is ordinary code: edit it freely.

### Your own fields

Give the fields, and the scaffold binds the ones that match the type:

```bash
autumn generate scaffold Post --schema-org BlogPosting \
  title:String body:Text author:Option<String> date_published:Option<Timestamp> internal_notes:Text
```

A column binds to a property in this order:

1. **`--schema-org-map column=property`.** It binds any column, and wins over
   the rules below. `--schema-org-map cost=offers.price` binds a property of a
   nested node.
2. **The name.** `date_published` is `datePublished`. A property that is a
   Rust keyword gets `_value`: `abstract_value` is `abstract`.
3. **A nested name.** `<property>_<sub>` is a property of a nested node:
   `offers_price` is `offers.price`, and `location_name` is `location.name`.
   The parent is not repeated: `address_locality` is
   `address.addressLocality`.
4. **An alias.** `title` is `headline` (else `name`), `body` and `content`
   are `articleBody`, `reviewBody`, `text` or `description`, `summary` is
   `description`, `published_at` is `datePublished`, `updated_at` is
   `dateModified`, `starts_at` is `startDate`, `image_url` is `image`,
   `phone` is `telephone`, `tags` is `keywords`, and a few more.

`created_at`, which every model has, binds to `dateCreated` on a creative
work. Each property binds once, to the first column that claims it. A column
that matches nothing, such as `internal_notes`, is not published.

How a value is written:

| Property | Column | JSON-LD |
|---|---|---|
| Text, number, date, boolean, URL | any | the value |
| A list (`recipeIngredient`, `sameAs`) | `String` or `Text` | one item per line, as a list |
| A node known by its name (`author`, `brand`, `organizer`) | any | `{"@type": "Person", "name": value}` |
| A property of a nested node (`offers_price`) | any | `"offers": {"@type": "Offer", "price": value}`, left out when all its columns are empty |
| An enumeration (`eventStatus`, `availability`) | `String` | the value: store `InStock` or `https://schema.org/InStock` |

Some columns are never published, even when the name matches: a
`references` id, an `Attachment`, `Bytea`, a `{translatable}` or
`{encrypted}` column, and a server-managed position or comment count.

The scaffold refuses:

- An unknown type. The error suggests the closest one.
- An enumeration as the type. It is a value, not a record.
- `--api`. It renders no show view.
- Fields that match no property. The JSON-LD would be empty.
- A type with nothing to seed, given no fields (`Thing`, `Intangible`).

In `autumn.generate.toml` the same options are `schema_org` and
`schema_org_map`:

```toml
[scaffold.Mug]
fields = ["name:String", "cost:money", "sku:String"]
schema_org = "Product"
schema_org_map = ["cost=offers.price"]
```

---

## Browse the types

```bash
autumn schema-org list
autumn schema-org show Product
autumn schema-org show Product --all
autumn schema-org show Event --format json
```

`list` prints the types the CLI knows, with a `*` on each Google rich result.
`show` prints a type's lineage, its Google documentation page, each property
with its expected types and what Google asks for (required, recommended or
optional), and the fields a scaffold with no fields would make. `--all` adds
the properties inherited from the parent types. For an enumeration, `show`
lists the members.

The vocabulary is a curated slice of schema.org: the types search engines
show rich results for (Article, Recipe, Product, Event, JobPosting,
LocalBusiness, Course, Dataset, VideoObject, SoftwareApplication, FAQPage and
more), and the types their properties point at (Person, Organization, Offer,
PostalAddress, Rating). The required and recommended lists follow Google's
structured data documentation, not schema.org, which requires nothing.

---

## Audit pages

```bash
autumn schema-org audit http://localhost:3000/recipes/1
autumn schema-org audit dist/ --strict
autumn schema-org audit page.html --format json
```

The audit reads every `<script type="application/ld+json">` block of each
page and checks it. A target is an http(s) URL, an HTML file, or a directory:
every `.html` file under it is read, such as the `dist/` that `autumn build`
writes.

| Rule | Severity | Meaning |
|---|---|---|
| `unreadable` | error | The page could not be fetched, or the file is not UTF-8. |
| `invalid-json` | error | The block is not JSON. |
| `missing-context` | error | A top-level node has no schema.org `@context`. |
| `missing-type` | error | A top-level node has no `@type`. |
| `missing-required` | error | Google requires a property the node lacks. |
| `unknown-property` | warning | The type has no such property. The message suggests the closest one. |
| `wrong-value` | warning | The value does not fit the property: a date that is not ISO 8601, a URL that is not one, a number that is text, a node of the wrong type, an enumeration value that is not a member. |
| `empty-value` | warning | The value is an empty string or list. |
| `missing-recommended` | warning | Google recommends a property a top-level node lacks. |
| `unknown-type` | notice | The type is not in the CLI's vocabulary, so it is not checked. A name close to a known type is a warning instead, with a "did you mean". |
| `no-json-ld` | notice | The page has no JSON-LD block. |

The audit fails, with exit code 1, on an error. With `--strict` it also fails
on a warning. A notice never fails it. Nested nodes are checked too: an
`Offer` with no `price` is an error. A review or rating nested in the item it
rates needs no `itemReviewed`.

The audit is a check of shape and vocabulary. It does not fetch schema.org,
and it is not Google's Rich Results Test: run that too before you count on a
rich result.

---

## See also

- [SEO](seo.md) — meta tags, `sitemap.xml` and `robots.txt`
- [AEO](aeo.md) — Markdown for agents, `llms.txt` and the other agent signals
- [Generators](generators.md) — the field DSL the scaffold uses

---

*This guide follows [ASD-STE100](https://www.asd-ste100.org/) Simplified
Technical English: short sentences, active voice, simple tenses, and one
meaning per word. Keep that style when you edit it.*
