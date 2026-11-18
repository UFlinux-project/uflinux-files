use super::{Location, VirtualDir};
use crate::error::Error;
use std::path::Path;

fn parse(s: &str) -> Location {
    Location::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

#[test]
fn bare_path_is_a_file_location() {
    assert_eq!(parse("/home/u/Docs"), Location::file("/home/u/Docs"));
}

#[test]
fn file_uri_round_trips() {
    for path in ["/", "/tmp", "/home/u/Docs", "/a/b/c/d"] {
        let loc = Location::file(path);
        let text = loc.to_string();
        assert_eq!(parse(&text), loc, "{text}");
    }
}

#[test]
fn file_uri_has_empty_authority() {
    assert_eq!("file:///", Location::file("/").to_string());
    assert_eq!("file:///tmp", Location::file("/tmp").to_string());
}

#[test]
fn percent_and_reserved_characters_survive_a_round_trip() {
    let names = [
        "with space",
        "hash#tag",
        "query?mark",
        "percent%25",
        "tab\tinside",
        "unicode-\u{e9}\u{1f600}",
        "bang!mark",
    ];
    for name in names {
        let loc = Location::file(Path::new("/tmp").join(name));
        let text = loc.to_string();
        assert_eq!(parse(&text), loc, "{name} -> {text}");
    }
}

#[test]
fn dot_and_double_dot_are_collapsed() {
    assert_eq!(parse("/a/./b"), Location::file("/a/b"));
    assert_eq!(parse("/a/b/../c"), Location::file("/a/c"));
    assert_eq!(parse("/a//b"), Location::file("/a/b"));
    // Popping past the root clamps instead of failing.
    assert_eq!(parse("/../../etc"), Location::file("/etc"));
}

#[test]
fn archive_uris_round_trip() {
    let cases = [
        ("archive:///home/u/a.zip!/", "archive root"),
        ("archive:///home/u/a.zip!/inner", "one level"),
        ("archive:///home/u/a.zip!/a/b/c", "three levels"),
    ];
    for (text, why) in cases {
        let loc = parse(text);
        assert!(loc.is_archive(), "{why}");
        assert_eq!(parse(&loc.to_string()), loc, "{why}");
    }
}

#[test]
fn archive_root_inner_is_empty() {
    let loc = parse("archive:///home/u/a.zip!/");
    assert_eq!(loc.archive_inner(), Some(""));
    assert_eq!(loc.to_string(), "archive:///home/u/a.zip!/");
}

#[test]
fn escaped_bang_inside_an_archive_path() {
    let loc = parse("archive:///tmp/a.zip!/we\\!ird/x");
    assert_eq!(loc.archive_inner(), Some("we!ird/x"));
    assert_eq!(loc.to_string(), "archive:///tmp/a.zip!/we\\!ird/x");
    // The container split happens at the first *unescaped* bang only.
    assert_eq!(loc.archive_path(), Some(Path::new("/tmp/a.zip")));
}

#[test]
fn archive_rejects_traversal() {
    for text in [
        "archive:///tmp/a.zip!/..",
        "archive:///tmp/a.zip!/a/../b",
        "archive:///tmp/a.zip!/./a",
        "archive:///tmp/a.zip!/a//b",
    ] {
        assert!(
            matches!(Location::parse(text), Err(Error::UnsafeComponent(_))),
            "{text} should be rejected"
        );
    }
}

#[test]
fn archive_needs_a_separator() {
    assert!(matches!(
        Location::parse("archive:///tmp/a.zip"),
        Err(Error::InvalidLocation(_))
    ));
}

#[test]
fn virtual_locations() {
    assert_eq!(parse("computer://"), Location::computer());
    assert_eq!(parse("network://"), Location::Virtual(VirtualDir::Network));
    assert_eq!(parse("network://").to_string(), "network://");
    assert_eq!(parse("recent://"), Location::Virtual(VirtualDir::Recent));
    assert_eq!(parse("starred://"), Location::Virtual(VirtualDir::Starred));
    assert_eq!("computer://", Location::computer().to_string());
}

#[test]
fn search_location_keeps_its_id() {
    let loc = parse("search://q-7f3a");
    assert_eq!(loc.to_string(), "search://q-7f3a");
    assert!(loc.is_virtual());
}

#[test]
fn unknown_scheme_is_rejected() {
    for text in ["smb://host/share", "ftp://host/", "http://x/y", "nonsense"] {
        assert!(
            matches!(Location::parse(text), Err(Error::InvalidLocation(_))),
            "{text} should be rejected: a share like this is mounted, not browsed"
        );
    }
}

#[test]
fn empty_input_is_rejected() {
    for text in ["", "   ", "file://", "search://"] {
        assert!(Location::parse(text).is_err(), "{text}");
    }
}

#[test]
fn parent_walks_up_one_level() {
    assert_eq!(
        Location::file("/a/b/c").parent(),
        Some(Location::file("/a/b"))
    );
    assert_eq!(Location::file("/a").parent(), Some(Location::file("/")));
    assert_eq!(Location::file("/").parent(), None);

    let deep = parse("archive:///t/a.zip!/x/y");
    assert_eq!(deep.parent(), Some(parse("archive:///t/a.zip!/x")));
    let shallow = parse("archive:///t/a.zip!/x");
    assert_eq!(shallow.parent(), Some(parse("archive:///t/a.zip!/")));
    // Up out of the archive lands on the folder holding the container, so the
    // breadcrumb can never dead-end inside a read-only filesystem.
    let root = parse("archive:///t/a.zip!/");
    assert_eq!(root.parent(), Some(Location::file("/t")));
    assert_eq!(Location::computer().parent(), None);
}

#[test]
fn join_descends_one_component() {
    assert_eq!(
        Location::file("/a").join("b").unwrap(),
        Location::file("/a/b")
    );
    assert_eq!(
        parse("archive:///t/a.zip!/").join("dir").unwrap(),
        parse("archive:///t/a.zip!/dir")
    );
    assert_eq!(
        parse("archive:///t/a.zip!/dir").join("sub").unwrap(),
        parse("archive:///t/a.zip!/dir/sub")
    );
    // A listing never contains a slash, and a virtual place has no children.
    assert!(Location::file("/a").join("a/b").is_err());
    assert!(Location::computer().join("x").is_err());
}

#[test]
fn join_inner_accepts_a_table_of_contents_path() {
    assert_eq!(
        parse("archive:///t/a.zip!/").join_inner("x/y/z").unwrap(),
        parse("archive:///t/a.zip!/x/y/z")
    );
    assert_eq!(
        parse("archive:///t/a.zip!/x").join_inner("y").unwrap(),
        parse("archive:///t/a.zip!/x/y")
    );
    assert_eq!(
        Location::file("/t").join_inner("x/y").unwrap(),
        Location::file("/t/x/y")
    );
    assert!(parse("archive:///t/a.zip!/").join_inner("../x").is_err());
}

#[test]
fn is_within_covers_both_kinds() {
    let root = Location::file("/a");
    assert!(Location::file("/a").is_within(&root));
    assert!(Location::file("/a/b/c").is_within(&root));
    assert!(!Location::file("/ab").is_within(&root));
    assert!(!Location::file("/").is_within(&root));

    let arc = parse("archive:///t/a.zip!/x");
    assert!(parse("archive:///t/a.zip!/x/y").is_within(&arc));
    assert!(arc.is_within(&arc));
    assert!(!parse("archive:///t/a.zip!/xy").is_within(&arc));
    assert!(!parse("archive:///t/b.zip!/x/y").is_within(&arc));
    assert!(!Location::file("/a").is_within(&arc));
}

#[test]
fn an_archive_member_is_within_its_own_container() {
    // The container stated as a plain file, which is how a delete names it: the
    // member has no storage apart from the container, so deleting the container
    // has to count as deleting the member.
    let container = Location::file("/t/a.zip");
    assert!(parse("archive:///t/a.zip!/x/y").is_within(&container));
    assert!(parse("archive:///t/a.zip!/").is_within(&container));
    assert!(!parse("archive:///t/a.zip.old!/x").is_within(&container));
    // The other direction does not hold: a folder on disk is not inside a
    // container that happens to share its name.
    assert!(!container.is_within(&parse("archive:///t/a.zip!/")));
}

#[test]
fn file_name_is_the_leaf() {
    assert_eq!(Location::file("/a/b").file_name(), Some("b"));
    assert_eq!(Location::file("/").file_name(), None);
    assert_eq!(parse("archive:///t/a.zip!/x/y").file_name(), Some("y"));
    // At the archive root the container's own name is the useful label.
    assert_eq!(parse("archive:///t/a.zip!/").file_name(), Some("a.zip"));
    assert_eq!(Location::computer().file_name(), Some("Computer"));
}

#[test]
fn display_shortens_home_only_at_a_boundary() {
    // `shorten_home` caches `$HOME` in a `OnceLock`, so mutating the environment
    // here would be a race against every other test in the binary. Build the
    // expectation from the real home instead, which is what the function reads.
    let home = std::env::var("HOME").unwrap_or_default();
    let under = Location::file(format!("{home}/Docs"));
    // A sibling whose name extends the home prefix must not be shortened.
    let sibling = Location::file(format!("{home}2/Docs"));
    if home.is_empty() {
        assert_eq!(under.display(), "/Docs");
    } else {
        assert_eq!(under.display(), "~/Docs");
        assert_eq!(sibling.display(), format!("{home}2/Docs"));
        assert_eq!(Location::file(&home).display(), "~");
    }
}

#[test]
fn serde_uses_the_uri_form() {
    let loc = parse("archive:///t/a.zip!/x");
    let json = serde_json::to_string(&loc).unwrap();
    assert_eq!(json, "\"archive:///t/a.zip!/x\"");
    assert_eq!(serde_json::from_str::<Location>(&json).unwrap(), loc);
    assert!(serde_json::from_str::<Location>("\"smb://h/s\"").is_err());
}

#[test]
fn trash_location_round_trips() {
    let location = Location::Virtual(VirtualDir::Trash);
    assert_eq!(parse("trash://"), location);
    assert_eq!(location.to_string(), "trash://");
}
