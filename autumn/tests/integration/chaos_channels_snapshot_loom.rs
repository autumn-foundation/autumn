#![cfg(feature = "ws")]
use autumn_web::channels::Channels;
use loom::thread;

#[test]
fn channels_concurrent_snapshot_and_publish() {
    loom::model(|| {
        let channels = Channels::new(32);
        let tx1 = channels.sender("test_gc");
        let tx2 = tx1.clone();

        let c1 = channels.clone();
        let t1 = thread::spawn(move || {
            let snap = c1.snapshot();
            let _ = snap.get("test_gc").map(|s| s.subscriber_count);
        });

        let t2 = thread::spawn(move || {
            tx2.send("hello").ok();
        });

        let _ = t1.join().unwrap();
        t2.join().unwrap();
    });
}
