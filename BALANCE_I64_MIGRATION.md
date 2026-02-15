# Balance `i64` Migration Checklist

1. Decide invariant first: can `balance` be negative or not.
If `no`, keep a hard rule `balance >= 0` and reject underflow before subtracting.
If `yes`, define minimum allowed debt and error behavior.

2. Update `RateState` type surface in `src/state/rate_state.rs:5`.
Change field `balance: u64` to `i64`.
Then update constructor/getter signatures in `src/state/rate_state.rs:14` and `src/state/rate_state.rs:21`.

3. Rework refill math in `src/state/rate_state.rs:25`.
`elapsed`/`added` are currently `u64`, so convert to `i64` safely (`try_from`) before adding to `balance`.
Keep cap logic with `max_capacity`, but compare using same signed type.

4. Handle `RateLimitRule.max_capacity` conversion in `src/state/rate_state.rs:10` and `src/state/rate_state.rs:28`.
`max_capacity` is currently `u64` (`src/usage_profile.rs:18`), so conversion can overflow `i64` for large values.
Define behavior for `> i64::MAX` (reject profile, clamp, or fail request).

5. Update withdraw API and arithmetic in `src/state/rate_state.rs:31`.
`amount` is currently `u64`; either:
- keep `amount: u64` and convert with `i64::try_from`, or
- change to `i64` and validate non-negative amounts.
Remove unsigned-only helpers (`saturating_sub` on `u64`) and replace with signed-safe logic matching your invariant.

6. Update state transition entrypoint in `src/lib.rs:95`.
`calculate_new_state` currently takes `amount: &u64`; align it with the new withdraw type/validation strategy.

7. Update call sites in `src/lib.rs:174` and `src/lib.rs:184`.
Method token amount (`1_000_000`) and `amount_msat` are effectively unsigned today.
Add conversion/error handling before calling state transitions.

8. Define overflow/error mapping for access control responses.
If conversion fails (`u64 -> i64`), decide whether to return:
- `rate limit exceeded` / `quota exceeded`, or
- a more explicit invalid-input/internal error path.

9. Decide whether `RateLimitRule` should stay `u64` or also move to `i64` (`src/usage_profile.rs:16-18`).
If only `balance` changes, you need boundary conversions everywhere.
If rule fields also change, you must update serde/tests and external profile compatibility.

10. Add/adjust tests before rollout.
At minimum:
- refill near `i64::MAX`,
- withdraw exact balance,
- insufficient balance path,
- conversion overflow from very large `u64` inputs/rules,
- behavior when negative balances are attempted/allowed.
Relevant tests: `tests/nwc_get_info_rate_limited.rs`, `tests/nwc_pay_keysend_quota.rs`, `tests/usage_profile.rs`.
