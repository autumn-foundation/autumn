### Fixed

- **contracts:** `autumn contracts export` no longer takes a local type for
  a path through a re-export. `Json<crate::dto::Item>`, where `dto`
  re-exports `shared::Item`, used to resolve to a local `models::Item`, so a
  change to `shared::Item` did not show. The export now refuses the path
  when another crate has a type of that name. Write the path where the type
  is defined.
- **contracts:** in a binary crate, a path written in `src/main.rs`
  (`routes![items::list]`, `crate::items::Item`) now names the module file
  `src/items.rs`. Before, `routes![items::list]` registered every `list`
  handler of the crate, so a handler removed from the router did not show.
- **contracts:** `#[contract_checked]` no longer reads a user type named
  `NoBody` as the request of an endpoint with no body.

### Documentation

- **contracts:** the wire contracts guide lists more limits: a caller's
  read-set is top level only, a `routes![…]` entry with no crate path does
  not judge another crate, and a `use … as` or `type` alias is read by its
  own name.
