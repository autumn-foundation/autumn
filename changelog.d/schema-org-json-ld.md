### Added

- **json-ld:** `autumn_web::json_ld::JsonLd` builds schema.org nodes and
  renders them as `<script type="application/ld+json">` blocks. Every `<`,
  `>` and `&` in the JSON is written as an escape, so a stored value cannot
  end the script element. `SeoMeta::json_ld` renders a block with the meta
  tags. See `docs/guide/schema-org.md`.
- **cli:** `autumn generate scaffold <Name> --schema-org <Type>` makes a
  resource a schema.org type. The show page renders its JSON-LD from the
  columns named after the type's properties, and with no fields the type's
  required and recommended properties become the fields.
  `--schema-org-map column=property` binds any other column.
- **cli:** `autumn schema-org list`, `autumn schema-org show <Type>` and
  `autumn schema-org audit <URL|FILE|DIR>`. The audit checks each JSON-LD
  block for invalid JSON, a missing `@context` or `@type`, unknown types and
  properties, values of the wrong shape, and the properties Google requires
  or recommends.
