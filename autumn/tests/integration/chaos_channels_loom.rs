#![cfg(feature = "ws")]
use autumn_web::channels::Channels;
use loom::thread;

#[test]
fn channels_concurrent_sender_creation() {
    loom::model(|| {
        let channels = Channels::new(32);

        let c1 = channels.clone();
        let t1 = thread::spawn(move || {
            let tx = c1.sender("test");
            let _rx = c1.subscribe("test");
            tx.send("hello").ok();
        });

        let c2 = channels.clone();
        let t2 = thread::spawn(move || {
            let tx = c2.sender("test");
            let _rx = c2.subscribe("test");
            tx.send("world").ok();
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(channels.channel_count(), 1);
    });
}
