### Fixed

- **🧭 Wayfinder: reddit-clone's create-community name field no longer
  carries native constraints narrower than the server rule (#2838):**
  the name `<input>` dropped `pattern="[a-zA-Z0-9_]+"` plus `minlength="2"`
  and `maxlength="32"`, which silently blocked server-valid names before the
  form's error round-trip could run — non-Latin names (`日本語`, `Привет`),
  names with spaces (`web dev`), and supplementary-plane names the server
  counts as 17 characters but `maxlength` counts as 34 UTF-16 code units.
  The field now states the real rule in its hint ("2-32 characters, with at
  least one letter or number") and lets the server's `ChangesetForm`
  round-trip enforce it.
