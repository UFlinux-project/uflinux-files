//! The "Connect to Server" dialog.
//!
//! One window, a handful of fields, and a worker thread. The fields assemble an
//! address that [`Share::parse`] understands rather than a [`Share`] built by
//! hand, so the dialog and a URI typed on the command line go through the same
//! parser and cannot drift into accepting different things.
//!
//! The mount runs on a worker because [`Runner::mount`] blocks on the backend
//! helper — `mount(8)`, `sshfs`, a polkit prompt — and a dialog that froze for
//! two seconds while a server answered looked broken even when it was only
//! waiting for a network. The result comes back through the window's listing
//! queue (see [`super::job`]), the same pipe that carries directory listings,
//! because a worker cannot touch a widget and no widget is `Send`.
//!
//! The dialog remains open while a mount is pending, prevents duplicate attempts,
//! and retains connection details when the worker reports a failure.

use std::thread;

use gio::prelude::*;
use gtk::prelude::*;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

use ufm_mount::credentials::Secret;
use ufm_mount::uri::{Protocol, Share};

use crate::runner::{Layout, Report, Runner};

use super::job::{Event, Handle};

/// Bytes that have to be escaped to survive a trip through [`Share::parse`].
///
/// The parser percent-decodes every part of an address, so a space or a `%`
/// typed into the share field has to arrive already encoded. `@`, `:`, `?` and
/// `#` are added because each is read as a separator somewhere in an address,
/// and a component containing one would move the boundary between the parts.
/// `/` is *not* added: the resource is a path, and a separator there means a
/// subdirectory, which is what the user meant by typing it.
const ESCAPE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'@')
    .add(b':')
    .add(b'?')
    .add(b'#');

/// The protocol the chooser selects to start with: the one a desktop user is
/// most likely to have been pointed at.
const DEFAULT_PROTOCOL: u32 = 0;

/// Open the connect dialog against `parent`.
///
/// The dialog is a plain [`gtk::Window`] rather than an application window: it is
/// transient, it is modal, and it is not a place to work. It is still added to
/// the application, so that a session whose last real window closed while the
/// dialog was open does not exit from under it.
pub fn open(parent: &gtk::ApplicationWindow, handle: &Handle) {
    let window = build(Some(parent), Some(handle));
    if let Some(app) = parent.application() {
        app.add_window(&window);
    }
    window.present();
}

/// Every widget the dialog owns, so that building them and wiring them can be
/// two readable steps rather than one function the length of a page.
struct Form {
    window: gtk::Window,
    protocols: gtk::DropDown,
    host: gtk::Entry,
    share: gtk::Entry,
    user: gtk::Entry,
    password: gtk::PasswordEntry,
    tls: gtk::CheckButton,
    remember: gtk::CheckButton,
    status: gtk::Label,
    cancel: gtk::Button,
    connect: gtk::Button,
}

/// Build the dialog.
///
/// `handle` is what makes Connect actually mount; without it — which is how the
/// widget tests build it — Connect only validates the address and reports the
/// result in the dialog's own status line. `parent` is optional for the same
/// reason: a test has no application window to be transient for.
fn build(parent: Option<&gtk::ApplicationWindow>, handle: Option<&Handle>) -> gtk::Window {
    let form = compose(parent);
    wire(&form, handle.cloned());
    form.window
}

/// Construct the dialog's widgets and lay them out. Nothing is connected yet.
fn compose(parent: Option<&gtk::ApplicationWindow>) -> Form {
    let window = {
        let mut builder = gtk::Window::builder()
            .title("Connect to Server")
            .modal(true)
            .resizable(false);
        if let Some(parent) = parent {
            builder = builder.transient_for(parent).destroy_with_parent(true);
        }
        builder.build()
    };

    let protocols = gtk::DropDown::from_strings(
        &Protocol::ALL
            .iter()
            .map(|protocol| protocol.label())
            .collect::<Vec<_>>(),
    );
    protocols.set_widget_name("ufm-connect-protocol");
    protocols.set_selected(DEFAULT_PROTOCOL);

    let host = field("ufm-connect-host", "server.example");
    let share = field("ufm-connect-share", "documents");
    let user = field("ufm-connect-user", "");
    let password = gtk::PasswordEntry::new();
    password.set_widget_name("ufm-connect-password");
    password.set_placeholder_text(Some("Password"));
    password.set_activates_default(true);
    password.set_show_peek_icon(true);
    password.set_width_chars(28);

    let tls = gtk::CheckButton::with_label("Use a secure connection");
    tls.set_widget_name("ufm-connect-tls");
    let remember = gtk::CheckButton::with_label("Remember this password");
    remember.set_widget_name("ufm-connect-remember");
    remember.set_active(true);

    let status = gtk::Label::new(None);
    status.set_widget_name("ufm-connect-status");
    status.set_wrap(true);
    status.set_lines(3);
    status.set_xalign(0.0);
    status.set_max_width_chars(44);
    status.set_visible(false);

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(12);
    grid.set_margin_top(18);
    grid.set_margin_bottom(12);
    grid.set_margin_start(18);
    grid.set_margin_end(18);
    attach(&grid, 0, "Server type", &protocols);
    attach(&grid, 1, "Address", &host);
    attach(&grid, 2, "Share", &share);
    attach(&grid, 3, "User name", &user);
    attach(&grid, 4, "Password", &password);
    grid.attach(&tls, 1, 5, 1, 1);
    grid.attach(&remember, 1, 6, 1, 1);
    grid.attach(&status, 0, 7, 2, 1);

    let cancel = gtk::Button::with_label("Cancel");
    cancel.set_widget_name("ufm-connect-cancel");
    let connect = gtk::Button::with_label("Connect");
    connect.set_widget_name("ufm-connect-connect");
    connect.add_css_class("suggested-action");

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(12);
    buttons.set_margin_start(18);
    buttons.set_margin_end(18);
    buttons.set_margin_bottom(18);
    buttons.append(&cancel);
    buttons.append(&connect);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&grid);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&buttons);
    window.set_child(Some(&content));
    window.set_default_widget(Some(&connect));

    Form {
        window,
        protocols,
        host,
        share,
        user,
        password,
        tls,
        remember,
        status,
        cancel,
        connect,
    }
}

/// Connect the dialog's signals, kept apart from construction so that the two
/// rules the dialog exists for — show only what a protocol can use, and hand a
/// parsed address to a worker — each read on their own.
///
/// `handle` is owned here rather than borrowed: a signal handler outlives the
/// call that installs it, and the queue end has to last as long as the dialog.
fn wire(form: &Form, handle: Option<Handle>) {
    // Which fields mean something depends on the protocol: NFS authenticates
    // with the client's identity and SFTP with a key, so a password box for
    // either is a promise the mount cannot keep.
    apply_protocol(&form.protocols, &form.share, &form.password, &form.tls);
    form.protocols.connect_selected_notify({
        let share = form.share.clone();
        let password = form.password.clone();
        let tls = form.tls.clone();
        // No trailing comma: the closure is the block's value, and a comma would
        // turn it into a statement of unit type.
        move |picker| apply_protocol(picker, &share, &password, &tls)
    });

    form.cancel.connect_clicked({
        let window = form.window.clone();
        move |_| window.close()
    });

    form.connect.connect_clicked({
        let window = form.window.clone();
        let protocols = form.protocols.clone();
        let host = form.host.clone();
        let share = form.share.clone();
        let user = form.user.clone();
        let password = form.password.clone();
        let tls = form.tls.clone();
        let remember = form.remember.clone();
        let status = form.status.clone();
        move |button| {
            let Some(protocol) = selected(&protocols) else {
                return;
            };
            let uri = build_uri(
                protocol,
                &host.text(),
                &share.text(),
                &user.text(),
                tls.is_visible() && tls.is_active(),
            );
            let parsed = match Share::parse(&uri) {
                Ok(parsed) => parsed,
                Err(error) => {
                    status.set_label(&error.to_string());
                    status.set_visible(true);
                    return;
                }
            };
            let Some(handle) = &handle else {
                window.close();
                return;
            };
            // "Do not remember" is carried to the worker as a flag rather than
            // applied here: `Runner::mount` caches whatever it is handed, and a
            // password that failed to mount was never stored in the first place.
            let secret = if protocol.uses_password() && !password.text().is_empty() {
                Some(Secret::from(password.text().to_string()))
            } else {
                parsed.password
            };
            password.set_text("");
            let result = mount(handle, parsed.share, secret, remember.is_active());
            button.set_sensitive(false);
            status.set_text("Connecting…");
            status.set_visible(true);
            let window = window.downgrade();
            let status = status.clone();
            let button = button.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
                let Some(window) = window.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                match result.try_recv() {
                    Ok(report) => {
                        button.set_sensitive(true);
                        if report.ok {
                            window.close();
                        } else {
                            status.set_text(&report.message);
                        }
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        button.set_sensitive(true);
                        status.set_text("The connection worker stopped. Try again.");
                        glib::ControlFlow::Break
                    }
                }
            });
        }
    });
}

/// Show the fields the selected protocol can use, and give the share field a
/// placeholder that matches what that protocol actually needs.
fn apply_protocol(
    protocols: &gtk::DropDown,
    share: &gtk::Entry,
    password: &gtk::PasswordEntry,
    tls: &gtk::CheckButton,
) {
    let Some(protocol) = selected(protocols) else {
        return;
    };
    share.set_visible(true);
    if let Some(grid) = password.parent().and_downcast::<gtk::Grid>() {
        if let Some(label) = grid.child_at(0, 4) {
            label.set_visible(protocol.uses_password());
        }
        if let Some(remember) = grid.child_at(1, 6) {
            remember.set_visible(protocol.uses_password());
        }
        if let Some(label) = grid.child_at(0, 2).and_downcast::<gtk::Label>() {
            label.set_text(if protocol == Protocol::Smb {
                "Share"
            } else {
                "Path"
            });
        }
    }
    password.set_visible(protocol.uses_password());
    tls.set_visible(protocol.supports_tls());
    share.set_placeholder_text(Some(if protocol == Protocol::Smb {
        "documents"
    } else {
        "path, optional"
    }));
}

/// The protocol the chooser points at, or `None` if it points past the end —
/// which cannot happen with a model built from [`Protocol::ALL`], but a chooser
/// is exactly the kind of widget that grows a row nobody added.
fn selected(protocols: &gtk::DropDown) -> Option<Protocol> {
    usize::try_from(protocols.selected())
        .ok()
        .and_then(|index| Protocol::ALL.get(index).copied())
}

/// Assemble the address the parser will read.
///
/// Kept apart from the widget code so it can be tested without a display: this
/// is the function that decides whether a typed address means what it looks
/// like, and a wrong answer here mounts the wrong thing rather than failing.
fn build_uri(protocol: Protocol, host: &str, share: &str, user: &str, tls: bool) -> String {
    let mut out = String::with_capacity(48 + host.len() + share.len() + user.len());
    out.push_str(protocol.uri_scheme(tls));
    out.push_str("://");
    let user = user.trim();
    if !user.is_empty() {
        out.push_str(&utf8_percent_encode(user, ESCAPE).to_string());
        out.push('@');
    }
    out.push_str(host.trim());
    let resource = share.trim().trim_matches('/');
    if !resource.is_empty() {
        out.push('/');
        out.push_str(
            &resource
                .split('/')
                .map(|part| utf8_percent_encode(part, ESCAPE).to_string())
                .collect::<Vec<_>>()
                .join("/"),
        );
    }
    out
}

/// Hand the mount to a worker. The caller closes the dialog, so that a share
/// which never got as far as an attempt can leave it open.
///
/// The worker builds its own [`Runner`] rather than borrowing the window's: the
/// window's lives behind the shared state's `RefCell`, and mounting mutates it
/// anyway. What the two share is the file on disk, which is why the window
/// re-reads it when the report arrives — see [`Runner::reload`].
fn mount(
    handle: &Handle,
    share: Share,
    password: Option<Secret>,
    keep: bool,
) -> std::sync::mpsc::Receiver<Report> {
    let (tx, rx) = std::sync::mpsc::channel();
    // Both ends of the message are needed on either path out of here: the worker
    // owns its copies, and the fallback below reports the share by name.
    let worker = handle.clone();
    let label = share.label();
    let spawned = thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut runner = Runner::open(Layout::system());
            let typed = password.is_some();
            let report = runner.mount(&share, password.as_ref());
            if typed && report.ok && !keep {
                // The box said not to keep it. A password already on disk from
                // an earlier connect is left alone, because `typed` is only true
                // for one entered in this dialog.
                let _ = runner.forget(&share);
            }
            let _ = tx.send(report.clone());
            worker.send(Event::Mounted { report });
        });
    if spawned.is_err() {
        // Out of threads. Nothing was attempted, so the message says so rather
        // than naming a backend that never ran.
        handle.send(Event::Mounted {
            report: Report {
                message: format!("{label}: cannot start a mount"),
                id: None,
                ok: false,
            },
        });
    }
    rx
}

/// One labelled row of the form field.
fn attach<W: IsA<gtk::Widget>>(grid: &gtk::Grid, row: i32, text: &str, widget: &W) {
    let label = row_label(text);
    label.set_mnemonic_widget(Some(widget));
    widget
        .as_ref()
        .update_relation(&[gtk::accessible::Relation::LabelledBy(&[label.upcast_ref()])]);
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(widget, 1, row, 1, 1);
}

/// A labelled text field; all three look the same.
fn field(name: &str, placeholder: &str) -> gtk::Entry {
    let entry = gtk::Entry::new();
    entry.set_widget_name(name);
    entry.set_placeholder_text(Some(placeholder));
    entry.set_activates_default(true);
    entry.set_width_chars(28);
    entry.set_max_width_chars(28);
    entry
}

/// A field label, right-aligned against the column of fields it names.
fn row_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(1.0);
    label
}

/// Pull one named widget out of a widget tree. Tests reach the dialog's fields
/// this way rather than by position, so reordering the rows costs them nothing.
#[cfg(test)]
fn find<W: IsA<gtk::Widget>>(root: &W, name: &str) -> Option<gtk::Widget> {
    let root: &gtk::Widget = root.as_ref();
    for child in root.observe_children().snapshot() {
        let Ok(widget) = child.downcast::<gtk::Widget>() else {
            continue;
        };
        if widget.widget_name() == name {
            return Some(widget);
        }
        if let Some(deeper) = find(&widget, name) {
            return Some(deeper);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_reassembles_into_the_share_it_looks_like() {
        let uri = build_uri(Protocol::Smb, "server", "documents", "ada", false);
        let parsed = Share::parse(&uri).expect("the dialog's own address parses");
        assert_eq!(parsed.share.protocol, Protocol::Smb);
        assert_eq!(parsed.share.host, "server");
        assert_eq!(parsed.share.resource, "/documents");
        assert_eq!(parsed.share.user.as_deref(), Some("ada"));
        assert!(!parsed.share.tls);
    }

    #[test]
    fn a_secure_protocol_gets_the_scheme_with_the_s_on_it() {
        let uri = build_uri(Protocol::WebDav, "files.example", "notes", "", true);
        let parsed = Share::parse(&uri).expect("a davs address parses");
        assert_eq!(parsed.share.protocol, Protocol::WebDav);
        assert!(parsed.share.tls);
        assert_eq!(parsed.share.port_used(), 443);
        assert_eq!(parsed.share.user, None);
    }

    #[test]
    fn ftp_switches_scheme_with_tls_too() {
        let plain = Share::parse(&build_uri(Protocol::Ftp, "h", "", "", false)).expect("ftp");
        let secure = Share::parse(&build_uri(Protocol::Ftp, "h", "", "", true)).expect("ftps");
        assert!(!plain.share.tls);
        assert!(secure.share.tls);
        assert_eq!(plain.share.port_used(), 21);
        assert_eq!(secure.share.port_used(), 990);
    }

    #[test]
    fn a_share_name_with_a_space_survives_the_round_trip() {
        let uri = build_uri(Protocol::Smb, "box", "My Docs", "", false);
        let parsed = Share::parse(&uri).expect("an encoded share name parses");
        assert_eq!(parsed.share.resource, "/My Docs");
    }

    #[test]
    fn a_user_name_that_looks_like_an_address_stays_one_user_name() {
        // A Windows UPN is `user@domain`, and an unescaped `@` would move the
        // boundary between the user name and the host.
        let uri = build_uri(Protocol::Smb, "box", "s", "ada@corp", false);
        let parsed = Share::parse(&uri).expect("an encoded user parses");
        assert_eq!(parsed.share.user.as_deref(), Some("ada@corp"));
        assert_eq!(parsed.share.host, "box");
    }

    #[test]
    fn a_nested_path_keeps_its_separators() {
        let uri = build_uri(Protocol::Nfs, "srv", "srv/www", "", false);
        let parsed = Share::parse(&uri).expect("a nested resource parses");
        assert_eq!(parsed.share.resource, "/srv/www");
    }

    #[test]
    fn an_empty_resource_is_no_resource_at_all() {
        let uri = build_uri(Protocol::Nfs, "srv", "  /  ", "", false);
        let parsed = Share::parse(&uri).expect("a bare host parses");
        assert_eq!(parsed.share.resource, "");
    }

    #[test]
    fn surrounding_spaces_are_not_part_of_the_host() {
        let uri = build_uri(Protocol::Sftp, "  srv  ", " x ", "  ada  ", false);
        let parsed = Share::parse(&uri).expect("trimmed fields parse");
        assert_eq!(parsed.share.host, "srv");
        assert_eq!(parsed.share.resource, "/x");
        assert_eq!(parsed.share.user.as_deref(), Some("ada"));
    }

    #[test]
    fn a_missing_host_is_rejected_before_anything_runs() {
        let uri = build_uri(Protocol::Smb, "", "docs", "", false);
        assert!(Share::parse(&uri).is_err(), "{uri} should not be mountable");
    }

    #[test]
    fn smb_without_a_share_name_is_rejected() {
        let uri = build_uri(Protocol::Smb, "server", "", "", false);
        assert!(
            Share::parse(&uri).is_err(),
            "smb://server has nothing to mount"
        );
    }

    #[test]
    fn the_other_protocols_may_mount_at_the_root() {
        for protocol in [
            Protocol::Nfs,
            Protocol::Sftp,
            Protocol::WebDav,
            Protocol::Ftp,
        ] {
            let uri = build_uri(protocol, "server", "", "", false);
            assert!(Share::parse(&uri).is_ok(), "{uri} should be mountable");
        }
    }

    #[test]
    fn every_offered_protocol_is_named_in_the_chooser() {
        // The chooser is built from `Protocol::ALL`, so a protocol added there
        // appears in the dialog without anything else changing.
        assert_eq!(Protocol::ALL.len(), 5);
        for protocol in Protocol::ALL {
            assert!(!protocol.label().is_empty());
        }
    }

    #[test]
    fn the_dialog_shows_only_the_fields_the_protocol_uses() {
        crate::ui::tk::ui(|| {
            let window = build(None, None);
            let picker = find(&window, "ufm-connect-protocol")
                .expect("the chooser is in the dialog")
                .downcast::<gtk::DropDown>()
                .expect("the chooser is a drop-down");
            let password =
                find(&window, "ufm-connect-password").expect("the password field is in the dialog");
            let tls = find(&window, "ufm-connect-tls").expect("the TLS row is in the dialog");

            for (index, protocol) in Protocol::ALL.iter().enumerate() {
                picker.set_selected(u32::try_from(index).expect("a small index"));
                assert_eq!(
                    password.get_visible(),
                    protocol.uses_password(),
                    "{} should decide the password field",
                    protocol.label()
                );
                assert_eq!(
                    tls.get_visible(),
                    protocol.supports_tls(),
                    "{} should decide the TLS row",
                    protocol.label()
                );
            }
        });
    }

    #[test]
    fn the_dialog_starts_on_the_default_protocol_with_no_error_shown() {
        crate::ui::tk::ui(|| {
            let window = build(None, None);
            let picker = find(&window, "ufm-connect-protocol")
                .expect("the chooser is in the dialog")
                .downcast::<gtk::DropDown>()
                .expect("the chooser is a drop-down");
            assert_eq!(picker.selected(), DEFAULT_PROTOCOL);
            let status = find(&window, "ufm-connect-status").expect("there is a status line");
            assert!(!status.get_visible(), "nothing has gone wrong yet");
            let remember = find(&window, "ufm-connect-remember")
                .expect("there is a remember box")
                .downcast::<gtk::CheckButton>()
                .expect("the remember box is a check box");
            assert!(remember.is_active(), "a password is kept by default");
        });
    }

    #[test]
    fn connect_without_a_host_reports_the_reason_in_the_dialog() {
        crate::ui::tk::ui(|| {
            // Built without a handle: Connect validates and reports in place,
            // which is the path a bad address takes either way.
            let window = build(None, None);
            let connect = find(&window, "ufm-connect-connect")
                .expect("there is a Connect button")
                .downcast::<gtk::Button>()
                .expect("the Connect button is a button");
            connect.emit_clicked();
            let status = find(&window, "ufm-connect-status").expect("there is a status line");
            assert!(status.get_visible(), "an empty host has to say why");
        });
    }
}
