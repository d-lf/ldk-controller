use crate::state::rate_state::RateState;
use crate::state::store::{get_access_rate_state, get_quota_state, AccessKey};
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip04;
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
use crate::lightning::LdkService;
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
    let _relay_pubkey = parts.next()?;
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
                match event.kind {
                    Kind::WalletConnectRequest => {
                        if let Err(e) = handle_nwc_request(&client_clone, &keys, event).await {
                            eprintln!("Failed to handle NWC request: {}", e);
                        }
                    }
                    Kind::Custom(30078) => {
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
                    _ => {}
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
    set_ldk_service(ldk_service);
    run_nwc_service(keys, relay_url).await
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
                .map_err(|e| NIP47Error {
                    code: ErrorCode::PaymentFailed,
                    message: format!("ldk pay_invoice failed: {e}"),
                })?;
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
                .map_err(|e| NIP47Error {
                    code: ErrorCode::PaymentFailed,
                    message: format!("ldk pay_keysend failed: {e}"),
                })?;
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
                .map_err(|e| NIP47Error {
                    code: ErrorCode::Other,
                    message: format!("ldk make_invoice failed: {e}"),
                })?;
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
        handler.execute(&request, &event.pubkey.to_string())
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
