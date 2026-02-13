use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip04;
use nwc::nostr::nips::nip47::{
    CancelHoldInvoiceResponse, ErrorCode, GetBalanceResponse, GetInfoResponse,
    LookupInvoiceResponse, MakeHoldInvoiceResponse, MakeInvoiceResponse, Method, NIP47Error,
    PayInvoiceResponse, PayKeysendResponse, Request, RequestParams, Response, ResponseResult,
    SettleHoldInvoiceResponse, TransactionState, TransactionType,
};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

static GLOBAL_KEYS: OnceLock<Keys> = OnceLock::new();
static OWNERS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();
#[derive(Debug, Clone)]
struct AccessRule {
    capacity_micros: u64,
    tokens_micros: u64,
    refill_per_micro: u64,
    last_refill_micros: u64,
}

#[derive(Debug, Clone)]
struct QuotaState {
    capacity_msat: u64,
    balance_msat: u64,
    refill_per_micro: u64,
    last_refill_micros: u64,
}

static ACCESS: OnceLock<RwLock<HashMap<String, HashMap<Method, AccessRule>>>> = OnceLock::new();
static QUOTAS: OnceLock<RwLock<HashMap<String, QuotaState>>> = OnceLock::new();

fn set_global_keys(keys: &Keys) {
    let _ = GLOBAL_KEYS.set(keys.clone());
}

pub fn set_owners(owners: Vec<String>) {
    let lock = OWNERS.get_or_init(|| RwLock::new(Vec::new()));
    let mut guard = lock.write().expect("owners lock poisoned");
    *guard = owners;
}

fn owners_contains(pubkey: &str) -> bool {
    let lock = OWNERS.get_or_init(|| RwLock::new(Vec::new()));
    let guard = lock.read().expect("owners lock poisoned");
    guard.iter().any(|owner| owner == pubkey)
}

fn access() -> &'static RwLock<HashMap<String, HashMap<Method, AccessRule>>> {
    ACCESS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn quotas() -> &'static RwLock<HashMap<String, QuotaState>> {
    QUOTAS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn set_access_rule(pubkey: &str, method: Method, rate_per_micro: u64, capacity: u64) {
    let now = now_micros();
    let mut map = access().write().expect("access map lock poisoned");
    let entry = map.entry(pubkey.to_string()).or_insert_with(HashMap::new);
    entry.insert(
        method,
        AccessRule {
            capacity_micros: capacity,
            tokens_micros: capacity,
            refill_per_micro: rate_per_micro,
            last_refill_micros: now,
        },
    );
}

pub fn set_quota(pubkey: &str, refill_per_micro: u64, capacity_msat: u64) {
    let now = now_micros();
    let mut map = quotas().write().expect("quota map lock poisoned");
    map.insert(
        pubkey.to_string(),
        QuotaState {
            capacity_msat,
            balance_msat: capacity_msat,
            refill_per_micro,
            last_refill_micros: now,
        },
    );
}

fn verify_access(request: &Request, event: &Event) -> Result<(), Response> {
    let caller_pubkey = event.pubkey.to_string();

    // Owners bypass all access checks.
    if owners_contains(&caller_pubkey) {
        return Ok(());
    }

    let mut access_map = access().write().expect("access map lock poisoned");

    // Deny when the caller has no access entry.
    let methods = access_map
        .get_mut(&caller_pubkey)
        .ok_or_else(|| access_denied_response(&request.method))?;

    // Deny when the method has no access rule.
    let rule = methods
        .get_mut(&request.method)
        .ok_or_else(|| access_denied_response(&request.method))?;

    // Missing or zeroed limits are treated as rate-limited.
    // TODO: Rethink edge cases once we have JSON in play
    if rule.capacity_micros == 0 || rule.refill_per_micro == 0 {
        return Err(rate_limited_response(&request.method));
    }

    // Refill the token bucket based on elapsed micros.
    let now = now_micros();

    // Enforce quota for spend-type methods before consuming rate tokens.
    let amount_msat = request_spend_msat(request);

    let mut quota_map = quotas().write().expect("quota map lock poisoned");
    // let quota = quota_map.get_mut(&caller_pubkey);

    let mut new_balance: Option<u64> = None;

    if let Some(amount_msat) = amount_msat {
        if let Some(quota) = quota_map.get(&caller_pubkey) {
            // if quota.refill_per_micro == 0 || quota.capacity_msat == 0 {
            //     return Err(quota_exceeded_response(&request.method));
            // }

            let mut calc_balance = quota.balance_msat;

            let elapsed = now.saturating_sub(quota.last_refill_micros);

            if elapsed > 0 {
                let added = quota.refill_per_micro.saturating_mul(elapsed);
                calc_balance = calc_balance.saturating_add(added).min(quota.capacity_msat);
                // quota.balance_msat = new_balance.min(quota.capacity_msat);
                // quota.last_refill_micros = now;
            }

            if calc_balance < amount_msat {
                return Err(quota_exceeded_response(&request.method));
            }

            calc_balance -= amount_msat;
            new_balance = Some(calc_balance);
        }
    }

    // Refill

    // Of rate, if rate capped
    let elapsed = now.saturating_sub(rule.last_refill_micros);

    if elapsed > 0 {
        let added = rule.refill_per_micro.saturating_mul(elapsed);
        let new_tokens = rule.tokens_micros.saturating_add(added);
        rule.tokens_micros = new_tokens.min(rule.capacity_micros);
        rule.last_refill_micros = now;
    }

    // Require at least one token (1_000_000 micros).
    if rule.tokens_micros < 1_000_000 {
        return Err(rate_limited_response(&request.method));
    }

    // Great we have made it apply the changes

    // Update
    // Consume a token and allow the request.
    rule.tokens_micros -= 1_000_000;

    if let Some(new_balance) = new_balance {
        if let Some(quota) = quota_map.get_mut(&caller_pubkey) {
            quota.balance_msat = new_balance;
            quota.last_refill_micros = now;
        }
    }

    Ok(())
}

fn access_denied_response(method: &Method) -> Response {
    Response {
        result_type: method.clone(),
        error: Some(NIP47Error {
            code: ErrorCode::Restricted,
            message: "access denied, insufficient permission".to_string(),
        }),
        result: None,
    }
}

fn rate_limited_response(method: &Method) -> Response {
    Response {
        result_type: method.clone(),
        error: Some(NIP47Error {
            code: ErrorCode::RateLimited,
            message: "rate limit exceeded".to_string(),
        }),
        result: None,
    }
}

fn quota_exceeded_response(method: &Method) -> Response {
    Response {
        result_type: method.clone(),
        error: Some(NIP47Error {
            code: ErrorCode::QuotaExceeded,
            message: "quota exceeded".to_string(),
        }),
        result: None,
    }
}

fn request_spend_msat(request: &Request) -> Option<u64> {
    match &request.params {
        RequestParams::PayInvoice(params) => params.amount,
        RequestParams::PayKeysend(params) => Some(params.amount),
        _ => None,
    }
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
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
    Method::PayKeysend,
    Method::MakeInvoice,
    Method::LookupInvoice,
    Method::ListTransactions,
    Method::MakeHoldInvoice,
    Method::CancelHoldInvoice,
    Method::SettleHoldInvoice,
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
        Ok(Response {
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
            })),
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
            result: Some(ResponseResult::GetBalance(GetBalanceResponse {
                balance: 0,
            })),
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

struct PayKeysendHandler;

impl Handler for PayKeysendHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::PayKeysend(params) = &req.params {
            if params.pubkey.trim().is_empty() {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "pubkey is required".to_string(),
                });
            }
            if params.amount == 0 {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "amount must be greater than 0".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for pay_keysend".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::PayKeysend,
            error: None,
            result: Some(ResponseResult::PayKeysend(PayKeysendResponse {
                preimage: "00".to_string(),
                fees_paid: Some(0),
            })),
        })
    }
}

struct MakeInvoiceHandler;

impl Handler for MakeInvoiceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::MakeInvoice(params) = &req.params {
            if params.amount == 0 {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "amount must be greater than 0".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for make_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::MakeInvoice,
            error: None,
            result: Some(ResponseResult::MakeInvoice(MakeInvoiceResponse {
                invoice: "dummy_invoice".to_string(),
                payment_hash: None,
                description: None,
                description_hash: None,
                preimage: None,
                amount: None,
                created_at: None,
                expires_at: None,
            })),
        })
    }
}

struct LookupInvoiceHandler;

impl Handler for LookupInvoiceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::LookupInvoice(params) = &req.params {
            if params.payment_hash.as_deref().unwrap_or("").is_empty()
                && params.invoice.as_deref().unwrap_or("").is_empty()
            {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "payment_hash or invoice is required".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for lookup_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::LookupInvoice,
            error: None,
            result: Some(ResponseResult::LookupInvoice(LookupInvoiceResponse {
                transaction_type: Some(TransactionType::Outgoing),
                state: Some(TransactionState::Settled),
                invoice: None,
                description: None,
                description_hash: None,
                preimage: None,
                payment_hash: "00".to_string(),
                amount: 0,
                fees_paid: 0,
                created_at: Timestamp::now(),
                expires_at: None,
                settled_at: None,
                metadata: None,
            })),
        })
    }
}

struct ListTransactionsHandler;

impl Handler for ListTransactionsHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::ListTransactions(_) = &req.params {
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for list_transactions".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::ListTransactions,
            error: None,
            result: Some(ResponseResult::ListTransactions(Vec::new())),
        })
    }
}

struct MakeHoldInvoiceHandler;

impl Handler for MakeHoldInvoiceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::MakeHoldInvoice(params) = &req.params {
            if params.payment_hash.trim().is_empty() {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "payment_hash is required".to_string(),
                });
            }
            if params.amount == 0 {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "amount must be greater than 0".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for make_hold_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::MakeHoldInvoice,
            error: None,
            result: Some(ResponseResult::MakeHoldInvoice(MakeHoldInvoiceResponse {
                transaction_type: TransactionType::Incoming,
                invoice: None,
                description: None,
                description_hash: None,
                payment_hash: "00".to_string(),
                amount: 0,
                created_at: Timestamp::now(),
                expires_at: Timestamp::now(),
                metadata: None,
            })),
        })
    }
}

struct CancelHoldInvoiceHandler;

impl Handler for CancelHoldInvoiceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::CancelHoldInvoice(params) = &req.params {
            if params.payment_hash.trim().is_empty() {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "payment_hash is required".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for cancel_hold_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::CancelHoldInvoice,
            error: None,
            result: Some(ResponseResult::CancelHoldInvoice(
                CancelHoldInvoiceResponse {},
            )),
        })
    }
}

struct SettleHoldInvoiceHandler;

impl Handler for SettleHoldInvoiceHandler {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error> {
        if let RequestParams::SettleHoldInvoice(params) = &req.params {
            if params.preimage.trim().is_empty() {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "preimage is required".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for settle_hold_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request) -> Result<Response, NIP47Error> {
        Ok(Response {
            result_type: Method::SettleHoldInvoice,
            error: None,
            result: Some(ResponseResult::SettleHoldInvoice(
                SettleHoldInvoiceResponse {},
            )),
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
        handlers.insert(Method::PayKeysend, Box::new(PayKeysendHandler));
        handlers.insert(Method::MakeInvoice, Box::new(MakeInvoiceHandler));
        handlers.insert(Method::LookupInvoice, Box::new(LookupInvoiceHandler));
        handlers.insert(Method::ListTransactions, Box::new(ListTransactionsHandler));
        handlers.insert(Method::MakeHoldInvoice, Box::new(MakeHoldInvoiceHandler));
        handlers.insert(
            Method::CancelHoldInvoice,
            Box::new(CancelHoldInvoiceHandler),
        );
        handlers.insert(
            Method::SettleHoldInvoice,
            Box::new(SettleHoldInvoiceHandler),
        );
        handlers
    })
}

async fn process_nwc_request(request: Request, event: &Event) -> Response {
    // Check that the user is authorized
    if let Err(response) = verify_access(&request, event) {
        return response;
    }

    // Check that we support the requested method
    if !request_handlers().contains_key(&request.method) {
        return Response {
            result_type: request.method.clone(),
            error: Some(NIP47Error {
                code: ErrorCode::NotImplemented,
                message: format!("{} not implemented yet", request.method.as_str()),
            }),
            result: None,
        };
    }

    // Select a handler
    let handler = request_handlers().get(&request.method).unwrap();

    // Validate the request
    if let Err(e) = handler.validate(&request) {
        return Response {
            result_type: request.method.clone(),
            error: Some(e),
            result: None,
        };
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
