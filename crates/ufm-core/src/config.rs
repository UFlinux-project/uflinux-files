//! Preferences, persisted as JSON under the user's configuration directory.
//!
//! Deliberately not `GSettings`: adding a compiled schema means a `glib-compile-
//! schemas` step in the package and a system-wide file, for a program that is the
//! only reader of its own settings. The one schema read is
//! `org.gnome.desktop.interface`, and only to follow the system light/dark
//! choice.
//!
//! Unknown fields are ignored rather than rejected, so a newer build can add a
//! key without stranding an older one, and a downgrade keeps working.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::error::Result;
use crate::location::Location;
use crate::sort::SortKey;

/// Current on-disk shape. Bump when a change cannot be absorbed by `#[serde(default)]`.
pub const VERSION: u32 = 1;

/// How the main pane shows a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// `GtkGridView` over a `GListStore`, the Nautilus icon view.
    #[default]
    Grid,
    /// `GtkColumnView`, sortable columns.
    List,
    /// Miller columns: one `GtkListView` per path component.
    Columns,
}

impl ViewMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Grid => "Grid",
            Self::List => "List",
            Self::Columns => "Columns",
        }
    }

    /// The accelerator shown in the primary menu. `Ctrl+1/2/3` mirror the
    /// numbering used elsewhere in the desktop.
    #[must_use]
    pub const fn accelerator(self) -> &'static str {
        match self {
            Self::Grid => "<Ctrl>1",
            Self::List => "<Ctrl>2",
            Self::Columns => "<Ctrl>3",
        }
    }

    #[must_use]
    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Grid => "view-grid-symbolic",
            Self::List => "view-list-symbolic",
            Self::Columns => "view-dual-symbolic",
        }
    }
}

/// Whether the "My Computer" panel is shown, and how much of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PanelMode {
    /// The whole panel, with its group headings.
    #[default]
    Visible,
    /// Groups run together under no heading.
    Merged,
    /// Only the preferred-folders shortcuts remain.
    Hidden,
}

/// One group of the My Computer panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelGroup {
    /// Preferred folders, drag-reorderable.
    Folders,
    System,
    Local,
    Removable,
    Disc,
    Network,
}

/// How the system colour scheme is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ColorMode {
    #[default]
    Auto,
    Light,
    Dark,
}

/// A saved search scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// The current folder and everything below it.
    Here,
    /// Where a search starts on a machine with no folder open: the user's own
    /// files, and nothing system-wide.
    #[default]
    Home,
    Everything,
}

/// Everything the user can change, in one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub version: u32,
    pub view_mode: ViewMode,
    /// Per-location overrides, keyed by the location URI. A folder opened in
    /// Columns mode stays in Columns mode without changing the default.
    pub view_modes: HashMap<String, ViewMode>,
    pub sort: SortKey,
    pub sorts: HashMap<String, SortKey>,
    pub show_hidden: bool,
    pub show_toolbar: bool,
    /// Show a picture of a picture instead of a generic icon.
    ///
    /// On by default, and switchable, because reading a folder of camera JPEGs is
    /// the one thing in a file manager that costs a low-end machine real work.
    /// The switch lives here rather than only in the view so that a second window
    /// and the desktop provider can be told the same thing.
    pub thumbnails: bool,
    /// Whether the selection inspector is requested in wide windows.
    pub preview_visible: bool,
    pub preview_width_px: u32,
    pub sidebar_width_px: u32,
    pub panel_mode: PanelMode,
    pub panel_groups: HashMap<PanelGroup, PanelMode>,
    /// Panel groups the user switched off entirely.
    pub panel_hidden: Vec<PanelGroup>,
    pub icon_scale: u32,
    pub list_density: u32,
    pub column_widths: HashMap<String, u32>,
    pub open_virtual_at_startup: bool,
    pub startup_location: Option<Location>,
    pub restore_session: bool,
    pub confirm_trash: bool,
    pub executable_text_prompt: bool,
    pub search_scope: SearchScope,
    pub search_content: bool,
    pub color_mode: ColorMode,
    /// Column widths of the Miller columns, outermost first.
    pub column_widths_px: Vec<u32>,
}

/// Where one desktop icon sits on the shell's grid.
///
/// `Ord` because the desktop-icon layout keeps a set of occupied cells and
/// walks it in reading order to find a free one.
///
/// This is the position on one monitor; which monitor is part of the desktop
/// layout, not of the view preferences. The provider keeps that layout in its own
/// file rather than here, because it is a separate process that should not have to
/// read the UI's preferences to answer a geometry report, and because a slot with
/// no monitor in it cannot say which of two screens an icon is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DesktopSlot {
    pub col: u16,
    pub row: u16,
}

impl Default for Preferences {
    fn default() -> Self {
        let mut panel_groups = HashMap::new();
        panel_groups.insert(PanelGroup::Folders, PanelMode::Visible);
        Self {
            version: VERSION,
            view_mode: ViewMode::Grid,
            view_modes: HashMap::new(),
            sort: SortKey::default(),
            sorts: HashMap::new(),
            show_hidden: false,
            show_toolbar: true,
            thumbnails: true,
            preview_visible: true,
            preview_width_px: 320,
            sidebar_width_px: 220,
            panel_mode: PanelMode::Visible,
            panel_groups,
            panel_hidden: Vec::new(),
            icon_scale: 128,
            list_density: 24,
            column_widths: HashMap::new(),
            open_virtual_at_startup: false,
            startup_location: None,
            restore_session: true,
            confirm_trash: false,
            executable_text_prompt: true,
            search_scope: SearchScope::default(),
            search_content: false,
            color_mode: ColorMode::default(),
            column_widths_px: Vec::new(),
        }
    }
}

impl Preferences {
    /// Load from `path`, falling back to the defaults when it is absent.
    pub fn load(path: &Path) -> Result<Self> {
        let mut prefs: Self = atomic::read_json(path)?;
        if prefs.version == 0 {
            prefs.version = VERSION;
        }
        // A scale of 0 would make every icon invisible and a width of 0 would
        // collapse a column to nothing, neither of which is recoverable from the
        // UI. Clamp rather than reset so the rest of the file survives.
        prefs.icon_scale = prefs.icon_scale.clamp(16, 512);
        prefs.list_density = prefs.list_density.clamp(16, 64);
        prefs.preview_width_px = prefs.preview_width_px.clamp(260, 640);
        prefs.sidebar_width_px = prefs.sidebar_width_px.clamp(160, 400);
        Ok(prefs)
    }

    #[must_use]
    pub fn path() -> PathBuf {
        crate::paths::config_dir().join("preferences.json")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic::write_json(path, self)
    }

    /// The mode to use for `location`: its own override if there is one,
    /// otherwise the default.
    #[must_use]
    pub fn view_mode_for(&self, location: &Location) -> ViewMode {
        self.view_modes
            .get(&location.to_string())
            .copied()
            .unwrap_or(self.view_mode)
    }

    pub fn set_view_mode_for(&mut self, location: &Location, mode: ViewMode) {
        self.view_modes.insert(location.to_string(), mode);
    }

    #[must_use]
    pub fn sort_for(&self, location: &Location) -> SortKey {
        self.sorts
            .get(&location.to_string())
            .copied()
            .or(self.sorts.get("").copied())
            .unwrap_or(self.sort)
    }

    pub fn set_sort_for(&mut self, location: &Location, key: SortKey) {
        self.sorts.insert(location.to_string(), key);
    }

    #[must_use]
    pub fn column_width(&self, column: &str) -> Option<u32> {
        self.column_widths.get(column).copied().filter(|w| *w >= 40)
    }

    pub fn set_column_width(&mut self, column: &str, width: u32) {
        self.column_widths.insert(column.to_string(), width.max(40));
    }

    #[must_use]
    pub fn panel_visible(&self, group: PanelGroup) -> bool {
        if self.panel_hidden.contains(&group) {
            return false;
        }
        matches!(
            self.panel_groups
                .get(&group)
                .copied()
                .unwrap_or(self.panel_mode),
            PanelMode::Visible | PanelMode::Merged
        )
    }

    #[must_use]
    pub fn panel_merged(&self, group: PanelGroup) -> bool {
        self.panel_groups
            .get(&group)
            .copied()
            .unwrap_or(self.panel_mode)
            == PanelMode::Merged
    }
}

/// A bookmark, stored so `GtkBookmarkList` could read it if it ever wants to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Bookmark {
    pub location: Location,
    /// `None` means "use the folder's own name".
    pub label: Option<String>,
    /// A symbolic icon chosen by the user, by name.
    pub icon: Option<String>,
}

impl Bookmark {
    #[must_use]
    pub fn display(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.location.file_name().map(str::to_string))
            .unwrap_or_else(|| self.location.display())
    }
}

/// The user's sidebar shortcuts. Kept in its own file, and kept compatible with
/// `GtkBookmarkList`'s `bookmarks` key, so the two can share a file later.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmarks {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub items: Vec<Bookmark>,
}

impl Bookmarks {
    pub fn path() -> PathBuf {
        crate::paths::config_dir().join("bookmarks.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        atomic::read_json::<Self>(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic::write_json(path, self)
    }

    pub fn add(&mut self, location: Location, label: Option<String>) {
        let uri = location.to_string();
        if self.items.iter().any(|b| b.location.to_string() == uri) {
            return;
        }
        self.items.push(Bookmark {
            location,
            label,
            icon: None,
        });
    }

    /// Where a location's bookmark sits, if it has one. The comparison is by URI
    /// rather than by `Location`, which is what [`Self::add`] does and for the
    /// same reason: two parses of the same place can differ in their parsed parts.
    #[must_use]
    pub fn find(&self, location: &Location) -> Option<usize> {
        let uri = location.to_string();
        self.items
            .iter()
            .position(|b| b.location.to_string() == uri)
    }

    /// Give a bookmark a name of its own, or take the name back off so the row
    /// says the folder's again. A name that is only whitespace is the same as no
    /// name: a row that reads as one space is worse than one that reads as the
    /// folder.
    ///
    /// Answers whether there was a bookmark to rename, so a caller can say so
    /// rather than reporting a change that did not happen — the list can have
    /// changed under a prompt left open.
    pub fn rename(&mut self, location: &Location, label: Option<String>) -> bool {
        let Some(index) = self.find(location) else {
            return false;
        };
        self.items[index].label = label.filter(|text| !text.trim().is_empty());
        true
    }

    pub fn remove(&mut self, location: &Location) {
        let uri = location.to_string();
        self.items.retain(|b| b.location.to_string() != uri);
    }

    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.items.len() || to >= self.items.len() || from == to {
            return;
        }
        let item = self.items.remove(from);
        self.items.insert(to, item);
    }
}

/// "Date modified" the way a person reads it: a clock time for today, a weekday
/// inside the week, a date otherwise. Written by hand to avoid pulling in a date
/// library for one column.
#[must_use]
pub fn format_modified(when: SystemTime) -> String {
    let Ok(age) = when.elapsed() else {
        // A timestamp in the future: clock skew, or a copy that did not
        // preserve mtime. Showing a date would look like a bug, and the file
        // is by definition the newest thing around, so it reads as just now.
        return "just now".to_string();
    };
    if age < Duration::from_secs(60) {
        return "just now".to_string();
    }
    if age < Duration::from_secs(3600) {
        return format!("{} minutes ago", age.as_secs() / 60);
    }
    if age < Duration::from_secs(86_400) {
        let hours = age.as_secs() / 3600;
        return if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        };
    }
    if age < Duration::from_secs(6 * 86_400) {
        let days = age.as_secs() / 86_400;
        return if days == 1 {
            "yesterday".to_string()
        } else {
            format!("{days} days ago")
        };
    }
    format_date(when)
}

/// Days since the epoch, then a plain `YYYY-MM-DD`. Civil-from-days is the
/// standard Howard Hinnant algorithm, integer only.
fn format_date(when: SystemTime) -> String {
    let secs = match when.duration_since(SystemTime::UNIX_EPOCH) {
        // Saturating rather than wrapping: a timestamp far enough out to overflow
        // `i64` seconds is corrupt, and a far-future date is more useful to show
        // than a negative one.
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        // A mtime before 1970: `duration_since` fails rather than going
        // negative, so measure the other way round. `div_euclid` below then
        // floors towards minus infinity, which is what a date needs.
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// An absolute date and clock time, `2026-01-02 15:04`, for the places that
/// must name the moment rather than its distance from now — the properties
/// dialog above all, where "4 days ago" cannot be copied into a bug report.
///
/// UTC, like [`format_date`], and for the same reason: the whole point of
/// writing these by hand was to avoid a date library, and a local time needs a
/// timezone database as well as an algorithm. The list column shows UTC dates
/// too, so the dialog and the column cannot disagree about the same file.
#[must_use]
pub fn format_full(when: SystemTime) -> String {
    let secs = match when.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    // `rem_euclid` rather than `%` so a timestamp before 1970 still shows a
    // clock time inside the day rather than a negative one.
    let time = secs.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}",
        format_date(when),
        time / 3_600,
        (time % 3_600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sort::SortField;

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("ufm-prefs-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_round_trip() {
        let t = Temp::new("round");
        let path = t.0.join("preferences.json");
        let prefs = Preferences::default();
        prefs.save(&path).unwrap();
        assert_eq!(Preferences::load(&path).unwrap(), prefs);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let t = Temp::new("missing");
        assert_eq!(
            Preferences::load(&t.0.join("nope.json")).unwrap(),
            Preferences::default()
        );
    }

    #[test]
    fn unknown_future_keys_are_ignored() {
        let t = Temp::new("future");
        let path = t.0.join("preferences.json");
        std::fs::write(
            &path,
            br#"{"version":1,"view_mode":"list","a_key_from_the_future":42}"#,
        )
        .unwrap();
        let prefs = Preferences::load(&path).unwrap();
        assert_eq!(prefs.view_mode, ViewMode::List);
        assert_eq!(prefs.icon_scale, 128);
    }

    #[test]
    fn a_bogus_scale_is_clamped_not_fatal() {
        let t = Temp::new("clamp");
        let path = t.0.join("preferences.json");
        std::fs::write(&path, br#"{"version":1,"icon_scale":0,"show_hidden":true}"#).unwrap();
        let prefs = Preferences::load(&path).unwrap();
        assert_eq!(prefs.icon_scale, 16);
        assert!(prefs.show_hidden, "the rest of the file must survive");
    }

    #[test]
    fn corrupt_file_is_reported() {
        let t = Temp::new("corrupt");
        let path = t.0.join("preferences.json");
        std::fs::write(&path, b"{").unwrap();
        assert!(Preferences::load(&path).is_err());
    }

    #[test]
    fn per_folder_mode_and_sort_override_the_default() {
        let mut prefs = Preferences::default();
        let loc = Location::parse("/home/u/Downloads").unwrap();
        assert_eq!(prefs.view_mode_for(&loc), ViewMode::Grid);
        prefs.set_view_mode_for(&loc, ViewMode::Columns);
        assert_eq!(prefs.view_mode_for(&loc), ViewMode::Columns);
        assert_eq!(
            prefs.view_mode_for(&Location::parse("/home/u").unwrap()),
            ViewMode::Grid
        );

        let key = SortKey {
            field: SortField::Modified,
            descending: true,
            folders_first: false,
        };
        prefs.set_sort_for(&loc, key);
        assert_eq!(prefs.sort_for(&loc), key);
        assert_eq!(
            prefs.sort_for(&Location::parse("/home/u").unwrap()),
            SortKey::default()
        );
    }

    #[test]
    fn panel_visibility_follows_the_group_then_the_whole_panel() {
        let mut prefs = Preferences::default();
        assert!(prefs.panel_visible(PanelGroup::Network));
        prefs.panel_mode = PanelMode::Hidden;
        assert!(!prefs.panel_visible(PanelGroup::Network));
        // An explicit group setting wins over the global one.
        prefs
            .panel_groups
            .insert(PanelGroup::Network, PanelMode::Merged);
        assert!(prefs.panel_visible(PanelGroup::Network));
        assert!(prefs.panel_merged(PanelGroup::Network));
        // Switching a group off outright beats both.
        prefs.panel_hidden.push(PanelGroup::Network);
        assert!(!prefs.panel_visible(PanelGroup::Network));
    }

    #[test]
    fn column_widths_have_a_floor() {
        let mut prefs = Preferences::default();
        prefs.set_column_width("name", 1);
        assert_eq!(prefs.column_width("name"), Some(40));
        prefs.set_column_width("name", 300);
        assert_eq!(prefs.column_width("name"), Some(300));
        assert_eq!(prefs.column_width("absent"), None);
    }

    #[test]
    fn bookmarks_dedupe_reorder_and_drop() {
        let mut b = Bookmarks::default();
        let docs = Location::parse("/home/u/Docs").unwrap();
        let dl = Location::parse("/home/u/Downloads").unwrap();
        b.add(docs.clone(), None);
        b.add(dl.clone(), Some("Files down".into()));
        b.add(docs.clone(), Some("Docs again".into()));
        assert_eq!(b.items.len(), 2);
        assert_eq!(
            b.items[0].display(),
            "Docs",
            "a repeat is dropped whole, label included"
        );
        assert_eq!(b.items[1].display(), "Files down");
        b.move_item(1, 0);
        assert_eq!(b.items[0].display(), "Files down");
        b.move_item(0, 99);
        assert_eq!(
            b.items[0].display(),
            "Files down",
            "an out of range move is a no-op"
        );
        b.remove(&docs);
        assert_eq!(b.items.len(), 1);
    }

    #[test]
    fn bookmarks_round_trip() {
        let t = Temp::new("bookmarks");
        let path = t.0.join("bookmarks.json");
        let mut b = Bookmarks::default();
        b.add(Location::parse("archive:///t/a.zip!/").unwrap(), None);
        b.save(&path).unwrap();
        let back = Bookmarks::load(&path).unwrap();
        assert_eq!(back.items.len(), 1);
        assert_eq!(
            back.items[0].location,
            Location::parse("archive:///t/a.zip!/").unwrap()
        );
    }

    #[test]
    fn a_bookmark_takes_a_name_and_gives_it_back() {
        let mut b = Bookmarks::default();
        let work = Location::parse("/data/work").unwrap();
        b.add(work.clone(), None);
        assert!(b.rename(&work, Some("Prace".into())));
        assert_eq!(b.items[0].display(), "Prace");
        assert!(
            b.rename(&work, Some("   ".into())),
            "a name of spaces is accepted and means no name"
        );
        assert_eq!(
            b.items[0].display(),
            "work",
            "and the row falls back to the folder"
        );
        assert!(
            !b.rename(&Location::parse("/data/gone").unwrap(), Some("x".into())),
            "a bookmark removed while the prompt was open is reported, not ignored"
        );
        assert_eq!(b.items.len(), 1, "and nothing was added");
    }

    #[test]
    fn a_half_written_bookmark_file_is_an_error_not_a_forget() {
        // The generic case lives in `atomic`; this one is here because the
        // consequence is specific: a list read as empty would be written back as
        // empty at the next toggle, and the bookmarks would be gone for good.
        let t = Temp::new("bookmarks-corrupt");
        let path = t.0.join("bookmarks.json");
        std::fs::write(&path, b"{\"version\":1,\"items\":[{\"location\":").unwrap();
        assert!(Bookmarks::load(&path).is_err());
    }

    #[test]
    fn dates_use_the_civil_calendar() {
        let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        // Midnight UTC, taken from the usual reference points rather than
        // recomputed with the same formula the formatter uses.
        assert_eq!(format_date(at(0)), "1970-01-01");
        assert_eq!(format_date(at(1_703_980_800)), "2023-12-31");
        assert_eq!(format_date(at(1_709_164_800)), "2024-02-29", "leap day");
        assert_eq!(format_date(at(1_774_396_800)), "2026-03-25");
        // Before the epoch, which a mtime of 0 minus an adjustment can produce.
        assert_eq!(
            format_date(SystemTime::UNIX_EPOCH - Duration::from_secs(86_400)),
            "1969-12-31"
        );
    }

    #[test]
    fn relative_ages_read_as_people_expect() {
        let ago = |secs| SystemTime::now() - Duration::from_secs(secs);
        assert_eq!(format_modified(ago(5)), "just now");
        assert_eq!(format_modified(ago(120)), "2 minutes ago");
        assert_eq!(format_modified(ago(3_600)), "1 hour ago");
        assert_eq!(format_modified(ago(7_200)), "2 hours ago");
        assert_eq!(format_modified(ago(90_000)), "yesterday");
        assert_eq!(format_modified(ago(4 * 86_400)), "4 days ago");
        // Older than a week falls back to a date.
        assert!(format_modified(ago(40 * 86_400)).contains('-'));
        // A clock skew into the future must not panic or say "-3 hours".
        assert!(!format_modified(SystemTime::now() + Duration::from_secs(3_600)).contains('-'));
    }

    #[test]
    fn full_timestamps_name_the_moment() {
        assert_eq!(format_full(SystemTime::UNIX_EPOCH), "1970-01-01 00:00");
        assert_eq!(
            format_full(SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 0)),
            "2023-11-14 22:13"
        );
        // Half a day before the epoch: the date is 1969 and the clock is still
        // inside its day, which is what `rem_euclid` buys.
        assert_eq!(
            format_full(SystemTime::UNIX_EPOCH - Duration::from_secs(43_200)),
            "1969-12-31 12:00"
        );
    }
}
