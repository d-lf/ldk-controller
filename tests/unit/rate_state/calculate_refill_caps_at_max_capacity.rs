//! Validates refill capping behavior when computed refill exceeds `max_capacity`.
//! Success condition: refill completes and resulting balance equals `max_capacity`.
//! Failure condition: refill errors or resulting balance is above/below expected cap.
use crate::{rule, RateState};

#[test]
fn calculate_refill_caps_at_max_capacity() {
    let mut state = RateState::new(90, 0);
    state.refill(20, &rule(2, 100)).expect("refill should work");
    assert_eq!(state.balance(), 100);
}
