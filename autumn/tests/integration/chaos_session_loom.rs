use autumn_web::session::MemoryStore;
use autumn_web::session::SessionStore;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

// MemoryStore uses tokio::sync::RwLock internally, which is not Loom-instrumented.
// This is a regular multithreaded stress test to exercise concurrent save/load behaviour.
#[test]
fn session_concurrent_mutations() {
    let store = MemoryStore::new();
    let store = Arc::new(store);

    let s1 = store.clone();
    let t1 = thread::spawn(move || {
        let mut data = HashMap::new();
        data.insert("key".to_string(), "val1".to_string());
        let _ = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(s1.save("test_id", data));
    });

    let s2 = store.clone();
    let t2 = thread::spawn(move || {
        let mut data = HashMap::new();
        data.insert("key".to_string(), "val2".to_string());
        let _ = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(s2.save("test_id", data));
    });

    t1.join().unwrap();
    t2.join().unwrap();

    let result = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(store.load("test_id"));
    // After both concurrent saves, the session must contain exactly one of the two writes.
    let data = result.unwrap().expect("session must be present after save");
    let val = data.get("key").expect("'key' must be present in session");
    assert!(val == "val1" || val == "val2");
}
