//!
//! Chaos tests for channels.
//!
#![cfg(feature = "ws")]

use autumn_web::channels::Channels;

#[tokio::test]
async fn test_sender_orphaned_by_gc() {
    let channels = Channels::new(16);
    let tx = channels.sender("chat");
    channels.gc();
    let mut rx = channels.subscribe("chat");

    tx.send("hello").unwrap();
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.as_str(), "hello");
}
