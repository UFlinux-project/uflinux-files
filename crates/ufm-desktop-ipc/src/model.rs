//! The desktop icon model, as it crosses the bus.
//!
//! The split of responsibility is the whole design. The shell owns the pixels:
//! it knows how many monitors there are, how big a cell is, and where the
//! wallpaper ends and the panel begins. The file manager owns the facts: which
//! files are on the desktop, what they are, and which cell each one belongs to.
//! Neither can render the other's job, so the shell reports geometry and asks
//! for the model, and the file manager answers and does the actual moving.
//!
//! What crosses the bus is therefore small. An [`Item`] is a name, a kind, an
//! icon and a cell — enough to paint a row of pixels and to decide whether a
//! drag over it means "drop into a folder". The path is not sent: it is the
//! desktop directory joined to the id, which is one path component, so the
//! model cannot name a file outside the directory it describes.
//!
//! A [`Cell`] names a monitor as well as a column and a row, because two
//! monitors can both have a cell at 0,0 and the model must not have to choose
//! which one an icon is on.
//!
//! Memory: the model is sent whole rather than as a stream of deltas. A desktop
//! is tens of items, not tens of thousands, and one self-consistent snapshot
//! costs one round trip and no reconciliation code on either side. The size is
//! bounded by [`MAX_ITEMS`] and by the reply cap in [`crate::wire`], and the
//! fields a renderer usually ignores are skipped when absent.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ufm_core::config::DesktopSlot;

use crate::wire::PROTOCOL_VERSION;

/// Largest model accepted or sent. Two thousand icons is far past what a
/// desktop holds; the cap exists so a runaway scan cannot make a renderer
/// allocate without limit.
pub const MAX_ITEMS: usize = 2048;

/// Largest number of monitors in one geometry report.
pub const MAX_MONITORS: usize = 8;

/// Widest grid accepted. A column of icons is 96 px at the largest icon size,
/// so 64 columns is a 6 k monitor at 1x.
pub const MAX_COLUMNS: u16 = 64;

/// Tallest grid accepted, for the same reason in the other axis.
pub const MAX_ROWS: u16 = 512;

/// Largest cell edge accepted, in pixels.
pub const MAX_CELL: u32 = 1024;

/// Longest item id accepted, which is `NAME_MAX` on every filesystem the
/// desktop directory can live on.
pub const MAX_ID_LEN: usize = 255;

/// Thumbnail edge the renderer should ask for unless the model says otherwise.
pub const DEFAULT_THUMBNAIL: u32 = 128;

/// Why an item is on the desktop, which is what decides the icon fallback and
/// whether a double-click launches or opens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A directory, or a symlink that points at one.
    Directory,
    /// Anything else with contents.
    #[default]
    File,
    /// A `.desktop` file: double-click runs `Exec`, it is not opened as text.
    Launcher,
}

impl Kind {
    /// Whether a double-click on this item should launch it rather than open it.
    #[must_use]
    pub const fn is_launchable(self) -> bool {
        matches!(self, Self::Launcher)
    }

    /// Whether dropping another icon on this one means "move it inside".
    #[must_use]
    pub const fn is_folder(self) -> bool {
        matches!(self, Self::Directory)
    }

    /// The icon name to use when the item carries none, so a renderer never has
    /// to invent a fallback of its own and every desktop shows the same thing.
    #[must_use]
    pub const fn fallback_icon(self) -> &'static str {
        match self {
            Self::Directory => "folder",
            Self::File => "text-x-generic",
            Self::Launcher => "application-x-executable",
        }
    }
}

/// A position on one monitor.
///
/// The wire form is flat — `{"monitor":0,"col":3,"row":7}` — because this object
/// appears once per icon and nesting a [`DesktopSlot`] inside it would repeat
/// the key names two thousand times for no gain in clarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    /// Which monitor's grid this is, matching [`Geometry::monitor`].
    pub monitor: u8,
    /// Column, zero-based from the left in the shell's layout order.
    pub col: u16,
    /// Row, zero-based from the top.
    pub row: u16,
}

impl Cell {
    /// A cell on `monitor`.
    #[must_use]
    pub const fn new(monitor: u8, slot: DesktopSlot) -> Self {
        Self {
            monitor,
            col: slot.col,
            row: slot.row,
        }
    }

    /// The position on its monitor, as the type the preferences use.
    #[must_use]
    pub const fn slot(self) -> DesktopSlot {
        DesktopSlot {
            col: self.col,
            row: self.row,
        }
    }

    /// The same position on a different monitor, which is what a repack after a
    /// monitor is unplugged needs.
    #[must_use]
    pub const fn on(self, monitor: u8) -> Self {
        Self {
            monitor,
            col: self.col,
            row: self.row,
        }
    }
}

/// One icon on the desktop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// The file's name inside the desktop directory, and the only way to refer
    /// to this item in a request. One path component, validated on the way in.
    pub id: String,
    /// What to show under the icon. For a launcher this is the entry's `Name`,
    /// which is usually not the file name.
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    /// An icon name or an absolute path. Empty means use [`Kind::fallback_icon`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    /// Secondary text for a tooltip: a launcher's `Comment`, or where a symlink
    /// points. Absent for the common case of a plain file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
    /// Where it sits. `None` means the item exists but has no cell, which is the
    /// state an item lands in when its monitor disappears and there is nowhere
    /// else to put it. Such an item is real and must not be deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Cell>,
}

impl Item {
    /// An item with no cell yet.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, kind: Kind) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind,
            icon: String::new(),
            tooltip: None,
            at: None,
        }
    }

    /// The text to render, falling back to the id so an icon is never labelled
    /// with nothing.
    #[must_use]
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    /// The icon to render, never empty.
    #[must_use]
    pub fn icon_name(&self) -> &str {
        if self.icon.is_empty() {
            self.kind.fallback_icon()
        } else {
            &self.icon
        }
    }

    /// Whether this item may be dragged: an item with no cell has nowhere to go
    /// back to, and the provider would have to invent one mid-drag.
    #[must_use]
    pub const fn is_placeable(&self) -> bool {
        self.at.is_some()
    }
}

/// The grid one monitor offers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    /// Identifies the monitor, stable for the session. The shell chooses it; the
    /// provider only echoes it back.
    pub monitor: u8,
    pub columns: u16,
    pub rows: u16,
    /// Cell width in pixels, including the icon's label.
    pub cell_w: u32,
    /// Cell height in pixels.
    pub cell_h: u32,
}

impl Geometry {
    /// A grid with the given shape on one monitor.
    #[must_use]
    pub const fn new(monitor: u8, columns: u16, rows: u16, cell_w: u32, cell_h: u32) -> Self {
        Self {
            monitor,
            columns,
            rows,
            cell_w,
            cell_h,
        }
    }

    /// Whether this can describe a real grid: non-empty and inside the caps. A
    /// zero-sized monitor is rejected rather than treated as an empty grid,
    /// because an empty grid would silently swallow every icon on it.
    #[must_use]
    pub const fn usable(&self) -> bool {
        self.columns > 0
            && self.rows > 0
            && self.columns <= MAX_COLUMNS
            && self.rows <= MAX_ROWS
            && self.cell_w > 0
            && self.cell_h > 0
            && self.cell_w <= MAX_CELL
            && self.cell_h <= MAX_CELL
    }

    /// Whether `slot` is inside this grid.
    #[must_use]
    pub const fn fits(&self, slot: DesktopSlot) -> bool {
        self.columns > 0 && self.rows > 0 && slot.col < self.columns && slot.row < self.rows
    }

    /// How many cells there are, for a progress total.
    #[must_use]
    pub const fn cells(&self) -> usize {
        self.columns as usize * self.rows as usize
    }
}

/// The whole desktop: what is there, and where.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// Protocol version this model is shaped for.
    pub version: u32,
    /// Bumped on every change. A renderer that gets two replies out of order
    /// keeps the newer one, which is why the number travels with the data and
    /// not only in the `Changed` signal.
    pub generation: u64,
    /// The directory, for display in a message. Not used to build paths; the
    /// provider does that and re-validates the id.
    pub desktop: String,
    /// Whether icons may be moved at all. `false` on a read-only desktop, and
    /// the safe reading of a payload that omits it.
    #[serde(default)]
    pub writable: bool,
    /// Whether to ask for thumbnails at all. Off by default: decoding one costs
    /// more than the shell's whole icon budget on a small machine.
    #[serde(default)]
    pub thumbnails: bool,
    /// Edge length to request, so the provider decodes once at the right size.
    /// A payload that omits it gets the default rather than zero, because a
    /// zero-sized thumbnail is a request that can never be satisfied.
    #[serde(default = "default_thumbnail_size")]
    pub thumbnail_size: u32,
    /// The grids the shell last reported. Empty until it says so, which is why
    /// an item may have no cell.
    #[serde(default)]
    pub monitors: Vec<Geometry>,
    /// Every icon, in the order the provider read them.
    #[serde(default)]
    pub items: Vec<Item>,
}

fn default_thumbnail_size() -> u32 {
    DEFAULT_THUMBNAIL
}

/// What one accepted [`Model::place`] did: where the item went, and what had to
/// move out of the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Where the requested item now is.
    pub placed: Cell,
    /// The item that was in that cell, and where it went instead.
    pub displaced: Option<(String, Cell)>,
}

/// Why a model or a change to it is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// An id that is not a single path component.
    BadId {
        id: String,
        why: &'static str,
    },
    TooManyItems {
        count: usize,
    },
    UnknownItem {
        id: String,
    },
    UnknownMonitor {
        monitor: u8,
    },
    DuplicateMonitor {
        monitor: u8,
    },
    /// A grid that is empty, oversized, or both.
    BadGeometry {
        monitor: u8,
    },
    TooManyMonitors {
        count: usize,
    },
    OutsideGrid {
        monitor: u8,
        slot: DesktopSlot,
        columns: u16,
        rows: u16,
    },
    /// The grid is full, so nothing could be displaced into a free cell.
    NoRoom {
        monitor: u8,
    },
    /// A serialized model over the reply cap.
    TooLarge {
        bytes: usize,
    },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadId { id, why } => write!(f, "invalid desktop item id {id:?}: {why}"),
            Self::TooManyItems { count } => {
                write!(f, "{count} items, the limit is {MAX_ITEMS}")
            }
            Self::UnknownItem { id } => write!(f, "no desktop item {id:?}"),
            Self::UnknownMonitor { monitor } => write!(f, "no monitor {monitor}"),
            Self::DuplicateMonitor { monitor } => write!(f, "monitor {monitor} reported twice"),
            Self::BadGeometry { monitor } => {
                write!(f, "geometry for monitor {monitor} is empty or too large")
            }
            Self::TooManyMonitors { count } => {
                write!(f, "{count} monitors, the limit is {MAX_MONITORS}")
            }
            Self::OutsideGrid {
                monitor,
                slot,
                columns,
                rows,
            } => write!(
                f,
                "cell {},{} is outside monitor {monitor} ({columns}x{rows})",
                slot.col, slot.row
            ),
            Self::NoRoom { monitor } => {
                write!(f, "monitor {monitor} has no free cell to displace into")
            }
            Self::TooLarge { bytes } => write!(f, "model is {bytes} bytes, too large to send"),
        }
    }
}

impl std::error::Error for ModelError {}

impl From<ModelError> for ufm_core::Error {
    fn from(e: ModelError) -> Self {
        match e {
            // An id with a slash in it is the same mistake a location can make,
            // and deserves the same name in a log.
            ModelError::BadId { .. } | ModelError::UnknownItem { .. } => {
                ufm_core::Error::UnsafeComponent(e.to_string())
            }
            other => ufm_core::Error::Protocol(other.to_string()),
        }
    }
}

/// Whether `id` can name a file inside the desktop directory.
///
/// This is the traversal guard. The provider joins the id to the directory, so
/// an id of `../../.ssh/id_rsa` would otherwise let the renderer ask for a
/// thumbnail of, or reveal, a file the desktop has nothing to do with.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.contains('/')
        && !id.contains('\0')
        && id != "."
        && id != ".."
}

impl Model {
    /// An empty model for one directory, before anything has been read.
    #[must_use]
    pub fn new(desktop: &Path) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            generation: 0,
            desktop: desktop.display().to_string(),
            writable: true,
            thumbnails: false,
            thumbnail_size: DEFAULT_THUMBNAIL,
            monitors: Vec::new(),
            items: Vec::new(),
        }
    }

    /// Add an item, refusing a duplicate id or an over-long model.
    ///
    /// The item's own cell is kept, so a provider that already knows where
    /// something goes can push it in one step.
    pub fn push(&mut self, item: Item) -> Result<(), ModelError> {
        if !valid_id(&item.id) {
            return Err(ModelError::BadId {
                id: item.id,
                why: "must be one path component",
            });
        }
        if self.items.len() >= MAX_ITEMS {
            return Err(ModelError::TooManyItems {
                count: self.items.len() + 1,
            });
        }
        if self.item(&item.id).is_some() {
            return Err(ModelError::BadId {
                id: item.id,
                why: "two items share an id",
            });
        }
        self.items.push(item);
        Ok(())
    }

    #[must_use]
    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    #[must_use]
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    #[must_use]
    pub fn cell_of(&self, id: &str) -> Option<Cell> {
        self.item(id).and_then(|i| i.at)
    }

    #[must_use]
    pub fn geometry(&self, monitor: u8) -> Option<&Geometry> {
        self.monitors.iter().find(|g| g.monitor == monitor)
    }

    /// The path an id refers to, or `None` if it would escape the directory.
    ///
    /// The join is checked rather than trusted: `Path::join` accepts an absolute
    /// component by replacing the whole path, so an id that somehow arrived as
    /// `/etc/passwd` would silently ignore the desktop directory.
    #[must_use]
    pub fn path_in(&self, id: &str) -> Option<PathBuf> {
        if !valid_id(id) {
            return None;
        }
        let base = Path::new(&self.desktop);
        let path = base.join(id);
        (path.parent() == Some(base)).then_some(path)
    }

    /// Every occupied cell on one monitor, in reading order.
    #[must_use]
    pub fn occupied(&self, monitor: u8) -> BTreeSet<DesktopSlot> {
        self.items
            .iter()
            .filter_map(|i| i.at)
            .filter(|c| c.monitor == monitor)
            .map(Cell::slot)
            .collect()
    }

    /// The first free cell on a monitor, scanning row by row from the top left,
    /// which is the order a left-to-right reader expects a new icon to appear.
    #[must_use]
    pub fn first_free(&self, monitor: u8) -> Option<Cell> {
        let grid = self.geometry(monitor)?;
        let taken = self.occupied(monitor);
        for row in 0..grid.rows {
            for col in 0..grid.columns {
                let slot = DesktopSlot { col, row };
                if !taken.contains(&slot) {
                    return Some(Cell::new(monitor, slot));
                }
            }
        }
        None
    }

    /// The first free cell, preferring the monitor an item already sits on and
    /// falling back to any other, lowest number first.
    ///
    /// The fallback is what keeps an icon visible when its monitor is unplugged:
    /// losing the icon would be worse than moving it to a screen the user did
    /// not put it on.
    #[must_use]
    pub fn first_free_anywhere(&self, preferred: u8) -> Option<Cell> {
        if let Some(cell) = self.first_free(preferred) {
            return Some(cell);
        }
        self.monitors
            .iter()
            .map(|g| g.monitor)
            .filter(|m| *m != preferred)
            .find_map(|m| self.first_free(m))
    }

    /// Move an item to a cell, displacing whatever is there.
    ///
    /// The occupant is not dropped: it goes to the first free cell, and if the
    /// grid is that full the whole move is refused rather than one icon
    /// silently vanishing. Placing an item on the cell it already occupies
    /// succeeds and displaces nothing.
    pub fn place(&mut self, id: &str, cell: Cell) -> Result<Outcome, ModelError> {
        let grid = self
            .geometry(cell.monitor)
            .ok_or(ModelError::UnknownMonitor {
                monitor: cell.monitor,
            })?;
        if !grid.fits(cell.slot()) {
            return Err(ModelError::OutsideGrid {
                monitor: cell.monitor,
                slot: cell.slot(),
                columns: grid.columns,
                rows: grid.rows,
            });
        }
        let index = self
            .index_of(id)
            .ok_or_else(|| ModelError::UnknownItem { id: id.to_string() })?;
        let occupant = self
            .items
            .iter()
            .enumerate()
            .find(|(i, item)| *i != index && item.at == Some(cell))
            .map(|(i, item)| (i, item.id.clone()));
        let displaced = match occupant {
            None => None,
            Some((i, name)) => {
                let from = self.items[i].at.unwrap_or(cell).monitor;
                let free = self
                    .first_free_anywhere(from)
                    .ok_or(ModelError::NoRoom { monitor: from })?;
                self.items[i].at = Some(free);
                Some((name, free))
            }
        };
        self.items[index].at = Some(cell);
        Ok(Outcome {
            placed: cell,
            displaced,
        })
    }

    /// Replace the geometry the shell reports and put back any icon that no
    /// longer fits, returning the ids left with no cell at all.
    ///
    /// Every monitor the shell stops mentioning is gone, which is why this takes
    /// the whole set rather than one grid: a per-monitor update cannot say that.
    pub fn set_geometry(&mut self, monitors: Vec<Geometry>) -> Result<Vec<String>, ModelError> {
        if monitors.len() > MAX_MONITORS {
            return Err(ModelError::TooManyMonitors {
                count: monitors.len(),
            });
        }
        let mut seen = BTreeSet::new();
        for grid in &monitors {
            if !grid.usable() {
                return Err(ModelError::BadGeometry {
                    monitor: grid.monitor,
                });
            }
            if !seen.insert(grid.monitor) {
                return Err(ModelError::DuplicateMonitor {
                    monitor: grid.monitor,
                });
            }
        }
        self.monitors = monitors;
        Ok(self.repack())
    }

    /// Clear and refill every cell that is no longer valid.
    ///
    /// An item is left alone only if its monitor still exists, its cell still
    /// fits, and no earlier item already claims that cell. Everything else is
    /// put back at the first free cell, preferring the monitor it was on, so a
    /// resolution change shuffles icons without deleting any.
    pub fn repack(&mut self) -> Vec<String> {
        let mut taken = BTreeSet::new();
        let mut orphans = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let Some(cell) = item.at else { continue };
            let keeps = self
                .geometry(cell.monitor)
                .is_some_and(|g| g.fits(cell.slot()))
                && taken.insert((cell.monitor, cell.slot()));
            if !keeps {
                orphans.push((cell.monitor, index));
            }
        }
        for (_, index) in &orphans {
            self.items[*index].at = None;
        }
        let mut lost = Vec::new();
        for (monitor, index) in orphans {
            match self.first_free_anywhere(monitor) {
                Some(cell) => self.items[index].at = Some(cell),
                None => lost.push(self.items[index].id.clone()),
            }
        }
        lost
    }

    /// Drop items whose files are gone, keeping the generation.
    ///
    /// `live` is the set of names the directory actually holds. Anything else is
    /// removed rather than left as a ghost, which is the difference between a
    /// deleted file and a stale icon the user has to click to find out about.
    pub fn retain(&mut self, live: &BTreeSet<String>) -> usize {
        let before = self.items.len();
        self.items.retain(|i| live.contains(&i.id));
        before - self.items.len()
    }

    /// Count this model as changed, saturating so a long-lived session cannot
    /// wrap to zero and make a renderer keep an old snapshot.
    pub fn bump(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// Reject a model that could not have come from a provider: duplicate ids,
    /// a cell on a monitor that was never reported, a cell outside its grid, or
    /// two items in one cell.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.items.len() > MAX_ITEMS {
            return Err(ModelError::TooManyItems {
                count: self.items.len(),
            });
        }
        if self.monitors.len() > MAX_MONITORS {
            return Err(ModelError::TooManyMonitors {
                count: self.monitors.len(),
            });
        }
        let mut ids = BTreeSet::new();
        for item in &self.items {
            if !valid_id(&item.id) {
                return Err(ModelError::BadId {
                    id: item.id.clone(),
                    why: "must be one path component",
                });
            }
            if !ids.insert(item.id.as_str()) {
                return Err(ModelError::BadId {
                    id: item.id.clone(),
                    why: "two items share an id",
                });
            }
        }
        let mut seen = BTreeSet::new();
        for grid in &self.monitors {
            if !grid.usable() {
                return Err(ModelError::BadGeometry {
                    monitor: grid.monitor,
                });
            }
            if !seen.insert(grid.monitor) {
                return Err(ModelError::DuplicateMonitor {
                    monitor: grid.monitor,
                });
            }
        }
        let mut taken = BTreeSet::new();
        for item in &self.items {
            let Some(cell) = item.at else { continue };
            let grid = self
                .geometry(cell.monitor)
                .ok_or(ModelError::UnknownMonitor {
                    monitor: cell.monitor,
                })?;
            if !grid.fits(cell.slot()) {
                return Err(ModelError::OutsideGrid {
                    monitor: cell.monitor,
                    slot: cell.slot(),
                    columns: grid.columns,
                    rows: grid.rows,
                });
            }
            if !taken.insert((cell.monitor, cell.slot())) {
                return Err(ModelError::OutsideGrid {
                    monitor: cell.monitor,
                    slot: cell.slot(),
                    columns: grid.columns,
                    rows: grid.rows,
                });
            }
        }
        Ok(())
    }

    /// Size of this model as it would go on the wire, or `0` if it cannot be
    /// serialized at all.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |v| v.len())
    }

    /// Refuse a model that would not fit one D-Bus message.
    pub fn check_size(&self, max: usize) -> Result<(), ModelError> {
        let bytes = self.encoded_len();
        if bytes > max {
            return Err(ModelError::TooLarge { bytes });
        }
        Ok(())
    }

    /// Cells the model could offer a new icon for, across every monitor.
    #[must_use]
    pub fn free_cells(&self) -> usize {
        self.monitors
            .iter()
            .map(|g| g.cells().saturating_sub(self.occupied(g.monitor).len()))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(monitor: u8, columns: u16, rows: u16) -> Geometry {
        Geometry::new(monitor, columns, rows, 96, 96)
    }

    fn model() -> Model {
        let mut m = Model::new(Path::new("/home/u/Desktop"));
        m.set_geometry(vec![grid(0, 3, 2)]).unwrap();
        m
    }

    fn item(id: &str) -> Item {
        Item::new(id, id, Kind::File)
    }

    #[test]
    fn an_id_must_be_one_path_component() {
        assert!(valid_id("notes.txt"));
        assert!(valid_id(".hidden"));
        assert!(valid_id("two words"));
        assert!(valid_id("a..b"));
        // A backslash is an ordinary character in a Linux file name, so a name
        // that looks like a Windows path is a legal component and must pass.
        assert!(valid_id("..\\.."));
        for bad in ["", ".", "..", "../x", "a/b", "/etc", "\0"] {
            assert!(!valid_id(bad), "{bad} should be refused");
        }
        assert!(!valid_id(&"x".repeat(MAX_ID_LEN + 1)));
    }

    #[test]
    fn an_id_cannot_escape_the_desktop_directory() {
        let m = model();
        assert_eq!(
            m.path_in("notes.txt"),
            Some(PathBuf::from("/home/u/Desktop/notes.txt"))
        );
        assert_eq!(m.path_in("../escape"), None);
        assert_eq!(m.path_in(""), None);
        // An absolute id would make `join` throw away the directory entirely.
        assert_eq!(m.path_in("/etc/passwd"), None);
    }

    #[test]
    fn a_new_model_is_read_only_about_thumbnails_and_has_no_grid() {
        let m = Model::new(Path::new("/tmp/d"));
        assert_eq!(m.version, PROTOCOL_VERSION);
        assert_eq!(m.generation, 0);
        assert!(m.writable);
        assert!(!m.thumbnails, "decoding a thumbnail is not free");
        assert_eq!(m.thumbnail_size, DEFAULT_THUMBNAIL);
        assert!(m.monitors.is_empty());
        assert_eq!(m.free_cells(), 0);
    }

    #[test]
    fn a_new_icon_lands_in_the_first_free_cell_reading_order() {
        let mut m = model();
        assert_eq!(
            m.first_free(0),
            Some(Cell {
                monitor: 0,
                col: 0,
                row: 0
            })
        );
        m.place("a", Cell::new(0, DesktopSlot { col: 0, row: 0 }))
            .unwrap_err();
        m.push(item("a")).unwrap();
        m.place("a", Cell::new(0, DesktopSlot { col: 0, row: 0 }))
            .unwrap();
        assert_eq!(
            m.first_free(0),
            Some(Cell {
                monitor: 0,
                col: 1,
                row: 0
            }),
            "the scan goes across the row before the next row"
        );
    }

    #[test]
    fn placing_an_item_displaces_its_neighbour_rather_than_losing_it() {
        let mut m = model();
        for id in ["a", "b"] {
            m.push(item(id)).unwrap();
        }
        m.place("a", Cell::new(0, DesktopSlot { col: 0, row: 0 }))
            .unwrap();
        let out = m
            .place("b", Cell::new(0, DesktopSlot { col: 0, row: 0 }))
            .unwrap();
        assert_eq!(
            m.cell_of("b").unwrap().slot(),
            DesktopSlot { col: 0, row: 0 }
        );
        assert_eq!(
            out.displaced,
            Some((
                "a".to_string(),
                Cell {
                    monitor: 0,
                    col: 1,
                    row: 0
                }
            ))
        );
        assert_eq!(
            m.cell_of("a").unwrap().slot(),
            DesktopSlot { col: 1, row: 0 }
        );
    }

    #[test]
    fn a_move_onto_the_same_cell_is_not_a_displacement() {
        let mut m = model();
        m.push(item("a")).unwrap();
        let cell = Cell::new(0, DesktopSlot { col: 2, row: 1 });
        m.place("a", cell).unwrap();
        let out = m.place("a", cell).unwrap();
        assert_eq!(out.placed, cell);
        assert_eq!(out.displaced, None);
    }

    #[test]
    fn a_full_grid_refuses_a_move_instead_of_dropping_an_icon() {
        let mut m = Model::new(Path::new("/home/u/Desktop"));
        m.set_geometry(vec![grid(0, 1, 1)]).unwrap();
        for id in ["a", "b"] {
            m.push(item(id)).unwrap();
        }
        m.place("a", Cell::new(0, DesktopSlot { col: 0, row: 0 }))
            .unwrap();
        let err = m
            .place("b", Cell::new(0, DesktopSlot { col: 0, row: 0 }))
            .unwrap_err();
        assert_eq!(err, ModelError::NoRoom { monitor: 0 });
        // The refusal left the first item where it was.
        assert_eq!(
            m.cell_of("a").unwrap().slot(),
            DesktopSlot { col: 0, row: 0 }
        );
        assert_eq!(m.cell_of("b"), None);
    }

    #[test]
    fn a_cell_outside_the_grid_or_on_no_monitor_is_refused() {
        let mut m = model();
        m.push(item("a")).unwrap();
        let err = m.place("a", Cell::new(0, DesktopSlot { col: 3, row: 0 }));
        assert!(matches!(
            err,
            Err(ModelError::OutsideGrid {
                columns: 3,
                rows: 2,
                ..
            })
        ));
        assert_eq!(
            m.place("a", Cell::new(7, DesktopSlot { col: 0, row: 0 })),
            Err(ModelError::UnknownMonitor { monitor: 7 })
        );
        assert_eq!(
            m.place("missing", Cell::new(0, DesktopSlot { col: 0, row: 0 })),
            Err(ModelError::UnknownItem {
                id: "missing".to_string()
            })
        );
    }

    #[test]
    fn unplugging_a_monitor_moves_its_icons_to_the_other_one() {
        let mut m = Model::new(Path::new("/home/u/Desktop"));
        m.set_geometry(vec![grid(0, 3, 2), grid(1, 2, 2)]).unwrap();
        for id in ["a", "b"] {
            m.push(item(id)).unwrap();
        }
        m.place("a", Cell::new(0, DesktopSlot { col: 1, row: 1 }))
            .unwrap();
        m.place("b", Cell::new(1, DesktopSlot { col: 0, row: 0 }))
            .unwrap();
        let lost = m.set_geometry(vec![grid(1, 2, 2)]).unwrap();
        assert!(lost.is_empty());
        assert_eq!(
            m.cell_of("b"),
            Some(Cell::new(1, DesktopSlot { col: 0, row: 0 }))
        );
        assert_eq!(
            m.cell_of("a"),
            Some(Cell::new(1, DesktopSlot { col: 1, row: 0 })),
            "the icon moves to the surviving screen instead of disappearing"
        );
    }

    #[test]
    fn a_shrunken_grid_repacks_without_losing_icons_and_reports_overflow() {
        let mut m = model();
        for id in ["a", "b", "c", "d", "e", "f"] {
            m.push(item(id)).unwrap();
        }
        for (i, id) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let slot = DesktopSlot {
                col: (i % 3) as u16,
                row: (i / 3) as u16,
            };
            m.place(id, Cell::new(0, slot)).unwrap();
        }
        assert_eq!(m.free_cells(), 1);
        assert_eq!(m.cell_of("f"), None, "an icon with no cell is legal");
        // Four cells for five placed icons: the one at column two no longer
        // fits and every remaining cell is held by an icon that does, so it is
        // reported rather than stacked on top of one of them.
        let lost = m.set_geometry(vec![grid(0, 2, 2)]).unwrap();
        assert_eq!(lost, vec!["c".to_string()]);
        assert_eq!(m.items.iter().filter(|i| i.at.is_some()).count(), 4);
        assert_eq!(m.cell_of("f"), None, "an unplaced icon stays unplaced");
        assert_eq!(m.validate(), Ok(()));
    }

    #[test]
    fn two_items_claiming_one_cell_are_repacked_not_rendered_on_top() {
        let mut m = model();
        let cell = Cell::new(0, DesktopSlot { col: 1, row: 1 });
        let mut a = item("a");
        a.at = Some(cell);
        let mut b = item("b");
        b.at = Some(cell);
        m.push(a).unwrap();
        m.push(b).unwrap();
        assert_eq!(
            m.validate(),
            Err(ModelError::OutsideGrid {
                monitor: 0,
                slot: cell.slot(),
                columns: 3,
                rows: 2
            }),
            "a duplicate cell is a broken model"
        );
        let lost = m.repack();
        assert!(lost.is_empty());
        assert_eq!(m.cell_of("a"), Some(cell), "the first one keeps it");
        assert_ne!(m.cell_of("b"), Some(cell));
        assert_eq!(m.validate(), Ok(()));
    }

    #[test]
    fn geometry_that_is_empty_or_absurd_is_refused() {
        assert!(!grid(0, 0, 4).usable(), "a zero-column grid");
        assert!(!Geometry::new(0, 4, 4, 0, 96).usable());
        assert!(!grid(0, MAX_COLUMNS + 1, 4).usable());
        assert!(!Geometry::new(0, 4, 4, MAX_CELL + 1, 96).usable());
        let mut m = model();
        assert_eq!(
            m.set_geometry(vec![grid(0, 0, 0)]),
            Err(ModelError::BadGeometry { monitor: 0 })
        );
        assert_eq!(
            m.set_geometry(vec![grid(0, 3, 3), grid(0, 3, 3)]),
            Err(ModelError::DuplicateMonitor { monitor: 0 })
        );
        let many = (0..=MAX_MONITORS as u8).map(|m| grid(m, 3, 3)).collect();
        assert_eq!(
            m.set_geometry(many),
            Err(ModelError::TooManyMonitors {
                count: MAX_MONITORS + 1
            })
        );
        assert_eq!(
            m.set_geometry(vec![grid(0, 3, 3)]).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_geometry_report_that_is_rejected_leaves_the_old_grid_alone() {
        let mut m = model();
        m.push(item("a")).unwrap();
        m.place("a", Cell::new(0, DesktopSlot { col: 2, row: 1 }))
            .unwrap();
        assert!(m.set_geometry(vec![grid(0, 0, 0)]).is_err());
        assert_eq!(
            m.cell_of("a"),
            Some(Cell::new(0, DesktopSlot { col: 2, row: 1 })),
            "a bad report must not scramble the desktop"
        );
    }

    #[test]
    fn a_push_refuses_a_duplicate_id_and_a_runaway_model() {
        let mut m = model();
        m.push(item("a")).unwrap();
        assert!(matches!(m.push(item("a")), Err(ModelError::BadId { .. })));
        assert!(matches!(m.push(item("a/b")), Err(ModelError::BadId { .. })));
        let mut big = model();
        for i in 0..MAX_ITEMS {
            big.push(item(&format!("f{i}"))).unwrap();
        }
        assert_eq!(
            big.push(item("one too many")),
            Err(ModelError::TooManyItems {
                count: MAX_ITEMS + 1
            })
        );
    }

    #[test]
    fn retain_drops_only_what_the_directory_still_holds() {
        let mut m = model();
        for id in ["a", "b", "c"] {
            m.push(item(id)).unwrap();
        }
        let live = ["a".to_string(), "c".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(m.retain(&live), 1);
        assert_eq!(m.items.len(), 2);
        assert!(m.item("b").is_none());
    }

    #[test]
    fn a_bump_never_wraps_to_zero() {
        let mut m = model();
        m.generation = u64::MAX;
        m.bump();
        assert_eq!(m.generation, u64::MAX);
    }

    #[test]
    fn a_label_and_an_icon_are_never_empty() {
        let mut i = Item::new("photos", "", Kind::Directory);
        assert_eq!(i.label(), "photos");
        assert_eq!(i.icon_name(), "folder");
        i.icon = "camera-photo".into();
        assert_eq!(i.icon_name(), "camera-photo");
        assert_eq!(
            Item::new("x", "x", Kind::Launcher).icon_name(),
            "application-x-executable"
        );
        assert_eq!(
            Item::new("x", "x", Kind::File).icon_name(),
            "text-x-generic"
        );
        assert!(Kind::Launcher.is_launchable());
        assert!(Kind::Directory.is_folder());
        assert!(!Kind::File.is_folder());
    }

    #[test]
    fn a_model_round_trips_and_omits_what_a_renderer_does_not_need() {
        let mut m = model();
        m.thumbnails = true;
        let mut i = Item::new("notes.txt", "Notes", Kind::File);
        i.at = Some(Cell::new(0, DesktopSlot { col: 0, row: 0 }));
        m.push(i).unwrap();
        let text = serde_json::to_string(&m).unwrap();
        assert!(
            !text.contains("tooltip"),
            "an absent tooltip must not cost bytes: {text}"
        );
        assert!(
            !text.contains("\"icon\""),
            "an empty icon must not cost bytes: {text}"
        );
        let back: Model = serde_json::from_str(&text).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.encoded_len(), text.len());
    }

    #[test]
    fn a_model_refuses_a_field_it_does_not_know() {
        let text = r#"{"version":1,"generation":0,"desktop":"/d","writable":true,
            "thumbnails":false,"thumbnail_size":128,"monitors":[],"items":[],
            "wallpaper":"lake.jpg"}"#;
        let err = serde_json::from_str::<Model>(text).unwrap_err();
        assert!(
            err.to_string().contains("wallpaper"),
            "the message should name the field: {err}"
        );
    }

    #[test]
    fn a_minimal_model_is_a_version_and_nothing_else() {
        let m: Model = serde_json::from_str(
            r#"{"version":1,"generation":0,"desktop":"/d","monitors":[],"items":[]}"#,
        )
        .unwrap();
        assert!(!m.writable, "a payload that omits it is read-only");
        assert!(!m.thumbnails);
        assert_eq!(
            m.thumbnail_size, DEFAULT_THUMBNAIL,
            "a missing size must not mean a zero-sized thumbnail"
        );
    }

    #[test]
    fn an_oversized_model_is_reported_rather_than_sent_and_truncated() {
        let mut m = model();
        for i in 0..50 {
            let mut item = item(&format!("f{i}"));
            item.tooltip = Some("x".repeat(200));
            m.push(item).unwrap();
        }
        assert!(m.check_size(1024).is_err());
        assert!(m.check_size(1024 * 1024).is_ok());
    }

    #[test]
    fn free_cells_counts_every_monitor() {
        let mut m = Model::new(Path::new("/d"));
        m.set_geometry(vec![grid(0, 3, 2), grid(1, 2, 2)]).unwrap();
        assert_eq!(m.free_cells(), 6 + 4);
        m.push(item("a")).unwrap();
        m.place("a", Cell::new(1, DesktopSlot { col: 0, row: 0 }))
            .unwrap();
        assert_eq!(m.free_cells(), 6 + 3);
    }

    #[test]
    fn an_error_names_the_thing_that_is_wrong() {
        let e = ModelError::BadId {
            id: "../x".into(),
            why: "must be one path component",
        };
        assert!(e.to_string().contains("../x"));
        let err: ufm_core::Error = ModelError::UnknownItem { id: "q".into() }.into();
        assert!(matches!(err, ufm_core::Error::UnsafeComponent(_)));
        let err: ufm_core::Error = ModelError::NoRoom { monitor: 1 }.into();
        assert!(matches!(err, ufm_core::Error::Protocol(_)));
    }
}
