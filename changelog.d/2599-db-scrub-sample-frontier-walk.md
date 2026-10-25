### Performance

- **db scrub:** the `--sample` closure walk joins only the keys the previous
  pass added, not every key found so far (issue #2599). A deep hierarchy
  needs one pass per level. Before this change, each pass read every key
  found so far, so each pass was slower than the last. On a 500 000-node,
  250-level tree the walk now takes 2.0 s, not 33.7 s. The rows
  kept and the pass count do not change. A descent through a foreign key
  column with no index still scans that table on each pass.
