### Changed

- **release [no-plugin]:** CLI archive provenance now names the release tag and the tagged
  commit (issue #2380). `gh attestation verify --source-ref refs/tags/<tag>`
  now passes for these assets. The Publish Gate builds
  and attests the archives. `release.yml` only uploads them.
