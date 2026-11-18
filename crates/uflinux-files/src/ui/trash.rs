//! The Trash collection. Sidecars are paired by their stored name, never origin,
//! so two deleted versions of the same file remain separately recoverable.
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use ufm_core::entry::Entry;
use ufm_core::{Location, paths};
use ufm_vfs::{Cancel, Readers};

#[derive(Debug, Clone)]
pub struct Item {
    pub stored: Location,
    pub original: PathBuf,
    pub deleted: String,
    pub info: PathBuf,
}

fn owned_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == paths::uid())
}

pub fn roots() -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut roots = vec![(paths::trash_dir(), None)];
    if let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") {
        for line in mounts.lines() {
            let Some(raw) = line.split_whitespace().nth(4) else {
                continue;
            };
            let mount = PathBuf::from(
                raw.replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\134", "\\"),
            );
            let uid = paths::uid();
            for root in [
                mount.join(format!(".Trash-{uid}")),
                mount.join(".Trash").join(uid.to_string()),
            ] {
                if owned_directory(&root) && !roots.iter().any(|(held, _)| *held == root) {
                    roots.push((root, Some(mount.clone())));
                }
            }
        }
    }
    roots
}

fn parse(root: &Path, mount: Option<&Path>, info: PathBuf) -> Option<Item> {
    let name = info.file_name()?.to_str()?.strip_suffix(".trashinfo")?;
    if name.is_empty() || fs::symlink_metadata(&info).ok()?.file_type().is_symlink() {
        return None;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&info)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 65_536 {
        return None;
    }
    let raw = bytes
        .split(|b| *b == b'\n')
        .find_map(|line| line.strip_prefix(b"Path="))?;
    let path = PathBuf::from(OsString::from_vec(
        percent_encoding::percent_decode(raw).collect(),
    ));
    let original = if path.is_absolute() {
        path
    } else {
        if path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        mount?.join(path)
    };
    if original.file_name().is_none() {
        return None;
    }
    let deleted = bytes
        .split(|b| *b == b'\n')
        .find_map(|line| line.strip_prefix(b"DeletionDate="))
        .map(|date| String::from_utf8_lossy(date).trim().to_string())
        .unwrap_or_default();
    let stored = Location::file(root.join("files").join(name));
    fs::symlink_metadata(stored.as_path()?).ok()?;
    Some(Item {
        stored,
        original,
        deleted,
        info,
    })
}

pub fn items_at(roots: &[(PathBuf, Option<PathBuf>)], cancel: &Cancel) -> Vec<Item> {
    let mut items = Vec::new();
    for (root, mount) in roots {
        if !owned_directory(root)
            || !owned_directory(&root.join("info"))
            || !owned_directory(&root.join("files"))
        {
            continue;
        }
        let Ok(infos) = fs::read_dir(root.join("info")) else {
            continue;
        };
        for info in infos.flatten() {
            if cancel.is_cancelled() {
                return items;
            }
            if let Some(item) = parse(root, mount.as_deref(), info.path()) {
                items.push(item);
            }
        }
    }
    items.sort_by(|a, b| b.deleted.cmp(&a.deleted));
    items
}

pub fn items(cancel: &Cancel) -> Vec<Item> {
    items_at(&roots(), cancel)
}

pub fn listing(readers: &Readers, cancel: &Cancel) -> Vec<Entry> {
    items(cancel)
        .into_iter()
        .filter_map(|item| {
            let mut entry = readers.metadata(&item.stored).ok()?;
            entry.name = item.original.file_name()?.to_string_lossy().into_owned();
            Some(entry)
        })
        .collect()
}

pub fn is_stored(location: &Location) -> bool {
    let Some(path) = location.as_path() else {
        return false;
    };
    let Some(files) = path.parent() else {
        return false;
    };
    if files.file_name().is_none_or(|name| name != "files") {
        return false;
    }
    let Some(root) = files.parent() else {
        return false;
    };
    root == paths::trash_dir()
        || root
            .file_name()
            .is_some_and(|name| name == format!(".Trash-{}", paths::uid()).as_str())
        || (root
            .file_name()
            .is_some_and(|name| name == paths::uid().to_string().as_str())
            && root
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".Trash"))
}

pub fn find(location: &Location) -> Option<Item> {
    items(&Cancel::new())
        .into_iter()
        .find(|item| item.stored == *location)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_origins_are_distinct_and_invalid_sidecars_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("files")).unwrap();
        fs::create_dir(root.path().join("info")).unwrap();
        for name in ["one", "two", "invalid"] {
            fs::write(root.path().join("files").join(name), b"contents").unwrap();
            fs::write(
                root.path().join("info").join(format!("{name}.trashinfo")),
                if name == "invalid" {
                    "[Trash Info]\nPath=../../escape"
                } else {
                    "[Trash Info]\nPath=/tmp/a%20b\nDeletionDate=2026-11-18T10:00:00"
                },
            )
            .unwrap();
        }
        fs::write(root.path().join("files/oversized"), b"keep").unwrap();
        let mut oversized = b"[Trash Info]\nPath=/tmp/oversized\n".to_vec();
        oversized.resize(65_537, b'x');
        fs::write(root.path().join("info/oversized.trashinfo"), oversized).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("info/one.trashinfo"),
            root.path().join("info/symlink.trashinfo"),
        )
        .unwrap();
        let items = items_at(&[(root.path().to_path_buf(), None)], &Cancel::new());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].original, Path::new("/tmp/a b"));
        assert_ne!(items[0].stored, items[1].stored);
        assert_eq!(items[0].deleted, "2026-11-18T10:00:00");
    }
}
