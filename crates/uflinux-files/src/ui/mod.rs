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
mod css;
mod job;
mod panel;
mod row;
mod view;
mod window;

#[cfg(test)]
mod tk;

use std::process::ExitCode;

use gtk::prelude::*;

/// Open a window per argument, or one window at the home folder.
///
/// The application is registered so a second invocation asks the first to open
/// a folder rather than starting a second process with its own windows, its own
/// thumbnail cache, and its own copy of the folder cache.
pub fn run(urls: &[String]) -> ExitCode {
    let app = gtk::Application::builder()
        .application_id("org.uflinux.FileManager")
        .build();
    app.connect_activate({
        let urls = urls.to_vec();
        move |app| {
            css::install();
            window::open(app, &urls);
        }
    });
    app.run().into()
}
