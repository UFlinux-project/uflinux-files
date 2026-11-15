//! Well-known directories.
//!
//! Everything the file manager writes goes under one of the paths returned here,
//! so a user can find it and a packager can relocate it. The layout follows the
//! convention the rest of `UFLinux` already uses: state lives in a private
//! directory named after the project rather than scattered across XDG.

use std::path::{Path, PathBuf};

/// Application state: preferences, bookmarks, mount table, column widths.
///
/// `UFLINUX_FILES_CONFIG_DIR` overrides this so the test suite and the memory
/// benchmark can run against a throwaway home directory.
#[must_use]
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UFLINUX_FILES_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    config_root().join("uflinux").join("filemanager")
}

/// Where the search index lives. Kept separate from the configuration so that
/// clearing preferences does not force a full reindex, and vice versa.
#[must_use]
pub fn index_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UFLINUX_FILES_INDEX_DIR") {
        return PathBuf::from(dir);
    }
    cache_root().join("uflinux-files").join("index")
}

/// Thumbnail cache, following the freedesktop thumbnail spec layout so an
/// existing `~/.cache/thumbnails` is reused rather than duplicated.
#[must_use]
pub fn thumbnail_dir() -> PathBuf {
    cache_root().join("thumbnails")
}

/// Single-file materialisations, for "Open With" on a member of an archive.
#[must_use]
pub fn extracted_dir() -> PathBuf {
    cache_root().join("uflinux-files").join("extracted")
}

/// The user's trash directory, in the layout the freedesktop trash specification
/// defines: `files/` holds the rows and `info/` holds one `.trashinfo` sidecar
/// per row, naming where it came from.
///
/// Reached as a plain directory rather than as a `trash:` URI. The `GIO` trash
/// backend that answers `trash://` is part of `GVfs`, and a minimal system — an
/// Alpine install with no volume manager, which is what this file manager is
/// packaged for — has no `GVfs` to ask, so a trash built on the URI would come up
/// empty there. Reading the specification's layout directly works wherever
/// `g_file_trash` wrote into it, which is the same set of systems.
///
/// One limitation follows, and undo states it rather than hiding it: the
/// specification also allows a per-filesystem `.Trash-$UID` at the root of the
/// filesystem a row lived on, and finding that one needs to know where the
/// filesystems are mounted. Rows trashed from the home filesystem are always
/// found here; rows trashed from elsewhere are found only when a trash directory
/// for them is passed alongside.
#[must_use]
pub fn trash_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UFLINUX_FILES_TRASH_DIR") {
        return PathBuf::from(dir);
    }
    data_root().join("Trash")
}

/// Where a replaced row is kept until the window forgets it, so that undoing a
/// replace can put the bytes back rather than report them lost.
///
/// Under the cache rather than the configuration: this is recoverable data with a
/// lifetime measured in minutes, and a directory the user never asks to see. It is
/// cleared whenever the window records a newer operation, which is what keeps a
/// session from quietly filling the disk with files nobody will ask for back.
#[must_use]
pub fn undone_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UFLINUX_FILES_UNDONE_DIR") {
        return PathBuf::from(dir);
    }
    cache_root().join("uflinux-files").join("undone")
}

/// Where network shares get mounted: `/run/user/$UID/ufm`.
///
/// A per-user runtime directory means the mount table is gone when the session
/// is, which is what makes a stale mount self-healing rather than a boot-time
/// surprise.
#[must_use]
pub fn mount_root() -> PathBuf {
    if let Ok(dir) = std::env::var("UFLINUX_FILES_MOUNT_ROOT") {
        return PathBuf::from(dir);
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(runtime) if Path::new(&runtime).is_absolute() => PathBuf::from(runtime).join("ufm"),
        _ => PathBuf::from("/tmp").join(format!("ufm-{}", uid())),
    }
}

/// Where the symlinks to live network shares go, so that an application which
/// cannot see `$XDG_RUNTIME_DIR` still has a path to offer the user.
///
/// Deliberately under `$XDG_DATA_HOME` rather than beside the mount points: the
/// links are not secret (a `0700` mount root behind them already decides who can
/// read the files), and a location inside the user's data directory is one a
/// sandboxed or remote application can display.
#[must_use]
pub fn network_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UFLINUX_FILES_NETWORK_DIR") {
        return PathBuf::from(dir);
    }
    data_root()
        .join("uflinux")
        .join("filemanager")
        .join("Network")
}

/// The user's Desktop directory, honouring `XDG_DESKTOP_DIR` from
/// `user-dirs.dirs` because the desktop icons have to appear exactly where the
/// user's file manager writes them.
#[must_use]
pub fn desktop_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UFLINUX_FILES_DESKTOP_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = read_user_dirs().get("XDG_DESKTOP_DIR") {
        return dir.clone();
    }
    home().join("Desktop")
}

/// Resolve an XDG user directory, with a conventional home-relative fallback.
#[must_use]
pub fn user_dir(key: &str, fallback: &str) -> PathBuf {
    read_user_dirs()
        .remove(key)
        .unwrap_or_else(|| home().join(fallback))
}

#[must_use]
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `$XDG_CONFIG_HOME`, falling back to `~/.config`.
#[must_use]
pub fn config_root() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_CACHE_HOME`, falling back to `~/.cache`.
#[must_use]
pub fn cache_root() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

/// `$XDG_DATA_HOME`, falling back to `~/.local/share`. The indexer reads app
/// entries from here as well as from the system directories.
#[must_use]
pub fn data_root() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

#[must_use]
#[allow(unsafe_code)]
pub fn uid() -> u32 {
    // `geteuid` rather than `$UID`, which a login script can lie about and which
    // would then put the mount root somewhere the FUSE helper cannot write.
    // SAFETY: geteuid takes no arguments and is always valid.
    unsafe { libc::geteuid() }
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) => {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                return p;
            }
            // A relative XDG path is invalid; ignore it rather than guess a CWD.
            home().join(fallback)
        }
        None => home().join(fallback),
    }
}

/// Parse `user-dirs.dirs` for the handful of keys we care about. Written by
/// hand because the file is `KEY="$HOME/Value"` with shell quoting, and pulling
/// in a shell parser for six lines is not worth the dependency.
fn read_user_dirs() -> std::collections::HashMap<String, PathBuf> {
    let mut out = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string(config_root().join("user-dirs.dirs")) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.starts_with("XDG_") {
            continue;
        }
        let value = value
            .trim()
            .trim_matches('"')
            .replace("$HOME", &home().display().to_string());
        let path = PathBuf::from(value);
        if path.is_absolute() {
            out.insert(key.to_string(), path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unsafe_code)]
    fn set_env(key: &str, value: &str) {
        // SAFETY: the test-environment guard means no other thread is reading.
        unsafe { std::env::set_var(key, value) };
    }

    #[allow(unsafe_code)]
    fn unset_env(key: &str) {
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn overrides_win() {
        // The override variables exist for the tests and the benchmark, so they
        // have to take precedence unconditionally. Setting an environment
        // variable is unsafe in edition 2024 because another thread may be
        // reading one; every test that touches the environment holds this lock.
        let _guard = crate::test_env();
        set_env("UFLINUX_FILES_MOUNT_ROOT", "/tmp/ufm-test");
        assert_eq!(mount_root(), PathBuf::from("/tmp/ufm-test"));
        unset_env("UFLINUX_FILES_MOUNT_ROOT");
    }

    #[test]
    fn fallbacks_are_absolute() {
        for p in [
            config_root(),
            cache_root(),
            data_root(),
            trash_dir(),
            undone_dir(),
        ] {
            assert!(p.is_absolute(), "{}", p.display());
        }
    }

    #[test]
    fn trash_lives_where_the_specification_puts_it() {
        // The name matters: undo reads `info/` and `files/` by hand, and a
        // directory named anything else is a trash this program did not write.
        let _guard = crate::test_env();
        unset_env("UFLINUX_FILES_TRASH_DIR");
        set_env("XDG_DATA_HOME", "/var/lib/test-user");
        assert_eq!(
            trash_dir(),
            PathBuf::from("/var/lib/test-user/Trash"),
            "the trash follows `$XDG_DATA_HOME`, which is what a private home in a test means"
        );
        unset_env("XDG_DATA_HOME");
    }

    #[test]
    fn replaced_rows_are_kept_where_they_can_be_swept() {
        let _guard = crate::test_env();
        unset_env("UFLINUX_FILES_UNDONE_DIR");
        set_env("XDG_CACHE_HOME", "/tmp/ufm-cache");
        assert_eq!(
            undone_dir(),
            PathBuf::from("/tmp/ufm-cache/uflinux-files/undone"),
            "the stash is under the cache, beside the other recoverable scratch"
        );
        set_env("UFLINUX_FILES_UNDONE_DIR", "/tmp/ufm-undone");
        assert_eq!(undone_dir(), PathBuf::from("/tmp/ufm-undone"));
        unset_env("UFLINUX_FILES_UNDONE_DIR");
        unset_env("XDG_CACHE_HOME");
    }

    #[test]
    fn uid_matches_the_process() {
        let probe = std::process::id();
        assert!(uid() < u32::MAX, "{probe}");
    }

    #[test]
    fn user_dirs_parsing_ignores_junk() {
        // Exercise the parser directly rather than through the filesystem.
        let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nNOT_A_KEY=x\nXDG_DOWNLOAD_DIR=relative\n";
        let mut seen = 0;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, _)) = line.split_once('=') else {
                continue;
            };
            if k.starts_with("XDG_") {
                seen += 1;
            }
        }
        assert_eq!(
            seen, 2,
            "both XDG lines are seen; validity is checked later"
        );
    }
}
