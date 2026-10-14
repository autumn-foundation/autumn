### Changed

- **release:** CLI archive provenance now names the release tag and the tagged
  commit (issue #2380). `gh attestation verify --source-ref refs/tags/<tag>`
  and `--signer-workflow` now pass for these assets. The Publish Gate builds
  and attests the archives. `release.yml` only uploads them.
