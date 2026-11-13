//! The search index behind `uflinux-files` and `uflinux-searchd`.
//!
//! Two indexes live side by side under [`ufm_core::paths::index_dir`]: `files`,
//! holding filenames and optionally file text, and `apps`, holding desktop entries.
//! They share a crate because they share everything that matters — the schema, the
//! record layout, the writer policy, the ranking — and differ only in where records
//! come from and how large the body-text field gets. Splitting them would mean two
//! places to change every time the writer policy is retuned, and a start menu that
//! ranks applications differently from files.
//!
//! # Why this crate is pure Rust
//!
//! The daemon it serves has a memory budget, and the way to hold a budget is to know
//! what is resident. Tantivy mmaps its segments, so the index is charged to the page
//! cache and can be dropped under pressure rather than sitting in the heap; the
//! writer's hash table is the one large allocation, and it exists only while
//! something is being indexed. Adding `GLib` here would mean paying for a main
//! context and a type system in a process whose job is to answer a query in a few
//! milliseconds, so the D-Bus surface lives in `uflinux-searchd` instead, and this
//! crate stays testable with `cargo test` and no display server.
//!
//! The target is 24 MiB resident while idle. That is what [`write::IndexHandle`]
//! keeping no writer, and [`scan`] reading a directory at a time, are for.
//!
//! # Layout
//!
//! [`schema`] declares the fields and analyzers, [`doc`] is the record type that
//! knows how to become a tantivy document, [`write`] opens an index and feeds it,
//! [`scan`] walks a filesystem to produce records, [`query`] turns user input into a
//! tantivy query and ranks the hits, and [`apps`] reads `.desktop` files.

#![deny(unsafe_code)]

pub mod apps;
pub mod doc;
pub mod query;
pub mod scan;
pub mod schema;
pub mod write;

pub use apps::{AppCrawler, AppEntry, AppScanStats, Skip as AppSkip, data_dirs, default_session};
pub use doc::{Kind, MAX_CONTENT_BYTES, Record, facet_for};
pub use query::{
    DEFAULT_PAGE_SIZE, Hit, Hits, MAX_PAGE_SIZE, Order as SearchOrder, Request as SearchRequest,
    Snippet as SearchSnippet, build_query, search,
};
pub use scan::{CrawlStats, DEBOUNCE, Extractor, PlainText, ScanPolicy, Scanner};
pub use schema::Fields;
pub use write::{IndexHandle, IndexKind, IndexStatus, WritePolicy, Writer};

pub use ufm_core::{Error, Result};

/// A record for the tests in this crate, so a test that only needs "some file at a
/// path" does not rebuild the same five fields each time.
#[cfg(test)]
pub(crate) fn test_record(path: &str) -> Record {
    let parent = doc::parent_of(path).expect("a test path with a parent");
    let name = path
        .rsplit('/')
        .next()
        .expect("a test path with a name")
        .to_string();
    Record {
        path: path.to_string(),
        parent,
        name,
        kind: Kind::File,
        mime: None,
        size: 1,
        modified: 1_700_000_000,
        hidden: false,
        content: None,
        labels: Vec::new(),
    }
}
