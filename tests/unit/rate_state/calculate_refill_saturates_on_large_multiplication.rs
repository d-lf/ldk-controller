//! Validates overflow-safe refill math under extremely large rate/time inputs.
//! Success condition: refill does not panic and resulting balance is capped at `i64::MAX`.
//! Failure condition: panic, overflow behavior, or incorrect final balance.
use crate::{rule, RateState};

#[test]
fn calculate_refill_saturates_on_large_multiplication() {
    let mut state = RateState::new(0, 0);
    state
        .refill(u64::MAX, &rule(u64::MAX, i64::MAX))
        .expect("refill should saturate and cap");
    assert_eq!(state.balance(), i64::MAX);
}
