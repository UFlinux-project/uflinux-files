//! Watching the folders a window has on screen.
//!
//! Without this a listing is a photograph: a file created by a terminal, an
//! editor's save, a download finishing in the background, none of it appears
//! until the user navigates or presses reload. [`Watcher`] answers the question
//! "which directory went dirty" and nothing else, so this module is the wiring:
//! say which folders are visible, and re-read one when the kernel says it moved.
//!
//! The watched set is whatever is on screen, which is why it is recomputed on
//! every navigation rather than accumulated: the current folder plus the column
//! chain is at most seven paths, and the watcher's cap is sixteen. Watching a
//! home directory recursively would cost one kernel watch per subfolder, which on
//! a large tree is more memory than the listing itself.
//!
//! [`Watcher`]: ufm_vfs::watch::Watcher

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use glib::ControlFlow;
use glib::IOCondition;
use glib::source::{SourceId, unix_fd_add_local};

use ufm_core::Location;
use ufm_vfs::watch::Watcher;

/// Keeps a window's folders watched, and says when one of them has settled.
///
/// `None` from [`Live::new`] means inotify refused an instance, which is a
/// kernel limit rather than a bug; a window without a watcher behaves exactly as
/// it did before this module existed.
pub struct Live {
    watcher: Arc<Watcher>,
    /// Held so that dropping this struct detaches the source. Dropping the
    /// [`SourceId`] alone leaves it armed, and its callback would then be reading
    /// a descriptor that has already been closed. `Option` because taking it out
    /// is how `Drop` gets ownership of a value that is not `Copy`.
    source: RefCell<Option<SourceId>>,
}

impl Live {
    /// Start watching, waking `on_change` on the main thread whenever a watched
    /// folder has been quiet long enough to re-read.
    ///
    /// Attaches a `GLib` fd source, so this has to run on the thread that owns
    /// the main context — the same rule [`Queue::new`] follows.
    ///
    /// [`Queue::new`]: super::job::Queue::new
    pub fn new<F>(mut on_change: F) -> Option<Rc<Self>>
    where
        F: FnMut(&[PathBuf], bool) + 'static,
    {
        let watcher = Arc::new(Watcher::new()?);
        // The callback keeps its own reference rather than borrowing this struct:
        // the source is owned by `Live`, and borrowing would be a cycle. It never
        // reaches back into `Live`, which is why a change is handed over as plain
        // paths, and the extra reference cannot outlive the source that holds it —
        // `Drop` detaches the source before releasing the watcher.
        let fd = watcher.notify_fd();
        let reading = Arc::clone(&watcher);
        let source = unix_fd_add_local(fd, IOCondition::IN, move |_, _| {
            // `ready` is what drains the wake descriptor, so it has to be called
            // even when the answer is thrown away: leaving the descriptor
            // readable would have the source fire again immediately, forever.
            let (ready, overflowed) = (reading.ready(), reading.take_overflow());
            on_change(&ready, overflowed);
            ControlFlow::Continue
        });
        Some(Rc::new(Self {
            watcher,
            source: RefCell::new(Some(source)),
        }))
    }

    /// Make the watched set the folders on screen: the one in the title bar plus
    /// the column panes, outermost first.
    ///
    /// Cheap when nothing moved, which is the common case: navigating between two
    /// folders in one parent changes one path out of several, and the watcher
    /// keeps every watch it already has rather than losing the ones it re-adds.
    ///
    /// The watcher's own set is what is reconciled, not a copy remembered here, so
    /// a watch the kernel dropped on its own comes back: deleting a folder makes
    /// the kernel retire its watch, and re-declaring the set is the only way to
    /// notice a folder that was deleted and then created again.
    pub fn sync(&self, current: Option<&Location>, panes: &[Location]) {
        self.watcher.sync(&folders(current, panes));
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Detaching first means no callback can run against descriptors the
        // watcher is about to close.
        if let Some(source) = self.source.borrow_mut().take() {
            source.remove();
        }
    }
}

/// The folders worth a kernel watch: one per location that is a directory the
/// kernel can see, in the order given, with duplicates removed.
///
/// An archive keeps its container rather than the path inside it — inotify
/// cannot see a member, and the container's own events are the honest signal
/// that the listing may have gone stale. A virtual folder has no path at all, so
/// it contributes nothing and stays as it was until the user reloads.
#[must_use]
pub fn folders(current: Option<&Location>, panes: &[Location]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(panes.len() + 1);
    for location in current.into_iter().chain(panes.iter()) {
        if let Some(path) = location.as_path()
            && !out.iter().any(|seen| seen == path)
        {
            out.push(path.to_path_buf());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(path: &str) -> Location {
        Location::file(path)
    }

    #[test]
    fn the_current_folder_and_every_pane_are_watched() {
        let got = folders(
            Some(&location("/home/u/projects")),
            &[location("/"), location("/home"), location("/home/u")],
        );
        assert_eq!(
            got,
            vec![
                PathBuf::from("/home/u/projects"),
                PathBuf::from("/"),
                PathBuf::from("/home"),
                PathBuf::from("/home/u"),
            ]
        );
    }

    #[test]
    fn a_pane_that_is_the_current_folder_is_listed_once() {
        // Columns show the current folder as their deepest pane, so the same
        // path arrives twice on every navigation in that mode.
        let here = location("/home/u");
        let got = folders(Some(&here), &[location("/home"), here.clone()]);
        assert_eq!(got, vec![PathBuf::from("/home/u"), PathBuf::from("/home")]);
    }

    #[test]
    fn a_virtual_folder_contributes_nothing() {
        assert!(folders(Some(&Location::computer()), &[]).is_empty());
    }

    #[test]
    fn an_archive_contributes_its_container() {
        let inside = Location::Archive {
            archive: PathBuf::from("/d/nested.zip"),
            inner: "deep".to_string(),
        };
        assert_eq!(
            folders(Some(&inside), &[]),
            vec![PathBuf::from("/d/nested.zip")]
        );
    }

    #[test]
    fn with_nothing_on_screen_there_is_nothing_to_watch() {
        assert!(folders(None, &[]).is_empty());
    }
}
