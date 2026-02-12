use nostr_sdk::prelude::*;

/// Connects to a nostr relay, subscribes to text notes, and responds
/// "Hi" to any message containing "hello".
///
/// The `client` is returned by reference so the caller (main or tests)
/// retains access to it for shutdown or further interaction.
pub async fn run_client(keys: Keys, relay_url: &str) -> Result<Client> {
    let client = Client::new(keys);
    client.add_relay(relay_url).await?;
    println!("Connecting to relay {}...", relay_url);
    client.connect().await;
    println!("Connected!");

    let filter = Filter::new().kind(Kind::TextNote);
    client.subscribe(filter, None).await?;
    println!("Subscribed to text notes. Listening for events...\n");

    // Clone the client so we can use it inside the notification handler
    // to publish responses. The original client is returned to the caller.
    let client_clone = client.clone();

    // Spawn the notification loop in a background task so this function
    // returns immediately. The caller can keep using the client while
    // events are being handled in the background.
    tokio::spawn(async move {
        let _ = client_clone
            .handle_notifications(|notification| async {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    println!(
                        "--- Event from {} ---",
                        event.pubkey.to_bech32().unwrap_or_default()
                    );
                    println!("{}", event.content);
                    println!();

                    // If the message contains "hello" (case-insensitive),
                    // respond with "Hi"
                    if event.content.to_lowercase().contains("hello") {
                        println!("Responding with Hi...");
                        let builder = EventBuilder::text_note("Hi");
                        if let Err(e) = client_clone.send_event_builder(builder).await {
                            eprintln!("Failed to publish response: {}", e);
                        }
                    }
                }
                Ok(false)
            })
            .await;
    });

    Ok(client)
}
