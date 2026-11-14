//! The GTK4 interface.
//!
//! The layout is the one GNOME users recognise, with the additions the
//! `nautilus-my-computer` extension made worth having: a folder tree on the
//! left, one of three presentations on the right, and a computer panel that
//! shows what is mounted and how full it is. There is no libadwaita here. The
//! window is a plain [`gtk::ApplicationWindow`] and the styling is a CSS
//! provider we ship (see [`css`]), which keeps the binary runnable on a
//! GTK-only install and keeps its footprint out of the reach of a toolkit we do
//! not need.
//!
//! Everything in this directory runs on the main thread. Reading a directory is
//! the one thing that must not, so [`view`] and [`columns`] receive listings
//! that a worker thread produced elsewhere (see [`job`]) and only ever splice
//! them into the model.

// The window, and the modules that follow it, are what make these live. Until
// `ui()` builds a window, nothing here is reachable from `main`, and a warning
// per public item would be noise about work in progress rather than about code.
#![allow(dead_code)]

mod columns;
mod connect;
mod css;
mod job;
mod ops;
mod panel;
mod row;
mod search;
mod view;
mod window;

#[cfg(test)]
mod tk;

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

// gio's, not gtk's: `connect_activate`, `connect_open` and `run_with_args` all
// live on gio's `ApplicationExtManual`, and the window API is reached through
// `window::open` rather than through a trait method here.
use gio::prelude::*;

use crate::cli;

/// The application id, also the bus name a shell uses to focus an existing window
/// and the `X11` class a startup notification is matched against.
const APP_ID: &str = "org.uflinux.FileManager";

/// Run the file manager until its last window closes.
///
/// The locations arrive through the `open` signal rather than as a list read here,
/// so that a second invocation hands its folders to the window already on screen
/// instead of starting a second copy of the program with its own thumbnail cache
/// and its own folder cache. `--new-window` opts out of that by adding
/// `NON_UNIQUE`, which makes the process its own primary instance and so costs a
/// second copy of the program — the price of asking for a window that is definitely
/// new rather than one that might only be focused. The id stays the same either
/// way, so the shell still folds the new window in with the old ones.
pub fn run(options: &cli::Ui) -> ExitCode {
    pick_renderer();
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(flags(options))
        .build();
    app.connect_activate(|app| {
        css::install();
        window::open(app, &[]);
    });
    app.connect_open(|app, files, _hint| {
        css::install();
        // URIs rather than paths: a location that is not a local directory — an
        // archive, a mount root, `computer://` — still has a URI that says what it
        // is, and `Location::parse` is the one place that knows how to read it.
        let urls: Vec<String> = files.iter().map(|file| file.uri().to_string()).collect();
        window::open(app, &urls);
    });

    // The argv is rebuilt from what [`cli`] already understood, so an option this
    // module has never heard of cannot reach GApplication and be rejected as a
    // malformed command line. What is left is the program name followed by the
    // locations, which is the shape GApplication's own file handling expects.
    let mut argv = vec![
        std::env::args()
            .next()
            .unwrap_or_else(|| String::from("uflinux-files")),
    ];
    argv.extend(options.urls.iter().cloned());
    app.run_with_args(&argv).into()
}

/// What the application tells `GApplication` it can do.
///
/// `HANDLES_OPEN` is in both arms. Leaving it out does not merely drop the flag:
/// `GApplication` refuses the locations outright and prints a critical, which was
/// measured against the built binary rather than read off the documentation.
fn flags(options: &cli::Ui) -> gio::ApplicationFlags {
    if options.new_window {
        gio::ApplicationFlags::NON_UNIQUE | gio::ApplicationFlags::HANDLES_OPEN
    } else {
        gio::ApplicationFlags::HANDLES_OPEN
    }
}

/// Pick a renderer before GTK reads one.
///
/// `GSK_RENDERER` is the documented knob and there is no setter for it, so the choice has
/// to be made before the display exists — which means before the application is built,
/// which is also before GTK, `GApplication` or any worker thread of ours exist. That is
/// the only context in which writing the environment is sound, and the reason this is a
/// function called on one line rather than a method on the builder.
///
/// GTK's default is GL, which is the right answer wherever there is a GPU. Where there is
/// none, Mesa answers with llvmpipe, and llvmpipe's rasteriser buffers are anonymous
/// memory: measured on a machine with no render node, one window on an empty folder cost
/// 222 MiB of PSS with 137 MiB of it inside Mesa's software pipeline, against 54 MiB with
/// cairo. A machine that cannot accelerate a shader is not a machine that can afford to
/// cache one, and cairo is what GTK recommends for exactly that case. An explicit
/// `GSK_RENDERER` always wins, including `=gl`, for whoever is measuring the difference.
fn pick_renderer() {
    let Some(name) = renderer_choice(
        std::env::var_os("GSK_RENDERER").as_deref(),
        Path::new("/dev/dri"),
    ) else {
        return;
    };
    // SAFETY: nothing has been started yet — no display, no thread, no GApplication — so
    // there is no concurrent reader of the environment to race with.
    unsafe { std::env::set_var("GSK_RENDERER", name) };
}

/// The renderer to ask for, given what the environment already says and what `/dev/dri`
/// holds. `None` means leave the environment alone, which is GTK's own default.
///
/// Split out from [`pick_renderer`] because the interesting part of this decision is that
/// it happens once, in a context where mutating the environment is safe, and a test that
/// called `pick_renderer` would change the renderer for every other test in the binary.
fn renderer_choice(explicit: Option<&OsStr>, dri: &Path) -> Option<&'static str> {
    if explicit.is_some() || has_render_node(dri) {
        None
    } else {
        Some("cairo")
    }
}

/// Whether the machine has a GPU this process could draw with.
///
/// `cardN` is what a driver creates and `renderD128N` is what it creates for rendering
/// without a display; a `/dev/dri` holding neither, or no directory at all, means every GL
/// context comes out of Mesa's software fallback.
fn has_render_node(dri: &Path) -> bool {
    std::fs::read_dir(dri).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("card") || name.starts_with("renderD")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(urls: &[&str], new_window: bool) -> cli::Ui {
        cli::Ui {
            urls: urls.iter().map(|u| (*u).to_string()).collect(),
            new_window,
        }
    }

    #[test]
    fn a_plain_run_declares_that_it_opens_locations() {
        // Without this the desktop entry's `%F` is dropped on the floor and the
        // program prints a GLib critical instead of showing the folder it was
        // given — which is exactly how the omission was first noticed.
        let flags = flags(&ui(&["/tmp"], false));
        assert!(flags.contains(gio::ApplicationFlags::HANDLES_OPEN));
    }

    #[test]
    fn a_new_window_run_gives_up_being_the_single_instance() {
        let plain = flags(&ui(&[], false));
        let fresh = flags(&ui(&[], true));
        // `NON_UNIQUE` is the whole mechanism: without it the second invocation
        // forwards its locations to the window already on screen and exits, and
        // the desktop entry's `new-window` action appears to do nothing.
        assert!(fresh.contains(gio::ApplicationFlags::NON_UNIQUE));
        assert!(
            !plain.contains(gio::ApplicationFlags::NON_UNIQUE),
            "the ordinary run must still be single-instance: {plain:?} vs {fresh:?}"
        );
        // Both arms need `HANDLES_OPEN`, and the flag set is the only place that
        // could lose it, so it is pinned here rather than left to the comment.
        assert!(fresh.contains(gio::ApplicationFlags::HANDLES_OPEN));
        assert!(plain.contains(gio::ApplicationFlags::HANDLES_OPEN));
    }

    #[test]
    fn the_application_id_matches_the_desktop_entry() {
        // The desktop entry is `data/org.uflinux.Files.desktop`; a shell matches a
        // startup notification to the window that answers it through this name.
        assert_eq!(APP_ID, "org.uflinux.FileManager");
        let entry = include_str!("../../../../data/org.uflinux.Files.desktop");
        assert!(
            entry.contains("StartupWMClass=uflinux-files"),
            "the desktop entry lost the class the window reports"
        );
    }

    #[test]
    fn a_machine_without_a_render_node_is_asked_for_cairo() {
        // The container this suite runs in has no /dev/dri at all, which is the case the
        // 90 MiB window budget was written for and the case GL fails by 130 MiB.
        let missing = std::env::temp_dir().join("ufm-no-such-dri");
        assert_eq!(
            renderer_choice(None, &missing),
            Some("cairo"),
            "a machine with no GPU must not be handed Mesa's software pipeline"
        );
    }

    #[test]
    fn a_render_node_leaves_the_renderer_to_gtk() {
        let dri = std::env::temp_dir().join("ufm-fake-dri");
        let _ = std::fs::remove_dir_all(&dri);
        std::fs::create_dir_all(&dri).unwrap();
        // Cleaned up before the assertions: a leftover directory under /tmp would make
        // every later run of this suite look like it had a GPU.
        std::fs::write(dri.join("renderD128"), []).unwrap();
        let with_render_node = renderer_choice(None, &dri);
        std::fs::remove_file(dri.join("renderD128")).unwrap();
        std::fs::write(dri.join("card0"), []).unwrap();
        let with_card = renderer_choice(None, &dri);
        std::fs::remove_dir_all(&dri).unwrap();
        assert_eq!(
            with_render_node, None,
            "renderD128 is a GPU as far as this cares"
        );
        assert_eq!(with_card, None, "card0 is a GPU as far as this cares");
    }

    #[test]
    fn an_explicit_renderer_is_never_overridden() {
        // Includes GSK_RENDERER=gl: whoever sets it is either shipping a GPU driver or
        // measuring what the two renderers cost, and in both cases being contradicted
        // would be worse than the memory saved.
        let missing = std::env::temp_dir().join("ufm-no-such-dri");
        assert_eq!(renderer_choice(Some(OsStr::new("gl")), &missing), None);
        assert_eq!(
            renderer_choice(Some(OsStr::new("cairo")), &missing),
            None,
            "setting it to cairo still needs no change, and must not be treated as a no-op"
        );
    }
}
