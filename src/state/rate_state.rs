use crate::RateLimitRule;

#[derive(Clone)]
pub(crate) struct RateState {
    balance: u64,
    last_refill_micros: u64,
}

impl RateState {
    pub(crate) fn from_rule(now: u64, rule: &RateLimitRule) -> Self {
        Self::new(rule.max_capacity, now)
    }

    pub(crate) fn new(balance: u64, last_refill_micros: u64) -> Self {
        Self {
            balance,
            last_refill_micros,
        }
    }

    pub(crate) fn balance(&self) -> u64 {
        self.balance
    }

    pub(crate) fn refill(&mut self, now: u64, rule: &RateLimitRule) {
        let elapsed = now.saturating_sub(self.last_refill_micros);
        let added = rule.rate_per_micro.saturating_mul(elapsed);
        self.balance = self.balance.saturating_add(added).min(rule.max_capacity);
    }

    pub(crate) fn withdraw(&mut self, amount: u64) -> Result<(), String> {
        if (self.balance < amount) {
            return Err("Insufficient balance".to_string());
        }

        self.balance = self.balance.saturating_sub(amount);

        Ok(())
    }
}
