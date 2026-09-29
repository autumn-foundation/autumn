### Security

- **htmx:** `HtmxFragments` now detects an `hx-swap-oob` attribute that follows
  an HTML comment closed with `--!>` or written as `<!-->`. The detector
  previously ended comments only at `-->`, so such markup was wrapped in a
  second OOB carrier while the browser still honoured the inner attribute.

### Fixed

- **htmx:** the OOB-attribute detector no longer reads markup inside raw-text elements
  (`<script>`, `<style>`, `<textarea>`, `<title>`, …) as real attributes, so a
  fragment containing such text keeps its server-generated OOB carrier.
