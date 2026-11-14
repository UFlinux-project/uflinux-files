//! What is inside a folder, counted without reading it into a listing.
//!
//! The properties dialog wants a folder's total, and a listing is the wrong
//! tool: a listing is sorted, filtered and delivered to a widget, and opening a
//! dialog should not replace what is on screen or double the memory a folder
//! costs. This is the same walk [`crate::local::LocalReader`] does — one
//! `read_dir` per directory, `lstat` per entry, symlinks never followed — but
//! it keeps only three numbers instead of an [`Entry`] per file.
//!
//! The walk is bounded three ways, because it runs while the window stays
//! responsive and a folder of a million files must still cost a bounded amount
//! of time and none of the heap: a depth cap against a tree that is deeper than
//! it is wide, an entry cap against a folder that is wider than anything, and a
//! cancellation flag for the user, who may close the dialog mid-walk. When a
//! bound stops the walk, [`Tally::partial`] says so and the dialog says "at
//! least" rather than quoting a number that is quietly wrong.

use std::path::Path;

use crate::Cancel;

/// How deep the walk goes. Past this, a folder's contents are counted but not
/// entered — a reasonable tree is far shallower, and the pathological ones
/// (a backup of a `node_modules`, a maildir of archives) are exactly what the
/// cap is for.
pub const MAX_DEPTH: usize = 32;

/// How many entries the walk reads before stopping. Generous enough that an
/// ordinary home folder is always counted whole, small enough that a walk
/// across a whole filesystem costs seconds rather than minutes.
pub const MAX_ENTRIES: u64 = 1_000_000;

/// What a walk found out about one folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    /// Bytes of the regular files below the root, counting the root itself
    /// when it is a file. Apparent sizes, like a listing shows — not blocks.
    pub bytes: u64,
    /// Files counted, including the root file.
    pub files: u64,
    /// Directories entered, excluding the root.
    pub folders: u64,
    /// Entries the walk could not read: a `read_dir` that failed, a metadata
    /// call that failed, a name that was not valid UTF-8. Reported rather than
    /// swallowed, because a total that quietly omits a folder nobody could
    /// open is a different claim from one that omits nothing.
    pub unreadable: u64,
    /// Whether a bound — depth, the entry cap, or cancellation — stopped the
    /// walk before every directory had been entered. A partial total is still
    /// a lower bound worth showing, with the words to match.
    pub partial: bool,
}

impl Tally {
    /// Whether nothing at all was counted, which the dialog reports as "empty
    /// or unreadable" rather than as a zero-byte folder.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes == 0 && self.files == 0 && self.folders == 0
    }
}

/// Walk `root` and total what is inside it.
///
/// Symlinks are never followed, which is both what a listing shows and the only
/// way the walk terminates on a tree containing a link back to one of its own
/// folders. A symlinked file counts as its link size, not the target's.
///
/// `cancel` is polled once per directory, which is coarse on purpose: a folder
/// of one enormous directory would notice a cancel one `read_dir` late, and
/// checking per file would cost an atomic per file for no visible gain.
#[must_use]
pub fn total(root: &Path, cancel: &Cancel) -> Tally {
    let mut tally = Tally::default();
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        tally.partial = true;
        tally.unreadable = 1;
        return tally;
    };
    if !meta.is_dir() {
        tally.bytes = meta.len();
        tally.files = 1;
        return tally;
    }
    // One stack of directories still to enter, seeded with the root at depth 0.
    // A Vec rather than recursion: a tree deep enough to overflow the stack is
    // exactly the input this walk promises to survive.
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if cancel.is_cancelled() {
            tally.partial = true;
            return tally;
        }
        if depth >= MAX_DEPTH {
            tally.partial = true;
            continue;
        }
        let Ok(children) = std::fs::read_dir(&dir) else {
            tally.unreadable += 1;
            continue;
        };
        for child in children.flatten() {
            if tally.files + tally.folders >= MAX_ENTRIES {
                tally.partial = true;
                return tally;
            }
            let Ok(meta) = child.metadata() else {
                tally.unreadable += 1;
                continue;
            };
            if meta.is_dir() {
                tally.folders += 1;
                stack.push((child.path(), depth + 1));
            } else {
                tally.bytes = tally.bytes.saturating_add(meta.len());
                tally.files += 1;
            }
        }
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Tree(tempfile::TempDir);

    impl Tree {
        /// `tree/a/b/leaf.txt` plus a file at the top, the shape the caps and
        /// the counts are checked against.
        fn sample() -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let root = dir.path();
            fs::create_dir_all(root.join("a/b")).expect("nested dirs");
            fs::write(root.join("top.txt"), b"12345").expect("top file");
            fs::write(root.join("a/b/leaf.txt"), b"67890").expect("leaf file");
            Self(dir)
        }
    }

    fn live() -> Cancel {
        Cancel::new()
    }

    #[test]
    fn a_folder_counts_every_file_below_it() {
        let tree = Tree::sample();
        let tally = total(tree.0.path(), &live());
        assert_eq!(tally.bytes, 10, "both files, wherever they sit");
        assert_eq!(tally.files, 2);
        assert_eq!(tally.folders, 2, "a and a/b, not the root");
        assert_eq!(tally.unreadable, 0);
        assert!(!tally.partial, "a small tree is walked whole");
        assert!(!tally.is_empty());
    }

    #[test]
    fn an_empty_folder_totals_zero_without_claiming_to_be_partial() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let tally = total(dir.path(), &live());
        assert!(tally.is_empty());
        assert!(!tally.partial, "nothing to count is not the same as a cap");
        assert_eq!(tally.unreadable, 0);
    }

    #[test]
    fn a_file_root_reports_the_file() {
        let tree = Tree::sample();
        let tally = total(&tree.0.path().join("top.txt"), &live());
        assert_eq!(tally.bytes, 5);
        assert_eq!(tally.files, 1);
        assert_eq!(tally.folders, 0);
    }

    #[test]
    fn a_root_that_cannot_be_read_is_partial_and_says_so() {
        let tally = total(Path::new("/definitely/not/here"), &live());
        assert!(tally.partial);
        assert_eq!(tally.unreadable, 1);
        assert!(tally.is_empty());
    }

    #[test]
    fn a_cancelled_walk_stops_and_reports_a_lower_bound() {
        let tree = Tree::sample();
        let cancel = live();
        cancel.cancel();
        let tally = total(tree.0.path(), &cancel);
        assert!(tally.partial, "a walk that never ran counted nothing");
        assert!(tally.is_empty());
    }

    #[test]
    fn a_symlink_is_counted_and_never_followed() {
        let tree = Tree::sample();
        let root = tree.0.path();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a"), root.join("loop"))
            .expect("a symlink to a folder above it");
        let tally = total(root, &live());
        // The link counts as a file — its own size, not the folder's — and the
        // walk neither loops nor counts `a` twice.
        assert_eq!(tally.files, 3, "two real files and one link");
        assert_eq!(tally.folders, 2, "the link did not open as a folder");
        assert!(!tally.partial);
    }

    #[test]
    fn a_tree_deeper_than_the_cap_stops_counting_and_says_at_least() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut deep = dir.path().to_path_buf();
        for _ in 0..=MAX_DEPTH {
            deep.push("d");
        }
        fs::create_dir_all(&deep).expect("a deep chain");
        fs::write(deep.join("bottom.txt"), b"x").expect("a file at the bottom");
        let tally = total(dir.path(), &live());
        assert!(tally.partial, "the depth cap stopped the walk");
        assert_eq!(tally.bytes, 0, "the file below the cap was not counted");
    }
}
