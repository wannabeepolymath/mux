use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;

use crate::backend::BackendWs;

/// Compute the reconnect backoff for a given attempt number.
///
/// Attempt 0 is the immediate first try (no delay). Subsequent attempts
/// follow an exponential schedule with ±25% jitter, capped at 5s.
///
/// Schedule (center values, before jitter):
///   0 → 0ms,  1 → 100ms,  2 → 250ms,  3 → 500ms,
///   4 → 1000ms, 5 → 2000ms, 6+ → 5000ms (capped)
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    let center_ms: u64 = match attempt {
        1 => 100,
        2 => 250,
        3 => 500,
        4 => 1000,
        5 => 2000,
        _ => 5000,
    };
    let jitter_ms = jitter_pct(center_ms, 25);
    Duration::from_millis(center_ms.saturating_add_signed(jitter_ms))
}

/// Symmetric jitter: returns a value in [-pct%, +pct%] of `center_ms`.
fn jitter_pct(center_ms: u64, pct: u64) -> i64 {
    use std::time::SystemTime;
    let max = (center_ms * pct / 100) as i64;
    if max == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(0);
    (nanos % (2 * max + 1)) - max
}

/// Open a WebSocket connection to a backend. Returns the warm WS or a
/// short error string. Disables Nagle for low TTFC.
///
/// Used by the per-backend lifecycle task; backoff scheduling is the
/// caller's responsibility.
pub async fn connect_once(
    addr: SocketAddr,
    connect_timeout: Duration,
) -> Result<BackendWs, String> {
    let url = format!("ws://{addr}/v1/ws/speech");
    let result =
        tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(&url)).await;
    match result {
        Ok(Ok((ws, _resp))) => {
            nodelay_backend_stream(ws.get_ref());
            Ok(ws)
        }
        Ok(Err(e)) => Err(format!("connect: {e}")),
        Err(_) => Err("connect timeout".into()),
    }
}

fn nodelay_backend_stream(stream: &MaybeTlsStream<TcpStream>) {
    if let MaybeTlsStream::Plain(tcp) = stream {
        let _ = tcp.set_nodelay(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_zero_is_immediate() {
        assert_eq!(backoff_for_attempt(0), Duration::ZERO);
    }

    #[test]
    fn schedule_is_monotonic_at_center() {
        let mut larger_count = 0;
        let trials = 100;
        for _ in 0..trials {
            let small = backoff_for_attempt(1);
            let large = backoff_for_attempt(4);
            if large > small {
                larger_count += 1;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        assert!(
            larger_count >= trials * 9 / 10,
            "expected attempt 4 > attempt 1 in ≥90% of {trials} trials, got {larger_count}",
        );
    }

    #[test]
    fn caps_at_five_seconds_plus_jitter() {
        for _ in 0..50 {
            let b = backoff_for_attempt(100);
            assert!(
                b <= Duration::from_millis(6250),
                "attempt 100 backoff {b:?} exceeds cap+jitter",
            );
            assert!(
                b >= Duration::from_millis(3750),
                "attempt 100 backoff {b:?} below cap-jitter",
            );
        }
    }

    #[test]
    fn jitter_within_bounds() {
        for _ in 0..50 {
            let b = backoff_for_attempt(2);
            assert!(
                b >= Duration::from_millis(187) && b <= Duration::from_millis(313),
                "attempt 2 backoff {b:?} outside [187, 313]",
            );
        }
    }
}
