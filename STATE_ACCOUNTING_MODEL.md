# State Accounting Model Summary

## Decisions Made

1. `RateLimitRule.max_capacity` uses `i64`.
2. JSON bounds for `max_capacity` are enforced to `0..=i64::MAX`.
3. `RateState.balance` uses `i64`.
4. Negative balance is forbidden.
5. Debt floor: `0` (no balance below zero).
6. `i64` is retained for compatibility with `RateLimitRule.max_capacity` and simpler signed conversion paths, while policy still enforces `balance >= 0`.

## Error Mapping Decisions

`RateStateError` is used internally and mapped to NIP47 errors:

- `InsufficientBalance`
  - Access-rate path -> `ErrorCode::RateLimited`
  - Quota path -> `ErrorCode::QuotaExceeded`
- `AmountTooLarge` -> `ErrorCode::Other` (`"invalid amount: exceeds i64::MAX"`)
- `InvalidRule` -> `ErrorCode::Other` (`"invalid rate limit rule"`)
- `InternalInvariantViolation` -> `ErrorCode::Other` (`"internal rate state error"`)

## Test Structure Decisions

JSON bounds and error-mapping tests are organized in subfolders:

- `tests/rate_limit_rule_json_bounds.rs`
- `tests/rate_limit_rule_json_bounds/positive.rs`
- `tests/rate_limit_rule_json_bounds/negative.rs`
- `tests/rate_state_error_codes.rs`
- `tests/rate_state_error_codes/mapping.rs`

## Lifecycle and Concurrency Direction

1. State is ephemeral and reconstructed from relay/profile data on startup.
2. State creation/reset is profile-driven only:
   - states are created only when profiles/rules are upserted
   - old states are removed when profile rules are replaced/removed
   - access handling must not lazily create missing states
3. On profile/rule updates from the Nostr relay, corresponding states are recreated:
   - new rule -> create corresponding state
   - replaced/old rule state -> delete previous state
   - counters are reset on update
4. Accounting is per-attempt, not per logical payment intent.
5. For correctness under concurrency, updates should happen with state locked:
   - check (compute projected balance after simulated refill and verify projected balance >= amount)
   - refill
   - debit
   - refund on fail
6. Preferred direction discussed:
   - map lock for structural changes
   - per-state lock for mutation (`Arc<Mutex<RateState>>` style)

## Accounting Policy Agreed

1. Reserve/debit at access acceptance (after check passes).
2. On execution failure, refund the reserved/debited amount.
3. Refund is capped by `max_capacity`.
4. Pre-check rejections are not refunded (no debit should occur).

## Risk List Review Status

1. Ambiguous timeout outcome: treated as not applicable for current LN flow assumptions.
2. Retry/idempotency duplicates: accepted by policy (per-attempt accounting).
3. Crash between reserve/finalize: accepted via startup reset policy.
4. Validate/commit interleaving: must be handled by locked in-place update model with non-negative balance enforcement.
5. Failure/refund policy: defined.
6. Hostile abuse/churn controls: intentionally out of scope for now.
7. Lock performance under load: active item to measure.
8. Reconciliation complexity: reduced by ephemeral-state policy.

## Performance Assumption and Trigger Point

Working assumption:

- `check(simulated refill) + refill + debit/refund` is fast on modern hardware.
- Contention is the likely bottleneck, not arithmetic.

Rule of thumb:

- For a hot key, if `arrival_rate * lock_hold_time >= 0.7`, latency rises sharply.
- At `>= 1.0`, sustained queueing/timeouts are likely.

Practical target:

- Keep p99 state-lock hold time under about `100-200us`.
- Keep per-hot-key utilization under about `50-70%`.

## Tracking Issue

Performance investigation task:

- https://github.com/dukeh3/ldk-controller/issues/36
