use std::time::{Duration, Instant};

pub struct Timer {
    start: Instant,
    duration: Duration,
}

impl Timer {
    pub fn new(start: Instant, duration: Duration) -> Self {
        Self { start, duration }
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.duration.saturating_sub(now.saturating_duration_since(self.start))
    }

    /// Fraction of the duration elapsed at `now`, from 0.0 to 1.0.
    pub fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start);
        (elapsed.as_secs_f64() / self.duration.as_secs_f64()).min(1.0)
    }

    pub fn is_done(&self, now: Instant) -> bool {
        self.remaining(now).is_zero()
    }
}
