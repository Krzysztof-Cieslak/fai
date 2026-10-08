//! Idle expiration measured from the end of the final active request.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Synchronizes request admission with automatic idle shutdown.
pub(crate) struct IdleTimer {
    state: Mutex<State>,
}

struct State {
    active: usize,
    idle_since: Instant,
}

impl State {
    fn finish(&mut self, now: Instant) {
        debug_assert!(self.active > 0);
        self.active -= 1;
        if self.active == 0 {
            self.idle_since = now;
        }
    }

    fn expired(&self, now: Instant, limit: Duration) -> bool {
        self.active == 0 && now.saturating_duration_since(self.idle_since) >= limit
    }
}

impl IdleTimer {
    pub(crate) fn new(now: Instant) -> Self {
        Self { state: Mutex::new(State { active: 0, idle_since: now }) }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn begin(&self) -> RequestActivity<'_> {
        self.lock().active += 1;
        RequestActivity(self)
    }

    /// The shutdown decision and request admission share a lock, so new work
    /// cannot begin between observing an idle daemon and committing its exit.
    pub(crate) fn expire_if_idle(&self, limit: Duration, shutdown: impl FnOnce()) {
        let state = self.lock();
        if state.expired(Instant::now(), limit) {
            shutdown();
        }
        drop(state);
    }
}

/// Keeps the daemon active until the complete request/response operation ends.
pub(crate) struct RequestActivity<'a>(&'a IdleTimer);

impl Drop for RequestActivity<'_> {
    fn drop(&mut self) {
        self.0.lock().finish(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_work_cannot_expire() {
        let start = Instant::now();
        let state = State { active: 1, idle_since: start };
        assert!(!state.expired(start + Duration::from_secs(100), Duration::from_secs(1)));
    }

    #[test]
    fn idle_time_starts_when_the_last_request_finishes() {
        let start = Instant::now();
        let mut state = State { active: 2, idle_since: start };
        state.finish(start + Duration::from_secs(20));
        assert!(!state.expired(start + Duration::from_secs(100), Duration::from_secs(1)));
        state.finish(start + Duration::from_secs(100));
        assert!(!state.expired(start + Duration::from_secs(104), Duration::from_secs(5)));
        assert!(state.expired(start + Duration::from_secs(105), Duration::from_secs(5)));
    }

    #[test]
    fn startup_idle_time_is_bounded() {
        let start = Instant::now();
        let state = State { active: 0, idle_since: start };
        assert!(!state.expired(start + Duration::from_secs(4), Duration::from_secs(5)));
        assert!(state.expired(start + Duration::from_secs(5), Duration::from_secs(5)));
    }

    #[test]
    fn request_activity_is_balanced_when_processing_unwinds() {
        let timer = IdleTimer::new(Instant::now());
        let result = std::panic::catch_unwind(|| {
            let _active = timer.begin();
            panic!("request failed");
        });
        assert!(result.is_err());
        assert_eq!(timer.lock().active, 0);
    }

    #[test]
    fn expiration_holds_the_request_admission_lock() {
        let timer = IdleTimer::new(Instant::now());
        let mut expired = false;
        timer.expire_if_idle(Duration::ZERO, || {
            expired = true;
            assert!(timer.state.try_lock().is_err());
        });
        assert!(expired);
    }

    #[test]
    fn live_request_guards_prevent_shutdown_until_dropped() {
        let timer = IdleTimer::new(Instant::now());
        let first = timer.begin();
        let second = timer.begin();
        drop(first);
        timer.expire_if_idle(Duration::ZERO, || panic!("second request remains active"));
        drop(second);
        let mut expired = false;
        timer.expire_if_idle(Duration::ZERO, || expired = true);
        assert!(expired);
    }
}
