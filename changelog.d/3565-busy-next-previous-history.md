### Fixed

- **widgets:** the `data-autumn-busy` runtime resolves `hx-indicator` with
  `next`, `next X`, `previous` and `previous X` as htmx 2 does. Before, these
  indicators did not get `aria-busy`, and the swap target was not marked.
- **widgets:** a keyword part next to a CSS part with a comma, such as
  `closest form, :is(.a, .b)`, no longer clears every indicator. As in htmx,
  `this` counts only as the full value.
- **widgets:** after Back or Forward, a button or target from an
  `hx-push-url` history snapshot is no longer stuck in `aria-busy="true"`.
  The runtime marks its own `aria-busy` with `data-autumn-busied` and removes
  it on `htmx:historyRestore`. A restored element that sends its load request
  during the restore stays busy until that request ends. An `aria-busy` that
  the app set stays.
