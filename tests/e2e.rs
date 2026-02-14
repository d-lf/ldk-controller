use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip47::{
    CancelHoldInvoiceRequest, ListTransactionsRequest, LookupInvoiceRequest, MakeHoldInvoiceRequest,
    MakeInvoiceRequest, Method, NostrWalletConnectUri, PayInvoiceRequest, PayKeysendRequest,
    Request, RequestParams, Response, ResponseResult, SettleHoldInvoiceRequest,
};
use std::time::Duration;

mod common;
use common::{start_relay, test_guard};

// Relay helpers live in tests/common/mod.rs

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
    let _guard = test_guard();
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
    let sender_client = Client::builder().signer(sender_keys).build();
    sender_client.add_relay(&relay_url).await?;
    sender_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Subscribe to text notes from the app's pubkey so we can see its "Hi" response
    let filter = Filter::new()
        .kind(Kind::TextNote)
        .author(app_pubkey);
    sender_client.subscribe(filter).await?;

    // Send "hello"
    let builder = EventBuilder::text_note("hello");
    sender_client.send_event_builder(builder).await?;
    println!("Sent 'hello', waiting for 'Hi' response...");

    // Wait for the app's "Hi" response (timeout after 10 seconds)
    let timeout = Duration::from_secs(10);
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = sender_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.pubkey == app_pubkey && event.content == "Hi" {
                    println!("Received 'Hi' from app!");
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
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

/// End-to-end test: send a NWC get_info request, expect a valid response.
///
/// 1. Start a clean strfry relay
/// 2. Start the NWC service (wallet side)
/// 3. Create a NWC client with a shared secret URI
/// 4. Client sends a get_info request (Kind 23194, NIP-04 encrypted)
/// 5. Service decrypts, handles, and responds (Kind 23195)
/// 6. Client decrypts and validates the get_info response
#[tokio::test]
async fn test_nwc_get_info_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    // Wallet service keys — the NWC service we're testing
    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify the service published its capabilities (Kind 13194)
    let info_filter = Filter::new()
        .kind(Kind::WalletConnectInfo)
        .author(service_pubkey);
    let info_client = Client::builder().signer(Keys::generate()).build();
    info_client.add_relay(&relay_url).await?;
    info_client.connect().await;
    let info_events = info_client
        .fetch_events(info_filter)
        .timeout(Duration::from_secs(5))
        .await?;
    assert_eq!(info_events.len(), 1);
    let info_event = info_events.iter().next().unwrap();
    assert_eq!(info_event.pubkey, service_pubkey);
    assert!(info_event.content.contains("get_info"));
    assert!(info_event.content.contains("get_balance"));
    println!(
        "Verified Kind 13194 capabilities: {}",
        info_event.content
    );

    // Build a NWC URI: the client uses this to know the service pubkey,
    // relay, and shared secret for NIP-04 encryption.
    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    // Create the NWC client — uses keys derived from the shared secret
    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Subscribe to responses from the service addressed to us
    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    // Send get_info request (NIP-04 encrypted, Kind 23194)
    let request_event = Request::get_info()
        // TODO: we should add the encryption method here
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC get_info request, waiting for response...");

    // Wait for the NWC response
    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let info = response
                        .to_get_info()
                        .expect("Response was not a valid get_info");

                    println!("Received get_info response: alias={:?}", info.alias);
                    assert_eq!(info.alias, Some("ldk-controller".to_string()));
                    assert_eq!(info.network, Some("regtest".to_string()));

                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC get_info roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC get_info request as a non-owner, expect Restricted.
#[tokio::test]
async fn test_nwc_get_info_restricted_for_non_owner() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    // Create the NWC client but do NOT register it as an owner.
    let client_keys = Keys::new(client_secret);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let request_event = Request::get_info()
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC get_info request as non-owner, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    assert_eq!(response.result_type, Method::GetInfo);
                    let err = response.error.expect("Expected Restricted error");
                    assert_eq!(err.code, nwc::nostr::nips::nip47::ErrorCode::Restricted);
                    assert_eq!(
                        err.message,
                        "access denied, insufficient permission".to_string()
                    );
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC non-owner access test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC get_info request as a non-owner with method access, expect success.
#[tokio::test]
async fn test_nwc_get_info_allowed_for_method_access() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    // Create the NWC client and grant method-level access (not owner).
    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_access_rule(&client_pubkey, Method::GetInfo, 1, 1_000_000);

    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let request_event = Request::get_info()
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC get_info request as non-owner with method access, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let info = response
                        .to_get_info()
                        .expect("Response was not a valid get_info");

                    assert_eq!(info.alias, Some("ldk-controller".to_string()));
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC method-access get_info test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: access rule allows one call, second should be rate-limited.
#[tokio::test]
async fn test_nwc_get_info_rate_limited_after_one_call() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    // Create the NWC client and grant one-token access (not owner).
    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_access_rule(&client_pubkey, Method::GetInfo, 1, 1_000_000);

    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    // First call should succeed (consume the only token).
    let request_event = Request::get_info()
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let _info = response
                        .to_get_info()
                        .expect("First response should be get_info");
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive first NWC response within 10 seconds"),
    }

    // Second call should be rate-limited.
    let request_event = Request::get_info()
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;

    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    assert_eq!(response.result_type, Method::GetInfo);
                    let err = response.error.expect("Expected rate-limited error");
                    assert_eq!(err.code, nwc::nostr::nips::nip47::ErrorCode::RateLimited);
                    assert_eq!(err.message, "rate limit exceeded".to_string());
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC rate-limit test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive second NWC response within 10 seconds"),
    }
}

/// End-to-end test: spend quota allows one payment, second should be quota-exceeded.
#[tokio::test]
async fn test_nwc_pay_keysend_quota_exceeded_after_one_call() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_access_rule(&client_pubkey, Method::PayKeysend, 1_000_000, 10_000_000);
    ldk_controller::set_quota(&client_pubkey, 1, 1_000_000);

    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = PayKeysendRequest {
        id: None,
        amount: 1_000_000,
        pubkey: "02".to_string(),
        preimage: None,
        tlv_records: Vec::new(),
    };
    let request_event = Request::pay_keysend(params.clone())
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let _pay = response
                        .to_pay_keysend()
                        .expect("First response should be pay_keysend");
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive first NWC response within 10 seconds"),
    }

    let request_event = Request::pay_keysend(params)
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;

    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    assert_eq!(response.result_type, Method::PayKeysend);
                    let err = response.error.expect("Expected quota-exceeded error");
                    assert_eq!(err.code, nwc::nostr::nips::nip47::ErrorCode::QuotaExceeded);
                    assert_eq!(err.message, "quota exceeded".to_string());
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC quota test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive second NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC get_balance request, expect a valid response.
#[tokio::test]
async fn test_nwc_get_balance_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let request_event = Request::get_balance()
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC get_balance request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let balance = response
                        .to_get_balance()
                        .expect("Response was not a valid get_balance");

                    assert_eq!(balance.balance, 0);
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC get_balance roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC pay_invoice request, expect a valid response.
#[tokio::test]
async fn test_nwc_pay_invoice_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let request_event = Request::pay_invoice(PayInvoiceRequest::new("dummy_invoice"))
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC pay_invoice request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let pay = response
                        .to_pay_invoice()
                        .expect("Response was not a valid pay_invoice");

                    assert_eq!(pay.preimage, "00");
                    assert_eq!(pay.fees_paid, Some(0));
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC pay_invoice roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}


/// End-to-end test: send a NWC pay_keysend request, expect a valid response.
#[tokio::test]
async fn test_nwc_pay_keysend_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = PayKeysendRequest {
        id: None,
        amount: 1,
        pubkey: "02".to_string(),
        preimage: None,
        tlv_records: Vec::new(),
    };
    let request_event = Request::pay_keysend(params)
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC pay_keysend request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let pay = response
                        .to_pay_keysend()
                        .expect("Response was not a valid pay_keysend");

                    assert_eq!(pay.preimage, "00");
                    assert_eq!(pay.fees_paid, Some(0));
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC pay_keysend roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}


/// End-to-end test: send a NWC make_invoice request, expect a valid response.
#[tokio::test]
async fn test_nwc_make_invoice_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = MakeInvoiceRequest {
        amount: 1,
        description: None,
        description_hash: None,
        expiry: None,
    };
    let request_event = Request::make_invoice(params)
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC make_invoice request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let invoice = response
                        .to_make_invoice()
                        .expect("Response was not a valid make_invoice");

                    assert_eq!(invoice.invoice, "dummy_invoice");
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC make_invoice roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC lookup_invoice request, expect a valid response.
#[tokio::test]
async fn test_nwc_lookup_invoice_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = LookupInvoiceRequest {
        payment_hash: Some("00".to_string()),
        invoice: None,
    };
    let request_event = Request::lookup_invoice(params)
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC lookup_invoice request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let invoice = response
                        .to_lookup_invoice()
                        .expect("Response was not a valid lookup_invoice");

                    assert_eq!(invoice.payment_hash, "00");
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC lookup_invoice roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC list_transactions request, expect a valid response.
#[tokio::test]
async fn test_nwc_list_transactions_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = ListTransactionsRequest::default();
    let request_event = Request::list_transactions(params)
        .to_event(&uri)
        .expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC list_transactions request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    let list = response
                        .to_list_transactions()
                        .expect("Response was not a valid list_transactions");

                    assert!(list.is_empty());
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC list_transactions roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC make_hold_invoice request, expect a valid response.
#[tokio::test]
async fn test_nwc_make_hold_invoice_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = MakeHoldInvoiceRequest {
        amount: 1,
        description: None,
        description_hash: None,
        expiry: None,
        payment_hash: "00".to_string(),
        min_cltv_expiry_delta: None,
    };
    let request = Request {
        method: Method::MakeHoldInvoice,
        params: RequestParams::MakeHoldInvoice(params),
    };
    let request_event = request.to_event(&uri).expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC make_hold_invoice request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    assert_eq!(response.result_type, Method::MakeHoldInvoice);
                    match response.result {
                        Some(ResponseResult::MakeHoldInvoice(resp)) => {
                            assert_eq!(resp.payment_hash, "00");
                        }
                        _ => panic!("Response was not a valid make_hold_invoice"),
                    }
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC make_hold_invoice roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC cancel_hold_invoice request, expect a valid response.
#[tokio::test]
async fn test_nwc_cancel_hold_invoice_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = CancelHoldInvoiceRequest {
        payment_hash: "00".to_string(),
    };
    let request = Request {
        method: Method::CancelHoldInvoice,
        params: RequestParams::CancelHoldInvoice(params),
    };
    let request_event = request.to_event(&uri).expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC cancel_hold_invoice request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    assert_eq!(response.result_type, Method::CancelHoldInvoice);
                    match response.result {
                        Some(ResponseResult::CancelHoldInvoice(_)) => {}
                        _ => panic!("Response was not a valid cancel_hold_invoice"),
                    }
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC cancel_hold_invoice roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}

/// End-to-end test: send a NWC settle_hold_invoice request, expect a valid response.
#[tokio::test]
async fn test_nwc_settle_hold_invoice_roundtrip() -> Result<()> {
    let _guard = test_guard();
    let (_container, relay_url) = start_relay().await;

    let service_keys = Keys::generate();
    let service_pubkey = service_keys.public_key();
    let _service_client =
        ldk_controller::run_nwc_service(service_keys, &relay_url).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let client_secret = Keys::generate().secret_key().clone();
    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(
        service_pubkey,
        vec![relay],
        client_secret.clone(),
        None,
    );

    let client_keys = Keys::new(client_secret);
    let client_pubkey = client_keys.public_key().to_string();
    ldk_controller::set_owners(vec![client_pubkey.clone()]);
    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let filter = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .author(service_pubkey);
    nwc_client.subscribe(filter).await?;

    let params = SettleHoldInvoiceRequest {
        preimage: "00".to_string(),
    };
    let request = Request {
        method: Method::SettleHoldInvoice,
        params: RequestParams::SettleHoldInvoice(params),
    };
    let request_event = request.to_event(&uri).expect("Failed to create NWC request event");
    nwc_client.send_event(&request_event).await?;
    println!("Sent NWC settle_hold_invoice request, waiting for response...");

    let timeout = Duration::from_secs(10);
    let uri_clone = uri.clone();
    let result = tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response = Response::from_event(&uri_clone, event)
                        .expect("Failed to decrypt NWC response");

                    assert_eq!(response.result_type, Method::SettleHoldInvoice);
                    match response.result {
                        Some(ResponseResult::SettleHoldInvoice(_)) => {}
                        _ => panic!("Response was not a valid settle_hold_invoice"),
                    }
                    break;
                }
            }
        }
        Ok::<(), nostr_sdk::client::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            println!("NWC settle_hold_invoice roundtrip test passed!");
            Ok(())
        }
        Ok(Err(e)) => panic!("Notification handler error: {}", e),
        Err(_) => panic!("Timeout: did not receive NWC response within 10 seconds"),
    }
}
