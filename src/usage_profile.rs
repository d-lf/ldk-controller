use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use nwc::nostr::nips::nip47::Method;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodAccessRule {
    pub access_rate: Option<RateLimitRule>,
}

// RateLimitRule represents a rate limit rule with rate per micro second and max capacity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitRule {
    #[serde(default)]
    pub rate_per_micro: u64,
    #[serde(default = "default_max_capacity")]
    pub max_capacity: u64,
}

fn default_max_capacity() -> u64 {
    u64::MAX
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageProfile {
    pub quota: Option<RateLimitRule>,
    pub methods: Option<HashMap<Method, MethodAccessRule>>,
}
