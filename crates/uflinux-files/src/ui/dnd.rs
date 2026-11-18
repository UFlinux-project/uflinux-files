//! GTK file drag-and-drop. Copy is the default; Shift requests a move.
use super::row::Row;
use gtk::{gdk, prelude::*};
use ufm_core::{Location, entry::Entry};

pub fn source(widget: &impl IsA<gtk::Widget>, selected: impl Fn() -> Vec<Entry> + 'static) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
    source.connect_prepare(move |_, _, _| {
        let files: Vec<_> = selected()
            .iter()
            .filter_map(|entry| entry.location.as_path().map(gio::File::for_path))
            .collect();
        (!files.is_empty())
            .then(|| gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()))
    });
    widget.add_controller(source);
}

pub fn target(widget: &impl IsA<gtk::Widget>, destination: impl Fn() -> Option<String> + 'static) {
    let target = gtk::DropTarget::new(
        gdk::FileList::static_type(),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );
    target.connect_enter(|target, _, _| {
        if target
            .current_event_state()
            .contains(gdk::ModifierType::SHIFT_MASK)
        {
            gdk::DragAction::MOVE
        } else {
            gdk::DragAction::COPY
        }
    });
    target.connect_motion(|target, _, _| {
        if target
            .current_event_state()
            .contains(gdk::ModifierType::SHIFT_MASK)
        {
            gdk::DragAction::MOVE
        } else {
            gdk::DragAction::COPY
        }
    });
    target.connect_drop(move |target, value, _, _| {
        let Some(destination) = destination() else {
            return false;
        };
        let Ok(files) = value.get::<gdk::FileList>() else {
            return false;
        };
        let uris: Vec<_> = files
            .files()
            .iter()
            .filter(|file| file.path().is_some())
            .map(|file| file.uri().to_string())
            .collect();
        if uris.is_empty() {
            return false;
        }
        let moving = target
            .current_event_state()
            .contains(gdk::ModifierType::SHIFT_MASK);
        let Some(window) = target
            .widget()
            .and_then(|widget| widget.root())
            .and_downcast::<gtk::ApplicationWindow>()
        else {
            return false;
        };
        gtk::prelude::WidgetExt::activate_action(
            &window,
            "win.drop-files",
            Some(&(destination, uris, moving).to_variant()),
        )
        .is_ok()
    });
    widget.add_controller(target);
}

pub fn cell(widget: &impl IsA<gtk::Widget>, item: &gtk::ListItem) {
    let item = item.downgrade();
    target(widget, move || {
        let row = item.upgrade()?.item()?.downcast::<Row>().ok()?;
        let entry = row.entry();
        (entry.kind.is_dir() && entry.location.is_real()).then(|| entry.location.to_string())
    });
}

pub fn place(widget: &impl IsA<gtk::Widget>, location: Location) {
    if location.is_real() {
        target(widget, move || Some(location.to_string()));
    }
}
