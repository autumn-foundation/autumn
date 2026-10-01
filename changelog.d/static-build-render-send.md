### Fixed

- **static_gen:** `render_static_routes` no longer breaks `Send` inference.
  The by-reference job closure made rustc report "implementation of `FnOnce`
  is not general enough", so `autumn_web::app().run()` could not be spawned.
  Jobs are now consumed by value.
