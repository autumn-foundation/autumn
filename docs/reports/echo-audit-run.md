# Echo DX Audit Complaint

> **Resolved.** Both findings below are fixed.
> - "Leaky Macro Error Messages": `route_macro` now re-emits the handler and a
>   stub `__autumn_route_info_*` companion when the route attribute fails to
>   parse, so `#[get()]` reports one error instead of three (Onramp,
>   autumn-macros/src/route.rs's `emit_with_attr_parse_error`, regression
>   fixture `autumn/tests/compile-fail/route_attr_error_cascades_through_routes.rs`).
> - "Verbose Extractor Import": `Path` is already re-exported from
>   `autumn_web::prelude` (`autumn/src/prelude.rs`).

## 1. Experience - The Walkthrough
I followed the README Quickstart:
- Ran `autumn new my-app2`
- Overwrote `my-app2/src/main.rs` with the `README.md` example block
- Ran `cargo build`

The application generated successfully and built successfully out of the box.

## 2. Stumble - The Friction Points
I performed the "Error Check" by intentionally removing the path in `#[get("/")]` to make it `#[get()]`. The rustc errors were:
```
error: unexpected end of input, expected string literal
 --> src/main.rs:3:1
  |
3 | #[get()]
  | ^^^^^^^^
```
And:
```
error[E0425]: cannot find function `__autumn_route_info_index` in this scope
```

While the first error is helpful indicating the string literal is expected, the second error (`__autumn_route_info_index` not found) exposes the underlying macro generation code which is very confusing and not helpful to a new user.

I performed the "Import Scan":
- The README explicitly uses `use autumn_web::prelude::*;` which nicely bundles imports. However, to extract a path parameter, it requires the fully qualified name `autumn_web::extract::Path<String>` instead of something shorter or having `Path` available in the prelude. It is quite verbose.

I performed the "Slang Check":
- The README is clear. However, terms like "proc-macro ergonomics", "hybrid-rendering", or "Cloud-Native" might be a bit jargon-heavy but acceptable given the context.

## 3. Report - The Complaint
- **"Leaky Macro Error Messages"**: When there is a syntax error in a route macro (e.g. `#[get()]` instead of `#[get("/")]`), the compiler emits an internal error (`__autumn_route_info_index` not found). The user should not see generated internal function names when they simply make a mistake in the macro argument.
- **"Verbose Extractor Import"**: The example in the README requires `autumn_web::extract::Path<String>`. If `Path` is a very common extractor, it really should be in the prelude so the user can just type `Path<String>`. (Note: my previous DX audit stated that `Path` wasn't in scope, but forcing the user to type `autumn_web::extract::Path` is still a bit verbose, although it works).

## 4. Verify - The "idiot proofing"
- **Error Messages**: The macro implementation needs to be improved so that on failure, it emits an empty dummy `__autumn_route_info_...` function so the user only sees the primary error ("expected string literal") and not the secondary "not found in scope" error.
- **Prelude**: Consider re-exporting `autumn_web::extract::Path` in `autumn_web::prelude` to simplify the examples.
