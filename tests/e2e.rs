use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip47::{
    CancelHoldInvoiceRequest, MakeHoldInvoiceRequest,
    Method, NostrWalletConnectUri,
    Request, RequestParams, Response, ResponseResult, SettleHoldInvoiceRequest,
};
use std::time::Duration;

mod common;
use common::{start_relay, test_guard};

// Relay helpers live in tests/common/mod.rs

// test_hello_gets_hi_response moved to tests/hello_gets_hi.rs

// test_nwc_get_info_roundtrip moved to tests/nwc_get_info_roundtrip.rs

// test_nwc_get_info_restricted_for_non_owner moved to tests/nwc_get_info_restricted.rs

// test_nwc_get_info_allowed_for_method_access moved to tests/nwc_get_info_allowed.rs

// test_nwc_get_info_rate_limited_after_one_call moved to tests/nwc_get_info_rate_limited.rs

// test_nwc_pay_keysend_quota_exceeded_after_one_call moved to tests/nwc_pay_keysend_quota.rs

// test_nwc_get_balance_roundtrip moved to tests/nwc_get_balance_roundtrip.rs

// test_nwc_pay_invoice_roundtrip moved to tests/nwc_pay_invoice_roundtrip.rs


// test_nwc_pay_keysend_roundtrip moved to tests/nwc_pay_keysend_roundtrip.rs


// test_nwc_make_invoice_roundtrip moved to tests/nwc_make_invoice_roundtrip.rs

// test_nwc_lookup_invoice_roundtrip moved to tests/nwc_lookup_invoice_roundtrip.rs

// test_nwc_list_transactions_roundtrip moved to tests/nwc_list_transactions_roundtrip.rs

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
