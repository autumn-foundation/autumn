### Fixed

- **🧭 Wayfinder: enforce `Tag::name`'s length bound when creating tags in
  `examples/reddit-clone` (error-path 0/1 → 1/1, dropped names reported):**
  `Tag::name` declares `#[validate(length(min = 1, max = 40))]`, but
  `resolve_or_create_tag_ids` (called from `manage_tags`, `POST
  /r/{sub}/posts/{post}/tags`) created new tags via a raw
  `diesel::insert_into` that never runs the model's generated
  `validator::Validate` — so the one declared bound on this model was
  enforced nowhere between the free-text tag field and the database. A tag
  name longer than 40 characters was silently persisted uncapped, with no
  error shown at all (not even a generic one — the request still redirected
  with a plain "Tags updated." success message). Fix: `parse_tag_names` now
  drops an overlong name the same way it already drops a name with no letter
  or number, and `manage_tags`' flash message reports both reasons a name was
  ignored, so the author is told rather than the request silently saving
  fewer/worse tags than asked for. Covered by new unit tests on the pure
  `parse_tag_names`/`tags_updated_notice` functions asserting the 40-character
  boundary and the combined-reasons message.
