//! Command line, kept apart from `main` so the shape of the interface is tested
//! and the entry point stays a dispatch.
//!
//! The set of modes is small on purpose. `--sweep-mounts` runs without GTK and is
//! entered by something other than a human — the session starter — so its argument
//! handling has to be boring and predictable. Everything else opens a window. The
//! desktop icons provider used to be a mode here and is now its own program,
//! `uflinux-files-provider`: sharing this executable meant sharing its import
//! table, and GTK's GL stack made the provider cost three times its budget without
//! ever running a line of provider code.

use std::error::Error;
use std::fmt;

/// What the process was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print the usage and exit successfully, which is what no arguments means:
    /// a desktop launcher that guesses the arguments wrong should show help, not
    /// fail with an error the user cannot read.
    Help,
    Version,
    /// Clean up after mounts that are no longer mounts, then exit. See
    /// [`crate::runner`].
    Sweep(Sweep),
    /// Open a window. The default.
    Ui(Ui),
}

/// The flags for opening a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ui {
    /// The locations to show, in the order given. Empty means the window opens
    /// wherever the preferences say a window should start.
    pub urls: Vec<String>,
    /// Ask for a window of this process's own rather than handing the locations to
    /// a window already on screen. What the desktop entry's `new-window` action
    /// passes; see [`crate::ui`] for what it costs in single-instance behaviour.
    pub new_window: bool,
}

/// The flags `--sweep-mounts` accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Detach shares idle for at least this long, in addition to the leftovers
    /// a sweep always removes. `None` leaves everything mounted.
    pub idle_after: Option<u64>,
    /// Print the summary as JSON instead of lines, for a session tool that wants
    /// to show what was cleaned up rather than dump it on a terminal.
    pub json: bool,
}

/// A bad argument, reported as one line. The usage text follows it, so the
/// message itself does not repeat the whole interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage(pub String);

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for Usage {}

/// Parse `argv` without the program name.
pub fn parse(args: &[String]) -> Result<Command, Usage> {
    let mut sweep = Sweep::default();
    let mut sweeping = false;
    let mut urls: Vec<String> = Vec::new();
    let mut new_window = false;
    let mut help = false;
    let mut version = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => help = true,
            "--version" => version = true,
            "--sweep-mounts" => sweeping = true,
            "--json" => sweep.json = true,
            "--new-window" => new_window = true,
            "--idle-after" => {
                let value = iter
                    .next()
                    .ok_or_else(|| Usage("--idle-after wants a number of seconds".into()))?;
                sweep.idle_after = Some(
                    value
                        .parse()
                        .map_err(|_| Usage(format!("--idle-after: {value} is not a number")))?,
                );
            }
            other if other.starts_with("--idle-after=") => {
                let value = &other["--idle-after=".len()..];
                sweep.idle_after = Some(
                    value
                        .parse()
                        .map_err(|_| Usage(format!("--idle-after: {value} is not a number")))?,
                );
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(Usage(format!("unknown option {other}")));
            }
            other => urls.push(other.to_string()),
        }
    }
    if help {
        return Ok(Command::Help);
    }
    if version {
        return Ok(Command::Version);
    }
    if sweeping {
        return Ok(Command::Sweep(sweep));
    }
    Ok(Command::Ui(Ui { urls, new_window }))
}

/// The usage text, shared by `--help` and by a bad argument, so the two cannot
/// disagree about what the program accepts.
#[must_use]
pub const fn usage() -> &'static str {
    "\
uflinux-files — browse files, archives and network shares

Usage:
  uflinux-files [PATH|URL ...]          open a window, one tab per location
  uflinux-files --sweep-mounts          clean up dead mounts and leftover files
  uflinux-files --version

Options for --sweep-mounts:
  --idle-after SECONDS   also detach shares unused for at least this long
  --json                 report what was done as one JSON object

Other options:
  --new-window           open a new window instead of focusing an open one
  -h, --help             show this text
  --version              print the version
"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    fn ui(urls: &[&str]) -> Command {
        Command::Ui(Ui {
            urls: args(urls),
            new_window: false,
        })
    }

    #[test]
    fn no_arguments_opens_a_window_with_no_locations() {
        assert_eq!(parse(&[]), Ok(ui(&[])));
    }

    #[test]
    fn a_path_or_a_url_is_a_location_and_not_an_option() {
        assert_eq!(
            parse(&args(&["/tmp", "smb://srv/share"])),
            Ok(ui(&["/tmp", "smb://srv/share"]))
        );
    }

    #[test]
    fn the_provider_flag_is_gone_loudly_rather_than_opening_a_window() {
        // An activation file or a session script left over from before the split
        // would otherwise open a window on the desktop, which is a confusing way to
        // report "that mode lives elsewhere now".
        let error = parse(&args(&["--provider"])).expect_err("should be refused");
        assert_eq!(error.to_string(), "unknown option --provider");
    }

    #[test]
    fn sweep_defaults_to_leaving_everything_mounted() {
        assert_eq!(
            parse(&args(&["--sweep-mounts"])),
            Ok(Command::Sweep(Sweep::default()))
        );
    }

    #[test]
    fn idle_after_accepts_both_spellings() {
        let expected = Ok(Command::Sweep(Sweep {
            idle_after: Some(900),
            json: false,
        }));
        assert_eq!(
            parse(&args(&["--sweep-mounts", "--idle-after", "900"])),
            expected
        );
        assert_eq!(
            parse(&args(&["--sweep-mounts", "--idle-after=900"])),
            expected
        );
    }

    #[test]
    fn idle_after_needs_a_number_and_says_which_argument_was_wrong() {
        for form in [
            args(&["--sweep-mounts", "--idle-after", "ten"]),
            args(&["--sweep-mounts", "--idle-after=ten"]),
        ] {
            let error = parse(&form).expect_err("should be refused");
            assert!(
                error.to_string().contains("ten"),
                "the message should name the bad value: {error}"
            );
        }
        let error = parse(&args(&["--sweep-mounts", "--idle-after"])).expect_err("needs a value");
        assert!(error.to_string().contains("seconds"), "{error}");
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_treated_as_a_location() {
        let error = parse(&args(&["--frobnicate"])).expect_err("should be refused");
        assert_eq!(error.to_string(), "unknown option --frobnicate");
    }

    #[test]
    fn a_lone_dash_is_a_location_because_stdin_is_not_a_flag() {
        assert_eq!(parse(&args(&["-"])), Ok(ui(&["-"])));
    }

    #[test]
    fn new_window_is_a_flag_on_opening_a_window_and_keeps_its_locations() {
        // The desktop entry's `new-window` action runs `uflinux-files --new-window`
        // with no location; a launcher that adds one must not lose it.
        assert_eq!(
            parse(&args(&["--new-window"])),
            Ok(Command::Ui(Ui {
                urls: Vec::new(),
                new_window: true,
            }))
        );
        assert_eq!(
            parse(&args(&["--new-window", "/tmp"])),
            Ok(Command::Ui(Ui {
                urls: args(&["/tmp"]),
                new_window: true,
            }))
        );
    }

    #[test]
    fn usage_names_every_mode_it_accepts() {
        let text = usage();
        for needle in ["--sweep-mounts", "--idle-after", "--json", "--new-window"] {
            assert!(text.contains(needle), "usage is missing {needle}");
        }
        assert!(
            !text.contains("--provider"),
            "usage advertising a mode that was refused would be a lie"
        );
    }
}
