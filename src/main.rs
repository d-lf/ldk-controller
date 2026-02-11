// Import the nostr-sdk prelude which brings in all commonly used types:
// Client, Keys, Filter, Kind, Event, RelayPoolNotification, Result, etc.
use nostr_sdk::prelude::*;
// serde::Deserialize lets us automatically parse TOML config into Rust structs
use serde::Deserialize;
use std::fs;

// Each struct maps to a section in config.toml. The #[derive(Deserialize)] macro
// auto-generates the code to parse TOML key-value pairs into struct fields.
// Field names must match the TOML keys exactly.

#[derive(Debug, Deserialize)]
struct Config {
    node: NodeConfig,
    nostr: NostrConfig,
    wallet: WalletConfig,
}

#[derive(Debug, Deserialize)]
struct NodeConfig {
    network: String,
    listening_port: u16,
    data_dir: String,
}

#[derive(Debug, Deserialize)]
struct NostrConfig {
    relay: String,
    private_key: String,
}

#[derive(Debug, Deserialize)]
struct WalletConfig {
    max_channel_size_sats: u64,
    min_channel_size_sats: u64,
    auto_accept_channels: bool,
}

// #[tokio::main] transforms this async fn into a synchronous fn that starts
// the tokio async runtime. This is required because nostr-sdk uses async/await
// for all network operations (connecting to relays, subscribing, receiving events).
// The Result<()> return type comes from nostr-sdk and lets us use the ? operator
// to propagate errors instead of manually handling each one.
#[tokio::main]
async fn main() -> Result<()> {
    // Read and parse the config file. expect() will crash with a clear message
    // if the file is missing or malformed — appropriate for startup config.
    let contents = fs::read_to_string("config.toml").expect("Failed to read config.toml");
    let config: Config = toml::from_str(&contents).expect("Failed to parse config.toml");

    println!("Loaded config:");
    println!("  Network:        {}", config.node.network);
    println!("  Listening port: {}", config.node.listening_port);
    println!("  Data dir:       {}", config.node.data_dir);
    println!("  Relay:          {}", config.nostr.relay);
    println!("  Max channel:    {} sats", config.wallet.max_channel_size_sats);
    println!("  Min channel:    {} sats", config.wallet.min_channel_size_sats);
    println!("  Auto accept:    {}", config.wallet.auto_accept_channels);

    // Keys are the nostr identity — a secp256k1 keypair. The private key (nsec)
    // signs events, the public key (npub) identifies the user.
    // We try to parse the key from config first (supports nsec bech32 or hex format).
    // If parsing fails (e.g. placeholder value "nsec1abc123..."), we generate
    // a fresh random keypair instead.
    let keys = match Keys::parse(&config.nostr.private_key) {
        Ok(keys) => {
            println!("Using keys from config");
            keys
        }
        Err(_) => {
            let keys = Keys::generate();
            println!("Generated new keys (config key invalid)");
            println!("  Public key: {}", keys.public_key().to_bech32()?);
            keys
        }
    };

    // The Client is the main interface to the nostr network. It manages relay
    // connections, sends/receives events, and handles subscriptions.
    // We pass our keys so the client can sign events on our behalf.
    let client = Client::new(keys);

    // Add a relay to connect to. A relay is a WebSocket server that stores
    // and forwards nostr events. You can add multiple relays for redundancy.
    client.add_relay(&config.nostr.relay).await?;
    println!("Connecting to relay {}...", config.nostr.relay);

    // connect() opens WebSocket connections to all added relays.
    // It returns immediately and connects in the background.
    client.connect().await;
    println!("Connected!");

    // Filters tell the relay which events we want to receive.
    // Kind::TextNote (kind 1) is a regular text post — the most common event type.
    // Other kinds include: 0 (metadata/profile), 4 (encrypted DMs), 7 (reactions),
    // and application-specific kinds like 23194 (NWC requests).
    let filter = Filter::new().kind(Kind::TextNote);

    // Subscribe sends our filter to the relay. The relay will:
    // 1. Send back all stored events matching the filter (historical)
    // 2. Forward any new events matching the filter in real time
    // The None parameter means we're not passing any subscription options.
    client.subscribe(filter, None).await?;
    println!("Subscribed to text notes. Listening for events...\n");

    // handle_notifications blocks and loops forever, calling our closure for
    // each notification from the relay pool. Notifications include:
    // - Event: a nostr event matching our subscription
    // - Message: raw relay messages (for debugging)
    // - Shutdown: the client is shutting down
    //
    // The closure returns Ok(false) to keep listening, or Ok(true) to stop.
    client
        .handle_notifications(|notification| async {
            // Pattern match to only handle Event notifications, ignoring others.
            // The event field contains the full nostr event with:
            // - pubkey: the author's public key
            // - content: the text content of the note
            // - created_at: unix timestamp
            // - id: unique hash of the event
            // - sig: the author's signature
            if let RelayPoolNotification::Event { event, .. } = notification {
                // Display the author's pubkey in npub (bech32) format for readability
                println!("--- Event from {} ---", event.pubkey.to_bech32().unwrap_or_default());
                println!("{}", event.content);
                println!();
            }
            // Return Ok(false) to continue listening for more events.
            // Returning Ok(true) would break out of the notification loop.
            Ok(false)
        })
        .await?;

    Ok(())
}
