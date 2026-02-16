use crate::state::rate_state::RateState;
use nwc::nostr::nips::nip47::Method;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub(crate) enum AccessKey {
    Method { pubkey: String, method: Method },
    Quota { pubkey: String },
}

pub(crate) struct AccessState {
    pub(crate) access_rate: RwLock<HashMap<AccessKey, RateState>>, // per-method access rate
    pub(crate) quota: RwLock<HashMap<AccessKey, RateState>>,       // per-user quota rate
}

static ACCESS_STATE: OnceLock<AccessState> = OnceLock::new();

/// Returns the shared access-state store, initializing empty maps on first use.
///
/// The returned store contains independent maps for:
/// - per-method access-rate counters (`access_rate`)
/// - per-user quota counters (`quota`)
///
/// # Returns
/// A process-global `AccessState` containing the two guarded maps.
pub(crate) fn access_state() -> &'static AccessState {
    ACCESS_STATE.get_or_init(|| AccessState {
        access_rate: RwLock::new(HashMap::new()),
        quota: RwLock::new(HashMap::new()),
    })
}
