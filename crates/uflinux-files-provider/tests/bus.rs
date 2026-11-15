//! The provider's wire, exercised against a real session bus.
//!
//! The unit tests in this crate drive dispatch with a core that has no connection at
//! all, which is what makes them runnable anywhere. That leaves the actual job —
//! answering D-Bus calls — untested: whether a real client accepts the introspection
//! data, whether the interface name is claimable, whether a refusal arrives as a D-Bus
//! error rather than a reply, whether `SIGTERM` gives the name back. Those are only
//! visible with a bus running, so they live in `tests/provider-bus.sh` and this file is
//! what makes them run as part of `cargo test` instead of as a favour.
//!
//! The script is the test; this is the harness that finds it, builds nothing, and
//! decides what its exit status means. It is given the binary through
//! `CARGO_BIN_EXE_`, so it checks the thing this build produced rather than whatever is
//! lying around in a target directory.
//!
//! Two tools are needed and neither is in a build chroot: `dbus-run-session` to own a
//! bus and `gdbus` to speak on it. Without them the script exits `2`, and this reports
//! a skip rather than a pass — a feature that went untested has to be visible as that,
//! because a green result from a check that ran nothing is the worst kind of green.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Where the fixture lives, relative to this crate. Two levels up because `tests/` is
/// shared by the whole workspace rather than belonging to one crate.
fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/provider-bus.sh")
        .canonicalize()
        .expect("tests/provider-bus.sh is part of the source bundle")
}

fn missing(tool: &str) -> bool {
    // `command -v` rather than a PATH walk in Rust, so the answer is the same one the
    // script itself would reach. A shell function or an alias is not enough for either.
    // No shell to ask counts as missing: the script would fail for the same reason.
    match Command::new("sh")
        .args(["-c", "command -v \"$1\"", "sh", tool])
        .output()
    {
        Ok(out) => !out.status.success(),
        Err(_) => true,
    }
}

#[test]
fn the_wire_answers_the_way_the_protocol_says() {
    for tool in ["dbus-run-session", "gdbus"] {
        if missing(tool) {
            eprintln!("SKIP provider-bus: {tool} is not installed; the bus went untested");
            return;
        }
    }

    let binary = env!("CARGO_BIN_EXE_uflinux-files-provider");
    let run = Command::new("sh")
        .arg(script())
        .arg(binary)
        .output()
        .expect("the fixture is runnable");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    match run.status.code() {
        // The script stops at the first answer that is wrong, so the failing check and
        // the reply that caused it are the last things it printed.
        Some(0) => println!("{text}"),
        Some(2) => eprintln!("SKIP provider-bus: the environment refused it\n{text}"),
        Some(code) => panic!("provider-bus exited {code}\n{text}"),
        None => panic!("provider-bus was killed by a signal\n{text}"),
    }
}

/// The script has to be run by `sh`, not executed: a source checkout pulled through a
/// tarball or a packaging pipeline is not guaranteed to have kept the mode bit, and a
/// fixture that fails with "permission denied" reports that as a broken program.
#[test]
fn the_fixture_is_readable_and_names_every_method_it_calls() {
    let text = std::fs::read_to_string(script()).expect("the fixture is readable");
    assert!(
        text.starts_with("#!/bin/sh"),
        "the fixture is POSIX shell, not a bashism"
    );
    // Every method the interface declares has to be exercised, or the introspection
    // check above certifies a surface nothing uses. `Reveal` is the one exception and it
    // says so in the file, so the list here is the list the file defends.
    for method in [
        "Status",
        "GetModel",
        "SetGeometry",
        "Place",
        "Launch",
        "ContextMenu",
        "Trash",
        "RequestThumbnail",
    ] {
        assert!(
            text.contains(&format!("call {method} '")),
            "the fixture never calls {method}"
        );
    }
    // Both signals, too, and not merely declared: a provider that only ever answers is
    // half of what a renderer needs, and the half that is missing is the half that makes
    // a desktop notice a file. `gdbus monitor` is what catches them, so its presence
    // alongside each payload is the assertion.
    assert!(
        text.contains("gdbus monitor"),
        "the fixture never attaches a client to the signals"
    );
    for payload in [
        "\"phase\":\"scanning\"",
        "\"phase\":\"done\"",
        "\"reason\":\"a.txt moved\"",
        "\"items\":2",
    ] {
        assert!(
            text.contains(&format!("seen '{payload}'")),
            "the fixture never waits for a signal carrying {payload}"
        );
    }
}
