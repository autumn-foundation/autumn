### Fixed

- **🧭 Wayfinder: full HTML document shell on `examples/collab-notes` pages
  (a11y 2/1 → 0):** `GET /` and `GET /notes/{id}` served `<!DOCTYPE html>`
  plus sibling `<meta>`/`<title>`/`<style>` and `<body>` elements with no
  `<html>` element at all. Browsers silently recover, but the
  browser-inserted `<html>` carries no `lang` attribute, so a screen reader
  has no page language to announce, and neither page had a `<main>`
  landmark for "jump to content" navigation. Both pages now render inside
  `<html lang="en">` with a real `<head>` and exactly one `<main>` landmark;
  `autumn check --a11y` goes from 2 Serious + 1 Moderate (`/`) and 2 Serious
  (`/notes/{id}`) to 0 violations on each.
