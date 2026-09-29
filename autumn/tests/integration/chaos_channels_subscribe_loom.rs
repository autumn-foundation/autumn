#![cfg(feature = "ws")]
use autumn_web::channels::Channels;
use loom::thread;

#[test]
fn channels_concurrent_subscribe() {
    loom::model(|| {
        let channels = Channels::new(32);

        let c1 = channels.clone();
        let t1 = thread::spawn(move || {
            let _ = c1.subscribe("test_sub");
        });

        let c2 = channels.clone();
        let t2 = thread::spawn(move || {
            let _ = c2.subscribe("test_sub");
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(channels.channel_count(), 1);
        let snapshot = channels.snapshot();
        assert_eq!(snapshot.get("test_sub").unwrap().subscriber_count, 0); // No receivers are kept alive
    });
}
