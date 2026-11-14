//! The bus surface: one object, nine methods, two signals.
//!
//! Everything here is translation. A D-Bus call arrives, [`Model`] and [`Layout`]
//! decide what it means, and the answer goes back. Neither of those knows anything
//! about [`gio`], which is what lets the whole of the provider's logic be exercised
//! by a test with no bus, no session and no main context — the same arrangement
//! `uflinux-searchd` uses, and for the same reason: the interesting bugs in a
//! service like this are about which icon went into which cell, and a test that
//! needs a session bus to find one is a test nobody runs.
//!
//! One JSON string in, one JSON string out — the shape `org.uflinux.Search1`
//! already uses. Adding a method means adding a variant to [`Request`] and a line to
//! the introspection data below; it never means inventing a signature, and
//! validation happens in one place rather than in nine argument lists.
//!
//! There is one exception to "one string out", and it is the reason this module is
//! longer than a pure translator: a thumbnail is bytes, and the protocol says they
//! travel as a file descriptor. [`Outcome::Reply`] therefore carries an optional
//! buffer, and [`Outcome::apply`] is where that becomes a memory descriptor attached
//! to the reply. Every other reply path leaves it `None` and pays nothing.
//!
//! The state sits behind one [`Mutex`] rather than being split per field. A request
//! reads the model, changes it and saves the layout as one unit — a `Place` that
//! updated the model and then failed to write the file would move an icon for the
//! length of one session and put it back on restart — and the provider runs on the
//! one thread that owns the connection, so the lock is never actually contended. It
//! is there for the shape of the code, not for throughput.
//!
//! There is no per-client bookkeeping, and there could not be: GIO 0.21 does not
//! bind `g_bus_watch_name`, so the provider cannot ask the bus whether a renderer
//! owns its half of the interface. `Status.renderer` is therefore answered from the
//! only thing that is observable — has anything been asked of us at all — which is
//! the question the `--desktop-icons` fallback actually wants to ask.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use glib::translate::{IntoGlib as _, ToGlibPtr as _};
use glib::variant::ToVariant as _;

use gio::prelude::*;

use ufm_core::DesktopSlot;
use ufm_desktop_ipc::{
    CHANGED, Cell, Changed, DEFAULT_THUMBNAIL, Geometry, Layout, MAX_MONITORS, Model, OBJECT_PATH,
    PROGRESS, PROTOCOL_VERSION, Phase, Progress, Reply, Request, SESSION_NAME, StatusReply,
    Thumbnail, WireError,
};

use super::scan::Scan;
use super::thumb;

/// The interface, in the form the bus serves it and the form a client asks for.
///
/// Every method takes one string and returns one string, so this does not change
/// when the protocol does — only the JSON inside those strings does. The one method
/// whose reply refers to something outside the message is `RequestThumbnail`, and
/// that is carried by a descriptor rather than by a signature.
pub const XML: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/Standards/dbus/introspect.dtd">
<node>
  <interface name="org.uflinux.FileManager.Desktop1">
    <method name="Status">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="GetModel">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="SetGeometry">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="Place">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="Launch">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="Reveal">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="ContextMenu">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="Trash">
      <arg name="body" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <method name="RequestThumbnail">
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

/// The method name a request is allowed to arrive on.
///
/// [`Request`] is internally tagged with its own method, so a body can be checked
/// against the message that carried it. That is what stops a `Place` sent to `Launch`
/// from being accepted, and turns a renderer with two swapped call sites into one
/// visible error instead of a silently wrong desktop. The names are spelled out here
/// rather than derived from [`Request::kind`] because the bus uses `CamelCase` and
/// the wire uses `kebab-case`, and the mapping is part of the protocol whichever way
/// it is written down.
fn bus_method(request: &Request) -> &'static str {
    match request {
        Request::Status => "Status",
        Request::GetModel => "GetModel",
        Request::SetGeometry { .. } => "SetGeometry",
        Request::Place { .. } => "Place",
        Request::Launch { .. } => "Launch",
        Request::Reveal { .. } => "Reveal",
        Request::ContextMenu { .. } => "ContextMenu",
        Request::Trash { .. } => "Trash",
        Request::RequestThumbnail { .. } => "RequestThumbnail",
    }
}

/// What the bus handler holds.
///
/// `Send + Sync` because GIO boxes the dispatch closure. Nothing here is touched from
/// more than one thread in practice — the provider has no worker — but the closure's
/// bound decides the type, and an `Arc`/`Mutex` pair costs nothing over
/// `Rc`/`RefCell` when the lock is never taken twice.
struct Core {
    /// Absent in a test core, which has no session bus to emit on. Everything else —
    /// the dispatch rules, the placement, the persistence — behaves identically,
    /// which is what lets the provider be tested without a bus.
    connection: Option<gio::DBusConnection>,
    /// Where placements are saved. A field rather than [`ufm_desktop_ipc::path`] so
    /// that a test can point a whole provider at a temporary directory.
    file: PathBuf,
    state: Mutex<State>,
}

/// Everything the provider knows, as one unit.
struct State {
    model: Model,
    layout: Layout,
    /// Whether this target has the memory for thumbnails. Provider policy rather
    /// than directory content, so it survives a rescan instead of being read back out
    /// of a freshly built model.
    thumbnails: bool,
    /// Whether this build can encode a PNG at all. Cached because
    /// [`gdk_pixbuf::Pixbuf::formats`] allocates and `Status` is asked often.
    loader: bool,
    thumbnail_size: u32,
    /// The desktop directory could not be read. Reported in `Status.note`, because an
    /// empty desktop and an unreadable one look the same to a renderer and want
    /// opposite messages.
    unreadable: bool,
    /// Anything else worth telling a person: a layout file that did not parse, a
    /// launch the operating system refused.
    note: Option<String>,
    /// Whether anything has ever asked a question. `Status.renderer` is the
    /// provider's only answer to "is anyone listening".
    asked: bool,
}

impl Core {
    /// A core with no bus, over a desktop and a layout file the caller chose.
    #[cfg(test)]
    fn alone(desktop: &Path, file: &Path) -> Self {
        Self {
            connection: None,
            file: file.to_path_buf(),
            state: Mutex::new(State::alone(desktop.to_path_buf())),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Emit one signal. A failure means no client is listening, which is the normal
    /// state of a session whose shell has no icon layer.
    fn emit(&self, signal: &str, body: &[u8]) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let text = String::from_utf8_lossy(body).into_owned();
        let _ = connection.emit_signal(
            None,
            OBJECT_PATH,
            SESSION_NAME,
            signal,
            Some(&(text,).to_variant()),
        );
    }

    /// Tell every renderer the model has a new generation.
    fn announce(&self, body: &[u8]) {
        self.emit(CHANGED, body);
    }

    /// Tell every renderer what the provider is doing right now.
    fn report(&self, phase: Phase, done: u64) {
        self.emit(
            PROGRESS,
            &Progress {
                phase,
                done,
                total: 0,
            }
            .encode(),
        );
    }

    /// Read the desktop again, and announce it if anything a renderer paints moved.
    ///
    /// The two `Progress` reports bracket the read, and are emitted here rather than
    /// inside [`State::rescan`] because that function holds the state lock while a
    /// signal handler on this same thread could call straight back into it.
    fn refresh(&self) {
        self.report(Phase::Scanning, 0);
        let body = {
            let mut state = self.state();
            let body = state.rescan(&self.file);
            let items = state.model.items.len() as u64;
            drop(state);
            self.report(Phase::Done, items);
            body
        };
        if let Some(body) = body {
            self.announce(&body);
        }
    }
}

impl State {
    /// A state that has read nothing yet, for a test core.
    #[cfg(test)]
    fn alone(desktop: PathBuf) -> Self {
        Self {
            model: Model::new(&desktop),
            layout: Layout::new(),
            thumbnails: false,
            loader: false,
            thumbnail_size: DEFAULT_THUMBNAIL,
            unreadable: false,
            note: None,
            asked: false,
        }
    }

    /// Read the desktop directory and put every icon where the layout says.
    ///
    /// Returns the `Changed` body to announce, or `None` when nothing a renderer
    /// paints changed. Those are the same question, and asking it once is why this
    /// returns the body rather than a bool: the generation has to be read from the
    /// model that was just built, not from a copy made afterwards.
    fn rescan(&mut self, file: &Path) -> Option<Vec<u8>> {
        let scan = Scan::read(Path::new(&self.model.desktop));
        self.unreadable = !scan.readable;
        let before = Drawn::of(&self.model);
        let (mut model, applied) = scan.merge(&mut self.layout);
        model.thumbnails = self.thumbnails && self.loader;
        model.thumbnail_size = self.thumbnail_size;
        let after = Drawn::of(&model);
        // One comparison decides both questions, because they are the same question.
        // A merge that restored every icon to the cell it was already painted in is
        // not a change, and bumping the generation for it would cost every renderer a
        // repaint every time the watcher fired for an unrelated reason.
        if before == after {
            self.model = model;
            self.layout = Layout::capture(&self.model);
            return None;
        }
        model.bump();
        self.model = model;
        self.layout = Layout::capture(&self.model);
        let _ = self.layout.save(file);
        Some(Changed::of(&self.model, scan.explain(&applied)).encode())
    }

    /// Take the placements from the model and save them, so a move survives a restart.
    fn remember(&mut self, file: &Path) {
        self.layout = Layout::capture(&self.model);
        let _ = self.layout.save(file);
    }

    /// What the provider can say about itself.
    fn status(&self) -> StatusReply {
        StatusReply {
            version: PROTOCOL_VERSION,
            desktop: self.model.desktop.clone(),
            generation: self.model.generation,
            items: self.model.items.len(),
            monitors: self.model.monitors.len(),
            writable: self.model.writable,
            renderer: self.asked,
            note: self.explain(),
        }
    }

    /// Everything worth telling a client, in one line: an unreadable directory, the
    /// last refusal, a machine that wants thumbnails and a build that cannot make
    /// them.
    fn explain(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.unreadable {
            parts.push(format!("cannot read {}", self.model.desktop));
        }
        if let Some(note) = &self.note {
            parts.push(note.clone());
        }
        if self.thumbnails && !self.loader {
            parts.push("no PNG loader, thumbnails are off".to_string());
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Everything about a model that a renderer would notice.
///
/// Labels, icons and tooltips are left out on purpose: they come from the directory,
/// and a rescan that found a changed label is not a reason to repaint a whole
/// desktop. The generation is left out for the same reason — it is the thing being
/// announced, not a thing to compare. A struct rather than a tuple because the three
/// fields are not obvious from their types.
#[derive(Debug, PartialEq, Eq)]
struct Drawn {
    monitors: Vec<Geometry>,
    writable: bool,
    cells: Vec<(String, Option<Cell>)>,
}

impl Drawn {
    fn of(model: &Model) -> Self {
        Self {
            monitors: model.monitors.clone(),
            writable: model.writable,
            cells: model.items.iter().map(|i| (i.id.clone(), i.at)).collect(),
        }
    }
}

/// What a request produced.
enum Outcome {
    /// A reply body, and the PNG to hand over with it when the answer is a thumbnail.
    Reply { body: Vec<u8>, png: Option<Vec<u8>> },
    /// A refusal, in the protocol's own words.
    Refuse(WireError),
}

impl Outcome {
    /// Answer the call.
    ///
    /// The descriptor is made here rather than in [`handle`] so that the whole of
    /// dispatch stays a pure function of the state and can be tested without one. A
    /// thumbnail whose descriptor could not be made becomes the icon fallback: the
    /// alternative is telling a renderer its request failed when the honest answer is
    /// that there is no picture to show, and those are different things.
    fn apply(self, invocation: gio::DBusMethodInvocation) {
        let (body, png) = match self {
            Self::Refuse(error) => {
                invocation.return_dbus_error(error.dbus_name(), error.message());
                return;
            }
            Self::Reply { body, png } => (body, png),
        };
        let text = String::from_utf8_lossy(&body).into_owned();
        let Some(png) = png else {
            invocation.return_value(Some(&(text,).to_variant()));
            return;
        };
        match thumb::to_memfd(&png) {
            Ok(file) => {
                let list = gio::UnixFDList::from_array([file]);
                invocation.return_value_with_unix_fd_list(Some(&(text,).to_variant()), Some(&list));
            }
            // No descriptor means no bytes, so the body has to stop promising one.
            // `Thumbnail::icon()` is the answer a missing thumbnail already gets,
            // which is why this path needs no new protocol state.
            Err(_) => invocation.return_value(Some(&(icon_reply(),).to_variant())),
        }
    }
}

/// The body for "there is no thumbnail", for when a descriptor could not be made.
fn icon_reply() -> String {
    String::from_utf8_lossy(
        &Reply::Thumbnail(Thumbnail::icon())
            .encode()
            .unwrap_or_default(),
    )
    .into_owned()
}

/// Parse, check, dispatch, and turn any failure into a refusal.
fn handle(core: &Core, method: &str, parameters: glib::Variant) -> Outcome {
    let Some(body) = parameters.get::<(String,)>().map(|(t,)| t) else {
        return Outcome::Refuse(WireError::Invalid);
    };
    let Ok(request) = Request::decode(body.as_bytes()) else {
        return Outcome::Refuse(WireError::Invalid);
    };
    if bus_method(&request) != method {
        return Outcome::Refuse(WireError::Invalid);
    }
    let mut state = core.state();
    let outcome = dispatch(core, &mut state, request);
    // After the dispatch, not before: the answer to "is anyone listening" has to
    // describe the state of the session as it was when the question was asked, or the
    // first `Status` of a session would report a renderer because a renderer asked.
    state.asked = true;
    outcome
}

/// One request, against the state it acts on.
///
/// Split out from [`handle`] so a test can drive it with a lock already held and a
/// core that has no bus, and so the two things that vary — whether the model changed,
/// and whether the answer needs a descriptor — are decided in one place.
fn dispatch(core: &Core, state: &mut State, request: Request) -> Outcome {
    match request {
        Request::Status => reply(Reply::Status(state.status())),
        Request::GetModel => reply(Reply::Model {
            model: state.model.clone(),
        }),
        Request::SetGeometry { monitors } => set_geometry(core, state, monitors),
        Request::Place {
            id,
            monitor,
            col,
            row,
        } => place(
            core,
            state,
            &id,
            Cell::new(monitor, DesktopSlot { col, row }),
        ),
        Request::Launch { id } => act(state, &id, launch),
        // Reveal and Context Menu want the same thing from the provider: the item is
        // on screen and the renderer takes it from there. What differs is what the
        // renderer does next, which is not this process's business.
        Request::Reveal { id } | Request::ContextMenu { id } => act(state, &id, reveal),
        Request::Trash { id } => trash(core, state, &id),
        Request::RequestThumbnail { path, size } => thumbnail(state, &path, size),
    }
}

/// Build a reply, turning an encoding failure into the protocol's own refusal.
fn reply(reply: Reply) -> Outcome {
    match reply.encode() {
        Ok(body) => Outcome::Reply { body, png: None },
        Err(error) => Outcome::Refuse(error),
    }
}

/// Refuse with a reason worth keeping in `Status.note` as well as in the error
/// message, for the failures a person has to diagnose from a log after the fact.
///
/// The D-Bus error name carries only three states, so a message that has to outlive
/// the call needs somewhere to be kept, and the one place a client is guaranteed to
/// look is the note in `Status`.
fn refused(state: &mut State, message: String) -> Outcome {
    state.note = Some(message);
    Outcome::Refuse(WireError::Invalid)
}

/// Replace the grids, and say which icons no longer fit.
///
/// Validated here rather than left to [`Layout::set_geometry`], which normalises a bad
/// grid away in silence: a renderer that sends a zero-sized screen has a bug, and an
/// error it can see is more use than a desktop that quietly kept the old layout.
fn set_geometry(core: &Core, state: &mut State, monitors: Vec<Geometry>) -> Outcome {
    if monitors.len() > MAX_MONITORS || !unique_and_usable(&monitors) {
        return Outcome::Refuse(WireError::Invalid);
    }
    state.layout.set_geometry(monitors.clone());
    // The grids have to reach the model as well as the layout, because
    // [`Layout::apply`] asks the model which monitors exist rather than consulting
    // the layout it is being applied from. The call cannot fail: the same checks
    // were just made above, and against stricter limits.
    let _ = state.model.set_geometry(monitors);
    let applied = state.layout.apply(&mut state.model);
    state.model.bump();
    state.remember(&core.file);
    let reason = (!applied.homeless.is_empty()).then(|| {
        format!(
            "{} no longer fit, {} moved",
            applied.homeless.len(),
            applied.restored + applied.placed
        )
    });
    core.announce(&Changed::of(&state.model, reason).encode());
    reply(Reply::Ok)
}

/// Every grid has room in it and no monitor number appears twice.
fn unique_and_usable(monitors: &[Geometry]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    monitors
        .iter()
        .all(|grid| grid.usable() && seen.insert(grid.monitor))
}

/// Move one icon to one cell and remember it.
fn place(core: &Core, state: &mut State, id: &str, cell: Cell) -> Outcome {
    match state.model.place(id, cell) {
        Ok(_) => {
            state.model.bump();
            state.remember(&core.file);
            core.announce(&Changed::of(&state.model, Some(format!("{id} moved"))).encode());
            reply(Reply::Ok)
        }
        Err(ufm_desktop_ipc::ModelError::UnknownItem { .. }) => Outcome::Refuse(WireError::NoItem),
        Err(_) => Outcome::Refuse(WireError::Invalid),
    }
}

/// Move one icon to the trash and re-read the directory.
fn trash(core: &Core, state: &mut State, id: &str) -> Outcome {
    // The item has to be in the model, not merely look like a name that belongs
    // there: `path_in` checks an id for path components and nothing else, so
    // without this a caller could trash a dot-file, or an entry the scan had to
    // drop at the cap, by naming it. An icon the model does not know about is not
    // something the renderer could have been showing.
    if state.model.item(id).is_none() {
        return Outcome::Refuse(WireError::NoItem);
    }
    let Some(path) = state.model.path_in(id) else {
        return Outcome::Refuse(WireError::NoItem);
    };
    if let Err(e) = gio::File::for_path(&path).trash(gio::Cancellable::NONE) {
        // A filesystem with no trash directory is a refusal with a reason, not a
        // protocol fault, and the message a person sees should be the OS's.
        return refused(state, format!("cannot trash {id}: {}", e.message()));
    }
    state.layout.forget(id);
    if let Some(body) = state.rescan(&core.file) {
        core.announce(&body);
    }
    reply(Reply::Ok)
}

/// Answer a thumbnail request.
fn thumbnail(state: &State, path: &str, size: u32) -> Outcome {
    let job = thumb::Job::new(Path::new(path), size);
    // The renderer sends a path and the provider re-validates it. Without this the
    // method is a way to make a 10 MiB process decode any image on the machine, and
    // the desktop directory is the only scope that makes sense for a desktop icon.
    if !job.inside(Path::new(&state.model.desktop)) || !state.model.thumbnails {
        return reply(Reply::Thumbnail(Thumbnail::icon()));
    }
    let answer = thumb::answer(&job);
    match answer.png {
        None => reply(Reply::Thumbnail(answer.body)),
        Some(png) => match Reply::Thumbnail(answer.body).encode() {
            Ok(body) => Outcome::Reply {
                body,
                png: Some(png),
            },
            Err(error) => Outcome::Refuse(error),
        },
    }
}

/// Run one request that acts on one item, refusing an id that is not on the desktop
/// before anything is attempted.
fn act(
    state: &mut State,
    id: &str,
    then: impl FnOnce(&Path, ufm_desktop_ipc::Kind) -> Result<(), String>,
) -> Outcome {
    let Some(kind) = state.model.item(id).map(|item| item.kind) else {
        return Outcome::Refuse(WireError::NoItem);
    };
    let Some(path) = state.model.path_in(id) else {
        return Outcome::Refuse(WireError::NoItem);
    };
    then(&path, kind).map_or_else(|message| refused(state, message), |()| reply(Reply::Ok))
}

/// Double-click. A launcher runs its `Exec`; anything else goes to whatever the user's
/// MIME defaults say, which is the same answer the file manager's own double-click
/// gives and the reason a renderer needs no knowledge of either.
fn launch(path: &Path, kind: ufm_desktop_ipc::Kind) -> Result<(), String> {
    if kind == ufm_desktop_ipc::Kind::Launcher {
        let Some(app) = gio::DesktopAppInfo::from_filename(path) else {
            return Err(format!("{} is not a usable application", path.display()));
        };
        return app
            .launch(&[], gio::AppLaunchContext::NONE)
            .map_err(|e| format!("cannot start {}: {}", path.display(), e.message()));
    }
    open(path)
}

/// Show the folder an icon lives in.
///
/// `Reveal` and `ContextMenu` both do this, and neither can be more precise from here:
/// selecting one file inside another process's window is a request this protocol does
/// not carry, and a context menu belongs to whichever window ends up showing the
/// directory. Opening the parent is the useful half of both, and doing nothing for
/// either would leave a right-click on a desktop icon with no answer at all.
fn reveal(path: &Path, _kind: ufm_desktop_ipc::Kind) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!("{} has no folder to show", path.display()));
    };
    open(parent)
}

/// Ask the desktop for whatever handles this path.
fn open(path: &Path) -> Result<(), String> {
    let uri = gio::File::for_path(path).uri().to_string();
    gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
        .map_err(|e| format!("cannot open {uri}: {}", e.message()))
}

/// The served object, plus the name it holds and the object it registered.
///
/// Not `Clone`, and held behind an `Arc` by the caller: each field is a lease that must
/// exist exactly once, and dropping the service gives both back — see [`Drop`].
pub struct Service {
    core: Arc<Core>,
    /// What `g_bus_own_name_on_connection` returned. A number rather than a
    /// [`gio::OwnerId`] because the only way to ask for the name without gio-rs's
    /// callbacks is the C entry point, which does not hand back a wrapper type. See
    /// [`own_name`].
    owner: u32,
    /// `Option` because there is no way to build a spent one — the id inside is private
    /// and there is no `from_raw` — and [`Drop`] has to be able to take the live value
    /// out. `None` therefore means "already withdrawn", never "never registered".
    ///
    /// No `Drop` impl exists for this type, so the field is only what keeps the
    /// registration alive: dropping it early would take the object off the bus while the
    /// name stayed claimed, which is a service that answers every call with
    /// `UnknownObject`. Released in [`Drop`], where the connection is at hand.
    registration: Option<gio::RegistrationId>,
}

/// Ask for the bus name, with no callback for either outcome.
///
/// gio-rs 0.21 binds this only through [`gio::bus_own_name_on_connection`], which wraps
/// both callbacks in a closure that unwraps the connection it is handed
/// (`gio-0.21.5/src/dbus.rs:19`). `GLib` is allowed to pass `NULL` there: when the bus goes
/// away, the ownership bookkeeping drops its reference to the connection before reporting
/// the loss, and the C contract says the argument may be null. So the callback gio-rs
/// would install aborts the process at session teardown — a panic inside a closure that
/// cannot unwind — and a service that lives as long as the desktop therefore leaves a
/// crash report behind on every logout. Measured: `dbus-run-session` with the provider
/// serving, the bus exiting, and the provider silent until the abort.
///
/// The plain C entry point takes both handlers as nullable, so this asks for the name and
/// nothing else. Nothing is lost: the provider has no use for either event, because a
/// service that has lost the bus has nobody left to serve, and [`super::watch_bus`] quits
/// the loop for exactly that case.
fn own_name(connection: &gio::DBusConnection) -> u32 {
    let name = std::ffi::CString::new(SESSION_NAME)
        .expect("the interface name is a literal with no interior NUL");
    // SAFETY: both handlers are null, which the C declaration documents as nullable; the
    // user data is null and has to be, since no destroy notify is given; the connection
    // and the name both outlive the call.
    unsafe {
        gio::ffi::g_bus_own_name_on_connection(
            connection.to_glib_none().0,
            name.as_ptr(),
            gio::BusNameOwnerFlags::ALLOW_REPLACEMENT.into_glib(),
            None,
            None,
            std::ptr::null_mut(),
            None,
        )
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // Withdraw the object first: a client that still sees the name should get
        // `UnknownObject` rather than a reply from a service whose state is on its way
        // out. Both calls are skipped once the bus has closed, because GLib then retires
        // the registration and the ownership itself, and touching that bookkeeping again
        // is touching memory GLib has already freed.
        let Some(connection) = self.core.connection.as_ref() else {
            return;
        };
        if connection.is_closed() {
            return;
        }
        if let Some(registration) = self.registration.take() {
            let _ = connection.unregister_object(registration);
        }
        // SAFETY: the id came from `g_bus_own_name_on_connection` in `own_name`, is used
        // exactly once, and the connection is open, so the ownership is still tracked.
        unsafe { gio::ffi::g_bus_unown_name(self.owner) };
    }
}

impl Service {
    /// Register the object, take the name, and read the desktop once.
    ///
    /// `ALLOW_REPLACEMENT` rather than `REPLACE`, for the same reason the search daemon
    /// uses it: during an upgrade the old provider is usually still holding the name
    /// while the new one starts, and forcing it out mid-move loses a placement rather
    /// than losing a cache.
    ///
    /// The first read happens before this returns, so the first client that arrives
    /// gets a desktop rather than an empty model and a signal thirty milliseconds
    /// later. A desktop directory holds tens of entries at one `stat` each, which is
    /// why that is affordable here and why the search daemon, which walks a whole
    /// tree, does not do the same.
    pub fn serve(
        connection: gio::DBusConnection,
        desktop: &Path,
        file: PathBuf,
        note: Option<String>,
    ) -> Result<Self, String> {
        let info = gio::DBusNodeInfo::for_xml(XML)
            .map_err(|e| format!("the introspection data is malformed: {e}"))?;
        let interface = info
            .interfaces()
            .first()
            .ok_or("the introspection data declares no interface")?
            .clone();

        // A layout that does not parse is not a reason to refuse to start: the icons
        // are still on the disk and still want to be drawn, and the worst case is that
        // they are laid out from scratch. The note is what makes that visible instead
        // of it looking as though the user's arrangement had been thrown away.
        let (mut layout, broken) = match Layout::load(&file) {
            Ok(layout) => (layout, None),
            Err(e) => (
                Layout::new(),
                Some(format!("{} ignored: {e}", file.display())),
            ),
        };
        let wanted = thumbnails_wanted();
        layout.thumbnails = wanted;
        let state = State {
            model: Model::new(desktop),
            thumbnails: wanted,
            loader: thumb::available(),
            thumbnail_size: advertised_size(layout.thumbnail_size),
            layout,
            unreadable: false,
            note: join_notes(broken, note),
            asked: false,
        };
        let core = Arc::new(Core {
            connection: Some(connection.clone()),
            file,
            state: Mutex::new(state),
        });

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

        let owner = own_name(&connection);

        let _ = core.state().rescan(&core.file);
        Ok(Self {
            core,
            owner,
            registration: Some(registration),
        })
    }

    /// Read the desktop again, because the watcher said something changed.
    pub fn refresh(&self) {
        self.core.refresh();
    }

    /// The directory being served, for a log line.
    #[must_use]
    pub fn desktop(&self) -> String {
        self.core.state().model.desktop.clone()
    }

    /// The bus this service is served on, for watching it. `None` only in a test core.
    #[must_use]
    pub fn connection(&self) -> Option<&gio::DBusConnection> {
        self.core.connection.as_ref()
    }
}

/// Two optional notes, or none, as one line.
fn join_notes(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (one, None) => one,
        (None, other) => other,
    }
}

/// The thumbnail edge to advertise for a size saved in a layout.
///
/// Clamped at both ends rather than raised. The number is what the model tells a
/// renderer to ask for, and an edge this process would not decode is a request that
/// always comes back as an icon name — a renderer would then conclude that
/// thumbnails do not work here rather than that it had asked for an unreasonable
/// size. The floor is the model's own default, because a layout holding less than
/// that carries a number from a version of this program that had other plans.
#[must_use]
fn advertised_size(saved: u32) -> u32 {
    saved.clamp(DEFAULT_THUMBNAIL, thumb::MAX_SIZE)
}

/// Whether this target should spend memory on thumbnails.
///
/// Off by default on a small machine, which is the rule in the integration document
/// and the one thing here that decides whether the cache directory is ever created.
/// `GetModel` reports the answer in its `thumbnails` field, so a renderer stops asking
/// rather than being refused nine times a second.
#[must_use]
pub fn thumbnails_wanted() -> bool {
    total_memory_kib().is_none_or(|kib| kib > 512 * 1024)
}

/// Total RAM, in KiB, from `/proc/meminfo`. `None` off Linux, where the answer is
/// assumed to be a machine with room for thumbnails.
fn total_memory_kib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
    line.split_whitespace().nth(1)?.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use ufm_desktop_ipc::{Format, Item, Kind};

    /// A provider over a real directory and a real layout file, with one screen
    /// reported and the directory read once — the state a renderer's first two calls
    /// leave a live provider in, which is what every test below starts from.
    struct Ctx {
        /// Kept only so the directory outlives the core. Named with an underscore
        /// because nothing reads it: the point is when it is dropped.
        _root: tempfile::TempDir,
        desktop: PathBuf,
        file: PathBuf,
        core: Core,
        /// The generation the setup left behind, so a test can say "this changed"
        /// without hard-coding how many changes the setup itself made.
        setup: u64,
    }

    fn ctx(names: &[&str]) -> Ctx {
        let root = tempfile::tempdir().expect("a temporary desktop");
        let desktop = root.path().join("Desktop");
        fs::create_dir_all(&desktop).expect("a desktop directory");
        for name in names {
            fs::write(desktop.join(name), b"contents").expect("a desktop file");
        }
        let file = root.path().join("desktop.json");
        let core = Core::alone(&desktop, &file);
        let monitors = vec![Geometry::new(0, 4, 4, 96, 96)];
        ok(ask(&core, Request::SetGeometry { monitors }));
        let setup = core.state().model.generation;
        let _ = core.state().rescan(&file);
        Ctx {
            _root: root,
            desktop,
            file,
            core,
            setup,
        }
    }

    /// Send a request the way the bus would: encoded, wrapped in a tuple, and
    /// arriving on the method its own body names.
    fn ask(core: &Core, request: Request) -> Outcome {
        let body = serde_json::to_vec(&request).expect("a request that serializes");
        let text = String::from_utf8(body).expect("a request that is UTF-8");
        let method = bus_method(&request);
        handle(core, method, (text,).to_variant())
    }

    /// The same body, deliberately sent to the wrong method.
    fn ask_on(core: &Core, method: &str, request: Request) -> Outcome {
        let body = serde_json::to_vec(&request).expect("a request that serializes");
        let text = String::from_utf8(body).expect("a request that is UTF-8");
        handle(core, method, (text,).to_variant())
    }

    /// The reply a request got, panicking on a refusal so a test that expects an
    /// answer says so in one line.
    fn ok(outcome: Outcome) -> Reply {
        match outcome {
            Outcome::Reply { body, png } => {
                assert!(png.is_none(), "a plain reply carries no pixels");
                serde_json::from_slice(&body).expect("a reply the provider would send")
            }
            Outcome::Refuse(error) => panic!("refused: {error:?}"),
        }
    }

    fn refusal(outcome: Outcome) -> WireError {
        match outcome {
            Outcome::Refuse(error) => error,
            Outcome::Reply { .. } => panic!("a reply where a refusal was expected"),
        }
    }

    /// The model as the provider now holds it, without going through the bus.
    fn model(core: &Core) -> Model {
        core.state().model.clone()
    }

    fn cell(core: &Core, id: &str) -> Option<Cell> {
        model(core).cell_of(id)
    }

    /// One representative of every request, so a new variant cannot be added to the
    /// protocol without meeting the test below.
    fn every_request() -> Vec<Request> {
        vec![
            Request::Status,
            Request::GetModel,
            Request::SetGeometry {
                monitors: vec![Geometry::new(0, 4, 4, 96, 96)],
            },
            Request::Place {
                id: "a".to_string(),
                monitor: 0,
                col: 1,
                row: 1,
            },
            Request::Launch {
                id: "a".to_string(),
            },
            Request::Reveal {
                id: "a".to_string(),
            },
            Request::ContextMenu {
                id: "a".to_string(),
            },
            Request::Trash {
                id: "a".to_string(),
            },
            Request::RequestThumbnail {
                path: "/home/u/Desktop/a.png".to_string(),
                size: 128,
            },
        ]
    }

    #[test]
    fn the_introspection_data_declares_every_method_and_no_other() {
        for request in every_request() {
            let name = bus_method(&request);
            assert!(
                XML.contains(&format!("<method name=\"{name}\">")),
                "{name} is reachable in code but not on the bus"
            );
        }
        assert_eq!(
            XML.matches("<method name=").count(),
            every_request().len(),
            "a method on the bus that no request can arrive on is a method nobody calls"
        );
        assert!(XML.contains(&format!("<interface name=\"{SESSION_NAME}\">")));
        assert!(XML.contains(&format!("<signal name=\"{CHANGED}\">")));
        assert!(XML.contains(&format!("<signal name=\"{PROGRESS}\">")));
    }

    #[test]
    fn the_bus_activation_file_names_this_interface_and_this_command() {
        // A typo in the shipped `.service` file produces no error message at all —
        // just a desktop with no icons, because the provider is never activated. The
        // name and the path are pinned here rather than trusted to match.
        let file = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../data/org.uflinux.FileManager.Desktop1.service"
        );
        let text = fs::read_to_string(file).expect("the activation file ships");
        assert!(
            text.contains(&format!("Name={SESSION_NAME}")),
            "the bus name in {file} is not the one the provider takes"
        );
        assert!(
            text.contains("Exec=/usr/bin/uflinux-files --provider"),
            "activation has to run the mode that serves this interface"
        );
    }

    #[test]
    fn every_request_names_the_method_it_arrives_on() {
        // The two spellings — `CamelCase` on the bus, `kebab-case` on the wire — are
        // both part of the protocol, so the mapping is checked against the variant
        // names rather than against a second hand-written list.
        for request in every_request() {
            let name = bus_method(&request);
            assert_eq!(
                name.to_lowercase(),
                request.kind().replace('-', ""),
                "{} maps to a method name that is not its own kind",
                request.kind()
            );
        }
    }

    #[test]
    fn a_request_on_the_wrong_method_is_refused() {
        let started = ctx(&["a"]);
        let place = Request::Place {
            id: "a".to_string(),
            monitor: 0,
            col: 2,
            row: 0,
        };
        assert_eq!(
            refusal(ask_on(&started.core, "Launch", place)),
            WireError::Invalid
        );
        assert_eq!(
            cell(&started.core, "a"),
            Some(Cell::new(0, DesktopSlot { col: 0, row: 0 })),
            "a refused move leaves the icon where it was"
        );
    }

    #[test]
    fn a_body_that_is_not_for_this_interface_is_refused() {
        let started = ctx(&["a"]);
        let outcome = handle(&started.core, "Status", (String::from("{}"),).to_variant());
        assert_eq!(refusal(outcome), WireError::Invalid);
        let outcome = handle(
            &started.core,
            "Status",
            (String::from("nonsense"),).to_variant(),
        );
        assert_eq!(refusal(outcome), WireError::Invalid);
    }

    #[test]
    fn the_first_status_says_nobody_has_listened_yet() {
        // The fallback renderer decides whether to draw a layer of its own from this
        // flag, so the answer has to describe the session, not the request.
        let core = Core::alone(
            Path::new("/nonexistent"),
            Path::new("/nonexistent/desktop.json"),
        );
        let Reply::Status(first) = ok(ask(&core, Request::Status)) else {
            panic!("Status does not answer with a status");
        };
        assert!(!first.renderer, "asking is not the same as being there");
        assert_eq!(first.version, PROTOCOL_VERSION);
        assert_eq!(first.items, 0);
        let Reply::Status(second) = ok(ask(&core, Request::Status)) else {
            panic!("Status does not answer with a status");
        };
        assert!(second.renderer, "by the second call, someone is");
    }

    #[test]
    fn a_screen_is_reported_and_icons_are_placed() {
        let started = ctx(&["b.txt", "a.txt"]);
        let got = model(&started.core);
        assert_eq!(got.items.len(), 2);
        assert_eq!(got.monitors.len(), 1);
        assert_eq!(
            got.cell_of("a.txt"),
            Some(Cell::new(0, DesktopSlot { col: 0, row: 0 })),
            "the first cell goes to the first name, and names are sorted"
        );
        assert_eq!(
            got.cell_of("b.txt"),
            Some(Cell::new(0, DesktopSlot { col: 1, row: 0 }))
        );
        assert_eq!(
            got.generation,
            started.setup + 1,
            "placing icons is a change"
        );
        let Reply::Model { model } = ok(ask(&started.core, Request::GetModel)) else {
            panic!("GetModel does not answer with a model");
        };
        assert_eq!(model, got, "the bus and the state agree");
    }

    #[test]
    fn a_grid_that_cannot_hold_a_desktop_is_refused() {
        let started = ctx(&["a"]);
        let before = model(&started.core);
        let empty = vec![Geometry::new(0, 0, 4, 96, 96)];
        assert_eq!(
            refusal(ask(&started.core, Request::SetGeometry { monitors: empty })),
            WireError::Invalid,
            "a zero-sized screen is a bug in the renderer, not an empty desktop"
        );
        let twice = vec![
            Geometry::new(0, 4, 4, 96, 96),
            Geometry::new(0, 8, 8, 96, 96),
        ];
        assert_eq!(
            refusal(ask(&started.core, Request::SetGeometry { monitors: twice })),
            WireError::Invalid,
            "one monitor cannot report two grids"
        );
        assert_eq!(
            model(&started.core),
            before,
            "a refused grid changes nothing"
        );
    }

    #[test]
    fn unplugging_a_screen_moves_what_was_on_it() {
        let started = ctx(&["a", "b"]);
        let monitors = vec![
            Geometry::new(0, 4, 4, 96, 96),
            Geometry::new(1, 2, 2, 96, 96),
        ];
        ok(ask(&started.core, Request::SetGeometry { monitors }));
        ok(ask(
            &started.core,
            Request::Place {
                id: "a".to_string(),
                monitor: 1,
                col: 1,
                row: 0,
            },
        ));
        assert_eq!(
            cell(&started.core, "a"),
            Some(Cell::new(1, DesktopSlot { col: 1, row: 0 }))
        );
        // The second screen goes away, and the icon on it has to come back somewhere.
        ok(ask(
            &started.core,
            Request::SetGeometry {
                monitors: vec![Geometry::new(0, 4, 4, 96, 96)],
            },
        ));
        let got = model(&started.core);
        assert_eq!(got.items.len(), 2, "nothing is lost with the monitor");
        assert!(
            got.cell_of("a").is_some_and(|c| c.monitor == 0),
            "an orphaned icon is placed again rather than left off screen"
        );
    }

    #[test]
    fn a_move_is_written_down_before_it_is_announced() {
        let started = ctx(&["a"]);
        let to = Cell::new(0, DesktopSlot { col: 3, row: 2 });
        ok(ask(
            &started.core,
            Request::Place {
                id: "a".to_string(),
                monitor: 0,
                col: 3,
                row: 2,
            },
        ));
        assert_eq!(cell(&started.core, "a"), Some(to));
        let saved = Layout::load(&started.file).expect("the layout was written");
        assert_eq!(
            saved.cell_of("a"),
            Some(to),
            "a move that is not saved is a move that is undone by the next login"
        );
        assert!(
            // Six trailing zero bits is the same statement as "no group or other
            // bits", written so that the check is one call rather than a mask.
            fs::metadata(&started.file).is_ok_and(|m| m.permissions().mode().trailing_zeros() >= 6),
            "the layout is the user's, and 0600 is what the rest of the state is"
        );
    }

    #[test]
    fn moving_something_that_is_not_there_says_so() {
        let started = ctx(&["a"]);
        let place = Request::Place {
            id: "gone".to_string(),
            monitor: 0,
            col: 1,
            row: 1,
        };
        assert_eq!(refusal(ask(&started.core, place)), WireError::NoItem);
        for request in [
            Request::Launch {
                id: "gone".to_string(),
            },
            Request::Reveal {
                id: "gone".to_string(),
            },
            Request::ContextMenu {
                id: "gone".to_string(),
            },
            Request::Trash {
                id: "gone".to_string(),
            },
        ] {
            assert_eq!(
                refusal(ask(&started.core, request)),
                WireError::NoItem,
                "an id that is not on the desktop is the same answer whichever method asks"
            );
        }
    }

    #[test]
    fn a_second_read_of_an_unchanged_directory_announces_nothing() {
        let started = ctx(&["a", "b"]);
        let before = model(&started.core).generation;
        assert_eq!(before, started.setup + 1, "the setup placed two icons");
        let body = started.core.state().rescan(&started.file);
        assert!(body.is_none(), "nothing moved, so nothing to repaint");
        assert_eq!(
            model(&started.core).generation,
            before,
            "a generation that moves for no reason costs every desktop a repaint"
        );
    }

    #[test]
    fn a_new_file_announces_a_new_generation() {
        let started = ctx(&["a"]);
        fs::write(started.desktop.join("c.txt"), b"x").expect("a new desktop file");
        let body = started
            .core
            .state()
            .rescan(&started.file)
            .expect("an icon appeared");
        let changed: Changed = serde_json::from_slice(&body).expect("a signal body");
        assert_eq!(changed.items, 2);
        assert_eq!(changed.generation, model(&started.core).generation);
        assert!(
            changed
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("placed")),
            "the signal should say what it did, and not name a file"
        );
        assert!(
            !changed.reason.unwrap().contains("c.txt"),
            "a signal body is a good place for a count and a bad one for a path"
        );
    }

    #[test]
    fn a_deleted_file_stops_being_an_icon() {
        let started = ctx(&["a", "b"]);
        fs::remove_file(started.desktop.join("a")).expect("a deleted desktop file");
        let body = started
            .core
            .state()
            .rescan(&started.file)
            .expect("an icon disappeared");
        let changed: Changed = serde_json::from_slice(&body).expect("a signal body");
        assert_eq!(changed.items, 1);
        assert!(
            changed
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("forgotten")),
            "a stale cell is reported, not silently dropped"
        );
        assert_eq!(model(&started.core).cell_of("a"), None);
        let saved = Layout::load(&started.file).expect("the layout was rewritten");
        assert_eq!(saved.cell_of("a"), None, "no ghost cell for a deleted file");
    }

    #[test]
    fn a_desktop_that_cannot_be_read_is_reported_rather_than_empty() {
        let started = ctx(&[]);
        fs::remove_dir(&started.desktop).expect("the desktop goes away");
        let _ = started.core.state().rescan(&started.file);
        let Reply::Status(status) = ok(ask(&started.core, Request::Status)) else {
            panic!("Status does not answer with a status");
        };
        assert_eq!(status.items, 0);
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|n| n.contains("cannot read")),
            "an empty desktop and an unreadable one look the same to a renderer"
        );
    }

    #[test]
    fn two_notes_become_one_line() {
        assert_eq!(join_notes(None, None), None);
        assert_eq!(
            join_notes(Some("a".to_string()), None),
            Some("a".to_string())
        );
        assert_eq!(
            join_notes(None, Some("b".to_string())),
            Some("b".to_string())
        );
        assert_eq!(
            join_notes(Some("a".to_string()), Some("b".to_string())),
            Some("a; b".to_string())
        );
    }

    #[test]
    fn a_thumbnail_outside_the_desktop_gets_an_icon() {
        // The renderer sends a path, so the provider has to decide what it may look at.
        let started = ctx(&["a"]);
        started.core.state().model.thumbnails = true;
        let outside = Request::RequestThumbnail {
            path: "/etc/passwd".to_string(),
            size: 128,
        };
        let Reply::Thumbnail(body) = ok(ask(&started.core, outside)) else {
            panic!("a thumbnail does not answer with a thumbnail");
        };
        assert_eq!(body.format, Format::Icon);
        assert_eq!(body.fd, None, "no pixels means no descriptor");
    }

    #[test]
    fn a_thumbnail_is_refused_while_thumbnails_are_off() {
        let started = ctx(&["a"]);
        let inside = format!("{}/photo.png", started.desktop.display());
        let request = Request::RequestThumbnail {
            path: inside.clone(),
            size: 128,
        };
        let Reply::Thumbnail(body) = ok(ask(&started.core, request)) else {
            panic!("a thumbnail does not answer with a thumbnail");
        };
        assert_eq!(
            body.format,
            Format::Icon,
            "a model that says thumbnails are off is a promise not to make any"
        );
        started.core.state().model.thumbnails = true;
        let Reply::Thumbnail(body) = ok(ask(
            &started.core,
            Request::RequestThumbnail {
                path: inside,
                size: 128,
            },
        )) else {
            panic!("a thumbnail does not answer with a thumbnail");
        };
        // The container has no PNG loader, and a machine with 512 MiB is told not to
        // ask at all; either way the answer is an icon name rather than an error.
        assert!(body.format == Format::Icon || body.format == Format::Png);
    }

    #[test]
    fn trashing_an_icon_takes_it_off_the_desktop() {
        let started = ctx(&["a", "b"]);
        let outcome = ask(
            &started.core,
            Request::Trash {
                id: "a".to_string(),
            },
        );
        match outcome {
            Outcome::Reply { .. } => {
                assert!(
                    !started.desktop.join("a").exists(),
                    "an accepted trash request moves the file"
                );
                assert_eq!(model(&started.core).items.len(), 1);
            }
            Outcome::Refuse(_) => {
                // A filesystem with no trash directory is a refusal with a reason, and
                // the reason belongs in `Status.note` where a client will find it.
                let Reply::Status(status) = ok(ask(&started.core, Request::Status)) else {
                    panic!("Status does not answer with a status");
                };
                assert!(
                    status
                        .note
                        .as_deref()
                        .is_some_and(|n| n.contains("cannot trash")),
                    "a refusal has to say why: {:?}",
                    status.note
                );
            }
        }
    }

    #[test]
    fn a_launch_that_the_desktop_refuses_is_kept_in_the_note() {
        // Whether anything here can open a folder depends on what the session has
        // installed, so the test is about the two answers that are always available:
        // it worked, or it said why in the one place a client looks.
        let started = ctx(&["a"]);
        match ask(
            &started.core,
            Request::Reveal {
                id: "a".to_string(),
            },
        ) {
            Outcome::Reply { .. } => {}
            Outcome::Refuse(_) => {
                let Reply::Status(status) = ok(ask(&started.core, Request::Status)) else {
                    panic!("Status does not answer with a status");
                };
                assert!(status.note.is_some(), "a refusal with no reason is a bug");
            }
        }
    }

    #[test]
    fn what_a_renderer_notices_excludes_what_a_rescan_cannot_see() {
        let mut left = Model::new(Path::new("/home/u/Desktop"));
        let mut right = Model::new(Path::new("/home/u/Desktop"));
        for (index, model) in [&mut left, &mut right].iter_mut().enumerate() {
            let mut item = Item::new("a", if index == 0 { "one" } else { "two" }, Kind::File);
            item.at = Some(Cell::new(0, DesktopSlot { col: 0, row: 0 }));
            model.push(item).unwrap();
        }
        left.generation = 4;
        right.generation = 9;
        assert_eq!(
            Drawn::of(&left),
            Drawn::of(&right),
            "a renamed label is not a reason to repaint a desktop"
        );
        right.items[0].at = None;
        assert_ne!(Drawn::of(&left), Drawn::of(&right), "a moved icon is");
    }

    #[test]
    fn thumbnails_are_off_where_there_is_no_memory_for_them() {
        // The rule is a threshold on the machine, not a preference; the reading comes
        // from the kernel, so the test checks the two agree rather than the value.
        let kib = total_memory_kib();
        assert_eq!(
            thumbnails_wanted(),
            kib.is_none_or(|kib| kib > 512 * 1024),
            "the decision has to follow the number it is documented as following"
        );
    }

    #[test]
    fn an_advertised_edge_is_one_that_can_be_decoded() {
        // The number goes into the model, so a layout written by anything — an older
        // version, a hand-edited file, a renderer that asked for a huge icon once —
        // cannot make this process promise a thumbnail it would refuse to make.
        assert_eq!(
            advertised_size(0),
            DEFAULT_THUMBNAIL,
            "a layout that saved nothing gets the default"
        );
        assert_eq!(
            advertised_size(64),
            DEFAULT_THUMBNAIL,
            "a saved size below the default is raised, not honoured"
        );
        assert_eq!(advertised_size(DEFAULT_THUMBNAIL), DEFAULT_THUMBNAIL);
        assert_eq!(
            advertised_size(u32::MAX),
            thumb::MAX_SIZE,
            "a size beyond what the decoder will do is capped, not refused"
        );
        assert!(
            advertised_size(0) <= advertised_size(u32::MAX),
            "the clamp cannot invert"
        );
    }
}
