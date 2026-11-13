//! The schema shared by the file index and the application index.
//!
//! Two decisions in here shape everything else.
//!
//! Paths are indexed as a facet. A facet term stores every ancestor of a path,
//! so "everything under `~/Documents`" is one `TermQuery` rather than a range
//! scan, and the same term doubles as the folder scope the search bar applies.
//! The cost is that a facet component may not contain `/` or a NUL byte, which
//! is checked in [`crate::doc`] rather than here, because a name containing one
//! of those is a perfectly valid file and the caller has to decide what to do.
//!
//! Names are indexed twice: once as words, which is what makes `report pdf` find
//! `annual-report.pdf`, and once as leading n-grams, which is what makes `rep`
//! find it while the user is still typing. A single field cannot carry both
//! tokenizers, so the prefix field exists on its own and is only ever queried by
//! prefix.

use tantivy::schema::{
    DateOptions, FacetOptions, Field, IndexRecordOption, NumericOptions, Schema, SchemaBuilder,
    TextFieldIndexing, TextOptions, Type,
};
use tantivy::tokenizer::{
    LowerCaser, NgramTokenizer, RAW_TOKENIZER_NAME, RemoveLongFilter, SimpleTokenizer,
    TextAnalyzer, TokenizerManager,
};

use ufm_core::{Error, Result};

/// Analyzer used for file and application names.
pub const NAME_ANALYZER: &str = "ufm_name";

/// Analyzer used for extracted file content.
pub const CONTENT_ANALYZER: &str = "ufm_content";

/// Analyzer used for the leading n-gram field.
pub const PREFIX_ANALYZER: &str = "ufm_prefix";

/// Smallest n-gram indexed for typeahead. Two is the point where a Latin prefix
/// stops matching half the vocabulary; single characters would make the term
/// dictionary larger than the postings.
pub const PREFIX_MIN: usize = 2;

/// Longest n-gram indexed. Going further mostly duplicates the word field, and
/// every extra gram is another posting per file name. The query layer needs the
/// same window: outside it there is no term to look up.
pub const PREFIX_MAX: usize = 5;

/// Field names, so a query can be built against a name without holding the
/// struct.
pub mod field {
    pub const PATH: &str = "path";
    pub const NAME: &str = "name";
    pub const NAME_PREFIX: &str = "name_prefix";
    pub const PARENT: &str = "parent";
    pub const KIND: &str = "kind";
    pub const MIME: &str = "mime";
    pub const SIZE: &str = "size";
    pub const MODIFIED: &str = "modified";
    pub const HIDDEN: &str = "hidden";
    pub const CONTENT: &str = "content";
    pub const LABELS: &str = "labels";
}

/// Every field of an index, resolved from a [`Schema`] so the order in which
/// they were declared does not matter.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub path: Field,
    pub name: Field,
    pub name_prefix: Field,
    pub parent: Field,
    pub kind: Field,
    pub mime: Field,
    pub size: Field,
    pub modified: Field,
    pub hidden: Field,
    /// Only present in an index that stores extracted text.
    pub content: Option<Field>,
    pub labels: Field,
}

/// Build the schema. `content` adds the stored body-text field used by full-text
/// search - extracted file text, or an application's `GenericName` and `Comment`.
///
/// The flag is not cosmetic. Without the field, a record's body text is dropped on
/// the way in and the query layer stops offering body-text matches at all, so an
/// index built with `false` is a smaller index that answers a strictly smaller set
/// of questions - which is a reasonable thing to want on a machine with little
/// memory, and the reason it is a parameter rather than always on.
#[must_use]
pub fn schema(content: bool) -> Schema {
    let mut b = SchemaBuilder::new();

    // The whole path, untokenised, stored so a hit can be opened without a
    // second read. Indexed so deleting a record is a term query.
    b.add_text_field(field::PATH, path_options());
    b.add_text_field(field::NAME, name_options());
    b.add_text_field(field::NAME_PREFIX, prefix_options());
    b.add_facet_field(field::PARENT, FacetOptions::default());
    // Stored as well as indexed: a result row has to know whether it is a folder to
    // pick an icon and decide whether double-clicking navigates or opens, and
    // re-reading the metadata of every hit to learn that is a syscall per row.
    b.add_facet_field(field::KIND, FacetOptions::default().set_stored());
    // Untokenised, indexed and stored. A type filter has to be a query rather
    // than a filter applied to a page, or the total shown would be the total
    // before filtering; and the vocabulary is a few hundred MIME types, not one
    // term per path.
    b.add_text_field(field::MIME, mime_options());
    b.add_i64_field(field::SIZE, size_options());
    b.add_date_field(field::MODIFIED, modified_options());
    b.add_bool_field(field::HIDDEN, hidden_options());
    // `content` is the body text to search: extracted file text for the file
    // index, and `GenericName` plus `Comment` for the application index. The
    // difference is scale - a few hundred short strings against potentially
    // millions of documents - which is why the application index still skips the
    // snippet machinery rather than the field itself.
    if content {
        b.add_text_field(field::CONTENT, content_options());
    }
    // One value per label, untokenised. A category filter is then a term query
    // that matches any record carrying that label, and the values read back are
    // already split - no payload encoding of our own. Not fast: a text fast field
    // carries a second term dictionary, and nothing sorts by label.
    b.add_text_field(field::LABELS, labels_options());

    b.build()
}

/// The tokenizers the schema refers to. The same manager has to be handed to
/// [`tantivy::IndexBuilder::tokenizers`] when opening, or an existing index
/// cannot read its own fields back.
#[must_use]
pub fn tokenizers() -> TokenizerManager {
    let manager = TokenizerManager::default();
    manager.register(
        NAME_ANALYZER,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(64))
            .filter(LowerCaser)
            .build(),
    );
    manager.register(
        CONTENT_ANALYZER,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(64))
            .filter(LowerCaser)
            .build(),
    );
    // `expect` is safe by construction: 0 < min <= max.
    let ngram = NgramTokenizer::prefix_only(PREFIX_MIN, PREFIX_MAX)
        .expect("a 2..=5 prefix n-gram tokenizer is always a valid argument");
    manager.register(PREFIX_ANALYZER, ngram);
    manager
}

impl Fields {
    /// Resolve every field of `schema`, checking that it is the shape this crate
    /// expects. A mismatch here is what an index written by an older version
    /// looks like, and the caller responds by rebuilding from scratch.
    pub fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            path: typed(schema, field::PATH, Type::Str)?,
            name: typed(schema, field::NAME, Type::Str)?,
            name_prefix: typed(schema, field::NAME_PREFIX, Type::Str)?,
            parent: typed(schema, field::PARENT, Type::Facet)?,
            kind: typed(schema, field::KIND, Type::Facet)?,
            mime: typed(schema, field::MIME, Type::Str)?,
            size: typed(schema, field::SIZE, Type::I64)?,
            modified: typed(schema, field::MODIFIED, Type::Date)?,
            hidden: typed(schema, field::HIDDEN, Type::Bool)?,
            content: optional(schema, field::CONTENT, Type::Str)?,
            labels: typed(schema, field::LABELS, Type::Str)?,
        })
    }

    /// The content field, or an error explaining that full-text search is not
    /// available. Kept separate from opening an index so a caller that asked for
    /// content search learns that the field is missing, rather than concluding
    /// that no file on disk happens to contain any of the words.
    pub fn require_content(&self) -> Result<Field> {
        self.content
            .ok_or_else(|| Error::Search("this index does not store file content".to_string()))
    }
}

fn typed(schema: &Schema, name: &str, want: Type) -> Result<Field> {
    let field = schema
        .get_field(name)
        .map_err(|_| Error::Search(format!("the index has no `{name}` field")))?;
    let entry = schema.get_field_entry(field);
    if entry.field_type().value_type() == want {
        Ok(field)
    } else {
        Err(Error::Search(format!(
            "the `{name}` field in this index has the wrong type"
        )))
    }
}

fn optional(schema: &Schema, name: &str, want: Type) -> Result<Option<Field>> {
    match schema.get_field(name) {
        Ok(_) => typed(schema, name, want).map(Some),
        Err(_) => Ok(None),
    }
}

/// `path`: untokenised, stored, one term per value.
fn path_options() -> TextOptions {
    raw_stored()
}

/// Text with no tokenisation: one term per distinct value, stored, and no
/// fieldnorms, since these fields are filtered, never ranked on their own length.
///
/// Deliberately not fast. A text fast field stores its own term dictionary on top
/// of the inverted index's, which for a field with one long unique value per
/// document - a path - would nearly double the index, and no ordering offered by
/// the file manager needs it.
fn raw_stored() -> TextOptions {
    TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(RAW_TOKENIZER_NAME)
                .set_fieldnorms(false),
        )
        .set_stored()
}

/// `name`: tokenised on word boundaries, with positions, so a phrase query can
/// be used on a quoted name.
///
/// Fast as well, which is what lets a result set be sorted by name without
/// reading the doc store. The fast field uses the raw tokenizer deliberately:
/// sorting wants one value per document, the whole name, not one value per word.
/// It also has to be raw, because a text fast field with any other tokenizer is
/// resolved against the *fast field* tokenizer manager, which
/// [`tantivy::IndexBuilder::open_or_create`] does not restore from the builder -
/// writing a segment would then fail with "tokenizer not found".
fn name_options() -> TextOptions {
    TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(NAME_ANALYZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions)
                .set_fieldnorms(true),
        )
        .set_fast(RAW_TOKENIZER_NAME)
        .set_stored()
}

/// `mime`: one term per value, so a type filter is exact.
fn mime_options() -> TextOptions {
    raw_stored()
}

/// `labels`: one value per label. See the note where the field is declared.
fn labels_options() -> TextOptions {
    raw_stored()
}

/// `name_prefix`: leading n-grams only, no positions, not stored. Every gram is
/// emitted at position 0, so this field is only ever used for prefix matching.
fn prefix_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(PREFIX_ANALYZER)
            .set_fieldnorms(false),
    )
}

/// `content`: indexed with positions for phrase search, stored so a highlighted
/// fragment can be produced without re-reading the file.
///
/// The positions are not the default: `TextFieldIndexing::default()` records
/// `Basic`, and a phrase query against a field without positions is a schema
/// error at query time rather than a miss. They cost the most here of any field -
/// one entry per word of body text - which is the main reason content search is
/// opt-in per root.
fn content_options() -> TextOptions {
    TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CONTENT_ANALYZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions)
                .set_fieldnorms(true),
        )
        .set_stored()
}

/// `size`: fast for sorting, not indexed. A size range query is not something
/// the file manager offers, and an indexed numeric field costs a term dictionary.
fn size_options() -> NumericOptions {
    NumericOptions::default().set_fast().set_stored()
}

/// `modified`: fast for sorting, stored for the list view, and indexed so a
/// "modified today" filter can be a range query.
fn modified_options() -> DateOptions {
    DateOptions::default().set_fast().set_stored().set_indexed()
}

/// `hidden`: fast, stored and indexed. Indexed because "hide dotfiles" is a
/// `MustNot` term query, and a `TermQuery` only falls back to a fast-field range
/// scan when scoring is switched off - under a ranked collector it would fail
/// outright. Otherwise a one-valued indexed field is a term dictionary for two
/// terms, which is as cheap as an index can get.
fn hidden_options() -> NumericOptions {
    NumericOptions::default()
        .set_fast()
        .set_stored()
        .set_indexed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_round_trips_through_its_own_fields() {
        let schema = schema(true);
        let fields = Fields::from_schema(&schema).expect("a freshly built schema is valid");
        assert!(fields.content.is_some());
        assert_eq!(schema.get_field_name(fields.path), field::PATH);
    }

    #[test]
    fn an_index_without_body_text_reports_no_content_field() {
        let fields = Fields::from_schema(&schema(false)).expect("a valid schema");
        assert!(fields.content.is_none());
        assert!(fields.require_content().is_err());
        // Everything a name search needs is still there.
        assert!(fields.name_prefix != fields.name);
    }

    #[test]
    fn a_schema_missing_a_field_is_rejected() {
        let mut builder = SchemaBuilder::new();
        builder.add_text_field(field::PATH, TextOptions::default());
        let err = Fields::from_schema(&builder.build()).expect_err("incomplete schema");
        assert!(
            err.to_string().contains(field::NAME),
            "the error should name the missing field: {err}"
        );
    }

    #[test]
    fn a_field_of_the_wrong_type_is_rejected() {
        let mut builder = SchemaBuilder::new();
        builder.add_text_field(field::PATH, TextOptions::default());
        builder.add_text_field(field::NAME, TextOptions::default());
        builder.add_text_field(field::NAME_PREFIX, TextOptions::default());
        builder.add_text_field(field::PARENT, TextOptions::default());
        let err = Fields::from_schema(&builder.build()).expect_err("parent is not a facet");
        assert!(
            err.to_string().contains(field::PARENT),
            "the error should name the bad field: {err}"
        );
    }

    #[test]
    fn the_two_schemas_are_not_interchangeable() {
        // Opening a file index with the application schema has to fail loudly,
        // because the missing content field would otherwise look like "no file
        // has any text in it".
        let with_content = schema(true);
        let without = schema(false);
        assert_ne!(with_content, without);
    }
}
