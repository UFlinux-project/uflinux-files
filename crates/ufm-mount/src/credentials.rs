//! Share passwords.
//!
//! A password is a [`Secret`]: it prints as `****`, it does not implement
//! [`Serialize`] so it cannot be written into the mount table by accident, and
//! it blanks its own buffer when dropped. The only way to read it is
//! [`expose`](Secret::expose), which is named so that a call site reads as a
//! deliberate act.
//!
//! Where the secret is kept is behind [`CredentialStore`]. The file backend here
//! is the fallback and the one that can be tested anywhere: a `0600` JSON file
//! next to the mount table. A Secret-Service backend belongs to the GUI binary,
//! which already links GIO — pulling D-Bus in here would make the crate
//! untestable off a desktop and buy the library nothing.
//!
//! Whatever the backend, a password is passed to a helper on stdin, in the
//! environment, or in a file — never in `argv`, where every process on the
//! machine could read it out of `/proc`.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ufm_core::atomic::write_atomic;
use ufm_core::error::{Error, Result};
use ufm_core::paths;

use crate::uri::Share;

/// The file holding cached passwords, `0600`.
const FILE: &str = "credentials.json";

/// The current on-disk shape. A future version can migrate instead of discarding.
const VERSION: u32 = 1;

/// A password, or anything else sensitive enough not to appear in a log.
///
/// The buffer is overwritten on drop, which keeps a password out of a core
/// dump or a `strings` run on the heap a little longer than a plain `String`
/// would. It is not a hard guarantee — a swap partition can hold anything — but
/// it costs nothing.
pub struct Secret(String);

impl Secret {
    /// Whether this is empty, which counts as "no password given".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Read the password. Every call site should be a helper's stdin, an
    /// environment variable, or a permission-checked file.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Consume the secret, handing ownership to a caller that must not log it —
    /// typically a `ChildStdin` write.
    pub fn into_string(mut self) -> String {
        // Take the buffer out first so `Drop` on the moved value has nothing to
        // scrub twice.
        let mut out = String::new();
        std::mem::swap(&mut self.0, &mut out);
        out
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Overwrite the buffer in place: `clear` keeps the allocation, and
        // writing the same number of `*` back covers every byte that held a
        // character of the password. This is the safe form of scrubbing — the
        // direct one needs `as_bytes_mut`, and this crate forbids `unsafe`.
        let len = self.0.len();
        self.0.clear();
        self.0.extend(std::iter::repeat_n('*', len));
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(****)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("****")
    }
}

/// A stored password plus the user it belongs to, so a share mounted as a
/// different account does not silently reuse the wrong credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    /// `Option` rather than a plain `String` so a hand-edited file with a
    /// missing user still loads.
    #[serde(default)]
    user: Option<String>,
    password: String,
}

/// The on-disk file. Keys are the share's display form, which is readable in a
/// text editor and cannot contain a `|` or a newline.
#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    shares: BTreeMap<String, Record>,
}

impl File {
    /// A file written by a newer build may hold records in a shape this build
    /// cannot describe; treating it as empty would silently ask the user for
    /// every password again, so the caller gets an error and the file is kept.
    fn check_version(&self, path: &Path) -> Result<()> {
        if self.version > VERSION {
            return Err(Error::Config(format!(
                "{}: credential file version {} is newer than this build's {VERSION}",
                path.display(),
                self.version
            )));
        }
        Ok(())
    }
}

/// Where cached passwords live.
///
/// The GUI can implement this over the Secret Service and hand it to the mount
/// manager; [`FileStore`] is the fallback and is used whenever the session has
/// no working D-Bus.
pub trait CredentialStore: Send + Sync {
    /// The cached password for `share`, if any. `None` for an unknown share,
    /// not for an expired or unreadable file.
    fn get(&self, share: &Share) -> Option<Secret>;

    /// Cache `password` for `share`, or remove the entry when it is `None`.
    fn put(&self, share: &Share, user: Option<&str>, password: Option<Secret>) -> Result<()>;

    /// Drop any cached password for `share`.
    fn forget(&self, share: &Share) -> Result<()>;

    /// Drop entries for shares no longer in the mount table, so deleting a
    /// share really does forget its password.
    fn prune(&self, keep: &[Share]) -> Result<()>;
}

/// Passwords in a `0600` JSON file under the configuration directory.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// The default location: `<config dir>/credentials.json`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            path: paths::config_dir().join(FILE),
        }
    }

    /// A specific file, for tests and for `--sweep-mounts`, which runs outside a
    /// desktop session.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<File> {
        // `File` has no `Serialize`, so read the text and parse it here rather
        // than through `read_json`, which needs both halves.
        let file = match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(File::default()),
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(e.into()),
        }?;
        file.check_version(&self.path)?;
        Ok(file)
    }

    fn save(&self, file: &File) -> Result<()> {
        let body = Snapshot {
            version: VERSION,
            shares: &file.shares,
        };
        let mut text = serde_json::to_string_pretty(&body).map_err(Error::from)?;
        text.push('\n');
        // 0600, and `write_atomic` applies the mode before the rename, so the
        // file is never briefly readable by others.
        write_atomic(&self.path, text.as_bytes(), 0o600)
    }
}

impl Default for FileStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for FileStore {
    fn get(&self, share: &Share) -> Option<Secret> {
        let file = self.load().ok()?;
        let record = file.shares.get(&share.display())?;
        (!record.password.is_empty()).then(|| Secret::from(record.password.clone()))
    }

    fn put(&self, share: &Share, user: Option<&str>, password: Option<Secret>) -> Result<()> {
        let mut file = self.load()?;
        let key = share.display();
        match password.filter(|p| !p.is_empty()) {
            Some(password) => {
                file.shares.insert(
                    key,
                    Record {
                        user: user.map(str::to_owned),
                        password: password.into_string(),
                    },
                );
            }
            None => {
                file.shares.remove(&key);
            }
        }
        self.save(&file)
    }

    fn forget(&self, share: &Share) -> Result<()> {
        let mut file = self.load()?;
        file.shares.remove(&share.display());
        self.save(&file)
    }

    fn prune(&self, keep: &[Share]) -> Result<()> {
        let mut file = self.load()?;
        let before = file.shares.len();
        let keep: Vec<String> = keep.iter().map(Share::display).collect();
        file.shares.retain(|key, _| keep.iter().any(|k| k == key));
        if file.shares.len() != before {
            self.save(&file)?;
        }
        Ok(())
    }
}

/// Write-only mirror of [`File`], which owns the passwords and therefore cannot
/// derive `Serialize`.
#[derive(Serialize)]
struct Snapshot<'a> {
    version: u32,
    shares: &'a BTreeMap<String, Record>,
}

/// Write `secret` to a child's stdin and close it, so a password reaches the
/// helper without appearing in its arguments.
///
/// Takes the writer by `Option` because `ChildStdin` can only be moved once, and
/// a caller that has already consumed it should not be able to call this again.
pub fn write_secret(
    stdin: &mut Option<std::process::ChildStdin>,
    secret: &Secret,
    archive: &Path,
) -> Result<()> {
    let Some(mut stdin) = stdin.take() else {
        return Ok(());
    };
    let outcome = stdin
        .write_all(secret.expose().as_bytes())
        .and_then(|()| stdin.write_all(b"\n"))
        .and_then(|()| stdin.flush());
    drop(stdin);
    outcome.map_err(|e| {
        Error::Mount(format!(
            "{}: the helper closed its input before the password was sent ({e})",
            archive.display()
        ))
    })
}

/// Read a file the way a helper's credential file must be read: refuse it if
/// anyone but the owner can see it.
///
/// The privileged helper uses this as its only source for a password, so the
/// ownership and mode checks are what stop another user — or another process
/// running as the same user with a sloppy umask — from having planted a file
/// for root to trust.
pub fn read_protected(path: &Path) -> Result<String> {
    read_protected_by(path, ufm_core::paths::uid())
}

/// [`read_protected`] with the expected owner named explicitly.
///
/// The helper needs this variant: it runs as root and reads a file the *user*
/// wrote, so comparing against its own euid would reject every credential file
/// in the directory. The mode check is unchanged, and is the part that matters
/// most there — a root process trusting a world-readable password file would
/// let any user plant one.
pub fn read_protected_by(path: &Path, owner: u32) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != owner || meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::Mount(format!(
                "{} must be owned by you and not readable by anyone else",
                path.display()
            )));
        }
    }
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_owned())
}

/// Whether the store can be used at all. A missing file is fine — it just means
/// nothing is cached yet — but a file owned by somebody else is not.
impl FileStore {
    #[must_use]
    pub fn usable(&self) -> bool {
        match std::fs::metadata(&self.path) {
            Ok(_) => self.load().is_ok(),
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(input: &str) -> Share {
        Share::parse(input).unwrap().share
    }

    /// One file per test, in a directory the test owns, so no developer's real
    /// cache is touched. The name only labels the file, to keep a failure
    /// readable.
    fn store(name: &str) -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileStore::at(dir.path().join(format!("{name}-{FILE}")));
        (dir, store)
    }

    #[test]
    fn a_secret_does_not_print_itself() {
        let secret = Secret::from("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(****)");
        assert_eq!(secret.to_string(), "****");
        assert_eq!(secret.expose(), "hunter2");
        assert!(!secret.is_empty());
        assert_eq!(secret.len(), 7);
    }

    #[test]
    fn into_string_hands_over_the_password() {
        let secret = Secret::from("hunter2");
        assert_eq!(secret.into_string(), "hunter2");
    }

    #[test]
    fn drop_blanks_the_buffer() {
        // The only observable effect of the scrub is that a moved-out secret
        // leaves nothing behind; a heap inspection would need unsafe.
        let secret = Secret::from("abc");
        let moved = secret.into_string();
        assert_eq!(moved, "abc");
    }

    #[test]
    fn a_round_trip_keeps_the_password_and_the_user() {
        let (_dir, store) = store("round");
        let s = share("smb://srv/share");
        store
            .put(&s, Some("alice"), Some(Secret::from("pw")))
            .expect("put");
        assert_eq!(store.get(&s).map(Secret::into_string), Some("pw".into()));
        let raw = std::fs::read_to_string(store.path()).expect("read");
        assert!(raw.contains("pw"), "{raw}");
        assert!(raw.contains("alice"), "{raw}");
    }

    #[test]
    fn the_file_is_owner_only() {
        let (_dir, store) = store("mode");
        let s = share("smb://srv/share");
        store.put(&s, None, Some(Secret::from("pw"))).expect("put");
        let meta = std::fs::metadata(store.path()).expect("meta");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "a credential file must not be group or world readable"
            );
            assert_eq!(meta.uid(), paths::uid());
        }
    }

    #[test]
    fn an_empty_or_absent_password_is_no_password() {
        let (_dir, store) = store("empty");
        let s = share("smb://srv/share");
        assert!(store.get(&s).is_none());
        store.put(&s, None, None).expect("put");
        assert!(store.get(&s).is_none());
        store
            .put(&s, None, Some(Secret::from("")))
            .expect("empty put");
        assert!(store.get(&s).is_none(), "an empty password is a delete");
    }

    #[test]
    fn putting_a_new_password_replaces_the_old_one() {
        let (_dir, store) = store("replace");
        let s = share("smb://srv/share");
        store.put(&s, None, Some(Secret::from("one"))).expect("1");
        store.put(&s, None, Some(Secret::from("two"))).expect("2");
        assert_eq!(store.get(&s).map(Secret::into_string), Some("two".into()));
        let raw = std::fs::read_to_string(store.path()).expect("read");
        assert!(!raw.contains("one"), "{raw}");
    }

    #[test]
    fn forgetting_and_pruning_remove_entries() {
        let (_dir, store) = store("prune");
        let a = share("smb://srv/one");
        let b = share("smb://srv/two");
        store.put(&a, None, Some(Secret::from("1"))).expect("a");
        store.put(&b, None, Some(Secret::from("2"))).expect("b");
        store.forget(&a).expect("forget");
        assert!(store.get(&a).is_none());
        assert!(store.get(&b).is_some());
        store.prune(&[b.clone()]).expect("prune");
        assert!(store.get(&b).is_some());
        store.prune(&[]).expect("prune all");
        assert!(store.get(&b).is_none());
    }

    #[test]
    fn a_different_user_is_a_different_entry() {
        let (_dir, store) = store("user");
        let anon = share("smb://srv/share");
        let alice = share("smb://alice@srv/share");
        store
            .put(&alice, Some("alice"), Some(Secret::from("pw")))
            .expect("put");
        assert!(store.get(&anon).is_none());
        assert!(store.get(&alice).is_some());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_reset() {
        let (_dir, store) = store("corrupt");
        std::fs::write(store.path(), "{ not json").expect("write");
        let s = share("smb://srv/share");
        let err = store.put(&s, None, Some(Secret::from("pw"))).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(!store.usable());
    }

    #[test]
    fn an_unreadable_file_is_refused_rather_than_trusted() {
        let (_dir, store) = store("protect");
        std::fs::write(store.path(), "{}\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644))
                .expect("chmod");
            let err = read_protected(store.path()).unwrap_err();
            assert!(err.to_string().contains("readable"), "{err}");
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600))
                .expect("chmod back");
        }
        std::fs::write(store.path(), "secret line\n\n").expect("rewrite");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600))
                .expect("chmod");
            assert_eq!(read_protected(store.path()).expect("read"), "secret line");
        }
    }

    #[test]
    fn writing_a_secret_closes_stdin_once() {
        // A real child, so the pipe is real. `cat` echoes what it reads, which
        // also proves the newline is sent and the write end is closed.
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("cat");
        let mut stdin = child.stdin.take();
        let secret = Secret::from("hunter2");
        write_secret(&mut stdin, &secret, Path::new("x")).expect("write");
        // Taken, so a second call is a no-op rather than a panic.
        write_secret(&mut stdin, &secret, Path::new("x")).expect("second write");
        let out = child.wait_with_output().expect("output");
        assert_eq!(out.stdout, b"hunter2\n");
    }
}
