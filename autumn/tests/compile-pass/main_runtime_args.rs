//! `#[autumn_web::main]`'s optional arguments reach the tokio runtime.
//!
//! trybuild `pass` fixtures are compiled *and run*, so this is a behavioral
//! check rather than a compile check: the tuned runtime has to boot, name its
//! threads as asked, and have called the `configure` escape hatch before the
//! body runs.

use std::sync::atomic::{AtomicUsize, Ordering};

static THREADS_STARTED: AtomicUsize = AtomicUsize::new(0);

fn tune_runtime(builder: &mut autumn_web::reexports::tokio::runtime::Builder) {
    // `on_thread_start` is exactly the kind of `Builder` method the
    // declarative arguments do not cover, which is what `configure` is for.
    builder.on_thread_start(|| {
        THREADS_STARTED.fetch_add(1, Ordering::SeqCst);
    });
}

#[autumn_web::main(
    flavor = "multi_thread",
    worker_threads = 2,
    max_blocking_threads = 8,
    thread_name = "autumn-worker",
    thread_stack_size = 2 * 1024 * 1024,
    thread_keep_alive = "30s",
    configure = tune_runtime
)]
async fn main() {
    let name = tokio::task::spawn_blocking(|| std::thread::current().name().map(str::to_owned))
        .await
        .expect("the blocking task must run on the tuned runtime");
    assert_eq!(
        name.as_deref(),
        Some("autumn-worker"),
        "`thread_name` must reach Builder::thread_name"
    );

    // The blocking thread above has run, so at least its start hook has fired.
    assert!(
        THREADS_STARTED.load(Ordering::SeqCst) > 0,
        "`configure` must have been applied to the builder"
    );
}
