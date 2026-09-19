//! The `current_thread` flavor is reachable through `#[autumn_web::main]`, and
//! the numeric arguments take computed expressions, not only literals.

#[autumn_web::main(
    flavor = "current_thread",
    max_blocking_threads = 1 + 1,
    thread_name = String::from("autumn-single")
)]
async fn main() {
    // A current-thread runtime still drives timers, so `enable_all` held.
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}
