//! Watchdog timer to detect stalled or dropped block subscriptions.
//!
//! If no sampled block is processed for `timeout`, the watchdog trips.
//! The timer only starts once `arm()` is called (when the WebSocket
//! subscription is established).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Information about a tripped watchdog condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogTripped {
    pub elapsed: Duration,
    pub timeout: Duration,
}

struct WatchdogState {
    armed: AtomicBool,
    epoch: Instant,
    last_activity_ms: AtomicU64,
}

/// Watchdog for monitoring live block processing activity.
#[derive(Clone)]
pub struct Watchdog {
    timeout: Duration,
    state: Arc<WatchdogState>,
}

impl Watchdog {
    /// Create a new watchdog with the given timeout.
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            state: Arc::new(WatchdogState {
                armed: AtomicBool::new(false),
                epoch: Instant::now(),
                last_activity_ms: AtomicU64::new(0),
            }),
        }
    }

    /// Configured timeout duration.
    #[allow(dead_code)]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Check if the watchdog has been armed.
    pub fn is_armed(&self) -> bool {
        self.state.armed.load(Ordering::SeqCst)
    }

    /// Arm the watchdog when the WebSocket subscription is established.
    ///
    /// Starts the timer. If called again (e.g. on reconnection), it preserves
    /// the ongoing timing baseline.
    pub fn arm(&self) {
        if !self.is_armed() {
            let now_ms = self.state.epoch.elapsed().as_millis() as u64;
            self.state.last_activity_ms.store(now_ms, Ordering::SeqCst);
            self.state.armed.store(true, Ordering::SeqCst);
        }
    }

    /// Reset the watchdog timer upon successfully processing a sampled block.
    pub fn notify_processed_block(&self) {
        let now_ms = self.state.epoch.elapsed().as_millis() as u64;
        self.state.last_activity_ms.store(now_ms, Ordering::SeqCst);
    }

    /// Duration elapsed since the last recorded activity, or None if not armed.
    pub fn elapsed_since_activity(&self) -> Option<Duration> {
        if !self.is_armed() {
            return None;
        }
        let last = self.state.last_activity_ms.load(Ordering::SeqCst);
        let now = self.state.epoch.elapsed().as_millis() as u64;
        Some(Duration::from_millis(now.saturating_sub(last)))
    }

    /// Seconds elapsed since the last recorded activity, or None if not armed.
    #[allow(dead_code)]
    pub fn secs_since_activity(&self) -> Option<u64> {
        self.elapsed_since_activity().map(|d| d.as_secs())
    }

    /// Run the watchdog check loop with the default 250ms interval.
    ///
    /// Resolves when the watchdog trips (i.e. `elapsed >= timeout`).
    pub async fn run(&self) -> WatchdogTripped {
        self.run_with_interval(Duration::from_millis(250)).await
    }

    /// Run the watchdog check loop with a custom check interval.
    pub async fn run_with_interval(&self, check_interval: Duration) -> WatchdogTripped {
        loop {
            tokio::time::sleep(check_interval).await;
            if self.is_armed() {
                if let Some(elapsed) = self.elapsed_since_activity() {
                    if elapsed >= self.timeout {
                        return WatchdogTripped {
                            elapsed,
                            timeout: self.timeout,
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_watchdog_unarmed_does_not_trip() {
        let wd = Watchdog::new(Duration::from_millis(40));
        assert!(!wd.is_armed());
        assert!(wd.elapsed_since_activity().is_none());

        // Wait longer than timeout without arming; select should hit timeout
        let res = tokio::select! {
            _ = wd.run_with_interval(Duration::from_millis(10)) => "tripped",
            _ = tokio::time::sleep(Duration::from_millis(60)) => "timed_out_waiting",
        };
        assert_eq!(res, "timed_out_waiting");
        assert!(!wd.is_armed());
    }

    #[tokio::test]
    async fn test_watchdog_trips_when_no_blocks_after_arm() {
        let wd = Watchdog::new(Duration::from_millis(50));
        wd.arm();
        assert!(wd.is_armed());

        let tripped = wd.run_with_interval(Duration::from_millis(10)).await;
        assert!(tripped.elapsed >= Duration::from_millis(50));
        assert_eq!(tripped.timeout, Duration::from_millis(50));
    }

    #[tokio::test]
    async fn test_watchdog_resets_on_processed_block() {
        let wd = Watchdog::new(Duration::from_millis(80));
        wd.arm();

        let wd_clone = wd.clone();
        tokio::spawn(async move {
            // Heartbeat every 25ms, 3 times = 75ms total without tripping 80ms timeout
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                wd_clone.notify_processed_block();
            }
        });

        // First 60ms: should NOT have tripped because notifications keep resetting it
        let tripped_early = tokio::select! {
            _ = wd.run_with_interval(Duration::from_millis(10)) => true,
            _ = tokio::time::sleep(Duration::from_millis(60)) => false,
        };
        assert!(!tripped_early);

        // After notifications stop, it should trip ~80ms after the last notification
        let tripped = wd.run_with_interval(Duration::from_millis(10)).await;
        assert!(tripped.elapsed >= Duration::from_millis(80));
    }
}
