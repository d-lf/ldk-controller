# Balance `i64` Migration Checklist

## Completed

1. `RateLimitRule.max_capacity` moved to `i64`.
`src/rate_limit_rule.rs`

2. JSON bounds are enforced in `RateLimitRule` deserialization.
Current rule: `0 <= max_capacity <= i64::MAX`.
`src/rate_limit_rule.rs`

3. Default `max_capacity` is now `i64::MAX`.
`src/rate_limit_rule.rs`

4. Runtime conversion points were updated where rate state still uses `u64`.
`max_capacity` is converted via `u64::try_from(...)` before use.
`src/state/rate_state.rs`

5. Dedicated JSON bounds tests were added in a subfolder structure.
`tests/rate_limit_rule_json_bounds.rs`
`tests/rate_limit_rule_json_bounds/positive.rs`
`tests/rate_limit_rule_json_bounds/negative.rs`

6. Signed-balance invariant decided.
`balance` may go negative, but only through a dedicated `withdraw_force` path.

7. `RateState.balance` moved from `u64` to `i64`.
`src/state/rate_state.rs`

8. `RateState` arithmetic moved to signed balance semantics.
- `refill` adds with signed saturation and caps at `rule.max_capacity`
- `withdraw` checks insufficient balance using signed comparison
- `withdraw_force` can push balance below zero (no debt floor)
`src/state/rate_state.rs`

## Remaining For Full `balance: i64` Migration

1. Define the force-withdraw debt policy.
Decision: `withdraw_force` has no debt floor (negative balance is unbounded).

2. Update state transition entrypoint.
`calculate_new_state` currently takes `amount: &u64`; align with the chosen withdraw strategy.
`src/lib.rs`

3. Update call sites.
Method token amount (`1_000_000`) and `amount_msat` are unsigned today; add conversion/error handling for the signed path.
`src/lib.rs`

4. Define overflow/error mapping for access control responses.
If conversion fails, decide whether to return rate/quota errors or a dedicated invalid-input/internal error.
`src/lib.rs`

5. Add/adjust tests for signed balance behavior.
At minimum:
- refill near `i64::MAX`
- withdraw exact balance
- insufficient balance
- conversion overflow from very large unsigned inputs
- behavior for attempted/allowed negative balances

Relevant existing tests:
`tests/nwc_get_info_rate_limited.rs`
`tests/nwc_pay_keysend_quota.rs`
`tests/usage_profile.rs`
