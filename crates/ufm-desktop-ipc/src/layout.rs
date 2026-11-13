//! The saved desktop layout: which cell each icon was in, and the grids.
//!
//! The provider owns this file, not the UI's preferences. Two reasons, both
//! practical. The provider is a separate process that answers a geometry report
//! and a move, and it should not have to read a window's preferences — with
//! their view modes, sort keys and column widths — to know where an icon goes.
//! And a slot needs a monitor to mean anything, which a layout keyed by name
//! alone cannot express.
//!
//! The file is written whole, atomically, at `0600`: it is not secret, but it is
//! state, and a world-readable file in `$XDG_CONFIG_HOME` is a smell. Writing it
//! whole is affordable because it is a few hundred bytes per icon.
//!
//! [`Layout::apply`] is where a saved layout meets a directory that has changed
//! since: an icon that is gone loses its cell, an icon that is new gets one from
//! the free-cell scan, and an icon that is still there goes back where it was.
//! That asymmetry is the whole point — the layout is a cache of the user's
//! intent, not a record of what files exist.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use ufm_core::atomic;
use ufm_core::config::DesktopSlot;

use crate::model::{Cell, Geometry, MAX_MONITORS, Model};
use crate::wire::PROTOCOL_VERSION;

/// Where the layout is saved: `<config dir>/desktop.json`.
///
/// A separate file from the preferences, so clearing UI settings does not
/// scramble the desktop and vice versa.
#[must_use]
pub fn path() -> std::path::PathBuf {
    ufm_core::paths::config_dir().join("desktop.json")
}

/// The saved layout.
///
/// A [`BTreeMap`] keyed by item id, so the file is byte-identical after a
/// round trip and a diff of two versions says something meaningful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub version: u32,
    /// Carried across restarts, so a renderer that compares generations never
    /// sees one go backwards after the provider is restarted.
    pub generation: u64,
    #[serde(default)]
    pub monitors: Vec<Geometry>,
    #[serde(default)]
    pub slots: BTreeMap<String, Cell>,
    /// Remembered so a provider that starts before the renderer has spoken can
    /// still answer `get-model` with the last known grids.
    #[serde(default = "default_true")]
    pub writable: bool,
    #[serde(default)]
    pub thumbnails: bool,
    #[serde(default = "default_size")]
    pub thumbnail_size: u32,
}

fn default_true() -> bool {
    true
}

fn default_size() -> u32 {
    crate::model::DEFAULT_THUMBNAIL
}

/// The layout a provider starts with when its file is missing. Written by hand
/// rather than derived, because `#[serde(default = "default_true")]` only
/// applies while deserializing: a derived `Default` would say the desktop is
/// read-only, and a missing file must not mean the icons cannot be moved.
impl Default for Layout {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
            generation: 0,
            monitors: Vec::new(),
            slots: BTreeMap::new(),
            writable: true,
            thumbnails: false,
            thumbnail_size: crate::model::DEFAULT_THUMBNAIL,
        }
    }
}

/// What [`Layout::apply`] did, so a caller can decide whether to announce a
/// change without comparing two models.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Icons that went back to their saved cell.
    pub restored: usize,
    /// Icons that had no saved cell and were given a free one.
    pub placed: usize,
    /// Icons that had nowhere to go: no grid, or a grid that is full.
    pub homeless: Vec<String>,
    /// Saved cells belonging to ids no longer on the desktop.
    pub forgotten: usize,
}

impl Applied {
    /// Whether anything a renderer paints actually moved.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.restored != 0 || self.placed != 0 || !self.homeless.is_empty()
    }
}

impl Layout {
    /// An empty layout for a fresh provider.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a layout. A missing file is the empty layout; a file that exists but
    /// does not parse is an error, because quietly starting over would move every
    /// icon on the user's desktop.
    pub fn load(path: &Path) -> ufm_core::Result<Self> {
        let mut layout: Self = atomic::read_json(path)?;
        if layout.version == 0 {
            layout.version = PROTOCOL_VERSION;
        }
        layout.normalize();
        Ok(layout)
    }

    /// Write the layout atomically.
    pub fn save(&self, path: &Path) -> ufm_core::Result<()> {
        atomic::write_json(path, self)
    }

    /// Drop grids that cannot describe a desktop and cells that point at a grid
    /// that is not there.
    ///
    /// A hand-edited or half-migrated file is repaired rather than rejected: the
    /// alternative is refusing to show a desktop because one line of a cache file
    /// is wrong.
    pub fn normalize(&mut self) {
        self.monitors.retain(Geometry::usable);
        self.monitors.sort_by_key(|g| g.monitor);
        self.monitors.dedup_by(|a, b| a.monitor == b.monitor);
        if self.monitors.len() > MAX_MONITORS {
            self.monitors.truncate(MAX_MONITORS);
        }
        self.slots.retain(|_, cell| {
            self.monitors
                .iter()
                .any(|g| g.monitor == cell.monitor && g.fits(cell.slot()))
        });
        if self.thumbnail_size == 0 {
            self.thumbnail_size = crate::model::DEFAULT_THUMBNAIL;
        }
    }

    /// Take the current placements and grids from a model, to be saved.
    #[must_use]
    pub fn capture(model: &Model) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            generation: model.generation,
            monitors: model.monitors.clone(),
            slots: model
                .items
                .iter()
                .filter_map(|i| i.at.map(|at| (i.id.clone(), at)))
                .collect(),
            writable: model.writable,
            thumbnails: model.thumbnails,
            thumbnail_size: model.thumbnail_size,
        }
    }

    /// Remember one placement without touching the rest.
    pub fn set(&mut self, id: &str, cell: Cell) {
        self.slots.insert(id.to_string(), cell);
    }

    /// Forget one placement, for an item that was deleted or moved off.
    pub fn forget(&mut self, id: &str) -> bool {
        self.slots.remove(id).is_some()
    }

    /// Replace the grids, dropping any cell that no longer fits.
    ///
    /// Returns the ids left without a cell, which is what a caller reports as
    /// "some icons could not be placed".
    pub fn set_geometry(&mut self, monitors: Vec<Geometry>) -> Vec<String> {
        let before: BTreeSet<String> = self.slots.keys().cloned().collect();
        self.monitors = monitors;
        self.normalize();
        let after: BTreeSet<String> = self.slots.keys().cloned().collect();
        before.difference(&after).cloned().collect()
    }

    /// Put a model's items where the layout says, giving a free cell to anything
    /// the layout has never seen.
    ///
    /// The order matters: saved cells are honoured first, so a new icon cannot
    /// take a cell an existing icon will return to, and only then are the new
    /// icons placed, in the model's own order, which is directory order.
    pub fn apply(&self, model: &mut Model) -> Applied {
        let mut out = Applied::default();
        let live = model
            .items
            .iter()
            .map(|i| i.id.clone())
            .collect::<BTreeSet<_>>();
        out.forgotten = self.slots.keys().filter(|id| !live.contains(*id)).count();
        let mut fresh = Vec::new();
        for index in 0..model.items.len() {
            let id = model.items[index].id.clone();
            match self.slots.get(&id).copied() {
                Some(cell) if model.geometry(cell.monitor).is_some() => {
                    model.items[index].at = Some(cell);
                    out.restored += 1;
                }
                _ => fresh.push(index),
            }
        }
        for index in fresh {
            let id = model.items[index].id.clone();
            // The shell's first grid is the primary screen, which is where an
            // icon belongs when nothing says otherwise.
            let preferred = model.monitors.first().map_or(0, |g| g.monitor);
            match model.first_free_anywhere(preferred) {
                Some(cell) => {
                    model.items[index].at = Some(cell);
                    out.placed += 1;
                }
                None => out.homeless.push(id),
            }
        }
        // Two items whose saved cells collide, or a cell that was valid when
        // saved and is not any more, must not be painted on top of each other.
        let lost = model.repack();
        for id in lost {
            if !out.homeless.contains(&id) {
                out.homeless.push(id);
            }
        }
        out.homeless.sort();
        out.homeless.dedup();
        out
    }

    /// The saved cell for one id.
    #[must_use]
    pub fn cell_of(&self, id: &str) -> Option<Cell> {
        self.slots.get(id).copied()
    }

    /// The saved slot for one id, ignoring which monitor it was on.
    #[must_use]
    pub fn slot_of(&self, id: &str) -> Option<DesktopSlot> {
        self.slots.get(id).copied().map(Cell::slot)
    }

    #[must_use]
    pub fn geometry(&self, monitor: u8) -> Option<&Geometry> {
        self.monitors.iter().find(|g| g.monitor == monitor)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Item, Kind};
    use std::fs;
    use std::path::PathBuf;

    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("ufm-layout-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir.join("desktop.json"))
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn grid(monitor: u8, columns: u16, rows: u16) -> Geometry {
        Geometry::new(monitor, columns, rows, 96, 96)
    }

    fn model(ids: &[&str]) -> Model {
        let mut m = Model::new(Path::new("/home/u/Desktop"));
        m.set_geometry(vec![grid(0, 4, 4)]).unwrap();
        for id in ids {
            m.push(Item::new(*id, *id, Kind::File)).unwrap();
        }
        m
    }

    fn cell(col: u16, row: u16) -> Cell {
        Cell::new(0, DesktopSlot { col, row })
    }

    #[test]
    fn a_missing_file_is_the_empty_layout_not_an_error() {
        let temp = Temp::new("missing");
        let layout = Layout::load(&temp.0).unwrap();
        assert_eq!(layout, Layout::new());
        assert!(layout.is_empty());
        assert_eq!(layout.version, PROTOCOL_VERSION);
        assert!(layout.writable);
        assert_eq!(layout.thumbnail_size, crate::model::DEFAULT_THUMBNAIL);
    }

    #[test]
    fn a_layout_round_trips_through_a_file() {
        let temp = Temp::new("round");
        let mut layout = Layout::new();
        layout.generation = 9;
        layout.monitors = vec![grid(0, 6, 4), grid(1, 3, 3)];
        layout.set("a", cell(1, 2));
        layout.set("b", Cell::new(1, DesktopSlot { col: 0, row: 1 }));
        layout.thumbnails = true;
        layout.save(&temp.0).unwrap();
        let back = Layout::load(&temp.0).unwrap();
        assert_eq!(back, layout);
        assert_eq!(back.cell_of("a"), Some(cell(1, 2)));
        assert_eq!(back.slot_of("a"), Some(DesktopSlot { col: 1, row: 2 }));
        assert_eq!(back.len(), 2);
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_not_a_reset() {
        let temp = Temp::new("broken");
        fs::write(&temp.0, "{ this is not json").unwrap();
        assert!(Layout::load(&temp.0).is_err());
    }

    #[test]
    fn a_saved_file_is_written_once_and_is_valid_json() {
        let temp = Temp::new("atomic");
        let mut layout = Layout::new();
        layout.monitors = vec![grid(0, 4, 4)];
        layout.set("a", cell(0, 0));
        layout.save(&temp.0).unwrap();
        layout.set("b", cell(1, 0));
        layout.save(&temp.0).unwrap();
        let text = fs::read_to_string(&temp.0).unwrap();
        assert_eq!(text.matches("\"version\"").count(), 1);
        assert!(text.ends_with('\n'), "a text file should end in a newline");
        let leftovers = fs::read_dir(temp.0.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(leftovers, 0, "a temp file was left behind");
    }

    #[test]
    fn an_unknown_field_in_the_saved_file_is_refused() {
        let temp = Temp::new("unknown");
        fs::write(
            &temp.0,
            r#"{"version":1,"generation":0,"slots":{},"wallpaper":"lake.jpg"}"#,
        )
        .unwrap();
        let err = Layout::load(&temp.0).unwrap_err();
        assert!(
            err.to_string().contains("wallpaper"),
            "the message should name the field: {err}"
        );
    }

    #[test]
    fn a_hand_edited_file_is_repaired_rather_than_refused() {
        let temp = Temp::new("repair");
        fs::write(
            &temp.0,
            r#"{"version":0,"generation":3,"monitors":[{"monitor":0,"columns":2,"rows":2,"cell_w":96,"cell_h":96},
            {"monitor":0,"columns":9,"rows":9,"cell_w":0,"cell_h":0},
            {"monitor":7,"columns":0,"rows":0,"cell_w":96,"cell_h":96}],
            "slots":{"a":{"monitor":0,"col":1,"row":1},"gone":{"monitor":4,"col":0,"row":0},
            "outside":{"monitor":0,"col":9,"row":0}},"thumbnail_size":0}"#,
        )
        .unwrap();
        let layout = Layout::load(&temp.0).unwrap();
        assert_eq!(
            layout.version, PROTOCOL_VERSION,
            "a zero version is filled in"
        );
        assert_eq!(layout.thumbnail_size, crate::model::DEFAULT_THUMBNAIL);
        assert_eq!(layout.monitors.len(), 1, "only the usable grid survives");
        assert_eq!(
            layout.slots.keys().cloned().collect::<Vec<_>>(),
            vec!["a".to_string()],
            "a cell on no grid or outside its grid is dropped"
        );
    }

    #[test]
    fn a_duplicate_grid_keeps_the_first_one() {
        let temp = Temp::new("dupgrid");
        fs::write(
            &temp.0,
            r#"{"version":1,"generation":0,"monitors":[
            {"monitor":0,"columns":2,"rows":2,"cell_w":96,"cell_h":96},
            {"monitor":0,"columns":8,"rows":8,"cell_w":96,"cell_h":96}],
            "slots":{"a":{"monitor":0,"col":1,"row":1}},"writable":true,"thumbnails":false,
            "thumbnail_size":128}"#,
        )
        .unwrap();
        let layout = Layout::load(&temp.0).unwrap();
        assert_eq!(layout.monitors.len(), 1);
        assert_eq!(layout.geometry(0).unwrap().columns, 2);
        assert_eq!(layout.cell_of("a"), Some(cell(1, 1)));
    }

    #[test]
    fn capture_and_apply_are_inverse_enough_to_survive_a_restart() {
        let mut m = model(&["a", "b", "c"]);
        m.place("a", cell(3, 3)).unwrap();
        m.place("b", cell(0, 1)).unwrap();
        m.generation = 5;
        let layout = Layout::capture(&m);
        assert_eq!(layout.generation, 5);
        assert_eq!(layout.len(), 2, "an icon with no cell is not saved");

        let mut fresh = model(&["a", "b", "c"]);
        let applied = layout.apply(&mut fresh);
        assert_eq!(
            applied,
            Applied {
                restored: 2,
                placed: 1,
                homeless: Vec::new(),
                forgotten: 0,
            }
        );
        assert_eq!(fresh.cell_of("a"), Some(cell(3, 3)));
        assert_eq!(fresh.cell_of("b"), Some(cell(0, 1)));
        assert_eq!(
            fresh.cell_of("c"),
            Some(cell(0, 0)),
            "a new icon takes the first free cell, not a taken one"
        );
        assert!(applied.changed());
    }

    #[test]
    fn a_saved_cell_never_reserves_a_spot_for_a_deleted_icon() {
        let mut m = model(&["a", "b"]);
        m.place("a", cell(0, 0)).unwrap();
        m.place("b", cell(1, 0)).unwrap();
        let layout = Layout::capture(&m);

        // "a" was deleted; "c" was created elsewhere and appears now.
        let mut next = model(&["b", "c"]);
        let applied = layout.apply(&mut next);
        assert_eq!(applied.restored, 1);
        assert_eq!(applied.placed, 1);
        assert_eq!(
            applied.forgotten, 1,
            "the stale cell is reported as forgotten"
        );
        assert_eq!(next.cell_of("b"), Some(cell(1, 0)));
        assert_eq!(next.cell_of("c"), Some(cell(0, 0)));
    }

    #[test]
    fn colliding_saved_cells_are_repacked_not_stacked() {
        let temp = Temp::new("collide");
        let mut layout = Layout::new();
        layout.monitors = vec![grid(0, 2, 2)];
        layout.set("a", cell(1, 1));
        layout.set("b", cell(1, 1));
        layout.save(&temp.0).unwrap();
        let layout = Layout::load(&temp.0).unwrap();
        let mut m = model(&["a", "b"]);
        let applied = layout.apply(&mut m);
        assert_eq!(m.cell_of("a"), Some(cell(1, 1)));
        assert_ne!(m.cell_of("b"), Some(cell(1, 1)));
        assert_eq!(m.validate(), Ok(()));
        assert_eq!(applied.restored, 2, "both were honoured from the file");
        let saved = Layout::capture(&m);
        let cells: BTreeSet<Cell> = saved.slots.values().copied().collect();
        assert_eq!(
            cells.len(),
            saved.len(),
            "a collision must not survive into the next save"
        );
    }

    #[test]
    fn with_no_grid_at_all_everything_is_homeless() {
        let mut m = Model::new(Path::new("/home/u/Desktop"));
        m.push(Item::new("a", "a", Kind::File)).unwrap();
        let applied = Layout::new().apply(&mut m);
        assert_eq!(applied.placed, 0);
        assert_eq!(applied.homeless, vec!["a".to_string()]);
        assert_eq!(m.cell_of("a"), None);
    }

    #[test]
    fn a_full_grid_leaves_a_new_icon_homeless_rather_than_burying_one() {
        let mut m = model(&["a", "b"]);
        m.set_geometry(vec![grid(0, 1, 1)]).unwrap();
        m.place("a", cell(0, 0)).unwrap();
        let layout = Layout::capture(&m);
        let mut next = model(&["a", "b"]);
        next.set_geometry(vec![grid(0, 1, 1)]).unwrap();
        let applied = layout.apply(&mut next);
        assert_eq!(applied.restored, 1);
        assert_eq!(applied.homeless, vec!["b".to_string()]);
        assert_eq!(next.cell_of("a"), Some(cell(0, 0)));
    }

    #[test]
    fn setting_geometry_forgets_cells_that_no_longer_fit() {
        let mut layout = Layout::new();
        layout.monitors = vec![grid(0, 4, 4), grid(1, 2, 2)];
        layout.set("a", cell(3, 3));
        layout.set("b", Cell::new(1, DesktopSlot { col: 1, row: 1 }));
        let dropped = layout.set_geometry(vec![grid(1, 2, 2)]);
        assert_eq!(layout.monitors.len(), 1);
        assert_eq!(layout.cell_of("a"), None);
        assert!(
            dropped.contains(&"a".to_string()),
            "the caller must be able to report the icon it could not place"
        );
        assert_eq!(
            layout.cell_of("b"),
            Some(Cell::new(1, DesktopSlot { col: 1, row: 1 }))
        );
    }

    #[test]
    fn forgetting_one_placement_leaves_the_others() {
        let mut layout = Layout::new();
        layout.monitors = vec![grid(0, 4, 4)];
        layout.set("a", cell(0, 0));
        layout.set("b", cell(1, 0));
        assert!(layout.forget("a"));
        assert!(!layout.forget("nope"));
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.cell_of("b"), Some(cell(1, 0)));
    }

    #[test]
    fn the_saved_path_lives_with_the_other_state() {
        let p = path();
        assert!(p.is_absolute());
        assert_eq!(p.file_name().unwrap(), "desktop.json");
    }
}
