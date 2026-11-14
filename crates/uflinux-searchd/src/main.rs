//! `uflinux-searchd` — the file and application index, served on the session bus.
//!
//! The daemon exists so that pressing `Ctrl+F` costs a round trip rather than a cold
//! start. It owns the two Tantivy indexes, answers `org.uflinux.Search1`, and is started
//! by the bus the first time anything asks it a question. That last fact decides its
//! shape: a process that appears on demand has to be small, has to come up fast, and has
//! to be willing to disappear again. Hence the idle exit in [`idle`], the single indexing
//! worker in [`worker`], and the fact that nothing here connects to a display server —
//! the daemon is on the memory budget of a machine with little RAM, not on the budget of
//! a window.
//!
//! The layout follows who owns what:
//!
//! - [`service`] is the bus surface: translation only, no indexing decisions.
//! - [`engine`](ufm_search::Engine) is the whole query and indexing core, and knows
//!   nothing about `gio`. It is what a test drives directly, with no bus and no main
//!   context.
//! - [`worker`] is the one thread that runs crawls, so a merge and a crawl cannot both be
//!   asking for memory at the same moment.
//! - [`watch`] turns inotify into re-reads, and is what keeps the index from going stale
//!   under a running desktop.
//!
//! Startup is deliberately short: read the configuration, open the two indexes, take the
//! bus name, queue the first crawl, enter the loop. Nothing walks a directory tree on the
//! way to taking the name, so a client that asks a question immediately gets an answer —
//! possibly an empty one, with `busy` set and a generation to watch — instead of waiting
//! on a crawl it did not ask for.

// The prose below names the things this daemon is made of — GLib, D-Bus, inotify,
// Tantivy, pkexec-free — which `doc_markdown` reads as unquoted code. Backticking them
// would make the comments harder to read, not easier.
#![allow(clippy::doc_markdown)]

mod idle;
mod service;
mod watch;
mod worker;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use glib::variant::ToVariant as _;

use ufm_search::{Engine, IndexConfig, Request, Task};

use service::Service;
use watch::Live;
use worker::Queue;

/// How often the exit decision and the rescan pass are considered.
///
/// The idle timeout itself is two minutes, so checking ten times more often than that is
/// accurate enough to leave on time and rare enough that the check is not what wakes the
/// machine from a suspend. It is also what runs [`Live::tick`], which is how a share
/// mounted while the daemon sat idle becomes searchable without a restart.
const TICK: Duration = Duration::from_secs(12);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("uflinux-searchd: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Serve until the loop is quit, then come down cleanly.
///
/// The whole daemon is this one function plus four modules, because there are only two
/// interesting things to do here: answer a call, and decide when to stop. Everything else
/// is `ufm_search`'s job.
fn run() -> Result<(), String> {
    // The session bus is the daemon's entire reason for existing, and a session bus
    // without a runtime directory is not a login session. Refusing here, rather than
    // failing later inside GIO with a message about a socket, is the same guard
    // `uflinux-settings` uses — and a daemon that exits immediately is what bus
    // activation wants, since the alternative is a process that starts, learns it has
    // nothing to do, and lingers.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|dir| dir.is_empty()) {
        return Err("a user login session is required".to_string());
    }

    let path = ufm_search::config::path();
    // A file that is not there yet is the normal state of a first login, not a problem to
    // report. A file that is there and cannot be parsed is a different matter: the
    // defaults are still the right place to land, but the user should find out that their
    // chosen roots were ignored, and `Status` is the only place a client will look.
    let (config, note) = if path.is_file() {
        IndexConfig::load_or_default(&path)
    } else {
        (IndexConfig::default(), None)
    };
    let idle_exit_secs = config.idle_exit_secs;

    let engine = Engine::open(config.clone()).map_err(|e| format!("cannot open the index: {e}"))?;
    engine.report(note);
    let engine = Arc::new(engine);

    // The main loop is built inside `block_on`, which requires that nothing is running on
    // the context yet, and run outside it. `held` is what keeps the served object, the
    // watcher and the indexing thread alive across the two: they have to outlive the
    // future, and dropping them at the end of it would give the bus name back before the
    // first client got a reply.
    let mut held: Option<Held> = None;
    let mainloop = glib::MainContext::default().block_on(async {
        let connection = gio::bus_get_future(gio::BusType::Session)
            .await
            .map_err(|e| format!("cannot reach the session bus: {e}"))?;
        let service = Arc::new(Service::serve(
            connection,
            Arc::clone(&engine),
            idle_exit_secs,
        )?);
        let live = Live::start(Arc::clone(service.engine()), Arc::clone(service.queue()));
        if live.is_none() {
            // No inotify, or no descriptor for the wake pipe. The daemon still answers
            // queries and still re-reads on a timer; only the immediate updates are lost.
            eprintln!("uflinux-searchd: live updates unavailable, falling back to polling");
        }
        bootstrap(&config, &engine, service.queue());

        let loop_ = glib::MainLoop::new(None, false);
        attach(&loop_, &service, live.as_ref());
        watch_bus(&loop_, &service);
        held = Some(Held { service, live });
        Ok::<_, String>(loop_)
    })?;

    // The sources hold weak references, so this is the last strong one: dropping it gives
    // the bus name back and joins the indexing thread, which lets tantivy finish its commit
    // while the process still exists to do it.
    mainloop.run();
    drop(held.take());
    Ok(())
}

/// What the daemon keeps alive for as long as it runs.
///
/// The tick source holds a weak pointer to each half rather than a strong one, so that
/// this struct — and not a callback that outlived the loop — decides when the service is
/// gone. A source that captured the service directly would keep the bus name claimed after
/// `run` returned, and the indexing thread would be killed mid-crawl at exit instead of
/// being joined.
#[allow(dead_code)]
struct Held {
    service: Arc<Service>,
    live: Option<Arc<Live>>,
}

/// Queue the work that should exist before the first question is asked.
///
/// A daemon started by bus activation is, almost by definition, about to be asked
/// something. If the index is empty — first login, or a cache directory that was cleaned
/// — answering that question with nothing while a crawl happens in the background is worse
/// than a short wait: the user sees an empty search field and types again. So the initial
/// crawl is queued here, before the loop is entered, and the first `SearchFiles` is
/// answered from whatever exists at that moment.
fn bootstrap(config: &IndexConfig, engine: &Engine, queue: &Queue) {
    if engine.is_empty() {
        let _ = queue.push(Task::all(config));
        let _ = queue.push(Task::Apps);
    }
    // Otherwise the index on disk is the answer and nothing is queued. A daemon that
    // re-crawled the world every time a window opened would spend its life re-reading
    // files that had not changed, which is what the watcher and the rescan timer are for.
}

/// Attach the tick and the two signals that mean "stop".
///
/// The returned [`glib::SourceId`]s are deliberately dropped: dropping a `SourceId` does
/// not remove a source, so they stay attached to the loop for as long as the loop exists,
/// which is the intended lifetime. Holding them would only be necessary if the daemon ever
/// learned to restart its loop in place.
fn attach(loop_: &glib::MainLoop, service: &Arc<Service>, live: Option<&Arc<Live>>) {
    let _ = {
        let service = Arc::downgrade(service);
        let live = live.map(Arc::downgrade);
        let loop_ = loop_.clone();
        glib::timeout_add_local(TICK, move || {
            // Both weak pointers are checked before anything else. If the service is gone
            // the daemon is on its way out and there is nothing left to decide, so the
            // source removes itself rather than spinning.
            let Some(service) = service.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if service.should_exit() {
                // Say why in the journal: a daemon that disappears on its own looks like
                // a crash to anyone reading `systemctl --user status`, and the two numbers
                // tell them whether the timeout is too aggressive or nothing was asked.
                eprintln!(
                    "uflinux-searchd: nothing asked for {} s of the {} s allowed, exiting",
                    service.quiet_for().as_secs(),
                    service.timeout().as_secs()
                );
                loop_.quit();
                return glib::ControlFlow::Break;
            }
            if let Some(live) = live.as_ref().and_then(Weak::upgrade) {
                live.tick();
            }
            glib::ControlFlow::Continue
        })
    };

    // Both signals get the same treatment: quit the loop, which lets `main` drop the
    // service and join the indexing thread, rather than exiting with a merge half-written.
    // Tantivy's commit is atomic, so leaving between commits is safe; being killed with
    // `SIGKILL` is what makes a recovery necessary.
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
/// A daemon outliving its session bus is not hypothetical: a session that ends without
/// stopping its services leaves this process holding a name nobody can call. Quitting here
/// is also what makes the shutdown orderly rather than a race — the loop stops while the
/// connection is still open, so `Service`'s `Drop` can withdraw the object and release the
/// name properly instead of being overtaken by GLib's own teardown of a closed bus, and
/// tantivy's commit is allowed to finish in a process that still has a bus to complain to.
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

/// Ask a running daemon what it thinks, from the command line.
///
/// Not part of the protocol's contract — the intended client is the file manager — but it
/// is what makes an installed daemon debuggable without a bus browser, and it doubles as
/// the smoke test in `tests/`.
#[allow(dead_code)]
fn status_probe() -> Result<String, String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, Option::<&gio::Cancellable>::None)
        .map_err(|e| format!("cannot reach the session bus: {e}"))?;
    let body = serde_json::to_string(&Request::Status).map_err(|e| e.to_string())?;
    let reply = connection
        .call_sync(
            Some(ufm_search::SESSION_NAME),
            ufm_search::OBJECT_PATH,
            ufm_search::SESSION_NAME,
            "Status",
            Some(&(body,).to_variant()),
            Some(glib::VariantTy::STRING),
            gio::DBusCallFlags::NONE,
            2_000,
            Option::<&gio::Cancellable>::None,
        )
        .map_err(|e| format!("Status failed: {e}"))?;
    let (text,) = reply
        .get::<(String,)>()
        .ok_or("the reply was not a string")?;
    Ok(text)
}
