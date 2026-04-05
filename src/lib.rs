use crate::state::rate_state::RateState;
use crate::state::store::{get_access_rate_state, get_quota_state, AccessKey};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use nwc::nostr::nips::nip04;
use nostr::nips::nip44;
use nwc::nostr::nips::nip47::{
    CancelHoldInvoiceResponse, ErrorCode, GetBalanceResponse, GetInfoResponse,
    LookupInvoiceResponse, MakeHoldInvoiceResponse, MakeInvoiceResponse, Method, NIP47Error,
    PayInvoiceResponse, PayKeysendResponse, Request, RequestParams, Response, ResponseResult,
    SettleHoldInvoiceResponse, TransactionState, TransactionType,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crate::lightning::{LdkService, LdkServiceError};
pub mod rate_limit_rule;
pub mod lightning;
mod state;
pub mod usage_profile;

use crate::state::store::access_store::SharedRateState;
pub use rate_limit_rule::RateLimitRule;
pub use state::rate_state::RateStateError;
pub use usage_profile::get_usage_profile;
pub use usage_profile::{MethodAccessRule, UsageProfile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessErrorContext {
    AccessRate,
    Quota,
}

static GLOBAL_KEYS: OnceLock<Keys> = OnceLock::new();
static RELAY_PUBKEY: OnceLock<PublicKey> = OnceLock::new();
static OWNERS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();
static APPLIED_GRANT_EVENTS: OnceLock<RwLock<HashMap<String, AppliedGrantEvent>>> = OnceLock::new();
static FORCED_EXECUTE_FAILURES: OnceLock<RwLock<HashSet<Method>>> = OnceLock::new();
static LDK_SERVICE: OnceLock<RwLock<Option<Arc<LdkService>>>> = OnceLock::new();
static BITCOIND_RPC: OnceLock<BitcoindRpc> = OnceLock::new();
static FORWARDING_EVENTS: OnceLock<RwLock<Vec<ForwardingEventRecord>>> = OnceLock::new();

/// Bitcoind RPC connection info for fee estimation.
#[derive(Debug, Clone)]
pub struct BitcoindRpc {
    pub url: String,
    pub user: String,
    pub password: String,
}

/// A persisted forwarding event.
#[derive(Debug, Clone, Serialize)]
struct ForwardingEventRecord {
    timestamp: u64,
    channel_id_in: String,
    channel_id_out: String,
    amount_in_msat: u64,
    fee_msat: u64,
    status: String,
}

pub const CONTROL_REQUEST_KIND: u16 = 23196;
pub const CONTROL_RESPONSE_KIND: u16 = 23197;

#[derive(Clone)]
struct AppliedGrantEvent {
    created_at: u64,
    event_id: String,
}

fn set_global_keys(keys: &Keys) {
    let _ = GLOBAL_KEYS.set(keys.clone());
}

fn set_ldk_service(ldk_service: Arc<LdkService>) {
    let lock = LDK_SERVICE.get_or_init(|| RwLock::new(None));
    let mut guard = lock.write().expect("ldk service lock poisoned");
    *guard = Some(ldk_service);
}

fn get_ldk_service() -> Option<Arc<LdkService>> {
    let lock = LDK_SERVICE.get_or_init(|| RwLock::new(None));
    let guard = lock.read().expect("ldk service lock poisoned");
    guard.clone()
}

pub fn set_relay_pubkey(pubkey: PublicKey) {
    let _ = RELAY_PUBKEY.set(pubkey);
}

pub fn set_owners(owners: Vec<String>) {
    let lock = OWNERS.get_or_init(|| RwLock::new(Vec::new()));
    let mut guard = lock.write().expect("owners lock poisoned");
    *guard = owners;
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
    let node_pubkey = parts.next()?;
    if let Some(configured_node_pubkey) = RELAY_PUBKEY.get() {
        if node_pubkey != configured_node_pubkey.to_string() {
            return None;
        }
    }
    let user_pubkey = parts.next()?;
    if user_pubkey.is_empty() {
        None
    } else {
        Some(user_pubkey.to_string())
    }
}

fn should_apply_grant_event(target_pubkey: &str, event: &Event) -> bool {
    let created_at = event.created_at.as_secs();
    let event_id = event.id.to_string();

    let mut map = APPLIED_GRANT_EVENTS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .expect("applied grant event map lock poisoned");

    match map.get(target_pubkey) {
        None => {
            map.insert(
                target_pubkey.to_string(),
                AppliedGrantEvent {
                    created_at,
                    event_id,
                },
            );
            true
        }
        Some(existing) => {
            let should_apply = created_at > existing.created_at
                || (created_at == existing.created_at && event_id > existing.event_id);
            if should_apply {
                map.insert(
                    target_pubkey.to_string(),
                    AppliedGrantEvent {
                        created_at,
                        event_id,
                    },
                );
            }
            should_apply
        }
    }
}

pub fn clear_usage_profiles() {
    usage_profile::clear_all_usage_profiles_and_states();
}

#[doc(hidden)]
pub fn clear_access_states_for_testing() {
    usage_profile::service::clear_all_access_states();
}

#[doc(hidden)]
pub fn set_execute_failure_for_testing(method: Method, enabled: bool) {
    let lock = FORCED_EXECUTE_FAILURES.get_or_init(|| RwLock::new(HashSet::new()));
    let mut guard = lock
        .write()
        .expect("forced execute failures lock poisoned");
    if enabled {
        guard.insert(method);
    } else {
        guard.remove(&method);
    }
}

#[doc(hidden)]
pub fn clear_execute_failures_for_testing() {
    let lock = FORCED_EXECUTE_FAILURES.get_or_init(|| RwLock::new(HashSet::new()));
    let mut guard = lock
        .write()
        .expect("forced execute failures lock poisoned");
    guard.clear();
}

fn should_force_execute_failure(method: &Method) -> bool {
    let lock = FORCED_EXECUTE_FAILURES.get_or_init(|| RwLock::new(HashSet::new()));
    let guard = lock.read().expect("forced execute failures lock poisoned");
    guard.contains(method)
}

struct StateUpdateRequest {
    key: AccessKey,
    rule: RateLimitRule,
    amount: u64,
    context: AccessErrorContext,
    state: SharedRateState,
}

struct GuardedStateUpdate<'a> {
    update: &'a StateUpdateRequest,
    guard: MutexGuard<'a, RateState>,
}

fn compare_access_keys(a: &AccessKey, b: &AccessKey) -> Ordering {
    match (a, b) {
        (
            AccessKey::Method {
                pubkey: ap,
                method: am,
            },
            AccessKey::Method {
                pubkey: bp,
                method: bm,
            },
        ) => ap.cmp(bp).then_with(|| am.as_str().cmp(bm.as_str())),
        (AccessKey::Method { .. }, AccessKey::Quota { .. }) => Ordering::Less,
        (AccessKey::Quota { .. }, AccessKey::Method { .. }) => Ordering::Greater,
        (AccessKey::Quota { pubkey: ap }, AccessKey::Quota { pubkey: bp }) => ap.cmp(bp),
    }
}

fn verify_access(request: &Request, event: &Event) -> Result<Vec<StateUpdateRequest>, Response> {
    let caller_pubkey = event.pubkey.to_string();

    // Require a UsageProfile grant for the caller.
    // TODO: Cant these to be combined
    let profile = get_usage_profile(&caller_pubkey);
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

    let mut state_updates: Vec<StateUpdateRequest> = Vec::new();
    let mut guarded_updates: Vec<GuardedStateUpdate> = Vec::new();

    // Prepare rate limit state updates, but only apply if all checks pass.
    if let Some(rule) = method_rule.as_ref().and_then(|r| r.access_rate.as_ref()) {
        let amount = 1_000_000;

        let key = AccessKey::Method {
            pubkey: caller_pubkey.clone(),
            method: request.method.clone(),
        };

        let state = get_access_rate_state(&key).ok_or_else(|| {
            missing_state_response(&request.method, AccessErrorContext::AccessRate)
        })?;

        let state_update = StateUpdateRequest {
            key,
            rule: rule.clone(),
            amount,
            context: AccessErrorContext::AccessRate,
            state,
        };

        state_updates.push(state_update);
    }

    // Prepare quota updates. Only apply if the request spends msats.
    let amount_msat = request_spend_msat(request).unwrap_or(0);
    if amount_msat > 0 {
        if let Some(rule) = profile.quota.as_ref() {
            let key = AccessKey::Quota {
                pubkey: caller_pubkey.clone(),
            };

            let state = get_quota_state(&key).ok_or_else(|| {
                missing_state_response(&request.method, AccessErrorContext::Quota)
            })?;

            let state_update = StateUpdateRequest {
                key,
                rule: rule.clone(),
                amount: amount_msat,
                context: AccessErrorContext::Quota,
                state,
            };

            state_updates.push(state_update);
        }
    }

    state_updates.sort_by(|a, b| compare_access_keys(&a.key, &b.key));

    // Lock
    for state_update in &state_updates {
        let guard = state_update
            .state
            .lock()
            .map_err(|_| poisoned_state_response(&request.method, state_update.context))?;
        guarded_updates.push(GuardedStateUpdate {
            update: state_update,
            guard,
        });
    }

    // Verify
    for guarded_update in &guarded_updates {
        let state_update = guarded_update.update;
        guarded_update
            .guard
            .check_withdraw_after_refill(state_update.amount, now, &state_update.rule)
            .map_err(|e| rate_state_error_response(&request.method, &e, state_update.context))?;
    }

    // Debit
    for guarded_update in &mut guarded_updates {
        let state_update = guarded_update.update;
        guarded_update
            .guard
            .withdraw_after_refill(state_update.amount, now, &state_update.rule)
            .map_err(|e| rate_state_error_response(&request.method, &e, state_update.context))?;
    }

    drop(guarded_updates);

    Ok(state_updates)
}

fn refund_applied_state_updates(method: &Method, updates: &[StateUpdateRequest]) {
    for update in updates {
        let mut guard = match update.state.lock() {
            Ok(guard) => guard,
            Err(_) => {
                eprintln!(
                    "Failed to refund {:?}: state lock poisoned ({:?})",
                    method, update.context
                );
                continue;
            }
        };

        if let Err(error) = guard.refund(update.amount, &update.rule) {
            let mapped = map_rate_state_error(&error, update.context);
            eprintln!(
                "Failed to refund {:?}: {} ({:?})",
                method, mapped.message, mapped.code
            );
        }
    }
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

fn map_ldk_service_error(
    operation: &'static str,
    code: ErrorCode,
    error: LdkServiceError,
) -> NIP47Error {
    NIP47Error {
        code,
        message: format!("ldk {operation} failed: {error}"),
    }
}

pub fn map_rate_state_error(error: &RateStateError, context: AccessErrorContext) -> NIP47Error {
    match error {
        RateStateError::InsufficientBalance => {
            let (code, message) = match context {
                AccessErrorContext::AccessRate => (ErrorCode::RateLimited, "rate limit exceeded"),
                AccessErrorContext::Quota => (ErrorCode::QuotaExceeded, "quota exceeded"),
            };
            NIP47Error {
                code,
                message: message.to_string(),
            }
        }
        RateStateError::AmountTooLarge { .. } => NIP47Error {
            code: ErrorCode::Other,
            message: "invalid amount: exceeds i64::MAX".to_string(),
        },
        RateStateError::InvalidRule { .. } => NIP47Error {
            code: ErrorCode::Other,
            message: "invalid rate limit rule".to_string(),
        },
        RateStateError::InternalInvariantViolation => NIP47Error {
            code: ErrorCode::Other,
            message: "internal rate state error".to_string(),
        },
    }
}

fn rate_state_error_response(
    method: &Method,
    error: &RateStateError,
    context: AccessErrorContext,
) -> Response {
    Response {
        result_type: method.clone(),
        error: Some(map_rate_state_error(error, context)),
        result: None,
    }
}

fn missing_state_response(method: &Method, context: AccessErrorContext) -> Response {
    let message = match context {
        AccessErrorContext::AccessRate => "missing access rate state",
        AccessErrorContext::Quota => "missing quota state",
    };
    Response {
        result_type: method.clone(),
        error: Some(NIP47Error {
            code: ErrorCode::Other,
            message: message.to_string(),
        }),
        result: None,
    }
}

fn poisoned_state_response(method: &Method, context: AccessErrorContext) -> Response {
    let message = match context {
        AccessErrorContext::AccessRate => "access rate state lock poisoned",
        AccessErrorContext::Quota => "quota state lock poisoned",
    };
    Response {
        result_type: method.clone(),
        error: Some(NIP47Error {
            code: ErrorCode::Other,
            message: message.to_string(),
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

const SUPPORTED_CONTROL_METHODS: &[&str] = &[
    "new_onchain_address",
    "make_onchain_address",
    "connect_peer",
    "open_channel",
    "close_channel",
    "list_channels",
    "get_channel",
    "list_peers",
    "disconnect_peer",
    "get_channel_fees",
    "set_channel_fees",
    "get_forwarding_history",
    "get_onchain_transactions",
    "export_channel_backup",
    "get_pending_htlcs",
    "list_network_nodes",
    "get_network_stats",
    "get_network_node",
    "get_network_channel",
    "query_routes",
    "estimate_route_fee",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlRequest {
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenChannelParams {
    pubkey: String,
    host: String,
    port: u16,
    capacity_sats: u64,
    #[serde(default)]
    push_msat: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ConnectPeerParams {
    pubkey: String,
    host: String,
    port: u16,
}

#[derive(Debug, Clone, Deserialize)]
struct DisconnectPeerParams {
    pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GetChannelParams {
    channel_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CloseChannelParams {
    channel_id: String,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct SetChannelFeesParams {
    channel_id: String,
    base_fee_msat: Option<u32>,
    fee_rate_ppm: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct GetNetworkNodeParams {
    pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GetNetworkChannelParams {
    channel_id: String,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlError {
    code: String,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlResponse {
    result_type: String,
    result: Option<Value>,
    error: Option<ControlError>,
}

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

    // Subscribe to wallet requests addressed to us (p-tagged with our pubkey)
    let wallet_filter = Filter::new()
        .kind(Kind::WalletConnectRequest)
        .pubkey(our_pubkey);
    client.subscribe(wallet_filter).await?;

    // Subscribe to control requests addressed to us (p-tagged with our pubkey)
    let control_filter = Filter::new()
        .kind(Kind::Custom(CONTROL_REQUEST_KIND))
        .pubkey(our_pubkey);
    client.subscribe(control_filter).await?;

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
                        if !should_apply_grant_event(&target_pubkey, event) {
                            continue;
                        }
                        if let Ok(profile) = serde_json::from_str::<UsageProfile>(&event.content) {
                            usage_profile::upsert_usage_profile_and_reset_states(
                                &target_pubkey,
                                profile,
                            );
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
                if event.kind == Kind::WalletConnectRequest {
                    if let Err(e) = handle_nwc_request(&client_clone, &keys, event).await {
                        eprintln!("Failed to handle NWC request: {}", e);
                    }
                } else if event.kind == Kind::Custom(CONTROL_REQUEST_KIND) {
                    if let Err(e) = handle_control_request(&client_clone, &keys, event).await {
                        eprintln!("Failed to handle control request: {}", e);
                    }
                } else if event.kind == Kind::Custom(30078) {
                    if let Some(target_pubkey) = parse_grant_target(event) {
                        if !should_apply_grant_event(&target_pubkey, event) {
                            continue;
                        }
                        match serde_json::from_str::<UsageProfile>(&event.content) {
                            Ok(profile) => {
                                usage_profile::upsert_usage_profile_and_reset_states(
                                    &target_pubkey,
                                    profile,
                                );
                            }
                            Err(e) => {
                                eprintln!("Failed to parse UsageProfile: {}", e);
                            }
                        }
                    } else {
                        eprintln!("Access grant event missing or invalid d tag");
                    }
                }
            }
        }
    });

    Ok(client)
}

pub async fn run_nwc_service_with_ldk(
    keys: Keys,
    relay_url: &str,
    ldk_service: Arc<LdkService>,
) -> Result<Client> {
    set_ldk_service(ldk_service.clone());
    // Initialize forwarding events store
    let _ = FORWARDING_EVENTS.set(RwLock::new(Vec::new()));
    // Spawn event loop to capture forwarding events from LDK-Node
    spawn_event_loop(ldk_service);
    run_nwc_service(keys, relay_url).await
}

/// Set bitcoind RPC config for fee estimation. Call before run_nwc_service_with_ldk.
pub fn set_bitcoind_rpc(rpc: BitcoindRpc) {
    let _ = BITCOIND_RPC.set(rpc);
}

/// Spawn a background task that polls LDK-Node events and records forwarding events.
fn spawn_event_loop(ldk_service: Arc<LdkService>) {
    tokio::spawn(async move {
        loop {
            // next_event_async blocks until an event is available
            let event = ldk_service.next_event_async().await;
            if let ldk_node::Event::PaymentForwarded {
                prev_channel_id,
                next_channel_id,
                total_fee_earned_msat,
                outbound_amount_forwarded_msat,
                ..
            } = &event
            {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let fee_msat = total_fee_earned_msat.unwrap_or(0);
                let amount_msat = outbound_amount_forwarded_msat.unwrap_or(0) + fee_msat;
                let record = ForwardingEventRecord {
                    timestamp: now,
                    channel_id_in: prev_channel_id.to_string(),
                    channel_id_out: next_channel_id.to_string(),
                    amount_in_msat: amount_msat,
                    fee_msat,
                    status: "settled".to_string(),
                };
                if let Some(store) = FORWARDING_EVENTS.get() {
                    if let Ok(mut events) = store.write() {
                        events.push(record);
                        // Cap at 10000 events in memory
                        if events.len() > 10_000 {
                            events.drain(0..1000);
                        }
                    }
                }
            }
            // Mark event as handled so LDK-Node moves to the next one
            let _ = ldk_service.event_handled();
        }
    });
}

trait Handler: Send + Sync {
    fn validate(&self, req: &Request) -> Result<(), NIP47Error>;
    fn execute(&self, req: &Request, caller_pubkey: &str) -> Result<Response, NIP47Error>;
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

    fn execute(&self, _req: &Request, caller_pubkey: &str) -> Result<Response, NIP47Error> {
        let methods = allowed_methods_for(caller_pubkey);
        let (pubkey, network) = if let Some(ldk_service) = get_ldk_service() {
            (ldk_service.node_id(), ldk_service.network().to_string())
        } else {
            (
                GLOBAL_KEYS.get().unwrap().public_key().to_string(),
                "regtest".to_string(),
            )
        };
        Ok(Response {
            result_type: Method::GetInfo,
            error: None,
            result: Some(ResponseResult::GetInfo(GetInfoResponse {
                alias: Some("ldk-controller".to_string()),
                color: None,
                pubkey: Some(pubkey),
                network: Some(network),
                block_height: Some(0),
                block_hash: None,
                methods,
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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
        let balance = if let Some(ldk_service) = get_ldk_service() {
            ldk_service.sync_wallets().map_err(|e| NIP47Error {
                code: ErrorCode::Other,
                message: format!("ldk sync failed: {e}"),
            })?;
            ldk_service.get_balance_msat().map_err(|e| NIP47Error {
                code: ErrorCode::Other,
                message: format!("ldk balance failed: {e}"),
            })?
        } else {
            0
        };

        Ok(Response {
            result_type: Method::GetBalance,
            error: None,
            result: Some(ResponseResult::GetBalance(GetBalanceResponse {
                balance,
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
            if params.amount == Some(0) {
                return Err(NIP47Error {
                    code: ErrorCode::Other,
                    message: "amount must be greater than 0".to_string(),
                });
            }
            return Ok(());
        }

        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "invalid params for pay_invoice".to_string(),
        })
    }

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
        if let (Some(ldk_service), RequestParams::PayInvoice(params)) =
            (get_ldk_service(), &_req.params)
        {
            let payment = ldk_service
                .pay_invoice(&params.invoice, params.amount)
                .map_err(|e| map_ldk_service_error("pay_invoice", ErrorCode::PaymentFailed, e))?;
            return Ok(Response {
                result_type: Method::PayInvoice,
                error: None,
                result: Some(ResponseResult::PayInvoice(PayInvoiceResponse {
                    preimage: payment.preimage,
                    fees_paid: payment.fees_paid_msat,
                })),
            });
        }

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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
        if let (Some(ldk_service), RequestParams::PayKeysend(params)) =
            (get_ldk_service(), &_req.params)
        {
            let payment = ldk_service
                .pay_keysend(&params.pubkey, params.amount)
                .map_err(|e| map_ldk_service_error("pay_keysend", ErrorCode::PaymentFailed, e))?;
            return Ok(Response {
                result_type: Method::PayKeysend,
                error: None,
                result: Some(ResponseResult::PayKeysend(PayKeysendResponse {
                    preimage: payment.preimage,
                    fees_paid: payment.fees_paid_msat,
                })),
            });
        }

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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
        if let (Some(ldk_service), RequestParams::MakeInvoice(params)) =
            (get_ldk_service(), &_req.params)
        {
            let invoice = ldk_service
                .make_invoice(
                    params.amount,
                    params.description.as_deref(),
                    params.description_hash.as_deref(),
                    params.expiry,
                )
                .map_err(|e| map_ldk_service_error("make_invoice", ErrorCode::Other, e))?;
            return Ok(Response {
                result_type: Method::MakeInvoice,
                error: None,
                result: Some(ResponseResult::MakeInvoice(MakeInvoiceResponse {
                    invoice: invoice.invoice,
                    payment_hash: invoice.payment_hash,
                    description: params.description.clone(),
                    description_hash: params.description_hash.clone(),
                    preimage: None,
                    amount: invoice.amount_msat,
                    created_at: None,
                    expires_at: invoice.expires_at.map(Into::into),
                })),
            });
        }

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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
        let ldk = get_ldk_service().ok_or_else(|| NIP47Error {
            code: ErrorCode::Other,
            message: "LDK service not initialized".to_string(),
        })?;

        let txns = ldk.list_lightning_transactions();
        let responses: Vec<LookupInvoiceResponse> = txns
            .into_iter()
            .map(|tx| {
                let transaction_type = if tx.tx_type == "incoming" {
                    TransactionType::Incoming
                } else {
                    TransactionType::Outgoing
                };
                let state = match tx.status.as_str() {
                    "settled" => TransactionState::Settled,
                    "failed" => TransactionState::Failed,
                    _ => TransactionState::Pending,
                };
                let settled_at = if tx.status == "settled" {
                    Some(Timestamp::from(tx.created_at))
                } else {
                    None
                };
                LookupInvoiceResponse {
                    transaction_type: Some(transaction_type),
                    state: Some(state),
                    invoice: None,
                    description: None,
                    description_hash: None,
                    preimage: tx.preimage,
                    payment_hash: tx.payment_hash,
                    amount: tx.amount_msat,
                    fees_paid: tx.fee_msat.unwrap_or(0),
                    created_at: Timestamp::from(tx.created_at),
                    expires_at: None,
                    settled_at,
                    metadata: None,
                }
            })
            .collect();

        Ok(Response {
            result_type: Method::ListTransactions,
            error: None,
            result: Some(ResponseResult::ListTransactions(responses)),
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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
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

    fn execute(&self, _req: &Request, _caller_pubkey: &str) -> Result<Response, NIP47Error> {
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
    let _applied_state_updates = match verify_access(&request, event) {
        Ok(state_updates) => state_updates,
        Err(response) => return response,
    };

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

    let execution_result = if should_force_execute_failure(&request.method) {
        Err(NIP47Error {
            code: ErrorCode::Other,
            message: "forced execute failure for testing".to_string(),
        })
    } else {
        let request_for_exec = request.clone();
        let caller_pubkey = event.pubkey.to_string();
        let handler = &**handler;
        match tokio::task::spawn_blocking(move || {
            handler.execute(&request_for_exec, &caller_pubkey)
        })
        .await
        {
            Ok(result) => result,
            Err(e) => Err(NIP47Error {
                code: ErrorCode::Other,
                message: format!("internal execution task failed: {e}"),
            }),
        }
    };

    // Execute the request.
    match execution_result {
        Ok(response) => response,
        Err(e) => {
            refund_applied_state_updates(&request.method, &_applied_state_updates);
            Response {
                result_type: request.method.clone(),
                error: Some(e),
                result: None,
            }
        }
    }
}

fn allowed_methods_for(caller_pubkey: &str) -> Vec<Method> {
    let profile = get_usage_profile(caller_pubkey);

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

/// Handle custom NWC methods not in the nwc crate's Method enum.
/// Returns Some(json_response) if handled, None if not a custom method.
fn handle_custom_nwc_method(method: &str, params: &Value) -> Option<String> {
    match method {
        "get_balance" => {
            let Some(ldk_service) = get_ldk_service() else {
                return Some(json!({
                    "result_type": "get_balance",
                    "error": { "code": "OTHER", "message": "ldk service unavailable" }
                }).to_string());
            };
            let _ = ldk_service.sync_wallets();
            let b = ldk_service.get_detailed_balance();
            Some(json!({
                "result_type": "get_balance",
                "result": {
                    "balance": (b.channel_balance_sat * 1000),
                    "onchain_balance": b.onchain_balance_sat,
                    "channel_balance": b.channel_balance_sat,
                    "pending_balance": b.pending_balance_sat,
                }
            }).to_string())
        }
        "decode_invoice" => {
            let invoice_str = params.get("invoice").and_then(|v| v.as_str()).unwrap_or("");
            let Some(ldk_service) = get_ldk_service() else {
                return Some(json!({
                    "result_type": "decode_invoice",
                    "error": { "code": "OTHER", "message": "ldk service unavailable" }
                }).to_string());
            };
            match ldk_service.decode_invoice_str(invoice_str) {
                Ok(info) => Some(json!({
                    "result_type": "decode_invoice",
                    "result": {
                        "amount": info.amount,
                        "description": info.description,
                        "destination": info.destination,
                        "payment_hash": info.payment_hash,
                        "expiry": info.expiry,
                    }
                }).to_string()),
                Err(e) => Some(json!({
                    "result_type": "decode_invoice",
                    "error": { "code": "OTHER", "message": format!("decode_invoice failed: {e}") }
                }).to_string()),
            }
        }
        "pay_onchain" | "send_onchain" => {
            let address = params.get("address").and_then(|v| v.as_str()).unwrap_or("");
            let amount_sats = params.get("amount_sats").and_then(|v| v.as_u64()).unwrap_or(0);
            let fee_rate = params.get("fee_rate_sat_per_vbyte").and_then(|v| v.as_u64());
            let Some(ldk_service) = get_ldk_service() else {
                return Some(json!({
                    "result_type": method,
                    "error": { "code": "OTHER", "message": "ldk service unavailable" }
                }).to_string());
            };
            if amount_sats == 0 {
                return Some(json!({
                    "result_type": method,
                    "error": { "code": "OTHER", "message": "amount_sats must be > 0" }
                }).to_string());
            }
            match ldk_service.send_to_address(address, amount_sats, fee_rate) {
                Ok(txid) => Some(json!({
                    "result_type": method,
                    "result": { "txid": txid }
                }).to_string()),
                Err(e) => Some(json!({
                    "result_type": method,
                    "error": { "code": "OTHER", "message": format!("{method} failed: {e}") }
                }).to_string()),
            }
        }
        "get_fee_estimates" => {
            let estimates = query_fee_estimates();
            Some(json!({
                "result_type": "get_fee_estimates",
                "result": estimates,
            }).to_string())
        }
        "make_onchain_address" | "new_onchain_address" => {
            let Some(ldk_service) = get_ldk_service() else {
                return Some(json!({
                    "result_type": method,
                    "error": { "code": "OTHER", "message": "ldk service unavailable" }
                }).to_string());
            };
            match ldk_service.new_onchain_address() {
                Ok(address) => Some(json!({
                    "result_type": method,
                    "result": { "address": address }
                }).to_string()),
                Err(e) => Some(json!({
                    "result_type": method,
                    "error": { "code": "OTHER", "message": format!("{e}") }
                }).to_string()),
            }
        }
        _ => None,
    }
}

async fn handle_nwc_request(
    client: &Client,
    keys: &Keys,
    event: &Event,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sender_pubkey = event.pubkey;

    // Decrypt: try NIP-44 first (modern), fall back to NIP-04 (legacy)
    let (decrypted, use_nip44) = match nip44::decrypt(keys.secret_key(), &sender_pubkey, &event.content) {
        Ok(d) => (d, true),
        Err(_) => (nip04::decrypt(keys.secret_key(), &sender_pubkey, &event.content)?, false),
    };

    // Check for custom/enriched methods first (handles both standard methods
    // that need extra fields like get_balance, and custom methods not in the nwc crate)
    let raw: Value = serde_json::from_str(&decrypted).unwrap_or(Value::Null);
    let method_str = raw.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = raw.get("params").cloned().unwrap_or(Value::Object(Default::default()));

    let response_json = if let Some(custom_resp) = handle_custom_nwc_method(method_str, &params) {
        custom_resp
    } else {
        // Fall back to standard NWC handler for methods not in custom handler
        match Request::from_json(&decrypted) {
            Ok(request) => {
                let response = process_nwc_request(request, event).await;
                response.as_json()
            }
            Err(_) => {
                json!({
                    "result_type": method_str,
                    "error": { "code": "NOT_IMPLEMENTED", "message": format!("unknown method: {method_str}") }
                }).to_string()
            }
        }
    };

    // Encrypt response with same NIP version the request used
    let encrypted = if use_nip44 {
        nip44::encrypt(keys.secret_key(), &sender_pubkey, response_json, nip44::Version::V2)?
    } else {
        nip04::encrypt(keys.secret_key(), &sender_pubkey, response_json)?
    };

    // Build and send the response event (Kind 23195)
    let response_event = EventBuilder::new(Kind::WalletConnectResponse, encrypted)
        .tag(Tag::public_key(sender_pubkey))
        .tag(Tag::event(event.id));

    client.send_event_builder(response_event).await?;

    Ok(())
}

fn control_error(code: &str, message: String) -> ControlError {
    ControlError {
        code: code.to_string(),
        message,
    }
}

/// Query bitcoind for fee estimates via JSON-RPC. Returns sensible defaults on failure.
fn query_fee_estimates() -> Value {
    let defaults = json!({
        "economy_fee": 1,
        "normal_fee": 5,
        "priority_fee": 10,
    });

    let Some(rpc) = BITCOIND_RPC.get() else {
        return defaults;
    };

    // Query estimatesmartfee for 3 targets: 2 blocks (priority), 6 blocks (normal), 25 blocks (economy)
    let targets = [(2, "priority_fee"), (6, "normal_fee"), (25, "economy_fee")];
    let mut result = serde_json::Map::new();

    for (target, key) in &targets {
        let body = json!({
            "jsonrpc": "1.0",
            "id": "fee",
            "method": "estimatesmartfee",
            "params": [target],
        });

        let fee_rate = std::thread::spawn({
            let url = rpc.url.clone();
            let user = rpc.user.clone();
            let password = rpc.password.clone();
            let body_str = body.to_string();
            move || -> Option<f64> {
                // Blocking HTTP request to bitcoind
                let client = reqwest::blocking::Client::new();
                let resp = client
                    .post(&url)
                    .basic_auth(&user, Some(&password))
                    .header("Content-Type", "application/json")
                    .body(body_str)
                    .send()
                    .ok()?;
                let json: Value = resp.json().ok()?;
                // estimatesmartfee returns BTC/kvB — convert to sat/vB
                let btc_per_kvb = json.get("result")?.get("feerate")?.as_f64()?;
                Some((btc_per_kvb * 100_000.0).round()) // BTC/kvB → sat/vB
            }
        })
        .join()
        .ok()
        .flatten()
        .unwrap_or(match *key {
            "priority_fee" => 10.0,
            "normal_fee" => 5.0,
            _ => 1.0,
        });

        result.insert(key.to_string(), json!(fee_rate as u64));
    }

    Value::Object(result)
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = match chunk.len() {
            3 => [chunk[0], chunk[1], chunk[2]],
            2 => [chunk[0], chunk[1], 0],
            _ => [chunk[0], 0, 0],
        };
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 { result.push(CHARS[((n >> 6) & 63) as usize] as char); } else { result.push('='); }
        if chunk.len() > 2 { result.push(CHARS[(n & 63) as usize] as char); } else { result.push('='); }
    }
    result
}

fn authorize_control_method(caller_pubkey: &str, method: &str) -> Result<(), ControlError> {
    let profile = get_usage_profile(caller_pubkey)
        .ok_or_else(|| control_error("UNAUTHORIZED", "unauthorized".to_string()))?;

    let Some(control_methods) = profile.control else {
        return Err(control_error(
            "RESTRICTED",
            "control access denied, missing control permissions".to_string(),
        ));
    };

    if control_methods.is_empty() || !control_methods.contains_key(method) {
        return Err(control_error(
            "RESTRICTED",
            "control access denied, insufficient permission".to_string(),
        ));
    }

    Ok(())
}

fn process_control_request(request: ControlRequest, caller_pubkey: &str) -> ControlResponse {
    let method = request.method.clone();

    if let Err(error) = authorize_control_method(caller_pubkey, &request.method) {
        return ControlResponse {
            result_type: method,
            result: None,
            error: Some(error),
        };
    }

    if !SUPPORTED_CONTROL_METHODS
        .iter()
        .any(|method| *method == request.method)
    {
        return ControlResponse {
            result_type: request.method.clone(),
            result: None,
            error: Some(control_error(
                "NOT_IMPLEMENTED",
                format!("unknown control method: {}", request.method),
            )),
        };
    }

    if request.method == "open_channel" {
        let params = match serde_json::from_value::<OpenChannelParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error(
                        "OTHER",
                        format!("invalid open_channel params: {e}"),
                    )),
                };
            }
        };
        if params.capacity_sats == 0 {
            return ControlResponse {
                result_type: "open_channel".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    "capacity_sats must be greater than 0".to_string(),
                )),
            };
        }
        let Some(ldk_service) = get_ldk_service() else {
            return ControlResponse {
                result_type: "open_channel".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    "ldk service unavailable".to_string(),
                )),
            };
        };

        let address = format!("{}:{}", params.host, params.port);
        if let Err(e) = ldk_service.open_channel(
            &params.pubkey,
            &address,
            params.capacity_sats,
            params.push_msat,
        ) {
            return ControlResponse {
                result_type: "open_channel".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    format!("open_channel failed: {e}"),
                )),
            };
        }

        return ControlResponse {
            result_type: "open_channel".to_string(),
            result: Some(json!({ "status": "accepted" })),
            error: None,
        };
    }

    if request.method == "new_onchain_address" || request.method == "make_onchain_address" {
        let Some(ldk_service) = get_ldk_service() else {
            return ControlResponse {
                result_type: "new_onchain_address".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    "ldk service unavailable".to_string(),
                )),
            };
        };
        return match ldk_service.new_onchain_address() {
            Ok(address) => ControlResponse {
                result_type: "new_onchain_address".to_string(),
                result: Some(json!({ "address": address })),
                error: None,
            },
            Err(e) => ControlResponse {
                result_type: "new_onchain_address".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    format!("new_onchain_address failed: {e}"),
                )),
            },
        };
    }

    if request.method == "connect_peer" {
        let params = match serde_json::from_value::<ConnectPeerParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error(
                        "OTHER",
                        format!("invalid connect_peer params: {e}"),
                    )),
                };
            }
        };
        let Some(ldk_service) = get_ldk_service() else {
            return ControlResponse {
                result_type: "connect_peer".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    "ldk service unavailable".to_string(),
                )),
            };
        };
        let address = format!("{}:{}", params.host, params.port);
        if let Err(e) = ldk_service.connect_peer(&params.pubkey, &address) {
            return ControlResponse {
                result_type: "connect_peer".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    format!("connect_peer failed: {e}"),
                )),
            };
        }
        return ControlResponse {
            result_type: "connect_peer".to_string(),
            result: Some(json!({ "status": "connected" })),
            error: None,
        };
    }

    if request.method == "disconnect_peer" {
        let params = match serde_json::from_value::<DisconnectPeerParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error(
                        "OTHER",
                        format!("invalid disconnect_peer params: {e}"),
                    )),
                };
            }
        };
        let Some(ldk_service) = get_ldk_service() else {
            return ControlResponse {
                result_type: "disconnect_peer".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    "ldk service unavailable".to_string(),
                )),
            };
        };
        if let Err(e) = ldk_service.disconnect_peer(&params.pubkey) {
            return ControlResponse {
                result_type: "disconnect_peer".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    format!("disconnect_peer failed: {e}"),
                )),
            };
        }
        return ControlResponse {
            result_type: "disconnect_peer".to_string(),
            result: Some(json!({ "status": "disconnected" })),
            error: None,
        };
    }

    if request.method == "list_channels" {
        let channels = if let Some(ldk_service) = get_ldk_service() {
            ldk_service.list_channels()
        } else {
            Vec::new()
        };
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "channels": channels })),
            error: None,
        };
    }

    if request.method == "list_peers" {
        let peers = if let Some(ldk_service) = get_ldk_service() {
            ldk_service.list_peers()
        } else {
            Vec::new()
        };
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "peers": peers })),
            error: None,
        };
    }

    if request.method == "get_channel" {
        let params = match serde_json::from_value::<GetChannelParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error(
                        "OTHER",
                        format!("invalid get_channel params: {e}"),
                    )),
                };
            }
        };
        let Some(ldk_service) = get_ldk_service() else {
            return ControlResponse {
                result_type: "get_channel".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    "ldk service unavailable".to_string(),
                )),
            };
        };
        return match ldk_service.get_channel(&params.channel_id) {
            Some(channel) => ControlResponse {
                result_type: "get_channel".to_string(),
                result: Some(serde_json::to_value(channel).unwrap_or(Value::Null)),
                error: None,
            },
            None => ControlResponse {
                result_type: "get_channel".to_string(),
                result: None,
                error: Some(control_error(
                    "NOT_FOUND",
                    format!("channel not found: {}", params.channel_id),
                )),
            },
        };
    }

    if request.method == "close_channel" {
        let params = match serde_json::from_value::<CloseChannelParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error(
                        "OTHER",
                        format!("invalid close_channel params: {e}"),
                    )),
                };
            }
        };
        let Some(ldk_service) = get_ldk_service() else {
            return ControlResponse {
                result_type: "close_channel".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    "ldk service unavailable".to_string(),
                )),
            };
        };
        if let Err(e) = ldk_service.close_channel(&params.channel_id, params.force) {
            return ControlResponse {
                result_type: "close_channel".to_string(),
                result: None,
                error: Some(control_error(
                    "OTHER",
                    format!("close_channel failed: {e}"),
                )),
            };
        }
        return ControlResponse {
            result_type: "close_channel".to_string(),
            result: Some(json!({ "status": "accepted", "force": params.force })),
            error: None,
        };
    }

    // ─── Fee management ───────────────────────────────────────────────

    if request.method == "get_channel_fees" {
        let channels = if let Some(ldk_service) = get_ldk_service() {
            ldk_service.list_channels()
        } else {
            Vec::new()
        };
        let fees: Vec<Value> = channels
            .iter()
            .map(|ch| {
                json!({
                    "channel_id": ch.channel_id,
                    "remote_pubkey": ch.counterparty_pubkey,
                    "base_fee_msat": ch.base_fee_msat,
                    "fee_rate_ppm": ch.fee_rate_ppm,
                    "time_lock_delta": ch.cltv_expiry_delta,
                    "min_htlc_msat": ch.inbound_htlc_minimum_msat,
                    "max_htlc_msat": ch.inbound_htlc_maximum_msat,
                })
            })
            .collect();
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "channel_fees": fees })),
            error: None,
        };
    }

    if request.method == "set_channel_fees" {
        let params = match serde_json::from_value::<SetChannelFeesParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error("OTHER", format!("invalid set_channel_fees params: {e}"))),
                };
            }
        };
        let Some(ldk_service) = get_ldk_service() else {
            return ControlResponse {
                result_type: "set_channel_fees".to_string(),
                result: None,
                error: Some(control_error("OTHER", "ldk service unavailable".to_string())),
            };
        };
        if let Err(e) = ldk_service.update_channel_fees(
            &params.channel_id,
            params.base_fee_msat,
            params.fee_rate_ppm,
        ) {
            return ControlResponse {
                result_type: "set_channel_fees".to_string(),
                result: None,
                error: Some(control_error("OTHER", format!("set_channel_fees failed: {e}"))),
            };
        }
        return ControlResponse {
            result_type: "set_channel_fees".to_string(),
            result: Some(json!({ "status": "updated" })),
            error: None,
        };
    }

    // ─── Forwarding / HTLCs ──────────────────────────────────────────

    if request.method == "get_forwarding_history" {
        let events = FORWARDING_EVENTS
            .get()
            .and_then(|store| store.read().ok())
            .map(|events| events.clone())
            .unwrap_or_default();
        let total = events.len();
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "forwarding_events": events, "total_events": total })),
            error: None,
        };
    }

    if request.method == "get_pending_htlcs" {
        // LDK-Node doesn't expose pending HTLCs
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "pending_htlcs": [], "total_count": 0, "current_block_height": 0 })),
            error: None,
        };
    }

    // ─── Onchain transactions ────────────────────────────────────────

    if request.method == "get_onchain_transactions" {
        let txns = if let Some(ldk_service) = get_ldk_service() {
            ldk_service.list_onchain_transactions()
        } else {
            Vec::new()
        };
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "transactions": txns })),
            error: None,
        };
    }

    // ─── Channel backup ──────────────────────────────────────────────

    if request.method == "export_channel_backup" {
        let channels = if let Some(ldk_service) = get_ldk_service() {
            ldk_service.list_channels()
        } else {
            Vec::new()
        };
        let backup_json = serde_json::to_string(&channels).unwrap_or_else(|_| "[]".to_string());
        let data = base64_encode(backup_json.as_bytes());
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({
                "format": "json",
                "data": data,
                "filename": "channel-backup.json",
            })),
            error: None,
        };
    }

    // ─── Network graph ───────────────────────────────────────────────

    if request.method == "list_network_nodes" {
        let nodes = if let Some(ldk_service) = get_ldk_service() {
            ldk_service.list_graph_nodes()
        } else {
            Vec::new()
        };
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "nodes": nodes })),
            error: None,
        };
    }

    if request.method == "get_network_stats" {
        let stats = if let Some(ldk_service) = get_ldk_service() {
            Some(ldk_service.get_graph_stats())
        } else {
            None
        };
        return ControlResponse {
            result_type: request.method,
            result: Some(serde_json::to_value(stats.unwrap_or_else(|| crate::lightning::GraphStats {
                total_nodes: 0,
                total_channels: 0,
                total_capacity_sats: 0,
                our_pubkey: String::new(),
                our_channel_count: 0,
                our_capacity_sat: 0,
            })).unwrap_or(json!({}))),
            error: None,
        };
    }

    if request.method == "get_network_node" {
        let params = match serde_json::from_value::<GetNetworkNodeParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error("OTHER", format!("invalid params: {e}"))),
                };
            }
        };
        let node = get_ldk_service().and_then(|s| s.get_graph_node(&params.pubkey));
        return match node {
            Some(n) => ControlResponse {
                result_type: "get_network_node".to_string(),
                result: Some(serde_json::to_value(n).unwrap_or(json!({}))),
                error: None,
            },
            None => ControlResponse {
                result_type: "get_network_node".to_string(),
                result: Some(json!({ "pubkey": params.pubkey })),
                error: None,
            },
        };
    }

    if request.method == "get_network_channel" {
        let params = match serde_json::from_value::<GetNetworkChannelParams>(request.params.clone()) {
            Ok(params) => params,
            Err(e) => {
                return ControlResponse {
                    result_type: request.method,
                    result: None,
                    error: Some(control_error("OTHER", format!("invalid params: {e}"))),
                };
            }
        };
        let scid = params.channel_id.parse::<u64>().unwrap_or(0);
        let channel = get_ldk_service().and_then(|s| s.get_graph_channel(scid));
        return match channel {
            Some(c) => ControlResponse {
                result_type: "get_network_channel".to_string(),
                result: Some(serde_json::to_value(c).unwrap_or(json!({}))),
                error: None,
            },
            None => ControlResponse {
                result_type: "get_network_channel".to_string(),
                result: Some(json!({ "channel_id": params.channel_id })),
                error: None,
            },
        };
    }

    // ─── Route queries (not exposed by LDK-Node) ────────────────────

    if request.method == "query_routes" {
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "routes": [] })),
            error: None,
        };
    }

    if request.method == "estimate_route_fee" {
        return ControlResponse {
            result_type: request.method,
            result: Some(json!({ "fee_sat": 0, "fee_msat": 0 })),
            error: None,
        };
    }

    ControlResponse {
        result_type: request.method,
        result: None,
        error: Some(control_error(
            "NOT_IMPLEMENTED",
            "control method not implemented yet".to_string(),
        )),
    }
}

async fn handle_control_request(
    client: &Client,
    keys: &Keys,
    event: &Event,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sender_pubkey = event.pubkey;

    // Decrypt: try NIP-44 first (modern), fall back to NIP-04 (legacy)
    let (decrypted, use_nip44) = match nip44::decrypt(keys.secret_key(), &sender_pubkey, &event.content) {
        Ok(d) => (d, true),
        Err(_) => (nip04::decrypt(keys.secret_key(), &sender_pubkey, &event.content)?, false),
    };

    let response = match serde_json::from_str::<ControlRequest>(&decrypted) {
        Ok(request) => process_control_request(request, &sender_pubkey.to_string()),
        Err(e) => ControlResponse {
            result_type: "unknown".to_string(),
            result: None,
            error: Some(control_error(
                "OTHER",
                format!("invalid control request payload: {e}"),
            )),
        },
    };

    let response_json = serde_json::to_string(&response)?;
    let encrypted = if use_nip44 {
        nip44::encrypt(keys.secret_key(), &sender_pubkey, response_json, nip44::Version::V2)?
    } else {
        nip04::encrypt(keys.secret_key(), &sender_pubkey, response_json)?
    };
    let response_event = EventBuilder::new(Kind::Custom(CONTROL_RESPONSE_KIND), encrypted)
        .tag(Tag::public_key(sender_pubkey))
        .tag(Tag::event(event.id));
    client.send_event_builder(response_event).await?;
    Ok(())
}
