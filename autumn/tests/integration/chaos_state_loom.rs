use autumn_web::AppState;
use loom::sync::Arc;
use loom::thread;

#[test]
fn app_state_concurrent_extensions() {
    loom::model(|| {
        let state = AppState::for_test();
        let state = Arc::new(state);

        let s1 = state.clone();
        let t1 = thread::spawn(move || {
            s1.insert_extension(42_u32);
        });

        let s2 = state.clone();
        let t2 = thread::spawn(move || {
            s2.insert_extension(String::from("hello"));
        });

        let s3 = state;
        let t3 = thread::spawn(move || {
            let _ = s3.extension::<u32>();
            let _ = s3.extension::<String>();
        });

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();
    });
}
