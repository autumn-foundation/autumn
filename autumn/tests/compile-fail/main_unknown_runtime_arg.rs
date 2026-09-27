//! A typo'd runtime argument must be a compile error, not a silently ignored
//! one: before these arguments existed the whole attribute list was dropped.

#[autumn_web::main(worker_thread = 4)]
async fn main() {}
