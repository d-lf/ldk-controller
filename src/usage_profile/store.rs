use crate::usage_profile::UsageProfile;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

static USAGE_PROFILES: OnceLock<RwLock<HashMap<String, UsageProfile>>> = OnceLock::new();

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

pub(crate) fn upsert_usage_profile(target_pubkey: &str, profile: UsageProfile) {
    let mut map = usage_profiles()
        .write()
        .expect("usage profile map lock poisoned");
    map.insert(target_pubkey.to_string(), profile);
}
