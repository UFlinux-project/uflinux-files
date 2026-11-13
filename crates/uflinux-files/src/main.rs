//! Entry point. The argument handling is in [`cli`], the mount bookkeeping in
//! [`runner`], and the two modes that need no display — `--sweep-mounts` and
//! `--provider` — run without touching GTK at all, which is what lets the session
//! start them before a compositor is ready and keeps their memory footprint a
//! fraction of a window's.

mod cli;

// The window reads mounts but does not yet make them: `mount`, `unmount`,
// `reconnect`, `remember` and `forget` are the API the "Connect to a server"
// dialog will call, and `Report` is what that dialog reports back. Delete this
// attribute when the dialog lands, so a method nobody ever wanted warns again.
#[allow(dead_code)]
mod runner;
mod ui;

use std::process::ExitCode;

use runner::{Layout, Runner};
use ufm_mount::sweep::{Action, Sweep, SweepItem};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Ok(cli::Command::Help) => {
            print!("{}", cli::usage());
            ExitCode::SUCCESS
        }
        Ok(cli::Command::Version) => {
            println!("uflinux-files {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(cli::Command::Sweep(options)) => sweep(&options),
        Ok(cli::Command::Provider) => {
            // The desktop icon provider is a D-Bus service; see Phase 3.
            eprintln!("uflinux-files: --provider is not implemented yet");
            ExitCode::from(3)
        }
        Ok(cli::Command::Ui(urls)) => ui(&urls),
        Err(usage) => {
            eprintln!("uflinux-files: {usage}");
            eprint!("{}", cli::usage());
            ExitCode::from(2)
        }
    }
}

/// Clean up after mounts and print what was done.
///
/// The exit code is about the sweep itself: a share that could not be detached is
/// reported in the summary and still exits successfully, because the cleanup made
/// progress and the caller — the session starter, usually — has nothing to do
/// differently.
fn sweep(options: &cli::Sweep) -> ExitCode {
    let mut runner = Runner::open(Layout::system());
    let asked = match options.idle_after {
        Some(secs) => runner.idle_options(secs),
        None => runner.leftover_options(),
    };
    match runner.sweep(&asked) {
        Ok(sweep) => {
            if options.json {
                println!("{}", json(&sweep));
            } else {
                print!("{}", sweep.summary());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("uflinux-files: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The summary as one JSON object: what this run removed, what still needs a
/// command, and what could not be classified. The field names are stable, because
/// a session tool parses them.
fn json(sweep: &Sweep) -> String {
    let lines = |items: &[SweepItem]| {
        items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "path": item.path.to_string_lossy(),
                    "action": verb(&item.action),
                    "why": item.why,
                })
            })
            .collect::<Vec<_>>()
    };
    let notes: Vec<&str> = sweep.notes().iter().map(String::as_str).collect();
    serde_json::json!({
        "done": lines(sweep.removed()),
        "pending": lines(sweep.items()),
        "notes": notes,
    })
    .to_string()
}

/// The word for an action, matching what [`Sweep::summary`] prints.
fn verb(action: &Action) -> &'static str {
    match action {
        Action::Detach(_) => "detach",
        Action::RemoveDir => "rmdir",
        Action::RemoveCredential => "rm",
    }
}

/// Open a window. The application is single-instance, so the arguments are
/// handed to whichever process already owns the name and this call returns once
/// that process has finished; a launcher mistake therefore shows up as an
/// already-open window rather than a second copy of the program.
fn ui(urls: &[String]) -> ExitCode {
    ui::run(urls)
}
