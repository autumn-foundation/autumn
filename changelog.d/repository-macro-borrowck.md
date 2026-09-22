### Performance

- **macros:** `autumn-macros-repository` compiles in 14.7s instead of 25.0s, and
  its peak memory drops from 1145 MB to 875 MB. `repository_macro` was one
  15,900-line function, and rustc's borrow checker costs grow with the size of a
  single function: it alone took 11.5s of the crate's 25s. The emitters now sit
  in seven functions and borrow checking takes 3.8s. The generated code does not
  change — the expansion is byte-for-byte what it was, over 53 `#[repository]`
  configurations.
