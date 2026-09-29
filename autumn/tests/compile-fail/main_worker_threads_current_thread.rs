//! A current-thread runtime has no worker pool to size, so a `worker_threads`
//! the runtime would ignore is rejected rather than accepted and dropped.

#[autumn_web::main(flavor = "current_thread", worker_threads = 4)]
async fn main() {}
