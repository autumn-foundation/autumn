//!
//! Chaos tests using proptest for channels.
//!
#![cfg(feature = "ws")]

use autumn_web::channels::Channels;
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_channels_capacity_fuzzing(capacity in any::<usize>()) {
        let channels = Channels::new(capacity);

        // This should never panic on any capacity (see explicit zero test)
        let tx = channels.sender("test_channel");
        let _rx = channels.subscribe("test_channel");
        prop_assert!(tx.send("test").is_ok());
    }
}

#[tokio::test]
async fn test_channels_zero_capacity_regression() -> Result<(), Box<dyn std::error::Error>> {
    let channels = Channels::new(0);

    // This should never panic even if capacity is 0 (which was the bug)
    let tx = channels.sender("test_channel");
    let mut rx = channels.subscribe("test_channel");

    // We shouldn't use expect/unwrap in tests
    tx.send("test_message")?;
    let msg = rx.recv().await?;
    assert_eq!(msg.as_str(), "test_message");

    Ok(())
}
