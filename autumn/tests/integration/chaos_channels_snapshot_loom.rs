#![cfg(feature = "ws")]
use autumn_web::channels::Channels;
use loom::thread;

#[test]
fn channels_concurrent_snapshot_and_publish() {
    loom::model(|| {
        let channels = Channels::new(32);
        let tx = channels.sender("test_gc");

        let t1 = thread::spawn(move || {
            let snap = channels.snapshot();
            let _ = snap.get("test_gc").map(|s| s.subscriber_count);
        });

        let t2 = thread::spawn(move || {
            tx.send("hello").ok();
        });

        t1.join().unwrap();
        t2.join().unwrap();
    });
}
