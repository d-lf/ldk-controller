use crate::state::rate_state::RateState;
use crate::AccessKey;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

pub(crate) struct AccessState {
    pub(crate) access_rate: RwLock<HashMap<AccessKey, RateState>>, // per-method access rate
    pub(crate) quota: RwLock<HashMap<AccessKey, RateState>>,       // per-user quota rate
}

static ACCESS_STATE: OnceLock<AccessState> = OnceLock::new();

pub(crate) fn access_state() -> &'static AccessState {
    ACCESS_STATE.get_or_init(|| AccessState {
        access_rate: RwLock::new(HashMap::new()),
        quota: RwLock::new(HashMap::new()),
    })
}
