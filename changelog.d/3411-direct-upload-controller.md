### Added

- **storage:** `autumn-widgets.js` now drives `direct_upload_input` (issue
  #3411). It gets a presigned URL and sends the file with a progress bar. It
  accepts dropped files and adds a hidden field with the blob key. A running
  upload holds the form.
- **storage:** `direct_upload_field(name, presign_url, &DirectUploadConfig)`
  sets the label, `id`, `accept` and `multiple`.

### Changed

- **storage:** `direct_upload_input` has a visible `<label>`, not an
  `aria-label`. Live regions announce the status and the errors.
- **storage:** without JS, the field is a plain file input. Use a
  `multipart/form-data` form.
- **storage:** the presign response must include `key`. See
  `docs/guide/storage.md`. The old `data-controller` attributes stay for
  custom controllers.
