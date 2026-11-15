//! The row model shared by all three views.
//!
//! One [`Row`] per directory entry, held in a [`gio::ListStore`], is the only
//! per-file object the interface keeps. A `Row` is one [`Entry`] plus the two
//! strings a widget might ask for, computed on demand and not cached: a
//! 20 000-entry folder is the whole budget of the directory cache
//! (`ufm_vfs::DirCache`) and the view must not double it. `GtkGridView` and
//! `GtkColumnView` build widgets only for visible rows, so the widget cost
//! stays with the viewport while the model cost stays with the directory.
//!
//! Rows are immutable once shown. A change anywhere is a change to the listing,
//! which replaces rows rather than mutating them, so no per-row change signal
//! exists and a factory never has to unsubscribe from one. The single exception
//! is [`Row::restamp`], which updates the one flag the filesystem does not know
//! about and is always followed by a rebind rather than by a signal.

use glib::subclass::prelude::*;
use std::cell::RefCell;

use ufm_core::entry::{Entry, EntryFlags, EntryKind};

mod imp {
    // The subclass prelude is a set of traits meant to be globbed; naming them
    // one by one would tie this file to glib's internal split of the module.
    #![allow(clippy::wildcard_imports)]
    use super::*;

    #[derive(Default)]
    pub struct RowPriv {
        pub(crate) entry: RefCell<Option<Entry>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowPriv {
        const NAME: &'static str = "UfmRow";
        type Type = super::Row;
    }

    impl ObjectImpl for RowPriv {}
}

glib::wrapper! {
    /// One line in a folder, or one row of a virtual panel.
    pub struct Row(ObjectSubclass<imp::RowPriv>);
}

impl Row {
    #[must_use]
    pub fn new(entry: Entry) -> Self {
        let row: Self = glib::Object::builder().build();
        row.imp().entry.replace(Some(entry));
        row
    }

    /// Read the entry without copying its name, mime, or path. The factory
    /// binds rows by the hundred on a large folder and a clone each is a
    /// malloc each.
    #[must_use]
    pub fn with<T>(&self, f: impl FnOnce(&Entry) -> T) -> T {
        let borrowed = self.imp().entry.borrow();
        f(borrowed.as_ref().expect("a row always holds an entry"))
    }

    #[must_use]
    pub fn entry(&self) -> Entry {
        self.with(Entry::clone)
    }

    /// Re-apply the star flag from `starred`, so a row can follow a change to the
    /// list without being thrown away and rebuilt.
    ///
    /// Rows are otherwise immutable — see the module documentation — and this is
    /// the one deliberate exception, because a star is not a fact about the file:
    /// the filesystem never had it, so re-reading the folder would not settle it
    /// either. The change is invisible on its own; the caller pairs this with a
    /// rebind, which asks the factories to draw the rows again.
    pub fn restamp(&self, starred: &ufm_core::Starred) {
        let mut borrowed = self.imp().entry.borrow_mut();
        if let Some(entry) = borrowed.as_mut() {
            starred.stamp(std::slice::from_mut(entry));
        }
    }

    #[must_use]
    pub fn name(&self) -> String {
        self.with(|e| e.name.clone())
    }

    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.with(|e| e.kind.is_dir())
    }

    /// The icon to show, honouring the per-entry override the reader set for
    /// things a themed name cannot express: a broken symlink, an archive that
    /// is by mime a plain file, a mount point. The view chooses the pixel size,
    /// not the model, so every mode asks for the same name.
    #[must_use]
    pub fn icon_name(&self) -> String {
        let entry = self.entry();
        if entry.kind == EntryKind::Archive {
            return "package-x-generic".to_string();
        }
        if entry.flags.contains(EntryFlags::BROKEN) {
            return "image-missing".to_string();
        }
        entry.kind.icon_name().to_string()
    }

    /// The secondary column: size for files, item count for folders. Folders
    /// report a size the reader already counted, so an empty string here would
    /// be a lie; `child_count` is `None` only when counting was skipped.
    #[must_use]
    pub fn size_label(&self) -> String {
        let entry = self.entry();
        if entry.kind.is_dir() {
            return entry
                .child_count
                .map_or_else(String::new, |n| format_items(u64::from(n)));
        }
        entry.size_label()
    }

    #[must_use]
    pub fn modified_label(&self) -> String {
        self.entry().modified_label()
    }

    /// What the row is, in the words the kind column shows. The wording lives on
    /// [`Entry::kind_label`] so that this column and the properties dialog cannot
    /// describe the same file differently.
    #[must_use]
    pub fn kind_label(&self) -> String {
        self.entry().kind_label()
    }

    #[must_use]
    pub fn rows(entries: Vec<Entry>) -> Vec<Self> {
        entries.into_iter().map(Self::new).collect()
    }
}

/// "3 items" / "1 item", the phrasing both the list column and the status bar
/// use so they never disagree.
#[must_use]
pub fn format_items(count: u64) -> String {
    match count {
        1 => "1 item".to_string(),
        n => format!("{n} items"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::prelude::*;
    use std::path::PathBuf;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            location: ufm_core::Location::file(PathBuf::from("/tmp").join(name)),
            name: name.to_string(),
            kind,
            flags: EntryFlags::empty(),
            size: 0,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: kind.icon_name().to_string(),
        }
    }

    #[test]
    fn a_row_holds_its_entry_without_copying_on_read() {
        let row = Row::new(entry("notes.txt", EntryKind::File));
        assert_eq!(row.name(), "notes.txt");
        let () = row.with(|e| assert_eq!(e.kind, EntryKind::File));
    }

    #[test]
    fn rows_build_a_list_store() {
        let store = gio::ListStore::new::<Row>();
        store.extend_from_slice(&Row::rows(vec![
            entry("a", EntryKind::Directory),
            entry("b", EntryKind::File),
        ]));
        assert_eq!(store.n_items(), 2);
        let first: Row = store
            .item(0)
            .and_then(|object| object.downcast::<Row>().ok())
            .expect("the first row");
        assert!(first.is_dir());
    }

    #[test]
    fn a_folder_shows_items_and_a_file_shows_bytes() {
        let mut dir = entry("docs", EntryKind::Directory);
        dir.child_count = Some(1);
        assert_eq!(Row::new(dir).size_label(), "1 item");
        let mut dir2 = entry("docs", EntryKind::Directory);
        dir2.child_count = Some(12);
        assert_eq!(Row::new(dir2).size_label(), "12 items");
        let mut dir3 = entry("docs", EntryKind::Directory);
        dir3.child_count = None;
        assert_eq!(Row::new(dir3).size_label(), "");
        let mut file = entry("raw.bin", EntryKind::File);
        file.size = 1024;
        assert_eq!(Row::new(file).size_label(), "1.0 KiB");
    }

    #[test]
    fn an_archive_is_an_archive_even_when_its_mime_says_zip() {
        let mut archive = entry("trip.zip", EntryKind::Archive);
        archive.mime = Some("application/zip".to_string());
        let row = Row::new(archive);
        assert_eq!(row.icon_name(), "package-x-generic");
        assert_eq!(row.kind_label(), "Archive");
    }

    #[test]
    fn a_broken_link_outranks_its_target_icon() {
        let mut link = entry("gone", EntryKind::File);
        link.flags = EntryFlags::SYMLINK | EntryFlags::BROKEN;
        assert_eq!(Row::new(link).icon_name(), "image-missing");
    }

    #[test]
    fn restamping_follows_the_list_in_both_directions() {
        let row = Row::new(entry("here.txt", EntryKind::File));
        let mut list = ufm_core::Starred::default();
        assert!(!row.with(|e| e.flags.contains(EntryFlags::STARRED)));
        list.star(
            row.with(|e| e.location.clone()),
            std::time::SystemTime::now(),
        );
        row.restamp(&list);
        assert!(row.with(|e| e.flags.contains(EntryFlags::STARRED)));
        row.restamp(&ufm_core::Starred::default());
        assert!(!row.with(|e| e.flags.contains(EntryFlags::STARRED)));
        // Restamping leaves the rest of the entry alone.
        assert_eq!(row.name(), "here.txt");
    }
}
