//! Ordering and filtering, kept out of the views so a listing can be sorted on
//! a worker thread before it is published.

use crate::entry::{Entry, EntryFlags, EntryKind};

/// A column the user can sort by. The set matches what the list view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    Name,
    Size,
    Kind,
    Modified,
}

impl SortField {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Kind => "Type",
            Self::Modified => "Date modified",
        }
    }
}

/// A field plus direction, as persisted per folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SortKey {
    pub field: SortField,
    pub descending: bool,
    /// Whether folders are held apart from files rather than mixed in.
    pub folders_first: bool,
}

impl Default for SortKey {
    fn default() -> Self {
        Self {
            field: SortField::Name,
            descending: false,
            folders_first: true,
        }
    }
}

impl SortKey {
    #[must_use]
    pub const fn reversed(self) -> Self {
        Self {
            descending: !self.descending,
            ..self
        }
    }
}

/// Sort in place. Folders are grouped first when the key asks for it, and
/// within a group the order is natural and case-insensitive so that `IMG_2.jpg`
/// precedes `IMG_10.jpg`, which is what a person scanning a photo folder expects.
pub fn sort_entries(entries: &mut [Entry], key: SortKey) {
    entries.sort_by(|a, b| {
        if key.folders_first {
            match (a.kind.is_dir(), b.kind.is_dir()) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
        }
        let ord = match key.field {
            SortField::Name => natural_cmp(
                a.name.trim_start_matches('.'),
                b.name.trim_start_matches('.'),
            ),
            SortField::Size => a.size.cmp(&b.size).then_with(|| name_ord(a, b)),
            SortField::Kind => kind_group(a)
                .cmp(&kind_group(b))
                .then_with(|| {
                    a.mime
                        .as_deref()
                        .unwrap_or("")
                        .cmp(b.mime.as_deref().unwrap_or(""))
                })
                .then_with(|| name_ord(a, b)),
            SortField::Modified => a.modified.cmp(&b.modified).then_with(|| name_ord(a, b)),
        };
        if key.descending { ord.reverse() } else { ord }
    });
}

/// Whether an entry is shown under the current filter.
#[must_use]
pub fn visible(entry: &Entry, show_hidden: bool, query: Option<&str>) -> bool {
    if entry.hidden() && !show_hidden {
        return false;
    }
    match query {
        None | Some("") => true,
        Some(q) => entry.name.to_lowercase().contains(&q.to_lowercase()),
    }
}

/// Drop what the filter rejects, keeping the rest in whatever order it has.
pub fn filter_entries(entries: &mut Vec<Entry>, show_hidden: bool, query: Option<&str>) {
    entries.retain(|e| visible(e, show_hidden, query));
}

/// Case-insensitive compare that splits embedded digits and compares them
/// numerically. Only used for ordering, never for display, so a lossy lowercase
/// is fine.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let (mut i, mut j) = (0, 0);
    while i < ab.len() && j < bb.len() {
        let (ca, cb) = (ab[i].to_ascii_lowercase(), bb[j].to_ascii_lowercase());
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // Skip leading zeros so 007 and 7 sort together, then compare the
            // runs by length and lexically, which avoids parsing a huge number.
            let sa = strip_zeros(&ab[i..]);
            let sb = strip_zeros(&bb[j..]);
            let ea = digits_end(sa);
            let eb = digits_end(sb);
            let na = &sa[..ea];
            let nb = &sb[..eb];
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            i += (ab[i..].len() - sa.len()) + ea;
            j += (bb[j..].len() - sb.len()) + eb;
        } else {
            if ca != cb {
                return ca.cmp(&cb);
            }
            i += 1;
            j += 1;
        }
    }
    (ab.len() - i).cmp(&(bb.len() - j))
}

fn strip_zeros(s: &[u8]) -> &[u8] {
    let mut k = 0;
    while k + 1 < s.len() && s[k] == b'0' {
        k += 1;
    }
    &s[k..]
}

fn digits_end(s: &[u8]) -> usize {
    s.iter()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(s.len())
}

fn name_ord(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    natural_cmp(
        a.name.trim_start_matches('.'),
        b.name.trim_start_matches('.'),
    )
}

/// Coarse type grouping, so sorting by Type puts programs next to programs even
/// though their MIME types differ.
fn kind_group(e: &Entry) -> u8 {
    match e.kind {
        EntryKind::Directory | EntryKind::MountPoint => 0,
        EntryKind::Archive => 1,
        EntryKind::Device | EntryKind::CharDevice | EntryKind::BlockDevice => 5,
        EntryKind::Socket | EntryKind::Fifo => 6,
        EntryKind::File => match e.mime.as_deref() {
            Some(m) if m.starts_with("text/") => 2,
            Some(m) if m.starts_with("image/") => 3,
            Some(m) if m.starts_with("video/") || m.starts_with("audio/") => 4,
            _ => 7,
        },
    }
}

/// Whether the trash can be used here, derived from the flags a reader sets.
#[must_use]
pub fn can_trash(entries: &[&Entry]) -> bool {
    entries
        .iter()
        .all(|e| !e.flags.contains(EntryFlags::NO_TRASH))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryFlags, EntryKind};
    use std::path::PathBuf;

    fn entry(name: &str, dir: bool, size: u64) -> Entry {
        Entry {
            location: crate::Location::file(PathBuf::from("/").join(name)),
            name: name.into(),
            kind: if dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            flags: if name.starts_with('.') {
                EntryFlags::HIDDEN
            } else {
                EntryFlags::empty()
            },
            size,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: if dir { None } else { Some("text/plain".into()) },
            child_count: if dir { Some(0) } else { None },
            icon_key: String::new(),
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn folders_come_first_when_asked() {
        let mut v = vec![
            entry("a.txt", false, 1),
            entry("Dir", true, 0),
            entry("b.txt", false, 2),
        ];
        sort_entries(&mut v, SortKey::default());
        assert_eq!(names(&v), ["Dir", "a.txt", "b.txt"]);
    }

    #[test]
    fn folders_mix_in_when_not_asked() {
        let mut v = vec![entry("b.txt", false, 1), entry("Dir", true, 0)];
        sort_entries(
            &mut v,
            SortKey {
                folders_first: false,
                ..SortKey::default()
            },
        );
        assert_eq!(names(&v), ["b.txt", "Dir"]);
    }

    #[test]
    fn case_does_not_matter() {
        let mut v = vec![entry("beta", false, 0), entry("Alpha", false, 0)];
        sort_entries(&mut v, SortKey::default());
        assert_eq!(names(&v), ["Alpha", "beta"]);
    }

    #[test]
    fn embedded_numbers_sort_numerically() {
        let mut v = vec![
            entry("IMG_10.jpg", false, 0),
            entry("IMG_2.jpg", false, 0),
            entry("IMG_1.jpg", false, 0),
        ];
        sort_entries(&mut v, SortKey::default());
        assert_eq!(names(&v), ["IMG_1.jpg", "IMG_2.jpg", "IMG_10.jpg"]);
    }

    #[test]
    fn leading_zeros_group_together_then_stable_order() {
        let mut v = vec![
            entry("a007", false, 0),
            entry("a7", false, 0),
            entry("a6", false, 0),
        ];
        sort_entries(&mut v, SortKey::default());
        let got = names(&v);
        assert_eq!(&got[2], &"a7", "the largest number must be last");
        assert!(got.contains(&"a007") && got.contains(&"a6"));
    }

    #[test]
    fn long_digit_runs_do_not_overflow() {
        let long = format!("x{}", "9".repeat(40));
        let mut v = vec![entry(&long, false, 0), entry("x10", false, 0)];
        sort_entries(&mut v, SortKey::default());
        assert_eq!(names(&v), ["x10", long.as_str()]);
    }

    #[test]
    fn descending_is_the_exact_reverse() {
        let mut asc = vec![
            entry("a", false, 1),
            entry("b", false, 2),
            entry("c", false, 0),
        ];
        sort_entries(&mut asc, SortKey::default());
        let mut desc = asc.clone();
        sort_entries(&mut desc, SortKey::default().reversed());
        assert_eq!(names(&desc), ["c", "b", "a"]);
    }

    #[test]
    fn size_sort_ties_fall_back_to_name() {
        let mut v = vec![
            entry("z", false, 5),
            entry("a", false, 5),
            entry("m", false, 9),
        ];
        sort_entries(
            &mut v,
            SortKey {
                field: SortField::Size,
                ..SortKey::default()
            },
        );
        assert_eq!(names(&v), ["a", "z", "m"]);
    }

    #[test]
    fn hidden_filtering_and_substring_query() {
        let dot = entry(".hidden", false, 0);
        let plain = entry("Report.PDF", false, 0);
        assert!(!visible(&dot, false, None));
        assert!(visible(&dot, true, None));
        assert!(visible(&plain, false, Some("report")));
        assert!(visible(&plain, false, Some("PDF")));
        assert!(!visible(&plain, false, Some("docx")));
        assert!(visible(&plain, false, Some("")));
    }

    #[test]
    fn filter_drops_only_what_it_should() {
        let mut v = vec![
            entry(".a", false, 0),
            entry("b", false, 0),
            entry("c", false, 0),
        ];
        filter_entries(&mut v, false, Some("c"));
        assert_eq!(names(&v), ["c"]);
    }
}
