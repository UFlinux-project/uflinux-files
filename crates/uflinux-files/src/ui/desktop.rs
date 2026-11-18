//! The desktop we paint ourselves, for the case where nobody else will.
//!
//! The desktop-icons feature is two halves. The provider answers for the model —
//! it watches the directory, keeps the layout, and knows where every icon sits —
//! and a renderer draws it. `uflinux-desktop` is the renderer that is supposed to
//! claim the interface and paint the grid in the layer it already owns. When that
//! shell is not running, or is running without the feature, the provider answers
//! `Status` with `renderer: false` and the desktop goes blank: the files are
//! there, the layout is there, and no window shows them.
//!
//! This module is the answer to that. It opens one layer-shell window per
//! monitor at the background layer, reports a grid for each, draws the model the
//! provider hands back, and forwards the handful of things an icon needs to do:
//! launch, place, a menu, a thumbnail. It is a client of the same bus a real
//! renderer uses and adds nothing of its own — no layout of its own, no idea what
//! a desktop directory is, no second copy of the model. Whatever it does not have
//! to know, it does not.
//!
//! The fallback claims the renderer name after a short startup grace period.
//! The resident shell can take the name over when it starts.
//!
//! # Cost, and why the shape is what it is
//!
//! The fallback runs where a full shell does not, so it is built to stay small:
//! one window per monitor, one widget per icon, and nothing per monitor beyond
//! the geometry. Redraws reuse the widgets rather than rebuilding them, which is
//! what keeps a rename from throwing away a decoded picture and asking for it
//! again. Thumbnails are the provider's decision rather than ours: the model says
//! `thumbnails: false` on a machine too small for them, and we never ask for one.
//!
//! # What is deliberately missing
//!
//! A marquee selection, keyboard navigation, a drag image, a drag from one
//! monitor to another, and a live view of the directory behind the grid. Each is
//! a feature of a renderer rather than of a fallback, and the keyboard is out for
//! a reason the compositor decides: a background layer surface is given no focus,
//! so Enter on an icon has nowhere to arrive. What is here is the set that makes
//! the desktop not blank.

// The prose in this module names the bus, the protocol's method names, and the
// bindings' type names — `GetModel`, `CamelCase`, `GdkEvent` — which
// `doc_markdown` reads as unquoted code. Backticking all of it would make the
// comments harder to read, not easier.
#![allow(clippy::doc_markdown)]

use std::cell::{Cell as StdCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use glib::variant::ToVariant;
// Brings gio's traits with it, through gdk and gdk-pixbuf, so the bus calls and
// the list-model helpers need no import of their own.
use gtk::prelude::*;
use gtk::{gio, glib};
// The crate is a dependency of this one and not a re-export of gtk4, so it is
// named directly. Renamed because the module is `ui::desktop` and `use
// gtk4_layer_shell as desktop` would collide with it.
use gtk4_layer_shell as layer_shell;
// The trait comes separately from the module: the bindings hang every setter off
// the window rather than passing the window to a free function, so `set_layer` on
// a window is the trait's method and not the module's.
use gtk4_layer_shell::LayerShell;
use ufm_core::config::DesktopSlot;
use ufm_desktop_ipc as ipc;

use super::css;

/// The application id for the desktop.
///
/// Deliberately not [`super::APP_ID`]. `GApplication` is single-instance per id,
/// so a desktop started with the window's id would hand its activation to whatever
/// window was already open and exit — the file manager would appear to start and
/// draw nothing. A separate id also means a desktop and a window are two processes
/// that can be restarted separately, which is what a session starter wants.
const DESKTOP_ID: &str = "org.uflinux.FileManager.Desktop";

/// How long we wait for a real renderer to claim the bus before committing to
/// a desktop of our own.
///
/// A timeout and not a poll, which is the difference between a fallback and a
/// race. The provider starts on the bus's demand, so the first `Status` usually
/// answers immediately; what the wait is really for is a shell that is still
/// coming up. Give it two seconds and it wins, and we never paint a desktop we
/// would have to stop painting. Ask again in half a second and we would sometimes
/// steal the desktop from a panel that was half a second away.
const GRACE: Duration = Duration::from_secs(2);

/// How long one bus call may take before we give up on it.
///
/// The provider is a small process on a local bus, so anything slower than this
/// is not slow, it is stuck. Painting the desktop is not worth blocking the frame
/// clock on, and the two calls that wait are both off the draw path.
const CALL: i32 = 2_000;

/// The band the provider clamps its thumbnail edge into, and so the band we
/// reserve room for. Both ends come from someone else's constant: the small one
/// is what the model advertises by default, the large one is what the thumbnailer
/// will render at most.
const THUMB_MIN: u32 = ipc::DEFAULT_THUMBNAIL;
const THUMB_MAX: u32 = ufm_thumb::MAX_SIZE;

/// How much of a cell the label may have before the icon takes the rest.
///
/// Two lines of text under an icon is what a desktop icon looks like, and a cell
/// that cannot fit both is too small to place anything in anyway.
const LABEL_LINES: i32 = 2;
const LABEL_CHARS: i32 = 18;
const LABEL_GAP: i32 = 4;
/// Room for those two lines, measured rather than guessed. It is reserved whether
/// or not a name is long enough to use it, so that placing an icon in a cell never
/// depends on the text it happens to have.
const LABEL_HEIGHT: u32 = 34;
/// A cell's edge beyond the icon's: the button's own padding and a gap, so two
/// neighbouring icons do not read as one label with two pictures.
const PAD: u32 = 24;

/// The window class, and the string a compositor shows when it lists surfaces.
const NAMESPACE: &str = "uflinux-files-desktop";

/// What a widget does when the user acts on it: hand the request to the bus.
type Sender = Rc<dyn Fn(ipc::Request)>;

/// What one monitor contributes: the window, the fixed container the cells live
/// in, and the size to fall back to before the compositor has allocated one.
struct Screen {
    window: gtk::Window,
    grid: gtk::Fixed,
    /// The number this monitor was reported under: its position in the list GDK
    /// gave us when the window was built, which is also the number the provider
    /// uses to name it.
    monitor: u8,
    width: i32,
    height: i32,
}

impl Screen {
    /// The grid this screen offers at `edge`, sized to what it actually has.
    ///
    /// The size the compositor gave us and the size of the monitor differ by
    /// exactly the exclusive zone of a panel, which is why the allocation wins
    /// when it exists. A grid sized to the monitor and drawn inside a smaller
    /// surface has a last row under the panel, and an icon the user placed there
    /// is an icon they cannot see or reach.
    fn geometry(&self, edge: u32) -> ipc::Geometry {
        let width = self.grid.width();
        let height = self.grid.height();
        let (width, height) = if width > 0 && height > 0 {
            (width, height)
        } else {
            (self.width, self.height)
        };
        grid(self.monitor, width, height, edge)
    }
}

/// One icon on screen.
///
/// The pieces are kept apart because a redraw has to change the text and the
/// position without touching the picture, which is the expensive part and the one
/// a rename should not invalidate.
#[derive(Clone)]
struct Painted {
    button: gtk::Button,
    image: gtk::Image,
    label: gtk::Label,
    /// Which screen's grid this widget is in, so a move to another monitor can
    /// take it out of the one it was in. `usize::MAX` before the first placement.
    ///
    /// An index and not a monitor number, because a rebuild that changes the number
    /// of screens throws away every widget with it: there is no window a widget can
    /// outlive here, and so no index that can go stale under it.
    screen: Rc<StdCell<usize>>,
}

/// The fallback's view of the desktop: the last model we were given, plus the
/// widgets drawn from it.
struct View {
    model: Option<ipc::Model>,
    screens: Vec<Screen>,
    /// The grids we told the provider about, in screen order. Shared with every
    /// drag gesture, which is what lets a drop be turned into a cell after the
    /// grid has been reported again underneath it. A `RefCell` and not a `Cell`
    /// because a vector of grids is not `Copy`, and copying one per drag would be
    /// an allocation on the pointer path.
    grids: Rc<RefCell<Vec<ipc::Geometry>>>,
    cells: HashMap<String, Painted>,
    /// The newest generation we have drawn. `Changed` carries a generation
    /// precisely so that two changes can collapse into one repaint, which only
    /// works if we can tell a repaint we already did from one we owe.
    drawn: u64,
    /// Whether the model said thumbnails are a thing here at all.
    thumbnails: bool,
    /// The edge our current grids were built for, clamped once from the model
    /// rather than per cell.
    edge: u32,
    /// The icons we have already asked a picture for at this generation. Cleared
    /// when a new model arrives, because that is the point at which a picture may
    /// have changed, and when the windows are rebuilt, because the widgets that
    /// held the old ones are gone.
    asked: HashSet<String>,
}

impl View {
    fn new() -> Self {
        View {
            model: None,
            screens: Vec::new(),
            grids: Rc::new(RefCell::new(Vec::new())),
            cells: HashMap::new(),
            drawn: 0,
            thumbnails: false,
            edge: THUMB_MIN,
            asked: HashSet::new(),
        }
    }

    /// Put one widget in the grid for `index`, moving it out of whatever grid it
    /// was in before.
    ///
    /// The screen a widget was on is looked up rather than trusted, and its
    /// absence is not an error: a redraw can reach a widget whose screen went away
    /// while the model was on the bus, and the right answer then is to draw it on
    /// its new screen and let the old window go with it.
    fn place(&self, painted: &Painted, index: usize, x: f64, y: f64) {
        let from = painted.screen.get();
        if from != usize::MAX
            && from != index
            && let Some(previous) = self.screens.get(from)
        {
            previous.grid.remove(&painted.button);
        }
        let Some(screen) = self.screens.get(index) else {
            return;
        };
        if from == index {
            screen.grid.move_(&painted.button, x, y);
        } else {
            screen.grid.put(&painted.button, x, y);
        }
        painted.screen.set(index);
    }

    /// Take a widget out of the grid it is in, for an item that is no longer on
    /// the desktop at all.
    fn unplace(&self, painted: &Painted) {
        if let Some(screen) = self.screens.get(painted.screen.get()) {
            screen.grid.remove(&painted.button);
        }
    }
}

/// Everything the drawing knows. One of these per process, shared by the monitor
/// handler, the `Changed` handler, and every gesture on every icon.
struct Inner {
    app: gtk::Application,
    connection: gio::DBusConnection,
    view: RefCell<View>,
    /// A `GetModel` is on the bus. One at a time, because two in flight can come
    /// back in the wrong order and paint an older desktop over a newer one.
    fetching: StdCell<bool>,
    /// A `Changed` arrived while one was in flight, so another is owed as soon as
    /// this one lands.
    stale: StdCell<bool>,
    /// We have stopped drawing, so a signal that arrives on the way out is noise.
    dead: StdCell<bool>,
    claimed: StdCell<bool>,
    owner_signal: RefCell<Option<gio::SignalSubscription>>,
    /// Subscriptions live here. Dropping one unsubscribes, which would leave a
    /// desktop that never notices a file landing on it.
    signals: RefCell<Vec<gio::SignalSubscription>>,
}

/// Draw the desktop until the session ends.
#[must_use]
pub fn run() -> ExitCode {
    // Before GTK opens a display, which is the only context in which writing the
    // environment is sound. The same argument the window makes: a machine with no
    // render node cannot afford Mesa's software pipeline, and a desktop is the
    // worst possible thing to paint with it. See [`super::pick_renderer`].
    super::pick_renderer();
    let app = gtk::Application::builder()
        .application_id(DESKTOP_ID)
        .build();
    app.connect_activate(start);
    // The process is kept alive by the hold rather than by a window. A desktop
    // with no monitor to draw on is still a desktop waiting for one, and a window
    // count of zero would read as a session that is over.
    let hold = app.hold();
    // The program name and nothing else: `GApplication` parses what it is handed,
    // and `--desktop-icons` is our option rather than its option to know about.
    let argv = vec![
        std::env::args()
            .next()
            .unwrap_or_else(|| String::from("uflinux-files")),
    ];
    let code = app.run_with_args(&argv);
    drop(hold);
    ExitCode::from(code.get())
}

/// The first activation: find out whether anyone else is drawing this, and if
/// not, start.
fn start(app: &gtk::Application) {
    static ASKED: std::sync::Once = std::sync::Once::new();
    ASKED.call_once(|| {
        css::install();
        if !layer_shell::is_supported() {
            // Without the protocol every window is an ordinary one, and an
            // ordinary full-screen window is a rectangle that hides the desktop
            // rather than a part of it. Declining is the honest answer.
            eprintln!(
                "uflinux-files: this compositor has no layer-shell protocol, so it cannot \
                 show a desktop"
            );
            app.quit();
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else {
            eprintln!("uflinux-files: the desktop has no display to draw on");
            app.quit();
            return;
        };
        // The bus is asked for synchronously. It is up before the session starter
        // runs anything, and if it is not, saying so and leaving beats a desktop
        // that retries forever without ever explaining what it is waiting for.
        let connection = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
            Ok(connection) => connection,
            Err(e) => {
                eprintln!("uflinux-files: no session bus for the desktop: {e}");
                app.quit();
                return;
            }
        };
        let inner = Rc::new(Inner {
            app: app.clone(),
            connection,
            view: RefCell::new(View::new()),
            fetching: StdCell::new(false),
            stale: StdCell::new(false),
            dead: StdCell::new(false),
            claimed: StdCell::new(false),
            owner_signal: RefCell::new(None),
            signals: RefCell::new(Vec::new()),
        });
        // A screen plugged in after login has to be told about: the provider
        // cannot place an icon on a grid it has never seen, and an icon on a
        // screen that went away has to be moved rather than hidden. Gdk has no
        // `monitors-changed` signal to connect to at this version; the list model
        // behind it is what reports the change.
        display.monitors().connect_items_changed({
            let later = Rc::clone(&inner);
            move |_, _, _, _| {
                let later = Rc::clone(&later);
                // Idle rather than here: the list is mid-change, and a new monitor
                // has no geometry worth reporting until the compositor is done
                // with it. Local and not the plain form because `Inner` is `Rc`,
                // which is the whole point of a single-threaded UI.
                glib::idle_add_local_once(move || {
                    take_over(&later);
                });
            }
        });
        ask(&inner);
    });
}

/// Ask the provider who is rendering this desktop, and take it over if the answer
/// is nobody.
fn ask(inner: &Rc<Inner>) {
    let Some(body) = body_of(&ipc::Request::Status) else {
        return;
    };
    let text = match call(inner, "Status", &body) {
        Ok(text) => text,
        Err(e) => {
            // No answer at all means no provider, and a fallback with nothing to
            // fall back to should say so and leave. Retrying would be a poll, and
            // the bus activates the service on the first call anyway, so a refusal
            // here is a real answer.
            eprintln!("uflinux-files: the desktop provider did not answer ({e}); not drawing");
            quit(inner);
            return;
        }
    };
    // An answer that is not a status is a provider from some other version of this
    // protocol, and guessing at a desktop from one is how an icon comes to be drawn
    // twice.
    let Ok(ipc::Reply::Status(status)) = ipc::Reply::decode(text.as_bytes()) else {
        eprintln!("uflinux-files: the desktop provider answered something unreadable");
        quit(inner);
        return;
    };
    if status.version != ipc::PROTOCOL_VERSION {
        quit(inner);
        return;
    }
    claim_after_grace(inner);
}

/// Give the resident shell a chance to start before claiming the background.
fn claim_after_grace(inner: &Rc<Inner>) {
    let inner = Rc::clone(inner);
    glib::timeout_add_local_once(GRACE, move || {
        let dbus = "org.freedesktop.DBus";
        let path = "/org/freedesktop/DBus";
        let name = ipc::RENDERER_NAME;
        let weak = Rc::downgrade(&inner);
        let lost = inner.connection.subscribe_to_signal(
            Some(dbus),
            Some(dbus),
            Some("NameOwnerChanged"),
            Some(path),
            Some(name),
            gio::DBusSignalFlags::NONE,
            move |signal| {
                let Some(inner) = weak.upgrade() else { return };
                let Some((_, previous, _)) = signal.parameters.get::<(String, String, String)>()
                else {
                    return;
                };
                if inner.claimed.get()
                    && inner.connection.unique_name().as_deref() == Some(&previous)
                {
                    quit(&inner);
                }
            },
        );
        inner.owner_signal.replace(Some(lost));
        // ALLOW_REPLACEMENT | DO_NOT_QUEUE. A running shell always wins.
        let result = inner
            .connection
            .call_sync(
                Some(dbus),
                path,
                dbus,
                "RequestName",
                Some(&(name, 5u32).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                CALL,
                gio::Cancellable::NONE,
            )
            .ok()
            .and_then(|reply| reply.get::<(u32,)>());
        if !matches!(result, Some((1 | 4,))) {
            quit(&inner);
            return;
        }
        inner.claimed.set(true);
        take_over(&inner);
    });
}

/// The grace period is over and nobody claimed the renderer: build the windows,
/// report the grids, draw what the provider knows. `false` when there was nothing
/// to draw on.
///
/// Idempotent, which is what lets the monitor handler call it again. Subscribing
/// twice is a no-op, rebuilding the windows is skipped when the count has not
/// changed, and reporting is always worth doing.
fn take_over(inner: &Rc<Inner>) -> bool {
    if !inner.claimed.get() || inner.dead.get() || !build(inner) {
        return false;
    }
    subscribe(inner);
    report(inner);
    true
}

/// One background window per monitor, each holding a `gtk::Fixed`. `false` when
/// there is no monitor to put one on.
///
/// The windows are undecorated, cannot be focused, and have every anchor on, which
/// is how a desktop is spelled in layer-shell: they cover a monitor, the
/// compositor shrinks them by whatever a panel reserves, and they cannot take
/// focus away from a real window.
fn build(inner: &Rc<Inner>) -> bool {
    let Some(display) = gtk::gdk::Display::default() else {
        return false;
    };
    // A snapshot rather than an iterator: the borrow an iterator holds is on the
    // list model itself, and a monitor that appears mid-list while we are walking
    // it makes that borrow panic.
    let monitors: Vec<gtk::gdk::Monitor> = display
        .monitors()
        .snapshot()
        .into_iter()
        .filter_map(|item| item.downcast::<gtk::gdk::Monitor>().ok())
        .collect();
    {
        let view = inner.view.borrow();
        // The same number of screens is not a change worth rebuilding for: a new
        // resolution arrives here too, and throwing away every window to find out
        // the grid is a different size is a lot of pictures to destroy. `report`
        // measures what we have instead.
        if !view.screens.is_empty() && view.screens.len() == monitors.len() {
            return true;
        }
    }
    let mut view = inner.view.borrow_mut();
    for screen in view.screens.drain(..) {
        screen.window.destroy();
    }
    // The widgets went with the windows, so every picture has to be asked for
    // again and every cell built from scratch.
    view.cells.clear();
    view.asked.clear();
    for (index, monitor) in monitors
        .iter()
        .enumerate()
        .take(ipc::MAX_MONITORS)
        .collect::<Vec<_>>()
    {
        // A gdk::Rectangle, which is integers and not the f32 graphene one: the
        // grid we report is in whole pixels, and so is the surface the compositor
        // gives us back.
        let geometry = monitor.geometry();
        let window = gtk::Window::builder()
            .application(&inner.app)
            .title(NAMESPACE)
            .decorated(false)
            .build();
        window.set_can_focus(false);
        window.set_default_size(geometry.width(), geometry.height());
        // On the window and not on the container: the rule that matters is the one
        // that takes the theme's opaque background away, and that background is on
        // the window node. See the comment on `.ufm-desktop` in [`super::css`].
        window.add_css_class("ufm-desktop");
        let grid = gtk::Fixed::new();
        window.set_child(Some(&grid));
        // The layer-shell setters are methods of one trait rather than free
        // functions, so they read like the widget's own properties — which is what
        // they are, as far as GTK is concerned.
        window.init_layer_shell();
        window.set_monitor(Some(monitor));
        window.set_layer(layer_shell::Layer::Background);
        for edge in [
            layer_shell::Edge::Top,
            layer_shell::Edge::Bottom,
            layer_shell::Edge::Left,
            layer_shell::Edge::Right,
        ] {
            window.set_anchor(edge, true);
        }
        // No keyboard, ever. A desktop that can be focused is a desktop that can
        // steal it, and a background surface is not going to be given it anyway.
        window.set_keyboard_mode(layer_shell::KeyboardMode::None);
        window.set_namespace(Some(NAMESPACE));
        view.screens.push(Screen {
            window,
            grid,
            monitor: index as u8,
            width: geometry.width().max(1),
            height: geometry.height().max(1),
        });
    }
    for screen in &view.screens {
        let window = screen.window.clone();
        // The size we were given is the size the grid has, and it is only known
        // once the compositor has placed us. So the map is the moment at which the
        // grids get reported properly and the panel's exclusive zone stops being a
        // guess.
        let inner = Rc::clone(inner);
        window.connect_map(move |_| {
            report(&inner);
        });
    }
    let windows = view
        .screens
        .iter()
        .map(|screen| screen.window.clone())
        .collect::<Vec<_>>();
    // Before showing: mapping runs `report`, which wants the view to itself.
    drop(view);
    for window in windows {
        window.set_visible(true);
    }
    !inner.view.borrow().screens.is_empty()
}

/// Tell the provider about every screen we have, then pull the model.
///
/// Geometry is reported as one complete set, which is what the provider requires:
/// a partial set would leave a monitor with a grid nobody owns. A refusal is
/// printed and survived rather than fatal — the next monitor change reports again,
/// and a desktop that quit because one report was refused would have taken the
/// panel's word for it.
fn report(inner: &Rc<Inner>) {
    let grids = {
        let view = inner.view.borrow();
        if view.screens.len() > ipc::MAX_MONITORS {
            eprintln!(
                "uflinux-files: {} screens, the desktop protocol takes {}; the rest stay empty",
                view.screens.len(),
                ipc::MAX_MONITORS
            );
        }
        view.screens
            .iter()
            .take(ipc::MAX_MONITORS)
            .map(|screen| screen.geometry(view.edge))
            .collect::<Vec<_>>()
    };
    if grids.is_empty() {
        eprintln!("uflinux-files: no monitor to draw a desktop on");
        return;
    }
    let Some(body) = body_of(&ipc::Request::SetGeometry {
        monitors: grids.clone(),
    }) else {
        return;
    };
    if let Err(e) = call(inner, "SetGeometry", &body) {
        eprintln!("uflinux-files: the desktop provider refused our grids: {e}");
        return;
    }
    // What we just said is what the drawing lays out against, so the two cannot
    // disagree even when a refusal or a resize lands between them.
    *inner.view.borrow().grids.borrow_mut() = grids;
    fetch(inner);
}

/// Pull the model, unless one is already on the bus.
fn fetch(inner: &Rc<Inner>) {
    if inner.fetching.replace(true) {
        inner.stale.set(true);
        return;
    }
    let Some(body) = body_of(&ipc::Request::GetModel) else {
        inner.fetching.set(false);
        return;
    };
    inner.connection.call(
        Some(ipc::SESSION_NAME),
        ipc::OBJECT_PATH,
        ipc::SESSION_NAME,
        "GetModel",
        Some(&(body,).to_variant()),
        Some(glib::VariantTy::TUPLE),
        gio::DBusCallFlags::NONE,
        CALL,
        gio::Cancellable::NONE,
        {
            let inner = Rc::clone(inner);
            move |result| {
                let stale = inner.stale.replace(false);
                inner.fetching.set(false);
                let reshaped = match result {
                    Ok(reply) => apply(&inner, &reply),
                    Err(_) => false,
                };
                if reshaped {
                    // The cell size follows the picture size, so a model that wants
                    // a different edge wants new grids reported. This does not
                    // recurse into itself: the call below is asynchronous, and the
                    // edge is already stored, so the reply it brings back reports no
                    // change and the loop ends after one extra round trip.
                    report(&inner);
                } else {
                    redraw(&inner);
                }
                if stale {
                    fetch(&inner);
                }
            }
        },
    );
}

/// Take a model off the bus. `true` when it wants a different cell size than the
/// one the grids we reported were built for.
fn apply(inner: &Rc<Inner>, reply: &glib::Variant) -> bool {
    let Some((text,)) = reply.get::<(String,)>() else {
        return false;
    };
    let Ok(ipc::Reply::Model { model }) = ipc::Reply::decode(text.as_bytes()) else {
        return false;
    };
    let edge = edge_for(&model);
    let mut view = inner.view.borrow_mut();
    let reshaped = edge != view.edge;
    view.edge = edge;
    view.thumbnails = model.thumbnails;
    view.drawn = model.generation;
    view.asked.clear();
    view.model = Some(model);
    reshaped
}

/// The edge to ask for: what the model says, inside the band the provider will
/// decode at. A model that names nothing gets the default rather than zero.
fn edge_for(model: &ipc::Model) -> u32 {
    model.thumbnail_size.clamp(THUMB_MIN, THUMB_MAX)
}

/// Redraw the desktop from the model we hold.
///
/// Every icon is accounted for, but the widgets are reused rather than rebuilt:
/// the model is a flat list with a cell per item, so the work is the same, and
/// reuse is the version where a rename does not throw away a decoded picture.
fn redraw(inner: &Rc<Inner>) {
    // Out of the view rather than borrowed out of it: the loop below both reads the
    // items and writes the sets the widgets are kept in, and a model that is a
    // local value makes that an ordinary sequence instead of a borrow argument.
    let Some(model) = inner.view.borrow_mut().model.take() else {
        return;
    };
    let send: Sender = {
        let inner = Rc::clone(inner);
        Rc::new(move |request| act(&inner, request))
    };
    let mut pictures: Vec<(gtk::Image, String)> = Vec::new();
    let edge;
    {
        let mut borrowed = inner.view.borrow_mut();
        let view = &mut *borrowed;
        edge = view.edge;
        let grids = view.grids.borrow().clone();
        let reserve = grid_reserve(edge).0;
        let old = std::mem::take(&mut view.cells);
        let mut next: HashMap<String, Painted> = HashMap::with_capacity(model.items.len());
        for item in &model.items {
            // An item with no cell is real and simply has nowhere to be drawn; an
            // item whose cell names a screen we have no window for is the same
            // problem one step later, and the provider will move it when it can.
            // A cell is `Copy`, so the option can be taken by value out of the item.
            let Some((index, x, y)) = item.at.and_then(|cell| spot(&grids, cell)) else {
                continue;
            };
            let painted = match old.get(&item.id) {
                Some(painted) => painted.clone(),
                None => cell_widget(
                    item,
                    i32::try_from(edge).unwrap_or(i32::MAX),
                    reserve,
                    Rc::clone(&view.grids),
                    Rc::clone(&send),
                ),
            };
            painted.label.set_text(item.label());
            painted.button.set_tooltip_text(item.tooltip.as_deref());
            view.place(&painted, index, x, y);
            if view.thumbnails
                && !view.asked.contains(&item.id)
                && let Some(path) = model.path_in(&item.id)
            {
                view.asked.insert(item.id.clone());
                pictures.push((painted.image.clone(), path.display().to_string()));
            }
            next.insert(item.id.clone(), painted);
        }
        for (id, painted) in &old {
            if !next.contains_key(id) {
                view.unplace(painted);
            }
        }
        view.cells = next;
        view.model = Some(model);
    }
    for (image, path) in pictures {
        thumbnail(inner, &image, path, edge);
    }
}

/// The widget for one icon: a picture, a name, and the gestures that act on it.
///
/// Split out from [`redraw`] so a test can build one with nothing on the bus. The
/// `send` closure is the only way out, which is what keeps a widget from needing
/// to know that a bus exists.
fn cell_widget(
    item: &ipc::Item,
    edge: i32,
    reserve: u32,
    grids: Rc<RefCell<Vec<ipc::Geometry>>>,
    send: Sender,
) -> Painted {
    let image = gtk::Image::from_icon_name(item.icon_name());
    // The pixel size rather than a size request: an icon name has to be rendered at
    // the cell's size and not at the theme default, and `set_pixel_size` is what
    // asks for that without a scale factor in the way.
    image.set_pixel_size(edge);
    image.set_halign(gtk::Align::Center);
    let label = gtk::Label::new(Some(item.label()));
    label.set_wrap(true);
    label.set_lines(LABEL_LINES);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_justify(gtk::Justification::Center);
    label.set_max_width_chars(LABEL_CHARS);
    label.add_css_class("ufm-name");
    let tile = gtk::Box::new(gtk::Orientation::Vertical, LABEL_GAP);
    tile.set_halign(gtk::Align::Center);
    tile.append(&image);
    tile.append(&label);
    let button = gtk::Button::new();
    button.set_child(Some(&tile));
    button.set_has_frame(false);
    button.set_can_focus(false);
    button.set_tooltip_text(item.tooltip.as_deref());
    button.add_css_class("ufm-cell");
    // The cell's width and not its height: the grid positions the button and the
    // label decides how tall it is, and asking for a height too would clip a name
    // that needs both of its lines.
    button.set_size_request(i32::try_from(reserve).unwrap_or(i32::MAX), 0);
    let screen: Rc<StdCell<usize>> = Rc::new(StdCell::new(usize::MAX));

    // Double-click, not single: a desktop icon that opens on a click cannot be
    // selected, and there is no `GestureMultiPress` at this version of the
    // bindings to get it any other way.
    let click = gtk::GestureClick::new();
    {
        let send = Rc::clone(&send);
        let id = item.id.clone();
        click.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                send(ipc::Request::Launch { id: id.clone() });
            }
        });
    }
    let menu = gtk::GestureClick::new();
    menu.set_button(3);
    {
        let send = Rc::clone(&send);
        let id = item.id.clone();
        menu.connect_pressed(move |_, n_press, _, _| {
            if n_press == 1 {
                send(ipc::Request::ContextMenu { id: id.clone() });
            }
        });
    }
    let drag = gtk::GestureDrag::new();
    let origin: Rc<StdCell<(f64, f64)>> = Rc::new(StdCell::new((0.0, 0.0)));
    {
        let origin = Rc::clone(&origin);
        drag.connect_drag_begin(move |_, x, y| origin.set((x, y)));
    }
    {
        let send = Rc::clone(&send);
        let id = item.id.clone();
        let screen = Rc::clone(&screen);
        drag.connect_drag_end(move |gesture, dx, dy| {
            let Some(button) = gesture
                .widget()
                .and_then(|widget| widget.downcast::<gtk::Button>().ok())
            else {
                return;
            };
            let Some(grid) = button
                .parent()
                .and_then(|parent| parent.downcast::<gtk::Fixed>().ok())
            else {
                return;
            };
            // The gesture's coordinates are the button's own, and `GdkEvent` has no
            // root coordinates at this version, so the drop point is built from
            // where the button sits in the grid. That is also why a drag cannot
            // cross a monitor: there is nothing here to say which one the pointer
            // is over once it has left.
            let (x, y) = grid.child_position(&button);
            let (start_x, start_y) = origin.get();
            let Some(geometry) = grids.borrow().get(screen.get()).copied() else {
                return;
            };
            let Some(slot) = slot_of(&geometry, x + start_x + dx, y + start_y + dy) else {
                return;
            };
            send(ipc::Request::Place {
                id: id.clone(),
                monitor: geometry.monitor,
                col: slot.col,
                row: slot.row,
            });
        });
    }
    button.add_controller(click);
    button.add_controller(menu);
    button.add_controller(drag);
    Painted {
        button,
        image,
        label,
        screen,
    }
}

/// Ask for one picture, and paint it over the icon name when it arrives.
///
/// Asked for once per icon per generation, and only while the model says pictures
/// are wanted here at all. The fallback has no scrolling, so every cell it has
/// drawn is a visible one — the saving is not in skipping icons below the fold, it
/// is in never asking for a picture the model said not to ask for.
fn thumbnail(inner: &Rc<Inner>, image: &gtk::Image, path: String, size: u32) {
    let Some(body) = body_of(&ipc::Request::RequestThumbnail { path, size }) else {
        return;
    };
    inner.connection.call_with_unix_fd_list(
        Some(ipc::SESSION_NAME),
        ipc::OBJECT_PATH,
        ipc::SESSION_NAME,
        "RequestThumbnail",
        Some(&(body,).to_variant()),
        Some(glib::VariantTy::TUPLE),
        gio::DBusCallFlags::NONE,
        CALL,
        None::<&gio::UnixFDList>,
        gio::Cancellable::NONE,
        {
            let image = image.clone();
            move |result| {
                let Ok((reply, descriptors)) = result else {
                    return;
                };
                let Some((text,)) = reply.get::<(String,)>() else {
                    return;
                };
                let Ok(ipc::Reply::Thumbnail(picture)) = ipc::Reply::decode(text.as_bytes()) else {
                    return;
                };
                // No descriptor is a real answer and not a failure: the provider
                // sends an icon instead of a picture for anything it will not
                // decode, and the icon we already painted is the right thing to
                // leave showing.
                let (Some(index), Some(list)) = (picture.fd, descriptors) else {
                    return;
                };
                let Ok(descriptor) = list.get(i32::from(index)) else {
                    return;
                };
                let stream = gio::UnixInputStream::take_fd(descriptor);
                // `0, 0` rather than the edge we asked for: the provider already
                // decoded at the size we requested, and scaling it again here would
                // only blur a picture that is already right.
                let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(
                    &stream,
                    0,
                    0,
                    false,
                    None::<&gio::Cancellable>,
                ) else {
                    return;
                };
                image.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&pixbuf)));
            }
        },
    );
}

/// Start listening for the provider's news, once.
///
/// Only `Changed`. `Progress` is real and this client has nowhere to put it: a grid
/// of icons on a wallpaper does not get a spinner, and subscribing to a signal in
/// order to drop it is a subscription that lies about what the client does.
fn subscribe(inner: &Rc<Inner>) {
    if !inner.signals.borrow().is_empty() {
        return;
    }
    let subscription = inner.connection.subscribe_to_signal(
        Some(ipc::SESSION_NAME),
        Some(ipc::SESSION_NAME),
        Some(ipc::CHANGED),
        Some(ipc::OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        {
            let inner = Rc::clone(inner);
            move |signal| on_changed(&inner, &signal)
        },
    );
    inner.signals.borrow_mut().push(subscription);
}

/// A `Changed` is a hint to fetch, not a model to apply. The generation is the
/// interesting part: two changes before we finish one repaint are one repaint.
fn on_changed(inner: &Rc<Inner>, signal: &gio::DBusSignalRef) {
    if inner.dead.get() {
        return;
    }
    // The body is parsed rather than ignored: a signal this client cannot read
    // should not cause work.
    let Some(changed) = signal_body::<ipc::Changed>(signal) else {
        return;
    };
    // Older than what is drawn is a reply that lost the race, and dropping it is
    // the whole reason the generation travels with the signal. Equal is not
    // dropped: the model may have been refetched since, and repainting from the
    // model we hold costs the widgets we already have.
    if changed.generation < inner.view.borrow().drawn {
        return;
    }
    fetch(inner);
}

/// The JSON body of a signal, or `None` when it is not one this client knows.
fn signal_body<T>(signal: &gio::DBusSignalRef) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    let (text,) = signal.parameters.get::<(String,)>()?;
    serde_json::from_str(&text).ok()
}

/// One request, fired and forgotten. The desktop is not a place where a failed call
/// has anywhere to be reported beyond the terminal it was started from.
fn act(inner: &Rc<Inner>, request: ipc::Request) {
    let method = bus_method(&request);
    let Some(body) = body_of(&request) else {
        return;
    };
    inner.connection.call(
        Some(ipc::SESSION_NAME),
        ipc::OBJECT_PATH,
        ipc::SESSION_NAME,
        method,
        Some(&(body,).to_variant()),
        Some(glib::VariantTy::TUPLE),
        gio::DBusCallFlags::NONE,
        CALL,
        gio::Cancellable::NONE,
        |result| {
            if let Err(e) = result {
                eprintln!("uflinux-files: the desktop could not finish a request: {e}");
            }
        },
    );
}

/// A request as the one string this protocol sends, or `None` when it is too large
/// to send at all.
fn body_of(request: &ipc::Request) -> Option<String> {
    let body = serde_json::to_string(request).ok()?;
    (body.len() <= ipc::MAX_REQUEST).then_some(body)
}

/// One request, and wait for the answer. `Err` carries the bus's own message, which
/// is the part worth printing; an answer that is not one string is the provider's
/// fault and says so without pretending otherwise.
fn call(inner: &Rc<Inner>, method: &str, body: &str) -> Result<String, String> {
    let reply = inner
        .connection
        .call_sync(
            Some(ipc::SESSION_NAME),
            ipc::OBJECT_PATH,
            ipc::SESSION_NAME,
            method,
            Some(&(body,).to_variant()),
            Some(glib::VariantTy::TUPLE),
            gio::DBusCallFlags::NONE,
            CALL,
            gio::Cancellable::NONE,
        )
        .map_err(|e| e.to_string())?;
    reply
        .get::<(String,)>()
        .map(|(text,)| text)
        .ok_or_else(|| String::from("the answer is not one this program can read"))
}

/// The bus name for a request. The wire spells methods in kebab-case and D-Bus
/// wants CamelCase, so this cannot be derived — and it is the same mapping the
/// provider checks the other way round, which is what a test below takes advantage
/// of.
fn bus_method(request: &ipc::Request) -> &'static str {
    match request {
        ipc::Request::Status => "Status",
        ipc::Request::GetModel => "GetModel",
        ipc::Request::SetGeometry { .. } => "SetGeometry",
        ipc::Request::Place { .. } => "Place",
        ipc::Request::Launch { .. } => "Launch",
        ipc::Request::Reveal { .. } => "Reveal",
        ipc::Request::ContextMenu { .. } => "ContextMenu",
        ipc::Request::Trash { .. } => "Trash",
        ipc::Request::RequestThumbnail { .. } => "RequestThumbnail",
    }
}

/// Stop drawing and say goodbye to the bus. The subscriptions go first: a desktop
/// that is leaving should not notice another change on its way out, and dropping
/// them is what makes that true rather than a matter of ordering.
fn quit(inner: &Rc<Inner>) {
    if inner.dead.replace(true) {
        return;
    }
    inner.signals.borrow_mut().clear();
    inner.owner_signal.borrow_mut().take();
    let screens = std::mem::take(&mut inner.view.borrow_mut().screens);
    for screen in screens {
        screen.window.destroy();
    }
    inner.app.quit();
}

/// The grid one screen of `width` by `height` offers at `edge`.
fn grid(monitor: u8, width: i32, height: i32, edge: u32) -> ipc::Geometry {
    let (cell_w, cell_h) = grid_reserve(edge);
    // Each axis is capped by its own limit, and they are not the same number: the
    // protocol allows a far taller grid than a wider one, because a screen is wide
    // and short. Taking the larger of the two for both would have us report a grid
    // the provider then refuses.
    let columns = divide(width, cell_w, ipc::MAX_COLUMNS);
    let rows = divide(height, cell_h, ipc::MAX_ROWS);
    ipc::Geometry::new(monitor, columns, rows, cell_w, cell_h)
}

/// How many cells of `cell` fit in `pixels`, with a floor of one and a ceiling of
/// `cap`, which is the protocol's own for this axis.
///
/// The floor matters more than it looks: the provider refuses a zero-sized grid
/// rather than reading it as an empty one, because an empty grid silently swallows
/// every icon on that screen. A monitor that reports no size yet — one that is
/// still being configured — therefore gets a grid of one cell, and the icons on it
/// come back as soon as it reports a real size.
fn divide(pixels: i32, cell: u32, cap: u16) -> u16 {
    let cells = u32::try_from(pixels.max(0)).unwrap_or(u32::MAX) / cell.max(1);
    cells.clamp(1, u32::from(cap)) as u16
}

/// The cell an icon with an `edge`-pixel picture needs, in pixels: the picture, the
/// two lines under it, and the gap that keeps neighbours apart.
fn grid_reserve(edge: u32) -> (u32, u32) {
    let edge = edge.clamp(THUMB_MIN, THUMB_MAX);
    (edge + PAD, edge + PAD + LABEL_HEIGHT)
}

/// Where an icon goes, in pixels, and in which screen.
///
/// Two separate reasons to have no answer, and the caller treats them the same: the
/// cell names a monitor we have no window for, or the cell is outside the grid we
/// reported for it. An icon with no cell at all never reaches here — the model says
/// so by leaving `at` empty, and such an icon is real and simply not drawn.
fn spot(grids: &[ipc::Geometry], cell: ipc::Cell) -> Option<(usize, f64, f64)> {
    let index = grids.iter().position(|grid| grid.monitor == cell.monitor)?;
    let geometry = grids[index];
    if !geometry.fits(cell.slot()) {
        return None;
    }
    let x = f64::from(u32::from(cell.col)) * f64::from(geometry.cell_w);
    let y = f64::from(u32::from(cell.row)) * f64::from(geometry.cell_h);
    Some((index, x, y))
}

/// Which cell of a grid a point in its own pixels falls in, or `None` when the point
/// is outside it. A drop outside the grid is not a place, and the icon goes back
/// where it was rather than to the nearest edge.
fn slot_of(geometry: &ipc::Geometry, x: f64, y: f64) -> Option<DesktopSlot> {
    let col = (x / f64::from(geometry.cell_w.max(1))).floor();
    let row = (y / f64::from(geometry.cell_h.max(1))).floor();
    // Finite first: `floor` of a NaN is a NaN, which is neither negative nor a
    // number `try_from` would reject, and a pointer that reported no position
    // should not place an icon in the corner of a screen.
    if !col.is_finite() || !row.is_finite() || col < 0.0 || row < 0.0 {
        return None;
    }
    let slot = DesktopSlot {
        col: u16::try_from(col as i64).ok()?,
        row: u16::try_from(row as i64).ok()?,
    };
    geometry.fits(slot).then_some(slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;

    /// An item with a cell, which is the only kind that gets drawn.
    fn placed(kind: ipc::Kind, name: &str, monitor: u8, col: u16, row: u16) -> ipc::Item {
        ipc::Item {
            at: Some(ipc::Cell::new(monitor, DesktopSlot { col, row })),
            ..ipc::Item::new(name, name, kind)
        }
    }

    #[test]
    fn a_grid_is_the_screen_divided_by_the_cell_an_icon_needs() {
        let geometry = grid(0, 1920, 1080, THUMB_MIN);
        let (cell_w, cell_h) = grid_reserve(THUMB_MIN);
        assert_eq!(geometry.cell_w, cell_w);
        assert_eq!(geometry.cell_h, cell_h);
        assert_eq!(
            geometry.columns,
            (1920 / i32::try_from(cell_w).unwrap()) as u16
        );
        assert_eq!(
            geometry.rows,
            (1080 / i32::try_from(cell_h).unwrap()) as u16
        );
        assert!(geometry.usable(), "the provider refuses one that is not");
    }

    #[test]
    fn a_grid_never_has_zero_cells_and_never_passes_the_caps() {
        // Zero would be refused by the provider, and a screen that has not been
        // configured yet is not a screen with no room on it.
        assert_eq!(grid(0, 0, 0, THUMB_MIN).cells(), 1);
        assert_eq!(grid(0, -4000, -4000, THUMB_MIN).cells(), 1);
        let huge = grid(0, i32::MAX, i32::MAX, THUMB_MIN);
        assert!(huge.columns <= ipc::MAX_COLUMNS);
        assert!(huge.rows <= ipc::MAX_ROWS);
        assert!(huge.usable());
    }

    #[test]
    fn the_cell_grows_with_the_picture_the_model_asks_for() {
        let small = grid(0, 1920, 1080, THUMB_MIN);
        let large = grid(0, 1920, 1080, THUMB_MAX);
        assert!(large.cell_w > small.cell_w);
        assert!(large.columns < small.columns, "bigger icons, fewer columns");
    }

    #[test]
    fn the_edge_is_clamped_into_the_band_the_provider_decodes_in() {
        let model = |size| ipc::Model {
            thumbnail_size: size,
            ..ipc::Model::default()
        };
        assert_eq!(edge_for(&model(0)), THUMB_MIN);
        assert_eq!(edge_for(&model(200)), 200);
        assert_eq!(edge_for(&model(4096)), THUMB_MAX);
        assert_eq!(grid_reserve(4096).0, THUMB_MAX + PAD);
    }

    #[test]
    fn an_icon_lands_in_the_grid_its_cell_names_or_nowhere() {
        // The second screen is tall rather than square: a cell is taller than it is
        // wide by the height of a two-line label, and a 500 px square only has room
        // for two rows, which is not enough to name a row at all.
        let grids = vec![grid(0, 1000, 1000, THUMB_MIN), grid(1, 500, 800, THUMB_MIN)];
        let item = placed(ipc::Kind::File, "b.txt", 1, 2, 3);
        let (index, x, y) =
            spot(&grids, item.at.expect("it has a cell")).expect("monitor 1 has a window");
        assert_eq!(index, 1);
        // Compared as whole pixels: a cell's offset is a product of integers, and
        // asserting on it as a float would be testing the representation.
        assert_eq!(x as i64, i64::from(grids[1].cell_w) * 2);
        assert_eq!(y as i64, i64::from(grids[1].cell_h) * 3);
        let gone = placed(ipc::Kind::File, "c.txt", 2, 0, 0);
        assert!(
            spot(&grids, gone.at.unwrap()).is_none(),
            "no window for that monitor"
        );
        let past = placed(ipc::Kind::File, "d.txt", 0, grids[0].columns, 0);
        assert!(
            spot(&grids, past.at.unwrap()).is_none(),
            "one past the last column is not a cell"
        );
    }

    #[test]
    fn a_drop_outside_the_grid_is_not_a_place() {
        let geometry = grid(0, 1000, 1000, THUMB_MIN);
        assert_eq!(
            slot_of(&geometry, 1.0, 1.0),
            Some(DesktopSlot { col: 0, row: 0 })
        );
        assert_eq!(
            slot_of(&geometry, -1.0, 10.0),
            None,
            "a drag that left the top edge"
        );
        assert_eq!(
            slot_of(&geometry, 1e9, 10.0),
            None,
            "a column far past the last"
        );
        assert_eq!(
            slot_of(&geometry, f64::NAN, 10.0),
            None,
            "a pointer that reported no position"
        );
    }

    #[test]
    fn the_grace_period_is_the_one_the_documents_promise() {
        // docs/desktop-integration.md states the figure. A fallback that polls
        // instead of waiting, or that waits long enough to be noticed, is a
        // different design and should have to say so here too.
        assert_eq!(GRACE, Duration::from_secs(2));
    }

    #[test]
    fn the_desktop_is_its_own_application() {
        // Sharing the window's id would make a desktop forward its activation to an
        // open window and exit, which looks exactly like the feature doing nothing
        // at all.
        assert_ne!(DESKTOP_ID, super::super::APP_ID);
    }

    #[test]
    fn every_request_names_the_method_the_bus_answers() {
        // The provider refuses a call whose method does not match the body's own
        // tag, so a mapping that drifts is a desktop that silently stops answering.
        // The wire's kebab-case name and this CamelCase one have to agree for every
        // request there is, which is why the expected names are written out here
        // rather than computed from them.
        let requests = [
            (ipc::Request::Status, "Status"),
            (ipc::Request::GetModel, "GetModel"),
            (
                ipc::Request::SetGeometry {
                    monitors: vec![grid(0, 100, 100, THUMB_MIN)],
                },
                "SetGeometry",
            ),
            (
                ipc::Request::Place {
                    id: "a.txt".into(),
                    monitor: 0,
                    col: 1,
                    row: 2,
                },
                "Place",
            ),
            (ipc::Request::Launch { id: "a.txt".into() }, "Launch"),
            (ipc::Request::Reveal { id: "a.txt".into() }, "Reveal"),
            (
                ipc::Request::ContextMenu { id: "a.txt".into() },
                "ContextMenu",
            ),
            (ipc::Request::Trash { id: "a.txt".into() }, "Trash"),
            (
                ipc::Request::RequestThumbnail {
                    path: "/tmp/Desktop/a.txt".into(),
                    size: THUMB_MIN,
                },
                "RequestThumbnail",
            ),
        ];
        assert_eq!(requests.len(), 9, "a new request needs a method name");
        for (request, method) in &requests {
            assert_eq!(bus_method(request), *method);
            assert_eq!(
                camel(request.kind()),
                *method,
                "{} is sent by the wrong name",
                request.kind()
            );
        }
    }

    /// The CamelCase form of a kebab-case method name.
    fn camel(kind: &str) -> String {
        kind.split('-')
            .map(|part| {
                let mut letters = part.chars();
                match letters.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + letters.as_str(),
                    None => String::new(),
                }
            })
            .collect()
    }

    #[test]
    fn an_icon_is_a_picture_a_name_and_a_button() {
        tk::ui(|| {
            let item = ipc::Item {
                tooltip: Some("a note".into()),
                ..ipc::Item::new("notes.txt", "Notes", ipc::Kind::File)
            };
            let grids: Rc<RefCell<Vec<ipc::Geometry>>> =
                Rc::new(RefCell::new(vec![grid(0, 1000, 1000, THUMB_MIN)]));
            let sent = Rc::new(RefCell::new(Vec::new()));
            let send: Sender = {
                let sent = Rc::clone(&sent);
                Rc::new(move |request| sent.borrow_mut().push(request))
            };
            let painted = cell_widget(
                &item,
                i32::try_from(THUMB_MIN).unwrap(),
                grid_reserve(THUMB_MIN).0,
                grids,
                send,
            );
            assert_eq!(painted.label.text(), "Notes");
            assert!(painted.label.wraps());
            assert_eq!(painted.label.lines(), LABEL_LINES);
            assert_eq!(
                painted.label.ellipsize(),
                gtk::pango::EllipsizeMode::End,
                "a long name has to be cut, not push the grid apart"
            );
            assert!(painted.button.has_css_class("ufm-cell"));
            assert!(
                !painted.button.can_focus(),
                "a surface that cannot be focused should not offer a widget that can"
            );
            assert_eq!(painted.button.tooltip_text().as_deref(), Some("a note"));
            assert_eq!(
                painted.screen.get(),
                usize::MAX,
                "a widget that has not been placed says so"
            );
            assert!(sent.borrow().is_empty(), "building one asks for nothing");
        });
    }

    #[test]
    fn an_icon_with_no_icon_of_its_own_still_shows_something() {
        // The fallback name comes from the model's own rule, so a desktop and a
        // window cannot disagree about what an unknown file looks like.
        for (kind, name) in [
            (ipc::Kind::Directory, "folder"),
            (ipc::Kind::File, "text-x-generic"),
            (ipc::Kind::Launcher, "application-x-executable"),
        ] {
            let item = ipc::Item::new("x", "X", kind);
            assert_eq!(item.icon_name(), name);
        }
    }
}
