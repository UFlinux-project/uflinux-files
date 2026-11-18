//! Desktop file clipboard formats, bounded reads, and off-thread metadata lookup.
use super::ops::Transfer;
use gio::prelude::*;
use gtk::gdk;
use std::sync::Arc;
use ufm_core::{Location, entry::Entry};
use ufm_vfs::Readers;

thread_local! {
    static OWNED: std::cell::RefCell<Option<(gdk::ContentProvider, Vec<Entry>, Transfer)>> = const { std::cell::RefCell::new(None) };
}

pub fn changed(clipboard: &gdk::Clipboard) {
    OWNED.with(|owned| {
        let stale = owned
            .borrow()
            .as_ref()
            .is_some_and(|(provider, _, _)| clipboard.content().as_ref() != Some(provider));
        if stale {
            owned.borrow_mut().take();
        }
    });
}

pub fn is_cut(location: &Location) -> bool {
    OWNED.with(|owned| {
        owned
            .borrow()
            .as_ref()
            .is_some_and(|(_, entries, transfer)| {
                *transfer == Transfer::Move
                    && entries.iter().any(|entry| entry.location == *location)
            })
    })
}

/// Only update a clipboard still owned by this operation; never replace newer data.
pub fn prune_moved(clipboard: gdk::Clipboard) {
    let owned = OWNED.with(|owned| owned.borrow().clone());
    let Some((provider, entries, Transfer::Move)) = owned else {
        return;
    };
    if clipboard.content().as_ref() != Some(&provider) {
        return;
    }
    let total = entries.len();
    glib::spawn_future_local(async move {
        let remaining = gio::spawn_blocking(move || {
            entries
                .into_iter()
                .filter(|entry| {
                    entry
                        .location
                        .as_path()
                        .is_some_and(|path| !matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound))
                })
                .collect::<Vec<_>>()
        })
        .await;
        if clipboard.content().as_ref() != Some(&provider) {
            return;
        }
        if let Ok(remaining) = remaining
            && remaining.len() != total
        {
            if remaining.is_empty() {
                OWNED.with(|owned| {
                    owned.borrow_mut().take();
                });
                let _ = clipboard.set_content(None::<&gdk::ContentProvider>);
            } else {
                let _ = publish(&clipboard, &remaining, Transfer::Move);
            }
        }
    });
}

const COPIED: &str = "x-special/gnome-copied-files";
const URIS: &str = "text/uri-list";
const LIMIT: usize = 1024 * 1024;

pub fn available(clipboard: &gdk::Clipboard) -> bool {
    let formats = clipboard.formats();
    formats.contain_mime_type(COPIED)
        || formats.contain_mime_type(URIS)
        || formats.contains_type(gdk::FileList::static_type())
}

pub fn publish(
    clipboard: &gdk::Clipboard,
    entries: &[Entry],
    transfer: Transfer,
) -> Result<(), glib::BoolError> {
    let files: Vec<_> = entries
        .iter()
        .filter_map(|entry| entry.location.as_path())
        .map(gio::File::for_path)
        .collect();
    let uris = files
        .iter()
        .map(|file| file.uri().to_string())
        .collect::<Vec<_>>()
        .join("\r\n");
    let copied = format!(
        "{}\n{}",
        if transfer == Transfer::Move {
            "cut"
        } else {
            "copy"
        },
        uris
    );
    let provider = gdk::ContentProvider::new_union(&[
        gdk::ContentProvider::for_bytes(COPIED, &glib::Bytes::from_owned(copied.into_bytes())),
        gdk::ContentProvider::for_bytes(
            URIS,
            &glib::Bytes::from_owned(format!("{uris}\r\n").into_bytes()),
        ),
        gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()),
    ]);
    OWNED.with(|owned| *owned.borrow_mut() = Some((provider.clone(), entries.to_vec(), transfer)));
    clipboard.set_content(Some(&provider))
}

fn decode(bytes: &[u8], copied: bool) -> Result<(Vec<Location>, Transfer), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "The file clipboard is not valid text.")?;
    let mut lines = text.lines();
    let transfer = if copied {
        match lines.next().map(str::trim) {
            Some("cut") => Transfer::Move,
            Some("copy") => Transfer::Copy,
            _ => return Err("The clipboard has an unknown file operation.".into()),
        }
    } else {
        Transfer::Copy
    };
    let mut locations = Vec::new();
    for line in lines
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        // GIO validates file URI authority and escaping, including non-UTF8 names.
        let file = gio::File::for_uri(line);
        let Some(path) = file.path() else {
            return Err("Only local files and mounted shares can be pasted.".into());
        };
        let location = Location::file(path);
        if !locations.contains(&location) {
            locations.push(location);
        }
    }
    Ok((locations, transfer))
}

pub async fn read(
    clipboard: gdk::Clipboard,
    readers: Arc<Readers>,
) -> Result<(Vec<Entry>, Transfer), String> {
    let formats = clipboard.formats();
    let (locations, transfer) =
        if formats.contain_mime_type(COPIED) || formats.contain_mime_type(URIS) {
            let (stream, mime) = clipboard
                .read_future(&[COPIED, URIS], glib::Priority::DEFAULT)
                .await
                .map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            loop {
                let chunk = stream
                    .read_bytes_future(16 * 1024, glib::Priority::DEFAULT)
                    .await
                    .map_err(|error| error.to_string())?;
                if chunk.is_empty() {
                    break;
                }
                if bytes.len() + chunk.len() > LIMIT {
                    return Err("The file clipboard is too large.".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            decode(&bytes, mime == COPIED)?
        } else {
            let value = clipboard
                .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                .await
                .map_err(|error| error.to_string())?;
            let files = value
                .get::<gdk::FileList>()
                .map_err(|error| error.to_string())?;
            (
                files
                    .files()
                    .iter()
                    .filter_map(|file| file.path())
                    .map(Location::file)
                    .collect(),
                Transfer::Copy,
            )
        };
    gio::spawn_blocking(move || {
        let mut entries = Vec::new();
        for location in locations {
            entries.push(
                readers
                    .metadata(&location)
                    .map_err(|error| format!("{}: {error}", location.display()))?,
            );
        }
        Ok((entries, transfer))
    })
    .await
    .map_err(|_| "Could not read clipboard files.".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::prelude::DisplayExt;
    #[test]
    fn native_clipboard_round_trip_preserves_cut_and_file_identity() {
        crate::ui::tk::ui(|| {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("hello % world.txt");
            std::fs::write(&path, "contents").unwrap();
            let readers = Arc::new(Readers::local_only());
            let entry = readers.metadata(&Location::file(&path)).unwrap();
            let clipboard = gdk::Display::default().unwrap().clipboard();
            publish(&clipboard, &[entry], Transfer::Move).unwrap();
            assert!(available(&clipboard));
            assert!(is_cut(&Location::file(&path)));
            let (tx, rx) = std::sync::mpsc::channel();
            glib::spawn_future_local(async move {
                let first = read(clipboard.clone(), readers.clone()).await;
                assert!(first.is_ok());
                tx.send(read(clipboard, readers).await).unwrap();
            });
            let start = std::time::Instant::now();
            let result = loop {
                if let Ok(result) = rx.try_recv() {
                    break result;
                }
                assert!(
                    start.elapsed() < std::time::Duration::from_secs(5),
                    "clipboard read timed out"
                );
                glib::MainContext::default().iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            let (entries, transfer) = result.unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].location, Location::file(path));
            assert_eq!(transfer, Transfer::Move);
        });
    }

    #[test]
    fn desktop_clipboard_preserves_cut_and_escaped_names() {
        let (locations, transfer) =
            decode(b"cut\nfile:///tmp/a%20b\nfile:///tmp/a%20b\n", true).unwrap();
        assert_eq!(transfer, Transfer::Move);
        assert_eq!(locations, vec![Location::file("/tmp/a b")]);
        assert!(decode(b"erase\nfile:///tmp/a", true).is_err());
        assert!(decode(b"sftp://server/path", false).is_err());
        assert_eq!(
            decode(b"# comment\r\nfile:///tmp/a\r\n", false).unwrap().1,
            Transfer::Copy
        );
    }
}
