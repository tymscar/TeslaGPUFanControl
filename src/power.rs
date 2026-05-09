//! Pure helpers for the Power Limit feature (ADR-0008).

use std::time::{Duration, Instant};

pub fn due(last_check: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last_check {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn first_call_is_always_due() {
        let now = Instant::now();
        assert!(due(None, now, Duration::from_secs(120)));
    }

    #[test]
    fn not_due_when_inside_interval() {
        let last = Instant::now();
        let now = last + Duration::from_secs(30);
        assert!(!due(Some(last), now, Duration::from_secs(120)));
    }

    #[test]
    fn due_at_exact_interval_boundary() {
        // The drift check must fire at t = last + interval (not t > last + interval).
        // Otherwise an upward drift sits unprotected for one extra poll cycle.
        let last = Instant::now();
        let now = last + Duration::from_secs(120);
        assert!(due(Some(last), now, Duration::from_secs(120)));
    }

    #[test]
    fn due_after_interval_elapsed() {
        let last = Instant::now();
        let now = last + Duration::from_secs(125);
        assert!(due(Some(last), now, Duration::from_secs(120)));
    }
}
