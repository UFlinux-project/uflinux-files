//! The desktop icons service: no window, no display, and no GTK.
//!
//! The renderer draws the icons; this decides what they are. That split is the reason
//! the process exists separately rather than as another widget in a file manager
//! window: a desktop background has to be up before the first window opens and has to
//! stay up after the last one closes, and it has to cost a fraction of what a window
//! costs. [`service`] is the bus surface, [`scan`] is the directory read, and
//! [`thumb`] is the only place image bytes are touched.
//!
//! Being a separate binary rather than a mode of `uflinux-files` is what makes the
//! memory budget reachable. As a mode it shared an executable with the UI, and the
//! linker resolves the whole import table at exec, so the 30 MiB of `LLVM` that
//! GTK's GL renderer drags in was resident before this process ran a single
//! instruction — measured at 28.6 MiB of proportional set against a 10 MiB budget,
//! of which all but 7 MiB was shared library text. As its own binary it maps
//! twenty-five shared libraries instead of a hundred and eleven, and the budget is
//! met. The check in `the_provider_does_not_depend_on_gtk_at_all` is what keeps it
//! that way: a `use gtk` here would quietly undo the split, and only a memory report
//! would notice.
//!
//! Startup mirrors `uflinux-searchd`: refuse without a login session, reach the bus,
//! register, take the name, enter the loop. Two things differ, and both are about the
//! fact that this service is not started on demand.
//!
//! There is no idle exit. A daemon that answers searches can go away and come back when
//! the next `Ctrl+F` arrives, because bus activation restarts it and the index on disk
//! is still there. The desktop provider has no such second life from a client's point of
//! view: the icons are the desktop, and a provider that exited after two quiet minutes
//! would leave a background full of icons that stop responding to a double-click until
//! something thought to ask it a question. The memory it holds is one directory listing
//! and one layout, which is the cheapest residency on the desktop.
//!
//! The first read happens before the name is taken, so the first renderer to arrive
//! gets a desktop rather than an empty model followed by a signal. A desktop directory
//! is tens of entries and one `stat` each, which is why that is affordable here and why
//! the search daemon, which walks a whole home directory, does not do the same.

mod scan;
mod service;
mod thumb;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use ufm_vfs::watch::Watcher;

use service::Service;

/// How often the rescan deadline is checked.
///
/// Shorter than [`RESCAN`] so that the deadline is honoured to within a fifth of
/// itself, and long enough that an idle provider is woken twice a minute rather than
/// continuously. The same timer is what notices a desktop directory that appeared after
/// startup, since a directory that did not exist could not be watched.
const TICK: Duration = Duration::from_secs(20);

/// How often the whole directory is re-read regardless of what the watcher reported.
///
/// The watcher covers the common case — anything written under the desktop directory —
/// so this is only here for what inotify cannot see: a directory that was not there at
/// startup, a watch that failed on a share that had just been unmounted, an event queue
/// that overflowed while nobody was looking. Five minutes is how long an icon placed by
/// another machine can take to appear, which is a delay nobody notices, and a rate at
/// which re-reading a directory costs nothing.
const RESCAN: Duration = Duration::from_secs(300);

/// Serve desktop icons until asked to stop.
fn main() -> ExitCode {
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("uflinux-files-provider: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Take the bus name, then run the loop.
///
/// The whole of provider mode is this function plus the three modules above it. The
/// shape — a future that sets everything up, a loop that runs afterwards, and a struct
/// that keeps the setup alive across the two — is the search daemon's, and for the same
/// reason: the registration and the name owner have to outlive the `block_on` that
/// created them, and the only place to put them is a value that `run` drops on the way
/// out.
fn serve() -> Result<(), String> {
    // Without a runtime directory there is no session bus, and GIO's own failure is a
    // message about a socket that tells a person nothing. The same guard the search
    // daemon uses, for the same reason: `--provider` is started by a session that is
    // already there, and if it is not, saying so and exiting is the whole job.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|dir| dir.is_empty()) {
        return Err("a user login session is required".to_string());
    }

    let desktop = ufm_core::paths::desktop_dir();
    let file = ufm_desktop_ipc::path();
    // A missing desktop directory is not a reason to refuse: the icons are the desktop
    // from a person's point of view, and an empty one is still worth serving. The note
    // is what distinguishes that from a desktop whose contents were lost.
    let note = (!desktop.is_dir()).then(|| format!("{} is not a folder", desktop.display()));

    let mut held: Option<Held> = None;
    let mainloop = glib::MainContext::default().block_on(async {
        let connection = gio::bus_get_future(gio::BusType::Session)
            .await
            .map_err(|e| format!("cannot reach the session bus: {e}"))?;
        let service = Arc::new(Service::serve(connection, &desktop, file.clone(), note)?);
        eprintln!("uflinux-files-provider: serving {}", service.desktop());
        let live = Live::start(Arc::downgrade(&service), &desktop);
        if live.is_none() {
            // No inotify, or no descriptor for the wake pipe. The desktop still works and
            // still updates; only changes that happen while the provider is awake stop
            // being immediate, and the rescan timer catches those.
            eprintln!("uflinux-files-provider: live updates unavailable, falling back to polling");
        }

        let loop_ = glib::MainLoop::new(None, false);
        attach(&loop_, &service, live.as_ref());
        watch_bus(&loop_, &service);
        held = Some(Held { service, live });
        Ok::<_, String>(loop_)
    })?;

    // Dropping this gives the bus name back, so a renderer that arrives during shutdown
    // finds no provider and falls back rather than waiting on a process that has stopped
    // answering. The watcher is dropped with it, which removes the fd source before
    // closing the descriptors underneath it.
    mainloop.run();
    drop(held.take());
    Ok(())
}

/// What the provider keeps alive for as long as it runs.
///
/// Both sources hold weak pointers, so this struct — and not a callback that outlived
/// the loop — decides when the service is gone. A source that captured the service
/// strongly would keep the bus name claimed after `serve` returned, and the name would
/// be released only when the process exited, which is too late for a session that is
/// tearing down and restarting the provider.
///
/// Neither field is read: the point of the struct is that dropping it destroys them.
#[allow(dead_code)]
struct Held {
    service: Arc<Service>,
    live: Option<Arc<Live>>,
}

/// The desktop directory, watched.
///
/// One directory, which is why this is a tenth the size of the search daemon's
/// equivalent: the provider's whole scope is a single folder, and everything below it is
/// a subfolder that a desktop icon points at rather than one whose contents are
/// enumerated. The watcher's cap therefore never comes into play here.
struct Live {
    watcher: Watcher,
    /// The directory being watched, kept so that the periodic pass can re-declare the
    /// watch without the caller threading the path through the timer.
    desktop: PathBuf,
    service: Weak<Service>,
    /// Held so it can be removed on drop. Dropping a [`glib::SourceId`] leaves the source
    /// armed, and this one returns `Continue` forever.
    source: Mutex<Option<glib::SourceId>>,
    /// When the next full re-read is due.
    due: Mutex<Instant>,
}

impl Live {
    /// Start watching. `None` when inotify or a wake descriptor cannot be created, which
    /// leaves the provider serving from a model that only the rescan timer updates.
    fn start(service: Weak<Service>, desktop: &Path) -> Option<Arc<Self>> {
        let watcher = Watcher::new()?;
        watcher.watch(desktop);
        let live = Arc::new(Self {
            watcher,
            desktop: desktop.to_path_buf(),
            service,
            source: Mutex::new(None),
            // Due immediately: the watch may have failed, and the first timer pass is
            // what notices a directory that was created a moment after startup.
            due: Mutex::new(Instant::now()),
        });
        // Weak, so the source cannot keep the watcher alive: the source is owned by
        // `live`, and a strong reference in the callback would be a cycle.
        let weak = Arc::downgrade(&live);
        let source = glib::source::unix_fd_add_local(
            live.watcher.notify_fd(),
            glib::IOCondition::IN,
            move |_, _| match weak.upgrade() {
                Some(live) => {
                    live.on_ready();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            },
        );
        *live.lock() = Some(source);
        Some(live)
    }

    /// The periodic pass: re-declare the watch, then re-read the directory.
    ///
    /// Re-declaring is what makes a desktop directory that appeared after startup become
    /// watched rather than merely polled, and is free when it is already watched.
    fn tick(&self) {
        {
            let now = Instant::now();
            let mut due = self.deadline();
            if now < *due {
                return;
            }
            *due = now + RESCAN;
        }
        self.watcher.watch(&self.desktop);
        self.read();
    }

    /// Drain the watcher and re-read whatever settled.
    fn on_ready(&self) {
        // The set is drained even when nothing can be done with it, because the pending
        // changes are what the notify descriptor was written for and leaving them would
        // leave the descriptor readable forever.
        let ready = self.watcher.ready();
        if self.watcher.take_overflow() {
            // Events were dropped, so the set of what changed is unknown rather than
            // empty, and re-reading the one directory we care about is cheap enough to be
            // the standing answer.
            self.read();
            return;
        }
        if !ready.is_empty() {
            self.read();
        }
    }

    /// Read the directory again, if there is still a service to tell.
    fn read(&self) {
        if let Some(service) = self.service.upgrade() {
            service.refresh();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<glib::SourceId>> {
        self.source.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn deadline(&self) -> MutexGuard<'_, Instant> {
        self.due.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Removing the source first means no callback can run against a watcher whose
        // descriptors have already been closed.
        if let Some(source) = self.lock().take() {
            source.remove();
        }
    }
}

/// Attach the tick and the two signals that mean "stop".
///
/// The returned [`glib::SourceId`]s are deliberately dropped: dropping a `SourceId` does
/// not remove a source, so they stay attached to the loop for as long as the loop exists,
/// which is the intended lifetime. Holding them would only be necessary if the provider
/// ever learned to restart its loop in place.
fn attach(loop_: &glib::MainLoop, service: &Arc<Service>, live: Option<&Arc<Live>>) {
    let _ = {
        let service = Arc::downgrade(service);
        let live = live.map(Arc::downgrade);
        glib::timeout_add_local(TICK, move || {
            // The service is checked before anything else. If it is gone the provider is
            // on its way out and there is nothing left to read.
            let Some(service) = service.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if let Some(live) = live.as_ref().and_then(Weak::upgrade) {
                live.tick();
            } else {
                // No watcher: the timer is the only thing that keeps the model fresh.
                service.refresh();
            }
            glib::ControlFlow::Continue
        })
    };

    // Both signals get the same treatment: quit the loop, which lets `serve` drop the
    // service and release the name, rather than exiting with a placement unsaved. The
    // layout is written whole and atomically, so leaving at any point between two writes
    // leaves a file that describes some earlier state rather than a half-written one.
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let loop_ = loop_.clone();
        let _ = glib::source::unix_signal_add_local(signal, move || {
            loop_.quit();
            glib::ControlFlow::Break
        });
    }
}

/// Quit when the bus goes away, because at that point there is nobody left to serve.
///
/// A provider outliving its session bus is not hypothetical: a session that ends without
/// stopping its services leaves this process holding a name nobody can call, reading a
/// directory nobody looks at, until something kills it. Quitting here is also what makes
/// the shutdown orderly rather than a race — the loop stops while the connection is still
/// open, so [`Service`]'s `Drop` can withdraw the object and release the name properly
/// instead of being overtaken by `GLib`'s own teardown of a closed bus.
///
/// The handler id is dropped, which does not detach the handler: the connection owns it,
/// and the connection is owned by the service the loop is about to release.
fn watch_bus(loop_: &glib::MainLoop, service: &Arc<Service>) {
    let Some(connection) = service.connection() else {
        return;
    };
    let loop_ = loop_.clone();
    connection.connect_closed(move |_, _, _| {
        loop_.quit();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one of this module's own files, so a claim about the source can be checked
    /// rather than asserted in a comment.
    fn own(source: &str) -> String {
        let file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join(source);
        std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{} should be readable: {e}", file.display()))
    }

    #[test]
    fn the_provider_does_not_depend_on_gtk_at_all() {
        // The budget for this process is 10 MiB, and GTK is what would break it: the
        // linker resolves the whole import table at exec, so a binary that mentions
        // GTK pays for `LLVM` — 30 MiB on the machine this was measured on — before
        // it runs a single instruction, whether or not it ever opens a window. That
        // is why this is a separate crate rather than a mode of the file manager, and
        // why the check is against the manifest rather than the source: a `use gtk`
        // that compiles is a regression even if no code path reaches it.
        let manifest = own("../Cargo.toml");
        let depends = manifest
            .split("[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("[dev-dependencies]").next())
            .expect("the manifest has a dependency list");
        for (number, line) in depends.lines().enumerate() {
            let name = line.split('=').next().unwrap_or_default().trim();
            assert!(
                name != "gtk" && !name.starts_with("gtk4"),
                "Cargo.toml dependency {} pulls GTK into the provider",
                number + 1
            );
        }
        // The source check still earns its place: a dependency could arrive through a
        // feature, and a stray `gtk::` would then compile against a crate the manifest
        // does not list.
        for source in ["main.rs", "scan.rs", "service.rs", "thumb.rs"] {
            let text = own(source);
            // Built rather than written, or the assertion below would find GTK in its
            // own source and fail for the wrong reason.
            let path = ["gtk", "::"].concat();
            let layer = ["gtk4", "_layer_shell"].concat();
            for (number, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                assert!(
                    !code.contains(&path) && !code.contains(&layer),
                    "{}:{} mentions GTK: {line}",
                    source,
                    number + 1
                );
            }
        }
    }

    #[test]
    fn the_bus_name_is_taken_without_a_callback_that_can_abort() {
        // gio-rs's `bus_own_name_on_connection` wraps both handlers in a closure that
        // unwraps the connection it is handed, and GLib passes NULL once the bus is gone.
        // A provider using it survives normal operation and aborts at logout, which no
        // unit test on a live bus would ever catch — so the source itself is pinned.
        let text = own("service.rs");
        assert!(
            text.contains(&["g_bus_own_name_on", "_connection"].concat()),
            "the name has to be taken through the C entry point, whose handlers are nullable"
        );
        assert!(
            text.contains(&["g_bus_unown_", "name"].concat()),
            "the name has to be given back, since the wrapper type has no Drop"
        );
        // The aborting binding, spelled so this test cannot match its own comment.
        let binding = ["bus_own_name_", "on_connection"].concat();
        let called = text
            .lines()
            .map(str::trim_start)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| line.contains(&binding))
            // The `ffi::` form is the C call itself; only the bare gio-rs function is a
            // regression.
            .any(|line| !line.contains("ffi::"));
        assert!(!called, "the closure-based binding is back");
    }

    #[test]
    fn the_polling_deadline_is_longer_than_the_timer_that_checks_it() {
        // The timer only reads a deadline, so a deadline shorter than the interval
        // would be a deadline that is always overdue and a timer that does nothing.
        assert!(RESCAN > TICK, "{RESCAN:?} against {TICK:?}");
        assert!(
            RESCAN.as_secs() / TICK.as_secs() >= 4,
            "a deadline checked twice is a deadline that fires late by half itself"
        );
    }

    #[test]
    fn a_missing_session_is_refused_before_the_bus_is_asked_for() {
        // The guard is the whole of the error handling for a provider started outside a
        // login session, and the message has to name the missing thing rather than a
        // socket path.
        let message = "a user login session is required";
        assert!(
            message.contains("session"),
            "the refusal has to say what is missing"
        );
        // `serve` reads the environment before it reads the desktop or the bus, so the
        // two paths cannot disagree about which one failed.
        let text = own("main.rs");
        let guard = text
            .find("XDG_RUNTIME_DIR")
            .expect("the guard against a missing session is there");
        let bus = text
            .find("bus_get_future")
            .expect("the bus is reached for eventually");
        assert!(guard < bus, "the guard has to come before the bus");
    }
}
