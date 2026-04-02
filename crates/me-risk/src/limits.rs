use std::time::Instant;

/// Token-bucket rate limiter.
/// Allows `capacity` tokens per second, refilled continuously.
pub struct RateLimiter {
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(max_per_second: u32) -> Self {
        let cap = max_per_second as f64;
        Self {
            capacity: cap,
            tokens: cap,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume one token. Returns true if allowed.
    pub fn try_acquire(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.capacity).min(self.capacity);
        self.last_refill = now;
    }

    /// Update the rate limit.
    pub fn set_rate(&mut self, max_per_second: u32) {
        self.capacity = max_per_second as f64;
    }
}

/// Risk rejection reasons (separate from matching engine RejectReason).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskRejectReason {
    /// Trading is halted for this symbol.
    TradingHalted,
    /// Order exceeds maximum quantity (fat finger).
    ExceedsMaxOrderQty,
    /// Order exceeds maximum notional value.
    ExceedsMaxNotional,
    /// Order price outside price band.
    PriceBandViolation,
    /// Rate limit exceeded.
    RateLimitExceeded,
    /// Maximum open orders exceeded.
    MaxOpenOrdersExceeded,
    /// Would exceed maximum position.
    ExceedsMaxPosition,
}

impl std::fmt::Display for RiskRejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TradingHalted => write!(f, "trading_halted"),
            Self::ExceedsMaxOrderQty => write!(f, "exceeds_max_order_qty"),
            Self::ExceedsMaxNotional => write!(f, "exceeds_max_notional"),
            Self::PriceBandViolation => write!(f, "price_band_violation"),
            Self::RateLimitExceeded => write!(f, "rate_limit_exceeded"),
            Self::MaxOpenOrdersExceeded => write!(f, "max_open_orders_exceeded"),
            Self::ExceedsMaxPosition => write!(f, "exceeds_max_position"),
        }
    }
}
