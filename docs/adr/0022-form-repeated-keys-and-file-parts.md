# ADR 0022: Repeated form keys and multipart file parts

- Status: Accepted
- Date: 2026-10-21
- Deciders: Autumn maintainers
- Tags: forms, multipart, security

## Context

Issue #3394. `ChangesetForm` decoded bodies through `serde_urlencoded`. That
decoder rejects a repeated key, so a `Vec<T>` field never decoded. Checkbox
groups, `<select multiple>` and tag inputs could not use a changeset. The
scaffold worked around this with a generated `parse_bulk_ids`. The multipart
path dropped file parts with no error.

## Decision

1. **Decoder.** Use `serde_html_form` (a fork of `serde_urlencoded`) for form
   bodies. It groups repeated keys. Values parse the same way, errors read the
   same way, and `serde_path_to_error` still names the field. `Form<T>` does
   not change.
2. **Repeated key on a single-value field: 422.** The decoder does not select a
   value. The check runs before the blank-drop retry, so the retry cannot
   remove one copy and accept the other.
3. **Blank element of a typed list is dropped**, as a blank `Option` is.
4. **File parts reach `ChangesetForm`** (`file`, `files`, `take_files`). The
   parts come through `crate::extract::Multipart`, so the `security.upload`
   policy applies. Each file is held in memory, inside the upload caps.
5. **Bulk delete** decodes `BulkSelection { ids: Vec<i64> }` through the new
   public `form::from_urlencoded`.

## Options considered

| Option | Verdict |
| --- | --- |
| `serde_html_form` | **Chosen.** Smallest change to single-value results. |
| `query_string` in a flat mode | Rejected. Its errors drop custom messages, and nested-form row errors that users see would change. |
| Own grouping deserializer | Rejected. Most new code. |
| Last value wins | Rejected. Hidden parameter-pollution risk. |
| First value wins | Rejected. Same risk. |
| Reject file parts with an error | Rejected. A file and validated fields on one route is the common case. |

## Consequences

- A repeated scalar was a 400 and is now a 422.
- A `bool` field also accepts `on`. Its error text now shows the submitted
  value, as the enum and type errors of `serde_urlencoded` did before.
- A blank numeric or `bool` `Option` is `None`, also when the field has a
  `#[serde(default = "...")]`.
- A `HashMap<String, String>` target rejects a repeated key.
- A fixed-size array or a newtype around a `Vec` takes one value. A repeat on
  it is a 422.
- Each retry removes all the blank pairs of one key, so the loop runs at most
  once for each field of `T`. One retry for each pair was quadratic.
- `ChangesetForm` keeps the route's `DefaultBodyLimit` on the multipart path.
- New dependency: `serde_html_form` (MIT). Its dependencies are in the lock
  file already.
- `verification/form_decode.rs` models one retry step: a repeated scalar always
  stops the loop, a retry removes only blank pairs, and each retry makes the
  input shorter.
