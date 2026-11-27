# ADR 0052: Schema.org JSON-LD in the Framework and the CLI

- Status: Accepted
- Date: 2026-11-27
- Deciders: Autumn maintainers
- Tags: seo, aeo, cli, generators, security

## Context

Search engines and AI agents read schema.org structured data to learn what a
page is about. Google shows rich results (recipe cards, prices, event dates)
only for pages that carry it. Autumn already wrote meta tags, `sitemap.xml`,
`robots.txt` and the agent signals, but nothing for schema.org. An app wrote
the `<script type="application/ld+json">` block by hand, and a hand-written
block put user text inside a `<script>` element, where the HTML parser ends
the element at `</script>` whatever the JSON means.

Three questions:

1. How does a page write a block that a stored value cannot break out of?
2. Where does the vocabulary come from, and how much of it?
3. How does an app find out that its blocks are wrong?

## Options

### Escaping

1. Escape `<`, `>`, `&`, U+2028 and U+2029 as `\u` escapes in the serialized
   JSON. These characters appear only inside JSON strings, so the value does
   not change, and the output holds no `<`.
2. Reject a value that holds `</script`. Rejected: it blocks a real value,
   and it misses `<!--`, which also changes how the parser reads the element.
3. Leave it to the app. Rejected: the safe path must be the default.

### Vocabulary

1. Vendor the whole schema.org release (about 800 types, 1,500 properties,
   a few MB of JSON-LD). Rejected for now: the CLI binary grows, most types
   never appear in a rich result, and the required and recommended lists,
   which matter most, are not in schema.org at all.
2. Fetch schema.org at run time. Rejected: `generate` and `audit` must work
   offline and give the same answer twice.
3. A curated table in Rust: the types with a Google rich result, the types
   their properties point at, and Google's required and recommended lists.

### Scaffold integration

1. A new DSL modifier per field (`title:String{schema:headline}`). Rejected:
   it touches the field parser for one consumer, and most columns already
   carry the property's name.
2. One `--schema-org <Type>` flag. Columns bind by name, by a nested name
   (`offers_price`), and by a short alias list. `--schema-org-map` binds the
   rest.

## Decision

- **`autumn_web::json_ld::JsonLd`** builds a node, and `to_json` and
  `render` escape it (escaping option 1). `to_json_unescaped` is the explicit
  opt-out, for an API body or a file. `SeoMeta::json_ld` renders blocks with
  the meta tags. `verification/json_ld_escape.rs` proves that the escaper's
  output has no `<`, `>` or `&` byte, so `</script` and `<!--` cannot occur.
  A property test checks the runtime on any string.
- **The vocabulary is a curated Rust table** (vocabulary option 3), in
  `autumn-cli/src/schema_org/vocab.rs`. Unit tests check that every parent
  and range entry resolves, every type reaches `Thing`, and every Google list
  names a real property. The audit reports a type outside the table as a
  notice, not an error: it may be a real type the table lacks.
- **`generate scaffold --schema-org <Type>`** (scaffold option 2) writes a
  plain `json_ld(row)` function in the routes file. The show view renders it.
  With no fields, the type's required and recommended properties become the
  fields. A reference id, a file, bytes, and a translated or encrypted column
  are never bound: none is a value to publish as it is.
- **`autumn schema-org list | show | audit`**. `audit` reads URLs, HTML files
  and directories, and fails on errors, or on warnings with `--strict`.

## Consequences

- A user value in a JSON-LD block cannot end the script element, on the
  default path.
- The table goes stale when Google changes a rich result. The `google_docs`
  link on each type names the page to check. Growing the table is a data
  change with tests, not a code change.
- The audit is a check of shape and vocabulary, not Google's Rich Results
  Test. The guide says so.
- A scaffold without `--schema-org` is unchanged byte for byte.
