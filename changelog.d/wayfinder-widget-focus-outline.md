### Fixed

- **ui:** widget form fields, submit button, search and autocomplete inputs keep
  a focus indicator in forced-colors (Windows High Contrast) mode; the
  `outline: none` rules now use a transparent outline, which is invisible in
  normal rendering and repainted in the system focus colour when forced.
