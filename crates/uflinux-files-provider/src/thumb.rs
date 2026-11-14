//! Thumbnails, and the one place the provider touches image bytes.
//!
//! The rules — where a cached picture lives, when it is stale, what may be
//! decoded at all — belong to [`ufm_thumb`], because the file manager builds the
//! same pictures for the same files and a cache the two disagree about is a
//! cache that is read twice. What stays here is the protocol half: the desktop
//! spec's reply body, and the fact that a picture travels over the bus as a file
//! descriptor rather than as bytes in a message.
//!
//! The descriptor is the shape that decision forced on this module. The provider
//! has to hand over something readable, which means either a temporary file or a
//! memory fd. A temporary file costs a directory entry, a write and an unlink per
//! icon, and leaves something behind when the provider is killed; `memfd_create`
//! costs one syscall and disappears with the process. On a target small enough to
//! turn thumbnails off by default, the fd is the cheaper answer by a wide margin.

use ufm_desktop_ipc::{Format, Thumbnail};

pub use ufm_thumb::{Job, MAX_SIZE, available, to_memfd};

/// The reply body for a job, plus the bytes that go with it.
///
/// The two are returned together because they are only meaningful together: the
/// fd index in the [`Thumbnail`] describes the D-Bus message that carries the
/// descriptor, so a caller cannot keep one and drop the other.
pub struct Reply {
    /// The JSON body to put on the bus.
    pub body: Thumbnail,
    /// The PNG to write to the pipe, when there is one.
    pub png: Option<Vec<u8>>,
}

impl Reply {
    /// The "use an icon" answer, which is also the answer to every failure here.
    #[must_use]
    pub const fn icon() -> Self {
        Self {
            body: Thumbnail::icon(),
            png: None,
        }
    }
}

/// Answer one request: cache first, then decode, then give up.
///
/// Every failure path returns [`Reply::icon`], including the ones that would be
/// errors elsewhere — an unreadable file, an unsupported format, a decode that ran
/// out of memory. A renderer told to fall back to an icon draws a desktop that
/// still looks right, and the alternative is a bus error per icon for a feature
/// that is optional by design.
#[must_use]
pub fn answer(job: &Job) -> Reply {
    let Some(png) = ufm_thumb::bytes(job) else {
        return Reply::icon();
    };
    // The size is read back from the bytes rather than assumed, because a cached
    // file may have been written at another size by another component, and a
    // renderer that scales to the wrong box looks broken rather than stale.
    let Some((width, height)) = ufm_thumb::png_size(&png) else {
        return Reply::icon();
    };
    Reply {
        body: Thumbnail {
            width,
            height,
            format: Format::Png,
            fd: Some(0),
        },
        png: Some(png),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_an_icon_and_not_an_error() {
        let reply = Reply::icon();
        assert_eq!(reply.body, Thumbnail::icon());
        assert_eq!(reply.body.format, Format::Icon);
        assert_eq!(reply.body.fd, None);
        assert!(reply.png.is_none());
    }

    #[test]
    fn a_missing_file_is_answered_with_an_icon() {
        let reply = answer(&Job::new("/nonexistent-desktop/a.png", 128));
        assert_eq!(reply.body, Thumbnail::icon());
        assert!(reply.png.is_none());
    }

    #[test]
    fn a_text_file_is_answered_with_an_icon() {
        let dir = tempfile::tempdir().expect("a desktop directory");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"not an image at all").expect("a file to thumbnail");
        let reply = answer(&Job::new(&path, 128));
        assert_eq!(reply.body, Thumbnail::icon());
    }
}
