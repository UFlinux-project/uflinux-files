//! The bus surface: one object, five methods, two signals.
//!
//! Everything here is translation. A D-Bus call arrives, [`ufm_search::Engine::dispatch`]
//! decides what it means, and the answer goes back. The engine is deliberately
//! unaware of [`gio`], which is what lets the same dispatcher be exercised by a test
//! with no bus, no session and no main context — and what keeps the daemon small
//! enough to be started on demand by the bus and to exit again two minutes later.
//!
//! One JSON string in, one JSON string out, the same shape `org.uflinux.Settings1`
//! uses. Adding a method to the protocol means adding a variant to
//! [`ufm_search::Request`] and a line to the introspection data below; it never means
//! inventing a signature, and validation happens in one place rather than in five
//! argument lists.
//!
//! Two things are worth knowing about the dispatch closure. It has to be `Send`,
//! because GIO stores it in a `Box`, so the shared state is `Arc`/`Mutex` rather than
//! the `Rc`/`RefCell` a single-threaded daemon would use. And it is a *local* closure,
//! so it runs on the thread that owns the connection — which is the thread that emits
//! signals, which is why a crawl's results have to be handed back through
//! [`glib::MainContext::invoke`] in [`sink_for`] instead of being emitted from the
//! worker.
//!
//! There is no per-client bookkeeping. A daemon started by the bus could in principle
//! count its clients and exit when the last one went away, which is what the
//! `NameOwnerChanged` subscription in `uflinux-settings` is for. Here it would need
//! every caller to announce itself first, and the protocol has no method for that; the
//! idle clock in [`crate::idle`] asks the question that actually matters instead — has
//! anything been asked of us recently — and a client that disappears mid-crawl leaves
//! behind an index that is more useful than the one it started from.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use glib::variant::ToVariant;
use serde::Serialize;

use ufm_search::{
    CHANGED, Dispatch, Engine, OBJECT_PATH, PROGRESS, Request, SESSION_NAME, WireError,
};

use crate::idle::Idle;
use crate::worker::{Event, Queue, Sink, Worker};

/// The interface, in the form the bus serves it and the form a client can ask for.
///
/// Every method takes one string and returns one string, so this never changes when
/// the protocol does — only the JSON inside those strings does.
pub const XML: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/Standards/dbus/introspect.dtd">
<node>
  <interface name="org.uflinux.Search1">
    <method name="SearchFiles">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="SearchApps">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="Status">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="Reindex">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="SetRoots">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <signal name="Changed">
      <arg name="body" type="s"/>
    </signal>
    <signal name="Progress">
      <arg name="body" type="s"/>
    </signal>
  </interface>
</node>
"#;

/// The answer to a request that changed something and has nothing to report.
///
/// Spelled out rather than serialised per call, so that the bytes on the bus are
/// pinned by a test instead of by a version of `serde_json`.
const ACK: &str = r#"{"reply":"ack"}"#;

/// What the bus handler and the worker share.
///
/// `Send + Sync` because the dispatch closure is boxed by GIO and the sink is called
/// from the indexing thread.
struct Core {
    /// Absent in a test core, which has no session bus to emit on. Everything else —
    /// the dispatch rules, the queue, the exit clock — behaves identically, which is
    /// what lets the daemon's own logic be tested without a bus, a session or a main
    /// context. A real daemon always has one; `Service::serve` cannot be called without
    /// a connection.
    connection: Option<gio::DBusConnection>,
    engine: Arc<Engine>,
    queue: Arc<Queue>,
    idle: Mutex<Idle>,
}

impl Core {
    /// A core with no bus, over an engine the caller built. Test-only.
    #[cfg(test)]
    fn alone(engine: Arc<Engine>, queue: Arc<Queue>, idle_exit_secs: u64) -> Self {
        Self {
            connection: None,
            engine,
            queue,
            idle: Mutex::new(Idle::new(idle_exit_secs)),
        }
    }

    fn idle(&self) -> MutexGuard<'_, Idle> {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Something happened, so the exit clock starts again.
    fn touch(&self) {
        self.idle().touch();
    }

    /// How long the daemon will wait for a request before leaving.
    fn timeout(&self) -> Duration {
        self.idle().timeout()
    }

    /// How long nothing has been asked of this daemon.
    fn quiet_for(&self) -> Duration {
        self.idle().quiet_for()
    }

    /// Whether the daemon has been asked for nothing for long enough to leave.
    ///
    /// The busy flag is read from the engine rather than remembered, so an error path
    /// that forgets to clear something cannot report an idle daemon while a crawl is
    /// still writing. Work that is queued but not started counts as busy too: exiting
    /// now would throw away a re-read somebody asked for, and nothing else would notice
    /// it was missed.
    ///
    /// `now` is passed in rather than read here because an [`Instant`] cannot be shifted
    /// backwards: without this, testing the timeout would mean sleeping for it.
    fn expired_at(&self, now: Instant) -> bool {
        let mut idle = self.idle();
        idle.set_busy(self.engine.is_busy() || !self.queue.is_empty());
        idle.expired(now)
    }

    fn should_exit(&self) -> bool {
        self.expired_at(Instant::now())
    }

    #[cfg(test)]
    fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    #[cfg(test)]
    fn queue(&self) -> &Arc<Queue> {
        &self.queue
    }

    /// Hand a worker's report to the bus. Runs on the main context, which is the only
    /// thread allowed to emit on this connection.
    fn deliver(&self, event: Event) {
        match event {
            Event::Progress(progress) => self.emit(PROGRESS, &progress),
            Event::Changed(changed) => {
                // A crawl that finished is the end of work somebody asked for, so the
                // timeout counts from here rather than from the request that started
                // it. Without this, a crawl longer than `idle_exit_secs` would be
                // followed by an exit that throws away the warm searcher it just built.
                self.touch();
                self.emit(CHANGED, &changed);
            }
            Event::Failed { kind, message } => {
                self.touch();
                // No signal: a client watching for `Changed` will simply see no new
                // generation, and `Status` now carries the reason.
                self.engine.report(Some(format!(
                    "the {kind} index could not be rebuilt: {message}"
                )));
            }
        }
    }

    /// Emit one signal. A failure here means no client is listening, which is the
    /// normal state of a desktop with the file manager closed.
    fn emit<T: Serialize>(&self, signal: &str, body: &T) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let Ok(text) = serde_json::to_string(body) else {
            return;
        };
        let _ = connection.emit_signal(
            None,
            OBJECT_PATH,
            SESSION_NAME,
            signal,
            Some(&(text,).to_variant()),
        );
    }
}

/// The served object, plus the worker it feeds and the name it holds.
///
/// Not `Clone`, and held behind an `Arc` by the daemon: each field below is a lease that
/// must exist exactly once, and the last handle is what gives the bus name back,
/// unregisters the object and joins the indexing thread.
pub struct Service {
    core: Arc<Core>,
    // Neither `gio::OwnerId` nor `gio::RegistrationId` has a `Drop` impl, so holding them
    // releases nothing; the real teardown is the process exiting, and the real cleanup is
    // `Worker`'s `Drop` joining the indexing thread.
    _worker: Worker,
    _owner: gio::OwnerId,
    _registration: gio::RegistrationId,
}

impl Service {
    /// Register the object, take the name, start the indexing thread.
    ///
    /// The name is taken with `ALLOW_REPLACEMENT` rather than `REPLACE`: during an
    /// upgrade the old daemon is usually still finishing a crawl, and a new one that
    /// forced it out would find tantivy's directory lock held and answer nothing until
    /// it was retried.
    pub fn serve(
        connection: gio::DBusConnection,
        engine: Arc<Engine>,
        idle_exit_secs: u64,
    ) -> Result<Self, String> {
        let info = gio::DBusNodeInfo::for_xml(XML)
            .map_err(|e| format!("the introspection data is malformed: {e}"))?;
        let interface = info
            .interfaces()
            .first()
            .ok_or("the introspection data declares no interface")?
            .clone();

        let queue = Arc::new(Queue::new());
        let core = Arc::new(Core {
            connection: Some(connection.clone()),
            engine: Arc::clone(&engine),
            queue: Arc::clone(&queue),
            idle: Mutex::new(Idle::new(idle_exit_secs)),
        });
        let worker = Worker::spawn(Arc::clone(&queue), Arc::clone(&engine), sink_for(&core));

        let registration = {
            let core = Arc::clone(&core);
            connection
                .register_object(OBJECT_PATH, &interface)
                .method_call(move |_, _, _, _, method, parameters, invocation| {
                    handle(&core, method, parameters).apply(invocation);
                })
                .build()
                .map_err(|e| format!("cannot serve {OBJECT_PATH}: {e}"))?
        };

        let owner = gio::bus_own_name_on_connection(
            &connection,
            SESSION_NAME,
            gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
            |_, _| {},
            |_, _| {},
        );

        Ok(Self {
            core,
            _worker: worker,
            _owner: owner,
            _registration: registration,
        })
    }

    /// The queue the file watcher pushes re-reads into.
    #[must_use]
    pub fn queue(&self) -> &Arc<Queue> {
        &self.core.queue
    }

    /// The engine, so the daemon can hand it to the watcher without cloning the whole
    /// service.
    #[must_use]
    pub fn engine(&self) -> &Arc<Engine> {
        &self.core.engine
    }

    /// Whether the daemon has been asked for nothing for long enough to leave.
    #[must_use]
    pub fn should_exit(&self) -> bool {
        self.core.should_exit()
    }

    /// How long the daemon is willing to sit with nothing asked of it.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.core.timeout()
    }

    /// How long nothing has been asked of us, for a log line and for a person reading
    /// the journal.
    #[must_use]
    pub fn quiet_for(&self) -> Duration {
        self.core.quiet_for()
    }
}

/// The callback the indexing thread reports into.
///
/// Each event is re-posted to the main context rather than emitted on the spot: the
/// dispatch closure is a local one, so emitting from the worker would be emitting from
/// the wrong thread. `invoke` only queues, so a burst of progress reports cannot block
/// the crawl that produced them, and a daemon on its way out simply drops the ones
/// still in the queue.
fn sink_for(core: &Arc<Core>) -> Sink {
    let context = glib::MainContext::default();
    let sink = {
        let core = Arc::clone(core);
        move |event| {
            let core = Arc::clone(&core);
            context.invoke(move || core.deliver(event));
        }
    };
    Arc::new(sink)
}

/// What answering a call amounts to.
///
/// The one thing that makes a D-Bus handler hard to test is that answering it needs a
/// connection. So the handler computes this and the caller applies it: in production the
/// caller is the registration closure, which knows about the invocation, and in a test it
/// is an assertion. The rules worth checking — which body is refused, what a task is
/// answered with, whether the exit decision is right — are all decided before anything
/// touches GLib.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Reply with one JSON string.
    Reply(String),
    /// Refuse with a named error. The message is separate because the queue-full case
    /// says how deep the queue is, which a fixed per-error string cannot.
    Refuse { error: WireError, message: String },
    /// The work is queued and there is nothing to report.
    Accepted,
}

impl Outcome {
    /// Hand the outcome to the caller who is waiting for a reply.
    fn apply(self, invocation: gio::DBusMethodInvocation) {
        match self {
            Self::Reply(text) => invocation.return_value(Some(&(text,).to_variant())),
            Self::Refuse { error, message } => {
                invocation.return_dbus_error(error.dbus_name(), &message);
            }
            Self::Accepted => invocation.return_value(Some(&(ACK.to_string(),).to_variant())),
        }
    }
}

/// Answer one call.
///
/// Every rejection is a named D-Bus error rather than an empty answer: a client that
/// sent the wrong shape needs to know that, because the alternative — an empty result
/// list — looks like a search that found nothing rather than as a call that was never
/// understood.
fn handle(core: &Core, method: &str, parameters: glib::Variant) -> Outcome {
    core.touch();

    let refuse = |error: WireError| Outcome::Refuse {
        error,
        message: error.message().to_string(),
    };

    let Some((body,)) = parameters.get::<(String,)>() else {
        return refuse(WireError::Invalid);
    };
    let request = match Request::decode(body.as_bytes()) {
        Ok(request) => request,
        Err(error) => return refuse(error),
    };
    // The body names its own method, and the message names the one it was sent to.
    // Both come from the same client and either can be wrong; answering a
    // `search-apps` body that arrived as `SearchFiles` would return application
    // results to a caller that asked for files.
    if request.bus_method() != method {
        return refuse(WireError::Invalid);
    }

    match core.engine.dispatch(&request) {
        Err(error) => refuse(error),
        Ok(Dispatch::Reply(reply)) => match reply.encode() {
            Ok(text) => Outcome::Reply(text),
            Err(error) => refuse(error),
        },
        Ok(Dispatch::Index { task }) => {
            // `false` means the queue was full of work nobody will evict. Saying so is
            // truer than accepting the task and never running it, and the depth is in
            // the message because "try later" is only useful if the caller can tell a
            // queue of one from a queue that is stuck.
            if core.queue.push(task) {
                Outcome::Accepted
            } else {
                Outcome::Refuse {
                    error: WireError::Unavailable,
                    message: format!(
                        "too much indexing work is already queued ({} tasks)",
                        core.queue.len()
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use ufm_search::{Changed, IndexConfig, Reply, Root, Task};

    /// A daemon with no bus: a tree to index, an engine over it, the queue, the exit
    /// clock, and optionally the indexing thread.
    ///
    /// `serve` cannot be tested here — it needs a session — but everything it wires up
    /// is `Core` plus a closure that calls [`handle`], and that is what this exercises.
    struct Fixture {
        /// Declared first so it is dropped first: joining the indexing thread has to
        /// happen before the temporary tree it is reading disappears.
        worker: Option<Worker>,
        /// The tree the index was built over. Held for cleanup, and checked by the one
        /// test that cares where the index actually went.
        dir: tempfile::TempDir,
        root: PathBuf,
        core: Arc<Core>,
        events: Arc<Mutex<Vec<Event>>>,
    }

    /// Build the tree, and a core over it. Without a worker, so the queue can be read
    /// by hand.
    fn core_over(idle_exit_secs: u64) -> Fixture {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let root = dir.path().join("home");
        fs::create_dir_all(root.join("Documents")).expect("mkdir");
        fs::write(root.join("Documents/notes.txt"), "the annual report\n").expect("write");
        let config = IndexConfig {
            roots: vec![Root::names(root.clone())],
            ..Default::default()
        };
        let engine = Arc::new(
            Engine::at(
                config,
                &dir.path().join("index"),
                vec![dir.path().join("apps")],
                None,
            )
            .expect("an engine over a fixture tree"),
        );
        let core = Arc::new(Core::alone(
            Arc::clone(&engine),
            Arc::new(Queue::new()),
            idle_exit_secs,
        ));
        Fixture {
            worker: None,
            dir,
            root,
            core,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build the same thing with the indexing thread running, delivering worker reports
    /// through [`Core::deliver`] as the real daemon does.
    fn with_worker(mut fixture: Fixture) -> Fixture {
        assert!(fixture.worker.is_none(), "the worker is added once");
        let engine = Arc::clone(fixture.core.engine());
        let queue = Arc::clone(fixture.core.queue());
        let core = Arc::clone(&fixture.core);
        let events = Arc::clone(&fixture.events);
        let sink = Arc::new(move |event: Event| {
            events.lock().expect("the event log").push(event.clone());
            core.deliver(event);
        });
        fixture.worker = Some(Worker::spawn(queue, engine, sink));
        fixture
    }

    impl Fixture {
        fn ask(&self, method: &str, body: &str) -> Outcome {
            handle(&self.core, method, (body.to_string(),).to_variant())
        }

        fn events(&self) -> Vec<Event> {
            self.events.lock().expect("the event log").clone()
        }

        /// Wait for the crawl the worker was handed to commit. Long enough for a
        /// three-file tree on any machine, and a failure rather than a hang otherwise.
        fn wait_for_a_crawl(&self) -> bool {
            for _ in 0..1_000 {
                if self
                    .events()
                    .iter()
                    .any(|event| matches!(event, Event::Changed(_)))
                {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        }
    }

    #[test]
    fn a_reindex_is_answered_with_an_ack_and_then_makes_files_findable() {
        // The whole path in one test: a call arrives, the answer is an ack, the task
        // reaches the worker, the crawl commits, and `Status` can see the result. Each
        // half is checked on its own elsewhere; what is worth pinning here is that they
        // join up.
        let fixture = with_worker(core_over(3_600));
        assert_eq!(
            Outcome::Accepted,
            fixture.ask("Reindex", r#"{"method":"reindex"}"#)
        );
        assert!(fixture.wait_for_a_crawl(), "the crawl never reported");

        let Outcome::Reply(text) = fixture.ask("Status", r#"{"method":"status"}"#) else {
            panic!("a status request is answered, not refused");
        };
        let reply = serde_json::from_str::<Reply>(&text).expect("a status reply");
        let Reply::Status(status) = reply else {
            panic!("expected a status reply, got {reply:?}");
        };
        assert_eq!(
            1,
            status
                .indexes
                .iter()
                .filter(|index| index.kind == "files" && index.records > 0)
                .count(),
            "the crawl should have left records behind: {status:?}"
        );
        assert_eq!(vec![fixture.root.display().to_string()], status.roots);
        assert!(
            fixture.dir.path().join("index").is_dir(),
            "the crawl wrote somewhere other than the index root it was given"
        );
    }

    #[test]
    fn a_second_reindex_while_one_runs_is_refused_rather_than_queued() {
        // The engine holds the writer for the duration of a crawl, so a second `Reindex`
        // has to be refused where it arrives — queueing it would mean running it later
        // against an index the caller believes is idle.
        let fixture = core_over(3_600);
        let held = fixture.core.engine().acquire().expect("an idle engine");
        let outcome = fixture.ask("Reindex", r#"{"method":"reindex"}"#);
        assert!(
            matches!(
                outcome,
                Outcome::Refuse {
                    error: WireError::Unavailable,
                    ..
                }
            ),
            "a crawl in hand should make a second one an error, not a queue entry: {outcome:?}"
        );
        assert!(
            fixture.core.queue().is_empty(),
            "the refused task was queued"
        );
        drop(held);
    }

    #[test]
    fn a_body_sent_as_the_wrong_method_is_refused() {
        // Two names for the same thing in one message; if they disagree, neither can be
        // trusted.
        let fixture = core_over(3_600);
        let outcome = fixture.ask("SearchApps", r#"{"method":"search-files","text":"x"}"#);
        assert!(
            matches!(
                outcome,
                Outcome::Refuse {
                    error: WireError::Invalid,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_request_the_daemon_has_never_heard_of_is_refused() {
        let fixture = core_over(3_600);
        let outcome = fixture.ask("Status", r#"{"method":"search-for-pictures"}"#);
        assert!(
            matches!(
                outcome,
                Outcome::Refuse {
                    error: WireError::Invalid,
                    ..
                }
            ),
            "an unknown method has to look like an error, not like an empty answer: \
             {outcome:?}"
        );
    }

    #[test]
    fn a_status_reply_names_both_indexes_before_anything_is_indexed() {
        // The settings page and the memory benchmark both read this, and both assume two
        // entries. A fresh daemon that reported one would look like a broken index rather
        // than an empty one.
        let fixture = core_over(3_600);
        let Outcome::Reply(text) = fixture.ask("Status", r#"{"method":"status"}"#) else {
            panic!("a status request is answered, not refused");
        };
        let reply = serde_json::from_str::<Reply>(&text).expect("a status reply");
        let Reply::Status(status) = reply else {
            panic!("expected a status reply, got {reply:?}");
        };
        assert_eq!(2, status.indexes.len());
        assert!(!status.busy);
        assert_eq!(None, status.note);
        assert!(
            status
                .indexes
                .iter()
                .all(|index| index.records == 0 && index.segments == 0)
        );
    }

    #[test]
    fn queued_work_holds_the_exit_clock() {
        // A zero timeout means "leave as soon as nothing is happening", so this is the
        // sharpest form of the rule: work nobody has started yet is still work.
        let fixture = core_over(0);
        assert!(fixture.core.queue().push(Task::Apps));
        assert!(
            !fixture.core.should_exit(),
            "a queued task must not be thrown away by an exit"
        );
        assert!(fixture.core.queue().take().is_some(), "the task vanished");
        assert!(
            fixture.core.should_exit(),
            "with nothing queued and nothing asked, a zero-timeout daemon may leave"
        );
    }

    #[test]
    fn a_crawl_that_just_committed_buys_the_daemon_another_timeout() {
        // The rule the idle clock exists to implement: a long crawl must not be followed
        // by an exit that discards the searcher it just warmed, because the next query
        // would pay for a reopen that nobody asked for.
        let fixture = core_over(60);
        assert!(
            fixture
                .core
                .expired_at(Instant::now() + Duration::from_secs(61)),
            "a minute of quiet is past a minute-long timeout"
        );
        // `deliver` touching the clock is the whole claim being tested here. The reading
        // is taken again rather than reused: `expired` latches, so the same instant that
        // fired a moment ago would fire again.
        fixture.core.deliver(Event::Changed(Changed {
            kind: "files".to_string(),
            generation: 1,
            records: 12,
        }));
        assert!(
            !fixture.core.expired_at(Instant::now()),
            "a commit is the end of work somebody asked for, so the count starts again"
        );
    }

    #[test]
    fn a_failed_crawl_lands_in_the_status_note_and_not_in_a_signal() {
        // A client watching `Changed` sees no new generation and keeps the index it has;
        // the reason belongs in `Status`, which is the only place a client looks when
        // something is wrong.
        let fixture = core_over(3_600);
        fixture.core.deliver(Event::Failed {
            kind: "files",
            message: "no space left on device".to_string(),
        });
        assert!(
            !fixture
                .events()
                .iter()
                .any(|event| matches!(event, Event::Changed(_))),
            "a failure is not a change"
        );
        let Outcome::Reply(text) = fixture.ask("Status", r#"{"method":"status"}"#) else {
            panic!("a status request is answered, not refused");
        };
        let Reply::Status(status) = serde_json::from_str::<Reply>(&text).expect("a reply") else {
            panic!("expected a status reply");
        };
        let note = status.note.expect("the failure should be reported");
        assert!(note.contains("files"), "{note}");
        assert!(note.contains("no space left"), "{note}");
    }

    #[test]
    fn the_introspection_data_declares_the_interface_the_wire_uses() {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("parsable introspection data");
        let interfaces = info.interfaces();
        assert_eq!(interfaces.len(), 1, "one object, one interface");
        // `DBusInterfaceInfo` has no name accessor in gio 0.21, so the declaration is
        // checked where it is written.
        assert!(
            XML.contains(&format!("<interface name=\"{SESSION_NAME}\">")),
            "the served interface has to be the one the client's proxy targets"
        );
    }

    #[test]
    fn every_request_variant_has_a_method_and_every_method_a_variant() {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("parsable introspection data");
        let interface = &info.interfaces()[0];
        for (body, method) in [
            (r#"{"method":"status"}"#, "Status"),
            (r#"{"method":"search-files","text":"x"}"#, "SearchFiles"),
            (r#"{"method":"search-apps","text":"x"}"#, "SearchApps"),
            (r#"{"method":"reindex"}"#, "Reindex"),
            (r#"{"method":"set-roots","roots":["/home/u"]}"#, "SetRoots"),
        ] {
            let request = Request::decode(body.as_bytes()).expect("a known request");
            assert_eq!(request.bus_method(), method, "{body}");
            assert!(
                interface.lookup_method(method).is_some(),
                "{method} is not in the introspection data, so the bus would refuse \
                 the call before this daemon ever saw it"
            );
        }
    }

    #[test]
    fn every_method_takes_one_string_and_returns_one_string() {
        // The whole protocol rides on this shape, so it is worth pinning: a method
        // declared with a second argument would be a signature change disguised as an
        // addition.
        for method in ["SearchFiles", "SearchApps", "Status", "Reindex", "SetRoots"] {
            let declared = format!("<method name=\"{method}\">");
            let body = XML
                .split(&declared)
                .nth(1)
                .unwrap_or_else(|| panic!("{method} is not declared"))
                .split("</method>")
                .next()
                .expect("a closing tag");
            assert_eq!(
                2,
                body.matches("type=\"s\"").count(),
                "{method} should take one string and return one string"
            );
            assert_eq!(1, body.matches("direction=\"in\"").count(), "{method}");
            assert_eq!(1, body.matches("direction=\"out\"").count(), "{method}");
        }
    }

    #[test]
    fn both_signals_are_declared() {
        let info = gio::DBusNodeInfo::for_xml(XML).expect("parsable introspection data");
        let interface = &info.interfaces()[0];
        for signal in [CHANGED, PROGRESS] {
            assert!(
                interface.lookup_signal(signal).is_some(),
                "{signal} is not in the introspection data"
            );
        }
    }

    #[test]
    fn the_ack_on_the_bus_is_the_encoded_ack_reply() {
        // The constant is written by hand so that the bytes are pinned; this is what
        // stops it from drifting away from the type.
        assert_eq!(ACK, Reply::Ack.encode().expect("an ack always fits"));
    }
}
