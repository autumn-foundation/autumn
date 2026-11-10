### Fixed

- **generate teams:** a rejected invite shows the members page again, with
  the typed email and role kept (#3595). A bad email, an unknown role, or a
  duplicate pending invitation used to end on a bare 422 or 409 error. Now
  the error shows at the field that caused it, and that field gets
  `aria-invalid="true"`.
