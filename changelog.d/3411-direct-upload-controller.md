### Added

- **storage:** `autumn-widgets.js` now drives `direct_upload_input` (issue
  #3411). It gets a presigned URL, sends the file with a progress bar, accepts
  dropped files, and adds a hidden field with the blob key. The form does not
  send the file bytes. A running upload holds the form.
- **storage:** `direct_upload_field(name, presign_url, &DirectUploadConfig)`
  sets the label, `id`, `accept` and `multiple`.

### Changed

- **storage:** `direct_upload_input` has a visible `<label>` and no
  `aria-label`. The status and the errors are in live regions. The no-JS
  fallback is a plain file input in a multipart form. The presign response
  needs a `key` field. See `docs/guide/storage.md`.
