//! Turning a search bar into a ranked page of results.
//!
//! The query is assembled by hand rather than parsed, because the shape is fixed
//! and small: match the words, inside one folder, of one kind, optionally in the
//! body. A [`QueryParser`] would add a grammar the user cannot reach - the search
//! bar has no operators - and its field-disjunction scoring is not what a file
//! search wants, where a name match should beat a body match by a wide margin.
//!
//! Three properties matter here, and [`Request`] and [`Hits`] exist to keep them
//! honest.
//!
//! The total is the total *after* filtering. A search bar that reads "3 of 4 021
//! results" has to have counted 4 021 matching documents, not 4 021 minus the
//! ones dropped from the page, which is why the type and label filters are
//! queries rather than Rust predicates over the results.
//!
//! Everything a row needs comes out of the index. Re-reading metadata per hit
//! would make a search over a slow network folder as slow as browsing it, so the
//! stored fields carry path, kind, size, mtime and labels.
//!
//! Ordering happens inside tantivy. Sorting a page in Rust afterwards gives a page
//! that is not the top of anything, so name, size and mtime are fast fields and
//! the collector merges across segments.

use std::collections::BTreeSet;

use tantivy::collector::{Count, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, DisjunctionMaxQuery, Occur,
    PhrasePrefixQuery, Query, TermQuery,
};
use tantivy::schema::{Facet, Field, IndexRecordOption, Value as _};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::TokenStream as _;
use tantivy::{
    DateTime, DocAddress, Order as TantivyOrder, Score, Searcher, TantivyDocument, Term,
};
use ufm_core::{Error, Result};

use crate::doc::{Kind, facet_for};
use crate::schema::{Fields, PREFIX_MAX, PREFIX_MIN};
use crate::write::IndexHandle;

/// How much a name match is worth against a content match. A file manager is
/// mostly "find that one file", where the word is in the name; content is a
/// fallback, and a document that merely mentions the word in its body should not
/// displace one named after it.
const NAME_BOOST: Score = 8.0;

/// A prefix of the name is worth slightly less than a whole word of it: typing
/// `repo` should show `repo/` before `report.pdf`.
const PREFIX_BOOST: Score = 6.0;

/// Content is the baseline of 1.0, so the boosts above read as multiples of it.
const CONTENT_BOOST: Score = 1.0;

/// How many terms a typeahead expansion may touch. The prefix clause is a range
/// scan over the term dictionary, so this is what bounds its cost when the user
/// has typed a single letter.
const MAX_EXPANSIONS: u32 = 128;

/// Characters of context in a content snippet. Long enough to recognise a
/// sentence, short enough that a page of them is not a wall of text.
const SNIPPET_CHARS: usize = 280;

/// Default page size: a screenful of icons plus room to scroll before the next
/// round trip.
pub const DEFAULT_PAGE_SIZE: usize = 200;

/// Largest page anyone may ask for, so a malformed request cannot ask for the
/// whole result set at once.
pub const MAX_PAGE_SIZE: usize = 1000;

/// What to order a result set by. Direction is part of the variant because each
/// one maps onto a different collector argument, and a `bool` field here would
/// mean every caller has to know that `Relevance` ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    /// BM25 over the name, prefix and content clauses.
    #[default]
    Relevance,
    NameAsc,
    NameDesc,
    SizeAsc,
    SizeDesc,
    ModifiedAsc,
    ModifiedDesc,
}

impl Order {
    /// The sort direction behind a field ordering. [`Order::Relevance`] has no
    /// direction - it is whatever BM25 says - and the only callers asking are
    /// already matched on a variant that has one.
    const fn direction(self) -> TantivyOrder {
        match self {
            Self::NameDesc | Self::SizeDesc | Self::ModifiedDesc => TantivyOrder::Desc,
            _ => TantivyOrder::Asc,
        }
    }
}

/// One search: what to match, where, and which slice of it to return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Raw text from the search bar. Empty means "everything in scope, filtered
    /// and sorted", which is useful on its own.
    pub text: String,
    /// Directory to search inside, absolute. `None` is the whole index.
    pub scope: Option<String>,
    /// Only return records whose MIME type is one of these.
    pub mimes: Vec<String>,
    /// Only return records carrying all of these labels. For applications this is
    /// the desktop-entry `Categories` list.
    pub labels: Vec<String>,
    pub order: Order,
    /// Hold folders above files within the page. See [`Hits::hits`] for what this
    /// does across pages.
    pub folders_first: bool,
    pub include_hidden: bool,
    /// Search extracted text as well. An index built without a content field
    /// simply has nothing to match, and [`Fields::require_content`] is how the
    /// caller finds that out before promising a snippet.
    pub content: bool,
    pub page: usize,
    pub page_size: usize,
}

impl Default for Request {
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
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

impl Request {
    /// Clamp what a caller may get wrong, so a page size of zero cannot ask
    /// tantivy for a collector over an empty range.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.page_size = self.page_size.clamp(1, MAX_PAGE_SIZE);
        self
    }

    /// Narrow the search to a directory.
    #[must_use]
    pub fn in_dir(mut self, dir: impl Into<String>) -> Self {
        self.scope = Some(dir.into());
        self
    }

    #[must_use]
    pub fn order(mut self, order: Order) -> Self {
        self.order = order;
        self
    }

    #[must_use]
    pub fn page(mut self, page: usize, page_size: usize) -> Self {
        self.page = page;
        self.page_size = page_size;
        self
    }
}

/// A highlighted fragment, as text plus the byte ranges to mark.
///
/// Ranges rather than markup: the view draws these with Pango attributes, and an
/// HTML string would have to be parsed again, escaped by hand, or trusted - none
/// of which is better than two numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// Half-open byte ranges into [`Snippet::text`], sorted and non-overlapping.
    pub marks: Vec<(usize, usize)>,
}

/// One result row, with everything needed to draw it and open it.
///
/// `PartialEq` rather than `Eq` because a score is an `f32`; nothing compares
/// hits for equality outside tests, which compare the paths anyway.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub kind: Kind,
    pub mime: Option<String>,
    pub size: i64,
    /// Whole seconds since the Unix epoch.
    pub modified: i64,
    pub hidden: bool,
    pub labels: Vec<String>,
    /// BM25 score, or 0 when the results are ordered by a field.
    pub score: Score,
    /// A highlighted fragment of the file's text, when content was searched and
    /// this file matched there.
    pub snippet: Option<Snippet>,
}

/// A page of results plus the size of the set it was cut from.
#[derive(Debug, Clone)]
pub struct Hits {
    /// Documents matching the whole request, not just this page.
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
    /// The page itself. When `folders_first` is set, folders lead - within the
    /// page, which is the only thing a caller can show at once.
    pub hits: Vec<Hit>,
}

impl Hits {
    /// Whether a later page exists.
    #[must_use]
    pub fn has_more(&self) -> bool {
        self.total > self.page * self.page_size + self.hits.len()
    }
}

/// Run a search against an open index.
pub fn search(handle: &IndexHandle, request: &Request) -> Result<Hits> {
    let request = request.clone().normalized();
    let searcher = handle.searcher();
    let query = build_query(handle, &request)?;
    let offset = request.page.saturating_mul(request.page_size);
    let (total, top) = collect_page(&searcher, query.as_ref(), &request, offset)?;

    // Snippets only where a body match is possible, and only for rows that could
    // have matched there: generating one per hit would decode the doc store of
    // every name match as well.
    let mut generator = match (request.content, handle.fields().content) {
        (true, Some(field)) => Some(snippet_generator(&searcher, query.as_ref(), field)?),
        _ => None,
    };

    let mut hits = read_hits(&searcher, &top, handle.fields(), generator.as_mut())?;
    if request.folders_first {
        // Stable, so the ranking chosen above still decides within each group.
        hits.sort_by_key(|hit| usize::from(!hit.kind.is_dir()));
    }
    Ok(Hits {
        total,
        page: request.page,
        page_size: request.page_size,
        hits,
    })
}

/// The query a [`Request`] compiles to. Public so the daemon can reuse it for a
/// count or a delete, and so ranking can be tested without paging.
pub fn build_query(handle: &IndexHandle, request: &Request) -> Result<Box<dyn Query>> {
    let fields = handle.fields();
    let words = tokens(handle, &request.text);
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    if let Some(scope) = &request.scope {
        let facet = facet_for(scope)
            .ok_or_else(|| Error::InvalidLocation(format!("`{scope}` is not an absolute path")))?;
        clauses.push((Occur::Must, filter(scope_query(*fields, &facet))));
    }

    // Every filter is a `Must` carrying score 0: it narrows the set without
    // changing the relative order of what survives, so a rare MIME type cannot
    // push its own matches to the top.
    if !request.mimes.is_empty() {
        let queries = request
            .mimes
            .iter()
            .map(|mime| term_query(Term::from_field_text(fields.mime, mime)) as Box<dyn Query>)
            .collect();
        clauses.push((
            Occur::Must,
            filter(Box::new(DisjunctionMaxQuery::new(queries))),
        ));
    }

    for label in &request.labels {
        clauses.push((
            Occur::Must,
            filter(term_query(Term::from_field_text(fields.labels, label))),
        ));
    }

    // Whether anything at all is required to match. The hidden filter is a
    // `MustNot`, and a boolean query whose only clause excludes something matches
    // nothing in tantivy - there is no candidate set to apply the exclusion to. So
    // "every visible file", which is what an empty search bar asks for, has to say
    // "everything" as well as "not hidden".
    let nothing_required = request.scope.is_none()
        && request.mimes.is_empty()
        && request.labels.is_empty()
        && words.is_empty();

    if !request.include_hidden {
        clauses.push((
            Occur::MustNot,
            filter(term_query(Term::from_field_bool(fields.hidden, true))),
        ));
    }
    if nothing_required && !clauses.is_empty() {
        clauses.push((Occur::Must, filter(Box::new(AllQuery))));
    }

    let content = fields.content.filter(|_| request.content);
    for word in &words {
        clauses.push((
            Occur::Should,
            ranked(
                term_query(Term::from_field_text(fields.name, word)),
                NAME_BOOST,
            ),
        ));
        // The n-gram field only holds prefixes of length 2..=5, so within that
        // window a term query there is exact and costs one dictionary lookup.
        if word.len() >= PREFIX_MIN && word.len() <= PREFIX_MAX {
            clauses.push((
                Occur::Should,
                ranked(
                    term_query(Term::from_field_text(fields.name_prefix, word)),
                    PREFIX_BOOST,
                ),
            ));
        }
        if let Some(field) = content {
            clauses.push((
                Occur::Should,
                ranked(
                    term_query(Term::from_field_text(field, word)),
                    CONTENT_BOOST,
                ),
            ));
        }
    }

    // The last word is also matched as a prefix, so `repo` finds `report.pdf`
    // while the user is still typing - and keeps finding it after Enter, rather
    // than having a result disappear between the last keystroke and the search.
    if let Some(last) = words.last() {
        let mut prefix = PhrasePrefixQuery::new(vec![Term::from_field_text(fields.name, last)]);
        prefix.set_max_expansions(MAX_EXPANSIONS);
        clauses.push((Occur::Should, ranked(Box::new(prefix), PREFIX_BOOST)));
    }

    if clauses.is_empty() {
        return Ok(Box::new(AllQuery));
    }
    let mut query = BooleanQuery::new(clauses);
    if !words.is_empty() {
        // Without this, the presence of any `Must` - the scope, a label - drops
        // the requirement to zero and the text becomes decoration: every document
        // in scope would match, ranked by nothing.
        query.set_minimum_number_should_match(1);
    }
    Ok(Box::new(query))
}

/// Collect one page and the total, in one pass. Each ordering is a different
/// collector type, so the branches are written out and all reduce to the same
/// list of addresses.
fn collect_page(
    searcher: &Searcher,
    query: &dyn Query,
    request: &Request,
    offset: usize,
) -> Result<(usize, Vec<(Score, DocAddress)>)> {
    let top = TopDocs::with_limit(request.page_size).and_offset(offset);
    let run = |e: tantivy::TantivyError| Error::Search(format!("cannot collect the page: {e}"));
    // The total comes from a `Count` collector paired with the page collector:
    // tantivy runs both over the same scorers, so the count is the filtered total
    // and it costs no second pass over the index.
    macro_rules! page {
        ($collector:expr) => {{
            let (hits, total) = searcher.search(query, &($collector, Count)).map_err(run)?;
            Ok((
                total,
                hits.into_iter().map(|(_, addr)| (0.0, addr)).collect(),
            ))
        }};
    }
    match request.order {
        Order::Relevance => searcher
            .search(query, &(top.order_by_score(), Count))
            .map_err(run)
            .map(|(hits, total)| (total, hits)),
        Order::NameAsc | Order::NameDesc => {
            page!(
                top.order_by_string_fast_field(
                    crate::schema::field::NAME,
                    request.order.direction()
                )
            )
        }
        Order::SizeAsc | Order::SizeDesc => page!(
            top.order_by_fast_field::<i64>(crate::schema::field::SIZE, request.order.direction())
        ),
        Order::ModifiedAsc | Order::ModifiedDesc => page!(top.order_by_fast_field::<DateTime>(
            crate::schema::field::MODIFIED,
            request.order.direction()
        )),
    }
}

fn read_hits(
    searcher: &Searcher,
    top: &[(Score, DocAddress)],
    fields: &Fields,
    mut snippet: Option<&mut SnippetGenerator>,
) -> Result<Vec<Hit>> {
    let mut hits = Vec::with_capacity(top.len());
    for (score, address) in top {
        let doc: TantivyDocument = searcher
            .doc(*address)
            .map_err(|e| Error::Search(format!("cannot read a result: {e}")))?;
        let text = |field: Field| {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let hit = Hit {
            // A hit with no path cannot be opened, and the stored path is what
            // wrote the record, so its absence means a corrupt index.
            path: text(fields.path),
            name: text(fields.name),
            kind: doc
                .get_first(fields.kind)
                .and_then(|v| v.as_facet())
                .map_or(Kind::File, Kind::from_facet_value),
            mime: doc
                .get_first(fields.mime)
                .and_then(|v| v.as_str())
                .map(str::to_string),
            size: doc
                .get_first(fields.size)
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            modified: doc
                .get_first(fields.modified)
                .and_then(|v| v.as_datetime())
                .map_or(0, DateTime::into_timestamp_secs),
            hidden: doc
                .get_first(fields.hidden)
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            labels: doc
                .get_all(fields.labels)
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect(),
            score: *score,
            snippet: snippet.as_mut().and_then(|g| snippet_of(g, &doc)),
        };
        if !hit.path.is_empty() {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn snippet_generator(
    searcher: &Searcher,
    query: &dyn Query,
    field: Field,
) -> Result<SnippetGenerator> {
    let mut generator = SnippetGenerator::create(searcher, query, field)
        .map_err(|e| Error::Search(format!("cannot build a snippet generator: {e}")))?;
    generator.set_max_num_chars(SNIPPET_CHARS);
    Ok(generator)
}

fn snippet_of(generator: &mut SnippetGenerator, doc: &TantivyDocument) -> Option<Snippet> {
    let snippet = generator.snippet_from_doc(doc);
    if snippet.is_empty() {
        return None;
    }
    // `highlighted()` hands back the raw ranges, which can overlap when two terms
    // land in one word; collapsing them is what `to_html` does, and it is what a
    // caller drawing Pango attributes needs too, so the ranges are sorted and
    // disjoint by the time they reach the view.
    let marks = tantivy::snippet::collapse_overlapped_ranges(snippet.highlighted())
        .into_iter()
        .map(|r| (r.start, r.end))
        .collect();
    Some(Snippet {
        text: snippet.fragment().to_string(),
        marks,
    })
}

/// Split the query text the way the name field was indexed, so the terms looked
/// for are exactly the terms stored. Going through the index's own analyzer rather
/// than a hand-rolled split is what keeps the lowercasing and the length limit
/// from drifting apart between indexing and search.
fn tokens(handle: &IndexHandle, text: &str) -> Vec<String> {
    let Ok(mut analyzer) = handle.index().tokenizer_for_field(handle.fields().name) else {
        return Vec::new();
    };
    let mut stream = analyzer.token_stream(text);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    while stream.advance() {
        let token = stream.token().text.clone();
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}

fn scope_query(fields: Fields, facet: &Facet) -> Box<dyn Query> {
    term_query(Term::from_facet(fields.parent, facet))
}

fn term_query(term: Term) -> Box<dyn Query> {
    Box::new(TermQuery::new(term, IndexRecordOption::Basic))
}

/// A clause that narrows the set and contributes nothing to the score.
fn filter(query: Box<dyn Query>) -> Box<dyn Query> {
    Box::new(ConstScoreQuery::new(query, 0.0))
}

fn ranked(query: Box<dyn Query>, boost: Score) -> Box<dyn Query> {
    Box::new(BoostQuery::new(query, boost))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Kind, Record, parent_of};
    use crate::write::{IndexHandle, IndexKind, WritePolicy};

    fn file(path: &str) -> Record {
        let name = path.rsplit('/').next().expect("a path with a name");
        Record {
            path: path.to_string(),
            parent: parent_of(path).expect("a path with a parent"),
            name: name.to_string(),
            kind: Kind::File,
            mime: None,
            size: 10,
            modified: 1_700_000_000,
            hidden: name.starts_with('.'),
            content: None,
            labels: Vec::new(),
        }
    }

    fn folder(path: &str) -> Record {
        let mut record = file(path);
        record.kind = Kind::Directory;
        record.size = 0;
        record
    }

    fn index(handle: &IndexHandle, records: impl IntoIterator<Item = Record>) {
        let mut writer = handle.writer(WritePolicy::default()).expect("a writer");
        for record in records {
            writer.upsert(&record).expect("index a record");
        }
        writer.finish().expect("finish");
    }

    fn handle(dir: &std::path::Path) -> IndexHandle {
        IndexHandle::open(IndexKind::Files, dir).expect("open a fresh index")
    }

    fn paths(hits: &Hits) -> Vec<&str> {
        hits.hits.iter().map(|hit| hit.path.as_str()).collect()
    }

    /// A tree with two folders, files in each, a dotfile, and one file whose only
    /// match would be in its body.
    fn tree() -> Vec<Record> {
        let mut records = vec![
            folder("/home/u"),
            folder("/home/u/Documents"),
            folder("/home/u/Pictures"),
            file("/home/u/Documents/report.pdf"),
            file("/home/u/Documents/summary.txt"),
            file("/home/u/Documents/report-draft.txt"),
            file("/home/u/Documents/.report.swp"),
            file("/home/u/Pictures/report.png"),
        ];
        records[4].mime = Some("text/plain".to_string());
        records[4].content = Some("the report was filed last spring".to_string());
        records[3].mime = Some("application/pdf".to_string());
        records[7].mime = Some("image/png".to_string());
        records
    }

    #[test]
    fn a_name_match_outranks_a_content_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let hits = search(
            &handle,
            &Request {
                text: "report".to_string(),
                content: true,
                folders_first: false,
                ..Request::default()
            },
        )
        .expect("search");
        // Both the file named `report` and the file mentioning it match, and the
        // one named for it comes first.
        assert_eq!(hits.total, 4, "four visible records match `report`");
        assert_eq!(
            hits.hits.first().expect("a first hit").path,
            "/home/u/Documents/report.pdf",
            "the name match must lead: {hits:?}"
        );
        assert!(
            paths(&hits).contains(&"/home/u/Documents/summary.txt"),
            "a body match still belongs in the results: {hits:?}"
        );
    }

    #[test]
    fn a_partial_word_finds_a_longer_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let hits = search(
            &handle,
            &Request {
                text: "repo".to_string(),
                folders_first: false,
                ..Request::default()
            },
        )
        .expect("search");
        assert!(
            paths(&hits).contains(&"/home/u/Documents/report-draft.txt"),
            "typing part of a word has to match: {hits:?}"
        );
    }

    #[test]
    fn a_scope_narrows_to_a_subtree_and_excludes_the_folder_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let hits = search(
            &handle,
            &Request {
                text: "report".to_string(),
                scope: Some("/home/u/Documents".to_string()),
                content: true,
                folders_first: false,
                ..Request::default()
            },
        )
        .expect("search");
        assert_eq!(hits.total, 3, "the Pictures file is out of scope");
        assert!(
            !paths(&hits).contains(&"/home/u/Documents"),
            "the folder searched is not a result of searching it"
        );
    }

    #[test]
    fn a_scope_of_the_root_matches_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let hits = search(
            &handle,
            &Request {
                text: "report".to_string(),
                scope: Some("/".to_string()),
                content: true,
                folders_first: false,
                ..Request::default()
            },
        )
        .expect("search");
        assert_eq!(hits.total, 4, "`/` is every record, not none of them");
    }

    #[test]
    fn a_type_filter_changes_the_total_not_just_the_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let request = |mimes: &[&str]| Request {
            text: "report".to_string(),
            mimes: mimes.iter().map(|m| (*m).to_string()).collect(),
            folders_first: false,
            ..Request::default()
        };
        let one = search(&handle, &request(&["image/png"])).expect("search");
        assert_eq!(one.total, 1);
        assert_eq!(one.hits[0].path, "/home/u/Pictures/report.png");
        let two = search(&handle, &request(&["image/png", "application/pdf"])).expect("search");
        assert_eq!(two.total, 2, "either type counts");
        let none = search(&handle, &request(&["audio/mpeg"])).expect("search");
        assert_eq!(none.total, 0, "a type nothing has matches nothing");
    }

    #[test]
    fn hidden_records_are_left_out_unless_asked_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let request = |include_hidden| Request {
            text: "report".to_string(),
            include_hidden,
            folders_first: false,
            ..Request::default()
        };
        let visible = search(&handle, &request(false)).expect("search");
        assert_eq!(visible.total, 3);
        let all = search(&handle, &request(true)).expect("search");
        assert_eq!(all.total, 4, "the swap file is the fourth");
        assert!(
            paths(&all).contains(&"/home/u/Documents/.report.swp"),
            "asking for hidden files has to return them"
        );
    }

    #[test]
    fn an_empty_search_lists_everything_visible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let hits = search(&handle, &Request::default()).expect("search");
        // Eight records, one of them hidden. Without a positive clause the query
        // would be a bare exclusion, which matches nothing.
        assert_eq!(hits.total, 7, "an empty search bar is a folder listing");
    }

    #[test]
    fn an_empty_search_inside_a_folder_lists_that_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let hits = search(
            &handle,
            &Request {
                scope: Some("/home/u/Documents".to_string()),
                ..Request::default()
            },
        )
        .expect("search");
        assert_eq!(hits.total, 3);
    }

    #[test]
    fn a_label_filter_requires_the_label() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let mut records = tree();
        records[3].labels = vec!["Office".to_string(), "Documents".to_string()];
        records[4].labels = vec!["Office".to_string()];
        index(&handle, records);
        let hits = search(
            &handle,
            &Request {
                text: "report".to_string(),
                labels: vec!["Office".to_string(), "Documents".to_string()],
                folders_first: false,
                ..Request::default()
            },
        )
        .expect("search");
        assert_eq!(hits.total, 1, "both labels, not either one");
        assert_eq!(hits.hits[0].labels, vec!["Office", "Documents"]);
    }

    #[test]
    fn results_can_be_ordered_by_a_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let mut records = vec![
            file("/home/u/b.txt"),
            file("/home/u/a.txt"),
            file("/home/u/c.txt"),
        ];
        records[0].size = 300;
        records[1].size = 100;
        records[2].size = 200;
        records[0].modified = 1_000;
        records[1].modified = 3_000;
        records[2].modified = 2_000;
        index(&handle, records);
        let names = |order| {
            search(
                &handle,
                &Request {
                    order,
                    folders_first: false,
                    ..Request::default()
                },
            )
            .expect("search")
        };
        assert_eq!(
            paths(&names(Order::NameAsc)),
            ["/home/u/a.txt", "/home/u/b.txt", "/home/u/c.txt"]
        );
        assert_eq!(
            paths(&names(Order::NameDesc)),
            ["/home/u/c.txt", "/home/u/b.txt", "/home/u/a.txt"]
        );
        assert_eq!(
            paths(&names(Order::SizeDesc)),
            ["/home/u/b.txt", "/home/u/c.txt", "/home/u/a.txt"]
        );
        assert_eq!(
            paths(&names(Order::ModifiedAsc)),
            ["/home/u/b.txt", "/home/u/c.txt", "/home/u/a.txt"]
        );
    }

    #[test]
    fn folders_lead_when_asked_and_keep_their_relative_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(
            &handle,
            vec![
                file("/home/u/zeta.txt"),
                folder("/home/u/alpha"),
                file("/home/u/beta.txt"),
            ],
        );
        let hits = search(&handle, &Request::default().order(Order::NameDesc)).expect("search");
        assert_eq!(
            paths(&hits),
            ["/home/u/alpha", "/home/u/zeta.txt", "/home/u/beta.txt"],
            "the folder moves up without disturbing the order underneath it"
        );
        let flat = search(
            &handle,
            &Request {
                folders_first: false,
                order: Order::NameDesc,
                ..Request::default()
            },
        )
        .expect("search");
        assert_eq!(
            paths(&flat),
            ["/home/u/zeta.txt", "/home/u/beta.txt", "/home/u/alpha"]
        );
    }

    #[test]
    fn paging_walks_the_whole_result_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, (0..5).map(|i| file(&format!("/home/u/f{i}.txt"))));
        let page = |page| {
            search(
                &handle,
                &Request {
                    order: Order::NameAsc,
                    folders_first: false,
                    ..Request::default()
                }
                .page(page, 2),
            )
            .expect("search")
        };
        let first = page(0);
        assert_eq!(first.total, 5);
        assert_eq!(paths(&first), ["/home/u/f0.txt", "/home/u/f1.txt"]);
        assert!(first.has_more());
        assert_eq!(paths(&page(1)), ["/home/u/f2.txt", "/home/u/f3.txt"]);
        let last = page(2);
        assert_eq!(paths(&last), ["/home/u/f4.txt"]);
        assert!(!last.has_more(), "the last page says so");
        assert!(
            page(3).hits.is_empty(),
            "past the end is empty, not an error"
        );
    }

    #[test]
    fn a_body_match_comes_back_with_marks_on_the_words() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let hits = search(
            &handle,
            &Request {
                text: "filed".to_string(),
                content: true,
                folders_first: false,
                ..Request::default()
            },
        )
        .expect("search");
        assert_eq!(hits.total, 1, "only the body mentions it");
        let hit = &hits.hits[0];
        assert_eq!(hit.path, "/home/u/Documents/summary.txt");
        let snippet = hit.snippet.as_ref().expect("a snippet for a body match");
        assert_eq!(snippet.marks.len(), 1, "{snippet:?}");
        let (start, end) = snippet.marks[0];
        assert!(
            end <= snippet.text.len() && snippet.text.is_char_boundary(start),
            "marks have to be usable as byte ranges: {snippet:?}"
        );
        assert_eq!(&snippet.text[start..end], "filed");
        let name_only = search(
            &handle,
            &Request {
                text: "report".to_string(),
                content: true,
                folders_first: false,
                ..Request::default()
            },
        )
        .expect("search");
        let pdf = name_only
            .hits
            .iter()
            .find(|h| h.path.ends_with("report.pdf"))
            .expect("the pdf");
        assert!(
            pdf.snippet.is_none(),
            "a file with no indexed text has nothing to highlight"
        );
    }

    #[test]
    fn a_hit_carries_the_stored_metadata_without_touching_the_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let mut record = file("/home/u/Documents/report.pdf");
        record.size = 4_242;
        record.modified = 1_600_000_000;
        record.mime = Some("application/pdf".to_string());
        record.labels = vec!["Office".to_string()];
        index(&handle, [record]);
        let hits = search(
            &handle,
            &Request {
                text: "report".to_string(),
                ..Request::default()
            },
        )
        .expect("search");
        let hit = &hits.hits[0];
        assert_eq!(hit.name, "report.pdf");
        assert_eq!(hit.mime.as_deref(), Some("application/pdf"));
        assert_eq!(hit.size, 4_242);
        assert_eq!(hit.modified, 1_600_000_000);
        assert_eq!(hit.labels, ["Office"]);
        assert!(!hit.hidden);
    }

    #[test]
    fn a_relative_scope_is_rejected_rather_than_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, tree());
        let err = search(
            &handle,
            &Request {
                scope: Some("Documents".to_string()),
                ..Request::default()
            },
        )
        .expect_err("a relative scope cannot mean anything");
        assert!(
            err.to_string().contains("Documents"),
            "the error should name the bad scope: {err}"
        );
    }

    #[test]
    fn punctuation_and_case_do_not_split_a_query_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        index(&handle, [file("/home/u/Annual-Report.PDF")]);
        let hits = search(
            &handle,
            &Request {
                text: "annual, report".to_string(),
                ..Request::default()
            },
        )
        .expect("search");
        assert_eq!(hits.total, 1, "both words are in the one name");
    }
}
