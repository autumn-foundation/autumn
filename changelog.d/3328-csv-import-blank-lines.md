### Fixed

- **data:** `import_csv` now reports the true line of a row that follows blank
  lines (issue #3328). Before, the line was too small by the number of blank
  lines. This also holds for blank lines before the header.
