//! What the indexer is configured to index, and how deeply.
//!
//! One JSON file next to the preferences, following the distro's convention of a
//! private directory rather than a `GSettings` schema. It is the indexer's own state,
//! deliberately not part of [`ufm_core::Preferences`]: the file manager's view
//! settings change constantly from the UI, while this file changes only when the
//! user says "index this folder's contents too", and rewriting the whole preference
//! file to move one root would risk the UI's state to protect nothing.
//!
//! The shape is a list of roots, each with how much of it to read. Names and
//! metadata are always indexed; body text is per root, because reading file bodies
//! is the one part of indexing whose cost the user can actually feel — it is what
//! turns a crawl of `~/Downloads` from a millisecond into a minute.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ufm_core::{Error, Result, atomic, paths};
use ufm_index::ScanPolicy;

/// The current shape of the file. A file claiming any other version is reported
/// rather than silently rewritten, because the older daemon would otherwise read a
/// newer file and quietly drop a field it has never seen.
pub const VERSION: u32 = 1;

/// How much of a root to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentMode {
    /// Index names and metadata only. The default, and the right answer for a root
    /// holding media, builds or downloads.
    #[default]
    Off,
    /// Read files the MIME table calls text. Cheap: one read, no subprocess.
    Text,
    /// Also run the external filters for PDF and Office documents. Costs a process
    /// per file, so it is opt-in per root rather than a global default.
    Rich,
}

impl ContentMode {
    /// Whether any text is read at all.
    #[must_use]
    pub const fn wants_content(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether the external filters may be run.
    #[must_use]
    pub const fn wants_helpers(self) -> bool {
        matches!(self, Self::Rich)
    }
}

/// One directory to index.
///
/// `Default` is a root with no path, which is what a configuration file with a
/// field missing deserialises to; [`Root::names`] is how a real one is built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Root {
    /// Absolute path. A relative root would mean different things to the daemon and
    /// to the file manager, which do not share a working directory.
    pub path: PathBuf,
    pub content: ContentMode,
    /// A name to show in the settings page. Absent means the path itself.
    pub label: Option<String>,
}

impl Root {
    /// A root indexed by name only.
    #[must_use]
    pub fn names(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            content: ContentMode::Off,
            label: None,
        }
    }

    /// The label, falling back to the path.
    #[must_use]
    pub fn display(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

/// The indexer's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexConfig {
    pub version: u32,
    /// What to index, in the order a crawl visits them.
    pub roots: Vec<Root>,
    /// How deep to go. A limit, not a preference: an extracted archive can nest a
    /// hundred levels and must not decide how long a crawl takes.
    pub max_depth: usize,
    /// Index dotfiles too, marked hidden. Leaving them out would make `.bashrc`
    /// unfindable, which is not what hidden means.
    pub include_hidden: bool,
    /// Do not cross into another filesystem. A network share mounted under
    /// `~/mnt` was mounted deliberately and is indexed as its own root or not at
    /// all; crawling it as part of home would make the crawl's cost depend on what
    /// happens to be mounted.
    pub stay_on_filesystem: bool,
    /// Directory names never descended into.
    pub prune: Vec<String>,
    /// Largest file whose text is read. Beyond this a file is still indexed by
    /// name; reading it costs more than it can add.
    pub max_content_size: u64,
    /// How long the daemon stays resident after its last client. The bus unit owns
    /// the real timeout; this is what the daemon tells the settings page.
    pub idle_exit_secs: u64,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            version: VERSION,
            roots: default_roots(),
            max_depth: 12,
            include_hidden: true,
            stay_on_filesystem: true,
            prune: ufm_index::scan::PRUNED_DIRS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            max_content_size: 2 * 1024 * 1024,
            idle_exit_secs: 120,
        }
    }
}

/// Where the file lives.
#[must_use]
pub fn path() -> PathBuf {
    paths::config_dir().join("index.json")
}

/// What to index before the user has said anything: the home directory, by name.
///
/// Home alone rather than a list of XDG user directories, because `~/Documents`
/// may not exist, may be a symlink to elsewhere, or may be the whole of a small
/// disk — and a crawl of home with the caches pruned is bounded well enough by the
/// prune list. Content is off until the user asks for it, which is the difference
/// between an indexer that finishes at login and one that is still running at
/// teatime.
#[must_use]
pub fn default_roots() -> Vec<Root> {
    vec![Root::names(paths::home())]
}

impl IndexConfig {
    /// Read the file. A missing file is the default configuration; a file that
    /// exists and does not parse is an error, because silently resetting would
    /// throw away the user's choice of what to read.
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = atomic::read_json(path)?;
        if config.version != VERSION {
            return Err(Error::Config(format!(
                "{}: unsupported indexer configuration version {}",
                path.display(),
                config.version
            )));
        }
        Ok(config)
    }

    /// Read the file, falling back to the defaults when it is missing *or* broken.
    ///
    /// The daemon uses this one: refusing to start because a configuration file got
    /// truncated would leave the user with no search at all, and the defaults are a
    /// defensible place to land. The caller is expected to report the fallback.
    pub fn load_or_default(path: &Path) -> (Self, Option<String>) {
        match Self::load(path) {
            Ok(config) => (config, None),
            Err(e) => (Self::default(), Some(e.to_string())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic::write_json(path, self)
    }

    /// The crawl policy for one root.
    #[must_use]
    pub fn scan_policy(&self, content: ContentMode) -> ScanPolicy {
        ScanPolicy {
            max_depth: self.max_depth,
            include_hidden: self.include_hidden,
            stay_on_filesystem: self.stay_on_filesystem,
            prune: self.prune.clone(),
            content_max_size: if content.wants_content() {
                self.max_content_size
            } else {
                0
            },
        }
    }

    /// Absolute, deduplicated, no root inside another root.
    ///
    /// Nesting matters because a crawl writes one record per path: indexing
    /// `/home/u` and `/home/u/Documents` would produce two records for every
    /// document, and every search would show it twice. The outer root wins, since
    /// it was asked for first.
    pub fn normalize(&mut self) {
        self.roots.retain(|root| {
            root.path.is_absolute() && root.path.file_name().is_some_and(|name| !name.is_empty())
        });
        let mut kept: Vec<Root> = Vec::with_capacity(self.roots.len());
        for root in std::mem::take(&mut self.roots) {
            // `starts_with` is true of a path against itself, so an exact duplicate
            // is dropped by the same rule that drops a nested one — and the copy
            // that survives is the one asked for first, which is the one whose
            // content mode the user chose deliberately.
            let nested = kept.iter().any(|outer| root.path.starts_with(&outer.path));
            if !nested {
                kept.push(root);
            }
        }
        self.roots = kept;
    }

    /// The roots as plain paths, for a `Status` reply.
    #[must_use]
    pub fn root_paths(&self) -> Vec<String> {
        self.roots
            .iter()
            .map(|root| root.path.display().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str, content: ContentMode) -> Root {
        Root {
            path: PathBuf::from(path),
            content,
            label: None,
        }
    }

    /// A configuration file is written by us and read back, so the round trip is
    /// the contract with the next version.
    #[test]
    fn a_configuration_round_trips_through_its_own_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("index.json");
        let config = IndexConfig {
            roots: vec![
                Root::names("/home/u"),
                root("/home/u/Documents", ContentMode::Text),
                root("/srv/books", ContentMode::Rich),
            ],
            ..Default::default()
        };
        config
            .save(&path)
            .expect("a writable place to put the configuration");
        assert_eq!(
            IndexConfig::load(&path).expect("the configuration we just wrote"),
            config
        );
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_a_broken_file_is_an_error() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("index.json");
        assert_eq!(
            IndexConfig::load(&path).expect("a missing file is not a failure"),
            IndexConfig::default()
        );
        std::fs::write(&path, "{ not json").expect("a writable fixture");
        assert!(
            IndexConfig::load(&path).is_err(),
            "a truncated file must not silently reset the user's choice"
        );
        let (config, note) = IndexConfig::load_or_default(&path);
        assert_eq!(config, IndexConfig::default());
        assert!(
            note.is_some(),
            "and the daemon is told that it fell back, rather than guessing"
        );
    }

    #[test]
    fn a_file_from_the_future_is_reported() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("index.json");
        std::fs::write(&path, r#"{"version":99,"roots":[]}"#).expect("a writable fixture");
        let err = IndexConfig::load(&path).expect_err("version 99 is not this version");
        assert!(
            err.to_string().contains("version 99"),
            "the message has to say what is wrong: {err}"
        );
    }

    #[test]
    fn the_default_is_home_by_name_only() {
        let config = IndexConfig::default();
        assert_eq!(config.roots.len(), 1);
        assert_eq!(config.roots[0].path, paths::home());
        assert_eq!(
            config.roots[0].content,
            ContentMode::Off,
            "reading file bodies is the part of indexing whose cost the user feels, \
             so it is never on by default"
        );
        assert!(
            config.prune.iter().any(|p| p == ".cache"),
            "the caches are the reason a default crawl is bounded"
        );
    }

    #[test]
    fn a_root_inside_another_root_is_dropped() {
        let mut config = IndexConfig {
            roots: vec![
                Root::names("/home/u"),
                root("/home/u/Documents", ContentMode::Text),
                Root::names("/home/u/other"),
                Root::names("relative"),
                Root::names("/elsewhere"),
            ],
            ..Default::default()
        };
        config.normalize();
        let kept: Vec<String> = config
            .roots
            .iter()
            .map(|root| root.path.display().to_string())
            .collect();
        assert_eq!(
            kept,
            vec!["/home/u", "/elsewhere"],
            "the nested root would index the same files twice, and a relative path \
             means different things to the daemon and to the file manager"
        );
    }

    #[test]
    fn the_same_directory_twice_keeps_the_first_answer() {
        let mut config = IndexConfig {
            roots: vec![root("/home/u", ContentMode::Text), Root::names("/home/u")],
            ..Default::default()
        };
        config.normalize();
        assert_eq!(config.roots.len(), 1);
        assert_eq!(
            config.roots[0].content,
            ContentMode::Text,
            "asked for twice, the richer answer is the one the user wanted"
        );
    }

    #[test]
    fn a_root_without_content_gets_a_policy_that_reads_nothing() {
        let config = IndexConfig {
            max_content_size: 4096,
            ..Default::default()
        };
        assert_eq!(
            config.scan_policy(ContentMode::Off).content_max_size,
            0,
            "a policy that would still read a megabyte would make the mode a lie"
        );
        assert_eq!(
            config.scan_policy(ContentMode::Rich).content_max_size,
            4096,
            "otherwise the configured cap applies"
        );
        let policy = config.scan_policy(ContentMode::Text);
        assert!(policy.include_hidden && policy.stay_on_filesystem);
        assert_eq!(policy.max_depth, config.max_depth);
    }

    #[test]
    fn only_the_rich_mode_may_start_a_helper() {
        assert!(!ContentMode::Off.wants_content());
        assert!(ContentMode::Text.wants_content());
        assert!(ContentMode::Rich.wants_content());
        assert!(
            !ContentMode::Text.wants_helpers(),
            "a root that opted into reading text files did not opt into a subprocess \
             per document"
        );
        assert!(ContentMode::Rich.wants_helpers());
    }

    #[test]
    fn an_unknown_key_in_the_configuration_is_refused() {
        let err: std::result::Result<IndexConfig, _> =
            serde_json::from_str(r#"{"version":1,"rootz":[]}"#);
        assert!(
            err.is_err(),
            "a typo would otherwise mean indexing nothing and saying nothing"
        );
    }

    #[test]
    fn a_root_displays_its_label_or_its_path() {
        let mut root = Root::names("/srv/books");
        assert_eq!(root.display(), "/srv/books");
        root.label = Some("Books".to_string());
        assert_eq!(root.display(), "Books");
    }
}
