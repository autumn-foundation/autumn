### Changed

- **pdf:** a page's content stream sets the font and the fill color once, not
  before every word. Rendered pages look the same; they allocate 17.7% fewer
  bytes and take 4.4% fewer instructions to render. [no-plugin]
