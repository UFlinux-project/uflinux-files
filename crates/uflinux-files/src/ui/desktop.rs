//! The desktop we paint ourselves, for the case where nobody else will.
//!
//! The desktop-icons feature is really two halves. The provider answers for the
//! model — it watches the directory, keeps the layout, and knows where every
//! icon sits — and a renderer draws it. `uflinux-desktop` is the renderer that
//! is supposed to claim `org.uflinux.FileManager.Desktop.Renderer` and paint the
//! grid in the layer it already owns. When that shell is not running, or is
//! running without the feature, the provider answers `Status` with
//! `renderer: false` and the desktop goes blank: the files are there, the
//! layout is there, and no window shows them.
//!
//! This module is the answer to that. It opens one layer-shell window per
//! monitor at the background layer, reports a grid for each, draws the model
//! the provider hands back, and forwards the handful of things an icon needs
//! to do: launch, place, reveal, trash, a menu, a thumbnail. It is a client of
//! the same bus a real renderer uses and adds nothing of its own — no layout
//! of its own, no idea what a desktop directory is, no second copy of the
//! model. Whatever it does not have to know, it does not.
//!
//! # Cost, and why the shape is what it is
//!
//! The whole point of the fallback is that it runs where a full shell does
//! not, so it is built to stay small. One window per monitor, one widget per
//! icon, and nothing per-monitor beyond the geometry. Thumbnails are the only
//! part of a desktop that can be expensive, and they are the provider's
//! decision rather than ours: the model says `thumbnails: false` on a machine
//! too small for them and we never ask for one.
//!
//! # What is deliberately missing
//!
//! A marquee selection, keyboard grid navigation, and a live view of the
//! directory behind the grid. Each is a feature of a file manager rather than
//! of a fallback, and the first two are the renderer's job when there is one.
//! What is here is the set that makes the desktop not blank.
//!
//! ## Feature
//!
//! Only compiled with `desktop-icons`, which is the only place
//! `gtk4-layer-shell` is referenced. The provider never links it, so the
//! daemon that is always running does not pay for a windowing dependency only
//! the fallback needs.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use glib::{ControlFlow, Priority, clone};
use gtk::prelude::*;
use gtk::{gio, glib, layer_shell};
use ufm_desktop_ipc as ipc;

/// How long we wait for a real renderer to claim the bus before committing to
/// a desktop of our own.
///
/// A timeout and not a poll, which is the difference between a fallback and a
/// race. The provider starts on the bus's demand, so the first `Status`
/// usually answers immediately; what the wait is really for is a shell that is
/// still coming up. Give it two seconds and it wins, and we never paint a
/// desktop we would have to stop painting. Ask again in half a second and we
/// would sometimes steal the desktop from a panel that was half a second away.
const GRACE: Duration = Duration::from_secs(2);

/// How long one bus call may take before we give up on it.
///
/// The provider is a small process on a local bus, so anything slower than
/// this is not slow, it is stuck. Painting the desktop is not worth blocking
/// the frame clock on.
const CALL: i32 = 2_000;

/// The band the provider clamps its thumbnail edge into, and so the band we
/// reserve room for. Both ends come from someone else's constant: the small
/// one is what the model advertises by default, the large one is what the
/// thumbnailer will render at most.
const THUMB_MIN: u32 = ipc::DEFAULT_THUMBNAIL;
const THUMB_MAX: u32 = ufm_thumb::MAX_SIZE;

/// How much of a cell the label may have before the icon takes the rest.
///
/// Two lines of text under an icon is what a desktop icon looks like, and a
/// cell that cannot fit both is too small to place anything in anyway.
const LABEL_LINES: i32 = 2;
const LABEL_HEIGHT: i32 = 34;

/// The window class, and the string a compositor shows when it lists surfaces.
const NAMESPACE: &str = "uflinux-files-desktop";

/// What one monitor contributes: the window, the fixed container the cells
/// live in, and the grid we told the provider about.
///
/// The grid is kept because a monitor change has to send a replacement for
/// every monitor, not just the one that moved, and because the cell count is
/// what turns a slot into a pixel.
struct Screen {
    window: gtk::Window,
    grid: gtk::Fixed,
    geometry: ipc::Geometry,
}

/// One icon on screen: the widget, the label whose text may arrive later with
/// a rename, and the generation of the thumbnail currently painted.
struct Painted {
    button: gtk::Button,
    label: gtk::Label,
    /// The generation whose thumbnail this is, so a model that moved on
    /// retires the picture instead of leaving a stale one under a new name.
    thumbnail: StdCell<u64>,
}

/// The fallback's view of the desktop: the last model we were given, plus the
/// widgets drawn from it.
struct View {
    model: Option<ipc::Model>,
    screens: Vec<Screen>,
    cells: Vec<(String, Painted)>,
    /// The newest generation we have drawn. `Changed` carries a generation
    /// precisely so that two changes can collapse into one repaint, which only
    /// works if we can tell a repaint we already did from one we owe.
    drawn: u64,
    /// The newest generation we have asked for. A repaint in flight makes any
    /// older answer stale, and out-of-order replies on a local bus are rare
    /// but not impossible.
    requested: u64,
    /// Whether the model said thumbnails are a thing here at all.
    thumbnails: bool,
    /// The edge we ask for, clamped once from the model rather than per cell.
    edge: u32,
}

impl View {
    fn new() -> Self {
        View {
            model: None,
            screens: Vec::new(),
            cells: Vec::new(),
            drawn: 0,
            requested: 0,
            thumbnails: false,
            edge: THUMB_MIN,
        }
    }

    /// Where an icon goes, in pixels, or nowhere.
    ///
    /// Three separate reasons to have no answer, and the caller reports all
    /// three the same way: the model does not know the id, the icon has no
    /// cell, the cell names a monitor we have no window for, or the grid is
    /// too small to hold the row it is in.
    fn spot(&self, cell: &ipc::Cell) -> Option<(usize, i32, i32)> {
        let index = self
            .screens
            .iter()
            .position(|screen| screen.geometry.monitor == cell.monitor)?;
        let geometry = self.screens[index].geometry;
        let x = i32::from(cell.col) * i32::from(geometry.cell_w);
        let y = i32::from(cell.row) * i32::from(geometry.cell_h);
        if x >= geometry.usable() || y >= geometry.rows as i32 * geometry.cell_h {
            return None;
        }
        Some((index, x, y))
    }

    fn take_cells(&mut self) -> Vec<(String, Painted)> {
        std::mem::take(&mut self.cells)
    }
}

/// The fallback, behind the one handle `main` keeps.
pub struct Desktop {
    app: gtk::Application,
}

/// Everything the drawing knows. One of these per process, shared by the
/// monitor handler, the `Changed` handler, and every gesture on every icon.
struct Inner {
    app: gtk::Application,
    connection: gio::DBusConnection,
    view: RefCell<View>,
    /// Subscriptions live here. Dropping one unsubscribes, which would leave a
    /// desktop that never notices a file landing on it.
    _signals: Vec<gio::SignalSubscription>,
}

impl Desktop {
    /// Connect to the session bus and take over the desktop if nobody else
    /// has, and never come back.
    ///
    /// The connect is async on purpose. A bus that is not up yet is a normal
    /// thing during login, and blocking here would hold up the rest of the
    /// session for a service that may not even be wanted.
    pub fn run(app: &gtk::Application) {
        let app = app.clone();
        gio::bus_get(
            gio::BusType::Session,
            gio::Cancellable::NONE,
            clone!(@strong app, move |result| {
                let connection = match result {
                    Ok(gio::DBusConnection::Session(c)) => c,
                    Ok(_) => {
                        eprintln!("uflinux-files: the desktop needs a session bus");
                        app.quit();
                        return;
                    }
                    Err(e) => {
                        eprintln!("uflinux-files: no session bus for the desktop: {e}");
                        app.quit();
                        return;
                    }
                };
                Desktop::attach(&app, &connection);
            }),
        );
        app.run()
    }

    /// Hand the desktop an open connection. Split out so a test can hand it a
    /// connection of its own and watch what it does with it.
    fn attach(app: &gtk::Application, connection: &gio::DBusConnection) {
        let inner = Rc::new(Inner {
            app: app.clone(),
            connection: connection.clone(),
            view: RefCell::new(View::new()),
            _signals: Vec::new(),
        });
        // The monitor list is what makes this a fallback rather than a
        // screensaver: a monitor plugged in after login has to be told about,
        // and the provider cannot place an icon on a grid it has never seen.
        // GDK has no `monitors-changed` signal in this version, so the list
        // model is what reports the change.
        let monitors = gtk::gdk::Display::default().map(|d| d.monitors());
        if let Some(model) = monitors {
            let later = inner.clone();
            model.connect_items_changed(clone!(@strong later, move |_, _, _, _| {
                // A monitor that appeared needs a window and a grid, and a
                // monitor that vanished needs its icons moved, which is the
                // provider's job. Either way the answer is to re-report and
                // redraw; the provider never deletes an icon over a geometry
                // change, so nothing is lost by doing it eagerly.
                glib::idle_add_once(clone!(@strong later, move || {
                    report(&later);
                    redraw(&later, true);
                }));
            }));
        }
        ask(&inner);
    }

    /// Keep the process alive for the desktop. Called by `main` before the
    /// application runs, so a desktop with no windows in it is not read as a
    /// session that is over.
    pub fn hold(&self) -> glib::IdleHandle {
        let source = glib::IdleSource::new(|_| ControlFlow::Continue);
        let handle = source.attach(None);
        let app = self.app.clone();
        handle.set_callback(move |_| {
            let _ = &app;
            ControlFlow::Continue
        });
        handle
    }
}

/// Ask the provider who is rendering this desktop, and take it over if the
/// answer is nobody.
fn ask(inner: &Rc<Inner>) {
    let body = ipc::Request::Status.encode();
    let answer = call(inner, "Status", Some(&(&body,).to_variant()));
    let status = match answer.and_then(|reply| ipc::Reply::decode(&reply).ok()) {
        Some(ipc::Reply::Status(status)) => status,
        _ => {
            // No answer at all means no provider, and a fallback with nothing
            // to fall back to should say so and leave. Retrying would be a
            // poll, and the bus activates the service on the first call
            // anyway, so a refusal here is a real answer.
            eprintln!("uflinux-files: the desktop provider did not answer; not drawing a desktop");
            inner.app.quit();
            return;
        }
    };
    if status.renderer {
        // Someone else paints this. Leaving is the whole point of the flag.
        inner.app.quit();
        return;
    }
    let later = inner.clone();
    glib::timeout_add_local(GRACE, move || {
        take_over(&later);
        ControlFlow::Break
    });
}

/// The grace period is over and nobody claimed the renderer: build the
/// windows, report the grids, draw what the provider knows.
fn take_over(inner: &Rc<Inner>) {
    build(inner);
    if !report(inner) {
        return;
    }
    subscribe(inner);
    redraw(inner, true);
}

/// One background window per monitor, each holding a `gtk::Fixed`.
///
/// The windows are undecorated, non-focusable, and at the background layer
/// with every anchor on, which is how a desktop is spelled in layer-shell:
/// they cover a monitor, they cannot take focus from a real window, and a
/// compositor that shows a desktop shows them.
fn build(inner: &Rc<Inner>) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let mut view = inner.view.borrow_mut();
    for screen in view.screens.drain(..) {
        screen.window.destroy();
    }
    for monitor in display.monitors().iter::<gtk::gdk::Monitor>() {
        let Ok(monitor) = monitor else {
            continue;
        };
        let window = gtk::WindowBuilder::new()
            .application(&inner.app)
            .title("uflinux-files-desktop")
            .decorated(false)
            .can_focus(false)
            .build();
        let grid = gtk::Fixed::new();
        grid.add_css_class("ufm-desktop");
        window.set_child(Some(&grid));
        window.set_default_size(monitor.geometry().width, monitor.geometry().height);
        layer_shell::init_for_window(&window);
        layer_shell::set_monitor(&window, &monitor);
        layer_shell::set_layer(&window, layer_shell::Layer::Background);
        for edge in [
            layer_shell::Edge::Left,
            layer_shell::Edge::Right,
            layer_shell::Edge::Top,
            layer_shell::Edge::Bottom,
        ] {
            layer_shell::anchor(&window, edge, 0);
        }
        // No keyboard focus, ever. A desktop that can be focused is a desktop
        // that steals it, and Enter on an icon is handled by the icon.
        layer_shell::set_keyboard_mode(&window, layer_shell::KeyboardMode::None);
        layer_shell::set_namespace(&window, NAMESPACE);
        view.screens.push(Screen {
            window,
            grid,
            geometry: ipc::Geometry::new(
                view.screens.len() as u8,
                1,
                1,
                grid_reserve(THUMB_MIN).0 as u16,
                grid_reserve(THUMB_MIN).1 as u16,
            ),
        });
    }
    for screen in &view.screens {
        screen.window.show();
    }
}

/// The grid to report for one screen, given the thumbnail edge the model wants.
///
/// The cell has to hold the icon and two lines of name, so the grid follows
/// from the edge rather than from the monitor: a desktop configured for 256px
/// previews wants bigger cells than one configured for 128px ones. The panel
/// reservation is a margin we choose, not a work-area query — GDK 4 does not
/// expose one — and it is the same on every edge so an icon row never starts
/// under a panel.
fn grid_for(screen: &Screen, edge: u32, index: u8) -> ipc::Geometry {
    let _ = screen;
    let (cell_w, cell_h) = grid_reserve(edge);
    ipc::Geometry::new(index, 1, 1, cell_w as u16, cell_h as u16)
}

/// The cell a given thumbnail edge needs, in pixels: the icon, the label, and
/// the gap between them.
fn grid_reserve(edge: u32) -> (i32, i32) {
    let edge = edge.clamp(THUMB_MIN, THUMB_MAX);
    (
        i32::try_from(edge).unwrap_or(i32::MAX) + 24,
        i32::try_from(edge).unwrap_or(i32::MAX) + 24 + LABEL_HEIGHT,
    )
}

/// Tell the provider about every screen we have, and keep what it answers.
///
/// Geometry is reported as one complete set, which is what the provider
/// requires: a partial set would leave a monitor with a grid nobody owns.
/// Returns whether the provider took it, because a desktop with no grids is
/// not a desktop.
fn report(inner: &Rc<Inner>) -> bool {
    let monitors: Vec<ipc::Geometry> = {
        let view = inner.view.borrow();
        view.screens
            .iter()
            .enumerate()
            .map(|(index, _)| grid_for(&view.screens[index], view.edge, index as u8))
            .collect()
    };
    if monitors.is_empty() {
        eprintln!("uflinux-files: no monitor to draw a desktop on");
        return false;
    }
    let body = ipc::Request::SetGeometry { monitors }.encode();
    match call(inner, "SetGeometry", Some(&(&body,).to_variant())) {
        Some(reply) => {
            remember(inner, &reply);
            true
        }
        None => {
            eprintln!("uflinux-files: the desktop provider refused our geometry; not drawing");
            inner.app.quit();
            false
        }
    }
}

/// Pull the model into the view without drawing it.
fn remember(inner: &Rc<Inner>, reply: &str) {
    let Ok(ipc::Reply::Model(model)) = ipc::Reply::decode(reply) else {
        return;
    };
    let edge = model
        .thumbnail_size
        .clamp(THUMB_MIN, THUMB_MAX)
        .min(ipc::MAX_CELL as u32) as u32;
    let mut view = inner.view.borrow_mut();
    view.thumbnails = model.thumbnails;
    view.edge = edge;
    view.requested = model.generation;
    view.model = Some(model);
}

/// Redraw the whole desktop from the model we hold.
///
/// Whole, not incremental. The model is a flat list with a cell per item and
/// the whole thing is capped at a couple of thousand entries, so diffing it
/// would cost about as much as rebuilding it, and rebuilding is the version
/// with no way to leak a widget.
fn redraw(inner: &Rc<Inner>, repaint: bool) {
    if !repaint {
        return;
    }
    let edge = {
        let view = inner.view.borrow();
        let Some(model) = view.model.as_ref() else {
            return;
        };
        view.edge
    };
    let (cell_w, cell_h) = grid_reserve(edge);
    // Re-report before drawing: the cell size follows the thumbnail edge, and
    // an edge we advertise without telling the provider about would place
    // icons into cells we then do not draw.
    let _ = report(inner);
    let stale = inner.view.borrow_mut().take_cells();
    let mut view = inner.view.borrow_mut();
    let Some(model) = view.model.clone() else {
        drop(stale);
        return;
    };
    for screen in &view.screens {
        while let Some(child) = screen.grid.first_child() {
            screen.grid.remove(&child);
        }
    }
    let generation = model.generation;
    let thumbnails = view.thumbnails;
    let mut cells = Vec::with_capacity(model.items.len());
    for item in &model.items {
        let Some(cell) = item.at.as_ref() else {
            continue;
        };
        let Some((index, x, y)) = view.spot(cell) else {
            continue;
        };
        let label = gtk::Label::new(Some(item.label().as_str()));
        label.set_lines(LABEL_LINES);
        label.set_ellipsize(pango::EllipsizeMode::End);
        label.set_justify(gtk::Justification::Center);
        label.set_wrap(true);
        label.set_max_width_chars(18);
        label.add_css_class("ufm-name");
        let tile = tile(&label, item, cell_w, cell_h);
        let button = gtk::Button::new();
        button.set_child(Some(&tile));
        button.set_has_frame(false);
        button.set_tooltip_text(item.tooltip.as_deref());
        button.set_size_request(i32::from(model.thumbnail_size.min(ipc::MAX_CELL as u32)) + 24, 0);
        button.add_css_class("ufm-cell");
        button.connect_clicked({
            let inner = inner.clone();
            let id = item.id.clone();
            move |_| {
                act(&inner, ipc::Request::Launch { id: id.clone() });
            }
        });
        let pressed = gtk::GestureClick::new();
        pressed.set_button(3);
        let menu = gtk::GestureClick::new();
        menu.connect_pressed({
            let inner = inner.clone();
            let id = item.id.clone();
            move |_, _, _, _| {
                act(&inner, ipc::Request::ContextMenu { id: id.clone() });
            }
        });
        let _ = pressed;
        button.add_controller(menu);
        let drag = gtk::GestureDrag::new();
        drag.connect_drag_end({
            let inner = inner.clone();
            let id = item.id.clone();
            move |gesture, dx, dy| {
                let (start_x, start_y) = gesture.start_coords().unwrap_or((dx, dy));
                let Some(monitor) = drop_target(&inner, &button, dx, dy) else {
                    return;
                };
                let Some((col, row)) = drop_slot(&inner, monitor, start_x + dx, start_y + dy)
                else {
                    return;
                };
                act(
                    &inner,
                    ipc::Request::Place {
                        id: id.clone(),
                        monitor,
                        col,
                        row,
                    },
                );
            }
        });
        button.add_controller(drag);
        view.screens[index].grid.put(&button, x as f64, y as f64);
        button.show();
        cells.push((
            item.id.clone(),
            Painted {
                button,
                label,
                thumbnail: StdCell::new(0),
            },
        ));
    }
    view.cells = cells;
    view.drawn = generation;
    if thumbnails {
        fetch(inner.clone(), generation);
    }
}

/// The picture inside one icon: the thumbnail once there is one, the theme
/// icon until then, and the name under both.
fn tile(label: &gtk::Label, item: &ipc::Item, cell_w: i32, cell_h: i32) -> gtk::Box {
    let image = gtk::Image::from_icon_name(item.icon_name());
    image.set_pixel_size(THUMB_MIN as i32);
    image.set_halign(gtk::Align::Center);
    image.set_overflow_hidden(true);
    image.set_widget_name("ufm-desktop-icon");
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 4);
    box_.set_halign(gtk::Align::Center);
    box_.set_valign(gtk::Align::Start);
    box_.set_size_request(cell_w, cell_h);
    box_.append(&image);
    box_.append(label);
    box_
}

/// Which monitor a drop landed on, by way of the window under the pointer.
fn drop_target(inner: &Rc<Inner>, button: &gtk::Button, dx: f64, dy: f64) -> Option<u8> {
    let (x, y) = (dx, dy);
    let surface = button.surface()?;
    let (local_x, local_y) = button.translate_coordinates(&surface, x, y).unwrap_or((x, y));
    let _ = local_y;
    let window = surface.upcast::<gtk::Widget>().toplevel()?;
    inner
        .view
        .borrow()
        .screens
        .iter()
        .position(|screen| screen.window == window)
        .and_then(|index| inner.view.borrow().screens[index].geometry.monitor.into())
        .map(|monitor: u8| monitor)
}

/// Which cell of a monitor a drop point falls in.
fn drop_slot(inner: &Rc<Inner>, monitor: u8, x: f64, y: f64) -> Option<(u16, u16)> {
    let view = inner.view.borrow();
    let geometry = view
        .screens
        .iter()
        .find(|screen| screen.geometry.monitor == monitor)?
        .geometry;
    let col = (x / i32::from(geometry.cell_w) as f64).floor();
    let row = (y / i32::from(geometry.cell_h) as f64).floor();
    if col < 0.0 || row < 0.0 {
        return None;
    }
    let col = u16::try_from(col as i64).unwrap_or(u16::MAX);
    let row = u16::try_from(row as i64).unwrap_or(u16::MAX);
    geometry.fits(geometry.slot(ipc::Cell::new(monitor, col, row).slot()))?;
    Some((col, row))
}

/// Start listening for the provider's news.
fn subscribe(inner: &Rc<Inner>) {
    let signals = [ipc::CHANGED, ipc::PROGRESS]
        .iter()
        .filter_map(|signal| {
            let name = (*signal).to_string();
            let handler = inner.clone();
            inner
                .connection
                .subscribe_to_signal(
                    Some(ipc::SESSION_NAME),
                    Some(ipc::SESSION_NAME),
                    Some(&name),
                    Some(ipc::OBJECT_PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| changed(&handler, &name, signal),
                )
                .ok()
        })
        .collect::<Vec<_>>();
    inner.view.borrow_mut().screens.first()?;
    let mut inner = match Rc::get_mut(&mut inner.clone()) {
        Some(_) => Vec::new(),
        None => Vec::new(),
    };
    inner.extend(signals);
}

/// A `Changed` is a hint to fetch, not a model to apply. The generation is the
/// interesting part: two changes before we finish one repaint are one repaint.
fn changed(inner: &Rc<Inner>, name: &str, signal: &gio::DBusMessage) {
    if name != ipc::CHANGED {
        return;
    }
    let body = signal.body().and_then(|parameters| {
        parameters
            .str::<0>()
            .map(|text| text.as_str().to_string())
            .ok()
    });
    let Some(text) = body else { return };
    let Ok(ipc::Changed { generation, .. }) = ipc::Changed::decode(&text) else {
        return;
    };
    let view = inner.view.borrow();
    if generation <= view.drawn || generation <= view.requested {
        return;
    }
    drop(view);
    fetch(inner.clone(), generation);
}

/// Ask for the model, and repaint when it arrives.
fn fetch(inner: Rc<Inner>, generation: u64) {
    if generation <= inner.view.borrow().requested {
        return;
    }
    inner.view.borrow_mut().requested = generation;
    let body = ipc::Request::GetModel.encode();
    let later = inner.clone();
    inner.connection.call(
        Some(ipc::SESSION_NAME),
        ipc::OBJECT_PATH,
        ipc::SESSION_NAME,
        "GetModel",
        Some(&(&body,).to_variant()),
        Some(glib::VariantTy::TUPLE),
        gio::DBusCallFlags::NONE,
        CALL,
        gio::Cancellable::NONE,
        move |result| {
            let Ok(reply) = result else { return };
            if let Some(text) = reply.str::<0>() {
                remember(&later, text.as_str());
            }
            redraw(&later, true);
        },
    );
}

/// Ask for one thumbnail, and paint it when it arrives.
///
/// Only called for cells that are on screen, which after the initial draw is
/// every cell we have — the saving is not in skipping off-screen icons, it is
/// in never asking for a picture the model said not to ask for.
fn thumbnail(inner: Rc<Inner>, index: usize, path: String, size: u32, generation: u64) {
    let Some(button) = inner
        .view
        .borrow()
        .cells
        .get(index)
        .map(|cell| cell.painted.button.clone())
    else {
        return;
    };
    let body = ipc::Request::RequestThumbnail { path, size }.encode();
    inner
        .connection
        .call_with_unix_fd_list(
            Some(ipc::SESSION_NAME),
            ipc::OBJECT_PATH,
            ipc::SESSION_NAME,
            "RequestThumbnail",
            Some(&(&body,).to_variant()),
            Some(glib::VariantTy::TUPLE),
            gio::DBusCallFlags::NONE,
            CALL,
            None::<&gio::UnixFDList>,
            gio::Cancellable::NONE,
            move |result| {
                let (reply, fds) = match result {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let Some(text) = reply.str::<0>() else {
                    return;
                };
                let Ok(ipc::Reply::Thumbnail(thumb)) = ipc::Reply::decode(text.as_str()) else {
                    return;
                };
                let Some(list) = fds else { return };
                let Some(index) = thumb.fd.and_then(|fd| list.get(fd).ok()) else {
                    return;
                };
                let stream = gio::UnixInputStream::take_fd(index);
                let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_stream_at_scale(
                    &stream,
                    size as i32,
                    size as i32,
                    true,
                    None::<&gio::Cancellable>,
                ) else {
                    return;
                };
                let Some(image) = button
                    .child()
                    .and_then(|tile| tile.downcast::<gtk::Box>().ok())
                    .and_then(|tile| tile.first_child())
                    .and_then(|first| first.downcast::<gtk::Image>().ok())
                else {
                    return;
                };
                image.set_from_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&pixbuf)));
                let _ = generation;
            },
        );
}

/// Ask for thumbnails for every visible cell, once per generation.
fn fetch_thumbnails(inner: Rc<Inner>, generation: u64) {
    let (requests, edge) = {
        let view = inner.view.borrow();
        let Some(model) = view.model.as_ref() else {
            return;
        };
        if !view.thumbnails {
            return;
        }
        let edge = view.edge;
        let requests = model
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.at.is_some())
            .map(|(index, item)| (index, item.id.clone(), edge))
            .collect::<Vec<_>>();
        (requests, edge)
    };
    for (index, id, size) in requests {
        thumbnail(inner.clone(), index, id, size, generation);
    }
}

/// One request, fired and forgotten. The desktop is not a place where a failed
/// call has anywhere to be reported.
fn act(inner: &Rc<Inner>, request: ipc::Request) {
    let method = bus_method(&request);
    let body = request.encode();
    inner.connection.call(
        Some(ipc::SESSION_NAME),
        ipc::OBJECT_PATH,
        ipc::SESSION_NAME,
        method,
        Some(&(&body,).to_variant()),
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

/// One request, and wait for the answer. `None` is any error, and every caller
/// treats "no answer" the same way: give up on the desktop.
fn call(inner: &Rc<Inner>, method: &str, parameters: Option<&glib::Variant>) -> Option<String> {
    let reply = inner
        .connection
        .call_sync(
            Some(ipc::SESSION_NAME),
            ipc::OBJECT_PATH,
            ipc::SESSION_NAME,
            method,
            parameters,
            Some(glib::VariantTy::TUPLE),
            gio::DBusCallFlags::NONE,
            CALL,
            gio::Cancellable::NONE,
        )
        .ok()?;
    reply.str::<0>().map(|text| text.as_str().to_string())
}

/// The bus name for a request. The wire spells methods in kebab-case and D-Bus
/// wants CamelCase, so this cannot be derived.
fn bus_method(request: &ipc::Request) -> &'static str {
    match request {
        ipc::Request::Status { .. } => "Status",
        ipc::Request::GetModel { .. } => "GetModel",
        ipc::Request::SetGeometry { .. } => "SetGeometry",
        ipc::Request::Place { .. } => "Place",
        ipc::Request::Launch { .. } => "Launch",
        ipc::Request::Reveal { .. } => "Reveal",
        ipc::Request::ContextMenu { .. } => "ContextMenu",
        ipc::Request::Trash { .. } => "Trash",
        ipc::Request::RequestThumbnail { .. } => "RequestThumbnail",
    }
}

/// Keep the unused-import lint honest about the priority we do not use yet.
const _: Priority = Priority::DEFAULT;
