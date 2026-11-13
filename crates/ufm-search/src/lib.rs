//! The layer a serving process needs around the index.
//!
//! [`ufm_index`] knows how to store records and rank matches. It does not know
//! anything about answering a request from another process, and it should not: a
//! library that has opinions about D-Bus cannot be linked into the file manager,
//! the indexer daemon and a command-line reindex tool at the same time. This crate
//! holds the parts those three agree on and nothing else.
//!
//! The wire format lives here rather than in the daemon because both ends need it.
//! The file manager cannot send a request it has to build by hand, and the daemon
//! cannot answer a request whose shape it guessed at. Duplicating the struct
//! definitions in each caller is how the two ends drift apart, and the failure mode
//! of that drift is a runtime "unknown field" error rather than a compile error, so
//! the shared types are the fix. The types are plain `serde`; nothing here names
//! `GLib`, which is what lets the file manager use them from a GTK process and this
//! workspace's tests run without any system libraries present.
//!
//! Five modules:
//!
//! [`wire`] is the protocol: request, reply, the two signal bodies, the size caps,
//! and the error names. It converts to and from the index layer's own request type,
//! so the two can be changed independently, and it shrinks an oversized reply rather
//! than refusing it.
//!
//! [`config`] is the indexer's own configuration file: which directories to index
//! and how much of each to read. Separate from the file manager's preferences on
//! purpose — see the module docs.
//!
//! [`extract`] turns a file into text, shelling out for formats that need it, and
//! [`launch`] expands a desktop entry's `Exec` into an argv. Both are here because
//! both are needed before anything is displayed: extraction decides what is
//! searchable, and `Exec` decides whether a result can be started.
//!
//! [`engine`] is the dispatcher: it owns the two indexes, answers a request, and
//! hands back the indexing work a worker thread should run rather than doing it
//! inline. The daemon is thin because of this — it converts a D-Bus call into a
//! [`wire::Request`], calls [`Engine::dispatch`], and writes the answer back.
//!
//! Memory: everything in here is bounded by construction. A request is capped
//! before it is parsed, a reply is capped before it is sent, extraction is capped
//! per file and per filter, and the only unbounded thing is a crawl's record count,
//! which is tantivy's writer budget rather than this crate's heap.

#![deny(unsafe_code)]

pub mod config;
pub mod engine;
pub mod extract;
pub mod launch;
pub mod wire;

pub use config::{ContentMode, IndexConfig, Root};
pub use engine::{Busy, Dispatch, Engine, Task};
pub use extract::{Chain, CommandExtractor, Limit};
pub use launch::{Context as LaunchContext, Exec, expand as expand_exec, launchable};
pub use ufm_core::{Error, Result};
pub use wire::{
    CHANGED, Changed, Hit, MAX_REPLY, MAX_REQUEST, OBJECT_PATH, Order, PROGRESS, PROTOCOL_VERSION,
    Progress, Query as SearchQuery, Reply, Request, SESSION_NAME, SearchReply, Snippet,
    StatusReply, Truncated, WireError, error as errors,
};
