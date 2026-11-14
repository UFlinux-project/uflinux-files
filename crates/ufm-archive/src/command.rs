//! Helper processes: what is installed, and how to run it.
//!
//! Two of the archive backends shell out — a compressed tar is decoded by the
//! system compressor, a `.7z` is read by `7z` — and both want to know whether the
//! program exists before promising anything about the archive. Spawning
//! `--version` to find out costs a fork on a machine the plan wants to keep
//! responsive, so `$PATH` is searched directly instead.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ufm_core::error::{Error, Result};

/// Whether `program` can be run by name, without running it.
///
/// An absolute path is checked as such; anything else is looked for in `$PATH`.
/// A directory that happens to be executable does not count.
pub(crate) fn available(program: &str) -> bool {
    if program.contains('/') {
        return executable(Path::new(program));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| executable(&dir.join(program)))
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The error to report when an archive needs a program that is not installed.
pub(crate) fn missing(program: &str, archive: &Path) -> Error {
    Error::Archive(format!(
        "{} needs {program}, which is not installed",
        archive.display()
    ))
}

/// A command with the environment the archive code relies on: no locale
/// translation of dates, no translated error text to parse.
pub(crate) fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env("LC_ALL", "C");
    command
}

/// The first non-empty line of a helper's output, which is where both `tar` and
/// `7z` put the reason they failed. Empty when they had nothing to say.
fn complaint(stderr: &[u8], stdout: &[u8]) -> String {
    for stream in [stderr, stdout] {
        let text = String::from_utf8_lossy(stream);
        if let Some(line) = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('-'))
        {
            return line.to_string();
        }
    }
    String::new()
}

/// Report a failed helper, keeping its own complaint over a generic message:
/// "this does not look like a tar archive" tells the user something that "the
/// archive could not be read" does not.
pub(crate) fn failure(archive: &Path, program: &str, stderr: &[u8], stdout: &[u8]) -> Error {
    let reason = complaint(stderr, stdout);
    let archive = archive.display();
    if reason.is_empty() {
        Error::Archive(if program.is_empty() {
            format!("{archive}: the archive could not be read")
        } else {
            format!("{archive}: {program} could not read it")
        })
    } else {
        Error::Archive(format!("{archive}: {reason}"))
    }
}

/// Run a command to completion and report a non-zero exit. A helper that cannot
/// start at all is reported as an unreadable archive rather than as an I/O error,
/// because to the user those are the same event.
///
/// Listing and streaming read stdout, so they use [`output`] or [`spawn`]
/// instead; this is for the helpers that only have to succeed, which is extracting
/// a whole container with `unzip`, `tar` or `7z` and creating one in a test.
pub(crate) fn status_ok(command: &mut Command, archive: &Path, program: &str) -> Result<()> {
    let output = match command.output() {
        Ok(output) => output,
        Err(e) => return Err(Error::Archive(format!("{}: {e}", archive.display()))),
    };
    if output.status.success() {
        Ok(())
    } else {
        Err(failure(archive, program, &output.stderr, &output.stdout))
    }
}

/// Run a command to completion and hand back its stdout.
///
/// `output()` is used rather than a pipe because it drains both streams
/// concurrently: a helper that complains at length while its stdout is being read
/// would otherwise fill the pipe buffer and stop, waiting for a reader that is
/// waiting for it.
pub(crate) fn output(command: &mut Command, archive: &Path, program: &str) -> Result<Vec<u8>> {
    let output = match command.output() {
        Ok(output) => output,
        Err(e) => return Err(Error::Archive(format!("{}: {e}", archive.display()))),
    };
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(failure(archive, program, &output.stderr, &output.stdout))
    }
}

/// Start a command with stdin closed and both output pipes open.
pub(crate) fn spawn(mut command: Command, archive: &Path) -> Result<Child> {
    let inner = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Archive(format!("{}: {e}", archive.display())))?;
    Ok(Child {
        inner,
        path: archive.to_path_buf(),
    })
}

/// A running helper whose stdout is the data, reaped on drop.
///
/// Dropping without waiting would leave a zombie, and a file manager that opens
/// many archives and abandons half-read ones would accumulate them.
pub(crate) struct Child {
    inner: std::process::Child,
    path: PathBuf,
}

impl Child {
    /// Wait for the helper and report a failure, keeping its first complaint.
    ///
    /// Only meaningful once stdout has been drained: a decompressor whose output
    /// was abandoned early exits non-zero for a reason that is not the archive's
    /// fault.
    pub(crate) fn finish(mut self) -> Result<()> {
        let status = self.inner.wait()?;
        if status.success() {
            return Ok(());
        }
        let mut stderr = Vec::new();
        if let Some(mut pipe) = self.inner.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_end(&mut stderr);
        }
        Err(failure(&self.path, "", &stderr, &[]))
    }
}

impl std::io::Read for Child {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.inner.stdout.as_mut() {
            Some(stdout) => std::io::Read::read(stdout, buf),
            None => Ok(0),
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.inner.kill();
        let _ = self.inner.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_is_found_by_name_and_by_path() {
        assert!(available("sh"), "PATH lookup works");
        assert!(available("/bin/sh"), "an absolute path is checked as such");
        assert!(!available("ufm-definitely-not-a-program"));
        assert!(!available("/tmp"), "a directory is not a program");
    }

    #[test]
    fn a_missing_program_names_the_archive_it_was_needed_for() {
        let text = missing("7z", Path::new("/x/a.7z")).to_string();
        assert!(text.contains("/x/a.7z"), "{text}");
        assert!(text.contains("7z"), "{text}");
    }

    #[test]
    fn a_command_carries_a_known_locale() {
        let command = command("sh");
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new("LC_ALL"))
                .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string)),
            Some(Some(std::ffi::OsString::from("C")))
        );
    }

    #[test]
    fn a_failing_command_is_reported_with_its_complaint() {
        let mut noisy = command("sh");
        noisy.args(["-c", "echo 'boom here' >&2; exit 3"]);
        let error = status_ok(&mut noisy, Path::new("/x/a.tar"), "tar").unwrap_err();
        assert!(error.to_string().contains("boom here"), "{error}");

        let mut quiet = command("sh");
        quiet.args(["-c", "exit 1"]);
        let error = status_ok(&mut quiet, Path::new("/x/a.tar"), "tar").unwrap_err();
        assert!(error.to_string().contains("tar"), "{error}");

        let mut fine = command("sh");
        fine.args(["-c", "exit 0"]);
        assert!(status_ok(&mut fine, Path::new("/x/a.tar"), "tar").is_ok());
    }

    #[test]
    fn a_7z_style_banner_is_not_mistaken_for_a_reason() {
        // `7z` prints `----------` and `--` around its listing; neither is a
        // complaint, and reporting one as the error would be useless.
        let error = failure(Path::new("/x/a.7z"), "7z", b"----------\n", b"--");
        assert!(error.to_string().contains("could not read"), "{error}");
    }

    #[test]
    fn a_pipe_hands_out_bytes_and_reports_its_exit() {
        let mut child = pipe("printf abc");
        let mut got = String::new();
        std::io::Read::read_to_string(&mut child, &mut got).expect("read");
        assert_eq!(got, "abc");
        child.finish().expect("succeeded");
    }

    #[test]
    fn a_pipe_that_fails_reports_after_the_data() {
        let child = pipe("echo oops >&2; exit 5");
        let error = child.finish().unwrap_err();
        assert!(error.to_string().contains("oops"), "{error}");
    }

    #[test]
    fn a_pipe_whose_output_is_abandoned_is_still_reaped() {
        let child = pipe("printf abc; exit 0");
        drop(child);
    }

    fn pipe(script: &str) -> Child {
        let mut command = command("sh");
        command.args(["-c", script]);
        spawn(command, Path::new("/x/a.7z")).expect("spawned")
    }
}
