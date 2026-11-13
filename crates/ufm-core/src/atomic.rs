//! Crash-safe writes for the small JSON files the application keeps.
//!
//! Every state file is rewritten whole, so the only requirement is that a reader
//! never sees a half-written file and that a crash during a write does not
//! destroy the previous contents. Write to a sibling, fsync, rename, fsync the
//! directory — the standard sequence, and cheap at these sizes.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Write `bytes` to `path` atomically, creating parent directories.
///
/// `mode` is applied to the temporary file before the rename, so a file that
/// must be `0600` — anything holding a credential — never exists world-readable
/// even briefly.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(path);
    {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let file = options.open(&tmp)?;
        #[cfg(not(unix))]
        {
            let _ = mode;
        }
        let mut writer = BufWriter::new(file);
        writer.write_all(bytes)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap_or(Path::new(".")));
    Ok(())
}

/// Serialize `value` as pretty JSON and write it atomically as `0600`, which is
/// the conservative default for anything under the user's control.
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value).map_err(Error::from)?;
    text.push('\n');
    write_atomic(path, text.as_bytes(), 0o600)
}

/// Read a JSON file, treating a missing file as the default value. A file that
/// exists but does not parse is an error rather than a silent reset: losing the
/// user's bookmarks to a truncated write should be visible, not quiet.
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(T::default()),
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(Error::from(e)),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "state".to_string(), |s| s.to_string_lossy().into_owned());
    // Include the pid so two processes writing the same file do not collide on
    // one temporary path and overwrite each other mid-write.
    let mut tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    // A nanosecond suffix keeps a retry distinct after a failed rename.
    if let Ok(n) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        tmp.set_extension(format!("tmp{}", n.subsec_nanos()));
    }
    tmp
}

/// Best-effort directory fsync. The rename is already durable enough for a file
/// manager; failing here should never fail the write.
fn sync_dir(dir: &Path) {
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("ufm-atomic-{}-{name}", std::process::id()));
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

    #[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug)]
    struct State {
        bookmarks: Vec<String>,
    }

    #[test]
    fn round_trip_creates_parents() {
        let t = Temp::new("round");
        let path = t.0.join("nested/deeper/state.json");
        let state = State {
            bookmarks: vec!["/home/u/Docs".into()],
        };
        write_json(&path, &state).unwrap();
        assert_eq!(read_json::<State>(&path).unwrap(), state);
    }

    #[test]
    fn missing_file_reads_as_default() {
        let t = Temp::new("missing");
        let path = t.0.join("nope.json");
        assert_eq!(read_json::<State>(&path).unwrap(), State::default());
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_reset() {
        let t = Temp::new("corrupt");
        let path = t.0.join("state.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(matches!(read_json::<State>(&path), Err(Error::Config(_))));
    }

    #[test]
    fn empty_file_reads_as_default() {
        let t = Temp::new("empty");
        let path = t.0.join("state.json");
        std::fs::write(&path, b"   \n").unwrap();
        assert_eq!(read_json::<State>(&path).unwrap(), State::default());
    }

    #[test]
    fn rewrite_leaves_no_temporary_files_behind() {
        let t = Temp::new("sweep");
        let path = t.0.join("state.json");
        for i in 0..5 {
            write_json(
                &path,
                &State {
                    bookmarks: vec![format!("/{i}")],
                },
            )
            .unwrap();
        }
        let leftovers: Vec<_> = std::fs::read_dir(t.0.join(""))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path() != path)
            .collect();
        assert!(leftovers.is_empty(), "leftover: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn mode_is_applied() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new("mode");
        let path = t.0.join("secret.json");
        write_atomic(&path, b"{}", 0o600).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
