#![cfg(feature = "ws")]
use autumn_web::channels::Channels;

use loom::thread;

#[test]
fn channels_concurrent_gc_and_send() {
    loom::model(|| {
        let channels = Channels::new(32);
        let tx1 = channels.sender("test_gc");

        let c1 = channels;
        let t1 = thread::spawn(move || {
            c1.gc();
        });

        let tx2 = tx1;
        let t2 = thread::spawn(move || {
            tx2.send("hello").ok();
        });

        t1.join().unwrap();
        t2.join().unwrap();
    });
}
