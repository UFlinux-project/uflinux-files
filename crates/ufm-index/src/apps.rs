//! Turning `.desktop` files into index records.
//!
//! The application index is the file schema minus the content field, so app search
//! and file search share a query path, a ranking and a paging protocol - which is
//! what lets the start menu reuse this daemon later instead of keeping a catalog of
//! its own. What differs is where records come from: a directory crawl here, and a
//! desktop entry rather than a file's metadata.
//!
//! # Why the INI parser is here
//!
//! `Gio::DesktopAppInfo` does this job properly - the whole spec, the localized
//! keys, the show-in rules, `Exec` field codes - and the settings app uses it. It is
//! also a `GLib` type, and pulling it in would put a main context and a type system
//! inside a daemon whose argument for existing is that it answers a query in a few
//! milliseconds on a machine with 512 MiB of RAM (see the crate docs).
//!
//! So this module parses the part of the desktop-entry spec that decides *whether*
//! an application should be offered and *how it is found*: the `[Desktop Entry]`
//! group, `Type`, `Name`, `GenericName`, `Comment`, `Keywords`, `Categories`,
//! `NoDisplay`, `Hidden`, `OnlyShowIn`, `NotShowIn`, `TryExec`, `Exec` and `Icon`.
//! Everything else is left to whoever launches the thing, which is the right
//! division of labour: [`AppEntry`] carries `Exec` and `Icon` verbatim so a caller
//! can hand them to a launcher without reading the file twice.
//!
//! Localized variants (`Name[cs]`) are ignored on purpose: the index stores the
//! default `Name`, and the component that knows the UI locale is the one that
//! displays it. `GKeyFile`-style line continuations are not part of the spec and are
//! not supported here either.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ufm_core::{Error, Result};

use crate::doc::{Kind, Record, parent_of};

/// The desktop-entry group that decides what an application is.
const ENTRY_GROUP: &str = "Desktop Entry";

/// Categories that mark an entry as not meant to be found. Following the menu
/// spec's deprecated list: an entry carrying only these is either an obsolete
/// technology or a wrapper, and offering it makes the result list worse.
const DEPRECATED_CATEGORIES: [&str; 6] = [
    "Core",
    "Monitor",
    "Filesystem",
    "Build",
    "ConsoleOnly",
    "Splash",
];

/// Where entries live, in the order the spec gives them: `$XDG_DATA_HOME` first, so
/// a user's copy shadows a system entry of the same name, then every
/// `$XDG_DATA_DIRS` entry.
#[must_use]
pub fn data_dirs() -> Vec<PathBuf> {
    let configured = std::env::var_os("XDG_DATA_DIRS").filter(|value| !value.is_empty());
    let bases = match configured {
        Some(value) => std::env::split_paths(&value).collect::<Vec<_>>(),
        // The spec's default when the variable is unset or empty.
        None => vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    };
    let mut dirs = vec![ufm_core::paths::data_root().join("applications")];
    dirs.extend(
        bases
            .into_iter()
            .filter(|base| base.is_absolute())
            .map(|base| base.join("applications")),
    );
    dirs
}

/// The session names used for `OnlyShowIn`/`NotShowIn`, as `XDG_CURRENT_DESKTOP`
/// holds them: a colon-separated list, since a session may advertise itself as part
/// of another one. The desktop this daemon ships with exports
/// `XDG_CURRENT_DESKTOP=labwc`; outside a session there is none, and then an entry
/// with a restriction is kept rather than dropped, because dropping it would hide an
/// application because of a missing environment variable.
#[must_use]
pub fn default_session() -> Option<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// One parsed entry, holding what the index needs and what a launcher needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    /// Absolute path of the `.desktop` file. Also the index key, so a change to one
    /// file replaces its row rather than adding a second one.
    pub path: PathBuf,
    /// The file name without its extension: the desktop id, the form a launcher
    /// takes when it should start an application without a command line of its own.
    pub id: String,
    /// `Name`, unescaped. What a result row shows and what a name match ranks.
    pub name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    /// `Categories`, split on `;` with the deprecated ones dropped. These become the
    /// record's labels, so filtering by category is a term query here exactly as it
    /// is for files.
    pub categories: Vec<String>,
    /// `Keywords`, split and lowercased: what makes "settings" find an entry named
    /// `System Configuration`.
    pub keywords: Vec<String>,
    /// `Icon`, verbatim - an icon name, or an absolute path.
    pub icon: Option<String>,
    /// `Exec`, verbatim, field codes included. Expanding it is the launcher's job.
    pub exec: Option<String>,
}

/// Why an entry is not in the index. Kept distinct from a read failure: a directory
/// of desktop files routinely holds a `mimeinfo.cache`, a `.directory`, or a file
/// another program is still writing, and none of those is worth reporting as a
/// problem with the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// No `[Desktop Entry]` group, or a `Type` that is not an application.
    NotAnApplication,
    /// No `Name`, or an empty one. An entry without a name cannot be displayed or
    /// searched for.
    Unnamed,
    /// `Hidden` or `NoDisplay`.
    NotDisplayed,
    /// `OnlyShowIn`/`NotShowIn` excludes the session this daemon was asked about.
    WrongSession,
    /// `TryExec` names a program that is not installed: a stale remnant of a
    /// removed package, which would be found by search and then fail to start.
    TryExecMissing,
}

impl AppEntry {
    /// Read and parse one file. Only a file that cannot be read is an error; one
    /// that is not an indexable application is `Ok(None)`. Pass [`default_session`] for
    /// the current desktop, or `None` to ignore the show-in rules.
    pub fn load(path: &Path, session: Option<&str>) -> Result<Option<Self>> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Ok(Self::parse(path, &text, session).ok())
    }

    /// Parse an entry from the text of a file. `session` is the desktop name to
    /// apply the show-in rules against, or `None` to ignore them.
    pub fn parse(
        path: &Path,
        text: &str,
        session: Option<&str>,
    ) -> std::result::Result<Self, Skip> {
        let fields = entry_fields(text).ok_or(Skip::NotAnApplication)?;
        match fields.get("Type").map(String::as_str) {
            // `Type` defaults to `Application`. A `Link` or a `Directory` is not
            // something this index is for.
            None | Some("Application") => {}
            Some(_) => return Err(Skip::NotAnApplication),
        }
        let name = fields.get("Name").cloned().filter(|n| !n.is_empty());
        let Some(name) = name else {
            return Err(Skip::Unnamed);
        };
        // `Hidden` is a user or a distro taking an entry out of the menus,
        // `NoDisplay` the packager's version of the same. Both are absolute.
        if flag(&fields, "Hidden") || flag(&fields, "NoDisplay") {
            return Err(Skip::NotDisplayed);
        }
        if !visible_for(&fields, session) {
            return Err(Skip::WrongSession);
        }
        if let Some(program) = fields.get("TryExec")
            && !resolvable(program)
        {
            return Err(Skip::TryExecMissing);
        }
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        Ok(Self {
            path: path.to_path_buf(),
            id,
            name,
            generic_name: fields.get("GenericName").cloned(),
            comment: fields.get("Comment").cloned(),
            categories: meaningful_categories(
                fields
                    .get("Categories")
                    .map_or_else(Vec::new, |value| split_semicolon(value.as_str())),
            ),
            keywords: fields
                .get("Keywords")
                .map_or_else(Vec::new, |value| keyword_terms(value.as_str())),
            icon: fields.get("Icon").cloned().filter(|v| !v.is_empty()),
            exec: fields.get("Exec").cloned().filter(|v| !v.is_empty()),
        })
    }

    /// The record to index.
    ///
    /// `GenericName` and `Comment` go in as body text rather than as labels:
    /// "disk usage" has to find the entry whose name is `Disk Usage Analyzer` *and*
    /// the one whose comment merely mentions it, and a label is a filter, not free
    /// text. That is what the content field is for in this index - a couple of
    /// hundred short strings, and the price of keeping them is a doc store the size
    /// of a screenshot.
    ///
    /// `mime` stays empty: the type of a `.desktop` file is not the type of what it
    /// opens, and an empty field makes a type filter match nothing here rather than
    /// everything.
    #[must_use]
    pub fn record(&self) -> Option<Record> {
        let path = self.path.to_str()?;
        let parent = parent_of(path)?;
        let content = [self.generic_name.as_deref(), self.comment.as_deref()]
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let mut labels = self.categories.clone();
        labels.extend(self.keywords.iter().cloned());
        Some(Record {
            path: path.to_string(),
            parent,
            name: self.name.clone(),
            kind: Kind::File,
            mime: None,
            // The size of a two-line text file says nothing useful; zero keeps a
            // size-ordered application list from pretending otherwise.
            size: 0,
            modified: modified_of(&self.path),
            hidden: false,
            content: (!content.is_empty()).then_some(content),
            labels,
        })
    }

    /// The `Categories` value, joined back the way the spec writes it. For a D-Bus
    /// reply, where a list would need a schema of its own.
    #[must_use]
    pub fn categories_field(&self) -> String {
        let mut out = self.categories.join(";");
        if !out.is_empty() {
            out.push(';');
        }
        out
    }
}

/// Collect the keys of the first `[Desktop Entry]` group, unescaping its values.
fn entry_fields(text: &str) -> Option<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    let mut in_group = false;
    let mut seen_group = false;
    // A byte-order mark is common in files written by tools that assume UTF-16
    // rules, and it would hide the group header.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix('[') {
            if seen_group {
                // Only the first group counts; a later one ends the entry rather
                // than adding to it, so a `[Desktop Action ...]` cannot smuggle its
                // `Name` in as the application's.
                break;
            }
            in_group = rest.trim_end_matches(']') == ENTRY_GROUP;
            seen_group = true;
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // A localized variant is a different key; see the module notes.
        if key.is_empty() || key.contains('[') {
            continue;
        }
        fields.insert(key.to_string(), unescape(value.trim()));
    }
    (!fields.is_empty()).then_some(fields)
}

/// Whether an entry survives its `OnlyShowIn`/`NotShowIn` restrictions.
fn visible_for(fields: &BTreeMap<String, String>, session: Option<&str>) -> bool {
    let Some(session) = session else {
        return true;
    };
    if let Some(only) = fields.get("OnlyShowIn") {
        let listed = split_semicolon(only);
        // An empty list is no restriction at all.
        if !listed.is_empty() && !listed.iter().any(|name| matches_session(name, session)) {
            return false;
        }
    }
    if let Some(not) = fields.get("NotShowIn")
        && split_semicolon(not)
            .iter()
            .any(|name| matches_session(name, session))
    {
        return false;
    }
    true
}

/// Session names compare case-insensitively, and `XDG_CURRENT_DESKTOP` is a
/// colon-separated list, so `labwc:GNOME` is excluded by `NotShowIn=GNOME`.
fn matches_session(listed: &str, session: &str) -> bool {
    listed.eq_ignore_ascii_case(session)
        || session
            .split(':')
            .any(|part| part.eq_ignore_ascii_case(listed))
}

fn flag(fields: &BTreeMap<String, String>, key: &str) -> bool {
    fields
        .get(key)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// Whether a `TryExec` value points at something installed. An absolute path is
/// checked directly, a bare name against `$PATH` - which is what "executable" means
/// here.
fn resolvable(program: &str) -> bool {
    let first = program.split_whitespace().next().unwrap_or(program);
    if first.contains('/') {
        return Path::new(first).is_file();
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join(first).is_file())
}

/// `Keywords` is a semicolon-separated list of ways to say the same thing, and a
/// user types one word of one of them. Splitting on whitespace as well costs nothing
/// (the values are a dozen short phrases) and turns "disk usage" into two terms that
/// a name match and a body match can both reach.
fn keyword_terms(value: &str) -> Vec<String> {
    split_semicolon(value)
        .iter()
        .flat_map(|phrase| phrase.split_whitespace())
        .map(str::to_lowercase)
        .collect()
}

fn split_semicolon(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// Drop the categories that mark an entry as not meant to be found. An entry left
/// with no category is still indexed: it is the user's own file, and hiding it over
/// a label would make it unfindable.
fn meaningful_categories(categories: Vec<String>) -> Vec<String> {
    categories
        .into_iter()
        .filter(|name| {
            !DEPRECATED_CATEGORIES
                .iter()
                .any(|old| name.eq_ignore_ascii_case(old))
        })
        .collect()
}

/// The desktop-entry unescaping rules for the values that get displayed.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            // `\\` lands here too, which is the whole rule: keep the character and
            // drop the backslash, since a backslash before anything else has no
            // meaning to unescape.
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn modified_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What a crawl found, for the `Status` method.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AppScanStats {
    /// Entries that went into the index.
    pub indexed: usize,
    /// Files that are not indexable applications.
    pub skipped: usize,
    /// Files that could not be read at all.
    pub unreadable: usize,
}

/// Walk the application directories and produce records.
///
/// Ids are tracked as they are seen so a user's copy in `$XDG_DATA_HOME` replaces a
/// system entry of the same name instead of appearing beside it - the alternative is
/// the same application twice in the start menu, with no rule for which one a
/// search result points at.
pub struct AppCrawler {
    dirs: Vec<PathBuf>,
    session: Option<String>,
    seen: BTreeSet<String>,
    stats: AppScanStats,
}

impl AppCrawler {
    /// Crawl the standard locations, applying the show-in rules for the session this
    /// process is running in.
    #[must_use]
    pub fn new() -> Self {
        Self::in_dirs(data_dirs())
    }

    /// Crawl exactly these directories, in precedence order, for the current session.
    #[must_use]
    pub fn in_dirs(dirs: Vec<PathBuf>) -> Self {
        Self::with_session(dirs, default_session())
    }

    /// Crawl these directories as though running in `session` (`None` to ignore the
    /// show-in rules). The daemon uses this to keep one answer for the life of a crawl:
    /// reading the environment per file would let an entry's visibility change halfway
    /// through a rebuild.
    #[must_use]
    pub fn with_session(dirs: Vec<PathBuf>, session: Option<String>) -> Self {
        Self {
            dirs,
            session,
            seen: BTreeSet::new(),
            stats: AppScanStats::default(),
        }
    }

    /// Every indexable entry, highest precedence first, one per desktop id.
    ///
    /// A file that cannot be read is counted and passed over: one broken entry in
    /// `/usr/share/applications` must not cost the user their whole application
    /// list, and the index is rebuilt from scratch often enough that a transient
    /// failure fixes itself.
    #[must_use]
    pub fn crawl(&mut self) -> Vec<AppEntry> {
        let mut entries = Vec::new();
        for dir in &self.dirs {
            for path in desktop_files(dir) {
                let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !self.seen.insert(stem.to_string()) {
                    continue;
                }
                match AppEntry::load(&path, self.session.as_deref()) {
                    Ok(Some(entry)) => {
                        self.stats.indexed += 1;
                        entries.push(entry);
                    }
                    Ok(None) => self.stats.skipped += 1,
                    Err(_) => self.stats.unreadable += 1,
                }
            }
        }
        entries
    }

    #[must_use]
    pub const fn stats(&self) -> AppScanStats {
        self.stats
    }
}

impl Default for AppCrawler {
    fn default() -> Self {
        Self::new()
    }
}

/// The `.desktop` files in `dir` and one level below it, sorted, so a crawl is
/// reproducible and a rebuild produces the same segments.
///
/// One level down is what the menu spec's sub-directory structure looks like in
/// practice. `max_depth` is also what keeps a symlinked directory from turning the
/// crawl into a long detour.
fn desktop_files(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .max_depth(2)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "desktop"))
        .map(walkdir::DirEntry::into_path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{IndexHandle, IndexKind, WritePolicy};

    /// The entry every parsing test starts from: a complete, ordinary application.
    const BAOB: &str = "\
[Desktop Entry]
Type=Application
Name=Disk Usage Analyzer
GenericName=Storage tool
Comment=Scan folders and draw pie charts
Exec=baob %U
Icon=org.gnome.baob
Categories=System;Filesystem;Monitor;
Keywords=disk usage;space;
Terminal=false
";

    fn path_for(id: &str) -> PathBuf {
        PathBuf::from("/usr/share/applications").join(format!("{id}.desktop"))
    }

    fn parse_ok(text: &str) -> AppEntry {
        AppEntry::parse(&path_for("org.gnome.baob"), text, Some("labwc"))
            .expect("the entry should be indexed")
    }

    fn file(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("a writable fixture directory");
        path
    }

    #[test]
    fn a_plain_entry_becomes_a_record_with_labels_and_body_text() {
        let entry = parse_ok(BAOB);
        assert_eq!(entry.name, "Disk Usage Analyzer");
        assert_eq!(entry.id, "org.gnome.baob");
        assert_eq!(entry.exec.as_deref(), Some("baob %U"));
        assert_eq!(entry.icon.as_deref(), Some("org.gnome.baob"));

        let record = entry.record().expect("a record for a visible entry");
        assert_eq!(
            record.path,
            "/usr/share/applications/org.gnome.baob.desktop"
        );
        assert_eq!(record.parent, "/usr/share/applications");
        assert_eq!(record.name, "Disk Usage Analyzer");
        assert!(
            record.mime.is_none(),
            "the type of the entry file is not the type of what it opens"
        );
        assert_eq!(
            record.content.as_deref(),
            Some("Storage tool Scan folders and draw pie charts"),
            "the description is body text, so a description word can match"
        );
        assert_eq!(
            record.labels,
            vec!["System", "disk", "usage", "space"],
            "categories and keywords both become labels, and the deprecated \
             categories are gone"
        );
    }

    #[test]
    fn a_hidden_or_no_display_entry_is_not_indexed() {
        for key in ["Hidden", "NoDisplay"] {
            let text = format!("{BAOB}{key}=true\n");
            assert_eq!(
                AppEntry::parse(&path_for("x"), &text, Some("labwc")),
                Err(Skip::NotDisplayed),
                "{key}=true should take the entry out of the index"
            );
        }
        // Only an explicit `true` counts; the spec has no other truthy value.
        let text = format!("{BAOB}NoDisplay=false\n");
        assert!(
            AppEntry::parse(&path_for("x"), &text, Some("labwc")).is_ok(),
            "NoDisplay=false is the default and must not hide the entry"
        );
    }

    #[test]
    fn the_show_in_rules_are_applied_against_the_session() {
        let excluded = format!("{BAOB}NotShowIn=labwc;\n");
        assert_eq!(
            AppEntry::parse(&path_for("x"), &excluded, Some("labwc")),
            Err(Skip::WrongSession)
        );
        let other = format!("{BAOB}NotShowIn=GNOME;KDE\n");
        assert!(AppEntry::parse(&path_for("x"), &other, Some("labwc")).is_ok());
        let only = format!("{BAOB}OnlyShowIn=GNOME;\n");
        assert_eq!(
            AppEntry::parse(&path_for("x"), &only, Some("labwc")),
            Err(Skip::WrongSession)
        );
        // `XDG_CURRENT_DESKTOP` is a list, so a compound session is excluded by
        // any name in it.
        assert_eq!(
            AppEntry::parse(&path_for("x"), &other, Some("labwc:GNOME")),
            Err(Skip::WrongSession),
            "a session that names GNOME is excluded by NotShowIn=GNOME"
        );
        assert!(
            AppEntry::parse(&path_for("x"), &excluded, None).is_ok(),
            "with no session to compare against, an entry is kept rather than hidden"
        );
    }

    #[test]
    fn an_entry_whose_program_is_gone_is_not_indexed() {
        let text = format!("{BAOB}TryExec=/usr/bin/a-program-that-is-not-here\n");
        assert_eq!(
            AppEntry::parse(&path_for("x"), &text, Some("labwc")),
            Err(Skip::TryExecMissing),
            "a stale entry would be found by search and then fail to start"
        );
        // Something that always exists, so the check itself is shown to work.
        let text = format!("{BAOB}TryExec=/bin/sh\n");
        assert!(AppEntry::parse(&path_for("x"), &text, Some("labwc")).is_ok());
    }

    #[test]
    fn anything_that_is_not_an_application_entry_is_skipped() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(dir.path(), "mimeinfo.cache", "mimeinfo.cache\n");
        assert_eq!(
            AppEntry::parse(&path, "mimeinfo.cache\n", None),
            Err(Skip::NotAnApplication),
            "a file with no group header is not an entry"
        );
        let link = "[Desktop Entry]\nType=Link\nName=Help\nURL=https://example.invalid\n";
        assert_eq!(
            AppEntry::parse(&path_for("help"), link, None),
            Err(Skip::NotAnApplication),
            "a link is not an application"
        );
        let unnamed = "[Desktop Entry]\nType=Application\nExec=xterm\n";
        assert_eq!(
            AppEntry::parse(&path_for("x"), unnamed, None),
            Err(Skip::Unnamed),
            "an entry with no name can be neither displayed nor searched for"
        );
    }

    #[test]
    fn a_later_group_cannot_pass_off_its_name_as_the_application_s() {
        let text = format!(
            "{BAOB}\n[Desktop Action new-window]\nName=Open a New Window\nExec=baob --new-window\n"
        );
        let entry = parse_ok(&text);
        assert_eq!(entry.name, "Disk Usage Analyzer");
    }

    #[test]
    fn a_localized_name_is_left_for_the_component_that_knows_the_locale() {
        let text = format!("{BAOB}Name[cs]=Analyzor vyuziti disku\n");
        let entry = parse_ok(&text);
        assert_eq!(entry.name, "Disk Usage Analyzer");
    }

    #[test]
    fn escapes_in_a_value_are_decoded() {
        let text = "[Desktop Entry]\nName=Files \\- Documents\nComment=one\\ntwo\n";
        let entry = parse_ok(text);
        assert_eq!(entry.name, "Files - Documents");
        assert_eq!(entry.comment.as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn an_entry_left_without_a_category_is_still_indexed() {
        let text = "[Desktop Entry]\nName=Thing\nCategories=Core;ConsoleOnly;\n";
        let entry = parse_ok(text);
        assert!(entry.categories.is_empty(), "every category was deprecated");
        assert!(
            entry.record().is_some(),
            "hiding an application over a category label would make it unfindable"
        );
    }

    #[test]
    fn a_crawl_indexes_a_directory_and_its_subdirectories() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let sub = dir.path().join("Utilities");
        std::fs::create_dir(&sub).expect("a subdirectory");
        file(&sub, "one.desktop", BAOB);
        file(dir.path(), "two.desktop", "[Desktop Entry]\nName=Other\n");
        // Not even looked at: the extension filter means a directory full of
        // `mimeinfo.cache` and `.directory` files costs nothing to crawl.
        file(dir.path(), "mimeinfo.cache", "not an entry\n");
        // A desktop entry that is not an application: seen, and counted as skipped.
        file(
            dir.path(),
            "help.desktop",
            "[Desktop Entry]\nType=Link\nName=Help\nURL=https://example.invalid\n",
        );

        let mut crawler = AppCrawler::with_session(vec![dir.path().to_path_buf()], None);
        let entries = crawler.crawl();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Disk Usage Analyzer", "Other"],
            "both files are found, and the order is the walk's: byte order within a \
             directory, depth-first, so the subdirectory sorts before a lowercase name"
        );
        assert_eq!(
            crawler.stats(),
            AppScanStats {
                indexed: 2,
                skipped: 1,
                unreadable: 0
            },
            "the entry that is not an application is counted, not reported as a failure"
        );
    }

    #[test]
    fn a_user_entry_replaces_a_system_entry_of_the_same_name() {
        let user = tempfile::tempdir().expect("a temporary directory");
        let system = tempfile::tempdir().expect("a temporary directory");
        file(
            user.path(),
            "files.desktop",
            "[Desktop Entry]\nName=My Files\n",
        );
        file(
            system.path(),
            "files.desktop",
            "[Desktop Entry]\nName=Files\n",
        );
        let mut crawler = AppCrawler::with_session(
            vec![user.path().to_path_buf(), system.path().to_path_buf()],
            None,
        );
        let entries = crawler.crawl();
        assert_eq!(entries.len(), 1, "one application, not two rows");
        assert_eq!(entries[0].name, "My Files", "the user's copy wins");
    }

    #[test]
    fn a_missing_directory_is_empty_rather_than_an_error() {
        let mut crawler =
            AppCrawler::with_session(vec![PathBuf::from("/nonexistent/ufm-apps")], None);
        assert!(crawler.crawl().is_empty());
        assert_eq!(crawler.stats(), AppScanStats::default());
    }

    #[test]
    fn an_application_index_is_searchable_by_name_label_and_description() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let index = IndexHandle::open(IndexKind::Apps, &dir.path().join("apps"))
            .expect("an application index");
        let mut crawler = AppCrawler::with_session(vec![dir.path().to_path_buf()], None);
        file(dir.path(), "baob.desktop", BAOB);
        file(
            dir.path(),
            "files.desktop",
            "[Desktop Entry]\nName=Files\nCategories=System;\n",
        );
        {
            let mut writer = index
                .writer(WritePolicy::default())
                .expect("a writer for a fresh index");
            for entry in crawler.crawl() {
                writer
                    .upsert(&entry.record().expect("a record for a visible entry"))
                    .expect("indexing an entry");
            }
            writer.finish().expect("commit and merge");
        }
        index.refresh().expect("the commit becomes visible");

        // A word that appears in neither name nor category, only in the comment.
        let request = crate::query::Request {
            text: "pie".to_string(),
            content: true,
            ..Default::default()
        };
        let hits = crate::query::search(&index, &request).expect("a search");
        assert_eq!(hits.total, 1, "only the entry describing pie charts");
        assert_eq!(hits.hits[0].name, "Disk Usage Analyzer");

        // A category is a filter, and the total reflects it rather than the page.
        let request = crate::query::Request {
            text: "files".to_string(),
            labels: vec!["System".to_string()],
            ..Default::default()
        };
        let hits = crate::query::search(&index, &request).expect("a search");
        assert_eq!(hits.total, 1);
        assert_eq!(
            hits.hits[0].path,
            dir.path()
                .join("files.desktop")
                .to_str()
                .expect("a UTF-8 temp path"),
        );
    }

    // The crawler decides visibility from the session it was built with, so a crawl
    // is reproducible and a test is not disturbed by the environment it runs under.
    #[test]
    fn a_crawl_applies_its_own_session_rather_than_the_ambient_one() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        file(
            dir.path(),
            "gnome-only.desktop",
            "[Desktop Entry]\nName=Files\nOnlyShowIn=GNOME;\n",
        );
        // The same directory, crawled as two different desktops.
        let as_gnome =
            AppCrawler::with_session(vec![dir.path().to_path_buf()], Some("GNOME".to_string()))
                .crawl();
        let as_labwc =
            AppCrawler::with_session(vec![dir.path().to_path_buf()], Some("labwc".to_string()))
                .crawl();
        assert_eq!(as_gnome.len(), 1, "GNOME is the session it asks to run in");
        assert!(
            as_labwc.is_empty(),
            "and an entry that excludes GNOME is not offered here"
        );
    }
}
