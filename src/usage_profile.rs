use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use nwc::nostr::nips::nip47::Method;
use crate::RateLimitRule;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodAccessRule {
    pub access_rate: Option<RateLimitRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageProfile {
    pub quota: Option<RateLimitRule>,
    pub methods: Option<HashMap<Method, MethodAccessRule>>,
}
