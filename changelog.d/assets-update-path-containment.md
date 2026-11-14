### Security

- **assets:** `autumn assets update` and `autumn assets add` now keep every
  write in `static/`. A manifest `file` must be a relative path with normal
  `/`-separated parts. The CLI rejects absolute paths, `..`, `\`, `:` and
  Windows device names, and it does not load a manifest that has one. A
  symlink below `static/` stops the write. A `source` must be an `http` or
  `https` URL. The CLI prints the source of each download.
