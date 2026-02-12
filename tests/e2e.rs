use nostr_sdk::prelude::*;
use std::time::Duration;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    GenericImage,
};

/// Starts a fresh strfry relay container and returns (container, relay_url).
/// The container is automatically removed when dropped.
async fn start_relay() -> (testcontainers::ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new("strfry-strfry", "latest")
        // strfry listens on port 7777 inside the container
        .with_exposed_port(7777.tcp())
        // Wait until strfry logs that the websocket server is ready
        .with_wait_for(WaitFor::message_on_stderr("Started websocket server"))
        .start()
        .await
        .expect("Failed to start strfry container");

    // testcontainers maps the container's port to a random host port
    let host_port = container
        .get_host_port_ipv4(7777)
        .await
        .expect("Failed to get mapped port");

    let relay_url = format!("ws://localhost:{}", host_port);
    println!("Strfry relay started on {}", relay_url);

    (container, relay_url)
}

/// End-to-end test: send "hello", expect the app to respond with "Hi".
///
/// Uses a fresh strfry container so there are no leftover events.
/// 1. Start a clean strfry relay via testcontainers
/// 2. Start the app client (run_client) — subscribes and responds to "hello"
/// 3. Create a sender client with different keys
/// 4. Sender publishes "hello"
/// 5. App sees "hello", publishes "Hi"
/// 6. Sender receives "Hi" — test passes
#[tokio::test]
async fn test_hello_gets_hi_response() -> Result<()> {
    // Start a fresh relay — no leftover events from previous runs
    let (_container, relay_url) = start_relay().await;

    // App client — this is what we're testing.
    let app_keys = Keys::generate();
    let app_pubkey = app_keys.public_key();
    let _app_client = ldk_controller::run_client(app_keys, &relay_url).await?;

    // Give the app client time to connect and subscribe
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Sender client — simulates an external user sending "hello"
    let sender_keys = Keys::generate();
    let sender_client = Client::new(sender_keys);
    sender_client.add_relay(&relay_url).await?;
    sender_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Subscribe to text notes from the app's pubkey so we can see its "Hi" response
    let filter = Filter::new()
        .kind(Kind::TextNote)
        .author(app_pubkey);
    sender_client.subscribe(filter, None).await?;

    // Send "hello"
    let builder = EventBuilder::text_note("hello");
    sender_client.send_event_builder(builder).await?;
    println!("Sent 'hello', waiting for 'Hi' response...");

    // Wait for the app's "Hi" response (timeout after 10 seconds)
    let timeout = Duration::from_secs(10);
    let result = tokio::time::timeout(timeout, async {
        sender_client
            .handle_notifications(|notification| async {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if event.pubkey == app_pubkey && event.content == "Hi" {
                        println!("Received 'Hi' from app!");
                        return Ok(true); // stop listening — test passed
                    }
                }
                Ok(false) // keep listening
            })
            .await
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("Test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive 'Hi' response within 10 seconds"),
    }
}
