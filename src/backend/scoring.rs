use std::collections::VecDeque;

const EWMA_ALPHA: f64 = 0.3;
const WINDOW_SIZE: usize = 20;

/// Tracks per-backend performance metrics for adaptive routing.
#[derive(Debug)]
pub struct BackendScoring {
    /// Exponentially weighted moving average of time-to-first-chunk (ms).
    ttfc_ewma_ms: f64,
    /// Sliding window of recent request outcomes (true = success).
    recent_results: VecDeque<bool>,
    /// Total requests processed (lifetime).
    pub total_requests: u64,
}

impl BackendScoring {
    pub fn new() -> Self {
        Self {
            ttfc_ewma_ms: 0.0,
            recent_results: VecDeque::with_capacity(WINDOW_SIZE),
            total_requests: 0,
        }
    }

    /// Current TTFC EWMA in milliseconds.
    pub fn ttfc_ewma_ms(&self) -> f64 {
        self.ttfc_ewma_ms
    }

    /// Error rate over the last WINDOW_SIZE requests (0.0 to 1.0).
    pub fn error_rate(&self) -> f64 {
        if self.recent_results.is_empty() {
            return 0.0;
        }
        let errors = self.recent_results.iter().filter(|&&ok| !ok).count();
        errors as f64 / self.recent_results.len() as f64
    }

    /// Record a time-to-first-chunk observation.
    pub fn record_ttfc(&mut self, ms: f64) {
        if self.ttfc_ewma_ms == 0.0 {
            self.ttfc_ewma_ms = ms;
        } else {
            self.ttfc_ewma_ms = EWMA_ALPHA * ms + (1.0 - EWMA_ALPHA) * self.ttfc_ewma_ms;
        }
    }

    /// Record a request outcome (success or failure).
    pub fn record_result(&mut self, success: bool) {
        self.total_requests += 1;
        self.recent_results.push_back(success);
        if self.recent_results.len() > WINDOW_SIZE {
            self.recent_results.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let s = BackendScoring::new();
        assert_eq!(s.ttfc_ewma_ms(), 0.0);
        assert_eq!(s.error_rate(), 0.0);
        assert_eq!(s.total_requests, 0);
    }

    #[test]
    fn ttfc_ewma_first_value() {
        let mut s = BackendScoring::new();
        s.record_ttfc(100.0);
        assert!((s.ttfc_ewma_ms() - 100.0).abs() < 0.01);
    }

    #[test]
    fn ttfc_ewma_converges() {
        let mut s = BackendScoring::new();
        for _ in 0..50 {
            s.record_ttfc(200.0);
        }
        assert!((s.ttfc_ewma_ms() - 200.0).abs() < 1.0);
    }

    #[test]
    fn error_rate_all_success() {
        let mut s = BackendScoring::new();
        for _ in 0..10 {
            s.record_result(true);
        }
        assert_eq!(s.error_rate(), 0.0);
    }

    #[test]
    fn error_rate_mixed() {
        let mut s = BackendScoring::new();
        for _ in 0..8 {
            s.record_result(true);
        }
        for _ in 0..2 {
            s.record_result(false);
        }
        assert!((s.error_rate() - 0.2).abs() < 0.01);
    }

    #[test]
    fn record_result_does_not_touch_ttfc_ewma() {
        // Guard: TTFC EWMA must stay strictly tied to successful-stream
        // timing. record_result() (success or failure) updates the error-rate
        // window only — not the latency signal.
        let mut s = BackendScoring::new();
        s.record_ttfc(120.0);
        let baseline = s.ttfc_ewma_ms();
        for _ in 0..10 {
            s.record_result(false);
        }
        for _ in 0..10 {
            s.record_result(true);
        }
        assert_eq!(s.ttfc_ewma_ms(), baseline);
    }

    #[test]
    fn sliding_window_evicts() {
        let mut s = BackendScoring::new();
        // Fill window with failures
        for _ in 0..20 {
            s.record_result(false);
        }
        assert_eq!(s.error_rate(), 1.0);

        // Push in 20 successes — old failures should be evicted
        for _ in 0..20 {
            s.record_result(true);
        }
        assert_eq!(s.error_rate(), 0.0);
    }
}
