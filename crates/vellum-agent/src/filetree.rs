//! Scoped, lazy directory reads — the file tree an agent node is pointed at.
//!
//! Feature 7 is *"a file tree, scoped per agent"*, and both halves are load-bearing.
//! **Scoped**: a tree rooted at `crates/vellum-agent` shows that subtree and refuses to walk
//! out of it, so two trees on one board can show two different parts of a project rather
//! than two copies of the whole thing. **Lazy**: one directory level is read at a time, on
//! demand, and never a whole tree.
//!
//! # Why laziness is not an optimisation here
//!
//! A `target/` directory holds forty thousand entries and a `node_modules` holds more. A
//! node that walked its root on creation would stat all of them on the frame it was placed,
//! and the user would blame the drop on the canvas rather than on the directory. So
//! [`list`] reads exactly one level, [`visible`] reads only the levels the user has opened,
//! and [`search`] is bounded in both directions before it starts.
//!
//! # The gitignore matcher, and exactly what it does not do
//!
//! There is no `ignore` crate in this dependency set, so this module carries a small
//! matcher. It is deliberately partial, and an honest partial matcher with its limits
//! written down is worth more than one that silently gets `**` wrong:
//!
//! **Implemented** — literal names; `*` and `?` globs within one path segment; directory-only
//! patterns (`build/`); negation (`!keep.log`); anchoring with a leading slash (`/target`)
//! and by a slash in the middle (`docs/*.pdf`); comments and blank lines; **last match
//! wins**, which is what makes a negation able to override an earlier rule; unanchored
//! patterns matching at any depth; `.gitignore` files read from the root down to the
//! directory being listed, parents first; and a path under an ignored *directory* being
//! ignored regardless of its own rules, as git does.
//!
//! **Not implemented, and it will get these wrong** — `**` (treated as an ordinary `*`
//! segment, so `a/**/b` matches only `a/<one>/b`); character classes `[a-z]`; backslash
//! escaping of `#`, `!`, spaces and glob characters; `.git/info/exclude`, the global
//! `core.excludesFile` and `$GIT_DIR/info/exclude`; and per-file negation inside an ignored
//! directory, which git also refuses but for a different reason.
//!
//! `.git` itself is skipped unconditionally and is not a rule — it is never interesting and
//! it is where a mistaken write would be unrecoverable.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::model::FileTreeModel;
use crate::{AgentError, Result};

/// The most rows [`visible`] will return.
///
/// A ceiling rather than a target: a user who expands a directory of forty thousand files
/// gets the first few thousand and a truncation flag, which is a legible answer, where an
/// unbounded version is a frame that never finishes. The painter cannot draw more than a few
/// dozen rows at a time in any case.
pub const MAX_ROWS: usize = 4_000;

/// The most directories [`search`] will open.
///
/// This is the second of the two things that make a symlink loop terminate — the first is
/// that a directory reached through a symlink is not descended into at all. Either alone
/// would do; both, because a bound that is never reached costs nothing and a loop that hangs
/// the frame loop costs the application.
pub const MAX_SEARCH_DIRS: usize = 5_000;

/// One row of a tree: what to draw, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name alone — what the row shows.
    pub name: String,
    /// The path relative to the tree's root, `/`-separated and never starting with one.
    ///
    /// This is the form [`FileTreeModel::expanded`] stores and the form every function here
    /// takes, so a board reopened on another machine finds the same directories open.
    pub relative: String,
    /// The absolute path, for opening the file or handing it to an agent as context.
    pub path: PathBuf,
    pub is_dir: bool,
    /// The file's length in bytes; zero for a directory, which is not the same statement as
    /// an empty file and is why [`Entry::is_dir`] is checked first everywhere.
    pub len: u64,
    /// Whether a `.gitignore` matched this. Only ever `true` when the tree was asked to show
    /// ignored files — otherwise the entry is not returned at all — so the painter can grey
    /// it rather than pretend it is ordinary.
    pub ignored: bool,
    /// Whether this entry is itself a symbolic link. Reported because a link is worth
    /// showing as one, and because [`search`] refuses to descend through it.
    pub is_symlink: bool,
}

/// A row in the flattened view: an entry, how deep it sits, and whether it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub entry: Entry,
    /// 0 for a child of the root. The painter's indent.
    pub depth: usize,
    /// Whether this directory's children follow. Always `false` for a file.
    pub expanded: bool,
}

/// What [`visible`] produced, and whether it is the whole story.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    pub rows: Vec<Row>,
    /// `true` when [`MAX_ROWS`] cut the list. The node says so rather than quietly showing
    /// a prefix — a tree that stops at four thousand rows with no sign is a tree the user
    /// believes is complete.
    pub truncated: bool,
}

impl FileTreeModel {
    /// Whether a directory, named relative to the root, is open.
    pub fn is_expanded(&self, relative: &str) -> bool {
        let key = normalise(relative);
        self.expanded.iter().any(|open| open == &key)
    }

    /// Opens a directory. Idempotent, so a double click cannot record it twice.
    pub fn expand(&mut self, relative: &str) {
        let key = normalise(relative);
        if key.is_empty() || self.expanded.iter().any(|open| open == &key) {
            return;
        }
        self.expanded.push(key);
    }

    /// Closes a directory **and everything under it**.
    ///
    /// Closing a parent while leaving its children recorded as open is how a tree reopens
    /// with a directory that is shut but whose grandchildren are somehow expanded — the
    /// state is unreachable by clicking and looks like corruption.
    pub fn collapse(&mut self, relative: &str) {
        let key = normalise(relative);
        if key.is_empty() {
            return;
        }
        let prefix = format!("{key}/");
        self.expanded.retain(|open| open != &key && !open.starts_with(&prefix));
    }

    pub fn toggle(&mut self, relative: &str) {
        if self.is_expanded(relative) {
            self.collapse(relative);
        } else {
            self.expand(relative);
        }
    }
}

/// Resolves a path relative to the tree's root, refusing anything that leaves it.
///
/// **This is the module's one security-shaped function.** The relative path reaches it from
/// the document — a board file the user may have been given — and from an agent asking to
/// read part of its tree, so `..` in one must not walk out of the root into the user's home
/// directory.
///
/// Two checks, because either alone is insufficient:
///
/// - **Lexical**, over [`Path::components`] rather than a substring search. `a..b` is a legal
///   file name and `..` is not a substring question; a check spelled `contains("..")` both
///   refuses valid names and misses an absolute path, which is the other way out.
/// - **Physical**, over [`fs::canonicalize`], because a *symlink* inside the tree pointing at
///   `/etc` passes every lexical test there is. Applied only when both sides canonicalise —
///   a path that does not exist yet cannot be resolved and has nothing to escape through.
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    // **Checked before normalising, which is the order that matters.** `normalise` drops
    // empty segments, so it turns `/etc` into `etc` — and the component scan below would
    // then see one perfectly ordinary `Normal` component and allow it. The path would land
    // inside the root rather than at `/etc`, so this is not an escape; it is an absolute
    // path being silently reinterpreted as a relative one, which is worse to debug than a
    // refusal. Measured: with this check removed, `resolve(root/src, "/etc")` answers
    // `Ok(root/src/etc)`.
    let folded = relative.replace('\\', "/");
    if Path::new(relative).is_absolute() || folded.starts_with('/') {
        return Err(AgentError::Refused(format!(
            "`{relative}` is an absolute path and leaves this tree's root, which it may not do"
        )));
    }

    let cleaned = normalise(relative);
    if cleaned.is_empty() {
        return Ok(root.to_path_buf());
    }
    for component in Path::new(&cleaned).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(AgentError::Refused(format!(
                    "`{relative}` leaves this tree's root, which it may not do"
                )));
            }
        }
    }

    let joined = root.join(&cleaned);
    if let (Ok(real_root), Ok(real)) = (fs::canonicalize(root), fs::canonicalize(&joined))
        && !real.starts_with(&real_root)
    {
        return Err(AgentError::Refused(format!(
            "`{relative}` points outside this tree's root through a link, which it may not do"
        )));
    }
    Ok(joined)
}

/// Reads **one** directory level.
///
/// Sorted directories first, then by name case-insensitively — what every file browser does
/// and therefore what the user expects. The case-sensitive comparison is the tie-break, so
/// `README` and `readme` in one directory have a stable order rather than whichever the
/// filesystem handed back first.
///
/// An entry that cannot be stat'ed is still returned, as a file of length zero: a broken
/// symlink is a thing the user has and a row saying so is better than a row that is missing.
pub fn list(root: &Path, relative: &str, show_ignored: bool) -> Result<Vec<Entry>> {
    let dir = resolve(root, relative)?;
    // The rules are loaded **even when everything is being shown**, because
    // [`Entry::ignored`] is what lets the painter grey an ignored row rather than draw it as
    // an ordinary one. Skipping the load when `show_ignored` is true saves a handful of file
    // reads and makes that flag permanently `false` on the one path where anybody sees it —
    // measured: `target` came back with `ignored: false` while being drawn as ignorable.
    let ignore = Ignore::load(root, relative);
    let base = normalise(relative);

    let reader = fs::read_dir(&dir)
        .map_err(|error| AgentError::file(dir.display().to_string(), &error))?;

    let mut out = Vec::new();
    for entry in reader.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().ok();
        let is_symlink = file_type.is_some_and(|kind| kind.is_symlink());
        // A symlink's own type says only that it is a link, so the target is stat'ed to find
        // out whether it should be drawn as a directory. An unresolvable link answers `Err`
        // — a loop returns `ELOOP` rather than hanging — and is drawn as a file.
        let metadata = if is_symlink { fs::metadata(&path).ok() } else { entry.metadata().ok() };
        let is_dir = metadata.as_ref().is_some_and(fs::Metadata::is_dir);
        let len = if is_dir { 0 } else { metadata.as_ref().map_or(0, fs::Metadata::len) };

        let child = join(&base, &name);
        let ignored = ignore.is_ignored(&child, is_dir);
        if ignored && !show_ignored {
            continue;
        }

        out.push(Entry { name, relative: child, path, is_dir, len, ignored, is_symlink });
    }

    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// The rows to draw: the root's children, plus the children of every directory the user has
/// opened, in order.
///
/// **Only the opened directories are read.** That is the laziness guarantee stated as code
/// rather than as a comment — a tree whose root holds a `target/` is one `read_dir` until
/// somebody clicks on it.
pub fn visible(root: &Path, model: &FileTreeModel, show_ignored: bool) -> Result<View> {
    let mut view = View::default();
    walk(root, model, "", 0, show_ignored, &mut view)?;
    Ok(view)
}

fn walk(
    root: &Path,
    model: &FileTreeModel,
    relative: &str,
    depth: usize,
    show_ignored: bool,
    view: &mut View,
) -> Result<()> {
    // A directory the user cannot have opened — because it cannot be read — is skipped
    // rather than failing the whole view. The root itself is the exception: a tree pointed
    // at a directory that is gone must say so.
    let entries = match list(root, relative, show_ignored) {
        Ok(entries) => entries,
        Err(error) if depth == 0 => return Err(error),
        Err(_) => return Ok(()),
    };

    for entry in entries {
        if view.rows.len() >= MAX_ROWS {
            view.truncated = true;
            return Ok(());
        }
        let expanded = entry.is_dir && model.is_expanded(&entry.relative);
        let child = entry.relative.clone();
        view.rows.push(Row { entry, depth, expanded });
        if expanded {
            walk(root, model, &child, depth + 1, show_ignored, view)?;
        }
    }
    Ok(())
}

/// Finds entries whose **name** contains `query`, case-insensitively, bounded in three ways.
///
/// Bounded by results (`limit`), by directories opened ([`MAX_SEARCH_DIRS`]) and by refusing
/// to descend through a symbolic link. The last is what makes a loop impossible rather than
/// merely slow: `a/b -> a` is an infinite path, and a depth limit alone would still walk it
/// to the limit every time.
///
/// Breadth-first, so a shallow match is found before a deep one — which is what somebody
/// typing three letters into a file tree means.
pub fn search(root: &Path, query: &str, limit: usize, show_ignored: bool) -> Result<Vec<Entry>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([String::new()]);
    let mut opened = 0usize;

    while let Some(relative) = queue.pop_front() {
        if opened >= MAX_SEARCH_DIRS || found.len() >= limit {
            break;
        }
        opened += 1;
        // An unreadable directory mid-walk is skipped: a permission error two levels down
        // must not turn a search into a failure.
        let Ok(entries) = list(root, &relative, show_ignored) else { continue };
        for entry in entries {
            if entry.name.to_lowercase().contains(&needle) {
                found.push(entry.clone());
                if found.len() >= limit {
                    break;
                }
            }
            if entry.is_dir && !entry.is_symlink {
                queue.push_back(entry.relative);
            }
        }
    }
    Ok(found)
}

/// A relative path in the one spelling this module stores: `/`-separated, no leading or
/// trailing separator, no `./`.
///
/// Backslashes are folded to `/` so a path recorded on Windows and a path recorded on macOS
/// name the same directory in the same board file.
fn normalise(relative: &str) -> String {
    let swapped = relative.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in swapped.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        parts.push(part);
    }
    parts.join("/")
}

fn join(base: &str, name: &str) -> String {
    if base.is_empty() { name.to_owned() } else { format!("{base}/{name}") }
}

// ---------------------------------------------------------------------------------------
// The gitignore matcher. See the module header for exactly what it covers.
// ---------------------------------------------------------------------------------------

/// One line of a `.gitignore`, with the directory that file sat in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// The directory the rule was written in, relative to the tree root and normalised.
    /// An anchored pattern is anchored *here*, not at the root.
    base: String,
    /// The pattern, with its `!`, leading `/` and trailing `/` already stripped.
    pattern: String,
    negated: bool,
    /// Written with a trailing `/`, so it matches only directories.
    dir_only: bool,
    /// Written with a leading `/`, or containing a `/` — either way it is matched against
    /// the whole path below `base` rather than against a name at any depth.
    anchored: bool,
}

/// The rules that apply to one directory, in the order they were read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ignore {
    rules: Vec<Rule>,
}

impl Ignore {
    /// No rules at all: everything is visible.
    ///
    /// For a caller that already knows there is no `.gitignore` to honour — a tree rooted
    /// outside a repository, say. Deliberately **not** what [`list`] uses for
    /// `show_ignored`: see the comment there.
    pub fn none() -> Self {
        Self::default()
    }

    /// Reads the `.gitignore` files from the tree's root down to `relative`, parents first.
    ///
    /// Parents first is what makes last-match-wins mean what git means by it: a child's
    /// `!keep.log` must be able to override a parent's `*.log`, and rule order is the only
    /// thing that expresses the override.
    ///
    /// A missing or unreadable `.gitignore` contributes nothing — it is the overwhelmingly
    /// common case, not an error.
    pub fn load(root: &Path, relative: &str) -> Self {
        let cleaned = normalise(relative);
        let mut rules = Vec::new();
        let mut base = String::new();

        // The root's own file, then one per level down to the listed directory.
        let mut dirs = vec![String::new()];
        for part in cleaned.split('/').filter(|part| !part.is_empty()) {
            base = join(&base, part);
            dirs.push(base.clone());
        }

        for dir in dirs {
            let Ok(path) = resolve(root, &dir) else { continue };
            let Ok(text) = fs::read_to_string(path.join(".gitignore")) else { continue };
            rules.extend(parse_ignore(&text, &dir));
        }
        Self { rules }
    }

    /// Parses one file's worth of rules, as though it sat in `base`. Public for tests and
    /// for a caller that already holds the text.
    pub fn from_text(text: &str, base: &str) -> Self {
        Self { rules: parse_ignore(text, &normalise(base)) }
    }

    /// Whether a path relative to the tree root is ignored.
    ///
    /// **Ancestors are tested first, as directories.** Git ignores everything under an
    /// ignored directory and will not let a rule re-include a file inside one — so `target/`
    /// hides `target/debug/build.log` without any rule naming it, which is the entire reason
    /// a `.gitignore` is short.
    pub fn is_ignored(&self, relative: &str, is_dir: bool) -> bool {
        let cleaned = normalise(relative);
        if cleaned.is_empty() {
            return false;
        }
        let parts: Vec<&str> = cleaned.split('/').collect();
        for cut in 1..parts.len() {
            let ancestor = parts[..cut].join("/");
            if self.matches(&ancestor, true) {
                return true;
            }
        }
        self.matches(&cleaned, is_dir)
    }

    /// Last match wins, which is what a negation needs to be able to do anything at all.
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        let mut verdict = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let Some(rest) = below(&rule.base, path) else { continue };
            let hit = if rule.anchored {
                path_match(&rule.pattern, rest)
            } else {
                // Unanchored: matched against each name on the way down, which is how
                // `*.log` in the root hides `a/b/c.log`.
                rest.split('/').any(|name| glob_match(&rule.pattern, name))
            };
            if hit {
                verdict = !rule.negated;
            }
        }
        verdict
    }
}

/// The part of `path` below `base`, or `None` when `path` is not under it.
fn below<'a>(base: &str, path: &'a str) -> Option<&'a str> {
    if base.is_empty() {
        return Some(path);
    }
    path.strip_prefix(base)?.strip_prefix('/')
}

fn parse_ignore(text: &str, base: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    for raw in text.lines() {
        // Trailing whitespace is not part of a pattern; a backslash-escaped trailing space
        // is, and this does not implement that — see the module header.
        let line = raw.trim_end();
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (negated, rest) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let (dir_only, rest) = match rest.strip_suffix('/') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        let anchored_by_slash = rest.starts_with('/');
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() {
            continue;
        }
        rules.push(Rule {
            base: base.to_owned(),
            // A slash anywhere but the end anchors the pattern — git's own rule, and the
            // reason `docs/*.pdf` does not match `a/docs/b.pdf`.
            anchored: anchored_by_slash || rest.contains('/'),
            pattern: rest.to_owned(),
            negated,
            dir_only,
        });
    }
    rules
}

/// A multi-segment match: every segment of the pattern against every segment of the path.
///
/// Equal lengths required, because an anchored pattern names a path and not a prefix —
/// containment below an ignored directory is [`Ignore::is_ignored`]'s ancestor walk, which
/// is a different question and is answered in one place.
fn path_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    pattern.len() == path.len()
        && pattern.iter().zip(path.iter()).all(|(rule, name)| glob_match(rule, name))
}

/// `*` and `?` within one path segment.
///
/// Iterative with one backtrack point rather than recursive: the recursive form is
/// exponential on a pattern like `*a*a*a*b` against a long name, and a `.gitignore` is
/// user-supplied text. Indices are over `Vec<char>`, never bytes — this codebase has aborted
/// twice on byte-slicing a multi-byte character (feedback 30), and a file name is exactly
/// the kind of string that carries one.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // Where to resume from if the current `*` turns out to have matched too little.
    let (mut star, mut resume) = (None, 0usize);

    while t < text.len() {
        // `.copied()` so every arm compares two `char`s: a literal pattern against a `&char`
        // leans on match ergonomics, and this is not the place to find out how far that goes.
        match pattern.get(p).copied() {
            Some('*') => {
                star = Some(p);
                resume = t;
                p += 1;
            }
            Some('?') => {
                p += 1;
                t += 1;
            }
            Some(ch) if ch == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some(at) => {
                    p = at + 1;
                    resume += 1;
                    t = resume;
                }
                None => return false,
            },
        }
    }
    pattern.get(p..).is_some_and(|rest| rest.iter().all(|ch| *ch == '*'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// ```text
    /// root/
    ///   .gitignore        target/ and *.log, but !keep.log
    ///   src/main.rs  src/lib.rs  src/notes.md
    ///   docs/guide.md
    ///   target/debug/huge.bin
    ///   build.log  keep.log  README.md
    /// ```
    fn sample_tree() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n!keep.log\n# a comment\n\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("src/notes.md"), "").unwrap();
        fs::write(root.join("docs/guide.md"), "").unwrap();
        fs::write(root.join("target/debug/huge.bin"), "0123456789").unwrap();
        fs::write(root.join("build.log"), "").unwrap();
        fs::write(root.join("keep.log"), "").unwrap();
        fs::write(root.join("README.md"), "hello").unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        temp
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    /// Directories first, then case-insensitively by name — and `.git` is never there,
    /// whatever the ignore rules say.
    #[test]
    fn a_listing_puts_directories_first_and_never_shows_dot_git() {
        let temp = sample_tree();
        let listed = list(temp.path(), "", false).unwrap();
        assert_eq!(names(&listed), vec!["docs", "src", ".gitignore", "keep.log", "README.md"]);
        assert!(listed[0].is_dir && listed[1].is_dir);
        assert!(!listed[2].is_dir);
        assert!(!names(&listed).contains(&".git"), "the git directory was listed");
    }

    /// The ignore rules, both directions. `build.log` is hidden by `*.log`; `keep.log`
    /// survives it because `!keep.log` comes later and **last match wins** — a matcher that
    /// stopped at the first hit passes every other assertion in this file and fails this one.
    #[test]
    fn ignored_files_are_hidden_until_they_are_asked_for() {
        let temp = sample_tree();

        let hidden = list(temp.path(), "", false).unwrap();
        assert!(!names(&hidden).contains(&"build.log"), "*.log did not hide build.log");
        assert!(names(&hidden).contains(&"keep.log"), "!keep.log did not survive *.log");
        assert!(!names(&hidden).contains(&"target"), "target/ was not hidden");

        let shown = list(temp.path(), "", true).unwrap();
        assert!(names(&shown).contains(&"build.log"));
        assert!(names(&shown).contains(&"target"));
        let target = shown.iter().find(|entry| entry.name == "target").unwrap();
        assert!(target.ignored, "a shown-but-ignored entry must say it is ignored");
        let readme = shown.iter().find(|entry| entry.name == "README.md").unwrap();
        assert!(!readme.ignored);
        assert_eq!(readme.len, 5, "a file's length is what the row draws");
    }

    /// A file under an ignored directory is ignored without any rule naming it — which is
    /// the whole reason a `.gitignore` is four lines rather than four thousand.
    #[test]
    fn everything_under_an_ignored_directory_is_ignored_too() {
        let ignore = Ignore::from_text("target/\n", "");
        assert!(ignore.is_ignored("target", true));
        assert!(ignore.is_ignored("target/debug/huge.bin", false));
        assert!(!ignore.is_ignored("src/main.rs", false));

        // `target/` is directory-only, so a *file* called `target` is not ignored.
        assert!(!ignore.is_ignored("target", false), "a dir-only rule matched a file");
    }

    /// Anchoring, both spellings, and the case that separates them: an unanchored pattern
    /// matches at any depth, an anchored one does not.
    #[test]
    fn anchoring_decides_whether_a_pattern_reaches_below_the_top() {
        let anchored = Ignore::from_text("/build\ndocs/*.pdf\n", "");
        assert!(anchored.is_ignored("build", false));
        assert!(!anchored.is_ignored("src/build", false), "a leading slash did not anchor");
        assert!(anchored.is_ignored("docs/spec.pdf", false));
        assert!(
            !anchored.is_ignored("a/docs/spec.pdf", false),
            "a slash in the middle did not anchor"
        );

        let loose = Ignore::from_text("build\n", "");
        assert!(loose.is_ignored("build", false));
        assert!(loose.is_ignored("src/build", false), "an unanchored pattern must match at depth");
    }

    /// A nested `.gitignore` is read after its parents, so its negation can override them.
    #[test]
    fn a_nested_gitignore_can_override_its_parent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("logs/.gitignore"), "!important.log\n").unwrap();
        fs::write(root.join("logs/noise.log"), "").unwrap();
        fs::write(root.join("logs/important.log"), "").unwrap();

        let listed = list(root, "logs", false).unwrap();
        assert!(
            names(&listed).contains(&"important.log"),
            "the child's negation was not applied: {:?}",
            names(&listed)
        );
        assert!(!names(&listed).contains(&"noise.log"), "the parent's rule stopped applying");
    }

    /// The security-shaped one. `..` must not walk out, and neither must an absolute path or
    /// a symlink pointing outside — the last is why the check is not purely lexical.
    #[test]
    fn a_relative_path_can_never_leave_the_root() {
        let temp = sample_tree();
        let root = temp.path().join("src");

        for hostile in ["..", "../docs", "../../etc", "/etc"] {
            let error = resolve(&root, hostile).expect_err("a path escaped the root");
            assert!(error.to_string().contains("leaves this tree's root"), "{hostile}: {error}");
        }

        // A legal name that merely contains dots is not an escape. A `contains("..")` check
        // refuses this one, which is how you can tell the two implementations apart.
        fs::write(root.join("a..b.rs"), "").unwrap();
        assert!(resolve(&root, "a..b.rs").is_ok(), "a legal file name was refused");

        // And the physical half: a link inside the tree pointing at its parent.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path(), root.join("out")).unwrap();
            let error = resolve(&root, "out").expect_err("a symlink out of the tree resolved");
            assert!(error.to_string().contains("outside this tree's root"), "{error}");
        }
    }

    /// Laziness, as an assertion rather than a comment: with nothing expanded, the view is
    /// exactly the root's children. An eager walker returns the whole tree here.
    #[test]
    fn nothing_below_an_unopened_directory_is_read() {
        let temp = sample_tree();
        let mut model = FileTreeModel::default();

        let closed = visible(temp.path(), &model, false).unwrap();
        assert_eq!(closed.rows.len(), 5, "{:?}", closed.rows);
        assert!(closed.rows.iter().all(|row| row.depth == 0));
        assert!(!closed.truncated);

        model.expand("src");
        let open = visible(temp.path(), &model, false).unwrap();
        let opened: Vec<&str> = open.rows.iter().map(|row| row.entry.name.as_str()).collect();
        assert_eq!(
            opened,
            vec![
                "docs",
                "src",
                "lib.rs",
                "main.rs",
                "notes.md",
                ".gitignore",
                "keep.log",
                "README.md"
            ]
        );
        assert_eq!(open.rows[2].depth, 1, "a child was not indented under its parent");
        assert!(open.rows[1].expanded, "the opened directory did not say it was open");
    }

    /// Collapsing a parent must take its children's state with it, or the tree reopens with
    /// a closed directory whose grandchildren are somehow expanded.
    #[test]
    fn collapsing_a_directory_forgets_what_was_open_inside_it() {
        let mut model = FileTreeModel::default();
        model.expand("src");
        model.expand("src/inner");
        model.expand("srcextra"); // A prefix in *characters*, not in path segments.
        assert!(model.is_expanded("src/inner"));

        model.collapse("src");
        assert!(!model.is_expanded("src"));
        assert!(!model.is_expanded("src/inner"), "a grandchild stayed open under a closed parent");
        assert!(model.is_expanded("srcextra"), "a sibling with a shared prefix was collapsed too");

        // Idempotent, and normalising: `./src/` and `src` are the same directory.
        model.expand("./src/");
        model.expand("src");
        assert_eq!(model.expanded, vec!["srcextra".to_owned(), "src".to_owned()]);
    }

    /// A directory symlink pointing at its own ancestor is an infinite path. Search must
    /// return rather than walk it — and the reason it returns is that a link is never
    /// descended into, not that some depth limit eventually fires.
    #[test]
    #[cfg(unix)]
    fn a_symlink_loop_does_not_hang_a_search() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/target-file.txt"), "").unwrap();
        std::os::unix::fs::symlink(root, root.join("a/b/loop")).unwrap();

        let found = search(root, "target-file", 10, true).unwrap();
        assert_eq!(found.len(), 1, "the loop was walked, or the file was missed: {found:?}");
        assert_eq!(found[0].relative, "a/b/target-file.txt");

        // The link is still *listed* — it is a thing the user has — it is simply not entered.
        let listed = list(root, "a/b", true).unwrap();
        let link = listed.iter().find(|entry| entry.name == "loop").unwrap();
        assert!(link.is_symlink && link.is_dir, "a directory symlink should draw as a directory");
    }

    #[test]
    fn search_is_bounded_and_case_insensitive() {
        let temp = sample_tree();
        assert_eq!(search(temp.path(), "", 10, false).unwrap(), Vec::new());
        assert_eq!(search(temp.path(), "main", 0, false).unwrap(), Vec::new());

        let found = search(temp.path(), "MAIN", 10, false).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].relative, "src/main.rs");

        // Ignored files stay out of a search too, or the answer disagrees with the tree.
        assert!(search(temp.path(), "huge", 10, false).unwrap().is_empty());
        assert_eq!(search(temp.path(), "huge", 10, true).unwrap().len(), 1);

        let capped = search(temp.path(), ".", 2, true).unwrap();
        assert_eq!(capped.len(), 2, "the result limit was not honoured");
    }

    /// The glob is the part most likely to be quietly wrong, and it is pure, so it is
    /// tested directly. The last case is the one that matters for a `.gitignore` written by
    /// a person: a pattern of stars against a long name must not take exponential time.
    #[test]
    fn the_glob_handles_stars_questions_and_multibyte_names() {
        assert!(glob_match("*.log", "build.log"));
        assert!(!glob_match("*.log", "build.logger"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("build", "build"));
        assert!(!glob_match("build", "Build"), "the matcher must be case-sensitive, as git is");
        assert!(glob_match("*", ""), "a bare star matches an empty name");
        assert!(!glob_match("?", ""));

        // Byte-slicing a multi-byte name is how this codebase aborted twice.
        assert!(glob_match("夕*", "夕焼け.md"));
        assert!(glob_match("*け.md", "夕焼け.md"));
        assert!(!glob_match("*x.md", "夕焼け.md"));

        assert!(!glob_match("*a*a*a*a*b", &"a".repeat(60)));
    }
}
