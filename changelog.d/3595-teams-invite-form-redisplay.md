### Fixed

- **generate teams:** a rejected invite shows the members page again with
  status 422, and keeps the email and role (#3595). A bad email, an unknown
  role, or a duplicate pending invitation used to end on a bare 422 or 409
  error. Now the error shows at the field that caused it, and that field
  gets `aria-invalid="true"`. Apps generated before this fix keep the old
  behavior.
