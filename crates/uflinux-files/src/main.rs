//! Entry point. The argument handling is in [`cli`], the mount bookkeeping in
//! [`runner`], and the widgets in [`ui`].
//!
//! `--sweep-mounts` runs without touching GTK at all, which is what lets the session
//! start it before a compositor is ready and keeps its memory footprint a fraction
//! of a window's.
//!
//! Desktop icons are not a mode of this program. They used to be `--provider`, and
//! the binary grew to 28.6 MiB of resident set on the strength of GTK's import
//! table alone — the linker maps `LLVM` at exec whether or not the code that needs
//! it ever runs. The provider is now `uflinux-files-provider`, whose whole library
//! closure is twenty-five shared objects instead of a hundred and eleven.

mod cli;

// Every method here has a caller: the connect dialog calls `mount` and `forget`, a
// card's or a sidebar row's Disconnect calls `unmount`, Reconnect calls
// `reconnect`, Forget calls `forget_share`, the login row calls `set_autostart`,
// `--sweep-mounts` calls `sweep` and `--connect-at-login` calls `connect_at_login`.
// The module used to carry an `allow(dead_code)` that hid two helpers nothing
// outside a test calls — `Runner::list` and `Login::is_empty` — and both now say so
// with `#[cfg(test)]` instead. See the note on each.
mod runner;
mod ui;

use std::process::ExitCode;

use runner::{Layout, Login, Runner};
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
        Ok(cli::Command::Connect(options)) => connect(options),
        Ok(cli::Command::Desktop) => desktop(),
        Ok(cli::Command::Ui(options)) => ui(&options),
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

/// Mount the shares the user asked to have mounted, and print what happened.
///
/// Like [`sweep`], the exit code is about the run rather than about each share: a
/// server that is not up yet is an ordinary morning, and the reason is stored
/// beside the record for the Network group to show. What makes this run worth
/// watching is its output, not its status — a session starter that failed here
/// would have failed to answer, not failed to mount.
fn connect(options: cli::Connect) -> ExitCode {
    let mut runner = Runner::open(Layout::system());
    let login = runner.connect_at_login();
    if options.json {
        println!("{}", json_login(&login));
    } else {
        print!("{}", login.summary());
    }
    ExitCode::SUCCESS
}

/// The outcome as one JSON object, in the shape [`json`] gives a sweep: a list of
/// per-share results plus a count, so a session tool can decide whether to bother
/// the user without parsing prose.
fn json_login(login: &Login) -> String {
    let attempts: Vec<_> = login
        .attempts()
        .iter()
        .map(|attempt| {
            serde_json::json!({
                "id": attempt.id,
                "label": attempt.label,
                "result": attempt.verb(),
                "attempted": attempt.attempted,
                "mounted": attempt.ok,
                "message": attempt.message,
            })
        })
        .collect();
    serde_json::json!({
        "attempts": attempts,
        "not_mounted": login.not_mounted(),
    })
    .to_string()
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

/// Open a window. Unless `--new-window` was given the application is
/// single-instance, so the locations are handed to whichever process already owns
/// the name and this call returns once that process has finished; a launcher
/// mistake therefore shows up as an already-open window rather than a second copy
/// of the program.
fn ui(options: &cli::Ui) -> ExitCode {
    ui::run(options)
}

/// Draw the desktop. See [`cli::Command::Desktop`] for why this is a mode of a
/// window program rather than a third binary, and [`ui::desktop`] for what it
/// costs to find out that nobody wanted it drawn.
#[cfg(feature = "desktop-icons")]
fn desktop() -> ExitCode {
    ui::desktop::run()
}

/// The same command in a build without the feature, refused by name.
///
/// The flag is parsed in every build, so the failure is "this build does not have
/// that" and not "that is not an option", which is the difference between a
/// package that needs rebuilding and a script with a typo in it. Exit code 2 is
/// the one this program already uses for a command line it cannot act on.
#[cfg(not(feature = "desktop-icons"))]
fn desktop() -> ExitCode {
    eprintln!(
        "uflinux-files: this build has no desktop icons; it needs the `desktop-icons` \n\
         feature, which pulls in gtk4-layer-shell. Nothing was drawn."
    );
    ExitCode::from(2)
}
