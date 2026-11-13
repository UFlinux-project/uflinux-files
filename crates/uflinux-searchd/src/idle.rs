//! When the daemon is allowed to go away.
//!
//! The bus starts this process on the first query, so the cost of an idle desktop is
//! whatever the daemon decides to keep resident. The decision is a single one: no
//! client has talked to us for `idle_exit_secs` and nothing is being indexed, so
//! nothing is lost by exiting. The index on disk survives, and the next daemon
//! reopens it with one `mmap` rather than rebuilding it.
//!
//! Two things reset the clock: a request arriving, and a crawl finishing. The second
//! matters because a crawl that takes longer than the timeout must not be followed by
//! an exit that throws away the warm searcher — the work just done was work somebody
//! asked for.
//!
//! The arithmetic lives here, apart from the GLib timer that calls it, so that the
//! rule can be tested without a main context.

use std::time::{Duration, Instant};

/// How the daemon decides it may exit.
#[derive(Debug, Clone, Copy)]
pub struct Idle {
    timeout: Duration,
    last: Instant,
    /// Whether a crawl is running right now. A busy daemon is never idle, however
    /// long it has been since a client asked for it: the crawl is the reason it
    /// exists.
    busy: bool,
    /// Set once [`expired`](Self::expired) has said yes, so a source that keeps
    /// ticking does not decide twice. Not strictly needed — the daemon exits — but
    /// it makes the state machine total rather than relying on process death.
    fired: bool,
}

impl Idle {
    /// Start counting from now. A timeout of zero means "exit as soon as nothing is
    /// happening", which is what a test uses and what `--once` would want.
    #[must_use]
    pub fn new(secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(secs),
            last: Instant::now(),
            busy: false,
            fired: false,
        }
    }

    /// A request arrived, or a crawl just finished.
    pub fn touch(&mut self) {
        self.last = Instant::now();
        self.fired = false;
    }

    /// Record whether a crawl holds the writer. Called before every check so the
    /// answer comes from the engine rather than from a flag that could have been
    /// forgotten on an error path.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// The configured timeout, for a status line.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// How long this daemon has been doing nothing, for `Status`.
    #[must_use]
    pub fn quiet_for(&self) -> Duration {
        self.last.elapsed()
    }

    /// Whether the daemon should exit now.
    pub fn expired(&mut self, now: Instant) -> bool {
        if self.busy || self.fired {
            return false;
        }
        // `checked_sub` rather than a saturating one: a clock that went backwards
        // means `now` is before the last activity, and exiting on that would be
        // exiting on a glitch in the monotonic clock rather than on real idleness.
        let Some(quiet) = now.checked_duration_since(self.last) else {
            return false;
        };
        if quiet >= self.timeout {
            self.fired = true;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_timer_is_not_expired() {
        let mut idle = Idle::new(120);
        assert!(!idle.expired(Instant::now()));
    }

    #[test]
    fn it_expires_once_the_timeout_passes_and_not_before() {
        let mut idle = Idle::new(120);
        let later = Instant::now() + Duration::from_secs(119);
        assert!(!idle.expired(later));
        let later = Instant::now() + Duration::from_secs(121);
        assert!(idle.expired(later));
    }

    #[test]
    fn a_crawl_holds_the_decision() {
        let mut idle = Idle::new(0);
        idle.set_busy(true);
        assert!(!idle.expired(Instant::now() + Duration::from_secs(3600)));
        idle.set_busy(false);
        assert!(idle.expired(Instant::now() + Duration::from_secs(3600)));
    }

    #[test]
    fn touching_restarts_the_count() {
        let mut idle = Idle::new(10);
        idle.touch();
        assert!(idle.quiet_for() < Duration::from_secs(1));
        assert!(!idle.expired(Instant::now() + Duration::from_secs(9)));
    }

    #[test]
    fn an_instant_from_before_the_last_activity_does_not_exit() {
        // An `Instant` read before the timer was built is the only way to name
        // "before the last activity": the monotonic clock itself never goes
        // backwards, but a caller may hand us a reading taken earlier.
        let before = Instant::now();
        let mut idle = Idle::new(0);
        assert!(!idle.expired(before));
        assert!(idle.expired(Instant::now()));
    }

    #[test]
    fn a_zero_timeout_exits_immediately() {
        let mut idle = Idle::new(0);
        assert!(idle.expired(Instant::now()));
    }
}
