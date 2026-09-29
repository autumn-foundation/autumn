### Fixed

- **plugin-sandbox:** the pre-compilation instruction-volume ceiling now
  counts the global section's bytes alongside the code section's
  (issue #2461). A global's initializer is a constant expression, and the
  extended-const proposal lets that expression run to arbitrary length — one
  global could carry most of a module while sitting under the entry-count
  ceiling, handing the compiler instruction volume no ceiling measured. The
  gate refuses when `code_bytes + global_bytes` exceeds `MAX_CODE_BYTES`,
  before `Module::new` runs, and reports the combined volume as
  "code and global section bytes" so the refusal names the sections that
  pushed the module over.
