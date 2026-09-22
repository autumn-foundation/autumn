### Performance

- **macros:** the two CRUD emitters inside `autumn-macros-repository` are split
  into nine functions, which takes the crate from 11.9s to 11.3s and its borrow
  checking from 2.89s to 2.49s. `emit_crud_bodies_hooked` was 3,861 lines and
  `emit_crud_bodies_plain` 2,501, and rustc's borrow checker costs grow with the
  size of a single function. The generated code does not change — the expansion
  is byte-for-byte what it was, over 53 `#[repository]` configurations.
