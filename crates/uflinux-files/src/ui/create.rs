//! New documents from templates and archives from a file selection.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use gtk::gio;
use gtk::prelude::*;
use ufm_core::{Entry, Location, paths};

use super::job::Handle;
use super::jobs::{Brake, Jobs, Kind, Unit};
use super::ops::{self, Outcome};
use super::undo::Record;

pub(super) struct Format {
    pub extension: &'static str,
    label: &'static str,
    args: &'static [&'static str],
}

const FORMATS: &[Format] = &[
    Format {
        extension: ".zip",
        label: "ZIP",
        args: &["--format", "zip"],
    },
    Format {
        extension: ".7z",
        label: "7z",
        args: &["--format", "7zip"],
    },
    Format {
        extension: ".rar",
        label: "RAR",
        args: &[],
    },
    Format {
        extension: ".tar",
        label: "TAR",
        args: &["--format", "pax"],
    },
    Format {
        extension: ".tar.gz",
        label: "TAR.GZ",
        args: &["--format", "pax", "-z"],
    },
    Format {
        extension: ".tar.bz2",
        label: "TAR.BZ2",
        args: &["--format", "pax", "-j"],
    },
    Format {
        extension: ".tar.xz",
        label: "TAR.XZ",
        args: &["--format", "pax", "-J"],
    },
    Format {
        extension: ".tar.zst",
        label: "TAR.ZST",
        args: &["--format", "pax", "--zstd"],
    },
];

pub(super) fn formats() -> gio::Menu {
    let menu = gio::Menu::new();
    for format in FORMATS {
        let item = gio::MenuItem::new(Some(format.label), None);
        item.set_action_and_target_value(
            Some("win.compress"),
            Some(&format.extension.to_variant()),
        );
        menu.append_item(&item);
    }
    menu
}

pub(super) fn format_for(extension: &str) -> Option<&'static Format> {
    FORMATS.iter().find(|format| format.extension == extension)
}

pub(super) fn is_rar(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rar"))
}

pub(super) fn source_folder(entries: &[Entry]) -> Option<PathBuf> {
    let folder = entries.first()?.location.as_path()?.parent()?;
    entries
        .iter()
        .all(|entry| {
            entry.location.is_real()
                && entry.location.as_path().and_then(Path::parent) == Some(folder)
        })
        .then(|| folder.to_path_buf())
}

pub(super) fn templates_root() -> Option<PathBuf> {
    let root = paths::user_dir("XDG_TEMPLATES_DIR", "Templates");
    (root.is_absolute() && root != paths::home()).then_some(root)
}

pub(super) enum Template {
    File(String, PathBuf),
    Folder(String, Vec<Template>),
}

pub(super) fn templates(dir: &Path, show_hidden: bool, depth: u8) -> Vec<Template> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type().ok()?;
            let path = entry.path();
            (!name.starts_with('.') || (show_hidden && kind.is_file()))
                .then_some((name, path, kind))
        })
        .collect();
    entries.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then(left.0.cmp(&right.0))
    });
    entries
        .into_iter()
        .take(30)
        .filter_map(|(name, path, kind)| {
            if kind.is_file() {
                Some(Template::File(name, path))
            } else if kind.is_dir() && depth < 5 {
                let children = templates(&path, show_hidden, depth + 1);
                (!children.is_empty()).then_some(Template::Folder(name, children))
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn template_menu(templates: &[Template]) -> gio::Menu {
    let menu = gio::Menu::new();
    for template in templates {
        match template {
            Template::Folder(name, children) => {
                menu.append_submenu(Some(&name.replace('_', "__")), &template_menu(children));
            }
            Template::File(name, path) => {
                let item = gio::MenuItem::new(Some(&name.replace('_', "__")), None);
                let uri = gio::File::for_path(path).uri();
                item.set_action_and_target_value(
                    Some("win.new-document"),
                    Some(&uri.as_str().to_variant()),
                );
                menu.append_item(&item);
            }
        }
    }
    menu
}

pub(super) fn document(handle: &Handle, jobs: &Jobs, source: PathBuf, folder: PathBuf) {
    let Some(name) = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return;
    };
    let label = name.clone();
    ops::spawn_job(
        handle,
        jobs,
        Kind::Copy,
        &label,
        None,
        Unit::Rows,
        move |brake, _| {
            let result = (|| -> io::Result<PathBuf> {
                let root =
                    templates_root().ok_or_else(|| io::Error::other("Templates is disabled"))?;
                if !source.starts_with(&root)
                    || !source
                        .parent()
                        .unwrap_or(&source)
                        .canonicalize()?
                        .starts_with(root.canonicalize()?)
                    || !fs::symlink_metadata(&source)?.is_file()
                {
                    return Err(io::Error::other("The template is no longer available"));
                }
                let temp = scratch(&folder)?;
                let staged = temp.path().join("document");
                copy(&source, &staged, brake).map_err(io::Error::other)?;
                let mut previous = None;
                loop {
                    let name = ops::unique_name(&name, |candidate| {
                        folder.join(candidate).symlink_metadata().is_ok()
                    });
                    let target = folder.join(name);
                    if previous.as_ref() == Some(&target) {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "No free name for this template",
                        ));
                    }
                    match publish(&staged, &target, brake) {
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            previous = Some(target)
                        }
                        result => return result.map(|()| target),
                    }
                }
            })();
            completed(result, &folder, brake)
        },
    );
}

pub(super) fn ask_archive(
    window: &gtk::ApplicationWindow,
    handle: &Handle,
    jobs: std::rc::Rc<Jobs>,
    folder: PathBuf,
    entries: Vec<Entry>,
    format: &'static Format,
) {
    let name = match entries.as_slice() {
        [entry] if entry.kind.is_dir() => &entry.name,
        [entry] => ops::split_name(&entry.name).0,
        _ => "Archive",
    };
    let paths: Vec<_> = entries
        .iter()
        .filter_map(|entry| entry.location.as_path().map(Path::to_path_buf))
        .collect();
    let check_folder = folder.clone();
    let handle = handle.clone();
    ops::ask_checked(
        window,
        "Compress",
        &format!("Archive name ({})", format.extension),
        name,
        "_Create",
        Some((0, -1)),
        move |name| {
            if matches!(name, "." | "..") || name.contains('/') {
                Some("Enter a filename.".into())
            } else if check_folder
                .join(format!("{name}{}", format.extension))
                .symlink_metadata()
                .is_ok()
            {
                Some("An item with this name already exists.".into())
            } else {
                None
            }
        },
        move |name| {
            let name = format!("{name}{}", format.extension);
            let label = name.clone();
            ops::spawn_job(
                &handle,
                &jobs,
                Kind::Compress,
                &label,
                None,
                Unit::Rows,
                move |brake, _| {
                    completed(
                        archive(&folder, &paths, &name, format, brake),
                        &folder,
                        brake,
                    )
                },
            );
        },
    );
}

fn completed(result: io::Result<PathBuf>, folder: &Path, brake: &Brake) -> Outcome {
    match result {
        Ok(path) => {
            let mut outcome = Outcome::done(format!(
                "Created {}.",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            outcome.note(&Location::file(folder));
            outcome.undo = Some(Record::created(Location::file(path)));
            outcome
        }
        Err(_) if brake.pulled() => Outcome::failed("Creation cancelled."),
        Err(error) => Outcome::failed(error.to_string()),
    }
}

fn scratch(folder: &Path) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(".ufm-create-")
        .tempdir_in(folder)
}

fn copy(source: &Path, target: &Path, brake: &Brake) -> Result<(), glib::Error> {
    let from = gio::File::for_path(source);
    let to = gio::File::for_path(target);
    if ops::is_directory(&from, brake) {
        ops::copy_tree(&from, &to, false, brake)
    } else {
        ops::copy_file(
            &from,
            &to,
            gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
            brake,
            None,
        )
    }
}

fn archive(
    folder: &Path,
    sources: &[PathBuf],
    name: &str,
    format: &Format,
    brake: &Brake,
) -> io::Result<PathBuf> {
    let temp = scratch(folder)?;
    let output = temp.path().join(format!("output{}", format.extension));
    let names: Vec<_> = sources
        .iter()
        .map(|source| {
            let name = source.strip_prefix(folder).map_err(io::Error::other)?;
            if name.file_name().is_none() || name.components().count() != 1 {
                return Err(io::Error::other("The selected items must share a folder"));
            }
            Ok(Path::new(".").join(name))
        })
        .collect::<io::Result<_>>()?;

    let log_path = temp.path().join("log");
    if format.extension == ".rar" {
        let mut command = Command::new("rar");
        command.args(["a", "-cfg-", "-idq", "-ierr", "-y", "-ol"]);
        // RAR interprets * and ? as patterns even in literal argv. Run it over
        // a private tree when any selected name would be interpreted as a mask.
        if names.iter().any(|name| {
            name.as_os_str()
                .as_bytes()
                .iter()
                .any(|b| matches!(b, b'*' | b'?'))
        }) || names
            .iter()
            .map(|name| name.as_os_str().len() + 1)
            .sum::<usize>()
            > 64 * 1024
        {
            let staging = temp.path().join("inputs");
            fs::create_dir(&staging)?;
            for (source, name) in sources.iter().zip(&names) {
                copy(source, &staging.join(name), brake).map_err(io::Error::other)?;
            }
            command
                .current_dir(staging)
                .args(["-r", "--"])
                .arg(&output)
                .arg(".");
        } else {
            command
                .current_dir(folder)
                .arg("--")
                .arg(&output)
                .args(&names);
        }
        run(&mut command, &log_path, brake)?;
    } else {
        let list = temp.path().join("inputs");
        let mut file = File::create(&list)?;
        for name in names {
            file.write_all(name.as_os_str().as_bytes())?;
            file.write_all(&[0])?;
        }
        let mut command = Command::new("bsdtar");
        command
            .current_dir(folder)
            .args(format.args)
            .args(["-cf"])
            .arg(&output)
            .args(["--null", "-T"])
            .arg(&list);
        run(&mut command, &log_path, brake)?;
    }
    let target = folder.join(name);
    publish(&output, &target, brake)?;
    Ok(target)
}

fn run(command: &mut Command, log: &Path, brake: &Brake) -> io::Result<()> {
    check_cancel(brake)?;
    let tool = command.get_program().to_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(log)?)
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::other(format!(
                    "{} is required to create this archive.",
                    tool.to_string_lossy()
                ))
            } else {
                error
            }
        })?;
    let status = loop {
        if brake.pulled() {
            let _ = child.kill();
            let _ = child.wait();
            return check_cancel(brake);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut message = Vec::new();
    File::open(log)?.take(4096).read_to_end(&mut message)?;
    let message = String::from_utf8_lossy(&message);
    Err(io::Error::other(format!(
        "{} failed ({status}). {}",
        tool.to_string_lossy(),
        message.trim()
    )))
}

fn check_cancel(brake: &Brake) -> io::Result<()> {
    brake.vfs().ensure_active().map_err(io::Error::other)
}

/// Publish atomically where supported, and reserve the destination exclusively
/// on filesystems that support neither no-replace rename nor hard links.
fn publish(source: &Path, target: &Path, brake: &Brake) -> io::Result<()> {
    check_cancel(brake)?;
    let src = std::ffi::CString::new(source.as_os_str().as_bytes())?;
    let dst = std::ffi::CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: Both paths are valid NUL-terminated strings for this syscall.
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if !matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
    ) {
        return Err(error);
    }
    match fs::hard_link(source, target) {
        Ok(()) => return Ok(()),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP | libc::EXDEV)
            ) => {}
        Err(error) => return Err(error),
    }
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let result = (|| {
        let mut buffer = vec![0; 64 * 1024];
        loop {
            check_cancel(brake)?;
            let count = input.read(&mut buffer)?;
            if count == 0 {
                return output.sync_all();
            }
            output.write_all(&buffer[..count])?;
        }
    })();
    if result.is_err()
        && let (Ok(opened), Ok(on_disk)) = (output.metadata(), target.symlink_metadata())
        && (opened.dev(), opened.ino()) == (on_disk.dev(), on_disk.ino())
    {
        let _ = fs::remove_file(target);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_files_never_replace_existing_items() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("part");
        let target = temp.path().join("archive");
        fs::write(&source, "new").unwrap();
        fs::write(&target, "old").unwrap();
        assert_eq!(
            publish(&source, &target, &Brake::new()).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        fs::remove_file(&target).unwrap();
        publish(&source, &target, &Brake::new()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn templates_include_document_subfolders_and_skip_empty_or_hidden_folders() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for dir in ["Empty", "Letters", ".Hidden"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        fs::write(root.join(".Hidden/ignored.txt"), "ignored").unwrap();
        fs::write(root.join("Letters/Letter.txt"), "template").unwrap();
        let rows = templates(root, false, 0);
        assert!(
            matches!(&rows[..], [Template::Folder(name, children)] if name == "Letters"
            && matches!(&children[..], [Template::File(name, _)] if name == "Letter.txt"))
        );
    }

    #[test]
    fn archive_formats_round_trip_selected_items() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        let file = folder.join("-literal *?.txt");
        let dir = folder.join("@nested");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("empty")).unwrap();
        fs::write(dir.join("inside.txt"), b"nested contents").unwrap();
        fs::write(&file, b"contents").unwrap();
        fs::write(folder.join("-literal extra.txt"), b"unselected").unwrap();

        for format in FORMATS {
            let program = if format.extension == ".rar" {
                "rar"
            } else {
                "bsdtar"
            };
            if glib::find_program_in_path(program).is_none() {
                continue;
            }
            let name = format!("archive{}", format.extension);
            let created = archive(
                folder,
                &[file.clone(), dir.clone()],
                &name,
                format,
                &Brake::new(),
            )
            .unwrap();
            let mut list = Command::new(program);
            if program == "rar" {
                list.arg("lb");
            } else {
                list.arg("-tf");
            }
            let listed = list.arg(created).output().unwrap();
            assert!(listed.status.success(), "{name}");
            let listed = String::from_utf8(listed.stdout).unwrap();
            for expected in ["-literal *?.txt", "@nested/inside.txt", "@nested/empty"] {
                assert!(listed.contains(expected), "{name}: {listed}");
            }
            assert!(!listed.contains("extra.txt"), "{name}: {listed}");
        }
    }

    #[test]
    fn cancelled_archive_leaves_no_output_or_scratch_directory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("item");
        fs::write(&source, b"contents").unwrap();
        let brake = Brake::new();
        brake.pull();
        assert!(archive(temp.path(), &[source], "archive.zip", &FORMATS[0], &brake).is_err());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
