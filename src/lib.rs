use std::collections::HashMap;
use std::sync::OnceLock;
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip04;
use nwc::nostr::nips::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, Method, NIP47Error, PayInvoiceResponse, Request,
    RequestParams, Response, ResponseResult,
};

static GLOBAL_KEYS: OnceLock<Keys> = OnceLock::new();

fn set_global_keys(keys: &Keys) {
    let _ = GLOBAL_KEYS.set(keys.clone());
}

/// Connects to a nostr relay, subscribes to text notes, and responds
/// "Hi" to any message containing "hello".
///
/// The `client` is returned by reference so the caller (main or tests)
/// retains access to it for shutdown or further interaction.
pub async fn run_client(keys: Keys, relay_url: &str) -> Result<Client> {
    set_global_keys(&keys);
    let client = Client::builder().signer(keys).build();
    client.add_relay(relay_url).await?;
    println!("Connecting to relay {}...", relay_url);
    client.connect().await;
    println!("Connected!");

    let filter = Filter::new().kind(Kind::TextNote);
    client.subscribe(filter).await?;
    println!("Subscribed to text notes. Listening for events...\n");

    // Clone the client so we can use it inside the notification handler
    // to publish responses. The original client is returned to the caller.
    let client_clone = client.clone();

    // Spawn the notification loop in a background task so this function
    // returns immediately. The caller can keep using the client while
    // events are being handled in the background.
    tokio::spawn(async move {
        let mut notifications = client_clone.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
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
        }
    });

    Ok(client)
}

/// Methods this wallet currently supports.
const SUPPORTED_METHODS: &[Method] = &[
    Method::GetInfo,
    Method::GetBalance,
    Method::PayInvoice,
    Method::MultiPayInvoice,
];

/// Starts a NWC (Nostr Wallet Connect) service that listens for NIP-47
/// requests and responds to them.
///
/// On startup, publishes a Kind 13194 (WalletConnectInfo) event advertising
/// supported methods. Then listens for Kind 23194 requests and responds.
///
/// Currently handles `get_info` requests with stub data. Other request
/// types receive a `NotImplemented` error response.
pub async fn run_nwc_service(keys: Keys, relay_url: &str) -> Result<Client> {
    set_global_keys(&keys);
    let client = Client::builder().signer(keys.clone()).build();
    client.add_relay(relay_url).await?;
    client.connect().await;

    // Publish capabilities (Kind 13194) — space-separated list of supported methods
    let methods_str: String = SUPPORTED_METHODS
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let info_event = EventBuilder::new(Kind::WalletConnectInfo, methods_str);
    client.send_event_builder(info_event).await?;

    let our_pubkey = keys.public_key();

    // Subscribe to NWC requests addressed to us (p-tagged with our pubkey)
    let filter = Filter::new()
        .kind(Kind::WalletConnectRequest)
        .pubkey(our_pubkey);
    client.subscribe(filter).await?;

    let client_clone = client.clone();

    tokio::spawn(async move {
        let mut notifications = client_clone.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectRequest {
                    if let Err(e) = handle_nwc_request(&client_clone, &keys, event).await {
                        eprintln!("Failed to handle NWC request: {}", e);
                    }
                }
            }
        }
    });

    Ok(client)
}

trait Handler: Send + Sync {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error>;
    fn execute(&self, req: &Request) -> Result<Response, NIP47Error>;
}

struct GetInfoHandler;

impl Handler for GetInfoHandler {
    fn validate(&self, _req: &Request) -> Result<(), NIP47Error> {
        if _req.params != RequestParams::GetInfo {
            return Err(NIP47Error {
                code: ErrorCode::Other,
                message: "invalid params for get_info".to_string(),
            });
        }
        Ok(())
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> { 
        Ok(
            Response {
        result_type: Method::GetInfo,
        error: None,
        result: Some(ResponseResult::GetInfo(GetInfoResponse {
            alias: Some("ldk-controller".to_string()),
            color: None,
            pubkey: Some(GLOBAL_KEYS.get().unwrap().public_key().to_string()),
            network: Some("regtest".to_string()),
            block_height: Some(0),
            block_hash: None,
            methods: SUPPORTED_METHODS.to_vec(),
            notifications: vec![],
            })
        ),
    })    
    }
}

struct GetBalanceHandler;

impl Handler for GetBalanceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if req.params != RequestParams::GetBalance {
            return Err(NIP47Error {
                code: ErrorCode::Other,
                message: "invalid params for get_balance".to_string(),
            });
        }
        Ok(())
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::GetBalance,
            error: None,
            result: Some(ResponseResult::GetBalance(GetBalanceResponse { balance: 0 })),
        })
    }
}

struct PayInvoiceHandler;

impl Handler for PayInvoiceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::PayInvoice(params) = &req.params {
            if params.invoice.trim().is_empty() {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "invoice is required".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for pay_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::PayInvoice,
            error: None,
            result: Some(ResponseResult::PayInvoice(PayInvoiceResponse {
                preimage: "00".to_string(),
                fees_paid: Some(0),
            })),
        })
    }
}

struct MultiPayInvoiceHandler;

impl Handler for MultiPayInvoiceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::MultiPayInvoice(params) = &req.params {
            if params.invoices.is_empty() {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "invoices list is required".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for multi_pay_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::MultiPayInvoice,
            error: None,
            result: Some(ResponseResult::MultiPayInvoice(PayInvoiceResponse {
                preimage: "00".to_string(),
                fees_paid: Some(0),
            })),
        })
    }
}

// Lazily initialize a static handler map to avoid rebuilding it per request.
fn request_handlers() -> &'static HashMap<Method, Box<dyn Handler + Send + Sync>> {
    static HANDLERS: OnceLock<HashMap<Method, Box<dyn Handler + Send + Sync>>> = OnceLock::new();

    HANDLERS.get_or_init(|| {
        let mut handlers: HashMap<Method, Box<dyn Handler + Send + Sync>> = HashMap::new();
        handlers.insert(Method::GetInfo, Box::new(GetInfoHandler));
        handlers.insert(Method::GetBalance, Box::new(GetBalanceHandler));
        handlers.insert(Method::PayInvoice, Box::new(PayInvoiceHandler));
        handlers.insert(Method::MultiPayInvoice, Box::new(MultiPayInvoiceHandler));
        handlers
    })
}

async fn process_nwc_request(request: Request, event: &Event) -> Response {

    // Check that the user is authorized
    println!("Here I will checking permissions for {}...", event.pubkey.to_bech32().unwrap_or_default());

    // Check that we support the requested method
    if ! request_handlers().contains_key(&request.method) {
        return Response {
            result_type: request.method.clone(),
            error: Some(NIP47Error {
                code: ErrorCode::NotImplemented,
                message: format!("{} not implemented yet", request.method.as_str()),
            }),
            result: None,
        }
    }

    // Select a handler
    let handler = request_handlers().get(&request.method).unwrap();

    // Validate the request
    if let Err(e) = handler.validate(&request) {
        return Response {
            result_type: request.method.clone(),
            error: Some(e),
            result: None,
        }
    }

    // Execute the request
    handler.execute(&request).unwrap()
}

async fn handle_nwc_request(
    client: &Client,
    keys: &Keys,
    event: &Event,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sender_pubkey = event.pubkey;

    // Decrypt the NIP-04 encrypted request content
    let decrypted = nip04::decrypt(keys.secret_key(), &sender_pubkey, &event.content)?;

    let request = Request::from_json(&decrypted)?;

    let response = process_nwc_request(request, event).await;
    //
    //
    //
    // let response = match request.method {
    //     Method::GetInfo => Response {
    //         result_type: Method::GetInfo,
    //         error: None,
    //         result: Some(ResponseResult::GetInfo(GetInfoResponse {
    //             alias: Some("ldk-controller".to_string()),
    //             color: None,
    //             pubkey: Some(keys.public_key().to_string()),
    //             network: Some("regtest".to_string()),
    //             block_height: Some(0),
    //             block_hash: None,
    //             methods: SUPPORTED_METHODS.to_vec(),
    //             notifications: vec![],
    //         })),
    //     },
    //     other => Response {
    //         result_type: other.clone(),
    //         error: Some(NIP47Error {
    //             code: ErrorCode::NotImplemented,
    //             message: format!("{} not implemented yet", other.as_str()),
    //         }),
    //         result: None,
    //     },
    // };

    // Encrypt the response for the sender
    let response_json = response.as_json();
    let encrypted = nip04::encrypt(keys.secret_key(), &sender_pubkey, response_json)?;

    // Build and send the response event (Kind 23195)
    let response_event = EventBuilder::new(Kind::WalletConnectResponse, encrypted)
        .tag(Tag::public_key(sender_pubkey))
        .tag(Tag::event(event.id));

    client.send_event_builder(response_event).await?;

    Ok(())
}
