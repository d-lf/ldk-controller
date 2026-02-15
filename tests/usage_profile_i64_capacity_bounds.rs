use ldk_controller::RateLimitRule;

#[test]
fn rate_limit_rule_accepts_i64_max_capacity() {
    let json = format!(
        "{{\"rate_per_micro\":1,\"max_capacity\":{}}}",
        i64::MAX
    );
    let rule: RateLimitRule = serde_json::from_str(&json).expect("rule should deserialize");
    assert_eq!(rule.max_capacity, i64::MAX as u64);
}

#[test]
fn rate_limit_rule_default_capacity_is_i64_max() {
    let rule: RateLimitRule = serde_json::from_str("{}").expect("rule should deserialize");
    assert_eq!(rule.max_capacity, i64::MAX as u64);
}

#[test]
fn rate_limit_rule_rejects_negative_capacity() {
    let json = "{\"rate_per_micro\":1,\"max_capacity\":-1}";
    let result = serde_json::from_str::<RateLimitRule>(json);
    assert!(result.is_err(), "negative capacity must be rejected");
}

#[test]
fn rate_limit_rule_rejects_capacity_above_i64_max() {
    let json = format!(
        "{{\"rate_per_micro\":1,\"max_capacity\":{}}}",
        (i64::MAX as u128) + 1
    );
    let result = serde_json::from_str::<RateLimitRule>(&json);
    assert!(
        result.is_err(),
        "capacity above i64::MAX must be rejected"
    );
}
