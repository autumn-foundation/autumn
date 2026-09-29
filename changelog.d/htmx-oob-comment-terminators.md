### Security

- **htmx:** `HtmxFragments` now detects an `hx-swap-oob` attribute that follows
  an HTML comment closed with `--!>` or written as `<!-->`. The detector
  previously ended comments only at `-->`, so such markup was wrapped in a
  second OOB carrier while the browser still honoured the inner attribute.
