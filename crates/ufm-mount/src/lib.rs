//! Network filesystem mounts for UFLinux Files.
//!
//! The plan is to mount rather than to proxy: a share mounted at
//! `$XDG_RUNTIME_DIR/ufm/<slug>` is readable by every application, including
//! ones with no concept of a remote location, and the file manager itself needs
//! only one code path for browsing — the local one in `ufm-vfs`.
//!
//! Two privilege levels are involved, and the split follows what UFLinux
//! actually ships:
//!
//! * SMB (`cifs`) and NFS need `mount(2)`, which needs root. UFLinux has no
//!   `udisks2`; it does have polkit with a session agent registered by
//!   `uflinux-desktop`, and an established precedent in the
//!   `org.uflinux.Settings.System1` D-Bus helper, which accepts "no shell
//!   commands or arbitrary paths". So these go through a small privileged helper
//!   invoked with `pkexec`, with a polkit action of our own. `doas` is not an
//!   option from a GUI: it reads its password from `/dev/tty`.
//! * SFTP, WebDAV and FTP are FUSE and need no privilege at all — `sshfs`,
//!   `mount.davfs` (which ships setuid) and `rclone mount` each mount into a
//!   directory the user owns.
//!
//! Everything here is about *planning* a mount: [`backend`] turns a [`Share`]
//! into the exact argv to run, [`table`] remembers the intent, and [`monitor`]
//! reports whether the kernel still agrees. No mount is attempted in this crate's
//! tests, which is also why the argv is asserted rather than the mount point.
//! Credentials travel on stdin, never in argv, where `/proc/<pid>/cmdline` would
//! show them to any user on the machine.

#![deny(unsafe_code)]
#![forbid(unsafe_code)]
// The prose in this crate names products and protocols — UFLinux, WebDAV, rclone
// — which `doc_markdown` reads as unquoted code. Backticking them would make the
// comments harder to read, not easier.
#![allow(clippy::doc_markdown)]

pub mod backend;
pub mod credentials;
pub mod monitor;
pub mod plan;
pub mod sweep;
pub mod table;
pub mod uri;

pub use backend::{Backend, MountTarget};
pub use credentials::{CredentialStore, FileStore, Secret};
pub use monitor::{MountState, observe};
pub use plan::{Command, Plan};
pub use sweep::{Action, Sweep, SweepItem};
pub use table::{MountRecord, MountTable, mountpoint_for, now};
pub use uri::{ParsedUri, Protocol, Share};
