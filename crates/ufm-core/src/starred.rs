//! Locations the user starred, in their own file.
//!
//! A star is a fact the user adds to a row that the filesystem does not store.
//! It could have gone into [`crate::config::Preferences`], and does not, for the
//! same reason bookmarks have their own file: the two grow without bound, are
//! written from wherever a row is selected rather than from a preferences
//! dialog, and rewriting the whole preference set to remember one star means a
//! crash can cost the user their view mode and column widths as well.
//!
//! The list is the collection behind `starred://`, and it is deliberately a list
//! of [`Location`] rather than of paths. A starred archive member and a starred
//! folder on a mounted share are both things the user pointed at, and both have
//! a location that says where; storing a `PathBuf` would mean a star on
//! `film.zip!/poster.png` could not be remembered at all.
//!
//! # What a star does not mean
//!
//! Nothing on disk changes, so a star outlives the thing it was put on. Rows
//! whose location has gone are kept rather than pruned: a share that is simply
//! not mounted right now is not a reason to forget what the user asked to keep
//! handy, and the view says how many are unavailable rather than quietly
//! dropping them. [`Starred::prune`] is the one place that forgets, and the
//! interface calls it only when the user deletes the row itself.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::entry::{Entry, EntryFlags};
use crate::error::Result;
use crate::location::Location;

/// Current on-disk shape. Bump when a change cannot be absorbed by `#[serde(default)]`.
pub const VERSION: u32 = 1;

/// One starred location, with the moment it was starred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Star {
    pub location: Location,
    /// Seconds since the epoch. Whole seconds rather than a [`SystemTime`]: the
    /// value is only ever compared for ordering and shown as a date, and the
    /// default serialisation of a `SystemTime` is a two-field struct that no
    /// other program would want to read.
    pub added: i64,
}

impl Star {
    #[must_use]
    pub fn added_at(&self) -> SystemTime {
        let secs = u64::try_from(self.added).unwrap_or(0);
        UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }
}

/// The user's stars, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Starred {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub items: Vec<Star>,
}

impl Starred {
    #[must_use]
    pub fn path() -> PathBuf {
        crate::paths::config_dir().join("starred.json")
    }

    /// Read the list. A missing file is an empty list; a file that exists and
    /// does not parse is an error, so a truncated write is visible rather than a
    /// silent loss of every star on the machine.
    pub fn load(path: &Path) -> Result<Self> {
        atomic::read_json::<Self>(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic::write_json(path, self)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The stars in the order shown: most recent first.
    #[must_use]
    pub fn stars(&self) -> &[Star] {
        &self.items
    }

    /// Whether `location` carries a star.
    ///
    /// Compared on the value rather than on the URI both locations would write:
    /// the two answers agree — `Location`'s equality is structural and its
    /// serialisation is canonical — but the comparison costs no percent-encoding,
    /// which matters because [`Starred::stamp`] runs it against every row of a
    /// folder before the folder is drawn.
    #[must_use]
    pub fn contains(&self, location: &Location) -> bool {
        self.items.iter().any(|s| &s.location == location)
    }

    /// Put a star on `location`, reporting whether that changed anything.
    ///
    /// Starring something already starred is not an error and not a re-order:
    /// the star stays where the list has it, so a row does not jump to the top of
    /// the collection because the action was triggered twice.
    pub fn star(&mut self, location: Location, when: SystemTime) -> bool {
        if self.contains(&location) {
            return false;
        }
        self.items.insert(
            0,
            Star {
                location,
                added: epoch_seconds(when),
            },
        );
        true
    }

    /// Take `location`'s star away, reporting whether there was one.
    pub fn unstar(&mut self, location: &Location) -> bool {
        let uri = location.to_string();
        let before = self.items.len();
        self.items.retain(|s| s.location.to_string() != uri);
        self.items.len() != before
    }

    /// Star or unstar, reporting the state the location is in afterwards — which
    /// is what a menu label and a status line both need, and what makes the two
    /// separate commands agree with each other.
    pub fn toggle(&mut self, location: Location, when: SystemTime) -> bool {
        if self.contains(&location) {
            self.unstar(&location);
            false
        } else {
            self.star(location, when);
            true
        }
    }

    /// The locations that no longer resolve, given a way to ask.
    ///
    /// The view owns the question — a share that is not mounted right now is not
    /// a missing row, and only the window knows what is mounted — so this takes
    /// the answer rather than working it out. What it gives back is the list the
    /// collection shows as unavailable, in the order the stars are listed.
    #[must_use]
    pub fn missing(&self, available: impl Fn(&Location) -> bool) -> Vec<Location> {
        self.items
            .iter()
            .map(|star| star.location.clone())
            .filter(|location| !available(location))
            .collect()
    }

    /// Set or clear [`EntryFlags::STARRED`] on the entries, according to the list.
    ///
    /// The listing a folder read produced knows nothing about stars — the
    /// filesystem has no room for them — so the view says which rows have one
    /// before drawing. Done in place and over the whole listing at once because
    /// the alternative is the view asking per row, which is one lookup per
    /// visible cell on every rebind rather than one pass per read.
    ///
    /// Clearing matters as much as setting: the same listing is stamped again
    /// when a star is taken away, and a flag left behind would keep drawing a
    /// badge on a row that no longer has one.
    pub fn stamp(&self, entries: &mut [Entry]) {
        for entry in entries {
            entry
                .flags
                .set(EntryFlags::STARRED, self.contains(&entry.location));
        }
    }

    /// Forget the stars on exactly these locations, reporting how many went.
    ///
    /// The delete path's half of [`Starred::prune`]: an operation that removed
    /// rows knows exactly which ones, and a star on a row that is gone for good
    /// describes something no longer there to unstar. Rows that were only moved
    /// out of sight — to the trash, where they can come back — are not this
    /// list's business.
    pub fn forget(&mut self, removed: &[Location]) -> usize {
        if removed.is_empty() || self.items.is_empty() {
            return 0;
        }
        let gone: std::collections::HashSet<&Location> = removed.iter().collect();
        let before = self.items.len();
        self.items.retain(|star| !gone.contains(&star.location));
        before - self.items.len()
    }

    /// Forget every star whose location fails `keep`.
    ///
    /// Used when a row is deleted, not when it is merely missing — see the module
    /// documentation for why missing is not the same thing.
    pub fn prune(&mut self, keep: impl Fn(&Location) -> bool) -> usize {
        let before = self.items.len();
        self.items.retain(|s| keep(&s.location));
        before - self.items.len()
    }

    /// Forget every star pointing into `root` or at it, for the delete and trash
    /// of a folder that holds starred rows inside it.
    ///
    /// [`Location::is_within`] rather than a path prefix, so that a star inside an
    /// archive is covered by deleting the archive: `film.zip!/a/b` is under
    /// `film.zip`, and a string prefix would also match a sibling called
    /// `film.zip.old`.
    pub fn prune_under(&mut self, root: &Location) -> usize {
        self.prune(|location| !location.is_within(root))
    }
}

fn epoch_seconds(when: SystemTime) -> i64 {
    match when.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        // A clock behind 1970: saturating keeps the ordering sane rather than
        // storing a negative age that sorts as the oldest star there is.
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn at(path: &str) -> Location {
        Location::file(PathBuf::from(path))
    }

    fn when(secs: u64) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    /// A row as a directory read would hand it over: no flags set, the kind's
    /// own icon, and a location built from the path.
    fn entry(path: &str) -> Entry {
        Entry {
            location: at(path),
            name: path.rsplit('/').next().unwrap_or_default().to_string(),
            kind: EntryKind::File,
            flags: EntryFlags::empty(),
            size: 1,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: EntryKind::File.icon_name().to_string(),
        }
    }

    fn file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ufm-starred-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir.join("starred.json")
    }

    #[test]
    fn a_missing_file_is_an_empty_list_not_an_error() {
        let path = file("missing");
        assert!(Starred::load(&path).expect("no file").is_empty());
    }

    #[test]
    fn a_truncated_file_is_an_error_rather_than_a_forget() {
        let path = file("truncated");
        std::fs::write(&path, "{\"version\": 1, \"item").expect("write");
        assert!(Starred::load(&path).is_err());
    }

    #[test]
    fn starring_twice_changes_nothing_and_reorders_nothing() {
        let mut list = Starred::default();
        assert!(list.star(at("/home/u/a"), when(100)));
        list.star(at("/home/u/b"), when(200));
        // The second star goes on top, so the list reads newest first.
        assert_eq!(list.stars()[0].location, at("/home/u/b"));
        // Re-starring the first is not a change, and must not move it.
        assert!(!list.star(at("/home/u/a"), when(300)));
        assert_eq!(list.len(), 2);
        assert_eq!(list.stars()[1].location, at("/home/u/a"));
        assert_eq!(list.stars()[1].added, 100, "the original moment survives");
    }

    #[test]
    fn unstar_reports_whether_there_was_anything_to_do() {
        let mut list = Starred::default();
        list.star(at("/home/u/a"), when(1));
        assert!(list.unstar(&at("/home/u/a")));
        assert!(!list.unstar(&at("/home/u/a")));
        assert!(list.is_empty());
    }

    #[test]
    fn toggle_reports_the_state_afterwards() {
        let mut list = Starred::default();
        assert!(list.toggle(at("/home/u/a"), when(1)), "now starred");
        assert!(!list.toggle(at("/home/u/a"), when(2)), "now not");
        assert!(list.is_empty());
    }

    #[test]
    fn an_archive_member_and_its_container_are_different_stars() {
        let container = at("/home/u/film.zip");
        let member = Location::archive("/home/u/film.zip", &["poster.png"]).expect("member");
        let mut list = Starred::default();
        list.star(member.clone(), when(1));
        assert!(list.contains(&member));
        assert!(
            !list.contains(&container),
            "a star on a member is not a star on the archive"
        );
    }

    #[test]
    fn a_star_inside_a_folder_goes_when_the_folder_is_deleted() {
        let mut list = Starred::default();
        list.star(at("/home/u/docs/notes.txt"), when(1));
        list.star(at("/home/u/docs.old/notes.txt"), when(1));
        assert_eq!(
            list.prune_under(&at("/home/u/docs")),
            1,
            "a sibling whose path merely starts with the same text is not inside it"
        );
        assert_eq!(list.stars()[0].location, at("/home/u/docs.old/notes.txt"));
    }

    #[test]
    fn deleting_an_archive_takes_the_star_on_its_member() {
        let mut list = Starred::default();
        list.star(
            Location::archive("/home/u/film.zip", &["a", "b"]).expect("member"),
            when(1),
        );
        assert_eq!(list.prune_under(&at("/home/u/film.zip")), 1);
        assert!(list.is_empty());
    }

    #[test]
    fn a_list_survives_a_round_trip_and_keeps_its_order() {
        let path = file("round-trip");
        let mut list = Starred {
            version: VERSION,
            items: Vec::new(),
        };
        list.star(at("/home/u/b"), when(200));
        list.star(at("/home/u/a"), when(100));
        list.save(&path).expect("save");
        let back = Starred::load(&path).expect("load");
        assert_eq!(back, list);
        assert_eq!(back.stars()[0].location, at("/home/u/a"));
    }

    #[test]
    fn a_file_written_by_a_newer_build_still_reads() {
        let path = file("newer");
        std::fs::write(
            &path,
            "{\"version\": 7, \"items\": [{\"location\": \"file:///home/u/a\", \
             \"added\": 5, \"colour\": \"red\"}]}\n",
        )
        .expect("write");
        let list = Starred::load(&path).expect("load");
        assert_eq!(list.len(), 1);
        assert_eq!(list.stars()[0].location, at("/home/u/a"));
    }

    #[test]
    fn the_missing_ones_are_the_ones_the_caller_cannot_find() {
        let mut list = Starred::default();
        list.star(at("/home/u/gone"), when(1));
        list.star(at("/home/u/here"), when(2));
        list.star(at("/mnt/share/off"), when(3));
        let gone = list.missing(|location| location.to_string().contains("here"));
        assert_eq!(
            gone,
            vec![at("/mnt/share/off"), at("/home/u/gone")],
            "in the order listed, newest first"
        );
    }

    #[test]
    fn forgetting_takes_exactly_the_rows_that_were_removed() {
        let mut list = Starred::default();
        list.star(at("/home/u/gone"), when(1));
        list.star(at("/home/u/here"), when(2));
        let taken = list.forget(&[at("/home/u/gone"), at("/home/u/nowhere")]);
        assert_eq!(taken, 1, "one star went, and one name matched nothing");
        assert_eq!(list.len(), 1);
        assert!(list.contains(&at("/home/u/here")));
        assert_eq!(Starred::default().forget(&[at("/home/u/here")]), 0);
    }

    #[test]
    fn stamping_marks_the_starred_rows_and_leaves_the_rest() {
        let mut list = Starred::default();
        list.star(at("/home/u/b"), when(1));
        let mut entries = vec![entry("/home/u/a"), entry("/home/u/b")];
        list.stamp(&mut entries);
        assert!(
            !entries[0].flags.contains(EntryFlags::STARRED),
            "an unstarred row keeps the flags the read gave it"
        );
        assert!(entries[1].flags.contains(EntryFlags::STARRED));
        // Stamping an empty list clears rather than leaves alone, which is the
        // other half of what a re-stamp after an unstar needs.
        let mut untouched = vec![entry("/home/u/a")];
        untouched[0].flags = EntryFlags::STARRED;
        Starred::default().stamp(&mut untouched);
        assert_eq!(untouched[0].flags, EntryFlags::empty());
    }

    #[test]
    fn unstarring_takes_the_flag_back_off_a_listing_already_stamped() {
        let mut list = Starred::default();
        list.star(at("/home/u/a"), when(1));
        list.star(at("/home/u/b"), when(2));
        let mut entries = vec![entry("/home/u/a"), entry("/home/u/b")];
        list.stamp(&mut entries);
        assert!(
            entries
                .iter()
                .all(|e| e.flags.contains(EntryFlags::STARRED))
        );
        list.unstar(&at("/home/u/b"));
        list.stamp(&mut entries);
        assert!(entries[0].flags.contains(EntryFlags::STARRED));
        assert_eq!(
            entries[1].flags,
            EntryFlags::empty(),
            "a row whose star went away stops claiming one"
        );
    }

    #[test]
    fn a_star_from_before_1970_reads_as_the_epoch() {
        let star = Star {
            location: at("/home/u/a"),
            added: -5,
        };
        assert_eq!(star.added_at(), UNIX_EPOCH);
    }
}
