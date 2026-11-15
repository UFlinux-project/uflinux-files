//! The Recent view: the files this session's own history says were used.
//!
//! `recent://` has no directory behind it either, but unlike `computer://` it is
//! a listing, so it goes into the ordinary view rather than a presentation of its
//! own. What it lists is the `XBEL` file at `$XDG_DATA_HOME/recently-used.xbel`,
//! which is the shared desktop-wide history every `GIO`-based file manager reads
//! and writes.
//!
//! The file is parsed by hand rather than through `GIO`. `gio::AppInfo::add_recent_uri`
//! and `gio::MountOperation` would be the cheap way in, but the file manager's
//! stance everywhere else is that it reads the plain file itself — the mount table,
//! the preferences, the undo sidecars — and a `GKeyFile`-shaped XML reader bought
//! nothing here that a scan for `bookmark` elements does not. The rules worth
//! testing are the ones a test can run without a display: which records count,
//! which are dropped, and in what order the rest arrive.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ufm_core::Location;

/// How many rows the view shows. The history file keeps hundreds of years of
/// them, and a list nobody scrolls to the end of is not a list of recent files.
const LIMIT: usize = 100;

/// One usable record: where the file is, and when it was last added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recent {
    pub path: PathBuf,
    pub added: SystemTime,
}

/// Read the history file, newest first, missing files dropped.
///
/// A file that has gone away is skipped rather than shown greyed out: the view is
/// a list of things to open, and a row that cannot be opened is worse than no row.
#[must_use]
pub fn load() -> Vec<Recent> {
    let path = ufm_core::paths::data_root().join("recently-used.xbel");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse(&text)
}

/// Every `file:` record in an `XBEL` document, newest first, at most [`LIMIT`] of them.
///
/// Pure over the text so the interesting rules — a record for a share that is not
/// mounted, a record written twice, a record whose timestamp is missing — can be
/// checked without a desktop to write the file.
#[must_use]
pub fn parse(text: &str) -> Vec<Recent> {
    let mut found: Vec<Recent> = Vec::new();
    for element in xml_elements(text) {
        if !element.is_bookmark {
            continue;
        }
        let Some(href) = element.attribute("href") else {
            continue;
        };
        // `Location::parse` is the same rule the address bar uses: a `file:` URI
        // becomes a path, percent escapes included, and anything else — an `http:`
        // bookmark, a share that is not mounted — is refused rather than guessed at.
        let Ok(Location::File { path }) = Location::parse(href) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        let added = element
            .attribute("added")
            .or_else(|| element.attribute("modified"))
            .and_then(parse_time)
            .unwrap_or(UNIX_EPOCH);
        match found.iter_mut().find(|recent| recent.path == path) {
            // The same file written twice — opened, then opened again — is one row,
            // and the newer of the two is the one that says how recent it is.
            Some(existing) => {
                if added > existing.added {
                    existing.added = added;
                }
            }
            None => found.push(Recent { path, added }),
        }
    }
    found.sort_by_key(|record| std::cmp::Reverse(record.added));
    found.truncate(LIMIT);
    found
}

/// One `XBEL` timestamp: `2024-05-01T10:20:30+02:00`, or the same with a `Z`.
///
/// Hand-rolled because the format is fixed and narrow, and pulling in a date
/// crate for one attribute of one file would put a parser in the binary that
/// nothing else uses. A timestamp that does not fit is no timestamp: the record
/// sorts last rather than being thrown away.
fn parse_time(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    // `YYYY-MM-DDThh:mm:ss` is nineteen characters; anything shorter cannot be
    // one, and the separators are checked before the digits so a string of the
    // right length but the wrong shape is rejected rather than half-parsed.
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&bytes[range]).ok()?.parse().ok()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let offset = offset_seconds(&text[19..]);
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// The `+02:00` / `Z` a timestamp ends with, in seconds. Missing means UTC.
fn offset_seconds(tail: &str) -> i64 {
    let sign = match tail.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return 0,
    };
    let rest = &tail[1..];
    let (hours, minutes) = rest.split_once(':').unwrap_or((rest, "0"));
    let hours: i64 = hours.parse().unwrap_or(0);
    let minutes: i64 = minutes.get(0..2).and_then(|m| m.parse().ok()).unwrap_or(0);
    sign * (hours * 3_600 + minutes * 60)
}

/// Days since the epoch for a civil date, after Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_shift = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_shift + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// One opening tag worth looking at, and the attributes it carries.
struct Element<'a> {
    is_bookmark: bool,
    tag: &'a str,
}

impl<'a> Element<'a> {
    /// An attribute's value, taken from the tag's text by name.
    ///
    /// `XBEL` quotes every attribute and never escapes a quote inside one that
    /// matters here — a path with a `"` in it is rare enough, and a record that
    /// mis-parses is a record that is skipped, which is the same outcome as a
    /// file that has gone away.
    fn attribute(&self, name: &str) -> Option<&'a str> {
        let prefix = format!("{name}=\"");
        let at = self.tag.find(&prefix)? + prefix.len();
        let rest = &self.tag[at..];
        let end = rest.find('"')?;
        Some(&rest[..end])
    }
}

/// The opening tags of a document, in order, without descending into them.
///
/// Only the tag's own text is kept, and only long enough to ask it for two
/// attributes, so this is a scan rather than a tree.
fn xml_elements(text: &str) -> Vec<Element<'_>> {
    let mut elements = Vec::new();
    let mut at = 0;
    while let Some(open) = text[at..].find('<') {
        let start = at + open;
        let Some(close) = text[start..].find('>') else {
            break;
        };
        let tag = &text[start..=start + close];
        // The name runs from the `<` to the first space or slash. A processing
        // instruction (`<?…`), a comment (`<!…`) and a closing tag (`</…`) all
        // name nothing and are skipped.
        let name = &tag[1..];
        let end = name
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(name.len());
        let name = &name[..end];
        if !name.starts_with(['?', '!', '/']) {
            elements.push(Element {
                is_bookmark: name == "bookmark",
                tag,
            });
        }
        at = start + close + 1;
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that change the process environment have to hold this, because
    /// `set_var` is unsafe in edition 2024 precisely because another thread may
    /// be reading the environment at the same time.
    fn env() -> std::sync::MutexGuard<'static, ()> {
        super::super::test_env()
    }

    #[allow(unsafe_code)]
    fn set_env(key: &str, value: &str) {
        // SAFETY: the guard means no other test in this binary is reading the
        // environment while this happens.
        unsafe { std::env::set_var(key, value) };
    }

    #[allow(unsafe_code)]
    fn unset_env(key: &str) {
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A record for one file that exists, with the attributes a real history file
    /// carries. `dir` keeps the files, because a record whose file has gone away is
    /// dropped and a test that forgot to create one would silently test nothing.
    fn bookmark(dir: &tempfile::TempDir, name: &str, added: &str) -> (PathBuf, String) {
        let path = dir.path().join(name);
        std::fs::write(&path, b"x").unwrap();
        let href = format!("file://{}", path.display());
        let stamp = if added.is_empty() {
            String::new()
        } else {
            format!(" added=\"{added}\"")
        };
        (path, format!(r#"<bookmark href="{href}"{stamp}/>"#))
    }

    fn doc(records: &[String]) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0" xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks">
{}
</xbel>"#,
            records.join("\n")
        )
    }

    #[test]
    fn a_local_record_becomes_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let (path, record) = bookmark(&dir, "note.txt", "2024-05-01T10:20:30+00:00");
        let found = parse(&doc(&[record]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, path);
        assert_eq!(found[0].added, at(1_714_558_830));
    }

    #[test]
    fn a_utc_offset_is_subtracted() {
        let dir = tempfile::tempdir().unwrap();
        // The same wall-clock instant written by a machine two hours east: the
        // record is two hours older, and a list sorted the other way would show a
        // file opened in Tokyo before one opened here an hour ago.
        let (_, record) = bookmark(&dir, "note.txt", "2024-05-01T12:20:30+02:00");
        let found = parse(&doc(&[record]));
        assert_eq!(found[0].added, at(1_714_558_830));
    }

    #[test]
    fn only_local_files_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (_, local) = bookmark(&dir, "note.txt", "2024-05-01T10:20:30Z");
        // A bookmark for a page, and one for a share that is not mounted. Both are
        // records the desktop kept; neither is a file this view can open.
        let remote = r#"<bookmark href="https://example.org/" added="2024-05-01T10:20:30Z"/>"#;
        let share = r#"<bookmark href="smb://nas/filmy" added="2024-05-01T10:20:30Z"/>"#;
        let found = parse(&doc(&[local, remote.to_string(), share.to_string()]));
        assert_eq!(found.len(), 1, "one local record, two refusals");
    }

    #[test]
    fn a_file_that_is_gone_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("was-here.txt");
        let text = doc(&[format!(
            r#"<bookmark href="file://{}" added="2024-05-01T10:20:30Z"/>"#,
            gone.display()
        )]);
        assert!(
            parse(&text).is_empty(),
            "a row that cannot open is worse than no row"
        );
    }

    #[test]
    fn percent_escapes_are_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not e.txt");
        std::fs::write(&path, b"x").unwrap();
        let text = doc(&[format!(
            r#"<bookmark href="file://{}"/>"#,
            path.display().to_string().replace(' ', "%20")
        )]);
        let found = parse(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, path);
    }

    #[test]
    fn newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let (old, a) = bookmark(&dir, "old.txt", "2024-01-01T00:00:00Z");
        let (new, b) = bookmark(&dir, "new.txt", "2024-06-01T00:00:00Z");
        let found = parse(&doc(&[a, b]));
        assert_eq!(
            found.iter().map(|r| r.path.as_path()).collect::<Vec<_>>(),
            [new.as_path(), old.as_path()]
        );
    }

    #[test]
    fn a_file_written_twice_is_one_row_with_the_newer_time() {
        let dir = tempfile::tempdir().unwrap();
        let (path, a) = bookmark(&dir, "twice.txt", "2024-06-01T00:00:00Z");
        let (_, b) = bookmark(&dir, "twice.txt", "2024-01-01T00:00:00Z");
        let found = parse(&doc(&[a, b]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, path);
        assert_eq!(
            found[0].added,
            at(1_717_200_000),
            "the newer of the two wins"
        );
    }

    #[test]
    fn modified_is_used_when_added_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.txt");
        std::fs::write(&path, b"x").unwrap();
        let text = doc(&[format!(
            r#"<bookmark href="file://{}" modified="2024-05-01T00:00:00Z"/>"#,
            path.display()
        )]);
        let found = parse(&text);
        assert_eq!(found[0].added, at(1_714_521_600));
    }

    #[test]
    fn a_record_without_a_usable_time_sorts_last_rather_than_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        let (_, junk) = bookmark(&dir, "junk.txt", "yesterday");
        let (_, none) = bookmark(&dir, "none.txt", "");
        let (_, good) = bookmark(&dir, "good.txt", "2024-05-01T00:00:00Z");
        let found = parse(&doc(&[junk, none, good]));
        assert_eq!(found.len(), 3, "a bad timestamp is not a bad record");
        assert_eq!(found[0].added, at(1_714_521_600));
        // Both fall back to the epoch, so they tie and only their order among
        // themselves is unspecified.
        assert_eq!(found[1].added, UNIX_EPOCH);
        assert_eq!(found[2].added, UNIX_EPOCH);
    }

    #[test]
    fn the_list_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let records: Vec<String> = (0..(LIMIT + 20))
            .map(|i| {
                let (_, record) = bookmark(&dir, &format!("f{i}.txt"), "2024-05-01T00:00:00Z");
                record
            })
            .collect();
        assert_eq!(parse(&doc(&records)).len(), LIMIT);
    }

    #[test]
    fn a_history_file_is_read_from_the_data_directory() {
        let _guard = env();
        let dir = tempfile::tempdir().unwrap();
        let (_, record) = bookmark(&dir, "note.txt", "2024-05-01T00:00:00Z");
        std::fs::write(dir.path().join("recently-used.xbel"), doc(&[record])).unwrap();
        set_env("XDG_DATA_HOME", &dir.path().display().to_string());
        let found = load();
        unset_env("XDG_DATA_HOME");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_missing_history_file_is_an_empty_list() {
        let _guard = env();
        let dir = tempfile::tempdir().unwrap();
        set_env(
            "XDG_DATA_HOME",
            &dir.path().join("nowhere").display().to_string(),
        );
        let found = load();
        unset_env("XDG_DATA_HOME");
        assert!(found.is_empty());
    }

    #[test]
    fn a_timestamp_is_a_fixed_shape() {
        // The nineteen characters before the offset, checked by separator before
        // the digits are parsed, so a plausible-looking wrong string is refused.
        assert!(parse_time("2024-05-01T10:20:30Z").is_some());
        // The right length and the wrong separators.
        assert!(parse_time("2024/05/01T10:20:30Z").is_none());
        assert!(parse_time("2024-05-01 10:20:30Z").is_none());
        assert!(parse_time("yesterday").is_none());
        assert!(parse_time("2024-05-01T10:20").is_none());
    }
}
