//! The D-Bus wire format of `org.uflinux.Search1`.
//!
//! One JSON string in, one JSON string out, as `org.uflinux.Settings1` does. That
//! choice is what keeps this protocol small: adding a method does not mean adding a
//! signature, and the payload is versioned and validated in one place rather than
//! spread across `GVariant` tuples. The cost is that the shape has to be checked by
//! hand, which is what [`Request::decode`] and [`encode`] are for.
//!
//! Three rules, all of them because the caller is another process:
//!
//! Unknown fields are rejected rather than ignored. A client that sends `mimetype`
//! where the daemon expects `mimes` would otherwise get an unfiltered answer and no
//! explanation, which looks like a bug in the search.
//!
//! Every field of every request has a default, so a client may send the minimum.
//! `{"method":"status"}` is a complete request.
//!
//! Both directions are bounded. A request over [`MAX_REQUEST`] is refused outright;
//! a reply over [`MAX_REPLY`] is shrunk by [`SearchReply::shrink`] rather than
//! refused, because the alternative is telling a user that a search for `a` failed.

use serde::{Deserialize, Serialize};
use ufm_core::Error;

/// The version this build speaks, and the value a client must send. A mismatch is
/// reported as an error rather than negotiated: the two binaries ship together.
pub const PROTOCOL_VERSION: u32 = 1;

/// D-Bus name claimed by the daemon.
pub const SESSION_NAME: &str = "org.uflinux.Search1";

/// Object path the interface is exported on.
pub const OBJECT_PATH: &str = "/org/uflinux/Search";

/// Name of the `Changed` signal: something was indexed, so a result set may be stale.
pub const CHANGED: &str = "Changed";

/// Name of the `Progress` signal: a crawl is running and says how far it has got.
pub const PROGRESS: &str = "Progress";

/// Largest request body accepted. A search bar plus a few filters is a few hundred
/// bytes; the cap exists so a confused or hostile client cannot make the daemon
/// allocate before it has decided anything.
pub const MAX_REQUEST: usize = 256 * 1024;

/// Largest reply body. A page of 1000 hits with snippets can exceed it, which is
/// what [`SearchReply::shrink`] is for.
pub const MAX_REPLY: usize = 2 * 1024 * 1024;

/// The error namespace. `Invalid` is a malformed request, `Unavailable` is a daemon
/// that cannot answer — no index, or a crawl holding the writer open.
pub mod error {
    /// A request this daemon cannot act on: unknown field, wrong version, bad scope.
    pub const INVALID: &str = "org.uflinux.Search1.Error.Invalid";
    /// The index is not open, or is busy being rebuilt. Retryable.
    pub const UNAVAILABLE: &str = "org.uflinux.Search1.Error.Unavailable";
    /// The answer would not fit in one D-Bus message even after shrinking.
    pub const TOO_LARGE: &str = "org.uflinux.Search1.Error.TooLarge";
}

/// Why a request was refused, with a message safe to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The body is not valid JSON for this interface, or carries a field the
    /// daemon does not know.
    Invalid,
    /// The index cannot answer right now.
    Unavailable,
    /// The reply does not fit.
    TooLarge,
}

impl WireError {
    /// The D-Bus error name to return.
    #[must_use]
    pub const fn dbus_name(self) -> &'static str {
        match self {
            Self::Invalid => error::INVALID,
            Self::Unavailable => error::UNAVAILABLE,
            Self::TooLarge => error::TOO_LARGE,
        }
    }

    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Invalid => "Unknown or malformed request",
            Self::Unavailable => "The search index is not available",
            Self::TooLarge => "The answer is too large to return",
        }
    }

    #[must_use]
    pub fn into_error(self) -> Error {
        Error::Protocol(self.message().to_string())
    }
}

impl From<WireError> for Error {
    fn from(e: WireError) -> Self {
        e.into_error()
    }
}

/// What to order a page by. Mirrors [`ufm_index::SearchOrder`]; a separate type
/// because the wire names are part of the protocol and must not change when the
/// collector internals do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Order {
    #[default]
    Relevance,
    NameAsc,
    NameDesc,
    SizeAsc,
    SizeDesc,
    ModifiedAsc,
    ModifiedDesc,
}

impl From<Order> for ufm_index::SearchOrder {
    fn from(order: Order) -> Self {
        match order {
            Order::Relevance => Self::Relevance,
            Order::NameAsc => Self::NameAsc,
            Order::NameDesc => Self::NameDesc,
            Order::SizeAsc => Self::SizeAsc,
            Order::SizeDesc => Self::SizeDesc,
            Order::ModifiedAsc => Self::ModifiedAsc,
            Order::ModifiedDesc => Self::ModifiedDesc,
        }
    }
}

fn yes() -> bool {
    true
}

fn page_size() -> usize {
    ufm_index::DEFAULT_PAGE_SIZE
}

/// Everything a caller may ask about, in both directions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Query {
    /// Raw text from the search bar.
    pub text: String,
    /// Directory to search inside. `None` is the whole index.
    pub scope: Option<String>,
    pub mimes: Vec<String>,
    pub labels: Vec<String>,
    pub order: Order,
    /// Hold folders above files within the page.
    #[serde(default = "yes")]
    pub folders_first: bool,
    pub include_hidden: bool,
    /// Match extracted text as well, and return snippets.
    pub content: bool,
    pub page: usize,
    #[serde(default = "page_size")]
    pub page_size: usize,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            text: String::new(),
            scope: None,
            mimes: Vec::new(),
            labels: Vec::new(),
            order: Order::default(),
            folders_first: true,
            include_hidden: false,
            content: false,
            page: 0,
            page_size: ufm_index::DEFAULT_PAGE_SIZE,
        }
    }
}

impl Query {
    /// Reject what the index layer would silently accept or reject unpredictably.
    /// A scope that is not absolute cannot name a directory, and a page size of
    /// zero would ask for no results at all.
    pub fn validate(&self) -> std::result::Result<(), WireError> {
        if let Some(scope) = &self.scope {
            if !scope.starts_with('/') {
                return Err(WireError::Invalid);
            }
        }
        if self.page_size == 0 || self.page_size > ufm_index::MAX_PAGE_SIZE {
            return Err(WireError::Invalid);
        }
        if self.text.len() > MAX_REQUEST {
            return Err(WireError::TooLarge);
        }
        Ok(())
    }
}

impl From<Query> for ufm_index::SearchRequest {
    fn from(query: Query) -> Self {
        Self {
            text: query.text,
            scope: query.scope,
            mimes: query.mimes,
            labels: query.labels,
            order: query.order.into(),
            folders_first: query.folders_first,
            include_hidden: query.include_hidden,
            content: query.content,
            page: query.page,
            page_size: query.page_size,
        }
    }
}

impl From<&ufm_index::SearchRequest> for Query {
    fn from(request: &ufm_index::SearchRequest) -> Self {
        Self {
            text: request.text.clone(),
            scope: request.scope.clone(),
            mimes: request.mimes.clone(),
            labels: request.labels.clone(),
            order: match request.order {
                ufm_index::SearchOrder::Relevance => Order::Relevance,
                ufm_index::SearchOrder::NameAsc => Order::NameAsc,
                ufm_index::SearchOrder::NameDesc => Order::NameDesc,
                ufm_index::SearchOrder::SizeAsc => Order::SizeAsc,
                ufm_index::SearchOrder::SizeDesc => Order::SizeDesc,
                ufm_index::SearchOrder::ModifiedAsc => Order::ModifiedAsc,
                ufm_index::SearchOrder::ModifiedDesc => Order::ModifiedDesc,
            },
            folders_first: request.folders_first,
            include_hidden: request.include_hidden,
            content: request.content,
            page: request.page,
            page_size: request.page_size,
        }
    }
}

/// One request. The tag names are the method names on the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// Filenames, and file text where a root opted in.
    SearchFiles {
        #[serde(flatten)]
        query: Query,
    },
    /// Desktop entries. Same shape, different index, and `scope` is meaningless
    /// there, so it is not accepted.
    SearchApps {
        text: String,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        order: Order,
        #[serde(default)]
        page: usize,
        #[serde(default = "page_size")]
        page_size: usize,
    },
    /// Index statistics, for the settings page and for the memory benchmark.
    Status,
    /// Re-index from scratch, or one root if given.
    Reindex {
        #[serde(default)]
        root: Option<String>,
    },
    /// Replace the set of directories to index. Absolute paths.
    SetRoots { roots: Vec<String> },
}

impl Request {
    /// Parse a request body, enforcing the size cap and the field rules.
    pub fn decode(body: &[u8]) -> std::result::Result<Self, WireError> {
        if body.len() > MAX_REQUEST {
            return Err(WireError::TooLarge);
        }
        let text = std::str::from_utf8(body).map_err(|_| WireError::Invalid)?;
        serde_json::from_str(text).map_err(|_| WireError::Invalid)
    }

    /// The index this request reads, where the question has one.
    #[must_use]
    pub fn kind(&self) -> Option<ufm_index::IndexKind> {
        match self {
            Self::SearchFiles { .. } => Some(ufm_index::IndexKind::Files),
            Self::SearchApps { .. } => Some(ufm_index::IndexKind::Apps),
            Self::Status | Self::Reindex { .. } | Self::SetRoots { .. } => None,
        }
    }
}

/// One result row, as it crosses the bus.
///
/// `PartialEq` and not `Eq`, because a score is an `f32`; the same reason applies to
/// [`ufm_index::Hit`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Hit {
    pub path: String,
    pub name: String,
    /// `"file"` or `"dir"`.
    pub kind: String,
    pub mime: Option<String>,
    pub size: i64,
    /// Whole seconds since the Unix epoch.
    pub modified: i64,
    pub hidden: bool,
    pub labels: Vec<String>,
    /// BM25 score, or 0 when ordered by a field. `f32` on the wire, so it is
    /// serialized as a number and a client that ignores it loses nothing.
    pub score: f32,
    pub snippet: Option<Snippet>,
}

/// A highlighted fragment. `marks` are half-open byte ranges into `text`, sorted and
/// non-overlapping, ready for Pango attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Snippet {
    pub text: String,
    pub marks: Vec<[usize; 2]>,
}

/// The answer to a search.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SearchReply {
    /// Documents matching the whole request, not just this page.
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
    pub hits: Vec<Hit>,
    /// Whether the reply was shrunk to fit [`MAX_REPLY`], and how. A client shows
    /// "not all results shown" rather than guessing.
    pub truncated: Option<String>,
}

/// What [`SearchReply::shrink`] did to a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncated {
    /// Snippets were dropped; every hit is still there.
    Snippets,
    /// Hits were dropped from the end of the page.
    Page,
}

impl Truncated {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Snippets => "snippets",
            Self::Page => "page",
        }
    }
}

impl SearchReply {
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        serde_json::to_string(self).map_or(usize::MAX, |s| s.len())
    }

    /// Give up something to fit the budget, returning what was given up, or `None`
    /// when there is nothing left to give.
    ///
    /// Snippets first: they are the largest part of a hit, and a page of results
    /// without highlights is still a correct answer, whereas a shorter page is not
    /// the answer that was asked for.
    pub fn shrink(&mut self) -> Option<Truncated> {
        if self.hits.iter().any(|hit| hit.snippet.is_some()) {
            for hit in &mut self.hits {
                hit.snippet = None;
            }
            self.truncated = Some(Truncated::Snippets.as_str().to_string());
            return Some(Truncated::Snippets);
        }
        // Half the page at a time: shrinking one hit per iteration would be a
        // hundred encodes of a still-too-large document.
        let keep = self.hits.len() / 2;
        if keep == 0 {
            return None;
        }
        self.hits.truncate(keep);
        self.truncated = Some(Truncated::Page.as_str().to_string());
        Some(Truncated::Page)
    }
}

/// The answer to `Status`.
///
/// `PartialEq` by hand: the per-index statistics come from tantivy, whose own type
/// does not implement it, and comparing them means naming the fields that a client
/// actually branches on rather than whatever a derive would sweep in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StatusReply {
    pub version: u32,
    /// One entry per index, `files` and `apps`.
    pub indexes: Vec<ufm_index::IndexStatus>,
    /// Directories the daemon is configured to index.
    pub roots: Vec<String>,
    /// Whether a crawl is running, which is when a `Reindex` would be refused.
    pub busy: bool,
    /// A problem worth showing, such as a root that cannot be read.
    pub note: Option<String>,
}

impl PartialEq for StatusReply {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.roots == other.roots
            && self.busy == other.busy
            && self.note == other.note
            && self.indexes.len() == other.indexes.len()
            && self
                .indexes
                .iter()
                .zip(&other.indexes)
                .all(|(mine, theirs)| {
                    mine.kind == theirs.kind
                        && mine.path == theirs.path
                        && mine.records == theirs.records
                })
    }
}

/// A signal body: something was indexed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Changed {
    /// `"files"` or `"apps"`.
    pub kind: String,
    /// Monotonic counter; a client that saw a larger generation already has this one.
    pub generation: u64,
    /// Records searchable now.
    pub records: u64,
}

/// A signal body: a crawl in progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Progress {
    pub kind: String,
    /// What the daemon is doing right now, for a status line.
    pub phase: String,
    pub done: u64,
    /// 0 when the total is not yet known, which is most of the time.
    pub total: u64,
}

/// Whatever a method returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Reply {
    Search(SearchReply),
    Status(StatusReply),
    /// A request that changes something and has nothing to report.
    Ack,
}

impl Reply {
    /// Serialize a reply, refusing to build a message over the budget.
    ///
    /// The engine shrinks a search reply before getting here; reaching the error
    /// means a `Status` reply alone is oversized, which means something is wrong
    /// rather than the user having asked for a lot.
    pub fn encode(&self) -> std::result::Result<String, WireError> {
        let text = serde_json::to_string(self).map_err(|_| WireError::Invalid)?;
        if text.len() > MAX_REPLY {
            return Err(WireError::TooLarge);
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(text: &str) -> std::result::Result<Request, WireError> {
        Request::decode(text.as_bytes())
    }

    #[test]
    fn a_minimal_request_needs_only_its_method() {
        let request = decode(r#"{"method":"status"}"#).expect("a status request");
        assert_eq!(request, Request::Status);
        assert_eq!(request.kind(), None);
    }

    #[test]
    fn every_optional_field_of_a_query_has_a_default() {
        let Request::SearchFiles { query } =
            decode(r#"{"method":"search-files","text":"notes"}"#).expect("a search")
        else {
            panic!("the method asked for a file search");
        };
        assert_eq!(query.text, "notes");
        assert_eq!(query.page_size, ufm_index::DEFAULT_PAGE_SIZE);
        assert!(
            query.folders_first,
            "folders first is the default and has to stay the default when omitted"
        );
        assert!(!query.content);
        assert_eq!(query.order, Order::Relevance);
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let err = decode(r#"{"method":"search-files","text":"x","mimetype":["text/plain"]}"#)
            .expect_err("a misspelled filter must not be dropped quietly");
        assert_eq!(
            err,
            WireError::Invalid,
            "a request carrying a field the daemon has never heard of is refused, "
        );
    }

    #[test]
    fn an_unknown_method_is_refused() {
        assert!(decode(r#"{"method":"search-everything"}"#).is_err());
    }

    #[test]
    fn a_request_over_the_cap_is_refused_before_parsing() {
        let big = format!(
            r#"{{"method":"search-files","text":"{}"}}"#,
            "x".repeat(MAX_REQUEST)
        );
        let err = Request::decode(big.as_bytes()).expect_err("over the cap");
        assert_eq!(err, WireError::TooLarge);
    }

    #[test]
    fn a_query_that_cannot_be_satisfied_is_rejected() {
        let err = Query {
            scope: Some("Documents".to_string()),
            ..Default::default()
        }
        .validate()
        .expect_err("a relative scope names nothing");
        assert_eq!(err, WireError::Invalid);

        let err = Query {
            page_size: 0,
            ..Default::default()
        }
        .validate()
        .expect_err("a page of zero results is not an answer");
        assert_eq!(err, WireError::Invalid);

        let err = Query {
            page_size: ufm_index::MAX_PAGE_SIZE + 1,
            ..Default::default()
        }
        .validate()
        .expect_err("a page must be bounded");
        assert_eq!(err, WireError::Invalid);

        Query {
            scope: Some("/home/u/Documents".to_string()),
            page_size: ufm_index::MAX_PAGE_SIZE,
            ..Default::default()
        }
        .validate()
        .expect("an absolute scope and a legal page are a valid request");
    }

    #[test]
    fn a_query_survives_the_round_trip_through_the_index_request() {
        let query = Query {
            text: "repo".to_string(),
            scope: Some("/home/u".to_string()),
            mimes: vec!["text/plain".to_string()],
            labels: vec!["System".to_string()],
            order: Order::ModifiedDesc,
            folders_first: false,
            include_hidden: true,
            content: true,
            page: 3,
            page_size: 50,
        };
        let request = ufm_index::SearchRequest::from(query.clone());
        assert_eq!(request.order, ufm_index::SearchOrder::ModifiedDesc);
        assert_eq!(Query::from(&request), query);
    }

    fn hit(path: &str, snippet: Option<&str>) -> Hit {
        Hit {
            path: path.to_string(),
            name: "notes.txt".to_string(),
            kind: "file".to_string(),
            mime: Some("text/plain".to_string()),
            size: 12,
            modified: 1_700_000_000,
            hidden: false,
            labels: Vec::new(),
            score: 1.5,
            snippet: snippet.map(|text| Snippet {
                text: text.to_string(),
                marks: vec![[0, 3]],
            }),
        }
    }

    #[test]
    fn a_reply_round_trips_and_reports_its_size() {
        let reply = Reply::Search(SearchReply {
            total: 1,
            page: 0,
            page_size: 200,
            hits: vec![hit("/home/u/notes.txt", Some("the quick brown fox"))],
            truncated: None,
        });
        let text = reply.encode().expect("a small reply");
        assert!(text.contains("\"reply\":\"search\""));
        let back: Reply = serde_json::from_str(&text).expect("the reply we just wrote");
        assert_eq!(back, reply);
    }

    #[test]
    fn an_oversized_reply_gives_up_snippets_before_results() {
        let mut reply = SearchReply {
            total: 2,
            page: 0,
            page_size: 200,
            hits: vec![
                hit("/home/u/notes.txt", Some("a".repeat(2000).as_str())),
                hit("/home/u/other.txt", Some("b".repeat(2000).as_str())),
            ],
            truncated: None,
        };
        assert_eq!(reply.shrink(), Some(Truncated::Snippets));
        assert!(
            reply.hits.iter().all(|h| h.snippet.is_none()),
            "both hits are still there, without highlights"
        );
        assert_eq!(reply.hits.len(), 2);
        assert_eq!(
            reply.shrink(),
            Some(Truncated::Page),
            "and the next thing to give up is half the page"
        );
        assert_eq!(reply.hits.len(), 1);
        assert_eq!(
            reply.shrink(),
            None,
            "one hit cannot be halved, so there is nothing left to do"
        );
    }

    #[test]
    fn a_reply_reports_its_own_length() {
        let reply = SearchReply {
            total: 0,
            page: 0,
            page_size: 200,
            hits: Vec::new(),
            truncated: None,
        };
        assert!(reply.encoded_len() > 0 && reply.encoded_len() < 200);
    }

    #[test]
    fn an_error_name_is_stable_because_a_client_matches_on_it() {
        assert_eq!(
            WireError::Invalid.dbus_name(),
            "org.uflinux.Search1.Error.Invalid"
        );
        assert_eq!(
            WireError::Unavailable.dbus_name(),
            "org.uflinux.Search1.Error.Unavailable"
        );
    }

    #[test]
    fn a_signal_body_is_small_and_typed() {
        let changed = Changed {
            kind: "files".to_string(),
            generation: 7,
            records: 12_345,
        };
        let text = serde_json::to_string(&changed).expect("a signal");
        assert_eq!(
            serde_json::from_str::<Changed>(&text).expect("the signal we just wrote"),
            changed
        );
        let progress = Progress {
            kind: "files".to_string(),
            phase: "/home/u".to_string(),
            done: 40,
            total: 0,
        };
        assert!(
            serde_json::to_string(&progress)
                .expect("a signal")
                .contains("\"total\":0")
        );
    }
}
