### Added

- **🧭 Wayfinder: `examples/reddit-clone`'s create-community name field no
  longer rejects valid names client-side (#2441/#2454 item 3) [no-plugin]:**
  the name input carried `pattern="[a-zA-Z0-9_]+"` plus `minlength="2"
  maxlength="32"`, all narrower than `validate_community_name`'s actual rule
  (2-32 *characters*, any script, needs at least one letter or number).
  `pattern` silently blocked server-valid names like `"web dev"` and
  `"日本語"` before the browser would even send the request — no 422, no
  error, nothing the already-fixed `ChangesetForm` round-trip (#2665) could
  catch — while accepting `"__"`, which the server rejects.
  `minlength`/`maxlength` had the same problem one layer down: they count
  UTF-16 code units, while the server counts Unicode scalar values, so a
  17-character supplementary-plane name (34 UTF-16 units, e.g. repeated
  Deseret letters) would have been rejected at `maxlength="32"` despite being
  well within the 32-*character* rule. All three attributes are now dropped
  in favor of the existing accessible server round-trip; the visible hint
  states the real rule instead.
