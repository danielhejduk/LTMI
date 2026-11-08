use std::time::{Duration, Instant};

/// A countdown that only runs while it is not paused.
pub struct Timer {
    duration: Duration,
    /// Time counted in earlier running stretches.
    banked: Duration,
    /// Start of the current running stretch, `None` while paused.
    running_since: Option<Instant>,
}

impl Timer {
    /// A running timer that started at `start`.
    pub fn new(start: Instant, duration: Duration) -> Self {
        Self {
            duration,
            banked: Duration::ZERO,
            running_since: Some(start),
        }
    }

    /// Time counted up to `now`; it never decreases.
    fn elapsed(&self, now: Instant) -> Duration {
        let current = self
            .running_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        self.banked + current
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.duration.saturating_sub(self.elapsed(now))
    }

    /// Fraction of the duration elapsed at `now`, from 0.0 to 1.0.
    pub fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (self.elapsed(now).as_secs_f64() / self.duration.as_secs_f64()).min(1.0)
    }

    pub fn is_done(&self, now: Instant) -> bool {
        self.remaining(now).is_zero()
    }

    /// Stop counting at `now`. Does nothing if already paused.
    pub fn pause(&mut self, now: Instant) {
        if let Some(since) = self.running_since.take() {
            self.banked += now.saturating_duration_since(since);
        }
    }

    /// Start counting again from `now`. Does nothing if already running.
    pub fn resume(&mut self, now: Instant) {
        self.running_since.get_or_insert(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn time_while_paused_is_not_counted() {
        let t0 = Instant::now();
        let mut timer = Timer::new(t0, secs(60));
        timer.pause(t0 + secs(10));
        timer.resume(t0 + secs(40));
        assert_eq!(timer.remaining(t0 + secs(50)), secs(40));
    }

    #[test]
    fn paused_timer_stands_still() {
        let t0 = Instant::now();
        let mut timer = Timer::new(t0, secs(60));
        timer.pause(t0 + secs(15));
        assert_eq!(timer.remaining(t0 + secs(100)), secs(45));
        assert_eq!(timer.progress(t0 + secs(100)), 0.25);
        assert!(!timer.is_done(t0 + secs(100)));
    }

    #[test]
    fn pausing_twice_counts_the_stretch_once() {
        let t0 = Instant::now();
        let mut timer = Timer::new(t0, secs(60));
        timer.pause(t0 + secs(10));
        timer.pause(t0 + secs(20));
        timer.resume(t0 + secs(30));
        assert_eq!(timer.remaining(t0 + secs(40)), secs(40));
    }

    #[test]
    fn resuming_a_running_timer_changes_nothing() {
        let t0 = Instant::now();
        let mut timer = Timer::new(t0, secs(60));
        timer.resume(t0 + secs(10));
        assert_eq!(timer.remaining(t0 + secs(20)), secs(40));
    }
}
