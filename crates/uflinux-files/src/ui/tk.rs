//! Test harness for the widget tests.
//!
//! GTK hands the default main context to the thread that initialises it and
//! refuses a second `gtk_init` from any other, so widget tests cannot run on
//! whatever worker the test harness picked: the second one to arrive would
//! panic while the first was still holding the context. Every widget test
//! therefore hands its body to one exclusive thread, which initialises GTK
//! once and then keeps it.
//!
//! The same thread also decides whether the test runs at all. `gtk_init_check`
//! succeeds even with nowhere to draw, because GTK4 opens a display lazily; a
//! widget test only means something once `gdk` has picked one, so the body is
//! skipped while there is no display, which is the normal state of a build
//! container. Run the suite under a virtual display to get the real thing.

#![cfg(test)]

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::OnceLock;
use std::sync::mpsc;

/// The one thread GTK was initialised on, made on first use.
static UI: OnceLock<glib::ThreadPool> = OnceLock::new();

/// Whether widgets can be built here: GTK up, and a display to build them on.
fn drawable() -> bool {
    gtk::init().is_ok() && gtk::gdk::Display::default().is_some()
}

/// Run a test body with GTK initialised, or skip it when there is no display.
///
/// A panic inside the body fails the test, and so does losing the thread.
pub fn ui(body: impl FnOnce() + Send + 'static) {
    let pool =
        UI.get_or_init(|| glib::ThreadPool::exclusive(1).expect("a thread to run widget tests on"));
    let (tx, rx) = mpsc::sync_channel::<Result<(), Box<dyn Any + Send>>>(1);
    let queued = pool.push(move || {
        if !drawable() {
            // No display: nothing to build widgets on, so nothing to assert.
            let _ = tx.send(Ok(()));
            return;
        }
        let _ = tx.send(panic::catch_unwind(AssertUnwindSafe(body)));
    });
    if let Err(error) = queued {
        panic!("the widget test thread is gone: {error}");
    }
    match rx.recv() {
        Err(dropped) => panic!("the widget test thread died without answering: {dropped}"),
        Ok(Err(panic)) => panic::resume_unwind(panic),
        Ok(Ok(())) => {}
    }
}
