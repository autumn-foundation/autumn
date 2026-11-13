### Security

- **markdown:** `autumn-web` now needs `ammonia` 4.2.5 or later
  (RUSTSEC-2026-0533). The old version let a MathML text element or SVG
  `foreignObject` keep a child from the other namespace after the sanitizer
  removed the tag that switched namespaces, so crafted user-submitted rich
  text could parse differently in the browser and run script.
