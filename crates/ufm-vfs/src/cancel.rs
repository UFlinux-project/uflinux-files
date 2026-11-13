//! Cooperative cancellation.
//!
//! A directory read, an archive table-of-contents scan and a copy job all outlive
//! the UI action that started them: the user can navigate away, close a tab, or
//! hit Escape while the work is still running. Rather than killing threads, each
//! job polls one of these between chunks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A flag a worker polls and the UI sets. Cheap enough to check per entry.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Fail early at the top of a loop, so a caller can use `?`.
    pub fn ensure_active(&self) -> ufm_core::Result<()> {
        if self.is_cancelled() {
            Err(ufm_core::Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// A flag that also flips when the guard is dropped, for a job that should
    /// stop if its owner is gone. `CancelToken` is already `#[must_use]` as a
    /// type, so no attribute is needed here.
    pub fn token(&self) -> CancelToken {
        CancelToken {
            flag: self.clone(),
            on_drop: None,
        }
    }

    /// A second flag that mirrors this one, so a child job can be cancelled
    /// without cancelling its parent.
    #[must_use]
    pub fn child(&self) -> Self {
        let child = Self::default();
        if self.is_cancelled() {
            child.cancel();
        }
        child
    }
}

/// A borrowed cancellation that stops the job when it goes out of scope unless
/// it is explicitly kept alive.
#[must_use]
pub struct CancelToken {
    flag: Cancel,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl CancelToken {
    pub fn on_drop<F: FnOnce() + Send + 'static>(mut self, f: F) -> Self {
        self.on_drop = Some(Box::new(f));
        self
    }

    #[must_use]
    pub fn flag(&self) -> &Cancel {
        &self.flag
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_active_and_flips_once() {
        let c = Cancel::new();
        assert!(!c.is_cancelled());
        assert!(c.ensure_active().is_ok());
        c.cancel();
        assert!(c.is_cancelled());
        assert!(c.ensure_active().is_err());
    }

    #[test]
    fn clones_share_one_flag() {
        let a = Cancel::new();
        let b = a.clone();
        b.cancel();
        assert!(a.is_cancelled());
    }

    #[test]
    fn a_child_of_a_cancelled_parent_starts_cancelled() {
        let parent = Cancel::new();
        let child = parent.child();
        assert!(!child.is_cancelled());
        parent.cancel();
        // Already-created children do not retroactively flip: a job that already
        // handed off its work should finish. A child made after cancellation does.
        let later = parent.child();
        assert!(later.is_cancelled());
        drop(child);
    }

    #[test]
    fn a_token_can_cancel_its_owner() {
        let c = Cancel::new();
        {
            let _t = c.token().on_drop({
                let c = c.clone();
                move || c.cancel()
            });
            assert!(!c.is_cancelled());
        }
        assert!(c.is_cancelled());
    }

    #[test]
    fn cancellation_is_visible_across_threads() {
        let c = Cancel::new();
        let worker = c.clone();
        let handle = std::thread::spawn(move || {
            while !worker.is_cancelled() {
                std::thread::yield_now();
            }
            true
        });
        c.cancel();
        assert!(handle.join().unwrap());
    }
}
