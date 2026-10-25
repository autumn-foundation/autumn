### Performance

- **db scrub:** the `--sample` closure walk joins only the keys the previous
  pass added, not every key reached so far (issue #2599). A deep hierarchy
  costs one pass per level, so each pass was slower than the last: on a
  500 000-node, 250-level tree the walk falls from 33.7 s to 2.0 s. The rows
  kept and the pass count do not change. A descent through a foreign key
  column with no index still scans that table on each pass.
