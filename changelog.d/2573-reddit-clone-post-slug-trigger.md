### Fixed

- **reddit-clone:** a database trigger gives a taken post slug the next free
  suffix, so a concurrent write through the generated repository no longer
  fails with a unique violation. The edit route links to the stored slug
  (issue #2573). [no-plugin]
