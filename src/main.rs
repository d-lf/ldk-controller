use nostr_sdk::prelude::*;
use serde::Deserialize;
use std::fs;

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

#[tokio::main]
async fn main() -> Result<()> {
    let contents = fs::read_to_string("config.toml").expect("Failed to read config.toml");
    let config: Config = toml::from_str(&contents).expect("Failed to parse config.toml");

    println!("Loaded config:");
    println!("  Network:        {}", config.node.network);
    println!("  Listening port: {}", config.node.listening_port);
    println!("  Data dir:       {}", config.node.data_dir);
    println!("  Relay:          {}", config.nostr.relay);
    println!(
        "  Max channel:    {} sats",
        config.wallet.max_channel_size_sats
    );
    println!(
        "  Min channel:    {} sats",
        config.wallet.min_channel_size_sats
    );
    println!("  Auto accept:    {}", config.wallet.auto_accept_channels);

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

    // Start the NWC service using the shared library code.
    let client = ldk_controller::run_nwc_service(keys, &config.nostr.relay).await?;

    // Keep the main function alive so the background notification handler
    // continues running. Ctrl+C to stop.
    println!("Press Ctrl+C to stop.\n");
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");
    client.disconnect().await;

    Ok(())
}
