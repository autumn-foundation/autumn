### Security

- **assets:** `autumn assets` now keeps every read and write in `static/`.
  - A manifest `file` must be a relative path with `/` separators. The CLI
    rejects absolute paths, `.` and `..` parts, `\`, `:` and Windows device
    names such as `con.js`. A manifest with such a path does not load. Thus
    `add`, `list`, `update` and `verify` stop with an error.
  - `add --url` rejects a URL whose file name is not a safe path.
  - A symlink below `static/` must point into `static/`. `static/` itself
    cannot be a symlink. A symlinked manifest does not load.
  - `update` downloads only from `https` sources. To keep an `http` source,
    add the asset again with `add --url`. `add --url` still accepts `http`.
  - The CLI checks every asset before the first download. It prints the
    source of each download.
  - Vendored files keep their mode. New files get mode `0644`.
