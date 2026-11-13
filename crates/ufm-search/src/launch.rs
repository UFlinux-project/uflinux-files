//! Expanding a desktop entry's `Exec` into something that can be started.
//!
//! The index stores `Exec` verbatim, because that is what the entry says. Nobody
//! starts a program by running the string: it has quoting rules, field codes that
//! only mean something at launch time, and a prefix of environment assignments.
//! This module is the half of that which needs no display server and no `GLib` — the
//! parse — so it can be tested here and so the daemon's launch path is one call.
//!
//! The rules implemented are the ones the desktop entry spec actually defines, and
//! only those:
//!
//! Quoting follows shell rules with a reduced set of escapes, because the spec says
//! so rather than because a shell is involved. There is no globbing, no `$VAR`
//! expansion, no pipes: an application's command line is not a script, and treating
//! it as one would let a `.desktop` file written by a packager run arbitrary code as
//! a side effect of a search result being clicked.
//!
//! Field codes (`%f`, `%U`, `%i`, …) are replaced from the launch context, and a
//! code with no value in that context is dropped rather than passed through — a
//! literal `%f` in a program's argv is never what was meant.
//!
//! Leading `NAME=value` assignments are separated from the command, since they are
//! not arguments and the first token is not always the program.

use std::path::{Path, PathBuf};

/// What a launch knows at the moment of launching. The codes are only meaningful
/// relative to this, which is why they are not expanded when the entry is indexed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context<'a> {
    /// One file to open, for `%f` and `%d`.
    pub file: Option<&'a Path>,
    /// Several files, for `%F` and `%D`.
    pub files: &'a [std::path::PathBuf],
    /// One URI, for `%u`.
    pub uri: Option<&'a str>,
    /// Several URIs, for `%U`.
    pub uris: &'a [String],
    /// The entry's `Icon`, for `%i`, passed as the two arguments the spec dictates.
    pub icon: Option<&'a str>,
    /// The localized name, for `%c`.
    pub name: Option<&'a str>,
    /// The entry's own path, for `%k`.
    pub desktop_file: Option<&'a Path>,
}

impl<'a> Context<'a> {
    /// A launch with no document: the application starts by itself.
    #[must_use]
    pub fn bare() -> Self {
        Self::default()
    }

    /// A launch of one file.
    #[must_use]
    pub fn for_file(path: &'a Path) -> Self {
        Self {
            file: Some(path),
            ..Self::default()
        }
    }

    fn one_file(&self) -> Option<String> {
        self.file
            .map(|p| p.display().to_string())
            .or_else(|| self.files.first().map(|p| p.display().to_string()))
    }

    fn many_files(&self) -> Vec<String> {
        if !self.files.is_empty() {
            return self.files.iter().map(|p| p.display().to_string()).collect();
        }
        self.one_file().into_iter().collect()
    }

    /// `%d`: the folder holding the file, which is what the spec asks for and what
    /// an application that uses the code means by it.
    fn one_directory(&self) -> Option<String> {
        let path = match self.file {
            Some(path) => path.to_path_buf(),
            None => self.files.first()?.clone(),
        };
        path.parent().map(|parent| parent.display().to_string())
    }

    /// `%D`.
    fn many_directories(&self) -> Vec<String> {
        let mut paths: Vec<&Path> = self.files.iter().map(PathBuf::as_path).collect();
        paths.extend(self.file);
        paths
            .iter()
            .filter_map(|path| path.parent())
            .map(|parent| parent.display().to_string())
            .collect()
    }

    fn one_uri(&self) -> Option<&str> {
        self.uri.or(self.uris.first().map(String::as_str))
    }
}

/// A parsed `Exec` line: the environment it asks for, and the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// `NAME=value` assignments preceding the program. Applied to the child, not
    /// expanded — a value is literal text.
    pub env: Vec<(String, String)>,
    /// The program, and its fixed arguments.
    pub argv: Vec<String>,
}

impl Exec {
    /// The program to start, or `None` for a line that names none.
    #[must_use]
    pub fn program(&self) -> Option<&str> {
        self.argv.first().map(String::as_str)
    }

    /// The program's path, resolved against `$PATH` when the entry gave a bare name.
    ///
    /// This is what `TryExec` checks and what a "the application is no longer
    /// installed" message is based on.
    #[must_use]
    pub fn resolved(&self) -> Option<std::path::PathBuf> {
        let program = self.program()?;
        let candidate = std::path::Path::new(program);
        if candidate.is_absolute() {
            return (candidate.exists() || candidate.is_symlink()).then(|| candidate.to_path_buf());
        }
        program
            .contains('/')
            .then(|| candidate.to_path_buf())
            .filter(|p| p.exists())
            .or_else(|| {
                let path = std::env::var_os("PATH")?;
                std::env::split_paths(&path)
                    .map(|dir| dir.join(program))
                    .find(|p| p.is_file())
            })
    }

    /// The full argv for a launch context: the command with its field codes filled
    /// in. Codes with no value in this context disappear.
    #[must_use]
    pub fn expand(&self, context: &Context<'_>) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.argv.len() + 2);
        let mut argv_iter = self.argv.iter();
        let Some(program) = argv_iter.next() else {
            return argv;
        };
        argv.push(program.clone());
        // One argument at a time, because a code can stand in the middle of one:
        // `100%%` is the single argument `100%`, and `%f.txt` is the file with a
        // suffix, not two arguments. Literal text accumulates; a code that yields
        // paths flushes what came before it and contributes its own arguments.
        for argument in argv_iter {
            let mut pending = String::new();
            for code in codes(argument) {
                match code {
                    Code::Literal(text) => pending.push_str(text),
                    Code::Percent => pending.push('%'),
                    Code::File => argv.extend(take(&mut pending, context.one_file())),
                    Code::Directory => argv.extend(take(&mut pending, context.one_directory())),
                    Code::Files => argv.extend(take(&mut pending, context.many_files())),
                    Code::Directories => {
                        argv.extend(take(&mut pending, context.many_directories()));
                    }
                    Code::Uri => {
                        argv.extend(take(&mut pending, context.one_uri().map(str::to_string)));
                    }
                    Code::Uris => argv.extend(take(&mut pending, context.uris.to_vec())),
                    Code::Icon => {
                        if let Some(icon) = context.icon.filter(|i| !i.is_empty()) {
                            argv.extend(take(&mut pending, Some("--icon".to_string())));
                            argv.extend(take(&mut pending, Some(icon.to_string())));
                        }
                    }
                    Code::Name => {
                        argv.extend(take(&mut pending, context.name.map(str::to_string)));
                    }
                    Code::Entry => argv.extend(take(
                        &mut pending,
                        context.desktop_file.map(|p| p.display().to_string()),
                    )),
                    // A code this build does not implement, or one that needs a
                    // display (`%v`, `%m`): dropped, so the program is not handed a
                    // literal `%v` to interpret as a filename.
                    Code::Dropped => {}
                }
            }
            if !pending.is_empty() {
                argv.push(pending);
            }
        }
        argv
    }
}

/// Attach `values` to the literal text accumulated so far. Text in front of a code
/// belongs to its first value — `%fprefix` is one path, not an argument `prefix` —
/// and text with no value to attach to is kept anyway, since dropping a literal
/// would silently change the command line.
fn take(pending: &mut String, values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<String> = values.into_iter().collect();
    if pending.is_empty() {
        return values;
    }
    if let Some(first) = values.first_mut() {
        first.insert_str(0, pending);
        pending.clear();
        return values;
    }
    let text = std::mem::take(pending);
    vec![text]
}

/// Expand an `Exec` value straight into an argv, without keeping the parse.
///
/// The shape the daemon wants when a search result is activated: one call, one
/// vector to hand to `gio::AppInfo::launch`'s equivalent. `None` when the line
/// names no program, which is the caller's "this entry cannot be started".
#[must_use]
pub fn expand(exec: &str, context: &Context<'_>) -> Option<Vec<String>> {
    parse(exec).map(|command| command.expand(context))
}

/// Parse one `Exec` value.
///
/// Returns `None` for a line with no command in it, which the caller treats as
/// "this entry cannot be launched" rather than as an error: an entry that only
/// carries a `DBusActivatable` D-Bus name has no `Exec` at all and is still a real
/// application.
#[must_use]
pub fn parse(exec: &str) -> Option<Exec> {
    let mut env = Vec::new();
    let mut argv = Vec::new();
    let mut first = true;
    for token in tokenize(exec) {
        // Only the tokens before the program can be assignments, so `FOO=bar` after
        // it is an argument that merely looks like one.
        if first {
            if let Some((name, value)) = assignment(&token) {
                env.push((name, value));
                continue;
            }
            first = false;
        }
        argv.push(token);
    }
    if argv.is_empty() {
        return None;
    }
    Some(Exec { env, argv })
}

/// The program an `Exec` line would start, without parsing the whole line. Used by
/// the indexer's `TryExec` check when the entry is being indexed and nothing is
/// being launched.
#[must_use]
pub fn launchable(exec: &str) -> Option<std::path::PathBuf> {
    parse(exec).and_then(|command| command.resolved())
}

/// `NAME=value`, and only in the form the spec allows: a name of `[A-Za-z_][A-Za-z0-9_]*`.
fn assignment(token: &str) -> Option<(String, String)> {
    let (name, value) = token.split_once('=')?;
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    (valid && !name.is_empty()).then(|| (name.to_string(), value.to_string()))
}

/// A piece of an argument, after field codes are taken out.
#[derive(Debug, PartialEq, Eq)]
enum Code<'a> {
    Literal(&'a str),
    /// A single file path.
    File,
    Files,
    /// The directory containing the file.
    Directory,
    Directories,
    Uri,
    Uris,
    Icon,
    Name,
    Entry,
    Percent,
    /// Anything else, including `%v` and `%m`.
    Dropped,
}

/// Split one argument into literal text and field codes.
fn codes(argument: &str) -> Vec<Code<'_>> {
    let bytes = argument.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' || index + 1 >= bytes.len() {
            index += 1;
            continue;
        }
        let code = match bytes[index + 1] {
            b'f' => Code::File,
            b'F' => Code::Files,
            b'd' => Code::Directory,
            b'D' => Code::Directories,
            b'u' => Code::Uri,
            b'U' => Code::Uris,
            b'i' => Code::Icon,
            b'c' => Code::Name,
            b'k' => Code::Entry,
            b'%' => Code::Percent,
            _ => Code::Dropped,
        };
        if index > start {
            out.push(Code::Literal(&argument[start..index]));
        }
        out.push(code);
        index += 2;
        start = index;
    }
    if start < argument.len() {
        out.push(Code::Literal(&argument[start..]));
    }
    out
}

/// Split an `Exec` value into tokens, applying the spec's quoting.
///
/// Single quotes are literal, double quotes allow `` \\ ` $ " `` and a backslash
/// escape, and a backslash outside quotes escapes the next character. No word
/// splitting beyond whitespace, and no expansion of anything that is not a field
/// code — see the module docs for why that is a security property, not a shortcut.
fn tokenize(exec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            '"' => {
                started = true;
                // Not a `for` loop: an escape inside double quotes consumes a second
                // character from the same iterator, which a `for` binding would hold
                // borrowed.
                #[allow(clippy::while_let_on_iterator)]
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(next @ ('\\' | '`' | '$' | '"' | '\n')) => current.push(next),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => break,
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some(next) => current.push(next),
                    None => current.push('\\'),
                }
            }
            other => {
                started = true;
                current.push(other);
            }
        }
    }
    if started {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn argv_of(exec: &str, context: &Context<'_>) -> Vec<String> {
        parse(exec)
            .unwrap_or_else(|| panic!("`{exec}` should parse"))
            .expand(context)
    }

    #[test]
    fn an_entry_with_no_arguments_is_just_its_program() {
        assert_eq!(argv_of("baob", &Context::bare()), vec!["baob"]);
        assert_eq!(parse("baob").expect("parses").env, Vec::new());
    }

    #[test]
    fn quoting_follows_the_spec_and_not_the_shell() {
        assert_eq!(
            argv_of("editor --title 'Two words' file.txt", &Context::bare()),
            vec!["editor", "--title", "Two words", "file.txt"]
        );
        assert_eq!(
            argv_of("editor \"a\\\"b\"", &Context::bare()),
            vec!["editor", "a\"b"]
        );
        assert_eq!(
            argv_of("editor a\\ b", &Context::bare()),
            vec!["editor", "a b"],
            "a backslash outside quotes escapes the next character"
        );
        assert_eq!(
            argv_of("sh -c 'echo $HOME | wc -l'", &Context::bare()),
            vec!["sh", "-c", "echo $HOME | wc -l"],
            "no expansion, no pipe: an Exec line is a command line, not a script"
        );
    }

    #[test]
    fn leading_environment_assignments_are_not_arguments() {
        let command =
            parse("FOO=bar BAZ=1 editor --wait %U").expect("a command with an environment prefix");
        assert_eq!(
            command.env,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "1".to_string())
            ]
        );
        assert_eq!(command.program(), Some("editor"));
        assert_eq!(
            command.expand(&Context::bare()),
            vec!["editor", "--wait"],
            "with no URI in the context, %U leaves no argument behind"
        );
    }

    #[test]
    fn an_assignment_after_the_program_is_an_argument() {
        assert_eq!(
            argv_of("env FOO=bar", &Context::bare()),
            vec!["env", "FOO=bar"],
            "the first token is the program, so this is an argument to `env`"
        );
    }

    #[test]
    fn a_name_that_is_not_an_identifier_is_an_argument() {
        assert_eq!(
            argv_of("1BAD=x editor", &Context::bare()),
            vec!["1BAD=x", "editor"],
            "a token that cannot be an environment name is the program's first argument"
        );
    }

    #[test]
    fn field_codes_are_filled_from_the_context() {
        let path = PathBuf::from("/home/u/notes.txt");
        let context = Context::for_file(&path);
        assert_eq!(
            argv_of("editor %f", &context),
            vec!["editor", "/home/u/notes.txt"]
        );
        assert_eq!(
            argv_of("editor %F", &context),
            vec!["editor", "/home/u/notes.txt"]
        );
        assert_eq!(
            argv_of(
                "editor %i --name %c %k",
                &Context {
                    icon: Some("org.gnome.baob"),
                    name: Some("Disk Usage Analyzer"),
                    desktop_file: Some(Path::new("/usr/share/applications/baob.desktop")),
                    ..Context::bare()
                }
            ),
            vec![
                "editor",
                "--icon",
                "org.gnome.baob",
                "--name",
                "Disk Usage Analyzer",
                "/usr/share/applications/baob.desktop"
            ],
            "%i expands to the two arguments the spec dictates, not to the icon alone"
        );
    }

    #[test]
    fn a_code_with_nothing_to_fill_in_leaves_no_argument() {
        assert_eq!(
            argv_of("editor %f %U", &Context::bare()),
            vec!["editor"],
            "a literal %f in a program's argv is never what was meant"
        );
        let context = Context {
            uris: &["file:///home/u/a.txt".to_string(), "sftp://h/b".to_string()],
            ..Context::bare()
        };
        assert_eq!(
            argv_of("editor %U", &context),
            vec!["editor", "file:///home/u/a.txt", "sftp://h/b"]
        );
        assert_eq!(
            argv_of("editor %u", &context),
            vec!["editor", "file:///home/u/a.txt"],
            "%u takes the first of several"
        );
    }

    #[test]
    fn a_percent_is_literal_and_an_unknown_code_is_dropped() {
        assert_eq!(
            argv_of("editor 100%% %v", &Context::bare()),
            vec!["editor", "100%"],
            "`%%` is a percent, and %v — which needs a display we do not have — is \
             dropped rather than handed over as a literal"
        );
        assert_eq!(
            argv_of("editor %n", &Context::bare()),
            vec!["editor"],
            "a deprecated code is dropped rather than passed through"
        );
    }

    #[test]
    fn d_passes_the_containing_folder_and_f_the_file() {
        let context = Context::for_file(Path::new("/home/u/docs/notes.txt"));
        assert_eq!(
            argv_of("viewer %d", &context),
            vec!["viewer", "/home/u/docs"],
            "the spec defines %d as the file's folder, and an application that asks \
             for it wants the folder"
        );
        assert_eq!(
            argv_of("viewer %f", &context),
            vec!["viewer", "/home/u/docs/notes.txt"]
        );
    }

    #[test]
    fn a_trailing_percent_sign_is_kept_as_text() {
        assert_eq!(
            argv_of("editor 50%", &Context::bare()),
            vec!["editor", "50%"]
        );
    }

    #[test]
    fn codes_inside_a_quoted_argument_still_expand() {
        let context = Context::for_file(Path::new("/home/u/a b.txt"));
        assert_eq!(
            argv_of("editor '%f'", &context),
            vec!["editor", "/home/u/a b.txt"],
            "the spec expands codes before quoting is resolved, and quoting an \
             argument containing a path with a space must not stop it being one path"
        );
    }

    #[test]
    fn an_empty_line_names_no_program() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(
            parse("FOO=bar"),
            None,
            "an environment with no command is nothing to start"
        );
        assert_eq!(launchable(""), None);
    }

    #[test]
    fn a_program_is_resolved_only_if_it_exists() {
        let command = parse("/bin/sh -c true").expect("a command");
        assert_eq!(
            command.resolved(),
            Some(PathBuf::from("/bin/sh")),
            "an absolute path is checked as given"
        );
        let missing = parse("/usr/bin/a-program-that-is-not-here").expect("a command");
        assert_eq!(missing.resolved(), None);
        let by_name = parse("sh").expect("a command");
        assert!(
            by_name.resolved().is_some(),
            "a bare name is found on $PATH, which is what TryExec means"
        );
    }
}
