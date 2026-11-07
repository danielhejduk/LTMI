use std::time::{Duration, Instant};

pub struct Timer {
    start: Instant,
    duration: Duration
}

impl Timer {
    pub fn new(start: Instant, duration: Duration) -> Self {
        Self {
            start, duration
        }
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.duration.saturating_sub(now.saturating_duration_since(self.start))
    }

    pub fn is_done(&self, now: Instant) -> bool {
        self.remaining(now).is_zero()
    }
}
