### Changed

- Bumped `validator` to 0.21 (and `validator_derive` with it). `autumn_web::prelude::Validate`
  now re-exports the 0.21 trait, so an application must pin
  `validator = { version = "0.21", features = ["derive"] }` for `#[derive(Validate)]` to satisfy
  `IntoChangeset`; a 0.20 pin resolves a second copy of the crate and the derive no longer
  matches. `autumn new`, `autumn generate scaffold|model|teams` and the `cms`/`saas` starters
  write the 0.21 pin.
