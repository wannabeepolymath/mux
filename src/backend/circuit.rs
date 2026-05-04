use std::time::{Duration, Instant};

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation, requests flow through.
    Closed,
    /// Too many failures, blocking requests until cooldown elapses.
    Open,
    /// Cooldown elapsed, allowing a single probe request.
    HalfOpen,
}

/// Cooldown schedule multipliers, applied to the configured base cooldown.
/// Index by `consecutive_opens.saturating_sub(1)` and saturate at the last entry.
///
/// With base = 2s (default), this yields [2s, 5s, 15s, 30s, 60s].
const COOLDOWN_MULTIPLIERS: [f32; 5] = [1.0, 2.5, 7.5, 15.0, 30.0];

/// Tracks consecutive failures and manages circuit state transitions.
#[derive(Debug)]
pub struct CircuitBreaker {
    state: CircuitState,
    consecutive_failures: u32,
    /// Number of times the circuit has opened in a row, without an intervening
    /// successful close. Drives the cooldown multiplier index.
    consecutive_opens: u32,
    threshold: u32,
    base_cooldown: Duration,
    opened_at: Option<Instant>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, base_cooldown: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            consecutive_opens: 0,
            threshold,
            base_cooldown,
            opened_at: None,
        }
    }

    pub fn state(&self) -> CircuitState {
        self.state
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Cooldown that applies to the *current* (or last) open. Reflects the
    /// exponential schedule based on `consecutive_opens`.
    pub fn current_cooldown(&self) -> Duration {
        let idx = self
            .consecutive_opens
            .saturating_sub(1)
            .min((COOLDOWN_MULTIPLIERS.len() - 1) as u32) as usize;
        let mult = COOLDOWN_MULTIPLIERS[idx];
        Duration::from_secs_f64(self.base_cooldown.as_secs_f64() * mult as f64)
    }

    /// Non-mutating: returns true iff state is `Open` and the current cooldown
    /// has elapsed. Lets callers distinguish a "stale connect failure during
    /// cooldown" from "this was the recovery probe."
    pub fn cooldown_elapsed(&self) -> bool {
        matches!(self.state, CircuitState::Open)
            && self
                .opened_at
                .map_or(false, |t| t.elapsed() >= self.current_cooldown())
    }

    /// Returns true if a request may be attempted on this backend.
    pub fn can_attempt(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                let Some(opened) = self.opened_at else {
                    return false;
                };
                if opened.elapsed() >= self.current_cooldown() {
                    self.state = CircuitState::HalfOpen;
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Record a successful request — resets failure counter, closes circuit,
    /// resets the consecutive-opens counter so the next open starts at base cooldown.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.consecutive_opens = 0;
        self.state = CircuitState::Closed;
        self.opened_at = None;
    }

    /// Record a failed request — increments counter; opens (or reopens) the
    /// circuit if the threshold is reached. Bumps `consecutive_opens` exactly
    /// once per Open transition so the cooldown index advances.
    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.threshold {
            // Open or reopen. Bump consecutive_opens for cooldown escalation.
            self.consecutive_opens = self.consecutive_opens.saturating_add(1);
            self.state = CircuitState::Open;
            self.opened_at = Some(Instant::now());
        }
    }

    /// Record a failed *connect probe* while the circuit is Open: the connect
    /// task spawned after a cooldown elapsed never produced a usable warm slot.
    /// Bumps `consecutive_opens` and re-arms the cooldown timestamp so the next
    /// cooldown is longer.
    ///
    /// Caller must check `state() == Open` before calling.
    pub fn record_open_failure(&mut self) {
        self.consecutive_opens = self.consecutive_opens.saturating_add(1);
        self.opened_at = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_closed() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(2));
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.can_attempt());
    }

    #[test]
    fn opens_after_threshold() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(2));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(!cb.can_attempt());
    }

    #[test]
    fn first_open_uses_base_cooldown() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(2));
        for _ in 0..3 {
            cb.record_failure();
        }
        // index 0 → multiplier 1.0 → 2s
        assert_eq!(cb.current_cooldown(), Duration::from_secs(2));
    }

    #[test]
    fn second_open_uses_index_one_cooldown() {
        let mut cb = CircuitBreaker::new(3, Duration::from_millis(10));
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(15));
        assert!(cb.can_attempt()); // → HalfOpen
        cb.record_failure(); // 4th failure: HalfOpen → Open, consecutive_opens=2
        assert_eq!(cb.state(), CircuitState::Open);
        // index 1 → multiplier 2.5 → 25ms (with base 10ms)
        assert_eq!(cb.current_cooldown(), Duration::from_millis(25));
    }

    #[test]
    fn cooldown_caps_at_last_multiplier() {
        let mut cb = CircuitBreaker::new(1, Duration::from_secs(2));
        // Force consecutive_opens way past schedule length.
        for _ in 0..10 {
            cb.record_failure();
        }
        // Last multiplier 30.0 × 2s = 60s
        assert_eq!(cb.current_cooldown(), Duration::from_secs(60));
    }

    #[test]
    fn record_success_resets_consecutive_opens() {
        let mut cb = CircuitBreaker::new(3, Duration::from_millis(10));
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(15));
        cb.can_attempt(); // → HalfOpen
        cb.record_failure(); // → Open, consecutive_opens=2

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        // Next open starts at base cooldown again.
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.current_cooldown(), Duration::from_millis(10));
    }

    #[test]
    fn record_open_failure_advances_cooldown_without_consec_failures() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(2));
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.current_cooldown(), Duration::from_secs(2));
        cb.record_open_failure();
        assert_eq!(cb.current_cooldown(), Duration::from_secs(5)); // 2.5×
        cb.record_open_failure();
        assert_eq!(cb.current_cooldown(), Duration::from_secs(15)); // 7.5×
    }

    #[test]
    fn half_open_after_cooldown() {
        let mut cb = CircuitBreaker::new(3, Duration::from_millis(10));
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);
        std::thread::sleep(Duration::from_millis(15));
        assert!(cb.can_attempt());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn half_open_failure_reopens_with_longer_cooldown() {
        let mut cb = CircuitBreaker::new(3, Duration::from_millis(10));
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(15));
        cb.can_attempt(); // → HalfOpen
        cb.record_failure(); // 4th failure, reopens
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.current_cooldown(), Duration::from_millis(25)); // index 1
    }
}
