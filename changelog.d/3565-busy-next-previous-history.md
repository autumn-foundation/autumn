### Fixed

- **widgets:** the `data-autumn-busy` runtime resolves `hx-indicator` with
  `next`, `next X`, `previous` and `previous X` as htmx 2 does. Before, these
  indicators did not get `aria-busy`, and the swap target was not marked.
- **widgets:** after Back, a button or target from an `hx-push-url` history
  snapshot is no longer stuck in `aria-busy="true"`. The runtime marks its own
  `aria-busy` with `data-autumn-busied` and removes it on
  `htmx:historyRestore`. An `aria-busy` that the app set stays.
