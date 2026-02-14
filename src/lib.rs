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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub mod usage_profile;

pub use usage_profile::{MethodAccessRule, RateLimitRule, UsageProfile};

static GLOBAL_KEYS: OnceLock<Keys> = OnceLock::new();
static RELAY_PUBKEY: OnceLock<PublicKey> = OnceLock::new();
static OWNERS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();

#[derive(Clone)]
struct RateState {
    balance: u64,
    last_refill_micros: u64,
}

struct AccessState {
    access_rate: RwLock<HashMap<(String, Method), RateState>>,   // per‑method access rate
    quota: RwLock<HashMap<String, RateState>>,            // per‑user quota rate
}

static ACCESS_STATE: OnceLock<AccessState> = OnceLock::new();
static USAGE_PROFILES: OnceLock<RwLock<HashMap<String, UsageProfile>>> = OnceLock::new();

fn set_global_keys(keys: &Keys) {
    let _ = GLOBAL_KEYS.set(keys.clone());
}

pub fn set_relay_pubkey(pubkey: PublicKey) {
    let _ = RELAY_PUBKEY.set(pubkey);
}

pub fn set_owners(owners: Vec<String>) {
    let lock = OWNERS.get_or_init(|| RwLock::new(Vec::new()));
    let mut guard = lock.write().expect("owners lock poisoned");
    *guard = owners;
}

fn access_state() -> &'static AccessState {
    ACCESS_STATE.get_or_init(|| AccessState {
        access_rate: RwLock::new(HashMap::new()),
        quota: RwLock::new(HashMap::new()),
    })
}

fn usage_profiles() -> &'static RwLock<HashMap<String, UsageProfile>> {
    USAGE_PROFILES.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn get_usage_profile(pubkey: &str) -> Option<UsageProfile> {
    let map = usage_profiles()
        .read()
        .expect("usage profile map lock poisoned");
    map.get(pubkey).cloned()
}

pub fn clear_usage_profiles() {
    let mut map = usage_profiles()
        .write()
        .expect("usage profile map lock poisoned");
    map.clear();
}

fn parse_grant_target(event: &Event) -> Option<String> {
    let d_tag = event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        if parts.get(0).map(|v| v.as_str()) == Some("d") {
            parts.get(1).cloned()
        } else {
            None
        }
    })?;
    let mut parts = d_tag.splitn(2, ':');
    let _relay_pubkey = parts.next()?;
    let user_pubkey = parts.next()?;
    if user_pubkey.is_empty() {
        None
    } else {
        Some(user_pubkey.to_string())
    }
}

fn upsert_usage_profile(target_pubkey: &str, profile: UsageProfile) {
    let mut map = usage_profiles()
        .write()
        .expect("usage profile map lock poisoned");
    map.insert(target_pubkey.to_string(), profile);
}


fn verify_access_new_new(request: &Request, event: &Event) -> Result<(), Response> {
    let caller_pubkey = event.pubkey.to_string();

    // Require a UsageProfile grant for the caller.
    let profile = {
        let map = usage_profiles()
            .read()
            .expect("usage profile map lock poisoned");
        map.get(&caller_pubkey).cloned()
    };
    let profile = profile.ok_or_else(|| unauthorized_response(&request.method))?;

    // Method authorization: missing methods means no restriction.
    let method_rule = match &profile.methods {
        None => None,
        Some(methods) => {
            if methods.is_empty() {
                return Err(access_denied_response(&request.method));
            }
            match methods.get(&request.method) {
                Some(rule) => Some(rule.clone()),
                None => return Err(access_denied_response(&request.method)),
            }
        }
    };

    let now = now_micros();

    // Prepare rate limit state updates, but only apply if all checks pass.
    let mut rate_update: Option<(RateState, RateState)> = None;
    
    
    if let Some(rule) = method_rule.as_ref().and_then(|r| r.access_rate.as_ref()) {
        let key = (caller_pubkey.clone(), request.method.clone());

        let current = {
            let access_rates = access_state()
                .access_rate
                .read()
                .expect("access state rate lock poisoned");
            access_rates
                .get(&key)
                .cloned()
                .unwrap_or(RateState {
                    balance: rule.max_capacity,
                    last_refill_micros: now,
                })
        };

        let elapsed = now.saturating_sub(current.last_refill_micros);
        let added = rule.rate_per_micro.saturating_mul(elapsed);
        let forecasted = current
            .balance
            .saturating_add(added)
            .min(rule.max_capacity);

        // check if we are below the rate
        if forecasted < 1_000_000 {
            return Err(rate_limited_response(&request.method));
        }

        // This is the new state
        let new_state = RateState {
            balance: forecasted.saturating_sub(1_000_000),
            last_refill_micros: now,
        };

        rate_update = Some((current, new_state));
    }

    // Prepare quota updates. Only apply if the request spends msats.
    let amount_msat = request_spend_msat(request).unwrap_or(0);
    let mut quota_update: Option<(RateState, RateState)> = None;
    if amount_msat > 0 {
        if let Some(quota_rule) = profile.quota.as_ref() {
            let current = {
                let quota_map = access_state()
                    .quota
                    .read()
                    .expect("quota state lock poisoned");
                
                // Assign current to the result from quota_map or default
                quota_map
                    .get(&caller_pubkey)
                    .cloned()
                    .unwrap_or(RateState {
                        balance: quota_rule.max_capacity,
                        last_refill_micros: now,
                    })
            };

            let elapsed = now.saturating_sub(current.last_refill_micros);
            let added = quota_rule.rate_per_micro.saturating_mul(elapsed);
            let forecasted = current
                .balance
                .saturating_add(added)
                .min(quota_rule.max_capacity);

            if forecasted < amount_msat {
                return Err(quota_exceeded_response(&request.method));
            }

            let new_state = RateState {
                balance: forecasted.saturating_sub(amount_msat),
                last_refill_micros: now,
            };
            quota_update = Some((current, new_state));
        }
    }

    // Apply rate and quota updates after all checks pass.
    if let Some((_, new_state)) = rate_update {
        let mut access_rate = access_state()
            .access_rate
            .write()
            .expect("access state rate lock poisoned");
        let key = (caller_pubkey.clone(), request.method.clone());
        access_rate.insert(key, new_state);
    }

    if let Some((_, new_state)) = quota_update {
        let mut quota_map = access_state()
            .quota
            .write()
            .expect("quota state lock poisoned");
        quota_map.insert(caller_pubkey.clone(), new_state);
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

fn unauthorized_response(method: &Method) -> Response {
    Response {
        result_type: method.clone(),
        error: Some(NIP47Error {
            code: ErrorCode::Unauthorized,
            message: "unauthorized".to_string(),
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

    // Subscribe to access grant updates (Kind 30078).
    let grants_filter = if let Some(relay_pubkey) = RELAY_PUBKEY.get() {
        Filter::new()
            .kind(Kind::Custom(30078))
            .pubkey(relay_pubkey.clone())
    } else {
        Filter::new().kind(Kind::Custom(30078))
    };
    client.subscribe(grants_filter.clone()).await?;

    let client_clone = client.clone();
    let grants_client = client.clone();
    let grants_filter_clone = grants_filter.clone();

    // Periodically fetch access grants in case subscription events are missed.
    tokio::spawn(async move {
        loop {
            if let Ok(events) = grants_client
                .fetch_events(grants_filter_clone.clone())
                .timeout(Duration::from_secs(2))
                .await
            {
                for event in events.iter() {
                    if let Some(target_pubkey) = parse_grant_target(event) {
                        if let Ok(profile) = serde_json::from_str::<UsageProfile>(&event.content) {
                            upsert_usage_profile(&target_pubkey, profile);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    tokio::spawn(async move {
        let mut notifications = client_clone.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                match event.kind {
                    Kind::WalletConnectRequest => {
                        if let Err(e) = handle_nwc_request(&client_clone, &keys, event).await {
                            eprintln!("Failed to handle NWC request: {}", e);
                        }
                    }
                    Kind::Custom(30078) => {
                        if let Some(target_pubkey) = parse_grant_target(event) {
                            match serde_json::from_str::<UsageProfile>(&event.content) {
                                Ok(profile) => {
                                    upsert_usage_profile(&target_pubkey, profile);
                                }
                                Err(e) => {
                                    eprintln!("Failed to parse UsageProfile: {}", e);
                                }
                            }
                        } else {
                            eprintln!("Access grant event missing or invalid d tag");
                        }
                    }
                    _ => {}
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
    if let Err(response) = verify_access_new_new(&request, event) {
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
    let mut response = handler.execute(&request).unwrap();

    // Filter get_info methods to those the caller can access.
    if response.result_type == Method::GetInfo {
        if let Some(ResponseResult::GetInfo(info)) = response.result.as_mut() {
            let caller_pubkey = event.pubkey.to_string();
            info.methods = allowed_methods_for(&caller_pubkey);
        }
    }

    response
}

fn allowed_methods_for(caller_pubkey: &str) -> Vec<Method> {
    let profile = {
        let map = usage_profiles()
            .read()
            .expect("usage profile map lock poisoned");
        map.get(caller_pubkey).cloned()
    };

    let Some(profile) = profile else {
        return Vec::new();
    };

    match profile.methods {
        None => SUPPORTED_METHODS.to_vec(),
        Some(methods) => {
            if methods.is_empty() {
                Vec::new()
            } else {
                SUPPORTED_METHODS
                    .iter()
                    .filter(|m| methods.contains_key(m))
                    .cloned()
                    .collect()
            }
        }
    }
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
