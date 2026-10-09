### Fixed

- **stories:** the `/_stories` gallery loads `htmx.min.js` before
  `autumn-widgets.js` on the index and detail pages (issue #2353). Stories
  that use `hx-*` attributes now work.
