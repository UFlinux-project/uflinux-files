//! Turning a file into text, and deciding when it is worth the trouble.
//!
//! [`ufm_index::PlainText`] covers the cheap half: read a `text/*` file and index
//! it. This module adds the expensive half — a PDF or an Office document has to be
//! run through an external program — and the budget that makes the whole thing
//! safe to leave switched on.
//!
//! Everything here shells out rather than linking a PDF library. That is a memory
//! decision first: `poppler` or `pdfium` would put tens of megabytes of resident
//! data in a daemon whose idle budget is 24 MiB, and would pay it whether or not a
//! single document is being read. A subprocess costs its own address space for the
//! length of one file and then leaves. The cost of that choice is that the filter
//! may be absent, which is why a missing program is a normal outcome and not an
//! error: the file is still indexed by name.
//!
//! The second reason is robustness. A malformed PDF should cost one file, not the
//! daemon, and a filter that crashes takes its own address space with it instead of
//! taking down the index mid-commit.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use ufm_index::Extractor;

/// How much text to take from one file, and how hard to look for it.
#[derive(Debug, Clone, Copy)]
pub struct Limit {
    /// Bytes of extracted text kept. Beyond this the text is cut, not rejected: a
    /// book that yields two megabytes of prose still has its first two megabytes
    /// indexed, and a search that matches later in the book is a worse outcome than
    /// a crawl that stops reading.
    pub max_output: usize,
    /// Last page `pdftotext` is asked to read. This is the bound on a filter's work,
    /// in place of a timeout: a timeout would need a thread to wait on, and a page
    /// limit bounds the cost where it is actually spent.
    pub max_pdf_pages: u32,
    /// Bytes read from a filter's stdout. A filter that ignores its input and
    /// streams forever must not be able to fill memory before the truncation applies.
    pub max_pipe: usize,
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            max_output: 2 * 1024 * 1024,
            max_pdf_pages: 200,
            max_pipe: 8 * 1024 * 1024,
        }
    }
}

impl Limit {
    /// Cut extracted text to the budget, on a character boundary.
    ///
    /// Splitting mid-UTF-8 would panic or force the lossy path; backing up to the
    /// start of the next character is two instructions and keeps the text valid.
    #[must_use]
    pub fn truncate(self, mut text: String) -> String {
        if text.len() <= self.max_output {
            return text;
        }
        let mut end = self.max_output;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text
    }
}

/// One external filter: a program, the MIME types it can read, and how to call it.
///
/// The program is a fixed name, never built from the file's path, and the path is
/// the only argument that comes from outside. It is passed as an absolute path — or
/// as `./relative` — so it can never begin with `-` and be read as an option: a file
/// called `--encipher=secret.pdf` is a file, not a flag. That is stronger than a `--`
/// separator, which neither `pdftotext` nor `unzip` documents as an end-of-options
/// marker and one of them would reject as an unknown option.
#[derive(Debug)]
pub struct CommandExtractor {
    program: &'static str,
    /// Arguments placed before the file.
    args: Vec<String>,
    /// Arguments placed after it: `-` for `pdftotext`'s output file, the one member
    /// to read for `unzip`. Both programs take the input first.
    after: Vec<String>,
    /// The MIME types this filter is asked about.
    mimes: &'static [&'static str],
    limit: Limit,
    /// Set once the program turns out not to be installed, so a crawl of a thousand
    /// PDFs does not attempt a thousand failed spawns.
    missing: std::sync::atomic::AtomicBool,
}

impl CommandExtractor {
    #[must_use]
    pub fn new(
        program: &'static str,
        args: &'static [&'static str],
        mimes: &'static [&'static str],
        limit: Limit,
    ) -> Self {
        Self::build(
            program,
            args.iter().map(|arg| (*arg).to_string()).collect(),
            Vec::new(),
            mimes,
            limit,
        )
    }

    fn build(
        program: &'static str,
        args: Vec<String>,
        after: Vec<String>,
        mimes: &'static [&'static str],
        limit: Limit,
    ) -> Self {
        Self {
            program,
            args,
            after,
            mimes,
            limit,
            missing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether this filter is asked about `mime`.
    #[must_use]
    pub fn handles(&self, mime: Option<&str>) -> bool {
        mime.is_some_and(|m| self.mimes.contains(&m))
    }

    /// `pdftotext`, from Alpine's `poppler-utils`. Reads the first pages of a PDF
    /// and writes them to stdout, which is what the trailing `-` asks for: without
    /// an output-file argument `pdftotext` derives one from the input's name and
    /// writes it into the daemon's working directory.
    #[must_use]
    pub fn pdf(limit: Limit) -> Self {
        let mut args = vec![
            "-q".to_string(),
            "-enc".to_string(),
            "UTF-8".to_string(),
            "-l".to_string(),
            // The page limit is the bound on a filter's work, in place of a timeout:
            // a timeout would need a thread to wait on, and this bounds the cost
            // where it is actually spent.
            limit.max_pdf_pages.to_string(),
        ];
        args.push("-".to_string());
        let after = args.split_off(5);
        Self::build("pdftotext", args, after, &["application/pdf"], limit)
    }

    /// `unzip -p`, for the XML inside an Office document. A `.docx` is a zip of
    /// XML, and the word processor's markup between the tags is close enough to
    /// prose to search — with the caveat that a document's formatting is indexed
    /// alongside its text, which is why this is a per-root opt-in.
    ///
    /// The member is named so a document with a hundred embedded images does not
    /// have them streamed through a pipe. An `OpenDocument` archive has no
    /// `word/document.xml`, so it yields no text here and is indexed by name; naming
    /// `content.xml` too would make one of the two a guess.
    #[must_use]
    pub fn office(limit: Limit) -> Self {
        Self::build(
            "unzip",
            vec!["-p".to_string(), "-qq".to_string()],
            vec!["word/document.xml".to_string()],
            &[
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "application/vnd.oasis.opendocument.text",
            ],
            limit,
        )
    }

    fn mark_missing(&self) {
        self.missing
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Extractor for CommandExtractor {
    fn extract(&self, path: &Path, mime: Option<&str>) -> Option<String> {
        if !self.handles(mime) || self.missing.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let text = match self.run(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.mark_missing();
                return None;
            }
            // A filter that failed, timed out or produced nothing. Most files in a
            // home directory hold no extractable text, so this is the common case
            // and not worth reporting anywhere.
            Err(_) => return None,
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();
        if text.trim().is_empty() {
            return None;
        }
        Some(self.limit.truncate(text))
    }
}

impl CommandExtractor {
    /// The program's arguments for one file: the fixed prefix, the path, the fixed
    /// suffix. Testable on its own, because the shape `pdftotext` expects is the
    /// thing that silently breaks when its options are reordered.
    fn argv(&self, path: &Path) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + self.after.len() + 1);
        argv.extend(self.args.iter().cloned());
        argv.push(option_safe(path).display().to_string());
        argv.extend(self.after.iter().cloned());
        argv
    }

    fn run(&self, path: &Path) -> std::io::Result<String> {
        let mut command = Command::new(self.program);
        command.args(self.argv(path));
        command.stdin(Stdio::null());
        command.stderr(Stdio::null());
        command.stdout(Stdio::piped());
        let mut child = command.spawn()?;
        let output = read_bounded(child.stdout.take(), self.limit.max_pipe)?;
        // Reap the child; its exit status is deliberately ignored. `pdftotext`
        // reports a warning about a damaged file by exiting non-zero after writing
        // the text it did manage to extract, and that text is worth indexing.
        let _ = child.wait();
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}

/// A path that cannot be mistaken for an option: absolute, or relative with a
/// leading `./`, which names the same file and cannot start with a dash.
fn option_safe(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::path::Path::new(".").join(path)
}

/// Read a pipe up to `max` bytes, so a filter that will not stop cannot decide how
/// much memory the daemon is using.
fn read_bounded(pipe: Option<std::process::ChildStdout>, max: usize) -> std::io::Result<Vec<u8>> {
    let Some(pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut buffer = Vec::with_capacity(8 * 1024);
    // One byte more than the budget, so "we stopped because we were full" is
    // distinguishable from "that was all of it" without a second stat.
    let mut reader = pipe.take((max + 1) as u64);
    reader.read_to_end(&mut buffer)?;
    buffer.truncate(max);
    Ok(buffer)
}

/// The extractors for one root, tried in order.
///
/// First answer wins. The order is cheap-to-expensive, so a `.txt` never causes a
/// subprocess, and a file no filter claims yields `None` — which the scanner treats
/// as "indexed by name only", the same outcome as a filter that is not installed.
#[derive(Default)]
pub struct Chain {
    extractors: Vec<Box<dyn Extractor>>,
    limit: Limit,
}

/// The filters themselves are opaque — a `dyn` trait object with nothing worth
/// printing — so the count and the budget are what identifies one.
impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("extractors", &self.extractors.len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl Chain {
    /// Text files only: no subprocess, nothing to install.
    #[must_use]
    pub fn text(limit: Limit) -> Self {
        Self {
            extractors: vec![Box::new(ufm_index::PlainText)],
            limit,
        }
    }

    /// Text files plus the external filters for PDF and Office documents.
    #[must_use]
    pub fn rich(limit: Limit) -> Self {
        Self {
            extractors: vec![
                Box::new(ufm_index::PlainText),
                Box::new(CommandExtractor::pdf(limit)),
                Box::new(CommandExtractor::office(limit)),
            ],
            limit,
        }
    }

    /// Nothing to extract, which is what a root configured `off` gets. Passing this
    /// rather than `None` keeps one code path in the crawler.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Add a filter, for a document type the distro gains a tool for later.
    pub fn push(&mut self, extractor: Box<dyn Extractor>) {
        self.extractors.push(extractor);
    }

    #[must_use]
    pub const fn limit(&self) -> Limit {
        self.limit
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extractors.is_empty()
    }
}

impl Extractor for Chain {
    fn extract(&self, path: &Path, mime: Option<&str>) -> Option<String> {
        self.extractors
            .iter()
            .find_map(|extractor| extractor.extract(path, mime))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("a writable fixture directory");
        path
    }

    #[test]
    fn text_extraction_is_unchanged_for_a_plain_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(dir.path(), "notes.txt", b"the quick brown fox\n");
        let chain = Chain::text(Limit::default());
        let text = chain
            .extract(&path, Some("text/plain"))
            .expect("a text file has text");
        assert!(text.contains("quick"));
    }

    #[test]
    fn a_file_no_filter_claims_has_no_text() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(dir.path(), "photo.jpg", &[0xff, 0xd8, 0xff, 0xe0]);
        let chain = Chain::rich(Limit::default());
        assert_eq!(chain.extract(&path, Some("image/jpeg")), None);
        assert_eq!(
            Chain::none().extract(&path, Some("text/plain")),
            None,
            "a root configured not to read bodies asks for no extraction at all"
        );
    }

    #[test]
    fn a_missing_filter_is_remembered_and_is_not_an_error() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(dir.path(), "scan.pdf", b"%PDF-1.4\n");
        let filter = CommandExtractor::new(
            "uflinux-a-program-that-is-not-installed",
            &["-q"],
            &["application/pdf"],
            Limit::default(),
        );
        assert_eq!(
            filter.extract(&path, Some("application/pdf")),
            None,
            "a distro without poppler-utils still indexes PDFs by name"
        );
        assert!(
            filter.missing.load(std::sync::atomic::Ordering::Relaxed),
            "the second PDF in the same folder must not try the spawn again"
        );
    }

    #[test]
    fn a_filter_only_answers_for_the_types_it_claims() {
        let pdf = CommandExtractor::pdf(Limit::default());
        assert!(pdf.handles(Some("application/pdf")));
        assert!(
            !pdf.handles(Some("text/plain")),
            "a text file must never reach a subprocess"
        );
        assert!(!pdf.handles(None));
    }

    #[test]
    fn extracted_text_is_cut_on_a_character_boundary() {
        let limit = Limit {
            max_output: 7,
            ..Default::default()
        };
        // Four two-byte characters: cutting at 7 would land inside the fourth one,
        // so the cut backs up to 6 and the third character survives.
        let text = limit.truncate("éééé".to_string());
        assert_eq!(
            text, "ééé",
            "cut back to a boundary rather than through a char"
        );
        assert_eq!(
            limit.truncate("short".to_string()),
            "short",
            "under the budget, nothing happens"
        );
    }

    #[test]
    fn a_filter_that_writes_too_much_is_still_bounded() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        // `cat` stands in for a filter that produces far more than we asked for.
        let path = file(dir.path(), "in.txt", &vec![b'x'; 400_000]);
        let filter = CommandExtractor::new(
            "cat",
            &[],
            &["application/pdf"],
            Limit {
                max_pipe: 1024,
                max_output: 512,
                ..Default::default()
            },
        );
        let text = filter
            .extract(&path, Some("application/pdf"))
            .expect("cat writes the file's bytes to stdout");
        assert_eq!(
            text.len(),
            512,
            "the pipe budget stops the read and the output budget is what survives"
        );
    }

    #[test]
    fn a_byte_order_mark_is_not_indexed_as_part_of_the_first_word() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(dir.path(), "bom.txt", "\u{feff}hello\n".as_bytes());
        let filter = CommandExtractor::new("cat", &[], &["application/pdf"], Limit::default());
        let text = filter
            .extract(&path, Some("application/pdf"))
            .expect("cat writes the file");
        assert!(
            text.starts_with("hello"),
            "the mark would otherwise glue itself to the first token: {text:?}"
        );
    }

    #[test]
    fn a_file_name_that_looks_like_an_option_is_still_a_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(dir.path(), "-n.txt", b"content\n");
        let filter = CommandExtractor::new("cat", &[], &["application/pdf"], Limit::default());
        let text = filter
            .extract(&path, Some("application/pdf"))
            .expect("the path is absolute, so it cannot read as a flag");
        assert_eq!(text, "content\n", "otherwise cat would print its own usage");
        let relative = Path::new("-n.txt");
        assert_eq!(
            filter.argv(relative),
            vec!["./-n.txt".to_string()],
            "a relative path gains a leading ./ for the same reason"
        );
    }

    /// The shape `pdftotext` actually documents: options, the last page, the input,
    /// then the output file. Getting this wrong is not an error but a stray
    /// `file.txt` written next to the daemon's working directory.
    #[test]
    fn the_pdf_filter_asks_for_stdout_and_bounds_its_pages() {
        let filter = CommandExtractor::pdf(Limit {
            max_pdf_pages: 42,
            ..Default::default()
        });
        assert_eq!(
            filter.argv(Path::new("/srv/a.pdf")),
            vec!["-q", "-enc", "UTF-8", "-l", "42", "/srv/a.pdf", "-"],
            "the page count belongs to -l and the trailing - is the output file"
        );
    }

    #[test]
    fn the_office_filter_names_the_one_member_it_reads() {
        let filter = CommandExtractor::office(Limit::default());
        assert_eq!(
            filter.argv(Path::new("/srv/a.docx")),
            vec!["-p", "-qq", "/srv/a.docx", "word/document.xml"],
            "the archive comes first, since that is how unzip takes it"
        );
    }

    #[test]
    fn a_chain_answers_with_the_first_filter_that_has_text() {
        struct Counter(Rc<std::cell::Cell<usize>>);
        impl Extractor for Counter {
            fn extract(&self, _path: &Path, _mime: Option<&str>) -> Option<String> {
                self.0.set(self.0.get() + 1);
                Some("never used".to_string())
            }
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(dir.path(), "notes.txt", b"hello\n");
        let mut chain = Chain::text(Limit::default());
        let calls = Rc::new(std::cell::Cell::new(0usize));
        chain.push(Box::new(Counter(Rc::clone(&calls))));
        assert_eq!(
            chain.extract(&path, Some("text/plain")).as_deref(),
            Some("hello\n"),
            "the cheap filter runs first and the expensive one is never reached"
        );
        assert_eq!(calls.get(), 0);
    }
}
