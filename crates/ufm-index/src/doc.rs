//! One indexable record, and the tantivy document it becomes.
//!
//! The record is a flat value type rather than a builder over a path, because
//! the two producers are very different: the file crawl has a `DirEntry` and a
//! `Metadata` at hand, while the application index has a parsed desktop entry.
//! Both end up here, and only this module knows how a record is laid out into
//! fields, which keeps the facet rules in one place.
//!
//! Paths have to be valid UTF-8 to be indexed at all. The stored `path` is the
//! key a hit is opened with and the term a delete is issued against, so a lossy
//! conversion would produce results that cannot be opened. A file whose name is
//! not valid UTF-8 is skipped, counted, and still shows up in a plain listing.

use std::path::Path;
use std::time::UNIX_EPOCH;

use tantivy::schema::{Facet, Term};
use tantivy::{DateTime, TantivyDocument};

use crate::schema::Fields;

/// Longest extracted text that goes into the `content` field.
///
/// Two megabytes is roughly 400 000 words, well past the point where more text
/// improves a snippet, and it bounds what one unusual log file can add to the
/// writer's memory and to the doc store.
pub const MAX_CONTENT_BYTES: usize = 2 * 1024 * 1024;

/// Whether a record is a folder or a file. Kept separate from
/// [`ufm_core::EntryKind`] because the index only ever distinguishes the two:
/// a mount point, an archive and a socket are all "a thing you can open", and
/// the view re-reads the real kind from disk when it shows the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
}

impl Kind {
    /// The facet component this kind is indexed under.
    #[must_use]
    pub const fn facet_component(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
        }
    }

    /// The one-component facet a record of this kind is indexed with.
    #[must_use]
    pub fn facet(self) -> Facet {
        Facet::from_path([self.facet_component()])
    }

    /// Read a kind back out of a stored facet value. Anything unrecognised is
    /// treated as a file, which is the safe answer: opening a folder as a file
    /// fails loudly, opening a file as a folder silently shows an empty one.
    #[must_use]
    pub fn from_facet_value(encoded: &str) -> Self {
        match encoded {
            "dir" => Self::Directory,
            _ => Self::File,
        }
    }
}

/// Everything the index knows about one file, folder or application.
#[derive(Debug, Clone)]
pub struct Record {
    /// Absolute, valid-UTF-8 path. For an application this is the path of the
    /// `.desktop` file, which is what gets opened.
    pub path: String,
    /// The directory containing `path`, absolute, with no trailing slash except
    /// for the filesystem root.
    pub parent: String,
    /// The last component of `path`, or the application's display name.
    pub name: String,
    pub kind: Kind,
    pub mime: Option<String>,
    /// Size in bytes, or 0 for a directory.
    pub size: i64,
    /// Modification time in whole seconds since the Unix epoch.
    pub modified: i64,
    pub hidden: bool,
    /// Extracted text, for an index built with a content field.
    pub content: Option<String>,
    /// Free-form tags: desktop-entry categories, keywords, the localized name.
    pub labels: Vec<String>,
}

impl Record {
    /// Build a record for a path plus the metadata already read for it. Returns
    /// `None` when the path cannot be indexed, for the reasons listed at the top
    /// of this module.
    #[must_use]
    pub fn from_path(path: &Path, metadata: &std::fs::Metadata, name: &str) -> Option<Self> {
        let path_str = path.to_str()?;
        if name.is_empty() || name == "." || name == ".." {
            return None;
        }
        let parent = parent_of(path_str)?;
        let kind = if metadata.is_dir() {
            Kind::Directory
        } else {
            Kind::File
        };
        Some(Self {
            path: path_str.to_string(),
            parent,
            name: name.to_string(),
            kind,
            mime: kind
                .is_file()
                .then(|| {
                    mime_guess::from_path(path)
                        .first()
                        .map(|m| m.essence_str().to_string())
                })
                .flatten(),
            size: if kind.is_file() {
                i64::try_from(metadata.len()).unwrap_or(i64::MAX)
            } else {
                0
            },
            modified: metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_secs()).ok())
                .unwrap_or(0),
            hidden: name.starts_with('.'),
            content: None,
            labels: Vec::new(),
        })
    }

    /// The term that identifies this record, for delete-by-term.
    #[must_use]
    pub fn term(&self, fields: &Fields) -> Term {
        Term::from_field_text(fields.path, &self.path)
    }

    /// Lay the record out into a tantivy document.
    #[must_use]
    pub fn to_doc(&self, fields: &Fields) -> TantivyDocument {
        let mut doc = TantivyDocument::with_capacity(
            self.content
                .as_ref()
                .map_or(256, |c| 256 + c.len().min(MAX_CONTENT_BYTES)),
        );
        doc.add_text(fields.path, &self.path);
        doc.add_text(fields.name, &self.name);
        // The prefix field indexes the name without its extension too, so `notes`
        // finds `notes.odt`. One extra token per record, no extra tokenizer.
        doc.add_text(fields.name_prefix, &self.name);
        if let Some(stem) = stem_of(&self.name) {
            doc.add_text(fields.name_prefix, stem);
        }
        if let Some(facet) = facet_for(&self.parent) {
            doc.add_facet(fields.parent, facet);
        }
        doc.add_facet(fields.kind, self.kind.facet());
        if let Some(mime) = &self.mime {
            doc.add_text(fields.mime, mime);
        }
        doc.add_i64(fields.size, self.size);
        doc.add_date(
            fields.modified,
            DateTime::from_timestamp_secs(self.modified),
        );
        doc.add_bool(fields.hidden, self.hidden);
        if let (Some(field), Some(text)) = (fields.content, &self.content) {
            doc.add_text(field, truncate(text));
        }
        for label in &self.labels {
            doc.add_text(fields.labels, label);
        }
        doc
    }
}

impl Kind {
    #[must_use]
    pub const fn is_file(self) -> bool {
        matches!(self, Self::File)
    }

    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// The parent directory of an absolute path, in the form the facet wants: no
/// trailing slash, except `"/"` itself.
#[must_use]
pub fn parent_of(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let end = path.rfind('/')?;
    if end == 0 {
        Some("/".to_string())
    } else {
        Some(path[..end].to_string())
    }
}

/// The facet for a directory path.
///
/// Splitting the string on `/` and dropping the empty pieces is exactly the
/// component list a facet wants, and it cannot produce a component containing
/// the separator, because a path component never contains `/` and a NUL byte
/// cannot appear in a path at all.
#[must_use]
pub fn facet_for(dir: &str) -> Option<Facet> {
    if !dir.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    // The root directory indexes as the root facet, which every other facet also
    // emits, so scoping to "/" matches everything without a special case.
    Some(Facet::from_path(parts))
}

/// The file name without its extension, when there is one worth removing.
fn stem_of(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    // Nothing before the dot (`.bashrc`) or nothing after it (`archive.`) means
    // the dot is not separating an extension.
    if dot == 0 || dot + 1 >= name.len() {
        return None;
    }
    // A long "extension" is not one: `2024.1024` is a version, not a type.
    if name.len() - dot - 1 > 8 {
        return None;
    }
    Some(&name[..dot])
}

/// Cut to [`MAX_CONTENT_BYTES`] without splitting a UTF-8 character, which
/// `truncate` on a string slice would panic on.
fn truncate(text: &str) -> &str {
    if text.len() <= MAX_CONTENT_BYTES {
        return text;
    }
    let mut end = MAX_CONTENT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::schema;
    // `as_str`, `as_facet` and friends are on the `Value` trait, which a document
    // value only has in scope where the trait is imported.
    use tantivy::schema::Value as _;

    fn fields() -> Fields {
        Fields::from_schema(&schema(true)).expect("test schema")
    }

    #[test]
    fn parent_of_a_nested_file() {
        assert_eq!(parent_of("/home/u/a.txt").as_deref(), Some("/home/u"));
        assert_eq!(parent_of("/home/u").as_deref(), Some("/home"));
        assert_eq!(parent_of("/a.txt").as_deref(), Some("/"));
        assert_eq!(parent_of("relative/a.txt"), None);
    }

    #[test]
    fn a_directory_indexes_the_root_facet() {
        let root = facet_for("/").expect("root is a facet");
        assert!(root.is_root(), "the root directory is the root facet");
        let nested = facet_for("/home/u/Documents").expect("absolute facet");
        assert_eq!(nested.to_path(), vec!["home", "u", "Documents"]);
    }

    #[test]
    fn the_root_facet_is_a_prefix_of_every_facet() {
        // This is what makes `scope = /` fall out of the same term query as any
        // other scope, instead of a special case that skips the clause.
        let root = facet_for("/").expect("root");
        let nested = facet_for("/home/u").expect("nested");
        assert!(root.is_prefix_of(&nested));
        assert!(!nested.is_prefix_of(&root));
        // Not reflexive, which is why scoping to a folder excludes the folder
        // itself and only returns what is inside it.
        assert!(!root.is_prefix_of(&root));
    }

    #[test]
    fn a_name_with_a_dot_indexes_both_forms() {
        assert_eq!(stem_of("notes.odt"), Some("notes"));
        assert_eq!(
            stem_of(".bashrc"),
            None,
            "a leading dot is not an extension"
        );
        assert_eq!(stem_of("archive."), None);
        assert_eq!(stem_of("no-extension"), None);
        assert_eq!(
            stem_of("v2.10240120345"),
            None,
            "a tail longer than any real extension is not one"
        );
    }

    #[test]
    fn content_is_cut_on_a_character_boundary() {
        let mut text = "a".repeat(MAX_CONTENT_BYTES - 1);
        text.push_str("编辑");
        let cut = truncate(&text);
        assert!(cut.len() <= MAX_CONTENT_BYTES);
        assert!(cut.is_char_boundary(cut.len()));
    }

    #[test]
    fn a_record_lays_out_every_field() {
        let record = Record {
            path: "/home/u/Documents/notes.odt".to_string(),
            parent: "/home/u/Documents".to_string(),
            name: "notes.odt".to_string(),
            kind: Kind::File,
            mime: Some("application/vnd.oasis.opendocument.text".to_string()),
            size: 1024,
            modified: 1_700_000_000,
            hidden: false,
            content: Some("meeting agenda".to_string()),
            labels: vec!["Office".to_string()],
        };
        let fields = fields();
        let doc = record.to_doc(&fields);
        assert_eq!(
            doc.get_first(fields.path).and_then(|v| v.as_str()),
            Some("/home/u/Documents/notes.odt")
        );
        assert_eq!(
            doc.get_first(fields.size).and_then(|v| v.as_i64()),
            Some(1024)
        );
        assert_eq!(
            doc.get_first(fields.modified)
                .and_then(|v| v.as_datetime())
                .map(tantivy::DateTime::into_timestamp_secs),
            Some(1_700_000_000)
        );
        assert_eq!(
            doc.get_first(fields.kind).and_then(|v| v.as_facet()),
            Some("file")
        );
        assert_eq!(
            doc.get_first(fields.parent)
                .and_then(|v| v.as_facet())
                .map(Kind::from_facet_value),
            Some(Kind::File),
            "the stored facet is readable back, which is how a hit becomes a row"
        );
        assert_eq!(
            doc.get_first(fields.require_content().expect("content field"))
                .and_then(|v| v.as_str()),
            Some("meeting agenda")
        );
        let labels: Vec<String> = doc
            .get_all(fields.labels)
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect();
        assert_eq!(labels, vec!["Office"]);
    }

    #[test]
    fn a_record_without_content_still_indexes_the_name_in_both_fields() {
        let record = Record {
            path: "/home/u/notes.odt".to_string(),
            parent: "/home/u".to_string(),
            name: "notes.odt".to_string(),
            kind: Kind::File,
            mime: None,
            size: 0,
            modified: 0,
            hidden: false,
            content: None,
            labels: Vec::new(),
        };
        let fields = fields();
        let doc = record.to_doc(&fields);
        let prefixes: Vec<&str> = doc
            .get_all(fields.name_prefix)
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(prefixes, vec!["notes.odt", "notes"]);
    }

    #[test]
    fn an_application_record_fills_the_body_text_field() {
        // The schema the application index is actually opened with: the same one as
        // the file index, since an entry's `GenericName` and `Comment` are body text
        // in the same sense a document's extracted text is.
        let fields = Fields::from_schema(&schema(true)).expect("apps schema");
        let record = Record {
            path: "/usr/share/applications/org.uflinux.Files.desktop".to_string(),
            parent: "/usr/share/applications".to_string(),
            name: "Files".to_string(),
            kind: Kind::File,
            mime: None,
            size: 0,
            modified: 1,
            hidden: false,
            content: Some("Browse and organise files".to_string()),
            labels: vec!["System".to_string()],
        };
        let doc = record.to_doc(&fields);
        assert_eq!(
            doc.get_first(fields.require_content().expect("content field"))
                .and_then(|v| v.as_str()),
            Some("Browse and organise files")
        );
    }
}
