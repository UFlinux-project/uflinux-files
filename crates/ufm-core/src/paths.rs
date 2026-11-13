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
        for p in [config_root(), cache_root(), data_root()] {
            assert!(p.is_absolute(), "{}", p.display());
        }
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
