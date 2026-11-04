### Fixed

- **custom domains:** a load that skips an unreadable record no longer lets
  `register` accept a new hostname (issue #2654). It returns
  `RegisterError::Incomplete` and names the files. Loaded domains keep
  routing and renewing. The retention prune also refuses to run.
